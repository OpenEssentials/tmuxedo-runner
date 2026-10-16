//! The chat screen (super::chat, super::chat_model, super::chat_app):
//! the model's mapping from events (web's chat-view / chat-tools semantics),
//! snapshots at 80×24, 120×40 and 50×15 of fixture chats (streaming, a tool
//! group, a helper, a decision, a failed turn, hostile text), key handling,
//! and the app's requests and frames (open as chat, attach, reconnect,
//! answers, back to the terminal).
//!
//! Snapshots live in tests/snapshots/tui-chat/; `UPDATE_SNAPSHOTS=1` rewrites them.

use super::app::{App, Out};
use super::chat::{self, ChatScreen, Cmd, Focus};
use super::chat_model::{self as m, ChatState, Detail, ToolState, ViewOptions};
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::Color;
use serde_json::{json, Value};
use std::collections::HashMap;
use tmuxedo_protocol::chat::{parse_event, ChatEvent, ChatFrame};

const SID: &str = "c-fixture0001";
/// 2026-10-07T08:00:00Z.
const T0: u64 = 1_791_360_000_000;

fn at(seq: u64) -> String {
    crate::util::iso(T0 + seq * 1000)
}

fn ev(seq: u64, ty: &str, extra: Value) -> ChatEvent {
    let mut v = json!({ "seq": seq, "at": at(seq), "sessionId": SID, "type": ty });
    for (k, x) in extra.as_object().expect("object") {
        v[k] = x.clone();
    }
    parse_event(&v.to_string()).unwrap_or_else(|| panic!("bad fixture event {v}"))
}

fn need(id: &str, risk: &str, title: &str, tool: &str, input: Value, options: &[&str]) -> Value {
    json!({ "item": {
        "id": id, "kind": "permission", "risk": risk, "options": options, "title": title,
        "tool": tool, "input": input, "expiresAt": at(600),
        "why": if risk == "high" { json!("It pushes to a remote.") } else { Value::Null },
    }})
}

fn opened() -> ChatEvent {
    ev(
        1,
        "session.opened",
        json!({ "plugin": "claude", "cwd": "/home/u/work", "mode": "ask_first", "login": "api_key", "features": [] }),
    )
}

fn user(seq: u64, text: &str) -> ChatEvent {
    ev(
        seq,
        "message.user",
        json!({ "messageId": format!("u{seq}"), "text": text }),
    )
}

fn said(seq: u64, text: &str) -> ChatEvent {
    ev(
        seq,
        "message.done",
        json!({ "messageId": format!("m{seq}"), "text": text }),
    )
}

fn started(seq: u64, id: &str, kind: &str, subject: &str, extra: Value) -> ChatEvent {
    let mut v =
        json!({ "toolId": id, "kind": kind, "title": format!("{kind} {subject}"), "input": {}, "subject": subject });
    for (k, x) in extra.as_object().expect("object") {
        v[k] = x.clone();
    }
    ev(seq, "tool.started", v)
}

fn done(seq: u64, id: &str, status: &str, extra: Value) -> ChatEvent {
    let mut v = json!({ "toolId": id, "status": status });
    for (k, x) in extra.as_object().expect("object") {
        v[k] = x.clone();
    }
    ev(seq, "tool.done", v)
}

fn state(events: &[ChatEvent]) -> ChatState {
    let mut s = ChatState::new(SID);
    events.iter().for_each(|e| s.apply(e, false));
    s
}

fn screen(events: &[ChatEvent], color: bool) -> ChatScreen {
    let mut c = ChatScreen::new("m1", SID, color);
    c.link = chat::Link::Live;
    events.iter().for_each(|e| c.on_event(e, false));
    c
}

// ── fixtures ────────────────────────────────────────────────────────────

fn streaming() -> Vec<ChatEvent> {
    vec![
        opened(),
        user(2, "Make the login test pass"),
        ev(3, "turn.started", json!({})),
        ev(
            4,
            "thought.done",
            json!({ "text": "Run the tests first.", "durationMs": 6000 }),
        ),
        said(5, "I'll run the tests first."),
        started(
            6,
            "t1",
            "execute",
            "npm test",
            json!({ "name": "Bash", "input": { "command": "npm test" } }),
        ),
        ev(
            7,
            "tool.updated",
            json!({ "toolId": "t1", "status": "in_progress", "output": "> app@1.0.0 test\n> jest\n\nPASS src/a.test.ts\n" }),
        ),
        ev(
            0,
            "message.delta",
            json!({ "messageId": "m9", "text": "The first suite passes; waiting for" }),
        ),
    ]
}

