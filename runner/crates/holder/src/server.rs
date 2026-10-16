//! The holder process. Small and boring on purpose: plain threads, no async.
//!
//! Per pane: a reader thread drains the PTY into a ring with absolute offsets
//! (so the child never blocks, whether or not anybody is attached) and
//! answers stateless terminal queries; a writer thread feeds input; a waiter
//! thread reaps the child and broadcasts its exit.
//!
//! Per connection: a request thread; per attachment, a sender thread that
//! streams the ring from an offset and then live. A slow or dead client only
//! ever blocks its own sender threads.

use crate::queries::QueryAnswerer;
use crate::wire::{self, Event, ExitInfo, Msg, PaneInfo, Request, SpawnSpec, HOLDER_PROTO};
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use serde_json::json;
use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

pub const DEFAULT_RING: usize = 2 * 1024 * 1024;
const READ_CHUNK: usize = 64 * 1024;
const SEND_CHUNK: usize = 256 * 1024;

/// Environment variables never passed to panes (decision 0001: a Claude
/// launched under an inherited child-session marker disables transcripts).
/// Exact names plus the `CLAUDE_CODE_` prefix: Claude Code keeps adding
/// session variables under that prefix, so it is matched whole. A bare
/// `CLAUDE_` prefix is NOT safe: `CLAUDE_CONFIG_DIR` is the user's own setting
/// and must reach panes. `AI_AGENT` and `CLAUDE_EFFORT` are session markers
/// outside the prefix, so they are listed by name. Keep the claude plugin's
/// `env.scrub` in step (the conformance check enforces it).
pub fn scrubbed_env_key(k: &str) -> bool {
    matches!(k, "CLAUDECODE" | "CLAUDE_PID" | "CLAUDE_EFFORT" | "AI_AGENT") || k.starts_with("CLAUDE_CODE_")
}

pub struct Config {
    pub socket: PathBuf,
    pub ring_capacity: usize,
}

struct Ring {
    buf: VecDeque<u8>,
    end: u64,
    closed: bool,
}

impl Ring {
    fn start(&self) -> u64 {
        self.end - self.buf.len() as u64
    }
}

struct Pane {
    id: String,
    pid: u32,
    argv: Vec<String>,
    cwd: Option<String>,
    size: Mutex<(u16, u16)>,
    ring: Mutex<Ring>,
    cv: Condvar,
    input: Mutex<mpsc::Sender<Vec<u8>>>,
    /// None for a pipe child.
    master: Mutex<Option<Box<dyn MasterPty + Send>>>,
    exited: Mutex<Option<ExitInfo>>,
}

impl Pane {
    fn info(&self) -> PaneInfo {
        let (start, end) = {
            let r = self.ring.lock().unwrap();
            (r.start(), r.end)
        };
        let (cols, rows) = *self.size.lock().unwrap();
        PaneInfo {
            id: self.id.clone(),
            pid: self.pid,
            argv: self.argv.clone(),
            cwd: self.cwd.clone(),
            cols,
            rows,
            start,
            end,
            exited: self.exited.lock().unwrap().clone(),
            pipe: self.master.lock().unwrap().is_none(),
        }
    }

    fn signal(&self, sig: i32) -> io::Result<()> {
        let pid = self.pid as libc::pid_t;
        // The child is a session (and so process-group) leader: signal the group.
        // SAFETY: plain syscall with integer arguments.
        if unsafe { libc::kill(-pid, sig) } == 0 {
            return Ok(());
        }
        if unsafe { libc::kill(pid, sig) } == 0 {
            return Ok(());
        }
        Err(io::Error::last_os_error())
    }
}

struct Conn {
    out: Mutex<UnixStream>,
    attachments: Mutex<HashMap<String, Arc<AtomicBool>>>,
    dead: AtomicBool,
}

