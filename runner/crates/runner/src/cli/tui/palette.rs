//! The command palette in the TUI (docs/design/navigation.md §0; the web's is
//! web/src/lib/palette.ts): `prefix k` (the `open_palette` keybinding) opens
//! one box that reaches any session, machine, project or pane action.
//!
//! The sections and their order are the web's: Needs you, Sessions, Actions,
//! Machines and projects. Before you type: Needs you and a few suggested
//! actions. Typing filters every section (five rows each, best first); `>`
//! narrows to actions and `@` to machines and projects. The actions are the
//! prefix overlay's (super::overlay), with the user's keys and the same
//! "does this apply" rules: a row that does not apply stays, greyed, and
//! choosing it says why. ↑↓ move, Enter goes, Esc closes.
//!
//! Recent (before you type, after Needs you): the account's list, the same
//! one the web palette keeps (`GET`/`POST /api/me/palette-recent`), so a
//! place opened in a browser shows here and the other way round. The TUI
//! lists the machines, projects and sessions it can go to (the web's pages
//! are left out) and adds each one chosen here.
//!
//! → on a machine or session row opens its action panel (the web's: Open,
//! Events, Machine settings; Open, Changes, Replay; plus the TUI's own Files
//! and Agent settings). ← or Esc goes back to the same row.

use super::app::{App, Item, Mode};
use super::keymap::Action;
use super::overlay::{self, Pick};
use super::view::status_word;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tmuxedo_protocol::frames::AgentStatus;

/// What is typed and the selected row (an index into [`rank`]'s rows).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct State {
    pub query: String,
    pub cursor: usize,
    /// A row's action panel, while open.
    pub panel: Option<Panel>,
}

/// The open action panel: the row it belongs to and the selected entry.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Panel {
    pub row: usize,
    pub cursor: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Section {
    Needs,
    Recent,
    Sessions,
    Actions,
    Places,
}

impl Section {
    pub fn title(self) -> &'static str {
        match self {
            Section::Needs => "Needs you",
            Section::Recent => "Recent",
            Section::Sessions => "Sessions",
            Section::Actions => "Actions",
            Section::Places => "Machines and projects",
        }
    }
}

/// What choosing a row does.
#[derive(Debug, Clone, PartialEq)]
pub enum Go {
    /// A sidebar row: a machine, a project (workspace) or a session.
    Item(Item),
    /// A needs-you item: its machine, and its pane or chat.
    Needs(usize, String, Option<String>),
    /// A prefix overlay row.
    Pick(Pick),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    /// The web palette's id for the same place (`machine:{id}`, `project:{machine}:{workspace}`,
    /// `session:{machine}:{pane}`): how Recent is matched across devices.
    pub id: Option<String>,
    pub section: Section,
    pub title: String,
    pub subtitle: String,
    /// The key after the prefix (actions only).
    pub key: Option<String>,
    /// Why it does nothing right now.
    pub disabled: Option<&'static str>,
    pub go: Go,
    pub suggested: bool,
    pub boost: i32,
}

pub const MAX_PER_SECTION: usize = 5;
/// Recent rows shown before you type (the web's RECENT_SHOWN).
pub const RECENT_SHOWN: usize = 4;
/// Places the account's Recent keeps (the web's RECENT_MAX).
pub const RECENT_MAX: usize = 20;
const RECENT_BOOST: i32 = 25;
pub const RECENT_PATH: &str = "/api/me/palette-recent";

/// One remembered place, as web stores it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecentPlace {
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    pub href: String,
}

/// The account's list from web, defensively: valid entries only, each id once, at most RECENT_MAX.
pub fn parse_recent(v: &Value) -> Vec<RecentPlace> {
    let mut out: Vec<RecentPlace> = vec![];
    for x in v.as_array().into_iter().flatten() {
        let Ok(p) = serde_json::from_value::<RecentPlace>(x.clone()) else {
            continue;
        };
        if p.id.is_empty() || !p.href.starts_with('/') || p.href.starts_with("//") || out.iter().any(|o| o.id == p.id) {
            continue;
        }
        out.push(p);
        if out.len() >= RECENT_MAX {
            break;
        }
    }
    out
}

/// Put a place first (each id once, at most RECENT_MAX).
pub fn push_recent(list: &mut Vec<RecentPlace>, p: RecentPlace) {
    list.retain(|r| r.id != p.id);
    list.insert(0, p);
    list.truncate(RECENT_MAX);
}

