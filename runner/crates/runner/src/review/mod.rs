//! Review and ship, the reading half (docs/design/away-and-review.md §3,
//! §4.2, §5): `git.status` and `git.diff` for a session's work folder.
//!
//! WHICH FOLDER. A request names a session (a pane or a chat); the folder
//! is the one the runner records for it, and the repository is the one git
//! finds there. A repository too broad to show (the home folder, `/`, the
//! runner's own folders: the file browser's rule) is refused. A file is
//! diffed only when this runner's own status just listed it.
//!
//! WHO. The gateway only. On the machine's local socket every `git.*`
//! method is `not_supported` (an agent has git itself; the review is for
//! the person). Commit and push are the shipping half ([`ship`]); discard
//! is not built: `not_supported`.
//!
//! GIT IS HOSTILE INPUT. The agent under review can write the repository's
//! own config, so nothing in it may run a program here:
//! - argv only, never a shell; fixed flags; a file name after `--`, with
//!   `--literal-pathspecs` so `*` or `:(...)` in a name is just a name;
//! - `core.fsmonitor=false` (a configured fsmonitor is a program `status`
//!   runs), `core.hooksPath=/dev/null`, `--no-pager` and `core.pager=cat`,
//!   `--no-ext-diff` and `--no-textconv` (`diff.external`, `diff.*.command`,
//!   `diff.*.textconv` are programs), `--ignore-submodules=all` (a
//!   submodule's own config would apply there), `protocol.ext.allow=never`;
//! - every clean/smudge/process filter the repository's own config (local
//!   or worktree scope, includes followed) names is blanked with `-c` (an
//!   empty command is no filter) and made optional;
//! - inherited `GIT_*` variables are dropped (`GIT_DIR`, `GIT_EXTERNAL_DIFF`,
//!   `GIT_CONFIG_PARAMETERS` …), `GIT_TERMINAL_PROMPT=0`, no optional locks;
//! - a time limit, and every output read up to a cap, then the process is
//!   killed; a patch is cut at [`PATCH_MAX`] and masked like every output.
//!
//! The machine's own (global) git config is the owner's and stays in force.
//! Secret-named files (the file browser's rule) are counted, never shown.

pub mod ship;
#[cfg(test)]
mod tests;

use crate::api::{err, ApiErr, ApiResult};
use crate::chat::mask::Masker;
use crate::jail;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tmuxedo_protocol::fs::path_components;
use tmuxedo_protocol::review::{
    self as proto, ReadParams, ShipParams, Target, LARGE_LINES, PATCH_MAX, STATUS_FILES_MAX,
};
use tokio::io::AsyncReadExt;

/// One git run at most.
pub const GIT_TIME: Duration = Duration::from_secs(10);
/// `git status -z` read at most (bytes).
const STATUS_OUT_MAX: usize = 4 * 1024 * 1024;
/// Small answers (rev-parse, config names) read at most (bytes).
const SMALL_OUT_MAX: usize = 256 * 1024;
/// An untracked file larger than this is listed without a diff or counts.
pub const UNTRACKED_MAX: u64 = 8 * 1024 * 1024;
/// Untracked files counted (`+n`) at most; the rest are listed without counts.
const UNTRACKED_COUNTED_MAX: usize = 100;
/// Requests being answered at once, at most.
pub const BUSY_MAX: usize = 4;

/// The settings every git run gets, before anything the repository says.
pub(crate) const SAFE: &[&str] = &[
    "-c",
    "core.fsmonitor=false",
    "-c",
    "core.hooksPath=/dev/null",
    "-c",
    "core.pager=cat",
    "-c",
    "core.untrackedCache=false",
    "-c",
    "core.quotePath=false",
    "-c",
    "color.ui=false",
    "-c",
    "protocol.ext.allow=never",
    "--no-pager",
    "--no-optional-locks",
    "--literal-pathspecs",
];

