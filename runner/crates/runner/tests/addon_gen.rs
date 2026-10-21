//! Add-ons (src/addon_gen.rs): what an `addon` item of the plugin set becomes
//! for each agent, from the shared fixture `packages/protocol/fixtures/addons.json`.
//!
//! The golden trees live in `tests/fixtures/addons/`; `TMUXEDO_BLESS=1`
//! rewrites them.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use tmuxedo_protocol::plugin_set::{parse_plugin_set, PluginSet};
use tmuxedo_runner::addon_gen::AgentHost;
use tmuxedo_runner::plugin::Host;
use tmuxedo_runner::plugin_gen::{self as gen, Ctx};

const EXE: &str = "/opt/tmuxedo/tmuxedo-runner";

fn fixtures() -> Value {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../packages/protocol/fixtures/addons.json"
    );
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn full() -> Value {
    fixtures()["full"].clone()
}

/// A set holding `addons` (name → definition), on for every agent.
fn set_of(addons: &[(&str, Value)]) -> PluginSet {
    let items: Vec<Value> = addons
        .iter()
        .enumerate()
        .map(|(i, (name, d))| json!({ "id": format!("ad-{i}"), "kind": "addon", "name": name, "version": 1, "definition": d }))
        .collect();
    let ids: Vec<String> = (0..addons.len()).map(|i| format!("ad-{i}")).collect();
    let entry = json!({ "items": ids, "values": {} });
    parse_plugin_set(&json!({
        "v": 1,
        "items": items,
        "machine": { "claude": entry, "codex": entry, "cursor": entry },
        "projects": {}
    }))
    .unwrap()
}

/// What tmuxedo's own plugin brings, fixed so the golden trees do not move.
fn host(plugin: &str, home: Option<&Path>) -> AgentHost {
    let mut h = AgentHost {
        home: home.map(Path::to_path_buf),
        ..Default::default()
    };
    match plugin {
        "claude" => {
            h.settings = Some(
                "{\"hooks\":{\"SessionStart\":[{\"hooks\":[{\"type\":\"command\",\"command\":\"'/opt/tmuxedo/tmuxedo-runner' hook\"}]}]}}\n"
                    .into(),
            );
            if let Some(home) = home {
                h.config_files = vec![home.join(".claude/settings.json")];
            }
        }
        "codex" => {
            h.hooks_argv = vec![
                "-c".into(),
                "hooks.SessionStart=[{hooks=[{type=\"command\",command=\"tmuxedo hook\"}]}]".into(),
            ];
            h.config_keys = vec!["developer_instructions".into()];
        }
        _ => {}
    }
    h
}

/// Build `plugin`'s machine scope; returns its folder.
fn build(state: &Path, plugin: &str, set: &PluginSet, host: &AgentHost) -> PathBuf {
    let ctx = Ctx {
        exe: Path::new(EXE),
        skill_text: None,
        host: Some(host),
    };
    assert!(gen::rebuild(state, plugin, set, "m1", &ctx).unwrap());
    let argv = gen::lookup(&gen::root(state, plugin), state).unwrap();
    argv.parent().unwrap().to_path_buf()
}

fn report(dir: &Path) -> Vec<Value> {
    let r: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("report.json")).unwrap()).unwrap();
    let rows = r.get("items").or(r.get("rows")).cloned().unwrap_or(r);
    rows.as_array().cloned().unwrap_or_default()
}

fn addon_rows(dir: &Path) -> Vec<Value> {
    report(dir).into_iter().filter(|r| r["kind"] == "addon").collect()
}

fn mode(p: &Path) -> u32 {
    std::fs::metadata(p).unwrap().permissions().mode() & 0o777
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<_> = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        out.push(p.clone());
        if p.is_dir() {
            walk(&p, out);
        }
    }
}

