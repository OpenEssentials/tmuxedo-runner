//! The terminal chat screen (Phase 21 C4, design agent-chat.md §9.3,
//! chat-tools.md §13 "TUI"). One chat of the selected machine, over the
//! panes' area: its transcript, the decision that waits (pinned above the
//! composer), the composer, one line of keys.
//!
//! - **Transcript.** The agent's words without a bubble; the person's
//!   message marked `›` in the gutter. Thinking, tool calls, groups of
//!   similar calls, helpers and turn summaries are drawn from
//!   [`super::chat_model::timeline`], the same model web draws: one line per
//!   call, `▸ ✓ Ran  npm test   exit 1 · 8 s`, a state shape and, where it
//!   matters, a word (never colour alone; `NO_COLOR` draws the same shapes).
//! - **Keys.** The composer has the keyboard: Enter sends, Alt+Enter or
//!   Ctrl+J is a new line (Shift+Enter where the terminal reports it), Esc in
//!   an empty composer or Ctrl+C stops the turn. Tab moves to the transcript
//!   (`j`/`k` or arrows move, Enter/`l` opens, `h` closes, `v` shows the whole
//!   output, `y` copies it, `G` back to the latest) and to the decision when
//!   one waits: `a` allow once, `s` allow for this chat (only when offered,
//!   never for high risk), `d` deny, `1`–`9` a choice. A high-risk allow
//!   needs a fresh step-up, which only web has: the card says "answer this
//!   on the web or phone" and `a` does nothing.
//! - **Untrusted text.** Everything the agent or its tools wrote passes
//!   [`super::chat_model::safe_text`] before a cell is written: escape
//!   sequences and control characters never reach the terminal.
//!
//! Pure like `app`: keys in, [`Cmd`]s out; the app turns them into requests.

