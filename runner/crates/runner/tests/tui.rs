//! Phase 8 gate: the `tmuxedo tui` process (started through the deprecated `aimux` symlink and AIMUX_* env names, which must keep working), driven through a real PTY, against
//! a real runner (holder + daemon in process) behind a small relay that
//! does what the gateway does for one machine (welcome + cached topology,
//! `attach` → runner `req` mapping, live output fan-out, INPUT/resize/req
//! forwarding, `detach` when the last viewer leaves). A second client on the
//! same pane checks that both stay in sync, and the end checks that
//! `ctrl-b d` only disconnects: the runner's pane keeps running.

#![allow(clippy::result_large_err)]

mod common;

use common::{call_once, options, start_holder};
use futures_util::{SinkExt, StreamExt};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tmuxedo_protocol::{DataFrame, DataKind};
use tmuxedo_runner::daemon;
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Message;

const BIN: &str = env!("CARGO_BIN_EXE_tmuxedo-runner");
const CLI_TOKEN: &str = "amx_testtesttesttesttesttesttesttesttest";
const S: Duration = Duration::from_secs(15);

fn aimux(dir: &Path) -> PathBuf {
    let link = dir.join("aimux");
    if !link.exists() {
        std::os::unix::fs::symlink(BIN, &link).unwrap();
    }
    link
}

/// Web's API for the CLI: machines and attach tickets. Returns (url, ticket scopes asked for).
fn fake_web() -> (String, Arc<Mutex<Vec<String>>>) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    let tickets = Arc::new(Mutex::new(vec![]));
    let t2 = tickets.clone();
    std::thread::spawn(move || {
        for conn in l.incoming() {
            let Ok(s) = conn else { continue };
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut line = String::new();
            if r.read_line(&mut line).is_err() {
                continue;
            }
            let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
            let (mut len, mut authed) = (0usize, false);
            loop {
                let mut h = String::new();
                if r.read_line(&mut h).is_err() || h == "\r\n" || h.is_empty() {
                    break;
                }
                let lower = h.to_ascii_lowercase();
                if let Some(v) = lower.strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap();
                }
                authed |= h.trim() == format!("Authorization: Bearer {CLI_TOKEN}");
            }
            let mut body = vec![0; len];
            let _ = r.read_exact(&mut body);
            let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let (status, resp) = match path.as_str() {
                "/api/machines" if authed => (
                    200,
                    json!({"items":[{"id":"m1","name":"box","online":true,"os":"linux","arch":"x86_64"}]}),
                ),
                "/api/gateway/ticket" if authed && body["machineId"] == "m1" => {
                    let mut t = t2.lock().unwrap();
                    t.push(body["scope"].as_str().unwrap_or("").to_string());
                    (
                        200,
                        json!({"ticket": format!("T{}", t.len()), "scope": body["scope"], "machineId": "m1", "expiresIn": 60}),
                    )
                }
                _ => (401, json!({"error":{"code":"unauthorized","message":"no"}})),
            };
            let resp = resp.to_string();
            let mut s = s;
            let _ = write!(s, "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{resp}", resp.len());
        }
    });
    (url, tickets)
}

// ── the relay (a one-machine gateway) ─────────────────────────────────────

struct Client {
    tx: UnboundedSender<Message>,
    subs: HashSet<String>,
}

#[derive(Default)]
struct Relay {
    runner: Option<UnboundedSender<Message>>,
    topology: Option<Value>,
    clients: HashMap<u32, Client>,
    next_cid: u32,
    /// runner attach req → (client, pane)
    reqs: HashMap<u32, (u32, String)>,
    next_req: u32,
    /// runner-side req id → (client, client's id)
    calls: HashMap<String, (u32, Value)>,
    /// Every text frame a client sent: (client, frame).
    log: Vec<(u32, Value)>,
    /// Every INPUT payload a client sent.
    inputs: Vec<(u32, Vec<u8>)>,
    closed: HashSet<u32>,
}

type Shared = Arc<Mutex<Relay>>;

fn text(v: Value) -> Message {
    Message::Text(v.to_string().into())
}

impl Relay {
    fn to_runner(&self, v: Value) {
        if let Some(r) = &self.runner {
            let _ = r.send(text(v));
        }
    }
    fn release(&mut self, cid: u32, pane: &str) {
        if let Some(c) = self.clients.get_mut(&cid) {
            c.subs.remove(pane);
        }
        if !self.clients.values().any(|c| c.subs.contains(pane)) {
            self.to_runner(json!({"t":"detach","paneId":pane}));
        }
    }
}

