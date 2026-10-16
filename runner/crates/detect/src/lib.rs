//! tmuxedo's agent detection: which coding agent is in a pane's foreground and
//! what its screen says it is doing. Specification:
//! `docs/design/agent-detection.md` (the section numbers below refer to it).
//!
//! - [`Agent`]: a supported agent (one bundled agent file each, `agents/*.toml`).
//! - [`read`] / [`explain`]: the screen reading (§7) and its explanation (§10).
//! - [`identify`]: the agent of a foreground job (§8); [`platform`] reads the job.
//!
//! Every line and branch of this crate's product code is covered by its tests
//! (`runner/scripts/detect-coverage.sh`, gated in CI); `#[cfg(test)]` modules
//! are left out of the measurement with `coverage(off)`.
#![cfg_attr(coverage_nightly, feature(coverage_attribute))]

pub mod catalog;
pub mod cond;
pub mod identify;
pub mod platform;
pub mod scope;

pub use catalog::{Catalog, Explanation, SignalResult};
pub use platform::{Job, Process};

use std::sync::OnceLock;

/// What a screen says (§2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Status {
    Working,
    Blocked,
    Idle,
    /// No usable evidence: the runner falls back to its heuristic.
    Unknown,
    /// A viewer hides the live state: keep the previous reading.
    Hold,
}

impl Status {
    pub const ALL: [Status; 5] = [
        Status::Working,
        Status::Blocked,
        Status::Idle,
        Status::Unknown,
        Status::Hold,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Status::Working => "working",
            Status::Blocked => "blocked",
            Status::Idle => "idle",
            Status::Unknown => "unknown",
            Status::Hold => "hold",
        }
    }

    pub fn parse(s: &str) -> Option<Status> {
        Status::ALL.into_iter().find(|v| v.as_str() == s)
    }
}

/// What the terminal shows (§3).
#[derive(Debug, Clone, Copy, Default)]
pub struct Input<'a> {
    pub screen: &'a str,
    pub title: &'a str,
    pub progress: &'a str,
}

impl<'a> Input<'a> {
    pub fn screen(screen: &'a str) -> Self {
        Input {
            screen,
            title: "",
            progress: "",
        }
    }
}

static CATALOG: OnceLock<Catalog> = OnceLock::new();

/// The process-wide catalogue: bundled files plus local overrides (§9), read once.
pub fn catalog() -> &'static Catalog {
    CATALOG.get_or_init(|| {
        let dir = catalog::override_dir(
            std::env::var_os("XDG_CONFIG_HOME").map(Into::into),
            std::env::var_os("HOME").map(Into::into),
        );
        Catalog::load(dir.as_deref())
    })
}

/// A supported agent: an index into the catalogue (bundled order).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Agent(usize);

impl Agent {
    /// Every supported agent.
    pub fn all() -> impl Iterator<Item = Agent> {
        (0..catalog().agents.len()).map(Agent)
    }

    /// The agent a process name, path or label names (§8.1).
    pub fn lookup(name: &str) -> Option<Agent> {
        identify::lookup(catalog(), name).map(Agent)
    }

    pub fn index(self) -> usize {
        self.0
    }

    fn def(self) -> &'static catalog::AgentDef {
        &catalog().agents[self.0]
    }

    /// The canonical id (`claude`, `copilot`, …): the pane's agent kind.
    pub fn id(self) -> &'static str {
        &self.def().id
    }

    pub fn title(self) -> &'static str {
        &self.def().title
    }

    /// The executable `agent.start` runs.
    pub fn launch(self) -> &'static str {
        &self.def().launch
    }
}

/// The screen reading for an agent (§7).
pub fn read(agent: Agent, input: &Input<'_>) -> Status {
    agent.def().read(input)
}

/// Every signal's result (§10).
pub fn explain(agent: Agent, input: &Input<'_>) -> Explanation {
    agent.def().explain(input)
}

/// Explain for a label that may name no agent.
pub fn explain_label(label: Option<&str>, input: &Input<'_>) -> Explanation {
    match label.and_then(Agent::lookup) {
        Some(agent) => explain(agent, input),
        None => Explanation::unknown(label),
    }
}

/// The agent in a foreground job (§8.4).
pub fn identify(job: &Job) -> Option<Agent> {
    identify::job(catalog(), job).map(Agent)
}

/// The agent one process runs, batch runs included (hook ancestry).
pub fn agent_of_process(p: &platform::Process) -> Option<Agent> {
    identify::process_any(catalog(), p).map(Agent)
}

/// `server.agent_manifests` rows (§10).
pub fn catalog_rows() -> Vec<serde_json::Value> {
    catalog().rows()
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))] // the tests themselves are not product code
mod agent_of_process_tests {
    use super::*;

    fn p(name: &str, argv: &[&str]) -> Process {
        Process {
            pid: 1,
            name: name.into(),
            argv: argv.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn who_a_hook_caller_descends_from() {
        let id = |x: &Process| agent_of_process(x).map(|a| a.id());
        assert_eq!(
            id(&p("claude", &["/usr/local/bin/claude", "--settings", "s"])),
            Some("claude")
        );
        // A one-shot run counts too.
        assert_eq!(id(&p("claude", &["claude", "-p", "hi"])), Some("claude"));
        // A script named after the agent, through a shell; node running the agent's script.
        assert_eq!(id(&p("claude", &["/bin/bash", "/opt/fake/bin/claude"])), Some("claude"));
        assert_eq!(
            id(&p(
                "node",
                &["node", "/usr/lib/node_modules/@openai/codex/bin/codex.js"]
            )),
            Some("codex")
        );
        // `exec -a` with a runtime underneath: argv[0] is not the runtime, comm is.
        assert_eq!(id(&p("node", &["codex-wrapper", "/x/codex.js"])), Some("codex"));
        assert_eq!(id(&p("bash", &["-bash"])), None);
        // No argv (unreadable): the kernel's name; neither an agent nor a runtime: none.
        assert_eq!(id(&p("claude", &[])), Some("claude"));
        assert_eq!(id(&p("cron", &["cron"])), None);
        assert_eq!(id(&p("sh", &["sh", "-c", "tmuxedo-runner hook"])), None);
        assert_eq!(id(&p("node", &["node", "-e", "1"])), None);
    }
}
