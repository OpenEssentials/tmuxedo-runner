//! Geometry and drawing: sidebar | tab bar over split panes | status line.
//! Pane contents come from each pane's vt100 screen, cell by cell (colors
//! and attributes); a pane whose PTY is smaller than its rectangle is
//! letterboxed, a larger one is cropped.

use super::app::{App, Item, Link, Mode};
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::KeyCode;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Widget, Wrap};
use ratatui::Frame;
use tmuxedo_protocol::frames::{AgentStatus, Layout, SplitDir};

pub struct Areas {
    pub sidebar: Rect,
    pub tabbar: Rect,
    pub main: Rect,
    pub status: Rect,
}

pub const SIDEBAR_MAX: u16 = 24;

pub fn areas((w, h): (u16, u16)) -> Areas {
    let body_h = h.saturating_sub(1);
    let sw = if w >= 60 { SIDEBAR_MAX.min(w / 3) } else { 0 };
    let mx = if sw > 0 { sw + 1 } else { 0 };
    let mw = w.saturating_sub(mx);
    Areas {
        sidebar: Rect::new(0, 0, sw, body_h),
        tabbar: Rect::new(mx, 0, mw, body_h.min(1)),
        main: Rect::new(mx, 1.min(body_h), mw, body_h.saturating_sub(1)),
        status: Rect::new(0, body_h, w, h.min(1)),
    }
}

/// Lay a split tree out in `area`: one-cell separators between siblings.
pub fn split_rects(l: &Layout, area: Rect, out: &mut Vec<(String, Rect)>, seps: &mut Vec<Rect>) {
    match l {
        Layout::Pane { pane } => out.push((pane.clone(), area)),
        Layout::Split { dir, ratio, a, b } => {
            let len = if *dir == SplitDir::H { area.width } else { area.height };
            let avail = len.saturating_sub(1);
            let la = if avail < 2 {
                avail
            } else {
                ((avail as f64) * ratio).round().clamp(1.0, (avail - 1) as f64) as u16
            };
            let lb = avail - la;
            let (ra, sep, rb) = if *dir == SplitDir::H {
                (
                    Rect::new(area.x, area.y, la, area.height),
                    Rect::new(area.x + la, area.y, len.min(1), area.height),
                    Rect::new(area.x + la + 1, area.y, lb, area.height),
                )
            } else {
                (
                    Rect::new(area.x, area.y, area.width, la),
                    Rect::new(area.x, area.y + la, area.width, len.min(1)),
                    Rect::new(area.x, area.y + la + 1, area.width, lb),
                )
            };
            seps.push(sep);
            split_rects(a, ra, out, seps);
            split_rects(b, rb, out, seps);
        }
    }
}

/// The pane next to `from` in direction `dir` (nearest, then most overlap).
pub fn neighbor(rects: &[(String, Rect)], from: &str, dir: KeyCode) -> Option<String> {
    let me = rects.iter().find(|(p, _)| p == from)?.1;
    let overlap = |a0: u16, a1: u16, b0: u16, b1: u16| a1.min(b1).saturating_sub(a0.max(b0));
    rects
        .iter()
        .filter(|(p, _)| p != from)
        .filter_map(|(p, r)| {
            let (dist, ov) = match dir {
                KeyCode::Left if r.right() <= me.x => (me.x - r.right(), overlap(me.y, me.bottom(), r.y, r.bottom())),
                KeyCode::Right if r.x >= me.right() => (r.x - me.right(), overlap(me.y, me.bottom(), r.y, r.bottom())),
                KeyCode::Up if r.bottom() <= me.y => (me.y - r.bottom(), overlap(me.x, me.right(), r.x, r.right())),
                KeyCode::Down if r.y >= me.bottom() => (r.y - me.bottom(), overlap(me.x, me.right(), r.x, r.right())),
                _ => return None,
            };
            (ov > 0).then_some((dist, std::cmp::Reverse(ov), p.clone()))
        })
        .min()
        .map(|(_, _, p)| p)
}

