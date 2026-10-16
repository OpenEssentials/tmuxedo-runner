//! The jail's tests: one per threat of docs/design/file-browser.md §9 that
//! the resolver answers (T1 to T6, T8, T23's lexical half). The names are
//! the design's. Every test that opens something runs twice: the default
//! resolution (`openat2` where the kernel has it) and the component walk
//! on its own (the path older kernels and macOS take). None needs root.

use super::*;
use proptest::prelude::*;
use proptest::test_runner::{Config, TestRunner};
use std::os::unix::fs::symlink;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const ENGINES: [Engine; 2] = [Engine::Auto, Engine::Walk];
const NO_LIMIT: Limits = Limits { max_size: None };
/// What every file outside the root holds. No read may ever return it.
const DECOY: &str = "DECOY: outside the root\n";
/// The only regular files a request can open in [`tree`], and what they hold.
const INSIDE: [(&str, &str); 3] = [
    ("top.txt", "top\n"),
    ("docs/a.md", "inside-a\n"),
    ("docs/sub/b.txt", "inside-b\n"),
];

struct Tree {
    /// Holds the temporary folder (removed on drop).
    _tmp: tempfile::TempDir,
    /// `<tmp>/root`, as created (not made real).
    path: PathBuf,
    /// `<tmp>/outside`: decoys with the same names as the files inside.
    outside: PathBuf,
    root: Root,
}

/// ```text
/// <tmp>/root/top.txt  docs/a.md  docs/sub/b.txt  .env
///            link.txt -> ../outside/secret.txt      (a link to a file outside)
///            linkdir  -> ../outside                 (a link to a folder outside)
///            abs      -> /                          (an absolute link)
///            self     -> .                          (a link that stays inside)
///            docs/inner -> sub                      (a link that stays inside, deeper)
///            docs/sub/out -> ../../../outside       (a link out, three folders down)
///            gone     -> nowhere                    (a dangling link)
///            hard.txt                               (a second name of outside/secret.txt)
/// <tmp>/outside/secret.txt  top.txt  a.md  b.txt  sub/b.txt  docs/a.md
/// ```
fn tree() -> Tree {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("root");
    let outside = tmp.path().join("outside");
    std::fs::create_dir_all(path.join("docs/sub")).unwrap();
    for (rel, text) in INSIDE {
        std::fs::write(path.join(rel), text).unwrap();
    }
    std::fs::write(path.join(".env"), "KEY=inside-secret\n").unwrap();
    std::fs::create_dir_all(outside.join("sub")).unwrap();
    std::fs::create_dir_all(outside.join("docs")).unwrap();
    for name in ["secret.txt", "top.txt", "a.md", "b.txt", "sub/b.txt", "docs/a.md"] {
        std::fs::write(outside.join(name), DECOY).unwrap();
    }
    symlink("../outside/secret.txt", path.join("link.txt")).unwrap();
    symlink("../outside", path.join("linkdir")).unwrap();
    symlink("/", path.join("abs")).unwrap();
    symlink(".", path.join("self")).unwrap();
    symlink("sub", path.join("docs/inner")).unwrap();
    symlink("../../../outside", path.join("docs/sub/out")).unwrap();
    symlink("nowhere", path.join("gone")).unwrap();
    std::fs::hard_link(outside.join("secret.txt"), path.join("hard.txt")).unwrap();
    let root = Root::open(&path).unwrap();
    Tree {
        _tmp: tmp,
        path,
        outside,
        root,
    }
}

/// A path with every component folded (a case-insensitive volume may report another spelling).
fn folded(rel: &str) -> String {
    rel.split('/').map(fold).collect::<Vec<_>>().join("/")
}

fn text(o: Opened) -> String {
    std::io::read_to_string(o.file).unwrap()
}

fn open(t: &Tree, rel: &str, engine: Engine) -> Result<String, Refusal> {
    let o = t.root.open_file_with(rel, NO_LIMIT, engine)?;
    assert!(
        o.real.starts_with(t.root.real()),
        "{rel}: {:?} is outside the root",
        o.real
    );
    let s = text(o);
    assert_ne!(s, DECOY, "{rel} read a file outside the root");
    Ok(s)
}

// ── T1: path escape ──────────────────────────────────────────────────

