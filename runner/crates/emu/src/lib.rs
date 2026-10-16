//! Terminal emulation for tmuxedo panes (decision 0001: `vt100`).
//!
//! - [`Emulator`] keeps a `vt100` parser per pane, tracks synchronized output
//!   (`?2026` h/l) itself, spots cursor-position queries (`\e[6n`) so the
//!   runner can answer them from the emulator cursor, and serialises
//!   snapshots: scrollback (as formatted lines) + screen + input modes.
//! - [`Matcher`] is a tiny byte-level matcher for fixed escape sequences that
//!   survives sequences split across reads (used here and by the holder to
//!   answer terminal queries).
//! - [`SnapshotGate`] is the "never snapshot mid synchronized frame, but wait
//!   at most 100 ms" rule as a pure, testable state machine.
//! - [`History`] (the runner's panes, [`Emulator::with_history`]) keeps tens of
//!   thousands of lines that scrolled off as compact formatted lines, with
//!   absolute indices for paging, honours `CSI 3 J`, and drops redraws of a
//!   synchronized frame taller than the screen (decision 0009).

pub mod history;
pub mod matcher;
pub mod snapshot;

use std::time::{Duration, Instant};

pub use history::{History, Line, Page};
pub use matcher::Matcher;

pub const ESC: u8 = 0x1b;
/// How long a snapshot may wait for a synchronized frame to end (decision 0001).
pub const SYNC_DEFER: Duration = Duration::from_millis(100);
/// Scrollback lines kept by a plain [`Emulator::new`] (the TUI's viewer side).
pub const DEFAULT_SCROLLBACK: usize = 2000;
/// History lines a runner pane keeps by default (decision 0009; configurable per machine).
pub const DEFAULT_HISTORY_LINES: usize = 50_000;
/// Upper bound for the configured history.
pub const MAX_HISTORY_LINES: usize = 1_000_000;
/// `vt100` scrollback rows used only as a staging area in history mode: every
/// line is copied out right after the bytes that scrolled it.
const STAGING: usize = 1024;
/// Bytes fed between two staging reads: fewer than [`STAGING`] lines can
/// scroll in this many bytes, except with a huge `CSI n S`.
const PIECE: usize = 512;

const SYNC_ON: &[u8] = b"\x1b[?2026h";
const SYNC_OFF: &[u8] = b"\x1b[?2026l";
const DSR_CPR: &[u8] = b"\x1b[6n";
/// Erase saved lines (xterm's "clear scrollback"; `clear` sends it).
const ED3: &[u8] = b"\x1b[3J";
/// Full reset.
const RIS: &[u8] = b"\x1bc";
const PATTERNS: &[&[u8]] = &[
    SYNC_ON,
    SYNC_OFF,
    DSR_CPR,
    ED3,
    RIS,
    // Alternate screen on/off: `vt100` zeroes the main screen's view offset
    // on entry, so history mode processes these on their own, unwatched.
    b"\x1b[?1049h",
    b"\x1b[?1049l",
    b"\x1b[?1047h",
    b"\x1b[?1047l",
    b"\x1b[?47h",
    b"\x1b[?47l",
];
/// Index of the first pattern processed on its own in history mode (RIS and the alternate-screen switches).
const ALONE_FROM: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmuEvent {
    SyncStart,
    SyncEnd,
    /// The program asked for the cursor position (`\e[6n`); answer with [`Emulator::cursor_report`].
    CursorQuery,
}

/// Terminal signals the agent-status heuristics and the detection engine read
/// (decision: tier 0 uses title, BEL, OSC 9/777 notifications, OSC 133 marks).
#[derive(Debug, Default, Clone)]
pub struct Signals {
    /// Last OSC 0/2 window title.
    pub title: String,
    /// Last OSC 9;4 progress payload (e.g. `4;1;50`), the detection input `progress`.
    pub progress: String,
    pub bells: u64,
    /// OSC 9 (non-progress) and OSC 777;notify messages, counted; the last one kept.
    pub notifications: u64,
    pub last_notification: String,
    /// OSC 133 prompt marks: the last mark letter (A prompt, B input, C running, D done).
    pub last_prompt_mark: Option<char>,
    pub prompt_marks: u64,
}