fn group() -> Vec<ChatEvent> {
    let mut v = vec![
        opened(),
        user(2, "Fix the login test"),
        ev(3, "turn.started", json!({})),
    ];
    let mut seq = 4;
    for f in [
        "src/a.ts", "src/b.ts", "src/c.ts", "src/d.ts", "src/e.ts", "src/f.ts", "src/g.ts",
    ] {
        v.push(started(
            seq,
            &format!("r{seq}"),
            "read",
            &format!("/home/u/work/{f}"),
            json!({ "name": "Read" }),
        ));
        v.push(done(
            seq + 1,
            &format!("r{seq}"),
            "completed",
            json!({ "output": "export {}\n" }),
        ));
        seq += 2;
    }
    v.push(started(
        seq,
        "s1",
        "search",
        "useAuth",
        json!({ "name": "Grep", "input": { "pattern": "useAuth", "path": "/home/u/work/src" } }),
    ));
    v.push(done(seq + 1, "s1", "completed", json!({ "resultCount": 3 })));
    v.push(started(
        seq + 2,
        "e1",
        "edit",
        "/home/u/work/src/auth.ts",
        json!({ "name": "Edit" }),
    ));
    v.push(ev(seq + 3, "tool.updated", json!({ "toolId": "e1", "status": "completed",
        "diff": { "path": "/home/u/work/src/auth.ts", "oldText": "if (!user) return null", "newText": "if (!user) throw new AuthError('no user')\nlog('x')" } })));
    v.push(done(seq + 4, "e1", "completed", json!({})));
    v.push(started(seq + 5, "x1", "execute", "npm test", json!({ "name": "Bash" })));
    v.push(done(
        seq + 6,
        "x1",
        "failed",
        json!({ "exitCode": 1, "durationMs": 8000, "output": "FAIL login\n1 failed" }),
    ));
    v.push(started(
        seq + 7,
        "f1",
        "fetch",
        "https://user@example.com/docs/api?q=1",
        json!({ "name": "WebFetch" }),
    ));
    v.push(done(seq + 8, "f1", "completed", json!({})));
    v.push(said(seq + 9, "The test passes now. I changed `auth.ts`."));
    v.push(ev(seq + 10, "turn.done", json!({ "status": "completed" })));
    v
}

fn helper() -> Vec<ChatEvent> {
    let inside = |e: ChatEvent| ChatEvent {
        agent_path: Some("h1".into()),
        ..e
    };
    vec![
        opened(),
        user(2, "Review the auth changes"),
        ev(3, "turn.started", json!({})),
        started(
            4,
            "h1",
            "other",
            "check the auth changes",
            json!({ "name": "Task", "variant": "agent", "input": { "prompt": "check the auth changes" } }),
        ),
        inside(started(
            5,
            "hr1",
            "read",
            "/home/u/work/src/auth.ts",
            json!({ "name": "Read" }),
        )),
        inside(done(6, "hr1", "completed", json!({}))),
        inside(started(
            7,
            "hr2",
            "read",
            "/home/u/work/src/db.ts",
            json!({ "name": "Read" }),
        )),
        inside(done(8, "hr2", "completed", json!({}))),
        inside(started(
            9,
            "hx1",
            "execute",
            "npm test -- auth",
            json!({ "name": "Bash" }),
        )),
    ]
}

fn decision(risk: &str) -> Vec<ChatEvent> {
    let input = json!({ "command": "git push" });
    vec![
        opened(),
        user(2, "Ship it"),
        ev(3, "turn.started", json!({})),
        started(
            4,
            "p1",
            "execute",
            "git push",
            json!({ "name": "Bash", "input": input }),
        ),
        ev(
            5,
            "needs.opened",
            need(
                "n1",
                risk,
                "Run `git push`",
                "Bash",
                input,
                &["allow_once", "allow_session", "deny"],
            ),
        ),
    ]
}

fn failed_turn() -> Vec<ChatEvent> {
    vec![
        opened(),
        user(2, "Build it"),
        ev(3, "turn.started", json!({})),
        started(4, "b1", "execute", "cargo build", json!({ "name": "Bash" })),
        done(
            5,
            "b1",
            "failed",
            json!({ "exitCode": 101, "durationMs": 12000, "summary": "error[E0308]: mismatched types", "output": "Compiling app\nerror[E0308]: mismatched types\n --> src/main.rs:3:5" }),
        ),
        started(6, "b2", "execute", "sleep 100", json!({ "name": "Bash" })),
        ev(
            7,
            "turn.done",
            json!({ "status": "failed", "error": "The agent stopped: API error 529" }),
        ),
    ]
}

/// Escape sequences, a title change, a bidi override and a zero-width space in
/// the agent's words, a tool's subject and its output.
fn hostile() -> Vec<ChatEvent> {
    vec![
        opened(),
        user(2, "hi"),
        ev(3, "turn.started", json!({})),
        said(
            4,
            "\u{1b}[2J\u{1b}]0;pwned\u{7}Hello \u{1b}[31mred\u{1b}[0m \u{202e}txt.exe\u{200b}!\u{1b}[?1049h",
        ),
        started(
            5,
            "h1",
            "execute",
            "echo \u{1b}[1A\u{1b}[2Kok\u{9b}31m",
            json!({ "name": "Bash" }),
        ),
        done(
            6,
            "h1",
            "completed",
            json!({ "output": "\u{1b}]8;;http://evil\u{7}link\u{1b}]8;;\u{7}\r\nline2\u{1b}P1;2q\u{1b}\\ end" }),
        ),
        ev(7, "turn.done", json!({ "status": "completed" })),
    ]
}