/// What one request works on.
#[derive(Clone)]
pub struct Ctx {
    /// The session's work folder (the runner's record).
    pub cwd: PathBuf,
    pub guard: jail::Guard,
    pub masker: Masker,
    /// The machine's own gh (`gh`, found on the runner's PATH).
    pub gh: PathBuf,
    /// Set on every git and gh run (empty in the daemon; tests' own config).
    pub env: Vec<(String, String)>,
}

/// The answer of one git run.
pub(crate) struct Out {
    ok: bool,
    code: Option<i32>,
    stdout: Vec<u8>,
    cut: bool,
    stderr: String,
}

/// A repository as one request sees it: where, and what to blank.
struct Repo {
    top: PathBuf,
    /// `-c` pairs that neutralise the repository's own filter programs.
    blank: Vec<String>,
    /// Every setting name the repository's own config (local or worktree
    /// scope, includes followed) has, lowercased: commit and push refuse some.
    names: Vec<String>,
}

fn git_missing() -> ApiErr {
    err("git_missing", "git isn't installed on this machine.")
}

/// How one program run is bounded and fed.
#[derive(Default)]
pub(crate) struct Opts<'a> {
    /// Stdout is read up to this many bytes, then the program is stopped.
    pub cap: usize,
    /// Written to stdin (otherwise stdin is empty).
    pub input: Option<&'a [u8]>,
    /// The time limit ([`GIT_TIME`] when unset).
    pub time: Option<Duration>,
    /// Set after the inherited `GIT_*` variables are dropped (tests point
    /// `GIT_CONFIG_GLOBAL` at a config of their own).
    pub env: &'a [(String, String)],
}

/// Run git in `dir` with the safe settings, `extra` and `args`; read stdout
/// up to `cap` bytes (then stop it), stderr up to 4 KiB.
async fn git(dir: &Path, extra: &[String], args: &[&str], cap: usize) -> Result<Out, ApiErr> {
    git_with(dir, extra, args, &Opts { cap, ..Opts::default() }).await
}

/// [`git`] with stdin, a time limit and variables of its own.
pub(crate) async fn git_with(dir: &Path, extra: &[String], args: &[&str], o: &Opts<'_>) -> Result<Out, ApiErr> {
    let mut c = tokio::process::Command::new("git");
    c.args(SAFE).args(extra).arg("-C").arg(dir).args(args);
    exec(c, o, git_missing, "git").await
}

/// Drop the inherited `GIT_*` variables and set the quiet ones; then `o.env`.
pub(crate) fn quiet_env(c: &mut tokio::process::Command, o: &Opts<'_>) {
    for (k, _) in std::env::vars_os() {
        if k.to_string_lossy().starts_with("GIT_") {
            c.env_remove(&k);
        }
    }
    c.env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_PAGER", "cat")
        .env("PAGER", "cat")
        .env("GIT_EDITOR", ":")
        .env("LC_ALL", "C");
    c.envs(o.env.iter().map(|(k, v)| (k, v)));
}