impl vt100::Callbacks for Signals {
    fn audible_bell(&mut self, _: &mut vt100::Screen) {
        self.bells += 1;
    }
    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        self.title = String::from_utf8_lossy(title).into_owned();
    }
    fn unhandled_osc(&mut self, _: &mut vt100::Screen, params: &[&[u8]]) {
        let text: Vec<String> = params.iter().map(|p| String::from_utf8_lossy(p).into_owned()).collect();
        match text.first().map(String::as_str) {
            Some("9") if text.get(1).map(String::as_str) == Some("4") => {
                self.progress = text[1..].join(";");
            }
            Some("9") => {
                self.notifications += 1;
                self.last_notification = text[1..].join(";");
            }
            Some("777") if text.get(1).map(String::as_str) == Some("notify") => {
                self.notifications += 1;
                self.last_notification = text[2..].join(": ");
            }
            Some("133") => {
                self.prompt_marks += 1;
                self.last_prompt_mark = text.get(1).and_then(|m| m.chars().next());
            }
            _ => {}
        }
    }
}

pub type Parser = vt100::Parser<Signals>;

/// How the main screen's staging scrollback is being watched (history mode).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Arm {
    /// Not watching (between feeds the view offset is always 0).
    Off,
    /// The staging area was empty: every line in it afterwards is new.
    Empty,
    /// The view offset was set to 1: `vt100` moves it up one per pushed line.
    Offset,
}

pub struct Emulator {
    parser: Parser,
    matcher: Matcher,
    in_sync: bool,
    scrollback_len: usize,
    history: Option<History>,
    arm: Arm,
}

impl Emulator {
    /// A plain emulator: `vt100` keeps `scrollback_len` rows of scrollback.
    pub fn new(rows: u16, cols: u16, scrollback_len: usize) -> Self {
        Self {
            parser: vt100::Parser::new_with_callbacks(rows.max(1), cols.max(1), scrollback_len, Signals::default()),
            matcher: Matcher::new(PATTERNS),
            in_sync: false,
            scrollback_len,
            history: None,
            arm: Arm::Off,
        }
    }

    /// A runner pane's emulator: up to `lines` lines of [`History`] instead of
    /// `vt100` scrollback (which is only a small staging area here).
    pub fn with_history(rows: u16, cols: u16, lines: usize) -> Self {
        Self::with_history_bytes(rows, cols, lines, usize::MAX)
    }

    /// [`Emulator::with_history`] whose lines also stay under `max_bytes`.
    pub fn with_history_bytes(rows: u16, cols: u16, lines: usize, max_bytes: usize) -> Self {
        let mut e = Self::new(rows, cols, STAGING);
        e.history = Some(History::with_max_bytes(lines.min(MAX_HISTORY_LINES), max_bytes));
        e
    }

    pub fn history(&self) -> Option<&History> {
        self.history.as_ref()
    }

    /// A fresh emulator of the same size, history and all (`pane.clear`, or
    /// when what was fed may hold a value that is now secret).
    /// Line indices keep counting, so a viewer's old indices never name new lines.
    pub fn reset(&mut self) {
        self.renew(true);
    }

    /// A fresh screen that keeps the history (after a gap in the stream: the
    /// screen state is meaningless, the lines already kept are still true).
    pub fn reset_screen(&mut self) {
        self.renew(false);
    }

    fn renew(&mut self, clear_history: bool) {
        let (rows, cols) = self.size();
        let signals = self.parser.callbacks().clone();
        let history = self.history.take().map(|mut h| {
            if clear_history {
                h.clear();
            } else {
                h.frame_end();
            }
            h
        });
        let sb = if history.is_some() {
            STAGING
        } else {
            self.scrollback_len
        };
        *self = Self::new(rows, cols, sb);
        self.history = history;
        *self.parser.callbacks_mut() = signals;
    }

