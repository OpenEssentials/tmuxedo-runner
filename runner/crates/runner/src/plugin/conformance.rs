//! The plugin conformance suite (docs/design/plugins.md §11), run by
//! `tmuxedo plugin test` and, for the built-in plugins, by `cargo test`.
//!
//! 1. **lint** — the manifest parses and lints clean; `env.scrub` is what
//!    the holder enforces.
//! 2. **screens** — `tests/screens/<name>.<state>.txt`: the vt100 text is
//!    read with the plugin's agent file (tmuxedo-detect); the state in the file
//!    name (`idle`, `working`, `blocked`, `unknown`) must come out.
//! 3. **hooks** — `tests/hooks/*.jsonl`: each line is either a native payload
//!    with the expected normalised events / status / hold / release /
//!    prompt / session fields, or a decision result with the exact stdout
//!    the agent must receive.
//! 4. **cli** — `tests/cli.toml`: a scripted fake CLI launched **through the
//!    generated shim** with the real `tmuxedo-runner hook` against a fake
//!    runner socket that holds and then allows; the argv the fake received,
//!    the hook sequence and its output are asserted.

use super::manifest::Manifest;
use super::sign::Files;
use super::{AgentPlugin, ManifestPlugin};
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq)]
pub struct Check {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Default)]
pub struct Report {
    pub plugin: String,
    pub checks: Vec<Check>,
}

impl Report {
    pub fn ok(&self) -> bool {
        self.checks.iter().all(|c| c.ok)
    }
    fn push(&mut self, name: impl Into<String>, result: Result<String, String>) {
        let (ok, detail) = match result {
            Ok(d) => (true, d),
            Err(d) => (false, d),
        };
        self.checks.push(Check {
            name: name.into(),
            ok,
            detail,
        });
    }
    /// One line per check, `ok` / `FAIL`.
    pub fn render(&self) -> String {
        let mut s = format!("plugin {}\n", self.plugin);
        for c in &self.checks {
            s.push_str(&format!(
                "  {} {}{}\n",
                if c.ok { "ok  " } else { "FAIL" },
                c.name,
                if c.detail.is_empty() {
                    String::new()
                } else {
                    format!(": {}", c.detail)
                }
            ));
        }
        let failed = self.checks.iter().filter(|c| !c.ok).count();
        s.push_str(&format!("{} checks, {} failed\n", self.checks.len(), failed));
        s
    }
}

fn text(files: &Files, path: &str) -> Option<String> {
    files.get(path).map(|b| String::from_utf8_lossy(b).into_owned())
}

/// Run steps 1–4. `hook_exe` is the binary registered as the hook (the
/// running `tmuxedo-runner`); None skips the scripted-CLI step.
pub fn run(files: &Files, hook_exe: Option<&Path>) -> Report {
    let mut r = Report::default();
    let Some(manifest_text) = text(files, "plugin.toml") else {
        r.plugin = "?".into();
        r.push("lint", Err("plugin.toml is missing".into()));
        return r;
    };
    let manifest = match Manifest::parse(&manifest_text) {
        Ok(m) => m,
        Err(e) => {
            r.plugin = "?".into();
            r.push("lint", Err(e));
            return r;
        }
    };
    r.plugin = manifest.id.clone();
    let problems = manifest.lint();
    r.push(
        "lint",
        if problems.is_empty() {
            Ok(String::new())
        } else {
            Err(problems.join("; "))
        },
    );
    let plugin = ManifestPlugin::new(manifest);
    let m = plugin.manifest();
    // env.scrub must be what the holder enforces (plugins.md §4 row 17).
    let not_enforced: Vec<&String> = m
        .env
        .scrub
        .iter()
        .filter(|s| !tmuxedo_holder::scrubbed_env_key(&s.replace('*', "X")))
        .collect();
    if !m.env.scrub.is_empty() {
        r.push(
            "env.scrub enforced by the holder",
            if not_enforced.is_empty() {
                Ok(format!("{} entries", m.env.scrub.len()))
            } else {
                Err(format!("not scrubbed by the holder: {not_enforced:?}"))
            },
        );
    }
    screens(&plugin, files, &mut r);
    hooks(&plugin, files, &mut r);
    processes(&plugin, files, &mut r);
    auth_screens(&plugin, files, &mut r);
    prompts(&plugin, files, &mut r);
    if let (Some(exe), Some(cfg)) = (hook_exe, text(files, "tests/cli.toml")) {
        r.push("cli", scripted_cli(&plugin, files, &cfg, exe));
    }
    r
}

