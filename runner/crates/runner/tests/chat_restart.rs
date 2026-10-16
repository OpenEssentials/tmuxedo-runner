//! Agent chat across a runner restart, with real processes (a holder and a
//! runner binary, the scripted agent from tests/fixtures/fake_claude.py):
//! the runner is SIGKILLed mid-turn and again while a decision is held; the
//! agent keeps running in the holder, the new runner picks its output up
//! where the old one stopped (no event lost, none twice, seqs contiguous),
//! the held decision is closed as expired, and the same agent process
//! answers the next message.

mod common;

use common::{FakeGateway, H, TOKEN};
use serde_json::{json, Value};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_tmuxedo-runner");
const S: Duration = Duration::from_secs(20);
const KEY: &str = "sk-ant-api03-RESTARTkey0123456789abcdefghijklmnop";

struct Killer(Child);
impl Drop for Killer {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn sigkill(k: &mut Killer) {
    // SAFETY: plain kill(2) on our own child.
    unsafe { libc::kill(k.0.id() as i32, libc::SIGKILL) };
    let _ = k.0.wait();
}

fn events(c: &common::Conn, sid: &str) -> Vec<Value> {
    let mut by_seq = std::collections::BTreeMap::new();
    for f in c.chats.iter().filter(|f| f.session_id == sid && !f.ephemeral) {
        let v: Value = serde_json::from_slice(&f.payload).unwrap();
        if let Some(old) = by_seq.insert(f.seq, v.clone()) {
            assert_eq!(old, v, "seq {} carried two different events", f.seq);
        }
    }
    by_seq.into_values().collect()
}

async fn wait_for(c: &mut common::Conn, sid: &str, pred: impl Fn(&Value) -> bool) -> Value {
    let s = sid.to_string();
    c.chats_until(S, |fs| {
        fs.iter().any(|f| {
            f.session_id == s && !f.ephemeral && serde_json::from_slice::<Value>(&f.payload).is_ok_and(|v| pred(&v))
        })
    })
    .await;
    events(c, sid).into_iter().find(|v| pred(v)).unwrap()
}

async fn welcome(c: &mut common::Conn, config: &Value) {
    c.send(json!({"t": "welcome", "machineId": "m1", "userId": "u1"})).await;
    c.send(config.clone()).await;
}

async fn ready(h: &H) {
    let until = Instant::now() + S;
    loop {
        let v = h.raw("runner.config", json!({})).await;
        if v["result"]["shell"]["env"]["FAKE_LOG"].is_string() {
            return;
        }
        assert!(Instant::now() < until, "config never applied: {v}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_chat_survives_runner_restarts_mid_turn_and_mid_decision() {
    let dir = common::tempdir();
    let d = dir.path();
    let bin = d.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(d.join("log")).unwrap();
    std::fs::create_dir_all(d.join("work")).unwrap();
    std::fs::copy(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/fake_claude.py"),
        bin.join("claude"),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(bin.join("claude"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    let cfg = d.join("runner.toml");
    std::fs::write(
        &cfg,
        format!("server = 'http://127.0.0.1:9'\ntoken = '{TOKEN}'\nauto_update = false\n"),
    )
    .unwrap();
    let hsock = d.join("h.sock");
    let rsock = d.join("runner.sock");
    let holder = |log: &Path| {
        let child = Command::new(BIN)
            .args(["holder", "--socket"])
            .arg(&hsock)
            .env("PATH", &path)
            .env("HOME", d)
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(log).unwrap())
            .spawn()
            .unwrap();
        let until = Instant::now() + S;
        while std::os::unix::net::UnixStream::connect(&hsock).is_err() {
            assert!(Instant::now() < until, "holder did not start");
            std::thread::sleep(Duration::from_millis(50));
        }
        Killer(child)
    };
    let gw = FakeGateway::start().await;
    let runner = |n: u32| {
        Killer(
            Command::new(BIN)
                .args(["run", "--no-start-holder", "--server", &gw.url, "--shell", "sh"])
                .env("TMUXEDO_RUNNER_TOKEN", TOKEN)
                .env("TMUXEDO_STATE_DIR", d.join("state"))
                .env("TMUXEDO_HOLDER_SOCKET", &hsock)
                .env("TMUXEDO_RUNNER_SOCKET", &rsock)
                .env("TMUXEDO_CONFIG", &cfg)
                .env("HOME", d)
                .env("PATH", &path)
                .stdout(Stdio::null())
                .stderr(std::fs::File::create(d.join(format!("runner{n}.log"))).unwrap())
                .spawn()
                .unwrap(),
        )
    };
    let config = json!({ "t": "config", "shell": { "aliases": {}, "rc": "", "env": {
        "ANTHROPIC_API_KEY": KEY, "FAKE_LOG": d.join("log").display().to_string(),
    } } });

    let _h = holder(&d.join("holder.log"));
    let mut r1 = runner(1);
    let (mut c, _) = gw.accept_hello(S).await.expect("runner 1 connects");
    welcome(&mut c, &config).await;
    let h = H::attach_to(&rsock).await;
    ready(&h).await;
    let r = h
        .raw("chat.start", json!({ "cwd": d.join("work").display().to_string() }))
        .await;
    let sid = r["result"]["session"]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("{r}"))
        .to_string();
    c.send(json!({ "t": "chat_attach", "req": 1, "sessionId": sid, "from": 1 }))
        .await;
    c.text(S, |v| v["t"] == "chat_attached").await;

    // 1. Mid-turn: the runner dies while the agent streams.
    let sent = h
        .raw("chat.send", json!({ "session_id": sid, "text": "slow 30" }))
        .await;
    assert_eq!(sent["result"]["queued"], false, "{sent}");
    c.chats_until(S, |fs| fs.iter().filter(|f| f.ephemeral).count() >= 4)
        .await;
    sigkill(&mut r1);
    let seen_before = events(&c, &sid);
    let mut r2 = runner(2);
    let (mut c, _) = gw.accept_hello(S).await.expect("runner 2 connects");
    welcome(&mut c, &config).await;
    let h = H::attach_to(&rsock).await;
    ready(&h).await;
    // The viewer catches up from where it was.
    let from = seen_before.len() as u64 + 1;
    c.send(json!({ "t": "chat_attach", "req": 2, "sessionId": sid, "from": from }))
        .await;
    let done = wait_for(&mut c, &sid, |v| v["type"] == "turn.done").await;
    assert_eq!(
        done["status"], "completed",
        "the turn finished in the same agent process: {done}"
    );
    let mut all = seen_before.clone();
    all.extend(events(&c, &sid));
    let texts: Vec<&Value> = all.iter().filter(|v| v["type"] == "message.done").collect();
    assert_eq!(texts.len(), 1, "exactly one final message: {all:#?}");
    let want: Vec<String> = (0..30).map(|i| format!("tick{i}")).collect();
    assert_eq!(texts[0]["text"], want.join(" "));
    for (i, e) in all.iter().enumerate() {
        assert_eq!(
            e["seq"].as_u64().unwrap(),
            i as u64 + 1,
            "contiguous, none lost, none twice: {all:#?}"
        );
    }
    assert_eq!(all.iter().filter(|v| v["type"] == "turn.started").count(), 1);
    // The log on disk agrees.
    let log = std::fs::read_to_string(d.join(format!("state/chat/{sid}/events.ndjson"))).unwrap();
    assert_eq!(log.lines().count(), all.len());

    // 2. Mid-decision: a held permission; the runner dies; the hook gets no
    // answer (the agent's own rules decide: refused); the new runner closes
    // the item as expired.
    h.raw("chat.send", json!({ "session_id": sid, "text": "tool" })).await;
    let item = wait_for(&mut c, &sid, |v| v["type"] == "needs.opened").await;
    sigkill(&mut r2);
    let _r3 = runner(3);
    let (mut c, _) = gw.accept_hello(S).await.expect("runner 3 connects");
    welcome(&mut c, &config).await;
    let h = H::attach_to(&rsock).await;
    ready(&h).await;
    c.send(json!({ "t": "chat_attach", "req": 3, "sessionId": sid, "from": 1 }))
        .await;
    let resolved = wait_for(&mut c, &sid, |v| {
        v["type"] == "needs.resolved" && v["id"] == item["item"]["id"]
    })
    .await;
    assert_eq!(resolved["outcome"], "expired");
    assert_eq!(resolved["by"], "runner restart");
    wait_for(&mut c, &sid, |v| {
        v["type"] == "message.done" && v["text"] == "Not running it: no answer"
    })
    .await;
    let stale = h
        .raw(
            "chat.respond",
            json!({ "session_id": sid, "need_id": item["item"]["id"], "response": { "kind": "allow_once" } }),
        )
        .await;
    assert_eq!(stale["result"]["outcome"], "stale");
    // 3. The same agent process answers the next message (it was never restarted).
    let until = Instant::now() + S;
    loop {
        let g = h.raw("chat.get", json!({ "session_id": sid })).await;
        if g["result"]["session"]["status"] == "idle" {
            break;
        }
        assert!(Instant::now() < until, "{g}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    h.raw("chat.send", json!({ "session_id": sid, "text": "hello" })).await;
    wait_for(&mut c, &sid, |v| {
        v["type"] == "message.done" && v["text"] == "Hello there"
    })
    .await;
    let starts = std::fs::read_to_string(d.join("log/argv.log")).unwrap();
    assert_eq!(
        starts.lines().count(),
        1,
        "one agent process for the whole chat: {starts}"
    );
}
