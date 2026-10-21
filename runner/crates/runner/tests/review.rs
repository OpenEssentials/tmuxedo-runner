//! Review and ship's `git.status` / `git.diff` through the gateway, in
//! process (docs/design/away-and-review.md §4.2, §5): a real holder, the
//! runner daemon and a fake gateway, on a temporary repository. The folder
//! comes from the runner's record of the session; a request never names one.

mod common;

use common::{options, start_holder, FakeGateway};
use serde_json::json;
use std::path::Path;
use std::process::Command;
use std::time::Duration;
use tmuxedo_runner::daemon;

const S: Duration = Duration::from_secs(10);

fn sh(dir: &Path, args: &[&str]) {
    let mut c = Command::new("git");
    for (k, _) in std::env::vars_os() {
        if k.to_string_lossy().starts_with("GIT_") {
            c.env_remove(&k);
        }
    }
    let ok = c
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.invalid",
            "-c",
            "commit.gpgSign=false",
        ])
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .status()
        .unwrap()
        .success();
    assert!(ok, "{args:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn review_reads_a_sessions_repository_through_the_gateway() {
    let dir = common::tempdir();
    let proj = dir.path().join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::write(proj.join("a.txt"), "one\n").unwrap();
    sh(&proj, &["init", "-q", "-b", "agent/work"]);
    sh(&proj, &["add", "-A"]);
    sh(&proj, &["commit", "-q", "-m", "init"]);
    std::fs::write(proj.join("a.txt"), "one\ntwo\n").unwrap();

    let holder = start_holder(dir.path());
    let gw = FakeGateway::start().await;
    let task = tokio::spawn(daemon::run(options(dir.path(), &gw.url, holder.clone())));
    let mut c = gw.accept(S).await.expect("runner dials out");
    let hello = c.hello(S).await;
    assert!(hello.to_string().contains("\"review\""), "{hello}");
    c.text_t(S, "topology").await;
    c.send(json!({"t":"welcome","machineId":"m1","userId":"u1"})).await;
    c.send(json!({"t":"req","id":"w","method":"workspace.create","params":{"cwd": proj.display().to_string(), "label": "proj"}}))
        .await;
    let w = c.res("w").await;
    let pane = w["result"]["root_pane"]["pane_id"].as_str().unwrap().to_string();

    c.send(json!({"t":"req","id":"s","method":"git.status","params":{"pane": pane}}))
        .await;
    let s = c.res("s").await;
    assert_eq!(s["result"]["branch"], "agent/work", "{s}");
    assert_eq!(s["result"]["files"][0]["path"], "a.txt");
    assert_eq!(s["result"]["files"][0]["add"], 1);
    assert!(
        !s.to_string().contains(&*dir.path().to_string_lossy()),
        "no machine path: {s}"
    );

    c.send(json!({"t":"req","id":"d","method":"git.diff","params":{"pane": pane, "path": "a.txt"}}))
        .await;
    let d = c.res("d").await;
    assert!(d["result"]["patch"].as_str().unwrap().contains("+two"), "{d}");

    // A path that escapes, a session that does not exist, and discard (not built).
    c.send(json!({"t":"req","id":"e","method":"git.diff","params":{"pane": pane, "path": "../x"}}))
        .await;
    assert_eq!(c.res("e").await["error"]["code"], "invalid_path");
    c.send(json!({"t":"req","id":"f","method":"git.status","params":{"pane": "w9:p9"}}))
        .await;
    assert_eq!(c.res("f").await["error"]["code"], "not_found");
    c.send(json!({"t":"req","id":"g","method":"git.discard","params":{"pane": pane}}))
        .await;
    assert_eq!(c.res("g").await["error"]["code"], "not_supported");
    c.send(json!({"t":"req","id":"h","method":"git.status","params":{"pane": pane, "chat": "c1"}}))
        .await;
    assert_eq!(c.res("h").await["error"]["code"], "invalid_params");
    task.abort();
}
