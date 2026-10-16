//! The plugin conformance harness (docs/design/plugins.md §11) run the way
//! CI and plugin authors run it: `tmuxedo plugin test` on every built-in
//! plugin (this test is the CI step for built-ins), on a plugin directory,
//! and on broken packages that it must catch.

use std::path::{Path, PathBuf};
use std::process::Command;
use tmuxedo_runner::plugin::{builtin, conformance, sign};

const BIN: &str = env!("CARGO_BIN_EXE_tmuxedo-runner");

fn cli(dir: &Path) -> PathBuf {
    let link = dir.join("tmuxedo");
    if !link.exists() {
        std::os::unix::fs::symlink(BIN, &link).unwrap();
    }
    link
}

fn write_package(dir: &Path, files: &sign::Files) {
    for (p, b) in files {
        let path = dir.join(p);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b).unwrap();
    }
}

#[test]
fn every_builtin_plugin_passes_conformance() {
    let d = tempfile::tempdir().unwrap();
    let out = Command::new(cli(d.path())).args(["plugin", "test"]).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    for want in [
        "plugin claude",
        "ok   lint",
        "ok   env.scrub enforced by the holder",
        "ok   screens/permission.blocked.txt",
        "ok   screens/prompt-box.idle.txt",
        "ok   screens/thinking.working.txt",
        "ok   hooks/lifecycle.jsonl: 18 cases",
        "ok   hooks/replies.jsonl: 7 cases",
        "ok   cli: 6 hooks through the shim",
        ", 0 failed",
        "source builtin:claude; built in",
        // Codex (PLAN Phase 16): hooks through `-c`, device-code lift, refresh-token reasons.
        "plugin codex",
        "ok   screens/approval.blocked.txt",
        "ok   screens/composer.idle.txt",
        "ok   screens/hooks-review.blocked.txt",
        "ok   screens/turn.working.txt",
        "ok   hooks/lifecycle.jsonl: 9 cases",
        "ok   hooks/replies.jsonl: 4 cases",
        "ok   processes.jsonl: 4 cases",
        "ok   auth.jsonl: 10 cases",
        "source builtin:codex; built in",
        // Cursor: node/argv detection, plugin-dir hooks, login link lift, keystroke fingerprints.
        "plugin cursor",
        "ok   screens/run-command.blocked.txt",
        "ok   screens/write-file.blocked.txt",
        "ok   screens/generating.working.txt",
        "ok   hooks/lifecycle.jsonl: 9 cases",
        "ok   processes.jsonl: 6 cases",
        "ok   auth.jsonl: 6 cases",
        "ok   prompts.jsonl: 4 cases",
        "ok   cli: 5 hooks through the shim",
        "source builtin:cursor; built in",
    ] {
        assert!(!stdout.contains("FAIL"), "a check failed:\n{stdout}");
        assert!(stdout.contains(want), "missing {want:?} in:\n{stdout}");
    }
}

#[test]
fn a_plugin_directory_is_tested_and_its_signature_reported() {
    let d = tempfile::tempdir().unwrap();
    let pkg = d.path().join("claude-dev");
    write_package(&pkg, &builtin::package("claude").unwrap());
    let out = Command::new(cli(d.path()))
        .args(["plugin", "test"])
        .arg(&pkg)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}");
    assert!(
        stdout.contains("unsigned for this runner: no plugin signing key is compiled into this runner"),
        "{stdout}"
    );
    // A symlink in the package is refused before anything runs.
    std::os::unix::fs::symlink("/etc/passwd", pkg.join("tests/leak")).unwrap();
    let out = Command::new(cli(d.path()))
        .args(["plugin", "test"])
        .arg(&pkg)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("symlinks are not allowed"));
}