/// Start `c` (its program and arguments set), feed and read it within the
/// bounds; `missing` answers a program that isn't there.
pub(crate) async fn exec(
    mut c: tokio::process::Command,
    o: &Opts<'_>,
    missing: fn() -> ApiErr,
    what: &'static str,
) -> Result<Out, ApiErr> {
    use tokio::io::AsyncWriteExt;
    quiet_env(&mut c, o);
    c.stdin(if o.input.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .kill_on_drop(true);
    let mut child = c.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            missing()
        } else {
            err("git_failed", format!("{what} could not be started on this machine."))
        }
    })?;
    let input = o.input.map(|b| b.to_vec());
    let stdin = child.stdin.take();
    let (so, se) = (child.stdout.take().expect("piped"), child.stderr.take().expect("piped"));
    let cap = o.cap;
    let work = async {
        let mut out = Vec::new();
        let mut errb = Vec::new();
        let feed = async move {
            if let (Some(mut w), Some(b)) = (stdin, input) {
                let _ = w.write_all(&b).await;
                // Dropped here: the program sees the end of its input.
            }
        };
        // Each pipe is dropped once read, so a program cut short gets EPIPE and ends.
        let read_out = async move {
            let _ = so.take(cap as u64 + 1).read_to_end(&mut out).await;
            out
        };
        let read_err = async move {
            let _ = se.take(4096).read_to_end(&mut errb).await;
            errb
        };
        let ((), mut out, errb) = tokio::join!(feed, read_out, read_err);
        let cut = out.len() > cap;
        if cut {
            out.truncate(cap);
            let _ = child.start_kill();
        }
        let status = child.wait().await.ok();
        (out, errb, cut, status)
    };
    let (stdout, errb, cut, status) = tokio::time::timeout(o.time.unwrap_or(GIT_TIME), work)
        .await
        .map_err(|_| err("git_timeout", format!("{what} took too long on this machine.")))?;
    Ok(Out {
        ok: !cut && status.is_some_and(|s| s.success()),
        code: status.and_then(|s| s.code()),
        stdout,
        cut,
        stderr: String::from_utf8_lossy(&errb).trim().to_string(),
    })
}

/// The top of the repository a session's folder is in, refused when the
/// folder is gone, is not in a repository, or the repository is too broad
/// (the home folder, `/`, the runner's own folders). Shared with the
/// checkpoints (crate::checkpoint).
pub(crate) async fn repo_top(cwd: &Path, guard: &jail::Guard) -> Result<PathBuf, ApiErr> {
    if !cwd.is_absolute() || !cwd.is_dir() {
        return Err(err("folder_gone", "This session's folder is not there any more."));
    }
    let o = git(
        cwd,
        &[],
        &["rev-parse", "--path-format=absolute", "--show-toplevel"],
        SMALL_OUT_MAX,
    )
    .await?;
    if !o.ok {
        if o.stderr.contains("dubious ownership") {
            return Err(err(
                "not_a_repo",
                "This folder's repository belongs to another user, so git won't read it.",
            ));
        }
        return Err(err(
            "not_a_repo",
            "This folder isn't a git repository: changes need git.",
        ));
    }
    let top = PathBuf::from(String::from_utf8_lossy(&o.stdout).trim());
    let real = top.canonicalize().unwrap_or(top.clone());
    if jail::too_broad(&real, guard) {
        return Err(err(
            "root_refused",
            "This repository covers too much of the machine (a home folder or a system folder), so tmuxedo does not show its changes.",
        ));
    }
    Ok(top)
}

/// The repository the session's folder is in, refused when too broad, and
/// the `-c` pairs that blank its own filters.
async fn open_repo(ctx: &Ctx) -> Result<Repo, ApiErr> {
    let top = repo_top(&ctx.cwd, &ctx.guard).await?;
    let mut blank = Vec::new();
    let mut names = Vec::new();
    for scope in ["--local", "--worktree"] {
        let o = git(
            &top,
            &[],
            &["config", "--includes", "--name-only", scope, "--list", "-z"],
            SMALL_OUT_MAX,
        )
        .await?;
        if !o.ok {
            continue;
        }
        for name in o.stdout.split(|b| *b == 0).filter(|n| !n.is_empty()) {
            let name = String::from_utf8_lossy(name).trim().to_ascii_lowercase();
            if !names.contains(&name) {
                names.push(name.clone());
            }
            let Some(rest) = name.strip_prefix("filter.") else {
                continue;
            };
            let Some((driver, key)) = rest.rsplit_once('.') else {
                continue;
            };
            if !matches!(key, "clean" | "smudge" | "process") {
                continue;
            }
            for k in ["clean", "smudge", "process"] {
                let pair = format!("filter.{driver}.{k}=");
                if !blank.contains(&pair) {
                    blank.push(pair);
                }
            }
            let req = format!("filter.{driver}.required=false");
            if !blank.contains(&req) {
                blank.push(req);
            }
        }
    }
    let blank = blank.into_iter().flat_map(|p| ["-c".to_string(), p]).collect();
    Ok(Repo { top, blank, names })
}

