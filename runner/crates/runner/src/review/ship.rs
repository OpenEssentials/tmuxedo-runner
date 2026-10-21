//! Review and ship, the shipping half (docs/design/away-and-review.md §3,
//! §5): `git.commit`, `git.push` (with a draft pull request), `git.discard`, and the CI
//! checks of a pull request read back. Only the web asks (a person on the
//! review screen, signed in again recently); the machine's own git identity
//! and its own gh do the work, never anything of tmuxedo's.
//!
//! COMMIT takes exactly what the review listed: the changed files the
//! status showed, never a secret-named one (those stay, counted as `left`),
//! and nothing already staged that the review did not list (refused). An
//! optional new branch is made first.
//!
//! PUSH sends the current branch to its remote (its upstream's, else
//! `origin`, else the only one). The default branch (main, master, develop,
//! trunk, or what the remote calls its HEAD) is never pushed from here:
//! commit to a new branch first. With `pr`, gh is checked first (installed,
//! signed in) so nothing is pushed when the pull request can't be opened.
//!
//! DISCARD puts the listed changes back as HEAD has them and removes new
//! files, after recording the folder as a checkpoint: it can be undone.
//!
//! THE REPOSITORY'S OWN CONFIG is still hostile input (see the module
//! above): the reading half's settings apply, filters are blanked for `git
//! add`, hooks are off (`--no-verify`, `core.hooksPath=/dev/null`), and a
//! repository whose own config names a program for signing, ssh,
//! credentials or a remote is refused outright. gh runs git itself: the
//! same settings reach it through `GIT_CONFIG_COUNT`.

#[cfg(test)]
#[path = "ship_tests.rs"]
mod tests;

use super::{base, exec, git_with, listing, open_repo, Ctx, Opts, Out, Repo, SAFE, SMALL_OUT_MAX};
use crate::api::{err, ApiErr, ApiResult};
use crate::checkpoint::{prune_dirs, safe_parents, Info, Store};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;
use tmuxedo_protocol::review::ShipParams;

const COMMIT_TIME: Duration = Duration::from_secs(60);
// The web waits up to 130 s for the whole push: 15 + 15 + 60 + 30 at worst.
const PUSH_TIME: Duration = Duration::from_secs(60);
const GH_TIME: Duration = Duration::from_secs(30);
const GH_CHECK_TIME: Duration = Duration::from_secs(15);

/// Branch names that are never pushed from the review.
const PROTECTED: &[&str] = &["main", "master", "develop", "trunk"];

/// Extra settings for runs that write or talk to a remote.
const SHIP: &[&str] = &[
    "-c",
    "gc.auto=0",
    "-c",
    "maintenance.auto=false",
    "-c",
    "push.recurseSubmodules=no",
    "-c",
    "submodule.recurse=false",
    "-c",
    "protocol.allow=never",
    "-c",
    "protocol.https.allow=always",
    "-c",
    "protocol.http.allow=always",
    "-c",
    "protocol.ssh.allow=always",
    "-c",
    "protocol.git.allow=always",
    "-c",
    "protocol.file.allow=always",
];

/// A setting name in the repository's own config that makes git run a
/// program (or hand credentials) on a commit or a push.
fn risky(name: &str, push: bool) -> bool {
    let program = name.starts_with("gpg.") && (name.ends_with("program") || name.ends_with("command"));
    if !push {
        return program;
    }
    program
        || matches!(
            name,
            "core.sshcommand" | "core.askpass" | "core.gitproxy" | "core.alternaterefscommand"
        )
        || name.starts_with("credential.")
        || (name.starts_with("remote.")
            && (name.ends_with(".receivepack") || name.ends_with(".uploadpack") || name.ends_with(".vcs")))
}

