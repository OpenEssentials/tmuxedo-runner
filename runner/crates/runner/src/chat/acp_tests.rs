//! The ACP translator on `tests/fixtures/acp/turn.jsonl`. That file is
//! written from memory of the Agent Client Protocol's specification (see
//! acp.rs), not captured from a real agent: these tests pin the translator,
//! not the vendor.

use super::*;

const TURN: &str = include_str!("../../tests/fixtures/acp/turn.jsonl");

fn parse(lines: &[Vec<u8>]) -> Vec<Value> {
    lines.iter().map(|l| serde_json::from_slice(l).unwrap()).collect()
}

fn ty(e: &Emit) -> &'static str {
    e.body.kind()
}

/// A session open, a prompt in flight (request id 3).
fn prompting() -> AState {
    let mut a = AState::default();
    a.begin(None, "/work", None, "1.0.0");
    a.line(&json!({ "id": 1, "result": { "protocolVersion": 1 } }));
    a.line(&json!({ "id": 2, "result": { "sessionId": "s1" } }));
    a.send("go");
    a
}

fn ask(rpc: Value, call: Value, kinds: &[&str]) -> Ask {
    let options: Vec<Value> = kinds
        .iter()
        .map(|k| json!({ "optionId": format!("opt-{k}"), "name": k, "kind": k }))
        .collect();
    Ask {
        rpc,
        method: "session/request_permission".into(),
        params: json!({ "sessionId": "s1", "toolCall": call, "options": options }),
    }
}

#[test]
fn the_handshake_opens_a_session_then_sends_what_waited() {
    let mut a = AState::default();
    let hello = parse(&[a.begin(None, "/work", Some("agent"), "1.0.0")]);
    assert_eq!(hello[0]["jsonrpc"], "2.0");
    assert_eq!(hello[0]["method"], "initialize");
    assert_eq!(hello[0]["params"]["protocolVersion"], 1);
    assert_eq!(hello[0]["params"]["clientCapabilities"]["terminal"], false);
    assert_eq!(hello[0]["params"]["clientInfo"]["name"], "tmuxedo");
    assert!(a.send("hi").is_empty(), "a message before the session waits");
    let mut lines = TURN.lines().map(|l| serde_json::from_str::<Value>(l).unwrap());
    let o = a.line(&lines.next().unwrap());
    assert!(a.can_load);
    let w = parse(&o.writes);
    assert_eq!(w[0]["method"], "session/new");
    assert_eq!(w[0]["params"], json!({ "cwd": "/work", "mcpServers": [] }));
    let o = a.line(&lines.next().unwrap());
    assert_eq!(o.thread.as_deref(), Some("sess_01"));
    assert_eq!(a.model.as_deref(), Some("m-test"));
    // Already in the native mode: no switch; the waiting message goes out.
    let w = parse(&o.writes);
    assert_eq!(w.len(), 1);
    assert_eq!(w[0]["method"], "session/prompt");
    assert_eq!(
        w[0]["params"],
        json!({ "sessionId": "sess_01", "prompt": [{ "type": "text", "text": "hi" }] })
    );
    assert_eq!(a.prompt, Some(3));

    // Another mode: switched once the session is open.
    let mut a = AState::default();
    a.begin(None, "/work", Some("agent"), "1.0.0");
    a.line(&json!({ "id": 1, "result": {} }));
    let o = a.line(&json!({ "id": 2, "result": { "sessionId": "s2",
        "modes": { "currentModeId": "ask", "availableModes": [{ "id": "agent" }, { "id": "ask" }] } } }));
    let w = parse(&o.writes);
    assert_eq!(w[0]["method"], "session/set_mode");
    assert_eq!(w[0]["params"], json!({ "sessionId": "s2", "modeId": "agent" }));
    // A mode it refuses is a notice, not the end.
    let o = a.line(&json!({ "id": 3, "error": { "code": -32602, "message": "no" } }));
    assert!(o.fatal.is_none());
    assert_eq!(ty(&o.emits[0]), "notice");
}