pub fn glyph(s: Option<AgentStatus>) -> (&'static str, Color) {
    match s {
        Some(AgentStatus::Blocked) => ("!", Color::Red),
        Some(AgentStatus::Working) => ("●", Color::Yellow),
        Some(AgentStatus::Done) => ("✓", Color::Green),
        Some(AgentStatus::Idle) => ("○", Color::Gray),
        Some(AgentStatus::Unknown) => ("?", Color::DarkGray),
        None => (" ", Color::Reset),
    }
}

pub fn status_word(s: AgentStatus) -> &'static str {
    match s {
        AgentStatus::Blocked => "needs you",
        AgentStatus::Working => "working",
        AgentStatus::Done => "done",
        AgentStatus::Idle => "idle",
        AgentStatus::Unknown => "unknown",
    }
}

/// Tab bar entries: (tab id, x0, x1, label).
pub fn tab_spans(app: &App) -> Vec<(String, u16, u16, String)> {
    let a = areas(app.size).tabbar;
    let (Some(t), Some(ws)) = (app.topo(), app.ws.as_ref()) else {
        return vec![];
    };
    let mut x = a.x;
    let mut out = vec![];
    for (i, tab) in t.tabs_of(ws).iter().enumerate() {
        let roll = super::app::rollup(
            t.panes
                .iter()
                .filter(|p| p.tab_id == tab.id)
                .map(|p| p.agent.as_ref().map(|a| a.status)),
        );
        let mark = if roll.is_some() { glyph(roll).0 } else { "" };
        let label = format!(" {}:{}{} ", i + 1, tab.name, mark);
        let w = unicode_width(&label);
        out.push((tab.id.clone(), x, x + w, label));
        x += w;
    }
    out
}

fn unicode_width(s: &str) -> u16 {
    Line::from(s).width() as u16
}

pub fn draw(app: &App, f: &mut Frame) {
    let a = areas(app.size);
    let buf = f.buffer_mut();
    draw_sidebar(app, a.sidebar, buf);
    if a.sidebar.width > 0 {
        let sep = Rect::new(a.sidebar.right(), 0, 1, a.sidebar.height);
        buf.set_style(sep, Style::default().fg(Color::DarkGray));
        for y in sep.top()..sep.bottom() {
            buf[(sep.x, y)].set_symbol("│");
        }
    }
    draw_tabbar(app, a.tabbar, buf);
    let cursor = match &app.chat {
        // The chat screen takes the whole body (sidebar and tabs included): a
        // transcript needs the width more than a list it can return to.
        Some(c) => super::chat::draw(c, Rect::new(0, 0, app.size.0, a.status.y), buf, app.chat_clock())
            .map(|(x, y)| Position::new(x, y)),
        None => draw_panes(app, a.main, buf),
    };
    draw_status(app, a.status, buf);
    // The prefix overlay (queue 3j) and `prefix ?`'s full list, over the panes.
    if app.mode == Mode::Keys {
        super::overlay::draw(&app.keymap, &app.overlay_ctx(), true, a.main, buf);
        return;
    }
    if app.overlay_shown(std::time::Instant::now()) {
        super::overlay::draw(&app.keymap, &app.overlay_ctx(), false, a.main, buf);
        return;
    }
    if let Mode::Arrange(sel) = &app.mode {
        super::arrange_ui::draw(app, *sel, a.main, buf);
        return;
    }
    if let Mode::Needs(cursor) = &app.mode {
        draw_needs(app, *cursor, a.main, buf);
        return;
    }
    if let (Mode::AgentSettings(top), Some(v)) = (&app.mode, &app.agent_settings) {
        draw_agent_settings(v, *top, a.main, buf);
        return;
    }
    if let Mode::Command(st) = &app.mode {
        super::palette::draw(app, st, a.main, buf);
        return;
    }
    if let (Mode::Search, Some(v)) = (&app.mode, &app.search) {
        super::search::draw(v, a.main, buf);
        return;
    }
    if let (Mode::Files, Some(v)) = (&app.mode, &app.files) {
        draw_files(v, a.main, buf);
        return;
    }
    if let Some((pane, _, agent)) = app.active_prompt().filter(|_| app.chat.is_none()) {
        draw_prompt(app, &pane, agent, a.main, buf);
    } else if let (Some(c), Mode::Normal) = (cursor, &app.mode) {
        f.set_cursor_position(c);
    }
}

