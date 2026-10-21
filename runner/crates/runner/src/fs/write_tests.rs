//! The writing methods against hostile trees, races and failures
//! (docs/design/file-browser.md §9: T6, T9, T12 to T15, T17 for slice A2).
//! None needs root.

use super::write::{self, Store};
use super::*;
use crate::jail::Root;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering as AO};
use tmuxedo_protocol::fs::parse_write_params;

const DECOY: &str = "DECOY-OUTSIDE-THE-ROOT";

struct T {
    _tmp: tempfile::TempDir,
    base: PathBuf,
    root: PathBuf,
    ctx: Ctx,
    store: Store,
    n: u32,
}

fn put(p: &Path, body: impl AsRef<[u8]>) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

fn tree() -> T {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let root = base.join("root");
    put(&root.join("README.md"), "# hello\n");
    put(&root.join("docs/a.md"), "inside-a\n");
    put(&root.join(".env"), "X=1\n");
    put(&base.join("outside/secret.txt"), DECOY);
    symlink(base.join("outside/secret.txt"), root.join("link.txt")).unwrap();
    symlink(base.join("outside"), root.join("linkdir")).unwrap();
    std::fs::hard_link(root.join("README.md"), base.join("outside/hard.md")).ok();
    put(&root.join("solo.md"), "solo\n");
    let ctx = Ctx {
        id: "rtest".into(),
        root: Arc::new(Root::open(&root).unwrap()),
        writable: true,
        masker: Masker::default(),
    };
    let store = Store::with_max(base.join("state/fs-undo"), write::UNDO_MAX_BYTES);
    T {
        _tmp: tmp,
        base,
        root,
        ctx,
        store,
        n: 0,
    }
}

impl T {
    /// One call as web sends it: control and a fresh step-up, a new op_id.
    fn call(&mut self, method: &str, mut p: Value) -> ApiResult {
        self.n += 1;
        p["root"] = json!("rtest");
        p["op_id"] = json!(format!("op{}", self.n));
        p["control"] = json!(true);
        p["stepup_at"] = json!(write::now_ms());
        let req = parse_write_params(method, &p).map_err(|(c, m)| err(c, m))?;
        write::run(&mut self.store, &self.ctx, &req, write::now_ms())
    }
    fn version(&self, rel: &str) -> String {
        stat(&self.ctx, rel).unwrap()["version"].as_str().unwrap().to_string()
    }
    fn outside_untouched(&self) {
        assert_eq!(
            std::fs::read_to_string(self.base.join("outside/secret.txt")).unwrap(),
            DECOY
        );
        let names: Vec<_> = std::fs::read_dir(self.base.join("outside"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().into_string().unwrap())
            .collect();
        assert!(
            names.iter().all(|n| n == "secret.txt" || n == "hard.md"),
            "something was written outside: {names:?}"
        );
    }
    fn no_temp_left(&self, dir: &str) {
        let left: Vec<_> = std::fs::read_dir(self.root.join(dir))
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(".tmuxedo-"))
            .collect();
        assert!(left.is_empty(), "temporary files left: {left:?}");
    }
}

fn code(r: ApiResult) -> String {
    r.expect_err("should be refused").code
}

#[test]
fn a_mutation_without_control_flags_is_refused() {
    let mut t = tree();
    let now = write::now_ms();
    let base = json!({ "root": "rtest", "path": "new.md", "text": "x", "create": true, "op_id": "a1" });
    let cases = [
        (json!({}), "control_required"),
        (json!({ "control": false, "stepup_at": now }), "control_required"),
        (json!({ "control": true }), "stepup_required"),
        (
            json!({ "control": true, "stepup_at": now - 16 * 60 * 1000 }),
            "stepup_required",
        ),
        (
            json!({ "control": true, "stepup_at": now + 60 * 60 * 1000 }),
            "stepup_required",
        ),
    ];
    for (flags, want) in cases {
        let mut p = base.clone();
        for (k, v) in flags.as_object().unwrap() {
            p[k] = v.clone();
        }
        let req = parse_write_params("fs.write", &p).unwrap();
        assert_eq!(code(write::run(&mut t.store, &t.ctx, &req, now)), want, "{p}");
    }
    assert!(!t.root.join("new.md").exists());
    // A read-only root refuses even with every flag.
    t.ctx.writable = false;
    assert_eq!(code(t.call("fs.mkdir", json!({ "path": "d" }))), "read_only");
    assert!(!t.root.join("d").exists());
}

