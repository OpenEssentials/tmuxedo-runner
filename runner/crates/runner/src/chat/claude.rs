//! The built-in translator for Claude Code's structured mode (`claude -p
//! --input-format stream-json --output-format stream-json --verbose
//! --include-partial-messages`, docs/design/agent-chat.md §2.1.1, §8.2).
//!
//! In: one stdout line (a JSON object). Out: normalised chat events. Pure
//! apart from `exists` (did a file exist before a write), so the golden
//! transcript in tests/chat_claude.rs pins it.
//!
//! Decisions do not travel on stdout: they come through the same hooks as
//! the terminal (the PermissionRequest hold, and PreToolUse on
//! AskUserQuestion), and [`needs_item`] / [`reply`] turn a held hook into a
//! needs-you item and an answer back into the hook's output.

use super::mask::strip_controls;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use tmuxedo_protocol::chat::{
    Attachment, ChatBody, Diff, FileChange, Location, NeedsItem, PlanEntry, Question, QuestionOption,
    MAX_TOOL_ATTACHMENTS, MAX_TOOL_NAME, MAX_TOOL_OUTPUT, MAX_TOOL_SUBJECT,
};

/// One translated event and the sub-agent it belongs to.
#[derive(Debug, Clone, PartialEq)]
pub struct Emit {
    pub body: ChatBody,
    pub agent_path: Option<String>,
}

fn top(body: ChatBody) -> Emit {
    Emit { body, agent_path: None }
}

/// What the translator remembers between lines (persisted with the session,
/// so a runner restart mid-turn carries on where it was).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TState {
    /// The CLI's session id (from `system/init`).
    #[serde(default)]
    pub vendor_session: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    /// The message being streamed, and how many text blocks of it began.
    #[serde(default)]
    pub msg: Option<String>,
    #[serde(default)]
    pub msg_blocks: u32,
    /// Text blocks already completed, per message id.
    #[serde(default)]
    pub done_blocks: BTreeMap<String, u32>,
    /// Tools started and not finished: tool id → tool name.
    #[serde(default)]
    pub tools: BTreeMap<String, String>,
    /// An interrupt was sent: the turn ends as `interrupted`.
    #[serde(default)]
    pub interrupting: bool,
    /// Lines this version could not show.
    #[serde(default)]
    pub unknown: u64,
    /// Files an edit tool is changing: tool id → (path, whether it existed
    /// when the tool was announced). Reported in `files.changed` once the
    /// tool succeeded (a denied or failed edit changed nothing).
    #[serde(default)]
    pub edits: BTreeMap<String, (String, bool)>,
    /// API retries in this turn: only the first is shown (the real CLI retries
    /// an authentication failure ten times over three minutes).
    #[serde(default)]
    pub retries: u32,
    /// More about the calls in `tools`: what was asked (a fingerprint) and
    /// which helper asked. A separate map, so a state written before it
    /// existed still loads.
    #[serde(default)]
    pub open: BTreeMap<String, OpenTool>,
    /// Calls a person or a rule said no to, whose result has not arrived yet.
    #[serde(default)]
    pub denials: Vec<Denial>,
    /// When the thinking block being streamed began (the host's clock).
    #[serde(default)]
    pub think_start: Option<u64>,
    /// How long each finished thinking block of the streamed message took, in order.
    #[serde(default)]
    pub thought_ms: Vec<u64>,
    /// The host's clock for the line being translated (milliseconds): the
    /// translator stays pure, so the host sets it before [`TState::line`].
    #[serde(skip)]
    pub now_ms: u64,
}

/// A call that was announced and has no result yet.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OpenTool {
    /// [`fingerprint`] of the call's input.
    #[serde(default)]
    pub input: String,
    /// The helper the call belongs to (its parent tool id).
    #[serde(default)]
    pub parent: Option<String>,
}

/// A refusal waiting for the call it is about (see [`TState::deny`]).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Denial {
    /// The call's id, when the hook that asked named it.
    #[serde(default)]
    pub tool_id: Option<String>,
    pub name: String,
    /// [`fingerprint`] of the input the hook showed.
    pub input: String,
}

/// A short, stable name for a JSON value: equal values (whatever the order
/// of their keys) have the same one. It ties a hook's `tool_input` to the
/// call the stream announced with the same input.
pub fn fingerprint(v: &Value) -> String {
    fn walk(v: &Value, out: &mut String, depth: usize) {
        match v {
            Value::Object(o) if depth < 32 => {
                let mut keys: Vec<&String> = o.keys().collect();
                keys.sort();
                out.push('{');
                for k in keys {
                    out.push_str(&Value::String(k.clone()).to_string());
                    out.push(':');
                    walk(&o[k], out, depth + 1);
                    out.push(',');
                }
                out.push('}');
            }
            Value::Array(a) if depth < 32 => {
                out.push('[');
                for x in a {
                    walk(x, out, depth + 1);
                    out.push(',');
                }
                out.push(']');
            }
            x => out.push_str(&x.to_string()),
        }
    }
    use sha2::Digest;
    let mut canon = String::new();
    walk(v, &mut canon, 0);
    hex::encode(&sha2::Sha256::digest(canon.as_bytes())[..12])
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}

/// Cut a string to `max` bytes on a char boundary (keeping the head).
pub fn cut(t: &str, max: usize) -> String {
    if t.len() <= max {
        return t.to_string();
    }
    let mut end = max;
    while !t.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &t[..end])
}

/// The last `max` bytes (tool output: the end is what matters).
pub fn tail(t: &str, max: usize) -> String {
    if t.len() <= max {
        return t.to_string();
    }
    let mut start = t.len() - max;
    while !t.is_char_boundary(start) {
        start += 1;
    }
    format!("…{}", &t[start..])
}

pub(super) fn one_line(t: &str, max: usize) -> String {
    let first = t.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
    let more = t.trim().lines().count() > 1;
    let mut out = cut(first, max);
    if more && !out.ends_with('…') {
        out.push('…');
    }
    out
}

/// Long strings inside a tool input are cut (the card shows "Show all"
/// from the terminal; the chat is not a log viewer).
pub(super) fn cut_input(v: &Value, depth: usize) -> Value {
    match v {
        Value::String(t) => Value::String(cut(t, 4096)),
        Value::Array(a) if depth < 8 => Value::Array(a.iter().take(200).map(|x| cut_input(x, depth + 1)).collect()),
        Value::Object(o) if depth < 8 => {
            Value::Object(o.iter().map(|(k, x)| (k.clone(), cut_input(x, depth + 1))).collect())
        }
        Value::Array(_) | Value::Object(_) => Value::String("…".into()),
        x => x.clone(),
    }
}

/// The first `max` characters.
fn cut_chars(t: &str, max: usize) -> String {
    match t.char_indices().nth(max) {
        Some((end, _)) => format!("{}…", &t[..end]),
        None => t.to_string(),
    }
}

/// The object of a call as one line: no control sequences, a line break
/// shown as `⏎`, at most [`MAX_TOOL_SUBJECT`] characters. `None` when empty.
pub(super) fn subject_line(t: &str) -> Option<String> {
    let clean = strip_controls(t);
    let line = clean.trim().lines().map(str::trim_end).collect::<Vec<_>>().join("⏎");
    Some(cut_chars(&line, MAX_TOOL_SUBJECT)).filter(|l| !l.is_empty())
}

/// What a tool call is (docs/design/chat-tools.md §3, §14).
#[derive(Debug, Clone, PartialEq)]
pub struct ToolFacts {
    /// The ACP kind (one of nine).
    pub kind: &'static str,
    /// A whole line in the past tense, for a reader that knows no `subject`.
    pub title: String,
    /// The tool's own name ("Bash"; for an MCP tool, without its server).
    pub name: String,
    /// The MCP server.
    pub server: Option<String>,
    /// The object of the call: the reader writes the verb, in the right tense.
    pub subject: Option<String>,
    /// A refinement of `kind`.
    pub variant: Option<&'static str>,
}

