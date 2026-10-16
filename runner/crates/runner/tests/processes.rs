//! Real processes: `tmuxedo-runner holder` and `tmuxedo-runner run` as separate
//! binaries, killed with SIGKILL.
//!
//! 1. A holder client is SIGKILLed mid-stream; reattaching from the last
//!    offset it wrote has no gap and no duplicate (port of the Phase 0 spike).
//! 2. The runner is SIGKILLed while a pane streams numbered lines; a new
//!    runner process reattaches to the holder, and a gateway attach from
//!    the last offset seen continues with no lost or duplicated bytes.

mod common;

use common::{check_counter, contiguous, FakeGateway, TOKEN};
use serde_json::json;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tmuxedo_holder::wire::{self, Event, Msg, Request, SpawnSpec, HOLDER_PROTO};

const BIN: &str = env!("CARGO_BIN_EXE_tmuxedo-runner");
const S: Duration = Duration::from_secs(10);

struct Killer(Child);
impl Drop for Killer {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start_holder(sock: &Path, log: &Path) -> Killer {
    let child = Command::new(BIN)
        .args(["holder", "--socket"])
        .arg(sock)
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(log).unwrap())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + S;
    while UnixStream::connect(sock).is_err() {
        assert!(Instant::now() < deadline, "holder did not start");
        std::thread::sleep(Duration::from_millis(50));
    }
    Killer(child)
}

/// A holder connection whose messages arrive on a channel (a reader thread
/// does blocking reads, so a frame is never split by a read timeout).
fn sync_client(sock: &Path) -> (UnixStream, std::sync::mpsc::Receiver<Msg>) {
    let mut s = UnixStream::connect(sock).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let mut r = s.try_clone().unwrap();
    std::thread::spawn(move || {
        while let Ok(m) = wire::read_msg(&mut r) {
            if tx.send(m).is_err() {
                return;
            }
        }
    });
    wire::write_all(
        &mut s,
        &wire::encode_json(&Request::Hello {
            proto: HOLDER_PROTO,
            client: "test".into(),
        }),
    )
    .unwrap();
    (s, rx)
}

