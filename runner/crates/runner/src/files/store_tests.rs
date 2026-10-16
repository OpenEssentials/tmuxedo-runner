//! The store, driven directly: chunking, hashes, resume across a new Store
//! (a runner restart), abort, quota, the free-space floor, confinement and
//! traversal, symlinks, sweeps, pinning, sharing, reading for downloads.

use super::store::{valid_entry_name, Store};
use super::worker::{Msg, Out, Worker};
use super::Limits;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use tmuxedo_protocol::files::{FileAck, FileCtl, FileFrame, CHUNK_MIN, FRAME_PAYLOAD};

const CS: u64 = CHUNK_MIN; // 256 KiB chunks keep the tests small
const GIB: u64 = 1024 * 1024 * 1024;

fn sha(b: &[u8]) -> String {
    hex::encode(Sha256::digest(b))
}

fn data(n: usize, seed: u8) -> Vec<u8> {
    (0..n).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)).collect()
}

struct T {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

fn setup() -> T {
    let dir = tempfile::tempdir().unwrap();
    let root = super::prepare_root(&dir.path().join("files")).unwrap();
    T { _dir: dir, root }
}

fn limits() -> Limits {
    Limits {
        fake_disk: Some((100 * GIB, 200 * GIB)),
        ..Limits::default()
    }
}

fn store(t: &T) -> Store {
    Store::new(t.root.clone(), limits())
}

fn init(s: &mut Store, id: &str, body: &[u8]) -> Value {
    s.upload_init(&json!({
        "upload_id": id, "project": "github.com_org_repo", "name": "Screen Shot 1.png",
        "size": body.len(), "chunk_size": CS, "by": "u1", "source": "web",
    }))
    .unwrap()
}

/// Send chunk n of body in frames of `frame` bytes; the final ack.
fn put(s: &mut Store, id: &str, body: &[u8], n: u64, frame: usize) -> Option<FileAck> {
    let start = (n * CS) as usize;
    let end = (start + CS as usize).min(body.len());
    let chunk = &body[start..end];
    if let Some(a) = s.put_begin(id, n as u32, chunk.len() as u64, &sha(chunk)) {
        return Some(a);
    }
    let mut last = None;
    for (i, part) in chunk.chunks(frame).enumerate() {
        last = s.put_data(&FileFrame {
            id: id.into(),
            req: n as u32,
            offset: (start + i * frame) as u64,
            payload: part.to_vec(),
        });
    }
    last
}

fn ok(a: Option<FileAck>) {
    let a = a.expect("an ack");
    assert!(a.ok, "{a:?}");
}

#[test]
fn an_upload_in_any_order_lands_in_the_inbox_read_only_with_provenance() {
    let t = setup();
    let mut s = store(&t);
    let body = data((CS * 3 + 1000) as usize, 7);
    let r = init(&mut s, "up_order_0001", &body);
    assert_eq!(r["chunkCount"], 4);
    assert_eq!(r["name"], "Screen_Shot_1.png");
    for n in [3, 1, 0, 2] {
        ok(put(&mut s, "up_order_0001", &body, n, 64 * 1024));
    }
    let done = s
        .upload_complete(&json!({"upload_id": "up_order_0001", "sha256": sha(&body)}))
        .unwrap();
    let path = PathBuf::from(done["path"].as_str().unwrap());
    assert!(path.starts_with(t.root.join("github.com_org_repo/inbox")), "{path:?}");
    let name = done["name"].as_str().unwrap();
    assert!(
        name.ends_with("_Screen_Shot_1.png") && name.len() == "2026-10-03T14-02-11Z_".len() + "Screen_Shot_1.png".len(),
        "{name}"
    );
    assert_eq!(std::fs::read(&path).unwrap(), body);
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o444, "read-only, never executable");
    let side: Value = serde_json::from_slice(
        &std::fs::read(path.parent().unwrap().join(".meta").join(format!("{name}.json"))).unwrap(),
    )
    .unwrap();
    assert_eq!(side["untrusted"], true);
    assert_eq!(side["sha256"], sha(&body));
    assert_eq!(side["originalName"], "Screen Shot 1.png");
    assert_eq!(side["source"], "web");
    // Completing twice answers twice (a retried request), with one file.
    let again = s
        .upload_complete(&json!({"upload_id": "up_order_0001", "sha256": sha(&body)}))
        .unwrap();
    assert_eq!(again, done);
    assert_eq!(
        std::fs::read_dir(t.root.join("github.com_org_repo/inbox"))
            .unwrap()
            .count(),
        2,
        "the file and .meta"
    );
    let note = s.notes.drain(..).next().unwrap();
    assert_eq!(note.kind, "file_uploaded");
    assert_eq!(note.data["sha256"], sha(&body));
    // The partial data is gone (renamed); only the remembered answer stays.
    assert!(!t.root.join(".partial/up_order_0001/data").exists());
    // Listing shows it.
    let l = s
        .list(&json!({"project": "github.com_org_repo", "dir": "inbox"}))
        .unwrap();
    assert_eq!(l["files"][0]["name"], name);
    assert_eq!(l["files"][0]["size"], body.len());
    assert_eq!(l["files"][0]["source"], "web");
    assert!(l["files"][0]["expiresAt"].as_u64().is_some());
}