/// What the work is compared with: HEAD, or the empty tree before the first commit.
async fn base(repo: &Repo) -> Result<(String, Option<String>), ApiErr> {
    let o = git(
        &repo.top,
        &repo.blank,
        &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"],
        SMALL_OUT_MAX,
    )
    .await?;
    let head = String::from_utf8_lossy(&o.stdout).trim().to_string();
    if o.ok && !head.is_empty() {
        return Ok((head.clone(), Some(head)));
    }
    let o = git(
        &repo.top,
        &repo.blank,
        &["hash-object", "-t", "tree", "/dev/null"],
        SMALL_OUT_MAX,
    )
    .await?;
    let empty = String::from_utf8_lossy(&o.stdout).trim().to_string();
    if !o.ok || empty.is_empty() {
        return Err(err("git_failed", "git could not read this repository."));
    }
    Ok((empty, None))
}

/// One changed file.
#[derive(Debug, Clone)]
struct Changed {
    path: String,
    /// `M` changed, `A` added, `D` deleted, `?` new and not added yet, `U` in conflict, `T` type changed.
    status: char,
    add: Option<u64>,
    del: Option<u64>,
    binary: bool,
    /// Why there is no diff to show, when there isn't.
    why: Option<&'static str>,
}

struct Listing {
    files: Vec<Changed>,
    hidden: u64,
    truncated: bool,
}

fn letter(code: &[u8]) -> char {
    match code {
        b"??" => '?',
        [b'U', _] | [_, b'U'] | b"AA" | b"DD" => 'U',
        [x, y] => {
            let c = if *y != b' ' { *y } else { *x };
            match c {
                b'A' => 'A',
                b'D' => 'D',
                b'T' => 'T',
                _ => 'M',
            }
        }
        _ => 'M',
    }
}

/// `git status` of the whole repository, with `+/−` per file.
async fn listing(repo: &Repo, base: &str) -> Result<Listing, ApiErr> {
    let o = git(
        &repo.top,
        &repo.blank,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--no-renames",
            "--ignore-submodules=all",
        ],
        STATUS_OUT_MAX,
    )
    .await?;
    if !o.ok && !o.cut {
        return Err(err("git_failed", "git could not read this repository's changes."));
    }
    let mut truncated = o.cut;
    let (mut files, mut hidden) = (Vec::new(), 0u64);
    for rec in o.stdout.split(|b| *b == 0) {
        if rec.len() < 4 {
            continue;
        }
        let Ok(path) = std::str::from_utf8(&rec[3..]) else {
            hidden += 1;
            continue;
        };
        if jail::secret_reason(path).is_some() {
            hidden += 1;
            continue;
        }
        if files.len() >= STATUS_FILES_MAX {
            truncated = true;
            continue;
        }
        let why = path_components(path).is_none().then_some("name");
        files.push(Changed {
            path: path.to_string(),
            status: letter(&rec[..2]),
            add: None,
            del: None,
            binary: false,
            why,
        });
    }
    // +/− of tracked files: one numstat against the base.
    let o = git(
        &repo.top,
        &repo.blank,
        &[
            "diff",
            "--numstat",
            "-z",
            "--no-renames",
            "--no-ext-diff",
            "--no-textconv",
            "--ignore-submodules=all",
            base,
            "--",
        ],
        STATUS_OUT_MAX,
    )
    .await?;
    let counts = parse_numstat(&o.stdout);
    let mut counted = 0usize;
    for f in &mut files {
        if f.status == '?' {
            match untracked_kind(&repo.top, &f.path) {
                Ok(()) if counted < UNTRACKED_COUNTED_MAX && f.why.is_none() => {
                    counted += 1;
                    let o = git(
                        &repo.top,
                        &repo.blank,
                        &[
                            "diff",
                            "--no-index",
                            "--numstat",
                            "-z",
                            "--no-ext-diff",
                            "--no-textconv",
                            "--",
                            "/dev/null",
                            &f.path,
                        ],
                        SMALL_OUT_MAX,
                    )
                    .await?;
                    if let Some((_, c)) = parse_numstat(&o.stdout).into_iter().next() {
                        set_counts(f, c);
                    }
                }
                Ok(()) => {}
                Err(why) => f.why = Some(why),
            }
        } else if let Some(c) = counts.iter().find(|(p, _)| *p == f.path).map(|(_, c)| *c) {
            set_counts(f, c);
        }
    }
    Ok(Listing {
        files,
        hidden,
        truncated,
    })
}

