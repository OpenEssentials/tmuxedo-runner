//! TUI core tests: TestBackend snapshots of the layout, key → frame mapping,
//! the decision overlay, data/ack handling, reconnect and size ownership.

use super::app::{App, Link, Mode, Out};
use super::view;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::style::Color;
use ratatui::Terminal;
use serde_json::{json, Value};
use tmuxedo_protocol::{DataFrame, DataKind};

fn topology() -> Value {
    let agent =
        |status: &str| json!({"kind":"claude","status":status,"since":"2026-10-02T10:00:00Z","source":"reported"});
    let mut blocked = agent("blocked");
    blocked["promptId"] = json!("pr1");
    blocked["prompt"] = json!({"tool":"Bash","input":{"command":"rm -rf build"}});
    let pane = |id: &str, tab: &str, agent: Value| json!({"id":id,"tabId":tab,"cwd":"/","command":"sh","title":"sh","cols":80,"rows":24,"agent":agent});
    json!({
        "t": "topology", "rev": 3,
        "workspaces": [{"id":"w1","name":"default","cwd":"/","position":0}],
        "tabs": [
            {"id":"w1:t1","workspaceId":"w1","name":"main","position":0,"layout":
                {"dir":"h","ratio":0.5,"a":{"pane":"w1:p1"},"b":{"dir":"v","ratio":0.5,"a":{"pane":"w1:p2"},"b":{"pane":"w1:p3"}}}},
            {"id":"w1:t2","workspaceId":"w1","name":"logs","position":1,"layout":{"pane":"w1:p4"}}
        ],
        "panes": [
            pane("w1:p1", "w1:t1", Value::Null),
            pane("w1:p2", "w1:t1", agent("working")),
            pane("w1:p3", "w1:t1", blocked),
            pane("w1:p4", "w1:t2", Value::Null),
        ]
    })
}

/// Every status of a screen-detected agent (codex: no hooks) in the sidebar row,
/// the workspace and machine roll-ups, and the glyph colours.
#[test]
fn sidebar_shows_every_agent_status() {
    for (status, glyph, word, colour) in [
        ("working", "●", "working", Color::Yellow),
        ("blocked", "!", "needs you", Color::Red),
        ("done", "✓", "done", Color::Green),
        ("idle", "○", "idle", Color::Gray),
        ("unknown", "?", "unknown", Color::DarkGray),
    ] {
        let mut app = App::new(true, (80, 16));
        app.on_machines(&[json!({"id":"m1","name":"box","online":true})]);
        app.select_initial(None);
        app.on_text("m1", &welcome("control"));
        let mut t = topology();
        t["panes"] = json!([{"id":"w1:p1","tabId":"w1:t1","cwd":"/","command":"codex","title":"codex","cols":80,"rows":24,
            "agent":{"kind":"codex","status":status,"since":"2026-10-02T10:00:00Z","source":"screen"}}]);
        t["tabs"] = json!([{"id":"w1:t1","workspaceId":"w1","name":"main","position":0,"layout":{"pane":"w1:p1"}}]);
        app.on_text("m1", &t);
        let (lines, buf) = render(&app);
        let row = format!("    {glyph} codex {word}");
        assert!(
            lines.iter().any(|l| l.starts_with(&row)),
            "{status}: no {row:?} in\n{}",
            lines.join("\n")
        );
        let y = lines.iter().position(|l| l.starts_with(&row)).unwrap() as u16;
        assert_eq!(buf[(4, y)].fg, colour, "{status} glyph colour");
        assert_eq!(view::glyph(Some(view_status(status))), (glyph, colour));
        assert_eq!(view::status_word(view_status(status)), word);
    }
}

fn view_status(s: &str) -> tmuxedo_protocol::frames::AgentStatus {
    serde_json::from_value(json!(s)).unwrap()
}

fn welcome(scope: &str) -> Value {
    json!({"t":"welcome","sub":"u1","mid":"m1","scope":scope,"machine":{"online":true}})
}

fn app_with(scope: &str) -> App {
    let mut app = App::new(scope == "control", (80, 16));
    app.on_machines(&[
        json!({"id":"m1","name":"box","online":true}),
        json!({"id":"m2","name":"lap","online":false}),
    ]);
    app.select_initial(None);
    app.on_text("m1", &welcome(scope));
    app.on_text("m1", &topology());
    app
}

fn snap(pane: &str, offset: u64, bytes: &[u8]) -> DataFrame {
    DataFrame {
        kind: DataKind::Snapshot,
        gap: false,
        req: 1,
        pane_id: pane.into(),
        offset,
        payload: bytes.to_vec(),
    }
}

fn output(pane: &str, offset: u64, bytes: &[u8]) -> DataFrame {
    DataFrame {
        kind: DataKind::Output,
        req: 0,
        ..snap(pane, offset, bytes)
    }
}

fn texts(app: &mut App) -> Vec<Value> {
    app.out
        .drain(..)
        .filter_map(|(_, o)| match o {
            Out::Text(v) => Some(v),
            Out::Data(_) => None,
        })
        .collect()
}

/// The id of the one `req` among the frames sent.
fn req_id(app: &mut App) -> Value {
    let reqs: Vec<Value> = texts(app).into_iter().filter(|v| v["t"] == "req").collect();
    assert_eq!(reqs.len(), 1, "{reqs:?}");
    reqs[0]["id"].clone()
}

fn key(c: KeyCode) -> KeyEvent {
    KeyEvent::new(c, KeyModifiers::NONE)
}

fn ctrl_b() -> KeyEvent {
    KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL)
}

fn prefixed(app: &mut App, c: KeyCode) {
    app.on_key(ctrl_b());
    app.on_key(key(c));
}

fn render(app: &App) -> (Vec<String>, ratatui::buffer::Buffer) {
    let mut t = Terminal::new(TestBackend::new(app.size.0, app.size.1)).unwrap();
    t.draw(|f| view::draw(app, f)).unwrap();
    let buf = t.backend().buffer().clone();
    let lines = (0..buf.area.height)
        .map(|y| {
            (0..buf.area.width)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect::<String>()
        })
        .collect();
    (lines, buf)
}

fn typed(app: &mut App, s: &str) {
    for c in s.chars() {
        app.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
    }
}

/// Wide enough for the rename prompt and the prefix hint on one status line.
fn wide(scope: &str) -> App {
    let mut app = app_with(scope);
    app.size = (170, 16);
    app
}

fn status_line(app: &App) -> String {
    let (lines, _) = render(app);
    lines.last().cloned().unwrap_or_default()
}

