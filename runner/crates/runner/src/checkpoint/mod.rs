//! Checkpoints and undo (Phase 23; docs/design/away-and-review.md §3, §5;
//! docs/protocol.md "Checkpoints"; docs/handoff/checkpoints.md).
//!
//! Before each agent turn the runner records the session's working tree as
//! a commit in a **shadow repository** of its own, outside the folder
//! (`${XDG_DATA_HOME}/tmuxedo/checkpoints/<hash of the repository's top>.git`),
//! under `refs/sessions/<session>/<number>`. The user's repository is only
//! ever read (`ls-files`, `rev-parse`, with `--no-optional-locks`): its
//! index, HEAD, branches, stash and object store are never written. The
//! tree is built with a temporary index (`GIT_INDEX_FILE`) inside the shadow.
//!
//! What a checkpoint holds: tracked files and untracked files git does not
//! ignore, as they are on disk (content, executable bit, symlinks), plus
//! each file's exact permission bits when they are not the usual 0644/0755.
//! Left out (and never touched by a restore): ignored files, the file
//! browser's secret names (crate::jail::secret_reason), the usual build
//! folders (`node_modules`, `.venv`, `target`, `dist`, `.next`), files over
//! 50 MB, nested repositories, and names git or a terminal cannot carry.
//!
//! A restore compares the checkpoint with the folder now (a fresh snapshot,
//! recorded first as the "restore" checkpoint, so the restore itself can be
//! undone), removes what the checkpoint did not have, writes what differs
//! (through a temporary file and a rename, never through a symlinked
//! folder), and puts the permission bits back.

pub mod daemon;

use crate::api::{err, ApiErr};
use crate::jail;
use crate::review::{repo_top, SAFE};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsString;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tmuxedo_protocol::checkpoint::{self as proto, KEEP, KEEP_MS, LARGE_FILE, PREVIEW_FILES_MAX};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

/// The longest one git run may take.
const GIT_TIME: Duration = Duration::from_secs(120);
/// A folder with more files than this is not recorded.
pub const FILES_MAX: usize = 200_000;
/// Untracked folders left out wherever they are (build output, installs).
pub const EXCLUDED_DIRS: &[&str] = &["node_modules", ".venv", "target", "dist", ".next"];
/// Names a skipped list keeps (the rest is counted).
const NAMES_MAX: usize = 20;
/// A file changed this recently is hashed again next time (the stat cache).
const RACY_MS: i64 = 2_000;
/// Stat cache entries kept per repository.
const CACHE_MAX: usize = 400_000;
/// Times, for git's own records.
const IDENT: &str = "tmuxedo";
const IDENT_EMAIL: &str = "checkpoints@tmuxedo.invalid";

/// Where checkpoints live: `$TMUXEDO_CHECKPOINTS_ROOT` (`off` turns them
/// off), else `${XDG_DATA_HOME:-~/.local/share}/tmuxedo/checkpoints`.
pub fn configured_root(home: &Path) -> Option<PathBuf> {
    if let Some(r) = tmuxedo_holder::env_os("CHECKPOINTS_ROOT") {
        let r = PathBuf::from(r);
        if r.as_os_str() == "off" || r.as_os_str().is_empty() || !r.is_absolute() {
            return None;
        }
        return Some(r);
    }
    let data = match std::env::var_os("XDG_DATA_HOME") {
        Some(d) if !d.is_empty() && Path::new(&d).is_absolute() => PathBuf::from(d),
        _ => home.join(".local/share"),
    };
    Some(data.join("tmuxedo").join("checkpoints"))
}

/// One file as a checkpoint records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ent {
    /// git's mode: 0o100644, 0o100755 or 0o120000.
    pub mode: u32,
    pub oid: String,
    /// The exact permission bits (0 for a symlink).
    pub perm: u32,
}

fn canonical_perm(mode: u32) -> u32 {
    match mode {
        0o100755 => 0o755,
        0o100644 => 0o644,
        _ => 0,
    }
}

/// What a snapshot left out.
#[derive(Debug, Default, Clone)]
pub struct Skipped {
    pub large: Vec<String>,
    pub large_n: usize,
    pub secret: usize,
    pub other: usize,
}

impl Skipped {
    fn json(&self) -> Value {
        json!({ "large": self.large, "large_count": self.large_n, "secret": self.secret, "other": self.other })
    }
}

/// A snapshot of a folder now.
pub struct Snap {
    /// The checkpoint number when one was recorded.
    pub id: Option<u64>,
    pub tree: String,
    pub files: BTreeMap<String, Ent>,
    pub skipped: Skipped,
}

/// A recorded checkpoint.
#[derive(Debug, Clone)]
pub struct Info {
    pub id: u64,
    pub refname: String,
    pub tree: String,
    pub kind: String,
    pub at: u64,
    pub files: u64,
    pub restoring: Option<u64>,
    /// The chat turns this checkpoint was taken before (`t3`): one, or more
    /// when the folder did not change between turns.
    pub turns: Vec<String>,
    pub exact: BTreeMap<String, u32>,
    pub skipped: Value,
}

impl Info {
    pub fn json(&self) -> Value {
        let mut v =
            json!({ "id": self.id, "kind": self.kind, "at": self.at, "files": self.files, "skipped": self.skipped });
        if let Some(r) = self.restoring {
            v["restoring"] = json!(r);
        }
        if !self.turns.is_empty() {
            v["turns"] = json!(self.turns);
        }
        v
    }
}

/// The most turn names one checkpoint keeps (the newest stay).
pub const TURNS_MAX: usize = 50;

