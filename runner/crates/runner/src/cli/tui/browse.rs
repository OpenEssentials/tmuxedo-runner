//! `prefix F`: the selected machine's files, read only (Phase 15b; the
//! design's file browser for the terminal). Through the same REST twin of
//! the runner's confined `fs.*` methods the web and the app use
//! (`GET /api/v1/machines/{id}/fs/{roots,list,read}`, the CLI token), drawn
//! over the panes:
//!
//!   Files on box › site › docs (read only)
//!   ▶ notes/
//!     readme.md                                    2.1 KB
//!     2 items hidden: they may hold secrets
//!
//! ↑↓ choose, Enter or → opens a folder or a file, ← or Backspace goes back
//! up, m shows more of a long folder, r reloads, Esc closes. A file is its
//! text, line by line (↑↓ PgUp PgDn scroll). Nothing is changed from here:
//! editing is on the web and in the app.
//!
//! Everything the machine answers is hostile until decoded here: a wrong type
//! drops the row, lists are capped, a name that is not one path component is
//! dropped, and every name and line reaches the screen through [`safe`]
//! (control, bidi and zero-width characters become `�`, a tab four spaces).
//! A refusal is one plain sentence chosen here, never the machine's words.

use super::app::{App, Mode};
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use serde_json::Value;

pub const ROOTS_MAX: usize = 64;
pub const LIST_MAX: usize = 500;
/// The lines of a file drawn (a 2 MiB file is read; the view stops here).
pub const LINES_MAX: usize = 20_000;
const LINE_CHARS_MAX: usize = 1_000;
const NAME_MAX: usize = 255;

