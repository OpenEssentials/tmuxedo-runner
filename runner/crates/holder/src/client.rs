//! Async client for the holder socket (used by the runner).
//!
//! Responses to requests (`ok` / `error`) are routed to the waiting caller;
//! everything else (`attached`, pane data, `exit`) arrives in order on the
//! event channel. `attached` is an event, not a response, so it is always
//! seen before the first data frame of that attachment.

use crate::wire::{self, Event, Msg, Request, SpawnSpec, HOLDER_PROTO};
use serde_json::Value;
use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot};

#[derive(Debug, Clone, PartialEq)]
pub enum HolderEvent {
    Attached {
        id: String,
        from: u64,
        gap: bool,
        end: u64,
    },
    Data {
        id: String,
        offset: u64,
        bytes: Vec<u8>,
    },
    Exit {
        id: String,
        code: Option<i32>,
        signal: Option<String>,
    },
    /// The holder connection is gone.
    Closed,
}

type Waiters = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

#[derive(Clone)]
pub struct HolderClient {
    tx: mpsc::UnboundedSender<Vec<u8>>,
    rid: Arc<AtomicU64>,
    waiters: Waiters,
    pub version: String,
    pub pid: u32,
    /// The holder's code fingerprint (None for a holder older than it).
    pub build: Option<String>,
}

async fn read_one(s: &mut (impl AsyncReadExt + Unpin)) -> io::Result<Msg> {
    let mut len = [0u8; 4];
    s.read_exact(&mut len).await?;
    let len = u32::from_be_bytes(len) as usize;
    if len == 0 || len > wire::MAX_MSG {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "bad length"));
    }
    let mut body = vec![0u8; len];
    s.read_exact(&mut body).await?;
    wire::decode_body(&body)
}

