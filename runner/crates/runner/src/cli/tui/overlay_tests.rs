//! The prefix overlay (queue 3j) in the TUI: TestBackend snapshots of the
//! popup (groups, keys, greyed reasons, footer), keys with it up, the
//! setting's delay and off modes, "don't show again", `prefix ?`.

use super::super::overlay::{OverlayMode, DELAY};
use super::*;

fn overlay_app(scope: &str) -> App {
    let mut app = app_with(scope);
    app.size = (120, 30);
    fill_screens(&mut app);
    texts(&mut app);
    app
}

fn screen(app: &App) -> String {
    render(app).0.join("\n")
}

/// On by default: the prefix opens a popup with every action grouped, the
/// 3c rename keys and the TUI's own keys, each with its key; Esc cancels.
#[test]
fn the_prefix_opens_the_overlay_with_every_action_and_esc_cancels() {
    let mut app = overlay_app("control");
    assert_eq!(app.keymap.overlay, OverlayMode::On, "on by default");
    app.on_key(ctrl_b());
    assert!(app.overlay_shown(std::time::Instant::now()));
    let s = screen(&app);
    for want in [
        "^b then…",
        "Panes",
        "Tabs and zoom",
        "Names",
        "View",
        "  % split side by side",
        "  \" split stacked",
        "  ← focus left",
        "  x close pane",
        "  [ scroll back",
        "  c new tab",
        "  n next tab",
        "1-9 go to tab",
        "  z zoom pane",
        "  , rename tab",
        "  $ rename workspace",
        "  P rename pane",
        "  w workspaces",
        "  S agent settings",
        "  d leave (agents keep running)",
        "  ? all keys",
        "^b again: send ^b to the pane · Esc cancel · ! don't show this again",
    ] {
        assert!(s.contains(want), "no {want:?} in\n{s}");
    }
    assert!(status_line(&app).contains("^b: press an action key (Esc cancels)"));
    // Context: focus is on w1:p1 (the left half); nothing to its left, a pane to its right.
    assert!(s.contains("  ← focus left (no pane left)"), "{s}");
    assert!(s.contains("  → focus right") && !s.contains("focus right ("), "{s}");
    app.on_key(key(KeyCode::Esc));
    assert_eq!(app.mode, Mode::Normal);
    assert!(!screen(&app).contains("then…"));
    assert!(inputs(&mut app).is_empty(), "Esc sends nothing");
}

/// Keys work as before with the overlay up: an action runs and closes it, a
/// second prefix sends the prefix to the pane.
#[test]
fn overlay_keys_run_actions_and_a_double_prefix_is_literal() {
    let mut app = overlay_app("control");
    prefixed(&mut app, KeyCode::Char('%'));
    let out = texts(&mut app);
    assert_eq!(out[0]["method"], "pane.split");
    assert_eq!(app.mode, Mode::Normal);
    assert!(!screen(&app).contains("then…"));
    app.on_key(ctrl_b());
    app.on_key(ctrl_b());
    assert_eq!(inputs(&mut app), vec![("w1:p1".to_string(), vec![0x02])]);
    assert!(!screen(&app).contains("then…"));
}

/// Observing: layout changes and renames are greyed out with the reason.
#[test]
fn an_observer_sees_what_needs_control_greyed_out() {
    let mut app = overlay_app("observe");
    app.on_key(ctrl_b());
    let (lines, buf) = render(&app);
    let s = lines.join("\n");
    assert!(s.contains("  % split side by side (take control first)"), "{s}");
    assert!(s.contains("  , rename tab (take control first)"), "{s}");
    let cell = |needle: &str| {
        let (y, line) = lines.iter().enumerate().find(|(_, l)| l.contains(needle)).unwrap();
        let x = line[..line.find(needle).unwrap()].chars().count() as u16 + 2;
        buf[(x, y as u16)].fg
    };
    assert_eq!(cell("  % split side by side"), Color::DarkGray, "greyed out");
    assert_eq!(cell("  w workspaces"), Color::Yellow, "available");
}

