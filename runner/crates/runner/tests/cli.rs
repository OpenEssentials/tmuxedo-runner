//! The `tmuxedo` CLI as a real process (invoked through a `tmuxedo` symlink;
//! tests/tui.rs goes through the deprecated `aimux` one):
//! device-flow login against a fake web, `attach` through a fake client
//! gateway (ticket, hello, attach, output, input with local terminal replies
//! filtered out, ack flow control, ctrl-b d), and runner API verbs against a
//! running runner's local socket.

#![allow(clippy::result_large_err)]

mod common;

use common::H;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tmuxedo_protocol::{DataFrame, DataKind};
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;

const BIN: &str = env!("CARGO_BIN_EXE_tmuxedo-runner");
const CLI_TOKEN: &str = "amx_testtesttesttesttesttesttesttesttest";

fn tmuxedo(dir: &Path) -> PathBuf {
    let link = dir.join("tmuxedo");
    if !link.exists() {
        std::os::unix::fs::symlink(BIN, &link).unwrap();
    }
    link
}

type Seen = Arc<Mutex<Vec<(String, String, Value)>>>;

/// Minimal HTTP/1.1 server answering web's API by path (one request per connection).
fn fake_web() -> (String, Seen) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    let seen: Seen = Arc::new(Mutex::new(vec![]));
    let s2 = seen.clone();
    std::thread::spawn(move || {
        let mut polls = 0;
        for conn in l.incoming() {
            let Ok(s) = conn else { continue };
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut line = String::new();
            if r.read_line(&mut line).is_err() {
                continue;
            }
            let mut parts = line.split_whitespace();
            let (method, path) = (
                parts.next().unwrap_or("").to_string(),
                parts.next().unwrap_or("").to_string(),
            );
            let (mut len, mut auth) = (0usize, String::new());
            loop {
                let mut h = String::new();
                r.read_line(&mut h).unwrap();
                if h == "\r\n" || h.is_empty() {
                    break;
                }
                let lower = h.to_ascii_lowercase();
                if let Some(v) = lower.strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap();
                }
                if lower.starts_with("authorization:") {
                    auth = h["authorization:".len()..].trim().to_string();
                }
            }
            let mut body = vec![0; len];
            r.read_exact(&mut body).unwrap();
            let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            s2.lock().unwrap().push((path.clone(), auth.clone(), body.clone()));
            let authed = auth == format!("Bearer {CLI_TOKEN}");
            let (status, resp) = match (method.as_str(), path.as_str()) {
                ("POST", "/api/device/start") => (
                    200,
                    json!({"kind":"cli","userCode":"ABCD-EFGH","verificationUrl":"http://x/signin/device/ABCD-EFGH","expiresAt":"2030-01-01T00:00:00Z","pollIntervalSeconds":1}),
                ),
                ("POST", "/api/device/poll") => {
                    polls += 1;
                    (
                        200,
                        json!({"status": if polls < 2 { "pending" } else { "approved" }, "pollIntervalSeconds": 1}),
                    )
                }
                ("GET", "/api/machines") if authed => (
                    200,
                    json!({"items":[{"id":"m1","name":"box","online":true,"os":"linux","arch":"x86_64"}]}),
                ),
                ("POST", "/api/gateway/ticket") if authed && body["machineId"] == "m1" => (
                    200,
                    json!({"ticket":"T1","scope":body["scope"],"machineId":"m1","expiresIn":60}),
                ),
                _ => (401, json!({"error":{"code":"unauthorized","message":"no"}})),
            };
            let resp = resp.to_string();
            let mut s = s;
            let _ = write!(s, "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{resp}", resp.len());
        }
    });
    (url, seen)
}

