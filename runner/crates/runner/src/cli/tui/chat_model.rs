//! The chat screen's model (Phase 21 C4, design agent-chat.md §9.3 and
//! chat-tools.md §13): the same picture web draws, from the same events.
//! `ChatState` + [`ChatState::apply`] mirror web's `lib/chat-view.ts`
//! (what happened), [`timeline`] and [`tool_line`] mirror `lib/chat-tools.ts`
//! (how it is drawn: the verb table, states, groups of similar calls,
//! helpers, turn summaries and folding, output previews). Pure, no I/O.
//!
//! Every string here came from an agent or a program it ran. Nothing is
//! drawn from it before [`safe_line`] / [`safe_text`]: control sequences go
//! (the runner's own `strip_controls`), and a character that reorders the
//! line or has no width is written out (`⟨U+202E⟩`).

use crate::chat::mask::strip_controls;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use tmuxedo_protocol::chat::{Attachment, ChatBody, ChatEvent, Diff, NeedsItem, PlanEntry};

/// A group shows its latest five calls (§5).
pub const GROUP_WINDOW: usize = 5;
/// An open panel shows this many lines of output before "more" (§7).
pub const PREVIEW_LINES: usize = 20;
/// Helpers nest at most this deep; deeper calls stay where they arrived.
pub const MAX_DEPTH: usize = 16;
/// The runner's cap for a subject; a line is cut there too.
pub const MAX_SUBJECT: usize = 300;

// ── safe text ───────────────────────────────────────────────────────────

/// A character that changes how text reads without being seen.
fn hidden(c: char) -> bool {
    let c = c as u32;
    c == 0x061c
        || (0x200b..=0x200f).contains(&c)
        || (0x202a..=0x202e).contains(&c)
        || (0x2060..=0x2069).contains(&c)
        || c == 0xfeff
}

fn push_safe(out: &mut String, c: char) {
    if c == '\t' {
        out.push(' ');
    } else if hidden(c) {
        out.push_str(&format!("⟨U+{:04X}⟩", c as u32));
    } else if !c.is_control() {
        out.push(c);
    }
}

/// Agent text as lines a terminal only prints: escape sequences and control
/// characters gone, tabs as spaces, hidden characters written out.
pub fn safe_text(text: &str) -> Vec<String> {
    let clean = strip_controls(text);
    clean
        .split('\n')
        .map(|l| {
            let mut out = String::with_capacity(l.len());
            l.chars().for_each(|c| push_safe(&mut out, c));
            out
        })
        .collect()
}

/// Agent text as one safe line (web's `oneLine`): a line break shows as `⏎`,
/// cut at `max` characters with `…`.
pub fn safe_line(text: &str, max: usize) -> String {
    let clean = strip_controls(text);
    let mut out = String::new();
    for (n, c) in clean.chars().enumerate() {
        if n >= max {
            out.push('…');
            break;
        }
        if c == '\n' {
            out.push('⏎');
        } else {
            push_safe(&mut out, c);
        }
    }
    out
}

/// A path cut in the middle so the file name survives: `src/…/auth.ts`.
pub fn cut_middle(path: &str, max: usize) -> String {
    let chars: Vec<char> = path.chars().collect();
    if chars.len() <= max {
        return path.to_string();
    }
    if max < 2 {
        return "…".into();
    }
    // A trailing slash belongs to the name (a folder).
    let trimmed = path.strip_suffix('/').unwrap_or(path);
    let name: Vec<char> = match trimmed.rfind('/') {
        Some(i) => path[i..].chars().collect(),
        None => vec![],
    };
    if name.is_empty() || name.len() + 2 > max {
        return format!("…{}", chars[chars.len() - (max - 1)..].iter().collect::<String>());
    }
    format!(
        "{}…{}",
        chars[..max - name.len() - 1].iter().collect::<String>(),
        name.iter().collect::<String>()
    )
}

/// "4 s", "1 min 12 s", "2 h 5 min"; empty under a second.
pub fn format_duration(ms: Option<u64>) -> String {
    let Some(ms) = ms.filter(|m| *m >= 1000) else {
        return String::new();
    };
    let s = ms / 1000;
    if s < 60 {
        format!("{s} s")
    } else if s < 3600 {
        format!("{} min {} s", s / 60, s % 60)
    } else {
        format!("{} h {} min", s / 3600, (s % 3600) / 60)
    }
}