/// A chat turn's name as the runner makes it (`t` and digits).
fn valid_turn(t: &str) -> bool {
    t.len() <= 24 && t.starts_with('t') && t.len() > 1 && t[1..].bytes().all(|b| b.is_ascii_digit())
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Stamp {
    size: u64,
    mtime_ns: i128,
    ctime_ns: i128,
    ino: u64,
    mode: u32,
}

type Cache = HashMap<String, (Stamp, String)>;

/// The checkpoints of this machine: one lock for every repository (runs
/// are short and a snapshot must not race a restore).
#[derive(Clone)]
pub struct Store {
    root: Option<PathBuf>,
    caches: Arc<Mutex<HashMap<PathBuf, Cache>>>,
}

/// Why checkpoints are not available for a folder (the list answers it).
pub struct Unavailable {
    pub reason: String,
    pub message: String,
}

impl From<ApiErr> for Unavailable {
    fn from(e: ApiErr) -> Self {
        Unavailable {
            reason: e.code,
            message: e.message,
        }
    }
}

impl Store {
    pub fn new(root: Option<PathBuf>) -> Self {
        Store {
            root,
            caches: Default::default(),
        }
    }

    pub fn enabled(&self) -> bool {
        self.root.is_some()
    }

    /// The repository the folder is in and its shadow; refused when
    /// checkpoints are off, the folder is not in a repository, or it is too
    /// broad.
    async fn open(&self, cwd: &Path, guard: &jail::Guard) -> Result<G, ApiErr> {
        let Some(root) = &self.root else {
            return Err(err("off", "Checkpoints are turned off on this machine."));
        };
        let top = repo_top(cwd, guard).await?;
        let real_top = top.canonicalize().unwrap_or(top.clone());
        let real_root = real_ancestor(root);
        if real_root.starts_with(&real_top) {
            return Err(err(
                "root_refused",
                "The checkpoints folder is inside this repository, so tmuxedo does not record it.",
            ));
        }
        let hash = hex::encode(Sha256::digest(real_top.as_os_str().as_encoded_bytes()));
        let shadow = root.join(format!("{}.git", &hash[..16]));
        let g = G {
            top: real_top,
            shadow,
            scratch: root.join(format!("{}.tmp", &hash[..16])),
        };
        g.init().await?;
        Ok(g)
    }

    /// Record the folder now as checkpoint `kind` of `session` (`None`
    /// when the folder is the same as the latest turn checkpoint). An
    /// `abandon` flag set meanwhile records nothing.
    pub async fn snapshot(
        &self,
        cwd: &Path,
        guard: &jail::Guard,
        session: &str,
        kind: &str,
        abandon: Option<Arc<AtomicBool>>,
    ) -> Result<Option<Info>, ApiErr> {
        self.snapshot_turn(cwd, guard, session, kind, None, abandon).await
    }

    /// [`Store::snapshot`], naming the chat turn it is taken before. When
    /// the folder is the same as the latest turn checkpoint, nothing new is
    /// recorded but that checkpoint is named for this turn too, so "Restore
    /// to here" on any message finds the folder as it was.
    pub async fn snapshot_turn(
        &self,
        cwd: &Path,
        guard: &jail::Guard,
        session: &str,
        kind: &str,
        turn: Option<&str>,
        abandon: Option<Arc<AtomicBool>>,
    ) -> Result<Option<Info>, ApiErr> {
        let turn = turn.filter(|t| valid_turn(t));
        let g = self.open(cwd, guard).await?;
        let mut caches = self.caches.lock().await;
        let cache = caches.entry(g.shadow.clone()).or_default();
        let snap = g.snapshot(cache).await?;
        let list = g.list(session).await?;
        if kind == proto::KIND_TURN {
            if let Some(last) = list.first() {
                if last.tree == snap.tree && last.kind == proto::KIND_TURN {
                    if let Some(t) = turn {
                        if !last.turns.iter().any(|x| x == t)
                            && !abandon.as_ref().is_some_and(|a| a.load(Ordering::Relaxed))
                        {
                            g.name_turn(session, last, t).await?;
                        }
                    }
                    return Ok(None);
                }
            }
        }
        if abandon.as_ref().is_some_and(|a| a.load(Ordering::Relaxed)) {
            return Ok(None);
        }
        let turns: Vec<String> = turn.map(|t| vec![t.to_string()]).unwrap_or_default();
        let info = g.record(session, kind, &snap, &list, None, turns).await?;
        Ok(Some(info))
    }

    /// The session's checkpoints, newest first.
    pub async fn list(&self, cwd: &Path, guard: &jail::Guard, session: &str) -> Result<Vec<Info>, Unavailable> {
        let g = self.open(cwd, guard).await?;
        let _lock = self.caches.lock().await;
        Ok(g.list(session).await?)
    }

    /// What restoring checkpoint `id` would change now.
    pub async fn preview(&self, cwd: &Path, guard: &jail::Guard, session: &str, id: u64) -> Result<Value, ApiErr> {
        let g = self.open(cwd, guard).await?;
        let mut caches = self.caches.lock().await;
        let cache = caches.entry(g.shadow.clone()).or_default();
        let list = g.list(session).await?;
        let target = find(&list, id)?;
        let now = g.snapshot(cache).await?;
        let want = g.tree_files(target).await?;
        let plan = compare(&now.files, &want, &g.top);
        let total = plan.len();
        let files: Vec<Value> = plan
            .iter()
            .take(PREVIEW_FILES_MAX)
            .map(|c| json!({"path": c.path, "change": c.change.word()}))
            .collect();
        let count = |w: Change| plan.iter().filter(|c| c.change == w).count();
        Ok(json!({
            "type": "checkpoint_preview",
            "checkpoint": target.json(),
            "files": files,
            "total": total,
            "truncated": total > PREVIEW_FILES_MAX,
            "added": count(Change::Added),
            "modified": count(Change::Modified),
            "removed": count(Change::Removed),
        }))
    }

    /// The away digest's facts (`session.summary`): what the folder changed
    /// since the oldest checkpoint at or after `since` (`files`, null when
    /// there is none), and the chat turns those checkpoints were taken
    /// before. Adds no checkpoint: the folder is read through the stat cache.
    pub async fn summary(
        &self,
        cwd: &Path,
        guard: &jail::Guard,
        session: &str,
        since: Option<u64>,
    ) -> Result<Value, Unavailable> {
        let g = self.open(cwd, guard).await?;
        let mut caches = self.caches.lock().await;
        let list = g.list(session).await?;
        let recent: Vec<&Info> = list.iter().filter(|i| since.is_none_or(|t| i.at >= t)).collect();
        let turns: usize = recent.iter().map(|i| i.turns.len()).sum();
        // Newest first: the oldest one since then is the last.
        let Some(first) = recent.last() else {
            return Ok(json!({ "turns": turns, "files": null }));
        };
        let cache = caches.entry(g.shadow.clone()).or_default();
        let now = g.snapshot(cache).await?;
        let then = g.tree_files(first).await?;
        // `compare` is what restoring would do: its "removed" is what the agent added.
        let plan = compare(&now.files, &then, &g.top);
        let count = |w: Change| plan.iter().filter(|c| c.change == w).count();
        Ok(json!({
            "turns": turns,
            "from": first.at,
            "files": {
                "added": count(Change::Removed),
                "modified": count(Change::Modified),
                "removed": count(Change::Added),
            },
        }))
    }

    /// Put the folder back as checkpoint `id` had it, after recording it as
    /// it is now (the answer's `safety`, so the restore can be undone).
    pub async fn restore(&self, cwd: &Path, guard: &jail::Guard, session: &str, id: u64) -> Result<Value, ApiErr> {
        let g = self.open(cwd, guard).await?;
        let mut caches = self.caches.lock().await;
        let cache = caches.entry(g.shadow.clone()).or_default();
        let list = g.list(session).await?;
        let target = find(&list, id)?.clone();
        let now = g.snapshot(cache).await?;
        let safety = g
            .record(session, proto::KIND_RESTORE, &now, &list, Some(id), vec![])
            .await?;
        let want = g.tree_files(&target).await?;
        let plan = compare(&now.files, &want, &g.top);
        let done = g.apply(&plan, &want).await?;
        // The files written are new on disk: their stat entries are stale.
        for c in &plan {
            cache.remove(&c.path);
        }
        let count = |w: Change| plan.iter().filter(|c| c.change == w).count();
        Ok(json!({
            "type": "checkpoint_restored",
            "id": id,
            "safety": safety.id,
            "added": count(Change::Added),
            "modified": count(Change::Modified),
            "removed": count(Change::Removed),
            "conflicts": done.conflicts,
            "conflict_count": done.conflict_n,
        }))
    }
}

impl Store {
    /// Record the folder as it is now (a `discard` checkpoint), then run
    /// `work` with the files that checkpoint holds, under the store's lock
    /// so no turn is recorded halfway through. `work` answers its result
    /// and the files it changed (their stat entries are dropped).
    pub async fn discard_with<F, Fut>(
        &self,
        cwd: &Path,
        guard: &jail::Guard,
        session: &str,
        work: F,
    ) -> Result<Value, ApiErr>
    where
        F: FnOnce(Info, HashSet<String>) -> Fut,
        Fut: std::future::Future<Output = Result<(Value, Vec<String>), ApiErr>>,
    {
        let g = self.open(cwd, guard).await?;
        let mut caches = self.caches.lock().await;
        let cache = caches.entry(g.shadow.clone()).or_default();
        let snap = g.snapshot(cache).await?;
        let list = g.list(session).await?;
        let info = g
            .record(session, proto::KIND_DISCARD, &snap, &list, None, vec![])
            .await?;
        let saved: HashSet<String> = snap.files.keys().cloned().collect();
        let (v, changed) = work(info, saved).await?;
        for p in &changed {
            cache.remove(p);
        }
        Ok(v)
    }
}

fn find(list: &[Info], id: u64) -> Result<&Info, ApiErr> {
    list.iter().find(|i| i.id == id).ok_or_else(|| {
        err(
            "not_found",
            "No such checkpoint in this session (it may have been pruned).",
        )
    })
}

/// The real path of `p`, or of its nearest existing ancestor joined with the rest.
fn real_ancestor(p: &Path) -> PathBuf {
    let mut rest = Vec::new();
    let mut cur = p.to_path_buf();
    loop {
        if let Ok(r) = cur.canonicalize() {
            let mut out = r;
            for c in rest.iter().rev() {
                out.push(c);
            }
            return out;
        }
        match (cur.file_name().map(|s| s.to_os_string()), cur.parent()) {
            (Some(n), Some(par)) => {
                rest.push(n);
                cur = par.to_path_buf();
            }
            _ => return p.to_path_buf(),
        }
    }
}

/// A session key as one ref name component.
pub fn enc(session: &str) -> String {
    let mut s = String::new();
    for b in session.bytes() {
        if b.is_ascii_alphanumeric() || b == b'_' || b == b'-' {
            s.push(b as char);
        } else {
            s.push_str(&format!("%{b:02X}"));
        }
    }
    s
}

// ── comparing ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    Added,
    Modified,
    Removed,
}

