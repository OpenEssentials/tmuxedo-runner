//! `tmuxedo` / `tmuxedo tui`: the terminal UI.
//!
//! Sidebar (machines → workspaces → agents, status rolled up blocked >
//! working > done > idle > unknown), the selected workspace's tabs and
//! split panes rendered from the runner topology, all through the gateway's
//! client socket — the same protocol as the web (see `app` for the
//! details). Keys are tmux-style behind `ctrl-b`; `ctrl-b d` detaches and
//! leaves every agent running.
//!
//! Copy-on-select (`copy_on_select` in cli.toml, default on): a left drag in
//! a pane selects text (Shift-drag when the pane's program uses the mouse),
//! highlighted while dragging; on release the text (from the pane's vt100
//! emulator: wrapped rows joined, wide characters once) goes to the local
//! clipboard with OSC 52 (wrapped for tmux), at most 100 KB, and the status
//! line says "copied N chars". `tmuxedo attach` leaves selection to the local
//! terminal.
//!
//! Layout: `app` (state machine, no I/O), `view` (geometry + drawing),
//! `keys` (key → bytes), `net` (one socket task per machine), and here the
//! event loop that joins them to a real terminal.

pub mod agent_settings;
pub mod app;
pub mod arrange_ui;
pub mod chat;
pub mod chat_app;
pub mod chat_model;
pub mod favourites;
pub mod keymap;
pub mod keys;
pub mod needs;
pub mod net;
pub mod overlay;
pub mod plugins;
pub mod view;

#[cfg(test)]
mod agent_settings_tests;
#[cfg(test)]
mod arrange_tests;
#[cfg(test)]
mod chat_tests;
#[cfg(test)]
mod needs_tests;
#[cfg(test)]
mod plugins_tests;
#[cfg(test)]
mod tests;

use super::{load_cli_config, machines, CliConfig};
use app::App;
use net::{Cmd, Conn, NetEvent};
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
};
use ratatui::crossterm::execute;
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::mpsc;

pub struct Options {
    pub control: bool,
    pub machine: Option<String>,
}

pub fn run(opts: Options) -> Result<(), String> {
    let cfg = load_cli_config()?;
    let list = machines(&cfg)?;
    let (w, h) = ratatui::crossterm::terminal::size().map_err(|e| format!("not a terminal: {e}"))?;
    let mut app = App::new(opts.control, (w, h));
    app.keymap = fetch_keymap(&cfg);
    app.chat_detail = fetch_chat_detail(&cfg);
    app.copy_on_select = cfg.copy_on_select.unwrap_or(true);
    app.web_url = cfg.server.clone();
    app.on_machines(&list);
    app.select_initial(opts.machine.as_deref());
    if let Some(want) = &opts.machine {
        if app.machine().is_none_or(|m| &m.id != want && &m.name != want) {
            return Err(format!("no machine named {want}"));
        }
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let mut terminal = ratatui::try_init().map_err(|e| e.to_string())?;
    let _ = execute!(std::io::stdout(), EnableMouseCapture, EnableBracketedPaste);
    let (ui_tx, ui_rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        while let Ok(e) = event::read() {
            if ui_tx.send(e).is_err() {
                return;
            }
        }
    });
    let r = rt.block_on(event_loop(&mut terminal, &mut app, cfg, ui_rx));
    let _ = execute!(std::io::stdout(), DisableMouseCapture, DisableBracketedPaste);
    ratatui::restore();
    rt.shutdown_timeout(Duration::from_millis(200));
    if r.is_ok() {
        eprintln!("[tmuxedo: detached; your agents keep running]");
    }
    r
}

/// The user's keybindings (`GET /api/me/keybindings`), else the defaults.
pub fn fetch_keymap(cfg: &CliConfig) -> keymap::Keymap {
    let fallback = |why: String| keymap::Keymap {
        source: format!("defaults ({why})"),
        ..keymap::Keymap::default()
    };
    match super::api(cfg, "GET", "/api/me/keybindings", None) {
        Ok((200, v)) => keymap::Keymap::from_json(&v),
        Ok((code, _)) => fallback(format!("keybindings: HTTP {code}")),
        Err(e) => fallback(format!("offline: {e}")),
    }
}

/// The account's "Tool calls in chat" (`GET /api/me/chat-tools`): `expanded`
/// or, for anything else (offline, an older server), `summary`.
pub fn fetch_chat_detail(cfg: &CliConfig) -> chat_model::Detail {
    match super::api(cfg, "GET", "/api/me/chat-tools", None) {
        Ok((200, v)) if v["detail"] == "expanded" => chat_model::Detail::Expanded,
        _ => chat_model::Detail::Summary,
    }
}
/// The account's layout favourites (`GET /api/me/layouts`, token scope `read`).
pub fn fetch_favourites(cfg: &CliConfig) -> Result<serde_json::Value, String> {
    match super::api(cfg, "GET", "/api/me/layouts", None) {
        Ok((200, v)) => Ok(v),
        Ok((code, _)) => Err(format!("favourites: HTTP {code}")),
        Err(e) => Err(format!("offline: {e}")),
    }
}