/// "512 B", "16 KB", "2.3 MB".
pub fn format_bytes(n: u64) -> String {
    let f = n as f64;
    if n < 1024 {
        format!("{n} B")
    } else if n < 1024 * 1024 {
        format!("{} KB", (f / 1024.0).round())
    } else if n < 1024 * 1024 * 1024 {
        format!("{:.1} MB", f / (1024.0 * 1024.0))
    } else {
        format!("{:.1} GB", f / (1024.0 * 1024.0 * 1024.0))
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn ms_of(at: &str) -> Option<u64> {
    crate::chat::log::parse_iso_ms(at)
}

fn between(from: Option<&str>, to: &str) -> Option<u64> {
    Some(ms_of(to)?.saturating_sub(ms_of(from?)?))
}

/// `p` relative to `cwd` when it is inside it, else as given.
pub fn relative_to(cwd: &str, p: &str) -> String {
    if cwd.is_empty() || !p.starts_with('/') {
        return p.strip_prefix("./").unwrap_or(p).to_string();
    }
    let root = if cwd.ends_with('/') {
        cwd.to_string()
    } else {
        format!("{cwd}/")
    };
    p.strip_prefix(&root).unwrap_or(p).to_string()
}

/// Lines in `b` not in `a` and the other way round ("+12 −3").
pub fn line_delta(a: &str, b: &str) -> (usize, usize) {
    fn count(s: &str) -> HashMap<&str, usize> {
        let mut m: HashMap<&str, usize> = HashMap::new();
        s.split('\n').for_each(|l| *m.entry(l).or_default() += 1);
        m
    }
    let (ca, cb) = (count(a), count(b));
    let added = cb.iter().map(|(l, n)| n.saturating_sub(*ca.get(l).unwrap_or(&0))).sum();
    let removed = if a.is_empty() {
        0
    } else {
        ca.iter().map(|(l, n)| n.saturating_sub(*cb.get(l).unwrap_or(&0))).sum()
    };
    (added, removed)
}

// ── the record (chat-view.ts) ───────────────────────────────────────────

const KINDS: &[&str] = &[
    "read", "edit", "delete", "move", "search", "execute", "think", "fetch", "other",
];
const VARIANTS: &[&str] = &[
    "list",
    "create",
    "glob",
    "web_search",
    "mcp",
    "agent",
    "plan",
    "question",
];

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ToolView {
    pub tool_id: String,
    pub kind: String,
    pub title: String,
    pub input: Value,
    /// running · completed · failed · denied · cancelled (or one from the future).
    pub status: String,
    pub output: Option<String>,
    pub diff: Option<Diff>,
    pub summary: Option<String>,
    pub auto: bool,
    pub agent_path: Option<String>,
    pub name: Option<String>,
    pub server: Option<String>,
    pub subject: Option<String>,
    pub variant: Option<String>,
    pub started_at: Option<String>,
    pub duration_ms: Option<u64>,
    pub queued: bool,
    /// Still running when its turn ended or the chat closed: nothing will end it.
    pub stopped: bool,
    pub exit_code: Option<i64>,
    pub output_bytes: Option<u64>,
    pub output_kept: Option<String>,
    pub attachments: Vec<Attachment>,
    pub result_count: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TurnRec {
    pub id: String,
    pub start_seq: u64,
    pub started_at: String,
    pub end_seq: Option<u64>,
    /// completed · interrupted · failed; None when the end was not a `turn.done`.
    pub status: Option<String>,
    pub error: Option<String>,
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    User {
        seq: u64,
        text: String,
        by: Option<String>,
        queued: bool,
    },
    Assistant {
        seq: u64,
        message_id: String,
        text: String,
        agent_path: Option<String>,
    },
    Thought {
        seq: u64,
        text: String,
        duration_ms: Option<u64>,
        redacted: bool,
        agent_path: Option<String>,
    },
    Tool {
        seq: u64,
        tool_id: String,
    },
    Need {
        seq: u64,
        id: String,
        agent_path: Option<String>,
    },
    Plan {
        seq: u64,
        entries: Vec<PlanEntry>,
        agent_path: Option<String>,
    },
    Notice {
        seq: u64,
        code: String,
        text: String,
    },
    Turn {
        seq: u64,
        status: String,
        error: Option<String>,
    },
    Handoff {
        seq: u64,
        to: String,
        pane_id: Option<String>,
    },
    Mode {
        seq: u64,
        mode: String,
    },
    Closed {
        seq: u64,
        reason: String,
    },
    Gap {
        seq: u64,
    },
}

impl Item {
    pub fn seq(&self) -> u64 {
        match self {
            Item::User { seq, .. }
            | Item::Assistant { seq, .. }
            | Item::Thought { seq, .. }
            | Item::Tool { seq, .. }
            | Item::Need { seq, .. }
            | Item::Plan { seq, .. }
            | Item::Notice { seq, .. }
            | Item::Turn { seq, .. }
            | Item::Handoff { seq, .. }
            | Item::Mode { seq, .. }
            | Item::Closed { seq, .. }
            | Item::Gap { seq } => *seq,
        }
    }
    fn agent_path(&self) -> Option<&str> {
        match self {
            Item::Assistant { agent_path, .. }
            | Item::Thought { agent_path, .. }
            | Item::Need { agent_path, .. }
            | Item::Plan { agent_path, .. } => agent_path.as_deref(),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Resolution {
    pub outcome: String,
    pub by: Option<String>,
    pub response: Value,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChatState {
    pub session_id: String,
    pub last_seq: u64,
    pub plugin: String,
    pub cwd: String,
    pub mode: String,
    pub items: Vec<Item>,
    pub tools: HashMap<String, ToolView>,
    pub turns: Vec<TurnRec>,
    pub needs: HashMap<String, NeedsItem>,
    pub resolved: HashMap<String, Resolution>,
    /// Open decisions, oldest first.
    pub open: Vec<String>,
    /// The message streaming now (deltas), not yet durable.
    pub streaming: Option<(String, String)>,
    pub working: bool,
    /// `Some(pane)` while the session is in a terminal.
    pub in_terminal: Option<String>,
    pub closed: bool,
}

impl ChatState {
    pub fn new(session_id: &str) -> Self {
        ChatState {
            session_id: session_id.into(),
            mode: "ask_first".into(),
            ..Default::default()
        }
    }

    fn open_turn(&mut self) -> Option<&mut TurnRec> {
        self.turns.last_mut().filter(|t| t.end_seq.is_none())
    }

    /// A thought or a call with no turn running: its start was never seen.
    fn activity(&mut self, seq: u64, at: &str) {
        if self.open_turn().is_none() {
            self.turns.push(TurnRec {
                id: format!("t{seq}"),
                start_seq: seq.saturating_sub(1),
                started_at: at.into(),
                end_seq: None,
                status: None,
                error: None,
                duration_ms: None,
            });
        }
    }

    /// Calls still running after seq `after` will never end: stopped at `at`.
    fn stop_running(&mut self, after: u64, at: &str) {
        for it in self.items.iter().rev() {
            if it.seq() <= after {
                break;
            }
            if let Item::Tool { tool_id, .. } = it {
                if let Some(t) = self.tools.get_mut(tool_id) {
                    if t.status == "running" && !t.stopped {
                        t.stopped = true;
                        if let Some(ms) = between(t.started_at.as_deref(), at) {
                            t.duration_ms = Some(ms);
                        }
                    }
                }
            }
        }
    }

    /// Apply one event (web's `applyEvent`). A duplicate or another
    /// session's event changes nothing. `gap`: events before it were lost.
    pub fn apply(&mut self, ev: &ChatEvent, gap: bool) {
        if ev.session_id != self.session_id {
            return;
        }
        if let ChatBody::MessageDelta { message_id, text } = &ev.body {
            match &mut self.streaming {
                Some((id, t)) if id == message_id => t.push_str(text),
                _ => {
                    if self
                        .items
                        .iter()
                        .any(|i| matches!(i, Item::Assistant { message_id: m, .. } if m == message_id))
                    {
                        return;
                    }
                    self.streaming = Some((message_id.clone(), text.clone()));
                }
            }
            self.working = true;
            return;
        }
        if ev.seq <= self.last_seq {
            return;
        }
        let first = self.last_seq == 0;
        self.last_seq = ev.seq;
        let seq = ev.seq;
        let at = ev.at.as_str();
        let path = ev.agent_path.clone();
        if gap && first {
            self.items.push(Item::Gap { seq });
        }
        match &ev.body {
            ChatBody::SessionOpened { plugin, cwd, mode, .. } => {
                self.plugin = plugin.clone();
                self.cwd = cwd.clone();
                self.mode = mode.clone();
                self.closed = false;
                self.in_terminal = None;
            }
            ChatBody::MessageUser { text, by, queued, .. } => self.items.push(Item::User {
                seq,
                text: text.clone(),
                by: by.clone(),
                queued: *queued,
            }),
            ChatBody::MessageDone { message_id, text } => {
                self.items.push(Item::Assistant {
                    seq,
                    message_id: message_id.clone(),
                    text: text.clone(),
                    agent_path: path,
                });
                if self.streaming.as_ref().is_some_and(|(id, _)| id == message_id) {
                    self.streaming = None;
                }
            }
            ChatBody::ThoughtDone {
                text,
                duration_ms,
                redacted,
            } => {
                self.activity(seq, at);
                self.items.push(Item::Thought {
                    seq,
                    text: text.clone(),
                    duration_ms: *duration_ms,
                    redacted: *redacted,
                    agent_path: path,
                });
            }
            ChatBody::ToolStarted {
                tool_id,
                kind,
                title,
                input,
                name,
                server,
                subject,
                variant,
                ..
            } => {
                self.activity(seq, at);
                let kind = if KINDS.contains(&kind.as_str()) {
                    kind.clone()
                } else {
                    "other".into()
                };
                self.tools.insert(
                    tool_id.clone(),
                    ToolView {
                        tool_id: tool_id.clone(),
                        kind,
                        title: title.clone(),
                        input: input.clone(),
                        status: "running".into(),
                        auto: self.mode == "auto_approve",
                        agent_path: path,
                        name: name.clone().filter(|s| !s.is_empty()),
                        server: server.clone().filter(|s| !s.is_empty()),
                        subject: subject.clone().filter(|s| !s.is_empty()),
                        variant: variant.clone().filter(|v| VARIANTS.contains(&v.as_str())),
                        started_at: Some(at.into()),
                        ..Default::default()
                    },
                );
                self.items.push(Item::Tool {
                    seq,
                    tool_id: tool_id.clone(),
                });
            }
            ChatBody::ToolUpdated {
                tool_id,
                status,
                output,
                diff,
            } => {
                if let Some(t) = self.tools.get_mut(tool_id) {
                    if output.is_some() {
                        t.output = output.clone();
                    }
                    if diff.is_some() {
                        t.diff = diff.clone();
                    }
                    t.queued = t.status == "running" && !t.stopped && status == "pending";
                }
            }
            ChatBody::ToolDone {
                tool_id,
                status,
                summary,
                output,
                exit_code,
                duration_ms,
                output_bytes,
                output_kept,
                attachments,
                result_count,
            } => {
                let known = self.tools.contains_key(tool_id);
                if !known {
                    self.activity(seq, at);
                }
                let t = self.tools.entry(tool_id.clone()).or_insert_with(|| ToolView {
                    tool_id: tool_id.clone(),
                    kind: "other".into(),
                    title: summary.clone().unwrap_or_else(|| "A tool".into()),
                    status: "running".into(),
                    ..Default::default()
                });
                t.status = status.clone();
                if summary.is_some() {
                    t.summary = summary.clone();
                }
                if output.is_some() {
                    t.output = output.clone();
                }
                t.duration_ms = duration_ms.or_else(|| between(t.started_at.as_deref(), at));
                if exit_code.is_some() {
                    t.exit_code = *exit_code;
                }
                t.output_bytes = *output_bytes;
                t.output_kept = output_kept.clone().filter(|k| k == "head" || k == "tail");
                t.attachments = attachments.clone();
                t.result_count = *result_count;
                t.stopped = false;
                t.queued = false;
                if !known {
                    self.items.push(Item::Tool {
                        seq,
                        tool_id: tool_id.clone(),
                    });
                }
            }
            ChatBody::PlanUpdated { entries } => self.items.push(Item::Plan {
                seq,
                entries: entries.clone(),
                agent_path: path,
            }),
            ChatBody::NeedsOpened { item } => {
                self.needs.insert(item.id.clone(), item.clone());
                self.open.retain(|x| x != &item.id);
                self.open.push(item.id.clone());
                self.items.push(Item::Need {
                    seq,
                    id: item.id.clone(),
                    agent_path: path,
                });
            }
            ChatBody::NeedsResolved {
                id,
                outcome,
                by,
                response,
            } => {
                self.resolved.insert(
                    id.clone(),
                    Resolution {
                        outcome: outcome.clone(),
                        by: by.clone(),
                        response: response.clone(),
                    },
                );
                self.open.retain(|x| x != id);
            }
            ChatBody::TurnStarted {} => {
                self.working = true;
                if let Some(t) = self.open_turn() {
                    t.end_seq = Some(seq - 1);
                }
                self.turns.push(TurnRec {
                    id: format!("t{seq}"),
                    start_seq: seq,
                    started_at: at.into(),
                    end_seq: None,
                    status: None,
                    error: None,
                    duration_ms: None,
                });
                if let Some(Item::User { queued, .. }) = self
                    .items
                    .iter_mut()
                    .find(|x| matches!(x, Item::User { queued: true, .. }))
                {
                    *queued = false;
                }
            }
            ChatBody::TurnDone { status, error, usage } => {
                self.working = false;
                self.streaming = None;
                let start = self.open_turn().map(|t| t.start_seq);
                if status != "completed" {
                    self.stop_running(start.unwrap_or(0), at);
                }
                let reported = usage.as_ref().and_then(|u| u["durationMs"].as_u64());
                if let Some(t) = self.open_turn() {
                    t.duration_ms = reported.or_else(|| between(Some(&t.started_at), at));
                    t.end_seq = Some(seq);
                    t.status = Some(status.clone());
                    t.error = error.clone();
                }
                if status != "completed" {
                    self.items.push(Item::Turn {
                        seq,
                        status: status.clone(),
                        error: error.clone(),
                    });
                }
            }
            ChatBody::SessionMode { mode, .. } => {
                if &self.mode != mode {
                    self.items.push(Item::Mode {
                        seq,
                        mode: mode.clone(),
                    });
                }
                self.mode = mode.clone();
            }
            ChatBody::SessionHandoff { to, pane_id } => {
                self.items.push(Item::Handoff {
                    seq,
                    to: to.clone(),
                    pane_id: pane_id.clone(),
                });
                if to == "terminal" {
                    self.in_terminal = Some(pane_id.clone().unwrap_or_default());
                    self.working = false;
                    self.streaming = None;
                } else {
                    self.in_terminal = None;
                }
            }
            ChatBody::SessionClosed { reason } => {
                self.closed = true;
                self.working = false;
                self.streaming = None;
                self.stop_running(0, at);
                if let Some(t) = self.open_turn() {
                    t.end_seq = Some(seq);
                }
                self.items.push(Item::Closed {
                    seq,
                    reason: reason.clone(),
                });
            }
            ChatBody::Notice { code, text, .. } => self.items.push(Item::Notice {
                seq,
                code: code.clone(),
                text: text.clone(),
            }),
            ChatBody::MessageDelta { .. } | ChatBody::FilesChanged { .. } | ChatBody::Usage { .. } => {}
        }
    }

    /// The chat in one word: the header's state.
    pub fn status_word(&self) -> &'static str {
        if self.closed {
            "closed"
        } else if self.in_terminal.is_some() {
            "in a terminal"
        } else if !self.open.is_empty() {
            "needs you"
        } else if self.working || self.streaming.is_some() {
            "working"
        } else {
            "idle"
        }
    }
}

/// One line for an answered (or gone) decision: "✓ Allowed once · you, web".
pub fn resolution_line(item: &NeedsItem, r: &Resolution) -> String {
    let title = safe_line(&item.title, 120);
    match r.outcome.as_str() {
        "expired" if r.by.as_deref() == Some("runner restart") => {
            return format!("Not answered: the machine restarted · {title}")
        }
        "expired" => return format!("Not answered in time · {title}"),
        "cancelled" => return format!("Cancelled · {title}"),
        _ => {}
    }
    let kind = r.response["kind"].as_str().unwrap_or("");
    let word = match kind {
        "allow_once" => "Allowed once",
        "allow_session" => "Allowed for this chat",
        "deny" if item.kind == "question" => "Skipped",
        "deny" => "Denied",
        "approve_plan" => "Plan approved",
        "keep_planning" => "Kept planning",
        _ => "Answered",
    };
    let told = if r.response["message"].as_str().is_some_and(|m| !m.is_empty())
        && (kind == "deny" || kind == "keep_planning")
    {
        " (with a note)"
    } else {
        ""
    };
    let by =
        r.by.as_deref()
            .map(|b| format!(" · {}", safe_line(b, 40)))
            .unwrap_or_default();
    format!("{word}{told} · {title}{by}")
}

// ── the line (chat-tools.ts) ────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolState {
    Waiting,
    Queued,
    Running,
    Done,
    Failed,
    Denied,
    Stopped,
}

impl ToolState {
    pub fn active(self) -> bool {
        matches!(self, ToolState::Waiting | ToolState::Queued | ToolState::Running)
    }
    /// Never hidden behind "earlier" in a group.
    fn loud(self) -> bool {
        matches!(
            self,
            ToolState::Waiting | ToolState::Failed | ToolState::Denied | ToolState::Stopped
        )
    }
    /// The shape (never colour alone, §4).
    pub fn glyph(self) -> &'static str {
        match self {
            ToolState::Waiting => "■",
            ToolState::Queued => "◷",
            ToolState::Running => "●",
            ToolState::Done => "✓",
            ToolState::Failed => "✕",
            ToolState::Denied => "⊘",
            ToolState::Stopped => "■",
        }
    }
    pub fn word(self) -> &'static str {
        match self {
            ToolState::Waiting => "needs you",
            ToolState::Failed => "failed",
            ToolState::Denied => "not run",
            ToolState::Stopped => "stopped",
            _ => "",
        }
    }
}

/// A value as text with its keys in order (two inputs compared whatever their key order).
fn canon(v: &Value) -> String {
    match v {
        Value::Object(o) => {
            let m: BTreeMap<&String, String> = o.iter().map(|(k, v)| (k, canon(v))).collect();
            format!(
                "{{{}}}",
                m.iter()
                    .map(|(k, v)| format!("{k:?}:{v}"))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        }
        Value::Array(a) => format!("[{}]", a.iter().map(canon).collect::<Vec<_>>().join(",")),
        v => v.to_string(),
    }
}

/// Which open decision is about which call: tool id → need id (web's `waitingCalls`).
pub fn waiting_calls(s: &ChatState) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for id in &s.open {
        let Some(item) = s.needs.get(id).filter(|i| i.kind == "permission") else {
            continue;
        };
        let Some(tool) = &item.tool else { continue };
        let input = canon(item.input.as_ref().unwrap_or(&Value::Null));
        let hits: Vec<&ToolView> = s
            .tools
            .values()
            .filter(|t| t.status == "running" && !t.stopped && t.name.as_deref() == Some(tool))
            .filter(|t| !out.contains_key(&t.tool_id) && canon(&t.input) == input)
            .collect();
        if hits.len() == 1 {
            out.insert(hits[0].tool_id.clone(), id.clone());
        }
    }
    out
}

pub fn tool_state(t: &ToolView, waiting: &HashMap<String, String>) -> ToolState {
    match t.status.as_str() {
        "failed" => ToolState::Failed,
        "denied" => ToolState::Denied,
        "cancelled" => ToolState::Stopped,
        "running" if t.stopped => ToolState::Stopped,
        "running" if waiting.contains_key(&t.tool_id) => ToolState::Waiting,
        "running" if t.queued => ToolState::Queued,
        "running" => ToolState::Running,
        _ => ToolState::Done,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    Code,
    Path,
    Address,
    Quote,
    Mcp,
    Helper,
    Name,
}

/// The verb table (§3): present while running, past when finished.
fn verb_for(t: &ToolView) -> Option<(&'static str, &'static str, Shape)> {
    let v = t.variant.as_deref();
    if t.kind == "think" || matches!(v, Some("plan" | "question")) {
        return None;
    }
    Some(match (t.kind.as_str(), v) {
        (_, Some("mcp")) => ("Using", "Used", Shape::Mcp),
        (_, Some("agent")) => ("Helper", "Helper", Shape::Helper),
        ("execute", _) => ("Running", "Ran", Shape::Code),
        ("read", Some("list")) => ("Listing", "Listed", Shape::Path),
        ("read", _) => ("Reading", "Read", Shape::Path),
        ("edit", Some("create")) => ("Creating", "Created", Shape::Path),
        ("edit", _) => ("Editing", "Edited", Shape::Path),
        ("delete", _) => ("Deleting", "Deleted", Shape::Path),
        ("move", _) => ("Moving", "Moved", Shape::Code),
        ("search", Some("glob")) => ("Looking for", "Looked for", Shape::Code),
        ("search", _) => ("Searching for", "Searched for", Shape::Code),
        ("fetch", Some("web_search")) => ("Searching the web for", "Searched the web for", Shape::Quote),
        ("fetch", _) => ("Fetching", "Fetched", Shape::Address),
        _ => ("Using", "Used", Shape::Name),
    })
}

/// A call the plan or question card shows: no line of its own.
pub fn is_card_tool(t: &ToolView) -> bool {
    t.kind == "think" || matches!(t.variant.as_deref(), Some("plan" | "question"))
}

/// What a helper's line says about the calls inside it (§8).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HelperFacts {
    pub steps: usize,
    pub failed: usize,
    /// The step it is on, while it runs: its one live line.
    pub current: Option<String>,
    pub needs_you: bool,
}

/// One call's summary line, already safe to draw.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolLine {
    pub state: ToolState,
    /// tmuxedo's verb (drawn in its own place) or empty when the line is the runner's title.
    pub verb: String,
    /// The object, the agent's text made safe: `object`, then `host` in
    /// bold (an address's domain, never cut; an MCP server), then `after`.
    pub object: String,
    pub host: Option<String>,
    pub after: String,
    pub facts: Vec<String>,
    pub word: String,
    pub struck: bool,
    pub auto: bool,
    pub duration_ms: Option<u64>,
}

impl ToolLine {
    pub fn text(&self) -> String {
        let mut s = self.verb.clone();
        if !s.is_empty() && !self.object.is_empty() {
            s.push(' ');
        }
        s.push_str(&self.object);
        s.push_str(self.host.as_deref().unwrap_or(""));
        s.push_str(&self.after);
        s
    }
}

/// The object of a call: (before the bold part, the bold part, after it).
/// Only an address (its domain) and an MCP call (its server) have a bold part.
type Object = (String, Option<String>, String);

fn object_of(s: &ChatState, t: &ToolView, shape: Shape) -> Option<Object> {
    let subject = t
        .subject
        .as_deref()
        .map(|x| safe_line(x, MAX_SUBJECT))
        .unwrap_or_default();
    let name = t.name.as_deref().map(|x| safe_line(x, 64)).unwrap_or_default();
    let plain = |o: String| Some((o, None, String::new()));
    match shape {
        Shape::Helper if subject.is_empty() => plain(String::new()),
        Shape::Helper => plain(format!("· {subject}")),
        Shape::Mcp if name.is_empty() => None,
        Shape::Mcp => match t.server.as_deref().map(|x| safe_line(x, 64)).filter(|x| !x.is_empty()) {
            Some(server) => Some((String::new(), Some(server), format!(" · {name}"))),
            None => plain(name),
        },
        Shape::Name if name.is_empty() && subject.is_empty() => None,
        Shape::Name => plain(if name.is_empty() { subject } else { name }),
        _ if subject.is_empty() => None,
        Shape::Path => plain(safe_line(
            &relative_to(&s.cwd, t.subject.as_deref().unwrap_or("")),
            MAX_SUBJECT,
        )),
        Shape::Quote => plain(format!("\"{subject}\"")),
        Shape::Address => {
            let (scheme, r) = match subject.split_once("://") {
                Some((sc, r)) => (Some(sc), r),
                None => (None, subject.as_str()),
            };
            let end = r.find(['/', '?', '#']).unwrap_or(r.len());
            let host_all = &r[..end];
            // What follows the last at-sign is the real host (a user part before it is not the domain).
            let host = &host_all[host_all.rfind('@').map(|i| i + 1).unwrap_or(0)..];
            if host.is_empty() {
                return plain(subject.clone());
            }
            let pre = match scheme {
                Some(sc) if !sc.eq_ignore_ascii_case("https") => format!("{sc}://"),
                _ => String::new(),
            };
            Some((pre, Some(host.to_string()), r[end..].to_string()))
        }
        Shape::Code => plain(subject),
    }
}

fn kind_facts(
    s: &ChatState,
    t: &ToolView,
    shape: Shape,
    state: ToolState,
    inside: Option<&HelperFacts>,
) -> Vec<String> {
    let mut out = vec![];
    let time = if state.active() {
        String::new()
    } else {
        format_duration(t.duration_ms)
    };
    if shape == Shape::Helper {
        if let Some(h) = inside {
            if h.steps > 0 {
                out.push(plural(h.steps, "step", "steps"));
            }
            if h.failed > 0 {
                out.push(format!("{} failed", h.failed));
            }
        }
        if !time.is_empty() {
            out.push(time);
        }
        return out;
    }
    if shape == Shape::Mcp {
        if let Some((k, v)) = t
            .input
            .as_object()
            .and_then(|o| o.iter().find(|(_, v)| v.as_str().is_some_and(|x| !x.is_empty())))
        {
            out.push(format!(
                "{}: {}",
                safe_line(k, 24),
                safe_line(v.as_str().unwrap_or(""), 40)
            ));
        }
        return out;
    }
    match t.kind.as_str() {
        "execute" => {
            if let Some(c) = t.exit_code.filter(|c| *c != 0) {
                out.push(format!("exit {c}"));
            }
            if !time.is_empty() {
                out.push(time);
            }
        }
        "read" if t.variant.as_deref() != Some("list") => {
            let (from, len) = (t.input["offset"].as_i64(), t.input["limit"].as_i64());
            match (from, len) {
                (Some(f), Some(l)) => out.push(format!("lines {f}–{}", f + l.max(1) - 1)),
                (Some(f), None) => out.push(format!("from line {f}")),
                (None, Some(l)) => out.push(format!("lines 1–{}", l.max(1))),
                _ => {}
            }
        }
        "edit" => {
            if let Some(d) = &t.diff {
                let (a, r) = line_delta(&d.old_text, &d.new_text);
                out.push(if t.variant.as_deref() == Some("create") {
                    format!("+{a}")
                } else {
                    format!("+{a} −{r}")
                });
            }
        }
        "search" => {
            if let Some(p) = t.input["path"].as_str().filter(|p| !p.is_empty()) {
                out.push(format!("in {}", safe_line(&relative_to(&s.cwd, p), 60)));
            }
            if let (Some(n), false) = (t.result_count, state.active()) {
                let n = n as usize;
                out.push(if t.variant.as_deref() == Some("glob") {
                    plural(n, "file", "files")
                } else {
                    plural(n, "match", "matches")
                });
            }
        }
        _ => {}
    }
    out
}

/// The first line of what a failed call said, unless it only repeats the exit code.
fn error_fact(t: &ToolView) -> String {
    let from = t
        .summary
        .clone()
        .or_else(|| if t.kind == "execute" { None } else { t.output.clone() })
        .unwrap_or_default();
    let first = from.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    let lower = first.to_ascii_lowercase();
    if t.exit_code.is_some()
        && lower
            .strip_prefix("exit code ")
            .is_some_and(|n| n.trim_start_matches('-').parse::<u64>().is_ok())
    {
        return String::new();
    }
    safe_line(first, 120)
}

/// The summary line of one call (§3, §4). `now`: ms since the epoch, for a running call's time.
pub fn tool_line(
    s: &ChatState,
    t: &ToolView,
    waiting: &HashMap<String, String>,
    now: Option<u64>,
    inside: Option<&HelperFacts>,
) -> ToolLine {
    let state = tool_state(t, waiting);
    let done = matches!(state, ToolState::Done | ToolState::Failed);
    let verb = verb_for(t);
    let object = verb.and_then(|(_, _, shape)| object_of(s, t, shape));
    let need = (state == ToolState::Waiting)
        .then(|| waiting.get(&t.tool_id).and_then(|id| s.needs.get(id)))
        .flatten();
    let titled = |t: &str| (safe_line(&t.replace('`', ""), MAX_SUBJECT), None, String::new());
    let (verb_s, (object_s, host, after)) = match (need, verb, object) {
        (Some(n), _, _) => (String::new(), titled(&n.title)),
        (None, Some((now_v, done_v, _)), Some(o)) => ((if done { done_v } else { now_v }).to_string(), o),
        _ => (String::new(), titled(&t.title)),
    };
    let mut facts = verb
        .map(|(_, _, shape)| kind_facts(s, t, shape, state, inside))
        .unwrap_or_default();
    if state == ToolState::Failed {
        let e = error_fact(t);
        if !e.is_empty() {
            facts.insert(0, e);
        }
    }
    let known = ["running", "completed", "failed", "denied", "cancelled"].contains(&t.status.as_str());
    let started = t.started_at.as_deref().and_then(ms_of);
    let duration_ms = if state.active() {
        now.zip(started).map(|(n, s)| n.saturating_sub(s))
    } else {
        t.duration_ms
    };
    ToolLine {
        state,
        verb: verb_s,
        object: object_s,
        host,
        after,
        facts,
        word: if known {
            state.word().into()
        } else {
            safe_line(&t.status, 24)
        },
        struck: state == ToolState::Denied,
        auto: t.auto && state == ToolState::Done,
        duration_ms,
    }
}

// ── nodes, groups, helpers, turns ───────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    /// Anything but a tool call.
    Item {
        key: String,
        depth: usize,
        item: Item,
    },
    Tool(ToolNode),
    Group(GroupNode),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolNode {
    pub key: String,
    pub depth: usize,
    pub tool_id: String,
    pub line: ToolLine,
    /// Shown by the plan or question card: no line.
    pub card: bool,
    /// Opens by itself: the call its turn failed on, a helper with an open
    /// decision, an edit or command under "expanded".
    pub open: bool,
    pub children: Vec<Node>,
    pub helper: Option<HelperFacts>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GroupNode {
    pub key: String,
    pub depth: usize,
    pub title: String,
    pub calls: Vec<ToolNode>,
    /// The latest five and every loud one, oldest first (indexes into `calls`).
    pub shown: Vec<usize>,
    pub earlier: usize,
    pub failed: usize,
}

impl Node {
    /// Thinking and tool calls: what a closed turn hides.
    pub fn is_activity(&self) -> bool {
        matches!(
            self,
            Node::Tool(_)
                | Node::Group(_)
                | Node::Item {
                    item: Item::Thought { .. },
                    ..
                }
        )
    }
}

fn family(n: &Node, s: &ChatState) -> Option<&'static str> {
    let Node::Tool(t) = n else { return None };
    let tv = s.tools.get(&t.tool_id)?;
    if t.card || t.helper.is_some() || matches!(tv.variant.as_deref(), Some("mcp" | "agent")) {
        return None;
    }
    match tv.kind.as_str() {
        "read" => Some("read"),
        "search" => Some("search"),
        _ => None,
    }
}

/// "Read 3 files", "Listed 2 folders", "Searched 4 times".
pub fn group_title(s: &ChatState, fam: &str, calls: &[ToolNode]) -> String {
    let now = calls.iter().any(|c| c.line.state.active());
    if fam == "search" {
        return format!(
            "{} {}",
            if now { "Searching" } else { "Searched" },
            plural(calls.len(), "time", "times")
        );
    }
    let folders = calls
        .iter()
        .filter(|c| {
            s.tools
                .get(&c.tool_id)
                .is_some_and(|t| t.variant.as_deref() == Some("list"))
        })
        .count();
    let files = calls.len() - folders;
    let (f, d) = (plural(files, "file", "files"), plural(folders, "folder", "folders"));
    let (r, l) = if now {
        ("Reading", "listing")
    } else {
        ("Read", "listed")
    };
    if folders == 0 {
        format!("{r} {f}")
    } else if files == 0 {
        format!("{} {d}", if now { "Listing" } else { "Listed" })
    } else {
        format!("{r} {f} and {l} {d}")
    }
}

fn grouped(s: &ChatState, nodes: Vec<Node>) -> Vec<Node> {
    let mut out = vec![];
    let mut run: Vec<ToolNode> = vec![];
    let mut fam: Option<&'static str> = None;
    let flush = |run: &mut Vec<ToolNode>, fam: Option<&'static str>, out: &mut Vec<Node>| {
        if run.len() >= 2 {
            let calls = std::mem::take(run);
            let n = calls.len();
            let shown: Vec<usize> = (0..n)
                .filter(|&i| i + GROUP_WINDOW >= n || calls[i].line.state.loud())
                .collect();
            out.push(Node::Group(GroupNode {
                key: format!("g{}", calls[0].key),
                depth: calls[0].depth,
                title: group_title(s, fam.unwrap_or("read"), &calls),
                earlier: n - shown.len(),
                failed: calls.iter().filter(|c| c.line.state == ToolState::Failed).count(),
                shown,
                calls,
            }));
        } else {
            out.extend(run.drain(..).map(Node::Tool));
        }
    };
    for n in nodes {
        let f = family(&n, s);
        if f.is_none() || f != fam {
            flush(&mut run, fam, &mut out);
        }
        fam = f;
        match n {
            Node::Tool(t) if f.is_some() => run.push(t),
            n => out.push(n),
        }
    }
    flush(&mut run, fam, &mut out);
    out
}

/// Every call under these nodes, at every depth, oldest first.
pub fn all_calls(nodes: &[Node]) -> Vec<&ToolNode> {
    let mut out = vec![];
    fn walk<'a>(ns: &'a [Node], out: &mut Vec<&'a ToolNode>) {
        for n in ns {
            match n {
                Node::Group(g) => g.calls.iter().for_each(|c| {
                    out.push(c);
                    walk(&c.children, out);
                }),
                Node::Tool(t) => {
                    out.push(t);
                    walk(&t.children, out);
                }
                Node::Item { .. } => {}
            }
        }
    }
    walk(nodes, &mut out);
    out
}