/// `prefix ,` like tmux: the status line asks for the tab's name, prefilled;
/// Enter sends `tab.rename` with the cleaned name; the reply's name is shown.
#[test]
fn prefix_comma_renames_the_tab_from_the_status_line() {
    let mut app = wide("control");
    app.out.clear();
    prefixed(&mut app, KeyCode::Char(','));
    let Mode::Rename(r) = &app.mode else {
        panic!("{:?}", app.mode)
    };
    assert_eq!(
        (r.id.as_str(), r.buf.as_str(), r.what.as_str()),
        ("w1:t1", "main", "tab 1")
    );
    assert!(
        status_line(&app).contains("rename tab 1 (enter saves, empty = default, esc cancels): main▏"),
        "{}",
        status_line(&app)
    );
    // Edit: ctrl-u clears, type a name with junk, backspace.
    app.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    typed(&mut app, "  api   logsX");
    app.on_key(key(KeyCode::Backspace));
    assert!(texts(&mut app).is_empty(), "nothing is sent while typing");
    app.on_key(key(KeyCode::Enter));
    assert_eq!(app.mode, Mode::Normal);
    let reqs: Vec<Value> = texts(&mut app).into_iter().filter(|v| v["t"] == "req").collect();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0]["method"], "tab.rename");
    assert_eq!(reqs[0]["params"], json!({"tab_id": "w1:t1", "label": "api logs"}));
    app.on_text(
        "m1",
        &json!({"t":"res","id":reqs[0]["id"],"result":{"type":"tab_info","tab":{"tab_id":"w1:t1","label":"api logs"}}}),
    );
    assert_eq!(app.note.as_deref(), Some("tab renamed to api logs"));
    // The next topology names it in the tab bar.
    let mut t = topology();
    t["tabs"][0]["name"] = json!("api logs");
    app.on_text("m1", &t);
    let (lines, _) = render(&app);
    assert!(lines[0].contains("1:api logs!"), "{}", lines[0]);
}

#[test]
fn workspace_and_pane_rename_and_an_empty_name_restores_the_default() {
    let mut app = wide("control");
    app.out.clear();
    prefixed(&mut app, KeyCode::Char('$'));
    let Mode::Rename(r) = &app.mode else { panic!() };
    assert_eq!(
        (r.id.as_str(), r.buf.as_str(), r.what.as_str()),
        ("w1", "default", "workspace default")
    );
    app.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    typed(&mut app, "   ");
    app.on_key(key(KeyCode::Enter));
    let req: Vec<Value> = texts(&mut app).into_iter().filter(|v| v["t"] == "req").collect();
    assert_eq!(req[0]["method"], "workspace.rename");
    assert_eq!(
        req[0]["params"],
        json!({"workspace_id": "w1", "label": null}),
        "empty → null → the default name"
    );

    let focus = app.focus.clone().unwrap();
    prefixed(&mut app, KeyCode::Char('P'));
    let Mode::Rename(r) = &app.mode else { panic!() };
    assert_eq!(
        (r.id.as_str(), r.buf.as_str()),
        (focus.as_str(), ""),
        "a pane without a label starts empty"
    );
    typed(&mut app, "build");
    app.on_key(key(KeyCode::Enter));
    let req: Vec<Value> = texts(&mut app).into_iter().filter(|v| v["t"] == "req").collect();
    assert_eq!(req[0]["method"], "pane.rename");
    assert_eq!(req[0]["params"], json!({"pane_id": focus, "label": "build"}));
    app.on_text(
        "m1",
        &json!({"t":"res","id":req[0]["id"],"result":{"type":"pane_info","pane":{"pane_id":focus,"label":"build"}}}),
    );
    assert_eq!(app.note.as_deref(), Some("pane renamed to build"));
    // Its label shows in the status line and, for an agent pane, in the sidebar.
    let mut t = topology();
    for p in t["panes"].as_array_mut().unwrap() {
        if p["id"] == json!(focus) || p["id"] == "w1:p2" {
            p["label"] = json!("build");
        }
    }
    app.on_text("m1", &t);
    let (lines, _) = render(&app);
    assert!(
        lines.iter().any(|l| l.starts_with("    ● build · claude wor")),
        "{}",
        lines.join("\n")
    );
    assert!(
        status_line(&app).contains(&format!("build ({focus}) · control")),
        "{}",
        status_line(&app)
    );
}

#[test]
fn renaming_is_cancelled_with_escape_and_capped_at_the_name_limit() {
    let mut app = wide("control");
    app.out.clear();
    prefixed(&mut app, KeyCode::Char(','));
    typed(&mut app, &"x".repeat(100));
    let Mode::Rename(r) = &app.mode else { panic!() };
    assert_eq!(r.buf.chars().count(), 64, "main + 60 x");
    // A paste goes into the name (newlines become spaces), not to the pane.
    app.on_event(Event::Paste("a\nb".into()));
    let Mode::Rename(r) = &app.mode else { panic!() };
    assert_eq!(r.buf.chars().count(), 64);
    app.on_key(key(KeyCode::Esc));
    assert_eq!(app.mode, Mode::Normal);
    assert_eq!(app.note.as_deref(), Some("rename cancelled"));
    assert!(texts(&mut app).iter().all(|v| v["t"] != "req"), "nothing sent");
    assert!(app.out.is_empty());

    // Paste within the limit.
    prefixed(&mut app, KeyCode::Char(','));
    app.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    app.on_event(Event::Paste("db\nshell".into()));
    app.on_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
    let Mode::Rename(r) = &app.mode else { panic!() };
    assert_eq!(r.buf, "db ", "ctrl-w removes the last word");
    assert!(texts(&mut app).iter().all(|v| v["t"] != "req" && v["t"] != "input"));
    assert!(app.out.is_empty(), "the paste never reached the pane");
}

/// Another client renames the tab while this one is typing: the draft is
/// kept (the topology does not overwrite it), and Enter sends it: the last
/// rename wins, as on the runner.
#[test]
fn a_rename_from_elsewhere_does_not_overwrite_the_draft() {
    let mut app = wide("control");
    app.out.clear();
    prefixed(&mut app, KeyCode::Char(','));
    app.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    typed(&mut app, "mine");
    let mut t = topology();
    t["rev"] = json!(4);
    t["tabs"][0]["name"] = json!("theirs");
    app.on_text("m1", &t);
    let Mode::Rename(r) = &app.mode else { panic!() };
    assert_eq!(r.buf, "mine");
    app.on_key(key(KeyCode::Enter));
    let req: Vec<Value> = texts(&mut app).into_iter().filter(|v| v["t"] == "req").collect();
    assert_eq!(req[0]["params"]["label"], "mine");
    // The tab was closed meanwhile: the runner's error is shown.
    app.on_text(
        "m1",
        &json!({"t":"res","id":req[0]["id"],"error":{"code":"tab_not_found","message":"no tab w1:t1"}}),
    );
    assert_eq!(app.note.as_deref(), Some("tab_not_found: no tab w1:t1"));
}

