//! Runner ↔ gateway, in process: a real holder on a temp socket, the runner
//! daemon, and a fake gateway. Checks hello, topology, attach → SNAPSHOT
//! tagged with req, live OUTPUT with req 0, INPUT reaching the shell, resize
//! → size frame, req methods, replay from an offset, reconnect after the
//! gateway drops, close 4003 stopping reconnection, and the hook shim's
//! held PermissionRequest.

mod common;

use common::{contiguous, options, start_holder, FakeGateway, TOKEN};
use serde_json::json;
use std::time::Duration;
use tmuxedo_protocol::DataKind;
use tmuxedo_runner::daemon::{self, ExitReason};

const S: Duration = Duration::from_secs(10);

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn runner_talks_to_the_gateway() {
    let dir = common::tempdir();
    let holder = start_holder(dir.path());
    let gw = FakeGateway::start().await;
    let task = tokio::spawn(daemon::run(options(dir.path(), &gw.url, holder)));

    // ── hello, topology ──
    let mut c = gw.accept(S).await.expect("runner dials out");
    assert_eq!(c.auth, format!("Bearer {TOKEN}"));
    assert_eq!(
        c.protocol, "tmuxedo.v1, aimux.v1",
        "the new name first, the old one for older gateways"
    );
    let hello = c.text_t(S, "hello").await;
    assert_eq!(hello["v"], 1);
    assert_eq!(hello["runner"]["version"], tmuxedo_runner::VERSION);
    assert_eq!(hello["runner"]["holderVersion"], tmuxedo_runner::VERSION);
    assert!(hello["runner"]["hostname"].as_str().is_some_and(|h| !h.is_empty()));
    assert_eq!(hello["runner"]["os"], tmuxedo_runner::util::os_name());
    let topo = c.text_t(S, "topology").await;
    assert_eq!(topo["workspaces"][0]["id"], "w1");
    assert_eq!(topo["tabs"][0]["id"], "w1:t1");
    assert_eq!(topo["tabs"][0]["layout"]["pane"], "w1:p1");
    assert_eq!(topo["panes"][0]["id"], "w1:p1");
    assert_eq!(topo["panes"][0]["command"], "sh -i");
    assert!(topo["panes"][0]["agent"].is_null());
    c.send(json!({"t":"welcome","machineId":"m1","userId":"u1"})).await;

    // ── attach without an offset → SNAPSHOT tagged with req ──
    c.send(json!({"t":"attach","req":5,"paneId":"w1:p1"})).await;
    let snap = c.bin(S, |f| f.kind == DataKind::Snapshot).await;
    assert_eq!(snap.req, 5);
    assert_eq!(snap.pane_id, "w1:p1");
    assert!(snap.payload.starts_with(b"\x1bc"));
    let snap_at = snap.offset;

    // ── INPUT reaches the shell; live OUTPUT carries req 0 ──
    c.input("w1:p1", "echo hi-$((40+2))\n").await;
    let live = c.output_until(0, "hi-42", S).await;
    let (first_live, mut live_end, mut live_bytes) = contiguous(&live);
    assert!(first_live >= snap_at, "live {first_live} before snapshot {snap_at}");
    assert!(live.iter().all(|f| f.req == 0 && !f.gap));

    // ── resize → size frame, and the PTY really changed ──
    c.send(json!({"t":"resize","paneId":"w1:p1","cols":100,"rows":30}))
        .await;
    let size = c.text_t(S, "size").await;
    assert_eq!(
        (size["paneId"].as_str(), size["cols"].as_u64(), size["rows"].as_u64()),
        (Some("w1:p1"), Some(100), Some(30))
    );
    c.input("w1:p1", "stty size\n").await;
    let more = c.output_until(0, "30 100", S).await;
    let (s2, e2, b2) = contiguous(&more);
    assert_eq!(s2, live_end, "live stream must stay contiguous");
    live_end = e2;
    live_bytes.extend(b2);

    // ── req methods ──
    c.send(json!({"t":"req","id":"r1","method":"ping","params":{}})).await;
    assert_eq!(c.res("r1").await["result"]["type"], "pong");
    c.send(json!({"t":"req","id":"r2","method":"pane.list","params":{}}))
        .await;
    let r = c.res("r2").await;
    assert_eq!(r["result"]["panes"][0]["pane_id"], "w1:p1");
    assert_eq!(r["result"]["panes"][0]["workspace_id"], "w1");
    c.send(json!({"t":"req","id":"r3","method":"pane.read","params":{"pane_id":"w1:p1","source":"visible"}}))
        .await;
    let r = c.res("r3").await;
    assert!(r["result"]["read"]["text"].as_str().unwrap().contains("hi-42"), "{r}");
    c.send(json!({"t":"req","id":"r4","method":"plugin.list","params":{}}))
        .await;
    // Plugins answer over the gateway since web classifies them (PLAN Phase 12).
    assert_eq!(c.res("r4").await["result"]["type"], "plugin_list");
    c.send(json!({"t":"req","id":"r4b","method":"no.such_method","params":{}}))
        .await;
    assert_eq!(c.res("r4b").await["error"]["code"], "unknown_method");
    c.send(json!({"t":"req","id":"r5","method":"pane.send_text","params":{"pane_id":"w1:p1","text":"echo sent-$((5*5))\n"}})).await;
    assert_eq!(c.res("r5").await["result"]["type"], "ok");
    let f = c.output_until(0, "sent-25", S).await;
    let (s3, e3, b3) = contiguous(&f);
    assert_eq!(s3, live_end);
    live_end = e3;
    live_bytes.extend(b3);
    c.send(json!({"t":"req","id":"r6","method":"pane.send_keys","params":{"pane_id":"w1:p1","keys":["e","c","h","o"," ","k","e","y","s","Enter"]}})).await;
    assert_eq!(c.res("r6").await["result"]["type"], "ok");
    let f = c.output_until(0, "\nkeys\r\n", S).await;
    let (s4, e4, b4) = contiguous(&f);
    assert_eq!(s4, live_end);
    live_end = e4;
    live_bytes.extend(b4);
    c.send(json!({"t":"req","id":"r7","method":"pane.read","params":{"pane_id":"nope","source":"visible"}}))
        .await;
    assert_eq!(c.res("r7").await["error"]["code"], "pane_not_found");

    // ── topology changes through `req` → a new topology frame with a higher rev ──
    c.send(json!({"t":"req","id":"r8","method":"pane.split","params":{"target_pane_id":"w1:p1","direction":"right"}}))
        .await;
    assert_eq!(c.res("r8").await["result"]["pane"]["pane_id"], "w1:p2");
    let t2 = c.text_t(S, "topology").await;
    assert!(t2["rev"].as_u64() > topo["rev"].as_u64(), "rev must increase");
    assert_eq!(t2["tabs"][0]["layout"]["dir"], "h");
    assert_eq!(t2["tabs"][0]["layout"]["b"]["pane"], "w1:p2");
    assert_eq!(t2["panes"].as_array().unwrap().len(), 2);

    // ── attach from an offset inside the ring → replay tagged with req ──
    c.send(json!({"t":"attach","req":6,"paneId":"w1:p1","from":first_live}))
        .await;
    c.drain(Duration::from_millis(300)).await;
    let replay: Vec<_> = c.bins.iter().filter(|f| f.req == 6).cloned().collect();
    let (rs, _re, rb) = contiguous(&replay);
    assert_eq!(rs, first_live);
    let n = rb.len().min(live_bytes.len());
    assert!(n > 0);
    assert_eq!(&rb[..n], &live_bytes[..n], "replay differs from what was streamed live");
    assert!(!c.bins.iter().any(|f| f.kind == DataKind::Snapshot && f.req == 6));
    // A stale offset (before the ring) gets a snapshot instead.
    c.send(json!({"t":"attach","req":8,"paneId":"w1:p1","from":1u64 << 50}))
        .await;
    let s8 = c.bin(S, |f| f.req == 8).await;
    assert_eq!(s8.kind, DataKind::Snapshot);
    c.bins.clear();

    // ── the gateway drops: the runner reconnects and resumes from offsets ──
    drop(c);
    let mut c = gw.accept(S).await.expect("runner reconnects");
    c.hello(S).await;
    c.text_t(S, "topology").await;
    c.send(json!({"t":"attach","req":7,"paneId":"w1:p1","from":live_end}))
        .await;
    let f = c.bin(S, |f| f.req == 7).await;
    assert_eq!(f.kind, DataKind::Output);
    assert_eq!(f.offset, live_end, "resume exactly where the old socket stopped");
    c.input("w1:p1", "echo after-$((6*7))\n").await;
    c.output_until(0, "after-42", S).await;

    // ── 4003 revoked: the runner stops and does not reconnect ──
    c.close(4003).await;
    let reason = tokio::time::timeout(S, task)
        .await
        .expect("runner exits")
        .unwrap()
        .unwrap();
    assert_eq!(reason, ExitReason::Fatal(4003));
    assert!(
        gw.accept(Duration::from_secs(2)).await.is_none(),
        "reconnected after 4003"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hook_shim_holds_permission_request_for_a_decision() {
    let dir = common::tempdir();
    let holder = start_holder(dir.path());
    let gw = FakeGateway::start().await;
    let opts = options(dir.path(), &gw.url, holder);
    let runner_sock = opts.runner_socket.clone();
    let task = tokio::spawn(daemon::run(opts));
    let mut c = gw.accept(S).await.unwrap();
    c.hello(S).await;
    c.text_t(S, "topology").await;

    let hook = |stdin: String| {
        let sock = runner_sock.clone();
        tokio::task::spawn_blocking(move || {
            use std::io::Write;
            let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_tmuxedo-runner"))
                .arg("hook")
                // A pane started before the rename has only the AIMUX_* names.
                .env("AIMUX_SOCKET_PATH", &sock)
                .env("AIMUX_PANE_ID", "w1:p1")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            child.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
            let out = child.wait_with_output().unwrap();
            assert!(out.status.success());
            String::from_utf8(out.stdout).unwrap()
        })
    };

    // A non-blocking event returns at once with no output and reports status.
    let out = hook(json!({"hook_event_name":"UserPromptSubmit","session_id":"s1"}).to_string())
        .await
        .unwrap();
    assert_eq!(out, "");
    let st = c.text_t(S, "status").await;
    assert_eq!(st["agent"]["status"], "working");

    // PermissionRequest waits for the decision from the gateway.
    let pending = hook(
        json!({
            "hook_event_name":"PermissionRequest","session_id":"s1",
            "tool_name":"Bash","tool_input":{"command":"touch x"},
            "permission_suggestions":[{"type":"addRules"}]
        })
        .to_string(),
    );
    let st = c.text_t(S, "status").await;
    assert_eq!(st["agent"]["status"], "blocked");
    assert_eq!(st["agent"]["prompt"]["tool"], "Bash");
    assert_eq!(st["agent"]["prompt"]["input"]["command"], "touch x");
    let prompt_id = st["agent"]["promptId"].as_str().unwrap().to_string();
    let ev = c.text_t(S, "event").await;
    assert_eq!(ev["type"], "blocked");
    assert_eq!(ev["seq"], 1);
    // A stale prompt id is refused with an event.
    c.send(json!({"t":"decision","paneId":"w1:p1","promptId":"stale","behavior":"allow"}))
        .await;
    let ev = c.text_t(S, "event").await;
    assert_eq!(ev["type"], "decision_stale");
    assert_eq!(ev["seq"], 2);
    c.send(json!({"t":"decision","paneId":"w1:p1","promptId":prompt_id,"behavior":"deny","message":"not now"}))
        .await;
    let out = tokio::time::timeout(S, pending).await.unwrap().unwrap();
    let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PermissionRequest");
    assert_eq!(v["hookSpecificOutput"]["decision"]["behavior"], "deny");
    assert_eq!(v["hookSpecificOutput"]["decision"]["message"], "not now");

    // The local socket also serves the runner API.
    let r = tokio::task::spawn_blocking({
        let sock = dir.path().join("runner.sock");
        move || {
            use std::io::{BufRead, Write};
            let mut s = std::os::unix::net::UnixStream::connect(sock).unwrap();
            s.write_all(b"{\"id\":\"x\",\"method\":\"pane.list\",\"params\":{}}\n")
                .unwrap();
            let mut line = String::new();
            std::io::BufReader::new(s).read_line(&mut line).unwrap();
            line
        }
    })
    .await
    .unwrap();
    let r: serde_json::Value = serde_json::from_str(&r).unwrap();
    assert_eq!(r["id"], "x");
    assert_eq!(r["result"]["panes"][0]["pane_id"], "w1:p1");

    c.close(4001).await;
    let reason = tokio::time::timeout(S, task).await.unwrap().unwrap().unwrap();
    assert_eq!(reason, ExitReason::Fatal(4001));
}

/// Focus is per machine: a client's `pane.focus` request moves it, and every
/// client sees it in the next topology frame (`focusedWorkspace`, each
/// workspace's `activeTab`, each tab's `focusedPane`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn focus_is_shared_through_topology_frames() {
    let dir = common::tempdir();
    let holder = common::start_holder(dir.path());
    let gw = FakeGateway::start().await;
    let task = tokio::spawn(daemon::run(common::options(dir.path(), &gw.url, holder)));
    let mut c = gw.accept(S).await.unwrap();
    c.hello(S).await;
    let t0 = c.text_t(S, "topology").await;
    assert_eq!(t0["focusedWorkspace"], "w1");
    assert_eq!(t0["workspaces"][0]["activeTab"], "w1:t1");
    assert_eq!(t0["tabs"][0]["focusedPane"], "w1:p1");
    c.send(json!({"t":"req","id":"s","method":"pane.split","params":{"target_pane_id":"w1:p1","direction":"right","focus":true}}))
        .await;
    c.res("s").await;
    let t1 = c
        .text(S, |v| v["t"] == "topology" && v["tabs"][0]["focusedPane"] == "w1:p2")
        .await;
    let rev1 = t1["rev"].as_u64().unwrap();
    c.send(json!({"t":"req","id":"f","method":"pane.focus","params":{"pane_id":"w1:p1"}}))
        .await;
    assert_eq!(c.res("f").await["result"]["pane"]["focused"], true);
    let t2 = c
        .text(S, |v| v["t"] == "topology" && v["tabs"][0]["focusedPane"] == "w1:p1")
        .await;
    assert!(t2["rev"].as_u64().unwrap() > rev1);
    c.close(4003).await;
    let _ = tokio::time::timeout(S, task).await;
}

struct Recording(std::sync::Arc<std::sync::Mutex<Vec<String>>>);
impl tmuxedo_runner::signer::Signer for Recording {
    fn sign(&self, input: &[u8]) -> Result<String, String> {
        self.0.lock().unwrap().push(String::from_utf8(input.to_vec()).unwrap());
        Ok("SIG_by_the_key".into())
    }
}

/// A key-bound token (docs/protocol.md "Runner proof"): the runner answers the
/// gateway's challenge through its signer with exactly the proof input, and
/// without a signer it answers nothing and stops at the gateway's 4003.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn key_bound_runner_answers_the_challenge_through_its_signer() {
    let dir = common::tempdir();
    let holder = common::start_holder(dir.path());
    let gw = FakeGateway::start().await;
    let seen = std::sync::Arc::new(std::sync::Mutex::new(vec![]));
    let mut opts = common::options(dir.path(), &gw.url, holder.clone());
    opts.signer = Some(tmuxedo_runner::signer::SignerHandle(std::sync::Arc::new(Recording(
        seen.clone(),
    ))));
    let task = tokio::spawn(daemon::run(opts));
    let mut c = gw.accept(S).await.expect("runner dials out");
    c.hello(S).await;
    let nonce = "Zm9vYmFyZm9vYmFyZm9vYmFyZm9vYmFyZm9vYmFyZm8";
    c.send(json!({"t":"challenge","nonce":nonce})).await;
    let proof = c.text_t(S, "proof").await;
    assert_eq!(proof, json!({"t":"proof","sig":"SIG_by_the_key"}));
    let lookup = TOKEN.split('.').nth(1).unwrap();
    assert_eq!(
        *seen.lock().unwrap(),
        vec![tmuxedo_protocol::runner_proof_input(nonce, lookup)]
    );
    c.send(json!({"t":"welcome","machineId":"m1","userId":"u1"})).await;
    c.close(4003).await;
    let _ = tokio::time::timeout(S, task).await;

    // No signer: nothing is signed or sent; the gateway's 4003 proof_failed stops the runner for good.
    let gw2 = FakeGateway::start().await;
    let task = tokio::spawn(daemon::run(common::options(dir.path(), &gw2.url, holder)));
    let mut c = gw2.accept(S).await.expect("runner dials out");
    c.hello(S).await;
    c.send(json!({"t":"challenge","nonce":nonce})).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    c.close(4003).await;
    let reason = tokio::time::timeout(S, task).await.unwrap().unwrap().unwrap();
    assert_eq!(reason, ExitReason::Fatal(4003));
    assert!(
        gw2.accept(Duration::from_secs(3)).await.is_none(),
        "never retried as a bearer token"
    );
}

