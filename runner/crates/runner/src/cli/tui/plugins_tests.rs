//! Plugin surfaces in the TUI (super::plugins; PLAN Phase 12): TestBackend
//! snapshots of each primitive, the `prefix :` palette sending
//! `plugin_action`, toasts once, logins from plugins[] and events, and an
//! older runner (no ui frames) drawing exactly as before.

use super::app::{App, Mode, Out};
use super::view;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::Color;
use ratatui::Terminal;
use serde_json::{json, Value};

fn topology() -> Value {
    let pane = |id: &str, agent: Value| json!({"id":id,"tabId":"w1:t1","cwd":"/","command":"sh","title":"sh","cols":80,"rows":24,"agent":agent});
    json!({
        "t": "topology", "rev": 1,
        "workspaces": [{"id":"w1","name":"default","cwd":"/","position":0}],
        "tabs": [{"id":"w1:t1","workspaceId":"w1","name":"main","position":0,"layout":{"dir":"h","ratio":0.5,"a":{"pane":"w1:p1"},"b":{"pane":"w1:p2"}}}],
        "panes": [
            pane("w1:p1", json!({"kind":"codex","status":"working","since":"t","source":"screen"})),
            pane("w1:p2", json!({"kind":"claude","status":"idle","since":"t","source":"reported"})),
        ]
    })
}

fn hello(auth_state: &str) -> Value {
    json!({"v":1,"runner":{"version":"1","claudeAuth":{"state":auth_state,"reason":"r","since":"t"},
        "plugins":[{"id":"claude","version":"1.0.0","contract":">=1.0, <2","state":"enabled","agent":"claude","auth":{"state":auth_state,"reason":"r","since":"t"}},
                   {"id":"demo","version":"1.0.0","contract":">=1.0, <2","state":"enabled","agent":"codex"}]}})
}

const CLAUDE_UI: &str = r#"{"t":"ui","pluginId":"claude","scope":"machine","items":[
  {"key":"sign-in","kind":"quick_action","when":"auth.state in ['missing', 'expired', 'failed']","label":"Sign in to Claude","action":"login"}]}"#;
const DEMO_UI: &str = r#"{"t":"ui","pluginId":"demo","scope":"machine","items":[
  {"key":"b1","kind":"banner","tone":"warning","text":"Demo needs a look on {machine.name}","actions":[{"label":"Say hi","action":"hi"}],"dismissible":true},
  {"key":"mb","kind":"badge","tone":"info","text":"demo"},
  {"key":"g1","kind":"badge","tone":"success","text":"ok","target":"agent"},
  {"key":"s1","kind":"status_line","text":"{agent.kind} {agent.status}","target":"agent","when":"agent.status == 'working'"},
  {"key":"t1","kind":"toast","text":"demo is busy","when":"machine.online"}]}"#;

