//! `tmuxedo login-callback <pasted-url> [--pane <machine>:<pane>]`: finish a
//! pane's browser login (docs/protocol.md "Login callback relay").
//!
//! A CLI login in a remote pane (Claude Code's `/login`, `codex login`)
//! redirects your browser to the machine's `localhost:<port>`, which your
//! device cannot reach: the browser shows an error page. Copy that page's
//! address and run this; the machine's runner delivers it to the waiting
//! login, once.
//!
//! The pane: `--pane`, or the newest login link an attached `tmuxedo` (TUI or
//! `attach`) saw whose callback is the pasted address's host, port and path
//! (remembered for 15 minutes in `<config>/login-callbacks.json`, without the
//! URL's state or code). Sent through web (`POST /api/machines/{id}/login-callback`),
//! which checks the CLI token's terminal scope and audits the host only.

use super::{api, api_error, load_cli_config, machines, parse_args, resolve_machine};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::PathBuf;

/// How long a seen login link is offered (the runner's own limit).
const KEEP_MS: u64 = 15 * 60 * 1000;
const KEEP_MAX: usize = 8;

/// A login link an attached client saw (no state, no code).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Seen {
    #[serde(rename = "machineId")]
    pub machine_id: String,
    #[serde(rename = "paneId")]
    pub pane_id: String,
    pub host: String,
    pub port: u16,
    pub path: String,
    /// Unix ms.
    pub at: u64,
}

fn store_path() -> PathBuf {
    crate::paths::config_dir().join("login-callbacks.json")
}

fn load() -> Vec<Seen> {
    std::fs::read(store_path())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// Remember an `open_url` event's `callback` for this machine's pane.
/// Best effort: a client that cannot write its config still works.
pub fn remember(machine_id: &str, pane_id: &str, callback: &Value) {
    let Some(s) = seen_from(machine_id, pane_id, callback, crate::util::now_ms()) else {
        return;
    };
    let list = keep(load(), s);
    if let Ok(b) = serde_json::to_vec(&list) {
        let _ = crate::config::write_private(&store_path(), &b);
    }
}

fn seen_from(machine_id: &str, pane_id: &str, cb: &Value, now: u64) -> Option<Seen> {
    let port = cb["port"].as_u64().filter(|p| (1..=65535).contains(p))? as u16;
    Some(Seen {
        machine_id: machine_id.to_string(),
        pane_id: pane_id.to_string(),
        host: cb["host"]
            .as_str()
            .filter(|h| *h == "localhost" || *h == "127.0.0.1")?
            .to_string(),
        port,
        path: cb["path"].as_str().filter(|p| p.starts_with('/'))?.to_string(),
        at: now,
    })
}

/// Newest last, at most KEEP_MAX, nothing older than KEEP_MS, no duplicates.
fn keep(mut list: Vec<Seen>, s: Seen) -> Vec<Seen> {
    let now = s.at;
    list.retain(|x| {
        now.saturating_sub(x.at) < KEEP_MS
            && !(x.machine_id == s.machine_id
                && x.pane_id == s.pane_id
                && x.port == s.port
                && x.path == s.path
                && x.host == s.host)
    });
    list.push(s);
    let n = list.len().saturating_sub(KEEP_MAX);
    list.drain(..n);
    list
}

/// The newest seen login whose callback is where `pasted` points.
pub fn pick<'a>(list: &'a [Seen], pasted: &str, now: u64) -> Option<&'a Seen> {
    let (host, port, path) = crate::login_relay::place_of(pasted)?;
    list.iter()
        .rev()
        .find(|s| now.saturating_sub(s.at) < KEEP_MS && s.host == host && s.port == port && s.path == path)
}

/// The status-line / attach hint when a login link with a callback arrives.
pub fn hint(callback: &Value) -> String {
    format!(
        "login link sent to your browser; if it ends on {}:{} (unreachable), copy that address and run: tmuxedo login-callback '<address>'",
        callback["host"].as_str().unwrap_or("localhost"),
        callback["port"].as_u64().unwrap_or(0)
    )
}