/// Machine-side controls (docs/design/mac-isolation.md §3): enforced by the runner whatever
/// the gateway sends. Observe-only: no input reaches a pane, mutating methods and decisions are
/// refused, reading works. The allow-list: a welcome for another account stops the runner.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn guardrails_hold_whatever_the_gateway_sends() {
    let dir = common::tempdir();
    let holder = common::start_holder(dir.path());
    let gw = FakeGateway::start().await;
    let mut opts = common::options(dir.path(), &gw.url, holder.clone());
    opts.guardrails = tmuxedo_runner::guardrails::Guardrails {
        allow_user: Some("u1".into()),
        observe_only: true,
    };
    let task = tokio::spawn(daemon::run(opts));
    let mut c = gw.accept(S).await.expect("runner dials out");
    c.hello(S).await;
    c.text_t(S, "topology").await;
    c.send(json!({"t":"welcome","machineId":"m1","userId":"u1"})).await;
    c.send(json!({"t":"attach","req":2,"paneId":"w1:p1"})).await;
    c.bin(S, |f| f.kind == DataKind::Snapshot).await;
    // Input is dropped: the marker never appears in the pane's output.
    c.input("w1:p1", "echo typed-$((6*7))\n").await;
    c.send(json!({"t":"req","id":"r1","method":"pane.send_text","params":{"pane_id":"w1:p1","text":"echo sent-$((6*7))\n"}})).await;
    let refused = c.res("r1").await;
    assert_eq!(refused["error"]["code"], "observe_only");
    c.send(json!({"t":"req","id":"r2","method":"pane.list","params":{}}))
        .await;
    assert_eq!(
        c.res("r2").await["result"]["panes"][0]["pane_id"],
        "w1:p1",
        "reading still works"
    );
    c.send(json!({"t":"req","id":"r3","method":"workspace.create","params":{}}))
        .await;
    assert_eq!(c.res("r3").await["error"]["code"], "observe_only");
    tokio::time::sleep(Duration::from_millis(800)).await;
    c.send(json!({"t":"req","id":"r4","method":"pane.read","params":{"pane_id":"w1:p1","source":"recent"}}))
        .await;
    let text = c.res("r4").await.to_string();
    assert!(
        !text.contains("typed-42") && !text.contains("sent-42"),
        "input reached the pane in observe-only mode: {text}"
    );
    c.close(4003).await;
    let _ = tokio::time::timeout(S, task).await;

    // The allow-list: tmuxedo names another account → the runner stops for good.
    let gw2 = FakeGateway::start().await;
    let mut opts = common::options(dir.path(), &gw2.url, holder);
    opts.guardrails = tmuxedo_runner::guardrails::Guardrails {
        allow_user: Some("u1".into()),
        observe_only: false,
    };
    let task = tokio::spawn(daemon::run(opts));
    let mut c = gw2.accept(S).await.expect("runner dials out");
    c.hello(S).await;
    c.send(json!({"t":"welcome","machineId":"m1","userId":"someone-else"}))
        .await;
    let reason = tokio::time::timeout(S, task).await.unwrap().unwrap().unwrap();
    assert_eq!(reason, ExitReason::Fatal(4003));
}

