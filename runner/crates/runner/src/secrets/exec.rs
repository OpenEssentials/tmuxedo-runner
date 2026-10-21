//! `tmuxedo secret exec NAME… -- cmd` and the MCP `secret-exec` wrapper
//! (docs/design/secrets-and-env.md §4.1).
//!
//! The values never pass through the caller. The CLI hands the runner its
//! stdin, stdout and stderr (SCM_RIGHTS over a 0600 socket next to the runner
//! socket) with the request; the runner checks who asks, fetches the values,
//! and starts the program itself with them in its environment and the
//! caller's descriptors as its stdio. Output that reaches the pane's terminal
//! goes through the pane's masker like any other byte. The CLI waits, forwards
//! SIGINT/SIGTERM/SIGHUP/SIGQUIT, and exits with the program's status.
//!
//! Wire (one connection per run):
//!   → sendmsg: JSON line `{names, argv, cwd, path, mcp?: {plugin, server}}` + 3 fds
//!   ← `{"started": pid}` or `{"error": {code, message}}`
//!   → `signal <n>` lines (optional)
//!   ← `{"exit": code}` or `{"signal": n}`
//!
//! Not a sandbox: a program that is allowed can still print or store the
//! value. The allowlist is for the resolved path of argv[0], so a shell is
//! refused unless allowed by name.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

/// A request the runner thread hands the daemon; `reply` gets the values
/// (name, value) or a refusal (code, message).
/// The values, or (code, message).
pub type ExecAnswer = Result<Vec<(String, zeroize::Zeroizing<Vec<u8>>)>, (String, String)>;

pub struct ExecAsk {
    pub pid: i32,
    pub names: Vec<String>,
    pub program: String,
    pub mcp: Option<(String, String)>,
    pub reply: std::sync::mpsc::Sender<ExecAnswer>,
}

pub fn socket_path(runner_socket: &Path) -> PathBuf {
    runner_socket.with_file_name("secret-exec.sock")
}

/// The pid at the other end of a unix socket.
pub fn peer_pid(fd: RawFd) -> Option<i32> {
    #[cfg(target_os = "linux")]
    {
        let mut c = libc::ucred { pid: 0, uid: 0, gid: 0 };
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: getsockopt writes at most `len` bytes into `c`.
        let r = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut c as *mut libc::ucred).cast(),
                &mut len,
            )
        };
        (r == 0 && c.pid > 0).then_some(c.pid)
    }
    #[cfg(target_os = "macos")]
    {
        const SOL_LOCAL: libc::c_int = 0;
        const LOCAL_PEERPID: libc::c_int = 2;
        let mut pid: libc::pid_t = 0;
        let mut len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
        // SAFETY: as above.
        let r = unsafe {
            libc::getsockopt(
                fd,
                SOL_LOCAL,
                LOCAL_PEERPID,
                (&mut pid as *mut libc::pid_t).cast(),
                &mut len,
            )
        };
        (r == 0 && pid > 0).then_some(pid)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = fd;
        None
    }
}

/// Resolve argv[0] the way exec would, with the caller's PATH, to a
/// canonical absolute path (symlinks followed).
pub fn resolve_program(argv0: &str, path: &str, cwd: &Path) -> Option<PathBuf> {
    let found = if argv0.contains('/') {
        let p = Path::new(argv0);
        Some(if p.is_absolute() { p.to_path_buf() } else { cwd.join(p) })
    } else {
        path.split(':')
            .filter(|d| !d.is_empty())
            .map(|d| Path::new(d).join(argv0))
            .find(|p| is_executable(p))
    }?;
    let real = std::fs::canonicalize(&found).ok()?;
    is_executable(&real).then_some(real)
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Does `program` (a resolved absolute path) match an allowlist entry: a bare
/// name matches the file name (`gh`), an absolute path matches exactly.
pub fn program_allowed(program: &Path, allowed: &[String]) -> bool {
    let name = program.file_name().and_then(|n| n.to_str()).unwrap_or("");
    allowed.iter().any(|a| {
        let a = a.split_whitespace().next().unwrap_or("");
        if a.starts_with('/') {
            std::fs::canonicalize(a).is_ok_and(|c| c == program)
        } else {
            !a.is_empty() && a == name
        }
    })
}

// ── server (in the runner) ───────────────────────────────────────────────

/// Serve the socket on a thread; each request goes to `ask`.
pub fn serve(path: &Path, ask: tokio::sync::mpsc::UnboundedSender<ExecAsk>) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::remove_file(path);
    let l = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    std::thread::Builder::new().name("secret-exec".into()).spawn(move || {
        for s in l.incoming().flatten() {
            let ask = ask.clone();
            let _ = std::thread::Builder::new()
                .name("secret-exec-run".into())
                .spawn(move || handle(s, ask));
        }
    })?;
    Ok(())
}

