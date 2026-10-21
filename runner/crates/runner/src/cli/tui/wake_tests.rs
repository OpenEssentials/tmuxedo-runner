//! `prefix W` and the kept-awake mark (super::wake): what `/api/machines`
//! gives becomes the sidebar ◐, the offline line and the wake request; the
//! answer becomes the status line. No request leaves the test.

use super::app::App;
use super::view;
use super::wake::{self, NO_RELAY_TEXT};
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use serde_json::json;

fn app(items: serde_json::Value) -> App {
    let mut a = App::new(true, (100, 24));
    a.on_machines(items.as_array().unwrap());
    a.select_initial(Some("m1"));
    a
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
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn prefix_w(a: &mut App) {
    a.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
    a.on_key(KeyEvent::new(KeyCode::Char('W'), KeyModifiers::NONE));
}

#[test]
fn facts_from_the_machines_list() {
    let it = json!({"online": true, "keepAwake": true, "keepAwakeAvailable": true, "agentsWorking": 2});
    assert_eq!(wake::kept_awake(&it), Some(2));
    assert_eq!(
        wake::kept_awake(&json!({"online": true, "keepAwake": true, "keepAwakeAvailable": true, "agentsWorking": 0})),
        None
    );
    assert_eq!(
        wake::kept_awake(&json!({"online": true, "keepAwake": false, "keepAwakeAvailable": true, "agentsWorking": 2})),
        None
    );
    assert_eq!(
        wake::kept_awake(&json!({"online": true, "keepAwake": true, "keepAwakeAvailable": null, "agentsWorking": 2})),
        None
    );
    assert_eq!(
        wake::kept_awake(&json!({"online": false, "keepAwake": true, "keepAwakeAvailable": true, "agentsWorking": 2})),
        None
    );
    assert_eq!(
        wake::wake_via(&json!({"wake": {"relay": {"id": "r", "name": "nas\u{1b}[2J"}}})).as_deref(),
        Some("nas [2J")
    );
    assert_eq!(
        wake::wake_via(&json!({"wake": {"relay": null, "block": "no_relay"}})),
        None
    );
    assert!(wake::wake_blocked(
        &json!({"wake": {"relay": null, "block": "no_relay"}})
    ));
    assert!(!wake::wake_blocked(&json!({})));
    assert_eq!(wake::path("m1"), "/api/v1/machines/m1/wake");
    assert_eq!(wake::path("a b/c"), "/api/v1/machines/a%20b%2Fc/wake");
}

#[test]
fn the_sidebar_marks_a_machine_kept_awake() {
    let a = app(json!([
        {"id": "m1", "name": "box", "online": true, "keepAwake": true, "keepAwakeAvailable": true, "agentsWorking": 3},
        {"id": "m2", "name": "quiet", "online": true, "keepAwake": true, "keepAwakeAvailable": true, "agentsWorking": 0}
    ]));
    let s = screen(&a);
    assert!(s.contains("box ◐3"), "{s}");
    assert!(!s.contains("quiet ◐"), "{s}");
}

#[test]
fn prefix_w_wakes_an_offline_machine_through_its_relay() {
    let mut a = app(
        json!([{"id": "m1", "name": "box", "online": false, "kind": "byo", "wake": {"relay": {"id": "r", "name": "nas"}}}]),
    );
    assert!(screen(&a).contains("machine offline: prefix W wakes it via nas"));
    prefix_w(&mut a);
    assert_eq!(a.wake_out, vec!["m1".to_string()]);
    assert_eq!(a.note.as_deref(), Some("Sending the wake signal via nas…"));
    a.on_wake("m1", Ok("nas".into()));
    assert_eq!(a.note.as_deref(), Some("Wake signal sent via nas… waiting for box"));
    a.on_wake("m1", Err("No machine on the same network is online.".into()));
    assert_eq!(
        a.note.as_deref(),
        Some("box: No machine on the same network is online.")
    );
}

#[test]
fn prefix_w_explains_when_nothing_can_wake_it_and_does_nothing_online() {
    let mut a = app(
        json!([{"id": "m1", "name": "box", "online": false, "kind": "byo", "wake": {"relay": null, "block": "no_relay"}}]),
    );
    assert!(screen(&a).contains(&format!("machine offline: {NO_RELAY_TEXT}")));
    prefix_w(&mut a);
    assert!(a.wake_out.is_empty());
    assert!(a
        .note
        .as_deref()
        .unwrap()
        .contains("wake relay can be set up in its settings"));

    let mut a = app(json!([{"id": "m1", "name": "box", "online": false}]));
    assert!(screen(&a).contains("machine offline: waiting for it"));
    prefix_w(&mut a);
    assert!(a.wake_out.is_empty());

    let mut a = app(json!([{"id": "m1", "name": "box", "online": true}]));
    prefix_w(&mut a);
    assert!(a.wake_out.is_empty());
    assert_eq!(a.note.as_deref(), Some("box is already online"));
}
