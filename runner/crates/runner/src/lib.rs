//! tmuxedo runner: the daemon that connects a machine to the tmuxedo gateway,
//! plus the small subcommands around it (enrol, hook shim, service install,
//! self-update). The PTY holder lives in `tmuxedo-holder`; the emulator in
//! `tmuxedo-emu`; the wire protocol in `tmuxedo-protocol`.

pub mod addon_gen;
pub mod addon_test;
pub mod agent;
pub mod ancestry;
pub mod api;
pub mod arrange;
pub mod awake;
pub mod chat;
pub mod checkpoint;
pub mod claude_auth;
pub mod cli;
pub mod config;
pub mod confirm;
pub mod daemon;
pub mod enrol;
pub mod events;
pub mod files;
pub mod fs;
pub mod gateway;
pub mod glance;
pub mod guardrails;
pub mod history;
pub mod hook;
pub mod inject;
pub mod jail;
pub mod keys;
pub mod launch_config;
pub mod local;
pub mod login_relay;
pub mod mcp_pin;
pub mod mcp_test;
pub mod memory_warn;
pub mod migrate;
pub mod mirror;
pub mod needs;
pub mod needs_ci;
pub mod needs_files;
pub mod open_url;
pub mod outbox;
pub mod pair;
pub mod pane_rt;
pub mod paths;
pub mod plugin;
pub mod plugin_api;
pub mod plugin_discover;
pub mod plugin_gen;
pub mod plugin_rt;
pub mod plugin_set;
pub mod plugin_wire;
pub mod project;
pub mod queued;
pub mod replay;
pub mod reprobe;
pub mod review;
pub mod risk;
pub mod role_launch;
pub mod role_set;
pub mod secrets;
pub mod service;
pub mod signer;
pub mod skill;
pub mod stats;
pub mod subagents;
pub mod task;
pub mod topology;
pub mod update;
pub mod usage;
pub mod util;
pub mod wake;
pub mod watchdog;
pub mod worktree;
pub mod worktree_api;

pub use tmuxedo_holder::VERSION;

/// Exit status meaning "do not restart me" (systemd `RestartPreventExitStatus=78`):
/// the token was revoked (4003) or another runner replaced this one (4001).
pub const EXIT_NO_RESTART: i32 = 78;

#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => {
        eprintln!("{} {}", $crate::util::iso_now(), format_args!($($arg)*))
    };
}
