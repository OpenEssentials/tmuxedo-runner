//! Who is calling: the hook caller ancestry check (PLAN Phase 13,
//! docs/design/not-yet-thought-of.md #1).
//!
//! The runner's local socket is per user, and the pane id a hook names comes
//! from the caller's environment, so any process in a pane (a `postinstall`
//! script in a cloned repo) could pretend to be the agent: fake a permission
//! prompt, hide a real one by reporting `done`, or send a login link. So the
//! socket reads the caller's pid from the kernel (`SO_PEERCRED` on Linux,
//! `LOCAL_PEERPID` on macOS, through tokio's `peer_cred`) and a hook counts
//! only when that process:
//!
//! 1. descends from the pane's own process (the holder's child), and
//! 2. sits at most [`MAX_HOPS`] parents below the pane's **agent**: the
//!    process nearest the pane's root that the detect catalogue recognises,
//!    which must be one of the plugin's agent labels.
//!
//! The agent runs its hook command through a shell (`sh -c …` → the hook
//! binary: one or two hops). A script the agent's own tools start sits much
//! deeper (bash → npm → node → sh → …), and a script started from the
//! pane's shell has no agent above it at all; both are logged and ignored.
//! A process that renames itself after an agent (`exec -a claude`) does not
//! help either: the agent that counts is the topmost one in the pane.

use tmuxedo_detect::platform::Process;

/// The most parents between a hook process and the pane's agent.
pub const MAX_HOPS: usize = 3;
/// Longest parent chain walked (a pane's tree is shallow; this bounds a loop).
const MAX_DEPTH: usize = 64;

/// Reads processes; tests use a fake table.
pub trait Procs {
    fn parent(&self, pid: u32) -> Option<u32>;
    fn process(&self, pid: u32) -> Option<Process>;
    /// The agent label a process runs (`claude`, `codex` …), if any.
    fn agent(&self, p: &Process) -> Option<String> {
        tmuxedo_detect::agent_of_process(p).map(|a| a.id().to_string())
    }
}

/// The live system (`/proc`, `libproc`).
pub struct System;

impl Procs for System {
    fn parent(&self, pid: u32) -> Option<u32> {
        tmuxedo_detect::platform::parent(pid)
    }
    fn process(&self, pid: u32) -> Option<Process> {
        tmuxedo_detect::platform::process(pid)
    }
}

/// Why a caller was refused (logged; never sent anywhere).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The kernel gave no pid (an unsupported platform, or a socket pair).
    NoPeer,
    /// The pane's process is not known yet.
    NoPane,
    /// The caller does not descend from the pane's process.
    NotInPane,
    /// No agent above the caller in the pane.
    NoAgent,
    /// The pane's agent is another CLI than the plugin's.
    OtherAgent,
    /// The caller is too far below the agent (a tool's subprocess).
    TooDeep,
}

impl Refusal {
    pub fn code(self) -> &'static str {
        match self {
            Refusal::NoPeer => "no_peer",
            Refusal::NoPane => "no_pane",
            Refusal::NotInPane => "not_in_pane",
            Refusal::NoAgent => "not_from_agent",
            Refusal::OtherAgent => "other_agent",
            Refusal::TooDeep => "too_deep",
        }
    }
}

/// The verdict for a caller: the agent's pid and label, or why not.
pub fn check(
    procs: &dyn Procs,
    caller: Option<u32>,
    pane_root: u32,
    labels: &[String],
) -> Result<(u32, String), Refusal> {
    let caller = caller.filter(|&p| p > 0).ok_or(Refusal::NoPeer)?;
    if pane_root == 0 {
        return Err(Refusal::NoPane);
    }
    // caller → … → pane root (inclusive), nearest first.
    let mut chain = vec![caller];
    let mut at = caller;
    while at != pane_root {
        if chain.len() > MAX_DEPTH {
            return Err(Refusal::NotInPane);
        }
        match procs.parent(at) {
            Some(p) if p != at => {
                chain.push(p);
                at = p;
            }
            _ => return Err(Refusal::NotInPane),
        }
    }
    // The topmost agent in the pane: scan from the root down to the caller's parent.
    let ancestors = &chain[1..];
    let top = ancestors
        .iter()
        .enumerate()
        .rev()
        .find_map(|(hops, &pid)| Some((hops + 1, pid, procs.agent(&procs.process(pid)?)?)));
    let Some((hops, pid, label)) = top else {
        return Err(Refusal::NoAgent);
    };
    if !labels.contains(&label) {
        return Err(Refusal::OtherAgent);
    }
    if hops > MAX_HOPS {
        return Err(Refusal::TooDeep);
    }
    Ok((pid, label))
}

