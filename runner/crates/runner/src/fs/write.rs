//! The file browser's writing methods (design §4, §4.1; slice A2):
//! `fs.write`, `fs.mkdir`, `fs.rename`, `fs.delete`, `fs.undo`.
//!
//! - Every path is resolved by the jail from the root's held handle
//!   ([`jail::Root::open_parent`]); every change is one `*at` call on the
//!   parent's handle and one validated name. No path is joined or reopened.
//! - Links, hard-linked files, other filesystems, secret names (on both
//!   sides of a rename), Windows device names: refused, as on the read side.
//! - A replace writes a temporary file in the same folder, `fsync`s it,
//!   exchanges it with the target (`RENAME_EXCHANGE`), then checks that what
//!   was swapped out is the file whose version was `base`; if not, it swaps
//!   back and answers `conflict`. A failure before the exchange leaves the
//!   old file as it was.
//! - What a change displaces is kept in the undo store (the runner's own
//!   state folder, never the person's tree): 24 h, 50 MiB, oldest first.
//! - Every change is journalled `started` before it commits and `done` or
//!   `failed` after, with the path hashed (`fs-undo/journal.jsonl`).
//! - The runner refuses a change without web's flags (`control_required`,
//!   `stepup_required`) and on a root that is not writable (`read_only`),
//!   whatever the gateway did. Observe-only is refused before this (the
//!   daemon's guardrail).

use super::{has_private_key, masks, refusal, Ctx};
use crate::api::{err, ApiErr, ApiResult};
use crate::jail::{self, Kind, Parent};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::ffi::CStr;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use tmuxedo_protocol::fs::{Version, WriteOp, EDITABLE_MAX};

/// The undo store's size at most (bytes of kept content).
pub const UNDO_MAX_BYTES: u64 = 50 * 1024 * 1024;
/// How long a kept version stays (seconds).
pub const UNDO_MAX_AGE: u64 = 24 * 60 * 60;
/// Answers remembered by `op_id`, so a retry gets the first answer.
pub const OP_MEMORY: usize = 256;
/// The journal is rotated past this size (bytes).
pub const JOURNAL_MAX: u64 = 1024 * 1024;

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// What one kept entry puts back.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Back {
    /// Replace the file (still at version `left`) with the kept bytes.
    Replace { left: String },
    /// Remove the file this action created (still at version `left`).
    Remove { left: String },
    /// Create the file again from the kept bytes (`mode`).
    Create { mode: u32 },
    /// Remove the folder this action made (if still empty).
    Rmdir,
    /// Make the folder again.
    Mkdir,
    /// Rename `rel` back to `to`.
    Rename { to: String },
}

/// The undo store and the journal: `<state>/fs-undo/`. One lock for all
/// writes, so the runner's own changes never interleave.
pub struct Store {
    dir: PathBuf,
    max_bytes: u64,
    answers: VecDeque<(String, ApiResult)>,
}

fn sys(e: i32) -> ApiErr {
    match e {
        libc::EEXIST => err("exists", "Something with that name is already there."),
        libc::ENOTEMPTY => err("not_empty", "That folder is not empty."),
        libc::ENOENT => err("not_found", "Nothing is there."),
        libc::ELOOP => err("symlink_refused", "Links are not followed."),
        libc::EXDEV => err("outside_root", "That is outside this folder."),
        libc::EINVAL => err("invalid_path", "That path is not allowed."),
        libc::ENOSPC | libc::EDQUOT => err("insufficient_storage", "The machine is out of space."),
        _ => err("permission_denied", "The machine refused that change."),
    }
}

fn io(e: std::io::Error) -> ApiErr {
    sys(e.raw_os_error().unwrap_or(libc::EIO))
}

fn conflict() -> ApiErr {
    err("conflict", "It changed since you opened it. Nothing was changed.")
}

