//! Every files operation, synchronous and confined to the root (see mod.rs).
//! The worker thread owns one [`Store`]; tests drive it directly.
//!
//! UPLOADS. `upload_init` checks the size, the quota and the free-space
//! floor, and makes `.partial/<id>/` with a sparse `data` file of the full
//! size and `state.json`. Each chunk arrives as `put_begin` (size and
//! SHA-256) and FILE frames (`put_data`), written with `pwrite` at the
//! chunk's offset while a SHA-256 runs over them; the last byte checks the
//! hash, `fdatasync`s and records the chunk in `state.json` before the ack,
//! so a runner restart (or a crash) resumes from the chunks on disk. Chunks
//! may arrive in any order and be sent again. `upload_complete` checks every
//! chunk is there, hashes the whole file, compares, renames it into the
//! inbox (read-only, never executable), writes the provenance sidecar and
//! remembers the answer (asking twice answers twice). A wrong whole-file
//! hash discards the upload.

use super::{set_mode, statvfs, valid_project_key, Limits};
use crate::api::{err, ApiErr};
use crate::jail;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{Read, Write};
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tmuxedo_protocol::files::{
    chunk_count, chunk_len, disk_level, sanitize_name, upload_stamp, valid_sha256, valid_transfer_id,
    valid_upload_shape, FileAck, FileFrame, FRAME_PAYLOAD, READ_WINDOW,
};

pub type R = Result<Value, ApiErr>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Done {
    pub path: String,
    pub name: String,
    pub project: String,
    pub sha256: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UploadState {
    pub v: u32,
    pub upload_id: String,
    pub project: String,
    /// Sanitised (files.ts sanitizeFileName rules).
    pub name: String,
    pub original_name: String,
    pub size: u64,
    pub chunk_size: u64,
    pub chunk_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunk_hashes: Option<Vec<String>>,
    /// Chunk number → its SHA-256, for every chunk on disk.
    pub received: BTreeMap<u64, String>,
    pub by: String,
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
    /// The file request it answers (`tmuxedo files request`; the needs-you item closes when it lands).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    /// An agent chat it was sent to (the composer's 📎): its `file_uploaded` says so, for "insert path".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// The whole file's SHA-256, given at the start (the phone's background upload): the
    /// upload completes itself when its last chunk arrives, with no client call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_sha256: Option<String>,
    /// An admin's "Upload anyway" (web's `override_space`): past the floor and the quota,
    /// never past the hard margin (`Limits::hard_margin_for`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub override_space: bool,
    pub created_ms: u64,
    pub last_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub done: Option<Done>,
}

impl UploadState {
    fn received_bytes(&self) -> u64 {
        self.received
            .keys()
            .filter_map(|n| chunk_len(self.size, self.chunk_size, *n))
            .sum()
    }
}

struct ChunkRx {
    size: u64,
    sha256: String,
    got: u64,
    hasher: Sha256,
}

struct Upload {
    st: UploadState,
    file: Option<std::fs::File>,
    rx: HashMap<u32, ChunkRx>,
}

/// What an operation wants told elsewhere: a durable event for web.
#[derive(Debug, Clone, PartialEq)]
pub struct Note {
    pub pane_id: String,
    pub kind: &'static str,
    pub data: Value,
}

pub struct Store {
    pub root: PathBuf,
    pub limits: Limits,
    uploads: HashMap<String, Upload>,
    usage: Option<(Instant, u64)>,
    /// Durable events to send (drained by the worker).
    pub notes: Vec<Note>,
}

fn now_ms() -> u64 {
    crate::util::now_ms()
}

fn io(e: std::io::Error) -> ApiErr {
    if e.raw_os_error() == Some(libc::ENOSPC) || e.raw_os_error() == Some(libc::EDQUOT) {
        return err("insufficient_storage", "The machine's disk is full.");
    }
    err("io_error", e.to_string())
}

/// A file name the API may name: no separators, not hidden, not `.`/`..`.
pub fn valid_entry_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 255 && !n.starts_with('.') && !n.contains(['/', '\\', '\0'])
}

fn dir_param(p: &Value) -> Result<&'static str, ApiErr> {
    match p["dir"].as_str().unwrap_or("inbox") {
        "inbox" => Ok("inbox"),
        "outbox" => Ok("outbox"),
        _ => Err(err("invalid_params", "dir must be inbox or outbox")),
    }
}

/// Is `p` a real directory (not a symlink to one)?
fn real_dir(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok_and(|m| m.is_dir())
}

impl Store {
    pub fn new(root: PathBuf, limits: Limits) -> Self {
        Store {
            root,
            limits,
            uploads: HashMap::new(),
            usage: None,
            notes: vec![],
        }
    }

    fn partial_dir(&self, id: &str) -> PathBuf {
        self.root.join(".partial").join(id)
    }

    /// `<root>/<project>/<dir>`, refusing a symlinked project or folder.
    fn folder(&self, project: &str, dir: &str, create: bool) -> Result<PathBuf, ApiErr> {
        if !valid_project_key(project) {
            return Err(err("invalid_params", "not a project of this machine's files"));
        }
        let pdir = self.root.join(project);
        let d = pdir.join(dir);
        if create {
            for x in [&pdir, &d] {
                if !x.exists() {
                    std::fs::create_dir(x)
                        .or_else(|e| if x.is_dir() { Ok(()) } else { Err(e) })
                        .map_err(io)?;
                    set_mode(x, 0o700);
                }
            }
        }
        for x in [&pdir, &d] {
            if x.exists() && !real_dir(x) {
                return Err(err("file_refused", "That folder is a link; files never follow links."));
            }
        }
        Ok(d)
    }