/// The tree as text: each path with its mode, each file's text with the
/// scope's folder written as `<scope>`.
fn dump(dir: &Path) -> String {
    let mut all = vec![];
    walk(dir, &mut all);
    let at = dir.display().to_string();
    let mut s = String::new();
    for p in all {
        let rel = p.strip_prefix(dir).unwrap().display().to_string();
        if p.is_dir() {
            s.push_str(&format!("== {rel}/ {:o}\n", mode(&p)));
        } else {
            let text = std::fs::read_to_string(&p).unwrap().replace(&at, "<scope>");
            s.push_str(&format!("== {rel} {:o}\n{text}", mode(&p)));
            if !text.ends_with('\n') {
                s.push('\n');
            }
        }
    }
    s
}

fn golden(name: &str, got: &str) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("tests/fixtures/addons/{name}.golden"));
    if std::env::var_os("TMUXEDO_BLESS").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, got).unwrap();
    }
    let want = std::fs::read_to_string(&path).unwrap_or_default();
    assert_eq!(got, want, "{} (TMUXEDO_BLESS=1 rewrites it)", path.display());
}

#[test]
fn the_fixture_builds_the_golden_trees() {
    let d = tempfile::tempdir().unwrap();
    let set = set_of(&[("telemetry", full())]);
    for plugin in ["claude", "codex", "cursor"] {
        let state = d.path().join(plugin);
        let dir = build(&state, plugin, &set, &host(plugin, None));
        golden(plugin, &dump(&dir));
    }
}

#[test]
fn claude_gets_its_settings_copy_env_and_files() {
    let d = tempfile::tempdir().unwrap();
    let dir = build(
        d.path(),
        "claude",
        &set_of(&[("telemetry", full())]),
        &host("claude", None),
    );
    let argv = std::fs::read_to_string(dir.join("argv.sh")).unwrap();
    assert!(argv.contains(
        "EXAMPLE_TELEMETRY_ENDPOINT='https://telemetry.example.com/v1'; export EXAMPLE_TELEMETRY_ENDPOINT\n"
    ));
    let readme = dir.join("files/telemetry/README.txt");
    assert!(argv.contains(&format!("EXAMPLE_TELEMETRY_README='{}'", readme.display())));
    assert!(argv.contains(&format!(
        "tmuxedo_hook_settings='{}/claude-settings.json'\n",
        dir.display()
    )));
    let s: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("claude-settings.json")).unwrap()).unwrap();
    let start = s["hooks"]["SessionStart"].as_array().unwrap();
    assert_eq!(start.len(), 2, "tmuxedo's group stays first, the add-on's follows");
    assert_eq!(start[0]["hooks"][0]["command"], "'/opt/tmuxedo/tmuxedo-runner' hook");
    let hook = dir.join("files/telemetry/bin/hook");
    assert_eq!(start[1]["hooks"][0]["command"], format!("'{}' start", hook.display()));
    assert_eq!(s["statusLine"]["command"], format!("'{}/statusline.sh'", dir.display()));
    for (f, m) in [
        ("files", 0o700),
        ("files/telemetry/bin/hook", 0o700),
        ("files/telemetry/bin/statusline", 0o700),
        ("files/telemetry/README.txt", 0o600),
        ("statusline.sh", 0o700),
        ("claude-settings.json", 0o600),
        ("addon-env.json", 0o600),
    ] {
        assert_eq!(mode(&dir.join(f)), m, "{f}");
    }
    let rows = addon_rows(&dir);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["status"], "built");
    assert_eq!(rows[0]["sendsTo"], json!(["https://telemetry.example.com"]));
}

#[test]
fn the_status_line_shows_the_persons_own_and_the_addons_when_there_is_none() {
    let d = tempfile::tempdir().unwrap();
    let dir = build(
        d.path(),
        "claude",
        &set_of(&[("telemetry", full())]),
        &host("claude", None),
    );
    let run = |chain: &str| {
        let mut script = std::fs::read_to_string(dir.join("statusline.sh")).unwrap();
        // The chain is tmuxedo's command; stand in for it.
        let line = script
            .lines()
            .find(|l| l.starts_with("TMUXEDO_STATUSLINE_CHAIN="))
            .unwrap()
            .to_string();
        script = script.replace(
            &line,
            &format!("TMUXEDO_STATUSLINE_CHAIN='{chain}'; export TMUXEDO_STATUSLINE_CHAIN"),
        );
        let out = std::process::Command::new("/bin/sh")
            .args(["-c", &script])
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap()
    };
    assert_eq!(run("echo mine"), "mine\n");
    assert_eq!(run("true"), "telemetry\n");
}