#[test]
fn dotdot_absolute_and_odd_components_are_refused_not_normalised() {
    let t = tree();
    let inside_abs = t.path.join("top.txt").to_string_lossy().into_owned();
    let outside_abs = t.outside.join("secret.txt").to_string_lossy().into_owned();
    let long = "a/".repeat(2048) + "b";
    let deep = vec!["d"; MAX_COMPONENTS + 1].join("/");
    let wide = "n".repeat(MAX_COMPONENT_BYTES + 1);
    let bad: Vec<&str> = vec![
        // `..` in every position
        "..",
        "../outside/secret.txt",
        "../root/top.txt",
        "docs/..",
        "docs/../top.txt",
        "docs/../../outside/secret.txt",
        "docs/sub/../../../outside/secret.txt",
        "docs/sub/..",
        "top.txt/..",
        "top.txt/../top.txt",
        "docs/sub/../a.md",
        // absolute
        "/",
        "/etc/passwd",
        "/top.txt",
        &inside_abs,
        &outside_abs,
        // empty and odd components: refused, never cleaned up
        "",
        ".",
        "./top.txt",
        "docs/./a.md",
        "docs//a.md",
        "docs/",
        "top.txt/",
        "docs/a.md/.",
        "docs\\a.md",
        "..\\outside\\secret.txt",
        "top.txt\0",
        "top\0.txt",
        "to\np.txt",
        "top.txt\r",
        "top\u{1b}[2J.txt",
        "top\u{85}.txt",
        "top.txt.",
        "top.txt ",
        "docs /a.md",
        "docs./a.md",
        "a:b",
        "C:",
        "C:/top.txt",
        // over-long input
        &long,
        &deep,
        &wide,
    ];
    for engine in ENGINES {
        for p in &bad {
            assert_eq!(open(&t, p, engine), Err(Refusal::Invalid), "{p:?} ({engine:?})");
            assert_eq!(t.root.open_dir(p).map(|o| o.rel), Err(Refusal::Invalid), "{p:?}");
        }
        // A clean path is opened as asked.
        for (rel, want) in INSIDE {
            assert_eq!(open(&t, rel, engine).as_deref(), Ok(want), "{rel} ({engine:?})");
        }
        assert_eq!(open(&t, "nope.md", engine), Err(Refusal::NotFound));
        assert_eq!(open(&t, "docs/nope/a.md", engine), Err(Refusal::NotFound));
        assert_eq!(open(&t, "top.txt/a.md", engine), Err(Refusal::NotFound));
        assert_eq!(open(&t, "docs", engine), Err(Refusal::NotAFile));
        // The limits themselves are fine lexically.
        let at_limit = vec!["d"; MAX_COMPONENTS].join("/");
        assert_eq!(open(&t, &at_limit, engine), Err(Refusal::NotFound));
    }
    assert_eq!(validate("docs/sub/b.txt"), Ok(vec!["docs", "sub", "b.txt"]));
    assert_eq!(t.root.open_dir("docs/sub").unwrap().rel, "docs/sub");
    assert_eq!(t.root.open_dir("top.txt").unwrap_err(), Refusal::NotAFile);
    assert_eq!(Refusal::Invalid.code(), "invalid_path");
}

/// The absolute form some callers are handed (`files.share`): matched to a
/// root by names alone, `..` refused, then walked like any other request.
#[test]
fn an_absolute_path_is_matched_to_a_root_by_name_and_walked() {
    let t = tree();
    let roots = [t.root];
    let abs = |rel: &str| t.path.join(rel).to_string_lossy().into_owned();
    assert_eq!(locate(&roots, &abs("docs/a.md")), Ok((0, "docs/a.md".to_string())));
    let (_, o) = open_absolute(&roots, &abs("docs/sub/b.txt"), NO_LIMIT).unwrap();
    assert_eq!(text(o), "inside-b\n");
    for (p, why) in [
        (abs("docs/../../outside/secret.txt"), Refusal::Invalid),
        (abs("../outside/secret.txt"), Refusal::Invalid),
        (abs(""), Refusal::NotAFile),
        (
            t.outside.join("secret.txt").to_string_lossy().into_owned(),
            Refusal::Outside,
        ),
        // A sibling whose name starts with the root's is not under it.
        (format!("{}X/top.txt", t.path.display()), Refusal::Outside),
        ("/etc/passwd".to_string(), Refusal::Outside),
        ("docs/a.md".to_string(), Refusal::Invalid),
        (String::new(), Refusal::Invalid),
        (abs("top.txt\n"), Refusal::Invalid),
    ] {
        assert_eq!(open_absolute(&roots, &p, NO_LIMIT).map(|_| ()), Err(why), "{p:?}");
    }
    assert_eq!(
        open_absolute(&roots, &abs("linkdir/secret.txt"), NO_LIMIT).map(|_| ()),
        Err(Refusal::Symlink)
    );
    assert_eq!(
        open_absolute(&roots, &abs(".env"), NO_LIMIT).map(|_| ()),
        Err(Refusal::Secret("environment file"))
    );
    assert_eq!(
        open_absolute(&[], &abs("top.txt"), NO_LIMIT).map(|_| ()),
        Err(Refusal::Outside)
    );
}

