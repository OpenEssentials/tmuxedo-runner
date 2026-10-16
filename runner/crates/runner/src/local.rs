//! The runner's local unix socket (0600): newline-delimited JSON
//! `{"id","method","params"}` → `{"id","result"|"error"}`, the runner API's
//! API plus `hook.event` for the Claude hook shim. After `events.subscribe`
//! the connection carries event lines until either side closes it.

use crate::api::LocalReply;
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

pub struct LocalReq {
    pub method: String,
    pub params: Value,
    /// The calling process (SO_PEERCRED / LOCAL_PEERPID), when the OS says.
    pub peer: Option<i32>,
    pub reply: oneshot::Sender<LocalReply>,
}

impl LocalReq {
    /// The caller's pid as a positive u32 (the hook ancestry check, crate::ancestry).
    pub fn peer_pid(&self) -> Option<u32> {
        self.peer.and_then(|p| u32::try_from(p).ok()).filter(|&p| p > 0)
    }
}

/// `TMUXEDO_RUNNER_SOCKET_GROUP=<gid>`: the socket and its directory are shared with that
/// group (0750 / 0660). The Mac's separate-user mode uses it so the owner's app can read the
/// dashboard from a runner that runs as the `tmuxedo` user (docs/design/mac-isolation.md §2.1);
/// the group holds only that user and the owner. Unset (everywhere else): owner-only, as before.
pub fn socket_group() -> Option<u32> {
    tmuxedo_holder::env_var("RUNNER_SOCKET_GROUP").and_then(|g| g.trim().parse().ok())
}

fn share_with(path: &Path, gid: u32, mode: u32) -> std::io::Result<()> {
    std::os::unix::fs::chown(path, None, Some(gid))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

pub async fn serve(path: &Path, tx: mpsc::Sender<LocalReq>) -> std::io::Result<JoinHandle<()>> {
    let group = socket_group();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
        match group {
            Some(gid) => share_with(dir, gid, 0o750)?,
            None => std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?,
        }
    }
    // A live runner answers; a dead one leaves a stale file to replace.
    if UnixStream::connect(path).await.is_ok() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AddrInUse,
            "another runner is serving this socket",
        ));
    }
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    match group {
        Some(gid) => share_with(path, gid, 0o660)?,
        None => std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?,
    }
    Ok(tokio::spawn(async move {
        loop {
            let Ok((s, _)) = listener.accept().await else { continue };
            let tx = tx.clone();
            tokio::spawn(handle(s, tx));
        }
    }))
}

fn line(mut v: Value, id: Value) -> Vec<u8> {
    v["id"] = id;
    let mut out = serde_json::to_vec(&v).unwrap_or_default();
    out.push(b'\n');
    out
}

async fn handle(s: UnixStream, tx: mpsc::Sender<LocalReq>) {
    let peer = {
        use std::os::fd::AsRawFd;
        crate::secrets::exec::peer_pid(s.as_raw_fd())
    };
    let (rd, mut wr) = s.into_split();
    let mut lines = BufReader::new(rd).lines();
    while let Ok(Some(text)) = lines.next_line().await {
        if text.trim().is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(&text) else {
            let e = json!({ "error": { "code": "invalid_request", "message": "not JSON" } });
            if wr.write_all(&line(e, Value::Null)).await.is_err() {
                return;
            }
            continue;
        };
        let id = v.get("id").cloned().unwrap_or(Value::Null);
        let Some(method) = v["method"].as_str() else {
            let e = json!({ "error": { "code": "invalid_request", "message": "method is required" } });
            if wr.write_all(&line(e, id)).await.is_err() {
                return;
            }
            continue;
        };
        let (rtx, rrx) = oneshot::channel();
        let req = LocalReq {
            method: method.to_string(),
            params: v.get("params").cloned().unwrap_or(json!({})),
            peer,
            reply: rtx,
        };
        if tx.send(req).await.is_err() {
            return;
        }
        let reply = rrx.await.unwrap_or(LocalReply {
            body: json!({ "error": { "code": "internal_error", "message": "runner dropped the request" } }),
            stream: None,
        });
        if wr.write_all(&line(reply.body, id)).await.is_err() {
            return;
        }
        if let Some(mut stream) = reply.stream {
            // Subscription: forward events until the client goes away.
            loop {
                tokio::select! {
                    ev = stream.recv() => match ev {
                        Some(l) => {
                            let mut b = l.into_bytes();
                            b.push(b'\n');
                            if wr.write_all(&b).await.is_err() {
                                return;
                            }
                        }
                        None => return,
                    },
                    more = lines.next_line() => match more {
                        Ok(Some(_)) => {}
                        _ => return,
                    },
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    /// Owner-only by default; shared with one group (0750 dir, 0660 socket) when asked.
    /// One test, so the environment variable is never read by a parallel one.
    #[tokio::test]
    async fn the_socket_is_owner_only_unless_a_group_is_named() {
        let d = tempfile::tempdir().unwrap();
        let (tx, _rx) = mpsc::channel(1);
        let p = d.path().join("a/runner.sock");
        std::env::remove_var("TMUXEDO_RUNNER_SOCKET_GROUP");
        let t = serve(&p, tx.clone()).await.unwrap();
        assert_eq!(std::fs::metadata(&p).unwrap().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(p.parent().unwrap()).unwrap().mode() & 0o777, 0o700);
        t.abort();

        // Our own effective group: the one group a test can always chown to.
        let gid = unsafe { libc::getegid() };
        std::env::set_var("TMUXEDO_RUNNER_SOCKET_GROUP", gid.to_string());
        assert_eq!(socket_group(), Some(gid));
        let p = d.path().join("b/runner.sock");
        let t = serve(&p, tx).await.unwrap();
        std::env::remove_var("TMUXEDO_RUNNER_SOCKET_GROUP");
        let m = std::fs::metadata(&p).unwrap();
        assert_eq!((m.mode() & 0o777, m.gid()), (0o660, gid));
        let dm = std::fs::metadata(p.parent().unwrap()).unwrap();
        assert_eq!((dm.mode() & 0o777, dm.gid()), (0o750, gid));
        t.abort();
    }
}
