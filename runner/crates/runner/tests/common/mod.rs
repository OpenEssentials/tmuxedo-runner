//! A fake gateway (tokio-tungstenite server) and helpers shared by the
//! runner's integration tests.
#![allow(dead_code, clippy::result_large_err)]

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::time::{Duration, Instant};
use tmuxedo_protocol::{ChatFrame, DataFrame, DataKind, KIND_CHAT};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

pub const TOKEN: &str = "amr.testlookuptestlookup.testsecrettestsecrettestsecrettestsecret";

pub struct FakeGateway {
    listener: TcpListener,
    pub url: String,
}

pub struct Conn {
    ws: WebSocketStream<TcpStream>,
    pub auth: String,
    pub protocol: String,
    pub texts: Vec<Value>,
    pub bins: Vec<DataFrame>,
    /// CHAT frames (agent chat), in arrival order.
    pub chats: Vec<ChatFrame>,
    /// FILE frames (downloads), in arrival order.
    pub files: Vec<tmuxedo_protocol::files::FileFrame>,
    pub closed: bool,
}

impl FakeGateway {
    pub async fn start() -> FakeGateway {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        FakeGateway {
            listener,
            url: format!("ws://127.0.0.1:{port}/gw/runner"),
        }
    }

    /// Accept one runner connection (or None after `t`).
    pub async fn accept(&self, t: Duration) -> Option<Conn> {
        let (tcp, _) = tokio::time::timeout(t, self.listener.accept()).await.ok()?.ok()?;
        let mut auth = String::new();
        let mut protocol = String::new();
        let ws = tokio_tungstenite::accept_hdr_async(tcp, |req: &Request, mut resp: Response| {
            auth = req
                .headers()
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            protocol = req
                .headers()
                .get("sec-websocket-protocol")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            resp.headers_mut()
                .insert("sec-websocket-protocol", HeaderValue::from_static("tmuxedo.v1"));
            Ok(resp)
        })
        .await
        .ok()?;
        Some(Conn {
            ws,
            auth,
            protocol,
            texts: vec![],
            bins: vec![],
            chats: vec![],
            files: vec![],
            closed: false,
        })
    }
}

impl FakeGateway {
    /// The next connection whose runner says `hello` within `t`, with that
    /// hello. A runner killed while it was reconnecting leaves its connection
    /// in the listen backlog, request and all; [`FakeGateway::accept`] hands
    /// that dead connection over first, and the live runner's connection
    /// waits behind it (events.rs, 18 of 40 runs under 4-way contention).
    /// Connections that close (or fail the handshake) before saying hello are
    /// dropped here.
    pub async fn accept_hello(&self, t: Duration) -> Option<(Conn, Value)> {
        let until = Instant::now() + t;
        loop {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            let Some(mut c) = self.accept(left).await else { continue };
            while !c.closed && Instant::now() < until {
                if let Some(i) = c.texts.iter().position(|v| v["t"] == "hello") {
                    let hello = c.texts.remove(i);
                    return Some((c, hello));
                }
                c.pump(until).await;
            }
        }
    }
}

impl Conn {
    async fn pump(&mut self, until: Instant) -> bool {
        let left = until.saturating_duration_since(Instant::now());
        match tokio::time::timeout(left, self.ws.next()).await {
            Ok(Some(Ok(Message::Text(t)))) => {
                self.texts.push(serde_json::from_str(&t).expect("runner sends JSON"));
                true
            }
            Ok(Some(Ok(Message::Binary(b)))) => {
                if tmuxedo_protocol::files::is_file_frame(&b) {
                    self.files
                        .push(tmuxedo_protocol::files::FileFrame::decode(&b).expect("runner sends valid file frames"));
                } else if b.first() == Some(&KIND_CHAT) {
                    self.chats
                        .push(ChatFrame::decode(&b).expect("runner sends valid chat frames"));
                } else {
                    self.bins
                        .push(DataFrame::decode(&b).expect("runner sends valid data frames"));
                }
                true
            }
            Ok(Some(Ok(_))) => true,
            Ok(Some(Err(_))) | Ok(None) => {
                self.closed = true;
                false
            }
            Err(_) => false,
        }
    }

    /// The next text frame matching `pred` (consumed), within `t`.
    pub async fn text(&mut self, t: Duration, pred: impl Fn(&Value) -> bool) -> Value {
        let until = Instant::now() + t;
        loop {
            if let Some(i) = self.texts.iter().position(&pred) {
                return self.texts.remove(i);
            }
            if Instant::now() >= until || self.closed {
                panic!("no matching text frame; have {:?}", self.texts);
            }
            self.pump(until).await;
        }
    }