/// The Needs you list (Phase 13), over the panes: one line per item, the selected one marked.
fn draw_needs(app: &App, cursor: usize, main: Rect, buf: &mut Buffer) {
    let items = app.needs_items();
    let w = main.width.saturating_sub(4).max(10);
    let h = ((items.len() as u16) + 3).min(main.height);
    if main.width < 14 || h < 3 {
        return;
    }
    let r = Rect::new(main.x + 2, main.y + 1, w, h);
    buf.set_style(r, Style::default().bg(Color::Indexed(235)));
    for x in r.left()..r.right() {
        for y in r.top()..r.bottom() {
            buf[(x, y)].set_symbol(" ");
        }
    }
    buf.set_string(r.x + 1, r.y, "Needs you", Style::default().add_modifier(Modifier::BOLD));
    for (i, it) in items.iter().enumerate() {
        let y = r.y + 1 + i as u16;
        if y >= r.bottom() - 1 {
            break;
        }
        let mark = if i == cursor.min(items.len().saturating_sub(1)) {
            "▶ "
        } else {
            "  "
        };
        let style = if it.risk == "high" {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default()
        };
        let text: String = format!("{mark}{}", super::needs::line(app, it))
            .chars()
            .take(w.saturating_sub(2) as usize)
            .collect();
        buf.set_string(r.x + 1, y, text, style);
    }
    buf.set_string(
        r.x + 1,
        r.bottom() - 1,
        "Esc closes",
        Style::default().fg(Color::DarkGray),
    );
}

/// `prefix S` (super::agent_settings): what each agent gets on this machine, read only, over the panes.
fn draw_agent_settings(v: &super::agent_settings::View, top: u16, main: Rect, buf: &mut Buffer) {
    use super::agent_settings::{rows, Tone};
    let rows = rows(v);
    let w = main.width.saturating_sub(4).max(10);
    let h = ((rows.len() as u16) + 3).min(main.height);
    if main.width < 14 || h < 3 {
        return;
    }
    let r = Rect::new(main.x + 2, main.y + (main.height - h).min(1), w, h);
    buf.set_style(r, Style::default().bg(Color::Indexed(235)));
    for x in r.left()..r.right() {
        for y in r.top()..r.bottom() {
            buf[(x, y)].set_symbol(" ");
        }
    }
    let title: String = format!("Agent settings on {} (read only)", v.machine)
        .chars()
        .filter(|c| !c.is_control())
        .take(w.saturating_sub(2) as usize)
        .collect();
    buf.set_string(r.x + 1, r.y, title, Style::default().add_modifier(Modifier::BOLD));
    let top = (top as usize).min(rows.len().saturating_sub(1));
    for (i, (tone, text)) in rows.iter().skip(top).enumerate() {
        let y = r.y + 1 + i as u16;
        if y >= r.bottom() - 1 {
            break;
        }
        let style = match tone {
            Tone::Heading => Style::default().add_modifier(Modifier::BOLD),
            Tone::Plain => Style::default(),
            Tone::Danger => Style::default().fg(Color::Red),
            Tone::Muted => Style::default().fg(Color::DarkGray),
        };
        let text: String = text.chars().take(w.saturating_sub(2) as usize).collect();
        buf.set_string(r.x + 1, y, text, style);
    }
    buf.set_string(
        r.x + 1,
        r.bottom() - 1,
        "e edit on the web · r reload · ↑↓ scroll · Esc closes",
        Style::default().fg(Color::DarkGray),
    );
}

