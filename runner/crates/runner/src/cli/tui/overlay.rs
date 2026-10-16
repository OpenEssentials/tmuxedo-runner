//! The prefix overlay (queue 3j; the web's is web/src/lib/prefix-overlay.ts):
//! after the prefix, a popup lists every prefix action from the user's
//! keybindings and the TUI's own prefix keys, grouped, each with its key,
//! greyed out with the reason when it does not apply right now ("only one
//! pane"). Pressing the key runs it; Esc cancels; the prefix again sends the
//! prefix to the pane; `?` opens every binding; `!` (when nothing is bound
//! to it) turns the overlay off for the account.
//!
//! The setting (`overlay` in GET /api/me/keybindings): `on` (default, shown
//! at once), `delay` (shown when no key follows within [`DELAY`]), `off`.

use super::keymap::{Action, Keymap};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Widget};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OverlayMode {
    #[default]
    On,
    Delay,
    Off,
}

/// "After a short delay" (the web's OVERLAY_DELAY_MS).
pub const DELAY: Duration = Duration::from_millis(700);

impl OverlayMode {
    /// The API's value; anything unknown (or absent: an older server) is `On`.
    pub fn parse(v: Option<&str>) -> OverlayMode {
        match v {
            Some("delay") => OverlayMode::Delay,
            Some("off") => OverlayMode::Off,
            _ => OverlayMode::On,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            OverlayMode::On => "on",
            OverlayMode::Delay => "delay",
            OverlayMode::Off => "off",
        }
    }
    /// Shown `elapsed` after the prefix?
    pub fn shown(self, elapsed: Duration) -> bool {
        match self {
            OverlayMode::On => true,
            OverlayMode::Delay => elapsed >= DELAY,
            OverlayMode::Off => false,
        }
    }
}

/// What the TUI knows right now, for the "does this apply" rules.
#[derive(Debug, Clone, Default)]
pub struct Ctx {
    pub control: bool,
    pub online: bool,
    pub panes: usize,
    pub focused: bool,
    pub tabs: usize,
    pub zoomed: bool,
    /// left, right, up, down: a pane to move focus to.
    pub neighbours: [bool; 4],
}

/// What an item does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pick {
    Action(Action),
    Scroll,
    Workspaces,
    NextTab,
    PrevTab,
    GoToTab,
    AllKeys,
    AgentSettings,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub key: String,
    pub label: String,
    pub disabled: Option<&'static str>,
    pub pick: Pick,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Group {
    pub title: &'static str,
    pub items: Vec<Item>,
}

const WATCHING: &str = "take control first";
const OFFLINE: &str = "machine offline";

fn reason(a: Action, c: &Ctx) -> Option<&'static str> {
    use Action as A;
    let mutates = matches!(
        a,
        A::NewTab | A::SplitRight | A::SplitDown | A::Zoom | A::ClosePane | A::Arrange | A::EvenOut
    );
    if mutates && !c.control {
        return Some(WATCHING);
    }
    let dir = match a {
        A::FocusLeft => Some((0, "no pane left")),
        A::FocusRight => Some((1, "no pane right")),
        A::FocusUp => Some((2, "no pane above")),
        A::FocusDown => Some((3, "no pane below")),
        _ => None,
    };
    if let Some((i, why)) = dir {
        if c.panes < 2 {
            return Some("only one pane");
        }
        if c.zoomed {
            return Some("zoomed");
        }
        return (!c.neighbours[i]).then_some(why);
    }
    match a {
        A::SplitRight | A::SplitDown | A::ClosePane => (!c.focused).then_some("no pane"),
        A::Zoom => (c.panes < 2).then_some("only one pane"),
        A::EvenOut => (c.panes < 2).then_some("only one pane"),
        A::Arrange => (!c.online).then_some(OFFLINE),
        A::RenameTab | A::RenameWorkspace | A::RenamePane => {
            if !c.online {
                Some(OFFLINE)
            } else if !c.control {
                Some(WATCHING)
            } else {
                None
            }
        }
        _ => None,
    }
}

fn label(a: Action, c: &Ctx) -> &'static str {
    use Action as A;
    match a {
        A::NewTab => "new tab",
        A::SplitRight => "split side by side",
        A::SplitDown => "split stacked",
        A::FocusLeft => "focus left",
        A::FocusRight => "focus right",
        A::FocusUp => "focus up",
        A::FocusDown => "focus down",
        A::Zoom if c.zoomed => "unzoom pane",
        A::Zoom => "zoom pane",
        A::ClosePane => "close pane",
        A::Detach => "leave (agents keep running)",
        A::RenameTab => "rename tab",
        A::RenameWorkspace => "rename workspace",
        A::RenamePane => "rename pane",
        A::Arrange => "arrange terminals",
        A::EvenOut => "even out sizes",
    }
}

/// The key `?` (all keys) and `!` (don't show again) use, unless an action has it.
pub fn free(km: &Keymap, key: &str) -> bool {
    km.prefix != key && !km.actions.iter().any(|(_, k)| k == key)
}