fn screens(plugin: &ManifestPlugin, files: &Files, r: &mut Report) {
    let label = plugin.agent_label();
    let agent = tmuxedo_detect::Agent::lookup(label);
    for (path, bytes) in files
        .iter()
        .filter(|(p, _)| p.starts_with("tests/screens/") && p.ends_with(".txt"))
    {
        let name = path.trim_start_matches("tests/screens/");
        let want = name.trim_end_matches(".txt").rsplit('.').next().unwrap_or("");
        let Some(agent) = agent else {
            r.push(
                format!("screens/{name}"),
                Err(format!("no built-in agent file for label {label}")),
            );
            continue;
        };
        let screen = String::from_utf8_lossy(bytes);
        // `@title <text>` on the first line is the terminal title (as in
        // tmuxedo-detect's own screen fixtures).
        let (title, screen) = match screen.strip_prefix("@title ") {
            Some(rest) => {
                let (t, s) = rest.split_once('\n').unwrap_or((rest, ""));
                (t.to_string(), s.to_string())
            }
            None => (String::new(), screen.to_string()),
        };
        let input = tmuxedo_detect::Input {
            screen: &screen,
            title: &title,
            progress: "",
        };
        let got = tmuxedo_detect::read(agent, &input).as_str();
        r.push(
            format!("screens/{name}"),
            if got == want {
                Ok(got.into())
            } else {
                Err(format!("expected {want}, screen rules say {got}"))
            },
        );
    }
}

fn status_name(s: Option<tmuxedo_protocol::frames::AgentStatus>) -> Value {
    match s {
        Some(s) => Value::String(crate::agent::status_str(s).into()),
        None => Value::Null,
    }
}

