//! The shell left in a pane after a resumed agent exits keeps the pane PATH.
//!
//! A resumed agent runs as `<shell> -lc '<agent> --resume <id>; <a shell>'`.
//! That last shell used to be `exec <shell> -l`: a login shell, which reads
//! the profile again. A profile that resets PATH (Debian's /etc/profile; on
//! macOS path_helper moves the system directories to the front) left the
//! shim directory out, so an agent started again from that shell ran without
//! the shim: no hooks, no status. The shell is now started through the same
//! rc wrappers a new pane's shell uses, and those end by putting the shim
//! directory first.
//!
//! Here the user's own startup files play the PATH-resetting profile (the
//! test cannot edit /etc/profile), and `.bashrc` also replaces
//! `PROMPT_COMMAND`, as many prompt setups do: the runner's own bash prompt
//! hook, which re-adds the shim, is gone too. A fake `claude` exits at once
//! when resumed; then `command -v claude` in the shell it leaves must be the
//! shim.

mod common;

use common::{FakeGateway, H, TOKEN};
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_tmuxedo-runner");
const S: Duration = Duration::from_secs(20);

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

/// `name` on the test process's PATH, if it is installed.
fn on_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

/// Resume a fake claude session in a pane whose shell is `shell`, let the
/// agent exit, and return what the shell left behind prints for
/// `command -v claude`: `RES=shim` or `RES=other:<path>`.
async fn shell_after_resume(shell: &Path) -> String {
    let dir = common::tempdir();
    let d = dir.path();
    let home = d.join("home");
    let real = d.join("real-bin");
    let proj = d.join("proj");
    for p in [&home, &real, &proj] {
        std::fs::create_dir_all(p).unwrap();
    }
    std::fs::write(
        real.join("claude"),
        "#!/bin/sh\n\
         if [ \"$1\" = \"--version\" ]; then echo \"2.1.289 (Claude Code)\"; exit 0; fi\n\
         case \" $* \" in *\" --resume \"*) echo \"FAKE-RESUMED-$((6*7))\" ;; esac\n\
         exit 0\n",
    )
    .unwrap();
    std::fs::set_permissions(real.join("claude"), std::fs::Permissions::from_mode(0o755)).unwrap();
    // The startup files reset PATH: the real claude is found, the shim is not.
    let reset = format!("PATH='{}:/usr/local/bin:/usr/bin:/bin'\nexport PATH\n", real.display());
    let profile = format!("{reset}[ -r \"$HOME/.bashrc\" ] && . \"$HOME/.bashrc\"\n");
    std::fs::write(home.join(".bash_profile"), profile).unwrap();
    std::fs::write(home.join(".bashrc"), format!("{reset}PROMPT_COMMAND='true'\n")).unwrap();
    std::fs::write(home.join(".zprofile"), &reset).unwrap();
    std::fs::write(home.join(".zshrc"), &reset).unwrap();
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
                .args(["run", "--no-start-holder", "--server", &gw.url, "--shell"])
                .arg(shell)
                .arg("-l")
                .env("AIMUX_RUNNER_TOKEN", TOKEN)
                .env("AIMUX_STATE_DIR", d.join("state"))
                .env("AIMUX_HOLDER_SOCKET", &hsock)
                .env("AIMUX_RUNNER_SOCKET", &rsock)
                .env("AIMUX_CONFIG", &cfg)
                .env("HOME", &home)
                .env("PATH", &path)
                .env_remove("ZDOTDIR")
                .env_remove("TMUXEDO_REAL_ZDOTDIR")
                .env_remove("TMUXEDO_SHELL_DIR")
                .env_remove("PROMPT_COMMAND")
                .stdout(Stdio::null())
                .stderr(std::fs::File::create(d.join(format!("runner{n}.log"))).unwrap())
                .spawn()
                .unwrap(),
        )
    };

    let mut h1 = holder(&hsock, &d.join("holder1.log"));
    let r1 = runner(1);
    let mut c = gw.accept(S).await.expect("runner connects");
    c.hello(S).await;
    let h = H::attach_to(&rsock).await;
    let cwd = json!({"cwd": proj.to_string_lossy()});
    let r = h.ok("workspace.create", cwd).await;
    let pane = r["root_pane"]["pane_id"].as_str().unwrap().to_string();
    h.wait_alive(&pane).await;
    // A pane that never printed anything restarts fresh (no resume).
    let ready = json!({"pane_id": pane, "text": "echo READY-$((6*7))\n"});
    h.ok("pane.send_text", ready).await;
    h.wait_text(&pane, "READY-42").await;
    let session = json!({"pane_id": pane, "source": "test", "agent": "claude", "agent_session_id": "S-path"});
    h.ok("pane.report_agent_session", session).await;
    let label = json!({"pane_id": pane, "label": "persist"});
    h.ok("pane.rename", label).await;
    drop(h);

    // The machine restarts: runner and holder die with every process; the new
    // pair resumes the session.
    drop(r1);
    sigkill(&mut h1);
    drop(c);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let _h2 = holder(&hsock, &d.join("holder2.log"));
    let _r2 = runner(2);
    let mut c = gw.accept(S).await.expect("runner 2 connects");
    c.hello(S).await;
    let resumed = |v: &Value| v["t"] == "event" && v["type"] == "resumed" && v["data"]["sessionId"] == "S-path";
    c.text(S, resumed).await;
    let h = H::attach_to(&rsock).await;
    // The fake agent ran through the shim and has exited; its shell is next.
    h.wait_text(&pane, "FAKE-RESUMED-42").await;
    let check = concat!(
        "case \"$(command -v claude)\" in \"$TMUXEDO_SHIM_DIR/claude\") echo \"RES=sh\"\"im\" ;; ",
        "*) echo \"RES=ot\"\"her:$(command -v claude)\" ;; esac; echo DONE-$((6*7))\n",
    );
    let ask = json!({"pane_id": pane, "text": check});
    h.ok("pane.send_text", ask).await;
    // "DONE-42" and the joined "RES=shim" are only in the shell's output, not
    // in the echoed command line.
    h.wait_text(&pane, "DONE-42").await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bash_left_after_a_resumed_agent_keeps_the_shim_first() {
    let bash = on_path("bash").expect("bash is installed wherever these tests run");
    let text = shell_after_resume(&bash).await;
    assert!(text.contains("RES=shim"), "claude is not the shim: {text}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn zsh_left_after_a_resumed_agent_keeps_the_shim_first() {
    let Some(zsh) = on_path("zsh") else {
        eprintln!("skipped: zsh is not installed");
        return;
    };
    let text = shell_after_resume(&zsh).await;
    assert!(text.contains("RES=shim"), "claude is not the shim: {text}");
}
