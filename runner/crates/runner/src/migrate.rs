//! The aimux → tmuxedo rename, on disk. Runs at the start of every command,
//! is idempotent, and never deletes user data:
//!
//! - Directories `~/.local/state/aimux`, `$XDG_CONFIG_HOME/aimux` (or
//!   `~/.config/aimux`) and `~/.aimux` are renamed to their `tmuxedo` names and
//!   the old path is left as a symlink to the new one, so anything that still
//!   names the old path (a pane started before the update, a git worktree's
//!   recorded path, a script) keeps working. A marker file
//!   `.migrated-from-aimux` records the move. When both exist, entries missing
//!   from the new directory are moved over; clashing ones stay where they are
//!   (logged), and the old directory is only replaced by the symlink once empty.
//! - The binary: an `aimux-runner` file (what a self-update from an aimux-era
//!   version leaves behind) is renamed to `tmuxedo-runner` beside it, with
//!   `aimux-runner` (deprecated) and the CLI names `tmuxedo` and `aimux`
//!   (deprecated) as symlinks to it. Services that still start `aimux-runner`
//!   keep working through the symlink until `install-service` moves them.
//!
//! A move that fails is logged and the old path stays in use
//! ([`crate::paths::prefer_migrated`]).

use std::path::{Path, PathBuf};
use tmuxedo_holder::{home_dir, LEGACY_NAME, NAME};

pub const MARKER: &str = ".migrated-from-aimux";
pub const BIN: &str = "tmuxedo-runner";
pub const LEGACY_BIN: &str = "aimux-runner";
pub const CLI: &str = "tmuxedo";
pub const LEGACY_CLI: &str = "aimux";

#[derive(Debug, PartialEq, Eq)]
pub enum DirOutcome {
    /// No legacy directory, or it is already a symlink.
    Nothing,
    /// Renamed; the old path is now a symlink.
    Moved,
    /// Both existed: `moved` entries went over, `kept` clashing ones stayed in
    /// the old directory (which then stays a directory).
    Merged { moved: usize, kept: Vec<String> },
}

fn now() -> String {
    let s = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{s}")
}

fn write_marker(new: &Path, old: &Path) {
    let body = format!("from = {:?}\nat_unix = {}\n", old.display().to_string(), now());
    let _ = std::fs::write(new.join(MARKER), body);
}

fn link_back(old: &Path, new: &Path) -> Result<(), String> {
    // Relative when they share a parent (survives a moved HOME), else absolute.
    let target: PathBuf = match (old.parent(), new.parent(), new.file_name()) {
        (Some(a), Some(b), Some(n)) if a == b => PathBuf::from(n),
        _ => new.to_path_buf(),
    };
    match std::os::unix::fs::symlink(&target, old) {
        Ok(()) => Ok(()),
        // Another process (holder and runner start together) got there first.
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && old.is_symlink() => Ok(()),
        Err(e) => Err(format!("symlink {} -> {}: {e}", old.display(), target.display())),
    }
}

/// Move `old` to `new` (see the module docs).
pub fn migrate_dir(old: &Path, new: &Path) -> Result<DirOutcome, String> {
    if old.is_symlink() || !old.is_dir() {
        return Ok(DirOutcome::Nothing);
    }
    if !new.exists() && !new.is_symlink() {
        if let Some(p) = new.parent() {
            std::fs::create_dir_all(p).map_err(|e| format!("{}: {e}", p.display()))?;
        }
        match std::fs::rename(old, new) {
            Ok(()) => {}
            // Raced with a sibling process that already moved it.
            Err(_) if !old.exists() || old.is_symlink() => return Ok(DirOutcome::Nothing),
            Err(e) => return Err(format!("rename {} -> {}: {e}", old.display(), new.display())),
        }
        write_marker(new, old);
        link_back(old, new)?;
        return Ok(DirOutcome::Moved);
    }
    if !new.is_dir() {
        return Err(format!("{} exists and is not a directory", new.display()));
    }
    let mut moved = 0;
    let mut kept = vec![];
    let entries = std::fs::read_dir(old).map_err(|e| format!("{}: {e}", old.display()))?;
    for e in entries.flatten() {
        let name = e.file_name();
        let dest = new.join(&name);
        if dest.exists() || dest.is_symlink() {
            kept.push(name.to_string_lossy().into_owned());
            continue;
        }
        match std::fs::rename(e.path(), &dest) {
            Ok(()) => moved += 1,
            Err(_) => kept.push(name.to_string_lossy().into_owned()),
        }
    }
    kept.sort();
    write_marker(new, old);
    if kept.is_empty() && std::fs::remove_dir(old).is_ok() {
        link_back(old, new)?;
    }
    Ok(DirOutcome::Merged { moved, kept })
}