/// Property: whatever the request, nothing outside the root is ever
/// opened. Component soups (`..`, links, decoy names, separators, random
/// text) against [`tree`]; every success is one of the three clean paths,
/// its handle's path is under the root, and it never holds decoy bytes.
#[test]
fn prop_no_request_ever_opens_outside_the_root() {
    let t = tree();
    let piece = prop_oneof![
        4 => prop::sample::select(vec![
            "..", ".", "", "docs", "sub", "a.md", "b.txt", "top.txt", "link.txt", "linkdir", "abs", "self", "inner",
            "out", "gone", "hard.txt", "outside", "secret.txt", "root", ".env", "DOCS", "Top.TXT", "etc", "passwd",
            "~", "*", "proc", "self", "fd", "cwd",
        ])
        .prop_map(str::to_string),
        1 => "[a-zA-Z0-9._ :~\\\\-]{0,8}",
        1 => "\\PC{0,4}",
        1 => any::<String>().prop_map(|s| s.chars().take(6).collect::<String>()),
    ];
    let request = (
        prop::collection::vec(piece, 0..9),
        prop::sample::select(vec!["/", "/", "/", "//", "\\", "/./", "/../"]),
        0..6u8,
    );
    let roots = [Root::open(&t.path).unwrap()];
    let check = |what: &str, r: Result<Opened, Refusal>, dir: bool| -> Result<(), TestCaseError> {
        let Ok(o) = r else {
            return Ok(());
        };
        prop_assert!(o.real.starts_with(t.root.real()), "{what:?} opened {:?}", o.real);
        if dir {
            return Ok(());
        }
        prop_assert!(o.meta.is_file() && o.meta.nlink() == 1, "{what:?}");
        let rel = o.rel.clone();
        let content = text(o);
        prop_assert!(!content.contains("DECOY"), "{what:?} read decoy bytes");
        prop_assert!(
            INSIDE.iter().any(|(p, c)| *p == folded(&rel) && *c == content),
            "{what:?} opened {rel:?}"
        );
        Ok(())
    };
    let mut runner = TestRunner::new(Config::with_cases(3000));
    runner
        .run(&request, |(pieces, sep, lead)| {
            let body = pieces.join(sep);
            let path = match lead {
                0 => format!("/{body}"),
                1 => format!("{}/{body}", t.path.display()),
                2 => format!("{}/{body}", t.outside.display()),
                3 => format!("../{body}"),
                _ => body,
            };
            for engine in ENGINES {
                let r = t.root.open_file_with(&path, NO_LIMIT, engine);
                if let Ok(o) = &r {
                    // Relative requests are never cleaned up: a success was asked for exactly (up to
                    // the spelling a case-insensitive volume stores).
                    prop_assert_eq!(folded(&o.rel), folded(&path));
                }
                check(&path, r, false)?;
            }
            check(&path, t.root.open_dir(&path), true)?;
            check(&path, open_absolute(&roots, &path, NO_LIMIT).map(|(_, o)| o), false)?;
            Ok(())
        })
        .unwrap();
    // The clean paths do open (so a soup that happens to be one is a real success).
    for (rel, _) in INSIDE {
        assert!(t.root.open_file(rel, NO_LIMIT).is_ok());
    }
}

// ── T2: links ────────────────────────────────────────────────────────

/// The listing half of this threat ("listed as a link") arrives with
/// `fs.list`; here: a link is refused wherever it stands, whether it
/// points outside or stays inside.
#[test]
fn a_link_at_any_depth_is_refused_and_listed_as_a_link() {
    let t = tree();
    for engine in ENGINES {
        for p in [
            // the last component
            "link.txt",
            "gone",
            "docs/inner",
            // a middle component, pointing out
            "linkdir/secret.txt",
            "linkdir/sub/b.txt",
            "abs/etc/passwd",
            "docs/sub/out/secret.txt",
            "docs/sub/out/docs/a.md",
            // a middle component that stays inside the root: still a link
            "self/top.txt",
            "self/docs/a.md",
            "docs/inner/b.txt",
            "self/self/self/top.txt",
            // the folder just above the file
            "docs/sub/out/top.txt",
        ] {
            assert_eq!(open(&t, p, engine), Err(Refusal::Symlink), "{p} ({engine:?})");
        }
        // Past a dangling link there is nothing to say but "link".
        assert_eq!(open(&t, "gone/x.txt", engine), Err(Refusal::Symlink));
    }
    for p in ["linkdir", "self", "docs/inner", "docs/sub/out", "abs"] {
        assert_eq!(t.root.open_dir(p).map(|o| o.rel), Err(Refusal::Symlink), "{p}");
    }
    assert_eq!(Refusal::Symlink.code(), "symlink_refused");
}

// ── T3: the swap race ────────────────────────────────────────────────

/// A thread flips `docs` between the real folder and a link to a folder
/// outside (which holds files of the same names) while reads run in a
/// loop: at least 10,000 of them and at least two seconds. A read either
/// returns the inside content or is refused; none returns decoy bytes.
fn swap_race(engine: Engine) {
    let t = tree();
    let stop = Arc::new(AtomicBool::new(false));
    let flips = Arc::new(AtomicU64::new(0));
    let (docs, parked, decoy) = (t.path.join("docs"), t.path.join("docs.parked"), t.outside.clone());
    let flipper = {
        let (stop, flips) = (stop.clone(), flips.clone());
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                std::fs::rename(&docs, &parked).unwrap();
                symlink(&decoy, &docs).unwrap();
                flips.fetch_add(1, Ordering::Relaxed);
                std::thread::yield_now();
                std::fs::remove_file(&docs).unwrap();
                std::fs::rename(&parked, &docs).unwrap();
                std::thread::yield_now();
            }
        })
    };
    let started = Instant::now();
    let (mut reads, mut inside, mut refused) = (0u64, 0u64, 0u64);
    while reads < 10_000 || started.elapsed() < Duration::from_secs(2) {
        // `docs` is the folder above the file, then a folder further up.
        let (rel, want) = if reads % 2 == 0 {
            ("docs/a.md", "inside-a\n")
        } else {
            ("docs/sub/b.txt", "inside-b\n")
        };
        match t.root.open_file_with(rel, NO_LIMIT, engine) {
            Ok(o) => {
                assert!(o.real.starts_with(t.root.real()), "{:?} is outside the root", o.real);
                assert_eq!(text(o), want, "read {reads} of {rel} returned other content");
                inside += 1;
            }
            Err(r) => {
                assert!(
                    matches!(r, Refusal::Symlink | Refusal::NotFound | Refusal::Outside),
                    "read {reads} of {rel}: {r:?}"
                );
                refused += 1;
            }
        }
        reads += 1;
    }
    stop.store(true, Ordering::Relaxed);
    flipper.join().unwrap();
    assert!(flips.load(Ordering::Relaxed) > 0, "the folder was never swapped");
    assert!(inside > 0, "no read ever succeeded: the race proved nothing");
    eprintln!(
        "swap race ({engine:?}): {reads} reads, {inside} inside, {refused} refused, {} swaps",
        flips.load(Ordering::Relaxed)
    );
}

