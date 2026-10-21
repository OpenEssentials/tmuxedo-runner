//! `tmuxedo`: the CLI. The same binary as the runner, dispatched when invoked
//! as `tmuxedo` (a symlink; `aimux` too, deprecated) or as `tmuxedo-runner cli …`.
//!
//! - `tmuxedo` / `tmuxedo tui` — the terminal UI (see `tui`).
//! - `tmuxedo login` — device flow (`kind: cli`; the CLI mints its own `tmx_`
//!   token and the server only ever sees its sha256, web/src/lib/auth/device.ts).
//! - `tmuxedo machines` — your machines.
//! - `tmuxedo attach <machine>[:<pane>]` — one pane through the gateway (see `attach`).
//! - `tmuxedo login-callback <address>` — finish a pane's browser login (see `login_callback`).
//! - `tmuxedo pane|agent|workspace|tab|events …` — runner API verbs against the
//!   local runner socket (`$TMUXEDO_SOCKET_PATH`).

pub mod attach;
pub mod changes;
pub mod files;
pub mod filter;
pub mod keys;
pub mod login_callback;
pub mod plugin;
pub mod secret;
pub mod tui;
pub mod widget;

use crate::config::{http_base, write_private};
use crate::util::{b64url, random_bytes, sha256_hex};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CliConfig {
    pub server: String,
    pub token: String,
    /// TUI: a mouse drag selects text and copies it to the local clipboard
    /// (OSC 52) on release. Default on; `copy_on_select = false` in cli.toml.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copy_on_select: Option<bool>,
}

pub fn cli_config_path() -> PathBuf {
    tmuxedo_holder::env_os("CLI_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::paths::config_dir().join("cli.toml"))
}

pub fn load_cli_config() -> Result<CliConfig, String> {
    let p = cli_config_path();
    let text = std::fs::read_to_string(&p).map_err(|_| "not logged in: run `tmuxedo login`".to_string())?;
    toml::from_str(&text).map_err(|e| format!("{}: {e}", p.display()))
}

pub(crate) fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(30))
        .user_agent(&format!("tmuxedo-cli/{}", crate::VERSION))
        .build()
}

/// GET/POST with the CLI token; returns (status, json).
pub(crate) fn api(cfg: &CliConfig, method: &str, path: &str, body: Option<Value>) -> Result<(u16, Value), String> {
    let url = format!("{}{path}", http_base(&cfg.server));
    let req = agent()
        .request(method, &url)
        .set("Authorization", &format!("Bearer {}", cfg.token));
    let res = match body {
        Some(b) => req.send_json(b),
        None => req.call(),
    };
    match res {
        Ok(r) => Ok((r.status(), r.into_json().unwrap_or(Value::Null))),
        Err(ureq::Error::Status(c, r)) => Ok((c, r.into_json().unwrap_or(Value::Null))),
        Err(e) => Err(format!("{url}: {e}")),
    }
}

fn api_error(v: &Value) -> String {
    v["error"]["message"]
        .as_str()
        .or(v["message"].as_str())
        .or(v["error"].as_str())
        .unwrap_or("request failed")
        .to_string()
}

// ── login ─────────────────────────────────────────────────────────────────

pub struct MintedCliToken {
    pub token: String,
    pub token_hash: String,
    pub prefix: String,
}

/// `tmx_` + base64url(32 bytes), as web's generateApiToken (aimux-era CLIs minted `amx_`, still accepted); prefix = first 12 chars.
pub fn mint_cli_token() -> MintedCliToken {
    let token = format!("tmx_{}", b64url(&random_bytes(32)));
    MintedCliToken {
        token_hash: sha256_hex(token.as_bytes()),
        prefix: token[..12].to_string(),
        token,
    }
}

