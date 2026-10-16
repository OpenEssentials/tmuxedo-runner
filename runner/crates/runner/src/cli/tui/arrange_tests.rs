//! The Arrange popup and favourites in the TUI (super::arrange_ui): the
//! popup drawn at several sizes, keys to requests in the runner's wire
//! format, refusals and undo in the web's words, a favourite's restore step
//! by step (never closing a pane), and the restore plans shared with
//! `packages/layouts` (tests/fixtures/favourites.json).

use super::app::{App, Mode, Out};
use super::arrange_ui::{box_art, Pick};
use super::favourites::{self as fav, OpenPane};
use super::view;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use serde_json::{json, Value};

/// A tab with `n` panes side by side (w1:p1 … w1:pn); p2 runs Claude.
fn topology(n: usize) -> Value {
    let ids: Vec<String> = (1..=n).map(|i| format!("w1:p{i}")).collect();
    fn chain(ids: &[String]) -> Value {
        if ids.len() == 1 {
            return json!({ "pane": ids[0] });
        }
        json!({ "dir": "h", "ratio": 1.0 / ids.len() as f64, "a": { "pane": ids[0] }, "b": chain(&ids[1..]) })
    }
    let panes: Vec<Value> = ids
        .iter()
        .enumerate()
        .map(|(i, id)| {
            let agent = if i == 1 {
                json!({"kind":"claude","status":"idle","since":"t","source":"reported"})
            } else {
                Value::Null
            };
            json!({"id":id,"tabId":"w1:t1","cwd":"/work","command":"sh","title":"sh","cols":60,"rows":20,"agent":agent})
        })
        .collect();
    json!({
        "t": "topology", "rev": 7,
        "workspaces": [{"id":"w1","name":"default","cwd":"/work","position":0}],
        "tabs": [{"id":"w1:t1","workspaceId":"w1","name":"main","position":0,"layout":chain(&ids),"focusedPane":"w1:p1"}],
        "panes": panes,
    })
}

fn app(n: usize, size: (u16, u16), scope: &str) -> App {
    let mut app = App::new(scope == "control", size);
    app.on_machines(&[json!({"id":"m1","name":"box","online":true})]);
    app.select_initial(None);
    app.on_text(
        "m1",
        &json!({"t":"welcome","sub":"u1","mid":"m1","scope":scope,"machine":{"online":true}}),
    );
    app.on_text("m1", &topology(n));
    app.out.clear();
    app
}

fn key(c: KeyCode) -> KeyEvent {
    KeyEvent::new(c, KeyModifiers::NONE)
}

fn prefixed(app: &mut App, c: KeyCode) {
    app.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
    app.on_key(key(c));
}

fn reqs(app: &mut App) -> Vec<Value> {
    app.out
        .drain(..)
        .filter_map(|(_, o)| match o {
            Out::Text(v) if v["t"] == "req" => Some(v),
            _ => None,
        })
        .collect()
}

fn one_req(app: &mut App) -> Value {
    let r = reqs(app);
    assert_eq!(r.len(), 1, "{r:#?}");
    r[0].clone()
}

fn res(app: &mut App, req: &Value, result: Value) {
    app.on_text("m1", &json!({"t":"res","id":req["id"],"result":result}));
}

fn refuse(app: &mut App, req: &Value, code: &str) {
    app.on_text(
        "m1",
        &json!({"t":"res","id":req["id"],"error":{"code":code,"message":"no"}}),
    );
}