#[test]
fn swap_race_never_reads_outside() {
    swap_race(Engine::Auto);
}

#[test]
fn swap_race_never_reads_outside_fallback_walk() {
    swap_race(Engine::Walk);
}

/// `Engine::Auto` really is `openat2` where the kernel has it (so the two
/// race tests above are two different code paths).
#[test]
fn the_default_resolution_is_openat2_where_the_kernel_has_it() {
    if !openat2_supported() {
        eprintln!("openat2 is not available here: every request takes the component walk");
        return;
    }
    #[cfg(target_os = "linux")]
    {
        let t = tree();
        let before = openat2::HITS.load(Ordering::Relaxed);
        assert_eq!(open(&t, "docs/sub/b.txt", Engine::Auto).as_deref(), Ok("inside-b\n"));
        assert!(openat2::HITS.load(Ordering::Relaxed) > before);
        assert!(!openat2::missing());
    }
}

// ── T4, T5: the deny list, by case and by Unicode ────────────────────

#[test]
fn secret_rules_match_the_shared_fixture() {
    let f: serde_json::Value = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../packages/protocol/fixtures/secret-files.json"
    )))
    .unwrap();
    for c in f["hidden"].as_array().unwrap() {
        let p = c.as_str().unwrap();
        assert!(secret_reason(p).is_some(), "{p:?} should be hidden");
    }
    for c in f["shown"].as_array().unwrap() {
        let p = c.as_str().unwrap();
        assert_eq!(secret_reason(p), None, "{p:?} should be shown");
    }
    for row in f["folded"].as_array().unwrap() {
        let (name, folded) = (row[0].as_str().unwrap(), row[1].as_str().unwrap());
        assert_eq!(fold(name), folded, "{name:?}");
    }
    assert!(f["hidden"].as_array().unwrap().len() > 60 && f["folded"].as_array().unwrap().len() > 10);
}

#[test]
fn secret_names_fold_case() {
    for p in [
        ".ENV",
        ".Env.Local",
        "deploy/PROD.ENV",
        ".Git/config",
        ".GIT/HEAD",
        "sub/.gIt/index",
        ".SSH/id",
        ".Aws/credentials",
        "Server.PEM",
        "TLS.Key",
        "ID_RSA",
        "Id_Ed25519.Pub",
        ".NPMRC",
        "Credentials",
        "SECRETS.YML",
    ] {
        assert!(secret_reason(p).is_some(), "{p}");
    }
    assert_eq!(secret_reason(".GIT/HEAD"), Some("git internals"));
    assert_eq!(secret_reason("a/.SSH/x"), Some("credentials"));
    assert_eq!(secret_reason("README.MD"), None);
    // On disk, whatever the volume's own rule: both spellings are refused before anything is opened.
    let t = tree();
    std::fs::write(t.path.join("Notes.PEM"), "key\n").unwrap();
    std::fs::create_dir(t.path.join(".Git")).unwrap();
    std::fs::write(t.path.join(".Git/config"), "[core]\n").unwrap();
    for engine in ENGINES {
        for p in [
            ".env",
            ".ENV",
            ".Env",
            "Notes.PEM",
            "notes.pem",
            ".Git/config",
            ".git/config",
        ] {
            assert!(
                matches!(open(&t, p, engine), Err(Refusal::Secret(_))),
                "{p} ({engine:?})"
            );
        }
    }
    assert_eq!(Refusal::Secret("key file").code(), "file_refused");
}

#[test]
fn secret_names_fold_unicode_and_trailing_dots() {
    for (p, why) in [
        // zero-width and other default-ignorable code points
        (".env\u{200b}", "environment file"),
        (".e\u{200d}nv", "environment file"),
        ("\u{feff}.env", "environment file"),
        (".g\u{ad}it/config", "git internals"),
        (".git\u{2060}/HEAD", "git internals"),
        (".s\u{200c}sh/config", "credentials"),
        ("server.p\u{fe0f}em", "key file"),
        ("id\u{202e}_rsa", "SSH key"),
        // compatibility forms
        ("\u{ff0e}env", "environment file"),
        (".\u{ff45}\u{ff4e}\u{ff56}", "environment file"),
        ("\u{ff0e}\u{ff47}\u{ff49}\u{ff54}/config", "git internals"),
        ("\u{17f}ecrets.yaml", "credentials"),
        (".\u{212a}ube/config", "credentials"),
        ("tls.\u{1d4f}ey", "key file"),
        // trailing dots and spaces
        (".env.", "environment file"),
        (".env ", "environment file"),
        (".env. . .", "environment file"),
        ("tls.key.", "key file"),
        ("id_rsa ", "SSH key"),
        (".git./config", "git internals"),
        (".ssh /known_hosts", "credentials"),
        (".git.", "git internals"),
        // all of it at once
        ("\u{ff0e}G\u{200b}I\u{ff34}. /config", "git internals"),
    ] {
        assert_eq!(secret_reason(p), Some(why), "{p:?}");
    }
    assert_eq!(fold("\u{ff0e}G\u{200b}I\u{ff34}. "), ".git");
    assert_eq!(fold("Stra\u{df}e"), "strasse");
    assert_eq!(fold("..."), "");
    // Folding does not invent secrets.
    for p in [
        "caf\u{e9}.md",
        "\u{65e5}\u{672c}\u{8a9e}.md",
        "env\u{200b}.ts",
        "notes.md.",
    ] {
        assert_eq!(secret_reason(p), None, "{p:?}");
    }
    // Stored under such a name, the file is refused when asked for by that name (and a name with a
    // trailing dot or space is not a request the jail accepts at all).
    let t = tree();
    for name in [".env\u{200b}", "\u{ff0e}env", "tls.\u{1d4f}ey", "id\u{200d}_rsa"] {
        std::fs::write(t.path.join(name), "secret\n").unwrap();
        for engine in ENGINES {
            assert!(
                matches!(open(&t, name, engine), Err(Refusal::Secret(_))),
                "{name:?} ({engine:?})"
            );
        }
    }
    std::fs::write(t.path.join("tls.key."), "secret\n").unwrap();
    assert_eq!(open(&t, "tls.key.", Engine::Auto), Err(Refusal::Invalid));
}

