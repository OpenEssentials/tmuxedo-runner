//! Arrange and favourites in the TUI (docs/design/pane-layouts.md §8.5).
//!
//! `prefix Space` opens a popup over the panes: the first-view suggestions
//! for this many terminals (`crate::arrange::offered`, the twin of
//! `offered()` in `packages/layouts`), drawn in box characters and numbered
//! 1–4, then the account's favourites. `1`–`4` apply a picture, arrows /
//! Space / Tab move, Enter applies or restores the selected one, `E` evens
//! out, `u` undoes the last arrangement made here, Esc closes. `prefix E`
//! evens out without the popup.
//!
//! Everything goes through `layout.arrange` (never `layout.apply`): nothing is
//! ever closed. A favourite's restore shows what it will do first (the
//! popup's bottom lines and the Enter label), then opens each missing
//! terminal by splitting the roomiest one (`pane.split`), starts an agent by
//! its kind where the slot holds one (`agent.start`), and arranges once with
//! the favourite's tree; panes with no slot move to a new tab "Extras".
//! Undo sends the answer's `undo.root` back with `expect_rev`.

use super::app::{App, Mode};
use super::favourites::{self as fav, Favourite, OpenPane};
use super::view;
use crate::arrange;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use serde_json::{json, Value};
use tmuxedo_protocol::frames::Layout;

/// A thumbnail's box, in cells, and the room each takes in a row.
pub const THUMB_W: u16 = 13;
pub const THUMB_H: u16 = 5;
const THUMB_STEP: u16 = THUMB_W + 2;
/// Favourites listed in the popup.
pub const FAV_SHOWN: usize = 6;

/// A layout request in flight, by what it was for.
#[derive(Debug, Clone, PartialEq)]
pub enum LayoutReq {
    /// A picture or Even out (its words).
    Arrange(String),
    Undo,
    /// A restore's `pane.split` for this slot (the slot, the terminal split).
    Split(usize, usize),
    /// A restore's `agent.start` (the agent's kind).
    Agent(String),
    /// A restore's last step.
    RestoreArrange,
}

/// What the last arrangement made here can be undone with.
#[derive(Debug, Clone, PartialEq)]
pub struct Undo {
    pub mid: String,
    pub tab: String,
    pub root: Value,
    pub rev: u64,
}

/// A restore in progress (one step in flight at a time).
#[derive(Debug, Clone, PartialEq)]
pub struct Restore {
    pub mid: String,
    pub tab: String,
    pub fav: Favourite,
    pub filled: Vec<Option<String>>,
    pub missing: Vec<usize>,
    /// The tab's terminals and their sizes (cols, rows), to pick which one to split.
    pub room: Vec<(String, u16, u16)>,
    pub opened: Vec<String>,
    pub agent_failed: Vec<String>,
    pub extras: usize,
    pub area: (u16, u16),
}

#[derive(Debug, Clone, Default)]
pub struct Layouts {
    /// The account's favourites; None until read.
    pub favourites: Option<Vec<Favourite>>,
    /// Why they could not be read.
    pub problem: Option<String>,
    /// The event loop reads `GET /api/me/layouts` (CLI token) when set, and hands the answer to `on_favourites`.
    pub fetch: bool,
    pub undo: Option<Undo>,
    pub restore: Option<Restore>,
}