#[test]
fn codex_gets_c_values_and_one_hooks_list_per_event() {
    let d = tempfile::tempdir().unwrap();
    let dir = build(
        d.path(),
        "codex",
        &set_of(&[("telemetry", full())]),
        &host("codex", None),
    );
    let argv = std::fs::read_to_string(dir.join("argv.sh")).unwrap();
    assert!(argv.contains("'-c' 'otel.environment=\"dev\"'"), "{argv}");
    assert!(argv.contains("'-c' 'otel.log_user_prompt=false'"), "{argv}");
    assert!(argv.contains("otel.exporter={"), "{argv}");
    // tmuxedo's group, then the add-on's, in one later -c (the last one wins).
    let hook = dir.join("files/telemetry/bin/hook");
    let want = format!(
        "hooks.SessionStart=[{{hooks=[{{type=\"command\",command=\"tmuxedo hook\"}}]}},{{hooks=[{{type=\"command\",command=\"'{}' start\"}}]}}]",
        hook.display()
    );
    let out = std::process::Command::new("/bin/sh")
        .args(["-c", &format!("{}printf '%s\\n' \"$@\"", argv.replace("eval ", ": "))])
        .output()
        .unwrap();
    let args = String::from_utf8(out.stdout).unwrap();
    assert_eq!(args.lines().last(), Some(want.as_str()), "{args}");
    assert!(!dir.join("claude-settings.json").exists());
    assert!(!argv.contains("tmuxedo_hook_settings"));
}

#[test]
fn a_key_c_cannot_carry_leaves_codex_out_and_writes_no_config_file() {
    let d = tempfile::tempdir().unwrap();
    let cases = fixtures()["good"].clone();
    let def = cases
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"].as_str().unwrap().contains("-c cannot carry"))
        .unwrap()["definition"]
        .clone();
    let dir = build(d.path(), "codex", &set_of(&[("odd", def)]), &host("codex", None));
    let rows = addon_rows(&dir);
    assert_eq!(rows[0]["status"], "skipped");
    assert!(
        rows[0]["reason"]
            .as_str()
            .unwrap()
            .starts_with("Not available for Codex"),
        "{rows:?}"
    );
    let mut all = vec![];
    walk(d.path(), &mut all);
    assert!(all.iter().all(|p| p.file_name().unwrap() != "config.toml"), "{all:?}");
    assert!(!std::fs::read_to_string(dir.join("argv.sh"))
        .unwrap()
        .contains("profile"));
}

