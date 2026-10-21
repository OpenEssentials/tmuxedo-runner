//! `prefix /`: search everything from the terminal (PLAN Phase 25; the web's
//! /search is web/src/components/search/SearchPage.tsx). A popup over the
//! panes asks the same two places the web does, with the CLI token:
//!
//! - `GET /api/search?q=…&kind=…`: your sessions, projects, machines and
//!   activity, as the server knows them;
//! - `GET /api/search/machines/{id}?q=…&kind=…`, once per online machine:
//!   the terminal and chat history that machine keeps (opt-in per project),
//!   one group per machine with a status line in the web's words.
//!
//! ```text
//! Search everything: deploy▏
//! All · Tab changes what is searched
//! Sessions
//! ▶ api tests — …the deploy failed at step 3… · box
//! box
//! box: 2 results.
//!   Terminal sh — make deploy · billing · 2026-10-18 14:02
//! ```
//!
//! Typing searches after a short pause (the web's 250 ms); Tab and Shift-Tab
//! change the kind (All, Sessions, Projects, Machines, Activity, Terminal,
//! Chats; the same rules as the web's chips); ↑↓ choose; Enter opens a
//! result (a pane, scrolled to the line when this terminal still has it;
//! a chat; a project or a machine; anything else in the browser); Esc
//! closes. An offline machine is searched once it comes back while this is
//! open. Too small for a box, it is a plain list.
//!
//! Everything an answer holds is hostile until decoded here: wrong types
//! drop the hit, lists and text are capped, offsets are checked, links are
//! only ever this app's own paths, and every string reaches the screen
//! through [`super::browse::safe`]. A refusal is one sentence chosen here.

use super::app::{App, Item, Mode};
use super::browse::safe;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use serde_json::Value;
use std::time::{Duration, Instant};

/// Typing waits this long before searching (the web's SEARCH_DEBOUNCE_MS).
pub const DEBOUNCE: Duration = Duration::from_millis(250);
/// The web's SEARCH_QUERY_MAX.
pub const QUERY_MAX: usize = 200;
/// Hits read from one answer (the server's limit is 50, a machine's 20).
pub const HITS_MAX: usize = 50;
const SNIPPET_MAX: usize = 400;
const TITLE_MAX: usize = 200;
const NAME_MAX: usize = 64;
/// How long an opened pane may take to arrive before the line is looked for.
const FIND_WAIT: Duration = Duration::from_secs(10);

/// What is searched: the web's kind chips, one at a time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    All,
    Sessions,
    Projects,
    Machines,
    Activity,
    Terminal,
    Chats,
}

impl Kind {
    pub const EVERY: [Kind; 7] = [
        Kind::All,
        Kind::Sessions,
        Kind::Projects,
        Kind::Machines,
        Kind::Activity,
        Kind::Terminal,
        Kind::Chats,
    ];
    pub fn title(self) -> &'static str {
        match self {
            Kind::All => "All",
            Kind::Sessions => "Sessions",
            Kind::Projects => "Projects",
            Kind::Machines => "Machines",
            Kind::Activity => "Activity",
            Kind::Terminal => "Terminal",
            Kind::Chats => "Chats",
        }
    }
    /// The server's kind (`?kind=`), if this is one.
    fn server(self) -> Option<&'static str> {
        match self {
            Kind::Sessions => Some("session"),
            Kind::Projects => Some("project"),
            Kind::Machines => Some("machine"),
            Kind::Activity => Some("event"),
            _ => None,
        }
    }
    /// A machine's kind (`?kind=`), if this is one.
    fn history(self) -> Option<&'static str> {
        match self {
            Kind::Terminal => Some("terminal"),
            Kind::Chats => Some("chat"),
            _ => None,
        }
    }
    /// The web's searchesServer: no kind, or one of the server's.
    pub fn searches_server(self) -> bool {
        self == Kind::All || self.server().is_some()
    }
    /// The web's searchesMachines: no kind, or a history kind.
    pub fn searches_machines(self) -> bool {
        self == Kind::All || self.history().is_some()
    }
    fn step(self, by: isize) -> Kind {
        let n = Kind::EVERY.len() as isize;
        let at = Kind::EVERY.iter().position(|k| *k == self).unwrap_or(0) as isize;
        Kind::EVERY[((at + by).rem_euclid(n)) as usize]
    }
}

