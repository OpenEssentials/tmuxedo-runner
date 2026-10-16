//! Login callback relay (docs/protocol.md "Login callback relay"), end to end
//! with a real runner and holder, a fake gateway and a fake local OAuth
//! callback server standing in for Claude Code's `/login` and `codex login`.
//! Expiry (15 minutes) is covered by the unit tests in login_relay.rs.

mod common;

use common::{options, start_holder, Conn, FakeGateway, H};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tmuxedo_runner::daemon;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const S: Duration = Duration::from_secs(20);

/// A one-port HTTP server that records each request (line + Host header)
/// and answers `status` with `body`.
struct FakeCallback {
    port: u16,
    seen: Arc<Mutex<Vec<(String, String)>>>,
}

impl FakeCallback {
    async fn start(want_port: Option<u16>, status: &'static str, body: &'static str) -> FakeCallback {
        let l = match want_port {
            Some(p) => match tokio::net::TcpListener::bind(("127.0.0.1", p)).await {
                Ok(l) => l,
                Err(_) => tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap(),
            },
            None => tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap(),
        };
        let port = l.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(vec![]));
        let s2 = seen.clone();
        tokio::spawn(async move {
            while let Ok((mut c, _)) = l.accept().await {
                let mut buf = vec![0u8; 8192];
                let mut n = 0;
                while !buf[..n].windows(4).any(|w| w == b"\r\n\r\n") {
                    match c.read(&mut buf[n..]).await {
                        Ok(0) | Err(_) => break,
                        Ok(k) => n += k,
                    }
                }
                // A connection that sends nothing is not a request: off Linux the runner
                // checks that the port listens with a bare connect first (login_relay.rs
                // `listening_addr`), which an HTTP server just closes.
                if n == 0 {
                    continue;
                }
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let line = req.lines().next().unwrap_or("").to_string();
                let host = req
                    .lines()
                    .find_map(|l| l.strip_prefix("Host: "))
                    .unwrap_or("")
                    .to_string();
                s2.lock().unwrap().push((line, host));
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: text/html\r\nContent-Length: {}\r\nLocation: https://example.com/success\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = c.write_all(resp.as_bytes()).await;
            }
        });
        FakeCallback { port, seen }
    }
    fn requests(&self) -> Vec<(String, String)> {
        self.seen.lock().unwrap().clone()
    }
}

fn claude_url(port: u16, state: &str) -> String {
    format!(
        "https://claude.ai/oauth/authorize?code=true&client_id=9d1c250a-e61b-44d9-88ed-5944d1962f5e&response_type=code&redirect_uri=http%3A%2F%2Flocalhost%3A{port}%2Fcallback&scope=user%3Ainference&code_challenge=abc&code_challenge_method=S256&state={state}"
    )
}

fn codex_url(port: u16, state: &str) -> String {
    format!(
        "https://auth.openai.com/oauth/authorize?response_type=code&client_id=app_EMoamEEZ73f0CkXaXp7hrann&redirect_uri=http%3A%2F%2F127.0.0.1%3A{port}%2Fauth%2Fcallback&code_challenge=x&code_challenge_method=S256&state={state}&scope=openid"
    )
}

/// What the opener stand-in sends (`pane.open_url` is the opener shim's local-only method).
async fn open(h: &H, params: Value) {
    let r = h.raw("pane.open_url", params).await;
    assert!(r.get("error").is_none(), "{r}");
}

async fn relay(c: &mut Conn, id: &str, pane: &str, url: &str) -> Value {
    c.send(json!({"t": "login_callback", "id": id, "paneId": pane, "url": url}))
        .await;
    c.res(id).await
}

type Task = tokio::task::JoinHandle<Result<daemon::ExitReason, String>>;