#[test]
fn create_replace_and_undo_round_trip() {
    let mut t = tree();
    let made = t
        .call(
            "fs.write",
            json!({ "path": "docs/new.md", "text": "one\n", "create": true }),
        )
        .unwrap();
    assert_eq!(made["type"], "fs_written");
    assert_eq!(std::fs::read_to_string(t.root.join("docs/new.md")).unwrap(), "one\n");
    assert_eq!(
        code(t.call(
            "fs.write",
            json!({ "path": "docs/new.md", "text": "x", "create": true })
        )),
        "exists"
    );
    let v1 = made["version"].as_str().unwrap().to_string();
    assert_eq!(v1, t.version("docs/new.md"));
    let saved = t
        .call(
            "fs.write",
            json!({ "path": "docs/new.md", "text": "two\n", "base": v1 }),
        )
        .unwrap();
    assert_eq!(std::fs::read_to_string(t.root.join("docs/new.md")).unwrap(), "two\n");
    // A stale base is a conflict and nothing is written.
    assert_eq!(
        code(t.call(
            "fs.write",
            json!({ "path": "docs/new.md", "text": "three\n", "base": v1 })
        )),
        "conflict"
    );
    assert_eq!(std::fs::read_to_string(t.root.join("docs/new.md")).unwrap(), "two\n");
    // Undo puts "one" back; the same undo twice is gone.
    let u = saved["undoId"].as_str().unwrap().to_string();
    let undone = t.call("fs.undo", json!({ "undoId": u })).unwrap();
    assert_eq!(undone["type"], "fs_undone");
    assert_eq!(std::fs::read_to_string(t.root.join("docs/new.md")).unwrap(), "one\n");
    assert_eq!(code(t.call("fs.undo", json!({ "undoId": u }))), "not_found");
    // Undo of the create removes the file while its content is what the create left (it is again).
    let first = made["undoId"].as_str().unwrap().to_string();
    t.call("fs.undo", json!({ "undoId": first })).unwrap();
    assert!(!t.root.join("docs/new.md").exists());
    t.no_temp_left("docs");
}

#[test]
fn an_undo_after_someone_else_edited_is_a_conflict() {
    let mut t = tree();
    let v = t.version("solo.md");
    let saved = t
        .call("fs.write", json!({ "path": "solo.md", "text": "mine\n", "base": v }))
        .unwrap();
    put(&t.root.join("solo.md"), "agent\n");
    assert_eq!(
        code(t.call("fs.undo", json!({ "undoId": saved["undoId"] }))),
        "conflict"
    );
    assert_eq!(std::fs::read_to_string(t.root.join("solo.md")).unwrap(), "agent\n");
}

#[test]
fn a_crash_between_temp_write_and_rename_leaves_the_old_file() {
    let mut t = tree();
    let v = t.version("solo.md");
    crate::jail::FAIL_SWAP.with(|f| f.set(true));
    let r = t.call("fs.write", json!({ "path": "solo.md", "text": "new\n", "base": v }));
    assert!(r.is_err(), "{r:?}");
    assert_eq!(std::fs::read_to_string(t.root.join("solo.md")).unwrap(), "solo\n");
    t.no_temp_left("");
    crate::jail::FAIL_SWAP.with(|f| f.set(true));
    assert!(t
        .call("fs.write", json!({ "path": "fresh.md", "text": "x", "create": true }))
        .is_err());
    assert!(!t.root.join("fresh.md").exists());
    t.no_temp_left("");
    // The failure was journalled as failed after started.
    let j = std::fs::read_to_string(t.base.join("state/fs-undo/journal.jsonl")).unwrap();
    assert!(j.contains("\"failed\""), "{j}");
}