#[test]
fn a_damaged_chunk_is_refused_and_can_be_sent_again() {
    let t = setup();
    let mut s = store(&t);
    let body = data((CS * 2) as usize, 1);
    init(&mut s, "up_damage_01", &body);
    let chunk = &body[..CS as usize];
    assert!(s.put_begin("up_damage_01", 0, CS, &sha(chunk)).is_none());
    let mut bad = chunk.to_vec();
    bad[100] ^= 0xff;
    let a = s
        .put_data(&FileFrame {
            id: "up_damage_01".into(),
            req: 0,
            offset: 0,
            payload: bad,
        })
        .unwrap();
    assert!(!a.ok && a.code.as_deref() == Some("chunk_hash_mismatch"), "{a:?}");
    let st = s.upload_status(&json!({"upload_id": "up_damage_01"})).unwrap();
    assert_eq!(st["received"], json!([]));
    ok(put(&mut s, "up_damage_01", &body, 0, FRAME_PAYLOAD));
    // Out of order bytes inside a chunk are refused (the gateway sends them in order).
    assert!(s.put_begin("up_damage_01", 1, CS, &sha(&body[CS as usize..])).is_none());
    let a = s
        .put_data(&FileFrame {
            id: "up_damage_01".into(),
            req: 1,
            offset: CS + 10,
            payload: vec![0; 10],
        })
        .unwrap();
    assert_eq!(a.code.as_deref(), Some("chunk_out_of_order"));
    // More bytes than the chunk holds: refused.
    assert!(s.put_begin("up_damage_01", 1, CS, &sha(&body[CS as usize..])).is_none());
    let a = s
        .put_data(&FileFrame {
            id: "up_damage_01".into(),
            req: 1,
            offset: CS,
            payload: vec![0; CS as usize + 1],
        })
        .unwrap();
    assert_eq!(a.code.as_deref(), Some("chunk_out_of_order"));
    // A wrong size or number never starts.
    assert_eq!(
        s.put_begin("up_damage_01", 1, CS - 1, &sha(b"x"))
            .unwrap()
            .code
            .as_deref(),
        Some("invalid_chunk")
    );
    assert_eq!(
        s.put_begin("up_damage_01", 2, CS, &sha(b"x")).unwrap().code.as_deref(),
        Some("invalid_chunk")
    );
    assert_eq!(
        s.put_begin("up_unknown_1", 0, CS, &sha(b"x")).unwrap().code.as_deref(),
        Some("upload_not_found")
    );
    // Not complete yet.
    let e = s.upload_complete(&json!({"upload_id": "up_damage_01"})).unwrap_err();
    assert_eq!(e.code, "incomplete");
    ok(put(&mut s, "up_damage_01", &body, 1, FRAME_PAYLOAD));
    // A whole-file hash that differs discards the upload.
    let e = s
        .upload_complete(&json!({"upload_id": "up_damage_01", "sha256": "0".repeat(64)}))
        .unwrap_err();
    assert_eq!(e.code, "file_hash_mismatch");
    assert!(!t.root.join(".partial/up_damage_01").exists());
}