fn version_of(bytes: &[u8], meta: &std::fs::Metadata) -> Version {
    Version {
        sha256: hex::encode(Sha256::digest(bytes)),
        size: bytes.len() as u64,
        mtime_ns: (meta.mtime().max(0) as u64)
            .saturating_mul(1_000_000_000)
            .saturating_add(meta.mtime_nsec().max(0) as u64),
    }
}

/// Read a whole handle, at most `max` bytes (`too_large` past it).
fn read_all(f: &mut std::fs::File, max: u64) -> Result<Vec<u8>, ApiErr> {
    f.seek(SeekFrom::Start(0)).map_err(io)?;
    let mut b = Vec::new();
    f.take(max + 1).read_to_end(&mut b).map_err(io)?;
    if b.len() as u64 > max {
        return Err(err("too_large", "That file is too large to change here."));
    }
    Ok(b)
}

impl Store {
    pub fn new(state_dir: &std::path::Path) -> Store {
        Store {
            dir: state_dir.join("fs-undo"),
            max_bytes: UNDO_MAX_BYTES,
            answers: VecDeque::new(),
        }
    }

    #[cfg(test)]
    pub fn with_max(dir: PathBuf, max_bytes: u64) -> Store {
        Store {
            dir,
            max_bytes,
            answers: VecDeque::new(),
        }
    }

