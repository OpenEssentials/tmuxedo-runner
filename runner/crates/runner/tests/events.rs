//! Durable events with real processes (docs/protocol.md, `events_ack`):
//! an event that was sent but never acknowledged, plus one raised while the
//! gateway was unreachable, both survive a runner SIGKILL and are delivered
//! after the restart's `welcome`; `events_ack` trims the on-disk outbox; a
//! duplicate ack and a reconnect after the ack resend nothing; seq stays
//! monotonic across the restart.

mod common;

use common::{options, start_holder, FakeGateway, H, TOKEN};
use serde_json::{json, Value};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tmuxedo_runner::daemon;

const BIN: &str = env!("CARGO_BIN_EXE_tmuxedo-runner");
const S: Duration = Duration::from_secs(15);

struct Killer(Child);
impl Drop for Killer {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn holder(sock: &Path, log: &Path) -> Killer {
    let child = Command::new(BIN)
        .args(["holder", "--socket"])
        .arg(sock)
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(log).unwrap())
        .spawn()
        .unwrap();
    let until = Instant::now() + S;
    while std::os::unix::net::UnixStream::connect(sock).is_err() {
        assert!(Instant::now() < until, "holder did not start");
        std::thread::sleep(Duration::from_millis(50));
    }
    Killer(child)
}

fn sigkill(k: &mut Killer) {
    // SAFETY: plain kill(2) on our own child.
    unsafe { libc::kill(k.0.id() as i32, libc::SIGKILL) };
    let _ = k.0.wait();
}

async fn report(h: &H, state: &str) {
    h.ok(
        "pane.report_agent",
        json!({"pane_id": "w1:p1", "source": "test", "agent": "claude", "state": state}),
    )
    .await;
}

