//! "Test on machine" for an MCP server (`mcp.test`, gateway only): start the
//! server once, the way an agent on this machine would, and ask it what it
//! offers (docs/design/plugin-settings-ux.md §4.1).
//!
//! Only a server in this machine's kept plugin set, turned on for the named
//! plugin somewhere on the machine, is started, so web can never run a
//! command the person did not save. Its secrets come from the vault exactly
//! as for an agent (binding `mcp` for that plugin and server); the values go
//! into the server's environment and are masked out of everything sent back.
//! A local server gets MCP `initialize` and `tools/list` over stdio, then is
//! killed with its process group; a remote one gets a name lookup and a TCP
//! connect (no request, nothing sent).

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tmuxedo_protocol::plugin_set::PluginSetItem;

pub const METHOD: &str = "mcp.test";
/// Start, `initialize` and `tools/list` together stay within this (a first
/// `npx` run downloads the package).
pub const TIMEOUT: Duration = Duration::from_secs(30);
/// What is kept of the server's stderr: the last lines, each cut short.
pub const STDERR_LINES: usize = 20;
const LINE_CHARS: usize = 300;
const MAX_TOOLS: usize = 200;
const MASK: &str = "••••";

/// The MCP server `id` from the kept set, when `plugin` has it on in the
/// machine's scope or in any project's.
pub fn find(state_dir: &Path, id: &str, plugin: &str) -> Option<PluginSetItem> {
    let set = crate::plugin_set::load(state_dir)?;
    let item = set.items.iter().find(|i| i.id == id && i.kind == "mcp")?;
    let on = std::iter::once(&set.machine)
        .chain(set.projects.values())
        .any(|scope| scope.get(plugin).is_some_and(|e| e.items.iter().any(|x| x == id)));
    on.then(|| item.clone())
}

/// `https://host[:port]/path` → `https://host[:port]`.
pub fn origin_of(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let host = rest.split(['/', '?', '#']).next().filter(|h| !h.is_empty())?;
    // userinfo is never part of what we connect to
    let host = host.rsplit('@').next()?;
    Some(format!("{scheme}://{host}"))
}

/// Replace every secret value in `s` (values shorter than 4 bytes are left:
/// they would mask ordinary words).
pub fn mask(s: &str, secrets: &[String]) -> String {
    let mut out = s.to_string();
    for v in secrets.iter().filter(|v| v.len() >= 4) {
        out = out.replace(v.as_str(), MASK);
    }
    out
}

fn cut(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n).collect::<String>() + "…"
    }
}

fn clean(s: &str) -> String {
    s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect()
}

/// A remote server: can this machine connect to its address?
pub fn test_http(url: &str) -> Value {
    let Some(origin) = origin_of(url) else {
        return json!({"ok": false, "transport": "http", "error": "invalid", "message": "the address is not an http(s) URL", "tools": [], "stderr": []});
    };
    let r = crate::addon_test::reach(&origin, crate::addon_test::TIMEOUT);
    let mut out = json!({"ok": r["ok"], "transport": "http", "origin": origin, "tools": [], "stderr": []});
    for k in ["ms", "error", "message"] {
        if let Some(v) = r.get(k) {
            out[k] = v.clone();
        }
    }
    if r["ok"] == true {
        out["message"] = json!("Reached. Sign-in and tools are checked when the agent connects.");
    }
    out
}

/// Wait for the JSON-RPC answer with `id`; other lines (logs, notifications) are skipped.
fn answer(rx: &mpsc::Receiver<String>, id: u64, deadline: Instant) -> Result<Value, (&'static str, String)> {
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(("timeout", format!("no answer within {}s", TIMEOUT.as_secs())));
        }
        match rx.recv_timeout(left) {
            Ok(line) => {
                let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
                    continue;
                };
                if v.get("id").and_then(Value::as_u64) != Some(id) {
                    continue;
                }
                if let Some(e) = v.get("error") {
                    let m = e.get("message").and_then(Value::as_str).unwrap_or("an error");
                    return Err(("protocol", format!("the server answered with an error: {m}")));
                }
                return Ok(v.get("result").cloned().unwrap_or(Value::Null));
            }
            Err(RecvTimeoutError::Timeout) => {
                return Err(("timeout", format!("no answer within {}s", TIMEOUT.as_secs())))
            }
            Err(RecvTimeoutError::Disconnected) => {
                return Err(("exited", "the server stopped before it answered".into()))
            }
        }
    }
}