pub fn run(rest: &[String]) -> Result<(), String> {
    let p = parse_args(rest);
    let Some(url) = p.pos.first().map(|u| u.trim().to_string()) else {
        return Err("login-callback <the address your browser ended on> [--pane <machine>:<pane>]".into());
    };
    if crate::login_relay::place_of(&url).is_none() {
        return Err("that is not an http://localhost:… or http://127.0.0.1:… address; copy the whole address from your browser's address bar".into());
    }
    let cfg = load_cli_config()?;
    let (mid, pane) = match p.f("--pane") {
        Some(t) => {
            let (m, pane) = t.split_once(':').ok_or("--pane <machine>:<pane>, e.g. box:w1:p1")?;
            let m = resolve_machine(&machines(&cfg)?, m)?;
            (
                m["id"].as_str().ok_or("machine without id")?.to_string(),
                pane.to_string(),
            )
        }
        None => {
            let list = load();
            let s = pick(&list, &url, crate::util::now_ms()).ok_or(
                "no login link for that address was seen in the last 15 minutes by an attached tmuxedo; name the pane with --pane <machine>:<pane>",
            )?;
            (s.machine_id.clone(), s.pane_id.clone())
        }
    };
    let (code, v) = api(
        &cfg,
        "POST",
        &format!("/api/machines/{mid}/login-callback"),
        Some(json!({ "paneId": pane, "url": url })),
    )?;
    match code {
        200 if v["ok"] == json!(true) => {
            println!("Delivered to the login in {pane} (it answered HTTP {}).", v["status"]);
            Ok(())
        }
        200 => Err(format!(
            "delivered, but the login in {pane} answered HTTP {}{}",
            v["status"],
            v["excerpt"].as_str().map(|e| format!(": {e}")).unwrap_or_default()
        )),
        _ => Err(format!(
            "{} ({code}): {}",
            v["error"]["code"].as_str().unwrap_or("refused"),
            api_error(&v)
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cb(port: u64, path: &str) -> Value {
        json!({"host": "127.0.0.1", "port": port, "path": path, "state": "s"})
    }

    #[test]
    fn remembers_without_state_and_picks_by_place() {
        let s = seen_from("m1", "w1:p1", &cb(1455, "/auth/callback"), 1_000).unwrap();
        assert!(!serde_json::to_string(&s).unwrap().contains("\"s\""), "no state kept");
        let list = keep(vec![], s);
        let list = keep(
            list,
            seen_from(
                "m2",
                "w2:p1",
                &json!({"host":"localhost","port":54545,"path":"/callback","state":"x"}),
                2_000,
            )
            .unwrap(),
        );
        assert_eq!(
            pick(&list, "http://127.0.0.1:1455/auth/callback?code=1&state=s", 3_000)
                .unwrap()
                .machine_id,
            "m1"
        );
        assert_eq!(
            pick(&list, "http://localhost:54545/callback?code=1&state=x", 3_000)
                .unwrap()
                .pane_id,
            "w2:p1"
        );
        assert!(pick(&list, "http://localhost:54546/callback?code=1", 3_000).is_none());
        assert!(
            pick(&list, "http://127.0.0.1:1455/auth/callback?code=1", 1_000 + KEEP_MS).is_none(),
            "expired"
        );
        assert!(seen_from("m", "p", &json!({"host":"evil.com","port":1,"path":"/"}), 0).is_none());
    }

    #[test]
    fn keeps_the_newest_few() {
        let mut list = vec![];
        for i in 0..20u64 {
            list = keep(
                list,
                seen_from("m", &format!("p{i}"), &cb(1000 + i, "/cb"), 10 + i).unwrap(),
            );
        }
        assert_eq!(list.len(), KEEP_MAX);
        assert_eq!(list.last().unwrap().pane_id, "p19");
        // The same login seen again moves to the end, once.
        let again = keep(list.clone(), seen_from("m", "p12", &cb(1012, "/cb"), 100).unwrap());
        assert_eq!(again.iter().filter(|s| s.pane_id == "p12").count(), 1);
        assert_eq!(again.last().unwrap().pane_id, "p12");
    }

    #[test]
    fn hint_names_the_address_and_command() {
        let h = hint(&cb(1455, "/auth/callback"));
        assert!(
            h.contains("127.0.0.1:1455") && h.contains("tmuxedo login-callback"),
            "{h}"
        );
    }
}
