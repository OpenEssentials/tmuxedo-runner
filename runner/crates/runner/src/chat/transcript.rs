//! An agent's own transcript (`[chat] transcript`), read when a terminal
//! session opens as a chat: the turns taken in the terminal are shown as turns,
//! and a session with no conversation yet is started under its id instead of
//! resumed (Claude writes its transcript from the first turn on).
//!
//! The file is the agent's, so its contents are untrusted data: only the file
//! of the matching session (`<projects dir>/<folder>/<session id>.jsonl`, a
//! regular file, never through a symlink), at most [`MAX_IMPORT`] bytes of it
//! (the newest), every event masked and cut like the chat's own on the way out.
//!
//! Format `claude-jsonl`: one JSON object a line; `user` lines are prompts (a
//! string or text blocks) or tool results, `assistant` lines are the agent's
//! messages, the same shapes the stream-json translator reads ([`TState::line`]).

use super::claude::{self, Emit, TState};
use serde_json::Value;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path};

/// The transcript format this module reads.
pub const CLAUDE_JSONL: &str = "claude-jsonl";
/// At most this much of a transcript is shown at once (its newest part).
pub const MAX_IMPORT: u64 = 4 << 20;
/// A prompt longer than this is cut.
const MAX_PROMPT: usize = 32 * 1024;

/// Whether `path` can be the transcript of agent session `vendor`: absolute, no `..`, named
/// `<vendor>.jsonl`, in a folder of a `projects` folder.
pub fn valid_path(path: &str, vendor: &str) -> bool {
    let p = Path::new(path);
    if vendor.is_empty() || !p.is_absolute() || p.components().any(|c| matches!(c, Component::ParentDir)) {
        return false;
    }
    let named = p.file_name().and_then(|n| n.to_str()) == Some(&format!("{vendor}.jsonl"));
    let under = p
        .parent()
        .and_then(Path::parent)
        .and_then(Path::file_name)
        .is_some_and(|n| n == "projects");
    named && under
}

/// The file, opened read-only without following a symlink; only a regular file.
fn open(path: &str) -> Option<File> {
    use std::os::unix::fs::OpenOptionsExt;
    let f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .ok()?;
    f.metadata().ok().filter(|m| m.is_file()).map(|_| f)
}

/// The transcript's length now (0 when there is none yet).
pub fn len(path: &str) -> u64 {
    open(path).and_then(|f| f.metadata().ok()).map_or(0, |m| m.len())
}

/// Whether the session has a conversation to resume (a prompt or an answer in its transcript).
pub fn has_conversation(path: &str) -> bool {
    let Some(f) = open(path) else { return false };
    let mut buf = Vec::new();
    if f.take(MAX_IMPORT).read_to_end(&mut buf).is_err() {
        return false;
    }
    buf.split(|b| *b == b'\n').any(|l| {
        serde_json::from_slice::<Value>(l)
            .ok()
            .is_some_and(|v| matches!(v["type"].as_str(), Some("user" | "assistant")))
    })
}

/// The whole lines written since `from`: (bytes, the offset after them, whether older lines
/// were left out to stay within [`MAX_IMPORT`]). None: no such file.
pub fn read_from(path: &str, from: u64) -> Option<(Vec<u8>, u64, bool)> {
    let mut f = open(path)?;
    let end = f.metadata().ok()?.len();
    if from >= end {
        return Some((vec![], end.min(from), false));
    }
    let start = from.max(end.saturating_sub(MAX_IMPORT));
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    f.take(end - start).read_to_end(&mut buf).ok()?;
    let mut skipped = false;
    if start > from {
        // Cut mid-line: from the next whole line on.
        skipped = true;
        let n = buf.iter().position(|b| *b == b'\n').map_or(buf.len(), |i| i + 1);
        buf.drain(..n);
    }
    let consumed = start + (end - start) - buf.len() as u64;
    // A last line still being written waits for the next read.
    let whole = buf.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
    buf.truncate(whole);
    Some((buf, consumed + whole as u64, skipped))
}

/// One thing a transcript says.
// Short-lived (one read's worth), so the larger variant is not boxed.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    /// A prompt typed in the terminal: a new turn begins.
    Prompt(String),
    /// What the agent did or said in the turn.
    Emit(Emit),
}

