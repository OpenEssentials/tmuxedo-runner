//! Machine-side controls (docs/design/mac-isolation.md §3, security S3): rules the
//! RUNNER enforces on what arrives from the gateway, so they hold even if tmuxedo's
//! servers are compromised. The Mac app sets them (`--allow-user`, `--observe-only`).
//!
//!   allow-list    the only account this machine serves; a `welcome` naming another
//!                 one stops the runner for good (as a revocation would).
//!   observe-only  remote viewers may watch: input, resizes, decisions, login
//!                 callbacks and every method that changes something are refused.

/// Runner API methods a remote viewer may call in observe-only mode: they only read.
pub const READ_ONLY_METHODS: &[&str] = &[
    "ping",
    "workspace.list",
    "workspace.get",
    "tab.list",
    "tab.get",
    "pane.list",
    "pane.get",
    "pane.current",
    "pane.read",
    "pane.process_info",
    "agent.list",
    "agent.get",
    "agent.read",
    "agent.explain",
    "layout.export",
    "server.agent_manifests",
    "plugin.list",
    "plugin.action.list",
    "plugin.log.list",
    "chat.list",
    "chat.get",
    "chat.read",
    "chat.file",
    "files.list",
    "files.stat",
    "files.quota",
    "files.read",
    "files.upload_status",
    "needs.list",
    "task.get",
    "task.list",
    "fs.roots",
    "fs.list",
    "fs.stat",
    "fs.read",
    "fs.search",
    "fs.download",
    "git.status",
    "git.diff",
    "checkpoint.list",
    "checkpoint.preview",
];

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Guardrails {
    /// The one account (user id) this machine serves; None = whoever owns the token.
    pub allow_user: Option<String>,
    pub observe_only: bool,
}

impl Guardrails {
    /// None = the welcome may proceed; Some(reason) = refuse it for good.
    pub fn check_welcome(&self, user_id: &str) -> Option<String> {
        match &self.allow_user {
            Some(allowed) if allowed != user_id => Some(format!(
                "tmuxedo connected this machine to account {user_id}, but it serves only {allowed}; refusing"
            )),
            _ => None,
        }
    }

    pub fn allows_method(&self, method: &str) -> bool {
        !self.observe_only || READ_ONLY_METHODS.contains(&method)
    }

    /// Input, resizes, decisions and login callbacks.
    pub fn allows_control(&self) -> bool {
        !self.observe_only
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allow_list() {
        let g = Guardrails::default();
        assert_eq!(g.check_welcome("u1"), None, "no list: anyone the token belongs to");
        let g = Guardrails {
            allow_user: Some("u1".into()),
            observe_only: false,
        };
        assert_eq!(g.check_welcome("u1"), None);
        let why = g.check_welcome("u2").unwrap();
        assert!(why.contains("u2") && why.contains("u1"), "{why}");
    }

    #[test]
    fn observe_only_blocks_every_mutation() {
        let g = Guardrails {
            allow_user: None,
            observe_only: true,
        };
        assert!(!g.allows_control());
        for m in [
            "pane.send_input",
            "pane.send_text",
            "pane.send_keys",
            "pane.close",
            "workspace.create",
            "agent.start",
            "agent.send_keys",
            "pane.split",
            "worktree.create",
            "plugin.action.invoke",
            "plugin.enable",
            "pane.rename",
        ] {
            assert!(!g.allows_method(m), "{m}");
        }
        for m in READ_ONLY_METHODS {
            assert!(g.allows_method(m), "{m}");
        }
        let open = Guardrails::default();
        assert!(open.allows_control());
        assert!(open.allows_method("pane.send_input"));
    }

    /// Every method in the list is a real one of the runner API, and none of them writes.
    #[test]
    fn the_list_names_only_reading_methods_of_the_schema() {
        let schema = include_str!("../../../api/runner-api.schema.json");
        for m in READ_ONLY_METHODS {
            if *m == "ping" {
                continue;
            }
            assert!(
                schema.contains(&format!("\"{m}\"")),
                "{m} is not in the runner API schema"
            );
            assert!(
                ![
                    "send", "create", "close", "start", "rename", "move", "split", "focus", "resize", "apply",
                    "enable", "invoke"
                ]
                .iter()
                .any(|w| m.contains(w)),
                "{m}"
            );
        }
    }
}