pub fn login(server: &str, name: Option<String>, poll_floor: Duration) -> Result<CliConfig, String> {
    let base = http_base(server);
    let t = mint_cli_token();
    let name = name.unwrap_or_else(|| format!("tmuxedo CLI on {}", crate::util::hostname()));
    let a = agent();
    let body = json!({
        "kind": "cli", "tokenHash": t.token_hash, "prefix": t.prefix, "deviceName": name,
        "scopes": ["read", "write", "terminal"],
    });
    let start = match a.post(&format!("{base}/api/device/start")).send_json(body) {
        Ok(r) => r.into_json::<Value>().map_err(|e| e.to_string())?,
        Err(ureq::Error::Status(c, r)) => {
            return Err(format!(
                "login refused ({c}): {}",
                api_error(&r.into_json().unwrap_or(Value::Null))
            ))
        }
        Err(e) => return Err(e.to_string()),
    };
    eprintln!(
        "\n  Open {}\n  and check the code matches: {}\n",
        start["verificationUrl"].as_str().unwrap_or(""),
        start["userCode"].as_str().unwrap_or("?")
    );
    let mut interval = Duration::from_secs(start["pollIntervalSeconds"].as_u64().unwrap_or(5)).max(poll_floor);
    let deadline = Instant::now() + Duration::from_secs(11 * 60);
    loop {
        std::thread::sleep(interval);
        if Instant::now() > deadline {
            return Err("timed out waiting for approval".into());
        }
        let r = a
            .post(&format!("{base}/api/device/poll"))
            .send_json(json!({ "tokenHash": t.token_hash }));
        let v: Value = match r {
            Ok(r) => r.into_json().unwrap_or(Value::Null),
            Err(ureq::Error::Status(429, _)) => {
                interval += Duration::from_secs(5);
                continue;
            }
            Err(ureq::Error::Status(_, r)) => r.into_json().unwrap_or(Value::Null),
            Err(_) => continue,
        };
        match v["status"].as_str() {
            Some("approved") => break,
            Some("denied") => return Err("the request was denied".into()),
            Some("expired") => return Err("the request expired; run `tmuxedo login` again".into()),
            _ => continue,
        }
    }
    let cfg = CliConfig {
        server: base,
        token: t.token,
        copy_on_select: None,
    };
    write_private(
        &cli_config_path(),
        toml::to_string(&cfg).map_err(|e| e.to_string())?.as_bytes(),
    )?;
    eprintln!(
        "  Logged in. Token saved to {} (mode 0600).",
        cli_config_path().display()
    );
    Ok(cfg)
}

// ── machines ──────────────────────────────────────────────────────────────

pub fn machines(cfg: &CliConfig) -> Result<Vec<Value>, String> {
    let (code, v) = api(cfg, "GET", "/api/machines", None)?;
    if code != 200 {
        return Err(format!("machines ({code}): {}", api_error(&v)));
    }
    Ok(v["items"].as_array().cloned().unwrap_or_default())
}

pub fn resolve_machine(list: &[Value], want: &str) -> Result<Value, String> {
    let hits: Vec<&Value> = list
        .iter()
        .filter(|m| m["id"].as_str() == Some(want) || m["name"].as_str() == Some(want))
        .collect();
    match hits.as_slice() {
        [one] => Ok((*one).clone()),
        [] => Err(format!("no machine named {want}")),
        _ => Err(format!("{want} is ambiguous; use the machine id")),
    }
}

// ── local socket verbs (runner API) ────────────────────────────────────────

