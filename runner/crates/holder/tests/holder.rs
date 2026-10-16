//! Holder behaviour against a real server on a temp socket: query answering
//! with no client attached, env scrub, process-group kill, ring-wrap gap
//! reporting, offset-exact reattach after a client vanishes, input and resize.
//! (The SIGKILLed-client test with real processes lives in the runner crate,
//! which owns the binary.)

use std::collections::BTreeMap;
use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tmuxedo_holder::server::{self, Config};
use tmuxedo_holder::wire::{self, Event, Msg, Request, SpawnSpec, HOLDER_PROTO};

fn start(ring: usize) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("h.sock");
    let bound = server::bind(&Config {
        socket: sock.clone(),
        ring_capacity: ring,
    })
    .unwrap();
    std::thread::spawn(move || server::run(bound));
    (dir, sock)
}

struct C {
    w: UnixStream,
    r: std::sync::mpsc::Receiver<Msg>,
    rid: u64,
    data: Vec<(String, u64, Vec<u8>)>,
    events: Vec<Event>,
}

impl C {
    fn new(sock: &PathBuf) -> C {
        let s = UnixStream::connect(sock).unwrap();
        // Blocking reads on a thread: a read timeout could split a frame.
        let (tx, rx) = std::sync::mpsc::channel();
        let mut rd = BufReader::new(s.try_clone().unwrap());
        std::thread::spawn(move || {
            while let Ok(m) = wire::read_msg(&mut rd) {
                if tx.send(m).is_err() {
                    return;
                }
            }
        });
        let mut c = C {
            w: s,
            r: rx,
            rid: 0,
            data: vec![],
            events: vec![],
        };
        c.send(&Request::Hello {
            proto: HOLDER_PROTO,
            client: "test".into(),
        });
        let ev = c.next_event(Duration::from_secs(5)).unwrap();
        assert!(matches!(ev, Event::Hello { proto: 1, .. }), "{ev:?}");
        c
    }
    fn send(&mut self, r: &Request) {
        wire::write_all(&mut self.w, &wire::encode_json(r)).unwrap();
    }
    fn input(&mut self, id: &str, b: &[u8]) {
        wire::write_all(&mut self.w, &wire::encode_data(id, 0, b)).unwrap();
    }
    /// Pump one message (or time out).
    fn pump(&mut self) -> Option<()> {
        match self.r.recv_timeout(Duration::from_millis(100)) {
            Ok(Msg::Json(j)) => {
                self.events.push(serde_json::from_slice(&j).unwrap());
                Some(())
            }
            Ok(Msg::Data { id, offset, bytes }) => {
                self.data.push((id, offset, bytes));
                Some(())
            }
            Err(_) => None,
        }
    }
    fn next_event(&mut self, t: Duration) -> Option<Event> {
        let deadline = Instant::now() + t;
        while Instant::now() < deadline {
            if !self.events.is_empty() {
                return Some(self.events.remove(0));
            }
            self.pump();
        }
        None
    }
    fn call(&mut self, make: impl FnOnce(u64) -> Request) -> Event {
        self.rid += 1;
        let rid = self.rid;
        self.send(&make(rid));
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Some(i) = self.events.iter().position(|e| match e {
                Event::Ok { rid: r, .. } | Event::Error { rid: r, .. } | Event::Attached { rid: r, .. } => *r == rid,
                _ => false,
            }) {
                return self.events.remove(i);
            }
            self.pump();
        }
        panic!("no response to rid {rid}");
    }
    fn spawn(&mut self, id: &str, script: &str, env: &[(&str, &str)]) -> u32 {
        let spec = SpawnSpec {
            id: id.into(),
            argv: vec!["sh".into(), "-c".into(), script.into()],
            cwd: Some("/".into()),
            env: env
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<BTreeMap<_, _>>(),
            cols: 80,
            rows: 24,
        };
        match self.call(|rid| Request::Spawn { rid, spec }) {
            Event::Ok { data, .. } => data["pid"].as_u64().unwrap() as u32,
            e => panic!("spawn failed: {e:?}"),
        }
    }
    fn bytes(&self, id: &str) -> Vec<u8> {
        self.data
            .iter()
            .filter(|d| d.0 == id)
            .flat_map(|d| d.2.clone())
            .collect()
    }
    fn wait_for(&mut self, id: &str, needle: &str, t: Duration) -> String {
        let deadline = Instant::now() + t;
        loop {
            let s = String::from_utf8_lossy(&self.bytes(id)).into_owned();
            if s.contains(needle) {
                return s;
            }
            if Instant::now() > deadline {
                panic!("timed out waiting for {needle:?}; got {s:?}");
            }
            self.pump();
        }
    }
    fn list_end(&mut self, id: &str) -> (u64, u64) {
        match self.call(|rid| Request::List { rid }) {
            Event::Ok { data, .. } => {
                let p = data["panes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|p| p["id"] == id)
                    .unwrap()
                    .clone();
                (p["start"].as_u64().unwrap(), p["end"].as_u64().unwrap())
            }
            e => panic!("{e:?}"),
        }
    }
}