// ── snapshots ───────────────────────────────────────────────────────────

const SIZES: &[(u16, u16)] = &[(80, 24), (120, 40), (50, 15)];
/// The clock for running calls: 20 s after T0.
const NOW: Option<u64> = Some(T0 + 20_000);

fn text(buf: &Buffer) -> String {
    let mut out = String::new();
    for y in 0..buf.area.height {
        let line: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol().to_string()).collect();
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

fn draw(c: &ChatScreen, w: u16, h: u16) -> Buffer {
    let mut buf = Buffer::empty(Rect::new(0, 0, w, h));
    chat::draw(c, buf.area, &mut buf, NOW);
    buf
}

fn sheet(c: &ChatScreen) -> String {
    let mut out = String::new();
    for &(w, h) in SIZES {
        out.push_str(&format!("=== {w}x{h} ===\n"));
        out.push_str(&text(&draw(c, w, h)));
    }
    out
}

fn snapshot(name: &str, got: &str) {
    let path = std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/snapshots/tui-chat")).join(name);
    if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, got).unwrap();
        return;
    }
    let want = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{}: {e} (run with UPDATE_SNAPSHOTS=1)", path.display()));
    assert_eq!(
        got, want,
        "{name} changed; run with UPDATE_SNAPSHOTS=1 and review the diff"
    );
}

/// Nothing the agent wrote reaches a cell as a control character.
fn assert_clean(buf: &Buffer) {
    for c in buf.content() {
        assert!(
            !c.symbol().chars().any(|ch| ch.is_control()),
            "control character in a cell: {:?}",
            c.symbol()
        );
    }
}

#[test]
fn snapshot_streaming() {
    let c = screen(&streaming(), true);
    snapshot("streaming.txt", &sheet(&c));
}

#[test]
fn snapshot_tool_group_and_folding() {
    let mut c = screen(&group(), true);
    // Open the edit to show its diff, and the failed command.
    c.open.insert("ce1".into(), true);
    snapshot("group.txt", &sheet(&c));
    // The next message folds the finished turn to its summary.
    c.on_event(&user(40, "Thanks. Now the docs."), false);
    snapshot("group-folded.txt", &sheet(&c));
}

#[test]
fn snapshot_helper() {
    let c = screen(&helper(), true);
    let mut s = sheet(&c);
    let mut open = screen(&helper(), true);
    open.open.insert("ch1".into(), true);
    s.push_str("=== helper opened, 80x24 ===\n");
    s.push_str(&text(&draw(&open, 80, 24)));
    snapshot("helper.txt", &s);
}

#[test]
fn snapshot_decision() {
    let mut s = sheet(&screen(&decision("normal"), true));
    s.push_str("=== high risk, 80x24 ===\n");
    s.push_str(&text(&draw(&screen(&decision("high"), true), 80, 24)));
    snapshot("decision.txt", &s);
}

#[test]
fn snapshot_failed_turn() {
    let c = screen(&failed_turn(), true);
    snapshot("failed.txt", &sheet(&c));
}

#[test]
fn hostile_text_never_reaches_the_terminal() {
    let mut c = screen(&hostile(), true);
    c.open.insert("ch1".into(), true);
    for &(w, h) in SIZES {
        let buf = draw(&c, w, h);
        assert_clean(&buf);
        let t = text(&buf);
        assert!(
            !t.contains("pwned") && !t.contains("evil"),
            "an OSC's text was drawn:\n{t}"
        );
    }
    let t = text(&draw(&c, 120, 40));
    assert!(t.contains("Hello red ⟨U+202E⟩txt.exe⟨U+200B⟩!"), "{t}");
    assert!(t.contains("echo ok"), "{t}");
    snapshot("hostile.txt", &sheet(&c));
    // The pager and the copy show the same clean text.
    c.focus = Focus::Transcript;
    c.sel = Some("ch1".into());
    let cmds = c.on_key(key(KeyCode::Char('y')), NOW);
    let [Cmd::Copy(copied)] = cmds.as_slice() else {
        panic!("{cmds:?}")
    };
    assert!(!copied.chars().any(|ch| ch.is_control() && ch != '\n'), "{copied:?}");
    assert!(copied.contains("link") && copied.contains("line2"));
}