/// `prefix F` (super::browse): the machine's files, read only, over the panes.
fn draw_files(v: &super::browse::View, main: Rect, buf: &mut Buffer) {
    use super::browse::Tone;
    let w = main.width.saturating_sub(4).max(10);
    let h = main.height.saturating_sub(1);
    if main.width < 14 || h < 4 {
        return;
    }
    let r = Rect::new(main.x + 2, main.y + 1.min(main.height - h), w, h);
    buf.set_style(r, Style::default().bg(Color::Indexed(235)));
    for x in r.left()..r.right() {
        for y in r.top()..r.bottom() {
            buf[(x, y)].set_symbol(" ");
        }
    }
    let inner = w.saturating_sub(2) as usize;
    let title: String = v.title().chars().take(inner).collect();
    buf.set_string(r.x + 1, r.y, title, Style::default().add_modifier(Modifier::BOLD));
    let rows = v.rows(inner);
    let room = h.saturating_sub(2) as usize;
    // A list keeps the selected row in sight; a file's rows already start at its first line shown.
    let skip = if matches!(v.place, super::browse::Place::File { .. }) {
        0
    } else {
        v.cursor.saturating_sub(room.saturating_sub(1))
    };
    for (i, (tone, text)) in rows.iter().skip(skip).take(room).enumerate() {
        let style = match tone {
            Tone::Selected => Style::default().add_modifier(Modifier::REVERSED),
            Tone::Plain => Style::default(),
            Tone::Danger => Style::default().fg(Color::Red),
            Tone::Muted => Style::default().fg(Color::DarkGray),
        };
        let text: String = text.chars().take(inner).collect();
        buf.set_string(r.x + 1, r.y + 1 + i as u16, text, style);
    }
    let foot: String = v.hint().chars().take(inner).collect();
    buf.set_string(r.x + 1, r.bottom() - 1, foot, Style::default().fg(Color::DarkGray));
}