fn attach(c: &mut C, id: &str, from: u64) -> (u64, bool) {
    match c.call(|rid| Request::Attach {
        rid,
        id: id.into(),
        from,
    }) {
        Event::Attached { from, gap, .. } => (from, gap),
        e => panic!("{e:?}"),
    }
}

#[test]
fn answers_da1_with_nobody_attached() {
    let (_d, sock) = start(1 << 20);
    let mut c = C::new(&sock);
    // The program asks DA1 and reads exactly the 9-byte reply in raw mode.
    c.spawn(
        "q",
        r#"stty raw -echo; printf '\033[c'; r=$(dd bs=1 count=9 2>/dev/null); stty sane; printf 'REPLY:%s\n' "$(printf '%s' "$r" | od -An -tx1 | tr -d ' \n')"; sleep 5"#,
        &[],
    );
    // Attach only after the reply must already have happened.
    std::thread::sleep(Duration::from_millis(800));
    attach(&mut c, "q", 0);
    let out = c.wait_for("q", "REPLY:", Duration::from_secs(5));
    c.wait_for("q", "1b5b3f36323b323263", Duration::from_secs(5));
    assert!(!out.contains("REPLY:\n"), "{out}");
}

#[test]
fn scrubs_claude_env_and_passes_the_rest() {
    std::env::set_var("CLAUDECODE", "1");
    std::env::set_var("CLAUDE_CODE_ENTRYPOINT", "cli");
    std::env::set_var("CLAUDE_PID", "123");
    std::env::set_var("CLAUDE_EFFORT", "high");
    std::env::set_var("AI_AGENT", "claude-code");
    let (_d, sock) = start(1 << 20);
    let mut c = C::new(&sock);
    c.spawn(
        "e",
        "env; echo ENV-END; sleep 5",
        &[("FOO_TMUXEDO", "bar"), ("CLAUDE_CODE_SSE_PORT", "9")],
    );
    attach(&mut c, "e", 0);
    let out = c.wait_for("e", "ENV-END", Duration::from_secs(5));
    assert!(out.contains("FOO_TMUXEDO=bar"), "{out}");
    assert!(out.contains("TERM=xterm-256color"), "{out}");
    assert!(!out.contains("CLAUDE") && !out.contains("AI_AGENT"), "{out}");
}

/// Running and not a zombie. `ps -o stat=` works on Linux and macOS (no /proc there).
fn alive(pid: u32) -> bool {
    let Ok(out) = std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
    else {
        return false;
    };
    let stat = String::from_utf8_lossy(&out.stdout);
    let state = stat.trim().chars().next();
    out.status.success() && state.is_some() && state != Some('Z')
}

#[test]
fn kill_signals_the_whole_process_group() {
    let (_d, sock) = start(1 << 20);
    let mut c = C::new(&sock);
    let pid = c.spawn("k", "sleep 300 & echo BG=$!; sleep 300", &[]);
    attach(&mut c, "k", 0);
    let out = c.wait_for("k", "BG=", Duration::from_secs(5));
    let bg: u32 = out
        .split("BG=")
        .nth(1)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap();
    assert!(alive(pid) && alive(bg));
    match c.call(|rid| Request::Kill {
        rid,
        id: "k".into(),
        signal: libc::SIGTERM,
    }) {
        Event::Ok { .. } => {}
        e => panic!("{e:?}"),
    }
    let ev = loop {
        match c.next_event(Duration::from_secs(5)) {
            Some(e @ Event::Exit { .. }) => break e,
            Some(_) => continue,
            None => panic!("no exit event"),
        }
    };
    match ev {
        Event::Exit { id, info } => {
            assert_eq!(id, "k");
            assert_eq!(info.code, None, "killed by a signal: {info:?}");
        }
        _ => unreachable!(),
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while alive(bg) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!alive(pid), "leader survived");
    assert!(!alive(bg), "background member of the group survived");
    // An exited pane can be removed, and its id reused.
    assert!(matches!(
        c.call(|rid| Request::Remove { rid, id: "k".into() }),
        Event::Ok { .. }
    ));
    c.spawn("k", "echo again; sleep 1", &[]);
}

