//! Phase 14 (docs/design/secrets-and-env.md) end to end, in one test binary: the masker,
//! sealed delivery and the mechanisms, the hook guard and exposure scan, the shared
//! fixtures, the ssh-agent against real sshd, and project env.

mod common;

#[path = "secrets/delivery.rs"]
mod delivery;
#[path = "secrets/fixtures.rs"]
mod fixtures;
#[path = "secrets/guard.rs"]
mod guard;
#[path = "secrets/mask.rs"]
mod mask;
#[path = "secrets/project_env.rs"]
mod project_env;
#[path = "secrets/ssh_agent.rs"]
mod ssh_agent;

/// Wait until the runner has applied a frame sent over the gateway, seen
/// through a file it writes while applying it. The gateway socket and the
/// local socket are separate channels: without this, a local request sent
/// right after the frame may be served before the frame is.
pub async fn wait_file_has(path: &std::path::Path, needle: &str) {
    let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if std::fs::read_to_string(path).is_ok_and(|s| s.contains(needle)) {
            return;
        }
        assert!(
            std::time::Instant::now() < until,
            "{} never held {needle:?}",
            path.display()
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}
