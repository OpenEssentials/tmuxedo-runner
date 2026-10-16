//! Where things live. Every path can be overridden by an env var (tests, pods);
//! `TMUXEDO_*` is read first, then the deprecated `AIMUX_*`.
//!
//! The directories were named `aimux` before the rename; [`crate::migrate`]
//! moves them on start and leaves the old path as a symlink. Should a move
//! fail, the functions here keep using the old directory rather than start
//! from an empty one (a lost runner.toml would unenrol the machine).

use std::path::{Path, PathBuf};
use tmuxedo_holder::{env_os, home_dir, LEGACY_NAME, NAME};

/// `new` unless only the (unmigrated) legacy directory exists.
pub fn prefer_migrated(new: PathBuf, legacy: &Path) -> PathBuf {
    if !new.exists() && legacy.is_dir() && !legacy.is_symlink() {
        return legacy.to_path_buf();
    }
    new
}

pub fn default_state_dir() -> PathBuf {
    home_dir().join(".local/state").join(NAME)
}

pub fn legacy_state_dir() -> PathBuf {
    home_dir().join(".local/state").join(LEGACY_NAME)
}

/// `$TMUXEDO_STATE_DIR` or `~/.local/state/tmuxedo` (topology, event seq, logs).
pub fn state_dir() -> PathBuf {
    if let Some(d) = env_os("STATE_DIR") {
        return PathBuf::from(d);
    }
    prefer_migrated(default_state_dir(), &legacy_state_dir())
}

/// `$XDG_CONFIG_HOME/tmuxedo` or `~/.config/tmuxedo`.
pub fn config_dir() -> PathBuf {
    prefer_migrated(config_home().join(NAME), &legacy_config_dir())
}

pub fn legacy_config_dir() -> PathBuf {
    config_home().join(LEGACY_NAME)
}

/// `$TMUXEDO_CONFIG` or `<config_dir>/runner.toml`.
pub fn config_path() -> PathBuf {
    if let Some(p) = env_os("CONFIG") {
        return PathBuf::from(p);
    }
    config_dir().join("runner.toml")
}

pub fn config_home() -> PathBuf {
    match std::env::var_os("XDG_CONFIG_HOME") {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => home_dir().join(".config"),
    }
}

/// `~/.tmuxedo/worktrees` (formerly `~/.aimux/worktrees`, migrated like the others).
pub fn default_worktree_dir(home: &Path) -> PathBuf {
    prefer_migrated(home.join(format!(".{NAME}")), &home.join(format!(".{LEGACY_NAME}"))).join("worktrees")
}

pub fn holder_socket() -> PathBuf {
    tmuxedo_holder::default_socket()
}

/// The runner's local NDJSON socket (runner API + hook shim):
/// `$TMUXEDO_RUNNER_SOCKET` or `<runtime_dir>/runner.sock`.
pub fn runner_socket() -> PathBuf {
    env_os("RUNNER_SOCKET")
        .map(PathBuf::from)
        .unwrap_or_else(|| tmuxedo_holder::runtime_dir().join("runner.sock"))
}

/// Where an aimux-era runner bound its socket, if that is a different place.
/// Panes started before the rename have it in `AIMUX_SOCKET_PATH`; the runner links it to [`runner_socket`].
pub fn legacy_runner_socket() -> Option<PathBuf> {
    if env_os("RUNNER_SOCKET").is_some() {
        return None;
    }
    tmuxedo_holder::legacy_runtime_dir().map(|d| d.join("runner.sock"))
}