/// The user's own keys; a TUI key an action took is not listed twice.
#[test]
fn the_overlay_shows_custom_bindings() {
    let mut app = overlay_app("control");
    app.keymap = super::super::keymap::Keymap::from_json(&json!({
        "version": 1, "prefix": "ctrl+a", "isDefault": false, "overlay": "on",
        "bindings": {"split_right": "|", "split_down": "-", "detach": "w", "rename_pane": "?"}
    }));
    app.on_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
    let s = screen(&app);
    assert!(s.contains("^a then…"), "{s}");
    assert!(
        s.contains("  | split side by side") && s.contains("  - split stacked"),
        "{s}"
    );
    assert!(s.contains("  w leave"), "{s}");
    assert!(!s.contains("w workspaces"), "an action shadows the TUI's own w: {s}");
    assert!(s.contains("  ? rename pane") && !s.contains("all keys"), "{s}");
    assert!(s.contains("send ^a to the pane"), "{s}");
}

/// After a short delay: nothing for 700 ms, then the overlay; off: never.
#[test]
fn delay_and_off_settings() {
    let mut app = overlay_app("control");
    app.keymap = super::super::keymap::Keymap::from_json(&json!({"version": 1, "overlay": "delay"}));
    assert_eq!(app.keymap.overlay, OverlayMode::Delay);
    app.on_key(ctrl_b());
    let t0 = app.prefix_at.unwrap();
    assert!(!app.overlay_shown(t0 + DELAY / 2));
    assert!(app.overlay_shown(t0 + DELAY));
    assert!(!screen(&app).contains("then…"), "not yet");
    app.prefix_at = Some(t0 - DELAY);
    assert!(screen(&app).contains("^b then…"));
    app.on_key(key(KeyCode::Esc));

    app.keymap = super::super::keymap::Keymap::from_json(&json!({"version": 1, "overlay": "off"}));
    app.on_key(ctrl_b());
    app.prefix_at = Some(t0 - DELAY * 10);
    assert!(!app.overlay_shown(std::time::Instant::now()));
    assert!(!screen(&app).contains("then…"));
    assert!(
        status_line(&app).contains("^b: c tab"),
        "the hint instead: {}",
        status_line(&app)
    );
    // An older server (no `overlay`), or a value from a newer one: on.
    assert_eq!(
        super::super::keymap::Keymap::from_json(&json!({"version": 1})).overlay,
        OverlayMode::On
    );
    assert_eq!(
        super::super::keymap::Keymap::from_json(&json!({"version": 1, "overlay": "bogus"})).overlay,
        OverlayMode::On
    );
}

/// `!` in the overlay turns it off for the account (one PUT) and says where to turn it back on.
#[test]
fn dont_show_again_turns_the_overlay_off() {
    let mut app = overlay_app("control");
    prefixed(&mut app, KeyCode::Char('!'));
    assert_eq!(app.keymap.overlay, OverlayMode::Off);
    assert_eq!(app.settings_out, vec![json!({"mode": "off"})]);
    assert!(app.note.as_deref().unwrap().contains("Settings → Terminal"));
    assert!(inputs(&mut app).is_empty());
    app.settings_out.clear();
    // With the overlay off, `!` after the prefix is just an unbound key.
    prefixed(&mut app, KeyCode::Char('!'));
    assert!(app.settings_out.is_empty());
}

/// `prefix ?` shows every binding; a key there runs its action; Esc closes.
#[test]
fn prefix_question_mark_lists_all_keys() {
    let mut app = overlay_app("control");
    prefixed(&mut app, KeyCode::Char('?'));
    assert_eq!(app.mode, Mode::Keys);
    let s = screen(&app);
    assert!(s.contains("all keys (Esc closes)"), "{s}");
    assert!(s.contains("  , rename tab") && s.contains("  z zoom pane"), "{s}");
    assert!(!s.contains("don't show this again"), "{s}");
    app.on_key(key(KeyCode::Esc));
    assert_eq!(app.mode, Mode::Normal);
    prefixed(&mut app, KeyCode::Char('?'));
    app.on_key(key(KeyCode::Char('c')));
    assert_eq!(texts(&mut app)[0]["method"], "tab.create");
}

/// A narrow terminal stacks the groups in fewer columns and still fits.
#[test]
fn the_overlay_fits_a_narrow_terminal() {
    let mut app = overlay_app("control");
    app.size = (70, 40);
    app.on_key(ctrl_b());
    let s = screen(&app);
    assert!(s.contains("Panes") && s.contains("Names") && s.contains("View"), "{s}");
    assert!(s.contains("  , rename tab"), "{s}");
}