/// Who a request asks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Server,
    Machine(String),
}

/// A request for the event loop: a GET with the CLI token. `gen` drops late answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub gen: u64,
    pub target: Target,
    pub path: String,
}

/// Where a result goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Jump {
    /// A pane; `line` is the machine's history index (the web scrolls to it).
    Pane {
        machine: String,
        pane: String,
        line: Option<u64>,
    },
    Chat {
        machine: String,
        chat: String,
    },
    Project {
        machine: String,
        ws: String,
    },
    Machine(String),
    /// Another page of the web app (a path, never a whole URL).
    Web(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    /// The server's group (Sessions, Projects, Machines, Activity); empty on a machine's hit.
    pub group: &'static str,
    /// "Terminal" or "Chat" on a machine's hit; empty on the server's.
    pub label: &'static str,
    pub title: String,
    /// The snippet around the match: before, the match, after (all safe).
    pub before: String,
    pub matched: String,
    pub after: String,
    /// Machine, project, time: dim, after the snippet.
    pub place: String,
    pub jump: Option<Jump>,
    /// The snippet's plain text, looked for in the pane's scrollback.
    pub find: String,
}

/// One part's answer.
#[derive(Debug, Clone, PartialEq)]
pub enum Answer {
    Searching,
    Done {
        hits: Vec<Hit>,
        partial: bool,
        more: bool,
    },
    Off,
    Offline,
    Old,
    /// A sentence saying why.
    Failed(String),
}

/// The popup's state while open (kept after Esc, so `prefix /` comes back to it).
#[derive(Debug, Clone, PartialEq)]
pub struct View {
    pub query: String,
    pub kind: Kind,
    pub gen: u64,
    /// When the typed query is searched.
    pub due: Option<Instant>,
    /// The query and kind the answers are for.
    pub sent: Option<(String, Kind)>,
    /// The server's hits by kind (None: not asked for this kind).
    pub server: Option<Answer>,
    /// The server had more than it sent.
    pub truncated: bool,
    /// (machine id, name, answer), in the sidebar's order.
    pub machines: Vec<(String, String, Answer)>,
    /// Index into [`View::hits`].
    pub cursor: usize,
}

impl View {
    pub fn new() -> View {
        View {
            query: String::new(),
            kind: Kind::All,
            gen: 0,
            due: None,
            sent: None,
            server: None,
            truncated: false,
            machines: vec![],
            cursor: 0,
        }
    }

    /// Every hit in the order drawn: the server's groups, then each machine's.
    pub fn hits(&self) -> Vec<&Hit> {
        let mut v: Vec<&Hit> = vec![];
        for (_, hits) in self.server_groups() {
            v.extend(hits);
        }
        for (_, _, a) in &self.machines {
            if let Answer::Done { hits, .. } = a {
                v.extend(hits.iter());
            }
        }
        v
    }

