//! `runner.toml`: server URL and the machine's `tmr.` (aimux-era: `amr.`) token (mode 0600).

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunnerConfig {
    /// `https://tmuxedo.dev`
    pub server: String,
    /// `tmr.<lookup>.<secret>` (or an aimux-era `amr.…`)
    pub token: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default = "yes")]
    pub auto_update: bool,
    /// Keep each pane's recent output on disk and replay it after a holder
    /// restart when the layout matches (off by default).
    #[serde(default)]
    pub pane_history: bool,
    /// Lines of history each pane keeps (decision 0009). Default 50,000; a
    /// hosted machine 20,000. `TMUXEDO_HISTORY_LINES` overrides it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_lines: Option<usize>,
    /// Memory cap for one pane's history lines, in MiB (default 32; hosted 8).
    /// `TMUXEDO_HISTORY_MIB` overrides it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_mib: Option<usize>,
    /// Inject Claude Code hooks through a PATH shim (on by default).
    #[serde(default = "yes")]
    pub hooks: bool,
    /// Root for `worktree.create` checkouts (`~` allowed); default
    /// `~/.tmuxedo/worktrees`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_dir: Option<String>,
    /// Labels that let headless automation start tasks here (docs/design/headless-tasks.md §4);
    /// empty (the default) = no automation reaches this machine.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub automation_labels: Vec<String>,
}

fn yes() -> bool {
    true
}

/// Pane history size (decision 0009): lines and a memory cap per pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistorySize {
    pub lines: usize,
    pub bytes: usize,
}

impl HistorySize {
    pub const DEFAULT: Self = Self {
        lines: 50_000,
        bytes: 32 << 20,
    };
    /// Hosted machines share a pod's memory request (512 MiB + a share per agent).
    pub const HOSTED: Self = Self {
        lines: 20_000,
        bytes: 8 << 20,
    };

    /// Environment, then `runner.toml`, then the default for this kind of machine.
    pub fn resolve(env_lines: Option<&str>, env_mib: Option<&str>, cfg: Option<&RunnerConfig>, hosted: bool) -> Self {
        let base = if hosted { Self::HOSTED } else { Self::DEFAULT };
        let lines = env_lines
            .and_then(|s| s.trim().parse().ok())
            .or(cfg.and_then(|c| c.history_lines))
            .unwrap_or(base.lines)
            .min(tmuxedo_emu::MAX_HISTORY_LINES);
        let mib: usize = env_mib
            .and_then(|s| s.trim().parse().ok())
            .or(cfg.and_then(|c| c.history_mib))
            .unwrap_or(base.bytes >> 20)
            .clamp(1, 1024);
        Self {
            lines,
            bytes: mib << 20,
        }
    }
}

impl RunnerConfig {
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Write atomically with mode 0600 (the token is a credential).
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let text = toml::to_string(self).map_err(|e| e.to_string())?;
        write_private(path, text.as_bytes())
    }
}

/// Create parents (0700) and write `bytes` to `path` with mode 0600, atomically.
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let dir = path.parent().ok_or("path has no parent")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let tmp = dir.join(format!(
        ".{}.tmp{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("f"),
        std::process::id()
    ));
    {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|e| format!("{}: {e}", tmp.display()))?;
        f.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|e| e.to_string())?;
        f.write_all(bytes).map_err(|e| e.to_string())?;
        f.sync_all().map_err(|e| e.to_string())?;
    }
    std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
}

/// `https://host` → `wss://host/gw/runner`; `ws(s)://…` is used as given.
pub fn gateway_url(server: &str) -> String {
    let s = server.trim_end_matches('/');
    if s.starts_with("ws://") || s.starts_with("wss://") {
        return s.to_string();
    }
    if let Some(rest) = s.strip_prefix("https://") {
        return format!("wss://{rest}/gw/runner");
    }
    if let Some(rest) = s.strip_prefix("http://") {
        return format!("ws://{rest}/gw/runner");
    }
    format!("wss://{s}/gw/runner")
}

/// The HTTP origin for downloads and the device flow.
pub fn http_base(server: &str) -> String {
    let s = server.trim_end_matches('/');
    if let Some(rest) = s.strip_prefix("wss://") {
        return format!("https://{}", rest.split('/').next().unwrap_or(rest));
    }
    if let Some(rest) = s.strip_prefix("ws://") {
        return format!("http://{}", rest.split('/').next().unwrap_or(rest));
    }
    if s.starts_with("http://") || s.starts_with("https://") {
        return s.to_string();
    }
    format!("https://{s}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        assert_eq!(gateway_url("https://tmuxedo.dev/"), "wss://tmuxedo.dev/gw/runner");
        assert_eq!(gateway_url("http://127.0.0.1:9"), "ws://127.0.0.1:9/gw/runner");
        assert_eq!(gateway_url("ws://127.0.0.1:9/x"), "ws://127.0.0.1:9/x");
        assert_eq!(http_base("wss://a.b/gw/runner"), "https://a.b");
        assert_eq!(http_base("https://a.b/"), "https://a.b");
    }

    #[test]
    fn saves_0600_and_loads() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("sub/runner.toml");
        let c = RunnerConfig {
            server: "https://x".into(),
            token: "amr.a.b".into(),
            name: Some("box".into()),
            auto_update: true,
            pane_history: false,
            history_lines: Some(1234),
            history_mib: None,
            hooks: true,
            worktree_dir: Some("~/wt".into()),
            automation_labels: vec!["ci".into()],
        };
        c.save(&p).unwrap();
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(RunnerConfig::load(&p).unwrap(), c);
        let minimal: RunnerConfig = toml::from_str("server='s'\ntoken='t'").unwrap();
        assert!(minimal.auto_update);
        assert_eq!(minimal.history_lines, None);
    }

    #[test]
    fn history_size_resolves_env_then_config_then_machine_kind() {
        let cfg: RunnerConfig = toml::from_str("server='s'\ntoken='t'\nhistory_lines=9000\nhistory_mib=4").unwrap();
        assert_eq!(HistorySize::resolve(None, None, None, false), HistorySize::DEFAULT);
        assert_eq!(HistorySize::resolve(None, None, None, true), HistorySize::HOSTED);
        assert_eq!(
            HistorySize::resolve(None, None, Some(&cfg), true),
            HistorySize {
                lines: 9000,
                bytes: 4 << 20
            }
        );
        assert_eq!(
            HistorySize::resolve(Some(" 70000 "), Some("64"), Some(&cfg), false),
            HistorySize {
                lines: 70_000,
                bytes: 64 << 20
            }
        );
        // Nonsense falls through; absurd values are bounded.
        assert_eq!(HistorySize::resolve(Some("lots"), Some("0"), None, false).lines, 50_000);
        assert_eq!(HistorySize::resolve(None, Some("0"), None, false).bytes, 1 << 20);
        assert_eq!(
            HistorySize::resolve(Some("999999999"), None, None, false).lines,
            tmuxedo_emu::MAX_HISTORY_LINES
        );
    }
}