    /// Feed bytes up to and including the next event, returning how many
    /// bytes were consumed and the event (if any). Call in a loop; the state
    /// of the emulator when an event is returned is exactly the state after
    /// the event's final byte, which is what a snapshot at a sync boundary
    /// or a cursor report needs.
    pub fn feed_until_event(&mut self, bytes: &[u8]) -> (usize, Option<EmuEvent>) {
        let mut done = 0;
        for (i, &b) in bytes.iter().enumerate() {
            if let Some(idx) = self.matcher.push(b) {
                if idx >= ALONE_FROM && self.history.is_some() {
                    // Watch the bytes before the sequence, then apply it unwatched.
                    let start = (i + 1).saturating_sub(PATTERNS[idx].len()).max(done);
                    self.process(&bytes[done..start]);
                    self.parser.process(&bytes[start..=i]);
                    self.arm = Arm::Off;
                    self.parser.screen_mut().set_scrollback(0);
                } else {
                    self.process(&bytes[done..=i]);
                }
                done = i + 1;
                let ev = match idx {
                    0 => {
                        self.in_sync = true;
                        if let Some(h) = self.history.as_mut() {
                            h.frame_start();
                        }
                        EmuEvent::SyncStart
                    }
                    1 => {
                        self.in_sync = false;
                        if let Some(h) = self.history.as_mut() {
                            h.frame_end();
                        }
                        EmuEvent::SyncEnd
                    }
                    2 => EmuEvent::CursorQuery,
                    3 | 4 => {
                        // ED 3 or RIS: the viewer's scrollback is gone, so is ours.
                        if let Some(h) = self.history.as_mut() {
                            h.clear();
                        }
                        continue;
                    }
                    _ => continue,
                };
                return (i + 1, Some(ev));
            }
        }
        self.process(&bytes[done..]);
        (bytes.len(), None)
    }

    /// Bytes into the parser; in history mode in pieces, copying out every
    /// line that scrolled off the main screen right after each piece.
    fn process(&mut self, bytes: &[u8]) {
        if self.history.is_none() {
            self.parser.process(bytes);
            return;
        }
        for piece in bytes.chunks(PIECE) {
            self.arm();
            self.parser.process(piece);
            self.harvest();
        }
    }

    fn arm(&mut self) {
        // On the alternate screen the main screen keeps whatever watch it had.
        if self.arm != Arm::Off || self.parser.screen().alternate_screen() {
            return;
        }
        if snapshot::scrollback_count(&mut self.parser) > 0 {
            self.parser.screen_mut().set_scrollback(1);
            self.arm = Arm::Offset;
        } else {
            self.arm = Arm::Empty;
        }
    }

    fn harvest(&mut self) {
        if self.arm == Arm::Off {
            return;
        }
        if self.parser.screen().alternate_screen() {
            // Entered the alternate screen by a form not split out (e.g. a
            // combined `CSI ? 1049 ; 2004 h`): what scrolled before it cannot
            // be read now.
            self.arm = Arm::Off;
            if let Some(h) = self.history.as_mut() {
                h.note_lost(1);
            }
            return;
        }
        let offset = self.parser.screen().scrollback();
        let len = snapshot::scrollback_count(&mut self.parser);
        let (pushed, saturated) = match self.arm {
            // The watch was lost (an alternate screen round trip not split
            // out): take nothing rather than lines already taken.
            Arm::Offset if offset == 0 => (0, true),
            Arm::Offset => (offset - 1, offset >= len && len >= STAGING),
            _ => (len, len >= STAGING),
        };
        self.arm = Arm::Off;
        let lines = if pushed > 0 {
            snapshot::tail_lines(&mut self.parser, pushed)
        } else {
            vec![]
        };
        self.parser.screen_mut().set_scrollback(0);
        if let Some(h) = self.history.as_mut() {
            if saturated {
                h.note_lost(1);
            }
            for l in lines {
                h.push(l);
            }
        }
    }