    /// The server's hits as the web groups them (Sessions, Projects, Machines, Activity).
    pub fn server_groups(&self) -> Vec<(&'static str, Vec<&Hit>)> {
        let Some(Answer::Done { hits, .. }) = &self.server else {
            return vec![];
        };
        let mut out: Vec<(&'static str, Vec<&Hit>)> = vec![];
        for g in ["Sessions", "Projects", "Machines", "Activity"] {
            let these: Vec<&Hit> = hits.iter().filter(|h| h.group == g).collect();
            if !these.is_empty() {
                out.push((g, these));
            }
        }
        out
    }
}

impl Default for View {
    fn default() -> Self {
        View::new()
    }
}

/// The web's normaliseQuery: controls become spaces, runs of space one, trimmed, at most 200 characters.
pub fn normalise(text: &str) -> Option<String> {
    let spaced: String = text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let q: String = spaced.split_whitespace().collect::<Vec<_>>().join(" ");
    let q: String = q.chars().take(QUERY_MAX).collect::<String>().trim().to_string();
    (!q.is_empty()).then_some(q)
}

/// Percent-encode everything but unreserved characters.
pub fn enc(s: &str) -> String {
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

fn dec(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = vec![];
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' => {
                let h = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
                out.push(u8::from_str_radix(h, 16).ok()?);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

/// `GET /api/search?…` for the server's part.
pub fn server_path(q: &str, kind: Kind) -> String {
    let mut p = format!("/api/search?q={}", enc(q));
    if let Some(k) = kind.server() {
        p.push_str(&format!("&kind={k}"));
    }
    p
}

/// `GET /api/search/machines/{id}?…` for one machine.
pub fn machine_path(mid: &str, q: &str, kind: Kind) -> String {
    let mut p = format!("/api/search/machines/{}?q={}", enc(mid), enc(q));
    if let Some(k) = kind.history() {
        p.push_str(&format!("&kind={k}"));
    }
    p
}

fn id_ok(s: &str, max: usize) -> bool {
    (1..=max).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"_:.-".contains(&b))
}

/// Where a link the server made goes: only this app's own paths, ids checked.
pub fn jump_of(href: &str) -> Option<Jump> {
    if !href.starts_with('/') || href.starts_with("//") || href.len() > 2048 || href.contains('\\') {
        return None;
    }
    let (path, query) = href.split_once('?').unwrap_or((href, ""));
    let Some(rest) = path.strip_prefix("/machines/") else {
        return Some(Jump::Web(href.to_string()));
    };
    let machine = dec(rest).filter(|m| id_ok(m, 64))?;
    let get = |k: &str| {
        query
            .split('&')
            .filter_map(|kv| kv.split_once('='))
            .find(|(a, _)| *a == k)
            .and_then(|(_, v)| dec(v))
    };
    if let Some(pane) = get("pane") {
        let pane = Some(pane).filter(|p| id_ok(p, 128))?;
        let line = get("line").filter(|l| (1..=15).contains(&l.len()) && l.bytes().all(|b| b.is_ascii_digit()));
        return Some(Jump::Pane {
            machine,
            pane,
            line: line.and_then(|l| l.parse().ok()),
        });
    }
    if let Some(chat) = get("chat") {
        return Some(chat)
            .filter(|c| id_ok(c, 128))
            .map(|chat| Jump::Chat { machine, chat });
    }
    if let Some(ws) = get("ws") {
        return Some(ws)
            .filter(|w| id_ok(w, 128))
            .map(|ws| Jump::Project { machine, ws });
    }
    Some(Jump::Machine(machine))
}

fn text(v: &Value, max: usize) -> String {
    v.as_str()
        .map(|s| s.chars().take(max).collect::<String>())
        .unwrap_or_default()
}

/// `2026-10-18T14:02:11.000Z` → `2026-10-18 14:02` (UTC), else nothing.
pub fn when(v: &Value) -> String {
    let Some(s) = v.as_str() else { return String::new() };
    let b = s.as_bytes();
    let shape = b.len() >= 16
        && b[..16].iter().enumerate().all(|(i, c)| match i {
            4 | 7 => *c == b'-',
            10 => *c == b'T',
            13 => *c == b':',
            _ => c.is_ascii_digit(),
        });
    if shape {
        format!("{} {}", &s[..10], &s[11..16])
    } else {
        String::new()
    }
}

/// The snippet cut at `[a, b)` (UTF-16 code units, as the web sends it), each part safe.
fn split_snippet(snippet: &str, m: &Value) -> (String, String, String) {
    let range = m.as_array().filter(|a| a.len() == 2).and_then(|a| {
        let (x, y) = (a[0].as_u64()? as usize, a[1].as_u64()? as usize);
        (x < y).then_some((x, y))
    });
    let Some((x, y)) = range else {
        return (safe(snippet), String::new(), String::new());
    };
    // UTF-16 offsets to char boundaries; an offset inside a pair or past the end voids the match.
    let mut at16 = 0usize;
    let (mut cx, mut cy) = (None, None);
    for (i, c) in snippet.char_indices() {
        if at16 == x {
            cx = Some(i);
        }
        if at16 == y {
            cy = Some(i);
        }
        at16 += c.len_utf16();
    }
    if at16 == y {
        cy = Some(snippet.len());
    }
    match (cx, cy) {
        (Some(i), Some(j)) => (safe(&snippet[..i]), safe(&snippet[i..j]), safe(&snippet[j..])),
        _ => (safe(snippet), String::new(), String::new()),
    }
}

fn plain(snippet: &str) -> String {
    safe(snippet)
        .trim_matches(|c: char| c == '…' || c.is_whitespace())
        .to_string()
}

/// The server's answer (`{query, hits, truncated}`), hostile until read.
pub fn decode_server(v: &Value) -> Option<(Vec<Hit>, bool)> {
    let list = v["hits"].as_array()?;
    let mut hits = vec![];
    for h in list.iter().take(HITS_MAX) {
        let group = match h["kind"].as_str() {
            Some("session") => "Sessions",
            Some("project") => "Projects",
            Some("machine") => "Machines",
            Some("event") => "Activity",
            _ => continue,
        };
        let title = safe(&text(&h["title"], TITLE_MAX));
        let snippet = text(&h["snippet"], SNIPPET_MAX);
        if title.trim().is_empty() && snippet.trim().is_empty() {
            continue;
        }
        let (before, matched, after) = split_snippet(&snippet, &h["match"]);
        let mut place = vec![];
        let name = safe(&text(&h["machineName"], NAME_MAX));
        if !name.trim().is_empty() {
            place.push(name);
        }
        let at = when(&h["at"]);
        if !at.is_empty() {
            place.push(at);
        }
        hits.push(Hit {
            group,
            label: "",
            title,
            before,
            matched,
            after,
            place: place.join(" · "),
            jump: h["href"].as_str().and_then(jump_of),
            find: plain(&snippet),
        });
    }
    Some((hits, v["truncated"].as_bool() == Some(true)))
}

/// One machine's answer (`{state, hits, partial, more}`).
pub fn decode_machine(v: &Value) -> Option<Answer> {
    match v["state"].as_str()? {
        "off" => Some(Answer::Off),
        "offline" => Some(Answer::Offline),
        "old" => Some(Answer::Old),
        "done" => {
            let mut hits = vec![];
            for h in v["hits"].as_array()?.iter().take(HITS_MAX) {
                let label = match h["kind"].as_str() {
                    Some("terminal") => "Terminal",
                    Some("chat") => "Chat",
                    _ => continue,
                };
                let snippet = text(&h["snippet"], SNIPPET_MAX);
                if snippet.trim().is_empty() {
                    continue;
                }
                let (before, matched, after) = split_snippet(&snippet, &h["match"]);
                let mut place = vec![];
                for s in [text(&h["project"], TITLE_MAX), text(&h["agent"], NAME_MAX)] {
                    let s = safe(&s);
                    if !s.trim().is_empty() {
                        place.push(s);
                    }
                }
                let at = when(&h["at"]);
                if !at.is_empty() {
                    place.push(at);
                }
                hits.push(Hit {
                    group: "",
                    label,
                    title: safe(&text(&h["title"], TITLE_MAX)),
                    before,
                    matched,
                    after,
                    place: place.join(" · "),
                    jump: h["href"].as_str().and_then(jump_of),
                    find: plain(&snippet),
                });
            }
            Some(Answer::Done {
                hits,
                partial: v["partial"].as_bool() == Some(true),
                more: v["more"].as_bool() == Some(true),
            })
        }
        _ => None,
    }
}

/// One sentence for a failed request (HTTP status and error code; None: no connection).
pub fn problem(status: Option<u16>, code: &str) -> String {
    match (status, code) {
        (None, _) => "No connection to tmuxedo. Type again to retry.",
        (_, "invalid_query") => "A search needs 1 to 200 characters.",
        (Some(429), _) | (_, "rate_limited") => "Too many searches at once. Try again in a moment.",
        (_, "insufficient_scope") | (Some(401), _) | (Some(403), _) => {
            "This sign-in may not search. Sign in again with `tmuxedo login`."
        }
        (Some(404), _) => "This machine is not in your account any more.",
        _ => "The search did not work. Try again in a moment.",
    }
    .to_string()
}

/// A machine's status line (the web's machineStatus).
pub fn machine_status(name: &str, a: &Answer) -> String {
    match a {
        Answer::Searching => format!("Searching {name}…"),
        Answer::Offline => format!("{name} is offline; will search when it's back (keep this open)."),
        Answer::Off => format!("{name}: History is off for every project (Settings → Search history)."),
        Answer::Old => format!("{name}: update its runner to search its history."),
        Answer::Failed(why) => format!("{name} didn't answer. {why}"),
        Answer::Done { hits, partial, more } => {
            let n = hits.len();
            let plus = if *more || *partial { "+" } else { "" };
            let stopped = if *partial { " (stopped after 5 s)" } else { "" };
            if n == 0 {
                format!("{name}: nothing found{}.", if *partial { " in 5 s" } else { "" })
            } else {
                let s = if n == 1 && plus.is_empty() { "" } else { "s" };
                format!("{name}: {n}{plus} result{s}{stopped}.")
            }
        }
    }
}

/// A line drawn in the popup.
#[derive(Debug, Clone, PartialEq)]
pub enum Line<'a> {
    Head(String),
    Note(String),
    Hit(usize, &'a Hit),
}

/// What the popup lists, top to bottom.
pub fn lines(v: &View) -> Vec<Line<'_>> {
    let mut out = vec![];
    if normalise(&v.query).is_none() {
        out.push(Line::Note(
            "Type to search your sessions, projects, machines and activity, and the terminal and chat history each machine keeps.".into(),
        ));
        return out;
    }
    let mut n = 0;
    match &v.server {
        Some(Answer::Searching) => out.push(Line::Note("Searching…".into())),
        Some(Answer::Failed(why)) => out.push(Line::Note(why.clone())),
        _ => {}
    }
    for (g, hits) in v.server_groups() {
        out.push(Line::Head(g.to_string()));
        for h in hits {
            out.push(Line::Hit(n, h));
            n += 1;
        }
    }
    if v.truncated {
        out.push(Line::Note(
            "More results: narrow the search, or Tab to one kind.".into(),
        ));
    }
    for (_, name, a) in &v.machines {
        out.push(Line::Head(name.clone()));
        out.push(Line::Note(machine_status(name, a)));
        if let Answer::Done { hits, .. } = a {
            for h in hits {
                out.push(Line::Hit(n, h));
                n += 1;
            }
        }
    }
    let settled = !matches!(v.server, Some(Answer::Searching))
        && v.machines.iter().all(|(_, _, a)| a != &Answer::Searching)
        && v.due.is_none();
    if n == 0 && settled && v.sent.is_some() && !matches!(v.server, Some(Answer::Failed(_))) {
        let q = normalise(&v.query).unwrap_or_default();
        out.insert(0, Line::Note(format!("Nothing found for \"{}\".", safe(&q))));
    }
    out
}

/// After `prefix /` opened a pane result: the line to look for once the pane is drawn.
#[derive(Debug, Clone, PartialEq)]
pub struct Find {
    pub machine: String,
    pub pane: String,
    pub needles: Vec<String>,
    pub until: Instant,
}

/// The scrollback offset that shows the newest row holding a needle a third
/// of the way down, or None. The pane is left where it was.
pub fn find_offset(t: &mut super::app::PaneTerm, needles: &[String]) -> Option<usize> {
    let max = t.history();
    let was = t.emu.screen().scrollback();
    let (rows, cols) = t.emu.screen().size();
    let rows = rows.max(1) as usize;
    let wanted: Vec<String> = needles
        .iter()
        .map(|n| n.to_lowercase())
        .filter(|n| !n.trim().is_empty())
        .collect();
    let mut found = None;
    let mut off = 0usize;
    'look: loop {
        t.emu.parser_mut().screen_mut().set_scrollback(off);
        let shown: Vec<String> = t.emu.screen().rows(0, cols).map(|r| r.to_lowercase()).collect();
        for n in &wanted {
            if let Some(r) = shown.iter().position(|row| row.contains(n.as_str())) {
                let to = (off + rows / 3).saturating_sub(r).min(max);
                found = Some(to);
                break 'look;
            }
        }
        if off >= max {
            break;
        }
        off = (off + rows).min(max);
    }
    t.emu.parser_mut().screen_mut().set_scrollback(was);
    found
}