fn set_counts(f: &mut Changed, c: Option<(u64, u64)>) {
    match c {
        Some((a, d)) => (f.add, f.del) = (Some(a), Some(d)),
        None => f.binary = true,
    }
}

/// `add\tdel\tpath\0` records; `-\t-` (binary) is None.
fn parse_numstat(out: &[u8]) -> Vec<(String, Option<(u64, u64)>)> {
    let mut v = Vec::new();
    for rec in out.split(|b| *b == 0) {
        let s = String::from_utf8_lossy(rec);
        let mut it = s.splitn(3, '\t');
        let (Some(a), Some(d), Some(p)) = (it.next(), it.next(), it.next()) else {
            continue;
        };
        let p = p.trim_start_matches('\n').to_string();
        let c = a.parse().ok().zip(d.parse().ok());
        v.push((p, c));
    }
    v
}

/// An untracked file is shown only when it is a plain file of a sane size:
/// a link is never followed (its target may be anywhere), nor a device.
fn untracked_kind(top: &Path, rel: &str) -> Result<(), &'static str> {
    let m = std::fs::symlink_metadata(top.join(rel)).map_err(|_| "gone")?;
    if m.file_type().is_symlink() {
        return Err("link");
    }
    if !m.is_file() {
        return Err("special");
    }
    if m.len() > UNTRACKED_MAX {
        return Err("too_large");
    }
    Ok(())
}

fn file_json(f: &Changed) -> Value {
    json!({
        "path": f.path,
        "status": f.status.to_string(),
        "add": f.add,
        "del": f.del,
        "binary": f.binary,
        "why": f.why,
    })
}

/// `git.status`: the repository, its branch, and every changed file.
pub async fn status(ctx: &Ctx) -> ApiResult {
    let repo = open_repo(ctx).await?;
    let (base_rev, head) = base(&repo).await?;
    let l = listing(&repo, &base_rev).await?;
    let b = git(
        &repo.top,
        &repo.blank,
        &["symbolic-ref", "--quiet", "--short", "HEAD"],
        SMALL_OUT_MAX,
    )
    .await?;
    let branch = String::from_utf8_lossy(&b.stdout).trim().to_string();
    let b_ok = b.ok;
    let label = repo
        .top
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let on = Some(branch.as_str()).filter(|b| b_ok && !b.is_empty());
    let (ahead, protected) = ship::status_extra(ctx, &repo, on, head.is_some()).await?;
    let (add, del) = l.files.iter().fold((0u64, 0u64), |(a, d), f| {
        (a + f.add.unwrap_or(0), d + f.del.unwrap_or(0))
    });
    Ok(json!({
        "type": "git_status",
        "repo": ctx.masker.text(&label),
        "branch": (b.ok && !branch.is_empty()).then(|| ctx.masker.text(&branch)),
        "head": head.map(|h| h.chars().take(12).collect::<String>()),
        "files": l.files.iter().map(file_json).collect::<Vec<_>>(),
        "add": add,
        "del": del,
        "hidden": l.hidden,
        "truncated": l.truncated,
        "ahead": ahead,
        "protected": protected,
    }))
}

