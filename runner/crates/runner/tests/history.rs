//! The search history store (crate::history; docs/protocol.md "Search
//! history"): masked before the write, retention, the size cap, nothing kept
//! for a project that has not opted in, and the `search` request's filters,
//! bounds and time box.

mod common;

use serde_json::{json, Value};
use std::path::Path;
use std::time::{Duration, Instant};
use tmuxedo_runner::chat::mask::Masker;
use tmuxedo_runner::history::{
    self, file_name, parse_query, search, snippet, validate, Entry, Policies, Policy, Recorder, Store,
};

const DAY: i64 = 24 * 60 * 60 * 1000;

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

fn entry(at_ms: i64, session: &str, agent: &str, text: &str) -> Entry {
    Entry {
        at_ms,
        kind: "terminal",
        session: session.into(),
        title: format!("{session} title"),
        agent: agent.into(),
        turn: None,
        line: Some(1),
        text: text.into(),
    }
}

/// Every byte SQLite keeps for a project: the file and its side files.
fn bytes_on_disk(dir: &Path, key: &str) -> Vec<u8> {
    let mut all = vec![];
    for suffix in ["", "-wal", "-shm", "-journal"] {
        if let Ok(b) = std::fs::read(dir.join(format!("{}{suffix}", file_name(key)))) {
            all.extend(b);
        }
    }
    all
}

fn contains(hay: &[u8], needle: &str) -> bool {
    hay.windows(needle.len()).any(|w| w == needle.as_bytes())
}

fn policies(keys: &[&str]) -> Policies {
    keys.iter().map(|k| (k.to_string(), Policy::new(k, 30))).collect()
}

#[test]
fn secrets_are_masked_before_the_write() {
    let dir = common::tempdir();
    let key = "github.com/org/shop";
    // Built at run time, so the source holds nothing a secret scan reads as one.
    let literal = ["zq7", "Lm2", "Vx9", "Pw4", "Tn6"].concat();
    let shaped = format!("{}{}", ["gh", "p_"].concat(), "a1B2c3D4e5F6g7H8i9J0k1L2");
    let mut masker = Masker::default();
    masker.add(&literal);

    let mut s = Store::open(dir.path(), &Policy::new(key, 30)).unwrap();
    let text = format!("export DEPLOY_KEY={literal} and a token {shaped} then deploy");
    // A colour code inside the value must not hide it from the masker.
    let split = format!("{}\x1b[31m{}", &literal[..6], &literal[6..]);
    let n = s
        .write(
            &[
                entry(now(), "w1:p1", "shell", &text),
                entry(now(), "w1:p1", "shell", &format!("again {split}")),
            ],
            &masker,
        )
        .unwrap();
    assert_eq!(n, 2);

    let disk = bytes_on_disk(dir.path(), key);
    assert!(!disk.is_empty());
    assert!(!contains(&disk, &literal), "the literal secret reached the file");
    assert!(!contains(&disk, &shaped), "the token-shaped secret reached the file");
    assert!(!contains(&disk, &literal[..10]), "part of the secret reached the file");

    // What was kept is the masked text, and it is still found.
    let q = parse_query(&json!({"q": "deploy"})).unwrap();
    let r = search(
        dir.path(),
        &policies(&[key]),
        &q,
        Instant::now() + Duration::from_secs(5),
    );
    let hits = r["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 1, "{r}");
    let snip = hits[0]["snippet"].as_str().unwrap();
    assert!(snip.contains("***") && !snip.contains(&literal), "{snip}");
    // The secret itself finds nothing.
    let q = parse_query(&json!({"q": literal})).unwrap();
    let r = search(
        dir.path(),
        &policies(&[key]),
        &q,
        Instant::now() + Duration::from_secs(5),
    );
    assert_eq!(r["hits"].as_array().unwrap().len(), 0);
    // The file is the owner's only.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.path().join(file_name(key)))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "{mode:o}");
    }
}