fn hooks(plugin: &ManifestPlugin, files: &Files, r: &mut Report) {
    for (path, bytes) in files
        .iter()
        .filter(|(p, _)| p.starts_with("tests/hooks/") && p.ends_with(".jsonl"))
    {
        let name = path.trim_start_matches("tests/");
        let text = String::from_utf8_lossy(bytes);
        let mut n = 0;
        let mut errors = vec![];
        for (i, line) in text.lines().enumerate().filter(|(_, l)| !l.trim().is_empty()) {
            n += 1;
            let ln = i + 1;
            let fx: Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(e) => {
                    errors.push(format!("line {ln}: not JSON ({e})"));
                    continue;
                }
            };
            let mut diffs = vec![];
            if let Some(native) = fx.get("native") {
                let out = plugin.map_hook(native);
                if let Some(want) = fx.get("events") {
                    let got: Vec<Value> = out.names().iter().map(|n| Value::String(n.to_string())).collect();
                    if Value::Array(got.clone()) != *want {
                        diffs.push(format!("events {got:?} != {want}"));
                    }
                }
                if let Some(want) = fx.get("status") {
                    let got = status_name(out.status());
                    if got != *want {
                        diffs.push(format!("status {got} != {want}"));
                    }
                }
                if let Some(want) = fx.get("hold").and_then(Value::as_bool) {
                    if out.hold != want {
                        diffs.push(format!("hold {} != {want}", out.hold));
                    }
                }
                if let Some(want) = fx.get("releases").and_then(Value::as_bool) {
                    if out.releases_holds() != want {
                        diffs.push(format!("releases {} != {want}", out.releases_holds()));
                    }
                }
                if let Some(want) = fx.get("session_id") {
                    let got = out.session_id.clone().map(Value::String).unwrap_or(Value::Null);
                    if got != *want {
                        diffs.push(format!("session_id {got} != {want}"));
                    }
                }
                if let Some(want) = fx.get("cwd") {
                    let got = out.cwd.clone().map(Value::String).unwrap_or(Value::Null);
                    if got != *want {
                        diffs.push(format!("cwd {got} != {want}"));
                    }
                }
                if let Some(want) = fx.get("prompt") {
                    let got = out.events.iter().find_map(|e| match e {
                        super::AgentEvent::PromptPermission {
                            tool,
                            input,
                            suggestions,
                            ..
                        } => {
                            let mut v = serde_json::json!({ "tool": tool, "input": input });
                            if let Some(s) = suggestions {
                                v["suggestions"] = s.clone();
                            }
                            Some(v)
                        }
                        _ => None,
                    });
                    if got.as_ref() != Some(want) {
                        diffs.push(format!("prompt {got:?} != {want}"));
                    }
                }
                if let Some(want) = fx.get("auth_error").and_then(Value::as_bool) {
                    let got = out.events.iter().any(|e| match e {
                        super::AgentEvent::TurnFailed { error } => plugin.is_auth_error(error),
                        _ => false,
                    });
                    if got != want {
                        diffs.push(format!("auth_error {got} != {want}"));
                    }
                }
            } else if fx.get("decision").is_some() || fx.get("expired").is_some() {
                let got = plugin.reply(&fx);
                let want = fx["stdout"].as_str().unwrap_or("");
                if got != want {
                    diffs.push(format!("stdout {got:?} != {want:?}"));
                }
            } else {
                diffs.push("neither a native payload nor a decision".into());
            }
            if !diffs.is_empty() {
                errors.push(format!("line {ln}: {}", diffs.join("; ")));
            }
        }
        r.push(
            name.to_string(),
            if errors.is_empty() {
                Ok(format!("{n} cases"))
            } else {
                Err(errors.join(" | "))
            },
        );
    }
}

/// The fixture lines of one `tests/<name>.jsonl`, or None when absent.
fn jsonl(files: &Files, name: &str) -> Option<Vec<(usize, Result<Value, String>)>> {
    let t = text(files, &format!("tests/{name}"))?;
    Some(
        t.lines()
            .enumerate()
            .filter(|(_, l)| !l.trim().is_empty())
            .map(|(i, l)| (i + 1, serde_json::from_str(l).map_err(|e| e.to_string())))
            .collect(),
    )
}

fn push_cases(r: &mut Report, name: &str, n: usize, errors: Vec<String>) {
    r.push(
        name.to_string(),
        if errors.is_empty() {
            Ok(format!("{n} cases"))
        } else {
            Err(errors.join(" | "))
        },
    );
}

/// `tests/processes.jsonl`: a foreground job (`pgid`, `processes[{pid,
/// name, argv}]`) → the agent tmuxedo-detect names (`agent`, null = none).
/// The plugin's own label must come out for its CLI's process shapes.
fn processes(plugin: &ManifestPlugin, files: &Files, r: &mut Report) {
    let Some(cases) = jsonl(files, "processes.jsonl") else {
        return;
    };
    let mut errors = vec![];
    for (ln, fx) in &cases {
        let fx = match fx {
            Ok(v) => v,
            Err(e) => {
                errors.push(format!("line {ln}: not JSON ({e})"));
                continue;
            }
        };
        let procs: Vec<tmuxedo_detect::Process> = fx["processes"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|p| tmuxedo_detect::Process {
                pid: p["pid"].as_u64().unwrap_or(0) as u32,
                name: p["name"].as_str().unwrap_or("").to_string(),
                argv: p["argv"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|a| a.as_str().map(str::to_owned))
                    .collect(),
            })
            .collect();
        let job = tmuxedo_detect::Job {
            pgid: fx["pgid"].as_u64().unwrap_or(0) as u32,
            processes: procs,
        };
        let got = tmuxedo_detect::identify(&job).map(|a| a.id().to_string());
        let want = fx["agent"].as_str().map(str::to_owned);
        if got != want {
            errors.push(format!("line {ln}: agent {got:?} != {want:?}"));
        }
        if want
            .as_deref()
            .is_some_and(|w| !plugin.manifest().agent.labels.iter().any(|l| l == w))
        {
            errors.push(format!("line {ln}: {want:?} is not one of this plugin's labels"));
        }
    }
    push_cases(r, "processes.jsonl", cases.len(), errors);
}