fn app(state: &str) -> App {
    let mut app = App::new(true, (100, 12));
    app.on_machines(&[json!({"id":"m1","name":"box","online":true})]);
    app.select_initial(None);
    app.on_text(
        "m1",
        &json!({"t":"welcome","sub":"u1","mid":"m1","scope":"control","machine":{"online":true,"hello":hello(state)}}),
    );
    app.on_text("m1", &topology());
    app
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

fn key(c: KeyCode) -> KeyEvent {
    KeyEvent::new(c, KeyModifiers::NONE)
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

#[test]
fn an_older_runner_draws_exactly_as_before() {
    let mut a = App::new(true, (100, 12));
    a.on_machines(&[json!({"id":"m1","name":"box","online":true})]);
    a.select_initial(None);
    a.on_text(
        "m1",
        &json!({"t":"welcome","sub":"u1","mid":"m1","scope":"control","machine":{"online":true,"hello":{"v":1,"runner":{"version":"0.9","claudeAuth":{"state":"ok","reason":"r","since":"t"}}}}}),
    );
    a.on_text("m1", &topology());
    let before = render(&a).0;
    // A plugin-aware hello without ui frames changes nothing on screen either.
    let b = app("ok");
    assert_eq!(render(&b).0, before);
    assert!(super::plugins::shown(b.machine().unwrap()).is_empty());
}

#[test]
fn badges_banner_status_line_and_toast() {
    let mut a = app("ok");
    a.on_text("m1", &serde_json::from_str(DEMO_UI).unwrap());
    let (s, buf) = render(&a);
    // Sidebar: the machine badge after its name, the agent badge on the codex pane only.
    assert!(s[0].starts_with("● box [demo]"), "{}", s[0]);
    let agent_row = s.iter().find(|l| l.contains("codex working")).unwrap();
    assert!(agent_row.contains("[ok]"), "{agent_row}");
    assert!(!s.iter().any(|l| l.contains("claude idle [ok]")));
    let x = s[0].find("[demo]").unwrap() as u16;
    assert_eq!(buf[(x, 0)].fg, Color::Cyan);
    // The toast was said once, in the status line (it holds until a key).
    let status = s.last().unwrap();
    assert!(status.contains("demo: demo is busy"), "{status}");
    a.on_key(key(KeyCode::Char('x')));
    // Then the banner speaks, with how to reach its actions; right: the focused pane's status line.
    a.focus = Some("w1:p1".into());
    let status = render(&a).0.last().cloned().unwrap();
    assert!(status.contains("Demo needs a look on box · ^b : actions"), "{status}");
    assert!(status.contains("w1:p1 · codex working · control"), "{status}");
    // Toasts do not repeat on later frames.
    a.on_text("m1", &topology());
    assert!(!render(&a).0.last().unwrap().contains("demo is busy"));
}

#[test]
fn the_palette_runs_actions_through_plugin_action() {
    let mut a = app("expired");
    a.size = (170, 12);
    a.on_text("m1", &serde_json::from_str(CLAUDE_UI).unwrap());
    a.on_text("m1", &serde_json::from_str(DEMO_UI).unwrap());
    a.on_key(key(KeyCode::Esc));
    sent(&mut a);
    a.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
    a.on_key(key(KeyCode::Char(':')));
    assert_eq!(a.mode, Mode::Palette);
    let status = render(&a).0.last().cloned().unwrap();
    assert!(
        status.contains("actions: 1 Sign in to Claude  2 Say hi  3 Dismiss: Demo needs a look on box"),
        "{status}"
    );
    a.on_key(key(KeyCode::Char('1')));
    assert_eq!(a.mode, Mode::Normal);
    let f = sent(&mut a);
    assert_eq!(f.len(), 1, "{f:?}");
    assert_eq!(f[0]["t"], "plugin_action");
    assert_eq!(f[0]["pluginId"], "claude");
    assert_eq!(f[0]["actionId"], "login");
    let id = f[0]["id"].as_str().unwrap().to_string();
    a.on_text(
        "m1",
        &json!({"t":"res","id":id,"result":{"type":"plugin_action_invoked","log":{"status":"succeeded"}}}),
    );
    assert!(render(&a).0.last().unwrap().contains("Sign in to Claude: started"));
    sent(&mut a); // (a reply also re-syncs attachments)
                  // Dismiss: the banner goes, the palette no longer offers it.
    a.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
    a.on_key(key(KeyCode::Char(':')));
    a.on_key(key(KeyCode::Char('3')));
    assert!(sent(&mut a).iter().all(|f| f["t"] != "plugin_action"));
    assert!(a.banner_line().is_none());
    // The banner's actions went with it; a refusal is said with its code.
    assert_eq!(super::plugins::palette(a.machine().unwrap()).len(), 1);
    a.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
    a.on_key(key(KeyCode::Char(':')));
    a.on_key(key(KeyCode::Char('1')));
    let id = sent(&mut a)[0]["id"].as_str().unwrap().to_string();
    a.on_text(
        "m1",
        &json!({"t":"res","id":id,"error":{"code":"not_supported","message":"update the runner"}}),
    );
    assert!(render(&a)
        .0
        .last()
        .unwrap()
        .contains("not_supported: update the runner"));
}

#[test]
fn the_claude_sign_in_follows_the_login_and_an_empty_set_clears() {
    let mut a = app("ok");
    a.on_text("m1", &serde_json::from_str(CLAUDE_UI).unwrap());
    assert!(super::plugins::palette(a.machine().unwrap()).is_empty());
    // An auth_* event (no data.plugin = claude) brings it.
    a.on_text("m1", &json!({"t":"event","seq":1,"paneId":"","type":"auth_expired","at":"t","data":{"state":"expired","reason":"x"}}));
    assert_eq!(super::plugins::palette(a.machine().unwrap())[0].0, "Sign in to Claude");
    // The plugin disabled: an empty set.
    a.on_text(
        "m1",
        &json!({"t":"ui","pluginId":"claude","scope":"machine","items":[]}),
    );
    assert!(super::plugins::palette(a.machine().unwrap()).is_empty());
    // Without control the palette says so instead of sending.
    let mut w = App::new(false, (100, 12));
    w.on_machines(&[json!({"id":"m1","name":"box","online":true})]);
    w.select_initial(None);
    w.on_text("m1", &json!({"t":"welcome","sub":"u1","mid":"m1","scope":"observe","machine":{"online":true,"hello":hello("expired")}}));
    w.on_text("m1", &serde_json::from_str(CLAUDE_UI).unwrap());
    w.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
    w.on_key(key(KeyCode::Char(':')));
    w.on_key(key(KeyCode::Char('1')));
    assert!(sent(&mut w).iter().all(|f| f["t"] != "plugin_action"));
    assert!(w.note.as_deref().unwrap_or("").contains("needs control"));
}

#[test]
fn an_organization_that_turned_claude_off_says_what_to_do() {
    let mut a = app("failed");
    a.on_text(
        "m1",
        &json!({"t":"machine","online":true,"hello":{"v":1,"runner":{"version":"1",
            "claudeAuth":{"state":"failed","reason":"org_disabled","since":"t"},
            "plugins":[{"id":"claude","version":"1.0.0","contract":">=1.0, <2","state":"enabled","agent":"claude",
                        "auth":{"state":"failed","reason":"org_disabled","since":"t"}}]}}}),
    );
    let line = a.banner_line().expect("a line");
    assert!(
        line.contains("/logout then /login") && line.contains("ANTHROPIC_API_KEY"),
        "{line}"
    );
    let (lines, _) = render(&a);
    assert!(lines.iter().any(|l| l.contains("claude off")), "{lines:#?}");
    // A plain login failure keeps the usual badge and no such line.
    let b = app("failed");
    assert!(!b.banner_line().is_some_and(|l| l.contains("organization")));
}