fn draw_sidebar(app: &App, area: Rect, buf: &mut Buffer) {
    if area.width == 0 {
        return;
    }
    for (row, item) in app.sidebar_items().iter().enumerate() {
        if row as u16 >= area.height {
            break;
        }
        let y = area.y + row as u16;
        let (line, selected) = match item {
            Item::Machine(i) => {
                let m = &app.machines[*i];
                let (g, c) = glyph(m.rollup());
                let mut spans = vec![
                    Span::styled(g, Style::default().fg(c)),
                    Span::raw(" "),
                    Span::styled(m.name.clone(), Style::default().add_modifier(Modifier::BOLD)),
                ];
                match &m.link {
                    Link::Stopped(_) => spans.push(Span::styled(" (revoked)", Style::default().fg(Color::Red))),
                    _ if !m.online => spans.push(Span::styled(" (offline)", Style::default().fg(Color::DarkGray))),
                    _ => {}
                }
                if org_disabled(m.auth.as_ref()) {
                    spans.push(Span::styled(" ✕ claude off", Style::default().fg(Color::Red)));
                } else if let Some((badge, c)) = auth_badge(m.auth.as_ref().map(|a| a.0.as_str())) {
                    spans.push(Span::styled(format!(" {badge}"), Style::default().fg(c)));
                }
                // Kept awake while agents work (super::wake).
                if let Some(b) = super::wake::badge(m) {
                    spans.push(Span::styled(format!(" {b}"), Style::default().fg(Color::Yellow)));
                }
                // Needs you (Phase 13): how many items wait; prefix ! lists them.
                if let Some(n) = super::needs::badge(m) {
                    spans.push(Span::styled(
                        format!(" {n}"),
                        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                    ));
                }
                for (text, tone) in super::plugins::badges(m, None) {
                    spans.push(Span::styled(
                        format!(" [{text}]"),
                        Style::default().fg(tone_color(&tone)),
                    ));
                }
                (Line::from(spans), *i == app.sel && app.ws.is_none())
            }
            Item::Workspace(i, ws) => {
                let m = &app.machines[*i];
                let t = m.topo.as_ref();
                let name = t
                    .and_then(|t| t.workspaces.iter().find(|w| &w.id == ws))
                    .map(|w| w.name.clone())
                    .unwrap_or_default();
                let roll = super::app::rollup(
                    t.into_iter()
                        .flat_map(|t| t.panes_of_ws(ws).map(|p| p.agent.as_ref().map(|a| a.status))),
                );
                let (g, c) = glyph(roll);
                (
                    Line::from(vec![
                        Span::raw("  "),
                        Span::styled(g, Style::default().fg(c)),
                        Span::raw(format!(" {name}")),
                    ]),
                    *i == app.sel && app.ws.as_ref() == Some(ws),
                )
            }
            Item::Agent(i, pane) => {
                let p = app.machines[*i].topo.as_ref().and_then(|t| t.pane(pane));
                let a = p.and_then(|p| p.agent.as_ref());
                let (g, c) = glyph(a.map(|a| a.status));
                let name = p
                    .and_then(|p| p.label.as_deref())
                    .map(|l| format!(" {l} ·"))
                    .unwrap_or_default();
                let text = a
                    .map(|a| format!("{name} {} {}", a.kind, status_word(a.status)))
                    .unwrap_or_default();
                let mut spans = vec![Span::raw("    "), Span::styled(g, Style::default().fg(c))];
                // Someone else can see this session (Phase 27): a mark that fits the narrow
                // sidebar; the status line spells it out for the focused pane.
                if app.shares.is_shared(&app.machines[*i].id, pane) {
                    spans.push(Span::styled("⇄", Style::default().fg(Color::Cyan)));
                }
                spans.push(Span::raw(text));
                for (b, tone) in super::plugins::badges(&app.machines[*i], Some(pane)) {
                    spans.push(Span::styled(format!(" [{b}]"), Style::default().fg(tone_color(&tone))));
                }
                (Line::from(spans), *i == app.sel && app.focus.as_ref() == Some(pane))
            }
            Item::SharedHeader => (
                Line::from(Span::styled(
                    "Shared with me",
                    Style::default().add_modifier(Modifier::BOLD),
                )),
                false,
            ),
            Item::SharedWithMe(n) => {
                let s = &app.shares.with_me[*n];
                (
                    Line::from(vec![
                        Span::styled("  ↗ ", Style::default().fg(Color::Cyan)),
                        Span::raw(super::shares::row_text(s, super::shares::now_ms())),
                    ]),
                    false,
                )
            }
            Item::AwayHeader => (
                Line::from(Span::styled(
                    "While you were away",
                    Style::default().add_modifier(Modifier::BOLD),
                )),
                false,
            ),
            Item::Away(n) => {
                let c = &app.away.cards[*n];
                let tone = match c.group.as_str() {
                    "needs_you" => Color::Magenta,
                    "finished" => Color::Green,
                    _ => Color::Red,
                };
                (
                    Line::from(vec![
                        Span::raw("  "),
                        Span::styled(super::away::glyph(&c.group), Style::default().fg(tone)),
                        Span::raw(format!(" {}", super::away::row_text(c))),
                    ]),
                    false,
                )
            }
        };
        let row_area = Rect::new(area.x, y, area.width, 1);
        buf.set_line(area.x, y, &line, area.width);
        if app.mode == Mode::Sidebar && row == app.sidebar_cursor {
            buf.set_style(row_area, Style::default().add_modifier(Modifier::REVERSED));
        } else if selected {
            buf.set_style(row_area, Style::default().bg(Color::Indexed(236)));
        }
    }
}