fn refuse_config(repo: &Repo, push: bool) -> Result<(), ApiErr> {
    if let Some(n) = repo.names.iter().find(|n| risky(n, push)) {
        let what = if push { "push" } else { "commit" };
        return Err(err(
            "config_refused",
            format!(
                "This repository's own git settings name a program ({n}), so tmuxedo won't {what} here. Do it in a terminal on the machine."
            ),
        ));
    }
    Ok(())
}

async fn run_git(
    ctx: &Ctx,
    repo: &Repo,
    args: &[&str],
    input: Option<&[u8]>,
    time: Option<Duration>,
) -> Result<Out, ApiErr> {
    let mut extra = repo.blank.clone();
    extra.extend(SHIP.iter().map(|s| s.to_string()));
    git_with(
        &repo.top,
        &extra,
        args,
        &Opts {
            cap: SMALL_OUT_MAX,
            input,
            time,
            env: &ctx.env,
        },
    )
    .await
}

fn text(o: &Out) -> String {
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

async fn current_branch(ctx: &Ctx, repo: &Repo) -> Result<Option<String>, ApiErr> {
    let o = run_git(ctx, repo, &["symbolic-ref", "--quiet", "--short", "HEAD"], None, None).await?;
    Ok(Some(text(&o)).filter(|b| o.ok && !b.is_empty()))
}

/// The remote a branch goes to: its upstream's, else `origin`, else the only one.
async fn remote_for(ctx: &Ctx, repo: &Repo, branch: &str) -> Result<Option<String>, ApiErr> {
    let key = format!("branch.{branch}.remote");
    let o = run_git(ctx, repo, &["config", "--get", &key], None, None).await?;
    let up = text(&o);
    let o = run_git(ctx, repo, &["remote"], None, None).await?;
    let remotes: Vec<String> = text(&o).lines().map(str::to_string).collect();
    if o.ok && !up.is_empty() && remotes.contains(&up) {
        return Ok(Some(up));
    }
    if remotes.iter().any(|r| r == "origin") {
        return Ok(Some("origin".into()));
    }
    Ok((remotes.len() == 1).then(|| remotes[0].clone()))
}

/// Whether a branch is one the review never pushes.
async fn protected(ctx: &Ctx, repo: &Repo, branch: &str) -> Result<bool, ApiErr> {
    if PROTECTED.contains(&branch) {
        return Ok(true);
    }
    let Some(remote) = remote_for(ctx, repo, branch).await? else {
        return Ok(false);
    };
    let head = format!("refs/remotes/{remote}/HEAD");
    let o = run_git(ctx, repo, &["symbolic-ref", "--quiet", "--short", &head], None, None).await?;
    Ok(o.ok && text(&o).strip_prefix(&format!("{remote}/")) == Some(branch))
}

/// `ahead` (commits no remote has) and `protected` for `git.status`.
pub(super) async fn status_extra(
    ctx: &Ctx,
    repo: &Repo,
    branch: Option<&str>,
    born: bool,
) -> Result<(u64, bool), ApiErr> {
    let ahead = if born {
        let o = run_git(
            ctx,
            repo,
            &["rev-list", "--count", "HEAD", "--not", "--remotes"],
            None,
            None,
        )
        .await?;
        text(&o).parse().unwrap_or(0)
    } else {
        0
    };
    let prot = match branch {
        Some(b) => protected(ctx, repo, b).await?,
        None => false,
    };
    Ok((ahead, prot))
}

/// `git.commit`: every listed change, on a new branch when one is named.
pub async fn commit(ctx: &Ctx, message: &str, branch: Option<&str>) -> ApiResult {
    let repo = open_repo(ctx).await?;
    refuse_config(&repo, false)?;
    let (base_rev, _) = base(&repo).await?;
    let l = listing(&repo, &base_rev).await?;
    if l.files.is_empty() {
        return Err(err("nothing_to_commit", "There are no changes to commit."));
    }
    if l.truncated {
        return Err(err(
            "too_many_files",
            "Too many files changed to commit from here. Commit in a terminal on the machine.",
        ));
    }
    if l.files.iter().any(|f| f.status == 'U') {
        return Err(err(
            "conflicts",
            "Some files are in conflict. Settle them first, then commit.",
        ));
    }
    // Whatever is staged already goes into the commit too: it must be listed.
    let o = run_git(
        ctx,
        &repo,
        &[
            "diff",
            "--cached",
            "--name-only",
            "-z",
            "--no-renames",
            "--ignore-submodules=all",
            &base_rev,
            "--",
        ],
        None,
        None,
    )
    .await?;
    if !o.ok {
        return Err(err("git_failed", "git could not read what is staged."));
    }
    let listed = |p: &[u8]| l.files.iter().any(|f| f.path.as_bytes() == p);
    if o.stdout.split(|b| *b == 0).any(|p| !p.is_empty() && !listed(p)) {
        return Err(err(
            "secret_staged",
            "A file that may hold secrets is staged, so tmuxedo won't commit. Unstage it in a terminal first.",
        ));
    }
    if let Some(b) = branch {
        let r = format!("refs/heads/{b}");
        let o = run_git(ctx, &repo, &["rev-parse", "--verify", "--quiet", &r], None, None).await?;
        if o.ok {
            return Err(err(
                "branch_exists",
                format!("A branch named {b} exists already. Pick another name."),
            ));
        }
        let o = run_git(ctx, &repo, &["checkout", "-q", "-b", b], None, None).await?;
        if !o.ok {
            return Err(err("git_failed", "git could not make the new branch."));
        }
    }
    let mut paths = Vec::new();
    for f in &l.files {
        paths.extend_from_slice(f.path.as_bytes());
        paths.push(0);
    }
    let o = run_git(
        ctx,
        &repo,
        &["add", "-A", "--pathspec-from-file=-", "--pathspec-file-nul"],
        Some(&paths),
        Some(COMMIT_TIME),
    )
    .await?;
    if !o.ok {
        return Err(err("git_failed", "git could not add the changes."));
    }
    let o = run_git(
        ctx,
        &repo,
        &["commit", "-q", "--no-verify", "--cleanup=strip", "-F", "-"],
        Some(message.as_bytes()),
        Some(COMMIT_TIME),
    )
    .await?;
    if !o.ok {
        let e = o.stderr.to_ascii_lowercase();
        // Put back what was added, so a failed commit leaves the index as it was.
        let _ = run_git(ctx, &repo, &["reset", "-q"], None, None).await;
        if e.contains("tell me who you are")
            || e.contains("auto-detection is disabled")
            || e.contains("unable to auto-detect")
        {
            return Err(err(
                "identity_missing",
                "git on this machine has no name and email set. Run git config --global user.name and user.email there, then try again.",
            ));
        }
        if e.contains("gpg") || e.contains("sign") {
            return Err(err(
                "sign_failed",
                "git could not sign the commit on this machine. Check its signing setup in a terminal.",
            ));
        }
        return Err(err("git_failed", "git could not make the commit."));
    }
    let o = run_git(ctx, &repo, &["rev-parse", "HEAD"], None, None).await?;
    let head: String = text(&o).chars().take(12).collect();
    let now = current_branch(ctx, &repo).await?;
    Ok(json!({
        "type": "git_committed",
        "commit": head,
        "branch": now.map(|b| ctx.masker.text(&b)),
        "files": l.files.len(),
        "left": l.hidden,
    }))
}

fn gh_missing() -> ApiErr {
    err(
        "gh_missing",
        "GitHub's gh command isn't installed on this machine, so tmuxedo can't open a pull request. Install gh there, or push only.",
    )
}

/// Run the machine's gh in `dir`, with the review's git settings reaching
/// the git it runs.
async fn gh(
    gh: &Path,
    dir: &Path,
    args: &[&str],
    env: &[(String, String)],
    blank: &[String],
    time: Duration,
) -> Result<Out, ApiErr> {
    let mut c = tokio::process::Command::new(gh);
    c.args(args).current_dir(dir);
    // Every `-c k=v` of the safe set and the blanked filters, as GIT_CONFIG_*.
    let mut pairs: Vec<&str> = SAFE.windows(2).filter(|w| w[0] == "-c").map(|w| w[1]).collect();
    pairs.extend(blank.iter().map(String::as_str).filter(|s| *s != "-c"));
    let mut all: Vec<(String, String)> = Vec::new();
    for (i, p) in pairs.iter().enumerate() {
        let (k, v) = p.split_once('=').unwrap_or((p, ""));
        all.push((format!("GIT_CONFIG_KEY_{i}"), k.to_string()));
        all.push((format!("GIT_CONFIG_VALUE_{i}"), v.to_string()));
    }
    all.push(("GIT_CONFIG_COUNT".into(), pairs.len().to_string()));
    for (k, v) in [
        ("GH_PROMPT_DISABLED", "1"),
        ("GH_NO_UPDATE_NOTIFIER", "1"),
        ("GH_SPINNER_DISABLED", "1"),
        ("NO_COLOR", "1"),
        ("GH_PAGER", "cat"),
    ] {
        all.push((k.into(), v.into()));
    }
    all.extend(env.iter().cloned());
    exec(
        c,
        &Opts {
            cap: SMALL_OUT_MAX,
            input: None,
            time: Some(time),
            env: &all,
        },
        gh_missing,
        "gh",
    )
    .await
}

/// A pull request link in gh's output: `https://host/owner/repo/pull/N`.
pub(crate) fn pr_link(s: &str) -> Option<(u64, String)> {
    s.split_whitespace().find_map(|w| {
        let rest = w.strip_prefix("https://")?;
        let mut parts = rest.split('/');
        let host = parts.next()?;
        let (owner, repo, pull, n) = (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
        if host.is_empty() || owner.is_empty() || repo.is_empty() || pull != "pull" || parts.next().is_some() {
            return None;
        }
        let n: u64 = n.parse().ok()?;
        Some((n, w.to_string()))
    })
}

/// `git.push`: the current branch to its remote; with `pr`, a draft pull request.
pub async fn push(ctx: &Ctx, pr: bool, base_branch: Option<&str>) -> ApiResult {
    let repo = open_repo(ctx).await?;
    refuse_config(&repo, true)?;
    let Some(branch) = current_branch(ctx, &repo).await? else {
        return Err(err(
            "detached",
            "This folder isn't on a branch. Commit to a new branch first.",
        ));
    };
    let o = run_git(
        ctx,
        &repo,
        &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"],
        None,
        None,
    )
    .await?;
    if !o.ok {
        return Err(err("nothing_to_push", "There are no commits to push yet."));
    }
    if protected(ctx, &repo, &branch).await? {
        return Err(err(
            "default_branch",
            format!("{branch} is the main line, so tmuxedo won't push it. Commit to a new branch first."),
        ));
    }
    let Some(remote) = remote_for(ctx, &repo, &branch).await? else {
        return Err(err("no_remote", "This repository has no remote to push to."));
    };
    if pr {
        let o = gh(&ctx.gh, &repo.top, &["--version"], &ctx.env, &repo.blank, GH_CHECK_TIME).await?;
        if !o.ok {
            return Err(gh_missing());
        }
        let o = gh(
            &ctx.gh,
            &repo.top,
            &["auth", "status"],
            &ctx.env,
            &repo.blank,
            GH_CHECK_TIME,
        )
        .await?;
        if !o.ok {
            return Err(err(
                "gh_signed_out",
                "gh isn't signed in on this machine. Run gh auth login there, then try again, or push only.",
            ));
        }
    }
    let spec = format!("refs/heads/{branch}:refs/heads/{branch}");
    let o = run_git(
        ctx,
        &repo,
        &["push", "--porcelain", "--no-verify", "-u", &remote, &spec],
        None,
        Some(PUSH_TIME),
    )
    .await?;
    if !o.ok {
        let e = format!("{}\n{}", o.stderr, text(&o)).to_ascii_lowercase();
        if e.contains("[rejected]") || e.contains("non-fast-forward") || e.contains("fetch first") {
            return Err(err(
                "push_rejected",
                "The remote has commits this branch doesn't. Pull them in a terminal first, then push.",
            ));
        }
        if e.contains("authentication")
            || e.contains("permission denied")
            || e.contains("could not read username")
            || e.contains("403")
        {
            return Err(err(
                "push_auth",
                "The remote refused this machine's credentials. Check git's sign-in there.",
            ));
        }
        return Err(err("push_failed", "git could not push this branch."));
    }
    let mut out = json!({
        "type": "git_pushed",
        "branch": ctx.masker.text(&branch),
        "remote": ctx.masker.text(&remote),
        "pr": null,
    });
    if !pr {
        return Ok(out);
    }
    let o = run_git(ctx, &repo, &["log", "-1", "--format=%s"], None, None).await?;
    let title = text(&o);
    let o = run_git(ctx, &repo, &["log", "-1", "--format=%b"], None, None).await?;
    let body = Some(text(&o))
        .filter(|b| !b.is_empty())
        .unwrap_or_else(|| "Opened from the tmuxedo review.".into());
    let mut args = vec![
        "pr", "create", "--draft", "--head", &branch, "--title", &title, "--body", &body,
    ];
    if let Some(b) = base_branch {
        args.extend(["--base", b]);
    }
    let o = gh(&ctx.gh, &repo.top, &args, &ctx.env, &repo.blank, GH_TIME).await?;
    let said = format!("{}\n{}", text(&o), o.stderr);
    let existing = !o.ok && said.contains("already exists");
    match pr_link(&said) {
        Some((number, url)) if o.ok || existing => {
            out["pr"] = json!({ "number": number, "url": url, "existing": existing });
            Ok(out)
        }
        _ => Err(err(
            "pr_failed",
            "The branch was pushed, but gh could not open the pull request. Open it on GitHub.",
        )),
    }
}

/// `git.discard`: every listed change put back as the base has it, after
/// the folder is recorded as a checkpoint (`session`'s, kind `discard`), so
/// "Restore to here" on that checkpoint undoes it. A file the checkpoint
/// could not hold (secret-named, over 50 MB, in a build folder, a special
/// file) is left as it is and counted in `left`, as are the hidden ones.
pub async fn discard(ctx: &Ctx, store: &Store, session: &str) -> ApiResult {
    let repo = open_repo(ctx).await?;
    let (base_rev, _) = base(&repo).await?;
    let l = listing(&repo, &base_rev).await?;
    if l.files.is_empty() {
        return Err(err("nothing_to_discard", "There are no changes to discard."));
    }
    if l.truncated {
        return Err(err(
            "too_many_files",
            "Too many files changed to discard from here. Use a terminal on the machine.",
        ));
    }
    if l.files.iter().any(|f| f.status == 'U') {
        return Err(err(
            "conflicts",
            "Some files are in conflict. Settle them in a terminal first.",
        ));
    }
    let work = |info: Info, saved: HashSet<String>| async move {
        let top = &repo.top;
        // Only what the checkpoint holds, or what is gone from disk already.
        let (mut tracked, mut new, mut kept) = (Vec::new(), Vec::new(), 0u64);
        for f in &l.files {
            let on_disk = std::fs::symlink_metadata(top.join(&f.path)).is_ok();
            if f.why == Some("name") || (on_disk && !saved.contains(&f.path)) {
                kept += 1;
            } else if f.status == '?' {
                new.push(f.path.clone());
            } else {
                tracked.push(f.path.clone());
            }
        }
        if !tracked.is_empty() {
            let mut paths = Vec::new();
            for p in &tracked {
                paths.extend_from_slice(p.as_bytes());
                paths.push(0);
            }
            let source = format!("--source={base_rev}");
            let o = run_git(
                ctx,
                &repo,
                &[
                    "restore",
                    "-q",
                    &source,
                    "--staged",
                    "--worktree",
                    "--pathspec-from-file=-",
                    "--pathspec-file-nul",
                ],
                Some(&paths),
                Some(COMMIT_TIME),
            )
            .await?;
            if !o.ok {
                return Err(err(
                    "git_failed",
                    format!(
                        "git could not put the files back. They are saved in checkpoint {}.",
                        info.id
                    ),
                ));
            }
        }
        let mut removed = Vec::new();
        for p in &new {
            let file = top.join(p);
            if !safe_parents(top, p) {
                kept += 1;
                continue;
            }
            match std::fs::symlink_metadata(&file) {
                Ok(m) if m.is_file() || m.file_type().is_symlink() => {
                    if std::fs::remove_file(&file).is_ok() {
                        prune_dirs(top, &file);
                        removed.push(p.clone());
                    } else {
                        kept += 1;
                    }
                }
                Ok(_) => kept += 1,
                Err(_) => {}
            }
        }
        let done = tracked.len() + removed.len();
        let mut changed = tracked;
        changed.extend(removed);
        Ok((
            json!({
                "type": "git_discarded",
                "files": done,
                "left": l.hidden + kept,
                "checkpoint": info.id,
            }),
            changed,
        ))
    };
    store.discard_with(&ctx.cwd, &ctx.guard, session, work).await
}

/// Answer one parsed request (a discard is the daemon's: it needs the checkpoints).
pub async fn run(ctx: &Ctx, p: &ShipParams) -> ApiResult {
    match p {
        ShipParams::Commit { message, branch, .. } => commit(ctx, message, branch.as_deref()).await,
        ShipParams::Push { pr, base, .. } => push(ctx, *pr, base.as_deref()).await,
        ShipParams::Discard { .. } => Err(err("not_supported", "Discard needs the checkpoints.")),
    }
}

/// What a pull request's checks say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Checks {
    /// None yet, or some still running.
    Pending,
    /// Every one passed (or was skipped).
    Passed,
    /// These failed (or were cancelled), of `total`.
    Failed { names: Vec<String>, total: usize },
    /// gh did not answer (gone, signed out, offline): asked again later.
    Unknown,
}

/// gh's `pr checks --json name,bucket` answer.
pub fn checks_outcome(out: &[u8]) -> Checks {
    let Ok(Value::Array(list)) = serde_json::from_slice::<Value>(out) else {
        return Checks::Unknown;
    };
    if list.is_empty() {
        return Checks::Pending;
    }
    let bucket = |c: &Value| c["bucket"].as_str().unwrap_or("").to_string();
    let names: Vec<String> = list
        .iter()
        .filter(|c| matches!(bucket(c).as_str(), "fail" | "cancel"))
        .map(|c| c["name"].as_str().unwrap_or("check").to_string())
        .collect();
    if !names.is_empty() {
        return Checks::Failed {
            names,
            total: list.len(),
        };
    }
    if list.iter().all(|c| matches!(bucket(c).as_str(), "pass" | "skipping")) {
        Checks::Passed
    } else {
        Checks::Pending
    }
}

/// Ask the machine's gh about a pull request's checks (exit codes 1 and 8
/// are its "some failed" and "some pending": the JSON says which).
pub async fn checks(gh_path: &Path, dir: &Path, url: &str, env: &[(String, String)]) -> Checks {
    if !dir.is_dir() {
        return Checks::Unknown;
    }
    match gh(
        gh_path,
        dir,
        &["pr", "checks", url, "--json", "name,bucket"],
        env,
        &[],
        GH_TIME,
    )
    .await
    {
        Ok(o) if !o.cut => checks_outcome(&o.stdout),
        _ => Checks::Unknown,
    }
}