fn open_need(s: &ChatState, nodes: &[Node]) -> bool {
    nodes.iter().any(|n| match n {
        Node::Item {
            item: Item::Need { id, .. },
            ..
        } => s.open.contains(id),
        Node::Item { .. } => false,
        Node::Group(g) => g
            .calls
            .iter()
            .any(|c| c.line.state == ToolState::Waiting || open_need(s, &c.children)),
        Node::Tool(t) => t.line.state == ToolState::Waiting || open_need(s, &t.children),
    })
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Detail {
    #[default]
    Summary,
    Expanded,
}

pub struct ViewOptions<'a> {
    pub now: Option<u64>,
    pub detail: Detail,
    /// Turn ids the person opened or closed by hand.
    pub turn_open: &'a HashMap<String, bool>,
}

struct Build<'a> {
    s: &'a ChatState,
    kids: HashMap<String, Vec<Item>>,
    waiting: HashMap<String, String>,
    culprits: HashSet<String>,
    now: Option<u64>,
    detail: Detail,
}

fn nodes(b: &Build, items: &[Item], depth: usize) -> Vec<Node> {
    let mut out = vec![];
    for it in items {
        let Item::Tool { seq, tool_id } = it else {
            out.push(Node::Item {
                key: format!("i{}", it.seq()),
                depth,
                item: it.clone(),
            });
            continue;
        };
        let tool = &b.s.tools[tool_id];
        let children = nodes(b, b.kids.get(tool_id).map(Vec::as_slice).unwrap_or(&[]), depth + 1);
        let helper = (tool.variant.as_deref() == Some("agent") || !children.is_empty()).then(|| {
            let inner: Vec<&ToolNode> = all_calls(&children).into_iter().filter(|c| !c.card).collect();
            let live = inner.iter().rev().find(|c| c.line.state.active());
            HelperFacts {
                steps: inner.len(),
                failed: inner.iter().filter(|c| c.line.state == ToolState::Failed).count(),
                current: live
                    .filter(|_| tool.status == "running" && !tool.stopped)
                    .map(|c| c.line.text()),
                needs_you: open_need(b.s, &children),
            }
        });
        let line = tool_line(b.s, tool, &b.waiting, b.now, helper.as_ref());
        let open = match &helper {
            Some(h) => h.needs_you || all_calls(&children).iter().any(|c| c.open),
            None => {
                b.culprits.contains(tool_id)
                    || (b.detail == Detail::Expanded
                        && ((tool.kind == "edit" && tool.diff.is_some())
                            || (tool.kind == "execute" && tool.output.is_some())))
            }
        };
        out.push(Node::Tool(ToolNode {
            key: format!("c{tool_id}"),
            depth,
            tool_id: tool_id.clone(),
            card: is_card_tool(tool),
            line,
            open,
            children,
            helper,
        }));
        let _ = seq;
    }
    grouped(b.s, out)
}