pub fn socket_path() -> PathBuf {
    tmuxedo_holder::env_os("SOCKET_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(crate::paths::runner_socket)
}

/// `reload-shell [pane]`: re-source ~/.config/tmuxedo/shell-init.sh in this
/// pane (or the one named) by typing it into the pane's shell, where it runs
/// as soon as this command exits.
fn reload_shell(rest: &[String]) -> Result<(), String> {
    let p = parse_args(rest);
    let pane = p
        .pos
        .first()
        .cloned()
        .or_else(|| tmuxedo_holder::env_var("PANE_ID"))
        .ok_or("reload-shell: run it in a pane, or name one (reload-shell w1:p2)")?;
    let cfg = call_local("runner.config", json!({}))?;
    let init = cfg["result"]["files"]["shell_init"].as_str().unwrap_or("").to_string();
    if init.is_empty() {
        return Err("the runner has no shell settings file".into());
    }
    let text = format!(" . {}\n", crate::launch_config::sq(&init));
    let r = call_local("pane.send_text", json!({ "pane_id": pane, "text": text }))?;
    match r.get("error") {
        Some(e) => Err(format!("reload-shell: {}", e["message"].as_str().unwrap_or("error"))),
        None => {
            eprintln!("reloading shell settings in {pane}");
            Ok(())
        }
    }
}

pub fn call_local(method: &str, params: Value) -> Result<Value, String> {
    let sock = socket_path();
    let mut s = std::os::unix::net::UnixStream::connect(&sock)
        .map_err(|e| format!("runner socket {}: {e} (is tmuxedo-runner running?)", sock.display()))?;
    let mut line = serde_json::to_vec(&json!({ "id": "cli", "method": method, "params": params })).unwrap_or_default();
    line.push(b'\n');
    s.write_all(&line).map_err(|e| e.to_string())?;
    let mut resp = String::new();
    BufReader::new(s).read_line(&mut resp).map_err(|e| e.to_string())?;
    serde_json::from_str(&resp).map_err(|e| format!("bad response: {e}"))
}

#[derive(Default, Debug)]
pub struct Args {
    pub pos: Vec<String>,
    pub flags: std::collections::BTreeMap<String, Vec<String>>,
}

const BOOL_FLAGS: &[&str] = &[
    "--print-argv",
    "--focus",
    "--no-focus",
    "--toggle",
    "--on",
    "--off",
    "--clear",
    "--ansi",
    "--raw",
    "--wait",
    "--new-tab",
    "--open-urls",
    "--new-workspace",
    "--current",
    "--json",
    "--observe",
    "--keep-alive",
    "--verbose",
];

pub fn parse_args(a: &[String]) -> Args {
    let mut out = Args::default();
    let mut i = 0;
    while i < a.len() {
        let x = &a[i];
        if x == "--" {
            out.flags
                .entry("--".into())
                .or_default()
                .extend(a[i + 1..].iter().cloned());
            break;
        }
        if x.starts_with("--") {
            if BOOL_FLAGS.contains(&x.as_str()) || i + 1 >= a.len() {
                out.flags.entry(x.clone()).or_default().push(String::new());
                i += 1;
            } else {
                out.flags.entry(x.clone()).or_default().push(a[i + 1].clone());
                i += 2;
            }
        } else {
            out.pos.push(x.clone());
            i += 1;
        }
    }
    out
}

impl Args {
    fn f(&self, k: &str) -> Option<String> {
        self.flags.get(k).and_then(|v| v.last().cloned())
    }
    fn has(&self, k: &str) -> bool {
        self.flags.contains_key(k)
    }
    fn all(&self, k: &str) -> Vec<String> {
        self.flags.get(k).cloned().unwrap_or_default()
    }
    fn num(&self, k: &str) -> Option<u64> {
        self.f(k).and_then(|v| v.parse().ok())
    }
    fn float(&self, k: &str) -> Option<f64> {
        self.f(k).and_then(|v| v.parse().ok())
    }
    fn pane_or_current(&self) -> Option<String> {
        self.f("--pane")
            .or_else(|| self.pos.first().cloned())
            .or_else(|| tmuxedo_holder::env_var("PANE_ID"))
    }
    fn env_map(&self) -> Value {
        let mut m = serde_json::Map::new();
        for kv in self.all("--env") {
            if let Some((k, v)) = kv.split_once('=') {
                m.insert(k.into(), json!(v));
            }
        }
        Value::Object(m)
    }
    fn focus(&self) -> Value {
        if self.has("--focus") {
            json!(true)
        } else {
            json!(false)
        }
    }
}

/// CLI verb → runner API (method, params).
pub fn verb_to_request(group: &str, verb: &str, a: &Args) -> Result<(String, Value), String> {
    let p0 = || {
        a.pos
            .first()
            .cloned()
            .ok_or_else(|| format!("{group} {verb}: missing id"))
    };
    let p1 = || {
        a.pos
            .get(1)
            .cloned()
            .ok_or_else(|| format!("{group} {verb}: missing argument"))
    };
    let read_params = |key: &str, id: String| {
        let source = a.f("--source").unwrap_or_else(|| "recent".into()).replace('-', "_");
        json!({ key: id, "source": source, "lines": a.num("--lines"),
                "format": if a.has("--ansi") { "ansi".to_string() } else { a.f("--format").unwrap_or_else(|| "text".into()) } })
    };
    let until = || json!(a.all("--until"));
    let r = match (group, verb) {
        ("workspace", "list") => ("workspace.list", json!({})),
        ("workspace", "create") => (
            "workspace.create",
            json!({ "cwd": a.f("--cwd"), "label": a.f("--label"), "focus": a.focus(), "env": a.env_map() }),
        ),
        ("workspace", "get") => ("workspace.get", json!({ "workspace_id": p0()? })),
        ("workspace", "focus") => ("workspace.focus", json!({ "workspace_id": p0()? })),
        ("workspace", "rename") => ("workspace.rename", json!({ "workspace_id": p0()?, "label": p1()? })),
        ("workspace", "close") => ("workspace.close", json!({ "workspace_id": p0()? })),
        ("workspace", "move") => (
            "workspace.move",
            json!({ "workspace_id": p0()?, "insert_index": p1()?.parse::<u64>().map_err(|e| e.to_string())? }),
        ),
        ("tab", "list") => ("tab.list", json!({ "workspace_id": a.f("--workspace") })),
        ("tab", "create") => (
            "tab.create",
            json!({ "workspace_id": a.f("--workspace"), "cwd": a.f("--cwd"), "label": a.f("--label"), "focus": a.focus(), "env": a.env_map() }),
        ),
        ("tab", "get") => ("tab.get", json!({ "tab_id": p0()? })),
        ("tab", "focus") => ("tab.focus", json!({ "tab_id": p0()? })),
        ("tab", "rename") => ("tab.rename", json!({ "tab_id": p0()?, "label": p1()? })),
        ("tab", "close") => ("tab.close", json!({ "tab_id": p0()? })),
        ("tab", "move") => (
            "tab.move",
            json!({ "tab_id": p0()?, "insert_index": p1()?.parse::<u64>().map_err(|e| e.to_string())? }),
        ),
        ("pane", "list") => ("pane.list", json!({ "workspace_id": a.f("--workspace") })),
        ("pane", "current") => ("pane.current", json!({ "caller_pane_id": a.pane_or_current() })),
        ("pane", "get") => ("pane.get", json!({ "pane_id": p0()? })),
        ("pane", "layout") => ("layout.export", json!({ "pane_id": a.pane_or_current() })),
        ("pane", "process-info") => ("pane.process_info", json!({ "pane_id": a.pane_or_current() })),
        ("pane", "resize") => (
            "pane.resize",
            json!({ "pane_id": a.pane_or_current(), "direction": a.f("--direction"), "amount": a.float("--amount") }),
        ),
        ("pane", "zoom") => {
            let mode = if a.has("--on") {
                "on"
            } else if a.has("--off") {
                "off"
            } else {
                "toggle"
            };
            ("pane.zoom", json!({ "pane_id": a.pane_or_current(), "mode": mode }))
        }
        ("pane", "rename") => (
            "pane.rename",
            json!({ "pane_id": p0()?, "label": if a.has("--clear") { Value::Null } else { json!(p1()?) } }),
        ),
        ("pane", "read") => ("pane.read", read_params("pane_id", p0()?)),
        ("pane", "split") => (
            "pane.split",
            json!({ "target_pane_id": a.pane_or_current(), "direction": a.f("--direction").unwrap_or_else(|| "right".into()),
                    "ratio": a.float("--ratio"), "cwd": a.f("--cwd"), "env": a.env_map(), "focus": a.focus() }),
        ),
        ("pane", "swap") => (
            "pane.swap",
            json!({ "pane_id": a.f("--pane"), "direction": a.f("--direction"), "source_pane_id": a.f("--source-pane"), "target_pane_id": a.f("--target-pane") }),
        ),
        ("pane", "move") => {
            let dest = if a.has("--new-tab") {
                json!({ "type": "new_tab", "workspace_id": a.f("--workspace"), "label": a.f("--label") })
            } else if a.has("--new-workspace") {
                json!({ "type": "new_workspace", "label": a.f("--label"), "tab_label": a.f("--tab-label") })
            } else {
                json!({ "type": "tab", "tab_id": a.f("--tab").ok_or("pane move: --tab, --new-tab or --new-workspace")?,
                        "split": a.f("--split").unwrap_or_else(|| "right".into()), "target_pane_id": a.f("--target-pane"), "ratio": a.float("--ratio") })
            };
            (
                "pane.move",
                json!({ "pane_id": p0()?, "destination": dest, "focus": a.focus() }),
            )
        }
        ("pane", "close") => ("pane.close", json!({ "pane_id": p0()? })),
        ("pane", "send-text") => (
            "pane.send_text",
            json!({ "pane_id": p0()?, "text": a.pos[1..].join(" ") }),
        ),
        ("pane", "send-keys") => (
            "pane.send_keys",
            json!({ "pane_id": p0()?, "keys": a.pos[1..].to_vec() }),
        ),
        ("pane", "run") => (
            "pane.send_input",
            json!({ "pane_id": p0()?, "text": a.pos[1..].join(" "), "keys": ["Enter"] }),
        ),
        ("pane", "wait-output") => {
            let m = match (a.f("--match"), a.f("--regex")) {
                (Some(t), _) => json!({ "type": "substring", "value": t }),
                (None, Some(r)) => json!({ "type": "regex", "value": r }),
                _ => return Err("pane wait-output: --match TEXT or --regex PATTERN".into()),
            };
            let mut p = read_params("pane_id", p0()?);
            p["match"] = m;
            p["timeout_ms"] = json!(a.num("--timeout"));
            ("pane.wait_for_output", p)
        }
        ("pane", "report-agent") => (
            "pane.report_agent",
            json!({ "pane_id": p0()?, "source": a.f("--source"), "agent": a.f("--agent"), "state": a.f("--state"),
                    "message": a.f("--message"), "agent_session_id": a.f("--agent-session-id"), "agent_session_path": a.f("--agent-session-path") }),
        ),
        ("pane", "report-agent-session") => (
            "pane.report_agent_session",
            json!({ "pane_id": p0()?, "source": a.f("--source"), "agent": a.f("--agent"),
                    "agent_session_id": a.f("--agent-session-id"), "agent_session_path": a.f("--agent-session-path") }),
        ),
        ("pane", "clear") => ("pane.clear", json!({ "pane_id": p0()? })),
        ("agent", "list") => ("agent.list", json!({})),
        ("agent", "get") => ("agent.get", json!({ "target": p0()? })),
        ("agent", "explain") => ("agent.explain", json!({ "target": p0()? })),
        ("agent", "read") => ("agent.read", read_params("target", p0()?)),
        ("agent", "send-keys") => (
            "agent.send_keys",
            json!({ "target": p0()?, "keys": a.pos[1..].to_vec() }),
        ),
        ("agent", "rename") => (
            "agent.rename",
            json!({ "target": p0()?, "name": if a.has("--clear") { Value::Null } else { json!(p1()?) } }),
        ),
        ("agent", "prompt") => {
            let mut p = json!({ "target": p0()?, "text": a.pos[1..].join(" ") });
            if a.has("--wait") || a.has("--until") {
                p["wait"] = json!({ "until": until(), "timeout_ms": a.num("--timeout") });
            }
            ("agent.prompt", p)
        }
        ("agent", "wait") => (
            "agent.wait",
            json!({ "target": p0()?, "until": until(), "timeout_ms": a.num("--timeout") }),
        ),
        ("agent", "start") => (
            "agent.start",
            json!({ "name": p0()?, "kind": a.f("--kind"), "pane_id": a.pane_or_current(), "args": a.all("--") }),
        ),
        ("events", "wait") => {
            let m: Value =
                serde_json::from_str(&a.f("--match-json").ok_or("events wait --match-json '{\"event\":…}'")?)
                    .map_err(|e| e.to_string())?;
            (
                "events.wait",
                json!({ "match_event": m, "timeout_ms": a.num("--timeout") }),
            )
        }
        _ => return Err(format!("unknown command: {group} {verb}")),
    };
    Ok((r.0.to_string(), strip_nulls(r.1)))
}

fn strip_nulls(v: Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(
            m.into_iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(k, v)| (k, strip_nulls(v)))
                .collect(),
        ),
        x => x,
    }
}

