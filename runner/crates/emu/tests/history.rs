//! History mode (decision 0009): the runner's long pane history.
//!
//! - every line that scrolls off is kept exactly once, byte-for-byte the same
//!   text a plain `vt100` scrollback would hold (real Claude Code recordings,
//!   synthetic colour / wide / wrapped streams, alternate-screen switches);
//! - the limit, absolute indices and paging;
//! - `CSI 3 J` and `ESC c` clear it; huge scroll bursts never panic;
//! - redraws of a synchronized frame taller than the screen are kept once;
//! - snapshots carry the newest lines and say which (the history OSC);
//! - memory per pane at 50,000 lines of a busy Claude session.

use std::time::Instant;
use tmuxedo_emu::{snapshot, Emulator, DEFAULT_HISTORY_LINES};

fn fixture(name: &str) -> Vec<u8> {
    let p = format!("{}/../../tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&p).unwrap_or_else(|e| panic!("{p}: {e}"))
}

fn history_text(e: &Emulator) -> Vec<String> {
    e.history().unwrap().iter().map(|l| l.text()).collect()
}

/// Plain vt100 scrollback text for the same stream (the reference).
fn plain_scrollback(rows: u16, cols: u16, bytes: &[u8]) -> Vec<String> {
    let mut p = vt100::Parser::new(rows, cols, 1_000_000);
    p.process(bytes);
    scrollback_text(&mut p)
}

#[track_caller]
fn assert_lines(got: &[String], want: &[String], label: &str) {
    if let Some(i) = (0..got.len().max(want.len())).find(|&i| got.get(i) != want.get(i)) {
        let a = i.saturating_sub(2);
        panic!(
            "{label}: first difference at line {i} of {} / {}\n got: {:?}\nwant: {:?}",
            got.len(),
            want.len(),
            &got[a.min(got.len())..(i + 3).min(got.len())],
            &want[a.min(want.len())..(i + 3).min(want.len())]
        );
    }
}

fn feed_in_reads(e: &mut Emulator, bytes: &[u8], read: usize) {
    for c in bytes.chunks(read) {
        e.feed(c);
    }
}

#[test]
fn claude_recordings_keep_every_line_once_and_match_vt100() {
    for (name, rows, cols) in [("claude-busy-120x40.bin", 40, 120), ("claude-busy-50x20.bin", 20, 50)] {
        let bytes = fixture(name);
        let want = plain_scrollback(rows, cols, &bytes);
        assert!(
            want.len() > 500,
            "{name}: the recording must scroll a lot ({})",
            want.len()
        );
        // PTY-sized reads, odd reads, one huge read: the same history.
        for read in [4096, 977, bytes.len()] {
            let mut e = Emulator::with_history(rows, cols, DEFAULT_HISTORY_LINES);
            feed_in_reads(&mut e, &bytes, read);
            assert_lines(&history_text(&e), &want, &format!("{name} in reads of {read}"));
            assert_eq!(e.history().unwrap().lost, 0);
        }
        // Every paragraph of every answer once: the conversation, not copies of frames.
        let mut e = Emulator::with_history(rows, cols, DEFAULT_HISTORY_LINES);
        e.feed(&bytes);
        let all = e.recent_unwrapped(usize::MAX).join("\n");
        let mut answers = 0;
        for answer in 1..=20 {
            let n = all.matches(&format!("Paragraph 5 of answer {answer}:")).count();
            assert!(n <= 1, "{name}: answer {answer} paragraph 5 appears {n} times");
            answers += n;
        }
        assert!(answers >= 3, "{name}: {answers} answers in the history");
        assert_eq!(all.matches("End of answer").count(), answers, "{name}");
    }
}

#[test]
fn full_screen_claude_adds_nothing_to_history_and_restores_exactly() {
    let bytes = fixture("claude-fullscreen-120x40.bin");
    let mut e = Emulator::with_history(40, 120, DEFAULT_HISTORY_LINES);
    e.feed(&bytes);
    assert!(e.screen().alternate_screen());
    assert!(e.history().unwrap().is_empty());
    let snap = e.snapshot(900 * 1024);
    let mut v = Emulator::new(40, 120, 2000);
    v.feed(&snap);
    assert_eq!(v.screen().contents(), e.screen().contents());
    assert!(v.screen().alternate_screen());
}

fn synthetic() -> Vec<u8> {
    let mut s = vec![];
    for i in 0..3000 {
        match i % 5 {
            0 => s.extend_from_slice(format!("\x1b[1;31mred {i}\x1b[0m plain\r\n").as_bytes()),
            1 => s.extend_from_slice(format!("wide 漢字テスト {i}\r\n").as_bytes()),
            2 => s.extend_from_slice(format!("{} wrapped {i}\r\n", "x".repeat(130)).as_bytes()),
            3 => s.extend_from_slice(format!("\x1b[44m   blue bg {i}   \x1b[0m\r\n").as_bytes()),
            _ => s.extend_from_slice(format!("\x1b[38;2;1;2;3mrgb\x1b[0m\ttab {i}\r\n").as_bytes()),
        }
        if i == 1000 {
            // Into the alternate screen and out again in one read, with output on both sides.
            s.extend_from_slice(b"before alt\r\n\x1b[?1049h\x1b[Halt screen\r\nalt 2\r\n\x1b[?1049lafter alt\r\n");
        }
    }
    s
}

#[test]
fn synthetic_streams_match_vt100_including_alternate_screen_switches() {
    let bytes = synthetic();
    for (rows, cols) in [(24u16, 80u16), (5, 30), (60, 200)] {
        let want = plain_scrollback(rows, cols, &bytes);
        for read in [1, 333, 65536] {
            let mut e = Emulator::with_history(rows, cols, 100_000);
            feed_in_reads(&mut e, &bytes, read);
            assert_lines(&history_text(&e), &want, &format!("{rows}x{cols} reads of {read}"));
        }
    }
}

#[test]
fn the_limit_keeps_the_newest_and_indices_keep_counting() {
    let mut e = Emulator::with_history(10, 40, 50_000);
    let mut s = Vec::new();
    for i in 0..60_009 {
        s.extend_from_slice(format!("line {i}\r\n").as_bytes());
    }
    e.feed(&s);
    let h = e.history().unwrap();
    assert_eq!(h.len(), 50_000);
    // 60,009 lines written + an empty cursor line: 60,000 scrolled off a 10-row screen.
    assert_eq!(h.end(), 60_000);
    assert_eq!(h.start(), 10_000);
    assert_eq!(h.get(10_000).unwrap().text(), "line 10000");
    assert_eq!(h.get(59_999).unwrap().text(), "line 59999");
}

#[test]
fn pages_cover_the_history_exactly_within_budget() {
    let mut e = Emulator::with_history(10, 40, 50_000);
    let mut s = Vec::new();
    for i in 0..20_000 {
        s.extend_from_slice(format!("\x1b[32mline\x1b[0m {i}\r\n").as_bytes());
    }
    e.feed(&s);
    let end = e.history().unwrap().end();
    let mut before = None;
    let mut got: Vec<String> = vec![];
    let mut pages = 0;
    loop {
        let p = e.history_page(before, 3000, 64 * 1024).unwrap();
        let cost: usize = p
            .lines
            .iter()
            .map(|l| tmuxedo_emu::history::json_len(&l.ansi) + 4)
            .sum();
        assert!(cost <= 64 * 1024, "page of {cost}");
        assert_eq!(p.end, end);
        assert_eq!(p.first + p.lines.len() as u64, before.unwrap_or(end));
        let mut texts: Vec<String> = p.lines.iter().map(|l| l.text()).collect();
        texts.append(&mut got);
        got = texts;
        pages += 1;
        if !p.more {
            break;
        }
        before = Some(p.first);
    }
    assert!(pages > 3);
    assert_eq!(got.len() as u64, end);
    assert_eq!(got[0], "line 0");
    assert_eq!(got.last().unwrap(), &format!("line {}", end - 1));
}

#[test]
fn clear_scrollback_and_full_reset_clear_history() {
    let mut e = Emulator::with_history(5, 20, 1000);
    for i in 0..20 {
        e.feed(format!("a{i}\r\n").as_bytes());
    }
    assert!(e.history().unwrap().len() > 10);
    // `clear` (bash): home, erase screen, erase saved lines; split across reads.
    e.feed(b"\x1b[H\x1b[2J\x1b[3");
    e.feed(b"J");
    assert!(e.history().unwrap().is_empty());
    let end = e.history().unwrap().end();
    for i in 0..10 {
        e.feed(format!("b{i}\r\n").as_bytes());
    }
    let h = e.history().unwrap();
    assert_eq!(h.start(), end);
    assert_eq!(h.get(end).unwrap().text(), "b0");
    e.feed(b"\x1bc");
    assert!(e.history().unwrap().is_empty());
    // Lines after the reset are kept again.
    for i in 0..8 {
        e.feed(format!("c{i}\r\n").as_bytes());
    }
    assert_eq!(history_text(&e)[0], "c0");
}

#[test]
fn a_huge_scroll_burst_is_counted_not_fatal() {
    let mut e = Emulator::with_history(30, 20, 10_000);
    for i in 0..40 {
        e.feed(format!("x{i}\r\n").as_bytes());
    }
    // 40 x CSI 999 S in one read: far more than the staging area.
    e.feed("\x1b[999S".repeat(80).as_bytes());
    let h = e.history().unwrap();
    assert!(h.lost >= 1);
    assert!(h.len() >= 11);
}

/// A program that redraws a frame taller than the screen in synchronized
/// updates, moving up as far as the terminal lets it, as log-update style
/// renderers do: the top of the frame scrolls off again on every redraw.
fn overflowing_redraws(rows: usize, frames: usize, sync: bool) -> Vec<u8> {
    let mut s = vec![];
    let height = rows + 6;
    for f in 0..frames {
        if sync {
            s.extend_from_slice(b"\x1b[?2026h");
        }
        if f > 0 {
            s.extend_from_slice(format!("\x1b[{}A\r\x1b[J", height - 1).as_bytes());
        }
        for i in 0..height {
            if i == height - 1 {
                s.extend_from_slice(format!("spinner {f}").as_bytes());
            } else {
                s.extend_from_slice(format!("conversation line {i}\r\n").as_bytes());
            }
        }
        if sync {
            s.extend_from_slice(b"\x1b[?2026l");
        }
    }
    s
}

#[test]
fn redrawn_frames_taller_than_the_screen_are_kept_once() {
    let rows = 10;
    let bytes = overflowing_redraws(rows, 200, true);
    let plain = plain_scrollback(rows as u16, 40, &bytes);
    let copies = plain.iter().filter(|l| *l == "conversation line 0").count();
    assert!(copies >= 100, "the stream must reproduce the copies ({copies})");
    let mut e = Emulator::with_history(rows as u16, 40, 50_000);
    feed_in_reads(&mut e, &bytes, 1000);
    let h = history_text(&e);
    assert_eq!(h.iter().filter(|l| *l == "conversation line 0").count(), 1, "{h:?}");
    // A 16-line frame on a 10-row screen: lines 0..6 are above the screen, once.
    assert_eq!(h, (0..6).map(|i| format!("conversation line {i}")).collect::<Vec<_>>());
    assert!(e.history().unwrap().redraws_dropped > 1000);
    // Without synchronized output nothing marks a redraw: everything is kept.
    let mut e = Emulator::with_history(rows as u16, 40, 50_000);
    e.feed(&overflowing_redraws(rows, 20, false));
    assert!(history_text(&e).iter().filter(|l| *l == "conversation line 0").count() >= 10);
}

#[test]
fn snapshots_carry_the_newest_history_and_say_which() {
    let bytes = fixture("claude-busy-120x40.bin");
    let mut e = Emulator::with_history(40, 120, DEFAULT_HISTORY_LINES);
    e.feed(&bytes);
    let h_end = e.history().unwrap().end();
    let snap = e.snapshot_with(900 * 1024, 16 * 1024);
    assert!(snap.len() < 40 * 1024, "{}", snap.len());
    let text = String::from_utf8_lossy(&snap);
    let osc = text
        .split("\x1b]777;tmuxedo-history;")
        .nth(1)
        .unwrap()
        .split('\x07')
        .next()
        .unwrap();
    let field = |k: &str| -> u64 {
        osc.split(';')
            .find_map(|kv| kv.strip_prefix(&format!("{k}=")))
            .unwrap()
            .parse()
            .unwrap()
    };
    assert_eq!(field("start"), 0);
    assert_eq!(field("end"), h_end);
    assert_eq!(field("limit"), DEFAULT_HISTORY_LINES as u64);
    let first = field("first");
    assert!(first > 0 && first < h_end);
    // A viewer that ignores the OSC gets exactly history[first..] then the screen.
    let mut v = Emulator::new(40, 120, 100_000);
    v.feed(&snap);
    assert_eq!(v.screen().contents(), e.screen().contents());
    let want: Vec<String> = e
        .history()
        .unwrap()
        .iter()
        .skip(first as usize)
        .map(|l| l.text())
        .collect();
    assert_lines(&plain_scrollback_of(&mut v), &want, "plain viewer");
    // A history viewer of the snapshot rebuilds the same lines (a second generation).
    let mut v2 = Emulator::with_history(40, 120, DEFAULT_HISTORY_LINES);
    v2.feed(&snap);
    assert_lines(&history_text(&v2), &want, "history viewer");
}

fn plain_scrollback_of(e: &mut Emulator) -> Vec<String> {
    scrollback_text(e.parser_mut())
}

fn scrollback_text<CB: vt100::Callbacks>(p: &mut vt100::Parser<CB>) -> Vec<String> {
    let (rows, cols) = p.screen().size();
    let n = snapshot::scrollback_count(p);
    let mut out = vec![];
    let mut k = n;
    while k > 0 {
        p.screen_mut().set_scrollback(k);
        let m = k.min(rows as usize);
        for r in 0..m as u16 {
            out.push(p.screen().contents_between(r, 0, r, cols).trim_end().to_string());
        }
        k -= m;
    }
    p.screen_mut().set_scrollback(0);
    out
}

/// The OSC is checked by both sides against one fixture
/// (packages/protocol/fixtures/history.json, also history.test.ts).
#[test]
fn history_osc_matches_the_shared_fixture() {
    let p = format!(
        "{}/../../../packages/protocol/fixtures/history.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let fx = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{p}: {e}"));
    for (s, f, e, l) in [
        (0u64, 1200u64, 3400u64, 50_000usize),
        (7, 7, 7, 20_000),
        (9_007_199_254_000, 9_007_199_254_100, 9_007_199_254_999, 1_000_000),
    ] {
        let osc = String::from_utf8(snapshot::history_osc(s, f, e, l)).unwrap();
        let json = osc.replace('\x1b', "\\u001b").replace('\x07', "\\u0007");
        assert!(
            fx.contains(&format!("\"osc\": \"{json}\"")),
            "{json} not in the fixture"
        );
    }
}

