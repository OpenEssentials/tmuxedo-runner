//! `git.commit` / `git.push` on temporary repositories with a local bare
//! remote and a fake `gh` (a shell script that logs its arguments): never
//! the network, never this repository, never the machine's own gh.

use super::super::Ctx;
use super::*;
use crate::chat::mask::Masker;
use crate::jail::Guard;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

struct T {
    _tmp: tempfile::TempDir,
    base: PathBuf,
    top: PathBuf,
    remote: PathBuf,
    env: Vec<(String, String)>,
}

fn put(p: &Path, body: impl AsRef<[u8]>) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

impl T {
    /// Plain git for setting up and checking (not the code under test).
    fn git(&self, dir: &Path, args: &[&str]) -> String {
        let mut c = Command::new("git");
        for (k, _) in std::env::vars_os() {
            if k.to_string_lossy().starts_with("GIT_") {
                c.env_remove(&k);
            }
        }
        let o = c
            .arg("-C")
            .arg(dir)
            .args(args)
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .output()
            .unwrap();
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    }

    fn ctx(&self) -> Ctx {
        Ctx {
            cwd: self.top.clone(),
            guard: Guard {
                home: Some(self.base.join("home")),
                own: vec![],
            },
            masker: Masker::default(),
            gh: self.base.join("bin/gh"),
            env: self.env.clone(),
        }
    }

    fn gh_log(&self) -> String {
        std::fs::read_to_string(self.base.join("bin/gh.log")).unwrap_or_default()
    }

    fn remote_branches(&self) -> String {
        self.git(
            &self.remote,
            &["for-each-ref", "--format=%(refname:short)", "refs/heads"],
        )
    }
}

const FAKE_GH: &str = r#"#!/bin/sh
here="$(dirname "$0")"
printf '%s\n' "$*" >> "$here/gh.log"
case "$1 $2" in
  "--version "*) echo "gh version 2.60.0"; exit 0 ;;
  "auth status") if [ -f "$here/signed-out" ]; then echo "You are not logged into any GitHub hosts." >&2; exit 1; fi; exit 0 ;;
  "pr create")
    if [ -f "$here/exists" ]; then echo 'a pull request for branch "agent/fix" into branch "main" already exists:' >&2; echo "https://github.example/o/r/pull/5" >&2; exit 1; fi
    echo "https://github.example/o/r/pull/7"; exit 0 ;;
  "pr checks") cat "$here/checks.json"; exit "$(cat "$here/checks.code" 2>/dev/null || echo 0)" ;;
esac
exit 2
"#;

