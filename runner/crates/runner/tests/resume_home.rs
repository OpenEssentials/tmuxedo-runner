//! Queue item 3g: no repeated "trust this folder?" on resume.
//!
//! Claude Code never remembers trust for `$HOME` (2.1.289 keeps that answer
//! for the session only), and hosted panes used to start, and so resume, in
//! `$HOME`. Now:
//!
//! 1. new panes start in the default working folder (`~/work`, created);
//!    `config.workDir` moves it per machine;
//! 2. after a pod loss a Claude session recorded in `$HOME` (also through a
//!    symlink to it) resumes in `~/work`; a session in a real project folder
//!    resumes exactly where it was; an agent whose plugin does not set
//!    `resume.leave_home` stays in `$HOME`. A fake `claude` records the cwd
//!    and argv it was resumed with.

mod common;

use common::{options, start_holder, FakeGateway, H, TOKEN};
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tmuxedo_runner::daemon;

const BIN: &str = env!("CARGO_BIN_EXE_tmuxedo-runner");
const S: Duration = Duration::from_secs(20);

fn s(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// The cwd of the root pane of a `workspace.create` / `tab.create` result.
async fn root(h: &H, r: Value) -> String {
    let p = r["root_pane"]["pane_id"].as_str().unwrap().to_string();
    h.ok("pane.get", json!({"pane_id": p})).await["pane"]["cwd"]
        .as_str()
        .unwrap_or("")
        .to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn new_panes_start_in_the_work_dir_and_config_moves_it() {
    let dir = common::tempdir();
    let home = dir.path().join("home");
    let proj = dir.path().join("proj");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&proj).unwrap();
    let holder = start_holder(dir.path());
    let gw = FakeGateway::start().await;
    let mut o = options(dir.path(), &gw.url, holder);
    o.home = s(&home);
    o.default_work_dir = "~/work".into();
    let sock = o.runner_socket.clone();
    let state = o.state_dir.clone();
    let task = tokio::spawn(daemon::run(o));
    let mut c = gw.accept(S).await.unwrap();
    c.hello(S).await;
    let h = H::attach_to(&sock).await;
    h.wait_alive("w1:p1").await;
    let work = home.join("work");
    assert!(work.is_dir(), "~/work is created");
    let cwd = |v: &Value| v["pane"]["cwd"].as_str().unwrap_or("").to_string();
    assert_eq!(
        cwd(&h.ok("pane.get", json!({"pane_id": "w1:p1"})).await),
        s(&work),
        "first pane"
    );
    // The shell really runs there.
    h.ok(
        "pane.send_text",
        json!({"pane_id": "w1:p1", "text": "echo \"AT=$(pwd)\"\n"}),
    )
    .await;
    h.wait_text("w1:p1", &format!("AT={}", s(&work))).await;

    // workspace.create without a cwd: ~/work; with one: exactly that.
    let r = h.ok("workspace.create", json!({})).await;
    assert_eq!(root(&h, r).await, s(&work));
    let r = h.ok("workspace.create", json!({"cwd": s(&proj)})).await;
    let wp = r["workspace"]["workspace_id"].as_str().unwrap().to_string();
    assert_eq!(root(&h, r).await, s(&proj));
    // A tab in a workspace anchored at $HOME starts in ~/work; in a project
    // workspace it inherits the project.
    let r = h.ok("workspace.create", json!({"cwd": s(&home)})).await;
    let wh = r["workspace"]["workspace_id"].as_str().unwrap().to_string();
    assert_eq!(root(&h, r).await, s(&home), "an explicit $HOME is respected");
    let t = h.ok("tab.create", json!({"workspace_id": wh})).await;
    assert_eq!(root(&h, t).await, s(&work));
    let t = h.ok("tab.create", json!({"workspace_id": wp})).await;
    assert_eq!(root(&h, t).await, s(&proj));

    // config.workDir moves the default (created), persists, shows in the view.
    c.send(json!({"t": "config", "workDir": "~/code/main"})).await;
    let code = home.join("code/main");
    let mut ok = false;
    for _ in 0..50 {
        let v = h.raw("runner.config", json!({})).await;
        if v["result"]["workDir"] == "~/code/main" {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(ok, "runner.config shows workDir");
    let saved: Value = serde_json::from_slice(&std::fs::read(state.join("config.json")).unwrap()).unwrap();
    assert_eq!(saved["work_dir"], "~/code/main");
    let r = h.ok("workspace.create", json!({})).await;
    assert_eq!(root(&h, r).await, s(&code));
    assert!(code.is_dir());
    // Refused (relative): the previous one stays. A frame without workDir
    // (web's claude/shell only) changes nothing. "" goes back to ~/work.
    c.send(json!({"t": "config", "workDir": "relative/dir"})).await;
    c.send(json!({"t": "config", "claude": {"args": []}})).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    let r = h.ok("workspace.create", json!({})).await;
    assert_eq!(root(&h, r).await, s(&code));
    c.send(json!({"t": "config", "workDir": ""})).await;
    let mut ok = false;
    for _ in 0..50 {
        if h.raw("runner.config", json!({})).await["result"]
            .get("workDir")
            .is_none()
        {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(ok, "workDir cleared");
    let r = h.ok("workspace.create", json!({})).await;
    assert_eq!(root(&h, r).await, s(&work));
    c.close(4003).await;
    let _ = tokio::time::timeout(S, task).await;
}

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

/// `<cwd>|<argv>` lines the fake claude wrote, by session id.
fn resumed_calls(log: &Path) -> Vec<(String, String)> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split_once('|').map(|(a, b)| (a.to_string(), b.to_string())))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn home_sessions_resume_in_work_and_project_sessions_stay() {
    let dir = common::tempdir();
    let d = dir.path();
    let home = d.join("home");
    let real = d.join("real-bin");
    let proj = d.join("proj");
    for p in [&home, &real, &proj] {
        std::fs::create_dir_all(p).unwrap();
    }
    std::fs::create_dir_all(proj.join(".git")).unwrap();
    // As on the hosted pods, where /home/aimux is a symlink to $HOME.
    let alias = d.join("alias-home");
    std::os::unix::fs::symlink(&home, &alias).unwrap();
    let calls = d.join("claude-calls.log");
    std::fs::write(
        real.join("claude"),
        format!(
            "#!/bin/sh\nprintf '%s|%s\\n' \"$(pwd)\" \"$(printf '%s' \"$*\" | tr '\\n' ' ')\" >> '{}'\necho \"RESUMED-IN: $(pwd)\"\nexec sleep 600\n",
            calls.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(real.join("claude"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let cfg = d.join("runner.toml");
    std::fs::write(
        &cfg,
        format!("server = 'http://127.0.0.1:9'\ntoken = '{TOKEN}'\nauto_update = false\n"),
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
    let work = home.join("work");

    let mut h1 = holder(&hsock, &d.join("holder1.log"));
    let _r1 = runner(1);
    let mut c = gw.accept(S).await.expect("runner connects");
    c.hello(S).await;
    let h = H::attach_to(&rsock).await;
    // w1:p1 is the default pane (~/work); sessions in $HOME, the symlinked
    // home, a project, and a non-Claude agent in $HOME.
    let mut panes = vec![];
    for cwd in [&home, &alias, &proj, &home] {
        let r = h.ok("workspace.create", json!({"cwd": s(cwd)})).await;
        panes.push(r["root_pane"]["pane_id"].as_str().unwrap().to_string());
    }
    for p in &panes {
        h.wait_alive(p).await;
        // A pane that never printed anything restarts fresh (no resume):
        // give each some output before the pod "dies".
        h.ok("pane.send_text", json!({"pane_id": p, "text": "echo READY-$((6*7))\n"}))
            .await;
        h.wait_text(p, "READY-42").await;
    }
    let sessions = [
        ("claude", "S-home"),
        ("claude", "S-alias"),
        ("claude", "S-proj"),
        ("other", "S-other"),
    ];
    for (p, (agent, sid)) in panes.iter().zip(sessions) {
        h.ok(
            "pane.report_agent_session",
            json!({"pane_id": p, "source": "test", "agent": agent, "agent_session_id": sid}),
        )
        .await;
    }
    h.ok("pane.rename", json!({"pane_id": "w1:p1", "label": "persist"}))
        .await;
    drop(h);

    // Pod loss: runner and holder die with every process; a new pair resumes.
    drop(_r1);
    sigkill(&mut h1);
    drop(c);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let _h2 = holder(&hsock, &d.join("holder2.log"));
    let _r2 = runner(2);
    let mut c = gw.accept(S).await.expect("runner 2 connects");
    c.hello(S).await;
    let mut resumed = std::collections::BTreeMap::new();
    while resumed.len() < 4 {
        let ev = c.text(S, |v| v["t"] == "event" && v["type"] == "resumed").await;
        resumed.insert(
            ev["data"]["sessionId"].as_str().unwrap().to_string(),
            ev["data"].clone(),
        );
    }
    let (sw, sh, sp) = (s(&work), s(&home), s(&proj));
    assert_eq!(resumed["S-home"]["cwd"], sw, "{resumed:?}");
    assert_eq!(resumed["S-home"]["movedFrom"], sh);
    assert_eq!(resumed["S-alias"]["cwd"], sw, "a symlink to $HOME is $HOME");
    assert_eq!(resumed["S-proj"]["cwd"], sp, "a real project folder is unchanged");
    assert!(resumed["S-proj"].get("movedFrom").is_none());
    assert_eq!(
        resumed["S-other"]["cwd"], sh,
        "only plugins with resume.leave_home move"
    );

    let h = H::attach_to(&rsock).await;
    for p in &panes[..3] {
        h.wait_text(p, "RESUMED-IN:").await;
    }
    // (The runner's own `claude --version` probe is not a resume.)
    let mut got: Vec<_> = resumed_calls(&calls)
        .into_iter()
        .filter(|(_, a)| a.contains("--resume"))
        .collect();
    got.sort();
    let shim = d.join("state/shim/claude");
    assert_eq!(got.len(), 3, "{got:?}");
    let want = |cwd: &str, sid: &str| {
        got.iter().any(|(c, a)| {
            // (After the files fragment, Phase 15: `--add-dir …`; its note is folded into one
            // leading `--append-system-prompt` holding new lines, so the fake logs them as spaces.)
            c == cwd && a.contains("--settings ") && a.ends_with(&format!("claude-settings.json --resume {sid}"))
        })
    };
    assert!(want(&sw, "S-home"), "{got:?}");
    assert!(want(&sw, "S-alias"), "{got:?}");
    assert!(want(&sp, "S-proj"), "{got:?}");
    assert!(shim.exists());
    // The pane now lives in ~/work: the next pod loss resumes there again.
    let cwd = h.ok("pane.get", json!({"pane_id": panes[0]})).await["pane"]["cwd"].clone();
    assert_eq!(cwd, sw);
    let cwd = h.ok("pane.get", json!({"pane_id": panes[2]})).await["pane"]["cwd"].clone();
    assert_eq!(cwd, sp);
}
