//! Hook injection and resume, for every agent plugin (docs/design/plugins.md
//! §4 rows 4, 5, 14, 15). The tables live in the plugin manifests
//! (`plugins/<id>/plugin.toml`); this module keeps the entry points the
//! daemon and the tests use and delegates to [`crate::plugin`].
//!
//! **Injection** (decision 0001 hook spike): a shim directory is prepended
//! to every pane's PATH. A plugin's shim finds the real binary further down
//! PATH and execs it with `launch.prepend` (for Claude: the user's launch
//! args, then `--settings <generated file>`, which registers
//! `tmuxedo-runner hook` for the lifecycle events). The user's own settings
//! files are never touched.
//!
//! **Resume**: after a holder restart or pod loss, a pane whose agent
//! reported a session id restarts with the plugin's `resume.argv` — only if
//! the recorded cwd still exists, and, if it was a git checkout when
//! recorded, is still one (the resume-cwd guard: a pruned
//! worktree must not be recreated empty under an agent that thinks it is
//! home). The guard stays core.

use crate::plugin::{self, AgentPlugin, Host};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// (event, timeout seconds, needs a tool matcher). A compatibility mirror of
/// the Claude manifest's `hooks.register`; a unit test keeps them equal.
pub const HOOK_EVENTS: &[(&str, Option<u32>, bool)] = &[
    ("SessionStart", None, false),
    ("UserPromptSubmit", None, false),
    ("PermissionRequest", Some(600), true),
    ("PostToolUse", None, true),
    ("PostToolUseFailure", None, true),
    ("PermissionDenied", None, true),
    ("Notification", None, false),
    ("Stop", None, false),
    ("StopFailure", None, false),
    ("SubagentStart", None, false),
    ("SubagentStop", None, false),
    ("SessionEnd", None, false),
];

/// The program names that open a URL; each gets a PATH stand-in.
pub const OPENERS: &[&str] = plugin::OPENERS;

/// Claude's generated `--settings` document with AI usage off (the default
/// until web says otherwise): the hooks and nothing else, no `statusLine`.
pub fn settings_json(hook_exe: &Path) -> Value {
    settings_json_with(hook_exe, false)
}

/// Claude's generated `--settings` document; `usage` adds the status line.
pub fn settings_json_with(hook_exe: &Path, usage: bool) -> Value {
    plugin::claude()
        .settings_doc(hook_exe, usage)
        .expect("the claude manifest generates hook settings")
}

pub use plugin::opener_script;

/// The claude shim for `settings` and the validated launch `args`
/// (docs/protocol.md `config`): embedded as single-quoted literals before
/// `--settings` and the user's own arguments: exec, never interpreted by a
/// shell.
pub fn shim_script(settings: &Path, args: &[String]) -> String {
    let gen: BTreeMap<String, PathBuf> = [("hook-settings".to_string(), settings.to_path_buf())].into();
    plugin::claude()
        .shim_script(&gen, args, None)
        .expect("the claude manifest has a launch section")
}

/// Write every built-in plugin's generated files and shim and the URL opener
/// stand-ins under `dir`; returns the shim dir.
pub fn install(dir: &Path, hook_exe: &Path) -> std::io::Result<PathBuf> {
    install_with(dir, hook_exe, &[])
}

/// [`install`] with launch args.
pub fn install_with(dir: &Path, hook_exe: &Path, args: &[String]) -> std::io::Result<PathBuf> {
    Host::builtin_only(&[plugin::DEFAULT_PLUGIN]).install(dir, hook_exe, args)
}

/// [`install`] with AI usage on: the settings also name the status line.
pub fn install_with_usage(dir: &Path, hook_exe: &Path) -> std::io::Result<PathBuf> {
    let mut host = Host::builtin_only(&[plugin::DEFAULT_PLUGIN]);
    host.set_usage(plugin::DEFAULT_PLUGIN, true);
    host.install(dir, hook_exe, &[])
}

/// Rewrite only the claude shim (new launch args); running claudes keep theirs.
pub fn write_claude_shim(dir: &Path, args: &[String]) -> std::io::Result<()> {
    plugin::write_shim(plugin::claude(), dir, args, None)
}

/// bash re-adds the shim at each prompt if a login profile dropped it from
/// PATH (Debian's /etc/profile resets PATH). Other shells keep whatever their
/// profile leaves; `$TMUXEDO_SHIM_DIR/claude` always works explicitly.
pub fn prompt_command() -> &'static str {
    r#"case ":$PATH:" in *":$TMUXEDO_SHIM_DIR:"*) ;; *) [ -n "$TMUXEDO_SHIM_DIR" ] && PATH="$TMUXEDO_SHIM_DIR:$PATH" ;; esac"#
}

/// The resume-cwd guard: Ok if `recorded` may be resumed into.
pub fn resume_cwd_ok(recorded: &str, was_repo: bool) -> Result<(), String> {
    let p = Path::new(recorded);
    if !p.is_dir() {
        return Err(format!(
            "recorded cwd {recorded} no longer exists; not recreating it empty"
        ));
    }
    if was_repo && !p.join(".git").exists() {
        return Err(format!("recorded cwd {recorded} was a git checkout and no longer is"));
    }
    Ok(())
}

