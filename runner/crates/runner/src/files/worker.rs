//! The files worker: one thread that owns the [`Store`], so disk work (a
//! chunk's pwrite and fdatasync, hashing a whole upload, a sweep) never
//! runs on the daemon's loop where terminals are served.
//!
//! Upload bytes reach it straight from the gateway connection's reader
//! through a BOUNDED channel ([`CHANNEL`] messages of at most 1 MiB): when
//! the disk is slower than the network the reader waits, the socket stops
//! being read, and the gateway sees its runner socket's buffer grow and
//! pauses the HTTP upload (backpressure end to end, nothing buffered whole).
//! API calls come from the daemon (which never waits: it spawns the send).
//! Results go back to the daemon as [`Out`], in order, so a download's
//! FILE frames always precede its `res`.

use super::store::{Note, Store};
use crate::api::{err, ApiResult, ReplyTo};
use serde_json::{json, Value};
use std::time::{Duration, Instant};
use tmuxedo_protocol::files::{disk_level, DiskFrame, FileCtl, FileFrame};
use tokio::sync::mpsc;

/// Messages in flight to the worker at most (upload frames are ≤ 1 MiB).
pub const CHANNEL: usize = 32;

pub enum Msg {
    Ctl(FileCtl),
    Data(FileFrame),
    Call {
        method: String,
        params: Value,
        reply: ReplyTo,
    },
    /// Every few seconds from the daemon: the disk check (every 60 s) and the sweeps.
    Tick,
    /// A new gateway connection: report the disk at once.
    DiskNow,
}

pub enum Out {
    /// A text frame for the gateway (`file_ack`, `disk`).
    Text(String),
    /// FILE frames (a download window), then the answer.
    Reply {
        reply: ReplyTo,
        result: ApiResult,
        frames: Vec<Vec<u8>>,
    },
    /// A durable event (`file_uploaded`, `file_shared`, `file_requested`, `disk_low`).
    Event(Note),
    /// The disk level changed (the daemon stops pane history at `critical`).
    Level(&'static str),
}

/// Free space (percent) above which a disk problem counts as over (the hysteresis of `disk_low`;
/// the server clears its open problem at the same line, docs/protocol.md "Files").
pub const DISK_REARM_PERCENT: u64 = 8;

pub struct Worker {
    pub store: Store,
    last_disk: Option<(u64, u64, u64, &'static str)>,
    last_disk_check: Option<Instant>,
    /// `disk_low` is raised once per problem: disarmed when raised, re-armed only after the
    /// disk has had more than DISK_REARM_PERCENT free (a disk hovering at 5 % stays one problem).
    disk_armed: bool,
    last_partial_sweep: Instant,
    last_file_sweep: Option<Instant>,
    started: Instant,
    pub disk_every: Duration,
}

impl Worker {
    pub fn new(store: Store) -> Self {
        Worker {
            store,
            last_disk: None,
            last_disk_check: None,
            disk_armed: true,
            last_partial_sweep: Instant::now(),
            last_file_sweep: None,
            started: Instant::now(),
            disk_every: Duration::from_secs(60),
        }
    }

    /// Handle one message; everything it produces goes to `out`.
    pub fn handle(&mut self, m: Msg, out: &mut dyn FnMut(Out)) {
        match m {
            Msg::Ctl(FileCtl::Put {
                upload_id,
                n,
                size,
                sha256,
            }) => {
                if let Some(ack) = self.store.put_begin(&upload_id, n, size, &sha256) {
                    out(Out::Text(ack.to_json()));
                }
            }
            Msg::Ctl(FileCtl::Cancel { upload_id, n }) => self.store.put_cancel(&upload_id, n),
            Msg::Data(f) => {
                if let Some(ack) = self.store.put_data(&f) {
                    let ok = ack.ok;
                    out(Out::Text(ack.to_json()));
                    // An upload that gave its whole-file hash at the start lands with its last chunk.
                    if ok {
                        self.store.auto_complete(&f.id);
                    }
                }
            }
            Msg::Call { method, params, reply } => {
                let (result, frames) = self.call(&method, &params);
                out(Out::Reply { reply, result, frames });
                // The person just looked (or tried): a changed disk level shows at once, not in a minute.
                if matches!(method.as_str(), "files.quota" | "files.upload_init") {
                    let (free, total) = self.store.disk();
                    if self.last_disk.is_some_and(|d| d.3 != disk_level(free, total)) {
                        self.tick(out, true);
                    }
                }
            }
            Msg::Tick => self.tick(out, false),
            Msg::DiskNow => self.tick(out, true),
        }
        for n in self.store.notes.drain(..) {
            out(Out::Event(n));
        }
    }