/// One entry of a row's action panel.
#[derive(Debug, Clone, PartialEq)]
pub struct PanelEntry {
    pub title: &'static str,
    pub act: PanelAct,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PanelAct {
    /// Go to the row (Enter on it).
    Open,
    /// Go to the row's machine, then a prefix key there (`F` files, `S` agent settings).
    Key(char),
    /// A web page for it (this path on the web app), in the browser.
    Web(String),
    /// Back to the results, on the same row.
    Back,
}

/// The web path of a machine, project or session (as the web palette's hrefs).
fn href_of(app: &App, it: &Item) -> Option<String> {
    use super::search::enc;
    let m = |i: usize| app.machines.get(i).map(|m| format!("/machines/{}", enc(&m.id)));
    match it {
        Item::Machine(i) => m(*i),
        Item::Workspace(i, w) => m(*i).map(|h| format!("{h}?ws={}", enc(w))),
        Item::Agent(i, p) => m(*i).map(|h| format!("{h}?pane={}", enc(p))),
        _ => None,
    }
}

/// A row's action panel: a machine's and a session's (the web's entries, and the TUI's own); none for the rest.
pub fn panel_of(app: &App, r: &Row) -> Vec<PanelEntry> {
    let Go::Item(it) = &r.go else { return vec![] };
    let Some(href) = href_of(app, it) else { return vec![] };
    let e = |title, act| PanelEntry { title, act };
    match it {
        Item::Machine(_) => vec![
            e("Open", PanelAct::Open),
            e("Files", PanelAct::Key('F')),
            e("Agent settings", PanelAct::Key('S')),
            e("Events (browser)", PanelAct::Web(format!("{href}/events"))),
            e("Machine settings (browser)", PanelAct::Web(format!("{href}/settings"))),
            e("Back to results", PanelAct::Back),
        ],
        Item::Agent(..) => vec![
            e("Open", PanelAct::Open),
            e("Changes (browser)", PanelAct::Web(format!("{href}&sheet=changes"))),
            e("Replay (browser)", PanelAct::Web(format!("{href}&sheet=replay"))),
            e("Back to results", PanelAct::Back),
        ],
        _ => vec![],
    }
}
const SUGGESTED: [Action; 4] = [Action::Arrange, Action::NewTab, Action::SplitRight, Action::RenamePane];

fn word_start(t: &[char], i: usize) -> bool {
    i == 0
        || matches!(t[i - 1], ' ' | '-' | '_' | '/' | '.' | ':' | '·' | '(')
        || (t[i - 1].is_lowercase() && t[i].is_uppercase())
}

/// `query` against `text` (the web's fuzzyMatch): a substring scores highest
/// at a word start; else every letter continues the last one or starts a
/// word ("ssbs" → Split side by side). None when it does not match.
pub fn fuzzy(query: &str, text: &str) -> Option<i32> {
    let q: Vec<char> = query.trim().to_lowercase().chars().collect();
    if q.is_empty() {
        return Some(0);
    }
    let orig: Vec<char> = text.chars().collect();
    let t: Vec<char> = text.to_lowercase().chars().collect();
    if t.len() != orig.len() {
        return None;
    }
    let mut best: Option<i32> = None;
    for from in 0..t.len() {
        if t[from..].starts_with(&q) {
            let s = 100
                + if word_start(&orig, from) { 60 } else { 0 }
                + if from == 0 { 30 } else { 0 }
                + q.len() as i32 * 4
                - from.min(20) as i32;
            best = Some(best.map_or(s, |b: i32| b.max(s)));
        }
    }
    if best.is_some() {
        return best;
    }
    let (mut at, mut prev, mut score) = (0usize, None::<usize>, 0i32);
    for &ch in q.iter().filter(|c| **c != ' ') {
        let mut hit = prev.filter(|p| t.get(p + 1) == Some(&ch)).map(|p| p + 1);
        if hit.is_none() {
            hit = (at..t.len()).find(|&i| t[i] == ch && word_start(&orig, i));
        }
        let h = hit?;
        score += 6 + if Some(h) == prev.map(|p| p + 1) { 5 } else { 10 } - (h - at).min(10) as i32 / 2;
        prev = Some(h);
        at = h + 1;
    }
    Some(score)
}

/// The title first; else the subtitle at half weight.
fn score(query: &str, r: &Row) -> Option<i32> {
    fuzzy(query, &r.title).or_else(|| fuzzy(query, &r.subtitle).map(|s| s / 2))
}

fn capitalise(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_uppercase().chain(c).collect())
        .unwrap_or_default()
}