/// Step 6 of the module text: on a volume that ignores case (the default
/// on macOS), a file is found under a spelling other than its stored one.
/// The stored name is the one reported and checked.
#[cfg(target_os = "macos")]
#[test]
fn stored_name_is_checked_on_a_case_insensitive_volume() {
    let t = tree();
    std::fs::create_dir(t.path.join("Notes")).unwrap();
    std::fs::write(t.path.join("Notes/ReadMe.md"), "stored\n").unwrap();
    if !t.path.join("notes/readme.md").exists() {
        eprintln!("this volume is case-sensitive: nothing to check");
        return;
    }
    std::fs::write(t.path.join("Notes/Server.Pem"), "key\n").unwrap();
    for engine in ENGINES {
        let o = t.root.open_file_with("notes/readme.md", NO_LIMIT, engine).unwrap();
        assert!(o.real.starts_with(t.root.real()));
        assert_eq!(
            o.rel.split('/').map(fold).collect::<Vec<_>>(),
            ["notes", "readme.md"],
            "{:?}",
            o.rel
        );
        assert_eq!(text(o), "stored\n");
        for p in [
            ".ENV",
            ".Env",
            "notes/server.pem",
            "NOTES/SERVER.PEM",
            "Notes/Server.Pem",
        ] {
            assert!(
                matches!(open(&t, p, engine), Err(Refusal::Secret(_))),
                "{p} ({engine:?})"
            );
        }
    }
}

// ── T6: hard links ───────────────────────────────────────────────────

#[test]
fn a_hard_linked_file_is_refused() {
    let t = tree();
    // A second name, inside the root, of a file inside the root; and of a secret.
    std::fs::hard_link(t.path.join("docs/a.md"), t.path.join("alias.md")).unwrap();
    std::fs::hard_link(t.path.join(".env"), t.path.join("notes.txt")).unwrap();
    for engine in ENGINES {
        // `hard.txt` is a second name of a file outside the root.
        for p in ["hard.txt", "alias.md", "docs/a.md", "notes.txt"] {
            assert_eq!(open(&t, p, engine), Err(Refusal::Linked), "{p} ({engine:?})");
        }
        assert_eq!(open(&t, "docs/sub/b.txt", engine).as_deref(), Ok("inside-b\n"));
    }
    // With its other name gone the file has one name again.
    std::fs::remove_file(t.path.join("alias.md")).unwrap();
    assert_eq!(open(&t, "docs/a.md", Engine::Auto).as_deref(), Ok("inside-a\n"));
    assert_eq!(Refusal::Linked.code(), "linked_file");
}

// ── T8: things that are not files, and huge files ────────────────────

fn mkfifo(path: &Path) {
    let c = CString::new(path.as_os_str().as_bytes()).unwrap();
    // SAFETY: `c` is a NUL-terminated path that outlives the call.
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
}