impl Conn {
    fn send(&self, bytes: &[u8]) -> bool {
        if self.dead.load(Ordering::Relaxed) {
            return false;
        }
        let mut s = self.out.lock().unwrap();
        if wire::write_all(&mut *s, bytes).is_err() {
            self.dead.store(true, Ordering::Relaxed);
            let _ = s.shutdown(std::net::Shutdown::Both);
            return false;
        }
        true
    }

    fn event(&self, ev: &Event) -> bool {
        self.send(&wire::encode_json(ev))
    }

    fn cancel_all(&self, panes: &HashMap<String, Arc<Pane>>) {
        let mut a = self.attachments.lock().unwrap();
        for (id, flag) in a.drain() {
            flag.store(true, Ordering::Relaxed);
            if let Some(p) = panes.get(&id) {
                p.cv.notify_all();
            }
        }
    }
}

struct Shared {
    panes: Mutex<HashMap<String, Arc<Pane>>>,
    conns: Mutex<Vec<Arc<Conn>>>,
    ring_capacity: usize,
}

/// Socket directory: created 0700.
fn prepare_dir(dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

/// Take an exclusive lock so two holders never share a socket path. The
/// returned file must stay open for the life of the process.
fn lock_file(path: &Path) -> io::Result<std::fs::File> {
    use std::os::fd::AsRawFd;
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)?;
    // SAFETY: flock on a valid fd.
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!("another holder holds {}", path.display()),
        ));
    }
    Ok(f)
}

pub struct Bound {
    listener: UnixListener,
    _lock: std::fs::File,
    shared: Arc<Shared>,
}

/// Bind the socket (0600) under an exclusive lock. Separate from [`run`] so a
/// caller can report readiness only once the socket exists.
pub fn bind(cfg: &Config) -> io::Result<Bound> {
    let dir = cfg
        .socket
        .parent()
        .ok_or_else(|| io::Error::other("socket path has no parent"))?;
    prepare_dir(dir)?;
    let lock = lock_file(&cfg.socket.with_extension("lock"))?;
    // We hold the lock, so any socket file left behind is stale.
    let _ = std::fs::remove_file(&cfg.socket);
    let listener = UnixListener::bind(&cfg.socket)?;
    std::fs::set_permissions(&cfg.socket, std::fs::Permissions::from_mode(0o600))?;
    Ok(Bound {
        listener,
        _lock: lock,
        shared: Arc::new(Shared {
            panes: Mutex::new(HashMap::new()),
            conns: Mutex::new(Vec::new()),
            ring_capacity: cfg.ring_capacity.max(1024),
        }),
    })
}

/// Serve forever.
pub fn run(bound: Bound) -> io::Result<()> {
    let Bound {
        listener,
        _lock,
        shared,
    } = bound;
    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        let shared = shared.clone();
        thread::spawn(move || serve_conn(shared, conn));
    }
    Ok(())
}

fn serve_conn(shared: Arc<Shared>, stream: UnixStream) {
    let Ok(out) = stream.try_clone() else { return };
    let conn = Arc::new(Conn {
        out: Mutex::new(out),
        attachments: Mutex::new(HashMap::new()),
        dead: AtomicBool::new(false),
    });
    let mut rd = io::BufReader::new(stream);

    // Versioned handshake.
    match wire::read_msg(&mut rd) {
        Ok(Msg::Json(j)) => match serde_json::from_slice::<Request>(&j) {
            Ok(Request::Hello { proto, .. }) if proto == HOLDER_PROTO => {
                conn.event(&Event::Hello {
                    proto: HOLDER_PROTO,
                    version: crate::VERSION.into(),
                    pid: std::process::id(),
                    build: Some(crate::BUILD.into()),
                });
            }
            Ok(Request::Hello { proto, .. }) => {
                conn.event(&Event::Error {
                    rid: 0,
                    message: format!("holder speaks protocol {HOLDER_PROTO}, client {proto}"),
                });
                return;
            }
            _ => return,
        },
        _ => return,
    }
    shared.conns.lock().unwrap().push(conn.clone());

    loop {
        match wire::read_msg(&mut rd) {
            Ok(Msg::Json(j)) => match serde_json::from_slice::<Request>(&j) {
                Ok(req) => handle(&shared, &conn, req),
                Err(_) => continue, // unknown or malformed op: ignored
            },
            Ok(Msg::Data { id, bytes, .. }) => {
                let pane = shared.panes.lock().unwrap().get(&id).cloned();
                if let Some(p) = pane {
                    let _ = p.input.lock().unwrap().send(bytes);
                }
            }
            Err(_) => break,
        }
    }
    conn.dead.store(true, Ordering::Relaxed);
    let panes = shared.panes.lock().unwrap().clone();
    conn.cancel_all(&panes);
    shared.conns.lock().unwrap().retain(|c| !Arc::ptr_eq(c, &conn));
}