#[test]
fn observers_and_offline_machines_cannot_rename() {
    let mut app = wide("observe");
    app.out.clear();
    prefixed(&mut app, KeyCode::Char(','));
    assert_eq!(app.mode, Mode::Normal);
    assert_eq!(app.note.as_deref(), Some("rename tab: needs control"));
    assert!(texts(&mut app).iter().all(|v| v["t"] != "req"));

    let mut app = wide("control");
    app.on_link_down("m1");
    app.machines[0].online = false;
    prefixed(&mut app, KeyCode::Char('$'));
    assert_eq!(app.mode, Mode::Normal);
    assert_eq!(
        app.note.as_deref(),
        Some("machine offline: names can be changed once it reconnects")
    );
}

#[test]
fn the_prefix_hint_lists_the_rename_keys() {
    let mut app = wide("control");
    // With the overlay off, the status line is the reminder.
    app.keymap.overlay = super::overlay::OverlayMode::Off;
    app.on_key(ctrl_b());
    assert!(
        status_line(&app).contains(", $ P rename tab/ws/pane"),
        "{}",
        status_line(&app)
    );
}

fn fill_screens(app: &mut App) {
    app.on_data("m1", snap("w1:p1", 100, b"\x1b[H\x1b[2J$ echo hi\r\nhi\r\n$ "));
    app.on_data("m1", snap("w1:p2", 7, b"\x1b[H\x1b[2J\x1b[1;32mclaude\x1b[0m working"));
    app.on_data("m1", snap("w1:p3", 9, b"\x1b[H\x1b[2Jneeds approval"));
}

#[test]
fn layout_renders_sidebar_tabs_and_three_panes() {
    let mut app = app_with("control");
    fill_screens(&mut app);
    let (lines, buf) = render(&app);
    let want = [
        "! box                   │ 1:main!  2:logs                                       ",
        "  ! default             │$ echo hi                  │claude working             ",
        "    ● claude working    │hi                         │                           ",
        "    ! claude needs you  │$                          │                           ",
        "  lap (offline)         │                           │                           ",
        "                        │                           │                           ",
        "                        │                           │                           ",
        "                        │                           │                           ",
        "                        │                           │───────────────────────────",
        "                        │                           │needs approval             ",
        "                        │                           │                           ",
        "                        │                           │                           ",
        "                        │                           │                           ",
        "                        │                           │                           ",
        "                        │                           │                           ",
        " ^b c tab  % \" split  arrows focus  z zoom  x close  w workspac w1:p1 · control ",
    ];
    assert_eq!(lines, want, "\n{}", lines.join("\n"));
    // Colors and attributes come through from the pane's emulator.
    assert_eq!(buf[(53, 1)].fg, Color::Indexed(2));
    assert!(buf[(53, 1)].modifier.contains(ratatui::style::Modifier::BOLD));
    // The roll-ups: blocked wins over working for the workspace and the machine.
    assert_eq!(buf[(0, 0)].fg, Color::Red);
    assert_eq!(buf[(2, 1)].fg, Color::Red);
    // The separators next to the focused pane are highlighted.
    assert_eq!(buf[(52, 3)].fg, Color::Green);
}