#[test]
fn resume_after_a_runner_restart_keeps_the_chunks_on_disk() {
    let t = setup();
    let body = data((CS * 3) as usize, 9);
    {
        let mut s = store(&t);
        init(&mut s, "up_resume_01", &body);
        ok(put(&mut s, "up_resume_01", &body, 0, FRAME_PAYLOAD));
        ok(put(&mut s, "up_resume_01", &body, 2, FRAME_PAYLOAD));
        // Chunk 1 is half sent when the runner dies.
        let c1 = &body[CS as usize..2 * CS as usize];
        assert!(s.put_begin("up_resume_01", 1, CS, &sha(c1)).is_none());
        assert!(s
            .put_data(&FileFrame {
                id: "up_resume_01".into(),
                req: 1,
                offset: CS,
                payload: c1[..1000].to_vec()
            })
            .is_none());
    }
    // A new runner: the same root, a new Store.
    let mut s = store(&t);
    let st = s.upload_status(&json!({"upload_id": "up_resume_01"})).unwrap();
    assert_eq!(st["received"], json!([0, 2]), "the half chunk is not counted");
    // Frames of the dead runner's chunk are ignored (no put_begin on this one).
    assert!(s
        .put_data(&FileFrame {
            id: "up_resume_01".into(),
            req: 1,
            offset: CS + 1000,
            payload: vec![1; 10]
        })
        .is_none());
    ok(put(&mut s, "up_resume_01", &body, 1, 4096));
    let done = s
        .upload_complete(&json!({"upload_id": "up_resume_01", "sha256": sha(&body)}))
        .unwrap();
    assert_eq!(std::fs::read(done["path"].as_str().unwrap()).unwrap(), body);
    // Init again with the same id and shape answers the same upload; another shape is refused.
    let again = init(&mut s, "up_resume_01", &body);
    assert_eq!(again["received"], json!([0, 1, 2]));
    let e = s
        .upload_init(&json!({"upload_id": "up_resume_01", "project": "p1", "name": "x", "size": 5, "chunk_size": CS}))
        .unwrap_err();
    assert_eq!(e.code, "upload_conflict");
}

#[test]
fn abort_removes_everything_and_always_succeeds() {
    let t = setup();
    let mut s = store(&t);
    let body = data(1000, 2);
    init(&mut s, "up_abort_001", &body);
    assert!(t.root.join(".partial/up_abort_001/data").exists());
    s.upload_abort(&json!({"upload_id": "up_abort_001"})).unwrap();
    assert!(!t.root.join(".partial/up_abort_001").exists());
    s.upload_abort(&json!({"upload_id": "up_abort_001"})).unwrap();
    s.upload_abort(&json!({"upload_id": "../../etc"})).unwrap();
    assert_eq!(
        s.put_begin("up_abort_001", 0, 1000, &sha(&body))
            .unwrap()
            .code
            .as_deref(),
        Some("upload_not_found")
    );
}