#[test]
fn cursor_gets_its_hooks_as_a_second_plugin_dir_and_the_files() {
    let d = tempfile::tempdir().unwrap();
    let home = d.path().join("home");
    std::fs::create_dir_all(home.join(".cursor")).unwrap();
    std::fs::write(home.join(".cursor/hooks.json"), "{\"version\":1,\"hooks\":{}}\n").unwrap();
    let dir = build(
        &d.path().join("state"),
        "cursor",
        &set_of(&[("telemetry", full())]),
        &host("cursor", Some(&home)),
    );
    let rows = addon_rows(&dir);
    assert_eq!(rows[0]["status"], "built", "{rows:?}");
    assert_eq!(rows[0]["sendsTo"], json!(["https://telemetry.example.com"]));
    let plug = dir.join("cursor-addons");
    let manifest: Value =
        serde_json::from_str(&std::fs::read_to_string(plug.join(".cursor-plugin/plugin.json")).unwrap()).unwrap();
    assert_eq!(manifest["hooks"], "hooks/hooks.json");
    let hooks: Value = serde_json::from_str(&std::fs::read_to_string(plug.join("hooks/hooks.json")).unwrap()).unwrap();
    assert_eq!(hooks["version"], 1);
    let cmd = hooks["hooks"]["sessionStart"][0]["command"].as_str().unwrap();
    let hook = dir.join("files/telemetry/bin/hook");
    assert!(
        cmd.contains(&hook.display().to_string()) && cmd.ends_with(" start"),
        "{cmd}"
    );
    assert_eq!(mode(&hook), 0o700);
    // One more --plugin-dir, after tmuxedo's own.
    let argv = std::fs::read_to_string(dir.join("argv.sh")).unwrap();
    assert_eq!(
        argv.matches("--plugin-dir").count(),
        1,
        "the manifest prepends the first: {argv}"
    );
    assert!(argv.contains(&plug.display().to_string()), "{argv}");
    // The person's own Cursor files are not touched.
    assert_eq!(
        std::fs::read_to_string(home.join(".cursor/hooks.json")).unwrap(),
        "{\"version\":1,\"hooks\":{}}\n"
    );
}

#[test]
fn cursor_without_hooks_for_it_is_not_available() {
    let d = tempfile::tempdir().unwrap();
    let def = json!({ "agents": { "claude": { "env": { "EXAMPLE_LEVEL": "2" } } } });
    let dir = build(d.path(), "cursor", &set_of(&[("level", def)]), &host("cursor", None));
    let rows = addon_rows(&dir);
    assert_eq!(rows[0]["status"], "skipped");
    assert_eq!(rows[0]["reason"], "Not available for Cursor");
    assert!(!dir.join("cursor-addons").exists());
}

#[test]
fn a_clashing_variable_or_key_is_refused_and_reported() {
    let d = tempfile::tempdir().unwrap();
    let other = json!({
        "agents": {
            "claude": { "env": { "EXAMPLE_TELEMETRY_ENDPOINT": "https://other.example.com", "CLAUDE_OWN": "x" } },
            "codex": { "config": { "otel": { "environment": "prod" }, "developer_instructions": "x", "model_reasoning_effort": "high" } }
        }
    });
    let set = set_of(&[("telemetry", full()), ("other", other)]);
    let mut h = host("claude", None);
    h.env = vec!["CLAUDE_OWN".into()];
    let dir = build(&d.path().join("c"), "claude", &set, &h);
    let argv = std::fs::read_to_string(dir.join("argv.sh")).unwrap();
    assert!(argv.contains("EXAMPLE_TELEMETRY_ENDPOINT='https://telemetry.example.com/v1'"));
    assert!(
        !argv.contains("other.example.com") && !argv.contains("CLAUDE_OWN"),
        "{argv}"
    );
    let rows = addon_rows(&dir);
    assert_eq!(rows[1]["name"], "other");
    assert_eq!(
        rows[1]["refused"],
        json!([
            { "key": "CLAUDE_OWN", "reason": "already set by the claude plugin" },
            { "key": "EXAMPLE_TELEMETRY_ENDPOINT", "reason": "already set by the add-on telemetry" }
        ])
    );
    let dir = build(&d.path().join("x"), "codex", &set, &host("codex", None));
    let argv = std::fs::read_to_string(dir.join("argv.sh")).unwrap();
    assert!(
        !argv.contains("prod") && !argv.contains("developer_instructions"),
        "{argv}"
    );
    assert!(argv.contains("model_reasoning_effort=\"high\""), "{argv}");
    let refused = &addon_rows(&dir)[1]["refused"];
    let keys: Vec<&str> = refused
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["key"].as_str().unwrap())
        .collect();
    assert_eq!(keys, ["developer_instructions", "otel"], "{refused}");
}