async fn start_relay() -> (u16, Shared) {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    let shared: Shared = Default::default();
    let sh = shared.clone();
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = l.accept().await else { return };
            tokio::spawn(serve(tcp, sh.clone()));
        }
    });
    (port, shared)
}

async fn serve(tcp: tokio::net::TcpStream, sh: Shared) {
    let mut path = String::new();
    let Ok(ws) = tokio_tungstenite::accept_hdr_async(tcp, |req: &Request, mut resp: Response| {
        path = req.uri().path().to_string();
        // Answer as a pre-rename gateway would: the TUI must accept the legacy subprotocol.
        resp.headers_mut()
            .insert("sec-websocket-protocol", HeaderValue::from_static("aimux.v1"));
        Ok(resp)
    })
    .await
    else {
        return;
    };
    let (mut sink, mut stream) = ws.split();
    let (tx, mut rx) = unbounded_channel::<Message>();
    tokio::spawn(async move {
        while let Some(m) = rx.recv().await {
            let close = matches!(m, Message::Close(_));
            if sink.send(m).await.is_err() || close {
                break;
            }
        }
    });
    if path == "/gw/runner" {
        while let Some(Ok(m)) = stream.next().await {
            let mut r = sh.lock().unwrap();
            match m {
                Message::Text(t) => {
                    let v: Value = serde_json::from_str(&t).unwrap();
                    match v["t"].as_str().unwrap_or("") {
                        "hello" => {
                            let _ = tx.send(text(json!({"t":"welcome","machineId":"m1","userId":"u1"})));
                            r.runner = Some(tx.clone());
                        }
                        "topology" | "status" | "event" | "size" => {
                            if v["t"] == "topology" {
                                r.topology = Some(v.clone());
                            }
                            for c in r.clients.values() {
                                let _ = c.tx.send(text(v.clone()));
                            }
                        }
                        "res" => {
                            if let Some((cid, id)) = v["id"].as_str().and_then(|i| r.calls.remove(i)) {
                                let mut out = v.clone();
                                out["id"] = id;
                                if let Some(c) = r.clients.get(&cid) {
                                    let _ = c.tx.send(text(out));
                                }
                            }
                        }
                        "ping" => {
                            let _ = tx.send(text(json!({"t":"pong","at":v["at"]})));
                        }
                        _ => {}
                    }
                }
                Message::Binary(b) => {
                    let f = DataFrame::decode(&b).unwrap();
                    let to: Vec<u32> = if f.req == 0 {
                        r.clients
                            .iter()
                            .filter(|(_, c)| c.subs.contains(&f.pane_id))
                            .map(|(id, _)| *id)
                            .collect()
                    } else {
                        r.reqs.get(&f.req).map(|(c, _)| *c).into_iter().collect()
                    };
                    for cid in to {
                        if let Some(c) = r.clients.get(&cid) {
                            let _ = c.tx.send(Message::Binary(b.clone()));
                        }
                    }
                }
                _ => {}
            }
        }
        return;
    }
    let cid = {
        let mut r = sh.lock().unwrap();
        let cid = r.next_cid;
        r.next_cid += 1;
        r.clients.insert(
            cid,
            Client {
                tx: tx.clone(),
                subs: HashSet::new(),
            },
        );
        cid
    };
    while let Some(Ok(m)) = stream.next().await {
        let mut r = sh.lock().unwrap();
        match m {
            Message::Text(t) => {
                let v: Value = serde_json::from_str(&t).unwrap();
                r.log.push((cid, v.clone()));
                let pane = v["paneId"].as_str().unwrap_or("").to_string();
                match v["t"].as_str().unwrap_or("") {
                    "hello" => {
                        let _ = tx.send(text(json!({"t":"welcome","sub":"u1","mid":"m1","scope":"control","machine":{"online":r.runner.is_some()}})));
                        if let Some(t) = &r.topology {
                            let _ = tx.send(text(t.clone()));
                        }
                    }
                    "attach" => {
                        r.clients.get_mut(&cid).unwrap().subs.insert(pane.clone());
                        r.next_req += 1;
                        let req = r.next_req;
                        r.reqs.insert(req, (cid, pane.clone()));
                        let mut a = json!({"t":"attach","req":req,"paneId":pane});
                        if let Some(f) = v.get("from") {
                            a["from"] = f.clone();
                        }
                        r.to_runner(a);
                    }
                    "detach" => r.release(cid, &pane),
                    "resize" => {
                        r.to_runner(json!({"t":"resize","paneId":pane,"cols":v["cols"],"rows":v["rows"]}));
                        for (id, c) in &r.clients {
                            if c.subs.contains(&pane) {
                                let _ = c.tx.send(text(json!({"t":"owner","paneId":pane,"you": *id == cid})));
                            }
                        }
                    }
                    "req" => {
                        let rid = format!("c{cid}-{}", v["id"]);
                        r.calls.insert(rid.clone(), (cid, v["id"].clone()));
                        r.to_runner(json!({"t":"req","id":rid,"method":v["method"],"params":v["params"]}));
                    }
                    "decision" => r.to_runner(v.clone()),
                    _ => {}
                }
            }
            Message::Binary(b) => {
                let f = DataFrame::decode(&b).unwrap();
                assert_eq!(f.kind, DataKind::Input);
                r.inputs.push((cid, f.payload.clone()));
                if let Some(rt) = &r.runner {
                    let _ = rt.send(Message::Binary(b));
                }
            }
            _ => {}
        }
    }
    let mut r = sh.lock().unwrap();
    let subs: Vec<String> = r
        .clients
        .get(&cid)
        .map(|c| c.subs.iter().cloned().collect())
        .unwrap_or_default();
    for p in subs {
        r.release(cid, &p);
    }
    r.clients.remove(&cid);
    r.closed.insert(cid);
}