#[test]
fn attaches_visible_panes_and_takes_their_size() {
    let mut app = App::new(true, (80, 16));
    app.on_machines(&[json!({"id":"m1","name":"box","online":true})]);
    app.select_initial(None);
    app.on_text("m1", &welcome("control"));
    assert!(texts(&mut app).is_empty(), "nothing to attach before the topology");
    app.on_text("m1", &topology());
    let out = texts(&mut app);
    let attaches: Vec<&Value> = out.iter().filter(|v| v["t"] == "attach").collect();
    assert_eq!(attaches.len(), 3);
    assert!(attaches.iter().all(|a| a.get("from").is_none()));
    let sizes: Vec<(String, u64, u64)> = out
        .iter()
        .filter(|v| v["t"] == "resize")
        .map(|v| {
            (
                v["paneId"].as_str().unwrap().to_string(),
                v["cols"].as_u64().unwrap(),
                v["rows"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        sizes,
        vec![
            ("w1:p1".into(), 27, 14),
            ("w1:p2".into(), 27, 7),
            ("w1:p3".into(), 27, 6)
        ]
    );
    // Switching tab detaches the hidden panes and attaches the new one.
    prefixed(&mut app, KeyCode::Char('n'));
    let out = texts(&mut app);
    assert_eq!(out.iter().filter(|v| v["t"] == "detach").count(), 3);
    assert!(out.iter().any(|v| v["t"] == "attach" && v["paneId"] == "w1:p4"));
    assert!(out
        .iter()
        .any(|v| v == &json!({"t":"resize","paneId":"w1:p4","cols":55,"rows":14})));
}

#[test]
fn prefix_keys_map_to_requests() {
    let mut app = app_with("control");
    texts(&mut app);
    let req = |app: &mut App| {
        let out = texts(app);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0]["t"], "req");
        assert!(out[0]["id"].as_str().unwrap().starts_with("tui-"));
        (out[0]["method"].as_str().unwrap().to_string(), out[0]["params"].clone())
    };
    prefixed(&mut app, KeyCode::Char('c'));
    assert_eq!(
        req(&mut app),
        ("tab.create".into(), json!({"workspace_id":"w1","focus":true}))
    );
    prefixed(&mut app, KeyCode::Char('%'));
    assert_eq!(
        req(&mut app),
        (
            "pane.split".into(),
            json!({"target_pane_id":"w1:p1","direction":"right","focus":true})
        )
    );
    prefixed(&mut app, KeyCode::Char('"'));
    assert_eq!(
        req(&mut app),
        (
            "pane.split".into(),
            json!({"target_pane_id":"w1:p1","direction":"down","focus":true})
        )
    );
    prefixed(&mut app, KeyCode::Char('z'));
    assert_eq!(
        req(&mut app),
        ("pane.zoom".into(), json!({"pane_id":"w1:p1","mode":"toggle"}))
    );

    // x needs a confirm: n cancels, y closes.
    prefixed(&mut app, KeyCode::Char('x'));
    assert_eq!(app.mode, Mode::Confirm("w1:p1".into()));
    assert!(texts(&mut app).is_empty());
    app.on_key(key(KeyCode::Char('n')));
    assert!(texts(&mut app).is_empty());
    assert_eq!(app.mode, Mode::Normal);
    prefixed(&mut app, KeyCode::Char('x'));
    app.on_key(key(KeyCode::Char('y')));
    assert_eq!(req(&mut app), ("pane.close".into(), json!({"pane_id":"w1:p1"})));

    // Arrows move focus by geometry.
    prefixed(&mut app, KeyCode::Right);
    assert_eq!(app.focus.as_deref(), Some("w1:p2"));
    prefixed(&mut app, KeyCode::Down);
    assert_eq!(app.focus.as_deref(), Some("w1:p3"));
    prefixed(&mut app, KeyCode::Left);
    assert_eq!(app.focus.as_deref(), Some("w1:p1"));
    assert!(texts(&mut app).is_empty(), "focus is local");
}

#[test]
fn request_results_move_focus_and_zoom() {
    let mut app = app_with("control");
    texts(&mut app);
    prefixed(&mut app, KeyCode::Char('z'));
    let id = req_id(&mut app);
    app.on_text(
        "m1",
        &json!({"t":"res","id":id,"result":{"type":"pane_zoom","zoom":{"pane_id":"w1:p1","zoomed":true,"changed":true}}}),
    );
    assert_eq!(app.visible(), vec![("w1:p1".to_string(), view::areas(app.size).main)]);
    assert!(texts(&mut app)
        .iter()
        .any(|v| v == &json!({"t":"detach","paneId":"w1:p2"})));
    // Split: the new pane gets focus once it is in the topology.
    prefixed(&mut app, KeyCode::Char('z'));
    let id = req_id(&mut app);
    app.on_text(
        "m1",
        &json!({"t":"res","id":id,"result":{"zoom":{"pane_id":"w1:p1","zoomed":false}}}),
    );
    assert_eq!(app.visible().len(), 3);
    texts(&mut app);
    prefixed(&mut app, KeyCode::Char('"'));
    let id = req_id(&mut app);
    app.on_text(
        "m1",
        &json!({"t":"res","id":id,"result":{"type":"pane_info","pane":{"pane_id":"w1:p5"}}}),
    );
    assert_eq!(app.focus.as_deref(), Some("w1:p1"));
    let mut topo = topology();
    topo["tabs"][0]["layout"]["a"] = json!({"dir":"v","ratio":0.5,"a":{"pane":"w1:p1"},"b":{"pane":"w1:p5"}});
    topo["panes"].as_array_mut().unwrap().push(
        json!({"id":"w1:p5","tabId":"w1:t1","cwd":"/","command":"sh","title":"sh","cols":80,"rows":24,"agent":null}),
    );
    app.on_text("m1", &topo);
    assert_eq!(app.focus.as_deref(), Some("w1:p5"));
    // An error result becomes a note.
    prefixed(&mut app, KeyCode::Char('c'));
    let id = req_id(&mut app);
    app.on_text(
        "m1",
        &json!({"t":"res","id":id,"error":{"code":"forbidden_scope","message":"tab.create needs control."}}),
    );
    assert_eq!(app.note.as_deref(), Some("forbidden_scope: tab.create needs control."));
}

#[test]
fn plain_keys_are_input_and_d_detaches() {
    let mut app = app_with("control");
    texts(&mut app);
    for k in [key(KeyCode::Char('l')), key(KeyCode::Char('s')), key(KeyCode::Enter)] {
        app.on_key(k);
    }
    app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
    prefixed(&mut app, KeyCode::Char('b')); // not ctrl: an unknown prefix key is dropped
    app.on_key(ctrl_b());
    app.on_key(ctrl_b()); // ctrl-b ctrl-b: a literal ctrl-b
    let inputs: Vec<(String, Vec<u8>)> = app
        .out
        .drain(..)
        .map(|(mid, o)| match o {
            Out::Data(f) => {
                assert_eq!((mid.as_str(), f.kind, f.req, f.offset), ("m1", DataKind::Input, 0, 0));
                (f.pane_id, f.payload)
            }
            Out::Text(v) => panic!("unexpected {v}"),
        })
        .collect();
    let p = |b: &[u8]| ("w1:p1".to_string(), b.to_vec());
    assert_eq!(inputs, vec![p(b"l"), p(b"s"), p(b"\r"), p(&[3]), p(&[2])]);
    // Pastes are bracketed when the pane asked for it.
    app.on_data("m1", snap("w1:p1", 0, b"\x1b[?2004h"));
    texts(&mut app);
    app.on_event(Event::Paste("a\nb".into()));
    match &app.out[0].1 {
        Out::Data(f) => assert_eq!(f.payload, b"\x1b[200~a\rb\x1b[201~"),
        o => panic!("{o:?}"),
    }
    app.out.clear();

    prefixed(&mut app, KeyCode::Char('d'));
    assert!(app.detached);
    let mut detached: Vec<String> = texts(&mut app)
        .into_iter()
        .map(|v| {
            assert_eq!(v["t"], "detach");
            v["paneId"].as_str().unwrap().to_string()
        })
        .collect();
    detached.sort();
    assert_eq!(detached, ["w1:p1", "w1:p2", "w1:p3"]);
}

#[test]
fn observers_cannot_type_or_mutate() {
    let mut app = app_with("observe");
    let out = texts(&mut app);
    assert!(out.iter().any(|v| v["t"] == "attach"));
    assert!(!out.iter().any(|v| v["t"] == "resize"), "observers never resize");
    app.on_key(key(KeyCode::Char('l')));
    prefixed(&mut app, KeyCode::Char('c'));
    assert!(app.out.is_empty());
    assert!(app.note.as_deref().unwrap().contains("needs control"));
}

#[test]
fn blocked_agent_prompt_becomes_a_decision() {
    let mut app = app_with("control");
    fill_screens(&mut app);
    texts(&mut app);
    assert!(app.active_prompt().is_none(), "w1:p1 has no agent");
    prefixed(&mut app, KeyCode::Right);
    prefixed(&mut app, KeyCode::Down);
    assert_eq!(app.focus.as_deref(), Some("w1:p3"));
    let (lines, _) = render(&app);
    let screen = lines.join("\n");
    assert!(screen.contains("w1:p3 · claude needs you"), "{screen}");
    assert!(screen.contains("Bash: rm -rf build"), "{screen}");
    assert!(screen.contains("[a] allow   [d] deny"), "{screen}");
    app.on_key(key(KeyCode::Char('a')));
    assert_eq!(
        texts(&mut app),
        vec![json!({"t":"decision","paneId":"w1:p3","promptId":"pr1","behavior":"allow"})]
    );
    assert!(app.active_prompt().is_none());
    // Typing goes to the pane again until a new prompt is held.
    app.on_key(key(KeyCode::Char('a')));
    assert!(matches!(&app.out[0].1, Out::Data(f) if f.payload == b"a"));
    app.out.clear();
    app.on_text(
        "m1",
        &json!({"t":"status","paneId":"w1:p3","agent":{"kind":"claude","status":"blocked","since":"x","source":"reported",
            "promptId":"pr2","prompt":{"tool":"Write","input":{"file_path":"/tmp/x"}}}}),
    );
    assert!(render(&app).0.join("\n").contains("Write: /tmp/x"));
    app.on_key(key(KeyCode::Char('n')));
    assert_eq!(
        texts(&mut app),
        vec![json!({"t":"decision","paneId":"w1:p3","promptId":"pr2","behavior":"deny"})]
    );
    // Esc dismisses without answering.
    app.on_text(
        "m1",
        &json!({"t":"status","paneId":"w1:p3","agent":{"kind":"claude","status":"blocked","since":"x","source":"reported",
            "promptId":"pr3","prompt":{"tool":"Bash","input":{}}}}),
    );
    app.on_key(key(KeyCode::Esc));
    assert!(app.out.is_empty() && app.active_prompt().is_none());
}

#[test]
fn output_is_deduplicated_and_acknowledged() {
    let mut app = app_with("control");
    texts(&mut app);
    app.on_data("m1", snap("w1:p1", 100, b"\x1b[H\x1b[2J"));
    assert_eq!(texts(&mut app), vec![json!({"t":"ack","paneId":"w1:p1","offset":100})]);
    app.on_data("m1", output("w1:p1", 100, b"abcdefghij"));
    assert!(texts(&mut app).is_empty(), "small output waits for the tick");
    app.tick();
    assert_eq!(texts(&mut app), vec![json!({"t":"ack","paneId":"w1:p1","offset":110})]);
    app.on_data("m1", output("w1:p1", 100, b"abcdefghij")); // replayed: ignored
    app.on_data("m1", output("w1:p1", 105, b"fghijKLMNO")); // overlapping: only KLMNO is new
    assert!(app
        .term("w1:p1")
        .unwrap()
        .emu
        .screen()
        .contents()
        .starts_with("abcdefghijKLMNO"));
    assert_eq!(app.term("w1:p1").unwrap().processed, Some(115));
    let big = vec![b'x'; 70 * 1024];
    app.on_data("m1", output("w1:p1", 115, &big));
    assert_eq!(
        texts(&mut app),
        vec![json!({"t":"ack","paneId":"w1:p1","offset":115 + 70 * 1024})],
        "at least every 64 KiB"
    );
    app.tick();
    assert!(texts(&mut app).is_empty());
}

#[test]
fn reconnect_reattaches_from_the_processed_offset() {
    let mut app = app_with("control");
    fill_screens(&mut app);
    app.on_data("m1", output("w1:p1", 100, b"more"));
    texts(&mut app);
    app.on_link_down("m1");
    assert_eq!(app.machines[0].link, Link::Down);
    app.on_text("m1", &welcome("control"));
    let out = texts(&mut app);
    let mut froms: Vec<(String, u64)> = out
        .iter()
        .filter(|v| v["t"] == "attach")
        .map(|v| (v["paneId"].as_str().unwrap().to_string(), v["from"].as_u64().unwrap()))
        .collect();
    froms.sort();
    assert_eq!(
        froms,
        vec![("w1:p1".into(), 104), ("w1:p2".into(), 7), ("w1:p3".into(), 9)]
    );
    assert_eq!(
        out.iter().filter(|v| v["t"] == "resize").count(),
        3,
        "size ownership is per socket"
    );
    // 4003: no more sockets for that machine.
    app.on_link_stopped("m1", "revoked");
    assert!(app.desired_links().is_empty());
}

#[test]
fn non_owners_render_at_the_pty_size() {
    let mut app = app_with("control");
    texts(&mut app);
    app.on_text("m1", &json!({"t":"owner","paneId":"w1:p1","you":false}));
    app.on_text("m1", &json!({"t":"size","paneId":"w1:p1","cols":10,"rows":3}));
    app.on_data("m1", snap("w1:p1", 0, b"0123456789\r\nline2"));
    texts(&mut app);
    assert_eq!(app.term("w1:p1").unwrap().pty, (10, 3));
    let (lines, buf) = render(&app);
    assert_eq!(
        lines[1].chars().skip(25).take(27).collect::<String>(),
        "0123456789                 "
    );
    assert_eq!(buf[(30, 1)].bg, Color::Reset);
    assert_eq!(buf[(40, 1)].bg, Color::Indexed(234), "letterboxed");
    assert_eq!(buf[(30, 5)].bg, Color::Indexed(234), "letterboxed");
    // A topology change does not steal the size back…
    app.on_text("m1", &topology());
    assert!(!texts(&mut app)
        .iter()
        .any(|v| v["t"] == "resize" && v["paneId"] == "w1:p1"));
    // …but resizing this terminal does.
    app.on_event(Event::Resize(90, 20));
    let out = texts(&mut app);
    assert!(
        out.iter()
            .any(|v| v == &json!({"t":"resize","paneId":"w1:p1","cols":32,"rows":18})),
        "{out:?}"
    );
    assert_eq!(out.iter().filter(|v| v["t"] == "resize").count(), 3);
}

#[test]
fn mouse_focuses_panes_and_selects_in_the_sidebar() {
    let mut app = app_with("control");
    texts(&mut app);
    let click = |x, y| MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: x,
        row: y,
        modifiers: KeyModifiers::NONE,
    };
    app.on_mouse(click(60, 12));
    assert_eq!(app.focus.as_deref(), Some("w1:p3"));
    app.on_mouse(click(6, 2)); // the "claude working" agent row
    assert_eq!(app.focus.as_deref(), Some("w1:p2"));
    app.on_mouse(click(35, 0)); // tab "2:logs"
    assert_eq!(app.tab_id().as_deref(), Some("w1:t2"));
    texts(&mut app);
    app.on_mouse(click(3, 4)); // machine "lap"
    assert_eq!(app.machine().unwrap().id, "m2");
    let out: Vec<(String, Value)> = app
        .out
        .drain(..)
        .filter_map(|(m, o)| match o {
            Out::Text(v) => Some((m, v)),
            _ => None,
        })
        .collect();
    assert_eq!(out, vec![("m1".to_string(), json!({"t":"detach","paneId":"w1:p4"}))]);
    assert_eq!(
        app.desired_links(),
        vec![("m1".to_string(), false), ("m2".to_string(), true)],
        "control for the selected machine, observe for the other online one"
    );
}

