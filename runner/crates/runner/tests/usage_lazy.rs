//! The restart-time lazy resume (docs/design/agent-usage.md, "Resume and
//! limits"): after a holder loss, an agent session whose plan's session window
//! is at 80 % or more is not resumed at once. Each one resumes when its pane is
//! first opened (or by "Resume all now" on the machine-level item); the reason
//! is sent as a `resume_waiting` event. Below 80 % resume is unchanged
//! (tests/restore.rs).

mod common;

use common::{options, start_holder, FakeGateway, H};
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;
use tmuxedo_runner::daemon;

const S: Duration = Duration::from_secs(20);

fn lazy_items(v: &Value) -> Vec<Value> {
    v["items"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|i| i["id"].as_str().is_some_and(|s| s.starts_with("usage-lazy-")))
        .collect()
}

async fn welcome(c: &mut common::Conn) {
    c.text_t(S, "hello").await;
    c.send(json!({"t": "welcome", "machineId": "m1", "userId": "u1", "lastEventSeq": 0, "features": ["ui", "needs"]}))
        .await;
    c.text_t(S, "topology").await;
    c.send(json!({"t": "config", "plugins": [{"id": "claude", "settings": {"usage": {"enabled": true}}}]}))
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn near_the_limit_agents_resume_on_first_open() {
    let dir = common::tempdir();
    let d = dir.path();
    let real = d.join("real-bin");
    let a = d.join("project-a");
    let b = d.join("project-b");
    for p in [&real, &a, &b, &d.join("h1"), &d.join("h2")] {
        std::fs::create_dir_all(p).unwrap();
    }
    for p in [&a, &b] {
        std::fs::create_dir_all(p.join(".git")).unwrap();
    }
    std::fs::write(
        real.join("claude"),
        "#!/bin/sh\necho \"RESUMED-ARGS: $*\"\nexec sleep 600\n",
    )
    .unwrap();
    std::fs::set_permissions(real.join("claude"), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::env::set_var(
        "PATH",
        format!("{}:{}", real.display(), std::env::var("PATH").unwrap_or_default()),
    );
    let gw = FakeGateway::start().await;

    let o = options(d, &gw.url, start_holder(&d.join("h1")));
    let sock = o.runner_socket.clone();
    let task = tokio::spawn(daemon::run(o));
    let mut c = gw.accept(S).await.expect("runner 1 connects");
    welcome(&mut c).await;
    let h = H::attach_to(&sock).await;
    h.ok("workspace.create", json!({"cwd": a.to_string_lossy()})).await;
    h.ok("workspace.create", json!({"cwd": b.to_string_lossy()})).await;
    h.wait_alive("w2:p1").await;
    h.wait_alive("w3:p1").await;
    for (p, s) in [("w2:p1", "S1"), ("w3:p1", "S2")] {
        h.ok(
            "pane.report_agent_session",
            json!({"pane_id": p, "source": "test", "agent": "claude", "agent_session_id": s}),
        )
        .await;
    }
    // The agent's status line: the 5-hour window at 86 %. Usage turns on
    // asynchronously, so report until the reading is kept.
    let reset = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3600;
    let report = json!({"paneId": "w2:p1", "plugin": "claude", "reading": {"source": "statusline",
        "windows": [{"id": "5h", "usedPct": 86.0, "resetsAt": reset, "windowMins": 300}]}});
    let mut seen = false;
    for _ in 0..40 {
        h.raw("usage.report", report.clone()).await;
        let l = h.ok("usage.list", json!({})).await;
        if l["records"][0]["windows"][0]["usedPct"] == 86.0 {
            seen = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(seen, "the reading never arrived");
    h.ok("pane.rename", json!({"pane_id": "w1:p1", "label": "keep"})).await; // persists state

    // ── Pod loss: the runner goes and comes back to a new, empty holder. ──
    task.abort();
    let _ = task.await;
    drop(c);
    drop(h);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut o = options(d, &gw.url, start_holder(&d.join("h2")));
    o.runner_socket = d.join("runner2.sock");
    let sock = o.runner_socket.clone();
    let _task = tokio::spawn(daemon::run(o));
    let mut c = gw.accept(S).await.expect("runner 2 connects");
    welcome(&mut c).await;
    let mut waiting = vec![];
    for _ in 0..2 {
        let ev = c.text(S, |v| v["t"] == "event" && v["type"] == "resume_waiting").await;
        waiting.push(ev);
    }
    waiting.sort_by_key(|e| e["paneId"].as_str().unwrap_or("").to_string());
    assert_eq!(waiting[0]["paneId"], "w2:p1");
    assert_eq!(waiting[0]["data"]["sessionId"], "S1");
    assert_eq!(waiting[1]["paneId"], "w3:p1");
    assert_eq!(
        waiting[0]["data"]["reason"],
        "Claude Code 5-hour limit at 86%: resumes when you open it"
    );
    let f = c.text(S, |v| v["t"] == "needs" && !lazy_items(v).is_empty()).await;
    let it = &lazy_items(&f)[0];
    assert_eq!(it["id"], "usage-lazy-claude");
    assert_eq!(it["paneId"], "");
    assert_eq!(
        it["title"],
        "2 Claude Code agents resume when you open them (5-hour limit at 86%)"
    );
    // Nothing resumed by itself.
    c.drain(Duration::from_millis(1500)).await;
    assert!(
        !c.texts.iter().any(|v| v["t"] == "event" && v["type"] == "resumed"),
        "{:?}",
        c.texts
    );
    let h = H::attach_to(&sock).await;
    let r = h
        .raw("pane.read", json!({"pane_id": "w2:p1", "source": "recent_unwrapped"}))
        .await;
    assert!(!r.to_string().contains("RESUMED-ARGS"), "{r}");

    // ── First open of one pane resumes that one only. ──
    c.send(json!({"t": "attach", "req": 7, "paneId": "w2:p1"})).await;
    let ev = c.text(S, |v| v["t"] == "event" && v["type"] == "resumed").await;
    assert_eq!(ev["paneId"], "w2:p1");
    assert_eq!(ev["data"]["sessionId"], "S1");
    assert_eq!(ev["data"]["lazy"], true);
    let text = h.wait_text("w2:p1", "RESUMED-ARGS").await;
    assert!(text.contains("--resume S1"), "{text}");
    let f = c
        .text(S, |v| {
            v["t"] == "needs" && lazy_items(v).iter().any(|i| i["detail"]["usage"]["lazy"] == 1)
        })
        .await;
    assert_eq!(
        lazy_items(&f)[0]["title"],
        "1 Claude Code agent resumes when you open it (5-hour limit at 86%)"
    );

    // ── "Resume all now" resumes the rest, and the item goes. ──
    c.send(json!({"t": "req", "id": "r1", "method": "needs.respond", "params": {"itemId": "usage-lazy-claude", "responseId": "resume_now"}}))
        .await;
    assert_eq!(c.res("r1").await["result"]["outcome"], "accepted");
    let ev = c.text(S, |v| v["t"] == "event" && v["type"] == "resumed").await;
    assert_eq!(ev["paneId"], "w3:p1");
    let text = h.wait_text("w3:p1", "RESUMED-ARGS").await;
    assert!(text.contains("--resume S2"), "{text}");
    c.text(S, |v| {
        v["t"] == "needs" && v["paneId"] == "" && lazy_items(v).is_empty()
    })
    .await;
}