/// One thing the popup offers.
#[derive(Debug, Clone, PartialEq)]
pub enum Pick {
    Pattern(&'static str),
    Favourite(usize),
}

/// The same tree, the same panes, ratios within a hair: the current layout already is this picture.
fn same(a: &Layout, b: &Layout) -> bool {
    match (a, b) {
        (Layout::Pane { pane: x }, Layout::Pane { pane: y }) => x == y,
        (
            Layout::Split {
                dir: d1,
                ratio: r1,
                a: a1,
                b: b1,
            },
            Layout::Split {
                dir: d2,
                ratio: r2,
                a: a2,
                b: b2,
            },
        ) => d1 == d2 && (r1 - r2).abs() < 0.02 && same(a1, a2) && same(b1, b2),
        _ => false,
    }
}

/// Fractions of the whole (x, y, w, h) as box-drawing lines `w` × `h` cells:
/// every edge drawn once, junctions joined (┬ ┼ ├ …).
pub fn box_art(boxes: &[[f64; 4]], w: u16, h: u16) -> Vec<String> {
    const L: u8 = 1;
    const R: u8 = 2;
    const U: u8 = 4;
    const D: u8 = 8;
    let (w, h) = (w.max(2) as usize, h.max(2) as usize);
    let mut g = vec![vec![0u8; w]; h];
    let sx = |f: f64| ((f * (w - 1) as f64).round() as usize).min(w - 1);
    let sy = |f: f64| ((f * (h - 1) as f64).round() as usize).min(h - 1);
    for [x, y, bw, bh] in boxes {
        let (x0, x1, y0, y1) = (sx(*x), sx(x + bw), sy(*y), sy(y + bh));
        for row in [y0, y1] {
            for c in x0..x1 {
                g[row][c] |= R;
                g[row][c + 1] |= L;
            }
        }
        for col in [x0, x1] {
            for r in y0..y1 {
                g[r][col] |= D;
                g[r + 1][col] |= U;
            }
        }
    }
    let glyph = |m: u8| match m {
        0 => ' ',
        x if x == L || x == R || x == L | R => '─',
        x if x == U || x == D || x == U | D => '│',
        x if x == R | D => '┌',
        x if x == L | D => '┐',
        x if x == R | U => '└',
        x if x == L | U => '┘',
        x if x == U | D | R => '├',
        x if x == U | D | L => '┤',
        x if x == L | R | D => '┬',
        x if x == L | R | U => '┴',
        _ => '┼',
    };
    g.iter().map(|row| row.iter().map(|m| glyph(*m)).collect()).collect()
}

/// A pattern's picture for `count` panes at `area`, from the code that builds the tree.
pub fn pattern_art(pattern: &'static str, count: usize, area: (u16, u16)) -> Vec<String> {
    let (_, rects, _) = arrange::thumbnail(pattern, count, area);
    let (aw, ah) = (area.0.max(1) as f64, area.1.max(1) as f64);
    let boxes: Vec<[f64; 4]> = rects
        .iter()
        .map(|r| [r.x as f64 / aw, r.y as f64 / ah, r.w as f64 / aw, r.h as f64 / ah])
        .collect();
    box_art(&boxes, THUMB_W, THUMB_H)
}

/// No control characters from a name reach the terminal.
fn shown(s: &str) -> String {
    s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect()
}

fn cut(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let mut t: String = s.chars().take(n.saturating_sub(1)).collect();
    t.push('…');
    t
}

impl App {
    /// The pane area in cells: what a picture is fitted to and what the runner checks sizes against.
    pub fn arrange_area(&self) -> (u16, u16) {
        let m = view::areas(self.size).main;
        (m.width.max(1), m.height.max(1))
    }

    /// The current tab's panes in reading order.
    fn tab_panes(&self) -> Vec<String> {
        self.tab()
            .map(|t| arrange::reading_order(&t.layout, self.arrange_area()))
            .unwrap_or_default()
    }

    /// The first-view pictures for this tab (none for one pane).
    pub fn arrange_patterns(&self) -> Vec<&'static str> {
        arrange::offered(self.tab_panes().len(), self.arrange_area()).0
    }

    /// Is the tab already arranged as `pattern`?
    pub fn arrange_current(&self, pattern: &'static str) -> bool {
        let (Some(t), Some(f)) = (self.tab(), self.focus.as_deref()) else {
            return false;
        };
        same(
            &arrange::plan(pattern, &t.layout, f, self.arrange_area()).layout,
            &t.layout,
        )
    }

    pub fn arrange_items(&self) -> Vec<Pick> {
        let mut v: Vec<Pick> = self.arrange_patterns().into_iter().map(Pick::Pattern).collect();
        let n = self.layouts.favourites.as_ref().map_or(0, |f| f.len().min(FAV_SHOWN));
        v.extend((0..n).map(Pick::Favourite));
        v
    }

