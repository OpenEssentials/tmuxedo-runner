//! Agent chat (docs/protocol.md "Agent chat", docs/design/agent-chat.md §3).
//!
//! A chat is an agent CLI driven in its structured mode by the runner. The
//! runner turns the CLI's stream into the **normalised chat event model**
//! below (shaped after the Agent Client Protocol, so a native ACP agent maps
//! one to one), masks every string, numbers each durable event with a
//! per-session `seq` and keeps it in the session's log on the machine.
//!
//! On the wire an event travels in a binary **CHAT frame** (kind 4): the
//! pane-frame header with the session id in the id field and the event's
//! `seq` as the offset, and the event JSON as an opaque payload. The gateway
//! reads only the header, exactly as for terminal bytes, so end-to-end
//! encryption can later wrap the payload with no gateway change.
//!
//! - durable event: `offset` = its `seq` (1, 2, 3 … per session, never reused);
//! - `message.delta` is **ephemeral** (flag bit 1): not logged, no seq of its
//!   own; `offset` = the seq of the last durable event before it, so a relay
//!   forwards it only to a viewer that has everything up to that seq;
//! - GAP (bit 0): events before this one were pruned (30-day history) or lost.

use crate::codec::{encode_raw, parse_header, EncodeError, FLAG_GAP, KIND_CHAT};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use crate::codec::FLAG_EPHEMERAL;

/// The hello feature of a runner that serves chats.
pub const FEATURE_CHAT: &str = "chat";
/// An event is at most this large after masking (bigger fields are cut).
pub const MAX_EVENT_BYTES: usize = 256 * 1024;
/// Tool output kept in an event: its head for reads, searches and fetches,
/// its tail for everything else (`tool.done` `outputKept` says which).
pub const MAX_TOOL_OUTPUT: usize = 16 * 1024;
/// A tool's `name` and `server` on `tool.started`, in characters.
pub const MAX_TOOL_NAME: usize = 64;
/// A tool's `subject` on `tool.started`, in characters (one line).
pub const MAX_TOOL_SUBJECT: usize = 300;
/// Attachments listed on one `tool.done` (their type and size, never their bytes).
pub const MAX_TOOL_ATTACHMENTS: usize = 20;
/// `tool.started` `variant`: a refinement of `kind`. A reader shows the
/// kind's default for a value it does not know.
pub const TOOL_VARIANTS: &[&str] = &[
    "list",
    "create",
    "glob",
    "web_search",
    "mcp",
    "agent",
    "plan",
    "question",
];
/// `tool.done` `status`. `denied`: a person or a rule said no and the call
/// did not run. `cancelled`: the call was still open when its turn was
/// interrupted.
pub const TOOL_STATUSES: &[&str] = &["completed", "failed", "denied", "cancelled"];
/// `tool.done` `outputKept`: which end of a longer output the event holds.
pub const OUTPUT_KEPT: &[&str] = &["head", "tail"];
/// A message sent to an agent (`chat.send` text).
pub const MAX_SEND_BYTES: usize = 64 * 1024;
/// How long a chat's events stay on the machine (D-C6).
pub const HISTORY_DAYS: u64 = 30;

