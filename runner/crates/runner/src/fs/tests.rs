//! The reading methods against hostile trees and hostile parameters
//! (docs/design/file-browser.md §9: T1, T2, T3, T6 to T9, T18, and the
//! refusal wording). Names from the design are kept. None needs root.

use super::*;
use crate::jail::{Guard, Root};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering as AO};

const DECOY: &str = "DECOY-OUTSIDE-THE-ROOT";

struct Tree {
    tmp: tempfile::TempDir,
    root: PathBuf,
    ctx: Ctx,
}

fn put(p: &Path, body: impl AsRef<[u8]>) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

fn tree() -> Tree {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let root = base.join("root");
    put(&root.join("README.md"), "# hello\n");
    put(&root.join("docs/a.md"), "inside-a\n");
    put(&root.join("docs/sub/b.txt"), "inside-b\n");
    put(&root.join("file10.txt"), "10");
    put(&root.join("file2.txt"), "2");
    put(&root.join(".env"), "TOKEN=1\n");
    put(&root.join("docs/id_rsa"), "x");
    put(&root.join(".git/config"), "[core]\n");
    put(&base.join("outside/secret.txt"), DECOY);
    symlink(base.join("outside/secret.txt"), root.join("link.txt")).unwrap();
    symlink(base.join("outside"), root.join("linkdir")).unwrap();
    let ctx = Ctx {
        id: "rtest".into(),
        root: Arc::new(Root::open(&root).unwrap()),
        writable: false,
        masker: Masker::default(),
    };
    Tree { tmp, root, ctx }
}

fn code(r: ApiResult) -> String {
    r.expect_err("should be refused").code
}