use super::chat_model::{
    self as m, output_preview, resolution_line, safe_line, safe_text, timeline, ChatState, Detail, Entry, Item, Node,
    ToolNode, ToolState, ViewOptions,
};
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use serde_json::{json, Value};
use std::cell::Cell;
use std::collections::HashMap;
use tmuxedo_protocol::chat::{ChatEvent, ChatFrame, NeedsItem};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// What the screen asks the app to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Cmd {
    Send(String),
    Interrupt,
    Respond { need: String, response: Value },
    Copy(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Composer,
    Transcript,
    Decision,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Link {
    Attaching,
    Live,
    /// The link is down: re-attached from the last seq when it returns.
    Reconnecting,
    Refused(String),
}

/// The whole output of one call, over the transcript (`v`).
#[derive(Debug, Clone, PartialEq)]
pub struct Pager {
    pub title: String,
    pub lines: Vec<String>,
    pub top: usize,
}

pub struct ChatScreen {
    pub mid: String,
    pub sid: String,
    pub state: ChatState,
    pub link: Link,
    pub focus: Focus,
    /// The composer's text and the cursor (a byte index).
    pub text: String,
    pub cursor: usize,
    /// Lines opened or closed by hand, by node key.
    pub open: HashMap<String, bool>,
    /// Turns opened or closed by hand.
    pub turn_open: HashMap<String, bool>,
    /// The selected transcript line's key.
    pub sel: Option<String>,
    /// The first transcript line shown; None follows the end.
    pub scroll: Option<usize>,
    pub pager: Option<Pager>,
    pub detail: Detail,
    pub color: bool,
    /// Decisions answered here and not yet resolved (no double answers).
    pub answered: Vec<String>,
    /// The transcript's last drawn size (width, height), for paging keys.
    pub view: Cell<(u16, u16)>,
    /// Lines in the transcript when the person scrolled up: more means "new activity".
    pub seen: Cell<usize>,
}

impl ChatScreen {
    pub fn new(mid: &str, sid: &str, color: bool) -> Self {
        ChatScreen {
            mid: mid.into(),
            sid: sid.into(),
            state: ChatState::new(sid),
            link: Link::Attaching,
            focus: Focus::Composer,
            text: String::new(),
            cursor: 0,
            open: HashMap::new(),
            turn_open: HashMap::new(),
            sel: None,
            scroll: None,
            pager: None,
            detail: Detail::Summary,
            color,
            answered: vec![],
            view: Cell::new((80, 20)),
            seen: Cell::new(0),
        }
    }

    /// The `chat_attach` frame: from the event after the last one held.
    pub fn attach_frame(&self) -> Value {
        json!({ "t": "chat_attach", "sessionId": self.sid, "from": self.state.last_seq + 1 })
    }

    /// One CHAT frame from the socket. Junk is dropped.
    pub fn on_frame(&mut self, f: &ChatFrame) {
        if f.session_id != self.sid {
            return;
        }
        let Some(ev) = std::str::from_utf8(&f.payload)
            .ok()
            .and_then(tmuxedo_protocol::chat::parse_event)
        else {
            return;
        };
        self.on_event(&ev, f.gap);
    }

    pub fn on_event(&mut self, ev: &ChatEvent, gap: bool) {
        let had = self.state.open.len();
        self.state.apply(ev, gap);
        self.answered.retain(|id| self.state.open.contains(id));
        // A decision that arrives while the composer is empty takes the keyboard.
        if self.state.open.len() > had && self.focus == Focus::Composer && self.text.is_empty() {
            self.focus = Focus::Decision;
        }
        if self.state.open.is_empty() && self.focus == Focus::Decision {
            self.focus = Focus::Composer;
        }
    }

    /// `chat_attached` from the gateway.
    pub fn on_attached(&mut self, v: &Value) {
        self.link = if v["ok"] == true {
            Link::Live
        } else {
            Link::Refused(v["code"].as_str().unwrap_or("refused").to_string())
        };
    }

    /// The decision the card shows: the oldest open one not answered here.
    pub fn decision(&self) -> Option<&NeedsItem> {
        self.state
            .open
            .iter()
            .find(|id| !self.answered.contains(id))
            .and_then(|id| self.state.needs.get(id))
    }

    fn opts(&self, now: Option<u64>) -> ViewOptions<'_> {
        ViewOptions {
            now,
            detail: self.detail.clone(),
            turn_open: &self.turn_open,
        }
    }

    // ── keys ────────────────────────────────────────────────────────────

    pub fn on_key(&mut self, k: KeyEvent, now: Option<u64>) -> Vec<Cmd> {
        let page = self.view.get().1.saturating_sub(2).max(1) as usize;
        if matches!(k.code, KeyCode::Esc | KeyCode::Char('q' | 'v')) && self.pager.take().is_some() {
            return vec![];
        }
        if let Some(p) = &mut self.pager {
            match k.code {
                KeyCode::Char('j') | KeyCode::Down => p.top += 1,
                KeyCode::Char('k') | KeyCode::Up => p.top = p.top.saturating_sub(1),
                KeyCode::PageDown | KeyCode::Char(' ') => p.top += page,
                KeyCode::PageUp | KeyCode::Char('b') => p.top = p.top.saturating_sub(page),
                KeyCode::Char('g') | KeyCode::Home => p.top = 0,
                KeyCode::Char('G') | KeyCode::End => p.top = usize::MAX,
                KeyCode::Char('y') => return vec![Cmd::Copy(p.lines.join("\n"))],
                _ => {}
            }
            let max = p.lines.len().saturating_sub(page);
            p.top = p.top.min(max);
            return vec![];
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && k.code == KeyCode::Char('c') {
            if self.state.working {
                return vec![Cmd::Interrupt];
            }
            self.text.clear();
            self.cursor = 0;
            return vec![];
        }
        if k.code == KeyCode::Tab || k.code == KeyCode::BackTab {
            self.focus = match (self.focus, self.decision().is_some()) {
                (Focus::Composer, _) => Focus::Transcript,
                (Focus::Transcript, true) => Focus::Decision,
                _ => Focus::Composer,
            };
            if self.focus == Focus::Transcript && self.sel.is_none() {
                self.sel = self.keys(now).last().cloned();
            }
            return vec![];
        }
        match self.focus {
            Focus::Composer => self.composer_key(k),
            Focus::Decision => self.decision_key(k),
            Focus::Transcript => {
                if self.decision().is_some() && matches!(k.code, KeyCode::Char('a' | 's' | 'd' | '1'..='9')) {
                    return self.decision_key(k);
                }
                self.transcript_key(k, now)
            }
        }
    }

    fn composer_key(&mut self, k: KeyEvent) -> Vec<Cmd> {
        let alt = k.modifiers.intersects(KeyModifiers::ALT | KeyModifiers::SHIFT);
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Enter if alt => self.insert('\n'),
            KeyCode::Char('j') if ctrl => self.insert('\n'),
            KeyCode::Enter => {
                let t = self.text.trim();
                if t.is_empty() {
                    return vec![];
                }
                let t = t.to_string();
                self.text.clear();
                self.cursor = 0;
                // Sending is a moment the person caused: back to the end.
                self.scroll = None;
                return vec![Cmd::Send(t)];
            }
            KeyCode::Esc if self.text.is_empty() && self.state.working => return vec![Cmd::Interrupt],
            KeyCode::Esc => {}
            KeyCode::Char('u') if ctrl => {
                self.text.clear();
                self.cursor = 0;
            }
            KeyCode::Char(c) if !ctrl => self.insert(c),
            KeyCode::Backspace => {
                if let Some(c) = self.text[..self.cursor].chars().next_back() {
                    self.cursor -= c.len_utf8();
                    self.text.remove(self.cursor);
                }
            }
            KeyCode::Delete => {
                if self.cursor < self.text.len() {
                    self.text.remove(self.cursor);
                }
            }
            KeyCode::Left => {
                if let Some(c) = self.text[..self.cursor].chars().next_back() {
                    self.cursor -= c.len_utf8();
                }
            }
            KeyCode::Right => {
                if let Some(c) = self.text[self.cursor..].chars().next() {
                    self.cursor += c.len_utf8();
                }
            }
            KeyCode::Home => self.cursor = self.text[..self.cursor].rfind('\n').map(|i| i + 1).unwrap_or(0),
            KeyCode::End => {
                self.cursor += self.text[self.cursor..]
                    .find('\n')
                    .unwrap_or(self.text.len() - self.cursor)
            }
            KeyCode::Up if !self.text[..self.cursor].contains('\n') => {
                self.focus = Focus::Transcript;
            }
            _ => {}
        }
        vec![]
    }

    /// Typed or pasted text: control characters are not kept (a paste's line breaks are).
    pub fn insert(&mut self, c: char) {
        let c = if c == '\r' { '\n' } else { c };
        if c != '\n' && c.is_control() {
            return;
        }
        self.text.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    pub fn paste(&mut self, s: &str) {
        let s = s.replace("\r\n", "\n");
        s.chars().for_each(|c| self.insert(c));
        self.focus = Focus::Composer;
    }

    fn decision_key(&mut self, k: KeyEvent) -> Vec<Cmd> {
        let Some(item) = self.decision().cloned() else {
            self.focus = Focus::Composer;
            return vec![];
        };
        let high = item.risk == "high";
        let has = |o: &str| item.options.iter().any(|x| x == o);
        let response = match (item.kind.as_str(), k.code) {
            (_, KeyCode::Esc) => {
                self.focus = Focus::Composer;
                return vec![];
            }
            ("plan", KeyCode::Char('a')) if has("approve_plan") => json!({ "kind": "approve_plan" }),
            ("plan", KeyCode::Char('d')) if has("keep_planning") => json!({ "kind": "keep_planning" }),
            ("question", KeyCode::Char(c @ '1'..='9')) => {
                let [q] = item.questions.as_slice() else { return vec![] };
                if q.multi || !has("answers") {
                    return vec![];
                }
                let Some(o) = q.options.get(c as usize - '1' as usize) else {
                    return vec![];
                };
                json!({ "kind": "answers", "answers": { q.text.clone(): o.label } })
            }
            ("question", KeyCode::Char('d')) if has("deny") => json!({ "kind": "deny" }),
            // High risk: allowing needs a fresh step-up, which only web has.
            ("permission", KeyCode::Char('a')) if has("allow_once") && !high => json!({ "kind": "allow_once" }),
            ("permission", KeyCode::Char('s')) if has("allow_session") && !high => json!({ "kind": "allow_session" }),
            ("permission", KeyCode::Char('d')) if has("deny") => json!({ "kind": "deny" }),
            _ => return vec![],
        };
        self.answered.push(item.id.clone());
        if self.decision().is_none() {
            self.focus = Focus::Composer;
        }
        vec![Cmd::Respond {
            need: item.id,
            response,
        }]
    }

    fn transcript_key(&mut self, k: KeyEvent, now: Option<u64>) -> Vec<Cmd> {
        let lines = self.lines(self.view.get().0.max(10), now);
        let keys: Vec<&String> = lines.iter().filter_map(|l| l.key.as_ref()).collect();
        let at = self.sel.as_ref().and_then(|s| keys.iter().position(|k| *k == s));
        let page = self.view.get().1.max(1) as usize;
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Esc | KeyCode::Char('i') => self.focus = Focus::Composer,
            KeyCode::Char('j') | KeyCode::Down => {
                match at {
                    Some(i) if i + 1 < keys.len() => self.sel = Some(keys[i + 1].clone()),
                    Some(_) => self.focus = Focus::Composer,
                    None => self.sel = keys.last().map(|k| (*k).clone()),
                }
                self.reveal(&lines);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.sel = match at {
                    Some(i) => Some(keys[i.saturating_sub(1)].clone()),
                    None => keys.last().map(|k| (*k).clone()),
                };
                self.reveal(&lines);
            }
            KeyCode::Char('g') | KeyCode::Home => {
                self.sel = keys.first().map(|k| (*k).clone());
                self.scroll = Some(0);
            }
            KeyCode::Char('G') | KeyCode::End => {
                self.sel = keys.last().map(|k| (*k).clone());
                self.scroll = None;
            }
            KeyCode::PageUp | KeyCode::Char('b') => {
                let top = self.top(lines.len());
                self.scroll = Some(top.saturating_sub(page));
            }
            KeyCode::Char('u') if ctrl => {
                let top = self.top(lines.len());
                self.scroll = Some(top.saturating_sub(page / 2));
            }
            KeyCode::PageDown | KeyCode::Char(' ') => self.page_down(lines.len(), page),
            KeyCode::Char('d') if ctrl => self.page_down(lines.len(), page / 2),
            KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => {
                if let Some(s) = self.sel.clone() {
                    self.toggle(&s, Some(true), &lines);
                }
            }
            KeyCode::Char('h') | KeyCode::Left => {
                if let Some(s) = self.sel.clone() {
                    self.toggle(&s, Some(false), &lines);
                }
            }
            KeyCode::Char('v') => {
                if let Some(p) = self.sel.as_ref().and_then(|s| self.pager_for(s)) {
                    self.pager = Some(p);
                }
            }
            KeyCode::Char('y') => {
                if let Some(p) = self.sel.as_ref().and_then(|s| self.pager_for(s)) {
                    return vec![Cmd::Copy(p.lines.join("\n"))];
                }
            }
            _ => {}
        }
        vec![]
    }

    fn page_down(&mut self, total: usize, by: usize) {
        let h = self.view.get().1 as usize;
        let top = self.top(total) + by.max(1);
        self.scroll = if top + h >= total { None } else { Some(top) };
    }

    /// The first line shown, for `total` lines.
    fn top(&self, total: usize) -> usize {
        let h = self.view.get().1 as usize;
        let end = total.saturating_sub(h);
        self.scroll.map(|t| t.min(end)).unwrap_or(end)
    }

    /// Scroll so the selected line is on screen.
    fn reveal(&mut self, lines: &[DLine]) {
        let Some(i) = self
            .sel
            .as_ref()
            .and_then(|s| lines.iter().position(|l| l.key.as_ref() == Some(s)))
        else {
            return;
        };
        let h = (self.view.get().1 as usize).max(1);
        let top = self.top(lines.len());
        if i < top {
            self.scroll = Some(i);
        } else if i >= top + h {
            let t = i + 1 - h;
            self.scroll = if t + h >= lines.len() { None } else { Some(t) };
        }
    }

    /// Open, close (or flip with `None`) the line with key `key`.
    fn toggle(&mut self, key: &str, want: Option<bool>, lines: &[DLine]) {
        let Some(l) = lines.iter().find(|l| l.key.as_deref() == Some(key)) else {
            return;
        };
        let Some(was) = l.open else { return };
        let to = want.unwrap_or(!was);
        if let Some(t) = key.strip_prefix("turn:") {
            self.turn_open.insert(t.to_string(), to);
        } else {
            self.open.insert(key.to_string(), to);
        }
    }

    /// The whole of what a call or thought holds, for the pager and copying.
    fn pager_for(&self, key: &str) -> Option<Pager> {
        if let Some(id) = key.strip_prefix('c') {
            let t = self.state.tools.get(id)?;
            let mut lines = input_lines(&self.state, t);
            if let Some(d) = &t.diff {
                lines.extend(diff_lines(&self.state.cwd, d));
            }
            let out = output_preview(t, usize::MAX);
            if !out.note.is_empty() {
                lines.push(out.note);
            }
            lines.extend(out.lines);
            lines.extend(out.attachments);
            let wait = m::waiting_calls(&self.state);
            let title = m::tool_line(&self.state, t, &wait, None, None).text();
            return Some(Pager { title, lines, top: 0 });
        }
        let seq: u64 = key.strip_prefix('i')?.parse().ok()?;
        self.state.items.iter().find_map(|it| match it {
            Item::Thought { seq: s, text, .. } if *s == seq => Some(Pager {
                title: "Thought".into(),
                lines: safe_text(text),
                top: 0,
            }),
            Item::Assistant { seq: s, text, .. } if *s == seq => Some(Pager {
                title: "Message".into(),
                lines: safe_text(text),
                top: 0,
            }),
            _ => None,
        })
    }

    /// The selectable keys, in order (the transcript at its last width).
    fn keys(&self, now: Option<u64>) -> Vec<String> {
        self.lines(self.view.get().0.max(10), now)
            .into_iter()
            .filter_map(|l| l.key)
            .collect()
    }

    // ── lines ───────────────────────────────────────────────────────────

    /// The transcript as display lines at width `w`.
    pub fn lines(&self, w: u16, now: Option<u64>) -> Vec<DLine> {
        let mut b = Lines {
            s: self,
            w: w as usize,
            out: vec![],
        };
        let entries = timeline(&self.state, &self.opts(now));
        for e in &entries {
            match e {
                Entry::Node(n) => b.node(n, 0),
                Entry::Turn(t) => b.turn(t),
            }
        }
        if let Some((_, text)) = &self.state.streaming {
            b.gap();
            for l in safe_text(text) {
                b.wrapped(&l, "", Style::default(), None);
            }
        }
        b.out
    }
}