    /// Wait for the runner's `hello` and answer `welcome`, as the gateway does.
    pub async fn hello(&mut self, t: Duration) -> Value {
        let h = self.text_t(t, "hello").await;
        self.send(serde_json::json!({"t": "welcome", "machineId": "m1", "userId": "u1"}))
            .await;
        h
    }

    pub async fn text_t(&mut self, t: Duration, kind: &str) -> Value {
        let k = kind.to_string();
        self.text(t, move |v| v["t"] == k.as_str()).await
    }

    pub async fn res(&mut self, id: &str) -> Value {
        let id = id.to_string();
        self.text(Duration::from_secs(10), move |v| {
            v["t"] == "res" && v["id"] == id.as_str()
        })
        .await
    }

    pub async fn bin(&mut self, t: Duration, pred: impl Fn(&DataFrame) -> bool) -> DataFrame {
        let until = Instant::now() + t;
        loop {
            if let Some(i) = self.bins.iter().position(&pred) {
                return self.bins.remove(i);
            }
            if Instant::now() >= until || self.closed {
                panic!("no matching binary frame");
            }
            self.pump(until).await;
        }
    }

    /// Pump until `pred` holds over the CHAT frames received so far.
    pub async fn chats_until(&mut self, t: Duration, pred: impl Fn(&[ChatFrame]) -> bool) {
        let until = Instant::now() + t;
        while !pred(&self.chats) {
            if Instant::now() >= until || self.closed {
                let seen: Vec<String> = self
                    .chats
                    .iter()
                    .map(|f| String::from_utf8_lossy(&f.payload).chars().take(200).collect())
                    .collect();
                panic!("chat frames never matched; have {seen:#?}");
            }
            self.pump(until).await;
        }
    }

    /// Collect frames for `t`.
    pub async fn drain(&mut self, t: Duration) {
        let until = Instant::now() + t;
        while Instant::now() < until && !self.closed {
            self.pump(until).await;
        }
    }

    /// Collect OUTPUT frames with `req` until their text contains `needle`;
    /// returns the frames (in order). Frames stay out of `bins`.
    pub async fn output_until(&mut self, req: u32, needle: &str, t: Duration) -> Vec<DataFrame> {
        let until = Instant::now() + t;
        let mut got = vec![];
        loop {
            let mut i = 0;
            while i < self.bins.len() {
                if self.bins[i].kind == DataKind::Output && self.bins[i].req == req {
                    got.push(self.bins.remove(i));
                } else {
                    i += 1;
                }
            }
            let text: Vec<u8> = got.iter().flat_map(|f| f.payload.clone()).collect();
            if String::from_utf8_lossy(&text).contains(needle) {
                return got;
            }
            if Instant::now() >= until || self.closed {
                panic!("{needle:?} never arrived; got {:?}", String::from_utf8_lossy(&text));
            }
            self.pump(until).await;
        }
    }

    pub async fn send(&mut self, v: Value) {
        self.ws.send(Message::Text(v.to_string().into())).await.unwrap();
    }

    pub async fn send_bin(&mut self, b: Vec<u8>) {
        self.ws.send(Message::Binary(b.into())).await.unwrap();
    }

    pub async fn input(&mut self, pane: &str, text: &str) {
        let f = DataFrame {
            kind: DataKind::Input,
            gap: false,
            req: 0,
            pane_id: pane.into(),
            offset: 0,
            payload: text.as_bytes().to_vec(),
        };
        self.ws.send(Message::Binary(f.encode().unwrap().into())).await.unwrap();
    }

    pub async fn close(&mut self, code: u16) {
        let _ = self
            .ws
            .send(Message::Close(Some(CloseFrame {
                code: CloseCode::from(code),
                reason: "test".into(),
            })))
            .await;
        let until = Instant::now() + Duration::from_secs(2);
        while !self.closed && Instant::now() < until {
            self.pump(until).await;
        }
    }
}

/// Assert frames are contiguous (each starts where the previous ended); returns (start, end, bytes).
pub fn contiguous(frames: &[DataFrame]) -> (u64, u64, Vec<u8>) {
    assert!(!frames.is_empty());
    let start = frames[0].offset;
    let mut pos = start;
    let mut bytes = vec![];
    for f in frames {
        assert_eq!(f.offset, pos, "frame at {} but expected {pos}", f.offset);
        pos += f.payload.len() as u64;
        bytes.extend_from_slice(&f.payload);
    }
    (start, pos, bytes)
}