/// The Mac app's side of confirm-on-this-Mac: answers `confirm` with a ticket and every
/// `confirm_poll` with whatever the test has set (pending until the "person" answers).
#[derive(Clone)]
struct Mac {
    answer: std::sync::Arc<std::sync::Mutex<&'static str>>,
    asked: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}
impl Mac {
    fn new(answer: &'static str) -> Self {
        Self {
            answer: std::sync::Arc::new(std::sync::Mutex::new(answer)),
            asked: Default::default(),
        }
    }
    fn set(&self, a: &'static str) {
        *self.answer.lock().unwrap() = a;
    }
    fn asked(&self) -> usize {
        self.asked.load(std::sync::atomic::Ordering::SeqCst)
    }
}
impl tmuxedo_runner::signer::Signer for Mac {
    fn sign(&self, _: &[u8]) -> Result<String, String> {
        Err("not asked to sign here".into())
    }
    fn call(&self, req: &serde_json::Value) -> Result<serde_json::Value, String> {
        match req["op"].as_str() {
            Some("confirm") => {
                self.asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(json!({ "ticket": "t1" }))
            }
            Some("confirm_poll") => Ok(json!({ "state": *self.answer.lock().unwrap() })),
            _ => Err("bad request".into()),
        }
    }
}

async fn confirming_runner(
    dir: &std::path::Path,
    holder: std::path::PathBuf,
    mac: &Mac,
) -> (common::FakeGateway, tokio::task::JoinHandle<Result<ExitReason, String>>) {
    let gw = FakeGateway::start().await;
    let mut opts = common::options(dir, &gw.url, holder);
    opts.confirm = tmuxedo_runner::confirm::Policy {
        enabled: true,
        ..Default::default()
    };
    opts.confirm_poll = Duration::from_millis(20);
    opts.signer = Some(tmuxedo_runner::signer::SignerHandle(std::sync::Arc::new(mac.clone())));
    let task = tokio::spawn(daemon::run(opts));
    (gw, task)
}