    /// The tab's panes as the favourites planner sees them.
    pub fn open_panes(&self) -> Vec<OpenPane> {
        let Some(topo) = self.topo() else { return vec![] };
        let ws_cwd = self
            .ws
            .as_ref()
            .and_then(|w| topo.workspaces.iter().find(|x| &x.id == w))
            .map(|w| w.cwd.as_str());
        self.tab_panes()
            .into_iter()
            .map(|id| {
                let p = topo.pane(&id);
                OpenPane {
                    agent: p
                        .and_then(|p| p.agent.as_ref())
                        .map(|a| a.kind.clone())
                        .filter(|k| !k.is_empty()),
                    label: p
                        .and_then(|p| p.label.clone())
                        .filter(|l| fav::LABEL_MAX >= l.chars().count()),
                    cwd: fav::cwd_slot(p.map(|p| p.cwd.as_str()), ws_cwd),
                    id,
                }
            })
            .collect()
    }

    /// `prefix Space`.
    pub(super) fn arrange_open(&mut self) {
        if self.tab().is_none() {
            self.note = Some("no tab to arrange".into());
            return;
        }
        if self.layouts.favourites.is_none() {
            self.layouts.fetch = true;
        }
        self.mode = Mode::Arrange(0);
    }

    pub(super) fn arrange_key(&mut self, sel: usize, k: KeyEvent) {
        let items = self.arrange_items();
        let n = items.len();
        let pick = |i: usize| items.get(i).cloned();
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => self.mode = Mode::Normal,
            KeyCode::Char(c @ '1'..='4') => {
                if let Some(Pick::Pattern(p)) = pick(c as usize - '1' as usize) {
                    self.mode = Mode::Normal;
                    self.arrange_pattern(p);
                }
            }
            KeyCode::Char(' ') | KeyCode::Tab | KeyCode::Right | KeyCode::Down if n > 0 => {
                self.mode = Mode::Arrange((sel + 1) % n);
            }
            KeyCode::BackTab | KeyCode::Left | KeyCode::Up if n > 0 => {
                self.mode = Mode::Arrange((sel + n - 1) % n);
            }
            KeyCode::Enter => match pick(sel.min(n.saturating_sub(1))) {
                Some(Pick::Pattern(p)) => {
                    self.mode = Mode::Normal;
                    self.arrange_pattern(p);
                }
                Some(Pick::Favourite(i)) => {
                    self.mode = Mode::Normal;
                    self.restore_start(i);
                }
                None => {}
            },
            KeyCode::Char('E') | KeyCode::Char('e') => {
                self.mode = Mode::Normal;
                self.even_out();
            }
            KeyCode::Char('u') if self.layouts.undo.is_some() => {
                self.mode = Mode::Normal;
                self.arrange_undo();
            }
            _ => {}
        }
    }

    /// Why nothing may be arranged here now, in the web's words; None = it may.
    fn arrange_blocked(&self) -> Option<String> {
        if !self.machine().is_some_and(|m| m.online) {
            return Some(fav::problem("machine_offline", false).into());
        }
        if !self.can_control() {
            return Some(fav::problem("forbidden_scope", false).into());
        }
        if self.layouts.restore.is_some() {
            return Some("A favourite is still being restored.".into());
        }
        None
    }

    fn arrange_params(&self, tab: &str) -> Value {
        let (w, h) = self.arrange_area();
        let mut p = json!({ "tab_id": tab, "area": { "width": w, "height": h } });
        if let Some(rev) = self.topo().map(|t| t.rev).filter(|r| *r > 0) {
            p["expect_rev"] = json!(rev);
        }
        p
    }

    fn arrange_pattern(&mut self, pattern: &'static str) {
        if let Some(why) = self.arrange_blocked() {
            self.note = Some(why);
            return;
        }
        let Some(tab) = self.tab_id() else { return };
        let mut params = self.arrange_params(&tab);
        params["pattern"] = json!(pattern);
        if let Some(f) = self.focus.clone().filter(|f| self.tab_panes().contains(f)) {
            params["focus"] = json!(f);
        }
        self.request_layout(
            "layout.arrange",
            params,
            LayoutReq::Arrange(arrange::label(pattern).into()),
        );
    }

    /// Even out (`prefix E`, or `E` in the popup): keep the shape, share rows and columns equally.
    pub(super) fn even_out(&mut self) {
        if self.tab_panes().len() < 2 {
            self.note = Some("only one pane: nothing to even out".into());
            return;
        }
        self.arrange_pattern("even");
    }

    fn arrange_undo(&mut self) {
        let Some(u) = self.layouts.undo.clone() else { return };
        if let Some(why) = self.arrange_blocked() {
            self.note = Some(why);
            return;
        }
        if self.machine().map(|m| m.id.as_str()) != Some(u.mid.as_str()) {
            self.note = Some("that arrangement was on another machine".into());
            return;
        }
        self.layouts.undo = None;
        let params = json!({ "tab_id": u.tab, "root": u.root, "expect_rev": u.rev });
        self.request_layout("layout.arrange", params, LayoutReq::Undo);
    }

    /// Enter on a favourite: the popup already showed what happens; do it.
    fn restore_start(&mut self, i: usize) {
        if let Some(why) = self.arrange_blocked() {
            self.note = Some(why);
            return;
        }
        let Some(f) = self.layouts.favourites.as_ref().and_then(|l| l.get(i)).cloned() else {
            return;
        };
        let (Some(tab), Some(mid)) = (self.tab_id(), self.machine().map(|m| m.id.clone())) else {
            return;
        };
        let open = self.open_panes();
        let p = fav::preview(&f, &open);
        let topo = self.topo();
        // The tab's terminals in reading order: on a tie the first is split, as on the web.
        let room = open
            .iter()
            .map(|o| &o.id)
            .map(|id| {
                let (c, r) = topo.and_then(|t| t.pane(id)).map_or((80, 24), |p| (p.cols, p.rows));
                (id.clone(), if c > 0 { c } else { 80 }, if r > 0 { r } else { 24 })
            })
            .collect();
        self.layouts.restore = Some(Restore {
            mid,
            tab,
            filled: p.plan.assigned.clone(),
            missing: p.plan.missing.clone(),
            room,
            opened: vec![],
            agent_failed: vec![],
            extras: p.plan.extras.len(),
            area: self.arrange_area(),
            fav: f,
        });
        self.restore_step();
    }

    /// The next step of a restore: open a missing terminal, or arrange.
    fn restore_step(&mut self) {
        let Some(r) = self.layouts.restore.as_mut() else { return };
        if self.machines.get(self.sel).map(|m| m.id.as_str()) != Some(r.mid.as_str()) {
            let r = self.layouts.restore.take().unwrap();
            self.note = Some(restore_text(&r, Some("disconnected")));
            return;
        }
        if let Some(slot) = r.missing.first().copied() {
            let area = |i: usize| r.room[i].1 as u32 * r.room[i].2 as u32;
            let Some(i) = (0..r.room.len()).fold(None, |best: Option<usize>, i| match best {
                Some(b) if area(b) >= area(i) => Some(b),
                _ => Some(i),
            }) else {
                let r = self.layouts.restore.take().unwrap();
                self.note = Some(restore_text(&r, Some("not_found")));
                return;
            };
            let (target, w, h) = r.room[i].clone();
            let wide = w >= 2 * h;
            if wide {
                r.room[i].1 = (w / 2).max(1);
            } else {
                r.room[i].2 = (h / 2).max(1);
            }
            let params =
                json!({ "target_pane_id": target, "direction": if wide { "right" } else { "down" }, "focus": false });
            return self.request_layout("pane.split", params, LayoutReq::Split(slot, i));
        }
        let Some(tree) = fav::restore_layout(&r.fav.shape, &r.filled) else {
            let r = self.layouts.restore.take().unwrap();
            self.note = Some(restore_text(&r, Some("invalid")));
            return;
        };
        let params = json!({
            "tab_id": r.tab,
            "root": fav::wire(&tree),
            "extras": if r.extras > 0 { "new_tab" } else { "refuse" },
            "area": { "width": r.area.0, "height": r.area.1 },
        });
        self.request_layout("layout.arrange", params, LayoutReq::RestoreArrange);
    }

    /// The runner's answer to a layout request (`error`: its code).
    pub(super) fn on_layout_res(&mut self, req: LayoutReq, result: &Value, error: Option<&str>) {
        match req {
            LayoutReq::Arrange(what) => {
                self.note = Some(match error {
                    Some(code) => fav::problem(code, false).into(),
                    None => {
                        self.keep_undo(result);
                        format!("Arranged as {what} · {} Space u undoes", self.keymap.prefix_label())
                    }
                });
            }
            LayoutReq::Undo => {
                self.note = Some(match error {
                    Some(code) => fav::problem(code, true).into(),
                    None => "Layout put back.".into(),
                });
            }
            LayoutReq::Split(slot, from) => {
                let Some(r) = self.layouts.restore.as_mut() else { return };
                let id = result["pane"]["pane_id"].as_str().filter(|id| !id.is_empty());
                match (error, id) {
                    (None, Some(id)) if !r.filled.iter().flatten().any(|f| f == id) => {
                        let (w, h) = r.room.get(from).map_or((80, 24), |x| (x.1, x.2));
                        r.room.push((id.to_string(), w, h));
                        r.opened.push(id.to_string());
                        r.filled[slot] = Some(id.to_string());
                        r.missing.retain(|s| *s != slot);
                        let spec = r.fav.slots[slot].clone();
                        if let (true, Some(kind)) = (spec.holds == "agent", spec.agent) {
                            let n = r.opened.len();
                            let stamp = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map_or(0, |d| d.as_secs() % 0x10000);
                            let params = json!({ "pane_id": id, "kind": kind, "name": format!("{kind}-{stamp:x}{n}") });
                            return self.request_layout("agent.start", params, LayoutReq::Agent(kind));
                        }
                        self.restore_step();
                    }
                    _ => {
                        let r = self.layouts.restore.take().unwrap();
                        self.note = Some(restore_text(&r, Some(error.unwrap_or("failed"))));
                    }
                }
            }
            LayoutReq::Agent(kind) => {
                if let (Some(r), Some(_)) = (self.layouts.restore.as_mut(), error) {
                    r.agent_failed.push(kind);
                }
                self.restore_step();
            }
            LayoutReq::RestoreArrange => {
                let Some(r) = self.layouts.restore.take() else { return };
                if error.is_none() {
                    self.keep_undo(result);
                }
                self.note = Some(restore_text(&r, error));
            }
        }
        self.sync(false);
    }

    fn keep_undo(&mut self, result: &Value) {
        let u = &result["undo"];
        self.layouts.undo = match (self.machine().map(|m| m.id.clone()), self.tab_id(), u["rev"].as_u64()) {
            (Some(mid), Some(tab), Some(rev)) if u["root"].is_object() => Some(Undo {
                mid,
                tab,
                root: u["root"].clone(),
                rev,
            }),
            _ => None,
        };
    }

    /// `GET /api/me/layouts`, from the event loop.
    pub fn on_favourites(&mut self, r: Result<Value, String>) {
        match r {
            Ok(v) => {
                self.layouts.favourites = Some(fav::parse_list(&v));
                self.layouts.problem = None;
            }
            Err(e) => {
                self.layouts.favourites = Some(vec![]);
                self.layouts.problem = Some(e);
            }
        }
    }
}

