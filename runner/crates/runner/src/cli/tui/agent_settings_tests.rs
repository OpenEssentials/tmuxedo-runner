//! `prefix S` (super::agent_settings, Phase 17 box 4): the read-only view of
//! what each agent gets on the selected machine. TestBackend snapshots of
//! the loaded, empty and failed states, the request the event loop sends,
//! `e` opening the web page, scrolling and closing.

use super::agent_settings::{self, launch_summary, Load};
use super::app::{App, Mode};
use super::view;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::Color;
use ratatui::Terminal;
use serde_json::{json, Value};

fn app() -> App {
    let mut a = App::new(true, (100, 24));
    a.on_machines(&[json!({"id":"m1","name":"box","online":true})]);
    a.select_initial(None);
    a.web_url = "https://example.test/".into();
    a
}

fn key(c: KeyCode) -> KeyEvent {
    KeyEvent::new(c, KeyModifiers::NONE)
}

fn open(a: &mut App) {
    a.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
    a.on_key(key(KeyCode::Char('S')));
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

fn decision(kind: &str, name: &str, enabled: bool, shadowed: Option<&str>) -> Value {
    json!({"id": format!("{kind}-{name}"), "kind": kind, "name": name, "version": 1,
           "definedAt": {"scope":"user","key":""}, "enabled": enabled,
           "decidedAt": null, "shadowedBy": shadowed})
}

fn effective() -> Value {
    json!({"machineId":"m1","projectKey":null,"plugins":[
        {"plugin":"claude","items":["mcp-github"],
         "values":{"model":"sonnet","args":["--verbose","--x"],"skipPermissions":true,"extras":false},
         "decisions":[decision("mcp","github",true,None), decision("mcp","linear",false,None),
                      decision("mcp","old",false,Some("x")), decision("instructions","house\u{1b}[2Jrules",true,None)]},
        {"plugin":"codex","items":[],"values":{},"decisions":[]},
        {"plugin":"cursor","items":[],"values":{},"decisions":[decision("mcp","linear",false,None)]}
    ]})
}

#[test]
fn prefix_s_opens_the_view_and_asks_for_the_selected_machine() {
    let mut a = app();
    open(&mut a);
    assert_eq!(a.mode, Mode::AgentSettings(0));
    assert_eq!(a.agent_settings_out, vec!["m1".to_string()]);
    assert_eq!(agent_settings::path("m1"), "/api/machines/m1/plugins/effective");
    assert_eq!(
        agent_settings::path("a b/c"),
        "/api/machines/a%20b%2Fc/plugins/effective"
    );
    let s = render(&a).0.join("\n");
    assert!(s.contains("Agent settings on box (read only)"), "{s}");
    assert!(s.contains("Loading…"), "{s}");
    assert!(s.contains("agent settings (read only): e edit on the web"), "{s}");
}

#[test]
fn the_loaded_view_shows_launch_items_and_the_dangerous_switch() {
    let mut a = app();
    open(&mut a);
    a.on_agent_settings("other", Ok(json!({"plugins": []})));
    assert_eq!(
        a.agent_settings.as_ref().unwrap().load,
        Load::Loading,
        "another machine's answer is ignored"
    );
    a.on_agent_settings("m1", Ok(effective()));
    let (lines, buf) = render(&a);
    let s = lines.join("\n");
    for want in [
        "Claude Code",
        "  Starts with: 2 extra arguments · Model: sonnet",
        "  ! Dangerous: Never ask for permission",
        "  MCP servers: github · 1 off",
        "  Instructions: house [2Jrules",
        "Codex",
        "  Nothing added: Codex starts with only its own setup.",
        "Cursor Agent",
        "  Starts as it would on its own.",
        "  MCP servers: none on · 1 off",
        "A project can change these. Press e to see or change them on the web.",
        "e edit on the web · r reload · ↑↓ scroll · Esc closes",
    ] {
        assert!(s.contains(want), "no {want:?} in\n{s}");
    }
    assert!(!s.contains("old"), "a shadowed item is not listed: {s}");
    assert!(
        !s.contains("extras"),
        "settings the runner does not read here are not shown"
    );
    let y = lines.iter().position(|l| l.contains("! Dangerous")).unwrap() as u16;
    let x = lines[y as usize].find("! Dangerous").unwrap() as u16;
    assert_eq!(buf[(x, y)].fg, Color::Red);
}

#[test]
fn a_failure_says_so_and_r_tries_again() {
    let mut a = app();
    open(&mut a);
    a.agent_settings_out.clear();
    a.on_agent_settings("m1", Err("offline: timed out".into()));
    let s = render(&a).0.join("\n");
    assert!(s.contains("Could not load: offline: timed out"), "{s}");
    a.on_key(key(KeyCode::Char('r')));
    assert_eq!(a.agent_settings_out, vec!["m1".to_string()]);
    assert_eq!(a.agent_settings.as_ref().unwrap().load, Load::Loading);
}

#[test]
fn e_opens_the_web_page_and_esc_closes() {
    let mut a = app();
    open(&mut a);
    a.on_key(key(KeyCode::Char('e')));
    assert_eq!(a.mode, Mode::Normal);
    assert_eq!(a.open_urls, vec!["https://example.test/settings/plugins".to_string()]);
    assert!(a.agent_settings.is_none());
    open(&mut a);
    a.on_agent_settings("m1", Ok(effective()));
    a.on_key(key(KeyCode::Down));
    a.on_key(key(KeyCode::Down));
    assert_eq!(a.mode, Mode::AgentSettings(2));
    let s = render(&a).0.join("\n");
    assert!(!s.contains("Claude Code"), "scrolled past the first rows: {s}");
    a.on_key(key(KeyCode::Up));
    assert_eq!(a.mode, Mode::AgentSettings(1));
    a.on_key(key(KeyCode::Esc));
    assert_eq!(a.mode, Mode::Normal);
    assert!(a.agent_settings.is_none());
    assert!(!render(&a).0.join("\n").contains("Agent settings on"));
}

#[test]
fn without_a_server_e_says_where_to_go() {
    let mut a = app();
    a.web_url.clear();
    open(&mut a);
    a.on_key(key(KeyCode::Char('e')));
    assert!(a.open_urls.is_empty());
    assert_eq!(
        a.note.as_deref(),
        Some("Open Settings → Plugins on the web to change these.")
    );
}

#[test]
fn launch_summary_follows_each_builtin_schema() {
    assert_eq!(launch_summary("claude", &json!({})), (String::new(), vec![]));
    let (text, danger) = launch_summary("codex", &json!({"bypassApprovals": true, "args": ["-q"]}));
    assert_eq!(text, "1 extra argument");
    assert_eq!(danger, vec!["Never ask and no sandbox".to_string()]);
    let (_, danger) = launch_summary("cursor", &json!({"force": true, "keystroke": true}));
    assert_eq!(danger, vec!["Never ask before commands".to_string()]);
    assert_eq!(agent_settings::agent_label("unknown"), "unknown");
}

#[test]
fn the_add_ons_on_the_machine_are_listed_with_where_each_sends() {
    let mut a = app();
    open(&mut a);
    let mut v = effective();
    v["addons"] = json!([
        {"name":"example-telemetry","title":"Example telemetry","from":"workspace",
         "sendsTo":["https://telemetry.example.com","http://192.0.2.10:8080"],"agents":[]},
        {"name":"local-notes","title":"Local\u{1b}[2J notes","from":"machine","sendsTo":[],"agents":[]}
    ]);
    a.on_agent_settings("m1", Ok(v));
    let rows: Vec<String> = agent_settings::rows(a.agent_settings.as_ref().unwrap())
        .into_iter()
        .map(|(_, t)| t)
        .collect();
    let s = rows.join("\n");
    for want in [
        "Add-ons on this machine",
        "  Example telemetry: sends data to telemetry.example.com, 192.0.2.10:8080 (workspace)",
        "  Local [2J notes: sends nothing off the machine (this machine)",
    ] {
        assert!(s.contains(want), "no {want:?} in\n{s}");
    }
    // An older server sends no list: no heading.
    a.on_agent_settings("m1", Ok(effective()));
    let rows = agent_settings::rows(a.agent_settings.as_ref().unwrap());
    assert!(!rows.iter().any(|(_, t)| t.contains("Add-ons")));
}