#[test]
fn secrets_go_by_name_and_no_value_is_written() {
    let d = tempfile::tempdir().unwrap();
    for plugin in ["claude", "codex"] {
        let dir = build(
            &d.path().join(plugin),
            plugin,
            &set_of(&[("telemetry", full())]),
            &host(plugin, None),
        );
        let env: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("addon-env.json")).unwrap()).unwrap();
        assert_eq!(
            env,
            json!({ "v": 1, "plugin": plugin, "env": { "EXAMPLE_TELEMETRY_TOKEN": "EXAMPLE_TELEMETRY_TOKEN" } })
        );
        let argv = std::fs::read_to_string(dir.join("argv.sh")).unwrap();
        assert!(!argv.contains("EXAMPLE_TELEMETRY_TOKEN="), "{argv}");
        assert!(argv.contains(&format!(
            "cli secret addon-env '{}/addon-env.json' 2>/dev/null)\"\n",
            dir.display()
        )));
    }
}

#[test]
fn the_probe_skips_an_addon_its_installer_set_up() {
    let d = tempfile::tempdir().unwrap();
    let set = set_of(&[("telemetry", full())]);
    // A marker file in the home folder.
    let home = d.path().join("home");
    std::fs::create_dir_all(home.join(".example-telemetry")).unwrap();
    std::fs::write(home.join(".example-telemetry/installed"), "").unwrap();
    let dir = build(&d.path().join("a"), "claude", &set, &host("claude", Some(&home)));
    let rows = addon_rows(&dir);
    assert_eq!(rows[0]["status"], "skipped");
    assert_eq!(
        rows[0]["reason"],
        "Already set up on this machine by Example Telemetry's installer"
    );
    assert!(!std::fs::read_to_string(dir.join("argv.sh"))
        .unwrap()
        .contains("EXAMPLE_TELEMETRY"));
    assert!(!dir.join("files").exists() && !dir.join("claude-settings.json").exists());
    // The agent's own settings naming it.
    let home = d.path().join("home2");
    std::fs::create_dir_all(home.join(".claude")).unwrap();
    std::fs::write(
        home.join(".claude/settings.json"),
        "{\"env\":{\"X\":\"example-telemetry\"}}",
    )
    .unwrap();
    let dir = build(&d.path().join("b"), "claude", &set, &host("claude", Some(&home)));
    assert_eq!(addon_rows(&dir)[0]["status"], "skipped");
    // Neither: built.
    let home = d.path().join("home3");
    std::fs::create_dir_all(&home).unwrap();
    let dir = build(&d.path().join("c"), "claude", &set, &host("claude", Some(&home)));
    assert_eq!(addon_rows(&dir)[0]["status"], "built");
}

#[test]
fn turning_an_addon_off_is_a_new_scope_without_it() {
    let d = tempfile::tempdir().unwrap();
    let h = host("claude", None);
    let on = build(d.path(), "claude", &set_of(&[("telemetry", full())]), &h);
    let mut set = set_of(&[("telemetry", full())]);
    set.machine.get_mut("claude").unwrap().items.clear();
    let ctx = Ctx {
        exe: Path::new(EXE),
        skill_text: None,
        host: Some(&h),
    };
    // Nothing on for Claude: no scope, so the next launch is plain.
    assert!(!gen::rebuild(d.path(), "claude", &set, "m1", &ctx).unwrap());
    assert_eq!(gen::lookup(&gen::root(d.path(), "claude"), d.path()), None);
    assert!(on.join("argv.sh").is_file(), "an agent already running keeps its own");
}