#[test]
fn saved_history_rebuilds_the_same_lines() {
    let bytes = fixture("claude-busy-50x20.bin");
    let mut e = Emulator::with_history(20, 50, DEFAULT_HISTORY_LINES);
    e.feed(&bytes);
    let saved = e.history_bytes(8 << 20);
    let mut r = Emulator::with_history(20, 50, DEFAULT_HISTORY_LINES);
    r.feed(&saved);
    let mut want = history_text(&e);
    want.extend(snapshot::screen_lines_text(e.screen()));
    while want.last().is_some_and(|l| l.is_empty()) {
        want.pop();
    }
    assert_eq!(r.recent_lines(usize::MAX), want);
    // A budget keeps the newest part.
    let small = e.history_bytes(4096);
    assert!(small.len() <= 4096 + 2048);
}

#[test]
fn recent_reads_match_plain_mode() {
    let bytes = fixture("claude-busy-50x20.bin");
    let mut h = Emulator::with_history(20, 50, DEFAULT_HISTORY_LINES);
    let mut p = Emulator::new(20, 50, 1_000_000);
    h.feed(&bytes);
    p.feed(&bytes);
    for n in [1, 24, 300, usize::MAX] {
        assert_eq!(h.recent_lines(n), p.recent_lines(n), "recent {n}");
        assert_eq!(h.recent_unwrapped(n), p.recent_unwrapped(n), "unwrapped {n}");
    }
    assert_eq!(h.detection_text(), p.detection_text());
}