#[test]
fn quota_floor_and_size_limits() {
    let t = setup();
    // 1 GiB quota, the disk has 10 GiB free of 20 (floor 2 GiB).
    let mut s = Store::new(
        t.root.clone(),
        Limits {
            fake_disk: Some((10 * GIB, 20 * GIB)),
            quota: Some(GIB),
            ..Limits::default()
        },
    );
    let q = s.quota().unwrap();
    assert_eq!(q["quotaBytes"], GIB);
    assert_eq!(q["floorBytes"], 2 * GIB);
    assert_eq!(q["level"], "ok");
    assert_eq!(q["uploadsAllowed"], true);
    let big = |id: &str, size: u64| json!({"upload_id": id, "project": "p1", "name": "big.bin", "size": size, "chunk_size": 16 * 1024 * 1024});
    let e = s.upload_init(&big("up_quota_001", GIB + 1)).unwrap_err();
    assert_eq!(e.code, "quota_exceeded");
    s.upload_init(&big("up_quota_002", GIB / 2)).unwrap();
    // A second half-gigabyte plus one byte: the first upload's declared size counts.
    let e = s.upload_init(&big("up_quota_003", GIB / 2 + 1)).unwrap_err();
    assert_eq!(e.code, "quota_exceeded");
    // Too large for the machine's limit.
    let e = s.upload_init(&big("up_quota_004", 3 * GIB)).unwrap_err();
    assert_eq!(e.code, "file_too_large");
    // The floor: 2.5 GiB free of 20 GiB, a 1 GiB upload would leave 1.5 GiB < 2 GiB.
    let mut s = Store::new(
        t.root.clone(),
        Limits {
            fake_disk: Some((5 * GIB / 2, 20 * GIB)),
            ..Limits::default()
        },
    );
    let e = s.upload_init(&big("up_floor_001", GIB)).unwrap_err();
    assert_eq!(e.code, "insufficient_storage");
    assert!(e.message.contains("must stay free"), "{}", e.message);
    let q = s.quota().unwrap();
    assert_eq!(q["level"], "ok");
    // The disk fills during an upload: the next chunk stops it.
    let body = data(CS as usize * 2, 3);
    let mut s = Store::new(t.root.clone(), limits());
    init(&mut s, "up_fill_0001", &body);
    ok(put(&mut s, "up_fill_0001", &body, 0, FRAME_PAYLOAD));
    s.limits.fake_disk = Some((GIB / 2, 20 * GIB));
    let a = put(&mut s, "up_fill_0001", &body, 1, FRAME_PAYLOAD).unwrap();
    assert_eq!(a.code.as_deref(), Some("insufficient_storage"));
    assert!(
        !t.root.join(".partial/up_fill_0001").exists(),
        "the upload was stopped and removed"
    );
}