    /// Feed everything, ignoring events (sync state is still tracked).
    pub fn feed(&mut self, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            let (n, _) = self.feed_until_event(bytes);
            bytes = &bytes[n..];
        }
    }

    pub fn in_sync(&self) -> bool {
        self.in_sync
    }

    pub fn size(&self) -> (u16, u16) {
        self.parser.screen().size()
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.parser.screen_mut().set_size(rows.max(1), cols.max(1));
    }

    pub fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    pub fn parser_mut(&mut self) -> &mut Parser {
        &mut self.parser
    }

    pub fn signals(&self) -> &Signals {
        self.parser.callbacks()
    }

    /// The detection input `screen`: the last `rows` lines (at least 24; the live screen region).
    pub fn detection_text(&mut self) -> String {
        let rows = self.size().0 as usize;
        self.recent_lines(rows.max(24)).join("\n")
    }

    /// CPR answer for `\e[6n`, from the emulator cursor (1-based).
    pub fn cursor_report(&self) -> Vec<u8> {
        let (r, c) = self.parser.screen().cursor_position();
        format!("\x1b[{};{}R", r as u32 + 1, c as u32 + 1).into_bytes()
    }

    /// ANSI that recreates scrollback + screen + modes in a fresh terminal of the same size.
    pub fn snapshot(&mut self, max_bytes: usize) -> Vec<u8> {
        self.snapshot_with(max_bytes, max_bytes)
    }

    /// [`Emulator::snapshot`] with at most `max_history_bytes` of scrollback.
    /// In history mode the scrollback is the newest history lines that fit,
    /// and the snapshot says which lines it carries ([`snapshot::history_osc`]).
    pub fn snapshot_with(&mut self, max_bytes: usize, max_history_bytes: usize) -> Vec<u8> {
        match self.history.as_ref() {
            Some(h) => snapshot::snapshot_history(&mut self.parser, h, max_bytes, max_history_bytes),
            None => snapshot::snapshot(&mut self.parser, max_bytes),
        }
    }

    /// A page of history before line `before` (history mode only), budgeted
    /// in JSON-encoded bytes (each line plus its CRLF).
    pub fn history_page(&self, before: Option<u64>, max_lines: usize, max_bytes: usize) -> Option<Page> {
        self.history
            .as_ref()
            .map(|h| h.page(before, max_lines, max_bytes, |l| history::json_len(&l.ansi) + 4))
    }

    /// Every history line as ANSI, then the screen's rows, each ending CRLF:
    /// fed into a fresh emulator it rebuilds the history (saved pane history).
    pub fn history_bytes(&mut self, max_bytes: usize) -> Vec<u8> {
        snapshot::history_replay(&mut self.parser, self.history.as_ref(), max_bytes)
    }

    /// The visible screen as plain text (trailing blanks trimmed).
    pub fn visible_text(&self) -> String {
        self.parser.screen().contents()
    }

    /// Up to `n` most recent lines (scrollback + screen), plain text, trailing blank lines dropped.
    pub fn recent_lines(&mut self, n: usize) -> Vec<String> {
        let mut lines = match self.history.as_ref() {
            None => snapshot::all_lines_text(&mut self.parser),
            Some(h) => {
                let screen = snapshot::screen_lines_text(self.parser.screen());
                let skip = h.len().saturating_sub(n.saturating_add(screen.len()));
                let mut out: Vec<String> = h.iter().skip(skip).map(Line::text).collect();
                out.extend(screen);
                out
            }
        };
        while lines.last().is_some_and(|l| l.is_empty()) {
            lines.pop();
        }
        let skip = lines.len().saturating_sub(n);
        lines.split_off(skip)
    }

    /// Up to `n` most recent lines with soft-wrapped rows joined (scrollback or history, then screen).
    pub fn recent_unwrapped(&mut self, n: usize) -> Vec<String> {
        let Some(h) = self.history.as_ref() else {
            let mut all = snapshot::all_lines_unwrapped(&mut self.parser);
            let skip = all.len().saturating_sub(n);
            return all.split_off(skip);
        };
        let screen = self.parser.screen();
        let (rows, cols) = screen.size();
        // Rows newest first: the screen, then history back until n logical
        // lines are complete (a logical line ends at a row that does not wrap).
        let mut newest_first: Vec<(String, bool)> = (0..rows)
            .rev()
            .map(|r| (screen.contents_between(r, 0, r, cols), screen.row_wrapped(r)))
            .collect();
        let mut complete = newest_first.iter().filter(|(_, w)| !*w).count();
        let mut it = h.iter().rev().peekable();
        while let Some(l) = it.peek() {
            if complete > n.saturating_add(rows as usize) && !l.wrapped {
                break;
            }
            if !l.wrapped {
                complete += 1;
            }
            newest_first.push((l.text_untrimmed(), l.wrapped));
            it.next();
        }
        let mut out = vec![];
        let mut cur = String::new();
        for (text, wrapped) in newest_first.into_iter().rev() {
            cur.push_str(&text);
            if !wrapped {
                out.push(std::mem::take(&mut cur).trim_end().to_string());
            }
        }
        if !cur.is_empty() {
            out.push(cur.trim_end().to_string());
        }
        while out.last().is_some_and(|l| l.is_empty()) {
            out.pop();
        }
        let skip = out.len().saturating_sub(n);
        out.split_off(skip)
    }
}

