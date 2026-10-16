//! `git.status` / `git.diff` on temporary repositories only (never this
//! repository or its remote): listing, diffs, caps, binary files, links out
//! of the repository, too-broad roots, and a hostile repository config whose
//! programs must never run.

use super::*;
use crate::jail::Guard;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::process::Command;

struct Repo {
    _tmp: tempfile::TempDir,
    base: PathBuf,
    top: PathBuf,
}

fn put(p: &Path, body: impl AsRef<[u8]>) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

/// Plain git for setting a test repository up (not the code under test).
fn sh_git(dir: &Path, args: &[&str]) -> std::process::Output {
    let mut c = Command::new("git");
    for (k, _) in std::env::vars_os() {
        if k.to_string_lossy().starts_with("GIT_") {
            c.env_remove(&k);
        }
    }
    c.args([
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@example.invalid",
        "-c",
        "commit.gpgSign=false",
    ])
    .arg("-C")
    .arg(dir)
    .args(args)
    .env("GIT_CONFIG_GLOBAL", "/dev/null")
    .env("GIT_CONFIG_NOSYSTEM", "1")
    .output()
    .unwrap()
}

fn repo() -> Repo {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let top = base.join("work");
    put(&top.join("README.md"), "# hello\n");
    put(&top.join("src/lib.rs"), "fn a() {}\nfn b() {}\n");
    put(&top.join("gone.txt"), "bye\n");
    assert!(sh_git(&top, &["init", "-q", "-b", "agent/fix"]).status.success());
    sh_git(&top, &["add", "-A"]);
    assert!(sh_git(&top, &["commit", "-q", "-m", "init"]).status.success());
    Repo { _tmp: tmp, base, top }
}

fn ctx(r: &Repo) -> Ctx {
    Ctx {
        cwd: r.top.join("src"),
        guard: Guard {
            home: Some(r.base.join("home")),
            own: vec![],
        },
        masker: Masker::default(),
    }
}

fn file<'a>(v: &'a Value, path: &str) -> &'a Value {
    v["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["path"] == path)
        .unwrap_or_else(|| panic!("{path} not listed: {v}"))
}