impl App {
    /// `prefix /`: open search (again: as it was, searched afresh).
    pub fn search_open(&mut self) {
        let mut v = self.search.take().unwrap_or_default();
        if normalise(&v.query).is_some() {
            v.due = Some(Instant::now());
        }
        self.search = Some(v);
        self.mode = Mode::Search;
    }

    /// Ask the server and every online machine for `v.query` (as the kind says).
    fn search_fire(&mut self) {
        let machines: Vec<(String, String, bool)> = self
            .machines
            .iter()
            .map(|m| (m.id.clone(), safe(&m.name), m.online))
            .collect();
        let Some(v) = self.search.as_mut() else { return };
        v.due = None;
        v.cursor = 0;
        v.gen += 1;
        v.truncated = false;
        let Some(q) = normalise(&v.query) else {
            v.sent = None;
            v.server = None;
            v.machines.clear();
            return;
        };
        v.sent = Some((q.clone(), v.kind));
        v.server = None;
        if v.kind.searches_server() {
            v.server = Some(Answer::Searching);
            self.search_out.push(Request {
                gen: v.gen,
                target: Target::Server,
                path: server_path(&q, v.kind),
            });
        }
        v.machines.clear();
        if v.kind.searches_machines() {
            for (id, name, online) in machines {
                if online {
                    self.search_out.push(Request {
                        gen: v.gen,
                        target: Target::Machine(id.clone()),
                        path: machine_path(&id, &q, v.kind),
                    });
                }
                let a = if online { Answer::Searching } else { Answer::Offline };
                v.machines.push((id, name, a));
            }
        }
    }