#[test]
fn no_color_draws_the_same_shapes_without_colour() {
    let a = screen(&group(), true);
    let b = screen(&group(), false);
    let (ba, bb) = (draw(&a, 80, 24), draw(&b, 80, 24));
    assert_eq!(text(&ba), text(&bb));
    assert!(ba.content().iter().any(|c| c.fg != Color::Reset));
    assert!(bb
        .content()
        .iter()
        .all(|c| c.fg == Color::Reset && c.bg == Color::Reset));
    assert!(text(&bb).contains("✕") && text(&bb).contains("failed"));
}

#[test]
fn tiny_sizes_do_not_panic() {
    for f in [
        streaming(),
        group(),
        helper(),
        decision("high"),
        failed_turn(),
        hostile(),
    ] {
        let c = screen(&f, true);
        for (w, h) in [(1, 1), (8, 3), (20, 5), (30, 8), (40, 10), (200, 3)] {
            let buf = draw(&c, w, h);
            assert_clean(&buf);
        }
    }
}

// ── the model ───────────────────────────────────────────────────────────

#[test]
fn deltas_stream_and_the_durable_message_replaces_them() {
    let mut s = state(&streaming());
    assert_eq!(
        s.streaming.as_ref().map(|x| x.1.as_str()),
        Some("The first suite passes; waiting for")
    );
    s.apply(
        &ev(0, "message.delta", json!({ "messageId": "m9", "text": " the rest" })),
        false,
    );
    assert_eq!(
        s.streaming.as_ref().unwrap().1,
        "The first suite passes; waiting for the rest"
    );
    s.apply(
        &ev(8, "message.done", json!({ "messageId": "m9", "text": "Done." })),
        false,
    );
    assert!(s.streaming.is_none());
    // A late delta for a message already done is ignored; a duplicate durable event changes nothing.
    s.apply(
        &ev(0, "message.delta", json!({ "messageId": "m9", "text": "x" })),
        false,
    );
    assert!(s.streaming.is_none());
    let before = s.clone();
    s.apply(&said(8, "again"), false);
    assert_eq!(s, before);
    // Another session's events are not this chat's.
    let mut other = ev(9, "notice", json!({ "code": "x", "text": "y" }));
    other.session_id = "c-other00001".into();
    s.apply(&other, false);
    assert_eq!(s, before);
}

#[test]
fn a_gap_on_the_first_event_is_shown() {
    let mut s = ChatState::new(SID);
    s.apply(&said(40, "later"), true);
    assert!(matches!(s.items[0], m::Item::Gap { seq: 40 }));
}

#[test]
fn an_interrupted_turn_stops_its_running_calls() {
    let mut s = state(&failed_turn());
    let w = m::waiting_calls(&s);
    assert_eq!(m::tool_state(&s.tools["b2"], &w), ToolState::Stopped);
    assert_eq!(m::tool_state(&s.tools["b1"], &w), ToolState::Failed);
    // `cancelled` from a newer runner reads the same.
    s.apply(&started(8, "c1", "execute", "x", json!({})), false);
    s.apply(&done(9, "c1", "cancelled", json!({})), false);
    assert_eq!(m::tool_state(&s.tools["c1"], &w), ToolState::Stopped);
    // The failed call is the turn's culprit: it opens itself.
    let t = m::timeline(
        &s,
        &ViewOptions {
            now: None,
            detail: Detail::Summary,
            turn_open: &HashMap::new(),
        },
    );
    let open: Vec<bool> = t
        .iter()
        .flat_map(|e| match e {
            m::Entry::Turn(t) => t.nodes.clone(),
            m::Entry::Node(n) => vec![n.clone()],
        })
        .filter_map(|n| match n {
            m::Node::Tool(t) if t.tool_id == "b1" => Some(t.open),
            _ => None,
        })
        .collect();
    assert_eq!(open, vec![true]);
}

#[test]
fn lines_read_as_web_draws_them() {
    let s = state(&group());
    let w = m::waiting_calls(&s);
    let line = |id: &str| m::tool_line(&s, &s.tools[id], &w, None, None);
    let x = line("x1");
    assert_eq!((x.verb.as_str(), x.object.as_str()), ("Ran", "npm test"));
    assert_eq!(x.facts, vec!["exit 1", "8 s"]);
    assert_eq!((x.state, x.word.as_str()), (ToolState::Failed, "failed"));
    let e = line("e1");
    assert_eq!((e.verb.as_str(), e.object.as_str()), ("Edited", "src/auth.ts"));
    assert_eq!(e.facts, vec!["+2 −1"]);
    let f = line("f1");
    assert_eq!(
        (f.object.as_str(), f.host.as_deref(), f.after.as_str()),
        ("", Some("example.com"), "/docs/api?q=1")
    );
    let sr = line("s1");
    assert_eq!(
        (sr.verb.as_str(), sr.facts.clone()),
        ("Searched for", vec!["in src".to_string(), "3 matches".into()])
    );
    // Running: present tense and the time so far.
    let s2 = state(&streaming());
    let r = m::tool_line(&s2, &s2.tools["t1"], &HashMap::new(), NOW, None);
    assert_eq!(
        (r.verb.as_str(), r.state, r.duration_ms),
        ("Running", ToolState::Running, Some(14_000))
    );
    // No subject (an older runner): the runner's title.
    let mut s3 = ChatState::new(SID);
    s3.apply(
        &ev(
            1,
            "tool.started",
            json!({ "toolId": "o1", "kind": "execute", "title": "Ran `ls`", "input": {} }),
        ),
        false,
    );
    let o = m::tool_line(&s3, &s3.tools["o1"], &HashMap::new(), None, None);
    assert_eq!((o.verb.as_str(), o.object.as_str()), ("", "Ran ls"));
}