#[test]
fn retention_removes_what_is_older_than_the_projects_days() {
    let dir = common::tempdir();
    let key = "path:m1:/work/site";
    let mut s = Store::open(dir.path(), &Policy::new(key, 7)).unwrap();
    let t = now();
    let m = Masker::default();
    s.write(
        &[
            entry(t - 30 * DAY, "w1:p1", "shell", "ancient build log"),
            entry(t - 8 * DAY, "w1:p1", "shell", "last week build log"),
            entry(t - 6 * DAY, "w1:p1", "shell", "recent build log"),
            entry(t, "w1:p1", "shell", "fresh build log"),
        ],
        &m,
    )
    .unwrap();
    assert_eq!(s.prune(t).unwrap(), 2);
    assert_eq!(s.count().unwrap(), 2);
    let q = parse_query(&json!({"q": "build log"})).unwrap();
    let r = search(
        dir.path(),
        &policies(&[key]),
        &q,
        Instant::now() + Duration::from_secs(5),
    );
    let texts: Vec<&str> = r["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["snippet"].as_str().unwrap())
        .collect();
    assert_eq!(texts, ["fresh build log", "recent build log"]);
    // Removed from the index too: a word only the old rows held is gone.
    let q = parse_query(&json!({"q": "ancient"})).unwrap();
    let r = search(
        dir.path(),
        &policies(&[key]),
        &q,
        Instant::now() + Duration::from_secs(5),
    );
    assert_eq!(r["hits"].as_array().unwrap().len(), 0);
    // Days are bounded.
    assert_eq!(Policy::new(key, 0).days, 1);
    assert_eq!(Policy::new(key, 10_000).days, history::MAX_DAYS);
}

#[test]
fn the_size_cap_drops_the_oldest_rows_first() {
    let dir = common::tempdir();
    let key = "github.com/org/big";
    let mut policy = Policy::new(key, 30);
    policy.cap_bytes = 256 * 1024;
    let mut s = Store::open(dir.path(), &policy).unwrap();
    let m = Masker::default();
    let t = now() - 60_000;
    let rows: Vec<Entry> = (0..3000)
        .map(|i| {
            entry(
                t + i,
                "w1:p1",
                "shell",
                &format!("row {i:05} {}", "lorem ipsum dolor ".repeat(8)),
            )
        })
        .collect();
    for chunk in rows.chunks(500) {
        s.write(chunk, &m).unwrap();
    }
    let before = s.count().unwrap();
    let removed = s.prune(now()).unwrap();
    assert!(removed > 0, "nothing removed at {} bytes", s.size_bytes().unwrap());
    assert!(
        s.size_bytes().unwrap() <= policy.cap_bytes,
        "{} bytes, {} rows of {before}",
        s.size_bytes().unwrap(),
        s.count().unwrap()
    );
    assert_eq!(s.count().unwrap(), before - removed as i64);
    // The newest row is kept, the oldest is gone.
    let find = |q: &str| {
        let q = parse_query(&json!({ "q": q })).unwrap();
        search(
            dir.path(),
            &policies(&[key]),
            &q,
            Instant::now() + Duration::from_secs(5),
        )["hits"]
            .as_array()
            .unwrap()
            .len()
    };
    assert_eq!(find("row 02999"), 1);
    assert_eq!(find("row 00000"), 0);
    // The file on disk shrank with it (incremental vacuum).
    let len = std::fs::metadata(dir.path().join(file_name(key))).unwrap().len();
    assert!(len <= policy.cap_bytes + 64 * 1024, "{len}");
}

fn wait_for(what: &str, f: impl Fn() -> bool) {
    let until = Instant::now() + Duration::from_secs(10);
    while !f() {
        assert!(Instant::now() < until, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn nothing_is_written_unless_the_project_opted_in() {
    let dir = common::tempdir();
    let hist = dir.path().join("history");
    let m = Masker::default();
    // An older web never says: nothing is kept.
    let mut r = Recorder::new(dir.path());
    assert!(!r.is_on());
    r.record(
        "github.com/org/shop",
        vec![entry(now(), "w1:p1", "shell", "hello there")],
        m.clone(),
    );
    // On for one project: another project's lines are not kept.
    r.apply(policies(&["github.com/org/shop"]));
    assert!(r.is_on());
    r.record(
        "github.com/org/other",
        vec![entry(now(), "w1:p2", "shell", "not kept")],
        m.clone(),
    );
    r.record(
        "github.com/org/shop",
        vec![entry(now(), "w1:p1", "shell", "kept here")],
        m.clone(),
    );
    let kept = hist.join(file_name("github.com/org/shop"));
    wait_for("the opted-in project's file", || {
        kept.exists()
            && Store::open(&hist, &Policy::new("github.com/org/shop", 30)).is_ok_and(|s| s.count().unwrap_or(0) == 1)
    });
    assert!(kept.exists());
    assert!(!hist.join(file_name("github.com/org/other")).exists());
    let files: Vec<_> = history::existing_files(&hist).into_iter().map(|(_, k)| k).collect();
    assert_eq!(files, ["github.com/org/shop"]);

    // Turned off: the file goes, and nothing more is written.
    r.apply(Policies::new());
    assert!(!r.is_on());
    wait_for("the file to be forgotten", || !kept.exists());
    r.record(
        "github.com/org/shop",
        vec![entry(now(), "w1:p1", "shell", "after off")],
        m,
    );
    std::thread::sleep(Duration::from_millis(200));
    assert!(!kept.exists());
    assert!(history::existing_files(&hist).is_empty());
}

#[test]
fn the_config_field_is_validated() {
    let p = validate(&json!({"projects": [{"key": "github.com/org/shop"}, {"key": "path:m1:/w", "days": 7}]})).unwrap();
    assert_eq!(p["github.com/org/shop"].days, history::DEFAULT_DAYS);
    assert_eq!(p["path:m1:/w"].days, 7);
    assert!(validate(&json!({"projects": []})).unwrap().is_empty());
    for bad in [
        json!({}),
        json!({"projects": [{"key": ""}]}),
        json!({"projects": [{"key": "has space"}]}),
        json!({"projects": [{"key": "k", "days": 0}]}),
        json!({"projects": [{"key": "k", "days": 400}]}),
    ] {
        assert!(validate(&bad).is_err(), "{bad}");
    }
    // Two keys never share a file, and the name is safe.
    assert_ne!(file_name("github.com/a/shop"), file_name("github.com/b/shop"));
    assert!(file_name("path:m1:/../../etc").ends_with(".sqlite"));
    assert!(!file_name("path:m1:/../../etc").contains('/'));
}

fn seeded(dir: &Path) -> Policies {
    let m = Masker::default();
    let t = now();
    let mut a = Store::open(dir, &Policy::new("github.com/org/shop", 30)).unwrap();
    a.write(
        &[
            entry(t - 3 * DAY, "w1:p1", "shell", "deploy failed: exit 1"),
            entry(t - DAY, "w1:p2", "claude", "deploy started"),
            entry(t - 1000, "w1:p1", "shell", "deploy done"),
        ],
        &m,
    )
    .unwrap();
    let mut chat = entry(t - 500, "c_123", "codex", "Should we deploy on Friday?");
    chat.kind = "chat";
    chat.line = None;
    chat.turn = Some("t_9".into());
    let mut b = Store::open(dir, &Policy::new("github.com/org/site", 30)).unwrap();
    b.write(&[chat], &m).unwrap();
    policies(&["github.com/org/shop", "github.com/org/site"])
}

fn run(dir: &Path, p: &Policies, params: Value) -> Value {
    let q = parse_query(&params).unwrap();
    search(dir, p, &q, Instant::now() + Duration::from_secs(5))
}

fn texts(r: &Value) -> Vec<String> {
    r["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["snippet"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn search_filters_by_session_agent_and_date_newest_first() {
    let dir = common::tempdir();
    let p = seeded(dir.path());
    let all = run(dir.path(), &p, json!({"q": "deploy"}));
    assert_eq!(all["type"], "search_hits");
    assert_eq!(
        texts(&all),
        [
            "Should we deploy on Friday?",
            "deploy done",
            "deploy started",
            "deploy failed: exit 1"
        ]
    );
    assert_eq!(all["projects"], 2);
    assert_eq!(all["partial"], false);
    let chat = &all["hits"][0];
    assert_eq!(chat["kind"], "chat");
    assert_eq!(chat["turn"], "t_9");
    assert_eq!(chat["project"], "github.com/org/site");
    assert_eq!(all["hits"][1]["line"], 1);

    assert_eq!(
        texts(&run(dir.path(), &p, json!({"q": "deploy", "session": "w1:p1"}))),
        ["deploy done", "deploy failed: exit 1"]
    );
    // The session's name works too.
    assert_eq!(
        texts(&run(dir.path(), &p, json!({"q": "deploy", "session": "w1:p2 title"}))),
        ["deploy started"]
    );
    assert_eq!(
        texts(&run(dir.path(), &p, json!({"q": "deploy", "agent": "claude"}))),
        ["deploy started"]
    );
    let since = now() - 2 * DAY;
    assert_eq!(
        texts(&run(dir.path(), &p, json!({"q": "deploy", "since": since}))).len(),
        3
    );
    assert_eq!(
        texts(&run(
            dir.path(),
            &p,
            json!({"q": "deploy", "project": "github.com/org/site"})
        ))
        .len(),
        1
    );
    // Case-insensitive, and short queries still work (a scan, not the trigram index).
    assert_eq!(
        texts(&run(dir.path(), &p, json!({"q": "DEPLOY DONE"}))),
        ["deploy done"]
    );
    assert_eq!(
        texts(&run(dir.path(), &p, json!({"q": "1"}))),
        ["deploy failed: exit 1"]
    );
    // Quotes and FTS syntax are plain text.
    assert!(texts(&run(dir.path(), &p, json!({"q": "\"deploy OR x"}))).is_empty());
    assert!(texts(&run(dir.path(), &p, json!({"q": "exit*"}))).is_empty());
    // A project not opted in is not searched even when its file is there.
    assert_eq!(
        texts(&run(
            dir.path(),
            &policies(&["github.com/org/shop"]),
            json!({"q": "Friday"})
        ))
        .len(),
        0
    );
}

#[test]
fn search_is_bounded_and_time_boxed() {
    let dir = common::tempdir();
    let p = seeded(dir.path());
    let r = run(dir.path(), &p, json!({"q": "deploy", "limit": 2}));
    assert_eq!(r["hits"].as_array().unwrap().len(), 2);
    assert_eq!(r["more"], true);
    // Bounds on every parameter.
    for bad in [
        json!({}),
        json!({"q": ""}),
        json!({"q": "   "}),
        json!({"q": "x".repeat(history::QUERY_MAX + 1)}),
        json!({"q": "x", "limit": 0}),
        json!({"q": "x", "limit": history::LIMIT_MAX + 1}),
        json!({"q": "x", "since": -1}),
        json!({"q": "x", "since": "yesterday"}),
        json!({"q": "x", "session": ""}),
        json!({"q": "x", "agent": 7}),
    ] {
        assert!(parse_query(&bad).is_err(), "{bad}");
    }
    assert_eq!(parse_query(&json!({"q": "x"})).unwrap().limit, history::LIMIT_DEFAULT);
    // A deadline already passed: no work, marked partial.
    let q = parse_query(&json!({"q": "deploy"})).unwrap();
    let r = search(dir.path(), &p, &q, Instant::now());
    assert_eq!(r["partial"], true);
    assert!(r["hits"].as_array().unwrap().is_empty());
    // A slow scan is interrupted at the deadline rather than running on.
    let mut big = Store::open(dir.path(), &Policy::new("github.com/org/big", 30)).unwrap();
    let rows: Vec<Entry> = (0..20_000)
        .map(|i| {
            entry(
                now() - 10_000 + i,
                "w1:p9",
                "shell",
                &format!("noise {i} {}", "xy".repeat(40)),
            )
        })
        .collect();
    big.write(&rows, &Masker::default()).unwrap();
    let p = policies(&["github.com/org/big"]);
    let q = parse_query(&json!({"q": "zz"})).unwrap();
    let started = Instant::now();
    let r = search(dir.path(), &p, &q, Instant::now() + Duration::from_millis(1));
    assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
    assert_eq!(r["partial"], true, "{r}");
}

#[test]
fn the_snippet_places_the_match_in_utf16_units() {
    let (s, m) = snippet("naïve 😀 deploy now", "DEPLOY");
    assert_eq!(s, "naïve 😀 deploy now");
    let units: Vec<u16> = s.encode_utf16().collect();
    let (a, b) = (m[0].as_u64().unwrap() as usize, m[1].as_u64().unwrap() as usize);
    assert_eq!(String::from_utf16(&units[a..b]).unwrap(), "deploy");
    // A long line is cut around the match, with its marks counted.
    let long = format!("{}needle{}", "a".repeat(500), "b".repeat(500));
    let (s, m) = snippet(&long, "needle");
    assert!(s.starts_with('…') && s.ends_with('…'));
    assert!(s.chars().count() <= history::SNIPPET_MAX + 2);
    let units: Vec<u16> = s.encode_utf16().collect();
    let (a, b) = (m[0].as_u64().unwrap() as usize, m[1].as_u64().unwrap() as usize);
    assert_eq!(String::from_utf16(&units[a..b]).unwrap(), "needle");
    // No place for the match: null.
    assert_eq!(snippet("abc", "zzz").1, Value::Null);
}