/// `tests/auth.jsonl`: screen text → the `auth.screen` rule and state it
/// raises (`rule`, `state`; `rule: null` = none), or the login link it
/// lifts (`lift: {id, url, code}`, `lift: null` = none).
fn auth_screens(plugin: &ManifestPlugin, files: &Files, r: &mut Report) {
    let Some(cases) = jsonl(files, "auth.jsonl") else {
        return;
    };
    let mut errors = vec![];
    for (ln, fx) in &cases {
        let fx = match fx {
            Ok(v) => v,
            Err(e) => {
                errors.push(format!("line {ln}: not JSON ({e})"));
                continue;
            }
        };
        let screen = fx["screen"].as_str().unwrap_or("");
        if let Some(want) = fx.get("rule") {
            let hit = plugin.auth_screen_hit(screen);
            let got = hit.map(|(id, _)| Value::String(id.into())).unwrap_or(Value::Null);
            if got != *want {
                errors.push(format!("line {ln}: rule {got} != {want}"));
            }
            if let (Some(w), Some((_, st))) = (fx.get("state"), hit) {
                if w != st {
                    errors.push(format!("line {ln}: state {st} != {w}"));
                }
            }
        }
        if let Some(want) = fx.get("lift") {
            let got = plugin
                .lift(screen)
                .map(|l| serde_json::json!({ "id": l.rule, "url": l.url, "code": l.code }))
                .unwrap_or(Value::Null);
            if got != *want {
                errors.push(format!("line {ln}: lift {got} != {want}"));
            }
        }
    }
    push_cases(r, "auth.jsonl", cases.len(), errors);
}

/// `tests/prompts.jsonl` (keystroke plugins): screen text → the dialog a
/// keystroke may answer (`tool`, null = none) and its exact fingerprint.
fn prompts(plugin: &ManifestPlugin, files: &Files, r: &mut Report) {
    let Some(cases) = jsonl(files, "prompts.jsonl") else {
        return;
    };
    let mut errors = vec![];
    for (ln, fx) in &cases {
        let fx = match fx {
            Ok(v) => v,
            Err(e) => {
                errors.push(format!("line {ln}: not JSON ({e})"));
                continue;
            }
        };
        let got = plugin.prompt_fingerprint(fx["screen"].as_str().unwrap_or(""));
        let tool = got.as_ref().map(|g| Value::String(g.0.clone())).unwrap_or(Value::Null);
        if tool != fx["tool"] {
            errors.push(format!("line {ln}: tool {tool} != {}", fx["tool"]));
        }
        if let (Some(w), Some((_, fp))) = (fx.get("fingerprint"), &got) {
            if w != fp {
                errors.push(format!("line {ln}: fingerprint {fp:?} != {w}"));
            }
        }
    }
    push_cases(r, "prompts.jsonl", cases.len(), errors);
}

/// What `smoke` saw.
#[derive(Debug, Clone, Default)]
pub struct Smoke {
    /// Exit status of the real CLI (None: killed at the timeout).
    pub status: Option<i32>,
    /// Native hook events, in arrival order.
    pub hooks: Vec<String>,
}