impl HolderClient {
    /// Connect and handshake. `events` capacity bounds how far the runner may
    /// lag the holder before the holder's sender threads block (which in
    /// turn only lets the ring wrap; it never blocks a PTY).
    pub async fn connect(path: &Path, client: &str) -> io::Result<(HolderClient, mpsc::Receiver<HolderEvent>)> {
        let mut s = UnixStream::connect(path).await?;
        s.write_all(&wire::encode_json(&Request::Hello {
            proto: HOLDER_PROTO,
            client: client.into(),
        }))
        .await?;
        let (version, pid, build) = match read_one(&mut s).await? {
            Msg::Json(j) => match serde_json::from_slice::<Event>(&j) {
                Ok(Event::Hello {
                    proto,
                    version,
                    pid,
                    build,
                }) if proto == HOLDER_PROTO => (version, pid, build),
                Ok(Event::Error { message, .. }) => return Err(io::Error::new(io::ErrorKind::Unsupported, message)),
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "holder speaks a different protocol; restart the holder",
                    ))
                }
            },
            Msg::Data { .. } => return Err(io::Error::other("unexpected data before hello")),
        };
        let (mut rd, mut wr) = s.into_split();
        let (tx, mut wrx) = mpsc::unbounded_channel::<Vec<u8>>();
        tokio::spawn(async move {
            while let Some(b) = wrx.recv().await {
                if wr.write_all(&b).await.is_err() {
                    break;
                }
            }
        });
        let waiters: Waiters = Arc::new(Mutex::new(HashMap::new()));
        let (etx, erx) = mpsc::channel::<HolderEvent>(1024);
        {
            let waiters = waiters.clone();
            tokio::spawn(async move {
                loop {
                    let msg = match read_one(&mut rd).await {
                        Ok(m) => m,
                        Err(_) => break,
                    };
                    let ev = match msg {
                        Msg::Data { id, offset, bytes } => HolderEvent::Data { id, offset, bytes },
                        Msg::Json(j) => match serde_json::from_slice::<Event>(&j) {
                            Ok(Event::Ok { rid, data }) => {
                                if let Some(w) = waiters.lock().unwrap().remove(&rid) {
                                    let _ = w.send(Ok(data));
                                }
                                continue;
                            }
                            Ok(Event::Error { rid, message }) => {
                                if let Some(w) = waiters.lock().unwrap().remove(&rid) {
                                    let _ = w.send(Err(message));
                                }
                                continue;
                            }
                            Ok(Event::Attached {
                                rid,
                                id,
                                from,
                                gap,
                                end,
                            }) => {
                                if let Some(w) = waiters.lock().unwrap().remove(&rid) {
                                    let _ = w.send(Ok(serde_json::json!({"from": from, "gap": gap, "end": end})));
                                }
                                HolderEvent::Attached { id, from, gap, end }
                            }
                            Ok(Event::Exit { id, info }) => HolderEvent::Exit {
                                id,
                                code: info.code,
                                signal: info.signal,
                            },
                            _ => continue,
                        },
                    };
                    if etx.send(ev).await.is_err() {
                        break;
                    }
                }
                // Fail every waiter, then report the loss.
                for (_, w) in waiters.lock().unwrap().drain() {
                    let _ = w.send(Err("holder connection closed".into()));
                }
                let _ = etx.send(HolderEvent::Closed).await;
            });
        }
        Ok((
            HolderClient {
                tx,
                rid: Arc::new(AtomicU64::new(1)),
                waiters,
                version,
                pid,
                build,
            },
            erx,
        ))
    }

    fn next_rid(&self) -> u64 {
        self.rid.fetch_add(1, Ordering::Relaxed)
    }

    fn send_json(&self, req: &Request) {
        let _ = self.tx.send(wire::encode_json(req));
    }

    /// Send a request that has a response; the future resolves when it arrives.
    fn call(&self, make: impl FnOnce(u64) -> Request) -> impl std::future::Future<Output = Result<Value, String>> {
        let rid = self.next_rid();
        let (tx, rx) = oneshot::channel();
        self.waiters.lock().unwrap().insert(rid, tx);
        self.send_json(&make(rid));
        async move { rx.await.unwrap_or_else(|_| Err("holder connection closed".into())) }
    }

    pub fn spawn(&self, spec: SpawnSpec) -> impl std::future::Future<Output = Result<Value, String>> {
        self.call(|rid| Request::Spawn { rid, spec })
    }

    /// A pipe child (wire `spawn_pipe`): answers `{pid, pipe: true}`; an
    /// older holder never answers (callers time out).
    pub fn spawn_pipe(
        &self,
        spec: SpawnSpec,
        stderr: Option<String>,
    ) -> impl std::future::Future<Output = Result<Value, String>> {
        self.call(|rid| Request::SpawnPipe { rid, spec, stderr })
    }

    pub fn list(&self) -> impl std::future::Future<Output = Result<Value, String>> {
        self.call(|rid| Request::List { rid })
    }

    /// Ask for pane `id` from `from`. The [`HolderEvent::Attached`] event
    /// (and the returned future) report where streaming actually starts.
    pub fn attach(&self, id: &str, from: u64) -> impl std::future::Future<Output = Result<Value, String>> {
        let id = id.to_owned();
        self.call(move |rid| Request::Attach { rid, id, from })
    }

    pub fn kill(&self, id: &str, signal: i32) -> impl std::future::Future<Output = Result<Value, String>> {
        let id = id.to_owned();
        self.call(move |rid| Request::Kill { rid, id, signal })
    }

    pub fn remove(&self, id: &str) -> impl std::future::Future<Output = Result<Value, String>> {
        let id = id.to_owned();
        self.call(move |rid| Request::Remove { rid, id })
    }

    /// Fire-and-forget attach: the outcome arrives as [`HolderEvent::Attached`].
    pub fn attach_nowait(&self, id: &str, from: u64) {
        let rid = self.next_rid();
        self.send_json(&Request::Attach {
            rid,
            id: id.into(),
            from,
        });
    }

    /// Fire-and-forget signal to a pane's process group.
    pub fn kill_nowait(&self, id: &str, signal: i32) {
        let rid = self.next_rid();
        self.send_json(&Request::Kill {
            rid,
            id: id.into(),
            signal,
        });
    }

    pub fn detach(&self, id: &str) {
        self.send_json(&Request::Detach { id: id.into() });
    }

    pub fn resize(&self, id: &str, cols: u16, rows: u16) {
        self.send_json(&Request::Resize {
            id: id.into(),
            cols,
            rows,
        });
    }

    pub fn input(&self, id: &str, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        // Keep messages well under MAX_MSG.
        for chunk in bytes.chunks(256 * 1024) {
            let _ = self.tx.send(wire::encode_data(id, 0, chunk));
        }
    }
}
