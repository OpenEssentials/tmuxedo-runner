//! Self-update: `<server>/dl/manifest.json` →
//! `{version, files: {"linux-x86_64": {url, sha256}, …}}`. The new binary is
//! downloaded beside the current one, its sha256 checked, and renamed over
//! it atomically. Only the runner re-execs; the holder keeps running the old
//! inode until an explicit `update --holder`.

use crate::util::sha256_hex;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone, Deserialize)]
pub struct FileEntry {
    pub url: String,
    pub sha256: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    pub version: String,
    pub files: BTreeMap<String, FileEntry>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Current,
    Updated(String),
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(300))
        .user_agent(&format!("tmuxedo-runner/{}", crate::VERSION))
        .build()
}

pub fn fetch_manifest(base: &str) -> Result<Manifest, String> {
    let url = format!("{}/dl/manifest.json", base.trim_end_matches('/'));
    agent()
        .get(&url)
        .call()
        .map_err(|e| format!("{url}: {e}"))?
        .into_json()
        .map_err(|e| format!("{url}: {e}"))
}

/// Resolve a manifest URL (absolute, or relative to `<base>/dl/`).
pub fn resolve(base: &str, url: &str) -> String {
    if url.starts_with("http://") || url.starts_with("https://") {
        url.to_string()
    } else if let Some(p) = url.strip_prefix('/') {
        format!("{}/{p}", base.trim_end_matches('/'))
    } else {
        format!("{}/dl/{url}", base.trim_end_matches('/'))
    }
}

/// What to do given a manifest: None if current, else the entry for this platform.
pub fn plan(m: &Manifest, current: &str, platform: &str) -> Result<Option<FileEntry>, String> {
    if m.version == current {
        return Ok(None);
    }
    m.files
        .get(platform)
        .cloned()
        .map(Some)
        .ok_or_else(|| format!("manifest {} has no build for {platform}", m.version))
}

/// The most an update download may hold.
const DOWNLOAD_MAX: u64 = 1024 * 1024 * 1024;

/// Verify `bytes` and atomically replace `target` with them (mode 0755).
pub fn install_bytes(target: &Path, bytes: &[u8], sha256: &str) -> Result<(), String> {
    let got = sha256_hex(bytes);
    if !got.eq_ignore_ascii_case(sha256.trim()) {
        return Err(format!("sha256 mismatch: expected {sha256}, got {got}"));
    }
    let dir = target.parent().ok_or("binary has no directory")?;
    let tmp = dir.join(format!(".tmuxedo-runner.new.{}", std::process::id()));
    // Durable before the swap (audit P2-16c): a crash must leave either the
    // old binary or the whole new one, never a truncated file.
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
        f.write_all(bytes).map_err(|e| format!("{}: {e}", tmp.display()))?;
        f.set_permissions(std::fs::Permissions::from_mode(0o755))
            .map_err(|e| e.to_string())?;
        f.sync_all().map_err(|e| format!("{}: {e}", tmp.display()))?;
    }
    std::fs::rename(&tmp, target).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("{}: {e}", target.display())
    })?;
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

static EXE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Record this binary's path at startup, before any update replaces it.
/// After an atomic swap, Linux's `/proc/self/exe` (what
/// `std::env::current_exe` reads) names the old, unlinked inode as
/// `<path> (deleted)`, so resolving it then makes the re-exec fail with
/// ENOENT and the runner never comes back (found by the live self-update
/// proof, r000001 → r000007).
pub fn remember_exe() {
    if let Ok(p) = resolve_exe() {
        let _ = EXE.set(p);
    }
}

/// `<path> (deleted)` → `<path>`.
pub fn strip_deleted(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    PathBuf::from(s.strip_suffix(" (deleted)").unwrap_or(&s).to_string())
}

fn resolve_exe() -> Result<PathBuf, String> {
    let p = strip_deleted(&std::env::current_exe().map_err(|e| e.to_string())?);
    Ok(std::fs::canonicalize(&p).unwrap_or(p))
}