impl Change {
    fn word(self) -> &'static str {
        match self {
            Change::Added => "added",
            Change::Modified => "modified",
            Change::Removed => "removed",
        }
    }
}

#[derive(Debug)]
struct Step {
    path: String,
    change: Change,
    /// Only the permission bits differ.
    perm_only: bool,
}

/// What turns `now` into `want`, in path order. A file the checkpoint has
/// that is on disk but outside the snapshot (a large one) counts as modified.
fn compare(now: &BTreeMap<String, Ent>, want: &BTreeMap<String, Ent>, top: &Path) -> Vec<Step> {
    let mut out = Vec::new();
    for (p, e) in now {
        if !want.contains_key(p) {
            out.push(Step {
                path: p.clone(),
                change: Change::Removed,
                perm_only: false,
            });
        } else if let Some(w) = want.get(p) {
            if w.mode != e.mode || w.oid != e.oid {
                out.push(Step {
                    path: p.clone(),
                    change: Change::Modified,
                    perm_only: false,
                });
            } else if w.perm != e.perm {
                out.push(Step {
                    path: p.clone(),
                    change: Change::Modified,
                    perm_only: true,
                });
            }
        }
    }
    for p in want.keys() {
        if !now.contains_key(p) {
            let there = std::fs::symlink_metadata(top.join(p)).is_ok();
            out.push(Step {
                path: p.clone(),
                change: if there { Change::Modified } else { Change::Added },
                perm_only: false,
            });
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

struct Applied {
    conflicts: Vec<String>,
    conflict_n: usize,
}

// ── git ───────────────────────────────────────────────────────────────────

struct Out {
    ok: bool,
    stdout: Vec<u8>,
    stderr: String,
}

fn git_missing() -> ApiErr {
    err("git_missing", "git isn't installed on this machine.")
}

fn failed(what: &str, o: &Out) -> ApiErr {
    crate::log!("checkpoints: git {what} failed: {}", o.stderr);
    err(
        "checkpoint_failed",
        format!("Recording the folder failed (git {what})."),
    )
}

/// One git run: the safe settings first, the inherited `GIT_*` dropped,
/// `env` set, `input` written to stdin, stdout read up to `cap`.
async fn git(
    dir: &Path,
    pre: &[OsString],
    env: &[(&str, OsString)],
    args: &[&str],
    input: Option<Vec<u8>>,
    cap: usize,
) -> Result<Out, ApiErr> {
    let mut c = tokio::process::Command::new("git");
    for (k, _) in std::env::vars_os() {
        if k.to_string_lossy().starts_with("GIT_") {
            c.env_remove(&k);
        }
    }
    c.args(SAFE)
        .args([
            "-c",
            "commit.gpgSign=false",
            "-c",
            "gc.autoDetach=false",
            "-c",
            "core.autocrlf=false",
        ])
        .arg("-C")
        .arg(dir)
        .args(pre)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_PAGER", "cat")
        .env("GIT_EDITOR", ":")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("LC_ALL", "C")
        .stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for (k, v) in env {
        c.env(k, v);
    }
    let mut child = c.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            git_missing()
        } else {
            err("git_failed", "git could not be started on this machine.")
        }
    })?;
    let stdin = child.stdin.take();
    let (so, se) = (child.stdout.take().expect("piped"), child.stderr.take().expect("piped"));
    let work = async {
        let write = async move {
            if let (Some(mut w), Some(bytes)) = (stdin, input) {
                let _ = w.write_all(&bytes).await;
                let _ = w.shutdown().await;
            }
        };
        let read_out = async move {
            let mut out = Vec::new();
            let _ = so.take(cap as u64 + 1).read_to_end(&mut out).await;
            out
        };
        let read_err = async move {
            let mut e = Vec::new();
            let _ = se.take(4096).read_to_end(&mut e).await;
            e
        };
        let (_, out, errb) = tokio::join!(write, read_out, read_err);
        let cut = out.len() > cap;
        if cut {
            let _ = child.start_kill();
        }
        let status = child.wait().await.ok();
        (out, errb, cut, status)
    };
    let (stdout, errb, cut, status) = tokio::time::timeout(GIT_TIME, work)
        .await
        .map_err(|_| err("git_timeout", "git took too long on this machine."))?;
    Ok(Out {
        ok: !cut && status.is_some_and(|s| s.success()),
        stdout,
        stderr: String::from_utf8_lossy(&errb).trim().to_string(),
    })
}