fn draw_tabbar(app: &App, area: Rect, buf: &mut Buffer) {
    if area.height == 0 {
        return;
    }
    buf.set_style(area, Style::default().bg(Color::Indexed(235)));
    let cur = app.tab_id();
    for (id, x0, _, label) in tab_spans(app) {
        if x0 >= area.right() {
            break;
        }
        let st = if Some(&id) == cur.as_ref() {
            Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
        } else {
            Style::default()
        };
        buf.set_stringn(x0, area.y, &label, (area.right() - x0) as usize, st);
    }
    if app.zoomed_pane().is_some() {
        let z = " [zoom] ";
        let x = area.right().saturating_sub(z.len() as u16);
        buf.set_string(x, area.y, z, Style::default().fg(Color::Yellow));
    }
}

/// Draws the panes; returns the focused pane's cursor position, if shown.
fn draw_panes(app: &App, area: Rect, buf: &mut Buffer) -> Option<Position> {
    let Some(tab) = app.tab() else {
        let msg = match app.machine() {
            None => "no machines: connect one with tmuxedo-runner enrol".to_string(),
            Some(m) if matches!(m.link, Link::Stopped(_)) => "machine removed".to_string(),
            Some(m) if !m.online => super::wake::offline_text(m),
            Some(_) => "connecting…".to_string(),
        };
        buf.set_stringn(
            area.x + 1,
            area.y + 1.min(area.height),
            &msg,
            area.width as usize,
            Style::default().fg(Color::DarkGray),
        );
        return None;
    };
    let mut rects = vec![];
    let mut seps = vec![];
    match app.zoomed_pane() {
        Some(z) => rects.push((z.clone(), area)),
        None => split_rects(&tab.layout, area, &mut rects, &mut seps),
    }
    let focus_rect = rects
        .iter()
        .find(|(p, _)| Some(p) == app.focus.as_ref())
        .map(|(_, r)| *r);
    for s in &seps {
        for y in s.top()..s.bottom() {
            for x in s.left()..s.right() {
                let near = focus_rect.is_some_and(|f| {
                    let grown = Rect::new(f.x.saturating_sub(1), f.y.saturating_sub(1), f.width + 2, f.height + 2);
                    grown.contains(Position::new(x, y))
                });
                let vertical = s.width == 1 && s.height > 1;
                buf[(x, y)]
                    .set_symbol(if vertical { "│" } else { "─" })
                    .set_style(Style::default().fg(if near { Color::Green } else { Color::DarkGray }));
            }
        }
    }
    let mut cursor = None;
    for (pane, r) in &rects {
        let Some(t) = app.term(pane) else { continue };
        let sel = app.selection.as_ref().filter(|s| &s.pane == pane);
        let screen = t.emu.screen();
        let (rows, cols) = screen.size();
        for y in 0..r.height {
            for x in 0..r.width {
                let cell = &mut buf[(r.x + x, r.y + y)];
                if y >= rows || x >= cols {
                    cell.reset();
                    cell.set_symbol(" ").set_bg(Color::Indexed(234));
                    continue;
                }
                let Some(c) = screen.cell(y, x) else { continue };
                cell.reset();
                if c.is_wide_continuation() {
                    continue;
                }
                let s = c.contents();
                cell.set_symbol(if s.is_empty() { " " } else { s });
                let mut st = Style::default().fg(color(c.fgcolor())).bg(color(c.bgcolor()));
                let mut m = Modifier::empty();
                if c.bold() {
                    m |= Modifier::BOLD;
                }
                if c.dim() {
                    m |= Modifier::DIM;
                }
                if c.italic() {
                    m |= Modifier::ITALIC;
                }
                if c.underline() {
                    m |= Modifier::UNDERLINED;
                }
                // Selected cells are drawn inverted (inverse text shows normal).
                if c.inverse() != sel.is_some_and(|s| s.contains(y, x)) {
                    m |= Modifier::REVERSED;
                }
                st = st.add_modifier(m);
                cell.set_style(st);
            }
        }
        if Some(pane) == app.focus.as_ref() && !screen.hide_cursor() {
            let (cy, cx) = screen.cursor_position();
            if cy < r.height && cx < r.width && cy < rows && cx < cols {
                cursor = Some(Position::new(r.x + cx, r.y + cy));
            }
        }
    }
    cursor
}

