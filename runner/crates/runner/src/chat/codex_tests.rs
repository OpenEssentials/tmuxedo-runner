//! The Codex translator on `tests/fixtures/codex/turn.jsonl`. That file is
//! written from memory of the app-server's protocol (see codex.rs), not
//! captured from a real CLI: these tests pin the translator, not the vendor.

use super::*;

const TURN: &str = include_str!("../../tests/fixtures/codex/turn.jsonl");

fn parse(lines: &[Vec<u8>]) -> Vec<Value> {
    lines.iter().map(|l| serde_json::from_slice(l).unwrap()).collect()
}

fn opened() -> CState {
    let mut c = CState::default();
    c.begin(None, "/work", Some("untrusted"), "1.0.0");
    c.line(&json!({ "id": 1, "result": {} }));
    c.line(&json!({ "id": 2, "result": { "thread": { "id": "thr_01" } } }));
    c
}

#[test]
fn the_handshake_opens_a_thread_then_sends_what_waited() {
    let mut c = CState::default();
    let hello = parse(&[c.begin(None, "/work", Some("untrusted"), "1.0.0")]);
    assert_eq!(hello[0]["method"], "initialize");
    assert_eq!(hello[0]["params"]["clientInfo"]["name"], "tmuxedo");
    // A message before the thread is open waits.
    assert!(c.send("hi").is_empty());
    let o = c.line(&json!({ "id": 1, "result": { "userAgent": "x" } }));
    let w = parse(&o.writes);
    assert_eq!(w[0], json!({ "method": "initialized" }));
    assert_eq!(w[1]["method"], "thread/start");
    assert_eq!(w[1]["params"], json!({ "cwd": "/work", "approvalPolicy": "untrusted" }));
    assert!(o.thread.is_none());
    let o = c.line(&json!({ "id": 2, "result": { "thread": { "id": "thr_01" }, "model": "m" } }));
    assert_eq!(o.thread.as_deref(), Some("thr_01"));
    let w = parse(&o.writes);
    assert_eq!(w[0]["method"], "turn/start");
    assert_eq!(
        w[0]["params"],
        json!({ "threadId": "thr_01", "input": [{ "type": "text", "text": "hi" }] })
    );
    assert_eq!(c.model.as_deref(), Some("m"));

    // A resume names the thread; a refusal is fatal, in plain words.
    let mut c = CState::default();
    c.begin(Some("thr_09"), "/work", None, "1.0.0");
    let w = parse(&c.line(&json!({ "id": 1, "result": {} })).writes);
    assert_eq!(w[1]["method"], "thread/resume");
    assert_eq!(w[1]["params"], json!({ "cwd": "/work", "threadId": "thr_09" }));
    let o = c.line(&json!({ "id": 2, "error": { "code": -32600, "message": "no rollout" } }));
    assert_eq!(
        o.fatal.as_deref(),
        Some("Codex could not open the conversation: no rollout")
    );
}

#[test]
fn a_whole_turn_from_the_fixture() {
    let mut c = CState::default();
    c.begin(None, "/work", Some("untrusted"), "1.0.0");
    c.send("fix the test");
    let mut kinds = vec![];
    let mut events = vec![];
    let mut asks = vec![];
    for (i, l) in TURN.lines().enumerate() {
        c.now_ms = 1000 + 100 * i as u64;
        let o = c.line(&serde_json::from_str(l).unwrap());
        assert!(o.fatal.is_none());
        for a in o.asks {
            let item = c.needs_item(&a, "n1", "soon", "/work");
            c.ask_opened(&a.rpc, a.params["itemId"].as_str().unwrap());
            asks.push((a, item));
        }
        for e in o.emits {
            assert!(e.agent_path.is_none());
            kinds.push(e.body.kind());
            events.push(serde_json::to_value(&e.body).unwrap());
        }
    }
    assert_eq!(
        kinds,
        [
            "thought.done",
            "tool.started",
            "tool.updated",
            "tool.updated",
            "tool.done",
            "tool.started",
            "tool.updated",
            "tool.updated",
            "tool.done",
            "files.changed",
            "tool.started",
            "tool.done",
            "message.delta",
            "message.delta",
            "message.done",
            "turn.done",
        ]
    );
    assert_eq!(events[0]["text"], "Looking at the tests.");
    assert_eq!(events[0]["durationMs"], 200);
    // The command: what ran, the live tail, then how it ended (docs/design/chat-tools.md §14).
    let started = &events[1];
    assert_eq!(
        (&started["name"], &started["kind"], &started["subject"]),
        (&json!("Bash"), &json!("execute"), &json!("cargo test"))
    );
    assert_eq!(events[3]["output"], "running 2 tests\ntest result: FAILED\n");
    let done = &events[4];
    assert_eq!(
        (
            &done["status"],
            &done["exitCode"],
            &done["durationMs"],
            &done["outputBytes"]
        ),
        (&json!("failed"), &json!(101), &json!(4200), &json!(36))
    );
    // The approval for it: the command comes from the item (the request did not repeat it).
    let (ask, item) = &asks[0];
    assert_eq!(ask.rpc, json!(40));
    assert_eq!(item.title, "Run `cargo test`");
    assert_eq!(item.risk, "normal");
    assert_eq!(item.options, ["allow_once", "allow_session", "deny"]);
    assert_eq!(item.why.as_deref(), Some("needs the network"));
    // The file change: two paths, a diff each, and what changed once it succeeded.
    assert_eq!(events[5]["name"], "Edit");
    assert_eq!(events[5]["locations"].as_array().unwrap().len(), 2);
    assert_eq!(events[6]["diff"]["path"], "/work/src/lib.rs");
    assert_eq!(asks[1].1.title, "Edit `/work/src/lib.rs` and 1 more");
    assert_eq!(
        events[9]["files"],
        json!([
            { "path": "/work/src/lib.rs", "change": "modified", "source": "tool", "toolId": "fc_1" },
            { "path": "/work/NOTES.md", "change": "created", "source": "tool", "toolId": "fc_1" },
        ])
    );
    assert_eq!(
        (&events[10]["name"], &events[10]["server"]),
        (&json!("create_issue"), &json!("tracker"))
    );
    assert_eq!(events[11]["status"], "completed");
    assert_eq!(events[14]["text"], "Fixed it.");
    assert_eq!(events[15]["status"], "completed");
    assert_eq!(events[15]["usage"]["outputTokens"], 80);
    // Nothing is left open, and the turn's approvals went with it.
    assert!(c.tools.is_empty() && c.turn.is_none() && c.asked.is_empty() && c.pending.is_empty());
    assert_eq!(c.unknown, 0);
}

