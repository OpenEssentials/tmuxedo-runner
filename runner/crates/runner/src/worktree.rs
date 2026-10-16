//! Git worktrees for the runner API's `worktree.*` methods (Phase 7): checkouts live at
//! `<worktree_dir>/<repo>/<branch-slug>` (`worktree_dir` in runner.toml,
//! default `~/.tmuxedo/worktrees`), a missing branch is generated as
//! `worktree/<colour>-<animal>-<hex>`, an existing local branch is checked
//! out instead of created, removal keeps the branch and refuses a dirty
//! checkout unless `force`. `trust_repository` adds `-c safe.directory=`.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;

const DEFAULT_PREFIX: &str = "worktree";

/// A workspace's place in a repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Membership {
    /// Canonical git common dir: one key per repository.
    pub key: String,
    pub repo_name: String,
    pub repo_root: String,
    pub checkout_path: String,
    pub is_linked_worktree: bool,
}

impl Membership {
    /// The `worktree` object of a workspace in the runner API.
    pub fn info(&self) -> serde_json::Value {
        serde_json::json!({
            "repo_key": self.key,
            "repo_name": self.repo_name,
            "repo_root": self.repo_root,
            "checkout_path": self.checkout_path,
            "is_linked_worktree": self.is_linked_worktree,
        })
    }
}

/// What `git rev-parse` says about a directory inside a work tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Space {
    pub key: String,
    pub repo_name: String,
    /// The checkout's top level (the linked checkout for a linked worktree).
    pub repo_root: PathBuf,
    pub is_linked_worktree: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub path: PathBuf,
    pub branch: Option<String>,
    pub is_bare: bool,
    pub is_detached: bool,
    pub is_prunable: bool,
}

pub fn canonical(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

fn git(dir: &Path, trust: bool) -> Command {
    let mut c = Command::new("git");
    if trust {
        c.arg("-c").arg(format!("safe.directory={}", dir.display()));
    }
    c.arg("-C").arg(dir).env("LC_ALL", "C").env("GIT_TERMINAL_PROMPT", "0");
    c
}

fn run(mut c: Command) -> Result<String, String> {
    let out = c.output().map_err(|e| format!("git: {e}"))?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
    let std = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Err(if !err.is_empty() {
        err
    } else if !std.is_empty() {
        std
    } else {
        format!("git failed with {}", out.status)
    })
}

/// The repository `dir` is in, or None outside a work tree.
pub fn space(dir: &Path, trust: bool) -> Option<Space> {
    let mut c = git(dir, trust);
    c.args([
        "rev-parse",
        "--path-format=absolute",
        "--show-toplevel",
        "--git-dir",
        "--git-common-dir",
    ]);
    let out = run(c).ok()?;
    let mut lines = out.lines();
    let top = PathBuf::from(lines.next()?);
    let git_dir = canonical(Path::new(lines.next()?));
    let common = canonical(Path::new(lines.next()?));
    // The repo is named after the dir holding `.git` (or `.bare`), else after the common dir.
    let label_path = match common.file_name().and_then(|n| n.to_str()) {
        Some(".git") | Some(".bare") => common.parent().unwrap_or(&top).to_path_buf(),
        _ => common.clone(),
    };
    Some(Space {
        key: common.display().to_string(),
        repo_name: label_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("repo")
            .to_string(),
        repo_root: top,
        is_linked_worktree: git_dir != common,
    })
}

/// A fresh branch name for a worktree created without one:
/// `worktree/<colour>-<animal>-<4 hex digits>`, all three picked from `seed`.
pub fn generated_branch(seed: u64) -> String {
    const COLOUR: [&str; 12] = [
        "amber", "azure", "coral", "indigo", "jade", "lilac", "mint", "ochre", "plum", "rust", "sand", "teal",
    ];
    const ANIMAL: [&str; 12] = [
        "badger", "crane", "falcon", "gecko", "heron", "lynx", "marten", "newt", "otter", "puffin", "stoat", "wren",
    ];
    let c = COLOUR[(seed % 12) as usize];
    let a = ANIMAL[((seed / 12) % 12) as usize];
    format!("{DEFAULT_PREFIX}/{c}-{a}-{:04x}", (seed >> 8) & 0xffff)
}

/// A branch name as one path component: ASCII letters and digits, lower-cased;
/// every run of anything else becomes one `-`; no `-` at either end.
/// Nothing left: `worktree`.
pub fn path_slug(branch: &str) -> String {
    let words: Vec<String> = branch
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_ascii_lowercase)
        .collect();
    if words.is_empty() {
        DEFAULT_PREFIX.to_string()
    } else {
        words.join("-")
    }
}

/// `<root>/<repo>/<branch-slug>`.
pub fn default_checkout_path(root: &Path, repo_name: &str, branch: &str) -> PathBuf {
    root.join(repo_name).join(path_slug(branch))
}