/// Where the view is.
#[derive(Debug, Clone, PartialEq)]
pub enum Place {
    Roots,
    Folder { root: String, label: String, path: String },
    File { root: String, label: String, path: String },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Root {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Dir,
    File,
    Link,
    Other,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub name: String,
    pub kind: Kind,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Load {
    Loading,
    Roots {
        roots: Vec<Root>,
        refused: Vec<String>,
    },
    Folder {
        entries: Vec<Entry>,
        hidden: u64,
        next: Option<String>,
        partial: bool,
    },
    File {
        lines: Vec<String>,
        notice: Option<String>,
        masked: bool,
        truncated: bool,
    },
    Failed(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct View {
    pub machine_id: String,
    pub machine: String,
    pub place: Place,
    pub load: Load,
    /// The selected row (lists) or the first line shown (a file).
    pub cursor: usize,
    /// The request the view waits for (an older answer is dropped).
    pub want: Option<String>,
    /// Where to put the cursor when the folder going back to arrives.
    pub reselect: Option<String>,
}

/// A request for the event loop: (machine id, API path).
pub type Request = (String, String);

fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// `GET /api/v1/machines/{id}/fs/{op}?…`, every value percent-encoded.
pub fn path(machine_id: &str, op: &str, q: &[(&str, &str)]) -> String {
    let mut p = format!("/api/v1/machines/{}/fs/{op}", enc(machine_id));
    for (i, (k, v)) in q.iter().enumerate() {
        p.push(if i == 0 { '?' } else { '&' });
        p.push_str(&format!("{k}={}", enc(v)));
    }
    p
}

fn hidden_char(c: char) -> bool {
    c.is_control()
        || matches!(c,
            '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2069}' | '\u{FEFF}' | '\u{061C}' | '\u{00AD}')
}

/// Text from the machine as inert text on one line.
pub fn safe(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars().take(LINE_CHARS_MAX) {
        match c {
            '\t' => out.push_str("    "),
            c if hidden_char(c) => out.push('�'),
            c => out.push(c),
        }
    }
    out
}

fn valid_root_id(s: &str) -> bool {
    (1..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// One path component, as the protocol reads it.
fn component(s: &str) -> bool {
    !s.is_empty() && s != "." && s != ".." && s.len() <= NAME_MAX && !s.contains(['/', '\\', '\0'])
}

fn of_type<'a>(v: &'a Value, t: &str) -> Option<&'a Value> {
    (v["type"] == t).then_some(v)
}

pub fn decode_roots(v: &Value) -> Option<Load> {
    let o = of_type(v, "fs_roots")?;
    let mut roots: Vec<Root> = vec![];
    for r in o["roots"].as_array()?.iter().take(ROOTS_MAX) {
        let Some(id) = r["id"].as_str().filter(|i| valid_root_id(i)) else {
            continue;
        };
        if roots.iter().any(|x| x.id == id) {
            continue;
        }
        let label = r["label"]
            .as_str()
            .map(safe)
            .filter(|l| !l.trim().is_empty())
            .unwrap_or_else(|| "Folder".into());
        roots.push(Root {
            id: id.into(),
            label: label.chars().take(120).collect(),
        });
    }
    let refused = o["refused"]
        .as_array()
        .into_iter()
        .flatten()
        .take(ROOTS_MAX)
        .map(|r| {
            let label: String = safe(r["label"].as_str().unwrap_or("Folder"))
                .chars()
                .take(120)
                .collect();
            let why = match r["why"].as_str() {
                Some("gone") => "was moved or deleted",
                Some("unreachable") => "could not be opened",
                _ => "is too broad to browse (a home or system folder)",
            };
            format!("{label} {why}")
        })
        .collect();
    Some(Load::Roots { roots, refused })
}

/// `fs.list` for the folder that was asked; folders first, then by name.
pub fn decode_list(v: &Value, root: &str, path: &str) -> Option<Load> {
    let o = of_type(v, "fs_list")?;
    if o["root"] != root || o["path"] != path {
        return None;
    }
    let raw = o["entries"].as_array()?;
    let mut entries: Vec<Entry> = vec![];
    for e in raw.iter().take(LIST_MAX) {
        let Some(name) = e["name"].as_str().filter(|n| component(n)) else {
            continue;
        };
        if entries.iter().any(|x| x.name == name) {
            continue;
        }
        let kind = match e["kind"].as_str() {
            Some("dir") => Kind::Dir,
            Some("file") => Kind::File,
            Some("link") => Kind::Link,
            _ => Kind::Other,
        };
        let size = if kind == Kind::File {
            e["size"].as_u64().unwrap_or(0)
        } else {
            0
        };
        entries.push(Entry {
            name: name.into(),
            kind,
            size,
        });
    }
    entries.sort_by(|a, b| {
        (b.kind == Kind::Dir)
            .cmp(&(a.kind == Kind::Dir))
            .then_with(|| a.name.cmp(&b.name))
    });
    let next = o["next"]
        .as_str()
        .filter(|n| (3..=257).contains(&n.len()) && (n.starts_with("d/") || n.starts_with("f/")))
        .map(str::to_string);
    Some(Load::Folder {
        entries,
        hidden: o["hidden"].as_u64().unwrap_or(0).min(1_000_000),
        next,
        partial: o["partial"] == true || raw.len() > LIST_MAX,
    })
}

/// `fs.read`: a text file's lines, or why it is not shown.
pub fn decode_file(v: &Value, root: &str, path: &str) -> Option<Load> {
    let o = of_type(v, "fs_file")?;
    if o["root"] != root || o["path"] != path {
        return None;
    }
    let masked = o["masked"] == true;
    let mut truncated = o["truncated"] == true;
    let text = (o["kind"] == "text").then(|| o["text"].as_str()).flatten();
    let notice = match (o["kind"].as_str(), text) {
        (Some("text"), Some(_)) => None,
        (Some("image"), _) => Some("This is a picture. Pictures are not shown here.".to_string()),
        _ if o["why"] == "not_utf8" => Some("This file is not UTF-8 text, so it is not shown.".to_string()),
        _ => Some("This file is not text, so it is not shown.".to_string()),
    };
    let mut lines = vec![];
    if let Some(t) = text {
        for l in t.split('\n') {
            if lines.len() == LINES_MAX {
                truncated = true;
                break;
            }
            lines.push(safe(l.strip_suffix('\r').unwrap_or(l)));
        }
    }
    Some(Load::File {
        lines,
        notice,
        masked,
        truncated,
    })
}

/// One plain sentence for a refusal (the error's code), never the machine's own words.
pub fn problem(status: Option<u16>, code: &str) -> String {
    match code {
        "invalid_path" => "That name cannot be opened from here. Open it in the terminal.",
        "root_unknown" => "That folder is no longer one of this machine's places.",
        "root_gone" => "This folder was moved or deleted on the machine.",
        "root_refused" => "That folder is too broad to browse (a home or system folder).",
        "outside_root" => "That is outside this folder, so it is not shown.",
        "symlink_refused" => "Links are not followed here. Open it in the terminal.",
        "linked_file" => "This file has more than one name (a hard link), so it is not opened here.",
        "file_refused" => "Hidden: this may hold secrets. Open it in the terminal if you need it.",
        "not_found" => "Nothing is there any more.",
        "not_a_file" => "That is not a regular file.",
        "not_a_dir" => "That is not a folder.",
        "too_large" => "That file is too large to show.",
        "permission_denied" => "The machine's system did not allow opening that.",
        "rate_limited" => "Too many file requests at once. Try again in a moment.",
        "not_supported" | "unknown_method" => {
            "The runner on this machine does not browse files yet. Update the runner."
        }
        "machine_offline" | "not_connected" => "The machine is not connected.",
        "timeout" => "The machine didn't answer in time.",
        "forbidden" | "insufficient_scope" => "This sign-in may not read files. Sign in again with `tmuxedo login`.",
        _ => match status {
            None => "No connection. Try again.",
            Some(404) => "This machine is not in your account any more.",
            _ => "The machine could not do that.",
        },
    }
    .to_string()
}

pub fn size_text(n: u64) -> String {
    match n {
        n if n < 1024 => format!("{n} B"),
        n if n < 1024 * 1024 => format!("{:.1} KB", n as f64 / 1024.0),
        n => format!("{:.1} MB", n as f64 / 1024.0 / 1024.0),
    }
}

fn parent(path: &str) -> &str {
    path.rfind('/').map_or("", |i| &path[..i])
}

fn last(path: &str) -> &str {
    path.rfind('/').map_or(path, |i| &path[i + 1..])
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Plain,
    Selected,
    Muted,
    Danger,
}

impl View {
    /// "box › site › docs".
    pub fn title(&self) -> String {
        let mut t = format!("Files on {}", safe(&self.machine));
        if let Place::Folder { label, path, .. } | Place::File { label, path, .. } = &self.place {
            t.push_str(&format!(" › {label}"));
            for part in path.split('/').filter(|p| !p.is_empty()) {
                t.push_str(&format!(" › {}", safe(part)));
            }
        }
        t.push_str(" (read only)");
        t
    }

    /// The rows the list has (what Enter can open).
    fn len(&self) -> usize {
        match &self.load {
            Load::Roots { roots, .. } => roots.len(),
            Load::Folder { entries, .. } => entries.len(),
            Load::File { lines, .. } => lines.len(),
            _ => 0,
        }
    }

    /// The rows to draw, `width` columns wide.
    pub fn rows(&self, width: usize) -> Vec<(Tone, String)> {
        let mark = |i: usize| if i == self.cursor { "▶ " } else { "  " };
        let tone = |i: usize| if i == self.cursor { Tone::Selected } else { Tone::Plain };
        match &self.load {
            Load::Loading => vec![(Tone::Muted, "Loading…".into())],
            Load::Failed(why) => vec![
                (Tone::Danger, why.clone()),
                (Tone::Muted, "r tries again · ← goes back".into()),
            ],
            Load::Roots { roots, refused } => {
                let mut out: Vec<(Tone, String)> = roots
                    .iter()
                    .enumerate()
                    .map(|(i, r)| (tone(i), format!("{}{}", mark(i), r.label)))
                    .collect();
                if roots.is_empty() {
                    out.push((
                        Tone::Muted,
                        "No folders yet: start a session in a folder on this machine.".into(),
                    ));
                }
                out.extend(refused.iter().map(|r| (Tone::Muted, format!("  {r}"))));
                out
            }
            Load::Folder {
                entries,
                hidden,
                next,
                partial,
            } => {
                let mut out: Vec<(Tone, String)> = entries
                    .iter()
                    .enumerate()
                    .map(|(i, e)| {
                        let (icon, tail) = match e.kind {
                            Kind::Dir => ("", "/".to_string()),
                            Kind::File => ("", String::new()),
                            Kind::Link => ("↪ ", " (link, not followed)".to_string()),
                            Kind::Other => ("", " (not a regular file)".to_string()),
                        };
                        let left = format!("{}{icon}{}{tail}", mark(i), safe(&e.name));
                        let right = if e.kind == Kind::File {
                            size_text(e.size)
                        } else {
                            String::new()
                        };
                        let gap = width
                            .saturating_sub(left.chars().count() + right.chars().count())
                            .max(1);
                        (tone(i), format!("{left}{}{right}", " ".repeat(gap)))
                    })
                    .collect();
                if entries.is_empty() {
                    out.push((Tone::Muted, "This folder is empty.".into()));
                }
                if *hidden > 0 {
                    let n = if *hidden == 1 {
                        "1 item".to_string()
                    } else {
                        format!("{hidden} items")
                    };
                    out.push((Tone::Muted, format!("  {n} hidden: they may hold secrets")));
                }
                if next.is_some() {
                    out.push((Tone::Muted, "  m shows more".into()));
                } else if *partial {
                    out.push((Tone::Muted, "  Only part of this folder is shown.".into()));
                }
                out
            }
            Load::File {
                lines,
                notice,
                masked,
                truncated,
            } => {
                let mut out = vec![];
                if *masked {
                    out.push((Tone::Muted, "Parts that looked like secrets are hidden.".into()));
                }
                if let Some(n) = notice {
                    out.push((Tone::Plain, n.clone()));
                }
                out.extend(lines.iter().skip(self.cursor).map(|l| (Tone::Plain, l.clone())));
                if *truncated {
                    out.push((Tone::Muted, "Only the start of this file is shown.".into()));
                }
                out
            }
        }
    }

    pub fn hint(&self) -> &'static str {
        match self.place {
            Place::File { .. } => "files (read only): ↑↓ PgUp PgDn scroll · ← back · r reload · esc close",
            _ => "files (read only): ↑↓ choose · enter open · ← back · m more · r reload · esc close",
        }
    }
}

impl App {
    /// Open the view on the selected machine's places.
    pub fn files_open(&mut self) {
        let Some(m) = self.machine() else {
            self.note = Some("No machine selected.".into());
            return;
        };
        let (id, name) = (m.id.clone(), m.name.clone());
        self.files = Some(View {
            machine_id: id,
            machine: name,
            place: Place::Roots,
            load: Load::Loading,
            cursor: 0,
            want: None,
            reselect: None,
        });
        self.files_fetch(None);
        self.mode = Mode::Files;
    }

    /// Ask for what the view's place shows (`cursor`: the next page of a folder).
    fn files_fetch(&mut self, cursor: Option<&str>) {
        let Some(v) = self.files.as_mut() else { return };
        let p = match &v.place {
            Place::Roots => path(&v.machine_id, "roots", &[]),
            Place::Folder { root, path: dir, .. } => match cursor {
                Some(c) => path(&v.machine_id, "list", &[("root", root), ("path", dir), ("cursor", c)]),
                None => path(&v.machine_id, "list", &[("root", root), ("path", dir)]),
            },
            Place::File { root, path: f, .. } => path(&v.machine_id, "read", &[("root", root), ("path", f)]),
        };
        if cursor.is_none() {
            v.load = Load::Loading;
        }
        v.want = Some(p.clone());
        self.files_out.push((v.machine_id.clone(), p));
    }

    fn files_go(&mut self, place: Place, reselect: Option<String>) {
        if let Some(v) = self.files.as_mut() {
            v.place = place;
            v.cursor = 0;
            v.reselect = reselect;
        }
        self.files_fetch(None);
    }

    /// An answer from the event loop: `Ok(result)` or `Err((HTTP status, error code))`.
    pub fn on_files(&mut self, machine_id: &str, req: &str, r: Result<Value, (Option<u16>, String)>) {
        let Some(v) = self
            .files
            .as_mut()
            .filter(|v| v.machine_id == machine_id && v.want.as_deref() == Some(req))
        else {
            return;
        };
        v.want = None;
        let more = req.contains("&cursor=");
        let bad = || Load::Failed("The machine sent an answer tmuxedo could not read. Nothing was shown.".into());
        let load = match r {
            Err((status, code)) => {
                if more {
                    self.note = Some(problem(status, &code));
                    return;
                }
                Load::Failed(problem(status, &code))
            }
            Ok(x) => match &v.place {
                Place::Roots => decode_roots(&x).unwrap_or_else(bad),
                Place::Folder { root, path, .. } => decode_list(&x, root, path).unwrap_or_else(bad),
                Place::File { root, path, .. } => decode_file(&x, root, path).unwrap_or_else(bad),
            },
        };
        match (&mut v.load, load) {
            // "m": the next page joins the rows already shown.
            (
                Load::Folder {
                    entries,
                    hidden,
                    next,
                    partial,
                },
                Load::Folder {
                    entries: e2,
                    hidden: h2,
                    next: n2,
                    partial: p2,
                },
            ) if more => {
                for e in e2 {
                    if entries.len() < LIST_MAX * 20 && !entries.iter().any(|x| x.name == e.name) {
                        entries.push(e);
                    }
                }
                *hidden += h2;
                *next = n2;
                *partial |= p2;
            }
            (slot, load) => *slot = load,
        }
        if let (Some(name), Load::Folder { entries, .. }) = (v.reselect.take(), &v.load) {
            v.cursor = entries.iter().position(|e| e.name == name).unwrap_or(0);
        } else if let Load::Roots { roots, .. } = &v.load {
            if let Some(name) = v.reselect.take() {
                v.cursor = roots.iter().position(|r| r.id == name).unwrap_or(0);
            }
        }
    }

    /// A key while the view is open.
    pub fn files_key(&mut self, k: KeyEvent) {
        let Some(v) = self.files.as_mut() else {
            self.mode = Mode::Normal;
            return;
        };
        let end = v.len().saturating_sub(1);
        let file = matches!(v.place, Place::File { .. });
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.files = None;
                self.mode = Mode::Normal;
            }
            KeyCode::Up | KeyCode::Char('k') => v.cursor = v.cursor.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => v.cursor = (v.cursor + 1).min(end),
            KeyCode::PageUp => v.cursor = v.cursor.saturating_sub(10),
            KeyCode::PageDown => v.cursor = (v.cursor + 10).min(end),
            KeyCode::Home | KeyCode::Char('g') => v.cursor = 0,
            KeyCode::End | KeyCode::Char('G') => v.cursor = end,
            KeyCode::Char('r') => self.files_fetch(None),
            KeyCode::Char('m') if !file => {
                if let Load::Folder { next: Some(n), .. } = &v.load {
                    let n = n.clone();
                    self.files_fetch(Some(&n));
                }
            }
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') if !file => {
                let to = match (&v.place, &v.load) {
                    (Place::Roots, Load::Roots { roots, .. }) => roots.get(v.cursor).map(|r| Place::Folder {
                        root: r.id.clone(),
                        label: r.label.clone(),
                        path: String::new(),
                    }),
                    (Place::Folder { root, label, path }, Load::Folder { entries, .. }) => {
                        entries.get(v.cursor).and_then(|e| {
                            let to = if path.is_empty() {
                                e.name.clone()
                            } else {
                                format!("{path}/{}", e.name)
                            };
                            let (root, label) = (root.clone(), label.clone());
                            match e.kind {
                                Kind::Dir => Some(Place::Folder { root, label, path: to }),
                                Kind::File => Some(Place::File { root, label, path: to }),
                                _ => None,
                            }
                        })
                    }
                    _ => None,
                };
                if let Some(to) = to {
                    self.files_go(to, None);
                }
            }
            KeyCode::Left | KeyCode::Backspace | KeyCode::Char('h') => {
                let back = match &v.place {
                    Place::Roots => None,
                    Place::Folder { root, path, .. } if path.is_empty() => Some((Place::Roots, Some(root.clone()))),
                    Place::Folder { root, label, path } | Place::File { root, label, path } => Some((
                        Place::Folder {
                            root: root.clone(),
                            label: label.clone(),
                            path: parent(path).to_string(),
                        },
                        Some(last(path).to_string()),
                    )),
                };
                if let Some((to, sel)) = back {
                    self.files_go(to, sel);
                }
            }
            _ => {}
        }
    }
}