/// A FIFO with no writer blocks an ordinary `open` for ever; a device can
/// do anything. Neither is opened as a file, and nothing waits.
#[test]
fn a_fifo_or_device_is_refused_without_blocking() {
    let t = tree();
    mkfifo(&t.path.join("pipe"));
    mkfifo(&t.path.join("docs/pipe.md"));
    let _socket = std::os::unix::net::UnixListener::bind(t.path.join("sock")).unwrap();
    let started = Instant::now();
    for engine in ENGINES {
        for p in ["pipe", "docs/pipe.md", "sock"] {
            assert_eq!(open(&t, p, engine), Err(Refusal::NotAFile), "{p} ({engine:?})");
        }
    }
    // Device nodes need root to make; the system's own are there to ask for.
    let dev = Root::open(Path::new("/dev")).unwrap();
    for engine in ENGINES {
        for p in ["null", "zero", "tty"] {
            let r = dev.open_file_with(p, NO_LIMIT, engine).map(|o| o.rel);
            assert!(
                matches!(r, Err(Refusal::NotAFile | Refusal::NotFound)),
                "/dev/{p}: {r:?}"
            );
        }
    }
    assert_eq!(dev.open_file("null", NO_LIMIT).map(|o| o.rel), Err(Refusal::NotAFile));
    // A name that is a file one moment and a FIFO the next: the handle is what is checked, and an
    // open that lands on the FIFO returns at once (O_NONBLOCK) instead of waiting for a writer.
    let stop = Arc::new(AtomicBool::new(false));
    let (name, spare) = (t.path.join("flip"), t.path.join("flip.next"));
    std::fs::write(&name, "regular\n").unwrap();
    let flipper = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            let mut fifo = true;
            while !stop.load(Ordering::Relaxed) {
                if fifo {
                    mkfifo(&spare);
                } else {
                    std::fs::write(&spare, "regular\n").unwrap();
                }
                std::fs::rename(&spare, &name).unwrap();
                fifo = !fifo;
            }
        })
    };
    let until = Instant::now() + Duration::from_millis(400);
    let mut n = 0u32;
    while Instant::now() < until || n < 2000 {
        match open(&t, "flip", ENGINES[(n % 2) as usize]) {
            Ok(s) => assert!(s == "regular\n" || s.is_empty(), "{s:?}"),
            Err(r) => assert!(matches!(r, Refusal::NotAFile | Refusal::NotFound), "{r:?}"),
        }
        n += 1;
    }
    stop.store(true, Ordering::Relaxed);
    flipper.join().unwrap();
    assert!(started.elapsed() < Duration::from_secs(60), "an open waited");
    assert_eq!(Refusal::NotAFile.code(), "not_a_file");
}

#[test]
fn a_file_over_the_limit_is_too_large() {
    let t = tree();
    // Sparse: 8 GiB of size, no blocks.
    let huge = 8u64 << 30;
    std::fs::File::create(t.path.join("huge.bin"))
        .unwrap()
        .set_len(huge)
        .unwrap();
    std::fs::write(t.path.join("small.txt"), vec![b'x'; 1024]).unwrap();
    let limit = |max| Limits { max_size: Some(max) };
    for engine in ENGINES {
        assert_eq!(
            t.root.open_file_with("huge.bin", limit(2 << 20), engine).map(|o| o.rel),
            Err(Refusal::TooLarge {
                size: huge,
                max: 2 << 20
            })
        );
        // The limit is the caller's: with none, the handle is returned and nothing has been read.
        let o = t.root.open_file_with("huge.bin", NO_LIMIT, engine).unwrap();
        assert_eq!(o.meta.len(), huge);
        // At the limit is allowed; one byte over is not.
        assert!(t.root.open_file_with("small.txt", limit(1024), engine).is_ok());
        assert_eq!(
            t.root.open_file_with("small.txt", limit(1023), engine).map(|o| o.rel),
            Err(Refusal::TooLarge { size: 1024, max: 1023 })
        );
    }
    assert_eq!(Refusal::TooLarge { size: 2, max: 1 }.code(), "file_too_large");
}

// ── roots ────────────────────────────────────────────────────────────

#[test]
fn a_root_that_is_a_link_is_refused_unless_the_runner_named_it() {
    let t = tree();
    let alias = t.path.with_file_name("alias");
    symlink(&t.path, &alias).unwrap();
    // A root is opened without following its last component.
    assert_eq!(Root::open(&alias).map(|_| ()), Err(Refusal::Symlink));
    assert_eq!(Root::open(&t.path.join("linkdir")).map(|_| ()), Err(Refusal::Symlink));
    assert_eq!(Root::open(&t.path.join("nope")).map(|_| ()), Err(Refusal::RootGone));
    assert_eq!(Root::open(&t.path.join("top.txt")).map(|_| ()), Err(Refusal::RootGone));
    assert_eq!(Root::open(Path::new("relative")).map(|_| ()), Err(Refusal::Invalid));
    assert_eq!(
        Root::open_trusted(&t.path.join("nope")).map(|_| ()),
        Err(Refusal::RootGone)
    );
    // A path from the runner's own records may be a link (a person's `~/code -> /mnt/code`): it is
    // made real once, and everything under it is walked as under any root.
    let trusted = Root::open_trusted(&alias).unwrap();
    assert_eq!(trusted.real(), t.root.real());
    for engine in ENGINES {
        let o = trusted.open_file_with("docs/a.md", NO_LIMIT, engine).unwrap();
        assert!(o.real.starts_with(t.root.real()));
        assert_eq!(text(o), "inside-a\n");
        assert_eq!(
            trusted
                .open_file_with("linkdir/secret.txt", NO_LIMIT, engine)
                .map(|o| o.rel),
            Err(Refusal::Symlink)
        );
    }
    // An absolute path matches by the name the root was opened by, or by its real path.
    let roots = [trusted];
    for base in [alias.clone(), t.root.real().to_path_buf()] {
        let p = base.join("docs/sub/b.txt").to_string_lossy().into_owned();
        assert_eq!(locate(&roots, &p), Ok((0, "docs/sub/b.txt".to_string())), "{p}");
    }
    // A root replaced after it was opened: the handle is still the folder that was opened, the
    // jail sees that its path no longer leads there, and the link put in its place is never followed.
    assert!(t.root.is_current());
    let moved = t.path.with_file_name("moved");
    std::fs::rename(&t.path, &moved).unwrap();
    symlink(&t.outside, &t.path).unwrap();
    assert!(!t.root.is_current());
    for engine in ENGINES {
        for (rel, want) in INSIDE {
            match t.root.open_file_with(rel, NO_LIMIT, engine) {
                // Where the system reports the handle's path (Linux, macOS), it is no longer under
                // the root's recorded path.
                Err(r) => assert_eq!(r, Refusal::Outside, "{rel}"),
                Ok(o) => assert_eq!(text(o), want, "{rel}"),
            }
        }
        assert_ne!(
            t.root
                .open_file_with("secret.txt", NO_LIMIT, engine)
                .map(text)
                .unwrap_or_default(),
            DECOY
        );
    }
    assert_eq!(Refusal::RootGone.code(), "root_gone");
    assert_eq!(Refusal::Outside.code(), "outside_root");
}

