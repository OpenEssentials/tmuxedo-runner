//! `tmuxedo-runner hook [--plugin <id>]`: the agent's command hook
//! (docs/design/plugins.md §5.4, transport `command`). Reads the native hook
//! JSON on stdin, forwards it to the runner's local socket, and for an event
//! the plugin holds (Claude's `PermissionRequest`) waits for a remote
//! decision, printing the plugin's reply template if one arrives. The runner
//! owns the 540 s deadline (`daemon::PROMPT_HOLD`) and answers
//! `{expired: true}`; this side waits a little longer so it never gives up
//! first. On expiry or any failure it prints nothing and exits 0, so the
//! agent's own dialog stays in charge. No `--plugin` means `claude`, so
//! settings files generated before the host existed keep working.

use crate::plugin::{AgentPlugin, Host, DEFAULT_PLUGIN};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

/// Longer than the runner's 540 s hold, within Claude's 600 s hook timeout.
pub const PERMISSION_WAIT: Duration = Duration::from_secs(550);
pub const OTHER_WAIT: Duration = Duration::from_secs(10);

/// The Claude hook (no `--plugin`). Returns what to print on stdout.
pub fn run_hook(stdin: &mut impl Read, socket: Option<PathBuf>, pane_id: Option<String>) -> String {
    run_hook_for(DEFAULT_PLUGIN, stdin, socket, pane_id)
}

/// `hook --plugin <plugin>`.
pub fn run_hook_for(plugin: &str, stdin: &mut impl Read, socket: Option<PathBuf>, pane_id: Option<String>) -> String {
    run_hook_in(plugin, stdin, socket, pane_id, None)
}

/// A hook from a pane (`pane_id`, `TMUXEDO_PANE_ID`) or from a chat child
/// (`chat_id`, `TMUXEDO_CHAT_ID`, docs/design/agent-chat.md §3.7). A chat
/// also holds the events its manifest's `chat.holds` lists (a question),
/// and its answer is the exact hook output the runner built (`output`).
pub fn run_hook_in(
    plugin: &str,
    stdin: &mut impl Read,
    socket: Option<PathBuf>,
    pane_id: Option<String>,
    chat_id: Option<String>,
) -> String {
    let mut input = String::new();
    if stdin.take(4 * 1024 * 1024).read_to_string(&mut input).is_err() {
        return String::new();
    }
    let Ok(event) = serde_json::from_str::<Value>(&input) else {
        return String::new();
    };
    let Some(p) = Host::builtin().get(plugin) else {
        return String::new();
    };
    let chat_hold = chat_id.is_some()
        && p.manifest().chat.as_ref().is_some_and(|c| {
            let name = event["hook_event_name"].as_str().unwrap_or("");
            let tool = event["tool_name"].as_str().unwrap_or("");
            c.holds
                .iter()
                .any(|h| h.event == name && (h.tool.is_empty() || h.tool == tool))
        });
    let hold = p.map_hook(&event).hold || chat_hold;
    let socket = socket.unwrap_or_else(crate::paths::runner_socket);
    let wait = if hold { PERMISSION_WAIT } else { OTHER_WAIT };
    match forward(&socket, plugin, pane_id, chat_id, event, wait) {
        // The runner's exact hook output (a held answer, a secret guard's
        // denial, a PostToolUse output with secrets masked).
        Some(result) => match result.get("output").filter(|o| o.is_object()) {
            Some(out) => out.to_string(),
            None if hold => p.reply(&result),
            None => String::new(),
        },
        None => String::new(),
    }
}

/// The Auto-approve guard (`hook --guard`, a chat's `PreToolUse` on every
/// tool while it runs in the CLI's bypass mode): the runner answers with the
/// hook output (a deny for the "never" list) or nothing (go on). It **fails
/// closed**: no runner, no chat id, junk or a late answer print a deny, since
/// in bypass mode silence would let the call through unchecked.
pub fn run_chat_guard(stdin: &mut impl Read, socket: Option<PathBuf>, chat_id: Option<String>) -> String {
    let mut input = String::new();
    let deny = |why: &str| {
        json!({ "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": why,
        }})
        .to_string()
    };
    if stdin.take(4 * 1024 * 1024).read_to_string(&mut input).is_err() {
        return deny("tmuxedo could not read this tool call, so Auto-approve blocked it.");
    }
    let Ok(event) = serde_json::from_str::<Value>(&input) else {
        return deny("tmuxedo could not read this tool call, so Auto-approve blocked it.");
    };
    let Some(chat_id) = chat_id else {
        return deny("tmuxedo's Auto-approve guard runs only in a chat; this call was blocked.");
    };
    let socket = socket.unwrap_or_else(crate::paths::runner_socket);
    let mut s = match UnixStream::connect(&socket) {
        Ok(s) => s,
        Err(_) => {
            return deny("tmuxedo could not check this action (the runner did not answer), so Auto-approve blocked it.")
        }
    };
    let _ = s.set_read_timeout(Some(OTHER_WAIT));
    let _ = s.set_write_timeout(Some(Duration::from_secs(5)));
    let req = json!({
        "id": "hook",
        "method": "hook.event",
        "params": { "paneId": null, "chatId": chat_id, "event": event, "plugin": DEFAULT_PLUGIN, "guard": true },
    });
    let mut line = serde_json::to_vec(&req).unwrap_or_default();
    line.push(b'\n');
    let mut resp = String::new();
    if s.write_all(&line).is_err() || BufReader::new(s).read_line(&mut resp).is_err() {
        return deny("tmuxedo could not check this action (the runner did not answer), so Auto-approve blocked it.");
    }
    match serde_json::from_str::<Value>(&resp)
        .ok()
        .and_then(|v| v.get("result").cloned())
    {
        Some(r) if r["allow"] == true => String::new(),
        Some(r) => match r.get("output").filter(|o| o.is_object()) {
            Some(out) => out.to_string(),
            None => deny("tmuxedo could not check this action, so Auto-approve blocked it."),
        },
        None => deny("tmuxedo could not check this action, so Auto-approve blocked it."),
    }
}