const HELP: &str = "tmuxedo — terminals on your machines, from anywhere (CLI: tmuxedo; `aimux` still works, deprecated)

  tmuxedo [tui] [--observe] [--machine M]         the terminal UI (prefix ctrl-b; ctrl-b d detaches)
  tmuxedo login [--server URL] [--name NAME]     sign this CLI in (device code)
  tmuxedo logout
  tmuxedo machines                                list your machines
  tmuxedo attach <machine>[:<pane>] [--observe]   attach to a pane (detach: ctrl-b d)
  tmuxedo changes <machine>[:<pane>] [--diff PATH] what a session changed: files and counts
  tmuxedo login-callback <address> [--pane M:P]  finish a pane's browser login: the localhost
                                                  address your browser ended on, delivered once
  tmuxedo workspace|tab|pane|agent <verb> …       runner API verbs on this machine
  tmuxedo events wait --match-json JSON [--timeout MS]
  tmuxedo api <method> [JSON params]              raw local API call
  tmuxedo files list|newest|share|request|get|rm|pin   files between you and agents (see `tmuxedo files help`)
  tmuxedo upload <file> --machine M [--project P] send a file to a machine's inbox
  tmuxedo plugin test [<dir>]                     plugin conformance suite (built-ins when no dir)
  tmuxedo plugin list|enable|disable|actions|invoke|log   plugins on this machine
  tmuxedo skill [--plugin ID]                     what the agent is told about tmuxedo (the generated skill)
  tmuxedo keys [revoke|github <name>]             your SSH keys and secrets (never a value)
  tmuxedo widget [usage|machine|agents|needs]   a live dashboard for this pane (q quits)
  tmuxedo secret list                              secrets this machine may use (names only)
  tmuxedo secret exec NAME… -- cmd …               run an allowed program with secrets in its env
  tmuxedo version
