//! The normalised agent event model (docs/design/plugins.md §5.3). Plugins
//! translate native signals into these; the core owns status precedence,
//! durable events, push, prompt ids and decisions, so web, app and TUI never
//! see agent-specific events.

use serde_json::Value;
use tmuxedo_protocol::frames::AgentStatus;

#[derive(Debug, Clone, PartialEq)]
pub enum AgentEvent {
    SessionStarted {
        session_id: Option<String>,
        cwd: Option<String>,
        source: Option<String>,
    },
    TurnStarted,
    TurnCompleted,
    TurnFailed {
        error: String,
    },
    AttentionBlocked {
        reason: String,
    },
    AttentionIdle,
    PromptPermission {
        tool: String,
        input: Value,
        suggestions: Option<Value>,
        /// A remote decision can be delivered (the hook is held).
        decidable: bool,
    },
    PromptQuestion {
        question: String,
        options: Option<Value>,
    },
    PromptResolved,
    SessionEnded,
    AuthState {
        state: String,
        reason: String,
    },
    OpenUrl {
        url: String,
    },
    Usage(Value),
    Log {
        level: String,
        message: String,
    },
    /// The agent started a sub-agent in its own process (a "helper").
    SubagentStarted(crate::subagents::Started),
    /// A helper finished; `id` and `name` as far as the agent's hook says.
    SubagentCompleted {
        id: Option<String>,
        name: Option<String>,
    },
}

impl AgentEvent {
    /// The manifest name of this event (`hooks.map[].emit`).
    pub fn name(&self) -> &'static str {
        match self {
            AgentEvent::SessionStarted { .. } => "session.started",
            AgentEvent::TurnStarted => "turn.started",
            AgentEvent::TurnCompleted => "turn.completed",
            AgentEvent::TurnFailed { .. } => "turn.failed",
            AgentEvent::AttentionBlocked { .. } => "attention.blocked",
            AgentEvent::AttentionIdle => "attention.idle",
            AgentEvent::PromptPermission { .. } => "prompt.permission",
            AgentEvent::PromptQuestion { .. } => "prompt.question",
            AgentEvent::PromptResolved => "prompt.resolved",
            AgentEvent::SessionEnded => "session.ended",
            AgentEvent::AuthState { .. } => "auth.state",
            AgentEvent::OpenUrl { .. } => "open_url",
            AgentEvent::Usage(_) => "usage",
            AgentEvent::Log { .. } => "log",
            AgentEvent::SubagentStarted(_) => "subagent.started",
            AgentEvent::SubagentCompleted { .. } => "subagent.completed",
        }
    }

    /// Core effect on the pane's reported status (plugins.md §5.3 table).
    /// `prompt.resolved` means the agent carried on after an answer, so it
    /// is `working` (today's PostToolUse / PermissionDenied mapping).
    pub fn status(&self) -> Option<AgentStatus> {
        match self {
            AgentEvent::SessionStarted { .. } => Some(AgentStatus::Idle),
            AgentEvent::TurnStarted => Some(AgentStatus::Working),
            AgentEvent::TurnCompleted => Some(AgentStatus::Done),
            AgentEvent::TurnFailed { .. } => Some(AgentStatus::Idle),
            AgentEvent::AttentionBlocked { .. } => Some(AgentStatus::Blocked),
            AgentEvent::AttentionIdle => Some(AgentStatus::Idle),
            AgentEvent::PromptPermission { .. } => Some(AgentStatus::Blocked),
            AgentEvent::PromptQuestion { .. } => Some(AgentStatus::Blocked),
            AgentEvent::PromptResolved => Some(AgentStatus::Working),
            AgentEvent::SessionEnded
            | AgentEvent::AuthState { .. }
            | AgentEvent::OpenUrl { .. }
            | AgentEvent::Usage(_)
            | AgentEvent::Log { .. }
            | AgentEvent::SubagentStarted(_)
            | AgentEvent::SubagentCompleted { .. } => None,
        }
    }

    /// Whether a prompt held in the pane was answered at the terminal: the
    /// agent moved on (a new turn, the tool ran or was refused, the turn
    /// ended, the session ended).
    pub fn releases_holds(&self) -> bool {
        matches!(
            self,
            AgentEvent::TurnStarted | AgentEvent::TurnCompleted | AgentEvent::PromptResolved | AgentEvent::SessionEnded
        )
    }

    /// `attention.blocked` keeps a prompt that is already held (a
    /// notification about the same dialog).
    pub fn keeps_prompt(&self) -> bool {
        matches!(self, AgentEvent::AttentionBlocked { .. })
    }
}

/// What one native hook payload means, per the plugin.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HookOutcome {
    pub events: Vec<AgentEvent>,
    /// The reply is deferred until a decision (or the hold deadline).
    pub hold: bool,
    /// The agent's session id and cwd, when the payload carries them.
    pub session_id: Option<String>,
    pub cwd: Option<String>,
    /// The session's transcript file, when the payload names it (`hooks.transcript_field`).
    pub transcript: Option<String>,
    /// A row said a held prompt is over (`hooks.map[].release`).
    pub release: bool,
}

impl HookOutcome {
    pub fn status(&self) -> Option<AgentStatus> {
        self.events.iter().rev().find_map(AgentEvent::status)
    }
    pub fn releases_holds(&self) -> bool {
        self.release || self.events.iter().any(AgentEvent::releases_holds)
    }
    pub fn names(&self) -> Vec<&'static str> {
        self.events.iter().map(AgentEvent::name).collect()
    }
}
