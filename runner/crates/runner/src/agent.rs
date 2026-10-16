//! Agent status per pane (docs/PLAN.md "Agent status"):
//! precedence **reported > screen > heuristic**.
//!
//! - *reported*: Claude hooks (`tmuxedo-runner hook`) and the runner API's
//!   `pane.report_agent`. Cleared when the agent's process leaves the
//!   foreground or the pane exits (staleness reconciliation).
//! - *screen*: tmuxedo-detect's agent files over vt100 text + OSC title/progress
//!   (docs/design/agent-detection.md).
//! - *heuristic* (tier 0): the foreground process identifies the agent;
//!   output in the last 1.5 s means working; a BEL or an OSC 9/777
//!   notification after work means done; otherwise idle.

use crate::plugin::AgentPlugin;
use std::time::{Duration, Instant};
use tmuxedo_protocol::frames::{Agent, AgentPrompt, AgentSource, AgentStatus};

pub const ACTIVE_WINDOW: Duration = Duration::from_millis(1500);

#[derive(Debug, Clone)]
pub struct Reported {
    pub status: AgentStatus,
    pub source: String,
}

#[derive(Debug, Clone)]
pub struct Tracker {
    /// Agent kind from the foreground process (`claude`, `codex`, …).
    pub process_kind: Option<String>,
    /// Agent kind reported by hooks / report_agent.
    pub reported_kind: Option<String>,
    pub reported: Option<Reported>,
    pub screen: Option<AgentStatus>,
    pub heuristic: AgentStatus,
    pub effective: AgentStatus,
    pub source: AgentSource,
    pub since: String,
    pub state_change_seq: u64,
    pub completion_seq: Option<u64>,
    pub prompt: Option<(String, AgentPrompt)>,
    pub last_output: Option<Instant>,
    seen_attention: u64,
    worked_since_attention: bool,
    /// The output instant a heuristic `done` was read at (the bell's own output).
    done_output: Option<Instant>,
    pub fg_pgid: Option<u32>,
    /// Mirror offset the last screen detection ran at.
    pub detected_at: u64,
    /// The agent's dialogs show only on screen (its hooks say "working"
    /// while it waits: Cursor, `permissions.mode = "keystroke"`), so a
    /// blocked screen wins over a reported `working`.
    pub screen_blocks: bool,
    /// The agent's in-process sub-agents (`Agent.children`).
    pub children: crate::subagents::Children,
}

impl Default for Tracker {
    fn default() -> Self {
        Self {
            process_kind: None,
            reported_kind: None,
            reported: None,
            screen: None,
            heuristic: AgentStatus::Unknown,
            effective: AgentStatus::Unknown,
            source: AgentSource::Heuristic,
            since: crate::util::iso_now(),
            state_change_seq: 0,
            completion_seq: None,
            prompt: None,
            last_output: None,
            seen_attention: 0,
            worked_since_attention: false,
            done_output: None,
            fg_pgid: None,
            detected_at: 0,
            screen_blocks: false,
            children: Default::default(),
        }
    }
}

pub fn status_str(s: AgentStatus) -> &'static str {
    match s {
        AgentStatus::Working => "working",
        AgentStatus::Blocked => "blocked",
        AgentStatus::Done => "done",
        AgentStatus::Idle => "idle",
        AgentStatus::Unknown => "unknown",
    }
}

pub fn parse_status(s: &str) -> Option<AgentStatus> {
    match s {
        "working" => Some(AgentStatus::Working),
        "blocked" => Some(AgentStatus::Blocked),
        "done" => Some(AgentStatus::Done),
        "idle" => Some(AgentStatus::Idle),
        "unknown" => Some(AgentStatus::Unknown),
        _ => None,
    }
}

impl Tracker {
    pub fn kind(&self) -> Option<&str> {
        self.reported_kind.as_deref().or(self.process_kind.as_deref())
    }