/// A repository on `agent/fix` with one commit, a bare remote `origin`
/// holding `main`, a fake gh, and a git config of its own (identity, no signing).
fn setup() -> T {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    put(
        &base.join("gitconfig"),
        "[user]\n\tname = Test\n\temail = test@example.invalid\n[commit]\n\tgpgSign = false\n[init]\n\tdefaultBranch = main\n",
    );
    put(&base.join("bin/gh"), FAKE_GH);
    std::fs::set_permissions(base.join("bin/gh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let env = vec![
        (
            "GIT_CONFIG_GLOBAL".to_string(),
            base.join("gitconfig").to_string_lossy().into_owned(),
        ),
        ("GIT_CONFIG_NOSYSTEM".to_string(), "1".to_string()),
    ];
    let top = base.join("work");
    let remote = base.join("remote.git");
    let t = T {
        _tmp: tmp,
        base: base.clone(),
        top: top.clone(),
        remote: remote.clone(),
        env,
    };
    std::fs::create_dir_all(&remote).unwrap();
    t.git(&remote, &["init", "-q", "--bare"]);
    put(&top.join("README.md"), "# hello\n");
    put(&top.join("src/lib.rs"), "fn a() {}\n");
    t.git(&top, &["init", "-q", "-b", "main"]);
    t.git(&top, &["add", "-A"]);
    t.git(&top, &["commit", "-q", "-m", "init"]);
    t.git(&top, &["remote", "add", "origin", remote.to_str().unwrap()]);
    t.git(&top, &["push", "-q", "origin", "main"]);
    t.git(&top, &["checkout", "-q", "-b", "agent/fix"]);
    t
}

#[tokio::test]
async fn commit_takes_the_listed_changes_and_leaves_secrets_alone() {
    let t = setup();
    put(&t.top.join("src/lib.rs"), "fn a() {}\nfn b() {}\n");
    put(&t.top.join("src/new.rs"), "fn c() {}\n");
    std::fs::remove_file(t.top.join("README.md")).unwrap();
    put(&t.top.join(".env"), "TOKEN=x\n");
    let v = commit(&t.ctx(), "Fix the thing\n\nWith a body.", None).await.unwrap();
    assert_eq!(v["type"], "git_committed", "{v}");
    assert_eq!(v["files"], 3, "{v}");
    assert_eq!(v["left"], 1, "{v}");
    assert_eq!(v["branch"], "agent/fix");
    assert_eq!(t.git(&t.top, &["log", "-1", "--format=%s"]), "Fix the thing");
    let changed = t.git(&t.top, &["show", "--name-status", "--format=", "HEAD"]);
    assert!(
        changed.contains("src/new.rs") && changed.contains("README.md"),
        "{changed}"
    );
    assert!(!changed.contains(".env"), "{changed}");
    assert_eq!(t.git(&t.top, &["status", "--porcelain"]), "?? .env");
}

#[tokio::test]
async fn commit_on_a_new_branch_and_the_refusals() {
    let t = setup();
    let e = commit(&t.ctx(), "x", None).await.unwrap_err();
    assert_eq!(e.code, "nothing_to_commit");
    put(&t.top.join("a.txt"), "a\n");
    let e = commit(&t.ctx(), "x", Some("main")).await.unwrap_err();
    assert_eq!(e.code, "branch_exists");
    let v = commit(&t.ctx(), "Add a", Some("agent/add-a")).await.unwrap();
    assert_eq!(v["branch"], "agent/add-a");
    assert_eq!(t.git(&t.top, &["symbolic-ref", "--short", "HEAD"]), "agent/add-a");
    // A secret already staged would be committed with the rest: refused, nothing done.
    put(&t.top.join("b.txt"), "b\n");
    put(&t.top.join(".env"), "TOKEN=x\n");
    t.git(&t.top, &["add", "-f", ".env"]);
    let e = commit(&t.ctx(), "x", None).await.unwrap_err();
    assert_eq!(e.code, "secret_staged");
    t.git(&t.top, &["rm", "-q", "--cached", ".env"]);
    // The repository's own config naming a program for signing.
    t.git(&t.top, &["config", "gpg.program", "/bin/false"]);
    let e = commit(&t.ctx(), "x", None).await.unwrap_err();
    assert_eq!(e.code, "config_refused");
    assert!(e.message.contains("gpg.program"), "{}", e.message);
}

#[tokio::test]
async fn commit_without_an_identity_says_so() {
    let t = setup();
    put(
        &t.base.join("gitconfig"),
        "[commit]\n\tgpgSign = false\n[user]\n\tuseConfigOnly = true\n",
    );
    put(&t.top.join("a.txt"), "a\n");
    let e = commit(&t.ctx(), "x", None).await.unwrap_err();
    assert_eq!(e.code, "identity_missing", "{}", e.message);
}

#[tokio::test]
async fn push_and_open_a_draft_pr_with_the_machines_own_gh() {
    let t = setup();
    put(&t.top.join("a.txt"), "a\n");
    commit(&t.ctx(), "Add a\n\nBecause.", None).await.unwrap();
    let st = crate::review::status(&t.ctx()).await.unwrap();
    assert_eq!(st["ahead"], 1, "{st}");
    assert_eq!(st["protected"], false, "{st}");
    let v = push(&t.ctx(), true, None).await.unwrap();
    assert_eq!(v["type"], "git_pushed", "{v}");
    assert_eq!(v["branch"], "agent/fix");
    assert_eq!(v["remote"], "origin");
    assert_eq!(v["pr"]["number"], 7, "{v}");
    assert_eq!(v["pr"]["url"], "https://github.example/o/r/pull/7");
    assert!(t.remote_branches().contains("agent/fix"));
    let log = t.gh_log();
    assert!(log.contains("auth status"), "{log}");
    assert!(
        log.contains("pr create --draft --head agent/fix --title Add a --body Because."),
        "{log}"
    );
    let st = crate::review::status(&t.ctx()).await.unwrap();
    assert_eq!(st["ahead"], 0, "{st}");
    // Asked again: the PR is there already, and that one is answered.
    std::fs::write(t.base.join("bin/exists"), "").unwrap();
    let v = push(&t.ctx(), true, Some("main")).await.unwrap();
    assert_eq!(v["pr"]["number"], 5, "{v}");
    assert_eq!(v["pr"]["existing"], true, "{v}");
    assert!(t.gh_log().contains("--base main"));
}

#[tokio::test]
async fn without_gh_or_signed_out_nothing_is_pushed_and_it_says_why() {
    let t = setup();
    put(&t.top.join("a.txt"), "a\n");
    commit(&t.ctx(), "Add a", None).await.unwrap();
    let mut c = t.ctx();
    c.gh = t.base.join("bin/not-there");
    let e = push(&c, true, None).await.unwrap_err();
    assert_eq!(e.code, "gh_missing");
    assert!(e.message.contains("isn't installed"), "{}", e.message);
    std::fs::write(t.base.join("bin/signed-out"), "").unwrap();
    let e = push(&t.ctx(), true, None).await.unwrap_err();
    assert_eq!(e.code, "gh_signed_out");
    assert!(e.message.contains("gh auth login"), "{}", e.message);
    assert!(!t.remote_branches().contains("agent/fix"));
    // Push only needs no gh at all.
    let v = push(&c, false, None).await.unwrap();
    assert!(v["pr"].is_null(), "{v}");
    assert!(t.remote_branches().contains("agent/fix"));
}

#[tokio::test]
async fn push_refuses_the_default_branch_and_risky_repository_settings() {
    let t = setup();
    t.git(&t.top, &["checkout", "-q", "main"]);
    put(&t.top.join("a.txt"), "a\n");
    commit(&t.ctx(), "Add a", None).await.unwrap();
    let st = crate::review::status(&t.ctx()).await.unwrap();
    assert_eq!(st["protected"], true, "{st}");
    let e = push(&t.ctx(), false, None).await.unwrap_err();
    assert_eq!(e.code, "default_branch");
    t.git(&t.top, &["checkout", "-q", "-b", "agent/other"]);
    for (k, v) in [
        ("core.sshCommand", "/bin/false"),
        ("credential.helper", "/bin/false"),
        ("remote.origin.receivepack", "/bin/false"),
    ] {
        t.git(&t.top, &["config", k, v]);
        let e = push(&t.ctx(), false, None).await.unwrap_err();
        assert_eq!(e.code, "config_refused", "{k}");
        t.git(&t.top, &["config", "--unset", k]);
    }
    assert!(!t.remote_branches().contains("agent/other"));
    let v = push(&t.ctx(), false, None).await.unwrap();
    assert_eq!(v["branch"], "agent/other");
}

#[tokio::test]
async fn ci_checks_are_read_with_gh() {
    let t = setup();
    put(
        &t.base.join("bin/checks.json"),
        r#"[{"name":"unit","bucket":"pass"},{"name":"e2e / checkout","bucket":"fail"},{"name":"lint","bucket":"pending"}]"#,
    );
    std::fs::write(t.base.join("bin/checks.code"), "1").unwrap();
    let o = checks(&t.ctx().gh, &t.top, "https://github.example/o/r/pull/7", &t.env).await;
    assert_eq!(
        o,
        Checks::Failed {
            names: vec!["e2e / checkout".into()],
            total: 3
        }
    );
    assert!(t
        .gh_log()
        .contains("pr checks https://github.example/o/r/pull/7 --json name,bucket"));
}

#[test]
fn check_answers_and_pr_links_parse() {
    assert_eq!(checks_outcome(br#"[]"#), Checks::Pending);
    assert_eq!(
        checks_outcome(br#"[{"name":"a","bucket":"pass"},{"name":"b","bucket":"skipping"}]"#),
        Checks::Passed
    );
    assert_eq!(
        checks_outcome(br#"[{"name":"a","bucket":"pass"},{"name":"b","bucket":"pending"}]"#),
        Checks::Pending
    );
    assert_eq!(checks_outcome(b"not json"), Checks::Unknown);
    assert_eq!(
        pr_link("Creating draft pull request\nhttps://github.example/o/r/pull/12\n"),
        Some((12, "https://github.example/o/r/pull/12".into()))
    );
    assert_eq!(pr_link("http://github.example/o/r/pull/12"), None);
    assert_eq!(pr_link("https://github.example/o/r/pull/x"), None);
    assert_eq!(pr_link("https://a b/o/r/pull/1"), None);
}

fn store(t: &T) -> Store {
    Store::new(Some(t.base.join("checkpoints")))
}

#[tokio::test(flavor = "multi_thread")]
async fn discard_puts_the_listed_changes_back_after_a_checkpoint_and_restore_undoes_it() {
    let t = setup();
    let s = store(&t);
    put(&t.top.join("src/lib.rs"), "fn a() {}\nfn b() {}\n");
    put(&t.top.join("src/deep/new.rs"), "fn c() {}\n");
    put(&t.top.join("added.txt"), "staged new\n");
    t.git(&t.top, &["add", "added.txt"]);
    std::fs::remove_file(t.top.join("README.md")).unwrap();
    put(&t.top.join(".env"), "TOKEN=x\n");
    let v = discard(&t.ctx(), &s, "pane:w1:p1").await.unwrap();
    assert_eq!(v["type"], "git_discarded", "{v}");
    assert_eq!(v["files"], 4, "{v}");
    assert_eq!(v["left"], 1, "{v}");
    let id = v["checkpoint"].as_u64().unwrap();
    assert_eq!(
        std::fs::read_to_string(t.top.join("src/lib.rs")).unwrap(),
        "fn a() {}\n"
    );
    assert_eq!(std::fs::read_to_string(t.top.join("README.md")).unwrap(), "# hello\n");
    assert!(!t.top.join("src/deep").exists(), "the emptied folder goes too");
    assert!(!t.top.join("added.txt").exists());
    // The secret-named file is never touched, and nothing is left staged.
    assert_eq!(std::fs::read_to_string(t.top.join(".env")).unwrap(), "TOKEN=x\n");
    assert_eq!(t.git(&t.top, &["status", "--porcelain"]), "?? .env");
    assert_eq!(t.git(&t.top, &["log", "--format=%s"]), "init", "no commit is made");

    // The checkpoint taken first brings it all back.
    let list = s.list(&t.top, &t.ctx().guard, "pane:w1:p1").await.ok().unwrap();
    assert_eq!(list[0].kind, "discard");
    s.restore(&t.top, &t.ctx().guard, "pane:w1:p1", id).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(t.top.join("src/lib.rs")).unwrap(),
        "fn a() {}\nfn b() {}\n"
    );
    assert_eq!(
        std::fs::read_to_string(t.top.join("src/deep/new.rs")).unwrap(),
        "fn c() {}\n"
    );
    assert!(!t.top.join("README.md").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn discard_before_the_first_commit_and_when_there_is_nothing() {
    let t = setup();
    let s = store(&t);
    let e = discard(&t.ctx(), &s, "pane:w1:p1").await.unwrap_err();
    assert_eq!(e.code, "nothing_to_discard");
    let fresh = t.base.join("fresh");
    put(&fresh.join("a.txt"), "a\n");
    put(&fresh.join("b.txt"), "b\n");
    t.git(&fresh, &["init", "-q", "-b", "main"]);
    t.git(&fresh, &["add", "a.txt"]);
    let mut ctx = t.ctx();
    ctx.cwd = fresh.clone();
    let v = discard(&ctx, &s, "chat:c1").await.unwrap();
    assert_eq!(v["files"], 2, "{v}");
    assert!(!fresh.join("a.txt").exists() && !fresh.join("b.txt").exists());
    assert_eq!(t.git(&fresh, &["status", "--porcelain"]), "");
}

#[tokio::test(flavor = "multi_thread")]
async fn discard_refuses_conflicts_and_needs_checkpoints() {
    let t = setup();
    put(&t.top.join("src/lib.rs"), "fn a() {}\nfn b() {}\n");
    let e = discard(&t.ctx(), &Store::new(None), "pane:w1:p1").await.unwrap_err();
    assert_eq!(e.code, "off");
    assert_eq!(
        std::fs::read_to_string(t.top.join("src/lib.rs")).unwrap(),
        "fn a() {}\nfn b() {}\n",
        "nothing changes without a checkpoint"
    );
    // A conflict: two branches change the same line.
    t.git(&t.top, &["commit", "-qam", "ours"]);
    t.git(&t.top, &["checkout", "-q", "-b", "other", "main"]);
    put(&t.top.join("src/lib.rs"), "fn z() {}\n");
    t.git(&t.top, &["commit", "-qam", "theirs"]);
    t.git(&t.top, &["merge", "-q", "agent/fix"]);
    let e = discard(&t.ctx(), &store(&t), "pane:w1:p1").await.unwrap_err();
    assert_eq!(e.code, "conflicts");
}
