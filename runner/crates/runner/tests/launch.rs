//! Open-URL stand-ins and the `config` frame (docs/protocol.md), end to end
//! with a real runner, holder and shells.

mod common;

use common::{options, start_holder, FakeGateway, H};
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;
use tmuxedo_runner::daemon;

const S: Duration = Duration::from_secs(20);
const BIN: &str = env!("CARGO_BIN_EXE_tmuxedo-runner");

fn outbox_lines(d: &Path) -> usize {
    std::fs::read_to_string(d.join("state/events.jsonl"))
        .map(|s| s.lines().count())
        .unwrap_or(0)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn opener_stand_ins_send_web_links_to_the_users_device() {
    let dir = common::tempdir();
    let holder = start_holder(dir.path());
    let gw = FakeGateway::start().await;
    let o = options(dir.path(), &gw.url, holder);
    let sock = o.runner_socket.clone();
    let task = tokio::spawn(daemon::run(o));
    let mut c = gw.accept(S).await.unwrap();
    c.hello(S).await;
    let h = H::attach_to(&sock).await;
    h.wait_alive("w1:p1").await;
    let before = outbox_lines(dir.path());
    // As Claude's /login does (the `open` package runs xdg-open on Linux),
    // and through $BROWSER.
    h.ok("pane.send_text", json!({"pane_id": "w1:p1", "text":
        "xdg-open 'https://claude.ai/oauth/authorize?code=1&state=x'; echo XDG=$?; \"$BROWSER\" https://example.com/b; echo BR=$?\n"}))
        .await;
    let ev = c.text(S, |v| v["t"] == "event" && v["type"] == "open_url").await;
    assert_eq!(ev["paneId"], "w1:p1");
    assert_eq!(ev["seq"], 0, "not durable");
    assert_eq!(ev["data"]["url"], "https://claude.ai/oauth/authorize?code=1&state=x");
    assert_eq!(ev["data"]["program"], "xdg-open");
    assert_eq!(ev["data"]["source"], "browser-shim");
    let ev = c.text(S, |v| v["t"] == "event" && v["type"] == "open_url").await;
    assert_eq!(
        (ev["data"]["url"].as_str(), ev["data"]["program"].as_str()),
        (Some("https://example.com/b"), Some("x-www-browser"))
    );
    let text = h.wait_text("w1:p1", "BR=0").await;
    assert!(text.contains("XDG=0"), "the stand-in exits 0 at once: {text}");
    assert_eq!(
        outbox_lines(dir.path()),
        before,
        "open_url never enters the durable outbox"
    );
    // Not a web link: no event; the real program is tried and fails (Linux: there
    // is no `open`; macOS: /usr/bin/open reports that the file does not exist).
    h.ok(
        "pane.send_text",
        json!({"pane_id": "w1:p1", "text": "open file:///nonexistent-tmuxedo-test-file; echo OPEN=$?\n"}),
    )
    .await;
    h.wait_text("w1:p1", "OPEN=1").await;
    let r = h
        .raw(
            "pane.open_url",
            json!({"pane_id": "w1:p1", "url": "javascript:alert(1)"}),
        )
        .await;
    assert_eq!(r["error"]["code"], "invalid_url");
    c.drain(Duration::from_millis(300)).await;
    assert!(!c.texts.iter().any(|v| v["type"] == "open_url"), "{:?}", c.texts);
    c.close(4003).await;
    let _ = tokio::time::timeout(S, task).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn claude_launch_args_are_prepended_and_never_interpreted() {
    let dir = common::tempdir();
    let real = dir.path().join("real-bin");
    std::fs::create_dir_all(&real).unwrap();
    std::fs::write(
        real.join("claude"),
        "#!/bin/sh\nfor a; do printf '<%s>' \"$a\"; done; echo\n",
    )
    .unwrap();
    std::fs::set_permissions(real.join("claude"), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::env::set_var(
        "PATH",
        format!("{}:{}", real.display(), std::env::var("PATH").unwrap_or_default()),
    );
    let holder = start_holder(dir.path());
    let gw = FakeGateway::start().await;
    let o = options(dir.path(), &gw.url, holder);
    let sock = o.runner_socket.clone();
    let task = tokio::spawn(daemon::run(o));
    let mut c = gw.accept(S).await.unwrap();
    c.hello(S).await;
    let marker = dir.path().join("pwned");
    let value = format!("$(touch {}); `id`", marker.display());
    c.send(
        json!({"t": "config", "claude": {"args": ["--dangerously-skip-permissions", "--append-system-prompt", value]}}),
    )
    .await;
    let h = H::attach_to(&sock).await;
    h.wait_alive("w1:p1").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    h.ok("pane.send_text", json!({"pane_id": "w1:p1", "text": "claude hello\n"}))
        .await;
    let text = h.wait_text("w1:p1", "<hello>").await.replace('\n', "");
    let want = format!("<--dangerously-skip-permissions><--append-system-prompt><{value}><--settings>");
    assert!(text.contains(&want), "{text}");
    assert!(!marker.exists(), "an arg is never run by a shell");
    // An injection attempt (not a flag, not a flag's value) is refused; the
    // previous args stay.
    c.send(json!({"t": "config", "claude": {"args": ["--model", "opus", "; touch /tmp/x"]}}))
        .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let cfg: Value = h.raw("runner.config", json!({})).await["result"].clone();
    assert_eq!(cfg["claude"]["args"][0], "--dangerously-skip-permissions", "{cfg}");
    // `tmuxedo config` (read-only) shows the same.
    let link = dir.path().join("tmuxedo");
    std::os::unix::fs::symlink(BIN, &link).unwrap();
    let out = tokio::task::spawn_blocking(move || {
        std::process::Command::new(link)
            .arg("config")
            .env("TMUXEDO_SOCKET_PATH", sock)
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let shown: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(shown["claude"]["args"][1], "--append-system-prompt");
    c.close(4003).await;
    let _ = tokio::time::timeout(S, task).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shell_settings_reach_new_bash_panes_and_reload() {
    let dir = common::tempdir();
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join(".bashrc"), "export FROM_BASHRC=yes\n").unwrap();
    let holder = start_holder(dir.path());
    let gw = FakeGateway::start().await;
    let mut o = options(dir.path(), &gw.url, holder);
    o.home = home.to_string_lossy().into_owned();
    o.shell = Some(vec!["bash".into(), "-l".into()]);
    let sock = o.runner_socket.clone();
    let task = tokio::spawn(daemon::run(o));
    let mut c = gw.accept(S).await.unwrap();
    c.hello(S).await;
    // PATH and AIMUX_PANE_ID (a pre-rename copy the runner sets) are protected: refused.
    c.send(json!({"t": "config", "shell": {
        "aliases": {"c": "echo it's $HOME", "bad name": "x"},
        "env": {"EDITOR": "vim", "QUOTED": "a'b $X `y`", "PATH": "/evil", "AIMUX_PANE_ID": "x"},
        "rc": "export FROM_RC=1"
    }}))
    .await;
    let h = H::attach_to(&sock).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    // A NEW pane (the first one predates the settings).
    let r = h
        .ok(
            "workspace.create",
            json!({"cwd": home.to_string_lossy(), "env": {"HOME": home.to_string_lossy()}}),
        )
        .await;
    let pane = r["root_pane"]["pane_id"].as_str().unwrap().to_string();
    h.wait_alive(&pane).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    h.ok("pane.send_text", json!({"pane_id": pane, "text":
        "type c; echo \"E=$EDITOR Q=$QUOTED R=$FROM_RC B=$FROM_BASHRC\"; case \":$PATH:\" in *:/evil:*) echo PATH=evil;; *) echo PATH=ok;; esac; echo ID=$AIMUX_PANE_ID; echo DONE-$((6*7))\n"}))
        .await;
    // Wait for a marker only the shell's OUTPUT contains: the echoed command line
    // already holds "ID=", "PATH=ok" and "DONE-", so waiting on any of those raced
    // the output on a loaded host (main run 37404825273). "DONE-42" is printed
    // last, after every line asserted below.
    let text = h.wait_text(&pane, "DONE-42").await;
    assert!(text.contains("c is aliased to `echo it's $HOME'"), "{text}");
    assert!(text.contains("E=vim Q=a'b $X `y` R=1 B=yes"), "{text}");
    assert!(text.contains("\nPATH=ok"), "{text}");
    assert!(text.contains(&format!("ID={pane}")), "protected var kept: {text}");
    let init = std::fs::read_to_string(home.join(".config/tmuxedo/shell-init.sh")).unwrap();
    assert!(!init.contains("bad name") && !init.contains("/evil"), "{init}");
    // A changed setting reaches the running pane with reload-shell.
    c.send(json!({"t": "config", "shell": {"aliases": {"c": "echo it's $HOME", "d": "echo reloaded"}}}))
        .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let link = dir.path().join("tmuxedo");
    std::os::unix::fs::symlink(BIN, &link).unwrap();
    let (sock2, pane2) = (sock.clone(), pane.clone());
    let out = tokio::task::spawn_blocking(move || {
        std::process::Command::new(link)
            .args(["reload-shell", &pane2])
            .env("TMUXEDO_SOCKET_PATH", sock2)
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    tokio::time::sleep(Duration::from_millis(500)).await;
    h.ok("pane.send_text", json!({"pane_id": pane, "text": "d\n"})).await;
    h.wait_text(&pane, "\nreloaded").await;
    c.close(4003).await;
    let _ = tokio::time::timeout(S, task).await;
}
