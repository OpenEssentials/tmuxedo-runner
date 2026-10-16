//! Restore with real processes.
//!
//! 1. Runner restart (SIGKILL): the topology comes back from disk and every
//!    pane is the same process (the holder kept them).
//! 2. Holder restart (SIGKILL, as on pod loss): panes respawn in their saved
//!    cwd; an agent with a reported session id resumes with
//!    `claude --resume <id>` (through the hook shim) when the cwd guard
//!    allows, a `resumed` event is sent; a pane whose cwd is gone gets a shell
//!    and a `resume_refused` event; opt-in pane history is replayed.

mod common;

use common::{FakeGateway, H, TOKEN};
use serde_json::json;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn runner_and_holder_restarts_restore_topology_resume_and_history() {
    let dir = common::tempdir();
    let d = dir.path();
    let home = d.join("home");
    let real = d.join("real-bin");
    let a = d.join("project-a");
    let b = d.join("project-b");
    for p in [&home, &real, &a, &b] {
        std::fs::create_dir_all(p).unwrap();
    }
    std::fs::create_dir_all(a.join(".git")).unwrap();
    std::fs::write(
        real.join("claude"),
        "#!/bin/sh\necho \"RESUMED-ARGS: $*\"\nexec sleep 600\n",
    )
    .unwrap();
    std::fs::set_permissions(real.join("claude"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let cfg = d.join("runner.toml");
    std::fs::write(
        &cfg,
        format!("server = 'http://127.0.0.1:9'\ntoken = '{TOKEN}'\npane_history = true\nauto_update = false\n"),
    )
    .unwrap();
    let hsock = d.join("h.sock");
    let rsock = d.join("runner.sock");
    let gw = FakeGateway::start().await;
    let path = format!("{}:{}", real.display(), std::env::var("PATH").unwrap_or_default());
    let runner = |n: u32| {
        Killer(
            Command::new(BIN)
                .args(["run", "--no-start-holder", "--server", &gw.url, "--shell", "sh", "-i"])
                .env("AIMUX_RUNNER_TOKEN", TOKEN)
                .env("AIMUX_STATE_DIR", d.join("state"))
                .env("AIMUX_HOLDER_SOCKET", &hsock)
                .env("AIMUX_RUNNER_SOCKET", &rsock)
                .env("AIMUX_CONFIG", &cfg)
                .env("HOME", &home)
                .env("PATH", &path)
                .stdout(Stdio::null())
                .stderr(std::fs::File::create(d.join(format!("runner{n}.log"))).unwrap())
                .spawn()
                .unwrap(),
        )
    };

    let mut h1 = holder(&hsock, &d.join("holder1.log"));
    let mut r1 = runner(1);
    let mut c = gw.accept(S).await.expect("runner 1 connects");
    c.hello(S).await;
    let h = H::attach_to(&rsock).await;
    let w2 = h.ok("workspace.create", json!({"cwd": a.to_string_lossy()})).await;
    let w3 = h.ok("workspace.create", json!({"cwd": b.to_string_lossy()})).await;
    assert_eq!(
        (w2["root_pane"]["pane_id"].as_str(), w3["root_pane"]["pane_id"].as_str()),
        (Some("w2:p1"), Some("w3:p1"))
    );
    h.wait_alive("w2:p1").await;
    h.wait_alive("w3:p1").await;
    h.ok("pane.split", json!({"target_pane_id": "w2:p1", "direction": "down"}))
        .await;
    for (p, s) in [("w2:p1", "S1"), ("w3:p1", "S2")] {
        h.ok(
            "pane.report_agent_session",
            json!({"pane_id": p, "source": "test", "agent": "claude", "agent_session_id": s}),
        )
        .await;
    }
    h.ok(
        "pane.send_text",
        json!({"pane_id": "w1:p1", "text": "echo HISTORY-$((40+2))\n"}),
    )
    .await;
    h.wait_text("w1:p1", "HISTORY-42").await;
    let pid_before = h.ok("pane.process_info", json!({"pane_id": "w2:p1"})).await["process_info"]["shell_pid"].clone();
    let rev_before = c.text(S, |v| v["t"] == "topology").await["rev"].as_u64().unwrap();
    h.ok("pane.rename", json!({"pane_id": "w1:p1", "label": "keep"})).await; // persists state + history

    // ── 1. runner SIGKILL + restart: same topology, same processes ──
    sigkill(&mut r1);
    drop(c);
    let _r1b = runner(2);
    let mut c = gw.accept(S).await.expect("runner 1b connects");
    c.hello(S).await;
    let topo = c.text_t(S, "topology").await;
    assert!(
        topo["rev"].as_u64().unwrap() > rev_before,
        "rev keeps increasing across restarts"
    );
    let mut ids: Vec<&str> = topo["panes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["id"].as_str().unwrap())
        .collect();
    ids.sort();
    assert_eq!(ids, vec!["w1:p1", "w2:p1", "w2:p2", "w3:p1"]);
    assert_eq!(topo["tabs"][1]["layout"]["dir"], "v");
    let h = H::attach_to(&rsock).await;
    let pid_after = h.ok("pane.process_info", json!({"pane_id": "w2:p1"})).await["process_info"]["shell_pid"].clone();
    assert_eq!(pid_before, pid_after, "the holder kept the process");
    assert_eq!(
        h.ok("pane.get", json!({"pane_id": "w1:p1"})).await["pane"]["label"],
        "keep"
    );

    // ── 2. holder SIGKILL (pod loss): respawn, resume, refuse, history ──
    std::fs::remove_dir_all(&b).unwrap();
    sigkill(&mut h1);
    drop(c);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let _h2 = holder(&hsock, &d.join("holder2.log"));
    let _r2 = runner(3);
    let mut c = gw.accept(S).await.expect("runner 2 connects");
    c.hello(S).await;
    let topo = c.text_t(S, "topology").await;
    assert_eq!(topo["panes"].as_array().unwrap().len(), 4, "layout restored: {topo}");
    let resumed = c.text(S, |v| v["t"] == "event" && v["type"] == "resumed").await;
    assert_eq!(resumed["paneId"], "w2:p1");
    assert_eq!(resumed["data"]["sessionId"], "S1");
    let refused = c.text(S, |v| v["t"] == "event" && v["type"] == "resume_refused").await;
    assert_eq!(refused["paneId"], "w3:p1");
    assert!(refused["data"]["reason"].as_str().unwrap().contains("no longer exists"));
    assert!(refused["seq"].as_u64().unwrap() != resumed["seq"].as_u64().unwrap());
    let h = H::attach_to(&rsock).await;
    let text = h.wait_text("w2:p1", "RESUMED-ARGS").await;
    assert!(text.contains("--resume S1"), "{text}");
    assert!(text.contains("--settings"), "resumed through the hook shim: {text}");
    h.wait_alive("w3:p1").await;
    let cwd = h.ok("pane.get", json!({"pane_id": "w3:p1"})).await["pane"]["cwd"].clone();
    // 3g: a pane whose folder is gone starts in the default working folder
    // (~/work, created), not $HOME, like any new pane.
    assert_eq!(
        cwd,
        home.join("work").to_string_lossy().as_ref(),
        "missing cwd falls back to the default working folder"
    );
    // Pane history: the old shell's output is replayed into the new incarnation.
    h.wait_text("w1:p1", "HISTORY-42").await;
    // And the respawned panes are new processes.
    let pid_new = h.ok("pane.process_info", json!({"pane_id": "w2:p2"})).await["process_info"]["shell_pid"].clone();
    assert!(pid_new.as_u64().unwrap() > 0);
}
