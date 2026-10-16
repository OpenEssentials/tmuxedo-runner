//! `tmuxedo-runner pair`: pair this machine by QR code (docs/design/mac-pairing.md
//! §2.5). The terminal shows a QR code and a link; the user scans it with the
//! tmuxedo app (or opens the link in any signed-in browser), checks where the
//! request came from, types the 6-digit code shown here, steps up and approves.
//! The machine polls and stores the token it is given, like `enrol` (whose
//! device-code flow is unchanged and still supported).
//!
//! A CLI pairing is keyless (bearer token): a Linux machine has no Secure
//! Enclave. tmuxedo for Mac pairs with its own key through the same API.
//!
//! Also here: `version --json` and `holder-info`, which the Mac app reads to
//! decide whether an update changed the holder.

use crate::config::{http_base, RunnerConfig};
use serde_json::{json, Value};
use std::path::Path;
use std::time::{Duration, Instant};

/// Features this binary offers its supervisor (the Mac app checks them before it starts a runner).
pub const FEATURES: &[&str] = &[
    "managed",
    "signer-fd",
    "signer-socket",
    "holder-build",
    "pair",
    "holder-info",
    "work-dir",
    "guardrails",
    "confirm-on-mac",
    "socket-group",
];

pub fn version_json() -> String {
    json!({
        "version": crate::VERSION,
        "holderBuild": tmuxedo_holder::BUILD,
        "protocol": tmuxedo_protocol::PROTOCOL_VERSION,
        "holderProto": tmuxedo_holder::HOLDER_PROTO,
        "platform": crate::util::platform_key(),
        "features": FEATURES,
    })
    .to_string()
}

/// `{running, version, pid, build}` of the holder at `socket` (`{running:false}` when none answers).
pub fn holder_info(socket: &Path) -> String {
    use tmuxedo_holder::wire::{self, Event, Msg, Request, HOLDER_PROTO};
    let none = || json!({ "running": false }).to_string();
    let Ok(mut s) = std::os::unix::net::UnixStream::connect(socket) else {
        return none();
    };
    let _ = s.set_read_timeout(Some(Duration::from_secs(3)));
    if wire::write_all(
        &mut s,
        &wire::encode_json(&Request::Hello {
            proto: HOLDER_PROTO,
            client: "holder-info".into(),
        }),
    )
    .is_err()
    {
        return none();
    }
    match wire::read_msg(&mut s) {
        Ok(Msg::Json(j)) => match serde_json::from_slice::<Event>(&j) {
            Ok(Event::Hello {
                proto,
                version,
                pid,
                build,
            }) => {
                json!({ "running": true, "proto": proto, "version": version, "pid": pid, "build": build }).to_string()
            }
            Ok(Event::Error { message, .. }) => json!({ "running": true, "error": message }).to_string(),
            _ => none(),
        },
        _ => none(),
    }
}

/// The QR code for `text`, drawn with half blocks (two rows per line), quiet zone included.
pub fn qr_lines(text: &str) -> Vec<String> {
    use qrcode::render::unicode::Dense1x2;
    use qrcode::{EcLevel, QrCode};
    let Ok(code) = QrCode::with_error_correction_level(text.as_bytes(), EcLevel::M) else {
        return vec![];
    };
    code.render::<Dense1x2>()
        .dark_color(Dense1x2::Light)
        .light_color(Dense1x2::Dark)
        .quiet_zone(true)
        .build()
        .lines()
        .map(str::to_string)
        .collect()
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

/// Knobs for tests.
#[derive(Debug, Clone)]
pub struct Timing {
    pub min_interval: Duration,
    pub slow_down: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            min_interval: Duration::from_secs(2),
            slow_down: Duration::from_secs(3),
        }
    }
}

pub fn pair(server: &str, name: Option<String>, config_path: &Path) -> Result<RunnerConfig, String> {
    pair_with(server, name, config_path, Timing::default(), &mut std::io::stderr())
}

