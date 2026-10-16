//! Snapshot round trip, cell-exact, on the Phase 0 recordings (nvim, htop,
//! Claude Code at 120x40) at 20 checkpoints each, plus synthetic streams that
//! exercise scrollback (colour, wide characters, soft wraps, blank runs with
//! a background colour) and the synchronized-output rule.

use std::time::Instant;
use tmuxedo_emu::{EmuEvent, Emulator, SnapshotGate};
use vt100::Parser;

const ROWS: u16 = 40;
const COLS: u16 = 120;
const SB: usize = 2000;
const MAX: usize = 900 * 1024;

fn fixture(name: &str) -> Vec<u8> {
    let p = format!("{}/../../tests/fixtures/{name}.bin", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&p).unwrap_or_else(|e| panic!("{p}: {e}"))
}

fn cell_eq(a: &vt100::Screen, b: &vt100::Screen, rows: u16, cols: u16, what: &str) -> Result<(), String> {
    for r in 0..rows {
        for c in 0..cols {
            let (x, y) = (a.cell(r, c).unwrap(), b.cell(r, c).unwrap());
            if x.contents() != y.contents()
                || x.fgcolor() != y.fgcolor()
                || x.bgcolor() != y.bgcolor()
                || x.bold() != y.bold()
                || x.dim() != y.dim()
                || x.italic() != y.italic()
                || x.underline() != y.underline()
                || x.inverse() != y.inverse()
                || x.is_wide() != y.is_wide()
            {
                return Err(format!(
                    "{what} cell ({r},{c}): {:?} {:?}/{:?} vs {:?} {:?}/{:?}",
                    x.contents(),
                    x.fgcolor(),
                    x.bgcolor(),
                    y.contents(),
                    y.fgcolor(),
                    y.bgcolor()
                ));
            }
        }
    }
    Ok(())
}

/// Compare screen, cursor, modes and every scrollback line.
fn assert_same<A: vt100::Callbacks, B: vt100::Callbacks>(orig: &mut Parser<A>, copy: &mut Parser<B>, label: &str) {
    let (rows, cols) = orig.screen().size();
    cell_eq(orig.screen(), copy.screen(), rows, cols, "screen").unwrap_or_else(|e| panic!("{label}: {e}"));
    let (a, b) = (orig.screen(), copy.screen());
    assert_eq!(a.cursor_position(), b.cursor_position(), "{label}: cursor");
    assert_eq!(a.alternate_screen(), b.alternate_screen(), "{label}: alt-screen");
    assert_eq!(a.hide_cursor(), b.hide_cursor(), "{label}: cursor visibility");
    assert_eq!(a.application_cursor(), b.application_cursor(), "{label}: app cursor");
    assert_eq!(a.application_keypad(), b.application_keypad(), "{label}: app keypad");
    assert_eq!(a.bracketed_paste(), b.bracketed_paste(), "{label}: bracketed paste");
    assert_eq!(a.mouse_protocol_mode(), b.mouse_protocol_mode(), "{label}: mouse mode");
    assert_eq!(
        a.mouse_protocol_encoding(),
        b.mouse_protocol_encoding(),
        "{label}: mouse enc"
    );
    if a.alternate_screen() {
        return;
    }
    let n = tmuxedo_emu::snapshot::scrollback_count(orig);
    assert_eq!(
        n,
        tmuxedo_emu::snapshot::scrollback_count(copy),
        "{label}: scrollback length"
    );
    let mut k = 0;
    while k < n {
        orig.screen_mut().set_scrollback(n - k);
        copy.screen_mut().set_scrollback(n - k);
        let m = (n - k).min(rows as usize) as u16;
        cell_eq(
            orig.screen(),
            copy.screen(),
            m,
            cols,
            &format!("scrollback page at {k}"),
        )
        .unwrap_or_else(|e| panic!("{label}: {e}"));
        k += m as usize;
    }
    orig.screen_mut().set_scrollback(0);
    copy.screen_mut().set_scrollback(0);
}

fn round_trip(emu: &mut Emulator, label: &str) -> usize {
    let (rows, cols) = emu.size();
    let snap = emu.snapshot(MAX);
    assert!(snap.len() <= MAX, "{label}: snapshot {} bytes", snap.len());
    let mut fresh = Parser::new(rows, cols, SB);
    fresh.process(&snap);
    assert_same(emu.parser_mut(), &mut fresh, label);
    snap.len()
}

#[test]
fn recordings_round_trip_at_checkpoints() {
    for name in ["nvim", "htop", "claude"] {
        let bytes = fixture(name);
        for k in 1..=20 {
            let cut = bytes.len() * k / 20;
            let mut emu = Emulator::new(ROWS, COLS, SB);
            emu.feed(&bytes[..cut]);
            round_trip(&mut emu, &format!("{name} checkpoint {k}"));
        }
    }
}

#[test]
fn recordings_survive_a_second_generation() {
    // A snapshot of a snapshot is still exact (what a viewer resyncing twice sees).
    for name in ["nvim", "htop", "claude"] {
        let mut emu = Emulator::new(ROWS, COLS, SB);
        emu.feed(&fixture(name));
        let snap = emu.snapshot(MAX);
        let mut second = Emulator::new(ROWS, COLS, SB);
        second.feed(&snap);
        assert_same(emu.parser_mut(), second.parser_mut(), name);
    }
}