/// The prompt a `user` line carries (None: a tool result, a command or a note of the CLI's own).
fn prompt(v: &Value) -> Option<String> {
    if v["isMeta"].as_bool() == Some(true) {
        return None;
    }
    let c = &v["message"]["content"];
    let text = match c {
        Value::String(t) => t.clone(),
        Value::Array(blocks) => {
            if blocks.iter().any(|b| b["type"] != "text") {
                return None;
            }
            blocks
                .iter()
                .filter_map(|b| b["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        }
        _ => return None,
    };
    let t = text.trim();
    // The CLI's own lines: a slash command and its output, an interruption note.
    if t.is_empty()
        || t.starts_with("<command-")
        || t.starts_with("<local-command-")
        || t.starts_with("[Request interrupted")
    {
        return None;
    }
    Some(claude::cut(t, MAX_PROMPT))
}

/// Translate transcript lines (whole lines, as [`read_from`] gives them).
pub fn translate(bytes: &[u8], tr: &mut TState) -> Vec<Item> {
    let mut out = vec![];
    for line in bytes.split(|b| *b == b'\n') {
        let Ok(v) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        if v["isSidechain"].as_bool() == Some(true) {
            continue; // a helper's own conversation: its calls are shown in the parent's
        }
        match v["type"].as_str() {
            Some("user") => {
                if let Some(p) = prompt(&v) {
                    out.push(Item::Prompt(p));
                    continue;
                }
                let is_result = v["message"]["content"]
                    .as_array()
                    .is_some_and(|b| b.iter().any(|b| b["type"] == "tool_result"));
                if is_result {
                    out.extend(tr.line(&v, &|_| true).into_iter().map(Item::Emit));
                }
            }
            Some("assistant") => out.extend(tr.line(&v, &|_| true).into_iter().map(Item::Emit)),
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tmuxedo_protocol::chat::ChatBody;

    #[test]
    fn only_the_sessions_own_file() {
        assert!(valid_path("/h/.claude/projects/-w-a/s1.jsonl", "s1"));
        assert!(!valid_path("/h/.claude/projects/-w-a/s2.jsonl", "s1"));
        assert!(!valid_path("h/.claude/projects/-w-a/s1.jsonl", "s1"));
        assert!(!valid_path("/h/.claude/other/-w-a/s1.jsonl", "s1"));
        assert!(!valid_path("/h/.claude/projects/s1.jsonl", "s1"));
        assert!(!valid_path("/h/projects/x/../../etc/s1.jsonl", "s1"));
        assert!(!valid_path("/h/.claude/projects/-w-a/.jsonl", ""));
    }

    #[test]
    fn prompts_turns_and_tools() {
        let lines = [
            r#"{"type":"summary","summary":"x"}"#,
            r#"{"type":"user","message":{"role":"user","content":"fix it"}}"#,
            r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"text","text":"Looking."}]}}"#,
            r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"a b"}]}}"#,
            r#"{"type":"assistant","isSidechain":true,"message":{"id":"h","content":[{"type":"text","text":"helper"}]}}"#,
            r#"{"type":"user","isMeta":true,"message":{"content":"caveat"}}"#,
            r#"{"type":"user","message":{"content":"<command-name>/clear</command-name>"}}"#,
            r#"{"type":"user","message":{"content":[{"type":"text","text":"thanks"}]}}"#,
            "not json",
        ]
        .join("\n");
        let mut tr = TState::default();
        let items = translate(lines.as_bytes(), &mut tr);
        let shape: Vec<String> = items
            .iter()
            .map(|i| match i {
                Item::Prompt(p) => format!("prompt {p}"),
                Item::Emit(e) => e.body.kind().to_string(),
            })
            .collect();
        assert_eq!(
            shape,
            [
                "prompt fix it",
                "message.done",
                "tool.started",
                "tool.done",
                "prompt thanks"
            ],
            "{items:?}"
        );
        assert!(
            matches!(&items[1], Item::Emit(Emit { body: ChatBody::MessageDone { text, .. }, .. }) if text == "Looking.")
        );
    }

    #[test]
    fn reads_whole_lines_and_keeps_the_newest_part() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("projects").join("-w");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("s.jsonl");
        let ps = p.to_str().unwrap();
        assert_eq!(read_from(ps, 0), None);
        assert!(!has_conversation(ps));
        std::fs::write(
            &p,
            "{\"type\":\"user\",\"message\":{\"content\":\"a\"}}\n{\"type\":\"assi",
        )
        .unwrap();
        assert!(has_conversation(ps));
        let (b, at, skipped) = read_from(ps, 0).unwrap();
        assert!(!skipped);
        assert_eq!(at as usize, b.len());
        assert!(b.ends_with(b"}\n"));
        let (b2, at2, _) = read_from(ps, at).unwrap();
        assert!(b2.is_empty());
        assert_eq!(at2, at);
        // Too much since the last read: only the newest MAX_IMPORT bytes, from a whole line.
        let big = format!("{}\n", "x".repeat(MAX_IMPORT as usize));
        std::fs::write(
            &p,
            format!("{big}{{\"type\":\"user\",\"message\":{{\"content\":\"z\"}}}}\n"),
        )
        .unwrap();
        let (b, at, skipped) = read_from(ps, 0).unwrap();
        assert!(skipped);
        assert!(b.starts_with(b"{\"type\""), "{}", String::from_utf8_lossy(&b[..20]));
        assert_eq!(at, len(ps));
        // Never through a symlink.
        let link = dir.join("l.jsonl");
        std::os::unix::fs::symlink(&p, &link).unwrap();
        assert_eq!(read_from(link.to_str().unwrap(), 0), None);
        assert_eq!(len(link.to_str().unwrap()), 0);
    }
}
