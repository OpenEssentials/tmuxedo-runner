use super::*;

fn tmp() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

fn snap(text: &str) -> impl FnOnce() -> Snap + '_ {
    move || Snap {
        ansi: text.as_bytes().to_vec(),
        cols: 80,
        rows: 24,
    }
}

const T0: u64 = 1_760_000_000_000;

#[test]
fn folder_names_hold_any_id_and_no_path() {
    for id in ["w1:p2", "..", ".", "a.b", "x_y-z", "w10:p300"] {
        let d = dir_name(id);
        assert!(!d.contains('/') && d != "." && d != "..", "{id} -> {d}");
        assert_eq!(pane_of(&d).as_deref(), Some(id));
    }
    assert_eq!(dir_name("w1:p2"), "w1~p2");
    assert_eq!(pane_of("%zz"), None);
    assert_eq!(pane_of("%2E"), None, "only the one spelling");
}

#[test]
fn records_and_reads_back_by_time() {
    let d = tmp();
    let mut r = Recorder::new(d.path(), "w1:p1", Limits::default());
    r.start(T0, snap("SCREEN"));
    r.output(0, b"hello ", T0 + 1500, snap("unused"));
    r.marker("status:working", T0 + 1600);
    r.resize(100, 30, T0 + 2000);
    r.output(6, b"world", T0 + 62_345, snap("unused"));
    r.flush();
    let page = read(d.path(), "w1:p1", &From::Start, 1 << 20, false).unwrap().unwrap();
    let got: Vec<(u64, &str, &str)> = page.events.iter().map(|e| (e.at - T0, e.k, e.d.as_str())).collect();
    assert_eq!(
        got,
        vec![
            (0, "r", "80x24"),
            (0, "s", "SCREEN"),
            (1500, "o", "hello "),
            (1600, "m", "status:working"),
            (2000, "r", "100x30"),
            (62_345, "o", "world"),
        ]
    );
    assert_eq!((page.cols, page.rows, page.start_ms, page.more), (80, 24, T0, false));
    // Markers alone.
    let m = read(d.path(), "w1:p1", &From::Start, 1 << 20, true).unwrap().unwrap();
    assert_eq!(m.events.len(), 1);
    // Nothing for another session.
    assert!(read(d.path(), "w1:p9", &From::Start, 100, false).unwrap().is_none());
}

#[test]
fn bytes_are_taken_once() {
    let d = tmp();
    let mut r = Recorder::new(d.path(), "p", Limits::default());
    r.output(0, b"abc", T0, snap(""));
    r.output(0, b"abc", T0 + 1, snap("")); // a re-read of the ring
    r.output(1, b"bcde", T0 + 2, snap("")); // overlaps: still whole, the caller splits
    r.flush();
    let page = read(d.path(), "p", &From::Start, 1 << 20, false).unwrap().unwrap();
    let out: String = page.events.iter().filter(|e| e.k == "o").map(|e| e.d.clone()).collect();
    assert_eq!(out, "abcbcde");
    assert_eq!(r.upto, 5);
}

#[test]
fn split_characters_wait_for_their_end() {
    let d = tmp();
    let mut r = Recorder::new(d.path(), "p", Limits::default());
    let s = "é€😀".as_bytes();
    r.start(T0, snap(""));
    for (i, b) in s.iter().enumerate() {
        r.output(i as u64, &[*b], T0 + i as u64, snap(""));
    }
    r.output(s.len() as u64, &[0xff, b'x'], T0 + 50, snap(""));
    r.flush();
    let page = read(d.path(), "p", &From::Start, 1 << 20, false).unwrap().unwrap();
    let out: String = page.events.iter().filter(|e| e.k == "o").map(|e| e.d.clone()).collect();
    assert_eq!(out, "é€😀\u{fffd}x");
}

