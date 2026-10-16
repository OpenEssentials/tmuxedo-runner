//! Launch and shell configuration from web (`config` frame, docs/protocol.md):
//! `claude {args}` and `shell {aliases, env, rc}`. Persisted in
//! `<state>/config.json`; applied to every NEW claude launch (the PATH shim
//! embeds the args as single-quoted literals: exec, never interpreted by a
//! shell) and every NEW pane's shell (an rc wrapper that sources the user's
//! own rc first, then `~/.config/tmuxedo/shell-init.sh`).

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const MAX_ARGS: usize = 64;
pub const MAX_ARG_LEN: usize = 1024;
pub const MAX_ENTRIES: usize = 256;
pub const MAX_RC: usize = 64 * 1024;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ShellConfig {
    #[serde(default)]
    pub aliases: BTreeMap<String, String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub rc: String,
}

impl ShellConfig {
    pub fn is_empty(&self) -> bool {
        self.aliases.is_empty() && self.env.is_empty() && self.rc.trim().is_empty()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LaunchConfig {
    #[serde(default)]
    pub claude_args: Vec<String>,
    #[serde(default)]
    pub shell: ShellConfig,
    /// `config.workDir`: the folder new panes start in (None = the runner's
    /// default, [`DEFAULT_WORK_DIR`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_dir: Option<String>,
    /// `config.projects`: env per project (crate::project), on top of `shell.env`.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub projects: crate::project::Projects,
}

/// `config.projects`: keys ≤ 512 printable bytes; names as shell env names
/// (not runner-managed); values one line ≤ 4 KiB, or null (unset here).
/// Invalid entries are dropped and named in the second list.
pub fn validate_projects(v: &Value) -> Result<(crate::project::Projects, Vec<String>), String> {
    let o = v.as_object().ok_or("projects must be an object")?;
    let mut out = crate::project::Projects::new();
    let mut refused = vec![];
    for (key, env) in o.iter().take(MAX_ENTRIES) {
        if key.is_empty() || key.len() > 512 || key.chars().any(|c| c.is_control()) {
            refused.push(format!("project {key:?}"));
            continue;
        }
        let Some(env) = env.as_object() else {
            refused.push(format!("project {key}"));
            continue;
        };
        let mut m = BTreeMap::new();
        for (n, val) in env.iter().take(MAX_ENTRIES) {
            let ok_name = valid_env_name(n) && !protected_env(n);
            match val {
                Value::Null if ok_name => {
                    m.insert(n.clone(), None);
                }
                Value::String(s) if ok_name && s.len() <= 4096 && !s.contains(['\n', '\r', '\0']) => {
                    m.insert(n.clone(), Some(s.clone()));
                }
                _ => refused.push(format!("{key}: {n}")),
            }
        }
        out.insert(key.clone(), m);
    }
    Ok((out, refused))
}

/// Where new panes start unless `config.workDir` says otherwise. Not `$HOME`:
/// Claude Code never remembers "trust this folder" for the home directory
/// (2.1.289 keeps that answer for the session only), so an agent started or
/// resumed there asks again every time.
pub const DEFAULT_WORK_DIR: &str = "~/work";

/// The hosted machines' home before the rename (now a symlink to
/// `/home/tmuxedo`); panes recorded under it are restored under `$HOME`.
pub const LEGACY_HOME: &str = "/home/aimux";

pub const MAX_WORK_DIR: usize = 1024;

/// `config.workDir`: `~`, `~/…` or an absolute path; `""` = back to the
/// default (Ok(None)).
pub fn validate_work_dir(v: &Value) -> Result<Option<String>, String> {
    let Some(s) = v.as_str() else {
        return Err("workDir must be a string".into());
    };
    if s.is_empty() {
        return Ok(None);
    }
    if s.len() > MAX_WORK_DIR || s.contains('\0') || has_control(s) {
        return Err(format!("invalid workDir {s:?}"));
    }
    if !(s == "~" || s.starts_with("~/") || s.starts_with('/')) {
        return Err(format!("workDir {s:?} must be absolute or start with ~/"));
    }
    Ok(Some(s.to_string()))
}

/// The default working folder for `home`: `configured` (or
/// [`DEFAULT_WORK_DIR`]) with `~` expanded, created if missing, canonical
/// (so the cwd an agent sees is the path its trust is stored under).
pub fn resolve_work_dir(home: &str, configured: Option<&str>) -> std::io::Result<String> {
    let p = crate::worktree::expand_tilde(configured.unwrap_or(DEFAULT_WORK_DIR), home);
    std::fs::create_dir_all(&p)?;
    Ok(std::fs::canonicalize(&p)?.to_string_lossy().into_owned())
}

/// `recorded` with a leading `legacy` home (the pre-rename hosted home)
/// replaced by `home`; anything else unchanged.
pub fn rewrite_legacy_home(recorded: &str, home: &str, legacy: &str) -> String {
    if home == legacy || home.is_empty() {
        return recorded.to_string();
    }
    match recorded.strip_prefix(legacy) {
        Some("") => home.to_string(),
        Some(rest) if rest.starts_with('/') => format!("{}{rest}", home.trim_end_matches('/')),
        _ => recorded.to_string(),
    }
}

/// `path` is the home directory itself (literally, through the legacy home,
/// or through a symlink).
pub fn is_home(path: &str, home: &str, legacy: &str) -> bool {
    let trim = |s: &str| -> String {
        let t = s.trim_end_matches('/');
        if t.is_empty() {
            "/".into()
        } else {
            t.into()
        }
    };
    let p = trim(&rewrite_legacy_home(path, home, legacy));
    if p == trim(home) {
        return true;
    }
    match (std::fs::canonicalize(&p), std::fs::canonicalize(home)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

fn has_control(s: &str) -> bool {
    s.chars().any(|c| c.is_control() && c != '\t')
}

/// `claude.args`: each starts with `-`, or is the value of the flag before
/// it (a `-x`/`--long` without `=`); no control characters; bounded.
pub fn validate_args(v: &Value) -> Result<Vec<String>, String> {
    let Some(arr) = v.as_array() else {
        return Err("claude.args must be an array of strings".into());
    };
    if arr.len() > MAX_ARGS {
        return Err(format!("at most {MAX_ARGS} claude args"));
    }
    let mut out: Vec<String> = vec![];
    for a in arr {
        let Some(a) = a.as_str() else {
            return Err("claude.args must be strings".into());
        };
        if a.is_empty() || a.len() > MAX_ARG_LEN || has_control(a) {
            return Err(format!("invalid claude arg {a:?}"));
        }
        let flag = |s: &str| s.starts_with('-') && s.len() > 1;
        let prev_takes_value = out.last().is_some_and(|p| flag(p) && !p.contains('=') && p != "--");
        let prev_is_value = out.len() >= 2 && !flag(&out[out.len() - 1]);
        if !flag(a) && (!prev_takes_value || prev_is_value) {
            return Err(format!("claude arg {a:?} is neither a flag nor a flag's value"));
        }
        out.push(a.to_string());
    }
    Ok(out)
}

pub fn valid_alias_name(n: &str) -> bool {
    let mut c = n.chars();
    matches!(c.next(), Some(f) if f.is_ascii_alphabetic() || f == '_')
        && c.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        && n.len() <= 64
}

pub fn valid_env_name(n: &str) -> bool {
    let mut c = n.chars();
    matches!(c.next(), Some(f) if f.is_ascii_uppercase() || f == '_')
        && c.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        && n.len() <= 128
}

/// Variables the runner manages; a config may not override them.
/// `AIMUX_*` are the pre-rename copies the runner still sets on every pane.
pub fn protected_env(n: &str) -> bool {
    n.starts_with("TMUXEDO_") || n.starts_with("AIMUX_") || n == "BROWSER" || n == "PATH"
}

/// `shell`: invalid or protected names are refused (listed in the error);
/// the rest of the document must be well-formed.
pub fn validate_shell(v: &Value) -> Result<(ShellConfig, Vec<String>), String> {
    let mut cfg = ShellConfig::default();
    let mut refused = vec![];
    let obj = |k: &str| -> Result<serde_json::Map<String, Value>, String> {
        match &v[k] {
            Value::Null => Ok(Default::default()),
            Value::Object(m) if m.len() <= MAX_ENTRIES => Ok(m.clone()),
            _ => Err(format!("shell.{k} must be an object of at most {MAX_ENTRIES} strings")),
        }
    };
    for (name, val) in obj("aliases")? {
        let Some(val) = val.as_str() else {
            return Err(format!("alias {name} must be a string"));
        };
        if !valid_alias_name(&name) || val.len() > 4096 || has_control(val) {
            refused.push(format!("alias {name}"));
            continue;
        }
        cfg.aliases.insert(name, val.to_string());
    }
    for (name, val) in obj("env")? {
        let Some(val) = val.as_str() else {
            return Err(format!("env {name} must be a string"));
        };
        if !valid_env_name(&name) || protected_env(&name) || val.len() > 16 * 1024 || val.contains('\0') {
            refused.push(format!("env {name}"));
            continue;
        }
        cfg.env.insert(name, val.to_string());
    }
    match &v["rc"] {
        Value::Null => {}
        Value::String(s) if s.len() <= MAX_RC && !s.contains('\0') => cfg.rc = s.clone(),
        _ => return Err(format!("shell.rc must be a string of at most {MAX_RC} bytes")),
    }
    Ok((cfg, refused))
}

/// POSIX single-quoted literal.
pub fn sq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// The contents of `shell-init.sh`.
pub fn shell_init(cfg: &ShellConfig) -> String {
    let mut s = String::from("# Generated by tmuxedo from your launch settings; edits are overwritten.\n");
    for (k, v) in &cfg.env {
        s.push_str(&format!("export {k}={}\n", sq(v)));
    }
    for (k, v) in &cfg.aliases {
        s.push_str(&format!("alias {k}={}\n", sq(v)));
    }
    if !cfg.rc.trim().is_empty() {
        s.push_str("# rc\n");
        s.push_str(&cfg.rc);
        if !cfg.rc.ends_with('\n') {
            s.push('\n');
        }
    }
    s
}

/// `~/.config/tmuxedo` under `home`.
pub fn config_dir(home: &str) -> PathBuf {
    Path::new(home).join(".config/tmuxedo")
}

pub fn shell_init_path(home: &str) -> PathBuf {
    config_dir(home).join("shell-init.sh")
}

/// The last lines of both wrappers: the pane's shim directory goes back to
/// the front of PATH when a profile or the user's rc dropped it or moved it
/// (Debian's /etc/profile resets PATH; macOS's path_helper puts the system
/// directories first). Only the shim directory moves: the rest of PATH stays
/// in the order the user's startup files left it.
pub const KEEP_PANE_PATH: &str = "# tmuxedo: the agent shims stay first on PATH.\n\
case \"$PATH\" in \"${TMUXEDO_SHIM_DIR:-}\":*) ;; \
*) if [ -n \"${TMUXEDO_SHIM_DIR:-}\" ]; then export PATH=\"$TMUXEDO_SHIM_DIR:$PATH\"; fi ;; esac\n";

/// bash `--rcfile`: the user's ~/.bashrc first (as an interactive bash
/// would), then shell-init.sh, then [`KEEP_PANE_PATH`].
pub fn bash_rcfile(home: &str) -> String {
    format!(
        "# tmuxedo: the user's bash startup first, then their tmuxedo shell settings.\n\
         [ -r \"$HOME/.bashrc\" ] && . \"$HOME/.bashrc\"\n\
         [ -r {init} ] && . {init}\n{KEEP_PANE_PATH}",
        init = sq(&shell_init_path(home).to_string_lossy())
    )
}

/// The zsh ZDOTDIR wrapper files: each sources the real one; `.zshrc` then
/// sources shell-init.sh and ends with [`KEEP_PANE_PATH`].
pub fn zsh_files(home: &str) -> Vec<(&'static str, String)> {
    let real = "${TMUXEDO_REAL_ZDOTDIR:-$HOME}";
    let mut v: Vec<(&'static str, String)> = [".zshenv", ".zprofile", ".zlogin"]
        .into_iter()
        .map(|f| (f, format!("[ -r \"{real}/{f}\" ] && . \"{real}/{f}\"\n")))
        .collect();
    v.push((
        ".zshrc",
        format!(
            "[ -r \"{real}/.zshrc\" ] && . \"{real}/.zshrc\"\n[ -r {init} ] && . {init}\n{KEEP_PANE_PATH}",
            init = sq(&shell_init_path(home).to_string_lossy())
        ),
    ));
    v
}

/// What a pane's shell needs: extra argv / env so its startup sources
/// shell-init.sh. `state_dir` holds the wrappers.
pub enum ShellKind {
    Bash,
    Zsh,
    Other,
}

pub fn shell_kind(argv0: &str) -> ShellKind {
    match Path::new(argv0).file_name().and_then(|n| n.to_str()).unwrap_or("") {
        "bash" => ShellKind::Bash,
        "zsh" => ShellKind::Zsh,
        _ => ShellKind::Other,
    }
}

/// The wrappers' directory under the state directory; panes see it as
/// `$TMUXEDO_SHELL_DIR`.
pub fn shell_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("shell")
}

/// The bash wrapper's file name and the zsh wrapper directory's name, both
/// inside [`shell_dir`].
pub const BASH_RC: &str = "bashrc";
pub const ZSH_DOTDIR: &str = "zdotdir";

/// A plain interactive shell (`argv`: the shell and nothing but `-l` / `-i`
/// / `--login`) started so that it reads the wrappers in `dir`: bash as
/// `--rcfile <wrapper> -i`, zsh through `ZDOTDIR` (set in `env`, with the
/// real one, or `home`, kept in `TMUXEDO_REAL_ZDOTDIR`). Anything else is
/// returned as it came.
pub fn wrapped_shell(argv: Vec<String>, dir: &Path, home: &str, env: &mut BTreeMap<String, String>) -> Vec<String> {
    let plain = !argv.is_empty() && argv[1..].iter().all(|a| a == "-l" || a == "-i" || a == "--login");
    if !plain {
        return argv;
    }
    match shell_kind(&argv[0]) {
        ShellKind::Bash => vec![
            argv[0].clone(),
            "--rcfile".into(),
            dir.join(BASH_RC).display().to_string(),
            "-i".into(),
        ],
        ShellKind::Zsh => {
            let real = env.get("ZDOTDIR").cloned().unwrap_or_else(|| home.to_string());
            env.insert("TMUXEDO_REAL_ZDOTDIR".into(), real);
            env.insert("ZDOTDIR".into(), dir.join(ZSH_DOTDIR).display().to_string());
            argv
        }
        ShellKind::Other => argv,
    }
}

/// [`wrapped_shell`] as the last words of a `sh -c` line: replace the
/// running shell with an interactive `shell` that reads the same wrappers,
/// found through the pane's `$TMUXEDO_SHELL_DIR`. A login shell here would
/// read the system profile again and lose the pane's PATH. Without the
/// wrappers (or with a shell that has none) it is `exec <shell> -l`.
pub fn exec_wrapped_shell(shell: &str) -> String {
    let q = sq(shell);
    let dir = "$TMUXEDO_SHELL_DIR";
    match shell_kind(shell) {
        ShellKind::Bash => {
            format!("[ -r \"{dir}/{BASH_RC}\" ] && exec {q} --rcfile \"{dir}/{BASH_RC}\" -i; exec {q} -l")
        }
        ShellKind::Zsh => format!(
            "[ -r \"{dir}/{ZSH_DOTDIR}/.zshrc\" ] && \
             export TMUXEDO_REAL_ZDOTDIR=\"${{TMUXEDO_REAL_ZDOTDIR:-${{ZDOTDIR:-$HOME}}}}\" \
             ZDOTDIR=\"{dir}/{ZSH_DOTDIR}\"; exec {q} -l"
        ),
        ShellKind::Other => format!("exec {q} -l"),
    }
}

/// Write shell-init.sh and the wrappers (atomically, mode 0644; the init
/// file 0600 because env values may be secrets).
pub fn write_shell_files(home: &str, state_dir: &Path, cfg: &ShellConfig) -> std::io::Result<()> {
    write_shell_files_with(home, state_dir, cfg, None)
}

/// As [`write_shell_files`], with the project env hook (crate::project) when
/// `project_hook` names the runner binary.
pub fn write_shell_files_with(
    home: &str,
    state_dir: &Path,
    cfg: &ShellConfig,
    project_hook: Option<&str>,
) -> std::io::Result<()> {
    let mut init = shell_init(cfg);
    if let Some(exe) = project_hook {
        init.push_str(&crate::project::shell_hook(exe));
    }
    crate::config::write_private(&shell_init_path(home), init.as_bytes()).map_err(std::io::Error::other)?;
    write_shell_wrappers(home, state_dir)
}

/// Write only the wrappers (they source shell-init.sh if it exists). Every
/// runner writes them at start: the shell left behind after an agent run
/// (`plugin::run_then_shell`) reads them whether or not there are settings.
pub fn write_shell_wrappers(home: &str, state_dir: &Path) -> std::io::Result<()> {
    let dir = shell_dir(state_dir);
    std::fs::create_dir_all(dir.join(ZSH_DOTDIR))?;
    std::fs::write(dir.join(BASH_RC), bash_rcfile(home))?;
    for (f, body) in zsh_files(home) {
        std::fs::write(dir.join(ZSH_DOTDIR).join(f), body)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn args_are_flags_or_their_values() {
        assert_eq!(
            validate_args(&json!([
                "--dangerously-skip-permissions",
                "--model",
                "opus",
                "--verbose"
            ]))
            .unwrap(),
            ["--dangerously-skip-permissions", "--model", "opus", "--verbose"]
        );
        assert!(
            validate_args(&json!(["--model", "a", "b"])).is_err(),
            "two values in a row"
        );
        assert!(validate_args(&json!(["; rm -rf ~"])).is_err());
        assert!(
            validate_args(&json!(["--x=1", "y"])).is_err(),
            "--x=1 already has its value"
        );
        assert!(validate_args(&json!(["--a\nb"])).is_err());
        assert!(validate_args(&json!("--x")).is_err());
        // A flag's value may contain anything printable: it is never evaluated.
        assert!(validate_args(&json!(["--append-system-prompt", "$(rm -rf ~); `id`"])).is_ok());
    }

    #[test]
    fn shell_names_quoting_and_protection() {
        let (cfg, refused) = validate_shell(&json!({
            "aliases": {"c": "claude --resume", "ll": "ls -la", "bad name": "x", "q": "echo 'it''s' \"$HOME\""},
            "env": {"EDITOR": "vim", "PATH": "/evil", "AIMUX_PANE_ID": "x", "lower": "x", "QUOTED": "a'b $HOME `x`"},
            "rc": "set -o vi"
        }))
        .unwrap();
        assert_eq!(
            refused,
            ["alias bad name", "env AIMUX_PANE_ID", "env PATH", "env lower"]
        );
        let s = shell_init(&cfg);
        assert!(s.contains("export QUOTED='a'\\''b $HOME `x`'\n"), "{s}");
        assert!(s.contains("alias c='claude --resume'\n"));
        assert!(s.ends_with("set -o vi\n"));
        assert!(validate_shell(&json!({"rc": 5})).is_err());
    }

    #[test]
    fn work_dir_validation() {
        assert_eq!(validate_work_dir(&json!("~/work")).unwrap().as_deref(), Some("~/work"));
        assert_eq!(validate_work_dir(&json!("~")).unwrap().as_deref(), Some("~"));
        assert_eq!(
            validate_work_dir(&json!("/srv/code")).unwrap().as_deref(),
            Some("/srv/code")
        );
        assert_eq!(validate_work_dir(&json!("")).unwrap(), None, "empty = the default");
        for bad in [
            json!("work"),
            json!("~x"),
            json!("/a\nb"),
            json!(5),
            json!("x".repeat(2000)),
        ] {
            assert!(validate_work_dir(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn work_dir_is_created_and_canonical() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("home");
        std::fs::create_dir(&home).unwrap();
        std::os::unix::fs::symlink(&home, d.path().join("alias")).unwrap();
        let alias = d.path().join("alias").to_string_lossy().into_owned();
        let real = std::fs::canonicalize(&home).unwrap();
        let w = resolve_work_dir(&alias, None).unwrap();
        assert_eq!(w, real.join("work").to_string_lossy());
        assert!(real.join("work").is_dir());
        let w = resolve_work_dir(&alias, Some("~/code/x")).unwrap();
        assert_eq!(w, real.join("code/x").to_string_lossy());
        // A file in the way is an error (the caller falls back to home).
        std::fs::write(home.join("blocked"), "").unwrap();
        assert!(resolve_work_dir(&alias, Some("~/blocked")).is_err());
    }

    #[test]
    fn legacy_home_and_home_detection() {
        let r = |p: &str| rewrite_legacy_home(p, "/home/tmuxedo", LEGACY_HOME);
        assert_eq!(r("/home/aimux"), "/home/tmuxedo");
        assert_eq!(r("/home/aimux/"), "/home/tmuxedo/");
        assert_eq!(r("/home/aimux/proj/a"), "/home/tmuxedo/proj/a");
        assert_eq!(r("/home/aimuxer/x"), "/home/aimuxer/x", "only the whole component");
        assert_eq!(r("/srv/x"), "/srv/x");
        assert_eq!(
            rewrite_legacy_home("/home/aimux/p", "/home/aimux", LEGACY_HOME),
            "/home/aimux/p",
            "a machine still on the old home keeps it"
        );
        assert!(is_home("/home/aimux", "/home/tmuxedo", LEGACY_HOME));
        assert!(is_home("/home/tmuxedo/", "/home/tmuxedo", LEGACY_HOME));
        assert!(!is_home("/home/aimux/proj", "/home/tmuxedo", LEGACY_HOME));
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("home");
        std::fs::create_dir_all(home.join("proj")).unwrap();
        std::os::unix::fs::symlink(&home, d.path().join("alias")).unwrap();
        let h = home.to_string_lossy();
        assert!(
            is_home(&d.path().join("alias").to_string_lossy(), &h, LEGACY_HOME),
            "through a symlink"
        );
        assert!(!is_home(&home.join("proj").to_string_lossy(), &h, LEGACY_HOME));
        assert!(!is_home(
            &d.path().join("alias/proj").to_string_lossy(),
            &h,
            LEGACY_HOME
        ));
    }
}