async fn start() -> (tempfile::TempDir, FakeGateway, Conn, H, Task) {
    let dir = common::tempdir();
    let holder = start_holder(dir.path());
    let gw = FakeGateway::start().await;
    let o = options(dir.path(), &gw.url, holder);
    let sock = o.runner_socket.clone();
    let task = tokio::spawn(daemon::run(o));
    let mut c = gw.accept(S).await.unwrap();
    let hello = c.hello(S).await;
    assert!(
        hello["runner"]["features"]
            .as_array()
            .unwrap()
            .contains(&json!("login_callback")),
        "{hello}"
    );
    let h = H::attach_to(&sock).await;
    h.wait_alive("w1:p1").await;
    (dir, gw, c, h, task)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn claude_shaped_callback_is_delivered_once_with_its_own_state() {
    let (_dir, _gw, mut c, h, task) = start().await;
    let srv = FakeCallback::start(None, "302 Found", "").await;
    let port = srv.port;
    open(
        &h,
        json!({"pane_id": "w1:p1", "url": claude_url(port, "St4te"), "program": "xdg-open"}),
    )
    .await;
    let ev = c.text(S, |v| v["t"] == "event" && v["type"] == "open_url").await;
    assert_eq!(
        ev["data"]["callback"],
        json!({"host": "localhost", "port": port, "path": "/callback", "state": "St4te"})
    );
    // A link that is not a loopback login carries no callback.
    open(&h, json!({"pane_id": "w1:p1", "url": "https://example.com/x?state=1"})).await;
    let ev = c.text(S, |v| v["t"] == "event" && v["type"] == "open_url").await;
    assert!(ev["data"].get("callback").is_none(), "{ev}");

    let good = format!("http://localhost:{port}/callback?code=AuthC0de&state=St4te");
    let r = relay(
        &mut c,
        "a",
        "w1:p1",
        &format!("http://localhost:{port}/callback?code=AuthC0de&state=nope"),
    )
    .await;
    assert_eq!(r["error"]["code"], "state_mismatch", "{r}");
    let r = relay(
        &mut c,
        "b",
        "w1:p1",
        &format!("http://localhost:{}/callback?code=AuthC0de&state=St4te", port ^ 1),
    )
    .await;
    assert_eq!(r["error"]["code"], "mismatch", "{r}");
    let r = relay(
        &mut c,
        "c",
        "w1:p1",
        &format!("http://localhost:{port}/other?code=AuthC0de&state=St4te"),
    )
    .await;
    assert_eq!(r["error"]["code"], "mismatch", "{r}");
    let r = relay(&mut c, "d", "w1:p2", &good).await;
    assert_eq!(r["error"]["code"], "no_pending", "another pane: {r}");
    assert!(srv.requests().is_empty(), "nothing reached the callback yet");

    let r = relay(&mut c, "e", "w1:p1", &good).await;
    assert_eq!(r["result"], json!({"ok": true, "status": 302}), "{r}");
    let reqs = srv.requests();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].0, "GET /callback?code=AuthC0de&state=St4te HTTP/1.1");
    assert_eq!(reqs[0].1, format!("localhost:{port}"), "the browser's Host");

    let r = relay(&mut c, "f", "w1:p1", &good).await;
    assert_eq!(r["error"]["code"], "already_used", "replay: {r}");
    assert_eq!(srv.requests().len(), 1, "a replay never reaches the callback");
    c.close(4003).await;
    let _ = tokio::time::timeout(S, task).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn codex_shaped_callback_not_listening_and_error_excerpt() {
    let (_dir, _gw, mut c, h, task) = start().await;
    // Nothing listens: a port that was free a moment ago.
    let free = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    open(&h, json!({"pane_id": "w1:p1", "url": codex_url(free, "s1")})).await;
    c.text(S, |v| v["t"] == "event" && v["type"] == "open_url").await;
    let r = relay(
        &mut c,
        "a",
        "w1:p1",
        &format!("http://127.0.0.1:{free}/auth/callback?code=x&state=s1"),
    )
    .await;
    assert_eq!(r["error"]["code"], "not_listening", "{r}");

    // Codex's fixed port when it is free here; its server answers 400 to a
    // code it does not like (the excerpt is masked).
    let srv = FakeCallback::start(
        Some(1455),
        "400 Bad Request",
        "<html><h1>Bad request</h1><p>code=Zm9vYmFyU2VjcmV0 rejected</p></html>",
    )
    .await;
    let port = srv.port;
    open(&h, json!({"pane_id": "w1:p1", "url": codex_url(port, "s2")})).await;
    let ev = c.text(S, |v| v["t"] == "event" && v["type"] == "open_url").await;
    assert_eq!(ev["data"]["callback"]["path"], "/auth/callback");
    assert_eq!(ev["data"]["callback"]["host"], "127.0.0.1");
    let r = relay(
        &mut c,
        "b",
        "w1:p1",
        &format!("http://127.0.0.1:{port}/auth/callback?code=Zm9vYmFyU2VjcmV0&scope=openid&state=s2"),
    )
    .await;
    assert_eq!(r["result"]["ok"], false, "{r}");
    assert_eq!(r["result"]["status"], 400);
    let ex = r["result"]["excerpt"].as_str().unwrap();
    assert!(
        ex.contains("Bad request") && !ex.contains("Zm9vYmFyU2VjcmV0") && !ex.contains('<'),
        "{ex}"
    );
    assert_eq!(
        srv.requests()[0].0,
        "GET /auth/callback?code=Zm9vYmFyU2VjcmV0&scope=openid&state=s2 HTTP/1.1"
    );
    // Spent even though the CLI said no: the code was delivered once.
    let r = relay(
        &mut c,
        "c",
        "w1:p1",
        &format!("http://127.0.0.1:{port}/auth/callback?code=Zm9vYmFyU2VjcmV0&state=s2"),
    )
    .await;
    assert_eq!(r["error"]["code"], "already_used", "{r}");
    // Rate limit: the pane has used its attempts for this minute.
    for i in 0..6 {
        let r = relay(&mut c, &format!("r{i}"), "w1:p1", "http://127.0.0.1:1/x?state=z").await;
        if r["error"]["code"] == "rate_limited" {
            c.close(4003).await;
            let _ = tokio::time::timeout(S, task).await;
            return;
        }
    }
    panic!("never rate limited");
}