#[test]
fn the_shim_passes_the_settings_copy_and_env_and_leaves_the_users_files_alone() {
    let d = tempfile::tempdir().unwrap();
    let state = d.path().join("state");
    let home = d.path().join("home");
    std::fs::create_dir_all(home.join(".claude")).unwrap();
    let own = [
        (home.join(".claude/settings.json"), "{\"model\":\"opus\"}\n"),
        (home.join(".claude.json"), "{}\n"),
    ];
    for (p, text) in &own {
        std::fs::write(p, text).unwrap();
    }
    let exe = Path::new(env!("CARGO_BIN_EXE_tmuxedo-runner"));
    let real = d.path().join("real");
    std::fs::create_dir_all(&real).unwrap();
    std::fs::write(
        real.join("claude"),
        "#!/bin/sh\nprintf 'ENDPOINT=%s\\nTOKEN=%s\\n' \"$EXAMPLE_TELEMETRY_ENDPOINT\" \"${EXAMPLE_TELEMETRY_TOKEN-unset}\"\nprintf '%s\\n' \"$@\"\n",
    )
    .unwrap();
    std::fs::set_permissions(real.join("claude"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut host = Host::builtin();
    host.hook_exe = Some(exe.to_path_buf());
    let shim = host.install(&state, exe, &[]).unwrap();
    let ah = AgentHost::of("claude", &state, exe, Some(&home));
    assert!(ah.settings.is_some(), "the host's hook settings are read");
    let ctx = Ctx {
        exe,
        skill_text: None,
        host: Some(&ah),
    };
    assert!(gen::rebuild(&state, "claude", &set_of(&[("telemetry", full())]), "m1", &ctx).unwrap());
    let root = gen::root(&state, "claude");
    let lines = [("claude".to_string(), gen::shim_line(exe, &root))]
        .into_iter()
        .collect();
    assert!(host.set_plugin_set(lines));
    host.write_shims(&state, &[]).unwrap();
    let out = std::process::Command::new(shim.join("claude"))
        .args(["-p", "hi"])
        .current_dir(d.path())
        .env("HOME", &home)
        .env("TMUXEDO_SOCKET_PATH", d.path().join("none.sock"))
        .env("PATH", format!("{}:{}:/usr/bin:/bin", shim.display(), real.display()))
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let out = String::from_utf8(out.stdout).unwrap();
    let got: Vec<&str> = out.lines().collect();
    assert_eq!(got[0], "ENDPOINT=https://telemetry.example.com/v1");
    assert_eq!(got[1], "TOKEN=unset", "no runner to ask: the secret stays out");
    let at = gen::lookup(&root, d.path()).unwrap();
    let at = at.parent().unwrap();
    let i = got.iter().position(|a| *a == "--settings").unwrap();
    assert_eq!(got[i + 1], at.join("claude-settings.json").display().to_string());
    assert_eq!(got.iter().filter(|a| **a == "--settings").count(), 1);
    assert!(out.ends_with("-p\nhi\n"), "{out}");
    for (p, text) in &own {
        assert_eq!(&std::fs::read_to_string(p).unwrap(), text, "{}", p.display());
    }
}

#[test]
fn the_report_for_web_has_each_agents_addon_rows_and_no_secret_names() {
    let d = tempfile::tempdir().unwrap();
    let level = json!({ "agents": { "claude": { "env": { "EXAMPLE_LEVEL": "2" } } } });
    let set = set_of(&[("level", level), ("telemetry", full())]);
    assert_eq!(gen::addon_report(d.path()), json!({ "items": [] }));
    for plugin in ["claude", "codex", "cursor"] {
        build(d.path(), plugin, &set, &host(plugin, None));
    }
    let r = gen::addon_report(d.path());
    let rows: Vec<(String, String, String)> = r["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| {
            (
                x["plugin"].as_str().unwrap().into(),
                x["name"].as_str().unwrap().into(),
                x["status"].as_str().unwrap().into(),
            )
        })
        .collect();
    let want = [
        ("claude", "level", "built"),
        ("claude", "telemetry", "built"),
        ("codex", "level", "skipped"),
        ("codex", "telemetry", "built"),
        ("cursor", "level", "skipped"),
        ("cursor", "telemetry", "built"),
    ];
    assert_eq!(
        rows,
        want.map(|(a, b, c)| (a.to_string(), b.to_string(), c.to_string()))
    );
    assert_eq!(r["items"][2]["reason"], "Not available for Codex");
    let text = r.to_string();
    assert!(
        !text.contains("EXAMPLE_TELEMETRY_TOKEN") && !text.contains("sendsTo"),
        "{text}"
    );
}