#[test]
fn every_mutating_method_emits_started_then_done() {
    let mut t = tree();
    t.call("fs.mkdir", json!({ "path": "d" })).unwrap();
    t.call("fs.write", json!({ "path": "d/f.md", "text": "x", "create": true }))
        .unwrap();
    let v = t.version("d/f.md");
    t.call("fs.rename", json!({ "from": "d/f.md", "to": "d/g.md", "base": v }))
        .unwrap();
    let v = t.version("d/g.md");
    let del = t.call("fs.delete", json!({ "path": "d/g.md", "base": v })).unwrap();
    t.call("fs.undo", json!({ "undoId": del["undoId"] })).unwrap();
    let j = std::fs::read_to_string(t.base.join("state/fs-undo/journal.jsonl")).unwrap();
    let lines: Vec<Value> = j.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    let ops: Vec<(String, String)> = lines
        .iter()
        .map(|l| (l["op"].as_str().unwrap().into(), l["state"].as_str().unwrap().into()))
        .collect();
    for op in ["mkdir", "write", "rename", "delete", "undo"] {
        let i = ops.iter().position(|o| o == &(op.into(), "started".into())).expect(op);
        assert_eq!(ops[i + 1], (op.into(), "done".into()), "{ops:?}");
    }
    // Paths are hashed, never written.
    assert!(!j.contains("g.md") && !j.contains("\"d\""), "{j}");
}

#[test]
fn hostile_paths_and_secret_names_are_refused_by_every_method() {
    let mut t = tree();
    let v = t.version("solo.md");
    for p in [
        "../x.md",
        "/abs.md",
        "a/../b.md",
        "docs/./a.md",
        "CON",
        "nul.txt",
        "com1.md",
        "a:b",
        "x.",
        "x ",
    ] {
        let c = code(t.call("fs.write", json!({ "path": p, "text": "x", "create": true })));
        assert_eq!(c, "invalid_path", "{p}");
        assert_eq!(code(t.call("fs.mkdir", json!({ "path": p }))), "invalid_path", "{p}");
        let c = code(t.call("fs.rename", json!({ "from": "solo.md", "to": p, "base": v })));
        assert_eq!(c, "invalid_path", "{p}");
    }
    // Secret names, on either side of a rename, and folded.
    for p in [
        ".env",
        ".ENV",
        "docs/id_rsa",
        ".git/config",
        ".ssh",
        "x.tfstate",
        ".git",
    ] {
        assert_eq!(
            code(t.call("fs.write", json!({ "path": p, "text": "x", "create": true }))),
            "file_refused",
            "{p}"
        );
        assert_eq!(code(t.call("fs.mkdir", json!({ "path": p }))), "file_refused", "{p}");
        assert_eq!(
            code(t.call("fs.rename", json!({ "from": "solo.md", "to": p }))),
            "file_refused",
            "{p}"
        );
    }
    // fs::rename_cannot_launder_a_secret_file
    assert_eq!(
        code(t.call("fs.rename", json!({ "from": ".env", "to": "notes.txt" }))),
        "file_refused"
    );
    assert_eq!(
        code(t.call("fs.delete", json!({ "path": ".env", "base": v }))),
        "file_refused"
    );
    assert!(t.root.join(".env").exists() && !t.root.join("notes.txt").exists());
    // Links: never written through, renamed or removed; a folder link is not entered.
    assert_eq!(
        code(t.call("fs.write", json!({ "path": "link.txt", "text": "x", "base": v }))),
        "symlink_refused"
    );
    assert_eq!(
        code(t.call("fs.delete", json!({ "path": "link.txt", "base": v }))),
        "symlink_refused"
    );
    assert_eq!(
        code(t.call("fs.rename", json!({ "from": "link.txt", "to": "l2" }))),
        "symlink_refused"
    );
    assert_eq!(
        code(t.call(
            "fs.write",
            json!({ "path": "linkdir/new.md", "text": "x", "create": true })
        )),
        "symlink_refused"
    );
    assert_eq!(
        code(t.call("fs.mkdir", json!({ "path": "linkdir/d" }))),
        "symlink_refused"
    );
    // A hard-linked file is not replaced, renamed or deleted.
    let hv = format!("v1.{}.8.0", hex::encode(Sha256::digest(b"# hello\n")));
    assert_eq!(
        code(t.call("fs.write", json!({ "path": "README.md", "text": "x", "base": hv }))),
        "linked_file"
    );
    assert_eq!(
        code(t.call("fs.delete", json!({ "path": "README.md", "base": hv }))),
        "linked_file"
    );
    assert_eq!(
        code(t.call("fs.rename", json!({ "from": "README.md", "to": "R.md" }))),
        "linked_file"
    );
    assert_eq!(std::fs::read_to_string(t.root.join("README.md")).unwrap(), "# hello\n");
    t.outside_untouched();
}