#[test]
fn confinement_traversal_and_symlinks() {
    let t = setup();
    let mut s = store(&t);
    // Hostile names never leave the folder.
    for bad in ["../x", "..", ".", "", "a/b", "a\\b", ".meta", ".hidden", "x\0y"] {
        assert!(!valid_entry_name(bad), "{bad:?}");
        let e = s
            .stat(&json!({"project": "p1", "dir": "inbox", "name": bad}))
            .unwrap_err();
        assert_eq!(e.code, "invalid_params", "{bad:?}");
        assert!(s
            .delete(&json!({"project": "p1", "dir": "inbox", "name": bad}))
            .is_err());
    }
    for bad in ["..", "../p", "a/b", ".", "", ".partial"] {
        assert!(s.list(&json!({"project": bad})).is_err(), "{bad:?}");
    }
    assert!(s.list(&json!({"project": "p1", "dir": "../etc"})).is_err());
    // A symlinked project folder, folder or file is refused, never followed.
    let outside = t._dir.path().join("outside");
    std::fs::create_dir_all(outside.join("inbox")).unwrap();
    std::fs::write(outside.join("inbox/secret.txt"), "secret").unwrap();
    std::os::unix::fs::symlink(&outside, t.root.join("linked")).unwrap();
    assert_eq!(s.list(&json!({"project": "linked"})).unwrap_err().code, "file_refused");
    s.ensure_project("p2").unwrap();
    std::os::unix::fs::symlink(outside.join("inbox/secret.txt"), t.root.join("p2/inbox/link.txt")).unwrap();
    let l = s.list(&json!({"project": "p2", "dir": "inbox"})).unwrap();
    assert_eq!(l["files"], json!([]), "a link is not listed");
    let e = s
        .read(&json!({"xfer": "dl_00000001", "project": "p2", "dir": "inbox", "name": "link.txt"}))
        .unwrap_err();
    assert_eq!(e.code, "file_refused");
    assert!(s
        .stat(&json!({"project": "p2", "dir": "inbox", "name": "link.txt"}))
        .is_err());
    // Deleting a link removes the link, not its target.
    s.delete(&json!({"project": "p2", "dir": "inbox", "name": "link.txt"}))
        .unwrap();
    assert!(outside.join("inbox/secret.txt").exists());
    // Upload names cannot traverse either.
    let body = b"hello".to_vec();
    s.upload_init(
        &json!({"upload_id": "up_trav_0001", "project": "p2", "name": "../../../.bashrc", "size": 5, "chunk_size": CS}),
    )
    .unwrap();
    ok(put(&mut s, "up_trav_0001", &body, 0, FRAME_PAYLOAD));
    let done = s.upload_complete(&json!({"upload_id": "up_trav_0001"})).unwrap();
    assert!(done["name"].as_str().unwrap().ends_with("_bashrc"));
    assert!(Path::new(done["path"].as_str().unwrap()).starts_with(t.root.join("p2/inbox")));
    // A hostile project or upload id at init.
    assert!(s
        .upload_init(&json!({"upload_id": "up_trav_0002", "project": "../x", "name": "a", "size": 1, "chunk_size": CS}))
        .is_err());
    assert!(s
        .upload_init(&json!({"upload_id": "../../x", "project": "p2", "name": "a", "size": 1, "chunk_size": CS}))
        .is_err());
}

#[test]
fn reads_for_downloads_come_in_frames_with_offsets() {
    let t = setup();
    let mut s = store(&t);
    let body = data(FRAME_PAYLOAD * 2 + 77, 5);
    s.ensure_project("p3").unwrap();
    std::fs::write(t.root.join("p3/outbox/report.bin"), &body).unwrap();
    let (r, frames) = s
        .read(&json!({"xfer": "dl_abcdefgh", "n": 4, "project": "p3", "dir": "outbox", "name": "report.bin", "offset": 10}))
        .unwrap();
    assert_eq!(r["size"], body.len());
    assert_eq!(r["bytes"], body.len() - 10);
    assert_eq!(r["eof"], true);
    let mut got = vec![];
    let mut at = 10u64;
    for f in &frames {
        let f = FileFrame::decode(f).unwrap();
        assert_eq!((f.id.as_str(), f.req, f.offset), ("dl_abcdefgh", 4, at));
        assert!(f.payload.len() <= FRAME_PAYLOAD);
        at += f.payload.len() as u64;
        got.extend(f.payload);
    }
    assert_eq!(got, body[10..]);
    // A window in the middle.
    let (r, frames) = s
        .read(&json!({"xfer": "dl_abcdefgh", "project": "p3", "dir": "outbox", "name": "report.bin", "offset": 5, "length": 3}))
        .unwrap();
    assert_eq!((r["bytes"].as_u64(), r["eof"].as_bool()), (Some(3), Some(false)));
    assert_eq!(FileFrame::decode(&frames[0]).unwrap().payload, body[5..8]);
    // Past the end: nothing.
    let (r, frames) = s
        .read(&json!({"xfer": "dl_abcdefgh", "project": "p3", "dir": "outbox", "name": "report.bin", "offset": 1_000_000_000}))
        .unwrap();
    assert_eq!((r["bytes"].as_u64(), frames.len()), (Some(0), 0));
    assert_eq!(
        s.read(&json!({"xfer": "dl_abcdefgh", "project": "p3", "dir": "outbox", "name": "nope"}))
            .unwrap_err()
            .code,
        "file_not_found"
    );
}