/// `git.diff`: one listed file's unified diff against the base, masked and cut.
pub async fn diff(ctx: &Ctx, path: &str) -> ApiResult {
    let repo = open_repo(ctx).await?;
    let (base_rev, _) = base(&repo).await?;
    if jail::secret_reason(path).is_some() {
        return Err(err(
            "file_refused",
            "This may hold secrets, so tmuxedo does not show it.",
        ));
    }
    let l = listing(&repo, &base_rev).await?;
    let Some(f) = l.files.into_iter().find(|f| f.path == path) else {
        return Err(err("not_found", "That file has no changes."));
    };
    let mut out = json!({
        "type": "git_diff",
        "path": f.path,
        "status": f.status.to_string(),
        "binary": f.binary,
        "patch": null,
        "truncated": false,
        "lines": 0,
        "why": f.why,
    });
    if f.why.is_some() || f.binary {
        return Ok(out);
    }
    let o = if f.status == '?' {
        git(
            &repo.top,
            &repo.blank,
            &[
                "diff",
                "--no-index",
                "--no-color",
                "--no-ext-diff",
                "--no-textconv",
                "--",
                "/dev/null",
                path,
            ],
            PATCH_MAX,
        )
        .await?
    } else {
        git(
            &repo.top,
            &repo.blank,
            &[
                "diff",
                "--no-color",
                "--no-ext-diff",
                "--no-textconv",
                "--no-renames",
                "--ignore-submodules=all",
                &base_rev,
                "--",
                path,
            ],
            PATCH_MAX,
        )
        .await?
    };
    // --no-index answers 1 when the files differ: that is the normal case here.
    if !(o.cut || o.ok || o.code == Some(1)) {
        return Err(err("git_failed", "git could not show this file's changes."));
    }
    let text = String::from_utf8_lossy(&o.stdout);
    if text
        .lines()
        .any(|l| l.starts_with("Binary files ") || l == "GIT binary patch")
    {
        out["binary"] = json!(true);
        return Ok(out);
    }
    let lines = text
        .lines()
        .filter(|l| (l.starts_with('+') && !l.starts_with("+++")) || (l.starts_with('-') && !l.starts_with("---")))
        .count() as u64;
    out["patch"] = json!(ctx.masker.text(&text));
    out["truncated"] = json!(o.cut);
    out["lines"] = json!(lines);
    out["large"] = json!(lines > LARGE_LINES);
    Ok(out)
}

/// Answer one parsed request.
pub async fn run(ctx: &Ctx, p: &ReadParams) -> ApiResult {
    match p {
        ReadParams::Status { .. } => status(ctx).await,
        ReadParams::Diff { path, .. } => diff(ctx, path).await,
    }
}

// ── the daemon's side ────────────────────────────────────────────────────

use crate::api::{respond, ReplyTo};
use crate::daemon::{Daemon, Internal};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// A parsed request.
enum Job {
    Read(ReadParams),
    Ship(ShipParams),
}

/// Work in flight.
#[derive(Default)]
pub struct Rt {
    busy: Arc<AtomicUsize>,
}

impl Daemon {
    /// A pane's folder now: its shell's (a plain `cd` never reaches the
    /// runner), else the one recorded at launch or by the agent's hook.
    pub(crate) fn pane_folder_now(&self, id: &str) -> Option<String> {
        let recorded = self.state.pane(id).map(|r| r.pane.cwd.clone())?;
        let shell = self.panes.get(id).map(|r| r.pid).filter(|p| *p > 0);
        Some(
            shell
                .and_then(tmuxedo_detect::platform::cwd)
                .filter(|c| std::path::Path::new(c).is_dir())
                .unwrap_or(recorded),
        )
    }

    /// The work folder the runner records for a session, never a client's.
    fn review_folder(&self, t: &Target) -> Option<PathBuf> {
        let cwd = match t {
            Target::Pane(id) => self.pane_folder_now(id)?,
            Target::Chat(id) => self.chats.sessions.get(id).map(|s| s.meta.cwd.clone())?,
        };
        Some(PathBuf::from(cwd)).filter(|p| p.is_absolute())
    }