#[test]
fn groups_form_from_the_second_call_and_show_the_latest_five() {
    let s = state(&group());
    let t = m::timeline(
        &s,
        &ViewOptions {
            now: None,
            detail: Detail::Summary,
            turn_open: &HashMap::new(),
        },
    );
    let [m::Entry::Node(_), m::Entry::Turn(turn)] = t.as_slice() else {
        panic!("{t:?}")
    };
    let m::Node::Group(g) = &turn.nodes[0] else {
        panic!("{:?}", turn.nodes[0])
    };
    assert_eq!(
        (g.title.as_str(), g.calls.len(), g.shown.len(), g.earlier),
        ("Read 7 files", 7, 5, 2)
    );
    // One search between reads and an edit is not a group.
    assert!(matches!(&turn.nodes[1], m::Node::Tool(t) if t.tool_id == "s1"));
    let sum = turn.summary.as_ref().unwrap();
    assert_eq!(
        sum.long,
        "Ran 1 command, edited 1 file, read 7, searched 1 time, fetched 1 page · 25 s"
    );
    assert_eq!((sum.tools, sum.failed), (11, 1));
    assert!(!turn.closed, "the latest turn stays open");
}

#[test]
fn a_turn_folds_when_the_person_sends_and_reopens_by_hand() {
    let mut s = state(&group());
    s.apply(&user(40, "next"), false);
    let mut by_hand = HashMap::new();
    let closed = |s: &ChatState, h: &HashMap<String, bool>, d: Detail| {
        m::timeline(
            s,
            &ViewOptions {
                now: None,
                detail: d,
                turn_open: h,
            },
        )
        .into_iter()
        .find_map(|e| match e {
            m::Entry::Turn(t) => Some(t.closed),
            _ => None,
        })
        .unwrap()
    };
    assert!(closed(&s, &by_hand, Detail::Summary));
    assert!(!closed(&s, &by_hand, Detail::Expanded), "expanded never folds");
    by_hand.insert("t3".to_string(), true);
    assert!(!closed(&s, &by_hand, Detail::Summary));
}

#[test]
fn helpers_nest_and_show_one_live_line() {
    let s = state(&helper());
    let t = m::timeline(
        &s,
        &ViewOptions {
            now: NOW,
            detail: Detail::Summary,
            turn_open: &HashMap::new(),
        },
    );
    let [_, m::Entry::Turn(turn)] = t.as_slice() else {
        panic!()
    };
    let [m::Node::Tool(h)] = turn.nodes.as_slice() else {
        panic!("{:?}", turn.nodes)
    };
    let facts = h.helper.as_ref().unwrap();
    assert_eq!(
        (facts.steps, facts.current.as_deref()),
        (3, Some("Running npm test -- auth"))
    );
    assert!(matches!(&h.children[0], m::Node::Group(g) if g.title == "Read 2 files"));
    assert_eq!(h.line.verb, "Helper");
}

#[test]
fn a_decision_is_tied_to_its_call() {
    let s = state(&decision("normal"));
    let w = m::waiting_calls(&s);
    assert_eq!(w.get("p1").map(String::as_str), Some("n1"));
    let l = m::tool_line(&s, &s.tools["p1"], &w, None, None);
    assert_eq!(
        (l.state, l.object.as_str(), l.word.as_str()),
        (ToolState::Waiting, "Run git push", "needs you")
    );
}

#[test]
fn safe_text_strips_and_shows() {
    assert_eq!(m::safe_line("a\u{1b}[31mb\nc\u{202e}d", 100), "ab⏎c⟨U+202E⟩d");
    assert_eq!(m::safe_line("abcdef", 3), "abc…");
    assert_eq!(m::safe_text("x\r\ny\u{7}\tz"), vec!["x".to_string(), "y z".into()]);
    assert_eq!(m::cut_middle("src/very/long/path/to/auth.ts", 16), "src/ver…/auth.ts");
    assert_eq!(m::format_duration(Some(72_000)), "1 min 12 s");
    assert_eq!(chat::wrap("one two three", 7), vec!["one two", "three"]);
}