    fn ensure(&self) -> Result<(), ApiErr> {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.dir)
            .map_err(io)
    }

    /// Append one journal line and flush it to disk. The path is hashed.
    fn journal(&self, line: Value) -> Result<(), ApiErr> {
        self.ensure()?;
        let p = self.dir.join("journal.jsonl");
        if std::fs::metadata(&p).is_ok_and(|m| m.len() > JOURNAL_MAX) {
            let _ = std::fs::rename(&p, self.dir.join("journal.1.jsonl"));
        }
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&p)
            .map_err(io)?;
        f.write_all(format!("{line}\n").as_bytes()).map_err(io)?;
        f.sync_data().map_err(io)
    }

    /// The ids kept, oldest first, with their content size and age.
    fn entries(&self) -> Vec<(String, u64, u64)> {
        let Ok(rd) = std::fs::read_dir(&self.dir) else {
            return vec![];
        };
        let mut out: Vec<(String, u64, u64)> = rd
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().into_string().ok()?;
                let id = name.strip_suffix(".json")?.to_string();
                let meta: Value = serde_json::from_slice(&std::fs::read(e.path()).ok()?).ok()?;
                let size = std::fs::metadata(self.dir.join(format!("{id}.bin"))).map_or(0, |m| m.len());
                Some((id, size, meta["at"].as_u64().unwrap_or(0)))
            })
            .collect();
        out.sort_by_key(|e| e.2);
        out
    }

    fn drop_entry(&self, id: &str) {
        let _ = std::fs::remove_file(self.dir.join(format!("{id}.json")));
        let _ = std::fs::remove_file(self.dir.join(format!("{id}.bin")));
    }

    /// Drop what is past 24 h, then the oldest until `incoming` more fits.
    fn prune(&self, incoming: u64) {
        let now = now_ms() / 1000;
        let mut kept = Vec::new();
        for (id, size, at) in self.entries() {
            if now.saturating_sub(at) > UNDO_MAX_AGE {
                self.drop_entry(&id);
            } else {
                kept.push((id, size));
            }
        }
        let mut total: u64 = kept.iter().map(|e| e.1).sum::<u64>() + incoming;
        for (id, size) in kept {
            if total <= self.max_bytes {
                break;
            }
            self.drop_entry(&id);
            total -= size;
        }
    }

    /// Keep what one action displaced; the new undo id.
    fn put(&self, root: &str, rel: &str, back: &Back, bytes: Option<&[u8]>) -> Result<String, ApiErr> {
        let size = bytes.map_or(0, |b| b.len() as u64);
        if size > self.max_bytes {
            return Err(err("too_large", "That file is too large to keep for Undo."));
        }
        self.ensure()?;
        self.prune(size);
        let id = format!("u{}", hex::encode(crate::util::random_bytes(8)));
        let (kind, extra) = match back {
            Back::Replace { left } => ("replace", json!({ "left": left })),
            Back::Remove { left } => ("remove", json!({ "left": left })),
            Back::Create { mode } => ("create", json!({ "mode": mode })),
            Back::Rmdir => ("rmdir", json!({})),
            Back::Mkdir => ("mkdir", json!({})),
            Back::Rename { to } => ("rename", json!({ "to": to })),
        };
        if let Some(b) = bytes {
            std::fs::write(self.dir.join(format!("{id}.bin")), b).map_err(io)?;
        }
        let meta = json!({ "root": root, "rel": rel, "back": kind, "x": extra, "at": now_ms() / 1000 });
        std::fs::write(self.dir.join(format!("{id}.json")), meta.to_string()).map_err(io)?;
        Ok(id)
    }

    /// The root an undo id belongs to (the daemon binds the request to it).
    pub fn root_of(&self, id: &str) -> Option<String> {
        let meta: Value = serde_json::from_slice(&std::fs::read(self.dir.join(format!("{id}.json"))).ok()?).ok()?;
        meta["root"].as_str().map(String::from)
    }

    fn take(&self, id: &str) -> Result<(String, Back, Option<Vec<u8>>), ApiErr> {
        let gone = || err("not_found", "That can no longer be undone.");
        let meta: Value = std::fs::read(self.dir.join(format!("{id}.json")))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .ok_or_else(gone)?;
        if (now_ms() / 1000).saturating_sub(meta["at"].as_u64().unwrap_or(0)) > UNDO_MAX_AGE {
            self.drop_entry(id);
            return Err(gone());
        }
        let s = |k: &str| meta["x"][k].as_str().map(String::from).ok_or_else(gone);
        let back = match meta["back"].as_str() {
            Some("replace") => Back::Replace { left: s("left")? },
            Some("remove") => Back::Remove { left: s("left")? },
            Some("create") => Back::Create {
                mode: meta["x"]["mode"].as_u64().unwrap_or(0o644) as u32,
            },
            Some("rmdir") => Back::Rmdir,
            Some("mkdir") => Back::Mkdir,
            Some("rename") => Back::Rename { to: s("to")? },
            _ => return Err(gone()),
        };
        let bytes = std::fs::read(self.dir.join(format!("{id}.bin"))).ok();
        let rel = meta["rel"].as_str().map(String::from).ok_or_else(gone)?;
        Ok((rel, back, bytes))
    }

    fn remembered(&self, op_id: &str) -> Option<ApiResult> {
        self.answers.iter().find(|(k, _)| k == op_id).map(|(_, r)| r.clone())
    }

    fn remember(&mut self, op_id: &str, r: &ApiResult) {
        if self.answers.len() >= OP_MEMORY {
            self.answers.pop_front();
        }
        self.answers.push_back((op_id.into(), r.clone()));
    }
}

fn path_hash(root: &str, rel: &str) -> String {
    let mut h = Sha256::new();
    h.update(root.as_bytes());
    h.update([0]);
    h.update(rel.as_bytes());
    hex::encode(&h.finalize()[..12])
}

/// The one way a change happens (design §3.9 `jail::commit`): journal
/// `started`, run it, journal how it ended. A change whose `started` line
/// cannot be written does not happen.
fn commit(store: &Store, ctx: &Ctx, op: &str, rel: &str, by: &str, f: impl FnOnce() -> ApiResult) -> ApiResult {
    let base = json!({ "at": now_ms(), "op": op, "root": ctx.id, "pathHash": path_hash(&ctx.id, rel), "by": by });
    let mut line = base.clone();
    line["state"] = json!("started");
    store.journal(line)?;
    let out = f();
    let mut line = base;
    match &out {
        Ok(v) => {
            line["state"] = json!("done");
            if let Some(u) = v.get("undoId") {
                line["undoId"] = u.clone();
            }
        }
        Err(e) => {
            line["state"] = json!("failed");
            line["code"] = json!(e.code);
        }
    }
    let _ = store.journal(line);
    out
}

