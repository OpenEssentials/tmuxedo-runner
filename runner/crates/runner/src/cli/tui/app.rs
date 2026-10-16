//! The TUI's state machine. Pure: frames and terminal events go in
//! (`on_text`, `on_data`, `on_event`, `tick`, …), protocol frames come out
//! in `out` as `(machineId, Out)`; the event loop (`super::run`) moves them
//! to the sockets. Everything here is testable without a terminal or a
//! network.
//!
//! - One gateway client socket per machine (`desired_links`): `control` for
//!   the selected machine (unless `--observe`), `observe` for every other
//!   online machine, which is how the sidebar keeps live agent status for
//!   all machines.
//! - Panes of the selected workspace's active tab are attached (`attach`
//!   with `from` = last processed offset), one vt100 emulator each, kept at
//!   the PTY's size. As the size owner (control), the TUI sends `resize`
//!   with each pane's rendered size; after `owner {you:false}` it renders at
//!   the PTY size (`size` frames) and letterboxes or crops.
//! - Processed bytes are acknowledged at least every 64 KiB (and on `tick`).

use super::keymap::{Action, Keymap};
use super::keys::{encode_key, encode_mouse, osc52};
use super::view;
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use tmuxedo_emu::Emulator;
use tmuxedo_protocol::frames::{Agent, AgentStatus, Layout, Pane, Tab, Workspace};
use tmuxedo_protocol::{DataFrame, DataKind};

pub const ACK_EVERY: u64 = crate::cli::attach::ACK_EVERY;
/// Lines of history kept per pane for scroll mode (snapshots carry it too).
pub const SCROLLBACK: usize = 5000;
/// Lines per mouse-wheel notch in scroll mode.
pub const WHEEL_LINES: usize = 3;

/// Something to send on a machine's socket.
#[derive(Debug, Clone, PartialEq)]
pub enum Out {
    Text(Value),
    Data(DataFrame),
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Topo {
    #[serde(default)]
    pub rev: u64,
    pub workspaces: Vec<Workspace>,
    pub tabs: Vec<Tab>,
    pub panes: Vec<Pane>,
}

impl Topo {
    pub fn workspaces_sorted(&self) -> Vec<&Workspace> {
        let mut v: Vec<&Workspace> = self.workspaces.iter().collect();
        v.sort_by_key(|w| w.position);
        v
    }
    pub fn tabs_of(&self, ws: &str) -> Vec<&Tab> {
        let mut v: Vec<&Tab> = self.tabs.iter().filter(|t| t.workspace_id == ws).collect();
        v.sort_by_key(|t| t.position);
        v
    }
    pub fn tab(&self, id: &str) -> Option<&Tab> {
        self.tabs.iter().find(|t| t.id == id)
    }
    pub fn pane(&self, id: &str) -> Option<&Pane> {
        self.panes.iter().find(|p| p.id == id)
    }
    /// (workspace, tab) of a pane.
    pub fn locate(&self, pane: &str) -> Option<(String, String)> {
        let t = self.tab(&self.pane(pane)?.tab_id)?;
        Some((t.workspace_id.clone(), t.id.clone()))
    }
    pub fn panes_of_ws<'a>(&'a self, ws: &'a str) -> impl Iterator<Item = &'a Pane> + 'a {
        self.panes
            .iter()
            .filter(move |p| self.tab(&p.tab_id).is_some_and(|t| t.workspace_id == ws))
    }
}

/// Panes of a layout, in order (left/top first).
pub fn layout_panes(l: &Layout) -> Vec<String> {
    match l {
        Layout::Pane { pane } => vec![pane.clone()],
        Layout::Split { a, b, .. } => {
            let mut v = layout_panes(a);
            v.extend(layout_panes(b));
            v
        }
    }
}

fn rank(s: Option<AgentStatus>) -> u8 {
    match s {
        Some(AgentStatus::Blocked) => 5,
        Some(AgentStatus::Working) => 4,
        Some(AgentStatus::Done) => 3,
        Some(AgentStatus::Idle) => 2,
        Some(AgentStatus::Unknown) => 1,
        None => 0,
    }
}

/// Roll-up: blocked > working > done > idle > unknown; None without agents.
pub fn rollup(it: impl IntoIterator<Item = Option<AgentStatus>>) -> Option<AgentStatus> {
    it.into_iter().max_by_key(|s| rank(*s)).flatten()
}

#[derive(Debug, Clone, PartialEq)]
pub enum Link {
    Down,
    /// Welcomed; `true` = control scope.
    Up(bool),
    /// Closed with 4003 or the token was refused: no reconnect.
    Stopped(String),
}

#[derive(Debug, Clone)]
pub struct Machine {
    pub id: String,
    pub name: String,
    pub online: bool,
    pub topo: Option<Topo>,
    pub link: Link,
    /// Claude login health (`ok|missing|expired|failed`, reason): from
    /// `/api/machines`, the runner's `hello.claudeAuth`, and `auth_*` events.
    pub auth: Option<(String, String)>,
    /// Plugin surfaces and logins (super::plugins, PLAN Phase 12).
    pub plugins: super::plugins::PluginState,
    /// Needs-you items (super::needs, PLAN Phase 13).
    pub needs: super::needs::NeedsState,
}

impl Machine {
    pub fn rollup(&self) -> Option<AgentStatus> {
        rollup(
            self.topo
                .iter()
                .flat_map(|t| t.panes.iter().map(|p| p.agent.as_ref().map(|a| a.status))),
        )
    }
}

/// One pane's terminal as this client sees it.
pub struct PaneTerm {
    pub emu: Emulator,
    /// Offset after the last byte fed to `emu` (the `from` of a re-attach).
    pub processed: Option<u64>,
    pub acked: u64,
    /// `owner {you}` from the gateway (None: never told).
    pub owner: Option<bool>,
    /// The PTY's size (cols, rows); `emu` is kept at this size.
    pub pty: (u16, u16),
    pub sent_size: Option<(u16, u16)>,
    pub attached: bool,
}

impl PaneTerm {
    pub fn new(cols: u16, rows: u16) -> Self {
        PaneTerm {
            emu: Emulator::new(rows.max(1), cols.max(1), SCROLLBACK),
            processed: None,
            acked: 0,
            owner: None,
            pty: (cols.max(1), rows.max(1)),
            sent_size: None,
            attached: false,
        }
    }
    /// Lines of history above the screen (`set_scrollback` clamps to it).
    pub fn history(&mut self) -> usize {
        let s = self.emu.parser_mut().screen_mut();
        let cur = s.scrollback();
        s.set_scrollback(usize::MAX);
        let n = s.scrollback();
        s.set_scrollback(cur);
        n
    }

