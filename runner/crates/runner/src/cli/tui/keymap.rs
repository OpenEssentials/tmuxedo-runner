//! The user's keybindings (`GET /api/me/keybindings`, docs/protocol.md
//! "Client settings"; notation shared with web/src/lib/keybindings.ts):
//! a prefix, then one key per action. Unknown actions are ignored, a bad
//! key falls back to that action's default, a document from a newer
//! `version` (or no network) falls back to the defaults entirely.
//!
//! Notation: a printable character is itself (`c`, `%`, `"`, `C`; shift is
//! part of the character); named keys `left right up down enter tab space
//! escape backspace home end pageup pagedown delete insert f1..f12`;
//! modifiers before the key in the order `ctrl+`, `alt+`, `shift+` (shift
//! only with a named key); letters with ctrl/alt are lowercase.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::Value;

pub const VERSION: u64 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    NewTab,
    SplitRight,
    SplitDown,
    FocusLeft,
    FocusRight,
    FocusUp,
    FocusDown,
    Zoom,
    ClosePane,
    Detach,
    RenameTab,
    RenameWorkspace,
    RenamePane,
    Arrange,
    EvenOut,
    /// Move the focused terminal's divider to the next snap target (the web's `resize_snap_*`).
    ResizeSnapLeft,
    ResizeSnapRight,
    ResizeSnapUp,
    ResizeSnapDown,
    /// Move it one cell (`resize_cell_*`).
    ResizeCellLeft,
    ResizeCellRight,
    ResizeCellUp,
    ResizeCellDown,
    /// The command palette (`prefix k`; super::palette, the web's `open_palette`).
    OpenPalette,
}

pub const ACTIONS: [(Action, &str, &str); 24] = [
    (Action::NewTab, "new_tab", "c"),
    (Action::SplitRight, "split_right", "%"),
    (Action::SplitDown, "split_down", "\""),
    (Action::FocusLeft, "focus_left", "left"),
    (Action::FocusRight, "focus_right", "right"),
    (Action::FocusUp, "focus_up", "up"),
    (Action::FocusDown, "focus_down", "down"),
    (Action::Zoom, "zoom", "z"),
    (Action::ClosePane, "close_pane", "x"),
    (Action::Detach, "detach", "d"),
    (Action::RenameTab, "rename_tab", ","),
    (Action::RenameWorkspace, "rename_workspace", "$"),
    (Action::RenamePane, "rename_pane", "P"),
    (Action::Arrange, "arrange", "space"),
    (Action::EvenOut, "even_out", "E"),
    (Action::ResizeSnapLeft, "resize_snap_left", "alt+left"),
    (Action::ResizeSnapRight, "resize_snap_right", "alt+right"),
    (Action::ResizeSnapUp, "resize_snap_up", "alt+up"),
    (Action::ResizeSnapDown, "resize_snap_down", "alt+down"),
    (Action::ResizeCellLeft, "resize_cell_left", "ctrl+left"),
    (Action::ResizeCellRight, "resize_cell_right", "ctrl+right"),
    (Action::ResizeCellUp, "resize_cell_up", "ctrl+up"),
    (Action::ResizeCellDown, "resize_cell_down", "ctrl+down"),
    (Action::OpenPalette, "open_palette", "k"),
];

/// The resize actions (web/src/lib/keybindings.ts `RESIZE_ACTIONS`): in the
/// full key sheet, not the quick overlay.
pub const RESIZE_ACTIONS: [Action; 8] = [
    Action::ResizeSnapLeft,
    Action::ResizeSnapRight,
    Action::ResizeSnapUp,
    Action::ResizeSnapDown,
    Action::ResizeCellLeft,
    Action::ResizeCellRight,
    Action::ResizeCellUp,
    Action::ResizeCellDown,
];