#[tokio::test]
async fn status_lists_every_change_with_counts_and_hides_secrets() {
    let r = repo();
    put(&r.top.join("src/lib.rs"), "fn a() {}\nfn c() {}\nfn d() {}\n");
    std::fs::remove_file(r.top.join("gone.txt")).unwrap();
    put(&r.top.join("new/notes.md"), "one\ntwo\n");
    put(&r.top.join(".env"), "TOKEN=abc\n");
    let v = status(&ctx(&r)).await.unwrap();
    assert_eq!(v["type"], "git_status");
    assert_eq!(v["repo"], "work");
    assert_eq!(v["branch"], "agent/fix");
    assert_eq!(v["head"].as_str().unwrap().len(), 12);
    let lib = file(&v, "src/lib.rs");
    assert_eq!(
        (lib["status"].as_str(), lib["add"].as_u64(), lib["del"].as_u64()),
        (Some("M"), Some(2), Some(1))
    );
    assert_eq!(file(&v, "gone.txt")["status"], "D");
    let n = file(&v, "new/notes.md");
    assert_eq!((n["status"].as_str(), n["add"].as_u64()), (Some("?"), Some(2)));
    assert_eq!(v["hidden"], 1, "{v}");
    assert!(!v.to_string().contains(".env"), "a secret is counted, never named");
    assert_eq!(v["files"].as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn diff_shows_one_listed_file_and_nothing_else() {
    let r = repo();
    put(&r.top.join("src/lib.rs"), "fn a() {}\nfn changed() {}\n");
    put(&r.top.join("brand-new.txt"), "fresh\n");
    let c = ctx(&r);
    let d = diff(&c, "src/lib.rs").await.unwrap();
    let patch = d["patch"].as_str().unwrap();
    assert!(
        patch.contains("-fn b() {}") && patch.contains("+fn changed() {}"),
        "{patch}"
    );
    assert_eq!((d["lines"].as_u64(), d["large"].as_bool()), (Some(2), Some(false)));
    let n = diff(&c, "brand-new.txt").await.unwrap();
    assert!(n["patch"].as_str().unwrap().contains("+fresh"), "{n}");
    // Unchanged, absent, or a secret: refused.
    assert_eq!(diff(&c, "README.md").await.unwrap_err().code, "not_found");
    assert_eq!(diff(&c, "nope.txt").await.unwrap_err().code, "not_found");
    assert_eq!(diff(&c, ".env").await.unwrap_err().code, "file_refused");
}

#[tokio::test]
async fn a_link_out_of_the_repository_is_listed_never_followed() {
    let r = repo();
    put(&r.base.join("outside/secret.txt"), "DECOY-OUTSIDE\n");
    symlink(r.base.join("outside/secret.txt"), r.top.join("peek.txt")).unwrap();
    let c = ctx(&r);
    let v = status(&c).await.unwrap();
    assert_eq!(file(&v, "peek.txt")["why"], "link");
    let d = diff(&c, "peek.txt").await.unwrap();
    assert!(d["patch"].is_null(), "{d}");
    assert!(!v.to_string().contains("DECOY") && !d.to_string().contains("DECOY"));
}

#[tokio::test]
async fn a_huge_diff_is_cut_and_marked_large() {
    let r = repo();
    let body: String = (0..60_000)
        .map(|i| format!("line {i} with some padding text\n"))
        .collect();
    put(&r.top.join("src/lib.rs"), body);
    let d = diff(&ctx(&r), "src/lib.rs").await.unwrap();
    assert_eq!(d["truncated"], true);
    assert!(d["patch"].as_str().unwrap().len() <= PATCH_MAX + 1024);
    assert_eq!(d["large"], true);
}

#[tokio::test]
async fn binary_files_are_described_not_shown() {
    let r = repo();
    put(&r.top.join("img.bin"), [0u8, 1, 2, 3, 0, 255]);
    sh_git(&r.top, &["add", "img.bin"]);
    sh_git(&r.top, &["commit", "-q", "-m", "bin"]);
    put(&r.top.join("img.bin"), [0u8, 9, 9, 9, 0, 255, 7]);
    let c = ctx(&r);
    let v = status(&c).await.unwrap();
    assert_eq!(file(&v, "img.bin")["binary"], true);
    let d = diff(&c, "img.bin").await.unwrap();
    assert_eq!(d["binary"], true);
    assert!(d["patch"].is_null());
}

#[tokio::test]
async fn not_a_repository_and_too_broad_are_refused_plainly() {
    let tmp = tempfile::tempdir().unwrap();
    let plain = tmp.path().canonicalize().unwrap();
    let c = Ctx {
        cwd: plain.clone(),
        guard: Guard::default(),
        masker: Masker::default(),
    };
    assert_eq!(status(&c).await.unwrap_err().code, "not_a_repo");
    // A repository whose top is the home folder covers too much.
    let r = repo();
    let mut c = ctx(&r);
    c.guard.home = Some(r.top.clone());
    assert_eq!(status(&c).await.unwrap_err().code, "root_refused");
    // A folder that is gone.
    c.cwd = r.base.join("nowhere");
    assert_eq!(status(&c).await.unwrap_err().code, "folder_gone");
}

/// The repository's own config names programs for fsmonitor, a pager, an
/// external diff, a textconv driver, a clean filter and hooks. Plain git
/// runs at least some of them (checked first, so the test means something);
/// the review never runs any.
#[tokio::test]
async fn a_hostile_repository_config_runs_nothing() {
    let r = repo();
    let marker = r.base.join("ran");
    let script = r.base.join("evil.sh");
    put(
        &script,
        format!("#!/bin/sh\necho \"$0 $*\" >> '{}'\ncat\n", marker.display()),
    );
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let s = script.display().to_string();
    for (k, v) in [
        ("core.fsmonitor", s.as_str()),
        ("core.pager", s.as_str()),
        ("pager.diff", s.as_str()),
        ("pager.status", s.as_str()),
        ("diff.external", s.as_str()),
        ("diff.evil.textconv", s.as_str()),
        ("diff.evil.command", s.as_str()),
        ("filter.evil.clean", s.as_str()),
        ("filter.evil.smudge", s.as_str()),
        ("filter.evil.required", "true"),
        ("core.hooksPath", r.base.to_str().unwrap()),
    ] {
        assert!(sh_git(&r.top, &["config", "--local", k, v]).status.success(), "{k}");
    }
    put(&r.top.join(".gitattributes"), "*.rs filter=evil diff=evil\n");
    put(&r.top.join("src/lib.rs"), "fn a() {}\nfn hostile() {}\n");
    // Control: plain git runs the repository's programs.
    let _ = sh_git(&r.top, &["diff", "HEAD"]);
    assert!(marker.exists(), "the control run should have run a configured program");
    std::fs::remove_file(&marker).unwrap();
    let c = ctx(&r);
    let v = status(&c).await.unwrap();
    assert_eq!(file(&v, "src/lib.rs")["status"], "M", "{v}");
    let d = diff(&c, "src/lib.rs").await.unwrap();
    assert!(d["patch"].as_str().unwrap().contains("+fn hostile() {}"), "{d}");
    let _ = diff(&c, ".gitattributes").await.unwrap();
    assert!(
        !marker.exists(),
        "a program from the repository's config ran: {:?}",
        std::fs::read_to_string(&marker)
    );
}

#[test]
fn numstat_records_parse() {
    let v = parse_numstat(b"3\t1\ta.txt\0-\t-\tb.bin\0");
    assert_eq!(v[0], ("a.txt".to_string(), Some((3, 1))));
    assert_eq!(v[1], ("b.bin".to_string(), None));
    assert_eq!(letter(b"??"), '?');
    assert_eq!(letter(b" M"), 'M');
    assert_eq!(letter(b"A "), 'A');
    assert_eq!(letter(b"UU"), 'U');
}