    /// The project folder for a cwd, with inbox and outbox made (pane start).
    pub fn ensure_project(&self, project: &str) -> Option<PathBuf> {
        self.folder(project, "inbox", true).ok()?;
        self.folder(project, "outbox", true).ok()?;
        Some(self.root.join(project))
    }

    pub fn disk(&self) -> (u64, u64) {
        self.limits
            .fake()
            .or_else(|| statvfs(&self.root))
            .unwrap_or((u64::MAX / 4, u64::MAX / 2))
    }

    /// Bytes under the root: inbox and outbox files, plus the declared size of
    /// every upload in progress (cached 10 s; changes invalidate it).
    pub fn usage(&mut self) -> u64 {
        if let Some((at, n)) = self.usage {
            if at.elapsed() < Duration::from_secs(10) {
                return n;
            }
        }
        let mut n = 0u64;
        if let Ok(rd) = std::fs::read_dir(&self.root) {
            for p in rd.flatten() {
                let path = p.path();
                if !real_dir(&path) {
                    continue;
                }
                if p.file_name() == ".partial" {
                    for u in std::fs::read_dir(&path).into_iter().flatten().flatten() {
                        if let Some(st) = read_state(&u.path()) {
                            if st.done.is_none() {
                                n += st.size;
                            }
                        }
                    }
                    continue;
                }
                for dir in ["inbox", "outbox"] {
                    for f in std::fs::read_dir(path.join(dir)).into_iter().flatten().flatten() {
                        if let Ok(m) = f.path().symlink_metadata() {
                            if m.is_file() {
                                n += m.len();
                            }
                        }
                    }
                }
            }
        }
        self.usage = Some((Instant::now(), n));
        n
    }

    fn changed(&mut self) {
        self.usage = None;
    }

    /// `files.quota`.
    pub fn quota(&mut self) -> R {
        let used = self.usage();
        let (free, total) = self.disk();
        let quota = self.limits.quota_for(total);
        let floor = self.limits.floor_for(total);
        Ok(json!({
            "type": "files_quota", "usedBytes": used, "quotaBytes": quota, "maxFileBytes": self.limits.max_file,
            "freeBytes": free, "totalBytes": total, "floorBytes": floor, "level": disk_level(free, total),
            "uploadsAllowed": free > floor && used < quota, "root": self.root.display().to_string(),
        }))
    }

    /// Would `extra` more bytes fit (quota and floor)? The refusal says which.
    /// `override_space` (an admin's "Upload anyway") checks only the hard margin.
    fn room_for(&mut self, extra: u64, already_counted: u64, override_space: bool) -> Result<(), ApiErr> {
        let (free, total) = self.disk();
        if override_space {
            let margin = self.limits.hard_margin_for(total);
            if free.saturating_sub(extra) < margin.max(1) || free < extra {
                return Err(err(
                    "insufficient_storage",
                    format!(
                        "Not enough space on the machine even to upload anyway: {} free, and at least {} must stay free for the machine to keep working.",
                        human(free),
                        human(margin)
                    ),
                ));
            }
            return Ok(());
        }
        let floor = self.limits.floor_for(total);
        if free.saturating_sub(extra) < floor {
            return Err(err(
                "insufficient_storage",
                format!(
                    "Not enough space on the machine: {} free, and at least {} must stay free.",
                    human(free),
                    human(floor)
                ),
            ));
        }
        let used = self.usage().saturating_sub(already_counted);
        let quota = self.limits.quota_for(total);
        if used + extra > quota {
            return Err(err(
                "quota_exceeded",
                format!(
                    "Files on this machine would use {} of {} allowed. Delete some in Files first.",
                    human(used + extra),
                    human(quota)
                ),
            ));
        }
        Ok(())
    }

    // ── uploads ───────────────────────────────────────────────────────

    fn load(&mut self, id: &str) -> Option<&mut Upload> {
        if !valid_transfer_id(id) {
            return None;
        }
        if !self.uploads.contains_key(id) {
            let st = read_state(&self.partial_dir(id))?;
            if st.upload_id != id {
                return None;
            }
            self.uploads.insert(
                id.to_string(),
                Upload {
                    st,
                    file: None,
                    rx: HashMap::new(),
                },
            );
        }
        self.uploads.get_mut(id)
    }