/// Mutations of the Claude package the suite must catch, one per check.
#[test]
fn broken_packages_fail_the_right_check() {
    let base = builtin::package("claude").unwrap();
    let edit = |path: &str, from: &str, to: &str| {
        let mut f = base.clone();
        let text = String::from_utf8(f[path].clone()).unwrap();
        assert!(text.contains(from), "{path} has no {from:?}");
        f.insert(path.into(), text.replacen(from, to, 1).into_bytes());
        f
    };
    let cases: Vec<(&str, sign::Files)> = vec![
        // lint: a hold row without the capability
        ("lint", edit("plugin.toml", "\"prompt.hold\", ", "")),
        // lint: hold_secs not below the hook timeout
        ("lint", edit("plugin.toml", "hold_secs = 540", "hold_secs = 600")),
        // env.scrub names something the holder does not scrub
        (
            "env.scrub enforced by the holder",
            edit("plugin.toml", "\"CLAUDE_PID\"", "\"CLAUDE_PID\", \"ANTHROPIC_API_KEY\""),
        ),
        // a hook mapping drifts: Stop no longer completes the turn
        (
            "hooks/lifecycle.jsonl",
            edit(
                "plugin.toml",
                "{ native = \"Stop\",               emit = \"turn.completed\" }",
                "{ native = \"Stop\",               emit = \"attention.idle\" }",
            ),
        ),
        // the decision reply drifts by one byte
        (
            "hooks/replies.jsonl",
            edit("plugin.toml", "\"behavior\":\"deny\"", "\"behavior\":\"denied\""),
        ),
        // a screen fixture whose expected state is wrong
        ("screens/", {
            let mut f = base.clone();
            let b = f.remove("tests/screens/permission.blocked.txt").unwrap();
            f.insert("tests/screens/permission.idle.txt".into(), b);
            f
        }),
        // the shim's argv order changes
        (
            "cli",
            edit(
                "plugin.toml",
                "prepend = [\"{settings.launch.args...}\", \"--settings\", \"{gen:hook-settings}\", \"{skill.argv...}\"]",
                "prepend = [\"--settings\", \"{gen:hook-settings}\", \"{settings.launch.args...}\", \"{skill.argv...}\"]",
            ),
        ),
    ];
    let exe = PathBuf::from(BIN);
    for (check, files) in cases {
        let r = conformance::run(&files, Some(&exe));
        assert!(!r.ok(), "{check}: expected a failure\n{}", r.render());
        let failed: Vec<&str> = r.checks.iter().filter(|c| !c.ok).map(|c| c.name.as_str()).collect();
        assert!(
            failed.iter().any(|n| n.starts_with(check)),
            "{check}: failures were {failed:?}\n{}",
            r.render()
        );
    }
}

/// Mutations of the Codex and Cursor packages the suite must catch (PLAN Phase 16).
#[test]
fn codex_and_cursor_drift_fails_the_right_check() {
    let edit = |id: &str, path: &str, from: &str, to: &str| {
        let mut f = builtin::package(id).unwrap();
        let text = String::from_utf8(f[path].clone()).unwrap();
        assert!(text.contains(from), "{id}/{path} has no {from:?}");
        f.insert(path.into(), text.replacen(from, to, 1).into_bytes());
        f
    };
    let cases: Vec<(&str, sign::Files)> = vec![
        // Codex: Esc no longer gives a held prompt back.
        (
            "hooks/lifecycle.jsonl",
            edit(
                "codex",
                "plugin.toml",
                "emit = \"attention.idle\", release = true",
                "emit = \"attention.idle\"",
            ),
        ),
        // Codex: a refresh token reused elsewhere is no longer a failure.
        (
            "auth.jsonl",
            edit(
                "codex",
                "plugin.toml",
                "contains = \"refresh token was already used\", state = \"failed\"",
                "contains = \"refresh token was already used\"",
            ),
        ),
        // Codex: the device code is no longer lifted.
        (
            "auth.jsonl",
            edit("codex", "plugin.toml", ", code = '\\b([A-Z0-9]{4}-[A-Z0-9]{5})\\b'", ""),
        ),
        // Codex: the hook flag changes (every BYO user would have to trust again).
        (
            "cli",
            edit(
                "codex",
                "plugin.toml",
                "config_flag = \"-c\"",
                "config_flag = \"--config\"",
            ),
        ),
        // Codex: a held hook with no timeout anywhere.
        ("lint", edit("codex", "plugin.toml", "default_timeout = 600", "")),
        // Cursor: the session id field drifts.
        (
            "hooks/lifecycle.jsonl",
            edit(
                "cursor",
                "plugin.toml",
                "session_field = \"conversation_id\"",
                "session_field = \"session\"",
            ),
        ),
        // Cursor: Tab (allowlist and approve) is never an answer key.
        (
            "lint",
            edit("cursor", "plugin.toml", "allow = \"y\"", "allow = \"\\t\""),
        ),
        // Cursor: a fingerprint drifts.
        (
            "prompts.jsonl",
            edit(
                "cursor",
                "plugin.toml",
                "fingerprint_lines = 6",
                "fingerprint_lines = 2",
            ),
        ),
        // Cursor: the login link is no longer lifted.
        (
            "auth.jsonl",
            edit("cursor", "plugin.toml", "loginDeepControl", "loginDeep"),
        ),
        // Cursor: NO_OPEN_BROWSER is dropped from the shim.
        (
            "cli",
            edit("cursor", "plugin.toml", "env = { NO_OPEN_BROWSER = \"1\" }", ""),
        ),
        // Cursor: a process fixture that names another agent.
        (
            "processes.jsonl",
            edit(
                "cursor",
                "tests/processes.jsonl",
                "\"agent\": null}\n{\"note\": \"a native",
                "\"agent\": \"cursor\"}\n{\"note\": \"a native",
            ),
        ),
    ];
    let exe = PathBuf::from(BIN);
    for (check, files) in cases {
        let r = conformance::run(&files, Some(&exe));
        assert!(!r.ok(), "{check}: expected a failure\n{}", r.render());
        let failed: Vec<&str> = r.checks.iter().filter(|c| !c.ok).map(|c| c.name.as_str()).collect();
        assert!(
            failed.iter().any(|n| n.starts_with(check)),
            "{check}: failures were {failed:?}\n{}",
            r.render()
        );
    }
}