#[test]
fn holder_survives_a_sigkilled_client_and_replays_exactly() {
    let dir = common::tempdir();
    let sock = dir.path().join("h.sock");
    let _holder = start_holder(&sock, &dir.path().join("holder.log"));

    let (mut s, rx) = sync_client(&sock);
    let spec = SpawnSpec {
        id: "counter".into(),
        argv: vec![
            "sh".into(),
            "-c".into(),
            "i=0; while :; do echo line$i; i=$((i+1)); sleep 0.005; done".into(),
        ],
        cwd: None,
        env: Default::default(),
        cols: 80,
        rows: 24,
    };
    wire::write_all(&mut s, &wire::encode_json(&Request::Spawn { rid: 1, spec })).unwrap();

    // Wait for the spawn to be confirmed before anybody attaches.
    let deadline = Instant::now() + S;
    loop {
        assert!(Instant::now() < deadline, "spawn not confirmed");
        if let Ok(Msg::Json(j)) = rx.recv_timeout(Duration::from_millis(100)) {
            match serde_json::from_slice::<Event>(&j) {
                Ok(Event::Ok { rid: 1, .. }) => break,
                Ok(Event::Error { message, .. }) => panic!("spawn failed: {message}"),
                _ => {}
            }
        }
    }

    // Client process A streams to a file; SIGKILL it mid-stream.
    let out = dir.path().join("a.out");
    let mut a = Command::new(BIN)
        .args(["holder-attach", "--id", "counter", "--from", "0", "--socket"])
        .arg(&sock)
        .arg("--out")
        .arg(&out)
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(800));
    // SAFETY: plain kill(2) on our child.
    unsafe { libc::kill(a.id() as i32, libc::SIGKILL) };
    let _ = a.wait();
    let got_a = std::fs::read(&out).unwrap();
    let start_a: u64 = std::fs::read_to_string(out.with_extension("start"))
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(start_a, 0);
    let end_a = start_a + got_a.len() as u64;
    assert!(got_a.len() > 20, "client A got {} bytes", got_a.len());

    // Nobody attached for a while: the child keeps running, never blocked.
    std::thread::sleep(Duration::from_secs(1));

    // Client B resumes from exactly where A's file ends.
    wire::write_all(
        &mut s,
        &wire::encode_json(&Request::Attach {
            rid: 2,
            id: "counter".into(),
            from: end_a,
        }),
    )
    .unwrap();
    let mut got_b = vec![];
    let mut first_offset = None;
    let deadline = Instant::now() + Duration::from_millis(600);
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(Msg::Json(j)) => {
                if let Ok(Event::Attached { from, gap, .. }) = serde_json::from_slice::<Event>(&j) {
                    assert_eq!((from, gap), (end_a, false));
                }
            }
            Ok(Msg::Data { offset, bytes, .. }) => {
                if first_offset.is_none() {
                    first_offset = Some(offset);
                }
                got_b.extend(bytes);
            }
            Err(_) => {}
        }
    }
    assert_eq!(first_offset, Some(end_a));
    let mut all = got_a;
    all.extend(got_b);
    let n = check_counter(&all, "line");
    assert!(n > 30, "only {n} lines checked");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn runner_sigkill_and_restart_loses_no_bytes() {
    let dir = common::tempdir();
    let sock = dir.path().join("h.sock");
    let _holder = start_holder(&sock, &dir.path().join("holder.log"));
    let gw = FakeGateway::start().await;
    let start_runner = |n: u32| {
        Killer(
            Command::new(BIN)
                .args(["run", "--no-start-holder", "--server", &gw.url, "--shell", "sh", "-c"])
                .arg("i=0; while :; do echo n$i; i=$((i+1)); sleep 0.01; done")
                .env("TMUXEDO_RUNNER_TOKEN", TOKEN)
                .env("TMUXEDO_STATE_DIR", dir.path().join("state"))
                .env("TMUXEDO_HOLDER_SOCKET", &sock)
                .env("TMUXEDO_RUNNER_SOCKET", dir.path().join("runner.sock"))
                .env("TMUXEDO_CONFIG", dir.path().join("none.toml"))
                .stdout(Stdio::null())
                .stderr(std::fs::File::create(dir.path().join(format!("runner{n}.log"))).unwrap())
                .spawn()
                .unwrap(),
        )
    };

    let mut r1 = start_runner(1);
    let mut c = gw.accept(S).await.expect("runner 1 connects");
    c.hello(S).await;
    c.text_t(S, "topology").await;
    c.send(json!({"t":"attach","req":1,"paneId":"w1:p1"})).await;
    let snap = c.bin(S, |f| f.req == 1).await;
    c.drain(Duration::from_millis(1000)).await;
    let live1: Vec<_> = c.bins.drain(..).filter(|f| f.req == 0).collect();
    let (s1, l1, b1) = contiguous(&live1);
    assert_eq!(s1, snap.offset, "live output starts where the snapshot left off");

    // SIGKILL the runner; the pane keeps producing in the holder.
    // SAFETY: plain kill(2) on our child.
    unsafe { libc::kill(r1.0.id() as i32, libc::SIGKILL) };
    let _ = r1.0.wait();
    drop(c);
    std::thread::sleep(Duration::from_secs(1));

    let _r2 = start_runner(2);
    let mut c = gw.accept(S).await.expect("runner 2 connects");
    c.hello(S).await;
    let topo = c.text_t(S, "topology").await;
    assert!(topo["panes"][0]["exited"].is_null(), "pane survived the runner: {topo}");
    c.send(json!({"t":"attach","req":2,"paneId":"w1:p1","from":l1})).await;
    // Wait for the replay to start (a loaded CI box is slow to restart),
    // then collect the rest of it and some live output.
    let first = c.bin(S, |f| f.req == 2).await;
    c.drain(Duration::from_millis(800)).await;
    let mut replay = vec![first];
    replay.extend(c.bins.iter().filter(|f| f.req == 2).cloned());
    assert!(!replay.is_empty(), "no replay");
    assert!(replay.iter().all(|f| f.kind == tmuxedo_protocol::DataKind::Output));
    let (s2, e2, b2) = contiguous(&replay);
    assert_eq!(s2, l1, "replay starts at the last offset seen");
    let live2: Vec<_> = c.bins.iter().filter(|f| f.req == 0).cloned().collect();
    // Live output after the replay continues exactly at its end.
    let (s3, _e3, b3) = contiguous(&live2);
    assert_eq!(s3, e2, "live resumes where the replay ended");

    let mut all = b1;
    all.extend(b2);
    all.extend(b3);
    let n = check_counter(&all, "n");
    assert!(n > 30, "only {n} lines checked across the restart");
}