    pub fn set_pty(&mut self, cols: u16, rows: u16) {
        let (cols, rows) = (cols.max(1), rows.max(1));
        if self.pty != (cols, rows) {
            self.pty = (cols, rows);
            self.emu.resize(rows, cols);
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Mode {
    Normal,
    /// After ctrl-b.
    Prefix,
    /// `x`: close this pane? (y/n)
    Confirm(String),
    /// `w`: the sidebar has the keyboard.
    Sidebar,
    /// `[` (or the wheel over a pane without mouse reporting): scrolling
    /// this pane's history.
    Scroll(String),
    /// `,` / `$` / `P`: typing a new name for a tab, workspace or pane.
    Rename(Rename),
    /// `prefix ?`: every binding (the overlay's full list); a key runs its action, Esc closes.
    Keys,
    /// `prefix :`: the plugin actions palette (super::plugins); a digit runs one, Esc closes.
    Palette,
    /// `prefix !` / `prefix N`: the Needs you list (super::needs), the selected row.
    Needs(usize),
    /// `prefix Space`: the Arrange popup (super::arrange_ui), the selected picture or favourite.
    Arrange(usize),
    /// `r` in that list: typing a reply to item (id), the text so far.
    Reply(String, String),
    /// `prefix S`: the agents' settings on this machine, read only (super::agent_settings); the first row shown.
    AgentSettings(u16),
}

/// What is being renamed, and the name typed so far (docs/protocol.md "Names").
#[derive(Debug, Clone, PartialEq)]
pub struct Rename {
    pub kind: NameKind,
    pub id: String,
    /// Shown in the prompt: "tab 2", "workspace default", "pane w1:p3".
    pub what: String,
    pub buf: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameKind {
    Workspace,
    Tab,
    Pane,
}

impl NameKind {
    pub fn method(self) -> &'static str {
        match self {
            NameKind::Workspace => "workspace.rename",
            NameKind::Tab => "tab.rename",
            NameKind::Pane => "pane.rename",
        }
    }
    pub fn id_param(self) -> &'static str {
        match self {
            NameKind::Workspace => "workspace_id",
            NameKind::Tab => "tab_id",
            NameKind::Pane => "pane_id",
        }
    }
    pub fn word(self) -> &'static str {
        match self {
            NameKind::Workspace => "workspace",
            NameKind::Tab => "tab",
            NameKind::Pane => "pane",
        }
    }
}

/// A text selection in one pane, in pane-relative screen cells (what is
/// drawn now, so it works in scroll mode too). `anchor` is where the drag
/// started, `head` where it is; `active` while the button is down.
#[derive(Debug, Clone, PartialEq)]
pub struct Selection {
    pub pane: String,
    pub anchor: (u16, u16),
    pub head: (u16, u16),
    pub active: bool,
}

impl Selection {
    /// (start, end) in reading order, as (row, col), end inclusive.
    pub fn range(&self) -> ((u16, u16), (u16, u16)) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }
    pub fn contains(&self, row: u16, col: u16) -> bool {
        let (s, e) = self.range();
        (row, col) >= s && (row, col) <= e
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    Machine(usize),
    Workspace(usize, String),
    Agent(usize, String),
}

#[derive(Debug, Clone)]
enum Pending {
    NewTab,
    Split,
    Zoom(String),
    Rename(NameKind),
    /// A plugin action (its label, for the status line).
    PluginAction(String),
    /// A needs-you answer (the item's title, for the status line).
    Needs(String),
    /// A chat request (super::chat_app).
    Chat(super::chat_app::ChatReq),
    /// Arrange, undo, or a step of a favourite's restore (super::arrange_ui).
    Layout(super::arrange_ui::LayoutReq),
    Other,
}

type Key2 = (String, String);

pub struct App {
    pub machines: Vec<Machine>,
    pub sel: usize,
    /// Selected workspace on the selected machine.
    pub ws: Option<String>,
    pub tab_of_ws: HashMap<Key2, String>,
    pub focus: Option<String>,
    /// (machine, tab) → zoomed pane (the protocol topology has no zoom; the `pane.zoom` result does).
    pub zoomed: HashMap<Key2, String>,
    pub terms: HashMap<Key2, PaneTerm>,
    pub mode: Mode,
    /// Ask for `control` on the selected machine.
    pub control: bool,
    pub size: (u16, u16),
    pub out: Vec<(String, Out)>,
    pub note: Option<String>,
    pub detached: bool,
    pub sidebar_cursor: usize,
    /// Prompt ids answered or dismissed here (the overlay stays down for them).
    pub dismissed: HashSet<String>,
    /// Prefix and action keys (the user's, or the defaults).
    pub keymap: Keymap,
    /// The pane a forwarded mouse press went to (drags and releases follow it).
    mouse_pane: Option<String>,
    /// Copy-on-select: drag to select, release copies (OSC 52).
    pub copy_on_select: bool,
    pub selection: Option<Selection>,
    /// Bytes for the local terminal (OSC 52), written by the event loop.
    pub local_out: Vec<Vec<u8>>,
    /// Running inside tmux (wrap OSC 52 for its passthrough).
    pub in_tmux: bool,
    /// Web links a pane asked to open (open_url events), opened locally by
    /// the event loop.
    pub open_urls: Vec<String>,
    /// Login callbacks seen in open_url events (machine, pane, callback),
    /// remembered by the event loop for `tmuxedo login-callback`.
    pub login_callbacks: Vec<(String, String, Value)>,
    /// When the prefix was pressed (the overlay's delay counts from here).
    pub prefix_at: Option<std::time::Instant>,
    /// Prefix overlay setting changes for the account (`PUT /api/me/prefix-overlay`), sent by the event loop.
    pub settings_out: Vec<Value>,
    /// `prefix S`'s view (super::agent_settings), while open.
    pub agent_settings: Option<super::agent_settings::View>,
    /// Machines whose agent settings the event loop fetches (`GET …/plugins/effective`).
    pub agent_settings_out: Vec<String>,
    /// The web app's address (cli.toml `server`), for `e` in that view.
    pub web_url: String,
    /// Arrange's undo, the account's favourites and a restore in progress.
    pub layouts: super::arrange_ui::Layouts,
    pending: HashMap<String, Pending>,
    pending_focus: Option<String>,
    next_id: u64,
    /// The chat screen (super::chat), over the panes while it is open.
    pub chat: Option<super::chat::ChatScreen>,
    /// The account's "Tool calls in chat" setting (`GET /api/me/chat-tools`).
    pub chat_detail: super::chat_model::Detail,
    /// Colour on the chat screen (`NO_COLOR` turns it off).
    pub chat_color: bool,
    /// A fixed clock for the chat's timers (tests); None reads the system clock.
    pub chat_now: Option<u64>,
}

impl App {
    pub fn new(control: bool, size: (u16, u16)) -> Self {
        App {
            machines: vec![],
            sel: 0,
            ws: None,
            tab_of_ws: HashMap::new(),
            focus: None,
            zoomed: HashMap::new(),
            terms: HashMap::new(),
            mode: Mode::Normal,
            control,
            size,
            out: vec![],
            note: None,
            detached: false,
            sidebar_cursor: 0,
            dismissed: HashSet::new(),
            keymap: Keymap::default(),
            mouse_pane: None,
            copy_on_select: true,
            selection: None,
            local_out: vec![],
            in_tmux: std::env::var_os("TMUX").is_some(),
            open_urls: vec![],
            login_callbacks: vec![],
            prefix_at: None,
            settings_out: vec![],
            agent_settings: None,
            agent_settings_out: vec![],
            web_url: String::new(),
            layouts: Default::default(),
            pending: HashMap::new(),
            pending_focus: None,
            next_id: 1,
            chat: None,
            chat_detail: Default::default(),
            chat_color: crate::cli::widget::color_on(&|k| std::env::var(k).ok()),
            chat_now: None,
        }
    }

    // ── queries ──────────────────────────────────────────────────────────

    pub fn machine(&self) -> Option<&Machine> {
        self.machines.get(self.sel)
    }
    fn mid(&self) -> Option<String> {
        self.machine().map(|m| m.id.clone())
    }
    pub fn topo(&self) -> Option<&Topo> {
        self.machine()?.topo.as_ref()
    }
    pub fn can_control(&self) -> bool {
        self.machine().is_some_and(|m| m.link == Link::Up(true))
    }
    pub fn tab_id(&self) -> Option<String> {
        let mid = self.mid()?;
        self.tab_of_ws.get(&(mid, self.ws.clone()?)).cloned()
    }
    pub fn tab(&self) -> Option<&Tab> {
        self.topo()?.tab(&self.tab_id()?)
    }
    pub fn zoomed_pane(&self) -> Option<&String> {
        self.zoomed.get(&(self.mid()?, self.tab_id()?))
    }
    pub fn term(&self, pane: &str) -> Option<&PaneTerm> {
        self.terms.get(&(self.mid()?, pane.to_string()))
    }

    /// The machine sockets this state wants: (machineId, control).
    pub fn desired_links(&self) -> Vec<(String, bool)> {
        self.machines
            .iter()
            .enumerate()
            .filter(|(_, m)| !matches!(m.link, Link::Stopped(_)))
            .filter_map(|(i, m)| {
                if i == self.sel {
                    Some((m.id.clone(), self.control))
                } else {
                    m.online.then(|| (m.id.clone(), false))
                }
            })
            .collect()
    }