/// Actions added after keybindings shipped (queue 3c, names). A document that
/// does not mention one (an older server, or a user who never changed it) may
/// already use its default key for another action; then it moves to the first
/// free key here instead of the whole keymap falling back to the defaults.
/// Same rule as web/src/lib/keybindings.ts `LATER_ACTIONS`.
pub const LATER_FALLBACKS: &[&str] = &["f2", "f3", "f4", "f5", "f6", "f7", "f8", "f9"];
/// Only the resize actions go on to these (the web's `MORE_FALLBACKS`).
fn more_fallbacks() -> Vec<String> {
    let mut v: Vec<String> = ["f10", "f11", "f12"].map(String::from).into();
    for m in ["alt", "ctrl"] {
        v.extend((1..=12).map(|i| format!("{m}+f{i}")));
    }
    v
}
const LATER_ACTIONS: [Action; 14] = [
    Action::RenameTab,
    Action::RenameWorkspace,
    Action::RenamePane,
    Action::Arrange,
    Action::EvenOut,
    Action::ResizeSnapLeft,
    Action::ResizeSnapRight,
    Action::ResizeSnapUp,
    Action::ResizeSnapDown,
    Action::ResizeCellLeft,
    Action::ResizeCellRight,
    Action::ResizeCellUp,
    Action::ResizeCellDown,
    Action::OpenPalette,
];

pub const DEFAULT_PREFIX: &str = "ctrl+b";

const NAMED: &[&str] = &[
    "left",
    "right",
    "up",
    "down",
    "enter",
    "tab",
    "space",
    "escape",
    "backspace",
    "home",
    "end",
    "pageup",
    "pagedown",
    "delete",
    "insert",
    "f1",
    "f2",
    "f3",
    "f4",
    "f5",
    "f6",
    "f7",
    "f8",
    "f9",
    "f10",
    "f11",
    "f12",
];

/// Parse and normalise a key string (None if it is not valid notation).
pub fn normalise(raw: &str) -> Option<String> {
    if raw.is_empty() || raw.chars().count() > 32 {
        return None;
    }
    let (mut ctrl, mut alt, mut shift) = (false, false, false);
    let mut rest = raw;
    loop {
        let lower = rest.to_ascii_lowercase();
        let (flag, len) = if lower.starts_with("ctrl+") && rest.len() > 5 {
            (&mut ctrl, 5)
        } else if lower.starts_with("alt+") && rest.len() > 4 {
            (&mut alt, 4)
        } else if lower.starts_with("shift+") && rest.len() > 6 {
            (&mut shift, 6)
        } else {
            break;
        };
        if *flag {
            return None;
        }
        *flag = true;
        rest = &rest[len..];
    }
    let key = if rest.chars().count() == 1 {
        let c = rest.chars().next()?;
        if c.is_whitespace() || c.is_control() || shift {
            return None;
        }
        if ctrl || alt {
            c.to_lowercase().to_string()
        } else {
            c.to_string()
        }
    } else {
        let k = rest.to_ascii_lowercase();
        if !NAMED.contains(&k.as_str()) {
            return None;
        }
        k
    };
    Some(format!(
        "{}{}{}{key}",
        if ctrl { "ctrl+" } else { "" },
        if alt { "alt+" } else { "" },
        if shift { "shift+" } else { "" }
    ))
}

/// A terminal key event in the notation (None for keys it cannot express).
pub fn notation(k: &KeyEvent) -> Option<String> {
    let m = k.modifiers;
    let (ctrl, alt) = (m.contains(KeyModifiers::CONTROL), m.contains(KeyModifiers::ALT));
    let named = match k.code {
        KeyCode::Left => "left",
        KeyCode::Right => "right",
        KeyCode::Up => "up",
        KeyCode::Down => "down",
        KeyCode::Enter => "enter",
        KeyCode::Tab | KeyCode::BackTab => "tab",
        KeyCode::Esc => "escape",
        KeyCode::Backspace => "backspace",
        KeyCode::Home => "home",
        KeyCode::End => "end",
        KeyCode::PageUp => "pageup",
        KeyCode::PageDown => "pagedown",
        KeyCode::Delete => "delete",
        KeyCode::Insert => "insert",
        KeyCode::Char(' ') => "space",
        KeyCode::F(n @ 1..=12) => return Some(format!("{}{}f{n}", mods(ctrl, alt), shift(m))),
        KeyCode::Char(c) => {
            let c = if ctrl || alt { c.to_lowercase().next()? } else { c };
            return Some(format!("{}{c}", mods(ctrl, alt)));
        }
        _ => return None,
    };
    let sh = m.contains(KeyModifiers::SHIFT) || k.code == KeyCode::BackTab;
    Some(format!(
        "{}{}{named}",
        mods(ctrl, alt),
        if sh && named != "space" { "shift+" } else { "" }
    ))
}

