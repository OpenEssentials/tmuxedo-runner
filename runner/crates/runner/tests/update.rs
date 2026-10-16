//! Self-update as on a real machine (no service manager): a running
//! `run` with `auto_update = true` finds a newer build in
//! `<server>/dl/manifest.json`, swaps its own binary atomically, and
//! re-execs in place (same pid) into the new file; the holder and the
//! pane's shell are untouched. Regression for the live proof, where the
//! re-exec resolved `/proc/self/exe` after the swap ("… (deleted)") and
//! failed with ENOENT, leaving no runner.
//!
//! The binary starts under its aimux-era name (`aimux-runner`, with the
//! deprecated AIMUX_* env), as on a machine updated across the rename: it
//! renames itself to `tmuxedo-runner` first (`aimux-runner` becomes a
//! symlink), and the update then replaces `tmuxedo-runner`.

mod common;

use common::{H, TOKEN};
use serde_json::json;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
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

fn serve_dl(files: Vec<(String, Vec<u8>)>) -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    std::thread::spawn(move || {
        for conn in l.incoming() {
            let Ok(mut s) = conn else { continue };
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut line = String::new();
            let _ = r.read_line(&mut line);
            loop {
                let mut h = String::new();
                if r.read_line(&mut h).is_err() || h == "\r\n" || h.is_empty() {
                    break;
                }
            }
            let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
            let _ = match files.iter().find(|(p, _)| *p == path) {
                Some((_, b)) => write!(
                    s,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    b.len()
                )
                .and_then(|_| s.write_all(b)),
                None => write!(
                    s,
                    "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                ),
            };
        }
    });
    url
}

fn log_count(p: &Path, needle: &str) -> usize {
    std::fs::read_to_string(p).unwrap_or_default().matches(needle).count()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn runner_swaps_its_binary_and_reexecs_while_the_shell_survives() {
    let dir = common::tempdir();
    let d = dir.path();
    let bin_dir = d.join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let exe: PathBuf = bin_dir.join("aimux-runner");
    std::fs::copy(BIN, &exe).unwrap();
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();

    // The "new release": the same program plus trailing bytes (still a valid
    // ELF/Mach-O, different sha256).
    let mut newer = std::fs::read(BIN).unwrap();
    newer.extend_from_slice(b"\ntmuxedo-update-test\n");
    let key = tmuxedo_runner::util::platform_key();
    let manifest = format!(
        r#"{{"version": "main-r999999-test", "files": {{"{key}": {{"url": "tmuxedo-runner-{key}", "sha256": "{}"}}}}}}"#,
        tmuxedo_runner::util::sha256_hex(&newer)
    );
    let server = serve_dl(vec![
        ("/dl/manifest.json".into(), manifest.into_bytes()),
        (format!("/dl/tmuxedo-runner-{key}"), newer.clone()),
    ]);
    let cfg = d.join("runner.toml");
    std::fs::write(
        &cfg,
        format!("server = '{server}'\ntoken = '{TOKEN}'\nauto_update = true\n"),
    )
    .unwrap();

    let hsock = d.join("h.sock");
    let _holder = Killer(
        Command::new(BIN)
            .args(["holder", "--socket"])
            .arg(&hsock)
            .stderr(std::fs::File::create(d.join("holder.log")).unwrap())
            .spawn()
            .unwrap(),
    );
    let until = Instant::now() + S;
    while std::os::unix::net::UnixStream::connect(&hsock).is_err() {
        assert!(Instant::now() < until, "holder did not start");
        std::thread::sleep(Duration::from_millis(50));
    }
    let log = d.join("runner.log");
    let rsock = d.join("runner.sock");
    let runner = Killer(
        Command::new(&exe)
            .args(["run", "--no-start-holder", "--shell", "sh"])
            .env("AIMUX_RUNNER_TOKEN", TOKEN)
            .env("AIMUX_STATE_DIR", d.join("state"))
            .env("AIMUX_HOLDER_SOCKET", &hsock)
            .env("AIMUX_RUNNER_SOCKET", &rsock)
            .env("AIMUX_CONFIG", &cfg)
            .env("AIMUX_UPDATE_DEV", "1")
            .env("AIMUX_UPDATE_AFTER_SECS", "2")
            .env("HOME", d)
            .stdout(Stdio::null())
            .stderr(std::fs::File::options().create(true).append(true).open(&log).unwrap())
            .spawn()
            .unwrap(),
    );
    let pid = runner.0.id();
    let h = H::attach_to(&rsock).await;
    h.wait_alive("w1:p1").await;
    let shell = h.ok("pane.process_info", json!({"pane_id": "w1:p1"})).await["process_info"]["shell_pid"].clone();
    drop(h);

    // The swap, then the same pid starting over and reattaching to the holder.
    let until = Instant::now() + S;
    // Each process start logs "… starting; gateway …" once: two = re-exec happened.
    while log_count(&log, "starting; gateway") < 2 || log_count(&log, "restarting the runner") < 1 {
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        assert!(!text.contains("re-exec failed"), "{text}");
        assert!(Instant::now() < until, "no update + re-exec:\n{text}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let renamed = bin_dir.join("tmuxedo-runner");
    assert_eq!(std::fs::read(&renamed).unwrap(), newer, "the new build is in place");
    assert_eq!(
        std::fs::read_link(&exe).unwrap(),
        Path::new("tmuxedo-runner"),
        "the old name is a symlink"
    );
    assert_eq!(std::fs::read(&exe).unwrap(), newer, "and runs the new build");
    let proc_exe = std::fs::read_link(format!("/proc/{pid}/exe"));
    if let Ok(p) = proc_exe {
        assert_eq!(p, renamed, "pid {pid} now runs the new file (not a deleted inode)");
    }
    let h = H::attach_to(&rsock).await;
    let again = h.ok("pane.process_info", json!({"pane_id": "w1:p1"})).await["process_info"]["shell_pid"].clone();
    assert_eq!(again, shell, "the pane's shell survived the runner update");
    // The new bytes match the manifest: no second update, no loop.
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert_eq!(
        log_count(&log, "restarting the runner"),
        1,
        "{}",
        std::fs::read_to_string(&log).unwrap()
    );
}