    /// Visible panes and their rectangles (zoom shows the zoomed pane only).
    pub fn visible(&self) -> Vec<(String, Rect)> {
        let main = view::areas(self.size).main;
        let Some(tab) = self.tab() else { return vec![] };
        if let Some(z) = self.zoomed_pane() {
            return vec![(z.clone(), main)];
        }
        let mut out = vec![];
        view::split_rects(&tab.layout, main, &mut out, &mut vec![]);
        out
    }

    /// The sidebar rows: machines → workspaces → agents.
    pub fn sidebar_items(&self) -> Vec<Item> {
        let mut v = vec![];
        for (i, m) in self.machines.iter().enumerate() {
            v.push(Item::Machine(i));
            let Some(t) = &m.topo else { continue };
            for w in t.workspaces_sorted() {
                v.push(Item::Workspace(i, w.id.clone()));
                for tab in t.tabs_of(&w.id) {
                    for p in layout_panes(&tab.layout) {
                        if t.pane(&p).is_some_and(|p| p.agent.is_some()) {
                            v.push(Item::Agent(i, p));
                        }
                    }
                }
            }
        }
        v
    }

    /// The focused pane's held permission prompt, unless answered here: (pane, promptId, agent).
    pub fn active_prompt(&self) -> Option<(String, String, &Agent)> {
        let pane = self.focus.clone()?;
        let a = self.topo()?.pane(&pane)?.agent.as_ref()?;
        let id = a.prompt_id.clone()?;
        (a.status == AgentStatus::Blocked && a.prompt.is_some() && !self.dismissed.contains(&id))
            .then_some((pane, id, a))
    }

    // ── machines and links ───────────────────────────────────────────────

    /// `GET /api/machines` items.
    pub fn on_machines(&mut self, items: &[Value]) {
        let selected = self.mid();
        let mut next = vec![];
        for it in items {
            let Some(id) = it["id"].as_str() else { continue };
            let old = self.machines.iter().find(|m| m.id == id).cloned();
            next.push(Machine {
                id: id.to_string(),
                name: it["name"].as_str().unwrap_or(id).to_string(),
                online: match &old {
                    Some(o) if matches!(o.link, Link::Up(_)) => o.online,
                    _ => it["online"].as_bool().unwrap_or(false),
                },
                topo: old.as_ref().and_then(|o| o.topo.clone()),
                auth: it["claudeAuthState"]
                    .as_str()
                    .map(|s| (s.to_string(), it["claudeAuthReason"].as_str().unwrap_or("").to_string()))
                    .or_else(|| old.as_ref().and_then(|o| o.auth.clone())),
                plugins: old.as_ref().map(|o| o.plugins.clone()).unwrap_or_default(),
                needs: old.as_ref().map(|o| o.needs.clone()).unwrap_or_default(),
                link: old.map(|o| o.link).unwrap_or(Link::Down),
            });
        }
        self.machines = next;
        self.sel = selected
            .and_then(|s| self.machines.iter().position(|m| m.id == s))
            .unwrap_or(0);
        self.fix_selection();
    }

    /// Pick the machine to show first: by id or name, else the first online one.
    pub fn select_initial(&mut self, want: Option<&str>) {
        let i = match want {
            Some(w) => self.machines.iter().position(|m| m.id == w || m.name == w),
            None => self.machines.iter().position(|m| m.online),
        };
        self.sel = i.unwrap_or(0);
        self.fix_selection();
    }

    /// The machine's socket dropped (or was replaced to change scope).
    pub fn on_link_down(&mut self, mid: &str) {
        if let Some(m) = self.machines.iter_mut().find(|m| m.id == mid) {
            if !matches!(m.link, Link::Stopped(_)) {
                m.link = Link::Down;
            }
        }
        for ((m, _), t) in self.terms.iter_mut() {
            if m == mid {
                t.attached = false;
                t.sent_size = None;
                t.owner = None;
            }
        }
        if let Some(c) = self.chat.as_mut().filter(|c| c.mid == mid) {
            c.link = super::chat::Link::Reconnecting;
        }
        // Its answers will not come: a restore on it stops (what it opened stays, nothing is closed).
        if self.layouts.restore.as_ref().is_some_and(|r| r.mid == mid) {
            let r = self.layouts.restore.take().unwrap();
            self.note = Some(super::arrange_ui::restore_text(&r, Some("disconnected")));
        }
    }

    /// No reconnect for this machine (4003 revoked, or the ticket was refused for good).
    pub fn on_link_stopped(&mut self, mid: &str, why: &str) {
        self.on_link_down(mid);
        if let Some(m) = self.machines.iter_mut().find(|m| m.id == mid) {
            m.link = Link::Stopped(why.to_string());
            self.note = Some(format!("{}: {why}", m.name));
        }
    }

    /// A `control` ticket was refused (no `terminal` scope): observe instead.
    pub fn on_control_refused(&mut self, mid: &str, why: &str) {
        self.control = false;
        self.on_link_down(mid);
        self.note = Some(format!("control refused ({why}); watching"));
    }

    // ── frames from the gateway ──────────────────────────────────────────

    pub fn on_text(&mut self, mid: &str, v: &Value) {
        let Some(mi) = self.machines.iter().position(|m| m.id == mid) else {
            return;
        };
        let selected = mi == self.sel;
        // Plugin surfaces and logins first (super::plugins); the frames are handled below as before.
        self.machines[mi].plugins.observe(v);
        self.machines[mi].needs.observe(v);
        if matches!(
            v["t"].as_str(),
            Some("ui" | "event" | "welcome" | "machine" | "status" | "topology")
        ) {
            self.plugin_toasts(mi, selected);
        }
        match v["t"].as_str().unwrap_or("") {
            "welcome" => {
                let m = &mut self.machines[mi];
                m.link = Link::Up(v["scope"] == "control");
                if let Some(o) = v["machine"]["online"].as_bool() {
                    m.online = o;
                }
                if let Some(a) = hello_auth(&v["machine"]["hello"]) {
                    m.auth = Some(a);
                }
                if selected {
                    self.sync(false);
                }
                // A chat open on this machine resumes from the event after its last one.
                self.chat_relink(mid);
            }
            "machine" => {
                if let Some(o) = v["online"].as_bool() {
                    self.machines[mi].online = o;
                }
                if let Some(a) = hello_auth(&v["hello"]) {
                    self.machines[mi].auth = Some(a);
                }
            }
            // An unverified link (Phase 13: not from the pane's agent, or a host its plugin does
            // not list) is never opened by itself: the status line says what and where.
            "event" if v["type"] == "open_url" && v["data"]["verified"] == false => {
                let url = v["data"]["url"].as_str().unwrap_or("");
                if selected && crate::open_url::valid_url(url) {
                    let host = tmuxedo_protocol::needs::url_host(url).unwrap_or_default();
                    let pane = v["paneId"].as_str().unwrap_or("");
                    self.note = Some(format!(
                        "⚠ Unverified link to {host} from a process in pane {pane}: not opened. Open it yourself only if you started it: {url}"
                    ));
                }
            }
            "event" if v["type"] == "open_url" => {
                let url = v["data"]["url"].as_str().unwrap_or("");
                if selected && crate::open_url::valid_url(url) {
                    self.note = Some(format!("opening {url}"));
                    self.open_urls.push(url.to_string());
                    // A login whose redirect goes to the machine's localhost: say
                    // how to finish it, and remember it for `tmuxedo login-callback`.
                    let cb = &v["data"]["callback"];
                    if cb.is_object() {
                        self.note = Some(crate::cli::login_callback::hint(cb));
                        self.login_callbacks.push((
                            mid.to_string(),
                            v["paneId"].as_str().unwrap_or("").to_string(),
                            cb.clone(),
                        ));
                    }
                }
            }
            "event" if v["type"].as_str().is_some_and(|t| t.starts_with("auth_")) => {
                let state = v["data"]["state"]
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| v["type"].as_str().unwrap_or("").trim_start_matches("auth_").to_string());
                let reason = v["data"]["reason"].as_str().unwrap_or("").to_string();
                self.machines[mi].auth = Some((state, reason));
            }
            "topology" => {
                let Ok(t) = serde_json::from_value::<Topo>(v.clone()) else {
                    return;
                };
                self.machines[mi].topo = Some(t);
                self.terms.retain(|(m, p), _| {
                    m != mid || self.machines[mi].topo.as_ref().is_some_and(|t| t.pane(p).is_some())
                });
                if selected {
                    self.fix_selection();
                    self.sync(false);
                }
            }
            "status" => {
                let pane = v["paneId"].as_str().unwrap_or("");
                let agent: Option<Agent> = serde_json::from_value(v["agent"].clone()).ok().flatten();
                if let Some(p) = self.machines[mi]
                    .topo
                    .as_mut()
                    .and_then(|t| t.panes.iter_mut().find(|p| p.id == pane))
                {
                    p.agent = agent;
                }
            }
            "size" => {
                let (Some(c), Some(r)) = (v["cols"].as_u64(), v["rows"].as_u64()) else {
                    return;
                };
                let pane = v["paneId"].as_str().unwrap_or("").to_string();
                if let Some(p) = self.machines[mi]
                    .topo
                    .as_mut()
                    .and_then(|t| t.panes.iter_mut().find(|p| p.id == pane))
                {
                    (p.cols, p.rows) = (c as u16, r as u16);
                }
                if let Some(t) = self.terms.get_mut(&(mid.to_string(), pane)) {
                    t.set_pty(c as u16, r as u16);
                }
            }
            "owner" => {
                let pane = v["paneId"].as_str().unwrap_or("").to_string();
                if let Some(t) = self.terms.get_mut(&(mid.to_string(), pane)) {
                    t.owner = v["you"].as_bool();
                    if t.owner == Some(false) {
                        t.sent_size = None;
                    }
                }
            }
            "res" => self.on_res(v),
            "chat_attached" => self.on_chat_attached(mid, v),
            "event" if v["type"] == "decision_stale" => {
                self.note = Some(format!(
                    "{}: that prompt was already answered",
                    v["paneId"].as_str().unwrap_or("")
                ));
            }
            "error" => {
                self.note = Some(format!(
                    "{}: {}",
                    v["code"].as_str().unwrap_or("error"),
                    v["message"].as_str().unwrap_or("")
                ));
            }
            _ => {}
        }
    }