pub fn pair_with(
    server: &str,
    name: Option<String>,
    config_path: &Path,
    timing: Timing,
    out: &mut dyn std::io::Write,
) -> Result<RunnerConfig, String> {
    let base = http_base(server);
    let name = name.unwrap_or_else(crate::util::hostname);
    let a = agent();
    let os = match std::env::consts::OS {
        "macos" => "macos",
        _ => "linux",
    };
    let (code, start) = post(
        &a,
        &format!("{base}/api/pair/start"),
        &json!({ "client": "cli", "deviceName": name, "os": os, "arch": crate::util::arch_name(), "appVersion": crate::VERSION }),
    )?;
    if code != 200 {
        return Err(format!(
            "pairing refused ({code}): {}",
            start["error"]["message"]
                .as_str()
                .or(start["error"].as_str())
                .unwrap_or("no message")
        ));
    }
    let (Some(pairing_id), Some(poll_token), Some(url), Some(app_url), Some(matching)) = (
        start["pairingId"].as_str(),
        start["pollToken"].as_str(),
        start["url"].as_str(),
        start["appUrl"].as_str(),
        start["matchCode"].as_str(),
    ) else {
        return Err("the pairing answer is incomplete".into());
    };
    let mut interval = Duration::from_secs(start["interval"].as_u64().unwrap_or(2).min(30)).max(timing.min_interval);
    let expires = Duration::from_secs(start["expiresIn"].as_u64().unwrap_or(300).clamp(10, 900));
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "  Pair {name} with tmuxedo: scan this with the tmuxedo app on your phone,"
    );
    let _ = writeln!(out);
    for l in qr_lines(app_url) {
        let _ = writeln!(out, "    {l}");
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "  or open this link in a browser where you are signed in:");
    let _ = writeln!(out);
    let _ = writeln!(out, "    {url}");
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "  When asked, type this code:  {} {}",
        &matching[..3.min(matching.len())],
        &matching[3.min(matching.len())..]
    );
    let _ = writeln!(
        out,
        "  (Only approve it yourself. tmuxedo never asks anyone to read this code out.)"
    );
    let _ = writeln!(out);
    let deadline = Instant::now() + expires + Duration::from_secs(10);
    let body = json!({ "pairingId": pairing_id, "pollToken": poll_token });
    let poll_url = format!("{base}/api/pair/poll");
    let mut said_opened = false;
    let granted = loop {
        std::thread::sleep(interval);
        if Instant::now() > deadline {
            return Err("the pairing expired; run `tmuxedo-runner pair` again".into());
        }
        let (code, poll) = match post(&a, &poll_url, &body) {
            Ok(x) => x,
            Err(e) => {
                let _ = writeln!(out, "  (poll failed: {e}; retrying)");
                continue;
            }
        };
        match (code, poll["error"].as_str()) {
            (200, _) => break poll,
            (428, _) | (_, Some("authorization_pending")) => {
                if poll["opened"].as_bool() == Some(true) && !said_opened {
                    said_opened = true;
                    let _ = writeln!(out, "  The approval page is open; finish there.");
                }
            }
            (429, _) | (_, Some("slow_down")) => interval += timing.slow_down,
            (403, _) | (_, Some("access_denied")) => {
                return Err("the pairing was denied or the code did not match".into())
            }
            (410, _) | (_, Some("expired_token")) => {
                return Err("the pairing expired; run `tmuxedo-runner pair` again".into())
            }
            (c, e) => {
                let _ = writeln!(out, "  (poll answered {c} {e:?}; retrying)");
            }
        }
    };
    let token = granted["runnerToken"]
        .as_str()
        .filter(|t| t.starts_with("tmr.") && t.split('.').count() == 3)
        .ok_or("the approval carried no valid runnerToken")?
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
    let _ = writeln!(
        out,
        "  Paired{} ({}). Saved {} (mode 0600).",
        granted["machineId"]
            .as_str()
            .map(|m| format!(" as machine {m}"))
            .unwrap_or_default(),
        granted["method"].as_str().unwrap_or("approved"),
        config_path.display()
    );
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    type Seen = Arc<Mutex<Vec<(String, Value)>>>;

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
                write!(s, "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
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

    fn started() -> (u16, Value) {
        (
            200,
            json!({"pairingId":"p1","pollToken":"pt","url":"https://t/pair?code=L","appUrl":"https://t/m/pair?code=L","matchCode":"123456","appleNonce":"n","interval":0,"expiresIn":300}),
        )
    }

    #[test]
    fn pairs_by_qr_shows_the_match_code_and_stores_the_token() {
        let token = "tmr.abcdefghijklmnopqrstuv.0123456789012345678901234567890123456789abc"; // gitleaks:allow
        let (url, seen) = fake_web(vec![
            started(),
            (428, json!({"error":"authorization_pending","opened":false})),
            (428, json!({"error":"authorization_pending","opened":true})),
            (429, json!({"error":"slow_down"})),
            (
                200,
                json!({"kind":"runner","runnerToken":token,"machineId":"m1","server":"https://tmuxedo.example","method":"phone"}),
            ),
        ]);
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("runner.toml");
        let mut out = vec![];
        let cfg = pair_with(&url, Some("box".into()), &path, fast(), &mut out).unwrap();
        assert_eq!(cfg.token, token);
        assert_eq!(cfg.server, "https://tmuxedo.example");
        assert_eq!(RunnerConfig::load(&path).unwrap(), cfg);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("https://t/pair?code=L"), "{text}");
        assert!(text.contains("123 456"), "{text}");
        assert!(text.contains("The approval page is open"), "{text}");
        assert!(
            text.contains('▀') || text.contains('▄') || text.contains('█'),
            "a QR code is drawn"
        );
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0].0, "/api/pair/start");
        assert_eq!(seen[0].1["client"], "cli");
        assert_eq!(seen[0].1["deviceName"], "box");
        assert!(seen[0].1.get("publicKey").is_none());
        for (p, b) in &seen[1..] {
            assert_eq!(p, "/api/pair/poll");
            assert_eq!(b, &json!({"pairingId":"p1","pollToken":"pt"}));
        }
    }

    #[test]
    fn denied_expired_refused_and_bad_tokens_fail_without_saving() {
        for (code, err, want) in [(403, "access_denied", "denied"), (410, "expired_token", "expired")] {
            let (url, _) = fake_web(vec![started(), (code, json!({ "error": err }))]);
            let d = tempfile::tempdir().unwrap();
            let path = d.path().join("runner.toml");
            let e = pair_with(&url, None, &path, fast(), &mut vec![]).unwrap_err();
            assert!(e.contains(want), "{e}");
            assert!(!path.exists());
        }
        let (url, _) = fake_web(vec![(
            429,
            json!({"error":{"code":"rate_limited","message":"Too many pairing attempts."}}),
        )]);
        let e = pair_with(
            &url,
            None,
            &tempfile::tempdir().unwrap().path().join("r"),
            fast(),
            &mut vec![],
        )
        .unwrap_err();
        assert!(e.contains("429") && e.contains("Too many"), "{e}");
        let (url, _) = fake_web(vec![started(), (200, json!({"runnerToken":"nope"}))]);
        assert!(pair_with(
            &url,
            None,
            &tempfile::tempdir().unwrap().path().join("r"),
            fast(),
            &mut vec![]
        )
        .is_err());
        let (url, _) = fake_web(vec![(200, json!({"pairingId":"p1"}))]);
        assert!(pair_with(
            &url,
            None,
            &tempfile::tempdir().unwrap().path().join("r"),
            fast(),
            &mut vec![]
        )
        .unwrap_err()
        .contains("incomplete"));
    }

    #[test]
    fn qr_and_version_json() {
        let lines = qr_lines("https://tmuxedo.dev/m/pair?code=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA");
        assert!(lines.len() > 10);
        assert!(
            lines.iter().all(|l| l.chars().count() == lines[0].chars().count()),
            "a square"
        );
        let v: Value = serde_json::from_str(&version_json()).unwrap();
        assert_eq!(v["version"], crate::VERSION);
        assert!(v["holderBuild"].as_str().unwrap().starts_with('h'));
        assert_eq!(v["holderBuild"].as_str().unwrap().len(), 17);
        assert!(v["features"].as_array().unwrap().iter().any(|f| f == "signer-fd"));
    }

    #[test]
    fn holder_info_reports_a_running_holder_and_none() {
        let d = tempfile::tempdir().unwrap();
        let sock = d.path().join("holder.sock");
        let none: Value = serde_json::from_str(&holder_info(&sock)).unwrap();
        assert_eq!(none, json!({"running": false}));
        let bound = tmuxedo_holder::server::bind(&tmuxedo_holder::Config {
            socket: sock.clone(),
            ring_capacity: 4096,
        })
        .unwrap();
        std::thread::spawn(move || {
            let _ = tmuxedo_holder::server::run(bound);
        });
        let mut v = Value::Null;
        for _ in 0..50 {
            v = serde_json::from_str(&holder_info(&sock)).unwrap();
            if v["running"] == true {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(v["running"], true);
        assert_eq!(v["build"], tmuxedo_holder::BUILD);
        assert_eq!(v["pid"], std::process::id());
        assert_eq!(v["version"], crate::VERSION);
    }
}