#[test]
fn parts_rotate_and_pages_follow_cursors() {
    let d = tmp();
    let lim = Limits {
        segment_bytes: 400,
        ..Limits::default()
    };
    let mut r = Recorder::new(d.path(), "p", lim);
    let mut n = 0u64;
    for i in 0..40u64 {
        let chunk = format!("line {i:02}\r\n");
        r.output(n, chunk.as_bytes(), T0 + i * 1000, snap("REDRAW"));
        n += chunk.len() as u64;
    }
    r.flush();
    let ps = parts(d.path(), "p");
    assert!(ps.len() > 2, "{} parts", ps.len());
    // Page through with a small limit: every line once, each part opened by a redraw.
    let mut from = From::Start;
    let mut all = vec![];
    let mut pages = 0;
    loop {
        let p = read(d.path(), "p", &from, 30, false).unwrap().unwrap();
        pages += 1;
        all.extend(p.events);
        if !p.more {
            break;
        }
        from = From::Cursor(p.next);
    }
    assert!(pages > 5);
    let lines: String = all.iter().filter(|e| e.k == "o").map(|e| e.d.clone()).collect();
    let want: String = (0..40).map(|i| format!("line {i:02}\r\n")).collect();
    assert_eq!(lines, want);
    assert_eq!(all.iter().filter(|e| e.k == "s").count(), ps.len());
    assert!(all.windows(2).all(|w| w[0].at <= w[1].at));
    // By time: the part holding that moment, from its start (its redraw).
    let t = T0 + 30_000;
    let p = read(d.path(), "p", &From::At(t), 1 << 20, false).unwrap().unwrap();
    assert_eq!(
        (p.events[0].k, p.events[1].k),
        ("r", "s"),
        "a part opens with its size, then its redraw"
    );
    assert!(p.events[1].at <= t);
    assert!(p.events.iter().any(|e| e.d == "line 30\r\n"));
    let holding = ps.iter().rposition(|x| x.start_ms <= t).unwrap();
    assert_eq!(p.events[1].at, ps[holding].start_ms);
    // A cursor into a dropped part continues at the next one.
    let c = Cursor {
        part: ps[0].start_ms,
        offset: 200,
        at: ps[0].start_ms,
    };
    fs::remove_file(&ps[0].path).unwrap();
    let p = read(d.path(), "p", &From::Cursor(c), 1 << 20, false).unwrap().unwrap();
    assert_eq!((p.events[1].k, p.events[1].at), ("s", ps[1].start_ms));
}

#[test]
fn retention_drops_old_parts_then_oldest_until_it_fits() {
    let d = tmp();
    let lim = Limits {
        segment_bytes: 1,
        max_bytes: 5_000,
        max_age_ms: 1000 * 3600,
        ..Limits::default()
    };
    let now = T0 + 10 * 3600 * 1000;
    for pane in ["a", "b"] {
        let mut r = Recorder::new(d.path(), pane, lim);
        for i in 0..4u64 {
            r.output(i * 1000, &[b'x'; 1000], T0 + i, snap(""));
        }
    }
    let mut all: Vec<PartFile> = parts(d.path(), "a").into_iter().chain(parts(d.path(), "b")).collect();
    assert_eq!(all.len(), 8);
    all.sort_by_key(|p| p.start_ms);
    // a's first part is two hours old; the rest are fresh, oldest first by mtime.
    set_mtime(&parts(d.path(), "a")[0].path, now - 2 * 3600 * 1000).unwrap();
    for (i, p) in parts(d.path(), "a")
        .iter()
        .skip(1)
        .chain(parts(d.path(), "b").iter())
        .enumerate()
    {
        set_mtime(&p.path, now - 1000 + i as u64).unwrap();
    }
    let size: u64 = parts(d.path(), "a")
        .iter()
        .chain(parts(d.path(), "b").iter())
        .map(|p| p.bytes)
        .sum();
    assert!(size > 5_000, "{size}");
    let open = vec![parts(d.path(), "a")[1].path.clone()];
    let gone = prune(d.path(), &lim, now, &open);
    assert!(gone > 0);
    let left: Vec<PartFile> = parts(d.path(), "a").into_iter().chain(parts(d.path(), "b")).collect();
    assert!(left.iter().map(|p| p.bytes).sum::<u64>() <= 5_000);
    assert!(left.iter().any(|p| p.path == open[0]), "the open part stays");
    assert!(
        !left.iter().any(|p| p.mtime_ms < now - 3600 * 1000),
        "the old part went"
    );
    // The newest are kept.
    let newest = parts(d.path(), "b").last().unwrap().path.clone();
    assert!(left.iter().any(|p| p.path == newest));
}

