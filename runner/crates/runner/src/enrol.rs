//! `tmuxedo-runner enrol`: the runner device flow (docs/protocol.md, "Runner
//! enrolment"). Web mints the `amr.` token server-side when a signed-in,
//! passkey-stepped-up user approves; the runner only asks, polls, and stores
//! what it is given.

use crate::config::{http_base, RunnerConfig};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

pub fn start_body(device_name: &str) -> Value {
    json!({
        "kind": "runner",
        "deviceName": device_name,
        "os": std::env::consts::OS,
        "arch": crate::util::arch_name(),
    })
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(30))
        .user_agent(&format!("tmuxedo-runner/{}", crate::VERSION))
        .build()
}

fn post(a: &ureq::Agent, url: &str, body: &Value) -> Result<(u16, Value), String> {
    match a.post(url).send_json(body.clone()) {
        Ok(r) => {
            let code = r.status();
            Ok((code, r.into_json().unwrap_or(Value::Null)))
        }
        Err(ureq::Error::Status(code, r)) => Ok((code, r.into_json().unwrap_or(Value::Null))),
        Err(e) => Err(e.to_string()),
    }
}

/// Knobs for tests: the poll interval floor and the slow_down increment.
#[derive(Debug, Clone)]
pub struct Timing {
    pub min_interval: Duration,
    pub slow_down: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            min_interval: Duration::from_secs(1),
            slow_down: Duration::from_secs(5),
        }
    }
}

pub fn enrol(server: &str, name: Option<String>, config_path: &std::path::Path) -> Result<RunnerConfig, String> {
    enrol_with(server, name, config_path, Timing::default())
}