fn parent(ctx: &Ctx, rel: &str) -> Result<Parent, ApiErr> {
    ctx.root.open_parent(rel).map_err(|r| refusal(&r, false))
}

/// The current file at `p`: its handle, bytes and version, refusing what
/// may not be changed (a link, a hard link, too large, key material,
/// text the masker would change).
fn current(ctx: &Ctx, p: &Parent) -> Result<(std::fs::File, std::fs::Metadata, Vec<u8>, Version), ApiErr> {
    let (mut f, meta) = ctx.root.open_in(p, false).map_err(|r| refusal(&r, false))?;
    if meta.len() > EDITABLE_MAX {
        return Err(err("too_large", "That file is too large to change here."));
    }
    let bytes = read_all(&mut f, EDITABLE_MAX)?;
    if has_private_key(&bytes) {
        return Err(err(
            "file_refused",
            "This may hold secrets (key material), so tmuxedo does not change it.",
        ));
    }
    if masks(ctx, &bytes) {
        return Err(err(
            "masked_file",
            "This file has values tmuxedo hides. Edit it in the terminal.",
        ));
    }
    let v = version_of(&bytes, &meta);
    Ok((f, meta, bytes, v))
}

/// Write `bytes` into a new temporary file beside the target and flush it.
fn staged(p: &Parent, bytes: &[u8], mode: u32) -> Result<(std::fs::File, std::ffi::CString), ApiErr> {
    let (mut f, name) = p.temp(mode).map_err(sys)?;
    let written = f.write_all(bytes).and_then(|_| f.sync_all());
    if let Err(e) = written {
        let _ = p.unlink(&name, false);
        return Err(io(e));
    }
    Ok((f, name))
}

/// Replace the file at `p` (whose content must be `base`) with `bytes`.
fn replace(store: &Store, ctx: &Ctx, p: &Parent, bytes: &[u8], base: &Version) -> ApiResult {
    let (_, meta, old, cur) = current(ctx, p)?;
    if !cur.same_content(base) {
        return Err(conflict());
    }
    let (new, tmp) = staged(p, bytes, meta.mode() & 0o777)?;
    let undo = match store.put(&ctx.id, &p.rel, &Back::Replace { left: String::new() }, Some(&old)) {
        Ok(u) => u,
        Err(e) => {
            let _ = p.unlink(&tmp, false);
            return Err(e);
        }
    };
    let fail = |e: ApiErr| {
        let _ = p.unlink(&tmp, false);
        store.drop_entry(&undo);
        Err(e)
    };
    let nm = new.metadata().map_err(io)?;
    match swap_in(p, &tmp, &meta, (nm.dev(), nm.ino()), base) {
        Ok(()) => {}
        Err(Swap::Before(e)) => return fail(e),
        Err(Swap::After(e)) => {
            store.drop_entry(&undo);
            return Err(e);
        }
    }
    let _ = p.sync();
    // Only the file that was checked goes: what is under the temporary name is the old file.
    if p.identity_of(&tmp).ok() == Some((meta.dev(), meta.ino())) {
        let _ = p.unlink(&tmp, false);
    }
    let nm = new.metadata().map_err(io)?;
    let v = version_of(bytes, &nm);
    // The entry learns the version this write left, so Undo can check it.
    rewrite_left(store, &undo, &v.encode());
    Ok(
        json!({ "type": "fs_written", "path": p.rel, "version": v.encode(), "size": v.size,
               "mtimeMs": v.mtime_ns / 1_000_000, "undoId": undo }),
    )
}