/// Check `nN` counter lines are consecutive; returns how many were seen.
pub fn check_counter(bytes: &[u8], prefix: &str) -> usize {
    let text = String::from_utf8_lossy(bytes).replace('\r', "");
    // A trailing partial line could be a prefix of a number ("n12" of "n123").
    let complete = match text.rfind('\n') {
        Some(i) => &text[..i],
        None => "",
    };
    let mut prev: Option<u64> = None;
    let mut seen = 0;
    for l in complete.lines() {
        if let Some(n) = l.strip_prefix(prefix).and_then(|n| n.parse::<u64>().ok()) {
            if let Some(p) = prev {
                assert_eq!(n, p + 1, "discontinuity after {prefix}{p}");
            }
            prev = Some(n);
            seen += 1;
        }
    }
    seen
}

// ── in-process daemon harness (runner API over the local socket) ─────────

/// Runners in tests (in process, and the binaries they spawn, which inherit
/// the environment) see a disk with plenty of room, whatever the test
/// machine's disk holds: a nearly full host would otherwise raise a durable
/// `disk_low` event and shift every event seq (crate::files).
///
/// They also never reach a real agent CLI: a PATH folder holding one (a
/// developer's `~/.local/bin/claude`) is dropped, so a pane that would start
/// an agent finds none, as on a clean CI machine. The system folders stay, and
/// so do folders under the temp root: that is where tests put their fakes.
pub fn hermetic_disk() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        std::env::set_var("TMUXEDO_DISK_FAKE", "100000000000,200000000000");
        const AGENTS: [&str; 4] = ["claude", "codex", "cursor-agent", "gemini"];
        let tmp = [
            std::env::temp_dir(),
            std::env::temp_dir().canonicalize().unwrap_or_default(),
        ];
        if let Some(path) = std::env::var_os("PATH") {
            let keep = std::env::split_paths(&path).filter(|d| {
                d == Path::new("/usr/bin")
                    || d == Path::new("/bin")
                    || tmp.iter().any(|t| d.starts_with(t))
                    || !AGENTS.iter().any(|a| d.join(a).exists())
            });
            std::env::set_var("PATH", std::env::join_paths(keep).expect("PATH"));
        }
    });
}

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use tmuxedo_runner::daemon::{self, ExitReason, RunOptions};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// A temp dir whose path is canonical. On macOS the temp root (`/var/folders/…`)
/// is a symlink into `/private/var`, and the runner reports canonical cwds and
/// paths; tests compare against these, so they start from the canonical root.
pub fn tempdir() -> tempfile::TempDir {
    hermetic_disk();
    let root = std::env::temp_dir().canonicalize().expect("temp dir");
    tempfile::Builder::new().tempdir_in(root).unwrap()
}

pub fn start_holder(dir: &Path) -> PathBuf {
    let sock = dir.join("holder.sock");
    let bound = tmuxedo_holder::server::bind(&tmuxedo_holder::Config {
        socket: sock.clone(),
        ring_capacity: tmuxedo_holder::DEFAULT_RING,
    })
    .unwrap();
    std::thread::spawn(move || tmuxedo_holder::server::run(bound));
    sock
}

pub fn options(dir: &Path, url: &str, holder: PathBuf) -> RunOptions {
    let mut o = RunOptions::new(url.into(), TOKEN.into());
    o.state_dir = dir.join("state");
    o.recordings_dir = dir.join("recordings");
    o.holder_socket = holder;
    o.legacy_holder_socket = None;
    o.runner_socket = dir.join("runner.sock");
    o.legacy_runner_socket = None;
    o.start_holder = false;
    o.shell = Some(vec!["sh".into(), "-i".into()]);
    o.home = "/".into();
    // New panes start in $HOME here (as before `config.workDir`); the
    // default-folder tests set their own home and `~/work`.
    o.default_work_dir = "~".into();
    o.cols = 80;
    o.rows = 24;
    o.backoff_min = Duration::from_millis(200);
    o.backoff_max = Duration::from_millis(400);
    o.watchdog = false;
    o.hook_exe = PathBuf::from(env!("CARGO_BIN_EXE_tmuxedo-runner"));
    // Most tests play the agent from this process (`hook.event` on the socket);
    // tests/needs.rs turns the ancestry check back on, as every real runner has it.
    o.verify_hook_callers = false;
    o.worktree_dir = dir.join("worktrees");
    o.files_root = Some(dir.join("files"));
    o.checkpoints_root = Some(dir.join("checkpoints"));
    o
}

