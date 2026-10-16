//! The tmuxedo PTY holder (decision 0001): a separate, rarely changing process
//! that owns PTYs, child process groups and a byte ring per pane, so the
//! runner can restart or self-update without killing sessions.

pub mod client;
pub mod queries;
pub mod server;
pub mod wire;

pub use client::{HolderClient, HolderEvent};
pub use server::{scrubbed_env_key, Config, DEFAULT_RING};
pub use wire::{PaneInfo, SpawnSpec, HOLDER_PROTO};

/// Build version: CI sets `TMUXEDO_VERSION` (formerly `AIMUX_VERSION`) to the release tag.
pub const VERSION: &str = match option_env!("TMUXEDO_VERSION") {
    Some(v) => v,
    None => match option_env!("AIMUX_VERSION") {
        Some(v) => v,
        None => concat!(env!("CARGO_PKG_VERSION"), "-dev"),
    },
};

/// A fingerprint of the holder's own code (build.rs): equal builds mean a
/// running holder and a newly installed binary run the same holder code.
pub const BUILD: &str = env!("TMUXEDO_HOLDER_BUILD");

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The product's name in paths, unit names and env vars (formerly `aimux`).
pub const NAME: &str = "tmuxedo";
/// The former name: its paths and `AIMUX_*` env vars are still read (deprecated).
pub const LEGACY_NAME: &str = "aimux";

/// `$TMUXEDO_<suffix>`, else the deprecated `$AIMUX_<suffix>`. Empty counts as unset.
pub fn env_os(suffix: &str) -> Option<OsString> {
    [format!("TMUXEDO_{suffix}"), format!("AIMUX_{suffix}")]
        .into_iter()
        .find_map(|k| std::env::var_os(k).filter(|v| !v.is_empty()))
}

/// [`env_os`] as UTF-8.
pub fn env_var(suffix: &str) -> Option<String> {
    env_os(suffix).and_then(|v| v.into_string().ok())
}

fn runtime_dir_named(name: &str) -> PathBuf {
    if let Some(d) = std::env::var_os("XDG_RUNTIME_DIR") {
        if !d.is_empty() {
            return PathBuf::from(d).join(name);
        }
    }
    home_dir().join(".local/state").join(name)
}

/// Directory for sockets: `$TMUXEDO_RUNTIME_DIR`, else `$XDG_RUNTIME_DIR/tmuxedo`,
/// else `~/.local/state/tmuxedo`.
pub fn runtime_dir() -> PathBuf {
    if let Some(d) = env_os("RUNTIME_DIR") {
        return PathBuf::from(d);
    }
    runtime_dir_named(NAME)
}

/// Where an aimux-era holder and runner put their sockets
/// (`$XDG_RUNTIME_DIR/aimux` or `~/.local/state/aimux`); None when the
/// runtime dir is set explicitly.
pub fn legacy_runtime_dir() -> Option<PathBuf> {
    if env_os("RUNTIME_DIR").is_some() {
        return None;
    }
    Some(runtime_dir_named(LEGACY_NAME))
}

pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

/// The holder's socket: `$TMUXEDO_HOLDER_SOCKET` or `<runtime_dir>/holder.sock`.
pub fn default_socket() -> PathBuf {
    env_os("HOLDER_SOCKET")
        .map(PathBuf::from)
        .unwrap_or_else(|| runtime_dir().join("holder.sock"))
}

/// An aimux-era holder's socket, unless a socket or runtime dir is set
/// explicitly. Such a holder keeps its sessions until it exits: the runner
/// attaches to it, and a new holder waits for it ([`holder_alive`]) instead
/// of starting an empty second one.
pub fn legacy_socket() -> Option<PathBuf> {
    if env_os("HOLDER_SOCKET").is_some() {
        return None;
    }
    let p = legacy_runtime_dir()?.join("holder.sock");
    // Same file as ours (the legacy dir is a symlink to the new one): not separate.
    if same_file(&p, &default_socket()) {
        return None;
    }
    Some(p)
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (
        a.parent().and_then(|d| d.canonicalize().ok()),
        b.parent().and_then(|d| d.canonicalize().ok()),
    ) {
        (Some(x), Some(y)) => x == y && a.file_name() == b.file_name(),
        _ => false,
    }
}

/// True while a holder holds the lock beside `socket` (`holder.lock`, flock'd
/// for the holder's lifetime). The probe takes the lock only momentarily.
pub fn holder_alive(socket: &Path) -> bool {
    use std::os::fd::AsRawFd;
    let Ok(f) = std::fs::OpenOptions::new()
        .write(true)
        .open(socket.with_extension("lock"))
    else {
        return false;
    };
    // SAFETY: flock on a valid fd; the lock is released when `f` drops.
    unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) != 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_held_lock_means_alive() {
        let d = tempfile::tempdir().unwrap();
        let sock = d.path().join("holder.sock");
        assert!(!holder_alive(&sock), "no lock file: nobody");
        let lock = std::fs::File::create(sock.with_extension("lock")).unwrap();
        assert!(!holder_alive(&sock), "unlocked file: nobody");
        use std::os::fd::AsRawFd;
        assert_eq!(
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        assert!(holder_alive(&sock));
        drop(lock);
        assert!(!holder_alive(&sock));
    }
}