/// Confirm on this Mac (docs/design/mac-isolation.md §3), whatever the gateway sends:
/// remote input is HELD until the Mac allows it, then replayed in order; one question for
/// a burst of input; a denied or unanswered question drops the held input and refuses
/// mutating calls; watching and reading never ask.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn confirm_on_mac_holds_control_until_allowed() {
    let dir = common::tempdir();
    let holder = common::start_holder(dir.path());

    // ── Allow ──
    let mac = Mac::new("pending");
    let (gw, task) = confirming_runner(dir.path(), holder.clone(), &mac).await;
    let mut c = gw.accept(S).await.expect("runner dials out");
    c.hello(S).await;
    c.text_t(S, "topology").await;
    c.send(json!({"t":"welcome","machineId":"m1","userId":"u1"})).await;
    c.send(json!({"t":"attach","req":2,"paneId":"w1:p1"})).await;
    c.bin(S, |f| f.kind == DataKind::Snapshot).await;
    assert_eq!(mac.asked(), 0, "watching never asks");
    c.input("w1:p1", "echo typed-$((6*7))\n").await;
    c.input("w1:p1", "echo more-$((7*7))\n").await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    c.send(json!({"t":"req","id":"r1","method":"pane.read","params":{"pane_id":"w1:p1","source":"recent"}}))
        .await;
    let text = c.res("r1").await.to_string();
    assert!(
        !text.contains("typed-42"),
        "input reached the pane before the Mac allowed it: {text}"
    );
    assert_eq!(mac.asked(), 1, "one question for a burst of input");
    c.send(json!({"t":"req","id":"r2","method":"pane.send_text","params":{"pane_id":"w1:p1","text":"echo x\n"}}))
        .await;
    assert_eq!(c.res("r2").await["error"]["code"], "confirm_pending");
    mac.set("allow");
    c.output_until(0, "more-49", S).await;
    c.send(json!({"t":"req","id":"r3","method":"pane.read","params":{"pane_id":"w1:p1","source":"recent"}}))
        .await;
    let text = c.res("r3").await.to_string();
    let (a, b) = (text.find("typed-42").unwrap(), text.find("more-49").unwrap());
    assert!(a < b, "held input replayed in order");
    // Open now: control passes without a new question.
    c.send(json!({"t":"req","id":"r4","method":"pane.send_text","params":{"pane_id":"w1:p1","text":"echo now-$((8*8))\n"}})).await;
    assert_eq!(c.res("r4").await["result"]["type"], "ok");
    c.output_until(0, "now-64", S).await;
    assert_eq!(mac.asked(), 1);
    c.close(4003).await;
    let _ = tokio::time::timeout(S, task).await;

    // ── Deny, then Timeout: the held input is dropped and mutating calls are refused ──
    for answer in ["deny", "timeout"] {
        let mac = Mac::new(answer);
        let (gw, task) = confirming_runner(dir.path(), holder.clone(), &mac).await;
        let mut c = gw.accept(S).await.expect("runner dials out");
        c.hello(S).await;
        c.send(json!({"t":"welcome","machineId":"m1","userId":"u1"})).await;
        c.send(json!({"t":"attach","req":2,"paneId":"w1:p1"})).await;
        c.bin(S, |f| f.kind == DataKind::Snapshot).await;
        c.input("w1:p1", &format!("echo {answer}-$((9*9))\n")).await;
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(mac.asked(), 1, "{answer}");
        c.send(json!({"t":"req","id":"r5","method":"pane.send_text","params":{"pane_id":"w1:p1","text":"echo y\n"}}))
            .await;
        assert_eq!(c.res("r5").await["error"]["code"], "confirm_denied", "{answer}");
        // More input in the cool-off: dropped, and no new question.
        c.input("w1:p1", &format!("echo again-{answer}\n")).await;
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(mac.asked(), 1, "{answer}: no question during the cool-off");
        c.send(json!({"t":"req","id":"r6","method":"pane.read","params":{"pane_id":"w1:p1","source":"recent"}}))
            .await;
        let text = c.res("r6").await.to_string();
        assert!(
            !text.contains(&format!("{answer}-81")) && !text.contains(&format!("again-{answer}")),
            "{answer}: input reached the pane: {text}"
        );
        c.close(4003).await;
        let _ = tokio::time::timeout(S, task).await;
    }
}