#[test]
fn sweeps_pins_and_deletes() {
    let t = setup();
    let mut s = store(&t);
    s.ensure_project("p4").unwrap();
    for n in ["old.txt", "pinned.txt", "new.txt"] {
        std::fs::write(t.root.join("p4/inbox").join(n), n).unwrap();
    }
    s.pin(&json!({"project": "p4", "dir": "inbox", "name": "pinned.txt", "pinned": true}))
        .unwrap();
    let l = s.list(&json!({"project": "p4", "dir": "inbox"})).unwrap();
    let pinned = l["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == "pinned.txt")
        .unwrap();
    assert_eq!(pinned["pinned"], true);
    assert!(pinned.get("expiresAt").is_none());
    // An expired partial and a fresh one.
    init(&mut s, "up_sweep_old", b"abc");
    init(&mut s, "up_sweep_new", b"abc");
    let mut st: Value =
        serde_json::from_slice(&std::fs::read(t.root.join(".partial/up_sweep_old/state.json")).unwrap()).unwrap();
    st["lastMs"] = json!(1);
    std::fs::write(t.root.join(".partial/up_sweep_old/state.json"), st.to_string()).unwrap();
    let mut s = store(&t); // fresh cache, as after a restart
    let now = crate::util::now_ms();
    assert_eq!(s.sweep(now, false), (1, 0));
    assert!(!t.root.join(".partial/up_sweep_old").exists());
    assert!(t.root.join(".partial/up_sweep_new").exists());
    // Fifteen days later: everything not pinned goes; the fresh partial too.
    let later = now + 15 * 24 * 3600 * 1000;
    let (p, f) = s.sweep(later, true);
    assert_eq!((p, f), (1, 2));
    let left: Vec<String> = std::fs::read_dir(t.root.join("p4/inbox"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(left.contains(&"pinned.txt".to_string()) && left.len() == 2, "{left:?}");
    // The sweep never follows a link out of the root.
    let outside = t._dir.path().join("outside2");
    std::fs::create_dir_all(outside.join("inbox")).unwrap();
    std::fs::write(outside.join("inbox/keep.txt"), "x").unwrap();
    std::os::unix::fs::symlink(&outside, t.root.join("evil")).unwrap();
    s.sweep(later, true);
    assert!(outside.join("inbox/keep.txt").exists());
    // Unpin, delete.
    s.pin(&json!({"project": "p4", "dir": "inbox", "name": "pinned.txt", "pinned": false}))
        .unwrap();
    s.delete(&json!({"project": "p4", "dir": "inbox", "name": "pinned.txt"}))
        .unwrap();
    assert_eq!(
        s.delete(&json!({"project": "p4", "dir": "inbox", "name": "pinned.txt"}))
            .unwrap_err()
            .code,
        "file_not_found"
    );
}

#[test]
fn share_copies_from_the_work_folder_only_and_never_secrets() {
    let t = setup();
    let mut s = store(&t);
    let work = t._dir.path().join("work/repo");
    std::fs::create_dir_all(work.join("out")).unwrap();
    std::fs::write(work.join("out/report final.pdf"), b"%PDF-1.7").unwrap();
    std::fs::write(work.join(".env"), "KEY=1").unwrap();
    std::fs::write(t._dir.path().join("elsewhere.txt"), "no").unwrap();
    let allowed = json!([work.display().to_string()]);
    let r = s
        .share(&json!({"project": "p5", "path": work.join("out/report final.pdf"), "note": "the Q3 report\u{7}", "allowed": allowed, "pane_id": "w1:p1"}))
        .unwrap();
    assert_eq!(r["name"], "report_final.pdf");
    assert_eq!(
        std::fs::read(t.root.join("p5/outbox/report_final.pdf")).unwrap(),
        b"%PDF-1.7"
    );
    let n = s.notes.drain(..).next().unwrap();
    assert_eq!((n.kind, n.pane_id.as_str()), ("file_shared", "w1:p1"));
    assert_eq!(n.data["note"], "the Q3 report");
    // Twice: a second name, never an overwrite.
    let r2 = s
        .share(&json!({"project": "p5", "path": work.join("out/report final.pdf"), "allowed": allowed}))
        .unwrap();
    assert_eq!(r2["name"], "report_final-1.pdf");
    let l = s.list(&json!({"project": "p5", "dir": "outbox"})).unwrap();
    assert!(l["files"]
        .as_array()
        .unwrap()
        .iter()
        .any(|f| f["note"] == "the Q3 report"));
    // Refusals: outside the allowed folders, a secret file, a link, relative paths, a folder.
    let e = s
        .share(&json!({"project": "p5", "path": t._dir.path().join("elsewhere.txt"), "allowed": allowed}))
        .unwrap_err();
    assert_eq!(e.code, "file_refused");
    let e = s
        .share(&json!({"project": "p5", "path": work.join(".env"), "allowed": allowed}))
        .unwrap_err();
    assert_eq!(e.code, "file_refused");
    assert!(e.message.contains("secrets"));
    std::os::unix::fs::symlink(t._dir.path().join("elsewhere.txt"), work.join("sneaky.txt")).unwrap();
    let e = s
        .share(&json!({"project": "p5", "path": work.join("sneaky.txt"), "allowed": allowed}))
        .unwrap_err();
    assert_eq!(e.code, "file_refused", "a link to a file outside resolves outside");
    assert_eq!(
        s.share(&json!({"project": "p5", "path": "out/x", "allowed": allowed}))
            .unwrap_err()
            .code,
        "invalid_params"
    );
    assert_eq!(
        s.share(&json!({"project": "p5", "path": work.join("out"), "allowed": allowed}))
            .unwrap_err()
            .code,
        "file_refused"
    );
    s.notes.clear();
    // A request records what the agent needs.
    let r = s
        .request(&json!({"project": "p5", "what": "  the CSV export ", "pane_id": "w1:p2"}))
        .unwrap();
    assert!(r["inbox"].as_str().unwrap().ends_with("p5/inbox"));
    let n = s.notes.drain(..).next().unwrap();
    assert_eq!(
        (n.kind, n.data["what"].as_str()),
        ("file_requested", Some("the CSV export"))
    );
    assert_eq!(
        s.request(&json!({"project": "p5", "what": " "})).unwrap_err().code,
        "invalid_params"
    );
}

#[test]
fn the_worker_answers_frames_calls_and_reports_the_disk() {
    let t = setup();
    let mut w = Worker::new(Store::new(
        t.root.clone(),
        Limits {
            fake_disk: Some((GIB, 100 * GIB)),
            ..Limits::default()
        },
    ));
    let mut outs = vec![];
    w.handle(Msg::DiskNow, &mut |o| outs.push(o));
    let disk = outs.iter().find_map(|o| match o {
        Out::Text(t) => Some(serde_json::from_str::<Value>(t).unwrap()),
        _ => None,
    });
    assert_eq!(disk.unwrap()["level"], "alert");
    assert!(outs.iter().any(|o| matches!(o, Out::Level("alert"))));
    assert!(outs.iter().any(|o| matches!(o, Out::Event(n) if n.kind == "disk_low")));
    // The same level again: no second disk_low.
    outs.clear();
    w.handle(Msg::DiskNow, &mut |o| outs.push(o));
    assert!(!outs.iter().any(|o| matches!(o, Out::Event(_))));
    // An upload refused by the floor answers the gateway with a file_ack.
    outs.clear();
    w.handle(
        Msg::Ctl(FileCtl::Put {
            upload_id: "up_worker_01".into(),
            n: 0,
            size: 5,
            sha256: sha(b"hello"),
        }),
        &mut |o| outs.push(o),
    );
    match &outs[0] {
        Out::Text(t) => assert!(t.contains("upload_not_found"), "{t}"),
        _ => panic!(),
    }
}

/// The phone's background upload: the whole file's SHA-256 given at the start,
/// the chunks sent by the OS with no app left to call `complete`; the upload
/// lands with its last chunk (in any order) and says so (`file_uploaded`,
/// with the request it answers and the chat it was for). A wrong hash aborts
/// it; without a hash nothing lands until `complete`.
#[test]
fn an_upload_with_its_hash_given_lands_with_its_last_chunk() {
    let t = setup();
    let mut w = Worker::new(store(&t));
    let body = data(CS as usize * 2 + 77, 9);
    let start = |w: &mut Worker, id: &str, sha256: Option<String>| {
        let mut outs = vec![];
        w.handle(
            Msg::Call {
                method: "files.upload_init".into(),
                params: json!({
                    "upload_id": id, "project": "github.com_org_repo", "name": "IMG_2041.HEIC", "size": body.len(),
                    "chunk_size": CS, "by": "u1", "source": "app", "request_id": "Rq7c1d2e3f4a",
                    "session_id": "c-abcdefgh12", "sha256": sha256,
                }),
                reply: crate::api::ReplyTo::Gateway("i".into()),
            },
            &mut |o| outs.push(o),
        );
        outs
    };
    let send = |w: &mut Worker, id: &str, n: u64| {
        let s = (n * CS) as usize;
        let chunk = &body[s..(s + CS as usize).min(body.len())];
        let mut outs = vec![];
        w.handle(
            Msg::Ctl(FileCtl::Put {
                upload_id: id.into(),
                n: n as u32,
                size: chunk.len() as u64,
                sha256: sha(chunk),
            }),
            &mut |o| outs.push(o),
        );
        for (i, part) in chunk.chunks(FRAME_PAYLOAD).enumerate() {
            let f = FileFrame {
                id: id.into(),
                req: n as u32,
                offset: (s + i * FRAME_PAYLOAD) as u64,
                payload: part.to_vec(),
            };
            w.handle(Msg::Data(f), &mut |o| outs.push(o));
        }
        outs
    };
    let landed = |outs: &[Out]| {
        outs.iter().find_map(|o| match o {
            Out::Event(n) if n.kind == "file_uploaded" => Some(n.data.clone()),
            _ => None,
        })
    };
    start(&mut w, "up_bg_hash_01", Some(sha(&body)));
    assert!(landed(&send(&mut w, "up_bg_hash_01", 2)).is_none());
    assert!(landed(&send(&mut w, "up_bg_hash_01", 0)).is_none());
    let d = landed(&send(&mut w, "up_bg_hash_01", 1)).expect("landed with its last chunk");
    assert_eq!(
        (d["requestId"].clone(), d["sessionId"].clone(), d["source"].clone()),
        (json!("Rq7c1d2e3f4a"), json!("c-abcdefgh12"), json!("app"))
    );
    let path = d["path"].as_str().unwrap().to_string();
    assert_eq!(std::fs::read(&path).unwrap(), body);
    // The app's own complete later (back in the foreground) is the same answer, not a second file.
    let again = w
        .store
        .upload_complete(&json!({"upload_id": "up_bg_hash_01", "sha256": sha(&body)}))
        .unwrap();
    assert_eq!(again["path"], path.as_str());

    // A hash that does not match: nothing lands.
    start(&mut w, "up_bg_hash_02", Some(sha(b"something else")));
    let mut outs = vec![];
    for n in 0..3 {
        outs.extend(send(&mut w, "up_bg_hash_02", n));
    }
    assert!(landed(&outs).is_none());

    // No hash given: every chunk there, still nothing until complete.
    start(&mut w, "up_bg_hash_03", None);
    let mut outs = vec![];
    for n in 0..3 {
        outs.extend(send(&mut w, "up_bg_hash_03", n));
    }
    assert!(landed(&outs).is_none());
}