#[derive(Debug, Clone, PartialEq)]
pub struct TurnSummary {
    pub tools: usize,
    pub failed: usize,
    pub duration_ms: Option<u64>,
    /// "3 tools · 12 s".
    pub short: String,
    /// "Ran 2 commands, edited 3 files, read 7 · 48 s".
    pub long: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TurnView {
    pub id: String,
    /// running · completed · interrupted · failed.
    pub status: String,
    pub error: Option<String>,
    pub latest: bool,
    /// Folded to its summary line: only nodes that are not activity are drawn.
    pub closed: bool,
    pub summary: Option<TurnSummary>,
    pub nodes: Vec<Node>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    Node(Node),
    Turn(TurnView),
}

pub fn turn_summary(s: &ChatState, rec: &TurnRec, turn_nodes: &[Node], now: Option<u64>) -> Option<TurnSummary> {
    let all: Vec<&ToolNode> = all_calls(turn_nodes).into_iter().filter(|c| !c.card).collect();
    if all.is_empty() {
        return None;
    }
    let mut n: HashMap<&str, usize> = HashMap::new();
    let (mut edited, mut read): (HashSet<String>, HashSet<String>) = Default::default();
    for c in &all {
        let Some(t) = s.tools.get(&c.tool_id) else { continue };
        let k = if c.helper.is_some() || t.variant.as_deref() == Some("agent") {
            "helper"
        } else if t.variant.as_deref() == Some("mcp") || t.kind == "other" || t.kind == "think" {
            "other"
        } else if t.kind == "fetch" {
            if t.variant.as_deref() == Some("web_search") {
                "web"
            } else {
                "fetch"
            }
        } else {
            t.kind.as_str()
        };
        *n.entry(k).or_default() += 1;
        let key = t.subject.clone().unwrap_or_else(|| t.tool_id.clone());
        match k {
            "edit" => edited.insert(key),
            "read" => read.insert(key),
            _ => false,
        };
    }
    let g = |k: &str| *n.get(k).unwrap_or(&0);
    let mut parts: Vec<String> = vec![];
    let mut add = |c: usize, said: String| {
        if c > 0 {
            parts.push(said)
        }
    };
    let (cmds, del, mov) = (g("execute"), g("delete"), g("move"));
    add(cmds, format!("ran {}", plural(cmds, "command", "commands")));
    add(
        edited.len(),
        format!("edited {}", plural(edited.len(), "file", "files")),
    );
    add(del, format!("deleted {}", plural(del, "file", "files")));
    add(mov, format!("moved {}", plural(mov, "file", "files")));
    let noun_said = !edited.is_empty() || del > 0 || mov > 0;
    add(
        read.len(),
        if noun_said {
            format!("read {}", read.len())
        } else {
            format!("read {}", plural(read.len(), "file", "files"))
        },
    );
    add(
        g("search"),
        format!("searched {}", plural(g("search"), "time", "times")),
    );
    add(g("fetch"), format!("fetched {}", plural(g("fetch"), "page", "pages")));
    add(
        g("web"),
        format!("searched the web {}", plural(g("web"), "time", "times")),
    );
    add(
        g("helper"),
        format!("used {}", plural(g("helper"), "helper", "helpers")),
    );
    let others = g("other");
    let any = !parts.is_empty();
    if others > 0 {
        parts.push(if any {
            format!("used {}", plural(others, "other tool", "other tools"))
        } else {
            format!("used {}", plural(others, "tool", "tools"))
        });
    }
    let duration_ms = if rec.end_seq.is_some() {
        rec.duration_ms
    } else {
        now.zip(ms_of(&rec.started_at)).map(|(n, s)| n.saturating_sub(s))
    };
    let time = format_duration(duration_ms);
    let tail = if time.is_empty() {
        String::new()
    } else {
        format!(" · {time}")
    };
    let said = parts.join(", ");
    let mut cs = said.chars();
    let long = match cs.next() {
        Some(c) => format!("{}{}{tail}", c.to_uppercase(), cs.as_str()),
        None => tail.clone(),
    };
    Some(TurnSummary {
        tools: all.len(),
        failed: all.iter().filter(|c| c.line.state == ToolState::Failed).count(),
        duration_ms,
        short: format!("{}{tail}", plural(all.len(), "tool", "tools")),
        long,
    })
}

/// The call each failed turn failed on: its last failed call.
fn culprits(s: &ChatState) -> HashSet<String> {
    let mut out = HashSet::new();
    for t in s.turns.iter().filter(|t| t.status.as_deref() == Some("failed")) {
        let end = t.end_seq.unwrap_or(u64::MAX);
        let last = s.items.iter().rev().find_map(|it| match it {
            Item::Tool { seq, tool_id }
                if *seq > t.start_seq && *seq <= end && s.tools.get(tool_id).is_some_and(|x| x.status == "failed") =>
            {
                Some(tool_id.clone())
            }
            _ => None,
        });
        out.extend(last);
    }
    out
}

/// Who is inside whom: an item is inside a helper when its `agentPath`
/// names a call that arrived before it (and not deeper than `MAX_DEPTH`).
fn tree(s: &ChatState) -> (Vec<Item>, HashMap<String, Vec<Item>>) {
    let mut roots = vec![];
    let mut kids: HashMap<String, Vec<Item>> = HashMap::new();
    let mut depth: HashMap<String, usize> = HashMap::new();
    for it in &s.items {
        if let Item::Tool { tool_id, .. } = it {
            if depth.contains_key(tool_id) || !s.tools.contains_key(tool_id) {
                continue;
            }
        }
        let parent = match it {
            Item::Tool { tool_id, .. } => s.tools[tool_id].agent_path.as_deref(),
            other => other.agent_path(),
        };
        let d = parent.and_then(|p| depth.get(p)).copied();
        let inside = d.is_some_and(|d| d < MAX_DEPTH);
        if inside {
            kids.entry(parent.unwrap_or_default().to_string())
                .or_default()
                .push(it.clone());
        } else {
            roots.push(it.clone());
        }
        if let Item::Tool { tool_id, .. } = it {
            depth.insert(tool_id.clone(), if inside { d.unwrap_or(0) + 1 } else { 0 });
        }
    }
    (roots, kids)
}

/// The chat as it is drawn (web's `timeline`): what is outside any turn as
/// plain nodes, and each turn with its nodes, summary and folding. A turn
/// folds when it ended, used tools and the person has sent a message since
/// (or a later turn started); never with an open decision, never under
/// "expanded"; the person's own choice (`turn_open`) wins.
pub fn timeline(s: &ChatState, o: &ViewOptions) -> Vec<Entry> {
    let (roots, kids) = tree(s);
    let b = Build {
        s,
        kids,
        waiting: waiting_calls(s),
        culprits: culprits(s),
        now: o.now,
        detail: o.detail.clone(),
    };
    let last_user = s
        .items
        .iter()
        .filter(|&i| matches!(i, Item::User { .. }))
        .map(|i| i.seq())
        .max()
        .unwrap_or(0);
    let mut out = vec![];
    let mut ti = 0;
    let mut batch: Vec<Item> = vec![];
    let mut batch_turn: Option<usize> = None;
    let flush = |batch: &mut Vec<Item>, bt: Option<usize>, out: &mut Vec<Entry>| {
        if batch.is_empty() {
            return;
        }
        let ns = nodes(&b, batch, 0);
        batch.clear();
        let Some(i) = bt else {
            out.extend(ns.into_iter().map(Entry::Node));
            return;
        };
        let rec = &s.turns[i];
        let summary = turn_summary(s, rec, &ns, o.now);
        let ended = rec.end_seq.is_some();
        let latest = i + 1 == s.turns.len();
        let auto = ended
            && summary.is_some()
            && o.detail != Detail::Expanded
            && (!latest || last_user > rec.end_seq.unwrap_or(0))
            && !open_need(s, &ns);
        let closed = summary.is_some() && o.turn_open.get(&rec.id).map(|open| !open).unwrap_or(auto);
        out.push(Entry::Turn(TurnView {
            id: rec.id.clone(),
            status: if ended {
                rec.status.clone().unwrap_or_else(|| "completed".into())
            } else {
                "running".into()
            },
            error: rec.error.clone(),
            latest,
            closed,
            summary,
            nodes: ns,
        }));
    };
    for it in roots {
        while ti < s.turns.len() && it.seq() > s.turns[ti].end_seq.unwrap_or(u64::MAX) {
            ti += 1;
        }
        let mine = (ti < s.turns.len() && it.seq() > s.turns[ti].start_seq).then_some(ti);
        if mine != batch_turn {
            flush(&mut batch, batch_turn, &mut out);
            batch_turn = mine;
        }
        batch.push(it);
    }
    flush(&mut batch, batch_turn, &mut out);
    out
}

// ── output ──────────────────────────────────────────────────────────────

/// What an open panel shows of a call's output: the end of a command's, the
/// start of anything else (§7), and what the event says about the rest.
#[derive(Debug, Clone, PartialEq)]
pub struct OutputPreview {
    pub lines: Vec<String>,
    pub from_tail: bool,
    pub total: usize,
    pub more: usize,
    /// "Showing the last 16 KB of 2.3 MB", or empty.
    pub note: String,
    /// The call ended and gave nothing.
    pub empty: bool,
    pub attachments: Vec<String>,
}

pub fn output_preview(t: &ToolView, max: usize) -> OutputPreview {
    let mut all = t.output.as_deref().map(safe_text).unwrap_or_default();
    if all.len() > 1 && all.last().is_some_and(String::is_empty) {
        all.pop();
    }
    let from_tail = t.kind == "execute";
    let total = all.len();
    let lines = if total <= max {
        all
    } else if from_tail {
        all[total - max..].to_vec()
    } else {
        all[..max].to_vec()
    };
    let has = t.output.as_ref().map(|o| o.len() as u64).unwrap_or(0);
    let note = match (&t.output_kept, t.output_bytes) {
        (Some(k), Some(b)) if b > has => format!(
            "Showing the {} {} of {}",
            if k == "tail" { "last" } else { "first" },
            format_bytes(has),
            format_bytes(b)
        ),
        _ => String::new(),
    };
    let attachments: Vec<String> = t
        .attachments
        .iter()
        .map(|a| {
            let mime = safe_line(&a.mime, 64);
            let size = format_bytes(a.bytes);
            match mime.split_once('/') {
                Some(("image", sub)) => format!("1 image ({}, {size})", sub.to_uppercase()),
                _ => format!("1 file ({mime}, {size})"),
            }
        })
        .collect();
    OutputPreview {
        more: total - lines.len(),
        empty: t.status != "running" && t.output.as_deref().is_none_or(str::is_empty) && attachments.is_empty(),
        lines,
        from_tail,
        total,
        note,
        attachments,
    }
}
