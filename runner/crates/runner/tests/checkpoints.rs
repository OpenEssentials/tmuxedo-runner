//! Checkpoints and undo (Phase 23): a restore puts the folder back byte for
//! byte (content, untracked files, modes, deletions, new files), and the
//! user's repository (index, HEAD, branches, stash) is never touched.

mod common;

use common::{options, start_holder, FakeGateway, H as LocalH};
use serde_json::json;
use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use tmuxedo_runner::checkpoint::Store;
use tmuxedo_runner::jail::Guard;

fn sh(dir: &Path, args: &[&str]) -> String {
    let o = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
        .output()
        .expect("git");
    assert!(
        o.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8_lossy(&o.stdout).to_string()
}

fn guard() -> Guard {
    Guard {
        home: None,
        own: vec![],
    }
}

/// Every file under `dir` (but `.git`): bytes or link target, and mode.
fn state(dir: &Path) -> BTreeMap<String, (Vec<u8>, u32)> {
    fn walk(top: &Path, d: &Path, out: &mut BTreeMap<String, (Vec<u8>, u32)>) {
        for e in std::fs::read_dir(d).unwrap() {
            let p = e.unwrap().path();
            let rel = p.strip_prefix(top).unwrap().to_string_lossy().to_string();
            if rel == ".git" {
                continue;
            }
            let m = std::fs::symlink_metadata(&p).unwrap();
            if m.file_type().is_symlink() {
                let t = std::fs::read_link(&p).unwrap();
                out.insert(rel, (t.into_os_string().into_encoded_bytes(), 0o120000));
            } else if m.is_dir() {
                out.insert(format!("{rel}/"), (vec![], m.permissions().mode() & 0o7777));
                walk(top, &p, out);
            } else {
                out.insert(rel, (std::fs::read(&p).unwrap(), m.permissions().mode() & 0o7777));
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, dir, &mut out);
    out
}

fn chmod(p: &Path, mode: u32) {
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap();
}

/// A repository with tracked, staged, untracked, ignored and executable files.
fn repo(base: &Path) -> PathBuf {
    let dir = base.join("work");
    std::fs::create_dir_all(dir.join("src/deep")).unwrap();
    sh(&dir, &["init", "-q", "-b", "main"]);
    std::fs::write(dir.join("a.txt"), b"one\ntwo\n").unwrap();
    std::fs::write(dir.join("src/lib.rs"), b"fn main() {}\n").unwrap();
    std::fs::write(dir.join("run.sh"), b"#!/bin/sh\necho hi\n").unwrap();
    chmod(&dir.join("run.sh"), 0o755);
    std::fs::write(dir.join(".gitignore"), b"*.log\n").unwrap();
    std::fs::write(dir.join("gone.txt"), b"to be deleted\n").unwrap();
    sh(&dir, &["add", "."]);
    sh(&dir, &["commit", "-q", "-m", "first"]);
    // Work in progress: staged, unstaged, untracked (binary, private
    // permission, a symlink), and an ignored file.
    std::fs::write(dir.join("a.txt"), b"one\ntwo\nthree\n").unwrap();
    std::fs::write(dir.join("staged.txt"), b"staged\n").unwrap();
    sh(&dir, &["add", "staged.txt"]);
    std::fs::write(dir.join("src/deep/notes.bin"), [0u8, 159, 146, 150, 255, 0, 10]).unwrap();
    std::fs::write(dir.join("private.txt"), b"only me\n").unwrap();
    chmod(&dir.join("private.txt"), 0o600);
    std::os::unix::fs::symlink("a.txt", dir.join("link")).unwrap();
    std::fs::write(dir.join("debug.log"), b"ignored\n").unwrap();
    dir
}

/// Modify, add untracked, delete, chmod: then undo, and the folder is as it was.
#[tokio::test(flavor = "multi_thread")]
async fn restore_is_byte_exact() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = repo(tmp.path());
    let store = Store::new(Some(tmp.path().join("checkpoints")));
    let before = state(&dir);
    let cp = store
        .snapshot(&dir, &guard(), "pane:w1:p1", "turn", None)
        .await
        .unwrap()
        .expect("recorded");
    assert_eq!(cp.id, 1);

    // The "agent's turn".
    std::fs::write(dir.join("a.txt"), b"rewritten entirely\n").unwrap();
    std::fs::write(dir.join("src/deep/notes.bin"), b"text now").unwrap();
    std::fs::write(dir.join("new.txt"), b"brand new\n").unwrap();
    std::fs::create_dir_all(dir.join("fresh/dir")).unwrap();
    std::fs::write(dir.join("fresh/dir/f.rs"), b"// new\n").unwrap();
    std::fs::remove_file(dir.join("gone.txt")).unwrap();
    std::fs::remove_file(dir.join("staged.txt")).unwrap();
    chmod(&dir.join("run.sh"), 0o644);
    chmod(&dir.join("private.txt"), 0o640);
    std::fs::remove_file(dir.join("link")).unwrap();
    std::os::unix::fs::symlink("src", dir.join("link")).unwrap();
    std::fs::write(dir.join("debug.log"), b"ignored, changed\n").unwrap();
    let ignored_after = std::fs::read(dir.join("debug.log")).unwrap();

    let pv = store.preview(&dir, &guard(), "pane:w1:p1", 1).await.unwrap();
    let words: BTreeMap<String, String> = pv["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| {
            (
                f["path"].as_str().unwrap().to_string(),
                f["change"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(words.get("new.txt").map(String::as_str), Some("removed"), "{pv}");
    assert_eq!(words.get("fresh/dir/f.rs").map(String::as_str), Some("removed"));
    assert_eq!(words.get("gone.txt").map(String::as_str), Some("added"));
    assert_eq!(words.get("a.txt").map(String::as_str), Some("modified"));
    assert_eq!(words.get("run.sh").map(String::as_str), Some("modified"));
    assert_eq!(words.get("private.txt").map(String::as_str), Some("modified"));
    assert!(
        !words.contains_key("debug.log"),
        "ignored files are not in a checkpoint"
    );

    let r = store.restore(&dir, &guard(), "pane:w1:p1", 1).await.unwrap();
    assert_eq!(r["safety"], 2, "{r}");
    assert_eq!(r["conflict_count"], 0, "{r}");
    let mut after = state(&dir);
    // The ignored file is never touched: it keeps the turn's content.
    assert_eq!(after.remove("debug.log").unwrap().0, ignored_after);
    let mut want = before.clone();
    want.remove("debug.log");
    assert_eq!(after, want, "the folder is back byte for byte, modes and all");

    // The restore can be undone: checkpoint 2 holds the turn's result.
    let list = store.list(&dir, &guard(), "pane:w1:p1").await.ok().unwrap();
    assert_eq!(
        list.iter().map(|i| (i.id, i.kind.as_str())).collect::<Vec<_>>(),
        [(2, "restore"), (1, "turn")]
    );
    assert_eq!(list[0].restoring, Some(1));
    store.restore(&dir, &guard(), "pane:w1:p1", 2).await.unwrap();
    assert_eq!(std::fs::read(dir.join("a.txt")).unwrap(), b"rewritten entirely\n");
    assert_eq!(std::fs::read(dir.join("fresh/dir/f.rs")).unwrap(), b"// new\n");
    assert!(!dir.join("gone.txt").exists());
    assert_eq!(
        std::fs::metadata(dir.join("private.txt")).unwrap().permissions().mode() & 0o777,
        0o640
    );
    assert_eq!(std::fs::read_link(dir.join("link")).unwrap(), Path::new("src"));
    // And back again: the folder as before the turn (empty folders the
    // turn made are gone with their files).
    store.restore(&dir, &guard(), "pane:w1:p1", 1).await.unwrap();
    let mut again = state(&dir);
    again.remove("debug.log");
    assert_eq!(again, want);
}

/// The user's index, HEAD, branches, stash and object store are untouched;
/// nothing is written inside the folder's `.git`.
#[tokio::test(flavor = "multi_thread")]
async fn the_users_repository_is_untouched() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = repo(tmp.path());
    std::fs::write(dir.join("stashed.txt"), b"stash me\n").unwrap();
    sh(&dir, &["stash", "push", "-q", "-u", "-m", "kept"]);
    std::fs::write(dir.join("a.txt"), b"one\ntwo\nthree\n").unwrap();
    let git_state = |dir: &Path| {
        let mut s = state(&dir.join(".git"));
        // The index's bytes, HEAD, refs, stash list, objects: all of .git.
        s.retain(|k, _| !k.ends_with('/'));
        (
            s,
            sh(dir, &["rev-parse", "HEAD"]),
            sh(dir, &["stash", "list"]),
            sh(dir, &["branch", "-a"]),
            sh(dir, &["diff", "--cached", "--name-status"]),
        )
    };
    let before = git_state(&dir);
    let store = Store::new(Some(tmp.path().join("checkpoints")));
    store
        .snapshot(&dir, &guard(), "chat:c1", "turn", None)
        .await
        .unwrap()
        .expect("recorded");
    std::fs::write(dir.join("a.txt"), b"changed by the turn\n").unwrap();
    std::fs::write(dir.join("other.txt"), b"new\n").unwrap();
    store.preview(&dir, &guard(), "chat:c1", 1).await.unwrap();
    store.restore(&dir, &guard(), "chat:c1", 1).await.unwrap();
    let after = git_state(&dir);
    assert_eq!(before.1, after.1, "HEAD");
    assert_eq!(before.2, after.2, "stash");
    assert_eq!(before.3, after.3, "branches");
    assert_eq!(before.4, after.4, "index");
    assert_eq!(
        before.0.keys().collect::<Vec<_>>(),
        after.0.keys().collect::<Vec<_>>(),
        ".git files"
    );
    assert!(before.0 == after.0, "no byte of .git changed");
    assert!(!dir.join("other.txt").exists());
}

/// A folder outside git, checkpoints turned off, and the dedupe/prune rules.
#[tokio::test(flavor = "multi_thread")]
async fn not_available_and_pruned() {
    let tmp = tempfile::tempdir().unwrap();
    let plain = tmp.path().join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    let store = Store::new(Some(tmp.path().join("checkpoints")));
    let Err(u) = store.list(&plain, &guard(), "pane:p").await else {
        panic!("not available");
    };
    assert_eq!(u.reason, "not_a_repo");
    let off = Store::new(None);
    assert_eq!(off.list(&plain, &guard(), "pane:p").await.err().unwrap().reason, "off");

    let dir = repo(tmp.path());
    store
        .snapshot(&dir, &guard(), "pane:p", "turn", None)
        .await
        .unwrap()
        .unwrap();
    // Nothing changed: no second turn checkpoint.
    assert!(store
        .snapshot(&dir, &guard(), "pane:p", "turn", None)
        .await
        .unwrap()
        .is_none());
    for i in 0..55 {
        std::fs::write(dir.join("a.txt"), format!("{i}\n")).unwrap();
        store
            .snapshot(&dir, &guard(), "pane:p", "turn", None)
            .await
            .unwrap()
            .unwrap();
    }
    let list = store.list(&dir, &guard(), "pane:p").await.ok().unwrap();
    assert_eq!(list.len(), 50);
    assert_eq!(list[0].id, 56);
    assert_eq!(list[49].id, 7);
    // Another session in the same folder has its own list.
    assert!(store.list(&dir, &guard(), "chat:x").await.ok().unwrap().is_empty());
}

/// The away digest's facts: what changed in the folder since the first
/// checkpoint at or after `since`, and the chat turns recorded since then.
#[tokio::test(flavor = "multi_thread")]
async fn a_summary_counts_what_changed_since() {
    let tmp = tempfile::tempdir().unwrap();
    let plain = tmp.path().join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    let store = Store::new(Some(tmp.path().join("checkpoints")));
    assert_eq!(
        store
            .summary(&plain, &guard(), "chat:c1", None)
            .await
            .err()
            .unwrap()
            .reason,
        "not_a_repo"
    );
    let dir = repo(tmp.path());
    // Nothing recorded yet: no facts.
    let s = store.summary(&dir, &guard(), "chat:c1", None).await.ok().unwrap();
    assert_eq!(s, json!({"turns": 0, "files": null}));

    let t1 = store
        .snapshot_turn(&dir, &guard(), "chat:c1", "turn", Some("t1"), None)
        .await
        .unwrap()
        .unwrap();
    std::fs::write(dir.join("a.txt"), b"the agent was here\n").unwrap();
    std::fs::write(dir.join("made.txt"), b"new\n").unwrap();
    std::fs::remove_file(dir.join("gone.txt")).unwrap();
    store
        .snapshot_turn(&dir, &guard(), "chat:c1", "turn", Some("t2"), None)
        .await
        .unwrap()
        .unwrap();
    std::fs::write(dir.join("a.txt"), b"and again\n").unwrap();

    let s = store.summary(&dir, &guard(), "chat:c1", None).await.ok().unwrap();
    assert_eq!(s["turns"], 2, "{s}");
    assert_eq!(s["files"], json!({"added": 1, "modified": 1, "removed": 1}), "{s}");
    assert_eq!(s["from"], t1.at, "{s}");
    // Since a moment after every checkpoint: nothing to compare with.
    let later = t1.at + 3_600_000;
    let s = store
        .summary(&dir, &guard(), "chat:c1", Some(later))
        .await
        .ok()
        .unwrap();
    assert_eq!(s, json!({"turns": 0, "files": null}));
    // Reading a summary changes nothing: no new checkpoint.
    assert_eq!(store.list(&dir, &guard(), "chat:c1").await.ok().unwrap().len(), 2);
}

/// Through the daemon: a terminal agent's prompt submit records the folder
/// before the hook is answered; the gateway lists, previews and restores;
/// a working agent's folder is not restored under it; the local socket is
/// refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_turn_is_recorded_and_undone_through_the_gateway() {
    let dir = common::tempdir();
    let proj = repo(dir.path());
    let holder = start_holder(dir.path());
    let gw = FakeGateway::start().await;
    let mut o = options(dir.path(), &gw.url, holder);
    o.probe.claude_argv = Some(vec!["true".into()]);
    o.claude_auth = Some(false);
    let sock = o.runner_socket.clone();
    let task = tokio::spawn(tmuxedo_runner::daemon::run(o));
    let s = std::time::Duration::from_secs(10);
    let mut c = gw.accept(s).await.expect("runner dials out");
    let hello = c.hello(s).await;
    assert!(hello.to_string().contains("\"checkpoints\""), "{hello}");
    assert!(hello.to_string().contains("\"session_summary\""), "{hello}");
    c.text_t(s, "topology").await;
    c.send(json!({"t":"welcome","machineId":"m1","userId":"u1"})).await;
    let h = LocalH::attach_to(&sock).await;
    let hook = |name: &str, extra: serde_json::Value| {
        let mut e = json!({"hook_event_name": name, "session_id": "s-1"});
        for (k, v) in extra.as_object().unwrap() {
            e[k] = v.clone();
        }
        json!({"paneId": "w1:p1", "event": e})
    };
    h.raw(
        "hook.event",
        hook(
            "SessionStart",
            json!({"cwd": proj.to_string_lossy(), "source": "startup"}),
        ),
    )
    .await;
    c.send(json!({"t":"req","id":"l0","method":"checkpoint.list","params":{"pane": "w1:p1"}}))
        .await;
    let l0 = c.res("l0").await;
    assert_eq!(l0["result"]["available"], true, "{l0}");
    assert_eq!(l0["result"]["checkpoints"], json!([]));

    // The prompt is submitted: the answer to the hook comes after the record.
    let before = std::fs::read(proj.join("a.txt")).unwrap();
    let r = h
        .raw("hook.event", hook("UserPromptSubmit", json!({"prompt": "x"})))
        .await;
    assert_eq!(r["result"], json!({}), "{r}");
    std::fs::write(proj.join("a.txt"), b"the agent was here\n").unwrap();
    std::fs::write(proj.join("made.txt"), b"new\n").unwrap();

    c.send(json!({"t":"req","id":"l1","method":"checkpoint.list","params":{"pane": "w1:p1"}}))
        .await;
    let l1 = c.res("l1").await;
    assert_eq!(l1["result"]["checkpoints"][0]["id"], 1, "{l1}");
    assert_eq!(l1["result"]["checkpoints"][0]["kind"], "turn");

    c.send(json!({"t":"req","id":"p","method":"checkpoint.preview","params":{"pane": "w1:p1", "id": 1}}))
        .await;
    let p = c.res("p").await;
    assert_eq!(
        p["result"]["files"],
        json!([{"path": "a.txt", "change": "modified"}, {"path": "made.txt", "change": "removed"}]),
        "{p}"
    );

    // The away digest's facts for this pane, through the gateway.
    c.send(json!({"t":"req","id":"s","method":"session.summary","params":{"pane": "w1:p1", "since": 0}}))
        .await;
    let sm = c.res("s").await;
    assert_eq!(sm["result"]["type"], "session_summary", "{sm}");
    assert_eq!(
        sm["result"]["files"],
        json!({"added": 1, "modified": 1, "removed": 0}),
        "{sm}"
    );
    assert!(sm["result"].get("line").is_none(), "{sm}");
    common::schema_check("success_response", &json!({"id": "s", "result": sm["result"]})).unwrap();
    c.send(json!({"t":"req","id":"s404","method":"session.summary","params":{"pane": "w9:p9"}}))
        .await;
    assert_eq!(c.res("s404").await["error"]["code"], "not_found");

    // The agent is still working: no restore under it.
    c.send(json!({"t":"req","id":"busy","method":"checkpoint.restore","params":{"pane": "w1:p1", "id": 1}}))
        .await;
    assert_eq!(c.res("busy").await["error"]["code"], "session_busy");
    h.raw("hook.event", hook("Stop", json!({}))).await;
    c.send(json!({"t":"req","id":"r","method":"checkpoint.restore","params":{"pane": "w1:p1", "id": 1}}))
        .await;
    let r = c.res("r").await;
    assert_eq!(r["result"]["safety"], 2, "{r}");
    assert_eq!(std::fs::read(proj.join("a.txt")).unwrap(), before);
    assert!(!proj.join("made.txt").exists());

    // A save by hand, an unknown checkpoint, and the local socket.
    c.send(json!({"t":"req","id":"m","method":"checkpoint.create","params":{"pane": "w1:p1"}}))
        .await;
    assert_eq!(c.res("m").await["result"]["checkpoint"]["kind"], "manual");
    c.send(json!({"t":"req","id":"nf","method":"checkpoint.preview","params":{"pane": "w1:p1", "id": 99}}))
        .await;
    assert_eq!(c.res("nf").await["error"]["code"], "not_found");
    assert_eq!(
        h.raw("checkpoint.list", json!({"pane": "w1:p1"})).await["error"]["code"],
        "not_supported"
    );
    assert_eq!(
        h.raw("session.summary", json!({"pane": "w1:p1"})).await["error"]["code"],
        "not_supported"
    );
    task.abort();
}

/// A chat turn names its checkpoint; a turn that finds the folder as the
/// last one left it adds its name to that checkpoint instead of a copy.
#[tokio::test(flavor = "multi_thread")]
async fn chat_turns_name_their_checkpoint() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = repo(tmp.path());
    let store = Store::new(Some(tmp.path().join("checkpoints")));
    let take = |turn: &'static str| {
        let (store, dir) = (store.clone(), dir.clone());
        async move {
            store
                .snapshot_turn(&dir, &guard(), "chat:c1", "turn", Some(turn), None)
                .await
                .unwrap()
        }
    };
    assert_eq!(take("t1").await.unwrap().turns, vec!["t1".to_string()]);
    assert!(take("t2").await.is_none(), "same folder: no copy");
    std::fs::write(dir.join("a.txt"), b"changed\n").unwrap();
    assert_eq!(take("t3").await.unwrap().turns, vec!["t3".to_string()]);
    // A name that is not a turn's is not kept.
    std::fs::write(dir.join("a.txt"), b"again\n").unwrap();
    let odd = store
        .snapshot_turn(&dir, &guard(), "chat:c1", "turn", Some("x;y"), None)
        .await
        .unwrap()
        .unwrap();
    assert!(odd.turns.is_empty());
    let l = store.list(&dir, &guard(), "chat:c1").await.ok().unwrap();
    let turns: Vec<_> = l.iter().map(|i| (i.id, i.turns.clone())).collect();
    assert_eq!(
        turns,
        vec![
            (3, vec![]),
            (2, vec!["t3".to_string()]),
            (1, vec!["t1".to_string(), "t2".to_string()])
        ]
    );
    assert_eq!(l[2].json()["turns"], json!(["t1", "t2"]));
    assert!(l[0].json().get("turns").is_none());
    // The renamed checkpoint still restores its folder.
    let r = store.restore(&dir, &guard(), "chat:c1", 1).await.unwrap();
    assert_eq!(r["type"], "checkpoint_restored", "{r}");
    assert_eq!(std::fs::read(dir.join("a.txt")).unwrap(), b"one\ntwo\nthree\n");
}

/// Headless: a command task's folder is recorded before the command runs,
/// so the run can be undone, also after its pane is gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_headless_command_is_recorded_and_undone() {
    let dir = common::tempdir();
    let proj = repo(dir.path());
    let holder = start_holder(dir.path());
    let gw = FakeGateway::start().await;
    let mut o = options(dir.path(), &gw.url, holder);
    o.automation_labels = vec!["ci".into()];
    let sock = o.runner_socket.clone();
    let task = tokio::spawn(tmuxedo_runner::daemon::run(o));
    let s = std::time::Duration::from_secs(10);
    let mut c = gw.accept(s).await.expect("runner dials out");
    c.hello(s).await;
    c.text_t(s, "topology").await;
    c.send(json!({"t":"welcome","machineId":"m1","userId":"u1"})).await;
    let h = LocalH::attach_to(&sock).await;
    let before = state(&proj);
    let root = proj.to_string_lossy().into_owned();
    let r = h
        .raw(
            "task.start",
            json!({ "task_id": "undo1", "kind": "command", "labels": ["ci"], "cwd": root, "roots": [root],
                "argv": ["/bin/sh", "-c", "echo changed > a.txt; echo new > made.txt"], "allowed_commands": ["/bin/sh"] }),
        )
        .await;
    let pane = r["result"]["task"]["pane_id"].as_str().expect("a pane").to_string();
    let until = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let t = h.raw("task.get", json!({ "task_id": "undo1" })).await;
        if t["result"]["task"]["status"] != "running" {
            assert_eq!(t["result"]["task"]["status"], "done", "{t}");
            break;
        }
        assert!(std::time::Instant::now() < until, "the task never ended: {t}");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(std::fs::read(proj.join("a.txt")).unwrap(), b"changed\n");

    c.send(json!({"t":"req","id":"l","method":"checkpoint.list","params":{"pane": pane}}))
        .await;
    let l = c.res("l").await;
    assert_eq!(l["result"]["checkpoints"][0]["kind"], "turn", "{l}");
    c.send(json!({"t":"req","id":"p","method":"checkpoint.preview","params":{"pane": pane, "id": 1}}))
        .await;
    let p = c.res("p").await;
    assert_eq!(
        p["result"]["files"],
        json!([{"path": "a.txt", "change": "modified"}, {"path": "made.txt", "change": "removed"}]),
        "{p}"
    );
    c.send(json!({"t":"req","id":"r","method":"checkpoint.restore","params":{"pane": pane, "id": 1}}))
        .await;
    let r = c.res("r").await;
    assert_eq!(r["result"]["type"], "checkpoint_restored", "{r}");
    assert_eq!(state(&proj), before);
    task.abort();
}
