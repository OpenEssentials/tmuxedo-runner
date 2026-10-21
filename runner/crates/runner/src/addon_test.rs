//! "Test on machine" for an add-on (`addon.test`, gateway only): can this
//! machine reach each origin the add-on sends to (its `net` list)?
//!
//! Only the origins of an add-on in this machine's kept plugin set are
//! tried, so the method never connects anywhere the person did not see on
//! the add-on's card. Each origin gets a name lookup and a TCP connect with
//! a time limit (no TLS, no request, nothing sent); the answer says per
//! origin whether it connected, how long it took, or what failed.

use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tmuxedo_protocol::addon::{parse_addon, KIND};

pub const METHOD: &str = "addon.test";
/// Per origin: the name lookup and the connect together stay within this.
pub const TIMEOUT: Duration = Duration::from_secs(5);

/// The origins of the add-on `name` in the kept plugin set; None when it is not there.
pub fn origins(state_dir: &Path, name: &str) -> Option<Vec<String>> {
    let set = crate::plugin_set::load(state_dir)?;
    let item = set.items.iter().find(|i| i.kind == KIND && i.name == name)?;
    parse_addon(&item.definition).ok().map(|a| a.net)
}

/// `https://host[:port]` → (host, port).
pub fn host_port(origin: &str) -> Option<(String, u16)> {
    let (rest, default) = match origin.strip_prefix("https://") {
        Some(r) => (r, 443),
        None => (origin.strip_prefix("http://")?, 80),
    };
    match rest.split_once(':') {
        Some((h, p)) => Some((h.to_string(), p.parse().ok().filter(|p| *p > 0)?)),
        None => Some((rest.to_string(), default)),
    }
}

/// Try one origin: `{origin, ok, ms}` or `{origin, ok: false, error, message}`.
pub fn reach(origin: &str, timeout: Duration) -> Value {
    let start = Instant::now();
    let fail = |code: &str, msg: String| json!({"origin": origin, "ok": false, "error": code, "message": msg});
    let Some((host, port)) = host_port(origin) else {
        return fail("invalid", "not an http(s) origin".into());
    };
    let addrs: Vec<SocketAddr> = match (host.as_str(), port).to_socket_addrs() {
        Ok(a) => a.collect(),
        Err(e) => return fail("dns", format!("{host} could not be looked up: {e}")),
    };
    if addrs.is_empty() {
        return fail("dns", format!("{host} has no address"));
    }
    let mut last = String::new();
    for a in addrs.iter().take(4) {
        let left = timeout.saturating_sub(start.elapsed());
        if left.is_zero() {
            return fail(
                "timeout",
                format!("no answer from {host}:{port} within {}s", timeout.as_secs()),
            );
        }
        match TcpStream::connect_timeout(a, left) {
            Ok(_) => return json!({"origin": origin, "ok": true, "ms": start.elapsed().as_millis() as u64}),
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                last = format!("no answer from {host}:{port} within {}s", timeout.as_secs())
            }
            Err(e) => last = format!("{host}:{port}: {e}"),
        }
    }
    let code = if last.starts_with("no answer") {
        "timeout"
    } else {
        "refused"
    };
    fail(code, last)
}

/// Every origin at once (each on its own thread), in the add-on's order.
pub fn reach_all(origins: &[String], timeout: Duration) -> Vec<Value> {
    let handles: Vec<_> = origins
        .iter()
        .cloned()
        .map(|o| std::thread::spawn(move || reach(&o, timeout)))
        .collect();
    handles
        .into_iter()
        .zip(origins)
        .map(|(h, o)| {
            h.join().unwrap_or_else(
                |_| json!({"origin": o, "ok": false, "error": "internal", "message": "the check failed"}),
            )
        })
        .collect()
}

impl crate::daemon::Daemon {
    /// `addon.test {name}` from the gateway: `{type: "addon_test", name, results: [...]}`.
    pub(crate) fn addon_test_request(&mut self, params: &Value, reply: crate::api::ReplyTo) {
        use crate::api::{err, respond, ReplyTo};
        if matches!(reply, ReplyTo::Local(_)) {
            let e = err(
                "not_supported",
                "Testing an add-on is not available on the machine's local socket.",
            );
            return respond(reply, Err(e), self);
        }
        let Some(name) = params
            .get("name")
            .and_then(Value::as_str)
            .filter(|n| !n.is_empty() && n.len() <= 64)
        else {
            return respond(reply, Err(err("invalid_params", "addon.test: name is required")), self);
        };
        let Some(list) = origins(&self.opts.state_dir, name) else {
            let e = err("addon_not_found", format!("{name} is not on for this machine"));
            return respond(reply, Err(e), self);
        };
        let (name, tx) = (name.to_string(), self.internal_tx.clone());
        tokio::task::spawn_blocking(move || {
            let results = reach_all(&list, TIMEOUT);
            let _ = tx.send(crate::daemon::Internal::Fs {
                reply,
                result: Ok(json!({"type": "addon_test", "name": name, "results": results})),
                frames: vec![],
            });
        });
    }
}
