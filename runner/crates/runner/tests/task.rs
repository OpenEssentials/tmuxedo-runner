//! Headless command tasks (docs/design/headless-tasks.md): the machine's
//! consent, the command and folder checks, the result, cancel and time-out.
//! Agent tasks are in `chat.rs` (they need the fake agent CLI).
mod common;
use common::H;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

async fn labelled() -> H {
    H::start_with(|o| o.automation_labels = vec!["ci".into(), "linux".into()]).await
}

fn cmd(h: &H, id: &str, script: &str) -> Value {
    let root = h.dir.path().to_string_lossy().into_owned();
    json!({ "task_id": id, "kind": "command", "labels": ["ci"], "cwd": root, "roots": [root],
        "argv": ["/bin/sh", "-c", script], "allowed_commands": ["/bin/sh"] })
}

async fn wait_end(h: &H, id: &str) -> Value {
    let until = Instant::now() + Duration::from_secs(30);
    loop {
        let t = h.ok("task.get", json!({ "task_id": id })).await["task"].clone();
        if t["status"] != "running" {
            return t;
        }
        assert!(Instant::now() < until, "task never ended: {t}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_command_task_returns_its_exit_code_and_output() {
    let h = labelled().await;
    h.ok("task.start", cmd(&h, "ok1", "echo all-good")).await;
    h.ok("task.start", cmd(&h, "bad1", "echo went-wrong; exit 7")).await;
    let ok = wait_end(&h, "ok1").await;
    assert_eq!(ok["status"], "done");
    assert_eq!(ok["exit_code"], 0);
    assert!(ok["output"].as_str().unwrap().contains("all-good"), "{ok}");
    assert!(ok["ended_at"].is_string());
    let bad = wait_end(&h, "bad1").await;
    assert_eq!(bad["status"], "failed");
    assert_eq!(bad["reason_code"], "exit_status");
    assert_eq!(bad["exit_code"], 7);
    assert!(bad["output"].as_str().unwrap().contains("went-wrong"));
    let list = h.ok("task.list", json!({})).await;
    assert_eq!(list["tasks"].as_array().unwrap().len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_task_is_a_pane_people_see_and_cancel_stops_it() {
    let h = labelled().await;
    let r = h.ok("task.start", cmd(&h, "long", "echo started; sleep 60")).await;
    let pane = r["task"]["pane_id"].as_str().unwrap().to_string();
    h.wait_text(&pane, "started").await;
    let panes = h.ok("pane.list", json!({})).await;
    assert!(panes.to_string().contains(&pane), "the task's pane is listed");
    let c = h.ok("task.cancel", json!({ "task_id": "long" })).await;
    assert_eq!(c["task"]["status"], "cancelled");
    assert!(c["task"]["output"].as_str().unwrap().contains("started"));
    // Cancelling again is harmless.
    assert_eq!(
        h.ok("task.cancel", json!({ "task_id": "long" })).await["task"]["status"],
        "cancelled"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_task_times_out() {
    let h = labelled().await;
    let mut p = cmd(&h, "slow", "sleep 60");
    p["timeout_ms"] = json!(1000);
    h.ok("task.start", p).await;
    let t = wait_end(&h, "slow").await;
    assert_eq!(t["status"], "timed_out");
    assert_eq!(t["reason_code"], "timeout");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn requests_outside_the_policy_or_the_machines_consent_are_refused() {
    let h = labelled().await;
    let root = h.dir.path().join("root");
    std::fs::create_dir_all(root.join("sub")).unwrap();
    std::fs::create_dir_all(h.dir.path().join("rootless")).unwrap();
    let root_s = root.to_string_lossy().into_owned();
    let base = json!({ "task_id": "x", "kind": "command", "labels": ["ci"], "cwd": root_s, "roots": [root_s],
        "argv": ["/bin/sh", "-c", "true"], "allowed_commands": ["/bin/sh"] });
    let refused = |patch: Value| {
        let mut p = base.clone();
        for (k, v) in patch.as_object().unwrap() {
            p[k] = v.clone();
        }
        p
    };
    // A label this machine's owner did not give it.
    let e = h.fails("task.start", refused(json!({"labels": ["macos"]}))).await;
    assert_eq!(e["code"], "automation_not_allowed");
    // A program off the list, by relative path, or through a link to another program.
    let e = h.fails("task.start", refused(json!({"argv": ["/bin/ls"]}))).await;
    assert_eq!(e["code"], "command_not_allowed");
    let e = h
        .fails("task.start", refused(json!({"argv": ["sh", "-c", "true"]})))
        .await;
    assert_eq!(e["code"], "command_not_allowed");
    let link = root.join("sh");
    std::os::unix::fs::symlink("/bin/ls", &link).unwrap();
    let e = h
        .fails("task.start", refused(json!({"argv": [link.to_string_lossy()]})))
        .await;
    assert_eq!(e["code"], "command_not_allowed");
    // A work folder outside the roots, by `..` or by a link.
    let up = format!("{root_s}/sub/../../rootless");
    assert_eq!(
        h.fails("task.start", refused(json!({"cwd": up}))).await["code"],
        "cwd_not_allowed"
    );
    let esc = root.join("esc");
    std::os::unix::fs::symlink(h.dir.path().join("rootless"), &esc).unwrap();
    let e = h
        .fails("task.start", refused(json!({"cwd": esc.to_string_lossy()})))
        .await;
    assert_eq!(e["code"], "cwd_not_allowed");
    // Shapes.
    for bad in [
        json!({"kind": "shell"}),
        json!({"task_id": "../x"}),
        json!({"timeout_ms": 5}),
        json!({"argv": []}),
    ] {
        assert_eq!(
            h.fails("task.start", refused(bad.clone())).await["code"],
            "invalid_params",
            "{bad}"
        );
    }
    assert_eq!(h.ok("task.list", json!({})).await["tasks"], json!([]), "nothing ran");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn observe_only_refuses_tasks_but_lets_them_be_read() {
    let h = H::start_with(|o| {
        o.automation_labels = vec!["ci".into()];
        o.guardrails.observe_only = true;
    })
    .await;
    let root = h.dir.path().to_string_lossy().into_owned();
    let p = json!({ "task_id": "o1", "kind": "command", "labels": ["ci"], "cwd": root, "roots": [root],
        "argv": ["/bin/sh", "-c", "true"], "allowed_commands": ["/bin/sh"] });
    assert_eq!(h.fails("task.start", p).await["code"], "observe_only");
    h.ok("task.list", json!({})).await;
}