pub fn enrol_with(
    server: &str,
    name: Option<String>,
    config_path: &std::path::Path,
    timing: Timing,
) -> Result<RunnerConfig, String> {
    let base = http_base(server);
    let name = name.unwrap_or_else(crate::util::hostname);
    let a = agent();
    let (code, start) = post(&a, &format!("{base}/api/device/start"), &start_body(&name))?;
    if code != 200 {
        return Err(format!(
            "device start refused ({code}): {}",
            start["error"]
                .as_str()
                .or(start["error"]["message"].as_str())
                .or(start["message"].as_str())
                .unwrap_or("no message")
        ));
    }
    let device_code = start["deviceCode"]
        .as_str()
        .ok_or("device start answer has no deviceCode")?
        .to_string();
    let user_code = start["userCode"].as_str().unwrap_or("?");
    let url = start["verificationUriComplete"]
        .as_str()
        .or(start["verificationUri"].as_str())
        .unwrap_or("");
    let mut interval = Duration::from_secs(start["interval"].as_u64().unwrap_or(5).min(60)).max(timing.min_interval);
    let expires = Duration::from_secs(start["expiresIn"].as_u64().unwrap_or(900).clamp(10, 3600));
    eprintln!();
    eprintln!("  To connect this machine ({name}) to tmuxedo, open:");
    eprintln!();
    eprintln!("    {url}");
    eprintln!();
    eprintln!("  and check the code matches: {user_code}");
    eprintln!();
    let deadline = Instant::now() + expires + Duration::from_secs(30);
    let body = json!({ "deviceCode": device_code });
    let poll_url = format!("{base}/api/device/poll");
    let granted = loop {
        std::thread::sleep(interval);
        if Instant::now() > deadline {
            return Err("timed out waiting for approval".into());
        }
        let (code, poll) = match post(&a, &poll_url, &body) {
            Ok(x) => x,
            Err(e) => {
                eprintln!("  (poll failed: {e}; retrying)");
                continue;
            }
        };
        match (code, poll["error"].as_str()) {
            (200, _) => break poll,
            (428, _) | (_, Some("authorization_pending")) => continue,
            (429, _) | (_, Some("slow_down")) => interval += timing.slow_down,
            (403, _) | (_, Some("access_denied")) => return Err("the request was denied".into()),
            (410, _) | (_, Some("expired_token")) => return Err("the request expired; run enrol again".into()),
            (c, e) => eprintln!("  (poll answered {c} {e:?}; retrying)"),
        }
    };
    let token = granted["runnerToken"]
        .as_str()
        .filter(|t| (t.starts_with("tmr.") || t.starts_with("amr.")) && t.split('.').count() == 3)
        .ok_or("approval carried no valid runnerToken")?
        .to_string();
    let server = granted["server"].as_str().map(str::to_owned).unwrap_or(base);
    let cfg = RunnerConfig {
        server,
        token,
        name: Some(name),
        auto_update: true,
        pane_history: false,
        history_lines: None,
        history_mib: None,
        hooks: true,
        worktree_dir: None,
        automation_labels: Vec::new(),
    };
    cfg.save(config_path)?;
    eprintln!(
        "  Approved{}. Saved {} (mode 0600).",
        granted["machineId"]
            .as_str()
            .map(|m| format!(" as machine {m}"))
            .unwrap_or_default(),
        config_path.display()
    );
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{Arc, Mutex};

    type Seen = Arc<Mutex<Vec<(String, Value)>>>;

    /// A one-request-per-connection HTTP server answering from a script.
    fn fake_web(script: Vec<(u16, Value)>) -> (String, Seen) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(vec![]));
        let seen2 = seen.clone();
        std::thread::spawn(move || {
            for (status, body) in script {
                let Ok((s, _)) = l.accept() else { return };
                let mut r = BufReader::new(s.try_clone().unwrap());
                let mut line = String::new();
                r.read_line(&mut line).unwrap();
                let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
                let mut len = 0;
                loop {
                    let mut h = String::new();
                    r.read_line(&mut h).unwrap();
                    if h == "\r\n" || h.is_empty() {
                        break;
                    }
                    if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap();
                    }
                }
                let mut b = vec![0; len];
                r.read_exact(&mut b).unwrap();
                seen2
                    .lock()
                    .unwrap()
                    .push((path, serde_json::from_slice(&b).unwrap_or(Value::Null)));
                let body = body.to_string();
                let mut s = s;
                write!(
                    s,
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
        });
        (url, seen)
    }

    fn fast() -> Timing {
        Timing {
            min_interval: Duration::from_millis(10),
            slow_down: Duration::from_millis(10),
        }
    }

    fn start_ok() -> (u16, Value) {
        (
            200,
            json!({"deviceCode":"dc-1","userCode":"ABCD-EFGH","verificationUri":"https://w/d",
                   "verificationUriComplete":"https://w/d/ABCD-EFGH","interval":0,"expiresIn":900}),
        )
    }

    #[test]
    fn approved_flow_stores_token_and_server() {
        let token = "tmr.abcdefghijklmnopqrstuv.0123456789012345678901234567890123456789abc"; // gitleaks:allow
        let (url, seen) = fake_web(vec![
            start_ok(),
            (428, json!({"error":"authorization_pending"})),
            (429, json!({"error":"slow_down"})),
            (
                200,
                json!({"kind":"runner","runnerToken":token,"machineId":"m1","server":"https://tmuxedo.example"}),
            ),
        ]);
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("tmuxedo/runner.toml");
        let cfg = enrol_with(&url, Some("box".into()), &path, fast()).unwrap();
        assert_eq!(cfg.token, token);
        assert_eq!(cfg.server, "https://tmuxedo.example");
        assert_eq!(RunnerConfig::load(&path).unwrap(), cfg);
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0].0, "/api/device/start");
        assert_eq!(seen[0].1["kind"], "runner");
        assert_eq!(seen[0].1["deviceName"], "box");
        assert_eq!(seen[0].1["os"], std::env::consts::OS);
        assert_eq!(seen[0].1["arch"], std::env::consts::ARCH);
        assert_eq!(seen.len(), 4);
        for (p, b) in &seen[1..] {
            assert_eq!(p, "/api/device/poll");
            assert_eq!(b, &json!({"deviceCode":"dc-1"}));
        }
    }

    #[test]
    fn an_aimux_era_runner_token_is_still_accepted() {
        let token = "amr.abcdefghijklmnopqrstuv.0123456789012345678901234567890123456789abc"; // gitleaks:allow
        let (url, _) = fake_web(vec![
            start_ok(),
            (
                200,
                json!({"kind":"runner","runnerToken":token,"machineId":"m1","server":"https://s"}),
            ),
        ]);
        let d = tempfile::tempdir().unwrap();
        let cfg = enrol_with(&url, None, &d.path().join("runner.toml"), fast()).unwrap();
        assert_eq!(cfg.token, token);
    }

    #[test]
    fn denied_and_expired_stop() {
        for (code, err, want) in [(403, "access_denied", "denied"), (410, "expired_token", "expired")] {
            let (url, _) = fake_web(vec![start_ok(), (code, json!({ "error": err }))]);
            let d = tempfile::tempdir().unwrap();
            let path = d.path().join("runner.toml");
            let e = enrol_with(&url, Some("box".into()), &path, fast()).unwrap_err();
            assert!(e.contains(want), "{e}");
            assert!(!path.exists());
        }
    }

    #[test]
    fn refused_start_and_bad_token_are_errors() {
        let (url, _) = fake_web(vec![(400, json!({"error":"invalid_kind"}))]);
        let d = tempfile::tempdir().unwrap();
        let e = enrol_with(&url, None, &d.path().join("r.toml"), fast()).unwrap_err();
        assert!(e.contains("400") && e.contains("invalid_kind"), "{e}");
        let (url, _) = fake_web(vec![start_ok(), (200, json!({"kind":"runner","runnerToken":"nope"}))]);
        assert!(enrol_with(&url, None, &d.path().join("r.toml"), fast()).is_err());
    }
}