/// A chat's hooks: the caller must sit at most [`MAX_HOPS`] below the
/// chat's agent child (`root`, which the runner started itself).
pub fn below(procs: &dyn Procs, caller: Option<u32>, root: u32) -> Result<usize, Refusal> {
    let caller = caller.filter(|&p| p > 0).ok_or(Refusal::NoPeer)?;
    if root == 0 {
        return Err(Refusal::NoPane);
    }
    let mut at = caller;
    for hops in 1..=MAX_DEPTH {
        match procs.parent(at) {
            Some(p) if p == root && hops <= MAX_HOPS => return Ok(hops),
            Some(p) if p == root => return Err(Refusal::TooDeep),
            Some(p) if p != at => at = p,
            _ => return Err(Refusal::NotInPane),
        }
    }
    Err(Refusal::NotInPane)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// pid → (ppid, name, argv).
    struct Fake(HashMap<u32, (u32, &'static str, Vec<&'static str>)>);

    impl Procs for Fake {
        fn parent(&self, pid: u32) -> Option<u32> {
            self.0.get(&pid).map(|p| p.0).filter(|&p| p > 0)
        }
        fn process(&self, pid: u32) -> Option<Process> {
            self.0.get(&pid).map(|(_, n, a)| Process {
                pid,
                name: n.to_string(),
                argv: a.iter().map(|s| s.to_string()).collect(),
            })
        }
    }

    fn tree() -> Fake {
        Fake(HashMap::from([
            (1, (0, "init", vec!["/sbin/init"])),
            (5, (1, "holder", vec!["tmuxedo-runner", "holder"])),
            // the pane: bash → claude → sh -c → hook
            (10, (5, "bash", vec!["-bash"])),
            (20, (10, "claude", vec!["/usr/local/bin/claude", "--settings", "x"])),
            (21, (20, "sh", vec!["/bin/sh", "-c", "tmuxedo-runner hook"])),
            (22, (21, "tmuxedo-runner", vec!["tmuxedo-runner", "hook"])),
            // a hook straight from claude
            (23, (20, "tmuxedo-runner", vec!["tmuxedo-runner", "hook"])),
            // a postinstall deep inside claude's Bash tool
            (30, (20, "bash", vec!["/bin/bash", "-c", "npm install"])),
            (31, (30, "npm", vec!["node", "/usr/bin/npm", "install"])),
            (32, (31, "sh", vec!["sh", "-c", "node postinstall.js"])),
            (33, (32, "node", vec!["node", "postinstall.js"])),
            (34, (33, "tmuxedo-runner", vec!["tmuxedo-runner", "hook"])),
            // ... which renames itself after the agent
            (35, (33, "claude", vec!["claude"])),
            (36, (35, "tmuxedo-runner", vec!["tmuxedo-runner", "hook"])),
            // a script run from the pane's shell, no agent above it
            (40, (10, "sh", vec!["sh", "./evil.sh"])),
            (41, (40, "tmuxedo-runner", vec!["tmuxedo-runner", "hook"])),
            // another pane
            (50, (5, "bash", vec!["-bash"])),
            (51, (50, "claude", vec!["claude"])),
            (52, (51, "tmuxedo-runner", vec!["tmuxedo-runner", "hook"])),
            // a scripted stand-in: bash running a script named claude, its subshell
            (60, (5, "bash", vec!["-bash"])),
            (
                61,
                (
                    60,
                    "claude",
                    vec!["/bin/bash", "/opt/fake/bin/claude", "--settings", "s"],
                ),
            ),
            (
                62,
                (
                    61,
                    "claude",
                    vec!["/bin/bash", "/opt/fake/bin/claude", "--settings", "s"],
                ),
            ),
            (63, (62, "tmuxedo-runner", vec!["tmuxedo-runner", "hook"])),
            // codex in a pane, claude plugin's hook from it
            (70, (5, "bash", vec!["-bash"])),
            (71, (70, "codex", vec!["codex"])),
            (72, (71, "tmuxedo-runner", vec!["tmuxedo-runner", "hook"])),
            // a process outside every pane
            (90, (1, "cron", vec!["cron"])),
            (91, (90, "tmuxedo-runner", vec!["tmuxedo-runner", "hook"])),
        ]))
    }

    fn claude() -> Vec<String> {
        vec!["claude".into()]
    }

    #[test]
    fn the_agents_own_hooks_pass() {
        let t = tree();
        assert_eq!(check(&t, Some(22), 10, &claude()), Ok((20, "claude".into())));
        assert_eq!(check(&t, Some(23), 10, &claude()), Ok((20, "claude".into())));
        // The pane runs the agent directly (its root is the agent).
        assert_eq!(check(&t, Some(52), 51, &claude()), Ok((51, "claude".into())));
        // A stand-in script named after the agent, hooking from a subshell.
        assert_eq!(check(&t, Some(63), 60, &claude()), Ok((61, "claude".into())));
    }

    #[test]
    fn everyone_else_is_refused() {
        let t = tree();
        assert_eq!(
            check(&t, Some(41), 10, &claude()),
            Err(Refusal::NoAgent),
            "script from the shell"
        );
        assert_eq!(
            check(&t, Some(34), 10, &claude()),
            Err(Refusal::TooDeep),
            "a tool's subprocess"
        );
        assert_eq!(
            check(&t, Some(36), 10, &claude()),
            Err(Refusal::TooDeep),
            "renamed after the agent"
        );
        assert_eq!(
            check(&t, Some(52), 10, &claude()),
            Err(Refusal::NotInPane),
            "another pane"
        );
        assert_eq!(
            check(&t, Some(91), 10, &claude()),
            Err(Refusal::NotInPane),
            "outside every pane"
        );
        assert_eq!(
            check(&t, Some(72), 70, &claude()),
            Err(Refusal::OtherAgent),
            "another CLI"
        );
        assert_eq!(
            check(&t, Some(999), 10, &claude()),
            Err(Refusal::NotInPane),
            "a gone process"
        );
        assert_eq!(check(&t, None, 10, &claude()), Err(Refusal::NoPeer));
        assert_eq!(check(&t, Some(0), 10, &claude()), Err(Refusal::NoPeer));
        assert_eq!(check(&t, Some(22), 0, &claude()), Err(Refusal::NoPane));
        assert_eq!(
            check(&t, Some(10), 10, &claude()),
            Err(Refusal::NoAgent),
            "the pane's shell itself"
        );
    }

    #[test]
    fn chat_hooks_come_from_just_below_the_chat_child() {
        let t = tree();
        assert_eq!(below(&t, Some(22), 20), Ok(2));
        assert_eq!(below(&t, Some(23), 20), Ok(1));
        assert_eq!(below(&t, Some(34), 20), Err(Refusal::TooDeep));
        assert_eq!(below(&t, Some(41), 20), Err(Refusal::NotInPane));
        assert_eq!(below(&t, Some(41), 0), Err(Refusal::NoPane));
        assert_eq!(below(&t, None, 20), Err(Refusal::NoPeer));
    }

    #[test]
    fn a_parent_loop_ends() {
        let t = Fake(HashMap::from([(7, (8, "a", vec![])), (8, (7, "b", vec![]))]));
        assert_eq!(check(&t, Some(7), 10, &claude()), Err(Refusal::NotInPane));
        let s = Fake(HashMap::from([(7, (7, "a", vec![]))]));
        assert_eq!(check(&s, Some(7), 10, &claude()), Err(Refusal::NotInPane));
    }

    #[test]
    fn the_live_system_reads_this_process() {
        let me = std::process::id();
        assert!(System.parent(me).is_some());
        assert_eq!(System.process(me).map(|p| p.pid), Some(me));
        // This test binary is no agent's hook (an agent may run the suite far above it).
        assert!(check(&System, Some(me), 1, &claude()).is_err());
    }
}