/// `~` and `~/x` against `home`.
pub fn expand_tilde(p: &str, home: &str) -> PathBuf {
    if p == "~" {
        return PathBuf::from(home);
    }
    match p.strip_prefix("~/") {
        Some(rest) => Path::new(home).join(rest),
        None => PathBuf::from(p),
    }
}

/// `git worktree list --porcelain`: one record per worktree, each starting with
/// `worktree <path>`, attribute lines after it (`branch refs/heads/<b>`,
/// `detached`, `bare`, `prunable …`; others ignored). Lines before the first
/// `worktree` line are ignored.
pub fn parse_porcelain(out: &str) -> Vec<Entry> {
    let mut entries: Vec<Entry> = vec![];
    for line in out.lines().map(str::trim_end).filter(|l| !l.is_empty()) {
        let (word, rest) = line.split_once(' ').unwrap_or((line, ""));
        if word == "worktree" {
            entries.push(Entry {
                path: PathBuf::from(rest),
                branch: None,
                is_bare: false,
                is_detached: false,
                is_prunable: false,
            });
            continue;
        }
        let Some(e) = entries.last_mut() else { continue };
        match word {
            "branch" => e.branch = Some(rest.trim_start_matches("refs/heads/").to_string()),
            "detached" => e.is_detached = true,
            "bare" => e.is_bare = true,
            "prunable" => e.is_prunable = true,
            _ => {}
        }
    }
    entries
}

pub fn list(repo_root: &Path, trust: bool) -> Result<Vec<Entry>, String> {
    let mut c = git(repo_root, trust);
    c.args(["worktree", "list", "--porcelain"]);
    Ok(parse_porcelain(&run(c)?))
}

fn branch_exists(repo_root: &Path, branch: &str, trust: bool) -> Result<bool, String> {
    let mut c = git(repo_root, trust);
    c.args(["show-ref", "--verify", "--quiet"])
        .arg(format!("refs/heads/{branch}"));
    let st = c.status().map_err(|e| format!("git: {e}"))?;
    match st.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(format!("git show-ref failed with {st}")),
    }
}

/// `git worktree add` — checks out `branch` if it exists locally, else
/// creates it from `base`.
pub fn add(repo_root: &Path, path: &Path, branch: &str, base: &str, trust: bool) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    let mut c = git(repo_root, trust);
    c.args(["worktree", "add"]);
    if branch_exists(repo_root, branch, trust)? {
        c.arg(path).arg(branch);
    } else {
        c.arg("-b").arg(branch).arg(path).arg(base);
    }
    run(c).map(|_| ())
}

/// Uncommitted or untracked files in the checkout.
pub fn is_dirty(path: &Path, trust: bool) -> Result<bool, String> {
    let mut c = git(path, trust);
    c.args(["status", "--porcelain", "--untracked-files=all"]);
    Ok(!run(c)?.is_empty())
}

/// `git worktree remove [--force]`; the branch is kept.
pub fn remove(repo_root: &Path, path: &Path, force: bool, trust: bool) -> Result<(), String> {
    let mut c = git(repo_root, trust);
    c.args(["worktree", "remove"]);
    if force {
        c.arg("--force");
    }
    c.arg(path);
    run(c).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_names_slugs_and_paths() {
        assert_eq!(generated_branch(0), "worktree/amber-badger-0000");
        assert_eq!(generated_branch(13 + 0x0012_3400), "worktree/lilac-marten-1234");
        assert_eq!(generated_branch(u64::MAX), "worktree/indigo-crane-ffff");
        assert_eq!(path_slug("feature/Login UI!"), "feature-login-ui");
        assert_eq!(path_slug("--a__b--"), "a-b");
        assert_eq!(path_slug("é/ü"), "worktree", "non-ASCII letters are separators");
        assert_eq!(path_slug("///"), "worktree");
        assert_eq!(
            default_checkout_path(Path::new("/w"), "app", "feat/x"),
            PathBuf::from("/w/app/feat-x")
        );
        assert_eq!(
            expand_tilde("~/.tmuxedo/worktrees", "/h"),
            PathBuf::from("/h/.tmuxedo/worktrees")
        );
        assert_eq!(expand_tilde("/abs", "/h"), PathBuf::from("/abs"));
    }

    #[test]
    fn parses_porcelain() {
        let out = "worktree /r\nHEAD abc\nbranch refs/heads/main\n\nworktree /w/a\nHEAD def\ndetached\n\nworktree /w/b\nbranch refs/heads/b\nprunable gitdir file points to non-existent location\n";
        let v = parse_porcelain(out);
        assert_eq!(v.len(), 3);
        assert_eq!(v[0].branch.as_deref(), Some("main"));
        assert!(v[1].is_detached && v[1].branch.is_none());
        assert!(v[2].is_prunable);
        let bare = parse_porcelain("junk before\nworktree /b.git\nbare\n\nworktree /p with space\nlocked\n");
        assert_eq!(bare.len(), 2);
        assert!(bare[0].is_bare);
        assert_eq!(bare[1].path, PathBuf::from("/p with space"));
        assert!(parse_porcelain("").is_empty());
    }
}