#[test]
fn login_runs_the_cli_device_flow() {
    let dir = common::tempdir();
    let (web, seen) = fake_web();
    let cfg = dir.path().join("cli.toml");
    let out = Command::new(tmuxedo(dir.path()))
        .args(["login", "--server", &web, "--name", "test box"])
        .env("TMUXEDO_CLI_CONFIG", &cfg)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = std::fs::read_to_string(&cfg).unwrap();
    let saved: toml::Value = toml::from_str(&text).unwrap();
    let token = saved["token"].as_str().unwrap();
    assert!(token.starts_with("tmx_"));
    let seen = seen.lock().unwrap();
    let start = &seen.iter().find(|s| s.0 == "/api/device/start").unwrap().2;
    assert_eq!(start["kind"], "cli");
    assert_eq!(start["deviceName"], "test box");
    assert_eq!(start["prefix"], &token[..12]);
    assert_eq!(start["tokenHash"], tmuxedo_runner::util::sha256_hex(token.as_bytes()));
    assert!(
        !start.to_string().contains(token),
        "the plaintext token never leaves the machine"
    );
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(&cfg).unwrap().permissions().mode() & 0o777, 0o600);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attach_passes_one_pane_through_the_gateway() {
    let dir = common::tempdir();
    let (web, seen) = fake_web();
    let cfg = dir.path().join("cli.toml");
    std::fs::write(&cfg, format!("server = '{web}'\ntoken = '{CLI_TOKEN}'\n")).unwrap();
    let gw = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gw_url = format!("ws://{}/gw/client", gw.local_addr().unwrap());

    let mut child = Command::new(tmuxedo(dir.path()))
        .args(["attach", "box:w1:p1"])
        .env("TMUXEDO_CLI_CONFIG", &cfg)
        .env("TMUXEDO_GATEWAY_URL", &gw_url)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let stdout = Arc::new(Mutex::new(Vec::new()));
    {
        let mut so = child.stdout.take().unwrap();
        let buf = stdout.clone();
        std::thread::spawn(move || {
            let mut b = [0u8; 4096];
            while let Ok(n) = so.read(&mut b) {
                if n == 0 {
                    break;
                }
                buf.lock().unwrap().extend_from_slice(&b[..n]);
            }
        });
    }

    let (tcp, _) = tokio::time::timeout(Duration::from_secs(15), gw.accept())
        .await
        .unwrap()
        .unwrap();
    let mut protocol = String::new();
    let mut ws = tokio_tungstenite::accept_hdr_async(tcp, |req: &Request, mut resp: Response| {
        protocol = req
            .headers()
            .get("sec-websocket-protocol")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        // Answer as a pre-rename gateway would: the CLI must accept the legacy subprotocol.
        resp.headers_mut()
            .insert("sec-websocket-protocol", HeaderValue::from_static("aimux.v1"));
        Ok(resp)
    })
    .await
    .unwrap();
    assert!(
        protocol.starts_with("tmuxedo.v1, aimux.v1") && protocol.contains("ticket.T1"),
        "{protocol}"
    );
    assert!(seen
        .lock()
        .unwrap()
        .iter()
        .any(|s| s.0 == "/api/gateway/ticket" && s.2["scope"] == "control"));

    async fn next_text(ws: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>, t: &str) -> Value {
        loop {
            let m = tokio::time::timeout(Duration::from_secs(10), ws.next())
                .await
                .expect("frame in time")
                .unwrap()
                .unwrap();
            if let Message::Text(s) = m {
                let v: Value = serde_json::from_str(&s).unwrap();
                if v["t"] == t {
                    return v;
                }
            }
        }
    }
    async fn next_bin(ws: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>) -> DataFrame {
        loop {
            let m = tokio::time::timeout(Duration::from_secs(10), ws.next())
                .await
                .expect("frame in time")
                .unwrap()
                .unwrap();
            if let Message::Binary(b) = m {
                return DataFrame::decode(&b).unwrap();
            }
        }
    }
    let hello = next_text(&mut ws, "hello").await;
    assert_eq!(hello["client"]["kind"], "tui");
    ws.send(Message::Text(
        json!({"t":"welcome","sub":"u1","mid":"m1","scope":"control","machine":{"online":true}})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    let attach = next_text(&mut ws, "attach").await;
    assert_eq!(attach["paneId"], "w1:p1");
    assert!(attach.get("from").is_none());
    let frame = |kind, offset, payload: &[u8]| {
        let f = DataFrame {
            kind,
            gap: false,
            req: 1,
            pane_id: "w1:p1".into(),
            offset,
            payload: payload.to_vec(),
        };
        Message::Binary(f.encode().unwrap().into())
    };
    ws.send(frame(DataKind::Snapshot, 100, b"\x1bc$ ")).await.unwrap();
    // The program asks for DA1 and the cursor position.
    let out1 = b"hello-from-pane \x1b[c\x1b[6n";
    ws.send(frame(DataKind::Output, 100, out1)).await.unwrap();
    let until = std::time::Instant::now() + Duration::from_secs(10);
    while !String::from_utf8_lossy(&stdout.lock().unwrap()).contains("hello-from-pane") {
        assert!(std::time::Instant::now() < until, "output never reached stdout");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // The local terminal's replies are typed back with the user's input: only the input goes out.
    stdin.write_all(b"ls\n\x1b[?62;22c\x1b[3;7R").unwrap();
    stdin.flush().unwrap();
    let input = next_bin(&mut ws).await;
    assert_eq!(input.kind, DataKind::Input);
    assert_eq!(input.payload, b"ls\n");
    // 70 KiB of output: an ack follows.
    let end = 100 + out1.len() as u64;
    let big = vec![b'x'; 70 * 1024];
    ws.send(frame(DataKind::Output, end, &big)).await.unwrap();
    let ack = next_text(&mut ws, "ack").await;
    assert_eq!(ack["paneId"], "w1:p1");
    assert_eq!(ack["offset"].as_u64(), Some(end + big.len() as u64));
    // ctrl-b ctrl-b sends a literal ctrl-b; ctrl-b d detaches.
    stdin.write_all(b"\x02\x02").unwrap();
    stdin.flush().unwrap();
    assert_eq!(next_bin(&mut ws).await.payload, vec![0x02]);
    stdin.write_all(b"\x02d").unwrap();
    stdin.flush().unwrap();
    next_text(&mut ws, "detach").await;
    let status = tokio::task::spawn_blocking(move || child.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(status.success());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn local_verbs_talk_to_the_runner_socket() {
    let h = H::start().await;
    let bin = tmuxedo(h.dir.path());
    let sock = h.sock.clone();
    let run = move |args: &[&str]| {
        let out = Command::new(&bin)
            .args(args)
            .env("TMUXEDO_SOCKET_PATH", &sock)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice::<Value>(&out.stdout).unwrap()
    };
    let r = tokio::task::spawn_blocking(move || {
        let list = run(&["pane", "list"]);
        assert_eq!(list["panes"][0]["pane_id"], "w1:p1");
        let split = run(&["pane", "split", "w1:p1", "--direction", "down"]);
        assert_eq!(split["pane"]["pane_id"], "w1:p2");
        std::thread::sleep(Duration::from_millis(500));
        run(&["pane", "run", "w1:p2", "echo", "hi-$((1+1))-cli"]);
        let w = run(&[
            "pane",
            "wait-output",
            "w1:p2",
            "--match",
            "hi-2-cli",
            "--timeout",
            "5000",
        ]);
        assert_eq!(w["type"], "output_matched");
        let ws = run(&["workspace", "list"]);
        ws["workspaces"][0]["pane_count"].clone()
    })
    .await
    .unwrap();
    assert_eq!(r, 2);
}

#[test]
fn machines_lists_the_users_machines_from_web() {
    let dir = common::tempdir();
    let (web, seen) = fake_web();
    let cfg = dir.path().join("cli.toml");
    let run = |cfg: &Path| {
        Command::new(tmuxedo(dir.path()))
            .arg("machines")
            .env("TMUXEDO_CLI_CONFIG", cfg)
            .output()
            .unwrap()
    };
    let out = run(&cfg);
    assert!(!out.status.success(), "no login yet");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("login"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    std::fs::write(&cfg, format!("server = '{web}'\ntoken = '{CLI_TOKEN}'\n")).unwrap();
    let out = run(&cfg);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8_lossy(&out.stdout);
    let row = text
        .lines()
        .find(|l| l.starts_with("box"))
        .unwrap_or_else(|| panic!("{text}"));
    for want in ["online", "linux/x86_64", "m1"] {
        assert!(row.contains(want), "{want} missing in {row:?}");
    }
    let seen = seen.lock().unwrap();
    let req = seen.iter().find(|s| s.0 == "/api/machines").unwrap();
    assert_eq!(req.1, format!("Bearer {CLI_TOKEN}"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_explain_shows_the_screen_reading() {
    let h = H::start().await;
    let bin = tmuxedo(h.dir.path());
    // The Codex stand-in (runner/tests/standin, no hooks) on its approval screen.
    let standin = h.dir.path().join("standin-bin");
    std::fs::create_dir_all(&standin).unwrap();
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/standin");
    std::os::unix::fs::symlink(&script, standin.join("codex")).unwrap();
    let screens = Path::new(env!("CARGO_MANIFEST_DIR")).join("../detect/tests/screens");
    h.ok(
        "pane.send_text",
        json!({"pane_id": "w1:p1", "text": format!("STANDIN_SCREENS='{}' '{}'\n", screens.display(), standin.join("codex").display())}),
    )
    .await;
    h.wait_text("w1:p1", "OpenAI Codex").await;
    h.ok("pane.send_text", json!({"pane_id": "w1:p1", "text": "b"})).await;
    h.wait_text("w1:p1", "Allow command?").await;
    let sock = h.sock.clone();
    tokio::task::spawn_blocking(move || {
        let run = |args: &[&str]| {
            let out = Command::new(&bin)
                .args(args)
                .env("TMUXEDO_SOCKET_PATH", &sock)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            serde_json::from_slice::<Value>(&out.stdout).unwrap()
        };
        let until = std::time::Instant::now() + Duration::from_secs(20);
        let ex = loop {
            let ex = run(&["agent", "explain", "w1:p1"]);
            if ex["explain"]["tmuxedo"]["effective_status"] == "blocked" || std::time::Instant::now() > until {
                break ex;
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        let e = &ex["explain"];
        assert_eq!(e["agent"], "codex", "{ex}");
        assert_eq!(e["status"], "blocked");
        assert_eq!(e["signal"], "approval");
        assert_eq!(e["default"], false);
        assert_eq!(e["source"], "built-in");
        assert!(e["revision"].as_str().unwrap().len() > 3);
        let signals = e["signals"].as_array().unwrap();
        let approval = signals.iter().find(|s| s["name"] == "approval").unwrap();
        assert_eq!(approval["matched"], true);
        assert_eq!(approval["status"], "blocked");
        assert_eq!(approval["scope"], "after-composer");
        assert!(approval["when"].as_str().unwrap().contains("allow command?"));
        assert!(approval["scope_preview"].as_str().unwrap().contains("Allow command?"));
        assert!(signals.iter().all(|s| s["rank"].is_i64() && s["scope_bytes"].is_u64()));
        assert_eq!(e["tmuxedo"]["source"], "screen");
        assert_eq!(e["tmuxedo"]["screen"], "blocked");
        assert_eq!(e["tmuxedo"]["process_kind"], "codex");
        assert_eq!(e["aimux"], e["tmuxedo"], "deprecated alias");
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_verbs_and_explain_end_to_end() {
    let h = H::start().await;
    let bin = tmuxedo(h.dir.path());
    let sock = h.sock.clone();
    let run = move |args: &[&str]| {
        let out = Command::new(&bin)
            .args(args)
            .env("TMUXEDO_SOCKET_PATH", &sock)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice::<Value>(&out.stdout).unwrap()
    };
    tokio::task::spawn_blocking(move || {
        run(&[
            "pane",
            "report-agent",
            "w1:p1",
            "--source",
            "cli-test",
            "--agent",
            "claude",
            "--state",
            "blocked",
        ]);
        let list = run(&["agent", "list"]);
        assert_eq!(list["agents"][0]["pane_id"], "w1:p1", "{list}");
        let got = run(&["agent", "get", "w1:p1"]);
        assert_eq!(got["agent"]["agent_status"], "blocked");
        let ex = run(&["agent", "explain", "w1:p1"]);
        assert_eq!(ex["explain"]["tmuxedo"]["effective_status"], "blocked", "{ex}");
        assert_eq!(ex["explain"]["tmuxedo"]["source"], "reported", "{ex}");
        let w = run(&["agent", "wait", "w1:p1", "--until", "blocked", "--timeout", "3000"]);
        assert_eq!(w["agent"]["agent_status"], "blocked");
        run(&["agent", "rename", "w1:p1", "builder"]);
        assert_eq!(run(&["agent", "get", "builder"])["agent"]["pane_id"], "w1:p1");
        run(&[
            "pane",
            "report-agent",
            "w1:p1",
            "--source",
            "cli-test",
            "--agent",
            "claude",
            "--state",
            "idle",
        ]);
        assert_eq!(run(&["agent", "get", "builder"])["agent"]["agent_status"], "idle");
    })
    .await
    .unwrap();
}