#[test]
fn pane_clear_forgets_history_and_a_gap_keeps_it() {
    let mut e = Emulator::with_history(5, 20, 100);
    for i in 0..30 {
        e.feed(format!("l{i}\r\n").as_bytes());
    }
    let n = e.history().unwrap().len();
    e.reset_screen();
    assert_eq!(e.history().unwrap().len(), n);
    assert_eq!(e.screen().contents(), "");
    let end = e.history().unwrap().end();
    e.reset();
    assert!(e.history().unwrap().is_empty());
    // Indices keep counting: a viewer's old index never names a new line.
    assert_eq!(e.history().unwrap().start(), end);
    assert_eq!(e.history().unwrap().limit(), 100);
}

#[test]
fn a_byte_cap_bounds_very_wide_colourful_lines() {
    let mut e = Emulator::with_history_bytes(5, 200, 50_000, 256 * 1024);
    let mut s = vec![];
    for i in 0..5000 {
        for c in 0..200 {
            s.extend_from_slice(format!("\x1b[38;2;{};{};{}m#", (i + c) % 256, c % 256, i % 256).as_bytes());
        }
        s.extend_from_slice(b"\r\n");
    }
    e.feed(&s);
    let h = e.history().unwrap();
    assert!(h.len() < 5000 && h.len() > 10, "{}", h.len());
    let lines: usize = h.iter().map(|l| l.cost()).sum();
    assert!(lines <= 256 * 1024, "{lines}");
    assert_eq!(h.end(), 4996);
}