// ── T23: Windows names (the lexical half, which runs everywhere) ─────

/// Streams and device names are refused as requests on every system, so
/// a path means the same everywhere. Short names (`GIT~1`) cannot be
/// told from ordinary names by spelling: they are caught on Windows by
/// the stored-name check, which arrives with the Windows resolver.
#[test]
fn windows_stream_device_and_short_names_are_refused() {
    for p in [
        "notes.md:secret",
        "notes.md::$DATA",
        "docs:$I30:$INDEX_ALLOCATION/a.md",
        "C:notes.md",
        "CON",
        "con",
        "Con.txt",
        "PRN",
        "aux.c",
        "NUL",
        "nul.tar.gz",
        "docs/NUL",
        "COM1",
        "com9.log",
        "LPT1",
        "lpt5.txt",
        "COM\u{b9}",
        "CONIN$",
        "conout$",
        "con .txt",
    ] {
        assert_eq!(validate(p), Err(Refusal::Invalid), "{p:?}");
    }
    // Names that only look like them are ordinary names.
    for p in [
        "console.md",
        "com.txt",
        "com10",
        "com0",
        "COM",
        "auxiliary.c",
        "nullable.rs",
        "lpt",
        "docs/icon.png",
        "GIT~1",
    ] {
        assert!(validate(p).is_ok(), "{p:?}");
    }
}

/// The control for [`swap_race_never_reads_outside`], run by hand (it is a
/// race, so it is not part of the suite): the check-then-open the jail
/// replaced (make the parent real, check the prefix, open by path) does
/// read the decoy under the very same swapping.
/// `cargo test -p tmuxedo-runner --lib check_then_open -- --ignored --nocapture`.
#[test]
#[ignore = "a demonstration of the gap the jail closes; a race, so not asserted in CI"]
fn check_then_open_by_path_reads_outside_under_the_same_race() {
    let t = tree();
    let stop = Arc::new(AtomicBool::new(false));
    let (docs, parked, decoy) = (t.path.join("docs"), t.path.join("docs.parked"), t.outside.clone());
    let flipper = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                std::fs::rename(&docs, &parked).unwrap();
                symlink(&decoy, &docs).unwrap();
                std::fs::remove_file(&docs).unwrap();
                std::fs::rename(&parked, &docs).unwrap();
            }
        })
    };
    let root = t.path.canonicalize().unwrap();
    let started = Instant::now();
    let mut escaped = 0u64;
    while started.elapsed() < Duration::from_secs(3) {
        let Ok(parent) = root.join("docs").canonicalize() else {
            continue;
        };
        if !parent.starts_with(&root) {
            continue;
        }
        if std::fs::read_to_string(parent.join("a.md")).is_ok_and(|s| s == DECOY) {
            escaped += 1;
        }
    }
    stop.store(true, Ordering::Relaxed);
    flipper.join().unwrap();
    eprintln!("check-then-open read the decoy {escaped} times in 3 s");
    assert!(escaped > 0);
}

/// T18: what may never be a root, automatic or added.
#[test]
fn too_broad_roots_are_refused() {
    let guard = Guard {
        home: Some(PathBuf::from("/srv/people/dev")),
        own: vec![
            PathBuf::from("/srv/people/dev/.local/state/app"),
            PathBuf::from("/data/app-files"),
        ],
    };
    let refused = [
        "/",
        "/etc",
        "/etc/nginx",
        "/var",
        "/usr",
        "/bin",
        "/sbin",
        "/lib",
        "/lib64",
        "/boot",
        "/dev",
        "/dev/shm",
        "/proc",
        "/proc/1/root",
        "/sys",
        "/run",
        "/run/secrets",
        "/var/run/secrets/tokens",
        "/private",
        "/System",
        "/Library",
        "/tmp",
        "/var/tmp",
        "/Users/Shared",
        "/home",
        "/Users",
        "/home/dev",
        "/Users/dev",
        "/root",
        // The runner's user's home wherever it is, and everything above it.
        "/srv/people/dev",
        "/srv/people",
        "/srv",
        // The runner's own folders: above one, at one, inside one.
        "/srv/people/dev/.local",
        "/srv/people/dev/.local/state/app",
        "/srv/people/dev/.local/state/app/chats",
        "/data",
        "/data/app-files/inbox",
        // Not absolute: never a root.
        "relative/folder",
        "",
    ];
    for p in refused {
        assert!(too_broad(Path::new(p), &guard), "{p} must be refused");
    }
    let ordinary = [
        "/home/dev/code/site",
        "/Users/dev/Projects/app",
        "/srv/people/dev/code",
        "/srv/www/site",
        "/var/www/site",
        "/tmp/build-1234",
        "/opt/app",
        "/usr/local/src/thing",
        "/mnt/disk/code",
        "/data/other",
    ];
    for p in ordinary {
        assert!(!too_broad(Path::new(p), &guard), "{p} is an ordinary folder");
    }
}