/// One drawn line of the transcript: its spans, and the key of the thing it
/// is when it can be selected (and whether that is open).
#[derive(Debug, Clone, PartialEq)]
pub struct DLine {
    pub spans: Vec<(String, Style)>,
    pub key: Option<String>,
    pub open: Option<bool>,
}

struct Lines<'a> {
    s: &'a ChatScreen,
    w: usize,
    out: Vec<DLine>,
}

fn width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// Cut `s` to `max` cells, with `…` when cut.
pub fn fit(s: &str, max: usize) -> String {
    if width(s) <= max {
        return s.to_string();
    }
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = c.width().unwrap_or(0);
        if w + cw + 1 > max {
            break;
        }
        out.push(c);
        w += cw;
    }
    if max > 0 {
        out.push('…');
    }
    out
}

/// Word-wrap one safe line to `w` cells (long words are broken).
pub fn wrap(s: &str, w: usize) -> Vec<String> {
    let w = w.max(1);
    if width(s) <= w {
        return vec![s.to_string()];
    }
    let mut out = vec![];
    let mut cur = String::new();
    let mut cw = 0;
    for word in s.split_inclusive(' ') {
        let ww = width(word);
        if cw + width(word.trim_end()) > w && cw > 0 {
            out.push(cur.trim_end().to_string());
            cur.clear();
            cw = 0;
        }
        if ww > w {
            for c in word.chars() {
                let x = c.width().unwrap_or(0);
                if cw + x > w {
                    out.push(std::mem::take(&mut cur));
                    cw = 0;
                }
                cur.push(c);
                cw += x;
            }
        } else {
            cur.push_str(word);
            cw += ww;
        }
    }
    if !cur.is_empty() || out.is_empty() {
        out.push(cur.trim_end().to_string());
    }
    out
}