fn forward(
    socket: &PathBuf,
    plugin: &str,
    pane_id: Option<String>,
    chat_id: Option<String>,
    event: Value,
    wait: Duration,
) -> Option<Value> {
    let mut s = UnixStream::connect(socket).ok()?;
    s.set_read_timeout(Some(wait)).ok()?;
    s.set_write_timeout(Some(Duration::from_secs(5))).ok()?;
    let req = json!({
        "id": "hook",
        "method": "hook.event",
        "params": { "paneId": pane_id, "chatId": chat_id, "event": event, "plugin": plugin },
    });
    let mut line = serde_json::to_vec(&req).ok()?;
    line.push(b'\n');
    s.write_all(&line).ok()?;
    let mut resp = String::new();
    BufReader::new(s).read_line(&mut resp).ok()?;
    let v: Value = serde_json::from_str(&resp).ok()?;
    v.get("result").cloned()
}

/// Claude's PermissionRequest hook output for a decision result, or nothing.
pub fn decision_output(result: &Value) -> String {
    crate::plugin::claude().reply(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decision_shapes() {
        assert_eq!(
            decision_output(&json!({"decision": {"behavior": "allow"}})),
            r#"{"hookSpecificOutput":{"decision":{"behavior":"allow"},"hookEventName":"PermissionRequest"}}"#
        );
        assert!(decision_output(&json!({})).is_empty());
        assert!(decision_output(&json!({"decision": {"behavior": "maybe"}})).is_empty());
    }

    #[test]
    fn the_auto_approve_guard_fails_closed() {
        let d = tempfile::tempdir().unwrap();
        let ev = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"ls"}}"#;
        let denied = |out: String| {
            let v: Value = serde_json::from_str(&out).unwrap_or_else(|_| panic!("JSON: {out}"));
            assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny", "{out}");
            assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PreToolUse");
        };
        // No runner listening.
        denied(run_chat_guard(
            &mut ev.as_bytes(),
            Some(d.path().join("none.sock")),
            Some("c-abcdefgh".into()),
        ));
        // Not from a chat.
        denied(run_chat_guard(
            &mut ev.as_bytes(),
            Some(d.path().join("none.sock")),
            None,
        ));
        // Junk on stdin.
        denied(run_chat_guard(
            &mut "nope".as_bytes(),
            Some(d.path().join("none.sock")),
            Some("c-abcdefgh".into()),
        ));
        // A runner that answers junk, or nothing useful.
        for answer in ["not json\n", "{\"result\":{}}\n", "{\"error\":{\"code\":\"x\"}}\n"] {
            let sock = d.path().join(format!("r{}.sock", answer.len()));
            let l = std::os::unix::net::UnixListener::bind(&sock).unwrap();
            let a = answer.to_string();
            let t = std::thread::spawn(move || {
                let (mut s, _) = l.accept().unwrap();
                let mut line = String::new();
                BufReader::new(s.try_clone().unwrap()).read_line(&mut line).unwrap();
                s.write_all(a.as_bytes()).unwrap();
            });
            denied(run_chat_guard(
                &mut ev.as_bytes(),
                Some(sock),
                Some("c-abcdefgh".into()),
            ));
            t.join().unwrap();
        }
        // A runner that says go on: nothing printed (the CLI's bypass mode proceeds).
        let sock = d.path().join("ok.sock");
        let l = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let t = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let mut line = String::new();
            BufReader::new(s.try_clone().unwrap()).read_line(&mut line).unwrap();
            let v: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(v["params"]["guard"], true);
            assert_eq!(v["params"]["chatId"], "c-abcdefgh");
            s.write_all(b"{\"result\":{\"allow\":true}}\n").unwrap();
        });
        assert_eq!(
            run_chat_guard(&mut ev.as_bytes(), Some(sock), Some("c-abcdefgh".into())),
            ""
        );
        t.join().unwrap();
    }

    #[test]
    fn junk_or_no_runner_prints_nothing() {
        assert_eq!(run_hook(&mut "not json".as_bytes(), None, None), "");
        let d = tempfile::tempdir().unwrap();
        let ev = r#"{"hook_event_name":"PermissionRequest"}"#;
        assert_eq!(run_hook(&mut ev.as_bytes(), Some(d.path().join("none.sock")), None), "");
        assert_eq!(
            run_hook_for("nope", &mut ev.as_bytes(), Some(d.path().join("none.sock")), None),
            "",
            "an unknown plugin prints nothing"
        );
    }
}