fn rewrite_left(store: &Store, id: &str, left: &str) {
    let p = store.dir.join(format!("{id}.json"));
    if let Some(mut m) = std::fs::read(&p)
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
    {
        m["x"]["left"] = json!(left);
        let _ = std::fs::write(&p, m.to_string());
    }
}

/// How a swap failed: before anything moved (the temporary file is ours to
/// remove) or after (the swap was undone or, if the folder kept changing,
/// what came out was left under the temporary name rather than lost).
enum Swap {
    Before(ApiErr),
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))] // only the Linux exchange can fail after the swap
    After(ApiErr),
}

/// Put `tmp` in the target's place and check what came out is the file
/// that was read (the same inode, still `base`); else put it back, but only
/// while the target is still our new file (`ours`). A link that came out is
/// removed (it holds no data); anything else is kept, never deleted.
#[cfg(target_os = "linux")]
fn swap_in(p: &Parent, tmp: &CStr, meta: &std::fs::Metadata, ours: (u64, u64), base: &Version) -> Result<(), Swap> {
    p.exchange(tmp).map_err(|e| Swap::Before(sys(e)))?;
    let same = p.identity_of(tmp).is_ok_and(|id| id == (meta.dev(), meta.ino()))
        && p.open_named(tmp)
            .ok()
            .and_then(|mut f| read_all(&mut f, EDITABLE_MAX).ok())
            .is_some_and(|b| hex::encode(Sha256::digest(&b)) == base.sha256 && b.len() as u64 == base.size);
    if !same {
        if p.identity_of(p.cname()).ok() == Some(ours) {
            // An edit landed after the check: put it back where it was; ours is discarded.
            if p.exchange(tmp).is_ok() && p.identity_of(tmp).ok() == Some(ours) {
                let _ = p.unlink(tmp, false);
            }
        }
        // A link that ended up under the temporary name holds no data: removed, never followed.
        if p.kind_of(tmp).ok() == Some(Kind::Link) {
            let _ = p.unlink(tmp, false);
        }
        return Err(Swap::After(conflict()));
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn swap_in(p: &Parent, tmp: &CStr, meta: &std::fs::Metadata, _ours: (u64, u64), _base: &Version) -> Result<(), Swap> {
    // No exchange here: the target's identity checked immediately before a plain rename.
    if p.identity_of(p.cname()).ok() != Some((meta.dev(), meta.ino())) {
        return Err(Swap::Before(conflict()));
    }
    p.replace(tmp).map_err(|e| Swap::Before(sys(e)))
}

/// Create the file at `p` from `bytes`; never replaces anything.
fn create(store: &Store, ctx: &Ctx, p: &Parent, bytes: &[u8], mode: u32) -> ApiResult {
    if p.kind().map_err(|r| refusal(&r, false))? != Kind::Missing {
        return Err(err("exists", "Something with that name is already there."));
    }
    let (new, tmp) = staged(p, bytes, mode)?;
    if let Err(e) = p.rename_noreplace(&tmp, p) {
        let _ = p.unlink(&tmp, false);
        return Err(sys(e));
    }
    let _ = p.sync();
    let nm = new.metadata().map_err(io)?;
    let v = version_of(bytes, &nm);
    let undo = store.put(&ctx.id, &p.rel, &Back::Remove { left: v.encode() }, None)?;
    Ok(
        json!({ "type": "fs_written", "path": p.rel, "version": v.encode(), "size": v.size,
               "mtimeMs": v.mtime_ns / 1_000_000, "undoId": undo }),
    )
}

fn mkdir(store: &Store, ctx: &Ctx, p: &Parent) -> ApiResult {
    match p.kind().map_err(|r| refusal(&r, false))? {
        Kind::Missing => {}
        _ => return Err(err("exists", "Something with that name is already there.")),
    }
    p.mkdir().map_err(sys)?;
    // What was made is a folder on the root's device, not a link someone raced in.
    ctx.root.open_in(p, true).map_err(|r| refusal(&r, true))?;
    let _ = p.sync();
    let undo = store.put(&ctx.id, &p.rel, &Back::Rmdir, None)?;
    Ok(json!({ "type": "fs_made", "path": p.rel, "undoId": undo }))
}

fn rename(store: &Store, ctx: &Ctx, from: &Parent, to: &Parent, base: Option<&Version>) -> ApiResult {
    let (handle_id, version) = match from.kind().map_err(|r| refusal(&r, false))? {
        Kind::File => {
            let (_, meta, _, cur) = current(ctx, from)?;
            if base.is_some_and(|b| !cur.same_content(b)) {
                return Err(conflict());
            }
            ((meta.dev(), meta.ino()), Some(cur))
        }
        Kind::Dir => {
            let (_, meta) = ctx.root.open_in(from, true).map_err(|r| refusal(&r, true))?;
            ((meta.dev(), meta.ino()), None)
        }
        Kind::Link => return Err(refusal(&jail::Refusal::Symlink, false)),
        Kind::Missing => return Err(refusal(&jail::Refusal::NotFound, false)),
        Kind::Other => return Err(refusal(&jail::Refusal::NotAFile, false)),
    };
    from.rename_noreplace(from.cname(), to).map_err(sys)?;
    if to.identity_of(to.cname()).ok() != Some(handle_id) {
        // Something else was swapped in between the check and the rename: put it back.
        let _ = to.rename_noreplace(to.cname(), from);
        return Err(conflict());
    }
    let _ = from.sync();
    let _ = to.sync();
    let undo = store.put(&ctx.id, &to.rel, &Back::Rename { to: from.rel.clone() }, None)?;
    let mut out = json!({ "type": "fs_renamed", "path": to.rel, "undoId": undo });
    if let Some(v) = version {
        out["version"] = json!(v.encode());
    }
    Ok(out)
}

fn delete(store: &Store, ctx: &Ctx, p: &Parent, base: Option<&Version>) -> ApiResult {
    match p.kind().map_err(|r| refusal(&r, false))? {
        Kind::Dir => {
            ctx.root.open_in(p, true).map_err(|r| refusal(&r, true))?;
            p.unlink(p.cname(), true).map_err(|e| match e {
                libc::EEXIST | libc::ENOTEMPTY => err("not_empty", "That folder is not empty."),
                e => sys(e),
            })?;
            let _ = p.sync();
            let undo = store.put(&ctx.id, &p.rel, &Back::Mkdir, None)?;
            Ok(json!({ "type": "fs_deleted", "path": p.rel, "undoId": undo }))
        }
        Kind::File => {
            let Some(base) = base else {
                return Err(err("invalid_params", "fs.delete: a file needs its base version"));
            };
            let (mut f, meta, bytes, cur) = current(ctx, p)?;
            if !cur.same_content(base) {
                return Err(conflict());
            }
            // Move it aside under a temporary name, then check it is the file that was read.
            let aside = p.temp(0o600).map_err(sys)?.1;
            let _ = p.unlink(&aside, false);
            let aside_p = ctx
                .root
                .open_parent(&aside_rel(&p.rel, &aside))
                .map_err(|r| refusal(&r, false))?;
            p.rename_noreplace(p.cname(), &aside_p).map_err(sys)?;
            let still = p.identity_of(&aside).ok() == Some((meta.dev(), meta.ino()))
                && read_all(&mut f, EDITABLE_MAX).is_ok_and(|b| b == bytes);
            if !still {
                let _ = aside_p.rename_noreplace(&aside, p);
                return Err(conflict());
            }
            let undo = match store.put(
                &ctx.id,
                &p.rel,
                &Back::Create {
                    mode: meta.mode() & 0o777,
                },
                Some(&bytes),
            ) {
                Ok(u) => u,
                Err(e) => {
                    let _ = aside_p.rename_noreplace(&aside, p);
                    return Err(e);
                }
            };
            p.unlink(&aside, false).map_err(sys)?;
            let _ = p.sync();
            Ok(json!({ "type": "fs_deleted", "path": p.rel, "undoId": undo }))
        }
        Kind::Link => Err(refusal(&jail::Refusal::Symlink, false)),
        Kind::Missing => Err(refusal(&jail::Refusal::NotFound, false)),
        Kind::Other => Err(refusal(&jail::Refusal::NotAFile, false)),
    }
}

/// The root-relative path of a temporary name beside `rel`.
fn aside_rel(rel: &str, aside: &CStr) -> String {
    let name = aside.to_string_lossy();
    match rel.rsplit_once('/') {
        Some((dir, _)) => format!("{dir}/{name}"),
        None => name.into_owned(),
    }
}

fn undo(store: &Store, ctx: &Ctx, id: &str) -> ApiResult {
    let (rel, back, bytes) = store.take(id)?;
    let p = parent(ctx, &rel)?;
    let need = |b: Option<Vec<u8>>| b.ok_or_else(|| err("not_found", "That can no longer be undone."));
    let ver = |s: &str| Version::parse(s).ok_or_else(|| err("not_found", "That can no longer be undone."));
    let out = match back {
        Back::Replace { left } => replace(store, ctx, &p, &need(bytes)?, &ver(&left)?),
        Back::Remove { left } => delete(store, ctx, &p, Some(&ver(&left)?)),
        Back::Create { mode } => create(store, ctx, &p, &need(bytes)?, mode),
        Back::Rmdir => delete(store, ctx, &p, None),
        Back::Mkdir => mkdir(store, ctx, &p),
        Back::Rename { to } => rename(store, ctx, &p, &parent(ctx, &to)?, None),
    }?;
    // Used once: the action it undid is no longer the latest.
    store.drop_entry(id);
    Ok(json!({ "type": "fs_undone", "path": out["path"], "undoId": out["undoId"], "version": out.get("version") }))
}

/// Run one mutating request against its root. `now` is this machine's
/// clock (milliseconds). The store's lock is held by the caller.
pub fn run(store: &mut Store, ctx: &Ctx, req: &tmuxedo_protocol::fs::WriteRequest, now: u64) -> ApiResult {
    req.flags.check(now).map_err(|code| match code {
        "control_required" => err(code, "Changing files needs control of this machine."),
        _ => err(code, "Confirm it's you first."),
    })?;
    if !ctx.writable {
        return Err(err("read_only", "This folder is read-only here."));
    }
    if let Some(r) = store.remembered(&req.op_id) {
        return r;
    }
    let by = req.flags.by.as_deref().unwrap_or("web");
    let s: &Store = store;
    let out = match &req.op {
        WriteOp::Write { path, text, base, .. } => {
            let p = parent(ctx, path)?;
            commit(s, ctx, "write", &p.rel.clone(), by, || match base {
                Some(b) => replace(s, ctx, &p, text.as_bytes(), b),
                None => create(s, ctx, &p, text.as_bytes(), 0o644),
            })
        }
        WriteOp::Mkdir { path, .. } => {
            let p = parent(ctx, path)?;
            commit(s, ctx, "mkdir", &p.rel.clone(), by, || mkdir(s, ctx, &p))
        }
        WriteOp::Rename { from, to, base, .. } => {
            let (pf, pt) = (parent(ctx, from)?, parent(ctx, to)?);
            commit(s, ctx, "rename", &pf.rel.clone(), by, || {
                rename(s, ctx, &pf, &pt, base.as_ref())
            })
        }
        WriteOp::Delete { path, base, .. } => {
            let p = parent(ctx, path)?;
            commit(s, ctx, "delete", &p.rel.clone(), by, || {
                delete(s, ctx, &p, base.as_ref())
            })
        }
        WriteOp::Undo { undo_id } => commit(s, ctx, "undo", undo_id, by, || undo(s, ctx, undo_id)),
    };
    store.remember(&req.op_id, &out);
    out
}