fn handle(shared: &Arc<Shared>, conn: &Arc<Conn>, req: Request) {
    match req {
        Request::Hello { .. } => {}
        Request::Spawn { rid, spec } => match spawn(shared, spec) {
            Ok(pid) => {
                conn.event(&Event::Ok {
                    rid,
                    data: json!({ "pid": pid }),
                });
            }
            Err(e) => {
                conn.event(&Event::Error {
                    rid,
                    message: e.to_string(),
                });
            }
        },
        Request::SpawnPipe { rid, spec, stderr } => match spawn_pipe(shared, spec, stderr) {
            Ok(pid) => {
                conn.event(&Event::Ok {
                    rid,
                    data: json!({ "pid": pid, "pipe": true }),
                });
            }
            Err(e) => {
                conn.event(&Event::Error {
                    rid,
                    message: e.to_string(),
                });
            }
        },
        Request::List { rid } => {
            let mut panes: Vec<PaneInfo> = shared.panes.lock().unwrap().values().map(|p| p.info()).collect();
            panes.sort_by(|a, b| a.id.cmp(&b.id));
            conn.event(&Event::Ok {
                rid,
                data: json!({ "panes": panes }),
            });
        }
        Request::Attach { rid, id, from } => {
            let pane = shared.panes.lock().unwrap().get(&id).cloned();
            let Some(pane) = pane else {
                conn.event(&Event::Error {
                    rid,
                    message: format!("no pane {id}"),
                });
                return;
            };
            let (pos, gap, end) = {
                let r = pane.ring.lock().unwrap();
                if from < r.start() {
                    (r.start(), true, r.end)
                } else {
                    (from.min(r.end), false, r.end)
                }
            };
            // Replace any previous attachment of this pane on this connection.
            let cancel = Arc::new(AtomicBool::new(false));
            if let Some(old) = conn.attachments.lock().unwrap().insert(id.clone(), cancel.clone()) {
                old.store(true, Ordering::Relaxed);
                pane.cv.notify_all();
            }
            conn.event(&Event::Attached {
                rid,
                id,
                from: pos,
                gap,
                end,
            });
            let conn = conn.clone();
            thread::spawn(move || stream_pane(pane, conn, pos, cancel));
        }
        Request::Detach { id } => {
            if let Some(flag) = conn.attachments.lock().unwrap().remove(&id) {
                flag.store(true, Ordering::Relaxed);
                if let Some(p) = shared.panes.lock().unwrap().get(&id) {
                    p.cv.notify_all();
                }
            }
        }
        Request::Resize { id, cols, rows } => {
            if let Some(p) = shared.panes.lock().unwrap().get(&id) {
                let (cols, rows) = (cols.clamp(1, 1000), rows.clamp(1, 1000));
                if let Some(m) = p.master.lock().unwrap().as_ref() {
                    *p.size.lock().unwrap() = (cols, rows);
                    let _ = m.resize(PtySize {
                        rows,
                        cols,
                        pixel_width: 0,
                        pixel_height: 0,
                    });
                }
            }
        }
        Request::Kill { rid, id, signal } => {
            let pane = shared.panes.lock().unwrap().get(&id).cloned();
            let res = match pane {
                Some(p) => p.signal(signal).map_err(|e| e.to_string()),
                None => Err(format!("no pane {id}")),
            };
            conn.event(&match res {
                Ok(()) => Event::Ok { rid, data: json!({}) },
                Err(message) => Event::Error { rid, message },
            });
        }
        Request::Remove { rid, id } => {
            let mut panes = shared.panes.lock().unwrap();
            let ev = match panes.get(&id) {
                Some(p) if p.exited.lock().unwrap().is_none() => Event::Error {
                    rid,
                    message: format!("pane {id} is still running"),
                },
                Some(_) => {
                    if let Some(p) = panes.remove(&id) {
                        p.ring.lock().unwrap().closed = true;
                        p.cv.notify_all();
                    }
                    Event::Ok { rid, data: json!({}) }
                }
                None => Event::Ok { rid, data: json!({}) },
            };
            drop(panes);
            conn.event(&ev);
        }
    }
}