fn outbox_lines(d: &Path) -> usize {
    std::fs::read_to_string(d.join("state/events.jsonl"))
        .map(|s| s.lines().count())
        .unwrap_or(0)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unacked_events_survive_a_runner_sigkill_and_ack_trims() {
    let dir = common::tempdir();
    let d = dir.path();
    let cfg = d.join("runner.toml");
    std::fs::write(
        &cfg,
        format!("server = 'http://127.0.0.1:9'\ntoken = '{TOKEN}'\nauto_update = false\n"),
    )
    .unwrap();
    let hsock = d.join("h.sock");
    let rsock = d.join("runner.sock");
    let gw = FakeGateway::start().await;
    let runner = |n: u32| {
        Killer(
            Command::new(BIN)
                .args(["run", "--no-start-holder", "--server", &gw.url, "--shell", "sh"])
                // The deprecated AIMUX_* names on purpose: they must keep working.
                .env("AIMUX_RUNNER_TOKEN", TOKEN)
                .env("AIMUX_STATE_DIR", d.join("state"))
                .env("AIMUX_HOLDER_SOCKET", &hsock)
                .env("AIMUX_RUNNER_SOCKET", &rsock)
                .env("AIMUX_CONFIG", &cfg)
                .env("HOME", d)
                .stdout(Stdio::null())
                .stderr(std::fs::File::create(d.join(format!("runner{n}.log"))).unwrap())
                .spawn()
                .unwrap(),
        )
    };

    let _h = holder(&hsock, &d.join("holder.log"));
    let mut r1 = runner(1);
    let mut c = gw.accept(S).await.expect("runner connects");
    c.hello(S).await;
    let h = H::attach_to(&rsock).await;

    // 1. Sent but never acknowledged.
    report(&h, "working").await;
    report(&h, "blocked").await;
    let first = c.text(S, |v| v["t"] == "event" && v["type"] == "blocked").await;
    let seq1 = first["seq"].as_u64().unwrap();
    assert!(outbox_lines(d) >= 1, "the event is on disk before the ack");

    // 2. Gateway unreachable: the next event is only queued (fsynced).
    drop(c);
    let before = outbox_lines(d);
    report(&h, "working").await;
    report(&h, "blocked").await;
    let until = Instant::now() + S;
    while outbox_lines(d) <= before {
        assert!(Instant::now() < until, "offline event never reached the outbox");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // 3. Runner SIGKILL, restart: both arrive after welcome, in order.
    sigkill(&mut r1);
    let _r2 = runner(2);
    // Runner 1 may have been reconnecting when it was killed: its dead connection can be
    // first in the backlog, so take the one that says hello (runner 2's).
    let (mut c, _) = gw.accept_hello(S).await.expect("runner 2 connects");
    c.drain(Duration::from_millis(500)).await;
    assert!(!c.texts.iter().any(|v| v["t"] == "event"), "no events before welcome");
    c.send(json!({"t": "welcome", "machineId": "m1", "userId": "u1"})).await;
    let again = c
        .text(S, |v| v["t"] == "event" && v["seq"].as_u64() == Some(seq1))
        .await;
    assert_eq!(
        again["type"], "blocked",
        "unacked event is resent (web drops the duplicate)"
    );
    let offline = c.text(S, |v| v["t"] == "event" && v["seq"].as_u64() > Some(seq1)).await;
    assert_eq!(offline["type"], "blocked", "the event raised offline arrives");
    let seq2 = offline["seq"].as_u64().unwrap();
    assert!(seq2 > seq1);

    // 4. events_ack trims the on-disk outbox; a duplicate ack is harmless.
    c.send(json!({"t": "events_ack", "upTo": seq2})).await;
    let until = Instant::now() + S;
    while outbox_lines(d) != 0 {
        assert!(
            Instant::now() < until,
            "ack upTo {seq2} did not trim the outbox: {:?}\nreceived: {:?}\nrunner2: {}",
            std::fs::read_to_string(d.join("state/events.jsonl")),
            c.texts,
            std::fs::read_to_string(d.join("runner2.log")).unwrap_or_default()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    c.send(json!({"t": "events_ack", "upTo": seq2})).await;

    // 5. Reconnect after the ack: nothing is resent; seq keeps climbing.
    drop(c);
    let (mut c, _) = gw.accept_hello(S).await.expect("runner 2 reconnects");
    c.send(json!({"t": "welcome", "machineId": "m1", "userId": "u1"})).await;
    let h = H::attach_to(&rsock).await;
    report(&h, "working").await;
    report(&h, "blocked").await;
    // The first event on this connection is the new one: acked ones are not resent.
    let next: Value = c.text(S, |v| v["t"] == "event").await;
    assert_eq!(next["type"], "blocked");
    assert!(
        next["seq"].as_u64().unwrap() > seq2,
        "seq is monotonic across restarts: {next}"
    );
}

/// `welcome.lastEventSeq` (docs/protocol.md): a runner whose seq is behind
/// web's continues above it, and an event queued before the welcome is
/// renumbered above it before it is sent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn welcome_last_event_seq_lifts_and_renumbers() {
    let dir = common::tempdir();
    let holder = start_holder(dir.path());
    let gw = FakeGateway::start().await;
    let o = options(dir.path(), &gw.url, holder);
    let sock = o.runner_socket.clone();
    let task = tokio::spawn(daemon::run(o));
    let mut c = gw.accept(S).await.unwrap();
    c.text_t(S, "hello").await;
    let h = H::attach_to(&sock).await;
    report(&h, "working").await;
    report(&h, "blocked").await; // queued as seq 1, not sent before welcome
    c.drain(Duration::from_millis(300)).await;
    assert!(!c.texts.iter().any(|v| v["t"] == "event"));
    c.send(json!({"t": "welcome", "machineId": "m1", "userId": "u1", "lastEventSeq": 40}))
        .await;
    let ev = c.text(S, |v| v["t"] == "event").await;
    assert_eq!(
        (ev["type"].as_str(), ev["seq"].as_u64()),
        (Some("blocked"), Some(41)),
        "{ev}"
    );
    report(&h, "working").await;
    report(&h, "blocked").await;
    let ev = c.text(S, |v| v["t"] == "event").await;
    assert_eq!(ev["seq"].as_u64(), Some(42));
    let state: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.path().join("state/state.json")).unwrap()).unwrap();
    assert_eq!(state["eventSeq"], 42, "the lifted seq is persisted");
    c.close(4003).await;
    let _ = tokio::time::timeout(S, task).await;
}

/// Upgrade: a state.json written by runner r000001 (Phase 2 format) is
/// migrated, not discarded — the topology, `rev` and `eventSeq` carry on.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn r000001_state_upgrades_without_resetting_rev_or_seq() {
    let dir = common::tempdir();
    std::fs::create_dir_all(dir.path().join("state")).unwrap();
    let fixture = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/state-r000001.json"
    ))
    .unwrap()
    // Same format; a shell and cwd that exist in the test environment.
    .replace("/bin/zsh", "/bin/sh")
    .replace("/home/u", "/");
    std::fs::write(dir.path().join("state/state.json"), fixture).unwrap();
    let holder = start_holder(dir.path());
    let gw = FakeGateway::start().await;
    let o = options(dir.path(), &gw.url, holder);
    let sock = o.runner_socket.clone();
    let task = tokio::spawn(daemon::run(o));
    let mut c = gw.accept(S).await.unwrap();
    c.hello(S).await;
    let topo = c.text_t(S, "topology").await;
    assert!(
        topo["rev"].as_u64().unwrap() > 5,
        "rev continues past r000001's 5: {topo}"
    );
    assert_eq!(topo["panes"][0]["id"], "w1:p1");
    assert_eq!(topo["panes"][0]["command"], "/bin/sh -l");
    let h = H::attach_to(&sock).await;
    report(&h, "working").await;
    report(&h, "blocked").await;
    let ev = c.text(S, |v| v["t"] == "event").await;
    assert_eq!(ev["seq"].as_u64(), Some(4), "seq continues past r000001's 3: {ev}");
    let state: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.path().join("state/state.json")).unwrap()).unwrap();
    assert_eq!(state["version"], 2);
    c.close(4003).await;
    let _ = tokio::time::timeout(S, task).await;
}