/// Start, replace (scope changed) or drop machine sockets to match the app.
fn reconcile(conns: &mut HashMap<String, Conn>, app: &mut App, cfg: &CliConfig, ev: &mpsc::UnboundedSender<NetEvent>) {
    let want: HashMap<String, bool> = app.desired_links().into_iter().collect();
    let stale: Vec<String> = conns
        .iter()
        .filter(|(mid, c)| want.get(*mid) != Some(&c.control) || c.task.is_finished())
        .map(|(mid, _)| mid.clone())
        .collect();
    for mid in stale {
        if let Some(c) = conns.remove(&mid) {
            let _ = c.tx.send(Cmd::Close);
            if !c.task.is_finished() {
                app.on_link_down(&mid);
            }
        }
    }
    for (mid, control) in want {
        if !conns.contains_key(&mid) {
            conns.insert(mid.clone(), net::spawn(cfg.clone(), mid, control, ev.clone()));
        }
    }
}

fn flush(app: &mut App, conns: &HashMap<String, Conn>, cfg: &CliConfig, ev: &mpsc::UnboundedSender<NetEvent>) {
    // `prefix S`: the agents' settings, read from web in the background.
    for mid in app.agent_settings_out.drain(..) {
        let (cfg, tx) = (cfg.clone(), ev.clone());
        std::thread::spawn(move || {
            let r = match super::api(&cfg, "GET", &agent_settings::path(&mid), None) {
                Ok((200, v)) => Ok(v),
                Ok((404, _)) => Err("this machine is not in your account any more".to_string()),
                Ok((code, v)) => Err(format!("HTTP {code}: {}", super::api_error(&v))),
                Err(e) => Err(format!("offline: {e}")),
            };
            let _ = tx.send(NetEvent::AgentSettings(mid, r));
        });
    }
    // The account's layout favourites for the Arrange popup, read with the CLI token in the background.
    if std::mem::take(&mut app.layouts.fetch) {
        let (cfg, tx) = (cfg.clone(), ev.clone());
        std::thread::spawn(move || {
            let _ = tx.send(NetEvent::Favourites(fetch_favourites(&cfg)));
        });
    }
    // "Don't show this again" in the prefix overlay: the account's setting, in the background.
    for body in app.settings_out.drain(..) {
        let cfg = cfg.clone();
        std::thread::spawn(move || {
            let _ = super::api(&cfg, "PUT", "/api/me/prefix-overlay", Some(body));
        });
    }
    // OSC 52 and the like go to the local terminal (between frames).
    if !app.local_out.is_empty() {
        use std::io::Write;
        let mut out = std::io::stdout();
        for b in app.local_out.drain(..) {
            let _ = out.write_all(&b);
        }
        let _ = out.flush();
    }
    for url in app.open_urls.drain(..) {
        if let Err(e) = crate::open_url::open_locally(&url) {
            app.note = Some(format!("could not open {url}: {e}"));
        }
    }
    for (mid, pane, cb) in app.login_callbacks.drain(..) {
        crate::cli::login_callback::remember(&mid, &pane, &cb);
    }
    for (mid, o) in app.out.drain(..) {
        if let Some(c) = conns.get(&mid) {
            let _ = c.tx.send(Cmd::Send(o));
        }
    }
}

pub fn apply(app: &mut App, e: NetEvent) {
    match e {
        NetEvent::Machines(Ok(list)) => app.on_machines(&list),
        NetEvent::Machines(Err(e)) => app.note = Some(e),
        NetEvent::Text(mid, v) => app.on_text(&mid, &v),
        NetEvent::Data(mid, f) => app.on_data(&mid, f),
        NetEvent::Chat(mid, f) => app.on_chat(&mid, &f),
        NetEvent::Down(mid) => app.on_link_down(&mid),
        NetEvent::Stopped(mid, why) => app.on_link_stopped(&mid, &why),
        NetEvent::ControlRefused(mid, why) => app.on_control_refused(&mid, &why),
        NetEvent::AgentSettings(mid, r) => app.on_agent_settings(&mid, r),
        NetEvent::Favourites(r) => app.on_favourites(r),
    }
}

async fn event_loop(
    term: &mut ratatui::DefaultTerminal,
    app: &mut App,
    cfg: CliConfig,
    mut ui: mpsc::UnboundedReceiver<Event>,
) -> Result<(), String> {
    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
    let mut conns: HashMap<String, Conn> = HashMap::new();
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    let mut poll = tokio::time::interval(Duration::from_secs(15));
    poll.tick().await;
    loop {
        reconcile(&mut conns, app, &cfg, &ev_tx);
        flush(app, &conns, &cfg, &ev_tx);
        if app.detached {
            break;
        }
        term.draw(|f| view::draw(app, f)).map_err(|e| e.to_string())?;
        tokio::select! {
            e = ui.recv() => match e {
                Some(e) => app.on_event(e),
                None => app.detach_all(),
            },
            e = ev_rx.recv() => {
                if let Some(e) = e {
                    apply(app, e);
                }
                // Drain a burst before redrawing.
                while let Ok(e) = ev_rx.try_recv() {
                    apply(app, e);
                }
            }
            _ = tick.tick() => app.tick(),
            _ = poll.tick() => {
                let (cfg, tx) = (cfg.clone(), ev_tx.clone());
                tokio::task::spawn_blocking(move || { let _ = tx.send(NetEvent::Machines(machines(&cfg))); });
            }
        }
    }
    // Detached: the detach frames went out with the last flush; close politely.
    for c in conns.values() {
        let _ = c.tx.send(Cmd::Close);
    }
    for (_, c) in conns.drain() {
        let _ = tokio::time::timeout(Duration::from_secs(2), c.task).await;
    }
    Ok(())
}