#[test]
fn remask_finds_a_value_split_across_events() {
    let d = tmp();
    // A value built at run time; not shaped like any real credential.
    let value = format!("val{}x{}", std::process::id(), T0 % 9973);
    let (a, b) = value.split_at(5);
    let mut r = Recorder::new(d.path(), "p", Limits::default());
    r.output(0, format!("before {a}").as_bytes(), T0, snap("S"));
    r.output(100, format!("{b} after").as_bytes(), T0 + 5, snap("S"));
    r.close();
    let path = parts(d.path(), "p")[0].path.clone();
    set_mtime(&path, T0).unwrap();
    let set = MaskSet::from_values([value.as_bytes()]);
    assert_eq!(remask(d.path(), &set, &[]), 1);
    let text = fs::read_to_string(&path).unwrap();
    assert!(!text.contains(&value) && !text.contains(a), "masked");
    let page = read(d.path(), "p", &From::Start, 1 << 20, false).unwrap().unwrap();
    let out: String = page
        .events
        .iter()
        .filter(|e| e.k == "o" || e.k == "s")
        .map(|e| e.d.clone())
        .collect();
    assert_eq!(out, format!("Sbefore {} after", "*".repeat(value.len())));
    assert_eq!(page.events[3].at, T0 + 5, "times kept");
    assert_eq!(parts(d.path(), "p")[0].mtime_ms, T0, "age kept");
    assert_eq!(remask(d.path(), &set, &[]), 0, "nothing left to mask");
}

#[test]
fn on_flag_list_and_delete() {
    let d = tmp();
    assert!(!is_on(d.path(), "w1:p1"));
    set_on(d.path(), "w1:p1", true).unwrap();
    assert!(is_on(d.path(), "w1:p1"));
    let l = list(d.path());
    assert_eq!(l.len(), 1);
    assert_eq!((l[0].0.pane.as_str(), l[0].0.segments, l[0].1), ("w1:p1", 0, true));
    let mut r = Recorder::new(d.path(), "w1:p1", Limits::default());
    r.start(T0, snap("S"));
    r.close();
    let mut r2 = Recorder::new(d.path(), "w2:p1", Limits::default());
    r2.start(T0, snap("S"));
    r2.close();
    let l = list(d.path());
    assert_eq!(l.len(), 2);
    assert_eq!((l[0].0.segments, l[0].0.start_ms, l[1].1), (1, T0, false));
    let bytes = l[1].0.bytes;
    assert_eq!(delete(d.path(), "w2:p1", None), bytes);
    assert_eq!(list(d.path()).len(), 1);
    set_on(d.path(), "w1:p1", false).unwrap();
    assert!(!is_on(d.path(), "w1:p1"));
    assert_eq!(list(d.path()).len(), 1, "the recording outlives the flag");
}

#[test]
fn a_failed_write_stops_recording() {
    let d = tmp();
    let file = d.path().join("not-a-dir");
    fs::write(&file, b"").unwrap();
    let mut r = Recorder::new(&file, "p", Limits::default());
    r.output(0, b"x", T0, snap(""));
    assert!(r.failed.is_some());
    assert!(!r.is_open());
}