fn mods(ctrl: bool, alt: bool) -> String {
    format!("{}{}", if ctrl { "ctrl+" } else { "" }, if alt { "alt+" } else { "" })
}
fn shift(m: KeyModifiers) -> &'static str {
    if m.contains(KeyModifiers::SHIFT) {
        "shift+"
    } else {
        ""
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Keymap {
    pub prefix: String,
    pub actions: Vec<(Action, String)>,
    /// Where the bindings came from, for the status line.
    pub source: String,
    /// When the prefix overlay shows (`overlay` in the same document; queue 3j).
    pub overlay: super::overlay::OverlayMode,
}

impl Default for Keymap {
    fn default() -> Self {
        Keymap {
            prefix: DEFAULT_PREFIX.into(),
            actions: ACTIONS.iter().map(|(a, _, k)| (*a, k.to_string())).collect(),
            source: "defaults".into(),
            overlay: super::overlay::OverlayMode::default(),
        }
    }
}

impl Keymap {
    /// From the API document; anything unusable falls back to the default.
    pub fn from_json(v: &Value) -> Keymap {
        let mut km = Keymap {
            overlay: super::overlay::OverlayMode::parse(v["overlay"].as_str()),
            ..Keymap::default()
        };
        if v["version"].as_u64().is_none_or(|n| n > VERSION) {
            km.source = format!("defaults (keybindings version {} not understood)", v["version"]);
            return km;
        }
        if let Some(p) = v["prefix"].as_str().and_then(normalise) {
            if p.starts_with("ctrl+") || p.starts_with("alt+") {
                km.prefix = p;
            }
        }
        let mut given = vec![];
        for (a, name, _) in ACTIONS {
            if let Some(k) = v["bindings"][name].as_str().and_then(normalise) {
                if k != km.prefix && k != "escape" {
                    km.set(a, k);
                    given.push(a);
                }
            }
        }
        km.move_later_actions(&given);
        // Two actions on one key (the server refuses this; be safe): defaults.
        let mut keys: Vec<&String> = km.actions.iter().map(|(_, k)| k).collect();
        keys.sort();
        keys.dedup();
        if keys.len() != km.actions.len() {
            return Keymap {
                source: "defaults (conflicting keybindings)".into(),
                overlay: km.overlay,
                ..Keymap::default()
            };
        }
        km.source = if v["isDefault"] == true {
            "defaults".into()
        } else {
            "your keybindings".into()
        };
        km
    }

    /// A later action the document did not set whose default key another
    /// action (or the prefix) uses moves to the first free fallback key.
    fn move_later_actions(&mut self, given: &[Action]) {
        for a in LATER_ACTIONS {
            if given.contains(&a) {
                continue;
            }
            let k = self.key(a).to_string();
            let taken = k == self.prefix || self.actions.iter().any(|(x, key)| *x != a && *key == k);
            if !taken {
                continue;
            }
            let mut pool: Vec<String> = LATER_FALLBACKS.iter().map(|f| f.to_string()).collect();
            if RESIZE_ACTIONS.contains(&a) {
                pool.extend(more_fallbacks());
            }
            let free = pool
                .into_iter()
                .find(|f| *f != self.prefix && !self.actions.iter().any(|(_, key)| key == f));
            if let Some(f) = free {
                self.set(a, f);
            }
        }
    }

    fn set(&mut self, a: Action, k: String) {
        if let Some(e) = self.actions.iter_mut().find(|(x, _)| *x == a) {
            e.1 = k;
        }
    }

    pub fn is_prefix(&self, k: &KeyEvent) -> bool {
        notation(k).as_deref() == Some(self.prefix.as_str())
    }

    pub fn action(&self, k: &KeyEvent) -> Option<Action> {
        let n = notation(k)?;
        self.actions.iter().find(|(_, key)| *key == n).map(|(a, _)| *a)
    }

    pub fn key(&self, a: Action) -> &str {
        self.actions
            .iter()
            .find(|(x, _)| *x == a)
            .map(|(_, k)| k.as_str())
            .unwrap_or("")
    }

    /// `^b` for `ctrl+b`, `M-a` for `alt+a`, else the notation.
    pub fn prefix_label(&self) -> String {
        if let Some(c) = self.prefix.strip_prefix("ctrl+").filter(|c| c.chars().count() == 1) {
            return format!("^{c}");
        }
        if let Some(c) = self.prefix.strip_prefix("alt+").filter(|c| c.chars().count() == 1) {
            return format!("M-{c}");
        }
        self.prefix.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn k(code: KeyCode, m: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, m)
    }

    #[test]
    fn notation_matches_the_web_spelling() {
        assert_eq!(normalise("Ctrl+B").as_deref(), Some("ctrl+b"));
        assert_eq!(normalise("ctrl+shift+Left").as_deref(), Some("ctrl+shift+left"));
        assert_eq!(normalise("%").as_deref(), Some("%"));
        assert_eq!(normalise("C").as_deref(), Some("C"));
        assert_eq!(normalise("shift+c"), None, "shift is part of the character");
        assert_eq!(normalise("ctrl+ctrl+b"), None);
        assert_eq!(normalise("hyper+x"), None);
        assert_eq!(
            notation(&k(KeyCode::Char('b'), KeyModifiers::CONTROL)).as_deref(),
            Some("ctrl+b")
        );
        assert_eq!(
            notation(&k(KeyCode::Char('%'), KeyModifiers::SHIFT)).as_deref(),
            Some("%")
        );
        assert_eq!(
            notation(&k(KeyCode::Left, KeyModifiers::SHIFT)).as_deref(),
            Some("shift+left")
        );
        assert_eq!(
            notation(&k(KeyCode::Char('A'), KeyModifiers::ALT)).as_deref(),
            Some("alt+a")
        );
        assert_eq!(notation(&k(KeyCode::F(5), KeyModifiers::NONE)).as_deref(), Some("f5"));
    }

    #[test]
    fn document_is_applied_with_per_key_fallbacks() {
        let km = Keymap::from_json(&json!({
            "version": 1, "prefix": "ctrl+a", "isDefault": false,
            "bindings": {"split_right": "|", "split_down": "-", "zoom": "bogus key", "detach": "ctrl+a", "future_action": "q"}
        }));
        assert_eq!(km.prefix, "ctrl+a");
        assert_eq!(km.prefix_label(), "^a");
        assert_eq!(km.key(Action::SplitRight), "|");
        assert_eq!(km.key(Action::SplitDown), "-");
        assert_eq!(km.key(Action::Zoom), "z", "bad notation → default");
        assert_eq!(km.key(Action::Detach), "d", "the prefix cannot be an action");
        assert!(km.is_prefix(&k(KeyCode::Char('a'), KeyModifiers::CONTROL)));
        assert!(!km.is_prefix(&k(KeyCode::Char('b'), KeyModifiers::CONTROL)));
        assert_eq!(
            km.action(&k(KeyCode::Char('|'), KeyModifiers::SHIFT)),
            Some(Action::SplitRight)
        );
        assert_eq!(km.source, "your keybindings");
    }

    #[test]
    fn newer_versions_bare_prefixes_and_conflicts_fall_back() {
        assert_eq!(
            Keymap::from_json(&json!({"version": 2, "prefix": "ctrl+a"})).prefix,
            "ctrl+b"
        );
        assert_eq!(
            Keymap::from_json(&json!({"version": 1, "prefix": "a"})).prefix,
            "ctrl+b"
        );
        let clash = Keymap::from_json(&json!({"version": 1, "bindings": {"zoom": "c"}}));
        assert_eq!(clash.key(Action::RenameTab), ",");
        assert_eq!(clash.key(Action::Zoom), "z");
        assert!(clash.source.contains("conflicting"));
        assert_eq!(
            Keymap::from_json(&Value::Null),
            Keymap {
                source: "defaults (keybindings version null not understood)".into(),
                ..Keymap::default()
            }
        );
    }

    #[test]
    fn rename_keys_default_like_tmux_and_never_break_an_older_document() {
        let km = Keymap::default();
        assert_eq!(km.key(Action::RenameTab), ",");
        assert_eq!(km.key(Action::RenameWorkspace), "$");
        assert_eq!(km.key(Action::RenamePane), "P");
        assert_eq!(
            km.action(&k(KeyCode::Char(','), KeyModifiers::NONE)),
            Some(Action::RenameTab)
        );
        assert_eq!(
            km.action(&k(KeyCode::Char('$'), KeyModifiers::SHIFT)),
            Some(Action::RenameWorkspace)
        );
        // An older server's document (no rename actions) whose user put zoom on `,`
        // and the prefix on `$`'s neighbour: zoom keeps `,`, rename_tab moves to f2,
        // and the rest of the user's keys survive (no fall back to the defaults).
        let km = Keymap::from_json(&json!({
            "version": 1, "prefix": "ctrl+a", "isDefault": false,
            "bindings": {"zoom": ",", "close_pane": "$", "split_right": "|"}
        }));
        assert_eq!(km.source, "your keybindings");
        assert_eq!(km.key(Action::Zoom), ",");
        assert_eq!(km.key(Action::ClosePane), "$");
        assert_eq!(km.key(Action::SplitRight), "|");
        assert_eq!(km.key(Action::RenameTab), "f2");
        assert_eq!(km.key(Action::RenameWorkspace), "f3");
        assert_eq!(km.key(Action::RenamePane), "P");
        // A document that sets them is taken as is.
        let km = Keymap::from_json(&json!({"version": 1, "bindings": {"rename_tab": "R", "rename_pane": "f9"}}));
        assert_eq!((km.key(Action::RenameTab), km.key(Action::RenamePane)), ("R", "f9"));
    }

    #[test]
    fn resize_keys_are_known_so_no_fallback_lands_on_one() {
        let km = Keymap::default();
        assert_eq!(km.key(Action::ResizeSnapLeft), "alt+left");
        assert_eq!(km.key(Action::ResizeCellDown), "ctrl+down");
        assert_eq!(
            km.action(&k(KeyCode::Right, KeyModifiers::ALT)),
            Some(Action::ResizeSnapRight)
        );
        assert_eq!(
            km.action(&k(KeyCode::Up, KeyModifiers::CONTROL)),
            Some(Action::ResizeCellUp)
        );
        // The web moved a resize action onto f2 and the arrange key onto f3; an older
        // document leaves out even_out, whose default `E` the user gave to zoom. Even out
        // must not be handed f2 or f3: it goes to f4.
        let km = Keymap::from_json(&json!({
            "version": 1, "isDefault": false,
            "bindings": {"zoom": "E", "resize_snap_left": "f2", "arrange": "f3"}
        }));
        assert_eq!(km.source, "your keybindings");
        assert_eq!(km.key(Action::ResizeSnapLeft), "f2");
        assert_eq!(km.key(Action::EvenOut), "f4");
        // A resize action whose default is taken (and f2..f9 too) goes on to f10 and beyond.
        let mut b = serde_json::Map::new();
        b.insert("zoom".into(), json!("alt+left"));
        let names = [
            "new_tab",
            "split_right",
            "split_down",
            "close_pane",
            "detach",
            "rename_tab",
            "rename_workspace",
            "rename_pane",
        ];
        for (i, name) in names.iter().enumerate() {
            b.insert(name.to_string(), json!(format!("f{}", i + 2)));
        }
        let km = Keymap::from_json(&json!({"version": 1, "bindings": b}));
        assert_eq!(km.source, "your keybindings");
        assert_eq!(km.key(Action::Zoom), "alt+left");
        assert_eq!(km.key(Action::ResizeSnapLeft), "f10");
    }
}