    /// Recompute the effective status; Some((old, new)) when it changed.
    pub fn recompute(&mut self) -> Option<(AgentStatus, AgentStatus)> {
        let screen_blocked = self.screen_blocks && self.screen == Some(AgentStatus::Blocked);
        let (status, source) = if let Some(r) = self
            .reported
            .as_ref()
            .filter(|r| !(screen_blocked && r.status == AgentStatus::Working))
        {
            (r.status, AgentSource::Reported)
        } else if screen_blocked && self.kind().is_some() {
            (AgentStatus::Blocked, AgentSource::Screen)
        } else if let (Some(s), Some(_)) = (self.screen, self.kind()) {
            (s, AgentSource::Screen)
        } else if self.kind().is_some() {
            (self.heuristic, AgentSource::Heuristic)
        } else {
            (AgentStatus::Unknown, AgentSource::Heuristic)
        };
        self.source = source;
        if status == self.effective {
            return None;
        }
        let old = self.effective;
        self.effective = status;
        self.since = crate::util::iso_now();
        self.state_change_seq += 1;
        if matches!(status, AgentStatus::Done | AgentStatus::Idle) && old == AgentStatus::Working {
            self.completion_seq = Some(self.state_change_seq);
        }
        if status != AgentStatus::Blocked {
            self.prompt = None;
        }
        Some((old, status))
    }

    pub fn on_output(&mut self, now: Instant) {
        self.last_output = Some(now);
        self.worked_since_attention = true;
    }

    /// Tier 0. `attention` = BEL count + notification count from the emulator.
    pub fn heuristic_tick(&mut self, now: Instant, attention: u64) {
        let active = self.last_output.is_some_and(|t| now.duration_since(t) < ACTIVE_WINDOW);
        let new_attention = attention > self.seen_attention;
        self.seen_attention = attention;
        self.heuristic = if self.kind().is_none() {
            AgentStatus::Unknown
        } else if new_attention && self.worked_since_attention {
            self.worked_since_attention = false;
            // The bell arrives as output itself: done holds until output that came after it.
            self.done_output = self.last_output;
            AgentStatus::Done
        } else if self.heuristic == AgentStatus::Done && self.last_output == self.done_output {
            AgentStatus::Done
        } else if active {
            AgentStatus::Working
        } else {
            AgentStatus::Idle
        };
    }

    /// The foreground process changed. Returns true if the agent went away
    /// (reported state is stale and was cleared).
    pub fn set_process_kind(&mut self, kind: Option<String>) -> bool {
        if kind == self.process_kind {
            return false;
        }
        let left = self.process_kind.is_some() && kind.is_none();
        self.process_kind = kind;
        if left {
            self.clear_agent();
        }
        left
    }

    /// The agent is gone (process exit, pane exit, SessionEnd).
    pub fn clear_agent(&mut self) {
        self.reported = None;
        self.reported_kind = None;
        self.screen = None;
        self.prompt = None;
        self.heuristic = AgentStatus::Unknown;
        self.children.clear();
    }

    pub fn report(&mut self, kind: &str, status: AgentStatus, source: &str) {
        self.reported_kind = Some(kind.to_string());
        self.reported = Some(Reported {
            status,
            source: source.into(),
        });
    }

    /// The protocol `Agent` (None when no agent is known in the pane).
    pub fn agent(&self, session_id: Option<&str>) -> Option<Agent> {
        let kind = self.kind()?;
        Some(Agent {
            kind: kind.to_string(),
            status: self.effective,
            since: self.since.clone(),
            source: self.source,
            session_id: session_id.map(str::to_owned),
            prompt_id: self.prompt.as_ref().map(|p| p.0.clone()),
            prompt: self.prompt.as_ref().map(|p| p.1.clone()),
            summary: None,
            chat: None,
            children: self.children.list(),
        })
    }
}

/// Claude Code hook event → reported status change: the Claude plugin's
/// `hooks.map` rows and the normalised events' core statuses
/// (docs/design/plugins.md §5.3).
pub fn hook_status(event: &str, notification_type: &str) -> Option<AgentStatus> {
    crate::plugin::claude()
        .map_hook(&serde_json::json!({ "hook_event_name": event, "notification_type": notification_type }))
        .status()
}