fn color(c: vt100::Color) -> Color {
    match c {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(i) => Color::Indexed(i),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

fn draw_status(app: &App, area: Rect, buf: &mut Buffer) {
    if area.height == 0 {
        return;
    }
    buf.set_style(area, Style::default().bg(Color::Indexed(236)));
    let left = match &app.mode {
        Mode::Prefix if app.overlay_shown(std::time::Instant::now()) => {
            format!("{}: press an action key (Esc cancels)", app.keymap.prefix_label())
        }
        Mode::Prefix => format!("{}: {}", app.keymap.prefix_label(), hints(app, false)),
        Mode::Keys => "all keys: press one to run it, Esc closes".to_string(),
        Mode::Palette => app.palette_line(),
        Mode::Needs(_) => {
            "needs you: ↑↓ choose · a approve · d deny · r reply · 1-9 choice · enter go to pane · esc close"
                .to_string()
        }
        Mode::Arrange(_) => {
            let pick = match app.arrange_patterns().len().min(9) {
                0 => String::new(),
                n => format!("1-{n} pick · "),
            };
            format!("arrange: {pick}←→ move · enter apply · E even out · s save · esc close")
        }
        Mode::SaveFavourite(buf) => app.save_line(buf),
        Mode::Reply(item, buf) => app.reply_line(item, buf),
        Mode::AgentSettings(_) => {
            "agent settings (read only): e edit on the web · r reload · ↑↓ scroll · esc close".to_string()
        }
        Mode::Command(_) => app.command_line(),
        Mode::Search => app.search_line(),
        Mode::Files => app.files.as_ref().map_or("", |v| v.hint()).to_string(),
        Mode::Confirm(p) => format!("close pane {p}? (y/n)"),
        Mode::Scroll(p) => format!("scroll {p}: {} lines up  PgUp/PgDn ↑↓ g/G  q exit", app.scroll_pos(p)),
        Mode::Sidebar => "workspaces: ↑↓ move  enter select  esc back".to_string(),
        Mode::Rename(r) => format!(
            "rename {} (enter saves, empty = default, esc cancels): {}▏",
            r.what, r.buf
        ),
        Mode::Normal => match (&app.note, app.banner_line()) {
            (Some(n), _) => n.clone(),
            (None, Some(b)) => b,
            (None, None) => format!("{} {}", app.keymap.prefix_label(), hints(app, true)),
        },
    };
    let right = match app.machine() {
        Some(m) => {
            let state = match &m.link {
                Link::Up(true) => "control",
                Link::Up(false) => "watching",
                Link::Down => "connecting",
                Link::Stopped(_) => "stopped",
            };
            let auth = match &m.auth {
                Some((s, _)) => format!(" · claude {s}"),
                None => String::new(),
            };
            // The focused pane by its name when it has one.
            let focus = app.focus.clone().unwrap_or_default();
            let focus_id = focus.clone();
            let label = app.topo().and_then(|t| t.pane(&focus)).and_then(|p| p.label.clone());
            let shown = match label {
                Some(l) => format!("{l} ({focus})"),
                None => focus,
            };
            let line = app.plugin_status().map(|s| format!(" · {s}")).unwrap_or_default();
            let shared = if app.shares.is_shared(&m.id, &focus_id) {
                " · shared"
            } else {
                ""
            };
            format!(" {shown}{shared}{line} · {state}{auth} ")
        }
        None => String::new(),
    };
    let rw = unicode_width(&right);
    let st = if app.note.is_some() && app.mode == Mode::Normal {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default()
    };
    buf.set_stringn(
        area.x,
        area.y,
        format!(" {left}"),
        area.width.saturating_sub(rw) as usize,
        st,
    );
    if rw <= area.width {
        buf.set_string(area.right() - rw, area.y, &right, Style::default().fg(Color::Gray));
    }
}

/// The keymap's action keys, e.g. `c tab  % " split  arrows focus  …`.
fn hints(app: &App, compact: bool) -> String {
    use super::keymap::Action as A;
    let k = |a| app.keymap.key(a).to_string();
    let arrows = [A::FocusLeft, A::FocusRight, A::FocusUp, A::FocusDown].map(k);
    let focus = if arrows == ["left", "right", "up", "down"].map(String::from) {
        "arrows".to_string()
    } else {
        arrows.join("/")
    };
    let split = if compact {
        format!("{} {} split", k(A::SplitRight), k(A::SplitDown))
    } else {
        format!("{} split→  {} split↓", k(A::SplitRight), k(A::SplitDown))
    };
    let rename = if compact {
        String::new()
    } else {
        format!(
            "  {} {} {} rename tab/ws/pane",
            k(A::RenameTab),
            k(A::RenameWorkspace),
            k(A::RenamePane)
        )
    };
    format!(
        "{} tab  {split}  {focus} focus  {} zoom  {} close  w workspaces  [ scroll  {} detach{rename}",
        k(A::NewTab),
        k(A::Zoom),
        k(A::ClosePane),
        k(A::Detach)
    )
}

/// Sidebar badge for a machine's Claude login (none when fine or unknown).
/// A plugin surface's tone as a colour (the text says the rest).
fn tone_color(t: &str) -> Color {
    match t {
        "danger" => Color::Red,
        "warning" => Color::Yellow,
        "success" => Color::Green,
        _ => Color::Cyan,
    }
}

/// The account's organization turned Claude Code off (login state `failed`,
/// reason `org_disabled`, or Claude's own `oauth_org_not_allowed`): signing in
/// again with the same account does not help (web lib/claude-auth.ts).
pub fn org_disabled(auth: Option<&(String, String)>) -> bool {
    auth.is_some_and(|(s, r)| s == "failed" && (r == "org_disabled" || r == "oauth_org_not_allowed"))
}

/// The status line's words for [`org_disabled`]: the three ways out.
pub const ORG_DISABLED_LINE: &str = "Claude Code is off for this account's organization: ask its owner to turn it on,      or /logout then /login with another account, or add ANTHROPIC_API_KEY (or CLAUDE_CODE_OAUTH_TOKEN) in Settings → Secrets";

pub fn auth_badge(state: Option<&str>) -> Option<(&'static str, Color)> {
    match state? {
        "ok" => None,
        "missing" => Some(("⚠ login", Color::Yellow)),
        "expired" => Some(("⚠ expired", Color::Red)),
        "failed" => Some(("⚠ failed", Color::Red)),
        _ => None,
    }
}

fn prompt_summary(input: &serde_json::Value) -> String {
    for k in ["command", "file_path", "path", "url", "pattern"] {
        if let Some(s) = input[k].as_str() {
            return s.to_string();
        }
    }
    input.to_string()
}

fn draw_prompt(app: &App, pane: &str, agent: &tmuxedo_protocol::frames::Agent, main: Rect, buf: &mut Buffer) {
    let Some(p) = &agent.prompt else { return };
    let w = main.width.min(64);
    let h = main.height.min(6);
    if w < 10 || h < 3 {
        return;
    }
    let r = Rect::new(main.x + (main.width - w) / 2, main.bottom() - h, w, h);
    Clear.render(r, buf);
    let keys = if app.can_control() {
        "[a] allow   [d] deny   [esc] dismiss"
    } else {
        "watching: cannot answer"
    };
    let body = vec![
        Line::from(vec![
            Span::styled(format!("{}: ", p.tool), Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(prompt_summary(&p.input)),
        ]),
        Line::from(Span::styled(keys, Style::default().fg(Color::Yellow))),
    ];
    Paragraph::new(body)
        .wrap(Wrap { trim: true })
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Red))
                .title(format!(" {pane} · {} needs you ", agent.kind)),
        )
        .render(r, buf);
}