/// Claude auth health: `hello.runner.claudeAuth`, then a durable `auth_*`
/// event for each transition — credentials appearing (Linux file check),
/// a pane showing "Please run /login", a StopFailure auth error from the
/// hook, and a completed turn. The token's `expiresAt` is never read.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn claude_auth_health_is_reported_and_alerted() {
    let dir = common::tempdir();
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let holder = start_holder(dir.path());
    let gw = FakeGateway::start().await;
    let mut o = options(dir.path(), &gw.url, holder);
    o.home = home.to_string_lossy().into_owned();
    o.claude_auth = Some(true);
    let sock = o.runner_socket.clone();
    let task = tokio::spawn(daemon::run(o));
    let mut c = gw.accept(S).await.unwrap();
    let hello = c.hello(S).await;
    if cfg!(target_os = "linux") {
        assert_eq!(hello["runner"]["claudeAuth"]["state"], "missing", "{hello}");
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        std::fs::write(
            home.join(".claude/.credentials.json"),
            r#"{"claudeAiOauth":{"expiresAt":1}}"#,
        )
        .unwrap();
        let ok = c.text(S, |v| v["t"] == "event" && v["type"] == "auth_ok").await;
        assert_eq!(
            (ok["paneId"].as_str(), ok["data"]["source"].as_str()),
            (Some(""), Some("credentials"))
        );
    }
    let h = H::attach_to(&sock).await;
    h.ok(
        "pane.send_text",
        json!({"pane_id": "w1:p1", "text": "printf 'Login expired %s\\n' '· Please run /login'\n"}),
    )
    .await;
    let exp = c.text(S, |v| v["t"] == "event" && v["type"] == "auth_expired").await;
    assert_eq!(exp["paneId"], "w1:p1");
    assert_eq!(exp["data"]["source"], "screen");
    let hook = |name: &str, extra: Value| {
        let mut ev = json!({"hook_event_name": name, "session_id": "s1"});
        for (k, v) in extra.as_object().unwrap() {
            ev[k] = v.clone();
        }
        json!({"paneId": "w1:p1", "event": ev})
    };
    h.raw(
        "hook.event",
        hook("StopFailure", json!({"error_type": "oauth_org_not_allowed"})),
    )
    .await;
    let failed = c.text(S, |v| v["t"] == "event" && v["type"] == "auth_failed").await;
    assert_eq!(failed["data"]["reason"], "oauth_org_not_allowed");
    h.raw("hook.event", hook("StopFailure", json!({"error_type": "rate_limit"})))
        .await;
    h.raw("hook.event", hook("Stop", json!({}))).await;
    let ok = c.text(S, |v| v["t"] == "event" && v["type"] == "auth_ok").await;
    assert_eq!(ok["data"]["reason"], "turn completed");
    assert!(
        !c.texts
            .iter()
            .any(|v| v["t"] == "event" && v["type"].as_str().is_some_and(|t| t.starts_with("auth_"))),
        "rate_limit is not an auth alert: {:?}",
        c.texts
    );
    // A reconnect's hello carries the current state.
    c.close(1001).await;
    let mut c = gw.accept(S).await.unwrap();
    let hello = c.hello(S).await;
    assert_eq!(hello["runner"]["claudeAuth"]["state"], "ok");
    c.close(4003).await;
    let _ = tokio::time::timeout(S, task).await;
}