fn stream_pane(pane: Arc<Pane>, conn: Arc<Conn>, mut pos: u64, cancel: Arc<AtomicBool>) {
    loop {
        let (offset, chunk) = {
            let mut r = pane.ring.lock().unwrap();
            loop {
                if cancel.load(Ordering::Relaxed) || conn.dead.load(Ordering::Relaxed) {
                    return;
                }
                if r.end > pos {
                    break;
                }
                if r.closed {
                    return;
                }
                r = pane.cv.wait_timeout(r, Duration::from_millis(500)).unwrap().0;
            }
            // Fell behind the ring: skip ahead. The jump in offsets tells the
            // client bytes were lost.
            if pos < r.start() {
                pos = r.start();
            }
            let skip = (pos - r.start()) as usize;
            let n = ((r.end - pos) as usize).min(SEND_CHUNK);
            let (a, b) = r.buf.as_slices();
            let mut chunk = Vec::with_capacity(n);
            if skip < a.len() {
                let take = n.min(a.len() - skip);
                chunk.extend_from_slice(&a[skip..skip + take]);
                chunk.extend_from_slice(&b[..n - take]);
            } else {
                let s = skip - a.len();
                chunk.extend_from_slice(&b[s..s + n]);
            }
            (pos, chunk)
        };
        if !conn.send(&wire::encode_data(&pane.id, offset, &chunk)) {
            return;
        }
        pos = offset + chunk.len() as u64;
    }
}

fn check_free(shared: &Arc<Shared>, spec: &SpawnSpec) -> io::Result<()> {
    let panes = shared.panes.lock().unwrap();
    if let Some(p) = panes.get(&spec.id) {
        if p.exited.lock().unwrap().is_none() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("pane {} is running", spec.id),
            ));
        }
    }
    if spec.argv.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty argv"));
    }
    Ok(())
}

/// The child's environment: ours plus the spec's, minus the scrubbed names.
fn child_env(spec: &SpawnSpec) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = std::env::vars().collect();
    env.extend(spec.env.iter().map(|(k, v)| (k.clone(), v.clone())));
    env.retain(|(k, _)| !scrubbed_env_key(k));
    env
}