/// The directory pairs [`run`] migrates (old, new).
pub fn dir_pairs() -> Vec<(PathBuf, PathBuf)> {
    let home = home_dir();
    let mut v = vec![];
    if tmuxedo_holder::env_os("STATE_DIR").is_none() {
        v.push((crate::paths::legacy_state_dir(), crate::paths::default_state_dir()));
    }
    let ch = crate::paths::config_home();
    v.push((ch.join(LEGACY_NAME), ch.join(NAME)));
    v.push((home.join(format!(".{LEGACY_NAME}")), home.join(format!(".{NAME}"))));
    v
}

/// Rename an aimux-era binary file at `exe` to `tmuxedo-runner` and install
/// the compatibility symlinks. Returns the binary's (new) path when it changed.
pub fn migrate_binary(exe: &Path) -> Result<Option<PathBuf>, String> {
    let Some(dir) = exe.parent() else { return Ok(None) };
    if exe.file_name().and_then(|n| n.to_str()) != Some(LEGACY_BIN) || exe.is_symlink() {
        return Ok(None);
    }
    let new = dir.join(BIN);
    if new.exists() {
        // Two real binaries side by side: someone installed both; leave them.
        return Ok(None);
    }
    std::fs::rename(exe, &new).map_err(|e| format!("rename {} -> {}: {e}", exe.display(), new.display()))?;
    ensure_aliases(dir)?;
    Ok(Some(new))
}

/// `aimux-runner` (deprecated), `tmuxedo` (the CLI) and `aimux` (deprecated
/// CLI) as relative symlinks to `tmuxedo-runner` in `dir`, where absent.
pub fn ensure_aliases(dir: &Path) -> Result<(), String> {
    for name in [LEGACY_BIN, CLI, LEGACY_CLI] {
        let p = dir.join(name);
        if p.exists() || p.is_symlink() {
            continue;
        }
        match std::os::unix::fs::symlink(BIN, &p) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(format!("symlink {}: {e}", p.display())),
        }
    }
    Ok(())
}