// ── §3.5: the names a browser reaches that one chat's work folder did not ──

#[test]
fn the_added_secret_names_are_refused_and_their_near_misses_are_not() {
    // (path, reason). Every row is also tried in upper case, beneath a folder,
    // with a trailing dot and with a zero-width character inside.
    let hidden: &[(&str, &str)] = &[
        ("terraform.tfstate", "infrastructure state"),
        ("terraform.tfstate.backup", "infrastructure state"),
        ("prod.tfstate.d", "infrastructure state"),
        (".tfstate", "infrastructure state"),
        ("terraform.tfvars", "infrastructure state"),
        ("prod.auto.tfvars", "infrastructure state"),
        (".terraform", "infrastructure state"),
        (".terraform/terraform.tfstate", "infrastructure state"),
        (".terraform/providers/a/b/c", "infrastructure state"),
        (".vault-token", "credentials"),
        ("kubeconfig", "credentials"),
        ("passwords.kdbx", "password database"),
        ("office.ovpn", "VPN profile"),
        (".config/gcloud", "credentials"),
        (".config/gcloud/credentials.db", "credentials"),
        (".config/gh", "credentials"),
        (".config/gh/hosts.yml", "credentials"),
        (".gcloud", "credentials"),
        (".gcloud/notes.md", "credentials"),
        (".bash_history", "command history"),
        (".zsh_history", "command history"),
        (".psql_history", "command history"),
        (".mysql_history", "command history"),
        (".node_repl_history", "command history"),
        ("service-account.json", "credentials"),
        ("service-account-prod.json", "credentials"),
        (".envrc", "environment file"),
        // A secret folder refuses itself, not only what is beneath it.
        (".ssh", "credentials"),
        (".gnupg", "credentials"),
        (".aws", "credentials"),
        (".kube", "credentials"),
        (".docker", "credentials"),
        (".azure", "credentials"),
        (".ssh/deep/er/notes.md", "credentials"),
    ];
    for (p, why) in hidden {
        for spelt in [
            p.to_string(),
            p.to_uppercase(),
            format!("a/b/{p}"),
            format!("{p}."),
            p.replacen('.', ".\u{200b}", 1),
        ] {
            assert_eq!(secret_reason(&spelt), Some(*why), "{spelt:?}");
        }
    }
    let shown = [
        "tfstate.md",
        "tfstate-notes.md",
        "terraform.tf",
        "main.tfvars.md",
        "tfvars",
        "terraform/main.tf",
        "vault-token",
        ".vault-token.md",
        "kubeconfig.md",
        "my-kubeconfig",
        "kube/config",
        "notes.kdbx.txt",
        "client.ovpn.txt",
        "config/gcloud/readme.md",
        ".config/app/settings.json",
        ".config/ghost/config.yml",
        ".config/gcloud.md",
        "gcloud/deploy.sh",
        "gh/notes.md",
        "history.txt",
        "bash_history",
        ".history",
        ".bash_history.md",
        "shell_history.md",
        "service-account.md",
        "service_account.json",
        "my-service-account.json",
        ".envrc.md",
        "envrc",
        "ssh/config",
        "aws/readme.md",
        "docker/Dockerfile",
    ];
    for p in shown {
        for spelt in [p.to_string(), p.to_uppercase(), format!("a/b/{p}")] {
            assert_eq!(secret_reason(&spelt), None, "{spelt:?}");
        }
    }
}

#[test]
fn the_added_secret_names_are_never_opened() {
    let t = tree();
    for (p, body) in [
        ("terraform.tfstate.backup", "{}"),
        (".vault-token", "t"),
        ("kubeconfig", "apiVersion: v1\n"),
        (".bash_history", "ls\n"),
        (".terraform/modules/modules.json", "{}"),
        (".config/gh/hosts.yml", "h\n"),
        ("deep/.kube/cache/x", "x"),
        ("kubeconfig.md", "notes\n"),
        ("history.txt", "notes\n"),
    ] {
        let at = t.path.join(p);
        std::fs::create_dir_all(at.parent().unwrap()).unwrap();
        std::fs::write(at, body).unwrap();
    }
    for engine in ENGINES {
        for p in [
            "terraform.tfstate.backup",
            "Terraform.TFSTATE.Backup",
            ".vault-token",
            "kubeconfig",
            "KubeConfig",
            ".bash_history",
            ".terraform/modules/modules.json",
            ".config/gh/hosts.yml",
            "deep/.kube/cache/x",
            // Nothing is there: the answer is the same, so it confirms nothing.
            "absent/.zsh_history",
            "absent.tfvars",
        ] {
            assert!(
                matches!(open(&t, p, engine), Err(Refusal::Secret(_))),
                "{p} ({engine:?})"
            );
        }
        assert_eq!(open(&t, "kubeconfig.md", engine).as_deref(), Ok("notes\n"));
        assert_eq!(open(&t, "history.txt", engine).as_deref(), Ok("notes\n"));
    }
    for d in [
        ".terraform",
        ".terraform/modules",
        ".config/gh",
        "deep/.kube",
        "deep/.kube/cache",
    ] {
        assert!(matches!(t.root.open_dir(d), Err(Refusal::Secret(_))), "{d}");
    }
    assert!(t.root.open_dir(".config").is_ok() && t.root.open_dir("deep").is_ok());
}