    fn on_res(&mut self, v: &Value) {
        let Some(p) = v["id"].as_str().and_then(|id| self.pending.remove(id)) else {
            return;
        };
        let p = match p {
            Pending::Chat(req) => return self.on_chat_res(req, v),
            p => p,
        };
        if let Pending::Layout(req) = p {
            let code = v
                .get("error")
                .filter(|e| !e.is_null())
                .map(|e| e["code"].as_str().unwrap_or("error"));
            return self.on_layout_res(req, &v["result"], code);
        }
        if let (Pending::Needs(what), Some(e)) = (&p, v.get("error").filter(|e| !e.is_null())) {
            self.note = Some(match e["code"].as_str() {
                Some("stepup_required") => format!("“{what}” is high risk: approve it on the web or in the app"),
                Some("keystroke_off") => {
                    "answering by keys is off for this agent (Settings → Terminal on the web)".into()
                }
                Some("answer_in_terminal") => format!("answer “{what}” in the terminal"),
                Some(c) => format!("{c}: {}", e["message"].as_str().unwrap_or("")),
                None => "error".into(),
            });
            return;
        }
        if let Some(e) = v.get("error").filter(|e| !e.is_null()) {
            self.note = Some(format!(
                "{}: {}",
                e["code"].as_str().unwrap_or("error"),
                e["message"].as_str().unwrap_or("")
            ));
            return;
        }
        let r = &v["result"];
        match p {
            Pending::NewTab => self.pending_focus = r["root_pane"]["pane_id"].as_str().map(str::to_owned),
            Pending::Split => self.pending_focus = r["pane"]["pane_id"].as_str().map(str::to_owned),
            Pending::Zoom(tab) => {
                let Some(mid) = self.mid() else { return };
                let z = &r["zoom"];
                match (z["zoomed"].as_bool(), z["pane_id"].as_str()) {
                    (Some(true), Some(p)) => {
                        self.zoomed.insert((mid, tab), p.to_string());
                    }
                    (Some(false), _) => {
                        self.zoomed.remove(&(mid, tab));
                    }
                    _ => {}
                }
            }
            Pending::Rename(kind) => {
                let name = match kind {
                    NameKind::Workspace => r["workspace"]["label"].as_str(),
                    NameKind::Tab => r["tab"]["label"].as_str(),
                    NameKind::Pane => r["pane"]["label"].as_str(),
                };
                self.note = Some(match name {
                    Some(n) => format!("{} renamed to {n}", kind.word()),
                    None => format!("{} name cleared", kind.word()),
                });
            }
            Pending::PluginAction(label) => {
                let failed = r["log"]["status"] == "failed";
                self.note = Some(if failed {
                    format!("{label}: {}", r["log"]["error"].as_str().unwrap_or("failed"))
                } else {
                    format!("{label}: started")
                });
            }
            Pending::Needs(what) => self.note = Some(super::needs::outcome_note(&what, r)),
            Pending::Chat(_) | Pending::Other | Pending::Layout(_) => {}
        }
        self.fix_selection();
        self.sync(false);
    }

    pub fn on_data(&mut self, mid: &str, f: DataFrame) {
        let key = (mid.to_string(), f.pane_id.clone());
        let Some(t) = self.terms.get_mut(&key) else { return };
        let end = match f.kind {
            DataKind::Snapshot => {
                let (c, r) = t.pty;
                t.emu = Emulator::new(r, c, SCROLLBACK);
                t.emu.feed(&f.payload);
                t.processed = Some(f.offset);
                // Tell the gateway where we are now (it may have been behind).
                t.acked = 0;
                f.offset
            }
            DataKind::Output => {
                let end = f.offset + f.payload.len() as u64;
                let payload = match t.processed {
                    Some(p) if end <= p => return,
                    Some(p) if f.offset < p => &f.payload[(p - f.offset) as usize..],
                    _ => &f.payload[..],
                };
                t.emu.feed(payload);
                t.processed = Some(end);
                end
            }
            DataKind::Input => return,
        };
        if t.acked == 0 || end.saturating_sub(t.acked) >= ACK_EVERY {
            t.acked = end;
            self.out.push((
                mid.to_string(),
                Out::Text(json!({ "t": "ack", "paneId": f.pane_id, "offset": end })),
            ));
        }
    }

    /// Periodic: acknowledge whatever was processed since the last ack.
    pub fn tick(&mut self) {
        for ((mid, pane), t) in self.terms.iter_mut() {
            if let Some(p) = t.processed.filter(|p| *p > t.acked && t.attached) {
                t.acked = p;
                self.out.push((
                    mid.clone(),
                    Out::Text(json!({ "t": "ack", "paneId": pane, "offset": p })),
                ));
            }
        }
    }

    // ── selection and attachment ─────────────────────────────────────────

    /// Keep ws / tab / focus / zoom pointing at things that exist.
    pub fn fix_selection(&mut self) {
        let Some(mid) = self.mid() else {
            self.ws = None;
            self.focus = None;
            return;
        };
        let Some(t) = self.topo().cloned() else { return };
        if let Some(p) = self.pending_focus.clone() {
            if let Some((ws, tab)) = t.locate(&p) {
                self.ws = Some(ws.clone());
                self.tab_of_ws.insert((mid.clone(), ws), tab);
                self.focus = Some(p);
                self.pending_focus = None;
            }
        }
        if !self
            .ws
            .as_ref()
            .is_some_and(|w| t.workspaces.iter().any(|x| &x.id == w))
        {
            self.ws = t.workspaces_sorted().first().map(|w| w.id.clone());
        }
        let Some(ws) = self.ws.clone() else {
            self.focus = None;
            return;
        };
        let tabs = t.tabs_of(&ws);
        let key = (mid.clone(), ws);
        let tab = match self.tab_of_ws.get(&key) {
            Some(cur) if tabs.iter().any(|x| &x.id == cur) => cur.clone(),
            _ => match tabs.first() {
                Some(x) => x.id.clone(),
                None => {
                    self.focus = None;
                    return;
                }
            },
        };
        self.tab_of_ws.insert(key, tab.clone());
        let panes = t.tab(&tab).map(|x| layout_panes(&x.layout)).unwrap_or_default();
        if !self.focus.as_ref().is_some_and(|f| panes.contains(f)) {
            self.focus = panes.first().cloned();
        }
        self.zoomed.retain(|(m, tb), p| {
            m != &mid
                || t.tab(tb).is_some_and(|x| {
                    let ps = layout_panes(&x.layout);
                    ps.len() > 1 && ps.contains(p)
                })
        });
        if let Some(z) = self.zoomed.get(&(mid, tab)) {
            self.focus = Some(z.clone());
        }
    }