/// The weekly real-CLI smoke (`tmuxedo plugin smoke <id> -- <argv>`): the
/// REAL agent binary, found on PATH behind the plugin's generated shim, run
/// with `argv` against a fake runner socket that answers every hook and
/// allows every held one. stdout/stderr are the caller's. `state` is a
/// scratch dir. Nothing of the user's is read or written besides what the
/// CLI itself does.
pub fn smoke(
    plugin: &ManifestPlugin,
    hook_exe: &Path,
    state: &Path,
    argv: &[String],
    timeout: Duration,
) -> Result<Smoke, String> {
    let m = plugin.manifest();
    let launch = m.launch.as_ref().ok_or("the plugin has no launch section")?;
    super::install_plugin(plugin, state, hook_exe, &[], None).map_err(|e| format!("install: {e}"))?;
    let sock = state.join("runner.sock");
    let _ = std::fs::remove_file(&sock);
    let seen = fake_runner(sock.clone(), std::sync::Arc::new(m.clone()));
    let shim_dir = state.join("shim");
    let path = format!("{}:{}", shim_dir.display(), std::env::var("PATH").unwrap_or_default());
    let mut child = std::process::Command::new(shim_dir.join(&launch.shim))
        .args(argv)
        .env("PATH", &path)
        .env("TMUXEDO_SOCKET_PATH", &sock)
        .env("TMUXEDO_PANE_ID", "w1:p1")
        .env("TMUXEDO_SHIM_DIR", &shim_dir)
        .stdin(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("{}: {e}", launch.shim))?;
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s.code(),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(100)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Err(e) => return Err(e.to_string()),
        }
    };
    // Late hooks (a SessionEnd after the exit) get a moment.
    std::thread::sleep(Duration::from_millis(500));
    if let Ok(mut s) = std::os::unix::net::UnixStream::connect(&sock) {
        let _ = s.write_all(b"{\"id\":\"x\",\"method\":\"__stop\"}\n");
    }
    let hooks = seen.lock().map(|v| v.clone()).unwrap_or_default();
    Ok(Smoke { status, hooks })
}

#[derive(serde::Deserialize)]
struct CliFixture {
    script: String,
    #[serde(default)]
    launch_args: Vec<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    expect_argv: Vec<String>,
    #[serde(default)]
    expect_hooks: Vec<String>,
    #[serde(default)]
    expect_output: Vec<String>,
    #[serde(default = "default_timeout")]
    timeout_secs: u64,
}

fn default_timeout() -> u64 {
    30
}

/// A fake runner socket: answers every `hook.event`, holding the plugin's
/// hold events for a moment and then allowing. Records the native names.
fn fake_runner(sock: PathBuf, plugin: std::sync::Arc<Manifest>) -> std::sync::Arc<std::sync::Mutex<Vec<String>>> {
    let seen = std::sync::Arc::new(std::sync::Mutex::new(vec![]));
    let listener = std::os::unix::net::UnixListener::bind(&sock).expect("bind fake runner socket");
    let out = seen.clone();
    std::thread::spawn(move || {
        let p = ManifestPlugin::new((*plugin).clone());
        for conn in listener.incoming() {
            let Ok(conn) = conn else { break };
            let mut line = String::new();
            if BufReader::new(conn.try_clone().unwrap()).read_line(&mut line).is_err() {
                continue;
            }
            let Ok(req) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if req["method"] == "__stop" {
                break;
            }
            let native = &req["params"]["event"];
            let o = p.map_hook(native);
            let event_field = p
                .manifest()
                .hooks
                .as_ref()
                .map(|h| h.event_field.clone())
                .unwrap_or_default();
            out.lock()
                .unwrap()
                .push(native[event_field].as_str().unwrap_or("?").to_string());
            let result = if o.hold {
                std::thread::sleep(Duration::from_millis(150));
                serde_json::json!({ "decision": { "behavior": "allow" } })
            } else {
                serde_json::json!({})
            };
            let mut w = conn;
            let _ = w.write_all(format!("{}\n", serde_json::json!({ "id": req["id"], "result": result })).as_bytes());
        }
    });
    seen
}