#[test]
fn ring_wrap_is_reported_as_gap() {
    let (_d, sock) = start(4096);
    let mut c = C::new(&sock);
    c.spawn(
        "g",
        "i=0; while [ $i -lt 3000 ]; do echo line-$i; i=$((i+1)); done; echo DONE; sleep 30",
        &[],
    );
    // Wait for the writer to finish (end stops moving), well past the ring size.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut last = 0;
    loop {
        let (start, end) = c.list_end("g");
        if end > 20_000 && start > 0 && end == last {
            assert_eq!(end - start, 4096);
            break;
        }
        last = end;
        assert!(Instant::now() < deadline, "ring never wrapped");
        std::thread::sleep(Duration::from_millis(200));
    }
    let (from, gap) = attach(&mut c, "g", 0);
    assert!(gap);
    assert!(from > 0);
    c.wait_for("g", "DONE", Duration::from_secs(5));
    assert_eq!(c.data[0].1, from, "first data frame starts where attached said");
    // Within range: no gap.
    let (start, end) = c.list_end("g");
    let mut c2 = C::new(&sock);
    let (f2, g2) = attach(&mut c2, "g", start + 10);
    assert_eq!((f2, g2), (start + 10, false));
    let _ = end;
}

#[test]
fn reattach_from_offset_has_no_gap_or_duplicate_after_client_vanishes() {
    let (_d, sock) = start(1 << 20);
    let mut c = C::new(&sock);
    c.spawn("s", "i=0; while :; do echo line $i; i=$((i+1)); sleep 0.003; done", &[]);
    attach(&mut c, "s", 0);
    let t = Instant::now();
    while t.elapsed() < Duration::from_millis(400) {
        c.pump();
    }
    let first = c.bytes("s");
    let end1 = c.data.iter().map(|d| d.1 + d.2.len() as u64).max().unwrap();
    assert_eq!(end1, first.len() as u64);
    // Vanish without a goodbye: the holder only sees the socket close.
    drop(c);
    std::thread::sleep(Duration::from_millis(700));
    let mut c2 = C::new(&sock);
    let (from, gap) = attach(&mut c2, "s", end1);
    assert_eq!((from, gap), (end1, false));
    // Pump until enough lines exist to make the continuity check meaningful
    // (a fixed window yields too few on a loaded CI box).
    let t = Instant::now();
    let lines = |b: &[u8]| String::from_utf8_lossy(b).matches("line ").count();
    while t.elapsed() < Duration::from_millis(300)
        || (lines(&first) + lines(&c2.bytes("s")) <= 150 && t.elapsed() < Duration::from_secs(10))
    {
        c2.pump();
    }
    assert_eq!(c2.data[0].1, end1);
    let mut all = first;
    all.extend(c2.bytes("s"));
    let text = String::from_utf8_lossy(&all).replace('\r', "");
    let mut prev: Option<u64> = None;
    let mut checked = 0;
    for l in text.lines() {
        if let Some(n) = l.strip_prefix("line ").and_then(|n| n.parse::<u64>().ok()) {
            if let Some(p) = prev {
                assert_eq!(n, p + 1, "discontinuity after {p}");
            }
            prev = Some(n);
            checked += 1;
        }
    }
    assert!(checked > 100, "only {checked} lines");
}

#[test]
fn input_and_resize_reach_the_program() {
    let (_d, sock) = start(1 << 20);
    let mut c = C::new(&sock);
    c.spawn(
        "i",
        "stty -echo; while read -r l; do echo \"got:$l\"; stty size; done",
        &[],
    );
    attach(&mut c, "i", 0);
    c.input("i", b"hello\n");
    c.wait_for("i", "got:hello", Duration::from_secs(5));
    c.wait_for("i", "24 80", Duration::from_secs(5));
    c.send(&Request::Resize {
        id: "i".into(),
        cols: 77,
        rows: 33,
    });
    c.input("i", b"again\n");
    c.wait_for("i", "33 77", Duration::from_secs(5));
}