fn names(v: &Value) -> Vec<String> {
    v["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn a_listing_is_folders_first_in_natural_order_and_pages() {
    let t = tree();
    let all = list(&t.ctx, "", None, 500).unwrap();
    assert_eq!(
        names(&all),
        ["docs", "file2.txt", "file10.txt", "link.txt", "linkdir", "README.md"]
    );
    assert!(all.get("next").is_none());
    // Pages of two, following the cursor, give the same entries once each.
    let (mut seen, mut cursor) = (vec![], None::<Cursor>);
    for _ in 0..10 {
        let page = list(&t.ctx, "", cursor.as_ref(), 2).unwrap();
        assert!(page["entries"].as_array().unwrap().len() <= 2);
        seen.extend(names(&page));
        match page["next"].as_str() {
            Some(n) => cursor = Some(Cursor::parse(n).expect("a cursor the codec takes")),
            None => break,
        }
    }
    assert_eq!(seen, names(&all));
    assert_eq!(code(list(&t.ctx, "README.md", None, 10)), "not_a_dir");
    assert_eq!(code(list(&t.ctx, "nope", None, 10)), "not_found");
}

#[test]
fn hidden_entries_are_counted_never_named() {
    let t = tree();
    let top = list(&t.ctx, "", None, 500).unwrap();
    assert_eq!(top["hidden"], 2, ".env and .git");
    let text = top.to_string();
    assert!(!text.contains(".env") && !text.contains(".git"), "{text}");
    let docs = list(&t.ctx, "docs", None, 500).unwrap();
    assert_eq!(docs["hidden"], 1);
    assert_eq!(names(&docs), ["sub", "a.md"]);
    // A hidden folder cannot be listed, nor a hidden file read or described.
    assert_eq!(code(list(&t.ctx, ".git", None, 10)), "file_refused");
    assert_eq!(code(read(&t.ctx, ".env")), "file_refused");
    assert_eq!(code(read(&t.ctx, ".ENV")), "file_refused");
    assert_eq!(code(stat(&t.ctx, "docs/id_rsa")), "file_refused");
    assert_eq!(code(read(&t.ctx, ".git/config")), "file_refused");
    // Search: a query that matches only hidden names finds nothing and names nothing.
    for q in ["env", "id_rsa", "config", "git"] {
        let found = search(&t.ctx, q, 50).unwrap();
        assert_eq!(found["matches"], json!([]), "{q}");
        assert!(found["hidden"].as_u64().unwrap() >= 2, "{found}");
    }
}

#[test]
fn links_are_listed_as_links_and_never_opened() {
    let t = tree();
    let top = list(&t.ctx, "", None, 500).unwrap();
    for e in top["entries"].as_array().unwrap() {
        let n = e["name"].as_str().unwrap();
        if n.starts_with("link") {
            assert_eq!(e["kind"], "link", "{n}");
            assert_eq!(e.as_object().unwrap().len(), 4, "a link carries no target: {e}");
        }
    }
    assert_eq!(code(read(&t.ctx, "link.txt")), "symlink_refused");
    assert_eq!(code(stat(&t.ctx, "link.txt")), "symlink_refused");
    assert_eq!(code(list(&t.ctx, "linkdir", None, 10)), "symlink_refused");
    assert_eq!(code(read(&t.ctx, "linkdir/secret.txt")), "symlink_refused");
    // Search neither names links nor walks through them.
    let found = search(&t.ctx, "secret", 50).unwrap();
    assert_eq!(found["matches"], json!([]));
    let found = search(&t.ctx, "link", 50).unwrap();
    assert_eq!(found["matches"], json!([]));
}

#[test]
fn a_hard_linked_file_is_refused_by_read_and_stat() {
    let t = tree();
    let outside = t.tmp.path().canonicalize().unwrap().join("outside/secret.txt");
    std::fs::hard_link(&outside, t.root.join("notes.txt")).unwrap();
    assert_eq!(code(read(&t.ctx, "notes.txt")), "linked_file");
    assert_eq!(code(stat(&t.ctx, "notes.txt")), "linked_file");
}

#[test]
fn hostile_paths_are_refused_by_the_jail_whatever_the_codec_let_through() {
    let t = tree();
    // Straight at the methods, as if the codec had been skipped.
    for p in [
        "..",
        "../outside/secret.txt",
        "docs/../../outside/secret.txt",
        "/etc/passwd",
        "docs//a.md",
        "./README.md",
        "docs/",
        "docs\\a.md",
        "README.md\0",
        "C:README.md",
        "README.md.",
        "nul",
        "a\nb",
    ] {
        assert_eq!(code(read(&t.ctx, p)), "invalid_path", "{p:?}");
        assert_eq!(code(stat(&t.ctx, p)), "invalid_path", "{p:?}");
        assert_eq!(code(list(&t.ctx, p, None, 10)), "invalid_path", "{p:?}");
    }
    let long = "a/".repeat(70) + "x";
    assert_eq!(code(read(&t.ctx, &long)), "invalid_path");
    assert_eq!(code(read(&t.ctx, &"n".repeat(256))), "invalid_path");
    // And the codec refuses them first, with the right code.
    for p in ["..", "/etc/passwd", "docs//a.md", "a\nb"] {
        let e = proto::parse_read_params("fs.read", &json!({"root": "rtest", "path": p})).unwrap_err();
        assert_eq!(e.0, "invalid_path", "{p:?}");
    }
    for bad in [
        json!({"root": "rtest", "path": 7}),
        json!({"root": "../x", "path": "a"}),
        json!({"root": ["rtest"], "path": "a"}),
        json!({"path": "a"}),
        json!([]),
        json!("fs.read"),
    ] {
        assert_eq!(
            proto::parse_read_params("fs.read", &bad).unwrap_err().0,
            "invalid_params",
            "{bad}"
        );
    }
}

#[test]
fn errors_never_name_a_path_outside_the_root() {
    let t = tree();
    let base = t.tmp.path().canonicalize().unwrap();
    let outside = base.to_string_lossy().into_owned();
    for r in [
        read(&t.ctx, "link.txt"),
        read(&t.ctx, "linkdir/secret.txt"),
        read(&t.ctx, "../outside/secret.txt"),
        read(&t.ctx, "nope/none.txt"),
        read(&t.ctx, ".env"),
        list(&t.ctx, "linkdir", None, 10),
        stat(&t.ctx, "docs"),
    ] {
        let text = match r {
            Ok(v) => v.to_string(),
            Err(e) => format!("{} {}", e.code, e.message),
        };
        assert!(
            !text.contains(&outside) && !text.contains("outside/") && !text.contains(DECOY),
            "{text}"
        );
    }
    // Whether something exists outside is not told apart: both are refused the same way.
    assert_eq!(
        read(&t.ctx, "../outside/secret.txt").unwrap_err().message,
        read(&t.ctx, "../outside/no-such-file").unwrap_err().message
    );
}

#[test]
fn a_text_file_is_read_with_its_shape_and_a_version() {
    let t = tree();
    put(&t.root.join("win.txt"), "\u{feff}a\r\nb\r\n");
    put(&t.root.join("mixed.txt"), "a\r\nb\nc");
    let r = read(&t.ctx, "docs/a.md").unwrap();
    assert_eq!(r["text"], "inside-a\n");
    assert_eq!(
        (
            r["kind"].as_str(),
            r["eol"].as_str(),
            r["bom"].as_bool(),
            r["finalNewline"].as_bool()
        ),
        (Some("text"), Some("lf"), Some(false), Some(true))
    );
    assert_eq!(
        (r["name"].as_str(), r["mime"].as_str()),
        (Some("a.md"), Some("text/markdown"))
    );
    // No root is writable in this slice, so nothing is editable, and it says why.
    assert_eq!(
        (r["editable"].as_bool(), r["why"].as_str()),
        (Some(false), Some("read_only_root"))
    );
    let v = Version::parse(r["version"].as_str().unwrap()).expect("a version the codec takes");
    assert_eq!(v.size, 9);
    assert_eq!(v.sha256, hex::encode(Sha256::digest(b"inside-a\n")));
    let s = stat(&t.ctx, "docs/a.md").unwrap();
    assert_eq!(s["version"], r["version"]);
    assert_eq!(s["entry"]["kind"], "file");
    assert_eq!(stat(&t.ctx, "docs").unwrap()["entry"]["kind"], "dir");
    let w = read(&t.ctx, "win.txt").unwrap();
    assert_eq!(
        (w["text"].as_str(), w["eol"].as_str(), w["bom"].as_bool()),
        (Some("a\r\nb\r\n"), Some("crlf"), Some(true))
    );
    let m = read(&t.ctx, "mixed.txt").unwrap();
    assert_eq!(
        (m["eol"].as_str(), m["finalNewline"].as_bool()),
        (Some("mixed"), Some(false))
    );
    // The same bytes are the same content, whatever the time says.
    put(&t.root.join("copy.md"), "inside-a\n");
    let c = Version::parse(read(&t.ctx, "copy.md").unwrap()["version"].as_str().unwrap()).unwrap();
    assert!(c.same_content(&v));
}

#[test]
fn a_huge_file_is_cut_and_never_editable() {
    let t = tree();
    // Larger than an answer may be: cut to fit one frame, never editable.
    put(&t.root.join("big.txt"), "line of text\n".repeat(120_000));
    let r = read(&t.ctx, "big.txt").unwrap();
    assert_eq!(
        (r["truncated"].as_bool(), r["editable"].as_bool()),
        (Some(true), Some(false))
    );
    assert_eq!(r["why"], "too_large");
    assert!(
        r.to_string().len() < crate::api::GW_FRAME_MAX,
        "the answer fits a frame"
    );
    assert!(r["version"].is_string(), "read to its end, so it has a version");
    // Text that grows when written as JSON is cut by what it costs, not by its length.
    put(&t.root.join("quotes.txt"), "\"\\\n".repeat(200_000));
    let q = read(&t.ctx, "quotes.txt").unwrap();
    assert!(q.to_string().len() < crate::api::GW_FRAME_MAX);
    assert_eq!(q["truncated"], true);
    // A sparse 8 GiB file: only the cap is read, no version, described as binary (NULs), never text.
    let f = std::fs::File::create(t.root.join("sparse.bin")).unwrap();
    f.set_len(8 << 30).unwrap();
    let started = Instant::now();
    let s = read(&t.ctx, "sparse.bin").unwrap();
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(
        (s["kind"].as_str(), s["size"].as_u64()),
        (Some("binary"), Some(8 << 30))
    );
    assert!(s.get("text").is_none() && s.get("version").is_none());
    assert_eq!(stat(&t.ctx, "sparse.bin").unwrap()["editable"], false);
    // Over the editable size but under the preview: shown whole, not editable.
    put(&t.root.join("mid.txt"), "x".repeat(600 * 1024));
    let m = read(&t.ctx, "mid.txt").unwrap();
    assert_eq!(
        (m["truncated"].as_bool(), m["why"].as_str()),
        (Some(false), Some("too_large"))
    );
}

#[test]
fn binary_and_non_utf8_are_download_only() {
    let t = tree();
    put(&t.root.join("blob.dat"), [b'a', 0, b'b']);
    put(&t.root.join("latin1.txt"), [b'c', b'a', b'f', 0xe9, b'\n']);
    put(&t.root.join("late-nul.txt"), [vec![b'a'; 20_000], vec![0]].concat());
    put(&t.root.join("pic.png"), [0x89, b'P', b'N', b'G']);
    for (name, kind, why) in [
        ("blob.dat", "binary", "binary"),
        ("latin1.txt", "binary", "not_utf8"),
        ("late-nul.txt", "binary", "binary"),
        ("pic.png", "image", "binary"),
    ] {
        let r = read(&t.ctx, name).unwrap();
        assert_eq!(
            (r["kind"].as_str(), r["why"].as_str()),
            (Some(kind), Some(why)),
            "{name}"
        );
        assert!(
            r.get("text").is_none() && r.get("data").is_none(),
            "{name}: no content in the answer"
        );
        assert_eq!(r["editable"], false, "{name}");
        assert_eq!(stat(&t.ctx, name).unwrap()["editable"], false, "{name}");
    }
}

#[test]
fn key_material_is_refused_whatever_the_name() {
    let t = tree();
    put(
        &t.root.join("notes.txt"),
        // The marker is put together here so this file holds no key-shaped text for a secret scan to flag.
        format!(
            "some notes\n-----BEGIN OPENSSH {k} KEY-----\nb3BlbnNzaA==\n-----END OPENSSH {k} KEY-----\n",
            k = "PRIVATE"
        ),
    );
    put(
        &t.root.join("pkcs8.md"),
        format!("-----BEGIN {} KEY-----\nMIIE\n", "PRIVATE"),
    );
    put(
        &t.root.join("cert.txt"),
        "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n",
    );
    for n in ["notes.txt", "pkcs8.md"] {
        let e = read(&t.ctx, n).unwrap_err();
        assert_eq!(e.code, "file_refused", "{n}");
        assert!(!e.message.contains("b3Blb"));
        assert_eq!(code(stat(&t.ctx, n)), "file_refused", "{n}");
    }
    assert_eq!(
        read(&t.ctx, "cert.txt").unwrap()["kind"],
        "text",
        "a certificate is not a key"
    );
}

#[test]
fn text_is_masked_and_a_masked_file_is_not_editable() {
    let mut t = tree();
    t.ctx.writable = true;
    t.ctx.masker.add("s3cr3t-value-0123456789");
    put(&t.root.join("conf.txt"), "key = s3cr3t-value-0123456789\n");
    let r = read(&t.ctx, "conf.txt").unwrap();
    assert_eq!(r["masked"], true);
    assert!(!r["text"].as_str().unwrap().contains("s3cr3t-value"), "{r}");
    assert_eq!(
        (r["editable"].as_bool(), r["why"].as_str()),
        (Some(false), Some("masked"))
    );
    assert_eq!(stat(&t.ctx, "conf.txt").unwrap()["why"], "masked");
    // With a writable root, a plain small text file is the one thing that is editable.
    let ok = read(&t.ctx, "docs/a.md").unwrap();
    assert_eq!(ok["editable"], true);
    assert!(ok.get("why").is_none());
}

#[test]
fn a_fifo_or_device_never_blocks_or_reads() {
    let t = tree();
    let fifo = t.root.join("pipe");
    let c = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
    // SAFETY: a NUL-terminated path; the result is checked.
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o644) }, 0);
    let _sock = std::os::unix::net::UnixListener::bind(t.root.join("sock")).unwrap();
    let started = Instant::now();
    for n in ["pipe", "sock"] {
        assert_eq!(code(read(&t.ctx, n)), "not_a_file", "{n}");
        assert_eq!(code(stat(&t.ctx, n)), "not_a_file", "{n}");
        assert_eq!(code(list(&t.ctx, n, None, 10)), "not_a_dir", "{n}");
    }
    let top = list(&t.ctx, "", None, 500).unwrap();
    let kinds: Vec<_> = top["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["name"] == "pipe" || e["name"] == "sock")
        .map(|e| e["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["other", "other"]);
    assert_eq!(search(&t.ctx, "pipe", 50).unwrap()["matches"], json!([]));
    assert!(started.elapsed() < Duration::from_secs(2), "nothing waited on the FIFO");
}

#[test]
fn names_that_are_not_text_are_counted_not_sent() {
    let t = tree();
    let bad = std::ffi::OsStr::from_bytes(b"caf\xe9.txt");
    // APFS refuses names that are not UTF-8 (EILSEQ), so there is nothing to skip there.
    let made_bad = match std::fs::write(t.root.join(bad), "x") {
        Ok(()) => true,
        Err(e) if cfg!(target_os = "macos") && e.raw_os_error() == Some(libc::EILSEQ) => {
            eprintln!("names_that_are_not_text: this filesystem refuses non-UTF-8 names; that case is skipped");
            false
        }
        Err(e) => panic!("{e}"),
    };
    std::fs::write(t.root.join("tab\there.txt"), "x").unwrap();
    let top = list(&t.ctx, "", None, 500).unwrap();
    assert_eq!(top["skipped"], u64::from(made_bad));
    // A name with a control character is listed (JSON carries it) but the jail will not open it.
    assert!(names(&top).contains(&"tab\there.txt".to_string()));
    assert_eq!(code(read(&t.ctx, "tab\there.txt")), "invalid_path");
    let found = search(&t.ctx, "tab", 50).unwrap();
    assert_eq!(found["matches"], json!([]), "search offers only what can be opened");
    serde_json::from_str::<Value>(&top.to_string()).expect("the answer is valid JSON");
}

#[test]
fn search_stops_at_its_bounds() {
    let t = tree();
    for i in 0..300 {
        put(&t.root.join(format!("many/hit-{i}.txt")), "");
    }
    put(&t.root.join("node_modules/pkg/hit-in-heavy.txt"), "");
    let mut deep = t.root.join("deep");
    for i in 0..20 {
        deep = deep.join(format!("d{i}"));
    }
    put(&deep.join("hit-at-the-bottom.txt"), "");
    // Results: capped, and it says so.
    let r = search(&t.ctx, "HIT", 50).unwrap();
    assert_eq!(r["matches"].as_array().unwrap().len(), 50);
    assert_eq!(r["partial"], true);
    assert_eq!(
        search(&t.ctx, "hit", 3).unwrap()["matches"].as_array().unwrap().len(),
        3
    );
    // Depth and heavy folders: never reached.
    let b = search(&t.ctx, "bottom", 50).unwrap();
    assert_eq!((b["matches"].clone(), b["partial"].as_bool()), (json!([]), Some(true)));
    assert_eq!(search(&t.ctx, "heavy", 50).unwrap()["matches"], json!([]));
    // Entries walked: a small budget ends the walk early.
    let w = search_within(&t.ctx, "hit-299", 50, 5, SEARCH_TIME).unwrap();
    assert_eq!((w["matches"].clone(), w["partial"].as_bool()), (json!([]), Some(true)));
    // Time: no time at all is partial, not an error.
    let z = search_within(&t.ctx, "hit", 50, SEARCH_WALK_MAX, Duration::ZERO).unwrap();
    assert_eq!(z["partial"], true);
    // A plain find, folded like names are.
    let one = search(&t.ctx, "readme", 50).unwrap();
    assert_eq!(one["matches"], json!([{ "path": "README.md", "kind": "file" }]));
}

#[test]
fn swap_race_never_reads_outside_through_fs_read() {
    let t = tree();
    let base = t.tmp.path().canonicalize().unwrap();
    put(&base.join("outside/a.md"), DECOY);
    let (stop, root) = (Arc::new(AtomicBool::new(false)), t.root.clone());
    let flipper = {
        let (stop, outside) = (stop.clone(), base.join("outside"));
        std::thread::spawn(move || {
            let (docs, aside) = (root.join("docs"), root.join("docs.real"));
            while !stop.load(AO::Relaxed) {
                if std::fs::rename(&docs, &aside).is_ok() {
                    let _ = symlink(&outside, &docs);
                    std::thread::yield_now();
                    let _ = std::fs::remove_file(&docs);
                    let _ = std::fs::rename(&aside, &docs);
                }
            }
        })
    };
    let (mut inside, mut refused) = (0u32, 0u32);
    let until = Instant::now() + Duration::from_millis(1500);
    while Instant::now() < until || inside + refused < 3000 {
        match read(&t.ctx, "docs/a.md") {
            Ok(v) => {
                assert_eq!(v["text"], "inside-a\n", "read something that is not the file inside");
                inside += 1;
            }
            Err(e) => {
                assert!(!e.message.contains(DECOY));
                refused += 1;
            }
        }
        if let Ok(v) = list(&t.ctx, "docs", None, 50) {
            assert!(
                !names(&v).contains(&"secret.txt".to_string()),
                "listed the folder outside"
            );
        }
    }
    stop.store(true, AO::Relaxed);
    flipper.join().unwrap();
    assert!(
        inside > 0 && refused > 0,
        "both happened: {inside} inside, {refused} refused"
    );
}

fn src(kind: &'static str, path: &Path) -> Source {
    Source {
        kind,
        label: path
            .file_name()
            .map_or("/".into(), |n| n.to_string_lossy().into_owned()),
        path: path.to_path_buf(),
    }
}

#[test]
fn roots_come_from_the_records_and_too_broad_ones_are_refused_and_shown() {
    let t = tree();
    let home = t.tmp.path().canonicalize().unwrap().join("home");
    let state = home.join(".local/state/app");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::create_dir_all(home.join("code/site")).unwrap();
    let guard = Guard {
        home: Some(home.clone()),
        own: vec![state.clone()],
    };
    let mut roots = Roots::default();
    let sources = [
        src("workspace", &t.root),
        src("workspace", &home),
        src("workspace", Path::new("/")),
        src("workspace", Path::new("/etc")),
        src("project", &home.join(".local")),
        src("project", &state.join("deeper")),
        src("project", &t.root.join("docs")),
        src("project", &home.join("code/site")),
        src("project", &t.root.join("no-such-folder")),
    ];
    roots.sync(&sources, &guard);
    let d = roots.describe();
    let listed: Vec<&str> = d["roots"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["label"].as_str().unwrap())
        .collect();
    assert_eq!(listed, ["root", "docs", "site"]);
    let why: Vec<(&str, &str)> = d["refused"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["label"].as_str().unwrap(), r["why"].as_str().unwrap()))
        .collect();
    for want in [
        ("home", "too_broad"),
        ("/", "too_broad"),
        ("etc", "too_broad"),
        (".local", "too_broad"),
    ] {
        assert!(why.contains(&want), "{want:?} in {why:?}");
    }
    assert!(why.iter().any(|w| w.1 == "unreachable"));
    // Workspace and project roots are writable (slice A2); the runner still wants web's flags.
    assert_eq!((d["canWrite"].as_bool(), d.get("why")), (Some(true), None));
    // No answer carries a path of the machine: ids and labels only.
    assert!(!d.to_string().contains(&*t.root.to_string_lossy()), "{d}");
    // A root inside another names it.
    let outer = roots.id_for(&t.root).unwrap().to_string();
    let inner = d["roots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["label"] == "docs")
        .unwrap();
    assert_eq!(inner["place"], json!({ "root": outer, "path": "docs" }));
    assert!(proto::valid_root_id(&outer));
    // Ids are the only way in: an id nobody gave is unknown.
    assert_eq!(roots.get("r0000000000000000").err().unwrap().code, "root_unknown");
    assert!(
        roots.get(&outer).unwrap().1,
        "a workspace or project root is writable (slice A2)"
    );
    // Syncing again opens nothing twice and keeps the ids.
    roots.sync(&sources, &guard);
    assert_eq!(roots.id_for(&t.root), Some(outer.as_str()));
}

#[test]
fn a_replaced_root_is_gone_not_followed() {
    let t = tree();
    let base = t.tmp.path().canonicalize().unwrap();
    let mut roots = Roots::default();
    let sources = [src("workspace", &t.root)];
    roots.sync(&sources, &Guard::default());
    let id = roots.id_for(&t.root).unwrap().to_string();
    assert!(roots.get(&id).is_ok());
    // The folder is moved away and a link to another place takes its name.
    std::fs::rename(&t.root, base.join("root.moved")).unwrap();
    symlink(base.join("outside"), &t.root).unwrap();
    assert_eq!(roots.get(&id).err().unwrap().code, "root_gone");
    roots.sync(&sources, &Guard::default());
    assert_eq!(
        roots.get(&id).err().unwrap().code,
        "root_gone",
        "not reopened by its path"
    );
    let d = roots.describe();
    assert_eq!(d["roots"], json!([]));
    assert_eq!(d["refused"][0]["why"], "gone");
    // Only its source going away and coming back registers it again.
    roots.sync(&[], &Guard::default());
    assert_eq!(roots.get(&id).err().unwrap().code, "root_unknown");
}

#[test]
fn observe_only_refuses_every_mutation() {
    let g = crate::guardrails::Guardrails {
        allow_user: None,
        observe_only: true,
    };
    for m in proto::MUTATING_METHODS {
        assert!(!g.allows_method(m), "{m}");
    }
    for m in METHODS {
        assert!(g.allows_method(m), "{m} only reads");
    }
    // The served list is the reading list: nothing that changes files is dispatched.
    assert!(METHODS.iter().all(|m| !proto::MUTATING_METHODS.contains(m)));
}

// ── §3.5: the added names, through every reading method ──────────────

#[test]
fn the_added_secret_names_are_hidden_from_every_reading_method() {
    let t = tree();
    // Top-level files that must be counted and never named.
    let denied_top = [
        "terraform.tfstate",
        "terraform.tfstate.backup",
        "prod.auto.tfvars",
        ".vault-token",
        "kubeconfig",
        "passwords.kdbx",
        "office.ovpn",
        ".bash_history",
        ".psql_history",
        "service-account-prod.json",
        ".envrc",
    ];
    // `.terraform`, `.gcloud`, `.kube` and `.ssh` are hidden themselves; `.config` is shown, its `gcloud` and `gh` are not.
    let denied_deep = [
        ".terraform/terraform.tfstate",
        ".terraform/modules/readme.md",
        ".gcloud/readme.md",
        ".kube/cache/readme.md",
        ".ssh/readme.md",
        ".config/gcloud/readme.md",
        ".config/gh/hosts.yml",
        "docs/sub/.aws/readme.md",
        "docs/.zsh_history",
    ];
    for p in denied_top.iter().chain(denied_deep.iter()) {
        put(&t.root.join(p), "MARK-hidden-content\n");
    }
    let near = [
        "kubeconfig.md",
        "history.txt",
        "tfstate.md",
        "vault-token",
        "service_account.json",
        "bash_history",
        ".config/app/readme.md",
    ];
    for p in near {
        put(&t.root.join(p), "near miss\n");
    }

    // fs.list: counted, never named; the near misses are there.
    let top = list(&t.ctx, "", None, 500).unwrap();
    // The tree's own `.env` and `.git`, the files above, and the four hidden folders.
    assert_eq!(top["hidden"], 2 + denied_top.len() as u64 + 4, "{top}");
    let shown = names(&top);
    for p in denied_top
        .iter()
        .chain([".terraform", ".gcloud", ".kube", ".ssh"].iter())
    {
        assert!(!shown.iter().any(|n| n == p), "{p} is listed");
    }
    for p in &near[..6] {
        assert!(shown.iter().any(|n| n == p), "{p} should be listed: {shown:?}");
    }
    assert!(shown.iter().any(|n| n == ".config"));
    let config = list(&t.ctx, ".config", None, 500).unwrap();
    assert_eq!(names(&config), ["app"]);
    assert_eq!(config["hidden"], 2, "gcloud and gh");
    assert_eq!(
        list(&t.ctx, "docs", None, 500).unwrap()["hidden"],
        2,
        "id_rsa and .zsh_history"
    );
    assert_eq!(list(&t.ctx, "docs/sub", None, 500).unwrap()["hidden"], 1, ".aws");
    assert!(!top.to_string().contains("MARK-hidden"));

    // fs.list and fs.stat of a hidden folder: refused, in any case.
    for d in [
        ".terraform",
        ".terraform/modules",
        ".gcloud",
        ".config/gcloud",
        ".config/GH",
        ".kube/cache",
        ".SSH",
        "docs/sub/.aws",
    ] {
        assert_eq!(code(list(&t.ctx, d, None, 10)), "file_refused", "{d}");
        assert_eq!(code(stat(&t.ctx, d)), "file_refused", "{d}");
    }
    // fs.stat and fs.read of a hidden file: refused, the same whether or not anything is there
    // (nothing is confirmed; no size, no version, no content).
    let absent = [
        "nothing.tfstate",
        ".fish_history",
        "nowhere/.vault-token",
        ".terraform/absent",
        "service-account.json",
    ];
    for p in denied_top.iter().chain(denied_deep.iter()).chain(absent.iter()) {
        for spelt in [p.to_string(), p.to_uppercase()] {
            let s = stat(&t.ctx, &spelt).unwrap_err();
            let r = read(&t.ctx, &spelt).unwrap_err();
            assert_eq!(
                (s.code.as_str(), r.code.as_str()),
                ("file_refused", "file_refused"),
                "{spelt}"
            );
            assert!(!s.message.contains("MARK") && !r.message.contains("MARK"));
        }
    }
    assert_eq!(
        stat(&t.ctx, "nothing.tfstate").unwrap_err().message,
        stat(&t.ctx, "terraform.tfstate").unwrap_err().message,
        "a refusal does not say whether the file exists"
    );
    for p in near {
        assert_eq!(read(&t.ctx, p).unwrap()["text"], "near miss\n", "{p}");
        assert!(stat(&t.ctx, p).is_ok(), "{p}");
    }

    // fs.search (names only: file content is never searched): no hidden name, nothing beneath a hidden folder.
    for q in [
        "tfstate",
        "tfvars",
        "vault",
        "kubeconfig",
        "kdbx",
        "ovpn",
        "history",
        "service",
        "envrc",
        "readme",
        "hosts",
        "terraform",
        "gcloud",
        "MARK",
    ] {
        let found = search(&t.ctx, q, 50).unwrap();
        let mut got: Vec<&str> = found["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["path"].as_str().unwrap())
            .collect();
        got.sort_unstable();
        for p in &got {
            assert!(jail::secret_reason(p).is_none(), "{q}: {p}");
        }
        let want: &[&str] = match q {
            "tfstate" => &["tfstate.md"],
            "vault" => &["vault-token"],
            "kubeconfig" => &["kubeconfig.md"],
            "history" => &["bash_history", "history.txt"],
            "service" => &["service_account.json"],
            "readme" => &[".config/app/readme.md", "README.md"],
            _ => &[],
        };
        assert_eq!(got, want, "{q}");
        assert!(!found.to_string().contains("MARK-hidden"));
    }
    assert!(search(&t.ctx, "readme", 50).unwrap()["hidden"].as_u64().unwrap() >= 10);
}

fn dl(path: &str, offset: u64, length: u64) -> DownloadParams {
    DownloadParams {
        root: "rtest".into(),
        path: path.into(),
        xfer: "dl_0123456789ab".into(),
        n: 3,
        offset,
        length,
    }
}

#[test]
fn a_download_goes_through_the_jail_and_sends_the_bytes_as_frames() {
    let t = tree();
    put(&t.root.join("data.bin"), [0u8, 1, 2, 3, 4, 5, 6, 7]);
    // length 0: the size and the checks only, no frames.
    let (v, frames) = download(&t.ctx, &dl("data.bin", 0, 0)).unwrap();
    assert_eq!(
        (v["size"].as_u64(), v["bytes"].as_u64(), frames.len()),
        (Some(8), Some(0), 0)
    );
    assert_eq!(v["name"], "data.bin");
    let (v, frames) = download(&t.ctx, &dl("data.bin", 2, 4)).unwrap();
    assert_eq!((v["bytes"].as_u64(), v["eof"].as_bool()), (Some(4), Some(false)));
    let f = FileFrame::decode(&frames[0]).unwrap();
    assert_eq!(
        (f.id.as_str(), f.req, f.offset, f.payload.as_slice()),
        ("dl_0123456789ab", 3, 2, &[2u8, 3, 4, 5][..])
    );
    // What the jail refuses is refused: secret names, links, outside, folders.
    let vcs = [".", "git/config"].concat();
    for (p, want) in [
        (".env", "file_refused"),
        ("docs/id_rsa", "file_refused"),
        (vcs.as_str(), "file_refused"),
        ("link.txt", "symlink_refused"),
        ("linkdir/secret.txt", "symlink_refused"),
        ("docs", "not_a_file"),
        ("nope.txt", "not_found"),
    ] {
        let e = download(&t.ctx, &dl(p, 0, 1 << 20)).unwrap_err();
        assert_eq!(e.code, want, "{p}");
        assert!(!e.message.contains(DECOY));
    }
}

#[test]
fn a_download_refuses_key_material_and_text_with_hidden_values() {
    let mut t = tree();
    t.ctx.masker.add("s3cr3t-value-0123456789");
    put(&t.root.join("conf.txt"), "key = s3cr3t-value-0123456789\n");
    put(
        &t.root.join("notes.txt"),
        format!("x\n-----BEGIN OPENSSH {} KEY-----\nb3Bl\n", "PRIVATE"),
    );
    assert_eq!(download(&t.ctx, &dl("conf.txt", 0, 0)).unwrap_err().code, "masked_file");
    assert_eq!(
        download(&t.ctx, &dl("notes.txt", 0, 0)).unwrap_err().code,
        "file_refused"
    );
    // A value past the first 2 MiB is caught in the window that holds it.
    let mut big = vec![b'a'; TEXT_MAX as usize + 10];
    big.extend_from_slice(b" s3cr3t-value-0123456789\n");
    put(&t.root.join("big.log"), &big);
    assert!(download(&t.ctx, &dl("big.log", 0, 1024)).is_ok());
    let e = download(&t.ctx, &dl("big.log", TEXT_MAX, 1 << 20)).unwrap_err();
    assert_eq!(e.code, "masked_file");
    // A binary is sent as it is.
    let mut bin = vec![0u8; 16];
    bin.extend_from_slice(b"s3cr3t-value-0123456789");
    put(&t.root.join("blob.bin"), &bin);
    assert!(download(&t.ctx, &dl("blob.bin", 0, 1 << 20)).is_ok());
}