    /// Attach visible panes, detach hidden ones, and (as size owner) send
    /// each visible pane's rendered size. `take_size`: resize even where
    /// another client owns the size (the user resized this terminal).
    pub fn sync(&mut self, take_size: bool) {
        let Some(m) = self.machine() else { return };
        let Link::Up(control) = m.link else { return };
        let mid = m.id.clone();
        let vis = self.visible();
        let ids: HashSet<&String> = vis.iter().map(|(p, _)| p).collect();
        let mut out = vec![];
        for ((m, p), t) in self.terms.iter_mut() {
            if m == &mid && t.attached && !ids.contains(p) {
                t.attached = false;
                out.push(json!({ "t": "detach", "paneId": p }));
            }
        }
        let topo = self.topo().cloned().unwrap_or_default();
        for (pid, rect) in &vis {
            let (c, r) = topo.pane(pid).map(|p| (p.cols, p.rows)).unwrap_or((80, 24));
            let t = self
                .terms
                .entry((mid.clone(), pid.clone()))
                .or_insert_with(|| PaneTerm::new(c, r));
            if !t.attached {
                let mut a = json!({ "t": "attach", "paneId": pid });
                if let Some(f) = t.processed {
                    a["from"] = json!(f);
                }
                out.push(a);
                t.attached = true;
            }
            let want = (rect.width, rect.height);
            if control
                && want.0 > 0
                && want.1 > 0
                && (take_size || (t.owner != Some(false) && t.sent_size != Some(want)))
            {
                out.push(json!({ "t": "resize", "paneId": pid, "cols": want.0, "rows": want.1 }));
                t.sent_size = Some(want);
                t.owner = Some(true);
                t.set_pty(want.0, want.1);
            }
        }
        self.out.extend(out.into_iter().map(|v| (mid.clone(), Out::Text(v))));
    }

    fn select_machine(&mut self, i: usize) {
        if i == self.sel || i >= self.machines.len() {
            return;
        }
        if let Some(old) = self.mid() {
            for ((m, p), t) in self.terms.iter_mut() {
                if m == &old && t.attached {
                    t.attached = false;
                    self.out
                        .push((old.clone(), Out::Text(json!({ "t": "detach", "paneId": p }))));
                }
            }
        }
        self.sel = i;
        self.ws = None;
        self.focus = None;
        self.fix_selection();
    }

    pub fn activate(&mut self, item: &Item) {
        // Going to a pane leaves the chat screen (the chat keeps running).
        if self.chat.is_some() {
            self.chat_close();
        }
        match item {
            Item::Machine(i) => self.select_machine(*i),
            Item::Workspace(i, ws) => {
                self.select_machine(*i);
                self.ws = Some(ws.clone());
                self.focus = None;
            }
            Item::Agent(i, pane) => {
                self.select_machine(*i);
                self.pending_focus = Some(pane.clone());
            }
        }
        self.fix_selection();
        self.sync(false);
    }

    fn select_tab(&mut self, tab: String) {
        let (Some(mid), Some(ws)) = (self.mid(), self.ws.clone()) else {
            return;
        };
        self.tab_of_ws.insert((mid, ws), tab);
        self.focus = None;
        self.fix_selection();
        self.sync(false);
    }

    fn cycle_tab(&mut self, delta: isize) {
        let Some(ws) = self.ws.clone() else { return };
        let Some(t) = self.topo() else { return };
        let tabs: Vec<String> = t.tabs_of(&ws).iter().map(|t| t.id.clone()).collect();
        if tabs.is_empty() {
            return;
        }
        let cur = self
            .tab_id()
            .and_then(|c| tabs.iter().position(|t| *t == c))
            .unwrap_or(0);
        let next = (cur as isize + delta).rem_euclid(tabs.len() as isize) as usize;
        self.select_tab(tabs[next].clone());
    }

    // ── user input ───────────────────────────────────────────────────────

    fn send(&mut self, v: Value) {
        if let Some(mid) = self.mid() {
            self.out.push((mid, Out::Text(v)));
        }
    }

    /// Run a plugin action on the selected machine (`plugin_action`, control scope).
    pub(crate) fn plugin_action(&mut self, label: &str, plugin: &str, action: &str, pane: Option<String>) {
        if !self.can_control() {
            self.note = Some(format!("{label}: needs control"));
            return;
        }
        let id = format!("tui-{}", self.next_id);
        self.next_id += 1;
        self.pending
            .insert(id.clone(), Pending::PluginAction(label.to_string()));
        let mut f = json!({ "t": "plugin_action", "id": id, "pluginId": plugin, "actionId": action });
        if let Some(p) = pane {
            f["paneId"] = json!(p);
        }
        self.send(f);
    }

    /// `needs.respond` for super::needs (`what`: the item's title).
    pub(super) fn request_needs(&mut self, params: Value, what: String) {
        self.request("needs.respond", params, Pending::Needs(what));
    }

    /// Go to a pane of the selected machine, whatever its tab or workspace.
    pub(super) fn focus_pane_anywhere(&mut self, pane: &str) {
        self.activate(&Item::Agent(self.sel, pane.to_string()));
    }

    /// A `chat.*` request (super::chat_app).
    pub(super) fn request_chat(&mut self, method: &str, params: Value, req: super::chat_app::ChatReq) {
        self.request(method, params, Pending::Chat(req));
    }
    /// A layout request (super::arrange_ui). Without control a restore in progress ends here.
    pub(super) fn request_layout(&mut self, method: &str, params: Value, req: super::arrange_ui::LayoutReq) {
        if !self.can_control() {
            if let Some(r) = self.layouts.restore.take() {
                self.note = Some(super::arrange_ui::restore_text(&r, Some("forbidden_scope")));
            }
            return;
        }
        self.request(method, params, Pending::Layout(req));
    }

    fn request(&mut self, method: &str, params: Value, p: Pending) {
        if !self.can_control() {
            self.note = Some(format!("{method}: needs control"));
            return;
        }
        let id = format!("tui-{}", self.next_id);
        self.next_id += 1;
        self.pending.insert(id.clone(), p);
        self.send(json!({ "t": "req", "id": id, "method": method, "params": params }));
    }

    fn send_input(&mut self, bytes: Vec<u8>) {
        let Some(pane) = self.focus.clone() else { return };
        if !self.can_control() {
            self.note = Some("watching: input is not sent".into());
            return;
        }
        let f = DataFrame {
            kind: DataKind::Input,
            gap: false,
            req: 0,
            pane_id: pane,
            offset: 0,
            payload: bytes,
        };
        if let Some(mid) = self.mid() {
            self.out.push((mid, Out::Data(f)));
        }
    }

    fn decide(&mut self, behavior: &str) {
        let Some((pane, id, _)) = self.active_prompt() else {
            return;
        };
        if !self.can_control() {
            self.note = Some("watching: cannot answer prompts".into());
            return;
        }
        self.send(json!({ "t": "decision", "paneId": pane, "promptId": id, "behavior": behavior }));
        self.note = Some(format!("{behavior} sent for {pane}"));
        self.dismissed.insert(id);
    }