    fn call(&mut self, method: &str, p: &Value) -> (ApiResult, Vec<Vec<u8>>) {
        let s = &mut self.store;
        let r = match method {
            "files.list" => s.list(p),
            "files.stat" => s.stat(p),
            "files.delete" => s.delete(p),
            "files.pin" => s.pin(p),
            "files.quota" => s.quota(),
            "files.share" => s.share(p),
            "files.request" => s.request(p),
            "files.upload_init" => s.upload_init(p),
            "files.upload_status" => s.upload_status(p),
            "files.upload_complete" => s.upload_complete(p),
            "files.upload_abort" => s.upload_abort(p),
            "files.read" => {
                return match s.read(p) {
                    Ok((v, frames)) => (Ok(v), frames),
                    Err(e) => (Err(e), vec![]),
                }
            }
            m => Err(err("unknown_method", format!("unknown method: {m}"))),
        };
        (r, vec![])
    }

    fn tick(&mut self, out: &mut dyn FnMut(Out), force: bool) {
        let now = Instant::now();
        if force
            || self
                .last_disk_check
                .is_none_or(|t| now.duration_since(t) >= self.disk_every)
        {
            self.last_disk_check = Some(now);
            let (free, total) = self.store.disk();
            let files = self.store.usage();
            let level = disk_level(free, total);
            let prev = self.last_disk;
            let moved = prev.is_none_or(|(f, t, u, l)| {
                l != level
                    || t != total
                    || f.abs_diff(free) > total / 100
                    || u.abs_diff(files) > (total / 100).max(1024 * 1024)
            });
            if force || moved {
                out(Out::Text(DiskFrame::new(free, total, files).to_json()));
            }
            if prev.is_none_or(|p| p.3 != level) {
                out(Out::Level(level));
            }
            if total > 0 && free.saturating_mul(100) > total.saturating_mul(DISK_REARM_PERCENT) {
                self.disk_armed = true;
            }
            if (level == "critical" || level == "alert") && self.disk_armed {
                self.disk_armed = false;
                crate::log!("disk almost full: {free} of {total} bytes free ({level})");
                out(Out::Event(Note {
                    pane_id: String::new(),
                    kind: "disk_low",
                    data: json!({ "freeBytes": free, "totalBytes": total, "filesBytes": files, "level": level }),
                }));
            }
            self.last_disk = Some((free, total, files, level));
        }
        // Partials hourly; inbox/outbox retention daily, the first five minutes after start.
        let files_due = match self.last_file_sweep {
            None => now.duration_since(self.started) >= Duration::from_secs(300),
            Some(t) => now.duration_since(t) >= Duration::from_secs(24 * 3600),
        };
        if files_due || now.duration_since(self.last_partial_sweep) >= Duration::from_secs(3600) {
            let (p, f) = self.store.sweep(crate::util::now_ms(), files_due);
            self.last_partial_sweep = now;
            if files_due {
                self.last_file_sweep = Some(now);
            }
            if p + f > 0 {
                crate::log!("files sweep: {p} expired uploads and {f} old files removed");
            }
        }
    }
}

/// Run the worker on its own thread until every sender is gone.
pub fn spawn(
    store: Store,
    mut rx: mpsc::Receiver<Msg>,
    mut out: impl FnMut(Out) + Send + 'static,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("tmuxedo-files".into())
        .spawn(move || {
            let mut w = Worker::new(store);
            while let Some(m) = rx.blocking_recv() {
                w.handle(m, &mut out);
            }
        })
        .expect("spawn the files thread")
}