#[test]
fn resume_loads_when_the_agent_can_and_starts_anew_when_it_cannot() {
    // Can: `session/load`, and the replayed conversation is not shown again.
    let mut a = AState::default();
    a.begin(Some("old"), "/work", None, "1.0.0");
    a.send("next");
    let o =
        a.line(&json!({ "id": 1, "result": { "protocolVersion": 1, "agentCapabilities": { "loadSession": true } } }));
    let w = parse(&o.writes);
    assert_eq!(w[0]["method"], "session/load");
    assert_eq!(
        w[0]["params"],
        json!({ "sessionId": "old", "cwd": "/work", "mcpServers": [] })
    );
    let replay = json!({ "method": "session/update", "params": { "sessionId": "old",
        "update": { "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "before" } } } });
    assert!(a.line(&replay).emits.is_empty());
    let o = a.line(&json!({ "id": 2, "result": null }));
    assert_eq!(o.thread.as_deref(), Some("old"));
    assert_eq!(parse(&o.writes)[0]["method"], "session/prompt");
    assert!(!a.line(&replay).emits.is_empty(), "after the load, updates are shown");

    // Cannot: a new session, said plainly.
    let mut a = AState::default();
    a.begin(Some("old"), "/work", None, "1.0.0");
    let o = a.line(&json!({ "id": 1, "result": { "protocolVersion": 1, "agentCapabilities": {} } }));
    assert!(matches!(&o.emits[0].body, ChatBody::Notice { code, .. } if code == "resume_unsupported"));
    assert_eq!(parse(&o.writes)[0]["method"], "session/new");

    // The session is gone: also a new one.
    let mut a = AState::default();
    a.begin(Some("gone"), "/work", None, "1.0.0");
    a.line(&json!({ "id": 1, "result": { "agentCapabilities": { "loadSession": true } } }));
    let o = a.line(&json!({ "id": 2, "error": { "code": -32002, "message": "no such session" } }));
    assert!(matches!(&o.emits[0].body, ChatBody::Notice { code, text, .. }
        if code == "resume_failed" && text.contains("no such session")));
    assert_eq!(parse(&o.writes)[0]["method"], "session/new");
    assert!(o.fatal.is_none());
}

#[test]
fn a_whole_turn_maps_to_neutral_events() {
    let mut a = AState::default();
    a.begin(None, "/work", None, "1.0.0");
    a.send("test it");
    let mut emits = vec![];
    let mut asks = vec![];
    for (i, l) in TURN.lines().enumerate() {
        a.now_ms = 1000 + 10 * i as u64;
        let o = a.line(&serde_json::from_str(l).unwrap());
        emits.extend(o.emits);
        asks.extend(o.asks);
    }
    let types: Vec<&str> = emits.iter().map(ty).collect();
    assert_eq!(
        types,
        [
            "thought.done",
            "message.delta",
            "message.delta",
            "message.done",
            "tool.started",
            "tool.updated",
            "tool.done",
            "tool.started",
            "tool.done",
            "tool.started",
            "tool.updated",
            "tool.updated",
            "tool.done",
            "files.changed",
            "plan.updated",
            "message.delta",
            "message.done",
            "turn.done"
        ]
    );
    let b = |i: usize| serde_json::to_value(&emits[i].body).unwrap();
    assert_eq!(b(0)["text"], "Looking at the tests.");
    assert_eq!(b(0)["durationMs"], 10);
    assert_eq!(b(3)["text"], "I'll run them.");
    assert_eq!(b(1)["messageId"], b(3)["messageId"]);
    assert_ne!(b(3)["messageId"], b(16)["messageId"], "each message has its own id");
    // The command: execute, named and subject from its input; the live output, then the end.
    assert_eq!(
        (&b(4)["kind"], &b(4)["name"], &b(4)["subject"], &b(4)["title"]),
        (
            &json!("execute"),
            &json!("Bash"),
            &json!("npm test"),
            &json!("npm test")
        )
    );
    assert_eq!(b(5)["output"], "ok 1\n");
    assert_eq!(
        (&b(6)["status"], &b(6)["output"], &b(6)["exitCode"]),
        (&json!("completed"), &json!("ok 1\n"), &json!(0))
    );
    assert_eq!(asks.len(), 1);
    assert_eq!(asks[0].rpc, json!(0));
    // The read: announced finished, its location kept, its output from the start.
    assert_eq!((&b(7)["kind"], &b(7)["name"]), (&json!("read"), &json!("Read")));
    assert_eq!(b(7)["locations"][0], json!({ "path": "/work/src/a.rs", "line": 1 }));
    assert_eq!(b(8)["output"], "fn main() {}");
    // The edit: both diffs, then the files it changed (one new).
    assert_eq!(b(10)["diff"]["oldText"], "fn main() {}");
    assert_eq!(b(11)["diff"]["path"], "/work/src/b.rs");
    assert_eq!(
        b(13)["files"],
        json!([
            { "path": "/work/src/a.rs", "change": "modified", "source": "tool", "toolId": "call_3" },
            { "path": "/work/src/b.rs", "change": "created", "source": "tool", "toolId": "call_3" }
        ])
    );
    assert_eq!(
        b(14)["entries"],
        json!([
            { "text": "Run the tests", "status": "done" },
            { "text": "Fix main", "status": "in_progress" },
            { "text": "Ship", "status": "pending" }
        ])
    );
    assert_eq!(b(17)["status"], "completed");
    assert!(a.tools.is_empty() && a.prompt.is_none());
    // Text not masked yet is never saved with the state.
    let saved = serde_json::to_string(&a).unwrap();
    assert!(!saved.contains("All green") && !saved.contains("test it"), "{saved}");
}

#[test]
fn permissions_offer_what_the_agent_offers_and_answer_with_its_options() {
    let mut a = prompting();
    a.line(
        &json!({ "method": "session/update", "params": { "sessionId": "s1", "update": {
        "sessionUpdate": "tool_call", "toolCallId": "c1", "title": "ls", "kind": "execute",
        "rawInput": { "command": "ls -la" } } } }),
    );
    let all = ["allow_once", "allow_always", "reject_once", "reject_always"];
    let k = ask(json!(7), json!({ "toolCallId": "c1" }), &all);
    let item = a.needs_item(&k, "n1", "2026-01-01T00:00:00Z", "/work");
    assert_eq!(item.kind, "permission");
    assert_eq!(item.risk, "normal");
    assert_eq!(item.title, "Run `ls -la`");
    assert_eq!(item.tool.as_deref(), Some("Bash"));
    assert_eq!(item.options, ["allow_once", "allow_session", "deny"]);
    a.opened(&k);
    // Each neutral answer is the option of that kind, on the request's own id.
    let w = parse(&a.answer_one(&json!(7), "acceptForSession"));
    assert_eq!(w[0]["id"], 7);
    assert_eq!(
        w[0]["result"]["outcome"],
        json!({ "outcome": "selected", "optionId": "opt-allow_always" })
    );
    assert!(a.answer_one(&json!(7), "accept").is_empty(), "answered once only");

    // High risk: never "for this session", even when offered.
    let k = ask(
        json!("r-2"),
        json!({ "toolCallId": "c2", "kind": "execute", "rawInput": { "command": "git push origin main" } }),
        &all,
    );
    let item = a.needs_item(&k, "n2", "", "/work");
    assert_eq!(item.risk, "high");
    assert_eq!(item.options, ["allow_once", "deny"]);
    // An edit outside the project, asked about a call it never announced.
    let k = ask(
        json!(9),
        json!({ "toolCallId": "c3", "kind": "edit", "locations": [{ "path": "/etc/hosts" }] }),
        &["allow_once", "reject_once"],
    );
    let item = a.needs_item(&k, "n3", "", "/work");
    assert_eq!((item.risk.as_str(), item.title.as_str()), ("high", "Edit `/etc/hosts`"));
    assert_eq!(item.options, ["allow_once", "deny"]);
    // Only "always" offered and low risk: no "once".
    let k = ask(
        json!(10),
        json!({ "toolCallId": "c4", "kind": "read", "locations": [{ "path": "/work/a" }] }),
        &["allow_always", "reject_always"],
    );
    let item = a.needs_item(&k, "n4", "", "/work");
    assert_eq!(item.options, ["allow_session", "deny"]);
    a.opened(&k);
    let w = parse(&a.answer_one(&json!(10), "decline"));
    assert_eq!(w[0]["result"]["outcome"]["optionId"], "opt-reject_always");
    assert!(a.denied.contains("c4"));

    // A decline makes the call `denied` when it ends; cancel answers `cancelled`.
    a.opened(&ask(json!(11), json!({ "toolCallId": "c1" }), &all));
    let w = parse(&a.answer_asked("decline"));
    assert_eq!(w[0]["result"]["outcome"]["optionId"], "opt-reject_once");
    let o = a.line(
        &json!({ "method": "session/update", "params": { "sessionId": "s1", "update": {
        "sessionUpdate": "tool_call_update", "toolCallId": "c1", "status": "failed" } } }),
    );
    assert!(matches!(&o.emits[0].body, ChatBody::ToolDone { status, .. } if status == "denied"));
    a.opened(&ask(json!(12), json!({ "toolCallId": "c9" }), &all));
    let w = parse(&a.answer_asked("cancel"));
    assert_eq!(w[0]["result"]["outcome"], json!({ "outcome": "cancelled" }));
    // Too many open: refused with the agent's own reject option.
    let w = parse(&[refuse(&ask(json!(13), json!({}), &["allow_once", "reject_once"]))]);
    assert_eq!(w[0]["result"]["outcome"]["optionId"], "opt-reject_once");
}

#[test]
fn cancel_failures_and_refusals_end_the_turn() {
    // Cancel: a notification, and the turn ends as interrupted with open calls cancelled.
    let mut a = prompting();
    a.line(
        &json!({ "method": "session/update", "params": { "sessionId": "s1", "update": {
        "sessionUpdate": "tool_call", "toolCallId": "h", "kind": "execute", "title": "sleep 9" } } }),
    );
    let w = parse(&a.interrupt());
    assert_eq!(
        w[0],
        json!({ "jsonrpc": "2.0", "method": "session/cancel", "params": { "sessionId": "s1" } })
    );
    let o = a.line(&json!({ "id": 3, "result": { "stopReason": "cancelled" } }));
    let types: Vec<&str> = o.emits.iter().map(ty).collect();
    assert_eq!(types, ["tool.done", "turn.done"]);
    assert!(matches!(&o.emits[0].body, ChatBody::ToolDone { status, .. } if status == "cancelled"));
    assert!(matches!(&o.emits[1].body, ChatBody::TurnDone { status, .. } if status == "interrupted"));
    // No prompt in flight: nothing to cancel.
    assert!(a.interrupt().is_empty());

    // An error answer: failed, in the agent's words.
    let mut a = prompting();
    let o = a.line(&json!({ "id": 3, "error": { "code": -32603, "message": "model overloaded" } }));
    assert!(
        matches!(&o.emits[0].body, ChatBody::TurnDone { status, error: Some(e), .. }
        if status == "failed" && e == "model overloaded")
    );
    // A refusal: failed too.
    let mut a = prompting();
    let o = a.line(&json!({ "id": 3, "result": { "stopReason": "refusal" } }));
    assert!(matches!(&o.emits[0].body, ChatBody::TurnDone { status, .. } if status == "failed"));
    // A limit: completed, with a notice.
    let mut a = prompting();
    let o = a.line(&json!({ "id": 3, "result": { "stopReason": "max_tokens" } }));
    assert_eq!(o.emits.iter().map(ty).collect::<Vec<_>>(), ["notice", "turn.done"]);
}

#[test]
fn untrusted_lines_are_refused_or_counted_never_followed() {
    let mut a = prompting();
    // A question it cannot carry: refused on its own id, the person told where to answer.
    let o = a.line(&json!({ "id": "q", "method": "cursor/ask_question", "params": {} }));
    assert_eq!(parse(&o.writes)[0]["error"]["code"], -32601);
    assert!(matches!(&o.emits[0].body, ChatBody::Notice { code, .. } if code == "answer_in_terminal"));
    // Files and terminals were not offered.
    let o = a.line(&json!({ "id": 5, "method": "fs/read_text_file", "params": { "path": "/etc/shadow" } }));
    assert_eq!(parse(&o.writes)[0]["error"]["code"], -32601);
    // A response to nothing, an unknown update, another session's update, wrong types.
    assert_eq!(ty(&a.line(&json!({ "id": 99, "result": {} })).emits[0]), "notice");
    a.line(&json!({ "method": "session/update", "params": { "sessionId": "s1", "update": { "sessionUpdate": "new_thing" } } }));
    let other = json!({ "method": "session/update", "params": { "sessionId": "s9", "update": {
        "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "x" } } } });
    assert!(a.line(&other).emits.is_empty());
    let o = a.line(
        &json!({ "method": "session/update", "params": { "sessionId": "s1", "update": {
        "sessionUpdate": "tool_call", "toolCallId": 5, "kind": ["x"], "title": {} } } }),
    );
    assert!(o.emits.is_empty());
    assert_eq!(a.unknown, 3);
    // More unknown lines are counted, not shown.
    let o = a.line(&json!({ "method": "x/y" }));
    assert!(o.emits.is_empty());

    // A protocol version it does not speak, a sign-in it wants: fatal, in plain words.
    let mut a = AState::default();
    a.begin(None, "/w", None, "1");
    let o = a.line(&json!({ "id": 1, "result": { "protocolVersion": 2 } }));
    assert!(o.fatal.unwrap().contains("version 2"));
    let mut a = AState::default();
    a.begin(None, "/w", None, "1");
    a.line(&json!({ "id": 1, "result": { "protocolVersion": 1 } }));
    let o = a.line(&json!({ "id": 2, "error": { "code": -32000, "message": "Authentication required" } }));
    assert!(o.fatal.unwrap().contains("sign in"));
    let mut a = AState::default();
    a.begin(None, "/w", None, "1");
    a.line(&json!({ "id": 1, "result": {} }));
    let o = a.line(&json!({ "id": 2, "result": { "sessionId": "" } }));
    assert!(o.fatal.is_some());
}