#[test]
fn answers_go_back_by_request_id_and_a_refused_call_is_denied() {
    let mut c = opened();
    let item = json!({ "type": "commandExecution", "id": "c1", "command": ["git", "push"], "status": "inProgress" });
    c.line(&json!({ "method": "item/started", "params": { "item": item } }));
    let o = c
        .line(&json!({ "id": "r-7", "method": "item/commandExecution/requestApproval", "params": { "itemId": "c1" } }));
    let ask = &o.asks[0];
    let need = c.needs_item(ask, "n", "soon", "/work");
    // An argv command reads as one line, and the shared risk rules see it.
    assert_eq!(need.title, "Run `git push`");
    assert_eq!(need.risk, "high");
    assert_eq!(need.options, ["allow_once", "deny"]);
    c.ask_opened(&ask.rpc, "c1");
    let r = |kind: &str| Response {
        kind: kind.into(),
        ..Default::default()
    };
    assert_eq!(decision(&need, &r("allow_once")), Ok("accept"));
    assert_eq!(decision(&need, &r("deny")), Ok("decline"));
    assert_eq!(decision(&need, &r("allow_session")).unwrap_err().0, "invalid_response");
    let w = parse(&c.answer_one(&json!("r-7"), "decline"));
    assert_eq!(w, [json!({ "id": "r-7", "result": { "decision": "decline" } })]);
    // Answered once: a second answer writes nothing.
    assert!(c.answer_one(&json!("r-7"), "accept").is_empty());
    let done = json!({ "type": "commandExecution", "id": "c1", "command": ["git", "push"], "status": "failed", "exitCode": 1 });
    let o = c.line(&json!({ "method": "item/completed", "params": { "item": done } }));
    let v = serde_json::to_value(&o.emits[0].body).unwrap();
    assert_eq!(v["status"], "denied");
    assert!(v.get("exitCode").is_none());

    let normal = NeedsItem {
        options: vec!["allow_once".into(), "allow_session".into(), "deny".into()],
        ..need
    };
    assert_eq!(decision(&normal, &r("allow_session")), Ok("acceptForSession"));
}

