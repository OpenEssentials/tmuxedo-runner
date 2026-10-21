//! tmuxedo relay protocol v1 (docs/protocol.md), runner side.
//!
//! Binary data frames ([`codec`]) and JSON control frames ([`frames`]). Both
//! parsers NEVER panic on input: junk becomes `None`, which the caller counts
//! and ignores. The binary codec is checked byte-for-byte against the golden
//! fixtures in `packages/protocol/fixtures/` that the TypeScript side
//! generates, so the two implementations cannot drift silently.

pub mod addon;
pub mod chat;
pub mod checkpoint;
pub mod codec;
pub mod files;
pub mod frames;
pub mod fs;
pub mod names;
pub mod needs;
pub mod plugin_set;
pub mod plugins;
pub mod queued;
pub mod replay;
pub mod review;
pub mod role_set;
pub mod roles;
pub mod stats;
pub mod subagents;
pub mod usage;
pub mod wake;
pub mod when;

pub use chat::ChatFrame;
pub use codec::{DataFrame, DataKind, EncodeError, FLAG_EPHEMERAL, FLAG_GAP, KIND_CHAT, MAX_FRAME_BYTES};

pub const PROTOCOL_VERSION: u32 = 1;
/// The WebSocket subprotocol (formerly `aimux.v1`, which gateways still accept).
pub const SUBPROTOCOL: &str = "tmuxedo.v1";
/// The pre-rename subprotocol name, offered after [`SUBPROTOCOL`] so an older
/// gateway still accepts the socket.
pub const LEGACY_SUBPROTOCOL: &str = "aimux.v1";
/// What clients put in `Sec-WebSocket-Protocol`, preferred first.
pub const SUBPROTOCOLS: [&str; 2] = [SUBPROTOCOL, LEGACY_SUBPROTOCOL];

/// True for a subprotocol a gateway may select for this protocol version.
pub fn is_subprotocol(s: &str) -> bool {
    SUBPROTOCOLS.contains(&s.trim())
}

/// The domain string of a runner proof (docs/protocol.md "Runner proof").
pub const RUNNER_PROOF_DOMAIN: &str = "tmuxedo-runner-proof-v1";

/// What a key-bound runner signs to answer the gateway's `challenge`:
/// `tmuxedo-runner-proof-v1\n<nonce>\n<token lookup>` (fixtures/runner-proof.json).
pub fn runner_proof_input(nonce: &str, lookup: &str) -> String {
    format!("{RUNNER_PROOF_DOMAIN}\n{nonce}\n{lookup}")
}

/// Close codes (docs/protocol.md "Close codes").
pub mod close {
    pub const GOING_AWAY: u16 = 1001;
    pub const REPLACED: u16 = 4001;
    pub const UNSUPPORTED_VERSION: u16 = 4002;
    pub const REVOKED: u16 = 4003;
    pub const MACHINE_OFFLINE: u16 = 4004;
}