/// Long pane history (decision 0009): an attach SNAPSHOT carries the newest
/// lines and says which (the history OSC); `pane.read` source `history`
/// pages back by line index to the configured limit, contiguous and
/// complete; `clear` (CSI 3 J) empties it and indices keep counting.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn history_pages_back_from_the_snapshot_to_the_limit() {
    let dir = common::tempdir();
    let holder = start_holder(dir.path());
    let gw = FakeGateway::start().await;
    let mut o = options(dir.path(), &gw.url, holder);
    o.history = tmuxedo_runner::config::HistorySize {
        lines: 20_000,
        bytes: 32 << 20,
    };
    let sock = o.runner_socket.clone();
    let task = tokio::spawn(daemon::run(o));
    let mut c = gw.accept(S).await.expect("runner dials out");
    c.hello(S).await;
    let h = common::H::attach_to(&sock).await;
    h.ok(
        "pane.send_text",
        json!({"pane_id":"w1:p1","text":"seq -f 'row %g' 1 30000; echo SEQ-$((2*21))\n"}),
    )
    .await;
    h.wait_text("w1:p1", "SEQ-42").await;

    // ── the attach snapshot: newest lines only, and which ones ──
    c.send(json!({"t":"attach","req":7,"paneId":"w1:p1"})).await;
    let snap = c.bin(S, |f| f.req == 7 && f.kind == DataKind::Snapshot).await;
    assert!(snap.payload.len() < 256 * 1024, "snapshot {} bytes", snap.payload.len());
    let text = String::from_utf8_lossy(&snap.payload).into_owned();
    let osc = text
        .split("\x1b]777;tmuxedo-history;")
        .nth(1)
        .and_then(|s| s.split('\x07').next())
        .expect("history OSC after ESC c")
        .to_string();
    let field = |k: &str| -> u64 {
        osc.split(';')
            .find_map(|kv| kv.strip_prefix(&format!("{k}=")))
            .unwrap_or_else(|| panic!("{k} in {osc}"))
            .parse()
            .unwrap()
    };
    let (start, first, end) = (field("start"), field("first"), field("end"));
    assert_eq!(field("limit"), 20_000);
    assert_eq!(end - start, 20_000, "the limit holds: {osc}");
    // Short rows: about 8,900 fit in the snapshot's 96 KiB; the rest are older.
    assert!(first > start + 1500 && first < end, "{osc}");
    assert!(text.contains("row 29990"));

    // ── page back from the snapshot's first line to the start ──
    let mut before = first;
    let mut lines: Vec<String> = vec![];
    let mut pages = 0;
    loop {
        let r = h
            .ok(
                "pane.read",
                json!({"pane_id":"w1:p1","source":"history","before":before,"lines":1000,"format":"text"}),
            )
            .await;
        let hist = &r["read"]["history"];
        assert_eq!(
            hist["first"].as_u64().unwrap() + hist["count"].as_u64().unwrap(),
            before
        );
        assert_eq!(hist["start"].as_u64(), Some(start));
        let mut page: Vec<String> = r["read"]["text"]
            .as_str()
            .unwrap()
            .split('\n')
            .map(str::to_owned)
            .collect();
        assert_eq!(page.len() as u64, hist["count"].as_u64().unwrap());
        page.append(&mut lines);
        lines = page;
        pages += 1;
        before = hist["first"].as_u64().unwrap();
        if !hist["more"].as_bool().unwrap() {
            break;
        }
    }
    assert!(pages >= 3, "{pages} pages");
    assert_eq!(before, start);
    // Contiguous rows, oldest first, ending right before the snapshot's first line.
    let nums: Vec<u64> = lines
        .iter()
        .filter_map(|l| l.strip_prefix("row "))
        .map(|n| n.parse().unwrap())
        .collect();
    assert_eq!(nums.len(), lines.len(), "only rows that old: {:?}", &lines[..3]);
    assert!(nums.windows(2).all(|w| w[1] == w[0] + 1), "rows not contiguous");

    // ANSI pages end every line with CRLF and are the same lines.
    let r = h
        .ok(
            "pane.read",
            json!({"pane_id":"w1:p1","source":"history","before":first,"lines":3}),
        )
        .await;
    assert_eq!(r["read"]["format"], "ansi");
    let t = r["read"]["text"].as_str().unwrap();
    assert_eq!(t.matches("\r\n").count(), 3);
    assert!(t.ends_with(&format!("{}\r\n", lines.last().unwrap())), "{t:?}");
    let e = h
        .fails("pane.read", json!({"pane_id":"w1:p1","source":"history","before":"x"}))
        .await;
    assert_eq!(e["code"], "invalid_params");

    // ── `clear` empties it; indices keep counting ──
    h.ok(
        "pane.send_text",
        json!({"pane_id":"w1:p1","text":"clear; echo CLEARED-$((3*3))\n"}),
    )
    .await;
    h.wait_text("w1:p1", "CLEARED-9").await;
    let r = h.ok("pane.read", json!({"pane_id":"w1:p1","source":"history"})).await;
    let hist = &r["read"]["history"];
    assert!(hist["start"].as_u64().unwrap() >= end, "{hist}");
    assert_eq!(hist["start"], hist["end"]);
    assert_eq!(hist["more"], false);
    c.close(4003).await;
    let _ = tokio::time::timeout(S, task).await;
}