";

/// Every top-level `tmuxedo` command (the arms of [`main`]; the skill's
/// staleness guard, tests/skill.rs, checks the two agree).
pub const TOP_COMMANDS: &[&str] = &[
    "help",
    "version",
    "login",
    "logout",
    "config",
    "skill",
    "reload-shell",
    "plugin",
    "machines",
    "tui",
    "attach",
    "changes",
    "login-callback",
    "files",
    "upload",
    "api",
    "workspace",
    "tab",
    "pane",
    "agent",
    "events",
    "secret",
    "keys",
    "project-env",
    "plugin-gen",
    "widget",
];

/// Does `tmuxedo <args>` name a real command: a top-level one, and for the
/// runner API groups a verb that resolves to a method?
pub fn names_a_command(args: &[String]) -> bool {
    let Some(cmd) = args.first() else { return false };
    if !TOP_COMMANDS.contains(&cmd.as_str()) {
        return false;
    }
    match cmd.as_str() {
        g @ ("workspace" | "tab" | "pane" | "agent" | "events") => args
            .get(1)
            .is_some_and(|verb| verb_to_request(g, verb, &parse_args(&args[2..])).is_ok()),
        _ => true,
    }
}

pub fn main(args: Vec<String>) -> i32 {
    let a: Vec<String> = args.into_iter().collect();
    let Some(cmd) = a.first().map(String::as_str) else {
        // SAFETY: isatty only inspects the descriptors.
        if unsafe { libc::isatty(0) == 1 && libc::isatty(1) == 1 } {
            return report(tui::run(tui::Options {
                control: true,
                machine: None,
            }));
        }
        eprint!("{HELP}");
        return 2;
    };
    let rest = &a[1..];
    let res: Result<(), String> = match cmd {
        "help" | "--help" | "-h" => {
            print!("{HELP}");
            Ok(())
        }
        "version" | "--version" => {
            println!("tmuxedo (CLI) {}", crate::VERSION);
            Ok(())
        }
        "login" => {
            let p = parse_args(rest);
            let server = p
                .f("--server")
                .or_else(|| load_cli_config().ok().map(|c| c.server))
                .unwrap_or_else(|| "https://tmuxedo.dev".into());
            login(&server, p.f("--name"), Duration::from_secs(1)).map(|_| ())
        }
        "logout" => std::fs::remove_file(cli_config_path()).map_err(|e| e.to_string()),
        // Read-only: the launch/shell settings this machine's runner applies
        // (they come from web; change them in Settings).
        "config" => call_local("runner.config", json!({})).and_then(|v| match v.get("result") {
            Some(r) => {
                println!("{}", serde_json::to_string_pretty(r).unwrap_or_default());
                Ok(())
            }
            None => Err(format!("runner: {}", v["error"]["message"].as_str().unwrap_or("error"))),
        }),
        // What the agent is told (the generated tmuxedo skill), and whether it is on.
        "skill" => {
            let p = parse_args(rest);
            let id = p.f("--plugin").unwrap_or_else(|| "claude".into());
            call_local("runner.skill", json!({ "plugin_id": id })).and_then(|v| match v.get("result") {
                Some(r) => {
                    let on = if r["enabled"] == true {
                        "on"
                    } else {
                        "off (web turns it on per plugin)"
                    };
                    println!(
                        "# skill for {}: {on}
",
                        r["plugin_id"].as_str().unwrap_or("")
                    );
                    print!("{}", r["markdown"].as_str().unwrap_or(""));
                    Ok(())
                }
                None => Err(format!("runner: {}", v["error"]["message"].as_str().unwrap_or("error"))),
            })
        }
        // Re-source ~/.config/tmuxedo/shell-init.sh in this pane (or the one
        // named): typed into the pane's shell after this command exits.
        "reload-shell" => reload_shell(rest),
        "plugin" => plugin::run(rest),
        "widget" => widget::run(rest),
        "secret" => return secret::run(rest),
        "keys" => keys::run(rest),
        // The shell's cd hook (crate::project): export/unset lines for this folder's project.
        "project-env" => secret::project_env(rest),
        // A plugin's shim asks which generated arguments this folder gets (crate::plugin_gen).
        "plugin-gen" => {
            let cwd = std::env::current_dir().ok();
            let found = rest
                .first()
                .zip(cwd)
                .and_then(|(root, cwd)| crate::plugin_gen::lookup(std::path::Path::new(root), &cwd));
            return match found {
                Some(p) => {
                    println!("{}", p.display());
                    0
                }
                None => 1,
            };
        }
        "machines" => load_cli_config().and_then(|c| machines(&c)).map(|list| {
            println!("{:<24} {:<10} {:<16} ID", "NAME", "STATUS", "OS/ARCH");
            for m in list {
                println!(
                    "{:<24} {:<10} {:<16} {}",
                    m["name"].as_str().unwrap_or("?"),
                    if m["online"].as_bool() == Some(true) {
                        "online"
                    } else {
                        "offline"
                    },
                    format!(
                        "{}/{}",
                        m["os"].as_str().unwrap_or("?"),
                        m["arch"].as_str().unwrap_or("?")
                    ),
                    m["id"].as_str().unwrap_or("")
                );
            }
        }),
        "tui" => {
            let p = parse_args(rest);
            tui::run(tui::Options {
                control: !p.has("--observe"),
                machine: p.f("--machine"),
            })
        }
        "attach" => {
            let p = parse_args(rest);
            match p.pos.first() {
                Some(target) => attach::run(target, !p.has("--observe"), p.has("--open-urls")),
                None => Err("attach <machine>[:<pane>]".into()),
            }
        }
        "changes" => changes::run(rest),
        "login-callback" => login_callback::run(rest),
        "files" => files::run(rest),
        "upload" => files::upload(rest),
        "api" => {
            let method = rest.first().cloned().unwrap_or_default();
            let params = rest
                .get(1)
                .map(|s| serde_json::from_str(s).map_err(|e| e.to_string()))
                .transpose();
            params.and_then(|p| print_local(&method, p.unwrap_or(json!({}))))
        }
        g @ ("workspace" | "tab" | "pane" | "agent" | "events") => match rest.first() {
            Some(verb) => verb_to_request(g, verb, &parse_args(&rest[1..])).and_then(|(m, p)| print_local(&m, p)),
            None => Err(format!("{g} <verb>; see `tmuxedo help`")),
        },
        other => Err(format!("unknown command {other}; see `tmuxedo help`")),
    };
    report(res)
}