#[test]
fn oversize_masked_and_key_material_are_never_written() {
    let mut t = tree();
    put(&t.root.join("big.txt"), vec![b'a'; (EDITABLE_MAX + 1) as usize]);
    let bv = format!("v1.{}.{}.0", "0".repeat(64), EDITABLE_MAX + 1);
    assert_eq!(
        code(t.call("fs.write", json!({ "path": "big.txt", "text": "x", "base": bv }))),
        "too_large"
    );
    let big = "a".repeat(EDITABLE_MAX as usize + 1);
    assert_eq!(
        code(t.call("fs.write", json!({ "path": "n.txt", "text": big, "create": true }))),
        "too_large"
    );
    assert_eq!(
        code(t.call(
            "fs.write",
            json!({ "path": "n.txt", "text": "a\u{0}b", "create": true })
        )),
        "binary_file"
    );
    t.ctx.masker.add("s3cr3t-value-0123456789");
    put(&t.root.join("conf.txt"), "key = s3cr3t-value-0123456789\n");
    let cv = format!(
        "v1.{}.31.0",
        hex::encode(Sha256::digest(b"key = s3cr3t-value-0123456789\n"))
    );
    assert_eq!(
        code(t.call(
            "fs.write",
            json!({ "path": "conf.txt", "text": "key = ****\n", "base": cv })
        )),
        "masked_file"
    );
    let key = format!("-----BEGIN {} KEY-----\nMIIE\n", "PRIVATE");
    put(&t.root.join("k.md"), &key);
    let kv = format!("v1.{}.{}.0", hex::encode(Sha256::digest(key.as_bytes())), key.len());
    assert_eq!(
        code(t.call("fs.delete", json!({ "path": "k.md", "base": kv }))),
        "file_refused"
    );
    assert!(t.root.join("k.md").exists());
}

#[test]
fn mkdir_rename_and_delete_with_undo() {
    let mut t = tree();
    let m = t.call("fs.mkdir", json!({ "path": "docs/new" })).unwrap();
    assert!(t.root.join("docs/new").is_dir());
    assert_eq!(code(t.call("fs.mkdir", json!({ "path": "docs/new" }))), "exists");
    assert_eq!(code(t.call("fs.mkdir", json!({ "path": "nope/new" }))), "not_found");
    t.call("fs.undo", json!({ "undoId": m["undoId"] })).unwrap();
    assert!(!t.root.join("docs/new").exists());
    // A rename never replaces.
    put(&t.root.join("docs/b.md"), "b\n");
    assert_eq!(
        code(t.call("fs.rename", json!({ "from": "docs/a.md", "to": "docs/b.md" }))),
        "exists"
    );
    assert_eq!(std::fs::read_to_string(t.root.join("docs/b.md")).unwrap(), "b\n");
    // Moving within the root, then back with Undo; a folder too.
    let r = t
        .call("fs.rename", json!({ "from": "docs/a.md", "to": "moved.md" }))
        .unwrap();
    assert!(t.root.join("moved.md").exists() && !t.root.join("docs/a.md").exists());
    t.call("fs.undo", json!({ "undoId": r["undoId"] })).unwrap();
    assert!(t.root.join("docs/a.md").exists());
    t.call("fs.rename", json!({ "from": "docs", "to": "papers" })).unwrap();
    assert!(t.root.join("papers/a.md").exists());
    assert_eq!(
        code(t.call("fs.rename", json!({ "from": "papers", "to": "papers/x" }))),
        "invalid_path"
    );
    // Delete: a file needs its base; a folder must be empty; Undo puts the file back.
    assert_eq!(
        code(t.call("fs.delete", json!({ "path": "papers/a.md" }))),
        "invalid_params"
    );
    assert_eq!(code(t.call("fs.delete", json!({ "path": "papers" }))), "not_empty");
    let v = t.version("papers/a.md");
    let d = t
        .call("fs.delete", json!({ "path": "papers/a.md", "base": v }))
        .unwrap();
    assert!(!t.root.join("papers/a.md").exists());
    t.no_temp_left("papers");
    let dd = t
        .call(
            "fs.delete",
            json!({ "path": "papers/b.md", "base": t.version("papers/b.md") }),
        )
        .unwrap();
    t.call("fs.delete", json!({ "path": "papers" })).unwrap();
    assert!(!t.root.join("papers").exists());
    assert_eq!(code(t.call("fs.undo", json!({ "undoId": d["undoId"] }))), "not_found");
    let _ = dd;
    // The kept content is in the runner's state folder, never in the tree.
    let kept = std::fs::read_dir(t.base.join("state/fs-undo")).unwrap().count();
    assert!(kept > 2);
}