/// Every row the palette could show, from what the TUI knows now.
pub fn rows(app: &App) -> Vec<Row> {
    let mut v = vec![];
    for (i, m) in app.machines.iter().enumerate() {
        let here = i == app.sel;
        for (n, it) in m.needs.items().into_iter().enumerate() {
            v.push(Row {
                id: None,
                section: Section::Needs,
                title: it.title.clone(),
                subtitle: format!("{} · {}", it.plugin, m.name),
                key: None,
                disabled: None,
                go: Go::Needs(i, it.pane_id.clone(), it.chat_id.clone()),
                suggested: false,
                boost: if here { 10 } else { 0 } - n as i32,
            });
        }
        let state = if m.online { "online" } else { "offline" };
        v.push(Row {
            id: Some(format!("machine:{}", m.id)),
            section: Section::Places,
            title: m.name.clone(),
            subtitle: format!("Machine · {state}"),
            key: None,
            disabled: None,
            go: Go::Item(Item::Machine(i)),
            suggested: false,
            boost: if here { 20 } else { 0 },
        });
        let Some(t) = &m.topo else { continue };
        for w in t.workspaces_sorted() {
            let off = if m.online { String::new() } else { " · offline".into() };
            v.push(Row {
                id: Some(format!("project:{}:{}", m.id, w.id)),
                section: Section::Places,
                title: if w.name.is_empty() {
                    w.cwd.clone()
                } else {
                    w.name.clone()
                },
                subtitle: format!("Project on {}{off}", m.name),
                key: None,
                disabled: None,
                go: Go::Item(Item::Workspace(i, w.id.clone())),
                suggested: false,
                boost: if here { 15 } else { 0 },
            });
            for tab in t.tabs_of(&w.id) {
                for pid in super::app::layout_panes(&tab.layout) {
                    let Some(p) = t.pane(&pid) else { continue };
                    let Some(a) = &p.agent else { continue };
                    let title = p
                        .label
                        .clone()
                        .filter(|l| !l.is_empty())
                        .or_else(|| (!p.title.is_empty()).then(|| p.title.clone()))
                        .unwrap_or_else(|| a.kind.clone());
                    let mut sub = vec![
                        status_word(a.status).to_string(),
                        a.kind.clone(),
                        w.name.clone(),
                        m.name.clone(),
                    ];
                    if !m.online {
                        sub.push("offline".into());
                    }
                    sub.retain(|s| !s.is_empty());
                    v.push(Row {
                        id: Some(format!("session:{}:{pid}", m.id)),
                        section: Section::Sessions,
                        title,
                        subtitle: sub.join(" · "),
                        key: None,
                        disabled: None,
                        go: Go::Item(Item::Agent(i, pid.clone())),
                        suggested: false,
                        boost: if here { 20 } else { 0 } + if a.status == AgentStatus::Blocked { 10 } else { 0 }
                            - if m.online { 0 } else { 10 },
                    });
                }
            }
        }
    }
    let machine = app.machine().map(|m| m.name.clone()).unwrap_or_default();
    for g in overlay::groups(&app.keymap, &app.overlay_ctx(), false) {
        for it in g.items {
            // "Go to tab 1-9" is a key range, not one thing to choose; the palette itself is here already.
            if matches!(it.pick, Pick::GoToTab | Pick::Action(Action::OpenPalette)) {
                continue;
            }
            let suggested = matches!(it.pick, Pick::Action(a) if SUGGESTED.contains(&a));
            v.push(Row {
                id: None,
                section: Section::Actions,
                title: capitalise(&it.label),
                subtitle: match it.disabled {
                    Some(why) => capitalise(why),
                    None if machine.is_empty() => g.title.to_string(),
                    None => format!("{} · {machine}", g.title),
                },
                key: Some(it.key.clone()),
                disabled: it.disabled,
                go: Go::Pick(it.pick),
                suggested,
                boost: 30,
            });
        }
    }
    // Recent: the account's places that are here now, newest first; and they rank higher when typing.
    let mut recent = vec![];
    for (n, p) in app.recent.iter().enumerate() {
        if let Some(r) = v.iter_mut().find(|r| r.id.as_deref() == Some(p.id.as_str())) {
            r.boost += RECENT_BOOST;
            if recent.len() < RECENT_SHOWN {
                recent.push(Row {
                    section: Section::Recent,
                    boost: 1000 - n as i32,
                    ..r.clone()
                });
            }
        }
    }
    v.extend(recent);
    v
}

/// What to show for `query`, in section order, best first in each.
pub fn rank(all: Vec<Row>, query: &str) -> Vec<Row> {
    let mut q = query.trim();
    let mut only = None;
    if let Some(rest) = q.strip_prefix('>') {
        only = Some(Section::Actions);
        q = rest.trim();
    } else if let Some(rest) = q.strip_prefix('@') {
        only = Some(Section::Places);
        q = rest.trim();
    }
    let mut scored: Vec<(i32, Row)> = all
        .into_iter()
        .filter(|r| only.is_none_or(|s| r.section == s))
        // Recent is the empty query's (the same places rank higher once you type).
        .filter(|r| r.section != Section::Recent || (q.is_empty() && only.is_none()))
        .filter_map(|r| {
            if q.is_empty() {
                let keep = only.is_some()
                    || matches!(r.section, Section::Needs | Section::Recent)
                    || (r.section == Section::Actions && r.suggested);
                keep.then_some((r.boost, r))
            } else {
                let s = score(q, &r)?;
                Some((s + r.boost - if r.disabled.is_some() { 5 } else { 0 }, r))
            }
        })
        .collect();
    scored.sort_by(|(a, x), (b, y)| x.section.cmp(&y.section).then(b.cmp(a)).then(x.title.cmp(&y.title)));
    let mut out: Vec<Row> = vec![];
    for (_, r) in scored {
        let cap = if q.is_empty() && only.is_none() && r.section == Section::Actions {
            SUGGESTED.len()
        } else {
            MAX_PER_SECTION
        };
        if out.iter().filter(|x| x.section == r.section).count() < cap {
            out.push(r);
        }
    }
    out
}

impl App {
    /// `prefix k` (the `open_palette` action).
    pub fn command_open(&mut self) {
        self.mode = Mode::Command(State::default());
        self.recent_fetch = true;
    }

    /// The rows the palette shows now.
    pub fn command_rows(&self, st: &State) -> Vec<Row> {
        rank(rows(self), &st.query)
    }