    /// Every `git.*` request: local callers are refused; a reading one, a
    /// commit or a push is answered from a task (the gateway lets only the
    /// web ask for a commit or a push).
    pub(crate) fn review_request(&mut self, method: &str, params: &Value, reply: ReplyTo) {
        if matches!(reply, ReplyTo::Local(_)) {
            let e = err(
                "not_supported",
                "Review is not available on the machine's local socket.",
            );
            return respond(reply, Err(e), self);
        }
        let parsed = if proto::READ_METHODS.contains(&method) {
            proto::parse_read_params(method, params).map(Job::Read)
        } else if proto::WEB_METHODS.contains(&method) {
            proto::parse_ship_params(method, params).map(Job::Ship)
        } else {
            let e = if proto::MUTATING_METHODS.contains(&method) {
                err("not_supported", "This runner does not do that from the review.")
            } else {
                err("unknown_method", format!("unknown method {method}"))
            };
            return respond(reply, Err(e), self);
        };
        let parsed = match parsed {
            Ok(p) => p,
            Err((code, why)) => return respond(reply, Err(err(code, format!("{method}: {why}"))), self),
        };
        let target = match &parsed {
            Job::Read(ReadParams::Status { target } | ReadParams::Diff { target, .. }) => target,
            Job::Ship(
                ShipParams::Commit { target, .. } | ShipParams::Push { target, .. } | ShipParams::Discard { target },
            ) => target,
        };
        // A discard records a checkpoint first, and must not race the agent.
        let ck_target = match target {
            Target::Pane(id) => crate::checkpoint::daemon::CkTarget::Pane(id.clone()),
            Target::Chat(id) => crate::checkpoint::daemon::CkTarget::Chat(id.clone()),
        };
        let discard = matches!(parsed, Job::Ship(ShipParams::Discard { .. }));
        if discard && self.checkpoint_busy(&ck_target) {
            let e = err(
                "session_busy",
                "The agent is working: stop it or wait for its turn to end, then discard.",
            );
            return respond(reply, Err(e), self);
        }
        let (store, key) = (self.checkpoints.clone(), ck_target.key());
        let pane = match target {
            Target::Pane(id) => Some(id.clone()),
            Target::Chat(_) => None,
        };
        let Some(cwd) = self.review_folder(target) else {
            return respond(reply, Err(err("not_found", "No such session on this machine.")), self);
        };
        if self.review.busy.load(Ordering::Relaxed) >= BUSY_MAX {
            let e = err("rate_limited", "Too many review requests at once; try again.");
            return respond(reply, Err(e), self);
        }
        let mut masker = Masker::from_env(std::iter::empty::<(&str, &str)>());
        masker.machine = self.secrets.shared();
        let ctx = Ctx {
            cwd,
            guard: self.fs_guard(),
            masker,
            gh: PathBuf::from("gh"),
            env: vec![],
        };
        let (busy, tx) = (self.review.busy.clone(), self.internal_tx.clone());
        busy.fetch_add(1, Ordering::Relaxed);
        tokio::spawn(async move {
            let result = match &parsed {
                Job::Read(p) => run(&ctx, p).await,
                Job::Ship(ShipParams::Discard { .. }) => ship::discard(&ctx, &store, &key).await,
                Job::Ship(p) => ship::run(&ctx, p).await,
            };
            busy.fetch_sub(1, Ordering::Relaxed);
            // A pull request opened here: its checks are watched (crate::needs_ci).
            if let Ok(v) = &result {
                if let (Some(url), Some(number)) = (v["pr"]["url"].as_str(), v["pr"]["number"].as_u64()) {
                    let _ = tx.send(Internal::CiWatch {
                        url: url.to_string(),
                        number,
                        dir: ctx.cwd.clone(),
                        pane: pane.clone(),
                    });
                }
            }
            // The same delivery as the file browser's: a reply and its result.
            let _ = tx.send(Internal::Fs {
                reply,
                result,
                frames: vec![],
            });
        });
    }
}