#[test]
fn a_deleted_file_comes_back_with_its_bytes_and_mode() {
    use std::os::unix::fs::PermissionsExt;
    let mut t = tree();
    std::fs::set_permissions(t.root.join("solo.md"), std::fs::Permissions::from_mode(0o640)).unwrap();
    let d = t
        .call("fs.delete", json!({ "path": "solo.md", "base": t.version("solo.md") }))
        .unwrap();
    assert!(!t.root.join("solo.md").exists());
    t.call("fs.undo", json!({ "undoId": d["undoId"] })).unwrap();
    assert_eq!(std::fs::read_to_string(t.root.join("solo.md")).unwrap(), "solo\n");
    let mode = std::fs::metadata(t.root.join("solo.md")).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o640);
}

#[test]
fn the_undo_store_is_bounded_oldest_first() {
    let mut t = tree();
    t.store = Store::with_max(t.base.join("state/fs-undo"), 64);
    let mut ids = vec![];
    for i in 0..4 {
        let rel = format!("f{i}.md");
        put(&t.root.join(&rel), "x".repeat(30));
        let d = t
            .call("fs.delete", json!({ "path": rel, "base": t.version(&rel) }))
            .unwrap();
        ids.push(d["undoId"].as_str().unwrap().to_string());
        std::thread::sleep(std::time::Duration::from_millis(1100));
    }
    // 64 bytes hold two 30-byte files: the two oldest were dropped.
    assert_eq!(code(t.call("fs.undo", json!({ "undoId": ids[0] }))), "not_found");
    assert_eq!(code(t.call("fs.undo", json!({ "undoId": ids[1] }))), "not_found");
    t.call("fs.undo", json!({ "undoId": ids[3] })).unwrap();
    // A file larger than the whole store is not deleted at all.
    put(&t.root.join("huge.md"), "y".repeat(100));
    assert_eq!(
        code(t.call("fs.delete", json!({ "path": "huge.md", "base": t.version("huge.md") }))),
        "too_large"
    );
    assert!(t.root.join("huge.md").exists());
}

#[test]
fn a_retry_with_the_same_op_id_gets_the_first_answer() {
    let mut t = tree();
    let p = json!({ "root": "rtest", "path": "once.md", "text": "1", "create": true, "op_id": "same",
                    "control": true, "stepup_at": write::now_ms() });
    let req = parse_write_params("fs.write", &p).unwrap();
    let a = write::run(&mut t.store, &t.ctx, &req, write::now_ms()).unwrap();
    let b = write::run(&mut t.store, &t.ctx, &req, write::now_ms()).unwrap();
    assert_eq!(a, b);
}