/// A pipe child (see [`wire::Request::SpawnPipe`]).
fn spawn_pipe(shared: &Arc<Shared>, spec: SpawnSpec, stderr: Option<String>) -> io::Result<u32> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    check_free(shared, &spec)?;
    let mut cmd = Command::new(&spec.argv[0]);
    cmd.args(&spec.argv[1..]);
    cmd.env_clear();
    for (k, v) in child_env(&spec) {
        cmd.env(k, v);
    }
    if let Some(cwd) = &spec.cwd {
        cmd.current_dir(cwd);
    }
    let err = match &stderr {
        Some(path) => {
            use std::os::unix::fs::OpenOptionsExt;
            Stdio::from(
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .mode(0o600)
                    .open(path)?,
            )
        }
        None => Stdio::null(),
    };
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(err);
    // Its own session (and process group), like a pane's child: a signal
    // to the group reaches the tools it started too.
    // SAFETY: setsid is async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn()?;
    let pid = child.id();
    let mut reader = child.stdout.take().ok_or_else(|| io::Error::other("no stdout"))?;
    let mut writer = child.stdin.take().ok_or_else(|| io::Error::other("no stdin"))?;
    let (in_tx, in_rx) = mpsc::channel::<Vec<u8>>();
    let pane = Arc::new(Pane {
        id: spec.id.clone(),
        pid,
        argv: spec.argv.clone(),
        cwd: spec.cwd.clone(),
        size: Mutex::new((spec.cols.clamp(1, 1000), spec.rows.clamp(1, 1000))),
        ring: Mutex::new(Ring {
            buf: VecDeque::with_capacity(64 * 1024),
            end: 0,
            closed: false,
        }),
        cv: Condvar::new(),
        input: Mutex::new(in_tx),
        master: Mutex::new(None),
        exited: Mutex::new(None),
    });
    thread::spawn(move || {
        while let Ok(bytes) = in_rx.recv() {
            if writer.write_all(&bytes).is_err() {
                return;
            }
            let _ = writer.flush();
        }
    });
    {
        let pane = pane.clone();
        let cap = shared.ring_capacity;
        thread::spawn(move || {
            let mut chunk = vec![0u8; READ_CHUNK];
            loop {
                let n = match reader.read(&mut chunk) {
                    Ok(0) | Err(_) => 0,
                    Ok(n) => n,
                };
                let mut r = pane.ring.lock().unwrap();
                if n == 0 {
                    r.closed = true;
                    pane.cv.notify_all();
                    return;
                }
                r.buf.extend(&chunk[..n]);
                r.end += n as u64;
                let over = r.buf.len().saturating_sub(cap);
                if over > 0 {
                    r.buf.drain(..over);
                }
                drop(r);
                pane.cv.notify_all();
            }
        });
    }
    {
        let pane = pane.clone();
        let shared = shared.clone();
        thread::spawn(move || {
            use std::os::unix::process::ExitStatusExt;
            let info = match child.wait() {
                Ok(s) => ExitInfo {
                    code: s.code(),
                    signal: s.signal().map(signal_name),
                },
                Err(_) => ExitInfo {
                    code: None,
                    signal: None,
                },
            };
            *pane.exited.lock().unwrap() = Some(info.clone());
            let ev = wire::encode_json(&Event::Exit {
                id: pane.id.clone(),
                info,
            });
            let conns = shared.conns.lock().unwrap().clone();
            for c in conns {
                c.send(&ev);
            }
        });
    }
    shared.panes.lock().unwrap().insert(spec.id, pane);
    Ok(pid)
}

fn signal_name(sig: i32) -> String {
    match sig {
        libc::SIGHUP => "SIGHUP".into(),
        libc::SIGINT => "SIGINT".into(),
        libc::SIGKILL => "SIGKILL".into(),
        libc::SIGTERM => "SIGTERM".into(),
        libc::SIGPIPE => "SIGPIPE".into(),
        n => format!("SIG{n}"),
    }
}