fn reply(s: &mut UnixStream, v: Value) {
    let _ = s.write_all(format!("{v}\n").as_bytes());
}

fn handle(mut s: UnixStream, ask: tokio::sync::mpsc::UnboundedSender<ExecAsk>) {
    let Some(pid) = peer_pid(s.as_raw_fd()) else {
        return reply(
            &mut s,
            json!({"error": {"code": "forbidden", "message": "unknown caller"}}),
        );
    };
    let (line, fds) = match recv_with_fds(&s) {
        Ok(x) => x,
        Err(e) => return reply(&mut s, json!({"error": {"code": "invalid_request", "message": e}})),
    };
    let req: Value = match serde_json::from_slice(&line) {
        Ok(v) => v,
        Err(_) => {
            return reply(
                &mut s,
                json!({"error": {"code": "invalid_request", "message": "not JSON"}}),
            )
        }
    };
    if fds.len() != 3 {
        return reply(
            &mut s,
            json!({"error": {"code": "invalid_request", "message": "stdio not passed"}}),
        );
    }
    let names: Vec<String> = req["names"]
        .as_array()
        .map(|a| a.iter().filter_map(|n| n.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let argv: Vec<String> = req["argv"]
        .as_array()
        .map(|a| a.iter().filter_map(|n| n.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let cwd = PathBuf::from(req["cwd"].as_str().unwrap_or("/"));
    let path = req["path"].as_str().unwrap_or("/usr/bin:/bin");
    if names.is_empty() || argv.is_empty() || names.len() > 32 {
        return reply(
            &mut s,
            json!({"error": {"code": "invalid_request", "message": "names and a command are required"}}),
        );
    }
    let Some(program) = resolve_program(&argv[0], path, &cwd) else {
        return reply(
            &mut s,
            json!({"error": {"code": "not_found", "message": format!("{}: command not found", argv[0])}}),
        );
    };
    let mcp = match (req["mcp"]["plugin"].as_str(), req["mcp"]["server"].as_str()) {
        (Some(p), Some(sv)) => Some((p.to_string(), sv.to_string())),
        _ => None,
    };
    let (tx, rx) = std::sync::mpsc::channel();
    let _ = ask.send(ExecAsk {
        pid,
        names,
        program: program.display().to_string(),
        mcp,
        reply: tx,
    });
    let values = match rx.recv_timeout(std::time::Duration::from_secs(30)) {
        Ok(Ok(v)) => v,
        Ok(Err((code, message))) => return reply(&mut s, json!({"error": {"code": code, "message": message}})),
        Err(_) => {
            return reply(
                &mut s,
                json!({"error": {"code": "timeout", "message": "the runner did not answer"}}),
            )
        }
    };
    let mut fds = fds.into_iter();
    let (i, o, e) = (fds.next().unwrap(), fds.next().unwrap(), fds.next().unwrap());
    let mut cmd = std::process::Command::new(&program);
    cmd.arg0(&argv[0]).args(&argv[1..]).current_dir(&cwd);
    cmd.stdin(std::process::Stdio::from(i))
        .stdout(std::process::Stdio::from(o))
        .stderr(std::process::Stdio::from(e));
    if let Some(env) = req["env"].as_object() {
        cmd.env_clear();
        for (k, v) in env {
            if let Some(v) = v.as_str() {
                cmd.env(k, v);
            }
        }
    }
    for (k, v) in &values {
        cmd.env(k, std::ffi::OsStr::from_bytes(v));
    }
    drop(values);
    // Its own process group, so a forwarded signal reaches what it starts too.
    cmd.process_group(0);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return reply(
                &mut s,
                json!({"error": {"code": "spawn_failed", "message": format!("{}: {e}", argv[0])}}),
            )
        }
    };
    let cpid = child.id() as i32;
    reply(&mut s, json!({"started": cpid}));
    // Forward signals the caller relays.
    if let Ok(r) = s.try_clone() {
        let _ = std::thread::Builder::new()
            .name("secret-exec-sig".into())
            .spawn(move || {
                let mut r = BufReader::new(r);
                let mut line = String::new();
                while r.read_line(&mut line).unwrap_or(0) > 0 {
                    if let Some(n) = line.trim().strip_prefix("signal ").and_then(|n| n.parse::<i32>().ok()) {
                        if [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT].contains(&n) {
                            // SAFETY: plain syscall on the child's group.
                            unsafe { libc::kill(-cpid, n) };
                        }
                    }
                    line.clear();
                }
            });
    }
    use std::os::unix::process::ExitStatusExt;
    match child.wait() {
        Ok(st) => match (st.code(), st.signal()) {
            (Some(c), _) => reply(&mut s, json!({"exit": c})),
            (None, Some(sig)) => reply(&mut s, json!({"signal": sig})),
            _ => reply(&mut s, json!({"exit": 1})),
        },
        Err(_) => reply(&mut s, json!({"exit": 1})),
    }
}

use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;

/// Read one message (up to 64 KiB of JSON) and any descriptors passed with it.
fn recv_with_fds(s: &UnixStream) -> Result<(Vec<u8>, Vec<OwnedFd>), String> {
    let mut buf = vec![0u8; 64 * 1024];
    let mut cmsg = vec![0u8; unsafe { libc::CMSG_SPACE((std::mem::size_of::<RawFd>() * 3) as u32) } as usize];
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    };
    // SAFETY: msghdr points at our buffers for the call's duration.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg.as_mut_ptr().cast();
    msg.msg_controllen = cmsg.len() as _;
    let n = unsafe { libc::recvmsg(s.as_raw_fd(), &mut msg, 0) };
    if n <= 0 {
        return Err("nothing received".into());
    }
    let mut fds = vec![];
    // SAFETY: walking the control messages the kernel wrote.
    unsafe {
        let mut c = libc::CMSG_FIRSTHDR(&msg);
        while !c.is_null() {
            if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                let data = libc::CMSG_DATA(c) as *const RawFd;
                let count = ((*c).cmsg_len as usize - libc::CMSG_LEN(0) as usize) / std::mem::size_of::<RawFd>();
                for i in 0..count {
                    let fd = std::ptr::read_unaligned(data.add(i));
                    libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
                    fds.push(OwnedFd::from_raw_fd(fd));
                }
            }
            c = libc::CMSG_NXTHDR(&msg, c);
        }
    }
    if msg.msg_flags & libc::MSG_CTRUNC != 0 {
        return Err("too many descriptors".into());
    }
    // A stream socket can split the message (macOS's local socket buffer is
    // 8 KiB and the caller's environment is often larger): read to the newline.
    let mut have = n as usize;
    // Bounded, and cleared after: the same socket later waits on the caller's signals.
    let _ = s.set_read_timeout(Some(std::time::Duration::from_secs(10)));
    while !buf[..have].contains(&b'\n') && have < buf.len() {
        match (&*s).read(&mut buf[have..]) {
            Ok(0) => break,
            Ok(k) => have += k,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.to_string()),
        }
    }
    let _ = s.set_read_timeout(None);
    buf.truncate(have);
    if let Some(nl) = buf.iter().position(|&b| b == b'\n') {
        buf.truncate(nl);
    }
    Ok((buf, fds))
}