fn screen(app: &App) -> String {
    let mut t = Terminal::new(TestBackend::new(app.size.0, app.size.1)).unwrap();
    t.draw(|f| view::draw(app, f)).unwrap();
    let buf = t.backend().buffer().clone();
    (0..buf.area.height)
        .map(|y| {
            (0..buf.area.width)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn favourites() -> Value {
    json!({ "favourites": [
        { "id": "f1", "name": "Morning", "shape": { "dir": "h", "ratio": 0.5, "a": { "slot": 0 }, "b": { "dir": "v", "ratio": 0.5, "a": { "slot": 1 }, "b": { "slot": 2 } } },
          "slots": [{ "holds": "agent", "agent": "claude" }, { "holds": "agent", "agent": "codex" }, { "holds": "shell" }] },
        { "id": "f2", "name": "Bad\u{7}name", "shape": { "slot": 0 }, "slots": [{ "holds": "any", "command": ["rm"] }] },
        { "id": "f3", "name": "Pair", "shape": { "dir": "h", "ratio": 0.5, "a": { "slot": 0 }, "b": { "slot": 1 } }, "slots": [{ "holds": "any" }, { "holds": "any" }] },
    ]})
}

#[test]
fn prefix_space_opens_the_popup_with_pictures_and_asks_for_favourites() {
    let mut a = app(4, (100, 30), "control");
    prefixed(&mut a, KeyCode::Char(' '));
    assert_eq!(a.mode, Mode::Arrange(0));
    assert!(a.layouts.fetch, "the event loop reads /api/me/layouts");
    let s = screen(&a);
    assert!(s.contains("Arrange 4 terminals"), "{s}");
    for w in [
        "1 Grid 2x2",
        "2 3 over 1",
        "3 Main + stack",
        "Reading your favourites…",
        "E Even out sizes",
    ] {
        assert!(s.contains(w), "{w} missing:\n{s}");
    }
    // The 2x2 grid in box characters.
    assert!(s.contains("┌─────┬─────┐"), "{s}");
    assert!(s.contains("├") && s.contains("┼") && s.contains("┤"), "{s}");
    a.on_favourites(Ok(favourites()));
    let s = screen(&a);
    assert!(s.contains("Morning · Claude, Codex, shell"), "{s}");
    assert!(s.contains("Pair"), "{s}");
    assert!(!s.contains("Bad"), "a row that does not read is left out:\n{s}");
}

#[test]
fn the_popup_fits_every_size_and_drops_pictures_when_short() {
    for (size, pictures) in [
        ((140, 40), true),
        ((100, 30), true),
        ((80, 24), true),
        ((80, 14), false),
        ((60, 16), false),
        ((44, 12), false),
    ] {
        let mut a = app(4, size, "control");
        prefixed(&mut a, KeyCode::Char(' '));
        a.on_favourites(Ok(favourites()));
        let s = screen(&a);
        assert!(s.contains("Arrange 4 terminals"), "{size:?}\n{s}");
        assert_eq!(s.contains("┼"), pictures, "{size:?}\n{s}");
        let listed = if size == (80, 14) {
            s.contains("▶ 1 Grid 2x2")
        } else {
            s.contains("Grid 2x2") || s.contains("No layout fits")
        };
        assert!(listed, "{size:?}\n{s}");
        for line in s.lines() {
            assert!(line.chars().count() <= size.0 as usize);
        }
    }
    // Tiny: no popup, no panic.
    let mut a = app(4, (24, 8), "control");
    prefixed(&mut a, KeyCode::Char(' '));
    let _ = screen(&a);
    // One pane: nothing to arrange, favourites still there.
    let mut a = app(1, (100, 30), "control");
    prefixed(&mut a, KeyCode::Char(' '));
    a.on_favourites(Ok(json!({"favourites": []})));
    let s = screen(&a);
    assert!(s.contains("One terminal: nothing to arrange yet."), "{s}");
    assert!(s.contains("None yet: save a layout"), "{s}");
}

#[test]
fn a_number_arranges_with_the_runners_wire_format_and_u_undoes() {
    let mut a = app(4, (100, 30), "control");
    prefixed(&mut a, KeyCode::Char(' '));
    a.on_key(key(KeyCode::Char('1')));
    assert_eq!(a.mode, Mode::Normal);
    let r = one_req(&mut a);
    assert_eq!(r["method"], "layout.arrange");
    let (w, h) = a.arrange_area();
    assert_eq!(
        r["params"],
        json!({"tab_id":"w1:t1","pattern":"grid_2x2","focus":"w1:p1","expect_rev":7,"area":{"width":w,"height":h}})
    );
    let root = json!({"type":"split","direction":"right","ratio":0.25,"first":{"type":"pane","pane_id":"w1:p1"},"second":{"type":"pane","pane_id":"w1:p2"}});
    res(
        &mut a,
        &r,
        json!({"layout":{},"pattern":"grid_2x2","opened":[],"extras":[],"extras_tab":null,"below_readable":[],"dry_run":false,"undo":{"root":root,"rev":8}}),
    );
    assert_eq!(a.note.as_deref(), Some("Arranged as Grid 2x2 · ^b Space u undoes"));
    prefixed(&mut a, KeyCode::Char(' '));
    assert!(screen(&a).contains("u Undo"));
    a.on_key(key(KeyCode::Char('u')));
    let u = one_req(&mut a);
    assert_eq!(u["params"], json!({"tab_id":"w1:t1","root":root,"expect_rev":8}));
    refuse(&mut a, &u, "changed");
    assert_eq!(
        a.note.as_deref(),
        Some("The layout has changed since, so there is nothing to undo.")
    );
    assert!(a.layouts.undo.is_none(), "an undo is used once");
}

#[test]
fn arrows_space_and_enter_pick_and_refusals_use_the_webs_words() {
    let mut a = app(3, (100, 30), "control");
    prefixed(&mut a, KeyCode::Char(' '));
    let items = a.arrange_items();
    assert_eq!(items[0], Pick::Pattern("columns"));
    a.on_key(key(KeyCode::Char(' ')));
    a.on_key(key(KeyCode::Right));
    a.on_key(key(KeyCode::Left));
    assert_eq!(a.mode, Mode::Arrange(1));
    assert!(screen(&a).contains("Enter: arrange as Main + stack"));
    a.on_key(key(KeyCode::Enter));
    let r = one_req(&mut a);
    assert_eq!(r["params"]["pattern"], "main_stack");
    refuse(&mut a, &r, "wont_fit");
    assert_eq!(
        a.note.as_deref(),
        Some("There is no room here for every terminal in that layout. Nothing was changed.")
    );
    // Esc closes and sends nothing.
    prefixed(&mut a, KeyCode::Char(' '));
    a.on_key(key(KeyCode::Esc));
    assert_eq!(a.mode, Mode::Normal);
    assert!(reqs(&mut a).is_empty());
}

#[test]
fn prefix_e_evens_out_and_one_pane_has_nothing_to_even() {
    let mut a = app(3, (100, 30), "control");
    prefixed(&mut a, KeyCode::Char('E'));
    let r = one_req(&mut a);
    assert_eq!(r["params"]["pattern"], "even");
    res(&mut a, &r, json!({"undo":null}));
    assert_eq!(a.note.as_deref(), Some("Arranged as Even out · ^b Space u undoes"));
    let mut a = app(1, (100, 30), "control");
    prefixed(&mut a, KeyCode::Char('E'));
    assert!(reqs(&mut a).is_empty());
    assert_eq!(a.note.as_deref(), Some("only one pane: nothing to even out"));
}

#[test]
fn watching_sends_nothing_and_says_take_control() {
    let mut a = app(4, (100, 30), "observe");
    prefixed(&mut a, KeyCode::Char(' '));
    a.on_key(key(KeyCode::Char('1')));
    assert!(reqs(&mut a).is_empty());
    assert_eq!(a.note.as_deref(), Some("You are watching. Take control to arrange."));
}

#[test]
fn a_favourite_previews_then_opens_whats_missing_and_arranges_without_closing() {
    let mut a = app(4, (100, 30), "control");
    prefixed(&mut a, KeyCode::Char(' '));
    a.on_favourites(Ok(favourites()));
    // Four pictures, then the favourites.
    let first = a.arrange_items().iter().position(|p| *p == Pick::Favourite(0)).unwrap();
    for _ in 0..first {
        a.on_key(key(KeyCode::Tab));
    }
    let s = screen(&a);
    assert!(s.contains("Reuses 2 terminals · opens 1: Codex"), "{s}");
    assert!(
        s.contains("2 other terminals move to a new tab") && s.contains("Nothing is") && s.contains("closed."),
        "{s}"
    );
    assert!(s.contains("Enter: Open 1 agent and arrange"), "{s}");
    a.on_key(key(KeyCode::Enter));
    // The roomiest terminal is split, the first in reading order on a tie (60x20 is twice as wide as tall: right).
    let split = one_req(&mut a);
    assert_eq!(split["method"], "pane.split");
    assert_eq!(
        split["params"],
        json!({"target_pane_id":"w1:p1","direction":"right","focus":false})
    );
    // Busy: nothing else may arrange meanwhile.
    prefixed(&mut a, KeyCode::Char('E'));
    assert!(reqs(&mut a).is_empty());
    res(&mut a, &split, json!({"pane":{"pane_id":"w1:p9"}}));
    let start = one_req(&mut a);
    assert_eq!(start["method"], "agent.start");
    assert_eq!(start["params"]["pane_id"], "w1:p9");
    assert_eq!(start["params"]["kind"], "codex");
    refuse(&mut a, &start, "not_signed_in");
    let arr = one_req(&mut a);
    assert_eq!(arr["method"], "layout.arrange");
    let (w, h) = a.arrange_area();
    assert_eq!(
        arr["params"],
        json!({"tab_id":"w1:t1","extras":"new_tab","area":{"width":w,"height":h},"root":{
            "type":"split","direction":"right","ratio":0.5,"first":{"type":"pane","pane_id":"w1:p2"},
            "second":{"type":"split","direction":"down","ratio":0.5,"first":{"type":"pane","pane_id":"w1:p9"},"second":{"type":"pane","pane_id":"w1:p1"}}}})
    );
    res(
        &mut a,
        &arr,
        json!({"extras":["w1:p3","w1:p4"],"extras_tab":"w1:t2","undo":{"root":{"type":"pane","pane_id":"w1:p1"},"rev":9}}),
    );
    assert_eq!(
        a.note.as_deref(),
        Some("Arranged as “Morning” · 1 terminal was opened · 2 terminals moved to “Extras”. Codex could not be started; that terminal is a shell.")
    );
    assert!(a.layouts.restore.is_none());
    assert!(a.layouts.undo.is_some());
    let all: Vec<Value> = a
        .out
        .drain(..)
        .filter_map(|(_, o)| if let Out::Text(v) = o { Some(v) } else { None })
        .collect();
    assert!(!all
        .iter()
        .any(|v| v["method"] == "pane.close" || v["method"] == "layout.apply"));
}

#[test]
fn a_restore_that_cannot_open_says_so_and_stops() {
    let mut a = app(1, (100, 30), "control");
    prefixed(&mut a, KeyCode::Char(' '));
    a.on_favourites(Ok(favourites()));
    a.on_key(key(KeyCode::Down));
    a.on_key(key(KeyCode::Enter));
    let split = one_req(&mut a);
    assert_eq!(split["params"]["target_pane_id"], "w1:p1");
    assert_eq!(split["params"]["direction"], "right");
    refuse(&mut a, &split, "too_small");
    assert_eq!(
        a.note.as_deref(),
        Some("A terminal for “Pair” could not be opened, so the layout was not changed.")
    );
    assert!(reqs(&mut a).is_empty());
    assert!(a.layouts.restore.is_none());
    // A machine that drops mid-restore ends it.
    prefixed(&mut a, KeyCode::Char(' '));
    a.on_key(key(KeyCode::Down));
    a.on_key(key(KeyCode::Enter));
    let split = one_req(&mut a);
    res(&mut a, &split, json!({"pane":{"pane_id":"w1:p5"}}));
    let _arrange = one_req(&mut a);
    a.on_link_down("m1");
    assert_eq!(
        a.note.as_deref(),
        Some("The machine is not connected. 1 terminal was opened; nothing was closed.")
    );
}

#[test]
fn favourites_unreadable_say_so() {
    let mut a = app(2, (100, 30), "control");
    prefixed(&mut a, KeyCode::Char(' '));
    a.on_favourites(Err("offline: no route".into()));
    assert!(screen(&a).contains("Favourites could not be read. Try again later."));
}

#[test]
fn box_art_joins_edges() {
    let grid = [
        [0.0, 0.0, 0.5, 0.5],
        [0.5, 0.0, 0.5, 0.5],
        [0.0, 0.5, 0.5, 0.5],
        [0.5, 0.5, 0.5, 0.5],
    ];
    assert_eq!(box_art(&grid, 5, 3), vec!["┌─┬─┐", "├─┼─┤", "└─┴─┘"]);
    let main = [[0.0, 0.0, 0.5, 1.0], [0.5, 0.0, 0.5, 0.5], [0.5, 0.5, 0.5, 0.5]];
    assert_eq!(box_art(&main, 5, 3), vec!["┌─┬─┐", "│ ├─┤", "└─┴─┘"]);
}

/// The restore plans `packages/layouts` is held to, read by the TUI's matcher.
#[test]
fn restore_plans_match_the_shared_fixture() {
    let fx: Value = serde_json::from_str(include_str!("../../../tests/fixtures/favourites.json")).unwrap();
    let cases = fx["cases"].as_array().unwrap();
    assert!(cases.len() >= 6);
    for c in cases {
        let n = c["slots"].as_array().unwrap().len();
        let row = json!({ "id": "x", "name": "x", "shape": { "slot": 0 }, "slots": c["slots"] });
        // The fixture's slots read; the shape is irrelevant to the plan.
        let slots = fav::parse_favourite(&json!({ "id": "x", "name": "x", "shape": shape(n), "slots": row["slots"] }))
            .unwrap()
            .slots;
        let panes: Vec<OpenPane> = c["panes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| OpenPane {
                id: p["id"].as_str().unwrap().into(),
                agent: p["agent"].as_str().map(Into::into),
                label: p["label"].as_str().map(Into::into),
                cwd: p.get("cwd").map(|c| fav::SlotCwd {
                    base: c["base"].as_str().unwrap().into(),
                    path: c["path"].as_str().unwrap().into(),
                }),
            })
            .collect();
        let plan = fav::plan_restore(&slots, &panes);
        let want = &c["expect"];
        assert_eq!(json!(plan.assigned), want["assigned"], "{}", c["name"]);
        assert_eq!(json!(plan.missing), want["missing"], "{}", c["name"]);
        assert_eq!(json!(plan.extras), want["extras"], "{}", c["name"]);
    }
}

fn shape(n: usize) -> Value {
    (1..n).fold(
        json!({ "slot": 0 }),
        |acc, i| json!({ "dir": "h", "ratio": 0.5, "a": acc, "b": { "slot": i } }),
    )
}

#[test]
fn hostile_favourites_are_left_out() {
    let deep = (0..20).fold(
        json!({ "slot": 0 }),
        |acc, _| json!({ "dir": "v", "ratio": 0.5, "a": acc, "b": { "slot": 1 } }),
    );
    for row in [
        json!({ "id": "a", "name": "x", "shape": deep, "slots": [{ "holds": "any" }, { "holds": "any" }] }),
        json!({ "id": "a", "name": "x", "shape": { "slot": 1 }, "slots": [{ "holds": "any" }] }),
        json!({ "id": "a", "name": "x", "shape": { "slot": 0 }, "slots": [{ "holds": "agent" }] }),
        json!({ "id": "a", "name": "x", "shape": { "slot": 0 }, "slots": [{ "holds": "shell", "env": {} }] }),
        json!({ "id": "a b", "name": "x", "shape": { "slot": 0 }, "slots": [{ "holds": "any" }] }),
        json!({ "id": "a", "name": "", "shape": { "slot": 0 }, "slots": [{ "holds": "any" }] }),
        json!({ "id": "a", "name": "x", "shape": { "dir": "h", "ratio": 1.5, "a": { "slot": 0 }, "b": { "slot": 1 } }, "slots": [{ "holds": "any" }, { "holds": "any" }] }),
    ] {
        assert!(fav::parse_favourite(&row).is_none(), "{row}");
    }
}