/// T3 for writes: `docs` flips between a folder and a link to a decoy
/// folder outside while creates, saves, renames and deletes run under it.
#[test]
fn swap_race_never_writes_outside() {
    let mut t = tree();
    put(&t.root.join("docs/keep.md"), "k\n");
    let stop = Arc::new(AtomicBool::new(false));
    let (docs, parked, decoy) = (t.root.join("docs"), t.root.join("docs.parked"), t.base.join("outside"));
    let flipper = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            let mut n = 0u64;
            while !stop.load(AO::Relaxed) {
                std::fs::rename(&docs, &parked).unwrap();
                symlink(&decoy, &docs).unwrap();
                std::thread::yield_now();
                std::fs::remove_file(&docs).unwrap();
                std::fs::rename(&parked, &docs).unwrap();
                n += 1;
            }
            n
        })
    };
    let started = std::time::Instant::now();
    let (mut ok, mut tries) = (0, 0);
    while started.elapsed() < std::time::Duration::from_millis(1500) {
        tries += 1;
        let rel = format!("docs/w{tries}.md");
        if t.call("fs.write", json!({ "path": rel, "text": "w", "create": true }))
            .is_ok()
        {
            ok += 1;
            let _ = t.call("fs.rename", json!({ "from": rel, "to": format!("docs/r{tries}.md") }));
        }
        let _ = t.call("fs.mkdir", json!({ "path": format!("docs/d{tries}") }));
    }
    stop.store(true, AO::Relaxed);
    let flips = flipper.join().unwrap();
    assert!(
        flips > 0 && ok > 0,
        "the race proved nothing: {flips} flips, {ok} writes"
    );
    t.outside_untouched();
    eprintln!("write swap race: {tries} rounds, {ok} creates, {flips} flips");
}

/// The target itself swapped for a link (or another file) while saves run:
/// the save is a conflict or lands on the real file; the decoy never changes
/// and no link is ever replaced by our bytes silently.
#[test]
fn a_target_swapped_during_a_save_is_a_conflict_never_a_write_through() {
    let mut t = tree();
    let stop = Arc::new(AtomicBool::new(false));
    let (target, parked, decoy) = (
        t.root.join("solo.md"),
        t.root.join("solo.parked"),
        t.base.join("outside/secret.txt"),
    );
    let flipper = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            while !stop.load(AO::Relaxed) {
                if std::fs::rename(&target, &parked).is_ok() {
                    // Where there is no exchange (macOS), a save's plain rename can put a file
                    // back under the name in the gap: then this round has no link.
                    if symlink(&decoy, &target).is_ok() {
                        std::thread::yield_now();
                        let _ = std::fs::remove_file(&target);
                    }
                    std::fs::rename(&parked, &target).unwrap();
                }
            }
        })
    };
    let started = std::time::Instant::now();
    let (mut saved, mut refused) = (0, 0);
    while started.elapsed() < std::time::Duration::from_millis(1500) {
        let Ok(s) = stat(&t.ctx, "solo.md") else { continue };
        let v = s["version"].as_str().unwrap().to_string();
        match t.call(
            "fs.write",
            json!({ "path": "solo.md", "text": format!("save {saved}\n"), "base": v }),
        ) {
            Ok(_) => saved += 1,
            Err(e) => {
                assert!(
                    ["conflict", "symlink_refused", "not_found", "exists"].contains(&e.code.as_str()),
                    "{e:?}"
                );
                refused += 1;
            }
        }
    }
    stop.store(true, AO::Relaxed);
    flipper.join().unwrap();
    t.outside_untouched();
    // When the folder keeps changing under a save, what came out is kept under the temporary
    // name rather than deleted: every leftover is one of the file's own versions, never the decoy.
    for e in std::fs::read_dir(&t.root).unwrap().flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with(".tmuxedo-") {
            let m = std::fs::symlink_metadata(e.path()).unwrap();
            assert!(m.is_file(), "a link was left: {name}");
            let body = std::fs::read_to_string(e.path()).unwrap();
            assert!(body == "solo\n" || body.starts_with("save "), "{name}: {body}");
        }
    }
    eprintln!("target swap race: {saved} saves, {refused} refused");
}