/// Snapshots requested while a synchronized frame is open wait for its end, or
/// for [`SYNC_DEFER`], whichever comes first.
#[derive(Debug, Default)]
pub struct SnapshotGate {
    pending: Vec<(u32, Instant)>,
}

impl SnapshotGate {
    /// A snapshot for `req` is wanted now. Returns true if it may be taken
    /// immediately; otherwise it is queued.
    pub fn request(&mut self, req: u32, now: Instant, in_sync: bool) -> bool {
        if !in_sync {
            return true;
        }
        self.pending.retain(|(r, _)| *r != req);
        self.pending.push((req, now + SYNC_DEFER));
        false
    }

    /// The frame ended: every queued snapshot may be taken now.
    pub fn sync_ended(&mut self) -> Vec<u32> {
        self.pending.drain(..).map(|(r, _)| r).collect()
    }

    /// Queued snapshots whose deadline has passed (taken mid-frame, as a last resort).
    pub fn expired(&mut self, now: Instant) -> Vec<u32> {
        let mut out = vec![];
        self.pending.retain(|(r, deadline)| {
            if *deadline <= now {
                out.push(*r);
                false
            } else {
                true
            }
        });
        out
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        self.pending.iter().map(|(_, d)| *d).min()
    }

    pub fn cancel_all(&mut self) {
        self.pending.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_split_across_chunks() {
        let mut e = Emulator::new(5, 20, 100);
        let stream = b"hi\x1b[?2026hframe\x1b[?20";
        let (n, ev) = e.feed_until_event(stream);
        assert_eq!(ev, Some(EmuEvent::SyncStart));
        assert_eq!(n, 2 + SYNC_ON.len());
        assert!(e.in_sync());
        let (n2, ev2) = e.feed_until_event(&stream[n..]);
        assert_eq!((n2, ev2), (stream.len() - n, None));
        let (n3, ev3) = e.feed_until_event(b"26lrest");
        assert_eq!((n3, ev3), (3, Some(EmuEvent::SyncEnd)));
        assert!(!e.in_sync());
    }

    #[test]
    fn cursor_report_follows_emulator() {
        let mut e = Emulator::new(10, 40, 0);
        e.feed(b"\x1b[3;7Habc");
        let (n, ev) = e.feed_until_event(b"\x1b[6n");
        assert_eq!((n, ev), (4, Some(EmuEvent::CursorQuery)));
        assert_eq!(e.cursor_report(), b"\x1b[3;10R");
    }

    #[test]
    fn gate_defers_until_sync_end_or_deadline() {
        let t0 = Instant::now();
        let mut g = SnapshotGate::default();
        assert!(g.request(1, t0, false));
        assert!(!g.request(2, t0, true));
        assert!(!g.request(3, t0, true));
        assert!(g.expired(t0 + Duration::from_millis(50)).is_empty());
        assert_eq!(g.sync_ended(), vec![2, 3]);
        assert!(g.is_empty());
        assert!(!g.request(4, t0, true));
        assert_eq!(g.next_deadline(), Some(t0 + SYNC_DEFER));
        assert_eq!(g.expired(t0 + SYNC_DEFER), vec![4]);
    }

    #[test]
    fn recent_lines_spans_scrollback() {
        let mut e = Emulator::new(3, 20, 100);
        for i in 0..10 {
            e.feed(format!("line {i}\r\n").as_bytes());
        }
        assert_eq!(e.recent_lines(4), vec!["line 6", "line 7", "line 8", "line 9"]);
        assert_eq!(e.recent_lines(100).len(), 10);
    }
}

#[cfg(test)]
mod unwrap_tests {
    #[test]
    fn unwrapped_joins_soft_wraps() {
        let mut e = super::Emulator::new(4, 10, 100);
        e.feed(b"0123456789abcdef\r\nshort\r\n");
        let lines = super::snapshot::all_lines_unwrapped(e.parser_mut());
        assert_eq!(lines, vec!["0123456789abcdef", "short"]);
        assert_eq!(e.recent_lines(10), vec!["0123456789", "abcdef", "short"]);
    }
}