    /// Leave: detach every attached pane (the runner keeps them running).
    pub fn detach_all(&mut self) {
        let mut out = vec![];
        for ((m, p), t) in self.terms.iter_mut() {
            if t.attached {
                t.attached = false;
                out.push((m.clone(), Out::Text(json!({ "t": "detach", "paneId": p }))));
            }
        }
        self.out.extend(out);
        self.detached = true;
    }

    pub fn on_event(&mut self, e: Event) {
        match e {
            Event::Key(k) if k.kind != KeyEventKind::Release => self.on_key(k),
            // The chat screen covers the panes: the mouse does not reach them.
            Event::Mouse(_) if self.chat.is_some() => {}
            Event::Mouse(m) => self.on_mouse(m),
            Event::Paste(s) if matches!(self.mode, Mode::Rename(_)) => {
                if let Mode::Rename(r) = &mut self.mode {
                    for c in s.chars() {
                        push_name_char(&mut r.buf, if c.is_control() { ' ' } else { c });
                    }
                }
            }
            Event::Paste(s) if self.chat.is_some() && self.mode == Mode::Normal => {
                if let Some(c) = self.chat.as_mut() {
                    c.paste(&s);
                }
            }
            Event::Paste(s) => {
                let bracketed = self
                    .focus
                    .as_ref()
                    .and_then(|f| self.term(f))
                    .is_some_and(|t| t.emu.screen().bracketed_paste());
                let s = s.replace("\r\n", "\r").replace('\n', "\r");
                let b = if bracketed { format!("\x1b[200~{s}\x1b[201~") } else { s };
                self.send_input(b.into_bytes());
            }
            Event::Resize(w, h) => {
                self.size = (w, h);
                self.sync(true);
            }
            _ => {}
        }
    }

    pub fn on_key(&mut self, k: KeyEvent) {
        self.selection = None;
        if self.mode == Mode::Normal && !self.keymap.is_prefix(&k) {
            self.note = None;
        }
        match self.mode.clone() {
            Mode::Scroll(pane) => self.scroll_key(&pane, k),
            Mode::Prefix => {
                let shown = self.overlay_shown(std::time::Instant::now());
                self.mode = Mode::Normal;
                self.prefix_at = None;
                self.prefix_key(k, shown);
            }
            Mode::Keys => {
                self.mode = Mode::Normal;
                if k.code != KeyCode::Esc && k.code != KeyCode::Char('q') {
                    self.prefix_key(k, false);
                }
            }
            Mode::Palette => {
                self.mode = Mode::Normal;
                if let KeyCode::Char(c @ '1'..='9') = k.code {
                    self.palette_pick(c as usize - '1' as usize);
                }
            }
            Mode::Needs(cursor) => self.needs_key(cursor, k),
            Mode::Arrange(sel) => self.arrange_key(sel, k),
            Mode::Reply(item, buf) => self.reply_key(item, buf, k),
            Mode::AgentSettings(top) => self.agent_settings_key(top, k),
            Mode::Confirm(pane) => {
                self.mode = Mode::Normal;
                if matches!(k.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                    self.request("pane.close", json!({ "pane_id": pane }), Pending::Other);
                } else {
                    self.note = Some("close cancelled".into());
                }
            }
            Mode::Sidebar => self.sidebar_key(k),
            Mode::Rename(r) => self.rename_key(r, k),
            Mode::Normal => {
                if self.keymap.is_prefix(&k) {
                    self.arm_prefix();
                    return;
                }
                if self.chat.is_some() {
                    return self.chat_key(k);
                }
                if self.active_prompt().is_some() {
                    match k.code {
                        KeyCode::Char('a') | KeyCode::Char('y') => return self.decide("allow"),
                        KeyCode::Char('d') | KeyCode::Char('n') => return self.decide("deny"),
                        KeyCode::Esc => {
                            if let Some((_, id, _)) = self.active_prompt() {
                                self.dismissed.insert(id);
                            }
                            return;
                        }
                        _ => {}
                    }
                }
                let app_cursor = self
                    .focus
                    .as_ref()
                    .and_then(|f| self.term(f))
                    .is_some_and(|t| t.emu.screen().application_cursor());
                if let Some(b) = encode_key(&k, app_cursor) {
                    self.send_input(b);
                }
            }
        }
    }

    fn arm_prefix(&mut self) {
        self.mode = Mode::Prefix;
        self.prefix_at = Some(std::time::Instant::now());
    }

    /// The prefix overlay is up (armed, and its setting's delay has passed).
    pub fn overlay_shown(&self, now: std::time::Instant) -> bool {
        self.mode == Mode::Prefix
            && self
                .prefix_at
                .is_some_and(|t| self.keymap.overlay.shown(now.saturating_duration_since(t)))
    }

    /// What the overlay's "does it apply" rules need (queue 3j).
    pub fn overlay_ctx(&self) -> super::overlay::Ctx {
        let vis = self.visible();
        let focus = self.focus.clone();
        let n = |d: KeyCode| focus.as_ref().is_some_and(|f| view::neighbor(&vis, f, d).is_some());
        let tabs = match (self.topo(), self.ws.as_ref()) {
            (Some(t), Some(ws)) => t.tabs_of(ws).len(),
            _ => 0,
        };
        super::overlay::Ctx {
            control: self.can_control(),
            online: self.machine().is_some_and(|m| m.online),
            panes: self
                .tab()
                .map(|t| {
                    let mut v = vec![];
                    view::split_rects(&t.layout, view::areas(self.size).main, &mut v, &mut vec![]);
                    v.len()
                })
                .unwrap_or(0),
            focused: focus.is_some(),
            tabs,
            zoomed: self.zoomed_pane().is_some(),
            neighbours: [n(KeyCode::Left), n(KeyCode::Right), n(KeyCode::Up), n(KeyCode::Down)],
        }
    }

    /// After the prefix; `overlay` = the overlay was up (its `!` applies only then).
    fn prefix_key(&mut self, k: KeyEvent, overlay: bool) {
        let focus = self.focus.clone();
        if self.keymap.is_prefix(&k) {
            // Prefix twice: the prefix key itself goes to the pane.
            let app_cursor = focus
                .as_ref()
                .and_then(|f| self.term(f))
                .is_some_and(|t| t.emu.screen().application_cursor());
            if let Some(b) = encode_key(&k, app_cursor) {
                self.send_input(b);
            }
            return;
        }
        if let Some(a) = self.keymap.action(&k) {
            return self.action(a, focus);
        }
        match k.code {
            // Agent chat: open this pane's agent as a chat, or hand the chat back to a terminal.
            KeyCode::Char('C') if super::overlay::free(&self.keymap, "C") => self.chat_toggle(),
            KeyCode::Char('q') if self.chat.is_some() && super::overlay::free(&self.keymap, "q") => self.chat_close(),
            KeyCode::Char('[') => {
                if let Some(p) = focus {
                    self.enter_scroll(&p);
                }
            }
            KeyCode::Char('w') => {
                let items = self.sidebar_items();
                let sel = self.sel;
                self.sidebar_cursor = items
                    .iter()
                    .position(|i| matches!(i, Item::Workspace(m, w) if *m == sel && Some(w) == self.ws.as_ref()))
                    .unwrap_or(0);
                self.mode = Mode::Sidebar;
            }
            KeyCode::Char('?') if super::overlay::free(&self.keymap, "?") => self.mode = Mode::Keys,
            KeyCode::Char(':') if super::overlay::free(&self.keymap, ":") => self.mode = Mode::Palette,
            KeyCode::Char('!') if overlay && super::overlay::free(&self.keymap, "!") => {
                self.keymap.overlay = super::overlay::OverlayMode::Off;
                self.settings_out.push(json!({ "mode": "off" }));
                self.note = Some("prefix overlay off: Settings → Terminal on the web turns it back on".into());
            }
            // Needs you (Phase 13): `!` when the overlay is not up (it says "don't show again" there), `N` always.
            KeyCode::Char('!') if super::overlay::free(&self.keymap, "!") => self.needs_open(),
            KeyCode::Char('N') if super::overlay::free(&self.keymap, "N") => self.needs_open(),
            // Phase 17: what each agent gets on this machine, read only.
            KeyCode::Char('S') if super::overlay::free(&self.keymap, "S") => self.agent_settings_open(),
            KeyCode::Char('n') => self.cycle_tab(1),
            KeyCode::Char('p') => self.cycle_tab(-1),
            KeyCode::Char(c @ '1'..='9') => {
                let Some(ws) = self.ws.clone() else { return };
                let tab = self
                    .topo()
                    .and_then(|t| t.tabs_of(&ws).get(c as usize - '1' as usize).map(|t| t.id.clone()));
                if let Some(tab) = tab {
                    self.select_tab(tab);
                }
            }
            _ => {}
        }
    }