impl Lines<'_> {
    fn st(&self, c: Color) -> Style {
        if self.s.color {
            Style::default().fg(c)
        } else {
            Style::default()
        }
    }
    fn dim(&self) -> Style {
        if self.s.color {
            Style::default().fg(Color::DarkGray)
        } else {
            Style::default()
        }
    }
    fn push(&mut self, spans: Vec<(String, Style)>, key: Option<String>, open: Option<bool>) {
        self.out.push(DLine { spans, key, open });
    }
    fn gap(&mut self) {
        if self.out.last().is_some_and(|l| !l.spans.is_empty()) {
            self.push(vec![], None, None);
        }
    }
    /// One safe line, wrapped, each piece after `prefix`.
    fn wrapped(&mut self, text: &str, prefix: &str, style: Style, key: Option<String>) {
        let pre_w = width(prefix);
        for (i, l) in wrap(text, self.w.saturating_sub(pre_w)).into_iter().enumerate() {
            let p = (prefix.to_string(), self.dim());
            self.push(vec![p, (l, style)], if i == 0 { key.clone() } else { None }, None);
        }
    }
    fn indent(depth: usize) -> String {
        match depth {
            0 => String::new(),
            1 | 2 => "┃ ".repeat(depth),
            d => format!("┃ ┃{d}"),
        }
    }
    fn is_open(&self, key: &str, default: bool) -> bool {
        self.s.open.get(key).copied().unwrap_or(default)
    }

    fn turn(&mut self, t: &m::TurnView) {
        if let Some(sum) = &t.summary {
            let key = format!("turn:{}", t.id);
            let mark = if t.closed { "▸ " } else { "▾ " };
            let failed = if sum.failed > 0 {
                format!(" · {} failed", sum.failed)
            } else {
                String::new()
            };
            let room = self.w.saturating_sub(2 + width(&failed));
            let said = if width(&sum.long) <= room {
                &sum.long
            } else {
                &sum.short
            };
            let mut spans = vec![(mark.to_string(), self.dim()), (fit(said, room), self.dim())];
            if !failed.is_empty() {
                spans.push((failed, self.st(Color::Red)));
            }
            self.gap();
            self.push(spans, Some(key), Some(!t.closed));
        }
        for n in &t.nodes {
            if t.closed && n.is_activity() {
                continue;
            }
            self.node(n, 0);
        }
    }

    fn node(&mut self, n: &Node, depth: usize) {
        let ind = Self::indent(depth);
        match n {
            Node::Tool(t) => self.tool(t, depth),
            Node::Group(g) => {
                let open = self.is_open(&g.key, true);
                let mut spans = vec![
                    (ind.clone(), self.dim()),
                    ((if open { "▾ " } else { "▸ " }).into(), self.dim()),
                    (g.title.clone(), Style::default()),
                ];
                if g.failed > 0 {
                    spans.push((format!(" · ✕ {} failed", g.failed), self.st(Color::Red)));
                }
                self.push(spans, Some(g.key.clone()), Some(open));
                if !open {
                    return;
                }
                let all = self.is_open(&format!("{}:earlier", g.key), false);
                if g.earlier > 0 && !all {
                    let k = format!("{}:earlier", g.key);
                    let line = format!("{ind}  … {} earlier", g.earlier);
                    self.push(vec![(line, self.dim())], Some(k), Some(false));
                }
                for (i, c) in g.calls.iter().enumerate() {
                    if all || g.shown.contains(&i) {
                        self.tool_in_group(c, depth);
                    }
                }
            }
            Node::Item { key, item, .. } => self.item(key, item, &ind),
        }
    }

    fn tool_in_group(&mut self, c: &ToolNode, depth: usize) {
        let mut c = c.clone();
        c.depth = depth;
        self.tool_with(&c, &format!("{}  ", Self::indent(depth)));
    }

    fn tool(&mut self, t: &ToolNode, depth: usize) {
        if t.card {
            return;
        }
        self.tool_with(t, &Self::indent(depth));
    }

    fn tool_with(&mut self, t: &ToolNode, ind: &str) {
        let open = self.is_open(&t.key, t.open);
        let line = &t.line;
        let (glyph, gstyle) = self.state_style(line.state);
        let mut left = vec![
            (ind.to_string(), self.dim()),
            ((if open { "▾ " } else { "▸ " }).into(), self.dim()),
            (format!("{glyph} "), gstyle),
        ];
        let verb_style = if line.state == ToolState::Failed {
            self.st(Color::Red)
        } else {
            Style::default()
        };
        let struck = if line.struck {
            Modifier::CROSSED_OUT
        } else {
            Modifier::empty()
        };
        if !line.verb.is_empty() {
            left.push((
                format!("{} ", line.verb),
                verb_style.add_modifier(Modifier::BOLD | struck),
            ));
        }
        // The facts at the end side: the time of a running call, the kind's facts, the state word.
        let mut right: Vec<String> = line.facts.clone();
        if line.state.active() {
            let t = m::format_duration(line.duration_ms);
            if !t.is_empty() {
                right.push(t);
            }
        }
        if let Some(h) = &t.helper {
            if h.needs_you {
                right.push("needs you".into());
            }
        }
        if !line.word.is_empty() {
            right.push(line.word.clone());
        }
        if line.auto {
            right.push("auto".into());
        }
        let mut right_s = right.join(" · ");
        let used: usize = left.iter().map(|(s, _)| width(s)).sum();
        let room = self.w.saturating_sub(used);
        if width(&right_s) + 12 > room {
            right_s = fit(&right_s, room.saturating_sub(12).max(room / 3));
        }
        let obj_room = room.saturating_sub(width(&right_s) + usize::from(!right_s.is_empty()));
        let host = line.host.clone().unwrap_or_default();
        let whole = format!("{}{}{}", line.object, host, line.after);
        let obj_style = verb_style.add_modifier(struck);
        if width(&whole) <= obj_room {
            left.push((line.object.clone(), obj_style));
            if !host.is_empty() {
                left.push((host, obj_style.add_modifier(Modifier::BOLD)));
            }
            left.push((line.after.clone(), obj_style));
        } else if !host.is_empty() && width(&host) <= obj_room {
            // The domain is never the part that is cut.
            left.push((host.clone(), obj_style.add_modifier(Modifier::BOLD)));
            left.push((fit(&line.after, obj_room - width(&host)), obj_style));
        } else {
            left.push((m::cut_middle(&whole, obj_room.max(2)), obj_style));
        }
        let used: usize = left.iter().map(|(s, _)| width(s)).sum();
        if !right_s.is_empty() {
            let pad = self.w.saturating_sub(used + width(&right_s)).max(1);
            left.push((" ".repeat(pad), Style::default()));
            let rs = if line.state == ToolState::Failed {
                self.st(Color::Red)
            } else {
                self.dim()
            };
            left.push((right_s, rs));
        }
        self.push(left, Some(t.key.clone()), Some(open));
        if let Some(h) = &t.helper {
            if !open {
                if let Some(cur) = &h.current {
                    let p = format!("{ind}  ┃ ");
                    self.push(
                        vec![
                            (p, self.dim()),
                            (fit(cur, self.w.saturating_sub(width(ind) + 4)), self.dim()),
                        ],
                        None,
                        None,
                    );
                }
                return;
            }
            let depth = t.depth + 1;
            for c in &t.children {
                self.node(c, depth);
            }
            return;
        }
        if open {
            self.panel(t, ind);
        }
    }

    fn state_style(&self, s: ToolState) -> (&'static str, Style) {
        let c = match s {
            ToolState::Waiting => Color::Cyan,
            ToolState::Running => Color::Yellow,
            ToolState::Failed => Color::Red,
            _ => Color::DarkGray,
        };
        (s.glyph(), self.st(c))
    }

    /// The open view of a call: what went in, then what came out (§7).
    fn panel(&mut self, t: &ToolNode, ind: &str) {
        let Some(tv) = self.s.state.tools.get(&t.tool_id) else {
            return;
        };
        let pre = format!("{ind}  │ ");
        let room = self.w.saturating_sub(width(&pre));
        // A diff names its own file.
        let input = if tv.diff.is_some() {
            vec![]
        } else {
            input_lines(&self.s.state, tv)
        };
        for l in input.into_iter().take(6) {
            for piece in wrap(&l, room) {
                self.push(vec![(pre.clone(), self.dim()), (piece, Style::default())], None, None);
            }
        }
        if let Some(d) = &tv.diff {
            let dl = diff_lines(&self.s.state.cwd, d);
            let n = dl.len();
            for l in dl.into_iter().take(m::PREVIEW_LINES) {
                let st = match l.chars().next() {
                    Some('+') => self.st(Color::Green),
                    Some('-') => self.st(Color::Red),
                    _ => self.dim(),
                };
                self.push(vec![(pre.clone(), self.dim()), (fit(&l, room), st)], None, None);
            }
            if n > m::PREVIEW_LINES {
                let more = format!("… {} more lines (v shows all)", n - m::PREVIEW_LINES);
                self.push(vec![(pre.clone(), self.dim()), (more, self.dim())], None, None);
            }
            return;
        }
        let out = output_preview(tv, m::PREVIEW_LINES);
        if !out.note.is_empty() {
            self.push(
                vec![(pre.clone(), self.dim()), (fit(&out.note, room), self.dim())],
                None,
                None,
            );
        }
        if out.from_tail && out.more > 0 {
            let more = format!("… {} earlier lines (v shows all)", out.more);
            self.push(vec![(pre.clone(), self.dim()), (more, self.dim())], None, None);
        }
        for l in &out.lines {
            self.push(
                vec![(pre.clone(), self.dim()), (fit(l, room), Style::default())],
                None,
                None,
            );
        }
        if !out.from_tail && out.more > 0 {
            let more = format!("… {} more lines (v shows all)", out.more);
            self.push(vec![(pre.clone(), self.dim()), (more, self.dim())], None, None);
        }
        for a in &out.attachments {
            self.push(vec![(pre.clone(), self.dim()), (a.clone(), self.dim())], None, None);
        }
        if out.empty {
            self.push(
                vec![(pre.clone(), self.dim()), ("No output".into(), self.dim())],
                None,
                None,
            );
        }
        if tv.kind == "execute" && tv.status != "running" {
            let mut f = vec![];
            if let Some(c) = tv.exit_code {
                f.push(format!("exit {c}"));
            }
            let d = m::format_duration(tv.duration_ms);
            if !d.is_empty() {
                f.push(d);
            }
            if !f.is_empty() {
                self.push(vec![(pre.clone(), self.dim()), (f.join(" · "), self.dim())], None, None);
            }
        }
    }

    fn item(&mut self, key: &str, item: &Item, ind: &str) {
        match item {
            Item::User { text, queued, .. } => {
                self.gap();
                let st = self.st(Color::Cyan).add_modifier(Modifier::BOLD);
                let mut first = true;
                for l in safe_text(text) {
                    for piece in wrap(&l, self.w.saturating_sub(2)) {
                        let mark = if first { "› " } else { "  " };
                        first = false;
                        self.push(vec![(mark.into(), st), (piece, self.st(Color::Cyan))], None, None);
                    }
                }
                if *queued {
                    self.push(
                        vec![("  queued: sent when this turn ends".into(), self.dim())],
                        None,
                        None,
                    );
                }
                self.gap();
            }
            Item::Assistant { text, .. } => {
                for l in safe_text(text) {
                    self.wrapped(&l, ind, Style::default(), None);
                }
            }
            Item::Thought {
                text,
                duration_ms,
                redacted,
                ..
            } => {
                let open = self.is_open(key, false);
                let d = m::format_duration(*duration_ms);
                let label = match (d.is_empty(), *redacted) {
                    (_, true) => "Thought (hidden by the agent)".to_string(),
                    (true, _) => "Thought".to_string(),
                    (false, _) => format!("Thought for {d}"),
                };
                let mark = if open { "▾ " } else { "▸ " };
                self.push(
                    vec![
                        (ind.to_string(), self.dim()),
                        (mark.into(), self.dim()),
                        (label, self.dim()),
                    ],
                    Some(key.to_string()),
                    Some(open),
                );
                if open {
                    let pre = format!("{ind}  │ ");
                    for l in safe_text(text) {
                        self.wrapped(&l, &pre, self.dim(), None);
                    }
                }
            }
            Item::Need { id, .. } => {
                let Some(n) = self.s.state.needs.get(id) else { return };
                if let Some(r) = self.s.state.resolved.get(id) {
                    let line = format!("{ind}✓ {}", resolution_line(n, r));
                    self.push(vec![(fit(&line, self.w), self.dim())], None, None);
                } else {
                    let st = self.st(Color::Cyan).add_modifier(Modifier::BOLD);
                    let line = format!("{ind}■ Needs you · {}", safe_line(&n.title, 200));
                    self.push(vec![(fit(&line, self.w), st)], None, None);
                }
            }
            Item::Plan { entries, .. } => {
                self.push(
                    vec![(format!("{ind}Plan"), Style::default().add_modifier(Modifier::BOLD))],
                    None,
                    None,
                );
                for e in entries {
                    let mark = match e.status.as_str() {
                        "done" => "✓",
                        "in_progress" => "●",
                        _ => "○",
                    };
                    let line = format!("{ind}  {mark} {}", safe_line(&e.text, 200));
                    self.push(vec![(fit(&line, self.w), Style::default())], None, None);
                }
            }
            Item::Notice { text, .. } => {
                for l in safe_text(text) {
                    self.wrapped(&l, &format!("{ind}· "), self.dim(), None);
                }
            }
            Item::Turn { status, error, .. } => {
                let (line, st) = if status == "failed" {
                    let e = error
                        .as_deref()
                        .map(|e| format!(": {}", safe_line(e, 300)))
                        .unwrap_or_default();
                    (format!("✕ The turn failed{e}"), self.st(Color::Red))
                } else {
                    ("■ Stopped".to_string(), self.dim())
                };
                self.wrapped(&line, ind, st, None);
            }
            Item::Handoff { to, .. } => {
                let line = if to == "terminal" {
                    "→ Continued in a terminal pane"
                } else {
                    "← Back in the chat"
                };
                self.push(vec![(line.into(), self.dim())], None, None);
            }
            Item::Mode { mode, .. } => {
                let line = format!("Mode: {}", mode_title(mode));
                self.push(vec![(line, self.dim())], None, None);
            }
            Item::Closed { reason, .. } => {
                let line = format!("Chat closed ({})", safe_line(reason, 60));
                self.push(vec![(line, self.dim())], None, None);
            }
            Item::Gap { .. } => {
                self.push(
                    vec![("… earlier events are no longer kept".into(), self.dim())],
                    None,
                    None,
                );
            }
            Item::Tool { .. } => {}
        }
    }
}

