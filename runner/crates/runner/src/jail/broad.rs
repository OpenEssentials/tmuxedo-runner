//! Folders too broad to be a root (docs/design/file-browser.md §3.2): the
//! filesystem root, a home folder or anything above one, system folders,
//! shared temporary folders, and the runner's own folders.

use std::path::{Path, PathBuf};

/// Refused when the root IS one of these.
const EXACT: &[&str] = &[
    "/",
    "/var",
    "/usr",
    "/bin",
    "/sbin",
    "/opt",
    "/private",
    "/private/var",
    "/private/tmp",
    "/System",
    "/Library",
    "/Applications",
    "/tmp",
    "/var/tmp",
    "/Users/Shared",
    "/home",
    "/Users",
    "/root",
    "/mnt",
    "/media",
    "/Volumes",
];

/// Refused when the root is one of these or anything inside one.
const SUBTREE: &[&str] = &[
    "/etc",
    "/boot",
    "/dev",
    "/proc",
    "/sys",
    "/run",
    "/var/run",
    "/private/etc",
    "/private/var/run",
    "/var/lib/kubelet",
];

/// What the too-broad rule needs to know about this machine.
#[derive(Debug, Clone, Default)]
pub struct Guard {
    /// The home folder of the runner's user (its real path).
    pub home: Option<PathBuf>,
    /// The runner's own folders (state, config, the files root): a root may
    /// not be one, hold one or be inside one.
    pub own: Vec<PathBuf>,
}

/// Is `real` (a root's real, absolute path) too broad to browse?
pub fn too_broad(real: &Path, guard: &Guard) -> bool {
    if !real.is_absolute() {
        return true;
    }
    let s = real.to_string_lossy();
    if EXACT.iter().any(|e| Path::new(e) == real) {
        return true;
    }
    if SUBTREE.iter().any(|e| real.starts_with(e)) {
        return true;
    }
    // `/lib`, `/lib64`, `/libx32`…
    let mut parts = real.components().skip(1);
    if let (Some(first), None) = (parts.next(), parts.next()) {
        if first.as_os_str().to_string_lossy().starts_with("lib") {
            return true;
        }
    }
    // Any user's home folder itself (`/home/<user>`, `/Users/<user>`).
    for base in ["/home", "/Users"] {
        if real.parent() == Some(Path::new(base)) {
            return true;
        }
    }
    // The runner's user's home, wherever it is, and anything above it.
    if guard.home.as_deref().is_some_and(|h| h.starts_with(real)) {
        return true;
    }
    // The runner's own folders, from either side.
    if guard.own.iter().any(|o| o.starts_with(real) || real.starts_with(o)) {
        return true;
    }
    s.contains('\0')
}