const BIG: usize = 512 * 1024 * 1024;
const SMALL: usize = 64 * 1024;

/// A repository and its shadow.
struct G {
    /// The user's repository top (its real path).
    top: PathBuf,
    /// The shadow repository (bare).
    shadow: PathBuf,
    /// Scratch room: the temporary index, symlink targets.
    scratch: PathBuf,
}

impl G {
    fn pre(&self) -> Vec<OsString> {
        let mut g = OsString::from("--git-dir=");
        g.push(&self.shadow);
        vec![g]
    }

    /// Git in the shadow, from the repository's top (paths are relative to it).
    async fn sh(&self, args: &[&str], input: Option<Vec<u8>>, cap: usize) -> Result<Out, ApiErr> {
        git(&self.top, &self.pre(), &[], args, input, cap).await
    }

    async fn sh_ok(&self, args: &[&str], input: Option<Vec<u8>>, cap: usize) -> Result<Vec<u8>, ApiErr> {
        let o = self.sh(args, input, cap).await?;
        if !o.ok {
            return Err(failed(args[0], &o));
        }
        Ok(o.stdout)
    }

    async fn init(&self) -> Result<(), ApiErr> {
        if self.shadow.join("HEAD").is_file() {
            return Ok(());
        }
        let parent = self.shadow.parent().unwrap_or(Path::new("/"));
        std::fs::create_dir_all(parent).map_err(|e| {
            err(
                "checkpoint_failed",
                format!("The checkpoints folder could not be made: {e}"),
            )
        })?;
        let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        let o = git(
            parent,
            &[],
            &[],
            &["init", "--bare", "--template=", "-q", &self.shadow.to_string_lossy()],
            None,
            SMALL,
        )
        .await?;
        if !o.ok {
            return Err(failed("init", &o));
        }
        // gc keeps nothing it does not need for long.
        for (k, v) in [
            ("gc.pruneExpire", "1.day.ago"),
            ("gc.reflogExpire", "now"),
            ("core.logAllRefUpdates", "false"),
        ] {
            let _ = self.sh(&["config", k, v], None, SMALL).await;
        }
        Ok(())
    }