#[test]
fn an_interrupt_waits_for_the_turn_to_be_named_and_closes_what_was_open() {
    let mut c = opened();
    let w = parse(&c.send("go"));
    assert_eq!(w[0]["method"], "turn/start");
    // The server has not named the turn yet: nothing to interrupt by id.
    assert!(c.interrupt().is_empty());
    let o = c.line(&json!({ "id": 3, "result": { "turn": { "id": "turn_5" } } }));
    let w = parse(&o.writes);
    assert_eq!(w[0]["method"], "turn/interrupt");
    assert_eq!(w[0]["params"], json!({ "threadId": "thr_01", "turnId": "turn_5" }));
    // `turn/started` for the same turn does not ask twice.
    let o = c.line(&json!({ "method": "turn/started", "params": { "turn": { "id": "turn_5" } } }));
    assert!(o.writes.is_empty());
    let item = json!({ "type": "commandExecution", "id": "c1", "command": "sleep 9" });
    c.line(&json!({ "method": "item/started", "params": { "item": item } }));
    c.ask_opened(&json!(9), "c1");
    assert_eq!(parse(&c.answer_asked("cancel"))[0]["result"]["decision"], "cancel");
    let o = c.line(
        &json!({ "method": "turn/completed", "params": { "turn": { "id": "turn_5", "status": "interrupted" } } }),
    );
    let v: Vec<Value> = o.emits.iter().map(|e| serde_json::to_value(&e.body).unwrap()).collect();
    assert_eq!(
        (&v[0]["type"], &v[0]["status"]),
        (&json!("tool.done"), &json!("cancelled"))
    );
    assert_eq!(
        (&v[1]["type"], &v[1]["status"]),
        (&json!("turn.done"), &json!("interrupted"))
    );
    assert!(!c.interrupting && c.turn.is_none());

    // A turn the server refuses, and one that fails, end as failed with its words.
    c.send("again");
    let o = c.line(&json!({ "id": 5, "error": { "message": "thread is busy" } }));
    let v = serde_json::to_value(&o.emits[0].body).unwrap();
    assert_eq!(
        (&v["status"], &v["error"]),
        (&json!("failed"), &json!("thread is busy"))
    );
    let o = c.line(&json!({ "method": "turn/completed", "params": { "turn": { "status": "failed", "error": { "message": "quota" } } } }));
    let v = serde_json::to_value(&o.emits[0].body).unwrap();
    assert_eq!((&v["status"], &v["error"]), (&json!("failed"), &json!("quota")));
}

#[test]
fn lines_of_the_wrong_shape_never_panic_and_the_state_stays_bounded() {
    let mut c = opened();
    let odd = [
        json!({}),
        json!({ "id": null }),
        json!({ "id": 99, "result": {} }),
        json!({ "id": { "a": 1 }, "method": 7, "params": [1, 2] }),
        json!({ "method": "item/started", "params": "text" }),
        json!({ "method": "item/started", "params": { "item": { "type": "commandExecution", "id": 5 } } }),
        json!({ "method": "item/completed", "params": { "item": { "type": "fileChange", "id": "f", "changes": "no" } } }),
        json!({ "method": "item/agentMessage/delta", "params": { "itemId": [], "delta": {} } }),
        json!({ "method": "turn/completed", "params": null }),
        json!({ "method": "error", "params": { "error": 5 } }),
        json!({ "method": "future/thing", "params": {} }),
    ];
    for v in &odd {
        let o = c.line(v);
        assert!(o.fatal.is_none(), "{v}");
    }
    // An unknown request is refused on its own id, and the person is told where to answer.
    let o = c.line(&json!({ "id": "q1", "method": "tool/requestUserInput", "params": {} }));
    assert_eq!(parse(&o.writes)[0]["error"]["code"], -32601);
    assert_eq!(o.emits[0].body.kind(), "notice");
    assert!(c.unknown >= 3);
    // A child that announces calls without end does not grow the state without end.
    for i in 0..2000 {
        let item = json!({ "type": "commandExecution", "id": format!("c{i}"), "command": "true" });
        c.line(&json!({ "method": "item/started", "params": { "item": item } }));
        c.ask_opened(&json!(i), "c");
        c.deny(&format!("c{i}"));
        c.send("x");
    }
    assert!(c.tools.len() <= MAX_OPEN && c.asked.len() <= MAX_OPEN);
    assert!(c.denied.len() <= MAX_OPEN && c.pending.len() <= MAX_OPEN);
    // Output deltas keep a tail only.
    for _ in 0..100 {
        c.line(&json!({ "method": "item/commandExecution/outputDelta", "params": { "itemId": "c0", "delta": "y".repeat(1024) } }));
    }
    assert!(c.tools["c0"].live.len() <= 2 * LIVE_TAIL + 8);
}

#[test]
fn the_state_round_trips_without_the_unsent_message() {
    let mut c = CState::default();
    c.begin(None, "/work", None, "1.0.0");
    c.send("a message that is not masked yet");
    let saved = serde_json::to_string(&c).unwrap();
    assert!(!saved.contains("not masked"), "{saved}");
    let back: CState = serde_json::from_str(&saved).unwrap();
    assert_eq!(back.pending, c.pending);
    // A record from before this engine existed loads as the empty state.
    assert_eq!(serde_json::from_str::<CState>("{}").unwrap(), CState::default());
}

#[test]
fn an_old_cli_gets_no_chat_and_an_unknown_version_is_not_refused() {
    let old = super::super::agent_too_old;
    assert!(old(Some("0.159.9"), "0.160.0"));
    assert!(!old(Some("0.160.0"), "0.160.0"));
    assert!(!old(Some("0.170.0-alpha.2"), "0.160.0"));
    assert!(!old(None, "0.160.0"));
    assert!(!old(Some("0.1.0"), ""));
}