// ── keys ────────────────────────────────────────────────────────────────

fn key(c: KeyCode) -> KeyEvent {
    KeyEvent::new(c, KeyModifiers::NONE)
}

fn typed(c: &mut ChatScreen, s: &str) {
    for ch in s.chars() {
        assert!(c.on_key(key(KeyCode::Char(ch)), NOW).is_empty());
    }
}

#[test]
fn enter_sends_and_alt_enter_is_a_new_line() {
    let mut c = screen(&[opened()], true);
    assert!(c.on_key(key(KeyCode::Enter), NOW).is_empty(), "nothing to send");
    typed(&mut c, "first");
    c.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT), NOW);
    typed(&mut c, "second");
    c.on_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL), NOW);
    typed(&mut c, "third");
    c.on_key(key(KeyCode::Backspace), NOW);
    assert_eq!(
        c.on_key(key(KeyCode::Enter), NOW),
        vec![Cmd::Send("first\nsecond\nthir".into())]
    );
    assert!(c.text.is_empty());
    // A paste keeps its line breaks and drops control characters.
    c.paste("a\r\nb\u{1b}[31m");
    assert_eq!(c.text, "a\nb[31m");
}

#[test]
fn esc_in_an_empty_composer_stops_the_turn() {
    let mut c = screen(&streaming(), true);
    assert_eq!(c.on_key(key(KeyCode::Esc), NOW), vec![Cmd::Interrupt]);
    typed(&mut c, "x");
    assert!(
        c.on_key(key(KeyCode::Esc), NOW).is_empty(),
        "text typed: Esc does not stop"
    );
    assert_eq!(
        c.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL), NOW),
        vec![Cmd::Interrupt]
    );
    // Idle: Ctrl+C clears the composer.
    let mut idle = screen(&group(), true);
    typed(&mut idle, "draft");
    assert!(idle
        .on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL), NOW)
        .is_empty());
    assert!(idle.text.is_empty());
}

#[test]
fn a_decision_takes_the_keys_and_answers_by_id() {
    let mut c = screen(&decision("normal"), true);
    assert_eq!(c.focus, Focus::Decision);
    assert_eq!(
        c.on_key(key(KeyCode::Char('s')), NOW),
        vec![Cmd::Respond {
            need: "n1".into(),
            response: json!({ "kind": "allow_session" })
        }]
    );
    // Answered here: no second answer while the runner resolves it.
    assert!(c.decision().is_none());
    assert_eq!(c.focus, Focus::Composer);
    let mut c = screen(&decision("normal"), true);
    assert_eq!(
        c.on_key(key(KeyCode::Char('a')), NOW),
        vec![Cmd::Respond {
            need: "n1".into(),
            response: json!({ "kind": "allow_once" })
        }]
    );
}

#[test]
fn high_risk_is_never_allowed_from_here() {
    let mut c = screen(&decision("high"), true);
    assert!(c.on_key(key(KeyCode::Char('a')), NOW).is_empty());
    assert!(c.on_key(key(KeyCode::Char('s')), NOW).is_empty());
    assert!(text(&draw(&c, 80, 24)).contains("answer this on the web or phone"));
    assert_eq!(
        c.on_key(key(KeyCode::Char('d')), NOW),
        vec![Cmd::Respond {
            need: "n1".into(),
            response: json!({ "kind": "deny" })
        }]
    );
}

#[test]
fn a_decision_arriving_while_typing_does_not_take_the_keys() {
    let mut events = decision("normal");
    let last = events.pop().unwrap();
    let mut c = screen(&events, true);
    typed(&mut c, "wait");
    c.on_event(&last, false);
    assert_eq!(c.focus, Focus::Composer);
    typed(&mut c, " a");
    assert_eq!(c.text, "wait a");
    // Tab, Tab reaches it.
    c.on_key(key(KeyCode::Tab), NOW);
    c.on_key(key(KeyCode::Tab), NOW);
    assert_eq!(c.focus, Focus::Decision);
}

#[test]
fn a_question_is_answered_by_number() {
    let mut events = vec![opened(), ev(2, "turn.started", json!({}))];
    events.push(ev(
        3,
        "needs.opened",
        json!({ "item": {
        "id": "q1", "kind": "question", "risk": "normal", "options": ["answers", "deny"], "title": "Answer 1 question",
        "questions": [{ "text": "Which database?", "options": [{ "label": "Postgres" }, { "label": "SQLite" }] }],
        "expiresAt": at(600) } }),
    ));
    let mut c = screen(&events, true);
    assert!(c.on_key(key(KeyCode::Char('3')), NOW).is_empty());
    assert_eq!(
        c.on_key(key(KeyCode::Char('2')), NOW),
        vec![Cmd::Respond {
            need: "q1".into(),
            response: json!({ "kind": "answers", "answers": { "Which database?": "SQLite" } })
        }]
    );
}

