//! Helpers: an agent's in-process sub-agents (docs/protocol.md "Helpers"),
//! the Rust twin of the helper section of packages/protocol/src/agents.ts.
//! `Agent.children` in `status` frames and the topology, and the two durable
//! event types. Checked against packages/protocol/fixtures/subagents.json.

use serde::{Deserialize, Serialize};

/// The most children one `Agent.children` carries (agents.ts MAX_AGENT_CHILDREN).
pub const MAX_AGENT_CHILDREN: usize = 16;
/// Longest child id on the wire (agents.ts AGENT_CHILD_ID_MAX).
pub const AGENT_CHILD_ID_MAX: usize = 128;
/// Longest child name or model on the wire (agents.ts AGENT_CHILD_NAME_MAX).
pub const AGENT_CHILD_NAME_MAX: usize = 80;

/// Durable event types (`event.type`); `data` is
/// `{kind, childId, name, parentId?, model?, roleId?}`.
pub const SUBAGENT_STARTED: &str = "subagent_started";
pub const SUBAGENT_COMPLETED: &str = "subagent_completed";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentChildStatus {
    Working,
    Done,
}

/// One sub-agent the pane's agent started inside its own process.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentChild {
    pub id: String,
    /// What the CLI calls it: its type or role name.
    pub name: String,
    pub status: AgentChildStatus,
    /// When the status began (ISO 8601).
    pub since: String,
    /// When it started; equal to `since` while working.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    /// The child that started it, when the CLI says (a nested helper).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The role it was built from (`config.roles`), when its name is one a
    /// builder gave a role; absent for any other helper.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role_id: Option<String>,
    /// Open needs-you items it raised (not sent yet).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub needs: Option<u32>,
}

/// `s` cut to at most `max` characters (never inside a character).
pub fn cut(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_child_writes_only_what_it_has() {
        let c = AgentChild {
            id: "a1".into(),
            name: "Explore".into(),
            status: AgentChildStatus::Working,
            since: "2026-10-12T10:00:00.000Z".into(),
            started_at: None,
            parent_id: None,
            model: None,
            role_id: None,
            needs: None,
        };
        assert_eq!(
            serde_json::to_string(&c).unwrap(),
            r#"{"id":"a1","name":"Explore","status":"working","since":"2026-10-12T10:00:00.000Z"}"#
        );
        assert_eq!(cut("héllo", 2), "hé");
    }
}