// ── client (the CLI in a pane) ───────────────────────────────────────────

/// Send `data` with `fds` attached to its first bytes. Only a short first part
/// goes with the descriptors (a stream socket may take less, and a small send
/// buffer refuses a large message with them); the rest follows as plain bytes.
fn send_with_fds(mut s: &UnixStream, data: &[u8], fds: &[RawFd]) -> std::io::Result<()> {
    let first = &data[..data.len().min(1024)];
    let sent = sendmsg_fds(s, first, fds)?;
    s.write_all(&data[sent..])
}

fn sendmsg_fds(s: &UnixStream, data: &[u8], fds: &[RawFd]) -> std::io::Result<usize> {
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of_val(fds) as u32) } as usize;
    let mut cmsg = vec![0u8; space];
    let mut iov = libc::iovec {
        iov_base: data.as_ptr() as *mut _,
        iov_len: data.len(),
    };
    // SAFETY: msghdr and the control buffer live for the call.
    unsafe {
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cmsg.as_mut_ptr().cast();
        msg.msg_controllen = space as _;
        let c = libc::CMSG_FIRSTHDR(&msg);
        (*c).cmsg_level = libc::SOL_SOCKET;
        (*c).cmsg_type = libc::SCM_RIGHTS;
        (*c).cmsg_len = libc::CMSG_LEN(std::mem::size_of_val(fds) as u32) as _;
        std::ptr::copy_nonoverlapping(fds.as_ptr(), libc::CMSG_DATA(c) as *mut RawFd, fds.len());
        loop {
            let n = libc::sendmsg(s.as_raw_fd(), &msg, 0);
            if n >= 0 {
                return Ok(n as usize);
            }
            let e = std::io::Error::last_os_error();
            if e.kind() != std::io::ErrorKind::Interrupted {
                return Err(e);
            }
        }
    }
}