    fn save(&self, st: &UploadState) -> Result<(), ApiErr> {
        let dir = self.partial_dir(&st.upload_id);
        let tmp = dir.join("state.json.tmp");
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&tmp)
            .map_err(io)?;
        f.write_all(&serde_json::to_vec(st).expect("serialisable"))
            .map_err(io)?;
        f.sync_data().map_err(io)?;
        std::fs::rename(&tmp, dir.join("state.json")).map_err(io)
    }

    /// `files.upload_init {upload_id, project, name, size, chunk_size, chunk_hashes?, by, source, pane_id?}`.
    pub fn upload_init(&mut self, p: &Value) -> R {
        let id = p["upload_id"]
            .as_str()
            .filter(|s| valid_transfer_id(s))
            .ok_or_else(|| err("invalid_params", "upload_id is required"))?;
        let project = p["project"]
            .as_str()
            .filter(|s| valid_project_key(s))
            .ok_or_else(|| err("invalid_params", "project is required"))?;
        let size = p["size"]
            .as_u64()
            .ok_or_else(|| err("invalid_params", "size is required"))?;
        let cs = p["chunk_size"]
            .as_u64()
            .ok_or_else(|| err("invalid_params", "chunk_size is required"))?;
        if size > self.limits.max_file {
            return Err(err(
                "file_too_large",
                format!(
                    "Files up to {} can be uploaded to this machine.",
                    human(self.limits.max_file)
                ),
            ));
        }
        if !valid_upload_shape(size, cs, self.limits.max_file) {
            return Err(err("invalid_params", "size and chunk_size are not a valid upload"));
        }
        let count = chunk_count(size, cs);
        let hashes = match p.get("chunk_hashes").filter(|v| !v.is_null()) {
            None => None,
            Some(v) => {
                let list: Vec<String> = v
                    .as_array()
                    .ok_or_else(|| err("invalid_params", "chunk_hashes must be a list"))?
                    .iter()
                    .map(|h| h.as_str().filter(|s| valid_sha256(s)).map(str::to_owned))
                    .collect::<Option<_>>()
                    .ok_or_else(|| err("invalid_params", "chunk_hashes must be SHA-256 hex"))?;
                if list.len() as u64 != count {
                    return Err(err("invalid_params", "chunk_hashes must have one hash per chunk"));
                }
                Some(list)
            }
        };
        let raw_name = p["name"].as_str().unwrap_or("");
        let name = sanitize_name(raw_name).unwrap_or_else(|| "file".into());
        // The same init again (a retried request): the existing upload.
        if let Some(u) = self.load(id) {
            let st = &u.st;
            if st.size != size || st.chunk_size != cs || st.project != project {
                return Err(err("upload_conflict", "That upload id is already in use."));
            }
            return Ok(self.init_answer(id));
        }
        let override_space = p["override_space"].as_bool() == Some(true);
        self.room_for(size, 0, override_space)?;
        let dir = self.partial_dir(id);
        let parent = self.root.join(".partial");
        if !parent.exists() {
            std::fs::create_dir(&parent).map_err(io)?;
            set_mode(&parent, 0o700);
        }
        if !real_dir(&parent) {
            return Err(err("file_refused", "the uploads folder is a link"));
        }
        std::fs::create_dir(&dir).map_err(io)?;
        set_mode(&dir, 0o700);
        let data = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(dir.join("data"))
            .map_err(io)?;
        data.set_len(size).map_err(io)?;
        let now = now_ms();
        let st = UploadState {
            v: 1,
            upload_id: id.into(),
            project: project.into(),
            name,
            original_name: raw_name.chars().take(512).collect(),
            size,
            chunk_size: cs,
            chunk_count: count,
            chunk_hashes: hashes,
            received: BTreeMap::new(),
            by: p["by"].as_str().unwrap_or("").chars().take(128).collect(),
            source: match p["source"].as_str() {
                Some(s @ ("web" | "app" | "cli")) => s.into(),
                _ => "web".into(),
            },
            pane_id: p["pane_id"].as_str().map(|s| s.chars().take(255).collect()),
            request_id: p["request_id"]
                .as_str()
                .filter(|s| tmuxedo_protocol::needs::valid_file_request_id(s))
                .map(str::to_owned),
            session_id: p["session_id"]
                .as_str()
                .filter(|s| {
                    (8..=64).contains(&s.len())
                        && s.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
                })
                .map(str::to_owned),
            file_sha256: p["sha256"].as_str().filter(|s| valid_sha256(s)).map(str::to_owned),
            override_space,
            created_ms: now,
            last_ms: now,
            done: None,
        };
        if let Err(e) = self.save(&st) {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(e);
        }
        self.uploads.insert(
            id.into(),
            Upload {
                st,
                file: Some(data),
                rx: HashMap::new(),
            },
        );
        self.changed();
        Ok(self.init_answer(id))
    }

    fn init_answer(&self, id: &str) -> Value {
        let st = &self.uploads[id].st;
        json!({
            "type": "upload_init", "uploadId": id, "project": st.project, "name": st.name,
            "size": st.size, "chunkSize": st.chunk_size, "chunkCount": st.chunk_count,
            "received": st.received.keys().collect::<Vec<_>>(),
            "expiresAt": st.last_ms + self.limits.partial_ttl_ms,
        })
    }

    /// `file_put`: chunk `n` starts. Some(ack) when it is refused at once.
    pub fn put_begin(&mut self, id: &str, n: u32, size: u64, sha256: &str) -> Option<FileAck> {
        let ttl = self.limits.partial_ttl_ms;
        let Some(u) = self.load(id) else {
            return Some(FileAck::err(
                id,
                n,
                "upload_not_found",
                "That upload is not on the machine (it expired or was cancelled).",
            ));
        };
        if u.st.done.is_some() {
            return Some(FileAck::err(
                id,
                n,
                "upload_complete",
                "That upload is already complete.",
            ));
        }
        if u.st.last_ms + ttl < now_ms() {
            return Some(FileAck::err(id, n, "upload_not_found", "That upload expired."));
        }
        let want = chunk_len(u.st.size, u.st.chunk_size, n as u64);
        if want != Some(size) {
            return Some(FileAck::err(
                id,
                n,
                "invalid_chunk",
                "That chunk number or size does not belong to this upload.",
            ));
        }
        if let Some(h) = u.st.chunk_hashes.as_ref().and_then(|l| l.get(n as usize)) {
            if h != sha256 {
                return Some(FileAck::err(
                    id,
                    n,
                    "chunk_hash_mismatch",
                    "That chunk's SHA-256 is not the one declared at the start.",
                ));
            }
        }
        // The floor, again, for what is still to come (the disk may have filled since).
        let remaining = u.st.size.saturating_sub(u.st.received_bytes());
        let override_space = u.st.override_space;
        let (free, total) = self.disk();
        // An admin's "Upload anyway": only the hard margin, else half the floor.
        let keep = if override_space {
            self.limits.hard_margin_for(total).max(1)
        } else {
            self.limits.floor_for(total) / 2
        };
        // The data file is sparse: what is still to come takes space as it arrives.
        if free.saturating_sub(remaining) < keep || free < size {
            let _ = self.abort_inner(id);
            return Some(FileAck::err(
                id,
                n,
                "insufficient_storage",
                "The machine's disk filled up; the upload was stopped.",
            ));
        }
        let u = self.uploads.get_mut(id).expect("loaded");
        u.rx.insert(
            n,
            ChunkRx {
                size,
                sha256: sha256.to_string(),
                got: 0,
                hasher: Sha256::new(),
            },
        );
        None
    }

    pub fn put_cancel(&mut self, id: &str, n: u32) {
        if let Some(u) = self.uploads.get_mut(id) {
            u.rx.remove(&n);
        }
    }

    /// One FILE frame of an upload chunk. Some(ack) when the chunk finished or failed.
    pub fn put_data(&mut self, f: &FileFrame) -> Option<FileAck> {
        let (id, n) = (f.id.as_str(), f.req);
        let dir = self.partial_dir(id);
        let u = self.uploads.get_mut(id)?;
        let rx = u.rx.get_mut(&n)?;
        let base = n as u64 * u.st.chunk_size;
        if f.offset != base + rx.got || rx.got + f.payload.len() as u64 > rx.size {
            u.rx.remove(&n);
            return Some(FileAck::err(
                id,
                n,
                "chunk_out_of_order",
                "Chunk bytes arrived out of order; send the chunk again.",
            ));
        }
        if u.file.is_none() {
            match std::fs::OpenOptions::new()
                .write(true)
                .read(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(dir.join("data"))
            {
                Ok(file) => u.file = Some(file),
                Err(e) => {
                    u.rx.remove(&n);
                    let e = io(e);
                    return Some(FileAck::err(id, n, &e.code, e.message));
                }
            }
        }
        let file = u.file.as_ref().expect("opened");
        if let Err(e) = file.write_all_at(&f.payload, f.offset) {
            u.rx.remove(&n);
            let e = io(e);
            return Some(FileAck::err(id, n, &e.code, e.message));
        }
        rx.hasher.update(&f.payload);
        rx.got += f.payload.len() as u64;
        if rx.got < rx.size {
            return None;
        }
        let rx = u.rx.remove(&n).expect("present");
        let got = hex::encode(rx.hasher.finalize());
        if got != rx.sha256 {
            return Some(FileAck::err(
                id,
                n,
                "chunk_hash_mismatch",
                format!("Chunk {n} arrived damaged (SHA-256 {got}); send it again."),
            ));
        }
        if let Err(e) = file.sync_data() {
            let e = io(e);
            return Some(FileAck::err(id, n, &e.code, e.message));
        }
        u.st.received.insert(n as u64, got.clone());
        u.st.last_ms = now_ms();
        let st = u.st.clone();
        if let Err(e) = self.save(&st) {
            if let Some(u) = self.uploads.get_mut(id) {
                u.st.received.remove(&(n as u64));
            }
            return Some(FileAck::err(id, n, &e.code, e.message));
        }
        Some(FileAck::ok(id, n, &got))
    }

    /// `files.upload_status {upload_id}`.
    pub fn upload_status(&mut self, p: &Value) -> R {
        let id = p["upload_id"].as_str().unwrap_or("");
        let ttl = self.limits.partial_ttl_ms;
        let u = self
            .load(id)
            .ok_or_else(|| err("upload_not_found", "That upload is not on the machine."))?;
        Ok(json!({
            "type": "upload_status", "uploadId": id, "project": u.st.project, "name": u.st.name,
            "size": u.st.size, "chunkSize": u.st.chunk_size, "chunkCount": u.st.chunk_count,
            "received": u.st.received.keys().collect::<Vec<_>>(), "expiresAt": u.st.last_ms + ttl,
            "done": u.st.done,
        }))
    }

    /// `files.upload_abort {upload_id}`: always succeeds (nothing to probe).
    pub fn upload_abort(&mut self, p: &Value) -> R {
        let id = p["upload_id"].as_str().unwrap_or("");
        let _ = self.abort_inner(id);
        Ok(json!({ "type": "upload_aborted" }))
    }

    fn abort_inner(&mut self, id: &str) -> std::io::Result<()> {
        if !valid_transfer_id(id) {
            return Ok(());
        }
        self.uploads.remove(id);
        self.changed();
        let d = self.partial_dir(id);
        if real_dir(&d) {
            std::fs::remove_dir_all(d)?;
        }
        Ok(())
    }

    /// `files.upload_complete {upload_id, sha256?}`.
    /// After a chunk landed: an upload that declared its whole-file SHA-256 at
    /// the start completes itself once every chunk is here (the phone's
    /// background upload has no JS left to call `complete`). A mismatch
    /// aborts it, as a client's complete would. → whether it completed now.
    pub fn auto_complete(&mut self, id: &str) -> bool {
        let Some(u) = self.load(id) else { return false };
        let st = &u.st;
        let Some(sha) = st.file_sha256.clone() else {
            return false;
        };
        if st.done.is_some() || (0..st.chunk_count).any(|n| !st.received.contains_key(&n)) {
            return false;
        }
        match self.upload_complete(&json!({ "upload_id": id, "sha256": sha })) {
            Ok(_) => true,
            Err(e) => {
                crate::log!("files: upload {id} did not complete by itself: {}", e.message);
                false
            }
        }
    }

    pub fn upload_complete(&mut self, p: &Value) -> R {
        let id = p["upload_id"].as_str().unwrap_or("").to_string();
        let want = p["sha256"].as_str().map(str::to_owned);
        if want.as_deref().is_some_and(|w| !valid_sha256(w)) {
            return Err(err("invalid_params", "sha256 must be 64 lowercase hex characters"));
        }
        let dir = self.partial_dir(&id);
        let u = self
            .load(&id)
            .ok_or_else(|| err("upload_not_found", "That upload is not on the machine."))?;
        if let Some(d) = &u.st.done {
            return Ok(done_answer(d));
        }
        let missing: Vec<u64> = (0..u.st.chunk_count)
            .filter(|n| !u.st.received.contains_key(n))
            .collect();
        if !missing.is_empty() {
            return Err(err(
                "incomplete",
                format!(
                    "{} of {} chunks are still missing (first: {}).",
                    missing.len(),
                    u.st.chunk_count,
                    missing[0]
                ),
            ));
        }
        let declared_differ = u.st.chunk_hashes.as_ref().is_some_and(|list| {
            list.iter()
                .enumerate()
                .any(|(i, h)| u.st.received.get(&(i as u64)) != Some(h))
        });
        if declared_differ {
            let _ = self.abort_inner(&id);
            return Err(err(
                "file_hash_mismatch",
                "The chunks do not match the hashes declared at the start; upload it again.",
            ));
        }
        // The whole file, hashed from disk.
        let data_path = dir.join("data");
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&data_path)
            .map_err(io)?;
        let mut h = Sha256::new();
        let mut buf = vec![0u8; 1024 * 1024];
        loop {
            let k = f.read(&mut buf).map_err(io)?;
            if k == 0 {
                break;
            }
            h.update(&buf[..k]);
        }
        let sha = hex::encode(h.finalize());
        if want.as_deref().is_some_and(|w| w != sha) {
            let _ = self.abort_inner(&id);
            return Err(err(
                "file_hash_mismatch",
                format!("The file arrived as SHA-256 {sha}, not the one your device computed; upload it again."),
            ));
        }
        let st = self.uploads[&id].st.clone();
        let inbox = self.folder(&st.project, "inbox", true)?;
        let stamp = upload_stamp(now_ms());
        let (final_name, final_path) = free_name(&inbox, &format!("{stamp}_{}", st.name));
        f.sync_all().map_err(io)?;
        drop(f);
        if let Some(u) = self.uploads.get_mut(&id) {
            u.file = None;
        }
        std::fs::rename(&data_path, &final_path).map_err(io)?;
        set_mode(&final_path, 0o444);
        let side = json!({
            "uploadedBy": st.by, "at": crate::util::iso(now_ms()), "source": st.source,
            "originalName": st.original_name, "sha256": sha, "size": st.size, "untrusted": true, "pinned": false,
        });
        let _ = write_sidecar(&inbox, &final_name, &side);
        set_provenance_xattr(&final_path, &side);
        sync_dir(&inbox);
        let done = Done {
            path: final_path.display().to_string(),
            name: final_name.clone(),
            project: st.project.clone(),
            sha256: sha.clone(),
            size: st.size,
        };
        let mut st2 = st.clone();
        st2.done = Some(done.clone());
        st2.last_ms = now_ms();
        let _ = self.save(&st2);
        if let Some(u) = self.uploads.get_mut(&id) {
            u.st = st2;
        }
        self.changed();
        self.notes.push(Note {
            // The pane it was sent for (its event, its agent: "insert path" on the phone).
            pane_id: st.pane_id.clone().unwrap_or_default(),
            kind: "file_uploaded",
            data: json!({
                "project": st.project, "name": final_name, "path": done.path, "size": st.size, "sha256": sha,
                "source": st.source, "paneId": st.pane_id, "requestId": st.request_id, "sessionId": st.session_id,
                "by": st.by,
            }),
        });
        Ok(done_answer(&done))
    }

    // ── browsing ──────────────────────────────────────────────────────

    fn entry(&self, dir_path: &Path, dir: &str, name: &str) -> Option<Value> {
        let path = dir_path.join(name);
        let m = path.symlink_metadata().ok()?;
        if !m.is_file() {
            return None;
        }
        let side = read_sidecar(dir_path, name).unwrap_or(Value::Null);
        let mtime = m
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let pinned = side["pinned"].as_bool().unwrap_or(false);
        let mut e = json!({
            "name": name, "dir": dir, "path": path.display().to_string(), "size": m.len(), "mtime": mtime, "pinned": pinned,
        });
        if !pinned {
            e["expiresAt"] = json!(mtime + self.limits.retention_ms);
        }
        if let Some(s) = side["source"].as_str() {
            e["source"] = json!(s);
        }
        if let Some(s) = side["sha256"].as_str() {
            e["sha256"] = json!(s);
        }
        if let Some(s) = side["note"].as_str() {
            e["note"] = json!(s);
        }
        if let Some(s) = side["originalName"].as_str() {
            e["originalName"] = json!(s);
        }
        Some(e)
    }

    /// `files.list {project, dir?}` (both folders when `dir` is absent).
    pub fn list(&mut self, p: &Value) -> R {
        let project = p["project"].as_str().unwrap_or("");
        let dirs: Vec<&str> = match p["dir"].as_str() {
            None => vec!["inbox", "outbox"],
            Some(_) => vec![dir_param(p)?],
        };
        let mut files = vec![];
        for dir in &dirs {
            let d = self.folder(project, dir, false)?;
            for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if !valid_entry_name(&name) {
                    continue;
                }
                if let Some(v) = self.entry(&d, dir, &name) {
                    files.push(v);
                }
            }
        }
        files.sort_by(|a, b| {
            b["mtime"]
                .as_u64()
                .cmp(&a["mtime"].as_u64())
                .then(a["name"].as_str().cmp(&b["name"].as_str()))
        });
        Ok(json!({
            "type": "files_list", "project": project, "path": self.root.join(project).display().to_string(),
            "files": files,
        }))
    }

    fn target(&self, p: &Value) -> Result<(PathBuf, &'static str, String), ApiErr> {
        let project = p["project"].as_str().unwrap_or("");
        let dir = dir_param(p)?;
        let name = p["name"]
            .as_str()
            .filter(|n| valid_entry_name(n))
            .ok_or_else(|| err("invalid_params", "name is not a file in that folder"))?;
        let d = self.folder(project, dir, false)?;
        Ok((d, dir, name.to_string()))
    }

    /// `files.stat {project, dir, name}`.
    pub fn stat(&mut self, p: &Value) -> R {
        let (d, dir, name) = self.target(p)?;
        let mut e = self
            .entry(&d, dir, &name)
            .ok_or_else(|| err("file_not_found", "That file is not there any more."))?;
        e["type"] = json!("files_stat");
        Ok(e)
    }

    /// `files.delete {project, dir, name}`.
    pub fn delete(&mut self, p: &Value) -> R {
        let (d, _, name) = self.target(p)?;
        let path = d.join(&name);
        match path.symlink_metadata() {
            Ok(m) if m.is_file() || m.file_type().is_symlink() => {}
            _ => return Err(err("file_not_found", "That file is not there any more.")),
        }
        std::fs::remove_file(&path).map_err(io)?;
        let _ = std::fs::remove_file(d.join(".meta").join(format!("{name}.json")));
        self.changed();
        Ok(json!({ "type": "files_deleted", "name": name }))
    }

    /// `files.pin {project, dir, name, pinned}`: kept past the retention sweep.
    pub fn pin(&mut self, p: &Value) -> R {
        let (d, _, name) = self.target(p)?;
        if !d.join(&name).symlink_metadata().is_ok_and(|m| m.is_file()) {
            return Err(err("file_not_found", "That file is not there any more."));
        }
        let pinned = p["pinned"].as_bool().unwrap_or(true);
        let mut side = read_sidecar(&d, &name).unwrap_or_else(|| json!({}));
        side["pinned"] = json!(pinned);
        write_sidecar(&d, &name, &side).map_err(io)?;
        Ok(json!({ "type": "files_pinned", "name": name, "pinned": pinned }))
    }

    /// `files.read {xfer, n, project, dir, name, offset, length}`: FILE frames
    /// with the bytes (at most READ_WINDOW), then the answer.
    pub fn read(&mut self, p: &Value) -> Result<(Value, Vec<Vec<u8>>), ApiErr> {
        let xfer = p["xfer"]
            .as_str()
            .filter(|s| valid_transfer_id(s))
            .ok_or_else(|| err("invalid_params", "xfer is required"))?;
        let n = p["n"].as_u64().filter(|n| *n <= u32::MAX as u64).unwrap_or(0) as u32;
        let (d, _, name) = self.target(p)?;
        let f = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(d.join(&name))
            .map_err(|e| match e.raw_os_error() {
                Some(libc::ELOOP) => err("file_refused", "That is a link; files never follow links."),
                _ if e.kind() == std::io::ErrorKind::NotFound => {
                    err("file_not_found", "That file is not there any more.")
                }
                _ => io(e),
            })?;
        let m = f.metadata().map_err(io)?;
        if !m.is_file() {
            return Err(err("file_refused", "That is not a regular file."));
        }
        let size = m.len();
        let offset = p["offset"].as_u64().unwrap_or(0);
        let length = p["length"].as_u64().unwrap_or(READ_WINDOW).min(READ_WINDOW);
        let end = size.min(offset.saturating_add(length));
        let mut frames = vec![];
        let mut at = offset.min(size);
        let mut buf = vec![0u8; FRAME_PAYLOAD];
        while at < end {
            let want = ((end - at) as usize).min(FRAME_PAYLOAD);
            let k = f.read_at(&mut buf[..want], at).map_err(io)?;
            if k == 0 {
                break;
            }
            let fr = FileFrame {
                id: xfer.into(),
                req: n,
                offset: at,
                payload: buf[..k].to_vec(),
            };
            frames.push(fr.encode().map_err(|_| err("io_error", "frame"))?);
            at += k as u64;
        }
        let bytes = at - offset.min(size);
        Ok((
            json!({ "type": "files_read", "size": size, "offset": offset, "bytes": bytes, "eof": at >= size, "mtime":
                m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_millis() as u64).unwrap_or(0) }),
            frames,
        ))
    }

    // ── for agents (local socket) ─────────────────────────────────────

    /// `files.share {path, project, note?, allowed: [dirs], pane_id?}`: copy a
    /// file into the outbox for the person to download. `allowed` (set by the
    /// daemon, never the caller) are the folders a file may come from: the
    /// pane's work folder (its repository) and the files root.
    pub fn share(&mut self, p: &Value) -> R {
        let project = p["project"]
            .as_str()
            .filter(|s| valid_project_key(s))
            .ok_or_else(|| err("invalid_params", "project is required"))?;
        let src = p["path"]
            .as_str()
            .filter(|s| !s.is_empty() && s.len() <= 4096 && !s.contains('\0'))
            .ok_or_else(|| err("invalid_params", "path is required"))?;
        if !Path::new(src).is_absolute() {
            return Err(err("invalid_params", "path must be absolute"));
        }
        // The jail opens it: under one of these folders, following no link (docs/design/file-browser.md §3.4).
        let mut roots: Vec<jail::Root> = p["allowed"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str())
                    .filter_map(|x| jail::Root::open_trusted(Path::new(x)).ok())
                    .collect()
            })
            .unwrap_or_default();
        let files_root = roots.len();
        roots.extend(jail::Root::open_trusted(&self.root).ok());
        let limits = jail::Limits {
            max_size: Some(self.limits.max_file),
        };
        let (which, opened) = jail::open_absolute(&roots, src, limits).map_err(|r| match r {
            jail::Refusal::Secret(why) => err(
                "file_refused",
                format!("That file may hold secrets ({why}); it is not shared."),
            ),
            jail::Refusal::Outside => err(
                "file_refused",
                "Only files in this session's work folder or the files folder can be shared.",
            ),
            jail::Refusal::Symlink => err(
                "file_refused",
                "Links are not followed: share the file the link points to.",
            ),
            jail::Refusal::Linked => err(
                "file_refused",
                "That file has more than one name (a hard link); it is not shared.",
            ),
            jail::Refusal::NotFound | jail::Refusal::RootGone => err("file_not_found", format!("{src} does not exist")),
            jail::Refusal::NotAFile => err("file_refused", "Only regular files can be shared."),
            jail::Refusal::TooLarge { .. } => err(
                "file_too_large",
                format!("Files up to {} can be shared.", human(self.limits.max_file)),
            ),
            jail::Refusal::Invalid => err(
                "invalid_params",
                "That path cannot be shared: no `..`, `:` or `\\` in it, and no name ending in a dot or a space.",
            ),
            jail::Refusal::Unreadable(e) => io(std::io::Error::from_raw_os_error(e)),
        })?;
        if which == files_root && opened.rel.starts_with(".partial/") {
            return Err(err("file_refused", "That upload is still in progress."));
        }
        let (mut f, m, real) = (opened.file, opened.meta, opened.real);
        self.room_for(m.len(), 0, false)?;
        let outbox = self.folder(project, "outbox", true)?;
        let base = sanitize_name(
            &real
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
        )
        .unwrap_or_else(|| "file".into());
        let (name, dest) = free_name(&outbox, &base);
        let mut out = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&dest)
            .map_err(io)?;
        let mut h = Sha256::new();
        let mut buf = vec![0u8; 1024 * 1024];
        loop {
            let k = f.read(&mut buf).map_err(io)?;
            if k == 0 {
                break;
            }
            h.update(&buf[..k]);
            if let Err(e) = out.write_all(&buf[..k]) {
                let _ = std::fs::remove_file(&dest);
                return Err(io(e));
            }
        }
        out.sync_all().map_err(io)?;
        let sha = hex::encode(h.finalize());
        let note: Option<String> = p["note"]
            .as_str()
            .map(|s| s.chars().filter(|c| !c.is_control()).take(500).collect());
        let side = json!({
            "source": "agent", "at": crate::util::iso(now_ms()), "sha256": sha, "size": m.len(), "pinned": false,
            "note": note, "from": real.display().to_string(), "paneId": p["pane_id"],
        });
        let _ = write_sidecar(&outbox, &name, &side);
        self.changed();
        self.notes.push(Note {
            pane_id: p["pane_id"].as_str().unwrap_or("").to_string(),
            kind: "file_shared",
            data: json!({ "project": project, "name": name, "path": dest.display().to_string(), "size": m.len(), "sha256": sha, "note": note }),
        });
        Ok(
            json!({ "type": "files_shared", "project": project, "name": name, "path": dest.display().to_string(), "size": m.len(), "sha256": sha }),
        )
    }

    /// `files.request {what, project, pane_id?}`: ask the person for a file.
    pub fn request(&mut self, p: &Value) -> R {
        let project = p["project"]
            .as_str()
            .filter(|s| valid_project_key(s))
            .ok_or_else(|| err("invalid_params", "project is required"))?;
        let what: String = p["what"]
            .as_str()
            .unwrap_or("")
            .chars()
            .filter(|c| !c.is_control())
            .take(300)
            .collect();
        if what.trim().is_empty() {
            return Err(err(
                "invalid_params",
                "say what you need: tmuxedo files request \"the CSV export\"",
            ));
        }
        let inbox = self.folder(project, "inbox", true)?;
        let id = crate::util::random_id();
        self.notes.push(Note {
            pane_id: p["pane_id"].as_str().unwrap_or("").to_string(),
            kind: "file_requested",
            data: json!({ "project": project, "what": what.trim(), "requestId": id, "inbox": inbox.display().to_string() }),
        });
        Ok(
            json!({ "type": "files_requested", "project": project, "requestId": id, "inbox": inbox.display().to_string() }),
        )
    }

    // ── retention ─────────────────────────────────────────────────────

    /// Remove expired partial uploads (24 h after their last chunk; finished
    /// ones after an hour), and inbox/outbox files past retention unless
    /// pinned. Never leaves the root, never follows a link. → (partials, files).
    pub fn sweep(&mut self, now: u64, files_too: bool) -> (usize, usize) {
        let mut partials = 0;
        let pd = self.root.join(".partial");
        if real_dir(&pd) {
            for e in std::fs::read_dir(&pd).into_iter().flatten().flatten() {
                let path = e.path();
                let id = e.file_name().to_string_lossy().into_owned();
                if !real_dir(&path) {
                    let _ = std::fs::remove_file(&path);
                    continue;
                }
                let (last, done) = match read_state(&path) {
                    Some(st) => (st.last_ms, st.done.is_some()),
                    None => (mtime_ms(&path), false),
                };
                let ttl = if done { 3600 * 1000 } else { self.limits.partial_ttl_ms };
                if last + ttl < now {
                    // A chunk still arriving keeps it.
                    if self.uploads.get(&id).is_some_and(|u| !u.rx.is_empty()) {
                        continue;
                    }
                    self.uploads.remove(&id);
                    if std::fs::remove_dir_all(&path).is_ok() {
                        partials += 1;
                    }
                }
            }
        }
        let mut files = 0;
        if files_too {
            for p in std::fs::read_dir(&self.root).into_iter().flatten().flatten() {
                let pdir = p.path();
                if p.file_name() == ".partial" || !real_dir(&pdir) {
                    continue;
                }
                for dir in ["inbox", "outbox"] {
                    let d = pdir.join(dir);
                    if !real_dir(&d) {
                        continue;
                    }
                    for f in std::fs::read_dir(&d).into_iter().flatten().flatten() {
                        let name = f.file_name().to_string_lossy().into_owned();
                        if !valid_entry_name(&name) {
                            continue;
                        }
                        let path = f.path();
                        let Ok(m) = path.symlink_metadata() else { continue };
                        if !(m.is_file() || m.file_type().is_symlink()) {
                            continue;
                        }
                        if read_sidecar(&d, &name).is_some_and(|s| s["pinned"].as_bool() == Some(true)) {
                            continue;
                        }
                        if mtime_of(&m) + self.limits.retention_ms < now && std::fs::remove_file(&path).is_ok() {
                            let _ = std::fs::remove_file(d.join(".meta").join(format!("{name}.json")));
                            files += 1;
                        }
                    }
                }
            }
        }
        if partials + files > 0 {
            self.changed();
        }
        (partials, files)
    }
}