#[test]
fn the_transcript_moves_opens_and_closes() {
    let mut c = screen(&group(), true);
    draw(&c, 80, 24);
    c.on_key(key(KeyCode::Tab), NOW);
    assert_eq!(c.focus, Focus::Transcript);
    // The last selectable line is the fetch; up twice is the failed command.
    assert_eq!(c.sel.as_deref(), Some("cf1"));
    c.on_key(key(KeyCode::Char('k')), NOW);
    assert_eq!(c.sel.as_deref(), Some("cx1"));
    // It opened itself? No: the turn did not fail. Enter opens it.
    let before = c.lines(80, NOW).len();
    c.on_key(key(KeyCode::Enter), NOW);
    let after = c.lines(80, NOW);
    assert!(after.len() > before);
    assert!(after.iter().any(|l| l.spans.iter().any(|(s, _)| s == "$ npm test")));
    c.on_key(key(KeyCode::Char('h')), NOW);
    assert_eq!(c.lines(80, NOW).len(), before);
    // `v` shows the whole output; q closes it.
    c.on_key(key(KeyCode::Char('v')), NOW);
    assert!(c
        .pager
        .as_ref()
        .is_some_and(|p| p.lines.iter().any(|l| l == "1 failed")));
    c.on_key(key(KeyCode::Char('q')), NOW);
    assert!(c.pager.is_none());
    // `g` goes to the top, the turn summary opens and closes the turn.
    c.on_key(key(KeyCode::Char('g')), NOW);
    let first = c.sel.clone().unwrap();
    assert_eq!(first, "turn:t3");
    c.on_key(key(KeyCode::Char('h')), NOW);
    assert_eq!(c.turn_open.get("t3"), Some(&false));
    assert!(c.lines(80, NOW).len() < before);
    // Esc goes back to the composer.
    c.on_key(key(KeyCode::Esc), NOW);
    assert_eq!(c.focus, Focus::Composer);
}

// ── the app ─────────────────────────────────────────────────────────────

fn topology(chat: Value) -> Value {
    json!({
        "t": "topology", "rev": 1,
        "workspaces": [{"id":"w1","name":"default","cwd":"/","position":0}],
        "tabs": [{"id":"w1:t1","workspaceId":"w1","name":"main","position":0,"layout":{"pane":"w1:p1"}}],
        "panes": [{"id":"w1:p1","tabId":"w1:t1","cwd":"/","command":"claude","title":"claude","cols":80,"rows":24,
            "agent":{"kind":"claude","status":"idle","since":"t","source":"reported","chat":chat}}]
    })
}

fn app(chat: Value) -> App {
    let mut a = App::new(true, (80, 24));
    a.chat_color = false;
    a.chat_now = NOW;
    a.on_machines(&[json!({"id":"m1","name":"box","online":true})]);
    a.select_initial(None);
    a.on_text("m1", &json!({"t":"welcome","sub":"u1","mid":"m1","scope":"control","machine":{"online":true,"hello":{"v":1,"runner":{"version":"1","features":["chat"]}}}}));
    a.on_text("m1", &topology(chat));
    a.out.clear();
    a
}

fn prefixed(a: &mut App, c: char) {
    a.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
    a.on_key(key(KeyCode::Char(c)));
}

/// The last `req` sent: (id, method, params).
fn last_req(a: &App) -> (String, String, Value) {
    let v = a
        .out
        .iter()
        .rev()
        .find_map(|(_, o)| match o {
            Out::Text(v) if v["t"] == "req" => Some(v.clone()),
            _ => None,
        })
        .expect("a req");
    (
        v["id"].as_str().unwrap().into(),
        v["method"].as_str().unwrap().into(),
        v["params"].clone(),
    )
}

fn frames(a: &App, t: &str) -> Vec<Value> {
    a.out
        .iter()
        .filter_map(|(_, o)| match o {
            Out::Text(v) if v["t"] == t => Some(v.clone()),
            _ => None,
        })
        .collect()
}

fn frame(e: &ChatEvent) -> ChatFrame {
    ChatFrame {
        gap: false,
        ephemeral: e.seq == 0,
        req: 0,
        session_id: SID.into(),
        seq: e.seq,
        payload: serde_json::to_vec(e).unwrap(),
    }
}