    /// A keybinding action (after the prefix).
    fn action(&mut self, a: Action, focus: Option<String>) {
        match a {
            Action::NewTab => {
                let ws = self.ws.clone();
                self.request(
                    "tab.create",
                    json!({ "workspace_id": ws, "focus": true }),
                    Pending::NewTab,
                );
            }
            Action::SplitRight | Action::SplitDown => {
                let Some(p) = focus else { return };
                let dir = if a == Action::SplitRight { "right" } else { "down" };
                self.request(
                    "pane.split",
                    json!({ "target_pane_id": p, "direction": dir, "focus": true }),
                    Pending::Split,
                );
            }
            Action::Zoom => {
                let (Some(p), Some(tab)) = (focus, self.tab_id()) else {
                    return;
                };
                self.request(
                    "pane.zoom",
                    json!({ "pane_id": p, "mode": "toggle" }),
                    Pending::Zoom(tab),
                );
            }
            Action::ClosePane => {
                if let Some(p) = focus {
                    self.mode = Mode::Confirm(p);
                }
            }
            Action::Detach => self.detach_all(),
            Action::Arrange => self.arrange_open(),
            Action::EvenOut => self.even_out(),
            Action::RenameTab | Action::RenameWorkspace | Action::RenamePane => self.start_rename(a, focus),
            Action::FocusLeft | Action::FocusRight | Action::FocusUp | Action::FocusDown => {
                if self.zoomed_pane().is_some() {
                    self.note = Some(format!(
                        "zoomed: {} {} to unzoom",
                        self.keymap.prefix_label(),
                        self.keymap.key(Action::Zoom)
                    ));
                    return;
                }
                let dir = match a {
                    Action::FocusLeft => KeyCode::Left,
                    Action::FocusRight => KeyCode::Right,
                    Action::FocusUp => KeyCode::Up,
                    _ => KeyCode::Down,
                };
                if let Some(n) = focus.and_then(|f| view::neighbor(&self.visible(), &f, dir)) {
                    self.focus = Some(n);
                }
            }
        }
    }

    // ── rename ───────────────────────────────────────────────────────────

    /// `,` (tab), `$` (workspace), `P` (pane), like tmux: prompt on the
    /// status line, prefilled with the current name. Needs control and a
    /// connected machine; the runner applies the shared name rules.
    fn start_rename(&mut self, a: Action, focus: Option<String>) {
        let kind = match a {
            Action::RenameWorkspace => NameKind::Workspace,
            Action::RenamePane => NameKind::Pane,
            _ => NameKind::Tab,
        };
        if !self.machine().is_some_and(|m| m.online) {
            self.note = Some("machine offline: names can be changed once it reconnects".into());
            return;
        }
        if !self.can_control() {
            self.note = Some(format!("rename {}: needs control", kind.word()));
            return;
        }
        let Some(t) = self.topo() else { return };
        let target = match kind {
            NameKind::Workspace => self.ws.clone().and_then(|w| {
                t.workspaces
                    .iter()
                    .find(|x| x.id == w)
                    .map(|x| (w.clone(), x.name.clone(), x.name.clone()))
            }),
            NameKind::Tab => self.tab_id().and_then(|id| {
                let ws = t.tab(&id)?.workspace_id.clone();
                let n = t.tabs_of(&ws).iter().position(|x| x.id == id)? + 1;
                let name = t.tab(&id)?.name.clone();
                Some((id, format!("{n}"), name))
            }),
            NameKind::Pane => focus.and_then(|id| {
                let label = t.pane(&id)?.label.clone().unwrap_or_default();
                Some((id.clone(), id, label))
            }),
        };
        let Some((id, what, current)) = target else { return };
        let mut buf = String::new();
        for c in current.chars() {
            push_name_char(&mut buf, c);
        }
        self.mode = Mode::Rename(Rename {
            kind,
            id,
            what: format!("{} {what}", kind.word()),
            buf,
        });
    }