/// Every bound action once (the user's keys), the TUI's own prefix keys
/// when no action shadows them, and "all keys".
pub fn groups(km: &Keymap, c: &Ctx) -> Vec<Group> {
    use Action as A;
    let act = |a: Action| Item {
        key: km.key(a).to_string(),
        label: label(a, c).to_string(),
        disabled: reason(a, c),
        pick: Pick::Action(a),
    };
    let own = |key: &str, label: &str, pick: Pick, disabled: Option<&'static str>| {
        free(km, key).then(|| Item {
            key: key.to_string(),
            label: label.to_string(),
            disabled,
            pick,
        })
    };
    let one_tab = (c.tabs < 2).then_some("only one tab");
    let mut panes: Vec<Item> = [
        A::SplitRight,
        A::SplitDown,
        A::FocusLeft,
        A::FocusRight,
        A::FocusUp,
        A::FocusDown,
        A::ClosePane,
    ]
    .map(act)
    .into();
    panes.extend(own("[", "scroll back", Pick::Scroll, (!c.focused).then_some("no pane")));
    let mut tabs = vec![act(A::NewTab)];
    tabs.extend(own("n", "next tab", Pick::NextTab, one_tab));
    tabs.extend(own("p", "previous tab", Pick::PrevTab, one_tab));
    if (1..=9).all(|n| free(km, &n.to_string())) {
        tabs.push(Item {
            key: "1-9".into(),
            label: "go to tab".into(),
            disabled: (c.tabs == 0).then_some("no tab"),
            pick: Pick::GoToTab,
        });
    }
    tabs.push(act(A::Zoom));
    let names = [A::RenameTab, A::RenameWorkspace, A::RenamePane].map(act).into();
    let mut view = vec![];
    view.extend(own("w", "workspaces", Pick::Workspaces, None));
    view.extend(own("S", "agent settings", Pick::AgentSettings, None));
    view.push(act(A::Detach));
    view.extend(own("?", "all keys", Pick::AllKeys, None));
    vec![
        Group {
            title: "Panes",
            items: panes,
        },
        Group {
            title: "Tabs and zoom",
            items: tabs,
        },
        Group {
            title: "Names",
            items: names,
        },
        Group {
            title: "Layout",
            items: vec![act(A::Arrange), act(A::EvenOut)],
        },
        Group {
            title: "View",
            items: view,
        },
    ]
}

/// `left` → `←`, `ctrl+b` → `^b`, else the notation.
pub fn cap(key: &str) -> String {
    match key {
        "left" => "←".into(),
        "right" => "→".into(),
        "up" => "↑".into(),
        "down" => "↓".into(),
        k => k
            .strip_prefix("ctrl+")
            .filter(|c| c.chars().count() == 1)
            .map(|c| format!("^{c}"))
            .unwrap_or_else(|| k.to_string()),
    }
}

/// Draw the overlay (`sheet` = every binding after `?`) centred in `main`.
pub fn draw(km: &Keymap, c: &Ctx, sheet: bool, main: Rect, buf: &mut Buffer) {
    let gs = groups(km, c);
    let lines: Vec<Vec<Line>> = gs
        .iter()
        .map(|g| {
            let mut v = vec![Line::from(Span::styled(
                g.title,
                Style::default().add_modifier(Modifier::BOLD).fg(Color::Cyan),
            ))];
            for it in &g.items {
                let key = format!("{:>3} ", cap(&it.key));
                let (ks, ls) = if it.disabled.is_some() {
                    (
                        Style::default().fg(Color::DarkGray),
                        Style::default().fg(Color::DarkGray),
                    )
                } else {
                    (
                        Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                        Style::default(),
                    )
                };
                let mut spans = vec![Span::styled(key, ks), Span::styled(it.label.clone(), ls)];
                if let Some(r) = it.disabled {
                    spans.push(Span::styled(format!(" ({r})"), Style::default().fg(Color::DarkGray)));
                }
                v.push(Line::from(spans));
            }
            v
        })
        .collect();
    let widest = |g: &[Line]| g.iter().map(Line::width).max().unwrap_or(0) as u16 + 2;
    // As many columns (4, 2 or 1) as fit, each as wide as its widest line; a group never splits.
    let layout = |cols: usize| -> (Vec<Vec<Line>>, Vec<u16>) {
        let mut columns: Vec<Vec<Line>> = vec![vec![]; cols];
        for (i, g) in lines.iter().enumerate() {
            let col = &mut columns[i % cols];
            if !col.is_empty() {
                col.push(Line::raw(""));
            }
            col.extend(g.iter().cloned());
        }
        let widths = columns.iter().map(|c| widest(c)).collect();
        (columns, widths)
    };
    let (columns, widths) = [4, 2, 1]
        .into_iter()
        .map(layout)
        .find(|(_, w)| w.iter().sum::<u16>() + 2 <= main.width)
        .unwrap_or_else(|| layout(1));
    let rows = columns.iter().map(Vec::len).max().unwrap_or(0) as u16;
    let mut footer = format!("{p} again: send {p} to the pane · Esc cancel", p = km.prefix_label());
    if !sheet && free(km, "!") {
        footer.push_str(" · ! don't show this again");
    }
    let body_w = widths
        .iter()
        .sum::<u16>()
        .max(Line::from(footer.as_str()).width() as u16);
    let w = (body_w + 2).min(main.width);
    let h = (rows + 4).min(main.height);
    if w < 12 || h < 4 {
        return;
    }
    let r = Rect::new(main.x + (main.width - w) / 2, main.bottom() - h, w, h);
    Clear.render(r, buf);
    let title = if sheet {
        " all keys (Esc closes) ".to_string()
    } else {
        format!(" {} then… ", km.prefix_label())
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(title);
    let inner = block.inner(r);
    block.render(r, buf);
    let mut x = inner.x;
    for (col, cw) in columns.iter().zip(&widths) {
        for (yi, line) in col.iter().enumerate() {
            let y = inner.y + yi as u16;
            if y >= inner.bottom().saturating_sub(1) || x >= inner.right() {
                break;
            }
            buf.set_line(x, y, line, (*cw).min(inner.right() - x));
        }
        x += cw;
    }
    let fy = inner.bottom().saturating_sub(1);
    buf.set_stringn(
        inner.x,
        fy,
        &footer,
        inner.width as usize,
        Style::default().fg(Color::Gray),
    );
}