fn scripted_cli(plugin: &ManifestPlugin, files: &Files, cfg: &str, hook_exe: &Path) -> Result<String, String> {
    let fx: CliFixture = toml::from_str(cfg).map_err(|e| format!("tests/cli.toml: {}", e.message()))?;
    let m = plugin.manifest();
    let launch = m.launch.as_ref().ok_or("the plugin has no launch section")?;
    let script = files
        .get(&format!("tests/{}", fx.script))
        .ok_or_else(|| format!("tests/{} is missing", fx.script))?;
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let state = dir.path().join("state");
    // A shim that checks its target (`launch.target_contains`) finds the
    // fake under a directory named like the real install.
    let bin = match &launch.target_contains {
        t if t.is_empty() => dir.path().join("bin"),
        t => dir.path().join(format!("{t}-bin")),
    };
    let home = dir.path().join("home");
    std::fs::create_dir_all(&bin).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&home).map_err(|e| e.to_string())?;
    let agent_bin = m.agent.binaries.first().ok_or("agent.binaries is empty")?;
    super::write_exec(&bin.join(agent_bin), &String::from_utf8_lossy(script)).map_err(|e| e.to_string())?;
    super::install_plugin(plugin, &state, hook_exe, &fx.launch_args, None).map_err(|e| format!("install: {e}"))?;
    let gen = super::gen_paths(plugin, &state);
    let sock = dir.path().join("runner.sock");
    let seen = fake_runner(sock.clone(), std::sync::Arc::new(m.clone()));
    let shim = state.join("shim").join(&launch.shim);
    let path = format!(
        "{}:{}:/usr/local/bin:/usr/bin:/bin",
        state.join("shim").display(),
        bin.display()
    );
    let mut child = std::process::Command::new(&shim)
        .args(&fx.args)
        .env_clear()
        .env("PATH", &path)
        .env("HOME", &home)
        .env("TMUXEDO_SOCKET_PATH", &sock)
        .env("TMUXEDO_PANE_ID", "w1:p1")
        .current_dir(&home)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("{}: {e}", shim.display()))?;
    let deadline = Instant::now() + Duration::from_secs(fx.timeout_secs);
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            Ok(None) => {
                let _ = child.kill();
                return Err(format!("the fake CLI did not finish within {} s", fx.timeout_secs));
            }
            Err(e) => return Err(e.to_string()),
        }
    };
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    // Unblock the fake runner's accept loop.
    if let Ok(mut s) = std::os::unix::net::UnixStream::connect(&sock) {
        let _ = s.write_all(b"{\"id\":\"x\",\"method\":\"__stop\"}\n");
    }
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let mut errors = vec![];
    if !status.success() {
        errors.push(format!("exit {status}; stderr: {}", stderr.trim()));
    }
    if !fx.expect_argv.is_empty() {
        let got: Vec<String> = stdout
            .lines()
            .find_map(|l| l.strip_prefix("ARGV:"))
            .map(|l| {
                l.trim_end_matches('>')
                    .trim_start_matches('<')
                    .split("><")
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let ctx = super::Ctx {
            gen: Some(&gen),
            ..super::Ctx::default()
        };
        let want: Vec<String> = fx
            .expect_argv
            .iter()
            .flat_map(|t| match t.as_str() {
                "{hooks.argv...}" => plugin.hooks_argv(hook_exe),
                t => vec![super::resolve(t, &ctx)],
            })
            .collect();
        if got != want {
            errors.push(format!("argv {got:?} != {want:?}"));
        }
    }
    if !fx.expect_hooks.is_empty() {
        let got = seen.lock().unwrap().clone();
        if got != fx.expect_hooks {
            errors.push(format!("hooks {got:?} != {:?}", fx.expect_hooks));
        }
    }
    for want in &fx.expect_output {
        if !stdout.lines().any(|l| l == want) {
            errors.push(format!("output line {want:?} missing in {stdout:?}"));
        }
    }
    if errors.is_empty() {
        Ok(format!(
            "{} hooks through the shim and `{} hook`",
            fx.expect_hooks.len(),
            hook_exe.file_name().and_then(|n| n.to_str()).unwrap_or("?")
        ))
    } else {
        Err(errors.join("; "))
    }
}