/// Run `argv` with secrets `names` through the runner; returns the exit code
/// to exit with (128+n for a signal, as a shell reports it).
pub fn client(socket: &Path, names: &[String], argv: &[String], mcp: Option<(&str, &str)>) -> Result<i32, String> {
    let s = UnixStream::connect(socket).map_err(|e| {
        format!(
            "the runner is not reachable at {} ({e}); run this in a tmuxedo pane",
            socket.display()
        )
    })?;
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "/".into());
    let env: serde_json::Map<String, Value> = std::env::vars().map(|(k, v)| (k, Value::String(v))).collect();
    let mut req = json!({
        "names": names, "argv": argv, "cwd": cwd,
        "path": std::env::var("PATH").unwrap_or_default(), "env": env,
    });
    if let Some((p, sv)) = mcp {
        req["mcp"] = json!({"plugin": p, "server": sv});
    }
    let mut line = req.to_string().into_bytes();
    line.push(b'\n');
    // Signals go to the runner, which relays them to the program's group.
    let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
    // SAFETY: sigset manipulation on a local.
    unsafe {
        libc::sigemptyset(&mut set);
        for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT] {
            libc::sigaddset(&mut set, sig);
        }
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
    }
    send_with_fds(&s, &line, &[0, 1, 2]).map_err(|e| format!("talking to the runner: {e}"))?;
    let mut r = BufReader::new(s.try_clone().map_err(|e| e.to_string())?);
    let mut first = String::new();
    r.read_line(&mut first).map_err(|e| e.to_string())?;
    let v: Value = serde_json::from_str(&first).map_err(|_| "the runner closed the connection".to_string())?;
    if let Some(e) = v.get("error") {
        return Err(format!(
            "{}: {}",
            e["code"].as_str().unwrap_or("error"),
            e["message"].as_str().unwrap_or("")
        ));
    }
    let mut w = s;
    std::thread::spawn(move || loop {
        let mut sig: libc::c_int = 0;
        // SAFETY: sigwait on the set blocked above.
        if unsafe { libc::sigwait(&set, &mut sig) } == 0 && w.write_all(format!("signal {sig}\n").as_bytes()).is_err() {
            return;
        }
    });
    let mut last = String::new();
    r.read_line(&mut last).map_err(|e| e.to_string())?;
    let v: Value = serde_json::from_str(&last).map_err(|_| "the runner went away".to_string())?;
    Ok(match (v["exit"].as_i64(), v["signal"].as_i64()) {
        (Some(c), _) => c as i32,
        (None, Some(s)) => 128 + s as i32,
        _ => 1,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn programs_resolve_and_match_by_name_or_path() {
        let sh = resolve_program("sh", "/nonexistent:/bin:/usr/bin", Path::new("/")).unwrap();
        assert!(sh.is_absolute());
        assert!(resolve_program("no-such-program-xyz", "/bin:/usr/bin", Path::new("/")).is_none());
        let name = sh.file_name().unwrap().to_str().unwrap().to_string();
        assert!(program_allowed(&sh, &[name]));
        assert!(program_allowed(&sh, &[sh.display().to_string()]));
        assert!(!program_allowed(&sh, &["gh".into(), "/usr/bin/git".into()]));
        assert!(!program_allowed(&sh, &[]));
    }

    /// A request larger than a small socket buffer (macOS: 8 KiB) arrives in
    /// parts; the runner reads it whole, descriptors and all.
    #[test]
    fn a_large_request_arrives_whole_with_its_descriptors() {
        let (a, b) = UnixStream::pair().unwrap();
        let mut line = json!({"env": {"BIG": "x".repeat(40 * 1024)}}).to_string().into_bytes();
        line.push(b'\n');
        let want = line.len() - 1;
        let sender = std::thread::spawn(move || send_with_fds(&a, &line, &[0, 1, 2]).unwrap());
        let (got, fds) = recv_with_fds(&b).unwrap();
        sender.join().unwrap();
        assert_eq!(fds.len(), 3);
        assert_eq!(got.len(), want);
        let v: Value = serde_json::from_slice(&got).unwrap();
        assert_eq!(v["env"]["BIG"].as_str().unwrap().len(), 40 * 1024);
    }
}