/// This binary's path (as recorded at startup when [`remember_exe`] ran).
pub fn current_exe() -> Result<PathBuf, String> {
    match EXE.get() {
        Some(p) => Ok(p.clone()),
        None => resolve_exe(),
    }
}

/// Check and, if needed, install. Never touches the holder.
pub fn update_runner(base: &str) -> Result<Outcome, String> {
    if crate::VERSION.ends_with("-dev") && tmuxedo_holder::env_os("UPDATE_DEV").is_none() {
        return Ok(Outcome::Current);
    }
    let m = fetch_manifest(base)?;
    let Some(entry) = plan(&m, crate::VERSION, &crate::util::platform_key())? else {
        return Ok(Outcome::Current);
    };
    // Already these exact bytes (e.g. a build whose version string differs
    // from the manifest's): nothing to install, no re-exec loop.
    let exe = current_exe()?;
    if std::fs::read(&exe).is_ok_and(|b| sha256_hex(&b).eq_ignore_ascii_case(entry.sha256.trim())) {
        return Ok(Outcome::Current);
    }
    let url = resolve(base, &entry.url);
    let resp = agent().get(&url).call().map_err(|e| format!("{url}: {e}"))?;
    // A release binary is tens of MiB; an unstripped debug build (the update
    // test serves one) runs past 200. One byte over the cap is read so that an
    // oversized download says so instead of failing as a sha256 mismatch.
    let mut bytes = vec![];
    resp.into_reader()
        .take(DOWNLOAD_MAX + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("{url}: {e}"))?;
    if bytes.len() as u64 > DOWNLOAD_MAX {
        return Err(format!("{url}: larger than {} MiB", DOWNLOAD_MAX >> 20));
    }
    install_bytes(&exe, &bytes, &entry.sha256)?;
    Ok(Outcome::Updated(m.version))
}

/// Replace this process with the (new) binary, same arguments.
pub fn reexec() -> std::io::Error {
    use std::os::unix::process::CommandExt;
    let exe = match current_exe() {
        Ok(e) => e,
        Err(e) => return std::io::Error::other(e),
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::Command::new(exe).args(args).exec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deleted_suffix_is_stripped() {
        assert_eq!(
            strip_deleted(Path::new("/home/u/.local/bin/tmuxedo-runner (deleted)")),
            PathBuf::from("/home/u/.local/bin/tmuxedo-runner")
        );
        assert_eq!(strip_deleted(Path::new("/usr/bin/x")), PathBuf::from("/usr/bin/x"));
    }

    fn manifest() -> Manifest {
        serde_json::from_str(
            r#"{"version":"main-r000012-abc1234","files":{
                "linux-x86_64":{"url":"tmuxedo-runner-linux-x86_64","sha256":"00"},
                "darwin-aarch64":{"url":"https://x/y","sha256":"11"}}}"#,
        )
        .unwrap()
    }

    #[test]
    fn plans() {
        let m = manifest();
        assert!(plan(&m, "main-r000012-abc1234", "linux-x86_64").unwrap().is_none());
        assert_eq!(
            plan(&m, "old", "linux-x86_64").unwrap().unwrap().url,
            "tmuxedo-runner-linux-x86_64"
        );
        assert!(plan(&m, "old", "linux-riscv64").is_err());
        assert_eq!(
            resolve("https://a.b", "tmuxedo-runner-linux-x86_64"),
            "https://a.b/dl/tmuxedo-runner-linux-x86_64"
        );
        assert_eq!(resolve("https://a.b/", "/dl/x"), "https://a.b/dl/x");
        assert_eq!(resolve("https://a.b", "https://c/d"), "https://c/d");
    }

    #[test]
    fn installs_atomically_and_checks_hash() {
        let d = tempfile::tempdir().unwrap();
        let target = d.path().join("tmuxedo-runner");
        std::fs::write(&target, b"old").unwrap();
        assert!(install_bytes(&target, b"new", "deadbeef").is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"old");
        install_bytes(&target, b"new", &sha256_hex(b"new")).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
        let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);
        assert_eq!(std::fs::read_dir(d.path()).unwrap().count(), 1, "no temp files left");
    }
}