    /// Read the folder (tracked and untracked-not-ignored files) and write
    /// its blobs and tree to the shadow. Records no ref.
    async fn snapshot(&self, cache: &mut Cache) -> Result<Snap, ApiErr> {
        let mut names: Vec<String> = Vec::new();
        let mut skipped = Skipped::default();
        let mut seen = HashSet::new();
        let tracked = git(&self.top, &[], &[], &["ls-files", "-z", "--cached"], None, BIG).await?;
        if !tracked.ok {
            return Err(failed("ls-files", &tracked));
        }
        let mut others_args = vec!["ls-files", "-z", "--others", "--exclude-standard"];
        for d in EXCLUDED_DIRS {
            others_args.push("-x");
            others_args.push(d);
        }
        let others = git(&self.top, &[], &[], &others_args, None, BIG).await?;
        if !others.ok {
            return Err(failed("ls-files", &others));
        }
        for raw in tracked
            .stdout
            .split(|b| *b == 0)
            .chain(others.stdout.split(|b| *b == 0))
        {
            if raw.is_empty() || raw.ends_with(b"/") {
                continue;
            }
            let Ok(name) = std::str::from_utf8(raw) else {
                skipped.other += 1;
                continue;
            };
            if name.chars().any(|c| c.is_control()) {
                skipped.other += 1;
                continue;
            }
            if seen.insert(name.to_string()) {
                names.push(name.to_string());
            }
        }
        if names.len() > FILES_MAX {
            return Err(err(
                "too_many_files",
                format!("This folder has more than {FILES_MAX} files, too many to record."),
            ));
        }
        let now_ns = (crate::util::now_ms() as i128) * 1_000_000;
        let mut files = BTreeMap::new();
        // Files to hash: (name, stamp, link target bytes).
        let mut todo: Vec<(String, Stamp, u32, Option<Vec<u8>>)> = Vec::new();
        for name in names {
            if jail::secret_reason(&name).is_some() {
                skipped.secret += 1;
                continue;
            }
            let path = self.top.join(&name);
            let Ok(md) = std::fs::symlink_metadata(&path) else {
                continue; // deleted in the index, or gone meanwhile
            };
            let ft = md.file_type();
            let (mode, link) = if ft.is_symlink() {
                match std::fs::read_link(&path) {
                    Ok(t) => (0o120000, Some(t.into_os_string().into_encoded_bytes())),
                    Err(_) => {
                        skipped.other += 1;
                        continue;
                    }
                }
            } else if ft.is_file() {
                if md.len() > LARGE_FILE {
                    skipped.large_n += 1;
                    if skipped.large.len() < NAMES_MAX {
                        skipped.large.push(name);
                    }
                    continue;
                }
                if std::fs::File::open(&path).is_err() {
                    skipped.other += 1;
                    continue;
                }
                (if md.mode() & 0o100 != 0 { 0o100755 } else { 0o100644 }, None)
            } else {
                continue; // a folder (a nested repository), a socket, a device
            };
            let perm = if mode == 0o120000 { 0 } else { md.mode() & 0o7777 };
            let stamp = Stamp {
                size: md.len(),
                mtime_ns: md.mtime() as i128 * 1_000_000_000 + md.mtime_nsec() as i128,
                ctime_ns: md.ctime() as i128 * 1_000_000_000 + md.ctime_nsec() as i128,
                ino: md.ino(),
                mode,
            };
            if let Some((s, oid)) = cache.get(&name) {
                if *s == stamp {
                    files.insert(
                        name,
                        Ent {
                            mode,
                            oid: oid.clone(),
                            perm,
                        },
                    );
                    continue;
                }
            }
            todo.push((name, stamp, perm, link));
        }
        if !todo.is_empty() {
            let _ = std::fs::create_dir_all(&self.scratch);
            let mut input = Vec::new();
            let mut links = 0usize;
            for (name, _, _, link) in &todo {
                if let Some(target) = link {
                    let p = self.scratch.join(format!("link-{links}"));
                    links += 1;
                    std::fs::write(&p, target)
                        .map_err(|e| err("checkpoint_failed", format!("The checkpoints folder: {e}")))?;
                    input.extend_from_slice(&quote(&p.to_string_lossy()));
                } else {
                    input.extend_from_slice(&quote(name));
                }
                input.push(b'\n');
            }
            let out = self
                .sh_ok(
                    &["hash-object", "-w", "--no-filters", "--stdin-paths"],
                    Some(input),
                    BIG,
                )
                .await?;
            let oids: Vec<&str> = std::str::from_utf8(&out).unwrap_or("").lines().collect();
            if oids.len() != todo.len() {
                return Err(err(
                    "checkpoint_failed",
                    "Recording the folder failed (a file changed while it was read).",
                ));
            }
            for ((name, stamp, perm, _), oid) in todo.into_iter().zip(oids) {
                if cache.len() < CACHE_MAX
                    && now_ns - stamp.mtime_ns > RACY_MS as i128 * 1_000_000
                    && now_ns - stamp.ctime_ns > RACY_MS as i128 * 1_000_000
                {
                    cache.insert(name.clone(), (stamp, oid.to_string()));
                }
                files.insert(
                    name,
                    Ent {
                        mode: stamp.mode,
                        oid: oid.to_string(),
                        perm,
                    },
                );
            }
        }
        let tree = self.write_tree(&files).await?;
        Ok(Snap {
            id: None,
            tree,
            files,
            skipped,
        })
    }