pub fn is_repo(cwd: &str) -> bool {
    Path::new(cwd).join(".git").exists()
}

/// argv that resumes an agent session and then leaves a shell behind
/// (the built-in plugins decide the command; see [`resume_argv_with`]).
pub fn resume_argv(
    shell: &str,
    shim_dir: &Path,
    kind: &str,
    session_id: &str,
    resume_argv: Option<&[String]>,
) -> Vec<String> {
    resume_argv_with(
        &Host::builtin_only(&[plugin::DEFAULT_PLUGIN]),
        shell,
        shim_dir,
        kind,
        session_id,
        resume_argv,
    )
}

/// The agent's own reported resume argv wins; else the plugin owning `kind`
/// renders `resume.argv`; else `<kind> --resume <id>`.
pub fn resume_argv_with(
    host: &Host,
    shell: &str,
    shim_dir: &Path,
    kind: &str,
    session_id: &str,
    resume_argv: Option<&[String]>,
) -> Vec<String> {
    let cmd = match resume_argv {
        Some(argv) if !argv.is_empty() => argv.iter().map(|a| plugin::sh_quote(a)).collect::<Vec<_>>().join(" "),
        _ => host
            .by_label(kind)
            .and_then(|p| p.resume_command(shim_dir, session_id))
            .unwrap_or_else(|| format!("{} --resume {}", plugin::sh_quote(kind), plugin::sh_quote(session_id))),
    };
    plugin::run_then_shell(shell, &cmd)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn settings_register_every_event() {
        let s = settings_json(Path::new("/opt/tmuxedo runner/tmuxedo-runner"));
        for (e, timeout, matcher) in HOOK_EVENTS {
            let entry = &s["hooks"][e][0];
            assert_eq!(
                entry["hooks"][0]["command"],
                "'/opt/tmuxedo runner/tmuxedo-runner' hook"
            );
            assert_eq!(entry["hooks"][0]["timeout"].as_u64(), timeout.map(u64::from));
            assert_eq!(entry["matcher"].is_string(), *matcher);
        }
        assert_eq!(s["hooks"]["PermissionRequest"][0]["hooks"][0]["timeout"], 600);
    }

    #[test]
    fn shim_execs_the_real_claude_with_settings() {
        let d = tempfile::tempdir().unwrap();
        let shim = install(d.path(), Path::new("/bin/true")).unwrap();
        let real = d.path().join("real");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("claude"), "#!/bin/sh\necho REAL \"$@\"\n").unwrap();
        std::fs::set_permissions(real.join("claude"), std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = format!("{}:{}:/usr/bin:/bin", shim.display(), real.display());
        let out = std::process::Command::new(shim.join("claude"))
            .arg("--version")
            .env("PATH", &path)
            .output()
            .unwrap();
        let s = String::from_utf8_lossy(&out.stdout);
        assert!(s.starts_with("REAL --settings "), "{s}");
        assert!(s.contains("claude-settings.json --version"), "{s}");
        // Without a real claude it fails cleanly instead of recursing.
        let out = std::process::Command::new(shim.join("claude"))
            .env("PATH", format!("{}:/usr/bin:/bin", shim.display()))
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(127));
    }

    #[test]
    fn resume_guard() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().to_string_lossy().into_owned();
        assert!(resume_cwd_ok(&p, false).is_ok());
        assert!(resume_cwd_ok(&p, true).is_err(), "was a repo, is not any more");
        std::fs::create_dir(d.path().join(".git")).unwrap();
        assert!(resume_cwd_ok(&p, true).is_ok());
        assert!(resume_cwd_ok(&format!("{p}/gone"), false).is_err());
        let argv = resume_argv("/bin/bash", Path::new("/s"), "claude", "abc-1", None);
        // bash is left behind through the rc wrapper, not as a login shell.
        assert!(
            argv[2].ends_with(
                "'/s/claude' --resume 'abc-1'; [ -r \"$TMUXEDO_SHELL_DIR/bashrc\" ] && \
                 exec '/bin/bash' --rcfile \"$TMUXEDO_SHELL_DIR/bashrc\" -i; exec '/bin/bash' -l"
            ),
            "{}",
            argv[2]
        );
        let argv = resume_argv(
            "/bin/sh",
            Path::new("/s"),
            "codex",
            "x",
            Some(&["codex".into(), "resume".into(), "x".into()]),
        );
        assert!(argv[2].ends_with("'codex' 'resume' 'x'; exec '/bin/sh' -l"));
        // A disabled plugin's agent resumes unwrapped (no shim to run).
        let mut host = Host::builtin();
        host.set_enabled("claude", false).unwrap();
        let argv = resume_argv_with(&host, "/bin/sh", Path::new("/s"), "claude", "q", None);
        assert!(argv[2].contains("'claude' --resume 'q';"), "{}", argv[2]);
    }
}