/// Memory per pane at 50,000 lines of a busy Claude session: the recording
/// replayed until 50k lines have scrolled off (decision 0009 quotes this).
#[test]
fn memory_at_50k_lines_of_a_busy_claude_session() {
    let bytes = fixture("claude-busy-120x40.bin");
    let mut e = Emulator::with_history(40, 120, DEFAULT_HISTORY_LINES);
    let t = Instant::now();
    let mut fed = 0usize;
    while e.history().unwrap().end() < 60_000 {
        e.feed(&bytes);
        fed += bytes.len();
    }
    let elapsed = t.elapsed();
    let h = e.history().unwrap();
    assert_eq!(h.len(), DEFAULT_HISTORY_LINES);
    let heap = h.heap_bytes();
    let payload: usize = h.iter().map(|l| l.ansi.len()).sum();
    let staging = 1024 * 120 * 32;
    let vt100_equiv = DEFAULT_HISTORY_LINES * 120 * 32;
    println!(
        "MEASURE history 50k lines @120 cols: heap {:.1} MiB (payload {:.1} MiB, avg {} B/line) + vt100 staging {:.1} MiB; \
         vt100 scrollback for the same lines would be {:.0} MiB; fed {:.1} MiB in {:.2?} ({:.0} MiB/s, debug build)",
        heap as f64 / 1048576.0,
        payload as f64 / 1048576.0,
        payload / h.len(),
        staging as f64 / 1048576.0,
        vt100_equiv as f64 / 1048576.0,
        fed as f64 / 1048576.0,
        elapsed,
        fed as f64 / 1048576.0 / elapsed.as_secs_f64()
    );
    assert!(heap < 16 << 20, "history heap {heap}");
}