#[test]
fn sidebar_keys_navigate_workspaces_and_other_machines_keep_status() {
    let mut app = app_with("control");
    app.machines[1].online = true;
    assert_eq!(
        app.desired_links(),
        vec![("m1".to_string(), true), ("m2".to_string(), false)]
    );
    // The observe socket of a machine that is not selected feeds the sidebar.
    let mut t2 = topology();
    t2["panes"][2]["agent"] = json!({"kind":"codex","status":"done","since":"x","source":"screen"});
    t2["panes"][1]["agent"] = Value::Null;
    app.on_text(
        "m2",
        &json!({"t":"welcome","scope":"observe","machine":{"online":true}}),
    );
    app.on_text("m2", &t2);
    assert_eq!(
        app.machines[1].rollup(),
        Some(tmuxedo_protocol::frames::AgentStatus::Done)
    );
    assert!(
        app.out.iter().all(|(m, _)| m == "m1"),
        "nothing is attached on a machine that is not selected"
    );
    let (lines, _) = render(&app);
    assert!(lines[4].starts_with("✓ lap"), "{lines:?}");
    prefixed(&mut app, KeyCode::Char('w'));
    assert_eq!(app.mode, Mode::Sidebar);
    assert_eq!(app.sidebar_cursor, 1, "starts on the current workspace");
    for _ in 0..4 {
        app.on_key(key(KeyCode::Down));
    }
    app.on_key(key(KeyCode::Enter)); // lap's workspace
    assert_eq!(app.mode, Mode::Normal);
    assert_eq!(
        (app.machine().unwrap().id.as_str(), app.ws.as_deref()),
        ("m2", Some("w1"))
    );
}