pub fn mode_title(mode: &str) -> String {
    match mode {
        "ask_first" => "Ask first".into(),
        "plan_first" => "Plan first".into(),
        "auto_edit" => "Auto-edit".into(),
        "auto_approve" => "Auto-approve".into(),
        m => safe_line(m, 24),
    }
}

/// What went into a call, as safe lines (§7 "In").
pub fn input_lines(s: &ChatState, t: &m::ToolView) -> Vec<String> {
    let input = &t.input;
    let str_of = |k: &str| input[k].as_str().filter(|x| !x.is_empty());
    let mut out = vec![];
    match t.kind.as_str() {
        "execute" => {
            let cmd = str_of("command")
                .map(str::to_string)
                .or_else(|| t.subject.clone())
                .unwrap_or_default();
            let lines = safe_text(&cmd);
            for (i, l) in lines.into_iter().enumerate() {
                out.push(if i == 0 { format!("$ {l}") } else { format!("  {l}") });
            }
        }
        _ if t.variant.as_deref() == Some("agent") => {
            let task = str_of("prompt")
                .or(str_of("description"))
                .map(str::to_string)
                .or_else(|| t.subject.clone());
            out.extend(task.map(|x| safe_text(&x)).unwrap_or_default());
        }
        "read" | "edit" | "delete" | "move" | "search" | "fetch" if t.variant.as_deref() != Some("mcp") => {
            if let Some(sub) = &t.subject {
                out.push(safe_line(&m::relative_to(&s.cwd, sub), 2000));
            }
        }
        _ => {
            if let Some(o) = input.as_object() {
                for (k, v) in o.iter().take(8) {
                    let v = v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string());
                    out.push(format!("{}: {}", safe_line(k, 40), safe_line(&v, 400)));
                }
            }
        }
    }
    out
}