pub struct H {
    pub dir: tempfile::TempDir,
    pub sock: PathBuf,
    pub task: tokio::task::JoinHandle<Result<ExitReason, String>>,
    n: std::sync::atomic::AtomicU64,
}

impl H {
    /// A holder + daemon with no gateway (it retries a closed port in the background).
    pub async fn start() -> H {
        Self::start_with(|_| {}).await
    }

    pub async fn start_with(f: impl FnOnce(&mut RunOptions)) -> H {
        let dir = tempdir();
        let holder = start_holder(dir.path());
        let mut o = options(dir.path(), "ws://127.0.0.1:9/gw/runner", holder);
        f(&mut o);
        let sock = o.runner_socket.clone();
        let task = tokio::spawn(daemon::run(o));
        let h = H {
            dir,
            sock,
            task,
            n: Default::default(),
        };
        h.wait_alive("w1:p1").await;
        h
    }

    /// Talk to an already running daemon's socket (no ownership).
    pub async fn attach_to(sock: &Path) -> H {
        let h = H {
            dir: tempdir(),
            sock: sock.to_path_buf(),
            task: tokio::spawn(async { Ok(ExitReason::Signal) }),
            n: Default::default(),
        };
        h.wait_alive("w1:p1").await;
        h
    }

    pub async fn wait_alive(&self, pane: &str) {
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            let r = self
                .raw("pane.read", json!({"pane_id": pane, "source": "visible"}))
                .await;
            if r.get("result").is_some() {
                return;
            }
            assert!(Instant::now() < until, "pane {pane} never came up: {r}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    pub async fn raw(&self, method: &str, params: Value) -> Value {
        let id = format!("t{}", self.n.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
        let req = json!({"id": id, "method": method, "params": params});
        call_once(&self.sock, &req).await
    }

    /// Validate the request and the success response against the runner API schema; return `result`.
    pub async fn ok(&self, method: &str, params: Value) -> Value {
        let id = format!("t{}", self.n.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
        let req = json!({"id": id, "method": method, "params": params});
        schema_check("request", &req)
            .unwrap_or_else(|e| panic!("{method} request invalid per the runner API schema: {e}\n{req}"));
        let resp = call_once(&self.sock, &req).await;
        assert!(resp.get("error").is_none(), "{method} failed: {resp}");
        schema_check("success_response", &resp)
            .unwrap_or_else(|e| panic!("{method} response invalid per the runner API schema: {e}\n{resp}"));
        resp["result"].clone()
    }

    /// Expect an error; the error response must match the schema's error_response.
    pub async fn fails(&self, method: &str, params: Value) -> Value {
        let resp = self.raw(method, params).await;
        schema_check("error_response", &resp)
            .unwrap_or_else(|e| panic!("{method} error invalid per the runner API schema: {e}\n{resp}"));
        resp["error"].clone()
    }

    pub async fn wait_text(&self, pane: &str, needle: &str) -> String {
        // 30 s, not 10: pane_read_is_capped prints ~800 KB, which took over 10 s
        // with the whole suite running on a loaded host. Returns as soon as it appears.
        // Soft-wrapped rows joined: a needle must not depend on the pane width
        // (long temp paths wrap differently from one OS to another).
        let until = Instant::now() + Duration::from_secs(30);
        loop {
            let r = self
                .raw("pane.read", json!({"pane_id": pane, "source": "recent_unwrapped"}))
                .await;
            let t = r["result"]["read"]["text"].as_str().unwrap_or("").to_string();
            if t.contains(needle) {
                return t;
            }
            assert!(Instant::now() < until, "{needle:?} never appeared in {pane}: {t:?}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

pub async fn connect(sock: &Path) -> UnixStream {
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        match UnixStream::connect(sock).await {
            Ok(s) => return s,
            Err(e) if Instant::now() > until => panic!("runner socket {}: {e}", sock.display()),
            Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
}

pub async fn call_once(sock: &Path, req: &Value) -> Value {
    let s = connect(sock).await;
    let (rd, mut wr) = s.into_split();
    let mut line = req.to_string().into_bytes();
    line.push(b'\n');
    wr.write_all(&line).await.unwrap();
    let mut lines = BufReader::new(rd).lines();
    let l = tokio::time::timeout(Duration::from_secs(30), lines.next_line())
        .await
        .expect("response in time")
        .unwrap()
        .unwrap_or_default();
    let resp: Value = serde_json::from_str(&l).unwrap_or_else(|_| panic!("bad response line {l:?}"));
    record(json!({"request": req, "response": resp}));
    resp
}

/// `TMUXEDO_API_RECORD=<file>`: append every request/response pair and every
/// streamed event the tests see, one JSON object per line. The runner API
/// schema (`runner/api/runner-api.schema.json`) is checked against such a
/// recording of the whole suite (`runner/api/README.md`).
pub fn record(v: Value) {
    if let Some(path) = std::env::var_os("TMUXEDO_API_RECORD") {
        use std::io::Write;
        let mut line = v.to_string().into_bytes();
        line.push(b'\n');
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        f.write_all(&line).unwrap();
    }
}

/// A subscription connection: returns a line reader after `subscription_started`.
pub async fn subscribe(sock: &Path, subs: Value) -> tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>> {
    let s = connect(sock).await;
    let (rd, mut wr) = s.into_split();
    let req = json!({"id": "sub", "method": "events.subscribe", "params": {"subscriptions": subs}});
    schema_check("request", &req).unwrap();
    wr.write_all(format!("{req}\n").as_bytes()).await.unwrap();
    std::mem::forget(wr); // keep the connection open for the life of the test
    let mut lines = BufReader::new(rd).lines();
    let ack: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    record(json!({"request": req, "response": ack}));
    schema_check("success_response", &ack).unwrap();
    assert_eq!(ack["result"]["type"], "subscription_started");
    lines
}

pub async fn next_json(lines: &mut tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>) -> Value {
    let l = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
        .await
        .expect("event in time")
        .unwrap()
        .unwrap();
    let v: Value = serde_json::from_str(&l).unwrap();
    record(json!({"event": v}));
    v
}

pub fn api_schema() -> &'static Value {
    static S: OnceLock<Value> = OnceLock::new();
    S.get_or_init(|| {
        let p = concat!(env!("CARGO_MANIFEST_DIR"), "/../../api/runner-api.schema.json");
        serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
    })
}

/// Validate `v` against `#/schemas/<root>` of the runner API schema (runner/api).
pub fn schema_check(root: &str, v: &Value) -> Result<(), String> {
    // Recording for a schema review (TMUXEDO_API_RECORD): capture, do not judge.
    if std::env::var_os("TMUXEDO_API_RECORD").is_some() {
        return Ok(());
    }
    static V: OnceLock<std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<jsonschema::Validator>>>> =
        OnceLock::new();
    let map = V.get_or_init(Default::default);
    let validator = {
        let mut m = map.lock().unwrap();
        m.entry(root.to_string())
            .or_insert_with(|| {
                let mut schema = api_schema().clone();
                let obj = schema.as_object_mut().unwrap();
                obj.insert("$ref".into(), json!(format!("#/schemas/{root}")));
                std::sync::Arc::new(jsonschema::validator_for(&schema).expect("the runner API schema compiles"))
            })
            .clone()
    };
    let errors: Vec<String> = validator
        .iter_errors(v)
        .map(|e| format!("{} at {}", e, e.instance_path()))
        .collect();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

/// The methods the runner API schema lists.
pub fn schema_methods() -> Vec<String> {
    api_schema()["methods"]
        .as_array()
        .expect("methods")
        .iter()
        .map(|m| m.as_str().unwrap().to_string())
        .collect()
}

/// Every request `method` constant anywhere in the schema.
pub fn schema_method_constants() -> Vec<String> {
    fn walk(v: &Value, out: &mut std::collections::BTreeSet<String>) {
        match v {
            Value::Object(m) => {
                if let Some(c) = m
                    .get("properties")
                    .and_then(|p| p.get("method"))
                    .and_then(|x| x.get("const"))
                {
                    if let Some(c) = c.as_str() {
                        out.insert(c.to_string());
                    }
                }
                m.values().for_each(|x| walk(x, out));
            }
            Value::Array(a) => a.iter().for_each(|x| walk(x, out)),
            _ => {}
        }
    }
    let mut out = Default::default();
    walk(api_schema(), &mut out);
    out.into_iter().collect()
}