/// A chat session id: 8..64 of `[A-Za-z0-9_-]`, so it fits a frame header
/// and a URL path segment untouched.
pub fn valid_session_id(s: &str) -> bool {
    (8..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// One CHAT frame (binary kind 4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatFrame {
    pub gap: bool,
    pub ephemeral: bool,
    /// 0 = live; otherwise the `chat_attach` request this replays for.
    pub req: u32,
    pub session_id: String,
    /// Durable: the event's seq. Ephemeral: the last durable seq before it.
    pub seq: u64,
    /// One event as UTF-8 JSON (one line of NDJSON).
    pub payload: Vec<u8>,
}

impl ChatFrame {
    pub fn encode(&self) -> Result<Vec<u8>, EncodeError> {
        let flags = (if self.gap { FLAG_GAP } else { 0 }) | (if self.ephemeral { FLAG_EPHEMERAL } else { 0 });
        encode_raw(KIND_CHAT, flags, self.req, &self.session_id, self.seq, &self.payload)
    }

    /// Parse a CHAT frame, or `None` for anything else. Never panics.
    pub fn decode(buf: &[u8]) -> Option<ChatFrame> {
        let (kind, flags, req, session_id, seq, start) = parse_header(buf)?;
        if kind != KIND_CHAT || flags & !(FLAG_GAP | FLAG_EPHEMERAL) != 0 {
            return None;
        }
        Some(ChatFrame {
            gap: flags & FLAG_GAP != 0,
            ephemeral: flags & FLAG_EPHEMERAL != 0,
            req,
            session_id,
            seq,
            payload: buf[start..].to_vec(),
        })
    }
}

// ── the event model ─────────────────────────────────────────────────────

/// One chat event. Every event has `seq`, `at`, `sessionId`, and optionally
/// `turnId` and `agentPath` (a sub-agent's path, outermost first).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatEvent {
    /// 0 on an ephemeral event.
    pub seq: u64,
    pub at: String,
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_path: Option<String>,
    #[serde(flatten)]
    pub body: ChatBody,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Location {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Diff {
    pub path: String,
    #[serde(default)]
    pub old_text: String,
    #[serde(default)]
    pub new_text: String,
    /// The first line of the change in the file (1-based), where the agent says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u64>,
}

/// Something other than text in a tool's result: what it is and how big,
/// never the bytes themselves.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Attachment {
    pub mime: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanEntry {
    pub text: String,
    /// `pending` | `in_progress` | `done`.
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestionOption {
    pub label: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Question {
    pub text: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub header: String,
    pub options: Vec<QuestionOption>,
    #[serde(default)]
    pub multi: bool,
}

/// A needs-you item raised by a chat (`answerVia: session`, design §3.7):
/// a permission request, a multiple-choice question or a plan to approve.
/// It is answered by id (`chat.respond`), never by keystrokes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NeedsItem {
    pub id: String,
    /// `permission` | `question` | `plan`.
    pub kind: String,
    /// `normal` | `high` (high: Allow once / Deny only, a fresh step-up).
    pub risk: String,
    /// What the person may answer: `allow_once`, `allow_session`, `deny`,
    /// `answers`, `approve_plan`, `keep_planning`.
    pub options: Vec<String>,
    /// One plain line: "Run `npm test`", "Answer 2 questions", "Approve the plan".
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub questions: Vec<Question>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub markdown: Option<String>,
    pub expires_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileChange {
    pub path: String,
    /// `created` | `modified` | `deleted`.
    pub change: String,
    /// `tool` | `watch`.
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ChatBody {
    /// First event of every session (and of every resume of it).
    #[serde(rename = "session.opened", rename_all = "camelCase")]
    SessionOpened {
        plugin: String,
        cwd: String,
        mode: String,
        /// `api_key` | `cloud` | `subscription`.
        login: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default)]
        features: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resumed_from: Option<String>,
    },
    #[serde(rename = "message.user", rename_all = "camelCase")]
    MessageUser {
        message_id: String,
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        by: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_msg_id: Option<String>,
        /// Sent while a turn ran: delivered when it ends.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        queued: bool,
    },
    /// Ephemeral (never logged): clients may drop it and wait for `message.done`.
    #[serde(rename = "message.delta", rename_all = "camelCase")]
    MessageDelta { message_id: String, text: String },
    #[serde(rename = "message.done", rename_all = "camelCase")]
    MessageDone { message_id: String, text: String },
    #[serde(rename = "thought.done", rename_all = "camelCase")]
    ThoughtDone {
        /// Empty when `redacted`.
        text: String,
        /// How long the agent thought, where its stream says.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
        /// The vendor hid this thought: there is no text to show.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        redacted: bool,
    },
    #[serde(rename = "tool.started", rename_all = "camelCase")]
    ToolStarted {
        tool_id: String,
        /// ACP kinds: read, edit, delete, move, search, execute, think, fetch, other.
        kind: String,
        title: String,
        #[serde(default)]
        input: Value,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        locations: Vec<Location>,
        /// The tool's own name ("Bash", "create_issue"), ≤ [`MAX_TOOL_NAME`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        /// The MCP server the tool belongs to.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        server: Option<String>,
        /// The object of the call (the command, the path, the pattern, the
        /// address): one line, ≤ [`MAX_TOOL_SUBJECT`], masked. The reader
        /// writes the verb.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        subject: Option<String>,
        /// One of [`TOOL_VARIANTS`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        variant: Option<String>,
    },
    #[serde(rename = "tool.updated", rename_all = "camelCase")]
    ToolUpdated {
        tool_id: String,
        status: String,
        /// The latest tail of the call's output so far: it replaces the
        /// one before it, it is not appended.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        diff: Option<Diff>,
    },
    #[serde(rename = "tool.done", rename_all = "camelCase")]
    ToolDone {
        tool_id: String,
        /// One of [`TOOL_STATUSES`].
        status: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_code: Option<i64>,
        /// The agent's own time for the call.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
        /// The size of the whole output, of which `output` is a part.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output_bytes: Option<u64>,
        /// `head` | `tail`: set when `output` is a cut of a longer one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output_kept: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<Attachment>,
        /// Matches or files found, where the agent gives a count.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result_count: Option<u64>,
    },
    #[serde(rename = "plan.updated", rename_all = "camelCase")]
    PlanUpdated { entries: Vec<PlanEntry> },
    #[serde(rename = "needs.opened", rename_all = "camelCase")]
    NeedsOpened { item: NeedsItem },
    #[serde(rename = "needs.resolved", rename_all = "camelCase")]
    NeedsResolved {
        id: String,
        /// `answered` | `expired` | `cancelled`.
        outcome: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        by: Option<String>,
        #[serde(default)]
        response: Value,
    },
    #[serde(rename = "files.changed", rename_all = "camelCase")]
    FilesChanged { files: Vec<FileChange> },
    #[serde(rename = "turn.started", rename_all = "camelCase")]
    TurnStarted {},
    #[serde(rename = "turn.done", rename_all = "camelCase")]
    TurnDone {
        /// completed, interrupted, failed.
        status: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Value>,
    },
    #[serde(rename = "usage", rename_all = "camelCase")]
    Usage { usage: Value },
    #[serde(rename = "session.mode", rename_all = "camelCase")]
    SessionMode {
        mode: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
    },
    #[serde(rename = "session.handoff", rename_all = "camelCase")]
    SessionHandoff {
        /// `terminal` | `chat`.
        to: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pane_id: Option<String>,
    },
    #[serde(rename = "session.closed", rename_all = "camelCase")]
    SessionClosed { reason: String },
    /// Something this version cannot show (CLI drift), or the runner's own
    /// note ("output was lost while the runner restarted"): a neutral row.
    #[serde(rename = "notice", rename_all = "camelCase")]
    Notice {
        code: String,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        native: Option<String>,
    },
}

impl ChatBody {
    /// The wire `type`.
    pub fn kind(&self) -> &'static str {
        match self {
            ChatBody::SessionOpened { .. } => "session.opened",
            ChatBody::MessageUser { .. } => "message.user",
            ChatBody::MessageDelta { .. } => "message.delta",
            ChatBody::MessageDone { .. } => "message.done",
            ChatBody::ThoughtDone { .. } => "thought.done",
            ChatBody::ToolStarted { .. } => "tool.started",
            ChatBody::ToolUpdated { .. } => "tool.updated",
            ChatBody::ToolDone { .. } => "tool.done",
            ChatBody::PlanUpdated { .. } => "plan.updated",
            ChatBody::NeedsOpened { .. } => "needs.opened",
            ChatBody::NeedsResolved { .. } => "needs.resolved",
            ChatBody::FilesChanged { .. } => "files.changed",
            ChatBody::TurnStarted {} => "turn.started",
            ChatBody::TurnDone { .. } => "turn.done",
            ChatBody::Usage { .. } => "usage",
            ChatBody::SessionMode { .. } => "session.mode",
            ChatBody::SessionHandoff { .. } => "session.handoff",
            ChatBody::SessionClosed { .. } => "session.closed",
            ChatBody::Notice { .. } => "notice",
        }
    }

    /// Ephemeral events are relayed live and never logged or replayed.
    pub fn ephemeral(&self) -> bool {
        matches!(self, ChatBody::MessageDelta { .. })
    }
}

/// Every event type, in the order of the table in docs/protocol.md.
pub const EVENT_TYPES: &[&str] = &[
    "session.opened",
    "message.user",
    "message.delta",
    "message.done",
    "thought.done",
    "tool.started",
    "tool.updated",
    "tool.done",
    "plan.updated",
    "needs.opened",
    "needs.resolved",
    "files.changed",
    "turn.started",
    "turn.done",
    "usage",
    "session.mode",
    "session.handoff",
    "session.closed",
    "notice",
];

/// Parse one event (a CHAT payload line). Junk is `None`, never a panic.
pub fn parse_event(text: &str) -> Option<ChatEvent> {
    if text.len() > MAX_EVENT_BYTES + 4096 {
        return None;
    }
    let ev: ChatEvent = serde_json::from_str(text).ok()?;
    if !valid_session_id(&ev.session_id) {
        return None;
    }
    // Only a delta is ephemeral (seq 0); every other event has a seq.
    if ev.body.ephemeral() != (ev.seq == 0) {
        return None;
    }
    if let ChatBody::NeedsOpened { item } = &ev.body {
        let ok = ["permission", "question", "plan"].contains(&item.kind.as_str())
            && ["normal", "high"].contains(&item.risk.as_str())
            && item.options.iter().all(|o| RESPONSE_KINDS.contains(&o.as_str()));
        if !ok {
            return None;
        }
    }
    Some(ev)
}

/// The answers a person may give a needs-you item (`chat.respond`).
pub const RESPONSE_KINDS: &[&str] = &[
    "allow_once",
    "allow_session",
    "deny",
    "answers",
    "approve_plan",
    "keep_planning",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_and_refuse_junk() {
        let f = ChatFrame {
            gap: true,
            ephemeral: false,
            req: 3,
            session_id: "c-abcdefgh".into(),
            seq: 42,
            payload: br#"{"x":1}"#.to_vec(),
        };
        let enc = f.encode().unwrap();
        assert_eq!(enc[0], 4);
        assert_eq!(enc[1], 1);
        assert_eq!(ChatFrame::decode(&enc), Some(f.clone()));
        for i in 0..enc.len() {
            let _ = ChatFrame::decode(&enc[..i]);
        }
        // A CHAT frame is not a pane data frame, and the reverse.
        assert!(crate::DataFrame::decode(&enc).is_none());
        let pane = crate::codec::encode_parts(crate::DataKind::Output, false, 0, "w1:p1", 0, b"x").unwrap();
        assert!(ChatFrame::decode(&pane).is_none());
        // Unknown flag bits are refused.
        let mut bad = enc.clone();
        bad[1] = 0x04;
        assert!(ChatFrame::decode(&bad).is_none());
        let eph = ChatFrame {
            ephemeral: true,
            gap: false,
            ..f
        }
        .encode()
        .unwrap();
        assert_eq!(eph[1], 2);
        assert!(ChatFrame::decode(&eph).unwrap().ephemeral);
    }

    #[test]
    fn session_ids() {
        assert!(valid_session_id("c-0123abcd"));
        assert!(valid_session_id(&"a".repeat(64)));
        for bad in ["", "short", "has space x", "w1:p1-long", "é-abcdefgh", &"a".repeat(65)] {
            assert!(!valid_session_id(bad), "{bad}");
        }
    }

    #[test]
    fn every_body_names_its_type() {
        let b = ChatBody::TurnStarted {};
        let v = serde_json::to_value(ChatEvent {
            seq: 1,
            at: "t".into(),
            session_id: "c-abcdefgh".into(),
            turn_id: Some("t1".into()),
            agent_path: None,
            body: b.clone(),
        })
        .unwrap();
        assert_eq!(v["type"], b.kind());
        assert!(EVENT_TYPES.contains(&b.kind()));
        assert_eq!(v["turnId"], "t1");
        assert!(parse_event(&v.to_string()).is_some());
        assert!(parse_event(r#"{"seq":1,"at":"t","sessionId":"c-abcdefgh","type":"nope"}"#).is_none());
        assert!(parse_event("not json").is_none());
    }
}