fn done_answer(d: &Done) -> Value {
    json!({ "type": "upload_complete", "path": d.path, "name": d.name, "project": d.project, "sha256": d.sha256, "size": d.size })
}

fn read_state(dir: &Path) -> Option<UploadState> {
    let p = dir.join("state.json");
    if !p.symlink_metadata().ok()?.is_file() {
        return None;
    }
    serde_json::from_slice(&std::fs::read(p).ok()?).ok()
}

fn read_sidecar(dir: &Path, name: &str) -> Option<Value> {
    let p = dir.join(".meta").join(format!("{name}.json"));
    if !p.symlink_metadata().ok()?.is_file() {
        return None;
    }
    serde_json::from_slice(&std::fs::read(p).ok()?).ok()
}

fn write_sidecar(dir: &Path, name: &str, v: &Value) -> std::io::Result<()> {
    let meta = dir.join(".meta");
    if !meta.exists() {
        std::fs::create_dir(&meta)?;
        set_mode(&meta, 0o700);
    }
    if !real_dir(&meta) {
        return Err(std::io::Error::other("the .meta folder is a link"));
    }
    let tmp = meta.join(format!(".{name}.json.tmp"));
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&tmp)?;
    f.write_all(&serde_json::to_vec_pretty(v).expect("json"))?;
    std::fs::rename(tmp, meta.join(format!("{name}.json")))
}