    /// A tree of `files`, built in a temporary index.
    async fn write_tree(&self, files: &BTreeMap<String, Ent>) -> Result<String, ApiErr> {
        let _ = std::fs::create_dir_all(&self.scratch);
        let index = self.scratch.join(format!("index-{}", std::process::id()));
        let _ = std::fs::remove_file(&index);
        let mut input = Vec::new();
        for (name, e) in files {
            input.extend_from_slice(format!("{:o} {}\t{}", e.mode, e.oid, name).as_bytes());
            input.push(0);
        }
        let mut wt = OsString::from("--work-tree=");
        wt.push(&self.scratch);
        let pre = vec![self.pre()[0].clone(), wt];
        let env = [("GIT_INDEX_FILE", index.clone().into_os_string())];
        let o = git(
            &self.scratch,
            &pre,
            &env,
            &["update-index", "-z", "--index-info"],
            Some(input),
            SMALL,
        )
        .await?;
        if !o.ok {
            let _ = std::fs::remove_file(&index);
            return Err(failed("update-index", &o));
        }
        let o = git(&self.scratch, &pre, &env, &["write-tree"], None, SMALL).await?;
        let _ = std::fs::remove_file(&index);
        if !o.ok {
            return Err(failed("write-tree", &o));
        }
        Ok(String::from_utf8_lossy(&o.stdout).trim().to_string())
    }