/// The facts of a tool call. `created`: a write to a path that does not exist yet.
pub fn tool_facts(name: &str, input: &Value, created: bool) -> ToolFacts {
    let path = || {
        let p = s(input, "file_path");
        if p.is_empty() {
            s(input, "notebook_path").to_string()
        } else {
            p.to_string()
        }
    };
    let mut server = None;
    let mut own = name;
    // kind, variant, title, subject
    let (kind, variant, title, subject): (&'static str, Option<&'static str>, String, String) = match name {
        "Bash" => {
            let c = s(input, "command");
            ("execute", None, format!("Ran `{}`", one_line(c, 80)), c.to_string())
        }
        "Read" => ("read", None, format!("Read `{}`", path()), path()),
        "Edit" | "MultiEdit" | "NotebookEdit" => ("edit", None, format!("Edited `{}`", path()), path()),
        "Write" => (
            "edit",
            created.then_some("create"),
            format!("Wrote `{}`", path()),
            path(),
        ),
        "Grep" => {
            let p = s(input, "pattern");
            (
                "search",
                None,
                format!("Searched for `{}`", one_line(p, 60)),
                p.to_string(),
            )
        }
        "Glob" => {
            let p = s(input, "pattern");
            (
                "search",
                Some("glob"),
                format!("Looked for `{}`", one_line(p, 60)),
                p.to_string(),
            )
        }
        "LS" => {
            let p = s(input, "path");
            ("read", Some("list"), format!("Listed `{p}`"), p.to_string())
        }
        "WebFetch" => {
            let u = s(input, "url");
            ("fetch", None, format!("Fetched {}", one_line(u, 80)), u.to_string())
        }
        "WebSearch" => {
            let q = s(input, "query");
            (
                "fetch",
                Some("web_search"),
                format!("Searched the web for “{}”", one_line(q, 60)),
                q.to_string(),
            )
        }
        "Task" | "Agent" => {
            let d = s(input, "description");
            (
                "other",
                Some("agent"),
                format!("Started a helper: {}", one_line(d, 60)),
                d.to_string(),
            )
        }
        "TodoWrite" => ("think", Some("plan"), "Updated the plan".into(), String::new()),
        "AskUserQuestion" => ("other", Some("question"), "Asked a question".into(), String::new()),
        "ExitPlanMode" => ("think", Some("plan"), "Proposed a plan".into(), String::new()),
        n if n.starts_with("mcp__") => {
            let mut parts = n.splitn(3, "__").skip(1);
            let srv = parts.next().unwrap_or("");
            let tool = parts.next().unwrap_or("");
            server = Some(srv).filter(|x| !x.is_empty());
            if !tool.is_empty() {
                own = tool;
            }
            ("other", Some("mcp"), format!("Used {srv} · {tool}"), String::new())
        }
        n => ("other", None, format!("Used {n}"), String::new()),
    };
    let short = |t: &str| cut_chars(strip_controls(t).trim(), MAX_TOOL_NAME);
    ToolFacts {
        kind,
        title: strip_controls(&title).into_owned(),
        name: short(own),
        server: server.map(short),
        subject: subject_line(&subject),
        variant,
    }
}

/// ACP tool kind and the one-line title for a tool call.
pub fn tool_kind_title(name: &str, input: &Value) -> (&'static str, String) {
    let f = tool_facts(name, input, false);
    (f.kind, f.title)
}

/// Which end of a long output says the most: the start of what was read,
/// found or fetched; the end of what a command (or anything else) printed.
fn keeps_head(kind: &str) -> bool {
    matches!(kind, "read" | "search" | "fetch")
}

/// What a tool_result holds besides text: each image's type and size (the
/// bytes stay behind).
fn result_attachments(c: &Value) -> Vec<Attachment> {
    let Value::Array(a) = c else { return vec![] };
    a.iter()
        .filter(|b| s(b, "type") == "image")
        .take(MAX_TOOL_ATTACHMENTS)
        .map(|b| {
            let src = &b["source"];
            let data = s(src, "data");
            let pad = data.bytes().rev().take_while(|&x| x == b'=').count();
            let mime = s(src, "media_type");
            Attachment {
                mime: cut_chars(if mime.is_empty() { "image" } else { mime }, 64),
                bytes: (data.len() / 4 * 3 + (data.len() % 4) * 3 / 4).saturating_sub(pad) as u64,
            }
        })
        .collect()
}

/// A count the CLI's own structured result gives (`tool_use_result` beside
/// the tool_result): files for a file search, lines or matches for a search
/// in files. `None` where it gives none.
fn result_count(name: &str, r: &Value) -> Option<u64> {
    let n = |k: &str| r.get(k).and_then(Value::as_u64);
    match name {
        "Glob" => n("numFiles"),
        "Grep" => match s(r, "mode") {
            "content" => n("numLines"),
            "count" => n("numMatches"),
            _ => n("numFiles"),
        },
        _ => None,
    }
}

/// The exit code a failed command's error text starts with ("Exit code 1").
fn exit_code(text: &str) -> Option<i64> {
    let first = text.lines().next()?.trim();
    let rest = first
        .strip_prefix("Error: ")
        .unwrap_or(first)
        .strip_prefix("Exit code ")?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit() || *c == '-').collect();
    digits.parse().ok()
}