    /// A key while the palette is open.
    pub fn command_key(&mut self, mut st: State, k: KeyEvent) {
        if k.kind == KeyEventKind::Release {
            return;
        }
        let shown = self.command_rows(&st);
        let n = shown.len();
        if let Some(mut p) = st.panel.take() {
            let Some(row) = shown.get(p.row).cloned() else {
                self.mode = Mode::Command(st);
                return;
            };
            let entries = panel_of(self, &row);
            let m = entries.len();
            match k.code {
                KeyCode::Esc | KeyCode::Left => {
                    st.cursor = p.row;
                    self.mode = Mode::Command(st);
                    return;
                }
                KeyCode::Up => {
                    p.cursor = if p.cursor == 0 {
                        m.saturating_sub(1)
                    } else {
                        p.cursor - 1
                    }
                }
                KeyCode::Down => p.cursor = if m == 0 { 0 } else { (p.cursor + 1) % m },
                KeyCode::Enter => {
                    let Some(e) = entries.get(p.cursor.min(m.saturating_sub(1))).cloned() else {
                        st.cursor = p.row;
                        self.mode = Mode::Command(st);
                        return;
                    };
                    if e.act == PanelAct::Back {
                        st.cursor = p.row;
                        self.mode = Mode::Command(st);
                        return;
                    }
                    self.mode = Mode::Normal;
                    self.command_panel_run(row, e.act);
                    return;
                }
                _ => {}
            }
            st.panel = Some(p);
            self.mode = Mode::Command(st);
            return;
        }
        match k.code {
            KeyCode::Right => {
                let at = st.cursor.min(n.saturating_sub(1));
                if let Some(r) = shown.get(at) {
                    if r.disabled.is_none() && !panel_of(self, r).is_empty() {
                        st.cursor = at;
                        st.panel = Some(Panel { row: at, cursor: 0 });
                    }
                }
            }
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                return;
            }
            KeyCode::Up => {
                st.cursor = if st.cursor == 0 {
                    n.saturating_sub(1)
                } else {
                    st.cursor - 1
                }
            }
            KeyCode::Down => st.cursor = if n == 0 { 0 } else { (st.cursor + 1) % n },
            KeyCode::Enter => {
                self.mode = Mode::Normal;
                if let Some(r) = shown.get(st.cursor.min(n.saturating_sub(1))) {
                    self.command_choose(r.clone());
                } else if !st.query.trim().is_empty() {
                    self.note = Some(format!("No direct match for {}", st.query.trim()));
                }
                return;
            }
            KeyCode::Backspace => {
                st.query.pop();
                st.cursor = 0;
            }
            KeyCode::Char(c) if !k.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
                st.query.push(c);
                st.cursor = 0;
            }
            _ => {}
        }
        self.mode = Mode::Command(st);
    }

    /// Remember a place in the account's Recent (here at once; web through the event loop).
    fn command_remember(&mut self, r: &Row) {
        let (Some(id), Go::Item(it)) = (&r.id, &r.go) else {
            return;
        };
        let Some(href) = href_of(self, it) else { return };
        let p = RecentPlace {
            id: id.clone(),
            title: r.title.clone(),
            subtitle: (!r.subtitle.is_empty()).then(|| r.subtitle.clone()),
            href,
        };
        if let Ok(body) = serde_json::to_value(&p) {
            self.recent_out.push(body);
        }
        push_recent(&mut self.recent, p);
    }

    /// An action panel entry (not Back).
    pub fn command_panel_run(&mut self, r: Row, act: PanelAct) {
        match act {
            PanelAct::Open => self.command_choose(r),
            PanelAct::Key(c) => {
                if let Go::Item(it) = &r.go {
                    let m = match it {
                        Item::Machine(i) | Item::Workspace(i, _) | Item::Agent(i, _) => *i,
                        _ => return,
                    };
                    self.activate(&Item::Machine(m));
                    self.run_prefix_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
                }
            }
            PanelAct::Web(path) => {
                self.open_urls
                    .push(format!("{}{path}", self.web_url.trim_end_matches('/')));
                self.note = Some("Opening it in your browser.".into());
            }
            PanelAct::Back => {}
        }
    }

    /// Go, run, or say why not.
    pub fn command_choose(&mut self, r: Row) {
        if let Some(why) = r.disabled {
            self.note = Some(format!("{}: {why}", r.title));
            return;
        }
        self.command_remember(&r);
        match r.go {
            Go::Item(it) => self.activate(&it),
            Go::Needs(m, pane, chat) => {
                if !pane.is_empty() {
                    self.activate(&Item::Agent(m, pane));
                } else {
                    self.activate(&Item::Machine(m));
                    if let Some(c) = chat {
                        self.chat_open(&c);
                    }
                }
            }
            Go::Pick(Pick::Action(a)) => {
                let focus = self.focus.clone();
                self.run_action(a, focus);
            }
            Go::Pick(p) => {
                let key = match p {
                    Pick::Scroll => '[',
                    Pick::Workspaces => 'w',
                    Pick::NextTab => 'n',
                    Pick::PrevTab => 'p',
                    Pick::AllKeys => '?',
                    Pick::AgentSettings => 'S',
                    Pick::Files => 'F',
                    Pick::Wake => 'W',
                    Pick::Search => '/',
                    Pick::Action(_) | Pick::GoToTab => return,
                };
                self.run_prefix_key(KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE));
            }
        }
    }

    /// The status line while the palette is open.
    pub fn command_line(&self) -> String {
        match &self.mode {
            Mode::Command(State { panel: Some(_), .. }) => "actions: ↑↓ move  enter go  ← or esc back".into(),
            _ => "search: type to filter  > actions  @ machines  ↑↓ move  enter go  → more  esc close".into(),
        }
    }
}