fn spawn(shared: &Arc<Shared>, spec: SpawnSpec) -> io::Result<u32> {
    {
        let panes = shared.panes.lock().unwrap();
        if let Some(p) = panes.get(&spec.id) {
            if p.exited.lock().unwrap().is_none() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("pane {} is running", spec.id),
                ));
            }
        }
    }
    if spec.argv.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty argv"));
    }
    let (cols, rows) = (spec.cols.clamp(1, 1000), spec.rows.clamp(1, 1000));
    let pair = native_pty_system()
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| io::Error::other(e.to_string()))?;

    let mut cmd = CommandBuilder::new(&spec.argv[0]);
    cmd.args(&spec.argv[1..]);
    cmd.env_clear();
    let mut env: Vec<(String, String)> = std::env::vars().collect();
    env.extend(spec.env.iter().map(|(k, v)| (k.clone(), v.clone())));
    for (k, v) in env {
        if !scrubbed_env_key(&k) {
            cmd.env(k, v);
        }
    }
    if !spec.env.contains_key("TERM") {
        cmd.env("TERM", "xterm-256color");
    }
    if !spec.env.contains_key("COLORTERM") {
        cmd.env("COLORTERM", "truecolor");
    }
    if let Some(cwd) = &spec.cwd {
        cmd.cwd(cwd);
    }
    let mut child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| io::Error::other(e.to_string()))?;
    drop(pair.slave);
    let pid = child.process_id().unwrap_or(0);
    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| io::Error::other(e.to_string()))?;
    let mut writer = pair.master.take_writer().map_err(|e| io::Error::other(e.to_string()))?;
    let (in_tx, in_rx) = mpsc::channel::<Vec<u8>>();

    let pane = Arc::new(Pane {
        id: spec.id.clone(),
        pid,
        argv: spec.argv.clone(),
        cwd: spec.cwd.clone(),
        size: Mutex::new((cols, rows)),
        ring: Mutex::new(Ring {
            buf: VecDeque::with_capacity(64 * 1024),
            end: 0,
            closed: false,
        }),
        cv: Condvar::new(),
        input: Mutex::new(in_tx.clone()),
        master: Mutex::new(Some(pair.master)),
        exited: Mutex::new(None),
    });

    // Input: its own thread, so a child that is not reading stdin can never
    // block the output reader (which also writes query answers).
    thread::spawn(move || {
        while let Ok(bytes) = in_rx.recv() {
            if writer.write_all(&bytes).is_err() {
                return;
            }
            let _ = writer.flush();
        }
    });

    // Output: drain continuously into the ring.
    {
        let pane = pane.clone();
        let cap = shared.ring_capacity;
        thread::spawn(move || {
            let mut q = QueryAnswerer::default();
            let mut chunk = vec![0u8; READ_CHUNK];
            loop {
                let n = match reader.read(&mut chunk) {
                    Ok(0) | Err(_) => 0,
                    Ok(n) => n,
                };
                let mut r = pane.ring.lock().unwrap();
                if n == 0 {
                    r.closed = true;
                    pane.cv.notify_all();
                    return;
                }
                r.buf.extend(&chunk[..n]);
                r.end += n as u64;
                let over = r.buf.len().saturating_sub(cap);
                if over > 0 {
                    r.buf.drain(..over);
                }
                drop(r);
                pane.cv.notify_all();
                let answer = q.scan(&chunk[..n]);
                if !answer.is_empty() {
                    let _ = in_tx.send(answer);
                }
            }
        });
    }

    // Exit: reap and tell every connection.
    {
        let pane = pane.clone();
        let shared = shared.clone();
        thread::spawn(move || {
            let status = child.wait();
            let info = match status {
                Ok(s) => ExitInfo {
                    code: if s.signal().is_some() {
                        None
                    } else {
                        Some(s.exit_code() as i32)
                    },
                    signal: s.signal().map(str::to_owned),
                },
                Err(_) => ExitInfo {
                    code: None,
                    signal: None,
                },
            };
            *pane.exited.lock().unwrap() = Some(info.clone());
            let ev = wire::encode_json(&Event::Exit {
                id: pane.id.clone(),
                info,
            });
            let conns = shared.conns.lock().unwrap().clone();
            for c in conns {
                c.send(&ev);
            }
        });
    }

    shared.panes.lock().unwrap().insert(spec.id, pane);
    Ok(pid)
}