fn report(res: Result<(), String>) -> i32 {
    match res {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("tmuxedo: {e}");
            1
        }
    }
}

fn print_local(method: &str, params: Value) -> Result<(), String> {
    let v = call_local(method, params)?;
    if let Some(e) = v.get("error") {
        return Err(format!(
            "{}: {}",
            e["code"].as_str().unwrap_or("error"),
            e["message"].as_str().unwrap_or("")
        ));
    }
    println!("{}", serde_json::to_string_pretty(&v["result"]).unwrap_or_default());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Args {
        parse_args(&s.split_whitespace().map(str::to_owned).collect::<Vec<_>>())
    }

    #[test]
    fn verbs_map_to_runner_methods() {
        let (m, p) = verb_to_request("pane", "split", &args("w1:p1 --direction down --ratio 0.3 --focus")).unwrap();
        assert_eq!(m, "pane.split");
        assert_eq!(
            p,
            json!({"target_pane_id":"w1:p1","direction":"down","ratio":0.3,"env":{},"focus":true})
        );
        let (m, p) = verb_to_request("pane", "wait-output", &args("w1:p1 --regex ^ok$ --timeout 500")).unwrap();
        assert_eq!(m, "pane.wait_for_output");
        assert_eq!(p["match"], json!({"type":"regex","value":"^ok$"}));
        assert_eq!(p["timeout_ms"], 500);
        let (m, p) = verb_to_request("pane", "move", &args("w1:p2 --new-workspace --label x")).unwrap();
        assert_eq!(
            (m.as_str(), p["destination"]["type"].as_str()),
            ("pane.move", Some("new_workspace"))
        );
        let (m, p) = verb_to_request(
            "agent",
            "prompt",
            &args("w1:p1 hello there --until blocked --until done"),
        )
        .unwrap();
        assert_eq!(m, "agent.prompt");
        assert_eq!(p["text"], "hello there");
        assert_eq!(p["wait"]["until"], json!(["blocked", "done"]));
        let (m, p) = verb_to_request("pane", "run", &args("w1:p1 echo hi")).unwrap();
        assert_eq!(
            (m.as_str(), p["text"].as_str(), p["keys"][0].as_str()),
            ("pane.send_input", Some("echo hi"), Some("Enter"))
        );
        assert!(verb_to_request("pane", "nope", &args("")).is_err());
    }

    #[test]
    fn cli_token_shape() {
        let t = mint_cli_token();
        assert!(t.token.starts_with("tmx_") && t.token.len() >= 36);
        assert!(t.prefix.len() == 12 && t.prefix.starts_with("tmx_"));
        assert_eq!(t.token_hash, sha256_hex(t.token.as_bytes()));
        let re = regex::Regex::new("^tmx_[A-Za-z0-9_-]{32,128}$").unwrap();
        assert!(re.is_match(&t.token));
    }

    #[test]
    fn machine_resolution() {
        let list = vec![json!({"id":"m1","name":"box"}), json!({"id":"m2","name":"lap"})];
        assert_eq!(resolve_machine(&list, "lap").unwrap()["id"], "m2");
        assert_eq!(resolve_machine(&list, "m1").unwrap()["name"], "box");
        assert!(resolve_machine(&list, "x").is_err());
    }
}
