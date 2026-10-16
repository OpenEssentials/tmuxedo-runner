//! Needs you in the TUI (super::needs; PLAN Phase 13): the sidebar count, the
//! `prefix !` / `prefix N` list (TestBackend), answers as `req needs.respond`
//! (approve, deny, a choice, a reply), high risk refused here, outcomes on the
//! status line, and an unverified open_url never opened.

use super::app::{App, Mode, Out};
use super::view;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use serde_json::{json, Value};

fn fx() -> Value {
    let p = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../packages/protocol/fixtures/needs.json"
    );
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

fn topology() -> Value {
    let pane = |id: &str| json!({"id":id,"tabId":"w1:t1","cwd":"/","command":"claude","title":format!("t-{id}"),"cols":80,"rows":24,"agent":{"kind":"claude","status":"blocked","since":"t","source":"reported"}});
    json!({
        "t": "topology", "rev": 1,
        "workspaces": [{"id":"w1","name":"default","cwd":"/","position":0}],
        "tabs": [{"id":"w1:t1","workspaceId":"w1","name":"main","position":0,"layout":{"dir":"h","ratio":0.5,"a":{"pane":"w1:p1"},"b":{"pane":"w1:p2"}}}],
        "panes": [pane("w1:p1"), pane("w1:p2")]
    })
}

fn app() -> App {
    let mut app = App::new(true, (120, 14));
    app.on_machines(&[json!({"id":"m1","name":"box","online":true})]);
    app.select_initial(None);
    app.on_text(
        "m1",
        &json!({"t":"welcome","sub":"u1","mid":"m1","scope":"control","machine":{"online":true,"hello":{"v":1,"runner":{"version":"1","features":["needs"]}}}}),
    );
    app.on_text("m1", &topology());
    let f = fx();
    let v = &f["items"]["valid"];
    app.on_text(
        "m1",
        &json!({"t":"needs","paneId":"w1:p1","items":[v["permission_hook"], v["permission_high"]]}),
    );
    app.on_text(
        "m1",
        &json!({"t":"needs","paneId":"w1:p2","items":[v["question_keystroke"]]}),
    );
    app
}

fn key(c: KeyCode) -> KeyEvent {
    KeyEvent::new(c, KeyModifiers::NONE)
}

fn prefixed(app: &mut App, c: KeyCode) {
    app.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
    app.on_key(key(c));
}

fn sent(app: &mut App) -> Vec<Value> {
    app.out
        .drain(..)
        .filter_map(|(_, o)| match o {
            Out::Text(v) => Some(v),
            Out::Data(_) => None,
        })
        .collect()
}

fn render(app: &App) -> Vec<String> {
    let mut t = Terminal::new(TestBackend::new(app.size.0, app.size.1)).unwrap();
    t.draw(|f| view::draw(app, f)).unwrap();
    let buf = t.backend().buffer().clone();
    (0..buf.area.height)
        .map(|y| {
            (0..buf.area.width)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect::<String>()
        })
        .collect()
}

#[test]
fn the_sidebar_counts_and_the_list_shows_every_item() {
    let mut a = app();
    let screen = render(&a).join("\n");
    assert!(screen.contains("box ■ 3"), "{screen}");
    a.keymap.overlay = super::overlay::OverlayMode::Off;
    prefixed(&mut a, KeyCode::Char('!'));
    assert_eq!(a.mode, Mode::Needs(0));
    let screen = render(&a).join("\n");
    assert!(screen.contains("Needs you"), "{screen}");
    assert!(screen.contains("Run npm test"), "{screen}");
    assert!(screen.contains("⚠ high risk"), "{screen}");
    assert!(screen.contains("Which colour should the button be?"), "{screen}");
    assert!(screen.contains("1 Red · 2 Blue · 3 Green"), "{screen}");
    // `N` opens it with the overlay up too; Esc closes.
    a.on_key(key(KeyCode::Esc));
    a.keymap.overlay = super::overlay::OverlayMode::On;
    prefixed(&mut a, KeyCode::Char('N'));
    assert_eq!(a.mode, Mode::Needs(0));
}