fn kick(sh: &Shared, cid: u32, code: u16) {
    let r = sh.lock().unwrap();
    let _ = r.clients[&cid].tx.send(Message::Close(Some(CloseFrame {
        code: CloseCode::from(code),
        reason: "test".into(),
    })));
}

async fn until<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let end = Instant::now() + S;
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < end, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}

// ── the TUI in a PTY ──────────────────────────────────────────────────────

struct Tui {
    screen: Arc<Mutex<vt100::Parser>>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    _master: Box<dyn portable_pty::MasterPty + Send>,
}

impl Tui {
    fn start(dir: &Path, web: &str, gw: &str) -> Tui {
        let cfg = dir.join("cli.toml");
        std::fs::write(&cfg, format!("server = '{web}'\ntoken = '{CLI_TOKEN}'\n")).unwrap();
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 30,
                cols: 100,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut cmd = CommandBuilder::new(aimux(dir));
        cmd.arg("tui");
        cmd.env("AIMUX_CLI_CONFIG", &cfg);
        cmd.env("AIMUX_GATEWAY_URL", gw);
        cmd.env("TERM", "xterm-256color");
        cmd.cwd(dir);
        let child = pair.slave.spawn_command(cmd).unwrap();
        drop(pair.slave);
        let screen = Arc::new(Mutex::new(vt100::Parser::new(30, 100, 0)));
        let mut rd = pair.master.try_clone_reader().unwrap();
        let sc = screen.clone();
        std::thread::spawn(move || {
            let mut b = [0u8; 8192];
            while let Ok(n) = rd.read(&mut b) {
                if n == 0 {
                    break;
                }
                sc.lock().unwrap().process(&b[..n]);
            }
        });
        Tui {
            screen,
            writer: pair.master.take_writer().unwrap(),
            child,
            _master: pair.master,
        }
    }
    fn text(&self) -> String {
        self.screen.lock().unwrap().screen().contents()
    }
    async fn shows(&self, needle: &str) {
        let s = self.screen.clone();
        until(&format!("{needle:?} on the TUI screen"), || {
            s.lock().unwrap().screen().contents().contains(needle).then_some(())
        })
        .await;
    }
    fn type_keys(&mut self, s: &[u8]) {
        self.writer.write_all(s).unwrap();
        self.writer.flush().unwrap();
    }
    async fn exits(&mut self) -> portable_pty::ExitStatus {
        let end = Instant::now() + S;
        loop {
            if let Some(st) = self.child.try_wait().unwrap() {
                return st;
            }
            assert!(Instant::now() < end, "the TUI did not exit; screen:\n{}", self.text());
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

// ── a second client (as the web would be) ─────────────────────────────────

#[derive(Default)]
struct Peer {
    bytes: Vec<u8>,
    end: Option<u64>,
}

async fn second_client(port: u16) -> (UnboundedSender<Message>, Arc<Mutex<Peer>>) {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let mut req = format!("ws://127.0.0.1:{port}/gw/client")
        .into_client_request()
        .unwrap();
    // A pre-rename client offers only the legacy subprotocol.
    req.headers_mut()
        .insert("Sec-WebSocket-Protocol", HeaderValue::from_static("aimux.v1, ticket.B"));
    let (ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    let (mut sink, mut stream) = ws.split();
    let (tx, mut rx) = unbounded_channel::<Message>();
    tokio::spawn(async move {
        while let Some(m) = rx.recv().await {
            if sink.send(m).await.is_err() {
                break;
            }
        }
    });
    let peer: Arc<Mutex<Peer>> = Default::default();
    let p2 = peer.clone();
    tokio::spawn(async move {
        while let Some(Ok(m)) = stream.next().await {
            let Message::Binary(b) = m else { continue };
            let f = DataFrame::decode(&b).unwrap();
            let mut p = p2.lock().unwrap();
            match f.kind {
                DataKind::Snapshot => {
                    p.bytes = f.payload.clone();
                    p.end = Some(f.offset);
                }
                DataKind::Output => {
                    let end = f.offset + f.payload.len() as u64;
                    if let Some(e) = p.end {
                        assert_eq!(f.offset, e, "the second client's stream is contiguous");
                    }
                    p.bytes.extend_from_slice(&f.payload);
                    p.end = Some(end);
                }
                DataKind::Input => {}
            }
        }
    });
    tx.send(text(json!({"t":"hello","client":{"kind":"web","version":"test"}})))
        .unwrap();
    tx.send(text(json!({"t":"attach","paneId":"w1:p1"}))).unwrap();
    (tx, peer)
}

fn input(pane: &str, s: &str) -> Message {
    let f = DataFrame {
        kind: DataKind::Input,
        gap: false,
        req: 0,
        pane_id: pane.into(),
        offset: 0,
        payload: s.as_bytes().to_vec(),
    };
    Message::Binary(f.encode().unwrap().into())
}

fn sees(peer: &Arc<Mutex<Peer>>, needle: &str) -> Option<()> {
    String::from_utf8_lossy(&peer.lock().unwrap().bytes)
        .contains(needle)
        .then_some(())
}

/// The last `ack` offset client `cid` sent for w1:p1.
fn last_ack(sh: &Shared, cid: u32) -> Option<u64> {
    sh.lock()
        .unwrap()
        .log
        .iter()
        .rev()
        .find(|(c, v)| *c == cid && v["t"] == "ack" && v["paneId"] == "w1:p1")
        .and_then(|(_, v)| v["offset"].as_u64())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tui_and_a_second_client_stay_in_sync_and_detach_leaves_the_pane_running() {
    let dir = common::tempdir();
    let (port, sh) = start_relay().await;
    let holder = start_holder(dir.path());
    let opts = options(dir.path(), &format!("ws://127.0.0.1:{port}/gw/runner"), holder);
    let runner_sock = opts.runner_socket.clone();
    let daemon = tokio::spawn(daemon::run(opts));
    let s2 = sh.clone();
    until("the runner's topology", || s2.lock().unwrap().topology.clone()).await;

    // ── the TUI attaches the selected workspace's pane through the "gateway" ──
    let (web, tickets) = fake_web();
    let mut tui = Tui::start(dir.path(), &web, &format!("ws://127.0.0.1:{port}/gw/client"));
    let s2 = sh.clone();
    until("the TUI's attach", || {
        s2.lock()
            .unwrap()
            .log
            .iter()
            .any(|(c, v)| *c == 0 && v["t"] == "attach" && v["paneId"] == "w1:p1")
            .then_some(())
    })
    .await;
    assert_eq!(tickets.lock().unwrap().as_slice(), ["control"]);
    tui.shows("box").await;
    tui.shows("1:1").await; // the tab bar
                            // The TUI took the pane's size: its rendered size (100x30 minus sidebar, tab bar, status line).
    let s2 = sh.clone();
    let resize = until("the TUI's resize", || {
        s2.lock()
            .unwrap()
            .log
            .iter()
            .find(|(c, v)| *c == 0 && v["t"] == "resize")
            .map(|(_, v)| v.clone())
    })
    .await;
    assert_eq!(resize, json!({"t":"resize","paneId":"w1:p1","cols":75,"rows":28}));

    // ── (a) a second client on the same pane: both see each other's input ──
    let (b, peer) = second_client(port).await;
    b.send(input("w1:p1", "echo from-b-$((1+1))\n")).unwrap();
    until("the second client's output", || sees(&peer, "from-b-2")).await;
    tui.shows("from-b-2").await;
    tui.type_keys(b"echo from-tui-$((2+2))\r");
    until("the TUI's input at the second client", || sees(&peer, "from-tui-4")).await;
    tui.shows("from-tui-4").await;
    let typed: Vec<u8> = sh
        .lock()
        .unwrap()
        .inputs
        .iter()
        .filter(|(c, _)| *c == 0)
        .flat_map(|(_, b)| b.clone())
        .collect();
    assert_eq!(typed, b"echo from-tui-$((2+2))\r", "keys reach the pane as typed");
    // Same bytes, same offsets: the TUI acknowledges exactly where the second client's stream ends.
    let (s2, p2) = (sh.clone(), peer.clone());
    let synced = until("the TUI's ack to reach the second client's offset", || {
        let end = p2.lock().unwrap().end?;
        (last_ack(&s2, 0) == Some(end)).then_some(end)
    })
    .await;
    assert!(synced > 0);

    // ── reconnect with resume: a dropped socket → fresh ticket, attach from the processed offset ──
    kick(&sh, 0, 1001);
    let s2 = sh.clone();
    let (cid, from) = until("the TUI to re-attach", || {
        s2.lock()
            .unwrap()
            .log
            .iter()
            .find(|(c, v)| *c >= 2 && v["t"] == "attach" && v["paneId"] == "w1:p1")
            .map(|(c, v)| (*c, v["from"].as_u64()))
    })
    .await;
    assert_eq!(from, Some(synced), "resumes from the last processed offset");
    assert_eq!(
        tickets.lock().unwrap().as_slice(),
        ["control", "control"],
        "a fresh ticket"
    );
    b.send(input("w1:p1", "echo after-$((5+5))\n")).unwrap();
    tui.shows("after-10").await;

    // ── (b) detach: the TUI leaves, the pane keeps running ──
    tui.type_keys(b"\x02d");
    let st = tui.exits().await;
    assert!(st.success(), "ctrl-b d exits cleanly");
    let s2 = sh.clone();
    until("the TUI's socket to close", || {
        s2.lock().unwrap().closed.contains(&cid).then_some(())
    })
    .await;
    {
        let r = sh.lock().unwrap();
        let tui_frames: Vec<&Value> = r.log.iter().filter(|(c, _)| *c != 1).map(|(_, v)| v).collect();
        assert!(tui_frames.contains(&&json!({"t":"detach","paneId":"w1:p1"})));
        assert!(
            !tui_frames.iter().any(|v| v["t"] == "req"),
            "detaching sends no method (nothing is closed): {tui_frames:?}"
        );
    }
    b.send(input("w1:p1", "echo still-$((3+3))\n")).unwrap();
    until("the pane to keep running after the TUI left", || sees(&peer, "still-6")).await;
    let list = call_once(&runner_sock, &json!({"id":"l","method":"pane.list","params":{}})).await;
    let panes: Vec<&str> = list["result"]["panes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["pane_id"].as_str().unwrap())
        .collect();
    assert_eq!(panes, ["w1:p1"]);
    assert!(!daemon.is_finished());
    daemon.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tui_stops_reconnecting_after_4003() {
    let dir = common::tempdir();
    let (port, sh) = start_relay().await;
    let (web, tickets) = fake_web();
    let mut tui = Tui::start(dir.path(), &web, &format!("ws://127.0.0.1:{port}/gw/client"));
    let s2 = sh.clone();
    until("the TUI's hello", || {
        s2.lock()
            .unwrap()
            .log
            .iter()
            .any(|(_, v)| v["t"] == "hello")
            .then_some(())
    })
    .await;
    kick(&sh, 0, 4003);
    tui.shows("(revoked)").await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(sh.lock().unwrap().next_cid, 1, "no reconnect after 4003");
    assert_eq!(tickets.lock().unwrap().len(), 1);
    tui.type_keys(b"\x02d");
    assert!(tui.exits().await.success());
}