/// Start a local server with `env` added to the runner's environment, run
/// the MCP handshake and `tools/list`, then stop it and its children.
/// Everything returned has `secrets` masked out.
pub fn test_stdio(
    argv: &[String],
    env: &[(String, String)],
    secrets: &[String],
    cwd: &Path,
    timeout: Duration,
) -> Value {
    let start = Instant::now();
    let deadline = start + timeout;
    let mut out = json!({"ok": false, "transport": "stdio", "tools": [], "stderr": []});
    let Some(program) = argv.first() else {
        out["error"] = json!("start");
        out["message"] = json!("it has no command");
        return out;
    };
    let mut cmd = Command::new(program);
    cmd.args(&argv[1..])
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            out["error"] = json!("start");
            out["message"] = json!(mask(&format!("{program} could not be started: {e}"), secrets));
            return out;
        }
    };
    let pid = child.id() as i32;
    let (tx, rx) = mpsc::channel::<String>();
    let stdout = child.stdout.take().expect("piped");
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(l) = line else { break };
            if tx.send(l).is_err() {
                break;
            }
        }
    });
    let tail: Arc<Mutex<VecDeque<String>>> = Arc::default();
    let stderr = child.stderr.take().expect("piped");
    let t = tail.clone();
    let err_reader = std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines() {
            let Ok(l) = line else { break };
            let mut q = t.lock().unwrap_or_else(|p| p.into_inner());
            q.push_back(cut(&clean(&l), LINE_CHARS));
            if q.len() > STDERR_LINES {
                q.pop_front();
            }
        }
    });
    let mut stdin = child.stdin.take().expect("piped");
    let mut send = |v: Value| -> bool { writeln!(stdin, "{v}").and_then(|_| stdin.flush()).is_ok() };
    let init = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "protocolVersion": "2025-06-18", "capabilities": {},
        "clientInfo": {"name": "tmuxedo-test", "version": env!("CARGO_PKG_VERSION")}}});
    let result = (|| {
        if !send(init) {
            return Err(("exited", "the server stopped before it answered".to_string()));
        }
        let info = answer(&rx, 1, deadline)?;
        send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        if !send(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}})) {
            return Err(("exited", "the server stopped before it answered".to_string()));
        }
        let tools = answer(&rx, 2, deadline)?;
        Ok((info, tools))
    })();
    // SAFETY: kill(2) on the process group this child leads.
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
    let _ = err_reader.join();
    out["ms"] = json!(start.elapsed().as_millis() as u64);
    let lines: Vec<String> = tail
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .iter()
        .map(|l| mask(l, secrets))
        .collect();
    match result {
        Ok((info, tools)) => {
            out["ok"] = json!(true);
            if let Some(si) = info.get("serverInfo") {
                let s = |k: &str| si.get(k).and_then(Value::as_str).map(|v| cut(&clean(v), 64));
                out["server"] = json!({"name": s("name"), "version": s("version")});
            }
            let list: Vec<Value> = tools
                .get("tools")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .take(MAX_TOOLS)
                        .filter_map(|t| {
                            let name = t.get("name").and_then(Value::as_str)?;
                            let mut v = json!({"name": mask(&cut(&clean(name), 128), secrets)});
                            if let Some(d) = t.get("description").and_then(Value::as_str) {
                                let first = d.lines().next().unwrap_or("");
                                v["description"] = json!(mask(&cut(&clean(first), 200), secrets));
                            }
                            Some(v)
                        })
                        .collect()
                })
                .unwrap_or_default();
            out["tools"] = json!(list);
        }
        Err((code, msg)) => {
            out["error"] = json!(code);
            out["message"] = json!(mask(&msg, secrets));
            out["stderr"] = json!(lines);
        }
    }
    out
}

fn home() -> std::path::PathBuf {
    std::env::var_os("HOME").map(Into::into).unwrap_or_else(|| "/".into())
}