fn mouse(kind: MouseEventKind, x: u16, y: u16) -> MouseEvent {
    MouseEvent {
        kind,
        column: x,
        row: y,
        modifiers: KeyModifiers::NONE,
    }
}

fn inputs(app: &mut App) -> Vec<(String, Vec<u8>)> {
    app.out
        .drain(..)
        .filter_map(|(_, o)| match o {
            Out::Data(f) if f.kind == DataKind::Input => Some((f.pane_id, f.payload)),
            _ => None,
        })
        .collect()
}

#[test]
fn mouse_is_forwarded_when_the_program_enabled_reporting() {
    let mut app = app_with("control");
    fill_screens(&mut app);
    texts(&mut app);
    let down = MouseEventKind::Down(MouseButton::Left);
    // w1:p1 (focused, x 25.., y 1..) has not asked for the mouse: a click is ours.
    app.on_mouse(mouse(down, 30, 3));
    assert!(inputs(&mut app).is_empty(), "no reporting, nothing sent");
    // The program turns on button-event tracking with SGR encoding (1002 + 1006).
    app.on_data("m1", output("w1:p1", 100 + 21, b"\x1b[?1002h\x1b[?1006h"));
    app.on_mouse(mouse(down, 30, 3));
    app.on_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 33, 4));
    app.on_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 200, 99)); // dragged off the pane: clamped
    app.on_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 34, 4));
    app.on_mouse(mouse(MouseEventKind::ScrollUp, 26, 1));
    let sent: Vec<String> = inputs(&mut app)
        .into_iter()
        .map(|(p, b)| {
            assert_eq!(p, "w1:p1");
            String::from_utf8(b).unwrap()
        })
        .collect();
    assert_eq!(
        sent,
        [
            "\x1b[<0;6;3M",
            "\x1b[<32;9;4M",
            "\x1b[<32;27;14M",
            "\x1b[<0;10;4m",
            "\x1b[<64;2;1M"
        ]
    );
    assert_eq!(app.mode, Mode::Normal, "the wheel went to the program, not scroll mode");
    // A click on another pane still focuses it first (not forwarded).
    app.on_mouse(mouse(down, 60, 12));
    assert_eq!(app.focus.as_deref(), Some("w1:p3"));
    assert!(inputs(&mut app).is_empty());
    // And the sidebar still works.
    app.on_mouse(mouse(down, 6, 2));
    assert_eq!(app.focus.as_deref(), Some("w1:p2"));
    // Observers never send mouse input.
    let mut obs = app_with("observe");
    fill_screens(&mut obs);
    obs.on_data("m1", output("w1:p1", 121, b"\x1b[?1000h"));
    obs.on_mouse(mouse(down, 30, 3));
    assert!(inputs(&mut obs).is_empty());
}

#[test]
fn scroll_mode_renders_history_from_the_emulator() {
    let mut app = app_with("control");
    fill_screens(&mut app);
    let mut lines = String::new();
    for i in 1..=60 {
        lines.push_str(&format!("line {i}\r\n"));
    }
    app.on_data("m1", output("w1:p1", 121, lines.as_bytes()));
    let row = |app: &App, y: u16| {
        render(app).0[y as usize]
            .chars()
            .skip(25)
            .take(26)
            .collect::<String>()
            .trim_end()
            .to_string()
    };
    assert_eq!(row(&app, 13), "line 60");
    // prefix [ → scroll mode on the focused pane; PgUp goes back a page.
    prefixed(&mut app, KeyCode::Char('['));
    assert_eq!(app.mode, Mode::Scroll("w1:p1".into()));
    app.on_key(key(KeyCode::PageUp));
    assert_eq!(app.scroll_pos("w1:p1"), 13);
    assert_eq!(row(&app, 13), "line 47");
    let (screen, _) = render(&app);
    assert!(screen[15].contains("scroll w1:p1: 13 lines up"), "{}", screen[15]);
    app.on_key(key(KeyCode::Up));
    assert_eq!(row(&app, 13), "line 46");
    app.on_key(key(KeyCode::Char('g')));
    assert_eq!(row(&app, 1), "$ echo hi", "the top of the history");
    // Typing does not reach the pane while scrolling.
    texts(&mut app);
    app.on_key(key(KeyCode::Char('x')));
    assert!(inputs(&mut app).is_empty());
    app.on_key(key(KeyCode::Char('q')));
    assert_eq!((app.mode.clone(), app.scroll_pos("w1:p1")), (Mode::Normal, 0));
    assert_eq!(row(&app, 13), "line 60");
    // The wheel scrolls panes that did not ask for the mouse, and leaves at the bottom.
    app.on_mouse(mouse(MouseEventKind::ScrollUp, 30, 5));
    assert_eq!(app.mode, Mode::Scroll("w1:p1".into()));
    assert_eq!(app.scroll_pos("w1:p1"), 3);
    app.on_mouse(mouse(MouseEventKind::ScrollDown, 30, 5));
    assert_eq!((app.mode.clone(), app.scroll_pos("w1:p1")), (Mode::Normal, 0));
}