#[test]
fn refuses_wrong_protocol_version() {
    let (_d, sock) = start(1 << 20);
    let mut s = UnixStream::connect(&sock).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    wire::write_all(
        &mut s,
        &wire::encode_json(&Request::Hello {
            proto: 99,
            client: "x".into(),
        }),
    )
    .unwrap();
    let Msg::Json(j) = wire::read_msg(&mut s).unwrap() else {
        panic!()
    };
    assert!(matches!(
        serde_json::from_slice::<Event>(&j).unwrap(),
        Event::Error { .. }
    ));
}

#[test]
fn second_holder_on_same_socket_is_refused() {
    let (_d, sock) = start(1 << 20);
    let err = server::bind(&Config {
        socket: sock.clone(),
        ring_capacity: 1024,
    });
    assert!(err.is_err());
    // The first one still answers.
    let _ = C::new(&sock);
}

/// A pipe child (`spawn_pipe`, agent chat): stdin/stdout pipes and no
/// terminal (no echo, no CRLF), stderr to its own file, the same ring and
/// offsets as a pane, reattach from an offset without a gap, its own
/// process group, and `list` marks it `pipe`.
#[test]
fn pipe_child_has_pipes_a_ring_and_survives_its_client() {
    let (d, sock) = start(1 << 20);
    let err = d.path().join("child.err");
    let mut c = C::new(&sock);
    let spec = SpawnSpec {
        id: "chat-abcdefgh".into(),
        argv: vec![
            "sh".into(),
            "-c".into(),
            r#"[ -t 0 ] && echo TTY; echo oops >&2; while read l; do echo "got:$l"; done; echo bye"#.into(),
        ],
        cwd: Some("/".into()),
        env: BTreeMap::new(),
        cols: 80,
        rows: 24,
    };
    let pid = match c.call(|rid| Request::SpawnPipe {
        rid,
        spec,
        stderr: Some(err.display().to_string()),
    }) {
        Event::Ok { data, .. } => {
            assert_eq!(data["pipe"], true);
            data["pid"].as_u64().unwrap() as u32
        }
        e => panic!("spawn_pipe failed: {e:?}"),
    };
    attach(&mut c, "chat-abcdefgh", 0);
    c.input("chat-abcdefgh", b"one\n");
    let out = c.wait_for("chat-abcdefgh", "got:one\n", Duration::from_secs(5));
    assert!(!out.contains("TTY"), "a pipe child has no terminal: {out:?}");
    assert!(!out.contains('\r'), "no CRLF translation: {out:?}");
    assert!(!out.contains("one\ngot"), "no echo: {out:?}");
    assert_eq!(std::fs::read_to_string(&err).unwrap(), "oops\n");
    // Its own process group (the holder signals the group).
    // SAFETY: plain syscall.
    assert_eq!(unsafe { libc::getpgid(pid as libc::pid_t) }, pid as libc::pid_t);
    match c.call(|rid| Request::List { rid }) {
        Event::Ok { data, .. } => {
            let p = data["panes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|p| p["id"] == "chat-abcdefgh")
                .unwrap()
                .clone();
            assert_eq!(p["pipe"], true);
        }
        e => panic!("{e:?}"),
    }
    // The client goes away; output keeps landing in the ring; a new client
    // reattaches from where the first one stopped, with no gap or repeat.
    let (_, end) = c.list_end("chat-abcdefgh");
    drop(c);
    let mut c2 = C::new(&sock);
    c2.input("chat-abcdefgh", b"two\n");
    std::thread::sleep(Duration::from_millis(200));
    let (from, gap) = attach(&mut c2, "chat-abcdefgh", end);
    assert_eq!((from, gap), (end, false));
    let out = c2.wait_for("chat-abcdefgh", "got:two\n", Duration::from_secs(5));
    assert_eq!(out, "got:two\n");
    // Stopping is by signal to the group; the exit is reported with its name.
    c2.call(|rid| Request::Kill {
        rid,
        id: "chat-abcdefgh".into(),
        signal: libc::SIGTERM,
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let exited = c2.events.iter().any(|e| {
            matches!(e, Event::Exit { id, info } if id == "chat-abcdefgh" && info.signal.as_deref() == Some("SIGTERM"))
        });
        if exited {
            break;
        }
        assert!(Instant::now() < deadline, "no exit event: {:?}", c2.events);
        c2.pump();
    }
}