fn synthetic_scrollback() -> Vec<u8> {
    let mut s = Vec::new();
    for i in 0..300u32 {
        let fg = 16 + (i % 200);
        match i % 6 {
            0 => s.extend_from_slice(format!("\x1b[38;5;{fg}mline {i} \x1b[1mbold\x1b[0m plain\r\n").as_bytes()),
            1 => s.extend_from_slice(format!("wide {i}: 日本語テキスト 🦀 café é\r\n").as_bytes()),
            2 => s.extend_from_slice(format!("\x1b[44m bg run {i} \x1b[K\x1b[0m\r\n").as_bytes()),
            3 => {
                // Soft-wrapped line longer than the width.
                let long: String = (0..(COLS as u32 + 37))
                    .map(|j| char::from(b'a' + ((i + j) % 26) as u8))
                    .collect();
                s.extend_from_slice(format!("{long}\r\n").as_bytes());
            }
            4 => s.extend_from_slice(
                format!("\x1b[38;2;10;200;30m\x1b[48;2;1;2;3mrgb {i}\x1b[0m   gap   \x1b[3;4;7mital-ul-inv\x1b[0m\r\n")
                    .as_bytes(),
            ),
            _ => s.extend_from_slice(format!("\x1b[2mdim {i}\x1b[0m\x1b[45m\x1b[5X\x1b[0m after-ech\r\n").as_bytes()),
        }
    }
    s.extend_from_slice(b"\x1b[?2004h\x1b[?1h\x1b=\x1b[?25l$ prompt");
    s
}

#[test]
fn scrollback_round_trip_is_cell_exact() {
    let stream = synthetic_scrollback();
    for (rows, cols) in [(ROWS, COLS), (24, 80), (5, 30)] {
        for k in [7, 13, 20] {
            let cut = stream.len() * k / 20;
            let mut emu = Emulator::new(rows, cols, SB);
            emu.feed(&stream[..cut]);
            let n = tmuxedo_emu::snapshot::scrollback_count(emu.parser_mut());
            assert!(n > 0, "test stream must create scrollback");
            round_trip(&mut emu, &format!("synthetic {rows}x{cols} at {k}/20"));
        }
    }
}

#[test]
fn snapshot_respects_size_budget_by_dropping_oldest_lines() {
    let mut emu = Emulator::new(24, 80, SB);
    emu.feed(&synthetic_scrollback());
    let small = emu.snapshot(8 * 1024);
    assert!(small.len() <= 8 * 1024, "{}", small.len());
    let mut fresh = Parser::new(24, 80, SB);
    fresh.process(&small);
    // Screen is still exact; the newest scrollback line survives.
    cell_eq(emu.screen(), fresh.screen(), 24, 80, "screen").unwrap();
    let want = emu.recent_lines(30);
    let mut f2 = Emulator::new(24, 80, SB);
    f2.feed(&small);
    assert_eq!(f2.recent_lines(30), want);
}

#[test]
fn snapshot_waits_for_synchronized_frame_end() {
    // A frame is half drawn when the snapshot is requested: the gate holds
    // it, and the snapshot taken at the SyncEnd event shows the whole frame.
    let mut emu = Emulator::new(5, 20, 100);
    emu.feed(b"old frame");
    let mut gate = SnapshotGate::default();
    let mut stream: &[u8] = b"\x1b[?2026h\x1b[H\x1b[2Jnew ";
    while !stream.is_empty() {
        let (n, _) = emu.feed_until_event(stream);
        stream = &stream[n..];
    }
    assert!(emu.in_sync());
    assert!(!gate.request(9, Instant::now(), emu.in_sync()), "must defer mid-frame");
    let mut taken = None;
    let mut stream: &[u8] = b"frame\x1b[?2026l and more after";
    while !stream.is_empty() {
        let (n, ev) = emu.feed_until_event(stream);
        stream = &stream[n..];
        if ev == Some(EmuEvent::SyncEnd) {
            for req in gate.sync_ended() {
                taken = Some((req, emu.snapshot(MAX)));
            }
        }
    }
    let (req, snap) = taken.expect("snapshot at sync end");
    assert_eq!(req, 9);
    let mut fresh = Parser::new(5, 20, 0);
    fresh.process(&snap);
    assert_eq!(fresh.screen().contents(), "new frame");
}

#[test]
fn signals_track_title_bell_osc() {
    let mut e = Emulator::new(5, 40, 10);
    e.feed(b"\x1b]0;my title\x07\x07\x1b]9;4;1;50\x07\x1b]9;build done\x07\x1b]777;notify;T;B\x07\x1b]133;D;0\x07");
    let s = e.signals();
    assert_eq!(s.title, "my title");
    assert_eq!(s.bells, 1);
    assert_eq!(s.progress, "4;1;50");
    assert_eq!(s.notifications, 2);
    assert_eq!(s.last_notification, "T: B");
    assert_eq!(s.last_prompt_mark, Some('D'));
}