    /// The session's checkpoints, newest first.
    async fn list(&self, session: &str) -> Result<Vec<Info>, ApiErr> {
        let prefix = format!("refs/sessions/{}/", enc(session));
        let out = self
            .sh_ok(
                &[
                    "for-each-ref",
                    "--format=%(refname)%00%(tree)%00%(contents:body)%1e",
                    &prefix,
                ],
                None,
                BIG,
            )
            .await?;
        let mut v = Vec::new();
        for rec in out.split(|b| *b == 0x1e) {
            let rec = String::from_utf8_lossy(rec);
            let rec = rec.trim_start_matches('\n');
            let mut parts = rec.splitn(3, '\0');
            let (Some(refname), Some(tree), Some(body)) = (parts.next(), parts.next(), parts.next()) else {
                continue;
            };
            let Some(id) = refname.strip_prefix(&prefix).and_then(|n| n.parse::<u64>().ok()) else {
                continue;
            };
            let m: Value = serde_json::from_str(body.trim()).unwrap_or(Value::Null);
            let exact = m["exact"]
                .as_object()
                .map(|o| {
                    o.iter()
                        .filter_map(|(k, v)| v.as_u64().map(|n| (k.clone(), n as u32)))
                        .collect()
                })
                .unwrap_or_default();
            v.push(Info {
                id,
                refname: refname.to_string(),
                tree: tree.to_string(),
                kind: m["kind"].as_str().unwrap_or(proto::KIND_TURN).to_string(),
                at: m["at"].as_u64().unwrap_or(0),
                files: m["files"].as_u64().unwrap_or(0),
                restoring: m["restoring"].as_u64(),
                turns: m["turns"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|t| t.as_str().filter(|t| valid_turn(t)).map(str::to_owned))
                            .collect()
                    })
                    .unwrap_or_default(),
                exact,
                skipped: m["skipped"].clone(),
            });
        }
        v.sort_by_key(|i| std::cmp::Reverse(i.id));
        Ok(v)
    }

    /// Record `snap` as the next checkpoint of `session`, then prune.
    async fn record(
        &self,
        session: &str,
        kind: &str,
        snap: &Snap,
        list: &[Info],
        restoring: Option<u64>,
        turns: Vec<String>,
    ) -> Result<Info, ApiErr> {
        let id = list.first().map(|i| i.id + 1).unwrap_or(1);
        let at = crate::util::now_ms();
        let exact: Map<String, Value> = snap
            .files
            .iter()
            .filter(|(_, e)| e.mode != 0o120000 && e.perm != canonical_perm(e.mode))
            .map(|(p, e)| (p.clone(), json!(e.perm)))
            .collect();
        let mut body = json!({
            "v": 1, "kind": kind, "session": session, "at": at,
            "files": snap.files.len(), "exact": exact, "skipped": snap.skipped.json(),
        });
        if let Some(r) = restoring {
            body["restoring"] = json!(r);
        }
        if !turns.is_empty() {
            body["turns"] = json!(turns);
        }
        let refname = format!("refs/sessions/{}/{id:06}", enc(session));
        self.write_checkpoint(&refname, &snap.tree, id, kind, &body, "").await?;
        let info = Info {
            id,
            refname,
            tree: snap.tree.clone(),
            kind: kind.to_string(),
            at,
            files: snap.files.len() as u64,
            restoring,
            turns,
            exact: exact
                .iter()
                .map(|(k, v)| (k.clone(), v.as_u64().unwrap_or(0) as u32))
                .collect(),
            skipped: snap.skipped.json(),
        };
        self.prune(at, list).await;
        Ok(info)
    }

    /// Name checkpoint `last` for chat turn `turn` as well: the same tree in
    /// a new parentless commit whose body lists the turn, swapped in under
    /// the same ref (only if the ref has not moved meanwhile).
    async fn name_turn(&self, session: &str, last: &Info, turn: &str) -> Result<(), ApiErr> {
        let mut turns = last.turns.clone();
        turns.push(turn.to_string());
        let cut = turns.len().saturating_sub(TURNS_MAX);
        turns.drain(..cut);
        let exact: Map<String, Value> = last.exact.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
        let mut body = json!({
            "v": 1, "kind": last.kind, "session": session, "at": last.at,
            "files": last.files, "exact": exact, "skipped": last.skipped, "turns": turns,
        });
        if let Some(r) = last.restoring {
            body["restoring"] = json!(r);
        }
        let old = self
            .sh_ok(&["rev-parse", "--verify", &last.refname], None, SMALL)
            .await?;
        let old = String::from_utf8_lossy(&old).trim().to_string();
        self.write_checkpoint(&last.refname, &last.tree, last.id, &last.kind, &body, &old)
            .await
    }

    /// One parentless commit of `tree` with the manifest `body`, set as
    /// `refname` (`old`: the ref's value it must still have; "" = new).
    async fn write_checkpoint(
        &self,
        refname: &str,
        tree: &str,
        id: u64,
        kind: &str,
        body: &Value,
        old: &str,
    ) -> Result<(), ApiErr> {
        let msg = format!("checkpoint {id} ({kind})\n\n{body}\n");
        let env = [
            ("GIT_AUTHOR_NAME", OsString::from(IDENT)),
            ("GIT_AUTHOR_EMAIL", OsString::from(IDENT_EMAIL)),
            ("GIT_COMMITTER_NAME", OsString::from(IDENT)),
            ("GIT_COMMITTER_EMAIL", OsString::from(IDENT_EMAIL)),
        ];
        // No parent: a pruned checkpoint must not stay reachable.
        let o = git(
            &self.top,
            &self.pre(),
            &env,
            &["commit-tree", tree, "-F", "-"],
            Some(msg.into_bytes()),
            SMALL,
        )
        .await?;
        if !o.ok {
            return Err(failed("commit-tree", &o));
        }
        let commit = String::from_utf8_lossy(&o.stdout).trim().to_string();
        self.sh_ok(&["update-ref", refname, &commit, old], None, SMALL).await?;
        Ok(())
    }

    /// Keep the newest [`KEEP`] and nothing older than [`KEEP_MS`] (the
    /// newest always stays); let git collect what nothing points at.
    async fn prune(&self, now: u64, older: &[Info]) {
        let mut gone = 0;
        for (i, c) in older.iter().enumerate() {
            let rank = i + 1; // the newest is rank 0
            let old = rank >= KEEP || now.saturating_sub(c.at) > KEEP_MS;
            if old
                && self
                    .sh(&["update-ref", "-d", &c.refname], None, SMALL)
                    .await
                    .is_ok_and(|o| o.ok)
            {
                gone += 1;
            }
        }
        if gone > 0 {
            let _ = self.sh(&["gc", "--auto", "--quiet"], None, SMALL).await;
        }
    }

    /// The files a checkpoint holds, with their exact permission bits.
    async fn tree_files(&self, c: &Info) -> Result<BTreeMap<String, Ent>, ApiErr> {
        let out = self
            .sh_ok(&["ls-tree", "-r", "-z", "--full-tree", &c.tree], None, BIG)
            .await?;
        let mut files = BTreeMap::new();
        for rec in out.split(|b| *b == 0) {
            let Some(tab) = rec.iter().position(|b| *b == b'\t') else {
                continue;
            };
            let head = String::from_utf8_lossy(&rec[..tab]);
            let name = String::from_utf8_lossy(&rec[tab + 1..]).to_string();
            let mut f = head.split(' ');
            let (Some(mode), Some("blob"), Some(oid)) = (f.next(), f.next(), f.next()) else {
                continue;
            };
            let Ok(mode) = u32::from_str_radix(mode, 8) else {
                continue;
            };
            let perm = c.exact.get(&name).copied().unwrap_or(canonical_perm(mode));
            files.insert(
                name,
                Ent {
                    mode,
                    oid: oid.to_string(),
                    perm,
                },
            );
        }
        Ok(files)
    }

    /// Carry out a plan: removals first, then the files, then the bits.
    async fn apply(&self, plan: &[Step], want: &BTreeMap<String, Ent>) -> Result<Applied, ApiErr> {
        let mut conflicts = Vec::new();
        let mut conflict_n = 0;
        let mut conflict = |p: &str| {
            conflict_n += 1;
            if conflicts.len() < NAMES_MAX {
                conflicts.push(p.to_string());
            }
        };
        for s in plan.iter().filter(|s| s.change == Change::Removed) {
            let path = self.top.join(&s.path);
            if !safe_parents(&self.top, &s.path) {
                conflict(&s.path);
                continue;
            }
            match std::fs::symlink_metadata(&path) {
                Ok(m) if !m.is_dir() => {
                    if std::fs::remove_file(&path).is_err() {
                        conflict(&s.path);
                    } else {
                        prune_dirs(&self.top, &path);
                    }
                }
                Ok(_) => conflict(&s.path),
                Err(_) => {}
            }
        }
        // Content: read every blob needed through one cat-file.
        let writes: Vec<&Step> = plan
            .iter()
            .filter(|s| s.change != Change::Removed && !s.perm_only)
            .collect();
        if !writes.is_empty() {
            let mut cat = Cat::start(self).await?;
            for s in writes {
                let e = &want[&s.path];
                let bytes = cat.blob(&e.oid).await?;
                if !self.write_file(&s.path, e, &bytes) {
                    conflict(&s.path);
                }
            }
            cat.finish().await;
        }
        for s in plan.iter().filter(|s| s.perm_only) {
            let e = &want[&s.path];
            let path = self.top.join(&s.path);
            if !safe_parents(&self.top, &s.path)
                || std::fs::set_permissions(&path, std::fs::Permissions::from_mode(e.perm)).is_err()
            {
                conflict(&s.path);
            }
        }
        Ok(Applied { conflicts, conflict_n })
    }

    /// Write one file through a temporary file and a rename; false when the
    /// place is taken by a folder or a parent is not a real folder.
    fn write_file(&self, rel: &str, e: &Ent, bytes: &[u8]) -> bool {
        let path = self.top.join(rel);
        let Some(dir) = path.parent() else { return false };
        if !make_parents(&self.top, rel) {
            return false;
        }
        if std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir()) {
            return false;
        }
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let tmp = dir.join(format!(".{name}.tmuxedo-restore-{}", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        let made = if e.mode == 0o120000 {
            use std::os::unix::ffi::OsStrExt;
            std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(bytes), &tmp).is_ok()
        } else {
            use std::io::Write;
            let f = std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp);
            match f {
                Ok(mut f) => {
                    f.write_all(bytes).is_ok() && f.set_permissions(std::fs::Permissions::from_mode(e.perm)).is_ok()
                }
                Err(_) => false,
            }
        };
        if !made || std::fs::rename(&tmp, &path).is_err() {
            let _ = std::fs::remove_file(&tmp);
            return false;
        }
        true
    }
}