    fn rename_key(&mut self, mut r: Rename, k: KeyEvent) {
        use ratatui::crossterm::event::KeyModifiers;
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.note = Some("rename cancelled".into());
                return;
            }
            KeyCode::Enter => {
                self.mode = Mode::Normal;
                let name = tmuxedo_protocol::names::clean_name(&r.buf);
                let label = if name.is_empty() {
                    Value::Null
                } else {
                    Value::String(name)
                };
                let mut params = json!({ "label": label });
                params[r.kind.id_param()] = json!(r.id);
                self.request(r.kind.method(), params, Pending::Rename(r.kind));
                return;
            }
            KeyCode::Backspace => {
                r.buf.pop();
            }
            KeyCode::Char('u') if ctrl => r.buf.clear(),
            KeyCode::Char('w') if ctrl => {
                let t = r.buf.trim_end().len();
                let cut = r.buf[..t].rfind(' ').map(|i| i + 1).unwrap_or(0);
                r.buf.truncate(cut);
            }
            KeyCode::Char(c) if !ctrl && !k.modifiers.contains(KeyModifiers::ALT) => push_name_char(&mut r.buf, c),
            _ => {}
        }
        self.mode = Mode::Rename(r);
    }

    // ── scroll mode ──────────────────────────────────────────────────────

    fn pane_height(&self, pane: &str) -> usize {
        self.visible()
            .into_iter()
            .find(|(p, _)| p == pane)
            .map(|(_, r)| r.height as usize)
            .unwrap_or(10)
    }

    /// How many lines `pane` is scrolled back.
    pub fn scroll_pos(&self, pane: &str) -> usize {
        self.term(pane).map(|t| t.emu.screen().scrollback()).unwrap_or(0)
    }

    fn enter_scroll(&mut self, pane: &str) {
        if self.term(pane).is_some() {
            self.mode = Mode::Scroll(pane.to_string());
        }
    }

    /// Scroll `pane` back (+) or forward (-) by `lines`; back at the bottom
    /// by scrolling forward leaves scroll mode.
    fn scroll_by(&mut self, pane: &str, lines: isize, leave_at_bottom: bool) {
        let mid = self.mid().unwrap_or_default();
        let Some(t) = self.terms.get_mut(&(mid, pane.to_string())) else {
            return;
        };
        let max = t.history();
        let now = t.emu.screen().scrollback() as isize;
        let to = (now + lines).clamp(0, max as isize) as usize;
        t.emu.parser_mut().screen_mut().set_scrollback(to);
        if to == 0 && lines < 0 && leave_at_bottom {
            self.mode = Mode::Normal;
        }
    }

    fn leave_scroll(&mut self, pane: &str) {
        let mid = self.mid().unwrap_or_default();
        if let Some(t) = self.terms.get_mut(&(mid, pane.to_string())) {
            t.emu.parser_mut().screen_mut().set_scrollback(0);
        }
        self.mode = Mode::Normal;
    }

    fn scroll_key(&mut self, pane: &str, k: KeyEvent) {
        let page = self.pane_height(pane).saturating_sub(1).max(1) as isize;
        match k.code {
            KeyCode::Char('q') | KeyCode::Esc => self.leave_scroll(pane),
            KeyCode::Up | KeyCode::Char('k') => self.scroll_by(pane, 1, false),
            KeyCode::Down | KeyCode::Char('j') => self.scroll_by(pane, -1, false),
            KeyCode::PageUp | KeyCode::Char('b') => self.scroll_by(pane, page, false),
            KeyCode::PageDown | KeyCode::Char(' ') | KeyCode::Char('f') => self.scroll_by(pane, -page, false),
            KeyCode::Home | KeyCode::Char('g') => self.scroll_by(pane, isize::MAX / 2, false),
            KeyCode::End | KeyCode::Char('G') => self.scroll_by(pane, isize::MIN / 2, false),
            _ if self.keymap.is_prefix(&k) => {
                self.leave_scroll(pane);
                self.arm_prefix();
            }
            _ => {}
        }
    }

    fn sidebar_key(&mut self, k: KeyEvent) {
        let items = self.sidebar_items();
        match k.code {
            KeyCode::Up | KeyCode::Char('k') => self.sidebar_cursor = self.sidebar_cursor.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                self.sidebar_cursor = (self.sidebar_cursor + 1).min(items.len().saturating_sub(1))
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                if let Some(it) = items.get(self.sidebar_cursor) {
                    self.activate(it);
                }
                self.mode = Mode::Normal;
            }
            KeyCode::Esc | KeyCode::Char('q') => self.mode = Mode::Normal,
            _ if self.keymap.is_prefix(&k) => self.arm_prefix(),
            _ => {}
        }
    }

    /// Mouse: the sidebar and tab bar are ours; over a pane, a press on an
    /// unfocused pane focuses it, and once focused, events go to the program
    /// if it enabled mouse reporting (encoded per its mode), else the wheel
    /// scrolls the pane's history.
    pub fn on_mouse(&mut self, m: MouseEvent) {
        let a = view::areas(self.size);
        let (x, y) = (m.column, m.row);
        let inside = |r: Rect| x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height;
        let press = m.kind == MouseEventKind::Down(MouseButton::Left);
        // An active selection follows the drag (clamped to its pane) and is
        // copied on release.
        if let Some(sel) = self.selection.as_ref().filter(|s| s.active) {
            let pane = sel.pane.clone();
            if let Some((_, r)) = self.visible().into_iter().find(|(p, _)| *p == pane) {
                let col = x.clamp(r.x, r.right().saturating_sub(1)) - r.x;
                let row = y.clamp(r.y, r.bottom().saturating_sub(1)) - r.y;
                match m.kind {
                    MouseEventKind::Drag(MouseButton::Left) => {
                        if let Some(s) = self.selection.as_mut() {
                            s.head = (row, col);
                        }
                        return;
                    }
                    MouseEventKind::Up(MouseButton::Left) => {
                        if let Some(s) = self.selection.as_mut() {
                            s.head = (row, col);
                            s.active = false;
                        }
                        self.copy_selection();
                        return;
                    }
                    _ => {}
                }
            }
        }
        if matches!(m.kind, MouseEventKind::Down(_)) {
            self.selection = None;
        }
        // A drag or release continues to the pane that got the press.
        if !matches!(m.kind, MouseEventKind::Down(_)) {
            if let Some(p) = self.mouse_pane.clone() {
                if matches!(m.kind, MouseEventKind::Up(_)) {
                    self.mouse_pane = None;
                }
                if let Some((_, r)) = self.visible().into_iter().find(|(q, _)| *q == p) {
                    let col = x.clamp(r.x, r.right().saturating_sub(1)) - r.x;
                    let row = y.clamp(r.y, r.bottom().saturating_sub(1)) - r.y;
                    self.forward_mouse(&p, m, col, row);
                }
                return;
            }
        }
        if inside(a.sidebar) {
            if press {
                let items = self.sidebar_items();
                if let Some(it) = items.get((y - a.sidebar.y) as usize) {
                    self.sidebar_cursor = (y - a.sidebar.y) as usize;
                    self.activate(it);
                }
                self.mode = Mode::Normal;
            }
            return;
        }
        if inside(a.tabbar) {
            if press {
                if let Some((tab, ..)) = view::tab_spans(self)
                    .into_iter()
                    .find(|(_, x0, x1, _)| x >= *x0 && x < *x1)
                {
                    self.select_tab(tab);
                }
            }
            return;
        }
        let Some((p, r)) = self.visible().into_iter().find(|(_, r)| inside(*r)) else {
            return;
        };
        let (col, row) = (x - r.x, y - r.y);
        let reporting = self
            .term(&p)
            .is_some_and(|t| t.emu.screen().mouse_protocol_mode() != vt100::MouseProtocolMode::None);
        let scrolled = matches!(&self.mode, Mode::Scroll(s) if *s == p);
        if matches!(m.kind, MouseEventKind::Down(_)) && self.focus.as_ref() != Some(&p) {
            // Click to focus (not forwarded: the first click picks the pane).
            if let Mode::Scroll(s) = self.mode.clone() {
                self.leave_scroll(&s);
            }
            self.focus = Some(p);
            if self.mode == Mode::Sidebar {
                self.mode = Mode::Normal;
            }
            return;
        }
        if self.mode == Mode::Sidebar && press {
            self.mode = Mode::Normal;
        }
        // Copy-on-select: a left drag selects when the program did not ask
        // for the mouse, or with Shift held when it did (and in scroll mode).
        let shift = m.modifiers.contains(ratatui::crossterm::event::KeyModifiers::SHIFT);
        if press && self.copy_on_select && (!reporting || shift || scrolled) {
            self.selection = Some(Selection {
                pane: p,
                anchor: (row, col),
                head: (row, col),
                active: true,
            });
            return;
        }
        if reporting && !scrolled {
            if matches!(m.kind, MouseEventKind::Down(_)) {
                self.mouse_pane = Some(p.clone());
            }
            self.forward_mouse(&p, m, col, row);
            return;
        }
        match m.kind {
            MouseEventKind::ScrollUp => {
                if !scrolled {
                    self.enter_scroll(&p);
                }
                self.scroll_by(&p, WHEEL_LINES as isize, true);
            }
            MouseEventKind::ScrollDown if scrolled => self.scroll_by(&p, -(WHEEL_LINES as isize), true),
            _ => {}
        }
    }

    /// Copy the finished selection from the pane's emulator (wrapped lines
    /// joined, wide characters once) to the local clipboard via OSC 52.
    fn copy_selection(&mut self) {
        let Some(sel) = self.selection.clone() else { return };
        let (s, e) = sel.range();
        if s == e {
            self.selection = None; // a click, not a selection
            return;
        }
        let Some(t) = self.term(&sel.pane) else { return };
        let screen = t.emu.screen();
        let (rows, cols) = screen.size();
        if s.0 >= rows {
            return;
        }
        let (er, ec) = (e.0.min(rows - 1), (e.1 + 1).min(cols));
        let text = screen.contents_between(s.0, s.1.min(cols), er, ec);
        if text.is_empty() {
            self.note = Some("nothing to copy".into());
            return;
        }
        let (bytes, n, cut) = osc52(&text, self.in_tmux);
        self.local_out.push(bytes);
        self.note = Some(if cut {
            format!("copied {n} chars (cut to the clipboard limit)")
        } else {
            format!("copied {n} chars")
        });
    }

    fn forward_mouse(&mut self, pane: &str, m: MouseEvent, col: u16, row: u16) {
        let Some(t) = self.term(pane) else { return };
        let s = t.emu.screen();
        if let Some(b) = encode_mouse(
            m.kind,
            m.modifiers,
            col,
            row,
            s.mouse_protocol_mode(),
            s.mouse_protocol_encoding(),
        ) {
            if self.focus.as_deref() == Some(pane) {
                self.send_input(b);
            }
        }
    }
}

/// `claudeAuth` from a forwarded runner hello (`{runner: {...}}` or the runner object).
fn hello_auth(h: &Value) -> Option<(String, String)> {
    let a = if h["runner"].is_object() {
        &h["runner"]["claudeAuth"]
    } else {
        &h["claudeAuth"]
    };
    let state = a["state"].as_str()?;
    Some((state.to_string(), a["reason"].as_str().unwrap_or("").to_string()))
}

/// Type one character into a name being edited: control characters are
/// dropped and the buffer stops at the name limit (the runner cleans the
/// rest with the shared rules).
pub fn push_name_char(buf: &mut String, c: char) {
    if c.is_control() || buf.chars().count() >= tmuxedo_protocol::names::NAME_MAX {
        return;
    }
    buf.push(c);
}