    /// Every 250 ms: search what was typed, ask machines that came back, look for an opened line.
    pub fn search_tick(&mut self, now: Instant) {
        if self.search.as_ref().and_then(|v| v.due).is_some_and(|d| now >= d) {
            self.search_fire();
        }
        let online: Vec<String> = self
            .machines
            .iter()
            .filter(|m| m.online)
            .map(|m| m.id.clone())
            .collect();
        if let Some(v) = self.search.as_mut() {
            if let Some((q, kind)) = v.sent.clone() {
                for (id, _, a) in v.machines.iter_mut() {
                    if *a == Answer::Offline && online.contains(id) {
                        *a = Answer::Searching;
                        self.search_out.push(Request {
                            gen: v.gen,
                            target: Target::Machine(id.clone()),
                            path: machine_path(id, &q, kind),
                        });
                    }
                }
            }
        }
        self.search_find_tick(now);
    }

    fn search_find_tick(&mut self, now: Instant) {
        let Some(f) = self.search_find.clone() else { return };
        if now >= f.until {
            self.search_find = None;
            return;
        }
        let here = self.machine().is_some_and(|m| m.id == f.machine) && self.focus.as_deref() == Some(&f.pane);
        let key = (f.machine.clone(), f.pane.clone());
        let ready = self.terms.get(&key).is_some_and(|t| t.processed.is_some());
        if !here || !ready {
            return;
        }
        self.search_find = None;
        let Some(t) = self.terms.get_mut(&key) else { return };
        match find_offset(t, &f.needles) {
            Some(off) => {
                t.emu.parser_mut().screen_mut().set_scrollback(off);
                self.mode = Mode::Scroll(f.pane.clone());
                self.note = Some("The line you searched for is in view. q goes back to the bottom.".into());
            }
            None => {
                self.note = Some("That line is no longer in this terminal's history here.".into());
            }
        }
    }