/// `user.tmuxedo.provenance` where the filesystem has user xattrs (best effort).
fn set_provenance_xattr(path: &Path, side: &Value) {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStrExt;
        let Ok(c) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
            return;
        };
        let v = side.to_string();
        // SAFETY: valid C strings and buffer.
        unsafe {
            libc::setxattr(
                c.as_ptr(),
                c"user.tmuxedo.provenance".as_ptr(),
                v.as_ptr() as *const libc::c_void,
                v.len(),
                0,
            );
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = (path, side);
}

fn sync_dir(d: &Path) {
    if let Ok(f) = std::fs::File::open(d) {
        let _ = f.sync_all();
    }
}

fn mtime_of(m: &std::fs::Metadata) -> u64 {
    m.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn mtime_ms(p: &Path) -> u64 {
    p.symlink_metadata().map(|m| mtime_of(&m)).unwrap_or(0)
}

/// `name`, or `stem-1.ext`, `stem-2.ext`, … if taken.
fn free_name(dir: &Path, name: &str) -> (String, PathBuf) {
    let p = dir.join(name);
    if p.symlink_metadata().is_err() {
        return (name.to_string(), p);
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    for k in 1..10_000 {
        let n = format!("{stem}-{k}{ext}");
        let p = dir.join(&n);
        if p.symlink_metadata().is_err() {
            return (n, p);
        }
    }
    let n = format!("{stem}-{}{ext}", crate::util::random_id());
    (n.clone(), dir.join(n))
}

pub fn human(b: u64) -> String {
    const U: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{b} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}

/// The set of chunk numbers, for tests.
pub fn received_set(st: &UploadState) -> BTreeSet<u64> {
    st.received.keys().copied().collect()
}