/// The text of a tool_result's `content` (a string or text blocks).
fn result_text(c: &Value) -> String {
    match c {
        Value::String(t) => t.clone(),
        Value::Array(a) => a
            .iter()
            .filter_map(|b| match s(b, "type") {
                "text" => Some(s(b, "text").to_string()),
                "image" => Some("[image]".into()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

impl TState {
    fn next_done_id(&mut self, msg: &str) -> String {
        let n = self.done_blocks.entry(msg.to_string()).or_insert(0);
        *n += 1;
        format!("{msg}:{n}")
    }

    /// Translate one stdout line. `exists` says whether a path existed
    /// before a write (created vs modified).
    pub fn line(&mut self, v: &Value, exists: &dyn Fn(&str) -> bool) -> Vec<Emit> {
        let parent = v
            .get("parent_tool_use_id")
            .and_then(Value::as_str)
            .filter(|p| !p.is_empty())
            .map(str::to_owned);
        match s(v, "type") {
            "system" => self.system(v),
            "stream_event" => {
                if parent.is_some() {
                    return vec![]; // a helper's tokens: its messages arrive whole
                }
                self.stream(&v["event"])
            }
            "assistant" => self.assistant(v, parent, exists),
            "user" => self.user(v, parent),
            "result" => self.result(v),
            // Other documented lines this view does not show.
            "rate_limit_event" | "keep_alive" | "control_response" | "control_request" => vec![],
            other => {
                self.unknown += 1;
                vec![top(ChatBody::Notice {
                    code: "unknown_event".into(),
                    text: "The agent did something this version of tmuxedo can't show. Open in terminal to see it."
                        .into(),
                    native: Some(cut(other, 64)),
                })]
            }
        }
    }

    fn system(&mut self, v: &Value) -> Vec<Emit> {
        match s(v, "subtype") {
            "init" => {
                if let Some(id) = v.get("session_id").and_then(Value::as_str) {
                    self.vendor_session = Some(id.to_string());
                }
                let model = v.get("model").and_then(Value::as_str).map(str::to_owned);
                if model.is_some() && model != self.model {
                    self.model = model;
                }
                vec![]
            }
            "api_retry" => {
                self.retries += 1;
                if self.retries > 1 {
                    return vec![];
                }
                let why = v
                    .get("error")
                    .and_then(|e| e.get("type").or(Some(e)))
                    .and_then(Value::as_str)
                    .or_else(|| v.get("error_category").and_then(Value::as_str))
                    .unwrap_or("a temporary error");
                vec![top(ChatBody::Notice {
                    code: "api_retry".into(),
                    text: format!("Retrying after {}.", cut(why, 80)),
                    native: None,
                })]
            }
            "compact_boundary" => vec![top(ChatBody::Notice {
                code: "compacted".into(),
                text: "The agent summarised the conversation so far to make room.".into(),
                native: None,
            })],
            _ => vec![],
        }
    }

    fn stream(&mut self, e: &Value) -> Vec<Emit> {
        match s(e, "type") {
            "message_start" => {
                self.msg = e["message"].get("id").and_then(Value::as_str).map(str::to_owned);
                self.msg_blocks = 0;
                self.think_start = None;
                self.thought_ms.clear();
                vec![]
            }
            "content_block_start" => {
                match s(&e["content_block"], "type") {
                    "text" => self.msg_blocks += 1,
                    "thinking" | "redacted_thinking" => self.think_start = Some(self.now_ms),
                    _ => {}
                }
                vec![]
            }
            "content_block_stop" => {
                if let Some(start) = self.think_start.take() {
                    if self.thought_ms.len() < 64 {
                        self.thought_ms.push(self.now_ms.saturating_sub(start));
                    }
                }
                vec![]
            }
            "content_block_delta" if s(&e["delta"], "type") == "text_delta" => {
                let Some(msg) = &self.msg else { return vec![] };
                let text = s(&e["delta"], "text");
                if text.is_empty() {
                    return vec![];
                }
                vec![top(ChatBody::MessageDelta {
                    message_id: format!("{msg}:{}", self.msg_blocks.max(1)),
                    text: text.to_string(),
                })]
            }
            _ => vec![],
        }
    }

    fn assistant(&mut self, v: &Value, parent: Option<String>, exists: &dyn Fn(&str) -> bool) -> Vec<Emit> {
        let m = &v["message"];
        let msg = s(m, "id").to_string();
        let mut out = vec![];
        let at = |body: ChatBody| Emit {
            body,
            agent_path: parent.clone(),
        };
        for b in m["content"].as_array().map(Vec::as_slice).unwrap_or(&[]) {
            match s(b, "type") {
                "text" => {
                    let text = s(b, "text");
                    if text.trim().is_empty() {
                        continue;
                    }
                    let id = if msg.is_empty() {
                        format!("m{}", self.done_blocks.len() + 1)
                    } else {
                        self.next_done_id(&msg)
                    };
                    out.push(at(ChatBody::MessageDone {
                        message_id: id,
                        text: text.to_string(),
                    }));
                }
                kind @ ("thinking" | "redacted_thinking") => {
                    // The time of this block, when its start and stop were streamed (the
                    // top-level agent's own message only: a helper's arrive whole).
                    let timed = parent.is_none() && !msg.is_empty() && self.msg.as_deref() == Some(msg.as_str());
                    let duration_ms = (timed && !self.thought_ms.is_empty()).then(|| self.thought_ms.remove(0));
                    let text = s(b, "thinking");
                    if kind == "redacted_thinking" {
                        // The vendor hid it: there are no words to show, only that it happened.
                        out.push(at(ChatBody::ThoughtDone {
                            text: String::new(),
                            duration_ms,
                            redacted: true,
                        }));
                    } else if !text.trim().is_empty() {
                        out.push(at(ChatBody::ThoughtDone {
                            text: strip_controls(text).into_owned(),
                            duration_ms,
                            redacted: false,
                        }));
                    }
                }
                "tool_use" => {
                    let id = s(b, "id").to_string();
                    let name = s(b, "name").to_string();
                    let input = b.get("input").cloned().unwrap_or(Value::Null);
                    self.open.insert(
                        id.clone(),
                        OpenTool {
                            input: fingerprint(&input),
                            parent: parent.clone(),
                        },
                    );
                    out.extend(self.tool_use(&id, &name, &input, exists).into_iter().map(&at));
                }
                _ => {}
            }
        }
        out
    }

    fn tool_use(&mut self, id: &str, name: &str, input: &Value, exists: &dyn Fn(&str) -> bool) -> Vec<ChatBody> {
        self.tools.insert(id.to_string(), name.to_string());
        let path = {
            let p = s(input, "file_path");
            if p.is_empty() {
                s(input, "notebook_path")
            } else {
                p
            }
        };
        let created = name == "Write" && !path.is_empty() && !exists(path);
        let facts = tool_facts(name, input, created);
        let mut locations = vec![];
        if !path.is_empty() {
            locations.push(Location {
                path: path.to_string(),
                line: input.get("offset").and_then(Value::as_u64),
            });
        }
        let mut out = vec![ChatBody::ToolStarted {
            tool_id: id.to_string(),
            kind: facts.kind.into(),
            title: facts.title,
            input: cut_input(input, 0),
            locations,
            name: Some(facts.name).filter(|n| !n.is_empty()),
            server: facts.server,
            subject: facts.subject,
            variant: facts.variant.map(str::to_owned),
        }];
        let diff = match name {
            "Edit" => Some((s(input, "old_string").to_string(), s(input, "new_string").to_string())),
            "MultiEdit" => {
                let edits = input["edits"].as_array().cloned().unwrap_or_default();
                let old = edits
                    .iter()
                    .map(|e| s(e, "old_string"))
                    .collect::<Vec<_>>()
                    .join("\n…\n");
                let new = edits
                    .iter()
                    .map(|e| s(e, "new_string"))
                    .collect::<Vec<_>>()
                    .join("\n…\n");
                Some((old, new))
            }
            "Write" => Some((String::new(), s(input, "content").to_string())),
            _ => None,
        };
        if let (Some((old, new)), false) = (diff, path.is_empty()) {
            out.push(ChatBody::ToolUpdated {
                tool_id: id.to_string(),
                status: "in_progress".into(),
                output: None,
                diff: Some(Diff {
                    path: path.to_string(),
                    old_text: cut(&old, 64 * 1024),
                    new_text: cut(&new, 64 * 1024),
                    // Claude's edit tools give the old and new text, not where in the file.
                    line: None,
                }),
            });
            self.edits.insert(id.to_string(), (path.to_string(), !created));
        }
        if name == "TodoWrite" {
            let entries = input["todos"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or(&[])
                .iter()
                .take(100)
                .map(|t| PlanEntry {
                    text: cut(
                        if s(t, "content").is_empty() {
                            s(t, "activeForm")
                        } else {
                            s(t, "content")
                        },
                        500,
                    ),
                    status: match s(t, "status") {
                        "completed" => "done",
                        "in_progress" => "in_progress",
                        _ => "pending",
                    }
                    .into(),
                })
                .collect();
            out.push(ChatBody::PlanUpdated { entries });
        }
        out
    }

    /// A person or a rule said no to a call (`native` is the hook payload
    /// that asked: `tool_name`, `tool_input` and, where the CLI sends it,
    /// `tool_use_id`). The call's result, when it arrives as an error, is
    /// then `denied` and not `failed`: see [`TState::take_denial`].
    pub fn deny(&mut self, native: &Value) {
        let name = s(native, "tool_name");
        if name.is_empty() || self.denials.len() >= 64 {
            return;
        }
        self.denials.push(Denial {
            tool_id: Some(s(native, "tool_use_id").to_string()).filter(|i| !i.is_empty()),
            name: name.to_string(),
            input: fingerprint(native.get("tool_input").unwrap_or(&Value::Null)),
        });
    }

    /// Whether the call `id` (just closed, with this name and input) is one
    /// that was refused. By the id the hook named, when it named one.
    /// Otherwise by what the stream itself says about the call: its tool and
    /// its exact input, and only while no other open call has the same (two
    /// identical calls in flight cannot be told apart, so neither is called
    /// denied: it stays `failed`, as before).
    fn take_denial(&mut self, id: &str, name: &str, input: Option<&str>) -> bool {
        if self.denials.is_empty() {
            return false;
        }
        let by = self.take_denial_by(id, name, input);
        // Which rule tied a refusal to its call, with a real CLI (no values).
        crate::log!("chat denial: matched={by} pending={}", self.denials.len());
        by != "none"
    }

    /// `"id"`, `"input"` or `"none"`: how [`Self::take_denial`] found the call.
    fn take_denial_by(&mut self, id: &str, name: &str, input: Option<&str>) -> &'static str {
        if let Some(i) = self.denials.iter().position(|d| d.tool_id.as_deref() == Some(id)) {
            self.denials.remove(i);
            return "id";
        }
        let Some(input) = input else { return "none" };
        let Some(i) = self
            .denials
            .iter()
            .position(|d| d.tool_id.is_none() && d.name == name && d.input == input)
        else {
            return "none";
        };
        let twin = self
            .open
            .iter()
            .any(|(other, o)| o.input == input && self.tools.get(other).map(String::as_str) == Some(name));
        if twin {
            return "none";
        }
        self.denials.remove(i);
        "input"
    }

    /// The calls still open, closed as `cancelled`: their turn was
    /// interrupted before they gave a result.
    pub fn cancel_open(&mut self) -> Vec<Emit> {
        let open = std::mem::take(&mut self.open);
        self.edits.clear();
        std::mem::take(&mut self.tools)
            .into_keys()
            .map(|id| Emit {
                agent_path: open.get(&id).and_then(|o| o.parent.clone()),
                body: ChatBody::ToolDone {
                    tool_id: id,
                    status: "cancelled".into(),
                    summary: None,
                    output: None,
                    exit_code: None,
                    duration_ms: None,
                    output_bytes: None,
                    output_kept: None,
                    attachments: vec![],
                    result_count: None,
                },
            })
            .collect()
    }

    fn user(&mut self, v: &Value, parent: Option<String>) -> Vec<Emit> {
        let mut out = vec![];
        let blocks = v["message"]["content"].as_array().map(Vec::as_slice).unwrap_or(&[]);
        // The CLI's structured twin of the result (one per line): trusted only when the line has one result.
        let one = blocks.iter().filter(|b| s(b, "type") == "tool_result").count() == 1;
        let structured = v.get("tool_use_result").filter(|r| one && r.is_object());
        for b in blocks {
            if s(b, "type") != "tool_result" {
                continue;
            }
            let id = s(b, "tool_use_id").to_string();
            let name = self.tools.remove(&id).unwrap_or_default();
            let open = self.open.remove(&id);
            let kind = tool_facts(&name, &Value::Null, false).kind;
            let raw = result_text(&b["content"]);
            // Control sequences go before anything is cut or masked (docs/design/chat-tools.md §15).
            let text = strip_controls(&raw);
            let failed = b.get("is_error").and_then(Value::as_bool).unwrap_or(false);
            let denied = failed && self.take_denial(&id, &name, open.as_ref().map(|o| o.input.as_str()));
            let status = if denied {
                "denied"
            } else if failed {
                "failed"
            } else {
                "completed"
            };
            let summary = failed.then(|| one_line(&text, 120)).filter(|t| !t.is_empty());
            let edit = self.edits.remove(&id);
            let (output, kept) = if text.len() <= MAX_TOOL_OUTPUT {
                (text.to_string(), None)
            } else if keeps_head(kind) {
                (cut(&text, MAX_TOOL_OUTPUT), Some("head"))
            } else {
                (tail(&text, MAX_TOOL_OUTPUT), Some("tail"))
            };
            out.push(Emit {
                body: ChatBody::ToolDone {
                    tool_id: id.clone(),
                    status: status.into(),
                    summary,
                    output: Some(output).filter(|t| !t.is_empty()),
                    exit_code: (failed && !denied && name == "Bash")
                        .then(|| exit_code(&text))
                        .flatten(),
                    duration_ms: structured.and_then(|r| r.get("durationMs")).and_then(Value::as_u64),
                    output_bytes: Some(raw.len() as u64).filter(|n| *n > 0),
                    output_kept: kept.map(str::to_owned),
                    attachments: result_attachments(&b["content"]),
                    result_count: structured.and_then(|r| result_count(&name, r)),
                },
                agent_path: parent.clone(),
            });
            if let (Some((path, existed)), false) = (edit, failed) {
                // The tool's own words win over our look before it ran (it may have run already).
                let lower = text.to_ascii_lowercase();
                let created = if lower.contains("created") {
                    true
                } else if lower.contains("updated") || lower.contains("modified") || lower.contains("edited") {
                    false
                } else {
                    !existed
                };
                out.push(Emit {
                    body: ChatBody::FilesChanged {
                        files: vec![FileChange {
                            path,
                            change: if created { "created" } else { "modified" }.into(),
                            source: "tool".into(),
                            tool_id: Some(id),
                        }],
                    },
                    agent_path: parent.clone(),
                });
            }
        }
        out
    }

    fn result(&mut self, v: &Value) -> Vec<Emit> {
        let is_error = v.get("is_error").and_then(Value::as_bool).unwrap_or(false);
        let sub = s(v, "subtype");
        let status = if self.interrupting {
            "interrupted"
        } else if is_error || sub.starts_with("error") {
            "failed"
        } else {
            "completed"
        };
        self.interrupting = false;
        // A call with no result when its turn was interrupted did not finish: say so, before
        // the turn's own end (every other end of a turn leaves an open call as it was).
        let mut out = if status == "interrupted" {
            self.cancel_open()
        } else {
            vec![]
        };
        self.tools.clear();
        self.open.clear();
        self.denials.clear();
        self.edits.clear();
        self.retries = 0;
        self.msg = None;
        self.think_start = None;
        self.thought_ms.clear();
        let mut usage = Map::new();
        let u = &v["usage"];
        for (k, out) in [
            ("input_tokens", "inputTokens"),
            ("output_tokens", "outputTokens"),
            ("cache_read_input_tokens", "cacheReadTokens"),
            ("cache_creation_input_tokens", "cacheWriteTokens"),
        ] {
            if let Some(n) = u.get(k).and_then(Value::as_u64) {
                usage.insert(out.into(), json!(n));
            }
        }
        if let Some(c) = v.get("total_cost_usd").and_then(Value::as_f64) {
            usage.insert("costUsd".into(), json!(c));
        }
        if let Some(d) = v.get("duration_ms").and_then(Value::as_u64) {
            usage.insert("durationMs".into(), json!(d));
        }
        let error = (status == "failed").then(|| {
            let r = s(v, "result");
            if r.is_empty() {
                sub.to_string()
            } else {
                one_line(r, 200)
            }
        });
        out.push(top(ChatBody::TurnDone {
            status: status.into(),
            error: error.map(|e| strip_controls(&e).into_owned()),
            usage: (!usage.is_empty()).then_some(Value::Object(usage)),
        }));
        out
    }
}

// ── user input ───────────────────────────────────────────────────────────

/// The stdin line for one user message (stream-json input).
pub fn user_line(text: &str, vendor_session: Option<&str>) -> Vec<u8> {
    let mut v = json!({
        "type": "user",
        "message": { "role": "user", "content": [{ "type": "text", "text": text }] },
        "parent_tool_use_id": null,
    });
    if let Some(id) = vendor_session {
        v["session_id"] = json!(id);
    }
    let mut b = serde_json::to_vec(&v).unwrap_or_default();
    b.push(b'\n');
    b
}

// ── risk (D-22 classes) ──────────────────────────────────────────────────

/// High-risk actions get Allow once / Deny only and a fresh step-up.
/// Returns the reason in plain words, or None for normal risk.
pub fn risk(tool: &str, input: &Value, cwd: &str) -> Option<&'static str> {
    crate::risk::classify(tool, input, cwd).map(|r| r.why)
}

// ── needs-you items from held hooks ──────────────────────────────────────

/// The needs-you item for a held hook payload, or None when this hook is
/// not one a chat holds. `id` and `expires_at` come from the host.
pub fn needs_item(native: &Value, id: &str, expires_at: &str, cwd: &str) -> Option<NeedsItem> {
    let event = s(native, "hook_event_name");
    let tool = s(native, "tool_name");
    let input = native.get("tool_input").cloned().unwrap_or(Value::Null);
    let base = |kind: &str, risk: &str, options: &[&str], title: String| NeedsItem {
        id: id.to_string(),
        kind: kind.into(),
        risk: risk.into(),
        options: options.iter().map(|o| o.to_string()).collect(),
        title,
        tool: Some(tool.to_string()).filter(|t| !t.is_empty()),
        input: None,
        why: None,
        questions: vec![],
        markdown: None,
        expires_at: expires_at.to_string(),
    };
    match (event, tool) {
        ("PreToolUse", "AskUserQuestion") => {
            let questions: Vec<Question> = input["questions"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or(&[])
                .iter()
                .take(4)
                .map(|q| Question {
                    text: cut(s(q, "question"), 2000),
                    header: cut(s(q, "header"), 40),
                    multi: q.get("multiSelect").and_then(Value::as_bool).unwrap_or(false),
                    options: q["options"]
                        .as_array()
                        .map(Vec::as_slice)
                        .unwrap_or(&[])
                        .iter()
                        .take(8)
                        .map(|o| QuestionOption {
                            label: cut(s(o, "label"), 200),
                            description: cut(s(o, "description"), 1000),
                            preview: o.get("preview").and_then(Value::as_str).map(|p| cut(p, 8192)),
                        })
                        .collect(),
                })
                .collect();
            if questions.is_empty() {
                return None;
            }
            let n = questions.len();
            let mut it = base(
                "question",
                "normal",
                &["answers", "deny"],
                if n == 1 {
                    "Answer a question".into()
                } else {
                    format!("Answer {n} questions")
                },
            );
            it.questions = questions;
            Some(it)
        }
        ("PermissionRequest", "ExitPlanMode") => {
            let mut it = base(
                "plan",
                "normal",
                &["approve_plan", "keep_planning"],
                "Approve the plan".into(),
            );
            it.markdown = Some(cut(s(&input, "plan"), 128 * 1024));
            Some(it)
        }
        ("PermissionRequest", _) if !tool.is_empty() => {
            let why = risk(tool, &input, cwd);
            let suggestions = native
                .get("permission_suggestions")
                .and_then(Value::as_array)
                .is_some_and(|a| !a.is_empty());
            let options: &[&str] = match (why, suggestions) {
                (Some(_), _) => &["allow_once", "deny"],
                (None, true) => &["allow_once", "allow_session", "deny"],
                (None, false) => &["allow_once", "deny"],
            };
            let (_, title) = tool_kind_title(tool, &input);
            let title = match tool {
                "Bash" => format!("Run `{}`", one_line(s(&input, "command"), 80)),
                "Edit" | "MultiEdit" | "NotebookEdit" => format!("Edit `{}`", s(&input, "file_path")),
                "Write" => format!("Write `{}`", s(&input, "file_path")),
                "Read" => format!("Read `{}`", s(&input, "file_path")),
                _ => title,
            };
            let mut it = base(
                "permission",
                if why.is_some() { "high" } else { "normal" },
                options,
                title,
            );
            it.input = Some(cut_input(&input, 0));
            it.why = why.map(str::to_owned);
            Some(it)
        }
        _ => None,
    }
}

/// A person's answer (`chat.respond` `response`).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct Response {
    pub kind: String,
    #[serde(default)]
    pub message: Option<String>,
    /// Question text (or its index, "0".."3") → the chosen label, the
    /// labels (multi-select), or free text ("Other…").
    #[serde(default)]
    pub answers: Option<Map<String, Value>>,
}

/// The hook's stdout for an answer to `item` (whose held payload was
/// `native`), or the reason the answer does not fit the item.
pub fn reply(item: &NeedsItem, native: &Value, r: &Response) -> Result<Value, (&'static str, String)> {
    if !item.options.contains(&r.kind) {
        return Err((
            "invalid_response",
            format!("{} is not an answer to this item ({})", r.kind, item.options.join(", ")),
        ));
    }
    let msg = r
        .message
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .map(|m| cut(m, 4000));
    let permission = |decision: Value| json!({ "hookSpecificOutput": { "hookEventName": "PermissionRequest", "decision": decision } });
    match r.kind.as_str() {
        "allow_once" | "approve_plan" => Ok(permission(json!({ "behavior": "allow" }))),
        "allow_session" => {
            // The CLI's own suggestions, scoped to this session only (never a saved rule).
            let rules: Vec<Value> = native["permission_suggestions"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(|mut sug| {
                    if let Some(o) = sug.as_object_mut() {
                        o.insert("destination".into(), json!("session"));
                    }
                    sug
                })
                .collect();
            Ok(permission(json!({ "behavior": "allow", "updatedPermissions": rules })))
        }
        "deny" if item.kind == "question" => Ok(json!({ "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": msg.unwrap_or_else(|| "The user skipped the question.".into()),
        }})),
        "deny" => Ok(permission(
            json!({ "behavior": "deny", "message": msg.unwrap_or_else(|| "The user denied this.".into()) }),
        )),
        "keep_planning" => Ok(permission(json!({
            "behavior": "deny",
            "message": match msg { Some(m) => format!("Keep planning: {m}"), None => "Keep planning; the user did not approve the plan yet.".into() },
        }))),
        "answers" => {
            let given = r.answers.clone().unwrap_or_default();
            let questions = native["tool_input"]["questions"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            let mut answers = Map::new();
            for (i, q) in item.questions.iter().enumerate() {
                let raw_text = questions
                    .get(i)
                    .map(|x| s(x, "question").to_string())
                    .unwrap_or_else(|| q.text.clone());
                let a = given.get(&q.text).or_else(|| given.get(&i.to_string()));
                let value = match a {
                    Some(Value::String(t)) if !t.trim().is_empty() => cut(t.trim(), 2000),
                    Some(Value::Array(list)) if q.multi && !list.is_empty() => list
                        .iter()
                        .filter_map(Value::as_str)
                        .map(|t| cut(t.trim(), 2000))
                        .filter(|t| !t.is_empty())
                        .collect::<Vec<_>>()
                        .join(", "),
                    _ => return Err(("invalid_response", format!("question {} has no answer", i + 1))),
                };
                if value.is_empty() {
                    return Err(("invalid_response", format!("question {} has no answer", i + 1)));
                }
                answers.insert(raw_text, Value::String(value));
            }
            let mut updated = native["tool_input"].clone();
            if !updated.is_object() {
                updated = json!({});
            }
            updated["answers"] = Value::Object(answers);
            Ok(json!({ "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "allow",
                "updatedInput": updated,
            }}))
        }
        k => Err(("invalid_response", format!("unknown answer {k}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no(_: &str) -> bool {
        false
    }

    #[test]
    fn text_deltas_and_done_share_ids() {
        let mut t = TState::default();
        assert!(t
            .line(
                &json!({"type":"system","subtype":"init","session_id":"S","model":"opus"}),
                &no
            )
            .is_empty());
        assert_eq!(t.vendor_session.as_deref(), Some("S"));
        t.line(
            &json!({"type":"stream_event","event":{"type":"message_start","message":{"id":"msg1"}}}),
            &no,
        );
        t.line(&json!({"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text"}}}), &no);
        let d = t.line(&json!({"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"Hel"}}}), &no);
        assert_eq!(
            d[0].body,
            ChatBody::MessageDelta {
                message_id: "msg1:1".into(),
                text: "Hel".into()
            }
        );
        let done = t.line(
            &json!({"type":"assistant","message":{"id":"msg1","content":[{"type":"text","text":"Hello"}]}}),
            &no,
        );
        assert_eq!(
            done[0].body,
            ChatBody::MessageDone {
                message_id: "msg1:1".into(),
                text: "Hello".into()
            }
        );
    }

    fn started(e: &[Emit]) -> Value {
        serde_json::to_value(&e[0].body).unwrap()
    }

    fn use_tool(t: &mut TState, id: &str, name: &str, input: Value) -> Vec<Emit> {
        t.line(
            &json!({"type":"assistant","message":{"id":format!("m-{id}"),"content":[{"type":"tool_use","id":id,"name":name,"input":input}]}}),
            &no,
        )
    }

    fn result_of(t: &mut TState, id: &str, content: Value, error: bool) -> Value {
        let e = t.line(
            &json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":id,"content":content,"is_error":error}]}}),
            &no,
        );
        serde_json::to_value(&e[0].body).unwrap()
    }

    /// One line per kind of call: the name, the object and the refinement a
    /// reader builds its own line from, and the old title beside them.
    #[test]
    fn a_call_says_what_it_is() {
        let mut t = TState::default();
        let mut facts = |name: &str, input: Value| {
            let v = started(&use_tool(&mut t, "x", name, input));
            assert!(
                v["title"].as_str().is_some_and(|x| !x.is_empty()),
                "the old title stays: {v}"
            );
            (
                v["kind"].as_str().unwrap().to_string(),
                v["name"].as_str().map(str::to_owned),
                v["subject"].as_str().map(str::to_owned),
                v["variant"].as_str().map(str::to_owned),
                v["server"].as_str().map(str::to_owned),
            )
        };
        let f = |k: &str, n: &str, sub: Option<&str>, var: Option<&str>| {
            (
                k.to_string(),
                Some(n.to_string()),
                sub.map(str::to_owned),
                var.map(str::to_owned),
                None::<String>,
            )
        };
        assert_eq!(
            facts("Bash", json!({"command":"npm test -- auth"})),
            f("execute", "Bash", Some("npm test -- auth"), None)
        );
        assert_eq!(
            facts("Read", json!({"file_path":"/w/src/auth.ts"})),
            f("read", "Read", Some("/w/src/auth.ts"), None)
        );
        assert_eq!(
            facts("LS", json!({"path":"/w/src"})),
            f("read", "LS", Some("/w/src"), Some("list"))
        );
        assert_eq!(
            facts("Edit", json!({"file_path":"/w/a.ts","old_string":"a","new_string":"b"})),
            f("edit", "Edit", Some("/w/a.ts"), None)
        );
        assert_eq!(
            facts("NotebookEdit", json!({"notebook_path":"/w/n.ipynb"})),
            f("edit", "NotebookEdit", Some("/w/n.ipynb"), None)
        );
        // `no`: the path does not exist yet, so the write creates it.
        assert_eq!(
            facts("Write", json!({"file_path":"/w/docs/plan.md","content":"x"})),
            f("edit", "Write", Some("/w/docs/plan.md"), Some("create"))
        );
        assert_eq!(
            facts("Grep", json!({"pattern":"useAuth","path":"src"})),
            f("search", "Grep", Some("useAuth"), None)
        );
        assert_eq!(
            facts("Glob", json!({"pattern":"**/*.test.ts"})),
            f("search", "Glob", Some("**/*.test.ts"), Some("glob"))
        );
        assert_eq!(
            facts("WebFetch", json!({"url":"https://example.com/docs/api"})),
            f("fetch", "WebFetch", Some("https://example.com/docs/api"), None)
        );
        assert_eq!(
            facts("WebSearch", json!({"query":"expo push limits"})),
            f("fetch", "WebSearch", Some("expo push limits"), Some("web_search"))
        );
        assert_eq!(
            facts("Task", json!({"description":"check the auth changes","prompt":"…"})),
            f("other", "Task", Some("check the auth changes"), Some("agent"))
        );
        assert_eq!(
            facts("TodoWrite", json!({"todos":[]})),
            f("think", "TodoWrite", None, Some("plan"))
        );
        assert_eq!(
            facts("ExitPlanMode", json!({"plan":"x"})),
            f("think", "ExitPlanMode", None, Some("plan"))
        );
        assert_eq!(
            facts("AskUserQuestion", json!({"questions":[]})),
            f("other", "AskUserQuestion", None, Some("question"))
        );
        assert_eq!(facts("Skill", json!({"skill":"x"})), f("other", "Skill", None, None));
        // An MCP tool: its own name and its server apart.
        assert_eq!(
            facts("mcp__tracker__create_issue", json!({"title":"Fix login"})),
            (
                "other".into(),
                Some("create_issue".into()),
                None,
                Some("mcp".into()),
                Some("tracker".into())
            )
        );
        // A write to a file that exists is an edit, not a creation.
        let yes = |_: &str| true;
        let e = TState::default().line(
            &json!({"type":"assistant","message":{"id":"m","content":[{"type":"tool_use","id":"w","name":"Write","input":{"file_path":"/w/a.md","content":"x"}}]}}),
            &yes,
        );
        assert!(started(&e).get("variant").is_none());
    }

    /// The object is one line, without control sequences, of a bounded length.
    #[test]
    fn a_subject_is_one_clean_line() {
        let mut t = TState::default();
        let v = started(&use_tool(
            &mut t,
            "a",
            "Bash",
            json!({"command":"  echo one \\\n  && \u{1b}[31mecho two\u{1b}[0m\r\necho ✓ done\u{7}\n"}),
        ));
        assert_eq!(v["subject"], "echo one \\⏎  && echo two⏎echo ✓ done");
        assert_eq!(v["title"], "Ran `echo one \\…`");
        let long = "é".repeat(1000);
        let v = started(&use_tool(&mut t, "b", "Bash", json!({ "command": long })));
        let sub = v["subject"].as_str().unwrap();
        assert_eq!(
            sub.chars().count(),
            MAX_TOOL_SUBJECT + 1,
            "cut, with the mark that it was"
        );
        assert!(sub.ends_with('…'));
        // Nothing to name: no subject at all (never an empty one).
        let v = started(&use_tool(&mut t, "c", "Bash", json!({"command":" \n "})));
        assert!(v.get("subject").is_none());
        // A tool name is not trusted to be short or clean either.
        let v = started(&use_tool(
            &mut t,
            "d",
            &format!("x\u{1b}[2J{}", "n".repeat(200)),
            json!({}),
        ));
        assert_eq!(v["name"].as_str().unwrap().chars().count(), MAX_TOOL_NAME + 1);
        assert!(!v.to_string().contains("\\u001b"), "{v}");
    }

    /// The start of what was read, found or fetched; the end of what a command printed.
    #[test]
    fn long_output_keeps_the_end_that_matters() {
        let big = format!("HEAD{}TAIL", "é".repeat(MAX_TOOL_OUTPUT));
        for (name, kept) in [
            ("Read", "head"),
            ("LS", "head"),
            ("Grep", "head"),
            ("Glob", "head"),
            ("WebFetch", "head"),
            ("WebSearch", "head"),
            ("Bash", "tail"),
            ("Edit", "tail"),
            ("Task", "tail"),
            ("mcp__s__t", "tail"),
            ("Unknown", "tail"),
        ] {
            let mut t = TState::default();
            use_tool(&mut t, "a", name, json!({}));
            let v = result_of(&mut t, "a", json!(big), false);
            let out = v["output"].as_str().unwrap();
            assert_eq!(v["outputKept"], kept, "{name}");
            assert_eq!(v["outputBytes"], big.len(), "{name}: the size of the whole output");
            assert!(out.len() <= MAX_TOOL_OUTPUT + 4, "{name}: {}", out.len());
            if kept == "head" {
                assert!(
                    out.starts_with("HEAD") && out.ends_with('…') && !out.contains("TAIL"),
                    "{name}"
                );
            } else {
                assert!(
                    out.ends_with("TAIL") && out.starts_with('…') && !out.contains("HEAD"),
                    "{name}"
                );
            }
        }
        // Output that fits is whole and says so by saying nothing.
        let mut t = TState::default();
        use_tool(&mut t, "a", "Bash", json!({"command":"echo hi"}));
        let v = result_of(&mut t, "a", json!("hi\n"), false);
        assert_eq!(v["output"], "hi\n");
        assert_eq!(v["outputBytes"], 3);
        assert!(v.get("outputKept").is_none());
        // No output: no size either.
        use_tool(&mut t, "b", "Bash", json!({"command":"true"}));
        let v = result_of(&mut t, "b", json!(""), false);
        assert_eq!(v, json!({"type":"tool.done","toolId":"b","status":"completed"}));
    }

    #[test]
    fn output_loses_its_control_sequences_before_it_is_cut() {
        let mut t = TState::default();
        use_tool(&mut t, "a", "Bash", json!({"command":"npm test"}));
        let v = result_of(
            &mut t,
            "a",
            json!("Exit code 1\n\u{1b}[31mFAIL\u{1b}[0m a.test.ts\r\n\u{1b}]0;✓ done\u{7}50%\r100%"),
            true,
        );
        assert_eq!(v["output"], "Exit code 1\nFAIL a.test.ts\n50%\n100%");
        assert_eq!(v["summary"], "Exit code 1…");
        assert_eq!(v["status"], "failed");
        assert_eq!(v["exitCode"], 1);
        // A sequence that would straddle the cut is gone before the cut is made.
        let noisy = format!("{}END", "\u{1b}[1mx\u{1b}[0m".repeat(MAX_TOOL_OUTPUT));
        use_tool(&mut t, "b", "Bash", json!({"command":"x"}));
        let v = result_of(&mut t, "b", json!(noisy), false);
        let out = v["output"].as_str().unwrap();
        assert!(
            out.ends_with("xxxEND") && !out.contains('\u{1b}') && !out.contains('['),
            "{}",
            &out[out.len() - 20..]
        );
        assert_eq!(v["outputBytes"], noisy.len());
    }

    #[test]
    fn facts_of_a_result() {
        // The exit code is only in a failed command's own words.
        for (text, code) in [
            ("Exit code 1\nboom", Some(1)),
            ("Exit code 127", Some(127)),
            ("Error: Exit code 2\nx", Some(2)),
            ("Exit code -1", Some(-1)),
            ("boom\nExit code 1", None),
            ("Exit code", None),
            ("Exit code x", None),
            ("", None),
        ] {
            assert_eq!(exit_code(text), code, "{text:?}");
        }
        let mut t = TState::default();
        // Not from a tool that is not a command, and not from one that worked.
        use_tool(&mut t, "a", "Read", json!({"file_path":"/w/a"}));
        assert!(result_of(&mut t, "a", json!("Exit code 3"), true)
            .get("exitCode")
            .is_none());
        use_tool(&mut t, "b", "Bash", json!({"command":"echo Exit code 3"}));
        assert!(result_of(&mut t, "b", json!("Exit code 3"), false)
            .get("exitCode")
            .is_none());

        // Images: what and how big, never the bytes.
        use_tool(&mut t, "c", "Read", json!({"file_path":"/w/shot.png"}));
        let data = "QUJD".repeat(1000) + "QUI=";
        let v = result_of(
            &mut t,
            "c",
            json!([{"type":"text","text":"a screenshot"},
                   {"type":"image","source":{"type":"base64","media_type":"image/png","data":data}},
                   {"type":"image","source":{"type":"base64","data":"QQ=="}},
                   {"type":"image"}]),
            false,
        );
        assert_eq!(
            v["attachments"],
            json!([{"mime":"image/png","bytes":3002},{"mime":"image","bytes":1},{"mime":"image","bytes":0}])
        );
        assert_eq!(v["output"], "a screenshot\n[image]\n[image]\n[image]");
        assert!(!v.to_string().contains("QUJD"), "no image bytes in the event");
        let many: Vec<Value> = (0..50)
            .map(|_| json!({"type":"image","source":{"data":"QQ=="}}))
            .collect();
        use_tool(&mut t, "d", "Read", json!({}));
        assert_eq!(
            result_of(&mut t, "d", json!(many), false)["attachments"]
                .as_array()
                .unwrap()
                .len(),
            MAX_TOOL_ATTACHMENTS
        );

        // A count and a time, where the CLI's structured result beside the text gives them.
        let mut counted = |name: &str, r: Value| {
            use_tool(&mut t, "e", name, json!({}));
            let e = t.line(
                &json!({"type":"user","tool_use_result":r,"message":{"content":[{"type":"tool_result","tool_use_id":"e","content":"x"}]}}),
                &no,
            );
            serde_json::to_value(&e[0].body).unwrap()
        };
        let v = counted("Glob", json!({"numFiles":14,"durationMs":31,"filenames":[]}));
        assert_eq!(
            (v["resultCount"].as_u64(), v["durationMs"].as_u64()),
            (Some(14), Some(31))
        );
        assert_eq!(
            counted("Grep", json!({"mode":"files_with_matches","numFiles":3}))["resultCount"],
            3
        );
        assert_eq!(
            counted("Grep", json!({"mode":"content","numFiles":3,"numLines":40}))["resultCount"],
            40
        );
        assert_eq!(
            counted("Grep", json!({"mode":"count","numFiles":3,"numMatches":7}))["resultCount"],
            7
        );
        assert!(counted("Bash", json!({"numFiles":3})).get("resultCount").is_none());
        assert!(counted("Glob", json!("Error: no")).get("resultCount").is_none());
        assert!(counted("Glob", json!({"numFiles":"many","durationMs":-4}))
            .get("resultCount")
            .is_none());
    }

    fn hook(name: &str, input: Value, id: Option<&str>) -> Value {
        let mut v = json!({"hook_event_name":"PermissionRequest","tool_name":name,"tool_input":input});
        if let Some(id) = id {
            v["tool_use_id"] = json!(id);
        }
        v
    }

    #[test]
    fn a_refused_call_is_denied_not_failed() {
        // The hook named the call.
        let mut t = TState::default();
        use_tool(&mut t, "a", "Bash", json!({"command":"git push"}));
        use_tool(&mut t, "b", "Bash", json!({"command":"git push"}));
        t.deny(&hook("Bash", json!({"command":"git push"}), Some("b")));
        assert_eq!(result_of(&mut t, "a", json!("boom"), true)["status"], "failed");
        let v = result_of(&mut t, "b", json!("Permission denied: no\nmore"), true);
        assert_eq!(
            (v["status"].as_str(), v["summary"].as_str()),
            (Some("denied"), Some("Permission denied: no…"))
        );
        assert!(v.get("exitCode").is_none());
        assert!(t.denials.is_empty());

        // The hook did not: the stream's own tool and input find the call, key order aside.
        let mut t = TState::default();
        use_tool(&mut t, "a", "Bash", json!({"command":"rm -rf x","description":"clean"}));
        use_tool(&mut t, "b", "Bash", json!({"command":"ls"}));
        t.deny(&hook("Bash", json!({"description":"clean","command":"rm -rf x"}), None));
        assert_eq!(result_of(&mut t, "b", json!("no such dir"), true)["status"], "failed");
        assert_eq!(result_of(&mut t, "a", json!("denied"), true)["status"], "denied");

        // The refusal can arrive before the call is announced.
        let mut t = TState::default();
        t.deny(&hook("Write", json!({"file_path":"/etc/hosts","content":"x"}), None));
        use_tool(&mut t, "a", "Write", json!({"file_path":"/etc/hosts","content":"x"}));
        let e = t.line(
            &json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"a","content":"blocked","is_error":true}]}}),
            &no,
        );
        assert_eq!(serde_json::to_value(&e[0].body).unwrap()["status"], "denied");
        assert_eq!(e.len(), 1, "a refused edit changed no file");

        // Another tool, another input, or a result that is not an error: not this call.
        let mut t = TState::default();
        use_tool(&mut t, "a", "Bash", json!({"command":"ls"}));
        t.deny(&hook("Bash", json!({"command":"ls -la"}), None));
        t.deny(&hook("Read", json!({"command":"ls"}), None));
        assert_eq!(result_of(&mut t, "a", json!("x"), true)["status"], "failed");
        use_tool(&mut t, "b", "Bash", json!({"command":"ls -la"}));
        assert_eq!(result_of(&mut t, "b", json!("x"), false)["status"], "completed");

        // Two identical calls in flight and no id: which one was refused is not known, so
        // neither is called denied.
        let mut t = TState::default();
        use_tool(&mut t, "a", "Bash", json!({"command":"make"}));
        use_tool(&mut t, "b", "Bash", json!({"command":"make"}));
        t.deny(&hook("Bash", json!({"command":"make"}), None));
        assert_eq!(result_of(&mut t, "a", json!("x"), true)["status"], "failed");
        // Once its twin is closed there is one call it can be.
        assert_eq!(result_of(&mut t, "b", json!("x"), true)["status"], "denied");

        // A refusal does not outlive its turn, and the list is bounded.
        let mut t = TState::default();
        for _ in 0..200 {
            t.deny(&hook("Bash", json!({"command":"x"}), None));
        }
        assert_eq!(t.denials.len(), 64);
        t.line(&json!({"type":"result","subtype":"success"}), &no);
        assert!(t.denials.is_empty() && t.open.is_empty());
        t.deny(&json!({"tool_input":{}}));
        assert!(t.denials.is_empty(), "a payload that names no tool refuses nothing");
    }

    #[test]
    fn calls_open_at_an_interrupt_are_cancelled() {
        let mut t = TState::default();
        use_tool(&mut t, "a", "Bash", json!({"command":"sleep 60"}));
        use_tool(&mut t, "b", "Read", json!({"file_path":"/w/a"}));
        // A helper's call keeps its place under the helper.
        t.line(
            &json!({"type":"assistant","parent_tool_use_id":"a","message":{"id":"h","content":[{"type":"tool_use","id":"c","name":"Edit","input":{"file_path":"/w/a","old_string":"a","new_string":"b"}}]}}),
            &no,
        );
        result_of(&mut t, "b", json!("x"), false);
        t.interrupting = true;
        let e = t.line(
            &json!({"type":"result","subtype":"error_during_execution","is_error":true}),
            &no,
        );
        let v: Vec<Value> = e.iter().map(|x| serde_json::to_value(&x.body).unwrap()).collect();
        assert_eq!(
            v,
            [
                json!({"type":"tool.done","toolId":"a","status":"cancelled"}),
                json!({"type":"tool.done","toolId":"c","status":"cancelled"}),
                json!({"type":"turn.done","status":"interrupted"}),
            ]
        );
        assert_eq!(
            e.iter().map(|x| x.agent_path.as_deref()).collect::<Vec<_>>(),
            [None, Some("a"), None]
        );
        assert!(t.tools.is_empty() && t.open.is_empty() && t.edits.is_empty());
        // A turn that ends any other way leaves an open call as it was: nothing is invented.
        for (line, status) in [
            (json!({"type":"result","subtype":"success"}), "completed"),
            (
                json!({"type":"result","subtype":"error_max_turns","is_error":true}),
                "failed",
            ),
        ] {
            use_tool(&mut t, "z", "Bash", json!({"command":"x"}));
            let e = t.line(&line, &no);
            assert_eq!(e.len(), 1);
            assert_eq!(serde_json::to_value(&e[0].body).unwrap()["status"], status);
            assert!(t.tools.is_empty());
        }
        // The host's own end of a turn (the agent exited): the same events.
        use_tool(&mut t, "q", "Bash", json!({"command":"x"}));
        assert_eq!(t.cancel_open().len(), 1);
        assert!(t.cancel_open().is_empty());
    }

    #[test]
    fn thoughts_carry_their_time_and_hidden_ones_say_so() {
        let mut t = TState::default();
        let ev = |e: Value| json!({"type":"stream_event","event":e});
        let at = |t: &mut TState, ms: u64, v: Value| {
            t.now_ms = ms;
            t.line(&v, &no)
        };
        at(&mut t, 1_000, ev(json!({"type":"message_start","message":{"id":"m1"}})));
        at(
            &mut t,
            1_000,
            ev(json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking"}})),
        );
        at(
            &mut t,
            4_000,
            ev(json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hm"}})),
        );
        at(&mut t, 7_200, ev(json!({"type":"content_block_stop","index":0})));
        let e = at(
            &mut t,
            7_300,
            json!({"type":"assistant","message":{"id":"m1","content":[{"type":"thinking","thinking":"Compare\u{1b}[1m both."}]}}),
        );
        assert_eq!(
            serde_json::to_value(&e[0].body).unwrap(),
            json!({"type":"thought.done","text":"Compare both.","durationMs":6200})
        );
        // A hidden thought: no words, the fact and the time.
        at(
            &mut t,
            8_000,
            ev(json!({"type":"content_block_start","index":1,"content_block":{"type":"redacted_thinking"}})),
        );
        at(&mut t, 8_500, ev(json!({"type":"content_block_stop","index":1})));
        // A text block's stop is not a thought's.
        at(
            &mut t,
            8_600,
            ev(json!({"type":"content_block_start","index":2,"content_block":{"type":"text"}})),
        );
        at(&mut t, 9_900, ev(json!({"type":"content_block_stop","index":2})));
        let e = at(
            &mut t,
            9_950,
            json!({"type":"assistant","message":{"id":"m1","content":[{"type":"redacted_thinking","data":"opaque"}]}}),
        );
        assert_eq!(
            serde_json::to_value(&e[0].body).unwrap(),
            json!({"type":"thought.done","text":"","durationMs":500,"redacted":true})
        );
        assert!(t.thought_ms.is_empty());
        // No stream of the block (a helper's thought, an old CLI): the thought without a time.
        let e = at(
            &mut t,
            10_000,
            json!({"type":"assistant","parent_tool_use_id":"toolu_1","message":{"id":"h1","content":[{"type":"thinking","thinking":"helper"}]}}),
        );
        assert_eq!(
            serde_json::to_value(&e[0].body).unwrap(),
            json!({"type":"thought.done","text":"helper"})
        );
        assert_eq!(e[0].agent_path.as_deref(), Some("toolu_1"));
        // A time measured for one message is never given to another's thought.
        at(
            &mut t,
            11_000,
            ev(json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking"}})),
        );
        at(&mut t, 12_000, ev(json!({"type":"content_block_stop","index":0})));
        let e = at(
            &mut t,
            12_100,
            json!({"type":"assistant","message":{"id":"other","content":[{"type":"thinking","thinking":"x"}]}}),
        );
        assert!(serde_json::to_value(&e[0].body).unwrap().get("durationMs").is_none());
        at(
            &mut t,
            13_000,
            ev(json!({"type":"message_start","message":{"id":"m2"}})),
        );
        assert!(t.thought_ms.is_empty() && t.think_start.is_none());
        // A clock that went back gives 0, never a negative or a huge time.
        at(
            &mut t,
            13_000,
            ev(json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking"}})),
        );
        at(&mut t, 12_000, ev(json!({"type":"content_block_stop","index":0})));
        assert_eq!(t.thought_ms, [0]);
    }

    /// A state saved by a runner from before these fields still loads, and
    /// the clock is not part of what is saved.
    #[test]
    fn an_older_saved_state_still_loads() {
        let old = json!({"vendor_session":"S","tools":{"toolu_1":"Bash"},"interrupting":true,"edits":{}});
        let mut t: TState = serde_json::from_value(old).unwrap();
        assert!(t.open.is_empty() && t.denials.is_empty());
        // Its open call is still cancelled, and still called failed when it errors.
        let e = t.line(
            &json!({"type":"result","subtype":"error_during_execution","is_error":true}),
            &no,
        );
        assert_eq!(e.len(), 2);
        let mut t = TState {
            now_ms: 99,
            ..TState::default()
        };
        use_tool(&mut t, "a", "Bash", json!({"command":"x"}));
        let saved = serde_json::to_value(&t).unwrap();
        assert!(saved.get("now_ms").is_none());
        let back: TState = serde_json::from_value(saved).unwrap();
        assert_eq!(back.open, t.open);
    }

    #[test]
    fn fingerprints() {
        assert_eq!(
            fingerprint(&json!({"a":1,"b":{"c":[1,2],"d":"x"}})),
            fingerprint(&json!({"b":{"d":"x","c":[1,2]},"a":1}))
        );
        assert_ne!(fingerprint(&json!({"a":1})), fingerprint(&json!({"a":"1"})));
        assert_ne!(fingerprint(&json!({"a":[1,2]})), fingerprint(&json!({"a":[2,1]})));
        assert_ne!(fingerprint(&json!({"a,b":1})), fingerprint(&json!({"a":1,"b":1})));
        assert_eq!(fingerprint(&Value::Null).len(), 24);
    }

    #[test]
    fn risk_classes() {
        let b = |c: &str| risk("Bash", &json!({ "command": c }), "/w");
        assert_eq!(b("git push origin main"), Some("Pushes to a remote"));
        assert_eq!(b("git -C x push"), Some("Pushes to a remote"));
        assert_eq!(b("npm install --save-dev vitest"), Some("Installs packages"));
        assert_eq!(b("rm -rf ~/old"), Some("Deletes files outside the project"));
        assert_eq!(b("curl https://x"), Some("Uses the network"));
        assert_eq!(b("cat .env"), Some("Reads credentials"));
        assert_eq!(b("npm test"), None);
        assert_eq!(b("rm -rf build"), None);
        assert_eq!(
            risk("Write", &json!({"file_path": "/etc/hosts"}), "/w"),
            Some("Changes files outside the project")
        );
        assert_eq!(risk("Write", &json!({"file_path": "/w/a.md"}), "/w"), None);
        assert_eq!(
            risk("Read", &json!({"file_path": "/w/.env"}), "/w"),
            Some("Reads credentials")
        );
    }

    #[test]
    fn replies_for_each_answer() {
        let native = json!({"hook_event_name":"PermissionRequest","tool_name":"Bash","tool_input":{"command":"npm test"},
            "permission_suggestions":[{"type":"addRules","rules":[{"toolName":"Bash","ruleContent":"npm test"}],"behavior":"allow","destination":"localSettings"}]});
        let it = needs_item(&native, "n1", "t", "/w").unwrap();
        assert_eq!(it.options, ["allow_once", "allow_session", "deny"]);
        assert_eq!(it.title, "Run `npm test`");
        let r = |kind: &str| Response {
            kind: kind.into(),
            ..Default::default()
        };
        let out = reply(&it, &native, &r("allow_session")).unwrap();
        assert_eq!(
            out["hookSpecificOutput"]["decision"]["updatedPermissions"][0]["destination"],
            "session"
        );
        let deny = reply(
            &it,
            &native,
            &Response {
                kind: "deny".into(),
                message: Some("use pnpm".into()),
                answers: None,
            },
        )
        .unwrap();
        assert_eq!(deny["hookSpecificOutput"]["decision"]["message"], "use pnpm");
        assert!(reply(&it, &native, &r("approve_plan")).is_err());
        // High risk: no allow_session.
        let hi = needs_item(&json!({"hook_event_name":"PermissionRequest","tool_name":"Bash","tool_input":{"command":"git push"},"permission_suggestions":[{}]}), "n2", "t", "/w").unwrap();
        assert_eq!((hi.risk.as_str(), hi.options.len()), ("high", 2));
    }
}