/// Draw the palette centred over the panes.
pub fn draw(app: &App, st: &State, main: Rect, buf: &mut Buffer) {
    let shown = app.command_rows(st);
    let mut lines: Vec<(String, Option<String>, Style)> = vec![];
    let panel = st.panel.as_ref().and_then(|p| shown.get(p.row).map(|r| (p, r)));
    if let Some((p, row)) = panel {
        lines.push((
            format!("Actions for {}", row.title),
            None,
            Style::default().add_modifier(Modifier::BOLD).fg(Color::Cyan),
        ));
        let entries = panel_of(app, row);
        let c = p.cursor.min(entries.len().saturating_sub(1));
        for (i, e) in entries.iter().enumerate() {
            let mark = if i == c { "▶ " } else { "  " };
            let style = if i == c {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            lines.push((format!("{mark}{}", e.title), None, style));
        }
    }
    let cursor = st.cursor.min(shown.len().saturating_sub(1));
    let mut last = None;
    for (i, r) in shown.iter().enumerate().filter(|_| panel.is_none()) {
        if last != Some(r.section) {
            if last.is_some() {
                lines.push((String::new(), None, Style::default()));
            }
            lines.push((
                r.section.title().to_string(),
                None,
                Style::default().add_modifier(Modifier::BOLD).fg(Color::Cyan),
            ));
            last = Some(r.section);
        }
        let mark = if i == cursor { "▶ " } else { "  " };
        let mut style = if r.disabled.is_some() {
            Style::default().fg(Color::DarkGray)
        } else {
            Style::default()
        };
        if i == cursor {
            style = style.add_modifier(Modifier::REVERSED);
        }
        let more = if r.disabled.is_none() && !panel_of(app, r).is_empty() {
            " ›"
        } else {
            ""
        };
        let text = format!("{mark}{} — {}{more}", r.title, r.subtitle);
        lines.push((text, r.key.as_deref().map(overlay::cap), style));
    }
    if shown.is_empty() && panel.is_none() {
        let q = st.query.trim();
        lines.push((
            if q.is_empty() {
                "Nothing to show yet".into()
            } else {
                format!("No direct match for {q}")
            },
            None,
            Style::default().fg(Color::DarkGray),
        ));
    }
    let w = main.width.saturating_sub(4).min(72);
    let h = (lines.len() as u16 + 4).min(main.height);
    if w < 20 || h < 5 {
        return;
    }
    let r = Rect::new(main.x + (main.width - w) / 2, main.y + 1, w, h);
    buf.set_style(r, Style::default().bg(Color::Indexed(235)));
    for x in r.left()..r.right() {
        for y in r.top()..r.bottom() {
            buf[(x, y)].set_symbol(" ");
        }
    }
    let inner = (w - 2) as usize;
    let fit = |s: &str, n: usize| s.chars().take(n).collect::<String>();
    buf.set_string(
        r.x + 1,
        r.y,
        fit(&format!("Search or jump to: {}▏", st.query), inner),
        Style::default().add_modifier(Modifier::BOLD),
    );
    // Keep the selected row in view when the list is taller than the popup.
    let room = (h - 3) as usize;
    let at = lines.iter().position(|(t, _, _)| t.starts_with("▶ ")).unwrap_or(0);
    let top = at.saturating_sub(room.saturating_sub(1));
    for (n, (text, key, style)) in lines.iter().skip(top).take(room).enumerate() {
        let y = r.y + 2 + n as u16;
        let kw = key.as_ref().map_or(0, |k| k.chars().count() + 1);
        buf.set_string(r.x + 1, y, fit(text, inner.saturating_sub(kw)), *style);
        if let Some(k) = key {
            buf.set_string(
                r.right() - 1 - k.chars().count() as u16,
                y,
                k,
                Style::default().fg(Color::Yellow),
            );
        }
    }
    buf.set_string(
        r.x + 1,
        r.bottom() - 1,
        fit(
            if panel.is_some() {
                "↑↓ move · Enter go · ← or Esc back"
            } else {
                "↑↓ move · Enter go · → more · Esc close · > actions · @ machines"
            },
            inner,
        ),
        Style::default().fg(Color::DarkGray),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(section: Section, title: &str, suggested: bool) -> Row {
        Row {
            id: None,
            section,
            title: title.into(),
            subtitle: String::new(),
            key: None,
            disabled: None,
            go: Go::Pick(Pick::AllKeys),
            suggested,
            boost: 0,
        }
    }

    #[test]
    fn fuzzy_prefers_word_starts_and_takes_initials() {
        let a = fuzzy("arr", "Arrange terminals").unwrap();
        let b = fuzzy("ter", "Arrange terminals").unwrap();
        assert!(a > b, "{a} {b}");
        assert!(fuzzy("sbs", "Split side by side").is_some());
        // Letters scattered inside words do not match.
        assert!(fuzzy("ren", "Arrange terminals").is_none());
        assert_eq!(fuzzy("", "anything"), Some(0));
        assert!(fuzzy("ARR", "arrange").is_some());
    }

    #[test]
    fn empty_query_shows_needs_and_suggested_actions_only() {
        let all = vec![
            row(Section::Places, "mac", false),
            row(Section::Actions, "Close pane", false),
            row(Section::Actions, "Arrange terminals", true),
            row(Section::Needs, "Approve rm", false),
            row(Section::Sessions, "tests", false),
        ];
        let got: Vec<String> = rank(all, "").into_iter().map(|r| r.title).collect();
        assert_eq!(got, vec!["Approve rm", "Arrange terminals"]);
    }

    #[test]
    fn typing_filters_every_section_in_order_five_each() {
        let mut all: Vec<Row> = (0..8)
            .map(|i| row(Section::Places, &format!("build {i}"), false))
            .collect();
        all.push(row(Section::Sessions, "build fix", false));
        all.push(row(Section::Actions, "New tab", false));
        let got = rank(all, "build");
        assert_eq!(got[0].section, Section::Sessions);
        assert_eq!(
            got.iter().filter(|r| r.section == Section::Places).count(),
            MAX_PER_SECTION
        );
        assert!(got.iter().all(|r| r.section != Section::Actions));
    }

    #[test]
    fn prefixes_narrow_to_one_section() {
        let all = vec![
            row(Section::Places, "new box", false),
            row(Section::Actions, "New tab", false),
        ];
        let a = rank(all.clone(), ">new");
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].title, "New tab");
        let p = rank(all, "@");
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].section, Section::Places);
    }

    #[test]
    fn a_disabled_row_ranks_below_an_equal_enabled_one() {
        let mut off = row(Section::Actions, "Zoom pane", false);
        off.disabled = Some("only one pane");
        let on = row(Section::Actions, "Zoom panel", false);
        let got = rank(vec![off, on], "zoom");
        assert_eq!(got[0].title, "Zoom panel");
    }

    use super::super::app::App;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use serde_json::json;

    fn app() -> App {
        let mut app = App::new(true, (100, 24));
        app.on_machines(&[
            json!({"id":"m1","name":"box","online":true}),
            json!({"id":"m2","name":"lap","online":false}),
        ]);
        app.select_initial(None);
        app.on_text(
            "m1",
            &json!({"t":"welcome","sub":"u1","mid":"m1","scope":"control","machine":{"online":true}}),
        );
        let agent = json!({"kind":"claude","status":"blocked","since":"2026-10-02T10:00:00Z","source":"reported"});
        let pane = |id: &str, tab: &str, agent: serde_json::Value, label: &str| json!({"id":id,"tabId":tab,"cwd":"/","command":"sh","title":"sh","cols":80,"rows":24,"agent":agent,"label":label});
        app.on_text("m1", &json!({
            "t": "topology", "rev": 1,
            "workspaces": [{"id":"w1","name":"billing","cwd":"/","position":0}],
            "tabs": [
                {"id":"w1:t1","workspaceId":"w1","name":"main","position":0,"layout":{"dir":"h","ratio":0.5,"a":{"pane":"w1:p1"},"b":{"pane":"w1:p2"}}},
                {"id":"w1:t2","workspaceId":"w1","name":"logs","position":1,"layout":{"pane":"w1:p3"}}
            ],
            "panes": [
                pane("w1:p1", "w1:t1", serde_json::Value::Null, "shell"),
                pane("w1:p2", "w1:t1", serde_json::Value::Null, "notes"),
                pane("w1:p3", "w1:t2", agent, "flaky tests"),
            ]
        }));
        app
    }

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    fn typed(app: &mut App, s: &str) {
        for c in s.chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
    }

    fn screen(app: &App) -> String {
        let mut t = Terminal::new(TestBackend::new(app.size.0, app.size.1)).unwrap();
        t.draw(|f| super::super::view::draw(app, f)).unwrap();
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

    #[test]
    fn prefix_k_opens_it_and_escape_closes_it() {
        let mut a = app();
        a.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        a.on_key(key(KeyCode::Char('k')));
        assert_eq!(a.mode, Mode::Command(State::default()));
        let s = screen(&a);
        assert!(s.contains("Search or jump to:"), "{s}");
        assert!(s.contains("Arrange terminals"), "{s}");
        a.on_key(key(KeyCode::Esc));
        assert_eq!(a.mode, Mode::Normal);
    }

    #[test]
    fn typing_a_session_name_and_enter_goes_to_its_pane() {
        let mut a = app();
        a.command_open();
        typed(&mut a, "flaky");
        let rows = match &a.mode {
            Mode::Command(st) => a.command_rows(st),
            m => panic!("{m:?}"),
        };
        assert_eq!(rows[0].title, "flaky tests");
        assert_eq!(rows[0].section, Section::Sessions);
        assert!(
            rows[0].subtitle.starts_with("needs you · claude · billing · box"),
            "{}",
            rows[0].subtitle
        );
        assert!(screen(&a).contains("Sessions"));
        a.on_key(key(KeyCode::Enter));
        assert_eq!(a.mode, Mode::Normal);
        assert_eq!(a.focus.as_deref(), Some("w1:p3"));
        assert_eq!(a.tab_id().as_deref(), Some("w1:t2"));
    }

    #[test]
    fn machines_and_projects_are_listed_and_an_action_runs_its_overlay_row() {
        let mut a = app();
        a.command_open();
        typed(&mut a, "@");
        let titles: Vec<String> = match &a.mode {
            Mode::Command(st) => a.command_rows(st).into_iter().map(|r| r.title).collect(),
            m => panic!("{m:?}"),
        };
        assert!(
            titles.contains(&"box".to_string())
                && titles.contains(&"lap".to_string())
                && titles.contains(&"billing".to_string()),
            "{titles:?}"
        );
        a.on_key(key(KeyCode::Esc));
        a.command_open();
        typed(&mut a, ">workspaces");
        a.on_key(key(KeyCode::Enter));
        assert_eq!(a.mode, Mode::Sidebar);
    }

    #[test]
    fn a_rebound_key_opens_it_and_k_is_free_again() {
        let mut a = app();
        a.keymap = super::super::keymap::Keymap::from_json(&json!({"version":1,"bindings":{"open_palette":"g"}}));
        a.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        a.on_key(key(KeyCode::Char('g')));
        assert!(matches!(a.mode, Mode::Command(_)), "{:?}", a.mode);
    }

    #[test]
    fn a_greyed_row_says_why_instead_of_running() {
        let mut a = app();
        a.command_open();
        typed(&mut a, "focus up");
        let r = match &a.mode {
            Mode::Command(st) => a.command_rows(st)[0].clone(),
            m => panic!("{m:?}"),
        };
        assert_eq!(r.title, "Focus up");
        assert!(r.disabled.is_some());
        a.on_key(key(KeyCode::Enter));
        assert!(
            a.note.as_deref().is_some_and(|n| n.starts_with("Focus up: ")),
            "{:?}",
            a.note
        );
    }

    fn place(id: &str, href: &str) -> RecentPlace {
        RecentPlace {
            id: id.into(),
            title: id.into(),
            subtitle: None,
            href: href.into(),
        }
    }

    fn st(a: &App) -> State {
        match &a.mode {
            Mode::Command(st) => st.clone(),
            m => panic!("{m:?}"),
        }
    }

    #[test]
    fn recent_is_capped_deduped_and_newest_first() {
        let mut l = vec![];
        for i in 0..30 {
            push_recent(&mut l, place(&format!("machine:m{i}"), "/machines/x"));
        }
        assert_eq!(l.len(), RECENT_MAX);
        assert_eq!(l[0].id, "machine:m29");
        push_recent(&mut l, place("machine:m15", "/machines/x"));
        assert_eq!(l[0].id, "machine:m15");
        assert_eq!(l.iter().filter(|p| p.id == "machine:m15").count(), 1);
        assert_eq!(l.len(), RECENT_MAX);
        let v = json!([
            {"id":"a","title":"A","href":"/a"},
            {"id":"a","title":"A again","href":"/a"},
            {"id":"b","title":"B","href":"https://elsewhere.example/"},
            {"id":"c","title":"C","href":"//elsewhere.example/"},
            {"id":"d","href":"/d"},
            3
        ]);
        assert_eq!(
            parse_recent(&v),
            vec![place("a", "/a")]
                .into_iter()
                .map(|mut p| {
                    p.title = "A".into();
                    p
                })
                .collect::<Vec<_>>()
        );
        assert!(parse_recent(&json!({"id":"a"})).is_empty());
        let many: Vec<Value> = (0..25)
            .map(|i| json!({"id":format!("x{i}"),"title":"t","href":"/x"}))
            .collect();
        assert_eq!(parse_recent(&Value::Array(many)).len(), RECENT_MAX);
    }

    #[test]
    fn recent_lists_the_accounts_places_here_before_typing_only() {
        let mut a = app();
        a.recent = vec![
            place("session:m1:w1:p3", "/machines/m1?pane=w1%3Ap3"),
            place("page:home", "/"),
            place("machine:m2", "/machines/m2"),
            place("machine:gone", "/machines/gone"),
        ];
        let got: Vec<(Section, String)> = rank(rows(&a), "")
            .into_iter()
            .filter(|r| r.section == Section::Recent)
            .map(|r| (r.section, r.title))
            .collect();
        assert_eq!(
            got,
            vec![
                (Section::Recent, "flaky tests".to_string()),
                (Section::Recent, "lap".to_string())
            ]
        );
        a.command_open();
        assert!(a.recent_fetch);
        assert!(screen(&a).contains("Recent"), "{}", screen(&a));
        // Once you type, Recent is not a section; its places rank higher where they are.
        assert!(rank(rows(&a), "la").iter().all(|r| r.section != Section::Recent));
        // Recent comes after Needs you and before the actions.
        let order: Vec<Section> = rank(rows(&a), "").into_iter().map(|r| r.section).collect();
        let mut sorted = order.clone();
        sorted.sort();
        assert_eq!(order, sorted);
    }

    #[test]
    fn choosing_a_place_adds_it_to_recent_and_sends_it_to_the_account() {
        let mut a = app();
        a.command_open();
        typed(&mut a, "@billing");
        a.on_key(key(KeyCode::Enter));
        assert_eq!(a.recent[0].id, "project:m1:w1");
        assert_eq!(a.recent[0].href, "/machines/m1?ws=w1");
        assert_eq!(a.recent_out.len(), 1);
        assert_eq!(a.recent_out[0]["id"], "project:m1:w1");
        assert_eq!(a.recent_out[0]["title"], "billing");
        // An action is redone, not revisited: not remembered.
        a.command_open();
        typed(&mut a, ">workspaces");
        a.on_key(key(KeyCode::Enter));
        assert_eq!(a.recent.len(), 1);
        assert_eq!(a.recent_out.len(), 1);
    }

    #[test]
    fn the_accounts_list_from_web_replaces_the_local_one() {
        let mut a = app();
        a.recent = vec![place("machine:m1", "/machines/m1")];
        super::super::apply(
            &mut a,
            super::super::net::NetEvent::Recent(Ok(
                json!({"recent":[{"id":"machine:m2","title":"lap","href":"/machines/m2"}]}),
            )),
        );
        assert_eq!(
            a.recent.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(),
            vec!["machine:m2"]
        );
        // Offline: what this machine remembers stays.
        super::super::apply(&mut a, super::super::net::NetEvent::Recent(Err("offline".into())));
        assert_eq!(a.recent.len(), 1);
    }

    #[test]
    fn right_opens_a_machines_actions_and_left_or_escape_goes_back() {
        let mut a = app();
        a.web_url = "https://app.example".into();
        a.command_open();
        typed(&mut a, "@box");
        let rows = a.command_rows(&st(&a));
        assert_eq!(rows[0].title, "box");
        a.on_key(key(KeyCode::Right));
        assert_eq!(st(&a).panel, Some(Panel { row: 0, cursor: 0 }));
        let s = screen(&a);
        assert!(
            s.contains("Actions for box") && s.contains("Machine settings (browser)") && s.contains("Files"),
            "{s}"
        );
        assert!(a.command_line().contains("back"));
        a.on_key(key(KeyCode::Left));
        assert_eq!(st(&a).panel, None);
        assert_eq!(st(&a).cursor, 0);
        assert_eq!(st(&a).query, "@box");
        a.on_key(key(KeyCode::Right));
        a.on_key(key(KeyCode::Esc));
        assert!(
            matches!(a.mode, Mode::Command(State { panel: None, .. })),
            "{:?}",
            a.mode
        );
        // Back to results is an entry too.
        a.on_key(key(KeyCode::Right));
        a.on_key(key(KeyCode::Up));
        a.on_key(key(KeyCode::Enter));
        assert!(
            matches!(a.mode, Mode::Command(State { panel: None, .. })),
            "{:?}",
            a.mode
        );
        // Events opens the web page for it.
        a.on_key(key(KeyCode::Right));
        for _ in 0..3 {
            a.on_key(key(KeyCode::Down));
        }
        a.on_key(key(KeyCode::Enter));
        assert_eq!(a.mode, Mode::Normal);
        assert_eq!(a.open_urls, vec!["https://app.example/machines/m1/events".to_string()]);
    }

    #[test]
    fn a_sessions_actions_open_it_or_its_changes_and_actions_have_no_panel() {
        let mut a = app();
        a.web_url = "https://app.example/".into();
        a.command_open();
        typed(&mut a, "flaky");
        a.on_key(key(KeyCode::Right));
        let titles: Vec<&str> = panel_of(&a, &a.command_rows(&st(&a))[0])
            .iter()
            .map(|e| e.title)
            .collect();
        assert_eq!(
            titles,
            vec!["Open", "Changes (browser)", "Replay (browser)", "Back to results"]
        );
        a.on_key(key(KeyCode::Down));
        a.on_key(key(KeyCode::Enter));
        assert_eq!(
            a.open_urls,
            vec!["https://app.example/machines/m1?pane=w1%3Ap3&sheet=changes".to_string()]
        );
        a.command_open();
        typed(&mut a, "flaky");
        a.on_key(key(KeyCode::Right));
        a.on_key(key(KeyCode::Enter));
        assert_eq!(a.mode, Mode::Normal);
        assert_eq!(a.focus.as_deref(), Some("w1:p3"));
        assert_eq!(a.recent[0].id, "session:m1:w1:p3");
        // An action row has no panel: → does nothing.
        a.command_open();
        typed(&mut a, ">new tab");
        a.on_key(key(KeyCode::Right));
        assert_eq!(st(&a).panel, None);
    }
}
