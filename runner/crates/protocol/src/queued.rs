//! Queued prompts (docs/design/away-from-desk.md D-AD6, docs/protocol.md
//! "Queued prompts"): a prompt typed while the machine was offline, sealed to
//! the runner's key by web and handed over by `queued.deliver` when the
//! machine is back. `packages/protocol/src/secrets.ts` holds the same names.

/// The runner hello feature (`hello.runner.features`).
pub const FEATURE_QUEUED: &str = "queued_prompts";
/// The one method: open, check and deliver a sealed prompt.
pub const METHOD_DELIVER: &str = "queued.deliver";
/// A prompt's text, as for `chat.send`.
pub const MAX_TEXT_BYTES: usize = 64 * 1024;
/// The sealed blob (text plus target, base64url, AEAD tag).
pub const MAX_SEALED_BYTES: usize = 96 * 1024;
/// How many delivered ids the runner remembers (exactly-once on a resend).
pub const REMEMBERED: usize = 512;

/// The AEAD context a queued prompt is sealed under: bound to the machine
/// and the prompt's id, so a blob replayed under another id or to another
/// machine does not open.
pub fn context(machine: &str, id: &str) -> String {
    format!("{machine}|queued:{id}")
}

/// A queued prompt id: `[A-Za-z0-9_-]{8,64}`.
pub fn valid_id(id: &str) -> bool {
    (8..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_and_context() {
        assert!(valid_id("q_abcdef12"));
        assert!(!valid_id("short"));
        assert!(!valid_id("has space here"));
        assert!(!valid_id(&"a".repeat(65)));
        assert_eq!(context("m1", "q_abcdef12"), "m1|queued:q_abcdef12");
    }
}