/// Every folder between the top and `rel` is a real folder (not a symlink).
pub(crate) fn safe_parents(top: &Path, rel: &str) -> bool {
    let mut cur = top.to_path_buf();
    let parts: Vec<&str> = rel.split('/').collect();
    for p in &parts[..parts.len().saturating_sub(1)] {
        if p.is_empty() || *p == "." || *p == ".." {
            return false;
        }
        cur.push(p);
        match std::fs::symlink_metadata(&cur) {
            Ok(m) if m.is_dir() => {}
            _ => return false,
        }
    }
    true
}

/// Make the folders `rel` needs, refusing a symlink or a file on the way.
fn make_parents(top: &Path, rel: &str) -> bool {
    let mut cur = top.to_path_buf();
    let parts: Vec<&str> = rel.split('/').collect();
    for p in &parts[..parts.len().saturating_sub(1)] {
        if p.is_empty() || *p == "." || *p == ".." {
            return false;
        }
        cur.push(p);
        match std::fs::symlink_metadata(&cur) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => return false,
            Err(_) => {
                if std::fs::create_dir(&cur).is_err() {
                    return false;
                }
            }
        }
    }
    true
}

/// Remove the folders a removal left empty, up to the top.
pub(crate) fn prune_dirs(top: &Path, file: &Path) {
    let mut d = file.parent();
    while let Some(dir) = d {
        if dir == top || !dir.starts_with(top) || std::fs::remove_dir(dir).is_err() {
            break;
        }
        d = dir.parent();
    }
}

/// A name for `--stdin-paths`: quoted when it starts with a quote.
fn quote(name: &str) -> Vec<u8> {
    if !name.starts_with('"') {
        return name.as_bytes().to_vec();
    }
    let mut s = String::from("\"");
    for c in name.chars() {
        match c {
            '"' => s.push_str("\\\""),
            '\\' => s.push_str("\\\\"),
            c => s.push(c),
        }
    }
    s.push('"');
    s.into_bytes()
}

/// A `cat-file --batch` in the shadow, one blob at a time.
struct Cat {
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    out: BufReader<tokio::process::ChildStdout>,
}

impl Cat {
    async fn start(g: &G) -> Result<Cat, ApiErr> {
        let mut c = tokio::process::Command::new("git");
        for (k, _) in std::env::vars_os() {
            if k.to_string_lossy().starts_with("GIT_") {
                c.env_remove(&k);
            }
        }
        c.args(SAFE)
            .arg("-C")
            .arg(&g.top)
            .args(g.pre())
            .args(["cat-file", "--batch"])
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("LC_ALL", "C")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = c.spawn().map_err(|_| git_missing())?;
        let stdin = child.stdin.take().expect("piped");
        let out = BufReader::new(child.stdout.take().expect("piped"));
        Ok(Cat { child, stdin, out })
    }

    async fn blob(&mut self, oid: &str) -> Result<Vec<u8>, ApiErr> {
        let bad = || err("checkpoint_failed", "A file of the checkpoint could not be read.");
        let work = async {
            self.stdin
                .write_all(format!("{oid}\n").as_bytes())
                .await
                .map_err(|_| bad())?;
            self.stdin.flush().await.map_err(|_| bad())?;
            let mut head = String::new();
            self.out.read_line(&mut head).await.map_err(|_| bad())?;
            let mut f = head.split_whitespace();
            let (Some(_), Some("blob"), Some(size)) = (f.next(), f.next(), f.next()) else {
                return Err(bad());
            };
            let size: usize = size.parse().map_err(|_| bad())?;
            let mut buf = vec![0u8; size + 1];
            self.out.read_exact(&mut buf).await.map_err(|_| bad())?;
            buf.pop();
            Ok(buf)
        };
        tokio::time::timeout(GIT_TIME, work)
            .await
            .map_err(|_| err("git_timeout", "git took too long on this machine."))?
    }

    async fn finish(mut self) {
        drop(self.stdin);
        let _ = tokio::time::timeout(Duration::from_secs(5), self.child.wait()).await;
    }
}