    /// An answer came back (`Err`: no connection).
    pub fn on_search(&mut self, gen: u64, target: Target, r: Result<(u16, Value), String>) {
        let Some(v) = self.search.as_mut() else { return };
        if gen != v.gen {
            return;
        }
        match target {
            Target::Server => {
                v.server = Some(match &r {
                    Ok((200, body)) => match decode_server(body) {
                        Some((hits, truncated)) => {
                            v.truncated = truncated;
                            Answer::Done {
                                hits,
                                partial: false,
                                more: truncated,
                            }
                        }
                        None => Answer::Failed(problem(Some(200), "")),
                    },
                    Ok((code, body)) => {
                        Answer::Failed(problem(Some(*code), body["error"]["code"].as_str().unwrap_or("")))
                    }
                    Err(_) => Answer::Failed(problem(None, "")),
                });
            }
            Target::Machine(mid) => {
                let Some(slot) = v.machines.iter_mut().find(|(id, _, _)| *id == mid) else {
                    return;
                };
                slot.2 = match &r {
                    Ok((200, body)) => decode_machine(body).unwrap_or_else(|| Answer::Failed(problem(Some(200), ""))),
                    Ok((code, body)) => {
                        let code_s = body["error"]["code"].as_str().unwrap_or("");
                        if code_s == "machine_offline" {
                            Answer::Offline
                        } else {
                            Answer::Failed(problem(Some(*code), code_s))
                        }
                    }
                    Err(_) => Answer::Failed(problem(None, "")),
                };
            }
        }
        let n = v.hits().len();
        v.cursor = v.cursor.min(n.saturating_sub(1));
    }