/// Everything above; returns log lines (callers print them once a logger is up).
pub fn run() -> Vec<String> {
    let mut out = vec![];
    if tmuxedo_holder::env_os("NO_MIGRATE").is_some() {
        return out;
    }
    for (old, new) in dir_pairs() {
        match migrate_dir(&old, &new) {
            Ok(DirOutcome::Nothing) => {}
            Ok(DirOutcome::Moved) => out.push(format!(
                "migrated {} -> {} (old path is a symlink now)",
                old.display(),
                new.display()
            )),
            Ok(DirOutcome::Merged { moved, kept }) if kept.is_empty() => out.push(format!(
                "merged {} into {} ({moved} entries)",
                old.display(),
                new.display()
            )),
            Ok(DirOutcome::Merged { moved, kept }) => out.push(format!(
                "merged {} into {} ({moved} entries); left {} because {} already has them: {}",
                old.display(),
                new.display(),
                old.display(),
                new.display(),
                kept.join(", ")
            )),
            Err(e) => out.push(format!("migration of {} failed, still using it: {e}", old.display())),
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        let exe = std::fs::canonicalize(crate::update::strip_deleted(&exe)).unwrap_or(exe);
        match migrate_binary(&exe) {
            Ok(Some(new)) => out.push(format!(
                "renamed {} -> {} ({LEGACY_BIN}, {CLI} and {LEGACY_CLI} are symlinks to it)",
                exe.display(),
                new.display()
            )),
            Ok(None) => {}
            Err(e) => out.push(format!("binary rename skipped: {e}")),
        }
    }
    out
}

/// Point an aimux-era runner socket path at ours, so hooks in panes started
/// before the rename (`AIMUX_SOCKET_PATH`) still reach this runner. Only when
/// the legacy directory exists and is a separate place.
pub fn link_legacy_runner_socket(ours: &Path, legacy: &Path) {
    let Some(dir) = legacy.parent() else { return };
    if !dir.is_dir() {
        return;
    }
    let (Ok(a), Ok(b)) = (
        dir.canonicalize(),
        ours.parent().map_or(Err(()), |p| p.canonicalize().map_err(|_| ())),
    ) else {
        return;
    };
    if a == b {
        return; // the legacy dir is (a symlink to) ours: same socket already
    }
    let _ = std::fs::remove_file(legacy);
    if let Err(e) = std::os::unix::fs::symlink(ours, legacy) {
        crate::log!("linking {} -> {}: {e}", legacy.display(), ours.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn moves_and_leaves_a_symlink_once() {
        let d = tempfile::tempdir().unwrap();
        let old = d.path().join("state/aimux");
        let new = d.path().join("state/tmuxedo");
        fs::create_dir_all(old.join("sub")).unwrap();
        fs::write(old.join("topology.json"), "{}").unwrap();
        fs::write(old.join("sub/x"), "1").unwrap();
        assert_eq!(migrate_dir(&old, &new).unwrap(), DirOutcome::Moved);
        assert_eq!(fs::read_to_string(new.join("topology.json")).unwrap(), "{}");
        assert_eq!(fs::read_to_string(new.join("sub/x")).unwrap(), "1");
        assert!(new.join(MARKER).exists());
        assert!(old.is_symlink());
        assert_eq!(fs::read_link(&old).unwrap(), PathBuf::from("tmuxedo"));
        assert_eq!(
            fs::read_to_string(old.join("topology.json")).unwrap(),
            "{}",
            "old path still resolves"
        );
        // Idempotent.
        assert_eq!(migrate_dir(&old, &new).unwrap(), DirOutcome::Nothing);
        assert_eq!(fs::read_to_string(new.join("sub/x")).unwrap(), "1");
    }

    #[test]
    fn nothing_to_do_without_a_legacy_dir() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(
            migrate_dir(&d.path().join("aimux"), &d.path().join("tmuxedo")).unwrap(),
            DirOutcome::Nothing
        );
        assert!(!d.path().join("tmuxedo").exists());
    }

    #[test]
    fn merges_without_overwriting_and_keeps_clashes() {
        let d = tempfile::tempdir().unwrap();
        let old = d.path().join("aimux");
        let new = d.path().join("tmuxedo");
        fs::create_dir_all(&old).unwrap();
        fs::create_dir_all(&new).unwrap();
        fs::write(old.join("runner.toml"), "old").unwrap();
        fs::write(old.join("cli.toml"), "cli").unwrap();
        fs::write(new.join("runner.toml"), "new").unwrap();
        let r = migrate_dir(&old, &new).unwrap();
        assert_eq!(
            r,
            DirOutcome::Merged {
                moved: 1,
                kept: vec!["runner.toml".into()]
            }
        );
        assert_eq!(
            fs::read_to_string(new.join("runner.toml")).unwrap(),
            "new",
            "never overwritten"
        );
        assert_eq!(fs::read_to_string(new.join("cli.toml")).unwrap(), "cli");
        assert_eq!(
            fs::read_to_string(old.join("runner.toml")).unwrap(),
            "old",
            "never deleted"
        );
        assert!(old.is_dir() && !old.is_symlink());
        // Once the clash is resolved by hand, the next run finishes the job.
        fs::remove_file(old.join("runner.toml")).unwrap();
        assert_eq!(
            migrate_dir(&old, &new).unwrap(),
            DirOutcome::Merged { moved: 0, kept: vec![] }
        );
        assert!(old.is_symlink());
    }

    #[test]
    fn prefer_migrated_falls_back_to_an_unmoved_legacy_dir() {
        let d = tempfile::tempdir().unwrap();
        let old = d.path().join("aimux");
        let new = d.path().join("tmuxedo");
        assert_eq!(
            crate::paths::prefer_migrated(new.clone(), &old),
            new,
            "neither: the new one"
        );
        fs::create_dir_all(&old).unwrap();
        assert_eq!(
            crate::paths::prefer_migrated(new.clone(), &old),
            old,
            "move failed: keep the data"
        );
        migrate_dir(&old, &new).unwrap();
        assert_eq!(crate::paths::prefer_migrated(new.clone(), &old), new);
    }

    #[test]
    fn binary_rename_installs_aliases() {
        let d = tempfile::tempdir().unwrap();
        let exe = d.path().join(LEGACY_BIN);
        fs::write(&exe, "#!/bin/sh\necho hi\n").unwrap();
        let new = migrate_binary(&exe).unwrap().unwrap();
        assert_eq!(new, d.path().join(BIN));
        assert_eq!(fs::read_to_string(&new).unwrap(), "#!/bin/sh\necho hi\n");
        for alias in [LEGACY_BIN, CLI, LEGACY_CLI] {
            let p = d.path().join(alias);
            assert!(p.is_symlink(), "{alias}");
            assert_eq!(fs::read_link(&p).unwrap(), PathBuf::from(BIN));
        }
        // The symlink is not migrated again; an existing tmuxedo-runner is left alone.
        assert_eq!(migrate_binary(&exe).unwrap(), None);
        assert_eq!(migrate_binary(&new).unwrap(), None);
    }

    #[test]
    fn binary_rename_leaves_two_real_binaries_and_other_names_alone() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join(LEGACY_BIN), "a").unwrap();
        fs::write(d.path().join(BIN), "b").unwrap();
        assert_eq!(migrate_binary(&d.path().join(LEGACY_BIN)).unwrap(), None);
        assert_eq!(fs::read_to_string(d.path().join(LEGACY_BIN)).unwrap(), "a");
        fs::write(d.path().join("my-runner"), "c").unwrap();
        assert_eq!(migrate_binary(&d.path().join("my-runner")).unwrap(), None);
    }

    #[test]
    fn legacy_runner_socket_link() {
        let d = tempfile::tempdir().unwrap();
        let ours = d.path().join("tmuxedo/runner.sock");
        let legacy = d.path().join("aimux/runner.sock");
        fs::create_dir_all(ours.parent().unwrap()).unwrap();
        // No legacy dir: nothing created.
        link_legacy_runner_socket(&ours, &legacy);
        assert!(!legacy.parent().unwrap().exists());
        // A legacy dir with a stale socket file: replaced by a link to ours.
        fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        fs::write(&legacy, "").unwrap();
        link_legacy_runner_socket(&ours, &legacy);
        assert_eq!(fs::read_link(&legacy).unwrap(), ours);
        // The legacy dir is a symlink to ours: left alone (it IS our socket).
        let d2 = tempfile::tempdir().unwrap();
        let ours2 = d2.path().join("tmuxedo/runner.sock");
        fs::create_dir_all(ours2.parent().unwrap()).unwrap();
        fs::write(&ours2, "real").unwrap();
        std::os::unix::fs::symlink("tmuxedo", d2.path().join("aimux")).unwrap();
        link_legacy_runner_socket(&ours2, &d2.path().join("aimux/runner.sock"));
        assert!(!ours2.is_symlink());
        assert_eq!(fs::read_to_string(&ours2).unwrap(), "real");
    }
}