#[test]
fn answers_go_as_needs_respond_and_outcomes_say_so() {
    let mut a = app();
    a.mode = Mode::Needs(0);
    sent(&mut a);
    a.on_key(key(KeyCode::Char('a')));
    let s = sent(&mut a);
    assert_eq!(s.len(), 1);
    assert_eq!(s[0]["method"], "needs.respond");
    assert_eq!(
        s[0]["params"],
        json!({"itemId": "p-6f1c2a9b", "paneId": "w1:p1", "responseId": "approve", "by": "TUI"})
    );
    let id = s[0]["id"].clone();
    a.on_text(
        "m1",
        &json!({"t":"res","id":id,"result":{"type":"needs_respond","outcome":"accepted"}}),
    );
    assert_eq!(a.note.as_deref(), Some("answered: Run npm test"));

    // High risk: never approved from here.
    a.mode = Mode::Needs(0);
    a.on_key(key(KeyCode::Down));
    a.on_key(key(KeyCode::Char('a')));
    assert!(sent(&mut a).is_empty());
    assert!(a
        .note
        .as_deref()
        .unwrap()
        .contains("approve it on the web or in the app"));
    // ...but denied with a reply.
    a.mode = Mode::Needs(1);
    a.on_key(key(KeyCode::Char('r')));
    for c in "not to main".chars() {
        a.on_key(key(KeyCode::Char(c)));
    }
    a.on_key(key(KeyCode::Enter));
    let s = sent(&mut a);
    assert_eq!(s[0]["params"]["responseId"], "deny");
    assert_eq!(s[0]["params"]["text"], "not to main");

    // A question: a choice by its digit; one that is not offered is refused here.
    a.mode = Mode::Needs(2);
    a.on_key(key(KeyCode::Char('7')));
    assert!(sent(&mut a).is_empty());
    a.on_key(key(KeyCode::Char('2')));
    let s = sent(&mut a);
    assert_eq!(
        s[0]["params"],
        json!({"itemId": "q-41aa90c2", "paneId": "w1:p2", "responseId": "choose", "optionId": "2", "by": "TUI"})
    );
    let id = s[0]["id"].clone();
    a.on_text(
        "m1",
        &json!({"t":"res","id":id,"result":{"type":"needs_respond","outcome":"stale"}}),
    );
    assert!(
        a.note.as_deref().unwrap().contains("the screen changed"),
        "{:?}",
        a.note
    );
    // A refusal from the gateway or runner reads plainly.
    a.mode = Mode::Needs(2);
    a.on_key(key(KeyCode::Char('d')));
    let id = sent(&mut a)[0]["id"].clone();
    a.on_text(
        "m1",
        &json!({"t":"res","id":id,"error":{"code":"keystroke_off","message":"off"}}),
    );
    assert!(a.note.as_deref().unwrap().contains("answering by keys is off"));
}

#[test]
fn sets_follow_the_frames_and_enter_goes_to_the_pane() {
    let mut a = app();
    a.on_text("m1", &json!({"t":"needs","paneId":"w1:p1","items":[]}));
    assert_eq!(a.needs_items().len(), 1);
    a.mode = Mode::Needs(0);
    a.on_key(key(KeyCode::Enter));
    assert_eq!(a.mode, Mode::Normal);
    // A new welcome (the gateway replays the current sets): what was kept is dropped.
    a.on_text(
        "m1",
        &json!({"t":"welcome","sub":"u1","mid":"m1","scope":"control","machine":{"online":true}}),
    );
    assert!(a.needs_items().is_empty());
    a.keymap.overlay = super::overlay::OverlayMode::Off;
    prefixed(&mut a, KeyCode::Char('!'));
    assert_eq!(a.mode, Mode::Normal);
    assert_eq!(a.note.as_deref(), Some("Nothing needs you on this machine."));
}

#[test]
fn an_unverified_link_is_never_opened() {
    let mut a = app();
    a.on_text(
        "m1",
        &json!({"t":"event","seq":0,"paneId":"w1:p1","type":"open_url","at":"t",
        "data":{"url":"https://claude-login.example/oauth","verified":false,"reason":"not_from_agent"}}),
    );
    assert!(a.open_urls.is_empty());
    let n = a.note.clone().unwrap();
    assert!(
        n.contains("Unverified link to claude-login.example from a process in pane w1:p1"),
        "{n}"
    );
    // A verified one (or one from an older runner) still opens.
    a.on_text(
        "m1",
        &json!({"t":"event","seq":0,"paneId":"w1:p1","type":"open_url","at":"t",
        "data":{"url":"https://claude.ai/oauth/authorize","verified":true,"plugin":"claude"}}),
    );
    assert_eq!(a.open_urls, vec!["https://claude.ai/oauth/authorize".to_string()]);
}