#[test]
fn a_pane_opens_as_a_chat_and_goes_back_to_the_terminal() {
    let mut a = app(json!({}));
    prefixed(&mut a, 'C');
    let (id, method, params) = last_req(&a);
    assert_eq!(
        (method.as_str(), params),
        ("chat.handoff", json!({ "to": "chat", "pane_id": "w1:p1" }))
    );
    a.on_text(
        "m1",
        &json!({ "t": "res", "id": id, "result": { "type": "chat_handoff", "session": { "id": SID } } }),
    );
    assert_eq!(a.chat.as_ref().map(|c| c.sid.as_str()), Some(SID));
    assert_eq!(
        frames(&a, "chat_attach"),
        vec![json!({ "t": "chat_attach", "sessionId": SID, "from": 1 })]
    );
    // Frames build the transcript; the screen replaces the panes.
    for e in decision("normal") {
        let f = frame(&e);
        let enc = f.encode().unwrap();
        a.on_chat("m1", &ChatFrame::decode(&enc).unwrap());
    }
    a.on_text(
        "m1",
        &json!({ "t": "chat_attached", "sessionId": SID, "ok": true, "lastSeq": 5 }),
    );
    let mut t = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
    t.draw(|f| super::view::draw(&a, f)).unwrap();
    let screen = text(t.backend().buffer());
    snapshot("app-80x24.txt", &screen);
    // An answer goes out as chat.respond by id.
    a.on_key(key(KeyCode::Char('d')));
    let (id, method, params) = last_req(&a);
    assert_eq!(method, "chat.respond");
    assert_eq!(
        params,
        json!({ "session_id": SID, "need_id": "n1", "response": { "kind": "deny" } })
    );
    a.on_text(
        "m1",
        &json!({ "t": "res", "id": id, "error": { "code": "stepup_required", "message": "" } }),
    );
    assert_eq!(a.note.as_deref(), Some("high risk: answer this on the web or phone"));
    assert!(
        a.chat.as_ref().unwrap().decision().is_some(),
        "a refused answer brings the card back"
    );
    // A message.
    for ch in "go".chars() {
        a.on_key(key(KeyCode::Char(ch)));
    }
    a.on_key(key(KeyCode::Enter));
    let (_, method, params) = last_req(&a);
    assert_eq!((method.as_str(), &params["text"]), ("chat.send", &json!("go")));
    // Back to the terminal: the chat is left and its pane focused.
    prefixed(&mut a, 'C');
    let (id, method, params) = last_req(&a);
    assert_eq!(
        (method.as_str(), params),
        ("chat.handoff", json!({ "to": "terminal", "session_id": SID }))
    );
    a.out.clear();
    a.on_text("m1", &json!({ "t": "res", "id": id, "result": { "type": "chat_handoff", "paneId": "w1:p1", "session": { "id": SID } } }));
    assert!(a.chat.is_none());
    assert_eq!(
        frames(&a, "chat_detach"),
        vec![json!({ "t": "chat_detach", "sessionId": SID })]
    );
    assert_eq!(a.focus.as_deref(), Some("w1:p1"));
}

#[test]
fn an_agent_without_chat_says_so() {
    let mut a = app(Value::Null);
    prefixed(&mut a, 'C');
    assert!(frames(&a, "req").is_empty());
    assert!(a.note.as_deref().unwrap_or("").contains("cannot open as a chat"));
}

#[test]
fn a_reconnect_resumes_from_the_last_event() {
    let mut a = app(json!({ "id": SID }));
    a.chat_open(SID);
    for e in streaming() {
        a.on_chat("m1", &frame(&e));
    }
    a.on_link_down("m1");
    assert_eq!(a.chat.as_ref().unwrap().link, chat::Link::Reconnecting);
    a.out.clear();
    a.on_text(
        "m1",
        &json!({"t":"welcome","sub":"u1","mid":"m1","scope":"control","machine":{"online":true}}),
    );
    assert_eq!(
        frames(&a, "chat_attach"),
        vec![json!({ "t": "chat_attach", "sessionId": SID, "from": 8 })]
    );
    // A refused attach is said on the screen.
    a.on_text(
        "m1",
        &json!({ "t": "chat_attached", "sessionId": SID, "ok": false, "code": "chat_not_found" }),
    );
    let mut buf = Buffer::empty(Rect::new(0, 0, 80, 23));
    chat::draw(a.chat.as_ref().unwrap(), buf.area, &mut buf, NOW);
    assert!(text(&buf).contains("not available (chat_not_found)"));
    // prefix q leaves the screen and stops watching.
    a.out.clear();
    prefixed(&mut a, 'q');
    assert!(a.chat.is_none());
    assert_eq!(frames(&a, "chat_detach").len(), 1);
}

proptest::proptest! {
    /// Any text: no control character survives, and cutting or wrapping never panics.
    #[test]
    fn any_text_is_safe(s in "\\PC*|[\\x00-\\x1f\\x7f-\\u{9f}\\u{202a}-\\u{202e}a-z/é漢 ]{0,80}", w in 0usize..40) {
        let line = m::safe_line(&s, 50);
        proptest::prop_assert!(!line.chars().any(|c| c.is_control()));
        for l in m::safe_text(&s) {
            proptest::prop_assert!(!l.chars().any(|c| c.is_control()));
            let _ = chat::wrap(&l, w);
            let _ = chat::fit(&l, w);
        }
        let _ = m::cut_middle(&s, w);
    }
}
