//! A pane's long history: the lines that scrolled off the top of the main
//! screen, kept as compact formatted lines (decision 0009).
//!
//! `vt100` keeps scrollback as full rows of 32-byte cells: 50,000 lines of
//! 120 columns would be 192 MB per pane. Here a line is the ANSI that draws
//! it (the snapshot row formatter's output: text, SGR, cursor-forward over
//! blank runs), typically 40–200 bytes, so the same history fits in a few MB.
//!
//! Every line has an absolute index that only grows: `start()` is the index
//! of the oldest line kept, `end()` one past the newest. Clients page through
//! older lines by index (`pane.read` source `history`), so the pages stay
//! consistent while new output arrives.
//!
//! **Redrawn frames.** A program that redraws a frame taller than the screen
//! inside a synchronized update (`?2026`) scrolls the same top lines off again
//! on every frame. Lines that scroll off inside a frame are held until the
//! frame ends; if the previous frame's lines are a prefix of them, that prefix
//! is the same content redrawn and is dropped. Outside synchronized frames
//! every line is kept: a program printing the same line twice printed it twice.

use std::collections::VecDeque;

/// One formatted line: ANSI that draws it from column 0 of a blank line with
/// default attributes, ending with attributes reset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    pub ansi: Box<[u8]>,
    /// The terminal soft-wrapped this row into the next one.
    pub wrapped: bool,
}

impl Line {
    pub fn new(ansi: Vec<u8>, wrapped: bool) -> Self {
        Self {
            ansi: ansi.into_boxed_slice(),
            wrapped,
        }
    }

    /// The line's plain text (trailing blanks trimmed). The formatter emits
    /// only SGR, `CSI n C` (move right over blanks) and `CSI n X` (erase
    /// blanks in place), so this is exact.
    pub fn text(&self) -> String {
        plain_text(&self.ansi)
    }

    /// [`Line::text`] without trimming (for joining soft-wrapped rows).
    pub fn text_untrimmed(&self) -> String {
        plain_untrimmed(&self.ansi)
    }

    /// Heap bytes this line costs (payload plus the allocation header estimate).
    pub fn cost(&self) -> usize {
        self.ansi.len() + std::mem::size_of::<Line>() + 16
    }
}

/// Plain text of formatter output (see [`Line::text`]).
pub fn plain_text(ansi: &[u8]) -> String {
    plain_untrimmed(ansi).trim_end().to_string()
}