/// What the status line says when a restore ends (`restoreText` on the web).
pub fn restore_text(r: &Restore, error: Option<&str>) -> String {
    let n = r.opened.len();
    let opened = if n > 0 {
        format!(
            "{} {} opened",
            fav::count(n, "terminal"),
            if n == 1 { "was" } else { "were" }
        )
    } else {
        String::new()
    };
    let name = shown(&r.fav.name);
    if let Some(code) = error {
        let head = if r.missing.is_empty()
            || matches!(
                code,
                "forbidden_scope" | "machine_offline" | "not_connected" | "disconnected"
            ) {
            fav::problem(code, false).to_string()
        } else {
            format!("A terminal for “{name}” could not be opened, so the layout was not changed.")
        };
        return if opened.is_empty() {
            head
        } else {
            format!("{head} {opened}; nothing was closed.")
        };
    }
    let mut parts = vec![format!("Arranged as “{name}”")];
    if !opened.is_empty() {
        parts.push(opened);
    }
    if r.extras > 0 {
        parts.push(format!("{} moved to “Extras”", fav::count(r.extras, "terminal")));
    }
    let mut text = parts.join(" · ");
    if !r.agent_failed.is_empty() {
        let mut kinds: Vec<String> = r.agent_failed.iter().map(|k| fav::agent_word(k)).collect();
        kinds.dedup();
        let one = r.agent_failed.len() == 1;
        text.push_str(&format!(
            ". {} could not be started; {} a shell.",
            kinds.join(", "),
            if one { "that terminal is" } else { "those terminals are" }
        ));
    }
    text
}