#[test]
fn the_users_keybindings_drive_the_prefix_and_actions() {
    let mut app = app_with("control");
    fill_screens(&mut app);
    texts(&mut app);
    app.keymap = super::keymap::Keymap::from_json(&json!({
        "version": 1, "prefix": "ctrl+a", "isDefault": false,
        "bindings": {"split_right": "|", "split_down": "-", "detach": "q"}
    }));
    let ctrl_a = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL);
    app.on_key(ctrl_a);
    app.on_key(KeyEvent::new(KeyCode::Char('|'), KeyModifiers::SHIFT));
    let reqs: Vec<Value> = texts(&mut app).into_iter().filter(|v| v["t"] == "req").collect();
    assert_eq!(reqs[0]["method"], "pane.split");
    assert_eq!(reqs[0]["params"]["direction"], "right");
    // ctrl-b is just a key now; ctrl-a twice sends a literal ctrl-a.
    app.on_key(ctrl_b());
    app.on_key(ctrl_a);
    app.on_key(ctrl_a);
    assert_eq!(
        inputs(&mut app),
        vec![("w1:p1".into(), vec![0x02]), ("w1:p1".into(), vec![0x01])]
    );
    // The old default key for an action rebound elsewhere does nothing.
    app.on_key(ctrl_a);
    app.on_key(key(KeyCode::Char('d')));
    assert!(!app.detached);
    let (screen, _) = render(&app);
    assert!(screen[15].starts_with(" ^a c tab  | - split"), "{}", screen[15]);
    app.on_key(ctrl_a);
    app.on_key(key(KeyCode::Char('q')));
    assert!(app.detached);
}

#[test]
fn the_machine_shows_its_claude_auth_badge() {
    let mut app = App::new(true, (80, 16));
    app.on_machines(&[
        json!({"id":"m1","name":"box","online":true,"claudeAuthState":"ok"}),
        json!({"id":"m2","name":"lap","online":false,"claudeAuthState":"missing","claudeAuthReason":"no ~/.claude/.credentials.json"}),
    ]);
    app.select_initial(None);
    let side = |app: &App, y: usize| {
        render(app).0[y]
            .chars()
            .take(24)
            .collect::<String>()
            .trim_end()
            .to_string()
    };
    assert_eq!(side(&app, 0), "  box");
    assert_eq!(side(&app, 1), "  lap (offline) ⚠ login");
    // The runner's hello (forwarded in welcome) and auth_* events update it.
    app.on_text(
        "m1",
        &json!({"t":"welcome","sub":"u1","mid":"m1","scope":"control","machine":{"online":true,
            "hello":{"v":1,"runner":{"version":"x","claudeAuth":{"state":"expired","reason":"login_expired","since":"t"}}}}}),
    );
    app.on_text("m1", &topology());
    let (screen, buf) = render(&app);
    assert!(screen[0].starts_with("! box ⚠ expired"), "{}", screen[0]);
    assert_eq!(buf[(7, 0)].fg, Color::Red);
    assert!(screen[15].contains("claude expired"), "{}", screen[15]);
    app.on_text(
        "m1",
        &json!({"t":"event","seq":9,"paneId":"","type":"auth_ok","at":"t","data":{"state":"ok","reason":"turn completed","source":"hook"}}),
    );
    assert!(!render(&app).0[0].contains('⚠'));
    app.on_text(
        "m1",
        &json!({"t":"event","seq":10,"paneId":"w1:p2","type":"auth_failed","at":"t","data":{"state":"failed","reason":"account_on_hold","source":"hook"}}),
    );
    assert!(render(&app).0[0].contains("⚠ failed"));
}

