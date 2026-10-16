//! Agent status from the screen alone, end to end: the stand-in agent
//! (`runner/tests/standin`, no hooks) draws every stand-in screen of every
//! supported agent in a real pane; the runner identifies the agent from the
//! process and reads each screen on its detection tick. Checked through the
//! socket API (`pane.get`, `pane.agent_status_changed` events).
//!
//! - idle / working / blocked screens become the pane's status;
//! - hold screens keep the previous status;
//! - unknown screens hand over to the output heuristic (working, then idle),
//!   and a bell after that work is `done`;
//! - a synchronized-output frame left open is not read until it closes.

mod common;

use common::{subscribe, H};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn screens_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../detect/tests/screens")
}

/// A directory with the stand-in linked under every agent's launch name.
fn standins(dir: &Path) -> PathBuf {
    let bin = dir.join("standin-bin");
    std::fs::create_dir_all(&bin).unwrap();
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/standin");
    for agent in tmuxedo_detect::Agent::all() {
        std::os::unix::fs::symlink(&script, bin.join(agent.launch())).unwrap();
    }
    bin
}

fn files(agent: &str, status: &str) -> usize {
    std::fs::read_dir(screens_root().join(agent))
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(&format!("{status}-")))
        .count()
}

async fn status(h: &H, pane: &str) -> (Option<String>, String) {
    let p = h.ok("pane.get", json!({"pane_id": pane})).await;
    (
        p["pane"]["agent"].as_str().map(str::to_owned),
        p["pane"]["agent_status"].as_str().unwrap().to_string(),
    )
}

async fn wait_status(h: &H, pane: &str, agent: &str, want: &str, what: &str) {
    let until = Instant::now() + Duration::from_secs(20);
    loop {
        let (a, s) = status(h, pane).await;
        if a.as_deref() == Some(agent) && s == want {
            return;
        }
        assert!(Instant::now() < until, "{agent} {what}: want {want}, have {a:?} {s}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn key(h: &H, pane: &str, k: &str) {
    h.ok("pane.send_text", json!({"pane_id": pane, "text": k})).await;
}

async fn launch(h: &H, pane: &str, bin: &Path, agent: tmuxedo_detect::Agent) {
    let cmd = format!(
        "STANDIN_SCREENS='{}' '{}'\n",
        screens_root().display(),
        bin.join(agent.launch()).display()
    );
    h.ok("pane.send_text", json!({"pane_id": pane, "text": cmd})).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_agent_screen_sets_the_pane_status() {
    let h = H::start().await;
    let bin = standins(h.dir.path());
    let mut checked = 0;
    for agent in tmuxedo_detect::Agent::all() {
        let id = agent.id();
        let pane = "w1:p1";
        launch(&h, pane, &bin, agent).await;
        // The first idle screen (every agent has one).
        wait_status(&h, pane, id, "idle", "starts").await;
        let mut last = "idle".to_string();
        for (status, k) in [("idle", "i"), ("working", "w"), ("blocked", "b"), ("hold", "h")] {
            // `i` already drew the first idle screen at start: cycle through all of them anyway.
            for n in 0..files(id, status) {
                key(&h, pane, k).await;
                let want = if status == "hold" {
                    last.clone()
                } else {
                    status.to_string()
                };
                if status == "hold" {
                    // Hold: the status does not move for a few ticks.
                    tokio::time::sleep(Duration::from_millis(800)).await;
                }
                wait_status(&h, pane, id, &want, &format!("{status} screen {n}")).await;
                last = want;
                checked += 1;
            }
        }
        if files(id, "unknown") > 0 {
            for n in 0..files(id, "unknown") {
                key(&h, pane, "u").await;
                // No screen evidence: the heuristic says idle once output stops.
                wait_status(&h, pane, id, "idle", &format!("unknown screen {n}")).await;
                checked += 1;
            }
            // Work, then a bell: done.
            key(&h, pane, "u").await;
            key(&h, pane, "d").await;
            wait_status(&h, pane, id, "done", "bell after work").await;
        }
        key(&h, pane, "q").await;
        // Back at the shell: no agent.
        let until = Instant::now() + Duration::from_secs(20);
        while status(&h, pane).await.0.is_some() {
            assert!(Instant::now() < until, "{id}: the agent never left");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    assert!(checked > 150, "only {checked} screens");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_open_synchronized_frame_is_not_read() {
    let h = H::start().await;
    let bin = standins(h.dir.path());
    let codex = tmuxedo_detect::Agent::lookup("codex").unwrap();
    launch(&h, "w1:p1", &bin, codex).await;
    wait_status(&h, "w1:p1", "codex", "idle", "starts").await;
    key(&h, "w1:p1", "b").await;
    wait_status(&h, "w1:p1", "codex", "blocked", "approval").await;
    let mut events = subscribe(
        &h.sock,
        json!([{"type": "pane.agent_status_changed", "pane_id": "w1:p1"}]),
    )
    .await;
    // Redraw the same approval inside a frame that stays half drawn for 1.5 s.
    key(&h, "w1:p1", "s").await;
    let until = Instant::now() + Duration::from_millis(2500);
    while Instant::now() < until {
        assert_eq!(status(&h, "w1:p1").await.1, "blocked", "a half-drawn frame was read");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // No status change was announced either.
    let next = tokio::time::timeout(Duration::from_millis(300), events.next_line()).await;
    assert!(next.is_err(), "unexpected event {next:?}");
    // And the screen after the frame is read normally.
    key(&h, "w1:p1", "w").await;
    wait_status(&h, "w1:p1", "codex", "working", "after the frame").await;
}