    /// A key while search is open.
    pub fn search_key(&mut self, k: KeyEvent) {
        if k.kind == KeyEventKind::Release {
            return;
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let Some(v) = self.search.as_mut() else {
            self.mode = Mode::Normal;
            return;
        };
        let n = v.hits().len();
        let typed = |v: &mut View| {
            v.due = Some(Instant::now() + DEBOUNCE);
            v.cursor = 0;
        };
        match k.code {
            KeyCode::Esc => self.mode = Mode::Normal,
            KeyCode::Up => {
                v.cursor = if v.cursor == 0 {
                    n.saturating_sub(1)
                } else {
                    v.cursor - 1
                }
            }
            KeyCode::Down => v.cursor = if n == 0 { 0 } else { (v.cursor + 1) % n },
            KeyCode::Tab | KeyCode::BackTab => {
                v.kind = v.kind.step(if k.code == KeyCode::Tab { 1 } else { -1 });
                v.due = Some(Instant::now());
            }
            KeyCode::Enter => {
                let hit = v.hits().get(v.cursor).map(|h| (*h).clone());
                match hit {
                    Some(h) => self.search_go(h),
                    None => {
                        if normalise(&v.query).is_some() {
                            v.due = Some(Instant::now());
                        }
                    }
                }
            }
            KeyCode::Backspace => {
                v.query.pop();
                typed(v);
            }
            KeyCode::Char('u') if ctrl => {
                v.query.clear();
                typed(v);
            }
            KeyCode::Char(c) if !ctrl && !k.modifiers.contains(KeyModifiers::ALT) && !c.is_control() => {
                if v.query.chars().count() < QUERY_MAX {
                    v.query.push(c);
                }
                typed(v);
            }
            _ => {}
        }
    }

    /// Bracketed paste into the query: one line, capped.
    pub fn search_paste(&mut self, s: &str) {
        if let Some(v) = self.search.as_mut() {
            for c in s.chars().map(|c| if c.is_control() { ' ' } else { c }) {
                if v.query.chars().count() >= QUERY_MAX {
                    break;
                }
                v.query.push(c);
            }
            v.due = Some(Instant::now() + DEBOUNCE);
            v.cursor = 0;
        }
    }

    /// Open a result here, or in the browser when this terminal cannot show it.
    pub fn search_go(&mut self, h: Hit) {
        self.mode = Mode::Normal;
        let at = |app: &App, mid: &str| app.machines.iter().position(|m| m.id == mid);
        let web = |app: &mut App, path: String| {
            app.open_urls
                .push(format!("{}{path}", app.web_url.trim_end_matches('/')));
            app.note = Some("Opening it in your browser.".into());
        };
        match h.jump {
            None => self.note = Some("This result cannot be opened from here.".into()),
            Some(Jump::Web(p)) => web(self, p),
            Some(Jump::Pane { machine, pane, line }) => match at(self, &machine) {
                Some(i) => {
                    self.activate(&Item::Agent(i, pane.clone()));
                    let mut needles = vec![];
                    if !h.find.is_empty() {
                        needles.push(h.find.clone());
                    }
                    if !h.matched.is_empty() {
                        needles.push(h.matched.clone());
                    }
                    if line.is_some() || h.label == "Terminal" {
                        self.search_find = Some(Find {
                            machine,
                            pane,
                            needles,
                            until: Instant::now() + FIND_WAIT,
                        });
                    }
                }
                None => {
                    let mut p = format!("/machines/{}?pane={}", enc(&machine), enc(&pane));
                    if let Some(l) = line {
                        p.push_str(&format!("&line={l}"));
                    }
                    web(self, p)
                }
            },
            Some(Jump::Chat { machine, chat }) => match at(self, &machine) {
                Some(i) => {
                    self.activate(&Item::Machine(i));
                    self.chat_open(&chat);
                }
                None => web(self, format!("/machines/{}?chat={}", enc(&machine), enc(&chat))),
            },
            Some(Jump::Project { machine, ws }) => match at(self, &machine) {
                Some(i) => self.activate(&Item::Workspace(i, ws)),
                None => web(self, format!("/machines/{}", enc(&machine))),
            },
            Some(Jump::Machine(machine)) => match at(self, &machine) {
                Some(i) => self.activate(&Item::Machine(i)),
                None => web(self, format!("/machines/{}", enc(&machine))),
            },
        }
    }

    /// The status line while search is open.
    pub fn search_line(&self) -> String {
        "search: type · tab what · ↑↓ choose · enter open · esc close".into()
    }
}

fn clear(buf: &mut Buffer, r: Rect, bg: Style) {
    buf.set_style(r, bg);
    for x in r.left()..r.right() {
        for y in r.top()..r.bottom() {
            buf[(x, y)].set_symbol(" ");
        }
    }
}

/// Spans on one row from `x`, cut at `right` (wide characters measured by the buffer).
fn spans(buf: &mut Buffer, x: u16, y: u16, right: u16, parts: &[(&str, Style)]) {
    let mut at = x;
    for (s, st) in parts {
        if at >= right || s.is_empty() {
            continue;
        }
        let (nx, _) = buf.set_stringn(at, y, s, (right - at) as usize, *st);
        at = nx;
    }
}

/// Draw search over the panes: a box when there is room, else a plain list.
pub fn draw(v: &View, main: Rect, buf: &mut Buffer) {
    if main.width < 8 || main.height < 2 {
        return;
    }
    let boxed = main.width >= 40 && main.height >= 10;
    let r = if boxed {
        let w = main.width.saturating_sub(4).min(100);
        Rect::new(main.x + (main.width - w) / 2, main.y + 1, w, main.height - 2)
    } else {
        main
    };
    clear(buf, r, Style::default().bg(Color::Indexed(235)));
    let (x, right) = if boxed {
        (r.x + 1, r.right() - 1)
    } else {
        (r.x, r.right())
    };
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(Color::DarkGray);
    let q = safe(&v.query);
    let ask = if boxed { "Search everything: " } else { "Search: " };
    spans(buf, x, r.y, right, &[(ask, bold), (&q, bold), ("▏", bold)]);
    let mut y = r.y + 1;
    let footer = boxed;
    let bottom = if footer { r.bottom() - 1 } else { r.bottom() };
    if boxed {
        let kind = format!("{} · Tab changes what is searched", v.kind.title());
        spans(buf, x, y, right, &[(&kind, dim)]);
        y += 1;
    } else if v.kind != Kind::All {
        spans(buf, x, y, right, &[(v.kind.title(), dim)]);
        y += 1;
    }
    let all = lines(v);
    let room = bottom.saturating_sub(y) as usize;
    // Keep the chosen hit in view.
    let at = all
        .iter()
        .position(|l| matches!(l, Line::Hit(i, _) if *i == v.cursor))
        .unwrap_or(0);
    let top = at.saturating_sub(room.saturating_sub(1));
    for l in all.iter().skip(top).take(room) {
        match l {
            Line::Head(t) => spans(buf, x, y, right, &[(t, bold.fg(Color::Cyan))]),
            Line::Note(t) => spans(buf, x, y, right, &[(t, dim)]),
            Line::Hit(i, h) => {
                let chosen = *i == v.cursor;
                let base = if chosen {
                    Style::default().add_modifier(Modifier::REVERSED)
                } else {
                    Style::default()
                };
                let mark = if chosen { "▶ " } else { "  " };
                let label = if h.label.is_empty() {
                    String::new()
                } else {
                    format!("{} ", h.label)
                };
                let title = if h.title.is_empty() {
                    String::new()
                } else {
                    format!("{} — ", h.title)
                };
                let place = if h.place.is_empty() {
                    String::new()
                } else {
                    format!(" · {}", h.place)
                };
                if chosen {
                    buf.set_style(Rect::new(x, y, right - x, 1), base);
                }
                spans(
                    buf,
                    x,
                    y,
                    right,
                    &[
                        (mark, base),
                        (&label, base.fg(Color::Cyan)),
                        (&title, base.add_modifier(Modifier::BOLD)),
                        (&h.before, base),
                        (&h.matched, base.fg(Color::Yellow).add_modifier(Modifier::BOLD)),
                        (&h.after, base),
                        (&place, base.fg(Color::DarkGray)),
                    ],
                );
            }
        }
        y += 1;
    }
    if footer {
        spans(
            buf,
            x,
            r.bottom() - 1,
            right,
            &[("↑↓ choose · Enter open · Tab what · Esc close", dim)],
        );
    }
}