#[test]
fn keybindings_are_fetched_with_the_cli_token_or_default_offline() {
    use std::io::{BufRead, BufReader, Write};
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    let seen = std::thread::spawn(move || {
        let (mut s, _) = l.accept().unwrap();
        let mut r = BufReader::new(s.try_clone().unwrap());
        let mut head = vec![];
        loop {
            let mut h = String::new();
            r.read_line(&mut h).unwrap();
            if h == "\r\n" || h.is_empty() {
                break;
            }
            head.push(h.trim_end().to_string());
        }
        let body = r#"{"version":1,"prefix":"alt+a","bindings":{"zoom":"m"},"isDefault":false}"#;
        write!(s, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        head
    });
    let cfg = crate::cli::CliConfig {
        server: url,
        token: "amx_testtoken".into(),
        copy_on_select: None,
    };
    let km = super::fetch_keymap(&cfg);
    let head = seen.join().unwrap();
    assert_eq!(head[0], "GET /api/me/keybindings HTTP/1.1");
    assert!(
        head.iter()
            .any(|h| h.eq_ignore_ascii_case("authorization: Bearer amx_testtoken")),
        "{head:?}"
    );
    assert_eq!((km.prefix.as_str(), km.prefix_label()), ("alt+a", "M-a".to_string()));
    assert_eq!(km.key(super::keymap::Action::Zoom), "m");
    let offline = super::fetch_keymap(&crate::cli::CliConfig {
        server: "http://127.0.0.1:9".into(),
        token: "amx_x".into(),
        copy_on_select: None,
    });
    assert_eq!(offline.prefix, "ctrl+b");
    assert!(offline.source.starts_with("defaults (offline"), "{}", offline.source);
}

fn clipboard(app: &mut App) -> Vec<String> {
    use base64::Engine;
    app.local_out
        .drain(..)
        .map(|b| {
            let s = String::from_utf8(b).unwrap();
            let b64 = s
                .strip_prefix("\x1b]52;c;")
                .and_then(|r| r.strip_suffix('\x07'))
                .unwrap();
            String::from_utf8(base64::engine::general_purpose::STANDARD.decode(b64).unwrap()).unwrap()
        })
        .collect()
}

#[test]
fn drag_selects_highlights_and_copies_via_osc52() {
    let mut app = app_with("control");
    fill_screens(&mut app);
    // w1:p1 is focused at x 25.., y 1..: "$ echo hi" / "hi" / "$ ".
    let down = MouseEventKind::Down(MouseButton::Left);
    let drag = MouseEventKind::Drag(MouseButton::Left);
    let up = MouseEventKind::Up(MouseButton::Left);
    app.on_mouse(mouse(down, 27, 1));
    app.on_mouse(mouse(drag, 26, 2));
    // Highlighted while dragging: from "echo" on row 0 to "hi" on row 1.
    let (_, buf) = render(&app);
    let rev = |x: u16, y: u16| buf[(x, y)].modifier.contains(ratatui::style::Modifier::REVERSED);
    assert!(rev(27, 1) && rev(40, 1) && rev(25, 2) && rev(26, 2));
    assert!(!rev(26, 1) && !rev(27, 2) && !rev(25, 3));
    assert!(inputs(&mut app).is_empty(), "the drag is ours, not the program's");
    app.on_mouse(mouse(up, 26, 2));
    assert_eq!(clipboard(&mut app), ["echo hi\nhi"]);
    assert_eq!(app.note.as_deref(), Some("copied 10 chars"));
    let (screen, _) = render(&app);
    assert!(screen[15].contains("copied 10 chars"), "{}", screen[15]);
    // A key clears the highlight.
    app.on_key(key(KeyCode::Char('x')));
    assert!(app.selection.is_none());
    // A plain click copies nothing.
    app.on_mouse(mouse(down, 30, 3));
    app.on_mouse(mouse(up, 30, 3));
    assert!(clipboard(&mut app).is_empty() && app.selection.is_none());
}

#[test]
fn selection_joins_wrapped_lines_and_copies_wide_chars_once() {
    let mut app = app_with("control");
    fill_screens(&mut app);
    // Pane w1:p1 is 26 columns wide here: 40 x's wrap onto two rows.
    let line = format!("\x1b[H\x1b[2J{}\r\n日本語 ok\r\n", "x".repeat(40));
    app.on_data("m1", output("w1:p1", 121, line.as_bytes()));
    let down = MouseEventKind::Down(MouseButton::Left);
    app.on_mouse(mouse(down, 25, 1));
    app.on_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 33, 3));
    app.on_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 33, 3));
    assert_eq!(clipboard(&mut app), [format!("{}\n日本語 ok", "x".repeat(40))]);
}

#[test]
fn shift_drag_selects_over_a_mouse_reporting_program_and_the_toggle_turns_it_off() {
    let mut app = app_with("control");
    fill_screens(&mut app);
    app.on_data("m1", output("w1:p1", 121, b"\x1b[?1002h\x1b[?1006h"));
    let down = MouseEventKind::Down(MouseButton::Left);
    let up = MouseEventKind::Up(MouseButton::Left);
    // Without Shift the program gets the mouse.
    app.on_mouse(mouse(down, 27, 1));
    app.on_mouse(mouse(up, 30, 1));
    assert_eq!(inputs(&mut app).len(), 2);
    assert!(clipboard(&mut app).is_empty());
    // With Shift it is a selection.
    let shifted = |kind, x, y| MouseEvent {
        modifiers: KeyModifiers::SHIFT,
        ..mouse(kind, x, y)
    };
    app.on_mouse(shifted(down, 25, 2));
    app.on_mouse(shifted(MouseEventKind::Drag(MouseButton::Left), 26, 2));
    app.on_mouse(shifted(up, 26, 2));
    assert_eq!(clipboard(&mut app), ["hi"]);
    assert!(inputs(&mut app).is_empty());
    // copy_on_select = false: drags over a plain pane do nothing.
    let mut off = app_with("control");
    fill_screens(&mut off);
    off.copy_on_select = false;
    off.on_mouse(mouse(down, 27, 1));
    off.on_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 30, 2));
    off.on_mouse(mouse(up, 30, 2));
    assert!(off.selection.is_none() && clipboard(&mut off).is_empty());
}

#[test]
fn open_url_events_open_on_this_machine() {
    let mut app = app_with("control");
    let ev = |url: &str| json!({"t":"event","seq":0,"paneId":"w1:p1","type":"open_url","at":"t","data":{"url":url,"source":"browser-shim","program":"xdg-open"}});
    app.on_text("m1", &ev("https://claude.ai/oauth/authorize?x=1"));
    assert_eq!(app.open_urls, ["https://claude.ai/oauth/authorize?x=1"]);
    assert_eq!(
        app.note.as_deref(),
        Some("opening https://claude.ai/oauth/authorize?x=1")
    );
    let (screen, _) = render(&app);
    assert!(screen[15].contains("opening https://claude.ai"), "{}", screen[15]);
    app.open_urls.clear();
    app.on_text("m1", &ev("file:///etc/passwd"));
    app.on_text("m2", &ev("https://other.machine/x")); // not the one shown
    assert!(app.open_urls.is_empty());
    assert!(app.login_callbacks.is_empty(), "no callback, nothing to remember");
}

#[test]
fn a_login_link_with_a_localhost_callback_shows_how_to_finish_it() {
    let mut app = app_with("control");
    let cb = json!({"host":"127.0.0.1","port":1455,"path":"/auth/callback","state":"s"});
    let url = "https://auth.openai.com/oauth/authorize?response_type=code&redirect_uri=http%3A%2F%2F127.0.0.1%3A1455%2Fauth%2Fcallback&state=s";
    app.on_text(
        "m1",
        &json!({"t":"event","seq":0,"paneId":"w1:p1","type":"open_url","at":"t","data":{"url":url,"source":"browser-shim","program":"codex","callback":cb}}),
    );
    assert_eq!(app.open_urls, [url]);
    let note = app.note.clone().unwrap();
    assert!(
        note.contains("127.0.0.1:1455") && note.contains("tmuxedo login-callback"),
        "{note}"
    );
    assert_eq!(app.login_callbacks, [("m1".to_string(), "w1:p1".to_string(), cb)]);
    let (screen, _) = render(&app);
    assert!(screen[15].contains("login link sent to your browser"), "{}", screen[15]);
}

// The prefix overlay (queue 3j): snapshots and keys, in their own file.
#[path = "overlay_tests.rs"]
mod overlay_tests;