/// The popup, over the panes.
pub fn draw(app: &App, sel: usize, main: Rect, buf: &mut Buffer) {
    if main.width < 20 || main.height < 6 {
        return;
    }
    let count = app
        .tab()
        .map_or(0, |t| arrange::reading_order(&t.layout, app.arrange_area()).len());
    let area = app.arrange_area();
    let items = app.arrange_items();
    let sel = sel.min(items.len().saturating_sub(1));
    let patterns = app.arrange_patterns();
    let favs: Vec<&Favourite> = app.layouts.favourites.iter().flatten().take(FAV_SHOWN).collect();
    let all_favs = app.layouts.favourites.as_ref().map_or(0, Vec::len);
    let pw = main.width.saturating_sub(2).clamp(20, 4 * THUMB_STEP + 4);
    let inner = pw.saturating_sub(4);
    let per_row = (inner / THUMB_STEP).max(1) as usize;
    let selected = items.get(sel);
    let preview = match selected {
        Some(Pick::Favourite(i)) => favs.get(*i).map(|f| fav::preview(f, &app.open_panes())),
        _ => None,
    };
    // Lines inside the border, top to bottom.
    let mut lines: Vec<(String, Style)> = vec![];
    let dim = Style::default().fg(Color::DarkGray);
    let hl = Style::default().fg(Color::Black).bg(Color::Cyan);
    let fav_lines = 1 + favs.len().max(1) + usize::from(all_favs > favs.len());
    let tail = 2 + preview.as_ref().map_or(0, |p| p.lines.len().min(3));
    let thumb_rows = patterns.len().div_ceil(per_row);
    let with_pictures =
        !patterns.is_empty() && (2 + thumb_rows * (THUMB_H as usize + 1) + 1 + fav_lines + tail) as u16 <= main.height;
    let mut pics: Vec<(u16, u16, &'static str, bool)> = vec![];
    if count < 2 {
        lines.push(("One terminal: nothing to arrange yet.".into(), dim));
    } else if patterns.is_empty() {
        lines.push(("No layout fits this many terminals at this size.".into(), dim));
    } else if with_pictures {
        for (row, chunk) in patterns.chunks(per_row).enumerate() {
            let top = lines.len() as u16;
            for (col, p) in chunk.iter().enumerate() {
                pics.push((
                    col as u16 * THUMB_STEP,
                    top,
                    p,
                    items.get(sel) == Some(&Pick::Pattern(p)),
                ));
            }
            for _ in 0..THUMB_H {
                lines.push((String::new(), Style::default()));
            }
            let mut words = String::new();
            for (col, p) in chunk.iter().enumerate() {
                let i = row * per_row + col + 1;
                let mark = if app.arrange_current(p) { " ✓" } else { "" };
                let w = cut(&format!("{i} {}{mark}", arrange::label(p)), THUMB_STEP as usize - 1);
                words.push_str(&format!("{w:<width$}", width = THUMB_STEP as usize));
            }
            lines.push((words.trim_end().to_string(), Style::default()));
        }
    } else {
        for (i, p) in patterns.iter().enumerate() {
            let mark = if app.arrange_current(p) { " ✓" } else { "" };
            let on = items.get(sel) == Some(&Pick::Pattern(p));
            lines.push((
                format!("{} {} {}{mark}", if on { "▶" } else { " " }, i + 1, arrange::label(p)),
                if on { hl } else { Style::default() },
            ));
        }
    }
    let undo = if app.layouts.undo.is_some() { "   u Undo" } else { "" };
    if count >= 2 {
        lines.push((format!("E Even out sizes{undo}"), Style::default()));
    } else if !undo.is_empty() {
        lines.push((undo.trim_start().to_string(), Style::default()));
    }
    lines.push(("Favourites".into(), Style::default().add_modifier(Modifier::BOLD)));
    match (&app.layouts.favourites, &app.layouts.problem) {
        (None, _) => lines.push(("Reading your favourites…".into(), dim)),
        (Some(_), Some(_)) => lines.push(("Favourites could not be read. Try again later.".into(), dim)),
        (Some(l), None) if l.is_empty() => {
            lines.push(("None yet: save a layout from the Arrange panel on the web.".into(), dim))
        }
        _ => {
            for (i, f) in favs.iter().enumerate() {
                let on = items.get(sel) == Some(&Pick::Favourite(i));
                let what = contents(f);
                let text = format!(
                    "{} {}{}",
                    if on { "▶" } else { " " },
                    shown(&f.name),
                    if what.is_empty() {
                        String::new()
                    } else {
                        format!(" · {what}")
                    }
                );
                lines.push((text, if on { hl } else { Style::default() }));
            }
            if all_favs > favs.len() {
                lines.push((format!("  +{} more on the web", all_favs - favs.len()), dim));
            }
        }
    }
    let foot = match (selected, &preview) {
        (Some(Pick::Favourite(_)), Some(p)) => {
            for l in p.lines.iter().take(3) {
                lines.push((l.clone(), dim));
            }
            format!("Enter: {}   Esc cancels", p.button)
        }
        (Some(Pick::Pattern(p)), _) => format!("Enter: arrange as {}   1-4 pick   Esc cancels", arrange::label(p)),
        _ => "Esc closes".into(),
    };
    lines.push((foot, dim));
    // Sentences wrap at word boundaries (the pictures above are never wider than the popup).
    let lines: Vec<(String, Style)> = lines
        .into_iter()
        .flat_map(|(t, s)| wrap(&t, inner as usize).into_iter().map(move |l| (l, s)))
        .collect();

    let ph = (lines.len() as u16 + 2).min(main.height);
    let r = Rect::new(main.x + (main.width - pw) / 2, main.y, pw, ph);
    buf.set_style(r, Style::default().bg(Color::Indexed(235)).fg(Color::Reset));
    for y in r.top()..r.bottom() {
        for x in r.left()..r.right() {
            buf[(x, y)].set_symbol(" ");
        }
    }
    let frame = Style::default().fg(Color::DarkGray);
    for x in r.left()..r.right() {
        buf[(x, r.top())].set_symbol("─").set_style(frame);
        buf[(x, r.bottom() - 1)].set_symbol("─").set_style(frame);
    }
    for y in r.top()..r.bottom() {
        buf[(r.left(), y)].set_symbol("│").set_style(frame);
        buf[(r.right() - 1, y)].set_symbol("│").set_style(frame);
    }
    for (x, y, s) in [
        (r.left(), r.top(), "╭"),
        (r.right() - 1, r.top(), "╮"),
        (r.left(), r.bottom() - 1, "╰"),
        (r.right() - 1, r.bottom() - 1, "╯"),
    ] {
        buf[(x, y)].set_symbol(s);
    }
    let title = cut(&format!(" Arrange {} ", fav::count(count, "terminal")), inner as usize);
    buf.set_string(r.x + 2, r.y, title, Style::default().add_modifier(Modifier::BOLD));
    let (x0, y0) = (r.x + 2, r.y + 1);
    let room = ph.saturating_sub(2);
    for (i, (text, style)) in lines.iter().enumerate() {
        if i as u16 >= room {
            break;
        }
        buf.set_string(x0, y0 + i as u16, cut(text, inner as usize), *style);
    }
    for (dx, dy, p, on) in pics {
        if dy + THUMB_H > room {
            continue;
        }
        let style = if on {
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        for (j, l) in pattern_art(p, count, area).iter().enumerate() {
            buf.set_string(x0 + dx, y0 + dy + j as u16, l, style);
        }
    }
}

/// `text` in lines of at most `width` characters, broken between words (a word longer than a line is cut).
fn wrap(text: &str, width: usize) -> Vec<String> {
    if text.chars().count() <= width || width < 8 {
        return vec![text.to_string()];
    }
    let mut out = vec![];
    let mut line = String::new();
    for word in text.split(' ') {
        let n = line.chars().count();
        if n > 0 && n + 1 + word.chars().count() > width {
            out.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    out.push(line);
    out
}

/// "Claude, 2 shell": what a favourite holds, agents first.
pub fn contents(f: &Favourite) -> String {
    let mut tally: Vec<(String, usize)> = vec![];
    for s in &f.slots {
        let w = match s.holds.as_str() {
            "agent" => fav::agent_word(s.agent.as_deref().unwrap_or("")),
            "shell" => "shell".into(),
            _ => continue,
        };
        match tally.iter_mut().find(|(x, _)| *x == w) {
            Some(t) => t.1 += 1,
            None => tally.push((w, 1)),
        }
    }
    tally.sort_by_key(|(w, _)| w == "shell");
    tally
        .into_iter()
        .map(|(w, n)| if n > 1 { format!("{n} {w}") } else { w })
        .collect::<Vec<_>>()
        .join(", ")
}