/// `plugin smoke` (the weekly real-CLI job's driver), here with the package's
/// own scripted CLI standing in for the real one on PATH.
#[test]
fn plugin_smoke_runs_a_cli_through_the_shim_and_checks_the_hooks() {
    let d = tempfile::tempdir().unwrap();
    let bin = d.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let fake = builtin::package("codex").unwrap()["tests/fake-cli.sh"].clone();
    std::fs::write(bin.join("codex"), fake).unwrap();
    std::fs::set_permissions(bin.join("codex"), std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let path = format!("{}:/usr/local/bin:/usr/bin:/bin", bin.display());
    let run = |expect: &str| {
        Command::new(cli(d.path()))
            .args([
                "plugin",
                "smoke",
                "codex",
                "--timeout",
                "30",
                "--expect",
                expect,
                "--",
                "hi",
            ])
            .env("PATH", &path)
            .current_dir(d.path())
            .output()
            .unwrap()
    };
    let out = run("SessionStart,PermissionRequest,Stop");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains("smoke codex: exit Some(0); hooks SessionStart UserPromptSubmit PermissionRequest PostToolUse Stop SessionEnd"),
        "{stdout}"
    );
    let out = run("SessionStart,Interrupt");
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("expected hooks did not arrive: [\"Interrupt\"]"));
    // The flags the shim passes, for the job's --strict-config check.
    let out = Command::new(cli(d.path()))
        .args(["plugin", "smoke", "codex", "--print-argv"])
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(
        s.starts_with("'-c' 'hooks.SessionStart=[{hooks=[{type=\"command\",command="),
        "{s}"
    );
    assert_eq!(s.matches("'-c'").count(), 9, "{s}");
}

#[test]
fn hook_with_plugin_flag_matches_the_default() {
    // `hook --plugin claude` and plain `hook` behave the same; an unknown
    // plugin prints nothing and exits 0 (the agent's own UI stays in charge).
    for args in [
        vec!["hook"],
        vec!["hook", "--plugin", "claude"],
        vec!["hook", "--plugin", "nope"],
    ] {
        let out = Command::new(BIN)
            .args(&args)
            .env("TMUXEDO_SOCKET_PATH", "/nonexistent/sock")
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        assert_eq!((out.status.code(), out.stdout.len()), (Some(0), 0), "{args:?}");
    }
}