/// The numbers in decision 0009 (run with
/// `cargo test --release -p tmuxedo-emu --test history -- --ignored --nocapture`):
/// feed throughput with and without history, and attach snapshot sizes then and now.
#[test]
#[ignore = "a measurement, not a check"]
fn measure_throughput_and_snapshot_sizes() {
    let claude = fixture("claude-busy-120x40.bin");
    let mut log = Vec::new();
    for i in 0..400_000 {
        log.extend_from_slice(format!("line {i} of a plain log with some text\r\n").as_bytes());
    }
    for (name, data, reps) in [("busy Claude session", &claude, 60usize), ("plain log flood", &log, 1)] {
        for history in [false, true] {
            let mut e = if history {
                Emulator::with_history(40, 120, DEFAULT_HISTORY_LINES)
            } else {
                Emulator::new(40, 120, 2000)
            };
            let t = Instant::now();
            for _ in 0..reps {
                for c in data.chunks(65536) {
                    e.feed(c);
                }
            }
            let mib = (data.len() * reps) as f64 / 1048576.0;
            let what = if history { "history 50k" } else { "vt100 2000 (before)" };
            println!(
                "MEASURE throughput {name}, {what}: {:.0} MiB/s",
                mib / t.elapsed().as_secs_f64()
            );
        }
    }
    for (f, r, c) in [
        ("claude-busy-120x40.bin", 40u16, 120u16),
        ("claude-busy-50x20.bin", 20, 50),
    ] {
        let bytes = fixture(f);
        let mut old = Emulator::new(r, c, 2000);
        let mut new = Emulator::with_history(r, c, DEFAULT_HISTORY_LINES);
        while new.history().unwrap().end() < 60_000 {
            old.feed(&bytes);
            new.feed(&bytes);
        }
        let o = old.snapshot(1024 * 1024 - 100);
        let n = new.snapshot_with(1024 * 1024 - 100, 96 * 1024);
        let txt = String::from_utf8_lossy(&n);
        let mark = txt
            .split("tmuxedo-history;")
            .nth(1)
            .unwrap()
            .split('\x07')
            .next()
            .unwrap()
            .to_string();
        println!(
            "MEASURE snapshot {f}: before {} B (2000 lines); now {} B ({mark}); all 50k lines {} B",
            o.len(),
            n.len(),
            new.history_bytes(usize::MAX).len()
        );
    }
}