fn plain_untrimmed(ansi: &[u8]) -> String {
    let mut out: Vec<u8> = Vec::with_capacity(ansi.len());
    let mut i = 0;
    while i < ansi.len() {
        if ansi[i] == 0x1b && ansi.get(i + 1) == Some(&b'[') {
            let mut j = i + 2;
            while j < ansi.len() && !(0x40..=0x7e).contains(&ansi[j]) {
                j += 1;
            }
            if j < ansi.len() && ansi[j] == b'C' {
                let n: usize = std::str::from_utf8(&ansi[i + 2..j])
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(1);
                out.extend(std::iter::repeat_n(b' ', n.min(4096)));
            }
            i = j + 1;
        } else {
            out.push(ansi[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Size of `bytes` as a JSON string body (what a `pane.read` reply costs).
pub fn json_len(bytes: &[u8]) -> usize {
    bytes
        .iter()
        .map(|&b| match b {
            b'"' | b'\\' | b'\n' | b'\r' | b'\t' => 2,
            0..=0x1f | 0x7f => 6,
            _ => 1,
        })
        .sum()
}

/// A page of history: lines `[first, first + lines.len())`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Page {
    /// Index of the first line returned.
    pub first: u64,
    /// The oldest index the history still holds.
    pub start: u64,
    /// One past the newest index.
    pub end: u64,
    pub lines: Vec<Line>,
    /// Older lines exist before `first`.
    pub more: bool,
}

pub struct History {
    lines: VecDeque<Line>,
    start: u64,
    limit: usize,
    /// Upper bound on [`History::heap_bytes`]'s line part: very wide, very
    /// colourful lines hit this before the line limit.
    max_bytes: usize,
    bytes: usize,
    /// Lines that scrolled off inside the current synchronized frame.
    frame: Vec<Line>,
    in_frame: bool,
    /// The previous frame's lines (before dropping), compared with the next frame's.
    prev_frame: Vec<Line>,
    /// Lines dropped as redraws (for tests and the log).
    pub redraws_dropped: u64,
    /// Lines lost because more scrolled at once than could be read (pathological input).
    pub lost: u64,
}

/// Lines held for one synchronized frame before it is committed regardless.
const FRAME_HOLD_MAX: usize = 4096;

impl History {
    pub fn new(limit: usize) -> Self {
        Self::with_max_bytes(limit, usize::MAX)
    }

    pub fn with_max_bytes(limit: usize, max_bytes: usize) -> Self {
        Self {
            lines: VecDeque::new(),
            start: 0,
            limit,
            max_bytes,
            bytes: 0,
            frame: vec![],
            in_frame: false,
            prev_frame: vec![],
            redraws_dropped: 0,
            lost: 0,
        }
    }

    pub fn limit(&self) -> usize {
        self.limit
    }

    pub fn max_bytes(&self) -> usize {
        self.max_bytes
    }

    pub fn start(&self) -> u64 {
        self.start
    }

    /// One past the newest committed line.
    pub fn end(&self) -> u64 {
        self.start + self.lines.len() as u64
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// Approximate heap bytes held (lines plus the deque's slots).
    pub fn heap_bytes(&self) -> usize {
        self.bytes
            + self.lines.capacity() * std::mem::size_of::<Line>()
            + self.frame.iter().chain(&self.prev_frame).map(Line::cost).sum::<usize>()
    }

    /// The program started a synchronized frame.
    pub fn frame_start(&mut self) {
        if self.in_frame {
            return;
        }
        self.in_frame = true;
        self.frame.clear();
    }

    /// The frame ended: commit its lines, minus a redraw of the previous frame's.
    pub fn frame_end(&mut self) {
        if !self.in_frame {
            return;
        }
        self.in_frame = false;
        let lines = std::mem::take(&mut self.frame);
        if lines.is_empty() {
            return;
        }
        let skip = if !self.prev_frame.is_empty() && lines.starts_with(&self.prev_frame) {
            self.prev_frame.len()
        } else {
            0
        };
        self.redraws_dropped += skip as u64;
        for l in &lines[skip..] {
            self.commit(l.clone());
        }
        self.prev_frame = lines;
    }

    /// A line scrolled off the top of the main screen.
    pub fn push(&mut self, line: Line) {
        if self.in_frame {
            self.frame.push(line);
            if self.frame.len() >= FRAME_HOLD_MAX {
                // Not a redraw of anything small: stop holding.
                self.in_frame = false;
                self.prev_frame.clear();
                for l in std::mem::take(&mut self.frame) {
                    self.commit(l);
                }
            }
            return;
        }
        self.prev_frame.clear();
        self.commit(line);
    }

    fn commit(&mut self, line: Line) {
        if self.limit == 0 {
            self.start += 1;
            return;
        }
        self.bytes += line.cost();
        self.lines.push_back(line);
        while self.lines.len() > self.limit || (self.bytes > self.max_bytes && self.lines.len() > 1) {
            if let Some(old) = self.lines.pop_front() {
                self.bytes -= old.cost();
            }
            self.start += 1;
        }
    }

    /// Clear every line (`CSI 3 J`, `ESC c`): indices keep counting.
    pub fn clear(&mut self) {
        self.start = self.end() + self.frame.len() as u64;
        self.lines.clear();
        self.lines.shrink_to_fit();
        self.frame.clear();
        self.prev_frame.clear();
        self.bytes = 0;
    }

    /// Record lines lost to a scroll burst larger than the emulator's staging area.
    pub fn note_lost(&mut self, n: u64) {
        self.lost += n;
    }

    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &Line> + ExactSizeIterator {
        self.lines.iter()
    }

    pub fn get(&self, index: u64) -> Option<&Line> {
        index.checked_sub(self.start).and_then(|i| self.lines.get(i as usize))
    }

    /// The newest lines before `before` (default: the end), at most `max_lines`
    /// and at most `max_bytes` measured by `cost` (e.g. [`json_len`] + 2 for CRLF).
    pub fn page(&self, before: Option<u64>, max_lines: usize, max_bytes: usize, cost: impl Fn(&Line) -> usize) -> Page {
        let end = self.end();
        let before = before.unwrap_or(end).clamp(self.start, end);
        let mut first = before;
        let mut used = 0usize;
        while first > self.start && (before - first) < max_lines as u64 {
            let l = self.get(first - 1).expect("index in range");
            let c = cost(l);
            if used + c > max_bytes {
                // A single line larger than the budget is still sent, alone.
                if first == before {
                    first -= 1;
                }
                break;
            }
            used += c;
            first -= 1;
        }
        let lines = (first..before).filter_map(|i| self.get(i).cloned()).collect();
        Page {
            first,
            start: self.start,
            end,
            lines,
            more: first > self.start,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn l(s: &str) -> Line {
        Line::new(s.as_bytes().to_vec(), false)
    }

    #[test]
    fn indices_grow_and_the_limit_drops_the_oldest() {
        let mut h = History::new(3);
        for i in 0..5 {
            h.push(l(&format!("line {i}")));
        }
        assert_eq!((h.start(), h.end(), h.len()), (2, 5, 3));
        assert_eq!(h.get(2).unwrap().text(), "line 2");
        assert!(h.get(1).is_none());
        assert!(h.heap_bytes() > 0);
    }

    #[test]
    fn pages_walk_back_by_index_and_respect_budgets() {
        let mut h = History::new(100);
        for i in 0..10 {
            h.push(l(&format!("line {i}")));
        }
        let p = h.page(None, 4, usize::MAX, |x| x.ansi.len());
        assert_eq!((p.first, p.end, p.more), (6, 10, true));
        assert_eq!(
            p.lines.iter().map(Line::text).collect::<Vec<_>>(),
            ["line 6", "line 7", "line 8", "line 9"]
        );
        let p2 = h.page(Some(p.first), 100, usize::MAX, |x| x.ansi.len());
        assert_eq!((p2.first, p2.lines.len(), p2.more), (0, 6, false));
        // A byte budget of two lines.
        let p3 = h.page(Some(5), 100, 12, |x| x.ansi.len());
        assert_eq!((p3.first, p3.lines.len()), (3, 2));
        // One oversized line is still sent, alone.
        let p4 = h.page(Some(5), 100, 1, |x| x.ansi.len());
        assert_eq!((p4.first, p4.lines.len()), (4, 1));
        // Out of range clamps.
        assert_eq!(h.page(Some(99), 1, 99, |_| 1).first, 9);
        assert!(h.page(Some(0), 5, 99, |_| 1).lines.is_empty());
    }

    #[test]
    fn a_frame_that_redraws_the_previous_frames_lines_adds_only_new_ones() {
        let mut h = History::new(100);
        h.push(l("before"));
        // Frame 1 scrolls A, B off; frame 2 redraws A, B and adds C; frame 3 redraws A, B, C.
        for frame in [&["A", "B"][..], &["A", "B", "C"], &["A", "B", "C"]] {
            h.frame_start();
            for x in frame {
                h.push(l(x));
            }
            h.frame_end();
        }
        let all: Vec<String> = h.iter().map(Line::text).collect();
        assert_eq!(all, ["before", "A", "B", "C"]);
        assert_eq!(h.redraws_dropped, 5);
    }

    #[test]
    fn output_between_frames_and_different_frames_are_kept() {
        let mut h = History::new(100);
        h.frame_start();
        h.push(l("A"));
        h.frame_end();
        h.push(l("A")); // printed outside a frame: real output
        h.frame_start();
        h.push(l("A")); // previous frame was interrupted by real output: kept
        h.frame_end();
        h.frame_start();
        h.push(l("X")); // a different frame
        h.frame_end();
        let all: Vec<String> = h.iter().map(Line::text).collect();
        assert_eq!(all, ["A", "A", "A", "X"]);
    }

    #[test]
    fn clear_keeps_counting_and_a_huge_frame_is_not_held() {
        let mut h = History::new(10_000);
        h.push(l("a"));
        h.clear();
        assert_eq!((h.start(), h.end()), (1, 1));
        h.frame_start();
        for i in 0..FRAME_HOLD_MAX {
            h.push(l(&i.to_string()));
        }
        assert_eq!(h.len(), FRAME_HOLD_MAX);
        h.frame_end();
        assert_eq!(h.len(), FRAME_HOLD_MAX);
    }

    #[test]
    fn plain_text_expands_blank_runs_and_skips_sgr() {
        assert_eq!(
            plain_text(b"\x1b[0;1mab\x1b[m\x1b[3Ccd\x1b[0;41m\x1b[2X\x1b[m"),
            "ab   cd"
        );
        assert_eq!(json_len(b"a\x1b\"\r\n"), 1 + 6 + 2 + 2 + 2);
    }
}