/// A diff as `-` and `+` lines.
pub fn diff_lines(cwd: &str, d: &tmuxedo_protocol::chat::Diff) -> Vec<String> {
    let mut out = vec![];
    let path = safe_line(&m::relative_to(cwd, &d.path), 200);
    let head = match d.line {
        Some(n) => format!("{path} · line {n}"),
        None => path,
    };
    out.push(head);
    if !d.old_text.is_empty() {
        out.extend(safe_text(&d.old_text).into_iter().map(|l| format!("- {l}")));
    }
    out.extend(safe_text(&d.new_text).into_iter().map(|l| format!("+ {l}")));
    out
}

// ── drawing ─────────────────────────────────────────────────────────────

fn put(buf: &mut Buffer, x: u16, y: u16, spans: &[(String, Style)], w: u16) {
    let mut cx = x;
    let end = x + w;
    for (s, st) in spans {
        if cx >= end {
            break;
        }
        let (nx, _) = buf.set_stringn(cx, y, s, (end - cx) as usize, *st);
        cx = nx;
    }
}

fn clear(buf: &mut Buffer, r: Rect) {
    for y in r.top()..r.bottom() {
        for x in r.left()..r.right() {
            buf[(x, y)].reset();
        }
    }
}

/// Composer rows at height `h`: 1..=6, smaller on a short terminal.
fn composer_rows(text: &str, w: usize, h: u16) -> u16 {
    let n: usize = text.split('\n').map(|l| wrap(l, w.saturating_sub(2)).len()).sum();
    let cap = if h < 16 { 2 } else { 6 };
    (n.max(1) as u16).min(cap)
}