/// Audit P2-16a: a held PermissionRequest expires at the runner's deadline
/// (540 s in production): the hook gets `{expired: true}` (it prints
/// nothing, Claude's dialog stays), the status drops the promptId, a
/// `prompt_expired` event goes out, and a late decision is
/// `decision_stale`, never accepted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_held_prompt_expires_and_a_late_decision_is_stale() {
    assert_eq!(daemon::PROMPT_HOLD, Duration::from_secs(540));
    const _: () = assert!(tmuxedo_runner::hook::PERMISSION_WAIT.as_secs() > daemon::PROMPT_HOLD.as_secs());
    let dir = common::tempdir();
    let holder = start_holder(dir.path());
    let gw = FakeGateway::start().await;
    let mut o = options(dir.path(), &gw.url, holder);
    o.prompt_hold = Duration::from_secs(1);
    let sock = o.runner_socket.clone();
    let task = tokio::spawn(daemon::run(o));
    let mut c = gw.accept(S).await.unwrap();
    c.hello(S).await;
    let h = H::attach_to(&sock).await;
    let ev = json!({"paneId": "w1:p1", "event": {"hook_event_name": "PermissionRequest", "session_id": "s1",
        "tool_name": "Bash", "tool_input": {"command": "rm -rf build"}}});
    let t0 = Instant::now();
    let held = tokio::spawn(async move { h.raw("hook.event", ev).await });
    let blocked = c
        .text(S, |v| v["t"] == "status" && v["agent"]["status"] == "blocked")
        .await;
    let prompt_id = blocked["agent"]["promptId"].as_str().unwrap().to_string();
    let reply = held.await.unwrap();
    assert_eq!(reply["result"]["expired"], true, "{reply}");
    assert!(reply["result"].get("decision").is_none());
    assert!(t0.elapsed() >= Duration::from_secs(1));
    assert_eq!(
        tmuxedo_runner::hook::decision_output(&reply["result"]),
        "",
        "the hook prints nothing"
    );
    let exp = c.text(S, |v| v["t"] == "event" && v["type"] == "prompt_expired").await;
    assert_eq!(exp["data"]["promptId"], prompt_id.as_str());
    let st = c
        .text(S, |v| v["t"] == "status" && v["agent"].get("promptId").is_none())
        .await;
    assert_eq!(
        st["agent"]["status"], "blocked",
        "the dialog is still open at the terminal"
    );
    c.send(json!({"t": "decision", "paneId": "w1:p1", "promptId": prompt_id, "behavior": "allow"}))
        .await;
    let stale = c.text(S, |v| v["t"] == "event" && v["type"] == "decision_stale").await;
    assert_eq!(stale["data"]["promptId"], prompt_id.as_str());
    c.close(4003).await;
    let _ = tokio::time::timeout(S, task).await;
}

/// `events_rejected {seqs, lastEventSeq}` (docs/protocol.md, audit P2-4):
/// the rejected events are renumbered above both counters and resent, and
/// an `events_ack` for the old numbers does not drop them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rejected_events_are_renumbered_and_resent() {
    let dir = common::tempdir();
    let holder = start_holder(dir.path());
    let gw = FakeGateway::start().await;
    let o = options(dir.path(), &gw.url, holder);
    let sock = o.runner_socket.clone();
    let task = tokio::spawn(daemon::run(o));
    let mut c = gw.accept(S).await.unwrap();
    c.hello(S).await;
    let h = H::attach_to(&sock).await;
    let mut got = vec![];
    for _ in 0..3 {
        report(&h, "working").await;
        report(&h, "blocked").await;
        got.push(c.text(S, |v| v["t"] == "event").await["seq"].as_u64().unwrap());
    }
    assert_eq!(got, [1, 2, 3]);
    c.send(json!({"t": "events_rejected", "seqs": [2], "lastEventSeq": 10}))
        .await;
    c.send(json!({"t": "events_ack", "upTo": 3})).await;
    let again = c.text(S, |v| v["t"] == "event").await;
    assert_eq!(
        (again["seq"].as_u64(), again["type"].as_str()),
        (Some(11), Some("blocked"))
    );
    let until = Instant::now() + S;
    while outbox_lines(dir.path()) != 1 {
        assert!(Instant::now() < until, "only the renumbered event stays queued");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    c.send(json!({"t": "events_ack", "upTo": 11})).await;
    while outbox_lines(dir.path()) != 0 {
        assert!(Instant::now() < until, "acked after renumbering");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    report(&h, "working").await;
    report(&h, "blocked").await;
    assert_eq!(c.text(S, |v| v["t"] == "event").await["seq"], 12);
    c.close(4003).await;
    let _ = tokio::time::timeout(S, task).await;
}