/// Durable `event` type for a transition into `s`.
pub fn durable_event(s: AgentStatus) -> Option<&'static str> {
    match s {
        AgentStatus::Blocked => Some("blocked"),
        AgentStatus::Done => Some("done"),
        AgentStatus::Idle => Some("idle"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_blocked_screen_wins_over_reported_working_for_screen_dialog_agents() {
        let mut t = Tracker::default();
        t.report("cursor", AgentStatus::Working, "hook");
        t.screen = Some(AgentStatus::Blocked);
        t.recompute();
        assert_eq!(t.effective, AgentStatus::Working, "off by default (Claude, Codex)");
        t.screen_blocks = true;
        t.recompute();
        assert_eq!((t.effective, t.source), (AgentStatus::Blocked, AgentSource::Screen));
        t.screen = Some(AgentStatus::Working);
        t.recompute();
        assert_eq!((t.effective, t.source), (AgentStatus::Working, AgentSource::Reported));
        t.screen = Some(AgentStatus::Blocked);
        t.report("cursor", AgentStatus::Done, "hook");
        t.recompute();
        assert_eq!(t.effective, AgentStatus::Done, "only a reported working gives way");
    }

    #[test]
    fn precedence_reported_over_screen_over_heuristic() {
        let mut t = Tracker::default();
        assert_eq!(t.recompute(), None, "no agent: unknown stays unknown");
        t.set_process_kind(Some("claude".into()));
        let now = Instant::now();
        t.on_output(now);
        t.heuristic_tick(now, 0);
        assert_eq!(t.recompute(), Some((AgentStatus::Unknown, AgentStatus::Working)));
        assert_eq!(t.source, AgentSource::Heuristic);
        t.screen = Some(AgentStatus::Blocked);
        assert_eq!(t.recompute(), Some((AgentStatus::Working, AgentStatus::Blocked)));
        assert_eq!(t.source, AgentSource::Screen);
        t.report("claude", AgentStatus::Done, "hook");
        assert_eq!(t.recompute(), Some((AgentStatus::Blocked, AgentStatus::Done)));
        assert_eq!(t.source, AgentSource::Reported);
        assert_eq!(t.completion_seq, None, "done after blocked is not a completion");
        // The agent's process leaves: reported state is stale.
        assert!(t.set_process_kind(None));
        assert_eq!(t.recompute(), Some((AgentStatus::Done, AgentStatus::Unknown)));
        assert!(t.agent(None).is_none());
    }

    #[test]
    fn heuristics_working_idle_done() {
        let mut t = Tracker::default();
        t.set_process_kind(Some("codex".into()));
        let t0 = Instant::now();
        t.on_output(t0);
        t.heuristic_tick(t0, 0);
        assert_eq!(t.heuristic, AgentStatus::Working);
        t.heuristic_tick(t0 + Duration::from_secs(2), 0);
        assert_eq!(t.heuristic, AgentStatus::Idle);
        t.on_output(t0 + Duration::from_secs(3));
        t.heuristic_tick(t0 + Duration::from_secs(3), 1);
        assert_eq!(t.heuristic, AgentStatus::Done, "attention after work");
        // The bell was output itself: the ticks right after it (inside the active
        // window) must not turn done into working.
        t.heuristic_tick(t0 + Duration::from_millis(3250), 1);
        assert_eq!(t.heuristic, AgentStatus::Done, "the bell's own output is not new work");
        t.heuristic_tick(t0 + Duration::from_millis(4000), 1);
        assert_eq!(t.heuristic, AgentStatus::Done);
        t.heuristic_tick(t0 + Duration::from_secs(9), 1);
        assert_eq!(t.heuristic, AgentStatus::Done, "stays done until new work");
        // A second bell with no work since the first changes nothing.
        t.heuristic_tick(t0 + Duration::from_millis(9500), 2);
        assert_eq!(t.heuristic, AgentStatus::Done);
        // New output after the bell: working, then idle.
        t.on_output(t0 + Duration::from_secs(10));
        t.heuristic_tick(t0 + Duration::from_secs(10), 2);
        assert_eq!(t.heuristic, AgentStatus::Working);
        t.heuristic_tick(t0 + Duration::from_secs(12), 2);
        assert_eq!(t.heuristic, AgentStatus::Idle);
        // The agent leaves: unknown.
        t.set_process_kind(None);
        t.heuristic_tick(t0 + Duration::from_secs(14), 2);
        assert_eq!(t.heuristic, AgentStatus::Unknown);
    }

    #[test]
    fn hook_mapping() {
        assert_eq!(hook_status("UserPromptSubmit", ""), Some(AgentStatus::Working));
        assert_eq!(hook_status("PermissionRequest", ""), Some(AgentStatus::Blocked));
        assert_eq!(
            hook_status("Notification", "permission_prompt"),
            Some(AgentStatus::Blocked)
        );
        assert_eq!(hook_status("Notification", "idle_prompt"), Some(AgentStatus::Idle));
        assert_eq!(hook_status("Notification", "auth_success"), None);
        assert_eq!(hook_status("Stop", ""), Some(AgentStatus::Done));
        assert_eq!(hook_status("SessionEnd", ""), None);
    }
}