/// The decision card's lines (pinned above the composer).
fn card_lines(s: &ChatScreen, item: &NeedsItem, w: usize) -> Vec<(String, Style)> {
    let accent = if s.color {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default()
    };
    let mut out = vec![(
        fit(&format!("■ Needs you · {}", safe_line(&item.title, 300)), w),
        accent.add_modifier(Modifier::BOLD),
    )];
    if let Some(why) = item.why.as_deref().filter(|_| item.risk == "high") {
        out.push((fit(&format!("  High risk: {}", safe_line(why, 300)), w), accent));
    }
    let has = |o: &str| item.options.iter().any(|x| x == o);
    let keys = match item.kind.as_str() {
        "plan" => "a approve the plan · d keep planning".to_string(),
        "question" => match item.questions.as_slice() {
            [q] if !q.multi && has("answers") => {
                let opts: Vec<String> = q
                    .options
                    .iter()
                    .take(9)
                    .enumerate()
                    .map(|(i, o)| format!("{} {}", i + 1, safe_line(&o.label, 40)))
                    .collect();
                out.push((fit(&format!("  {}", safe_line(&q.text, 300)), w), Style::default()));
                format!("{} · d skip", opts.join(" · "))
            }
            _ => "answer this on the web or phone · d skip".to_string(),
        },
        _ if item.risk == "high" => {
            "answer this on the web or phone (it needs a fresh sign-in check) · d deny".to_string()
        }
        _ => {
            let mut k = vec![];
            if has("allow_once") {
                k.push("a allow once");
            }
            if has("allow_session") {
                k.push("s allow for this chat");
            }
            if has("deny") {
                k.push("d deny");
            }
            k.join(" · ")
        }
    };
    let focused = s.focus == Focus::Decision || s.focus == Focus::Transcript;
    let keys = if focused {
        keys
    } else {
        format!("Tab to answer: {keys}")
    };
    out.push((fit(&format!("  {keys}"), w), Style::default()));
    out
}

/// Draw the chat screen in `r` (the panes' area). Returns the composer's
/// cursor position when the composer has the keyboard.
pub fn draw(s: &ChatScreen, r: Rect, buf: &mut Buffer, now: Option<u64>) -> Option<(u16, u16)> {
    clear(buf, r);
    if r.width < 8 || r.height < 3 {
        return None;
    }
    let w = r.width as usize;
    let dim = if s.color {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default()
    };
    // Header: what this chat is and its state.
    let st = &s.state;
    let link = match &s.link {
        Link::Attaching => " · loading",
        Link::Reconnecting => " · reconnecting",
        Link::Live => "",
        Link::Refused(_) => " · not available",
    };
    let word = st.status_word();
    let glyph = match word {
        "needs you" => "■",
        "working" => "●",
        "closed" => "■",
        "in a terminal" => "→",
        _ => "○",
    };
    let right = format!("{glyph} {word}{link}");
    let plugin = if st.plugin.is_empty() {
        "chat".to_string()
    } else {
        safe_line(&st.plugin, 20)
    };
    let left = format!("Chat · {plugin} · {}", safe_line(&m::cut_middle(&st.cwd, 40), 60));
    let left = fit(&left, w.saturating_sub(width(&right) + 1));
    let hs = Style::default().add_modifier(Modifier::BOLD);
    put(buf, r.x, r.y, &[(left.clone(), hs)], r.width);
    let rx = r.x + (w.saturating_sub(width(&right))) as u16;
    if rx > r.x + width(&left) as u16 {
        let rs = if s.color && word == "needs you" {
            Style::default().fg(Color::Cyan)
        } else {
            dim
        };
        put(buf, rx, r.y, &[(right, rs)], r.right() - rx);
    }
    let mut bottom = r.bottom();
    // Keys line.
    if r.height >= 6 {
        bottom -= 1;
        let keys = hint(s);
        put(buf, r.x, bottom, &[(fit(&keys, w), dim)], r.width);
    }
    // Composer.
    let crow = composer_rows(&s.text, w, r.height);
    let ctop = bottom.saturating_sub(crow);
    let cursor = draw_composer(s, Rect::new(r.x, ctop, r.width, crow), buf);
    bottom = ctop;
    // Separator above the composer, with the refusal or the terminal's state.
    if bottom > r.y + 2 {
        bottom -= 1;
        let note = match (&s.link, &st.in_terminal, st.closed) {
            (Link::Refused(code), _, _) => format!(" this chat is not available ({}) ", safe_line(code, 40)),
            (_, Some(_), _) => " in a terminal: prefix C brings it back ".into(),
            (_, _, true) => " closed ".into(),
            _ => String::new(),
        };
        let line = format!("─{note}{}", "─".repeat(w));
        put(
            buf,
            r.x,
            bottom,
            &[(line.chars().take(w).collect::<String>(), dim)],
            r.width,
        );
    }
    // The decision card.
    if let Some(item) = s.decision() {
        let lines = card_lines(s, item, w);
        let room = bottom.saturating_sub(r.y + 2);
        let n = (lines.len() as u16).min(room);
        let top = bottom - n;
        for (i, (l, st)) in lines.iter().take(n as usize).enumerate() {
            put(buf, r.x, top + i as u16, &[(l.clone(), *st)], r.width);
        }
        bottom = top;
    }
    // Transcript.
    let t = Rect::new(r.x, r.y + 1, r.width, bottom.saturating_sub(r.y + 1));
    s.view.set((t.width, t.height));
    if let Some(p) = &s.pager {
        draw_pager(p, t, buf, dim);
        return None;
    }
    let lines = s.lines(t.width, now);
    let top = s.top(lines.len());
    let h = t.height as usize;
    if s.scroll.is_none() {
        s.seen.set(lines.len());
    }
    let newer = s.scroll.is_some() && lines.len() > s.seen.get();
    for (i, l) in lines.iter().skip(top).take(h).enumerate() {
        let y = t.y + i as u16;
        let selected = s.focus == Focus::Transcript && l.key.is_some() && l.key == s.sel;
        if selected {
            let mut spans = l.spans.clone();
            spans
                .iter_mut()
                .for_each(|(_, st)| *st = st.add_modifier(Modifier::REVERSED));
            put(buf, t.x, y, &spans, t.width);
        } else {
            put(buf, t.x, y, &l.spans, t.width);
        }
    }
    if newer && t.height > 0 {
        let pill = "↓ New activity (G)";
        put(
            buf,
            t.x,
            t.bottom() - 1,
            &[(fit(pill, w), Style::default().add_modifier(Modifier::REVERSED))],
            t.width,
        );
    }
    (s.focus == Focus::Composer).then_some(cursor).flatten()
}