impl crate::daemon::Daemon {
    /// `mcp.test {id, plugin}` from the gateway:
    /// `{type: "mcp_test", id, name, plugin, ok, transport, ms?, server?, tools, error?, message?, stderr}`.
    pub(crate) fn mcp_test_request(&mut self, params: &Value, reply: crate::api::ReplyTo) {
        use crate::api::{err, respond, ReplyTo};
        if matches!(reply, ReplyTo::Local(_)) {
            let e = err(
                "not_supported",
                "Testing an MCP server is not available on the machine's local socket.",
            );
            return respond(reply, Err(e), self);
        }
        let s = |k: &str, max: usize| {
            params
                .get(k)
                .and_then(Value::as_str)
                .filter(|v| !v.is_empty() && v.len() <= max)
                .map(str::to_string)
        };
        let (Some(id), Some(plugin)) = (s("id", 64), s("plugin", 32)) else {
            return respond(
                reply,
                Err(err("invalid_params", "mcp.test: id and plugin are required")),
                self,
            );
        };
        let Some(item) = find(&self.opts.state_dir, &id, &plugin) else {
            let e = err(
                "mcp_not_found",
                format!("this server is not on for {plugin} on this machine"),
            );
            return respond(reply, Err(e), self);
        };
        let head = json!({"type": "mcp_test", "id": id, "name": item.name, "plugin": plugin});
        let finish = move |mut body: Value| {
            for (k, v) in head.as_object().expect("object") {
                body[k] = v.clone();
            }
            body
        };
        let tx = self.internal_tx.clone();
        if item.definition["transport"] == "http" {
            let url = item.definition["url"].as_str().unwrap_or_default().to_string();
            tokio::task::spawn_blocking(move || {
                let result = Ok(finish(test_http(&url)));
                let _ = tx.send(crate::daemon::Internal::Fs {
                    reply,
                    result,
                    frames: vec![],
                });
            });
            return;
        }
        let (argv, plain, secret) = match crate::plugin_gen::stdio_parts(&item) {
            Ok(p) => p,
            Err(e) => return respond(reply, Err(err("invalid", e)), self),
        };
        let plain: Vec<(String, String)> = plain
            .into_iter()
            .map(|(k, v)| (k, v.as_str().unwrap_or_default().to_string()))
            .collect();
        // Either the test's environment and the values to mask, or why it cannot start.
        type Ready = Result<(Vec<(String, String)>, Vec<String>), String>;
        let run = move |tx: tokio::sync::mpsc::UnboundedSender<crate::daemon::Internal>, ready: Ready| {
            tokio::task::spawn_blocking(move || {
                let body = match ready {
                    Ok((env, values)) => test_stdio(&argv, &env, &values, &home(), TIMEOUT),
                    Err(message) => json!({"ok": false, "transport": "stdio", "error": "secret",
                                           "message": message, "tools": [], "stderr": []}),
                };
                let result = Ok(finish(body));
                let _ = tx.send(crate::daemon::Internal::Fs {
                    reply,
                    result,
                    frames: vec![],
                });
            });
        };
        if secret.is_empty() {
            return run(tx, Ok((plain, vec![])));
        }
        let mut names: Vec<String> = secret.values().cloned().collect();
        names.sort();
        names.dedup();
        let purpose = crate::secrets::delivery::Purpose {
            kind: "mcp".into(),
            plugin: Some(plugin.clone()),
            mcp: Some(item.name.clone()),
            program: Some(item.definition["command"][0].as_str().unwrap_or_default().to_string()),
            ..Default::default()
        };
        self.secret_want(
            names,
            purpose,
            Box::new(move |_, r| match r {
                Err(e) => run(tx, Err(e.message)),
                Ok(values) => {
                    let by_name: std::collections::HashMap<String, String> = values
                        .iter()
                        .map(|(n, v)| (n.clone(), String::from_utf8_lossy(v).into_owned()))
                        .collect();
                    let mut env = plain;
                    for (var, name) in &secret {
                        if let Some(v) = by_name.get(name) {
                            env.push((var.clone(), v.clone()));
                        }
                    }
                    run(tx, Ok((env, by_name.into_values().collect())));
                }
            }),
        );
    }
}
