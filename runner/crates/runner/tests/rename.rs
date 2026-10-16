//! The aimux → tmuxedo rename on a real machine layout, with real processes:
//!
//! 1. An aimux-era holder keeps its sessions: a new runner attaches to it at
//!    the old socket, a new `holder --wait` stands by without binding, and
//!    takes over only once the old holder is gone; the runner then moves to
//!    it (the service manager restarts the runner).
//! 2. The first start of a self-updated binary (still named `aimux-runner`)
//!    renames itself to `tmuxedo-runner` (old names become symlinks) and moves
//!    `~/.local/state/aimux` and `~/.config/aimux` to their tmuxedo names, old
//!    paths left as symlinks; runner.toml is read from the new place; a
//!    second start changes nothing.

mod common;

use common::{FakeGateway, TOKEN};
use serde_json::json;
use std::os::unix::net::UnixStream;
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

fn base(home: &Path, run: &Path) -> Command {
    let mut c = Command::new(BIN);
    c.env("HOME", home)
        .env("XDG_RUNTIME_DIR", run)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_STATE_HOME")
        .env_remove("TMUXEDO_HOLDER_SOCKET")
        .env_remove("AIMUX_HOLDER_SOCKET")
        .env_remove("TMUXEDO_RUNTIME_DIR")
        .env_remove("AIMUX_RUNTIME_DIR")
        .stdout(Stdio::null());
    c
}

fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + S;
    while !f() {
        assert!(Instant::now() < deadline, "timed out: {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_aimux_era_holder_keeps_its_sessions_until_it_exits() {
    let dir = common::tempdir();
    let home = dir.path().join("home");
    let run = dir.path().join("run");
    std::fs::create_dir_all(&home).unwrap();
    let legacy_sock = run.join("aimux/holder.sock");
    let new_sock = run.join("tmuxedo/holder.sock");
    let log = |n: &str| std::fs::File::create(dir.path().join(n)).unwrap();

    // The holder an aimux-era service started (its socket under …/aimux).
    let mut old_holder = Killer(
        base(&home, &run)
            .args(["holder", "--socket"])
            .arg(&legacy_sock)
            .stderr(log("old-holder.log"))
            .spawn()
            .unwrap(),
    );
    wait_for("old holder up", || UnixStream::connect(&legacy_sock).is_ok());

    // tmuxedo-holder.service (`holder --wait`) stands by.
    let mut new_holder = Killer(
        base(&home, &run)
            .args(["holder", "--wait"])
            .stderr(log("new-holder.log"))
            .spawn()
            .unwrap(),
    );
    std::thread::sleep(Duration::from_millis(1500));
    assert!(
        !new_sock.exists(),
        "the new holder must not bind while the old one serves"
    );
    assert!(new_holder.0.try_wait().unwrap().is_none(), "it waits, it does not exit");
    let nl = std::fs::read_to_string(dir.path().join("new-holder.log")).unwrap();
    assert!(nl.contains("waiting for it to exit"), "{nl}");

    // tmuxedo-runner.service attaches to the old holder and its pane.
    let gw = FakeGateway::start().await;
    let start_runner = |n: u32| {
        Killer(
            base(&home, &run)
                .args(["run", "--no-start-holder", "--server", &gw.url, "--shell", "sh"])
                .env("TMUXEDO_RUNNER_TOKEN", TOKEN)
                .env("TMUXEDO_STATE_DIR", dir.path().join("state"))
                .env("TMUXEDO_RUNNER_SOCKET", dir.path().join("runner.sock"))
                .env("TMUXEDO_CONFIG", dir.path().join("none.toml"))
                .stderr(log(&format!("runner{n}.log")))
                .spawn()
                .unwrap(),
        )
    };
    let mut r1 = start_runner(1);
    let mut c = gw.accept(S).await.expect("runner connects");
    c.hello(S).await;
    let topo = c.text_t(S, "topology").await;
    assert!(topo["panes"][0]["exited"].is_null(), "{topo}");
    let rl = std::fs::read_to_string(dir.path().join("runner1.log")).unwrap();
    assert!(rl.contains("using the aimux-era holder"), "{rl}");
    assert!(!new_sock.exists());

    // The old holder goes (logout, reboot, or `systemctl --user stop
    // aimux-holder.service`): the new one takes over, the runner exits to
    // be restarted by its service, and comes back on the new holder.
    // SAFETY: plain kill(2) on our child.
    unsafe { libc::kill(old_holder.0.id() as i32, libc::SIGTERM) };
    let _ = old_holder.0.wait();
    wait_for("new holder bound", || UnixStream::connect(&new_sock).is_ok());
    assert!(new_holder.0.try_wait().unwrap().is_none());
    wait_for("runner exits after losing its holder", || {
        r1.0.try_wait().unwrap().is_some()
    });
    drop(c);
    let _r2 = start_runner(2);
    let mut c = gw.accept(S).await.expect("runner 2 connects");
    c.hello(S).await;
    c.text_t(S, "topology").await;
    c.send(json!({"t":"attach","req":1,"paneId":"w1:p1"})).await;
    c.bin(S, |f| f.req == 1).await;
    let rl = std::fs::read_to_string(dir.path().join("runner2.log")).unwrap();
    assert!(!rl.contains("aimux-era holder"), "{rl}");
}

#[test]
fn first_start_after_self_update_renames_binary_and_directories() {
    let dir = common::tempdir();
    let home = dir.path().join("home");
    let bin = home.join(".local/bin");
    let state = home.join(".local/state");
    let config = home.join(".config");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(state.join("aimux")).unwrap();
    std::fs::create_dir_all(config.join("aimux")).unwrap();
    std::fs::write(state.join("aimux/topology.json"), "{\"kept\":true}").unwrap();
    std::fs::write(
        config.join("aimux/runner.toml"),
        "server = \"http://127.0.0.1:9\"\ntoken = \"amr.lookuplookuplookuplookup.secretsecretsecretsecretsecretsecret\"\n",
    )
    .unwrap();
    // What a self-update from an aimux-era runner leaves: the new bytes under the old name.
    // Copied by a child process: a write fd opened here could be inherited by
    // a child the other test forks on a parallel thread, and executing the
    // file while any copy of that fd lives fails with ETXTBSY ("Text file
    // busy"). The `cp` child has exited and been waited for before we exec.
    let cp = Command::new("cp")
        .arg(BIN)
        .arg(bin.join("aimux-runner"))
        .status()
        .unwrap();
    assert!(cp.success(), "cp failed: {cp}");

    let start = || {
        Command::new(bin.join("aimux-runner"))
            .args(["update", "--check"])
            .env("HOME", &home)
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_STATE_HOME")
            .env_remove("XDG_RUNTIME_DIR")
            .env_remove("TMUXEDO_STATE_DIR")
            .env_remove("AIMUX_STATE_DIR")
            .env_remove("TMUXEDO_CONFIG")
            .env_remove("AIMUX_CONFIG")
            .output()
            .unwrap()
    };
    let out = start();
    let err = String::from_utf8_lossy(&out.stderr);
    // It read runner.toml (now under ~/.config/tmuxedo) and tried that server.
    assert!(err.contains("http://127.0.0.1:9/dl/manifest.json"), "{err}");
    assert!(err.contains("migrated"), "{err}");

    assert!(bin.join("tmuxedo-runner").is_file() && !bin.join("tmuxedo-runner").is_symlink());
    for alias in ["aimux-runner", "tmuxedo", "aimux"] {
        assert_eq!(
            std::fs::read_link(bin.join(alias)).unwrap(),
            Path::new("tmuxedo-runner"),
            "{alias}"
        );
    }
    assert_eq!(
        std::fs::read_to_string(state.join("tmuxedo/topology.json")).unwrap(),
        "{\"kept\":true}"
    );
    assert!(config.join("tmuxedo/runner.toml").is_file());
    assert!(config.join("tmuxedo/.migrated-from-aimux").is_file());
    assert!(state.join("tmuxedo/.migrated-from-aimux").is_file());
    for old in [state.join("aimux"), config.join("aimux")] {
        assert!(old.is_symlink(), "{} is a symlink now", old.display());
    }
    assert!(
        std::fs::read_to_string(config.join("aimux/runner.toml"))
            .unwrap()
            .contains("amr."),
        "old path resolves"
    );

    // Second start: nothing left to do.
    let out = start();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!err.contains("migrated") && !err.contains("renamed"), "{err}");
    assert!(err.contains("http://127.0.0.1:9/dl/manifest.json"), "{err}");
}