fn draw_composer(s: &ChatScreen, r: Rect, buf: &mut Buffer) -> Option<(u16, u16)> {
    let w = r.width as usize;
    let mut rows: Vec<String> = vec![];
    let mut cur: Option<(usize, usize)> = None;
    let mut off = 0;
    for l in s.text.split('\n') {
        let pieces = wrap_hard(l, w.saturating_sub(2));
        let mut p_off = off;
        for (i, p) in pieces.iter().enumerate() {
            let end = p_off + p.len();
            let last = i + 1 == pieces.len();
            if cur.is_none() && s.cursor >= p_off && (s.cursor < end || (last && s.cursor <= end)) {
                cur = Some((rows.len(), width(&s.text[p_off..s.cursor])));
            }
            rows.push(p.clone());
            p_off = end;
        }
        off += l.len() + 1;
    }
    let (cy, cx) = cur.unwrap_or((rows.len().saturating_sub(1), 0));
    let h = r.height as usize;
    let first = (cy + 1).saturating_sub(h);
    let hint_style = if s.color {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default()
    };
    for (i, row) in rows.iter().skip(first).take(h).enumerate() {
        let mark = if i + first == 0 { "> " } else { "  " };
        put(
            buf,
            r.x,
            r.y + i as u16,
            &[
                (mark.into(), Style::default().add_modifier(Modifier::BOLD)),
                (row.clone(), Style::default()),
            ],
            r.width,
        );
    }
    if s.text.is_empty() {
        let ph = if s.state.in_terminal.is_some() || s.state.closed {
            "messages are off while the chat is not running here"
        } else if s.state.working {
            "a message now is sent when this turn ends · Esc stops"
        } else {
            "Message the agent"
        };
        put(
            buf,
            r.x + 2,
            r.y,
            &[(fit(ph, w.saturating_sub(2)), hint_style)],
            r.width.saturating_sub(2),
        );
    }
    Some((r.x + 2 + cx as u16, r.y + (cy - first) as u16))
}

/// Break a line at exactly `w` cells (the composer: what is typed is not reflowed).
fn wrap_hard(s: &str, w: usize) -> Vec<String> {
    let w = w.max(1);
    let mut out = vec![String::new()];
    let mut cw = 0;
    for c in s.chars() {
        let x = c.width().unwrap_or(0);
        if cw + x > w {
            out.push(String::new());
            cw = 0;
        }
        out.last_mut().expect("one").push(c);
        cw += x;
    }
    out
}

fn draw_pager(p: &Pager, r: Rect, buf: &mut Buffer, dim: Style) {
    if r.height == 0 {
        return;
    }
    let w = r.width as usize;
    put(
        buf,
        r.x,
        r.y,
        &[(fit(&p.title, w), Style::default().add_modifier(Modifier::BOLD))],
        r.width,
    );
    let h = r.height.saturating_sub(2) as usize;
    let top = p.top.min(p.lines.len().saturating_sub(h));
    for (i, l) in p.lines.iter().skip(top).take(h).enumerate() {
        put(buf, r.x, r.y + 1 + i as u16, &[(fit(l, w), Style::default())], r.width);
    }
    if r.height >= 2 {
        let foot = format!(
            "lines {}–{} of {} · j/k PgUp/PgDn g/G move · y copy · q close",
            (top + 1).min(p.lines.len()),
            (top + h).min(p.lines.len()),
            p.lines.len()
        );
        put(buf, r.x, r.bottom() - 1, &[(fit(&foot, w), dim)], r.width);
    }
}

/// The keys line, for where the keyboard is.
pub fn hint(s: &ChatScreen) -> String {
    if s.pager.is_some() {
        return "j/k move · y copy · q close".into();
    }
    match s.focus {
        Focus::Composer if s.state.working => {
            "Enter send · Alt+Enter new line · Esc stop · Tab transcript · ^b C terminal".into()
        }
        Focus::Composer => "Enter send · Alt+Enter new line · Tab transcript · ^b C terminal · ^b q panes".into(),
        Focus::Transcript => "j/k move · Enter open · h close · v view all · y copy · G latest · Esc composer".into(),
        Focus::Decision => "answer with the keys above · Esc composer".into(),
    }
}
