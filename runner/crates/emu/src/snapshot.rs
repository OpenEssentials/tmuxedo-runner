//! Snapshot serialisation: ANSI that recreates scrollback + screen + modes.
//!
//! Layout of a snapshot:
//!
//! ```text
//! ESC c                               full reset (clears the viewer's screen and scrollback)
//! [main screen only]
//!   line_0 SGR0 CRLF ... line_{S-1} SGR0 CRLF   scrollback, oldest first, own formatter
//!   LF x (rows-1)                     scrolls exactly S lines into the viewer's scrollback
//! [alternate screen] ESC[?1049h       (the main screen's scrollback is not reachable in vt100)
//! contents_formatted()                clears the screen, draws it, cursor + visibility + SGR
//! input_mode_formatted()              keypad, cursor keys, bracketed paste, mouse modes
//! ```
//!
//! vt100's `rows_formatted` cannot be used for scrollback: for soft-wrapped
//! rows it emits absolute cursor moves based on the row's index in the view,
//! which is wrong once lines are written sequentially. The row formatter
//! below only ever moves the cursor forward within a line.

use vt100::{Callbacks, Color, Parser, Screen};

#[derive(Clone, Copy, PartialEq, Eq, Default)]
struct Attrs {
    fg: Color,
    bg: Color,
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
    inverse: bool,
}

impl Attrs {
    fn of(cell: &vt100::Cell) -> Self {
        Self {
            fg: cell.fgcolor(),
            bg: cell.bgcolor(),
            bold: cell.bold(),
            dim: cell.dim(),
            italic: cell.italic(),
            underline: cell.underline(),
            inverse: cell.inverse(),
        }
    }

    fn write_sgr(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(b"\x1b[0");
        if self.bold {
            out.extend_from_slice(b";1");
        }
        if self.dim {
            out.extend_from_slice(b";2");
        }
        if self.italic {
            out.extend_from_slice(b";3");
        }
        if self.underline {
            out.extend_from_slice(b";4");
        }
        if self.inverse {
            out.extend_from_slice(b";7");
        }
        color(out, self.fg, 38);
        color(out, self.bg, 48);
        out.push(b'm');
    }
}

fn color(out: &mut Vec<u8>, c: Color, base: u8) {
    match c {
        Color::Default => {}
        Color::Idx(i) => out.extend_from_slice(format!(";{base};5;{i}").as_bytes()),
        Color::Rgb(r, g, b) => out.extend_from_slice(format!(";{base};2;{r};{g};{b}").as_bytes()),
    }
}

/// One row of the current view, formatted so that it can be written at
/// column 0 of a blank line with default attributes. Ends with attributes reset.
fn format_row(screen: &Screen, row: u16, cols: u16, out: &mut Vec<u8>) {
    let default = Attrs::default();
    let mut cur = default;
    // Pending run of empty cells: (count, attrs).
    let mut blank: Option<(u16, Attrs)> = None;
    let flush_blank = |out: &mut Vec<u8>, cur: &mut Attrs, run: (u16, Attrs), at_end: bool| {
        let (n, a) = run;
        if a == default {
            if !at_end {
                out.extend_from_slice(format!("\x1b[{n}C").as_bytes());
            }
            return;
        }
        if a != *cur {
            a.write_sgr(out);
            *cur = a;
        }
        out.extend_from_slice(format!("\x1b[{n}X").as_bytes());
        if !at_end {
            out.extend_from_slice(format!("\x1b[{n}C").as_bytes());
        }
    };
    for col in 0..cols {
        let Some(cell) = screen.cell(row, col) else {
            break;
        };
        if cell.is_wide_continuation() {
            continue;
        }
        let a = Attrs::of(cell);
        if cell.has_contents() {
            if let Some(run) = blank.take() {
                flush_blank(out, &mut cur, run, false);
            }
            if a != cur {
                a.write_sgr(out);
                cur = a;
            }
            out.extend_from_slice(cell.contents().as_bytes());
        } else {
            blank = match blank {
                Some((n, ba)) if ba == a => Some((n + 1, ba)),
                Some(run) => {
                    flush_blank(out, &mut cur, run, false);
                    Some((1, a))
                }
                None => Some((1, a)),
            };
        }
    }
    if let Some(run) = blank.take() {
        flush_blank(out, &mut cur, run, true);
    }
    if cur != default {
        out.extend_from_slice(b"\x1b[m");
    }
}

/// Number of scrollback lines currently held.
pub fn scrollback_count<CB: Callbacks>(parser: &mut Parser<CB>) -> usize {
    let screen = parser.screen_mut();
    let prev = screen.scrollback();
    screen.set_scrollback(usize::MAX);
    let n = screen.scrollback();
    screen.set_scrollback(prev);
    n
}

/// Visit every scrollback line (oldest first) with the view positioned so
/// that it is row `r` of the screen.
fn for_each_scrollback_row<CB: Callbacks>(parser: &mut Parser<CB>, mut f: impl FnMut(&Screen, u16)) {
    let total = scrollback_count(parser);
    let (rows, _) = parser.screen().size();
    let mut k = 0usize;
    while k < total {
        parser.screen_mut().set_scrollback(total - k);
        let n = (total - k).min(rows as usize);
        for r in 0..n {
            f(parser.screen(), r as u16);
        }
        k += n;
    }
    parser.screen_mut().set_scrollback(0);
}

pub fn snapshot<CB: Callbacks>(parser: &mut Parser<CB>, max_bytes: usize) -> Vec<u8> {
    parser.screen_mut().set_scrollback(0);
    let (rows, cols) = parser.screen().size();
    let alt = parser.screen().alternate_screen();
    let mut tail = vec![];
    if alt {
        tail.extend_from_slice(b"\x1b[?1049h");
    }
    tail.extend_from_slice(&parser.screen().contents_formatted());
    tail.extend_from_slice(&parser.screen().input_mode_formatted());

    let mut lines: Vec<Vec<u8>> = vec![];
    if !alt {
        for_each_scrollback_row(parser, |screen, r| {
            let mut l = vec![];
            format_row(screen, r, cols, &mut l);
            l.extend_from_slice(b"\r\n");
            lines.push(l);
        });
    }
    // Keep the newest lines that fit.
    let fixed = 2 + tail.len() + rows as usize;
    let mut budget = max_bytes.saturating_sub(fixed);
    let mut keep_from = lines.len();
    while keep_from > 0 && lines[keep_from - 1].len() <= budget {
        budget -= lines[keep_from - 1].len();
        keep_from -= 1;
    }
    let kept = &lines[keep_from..];

    let mut out = Vec::with_capacity(max_bytes.min(fixed + kept.iter().map(Vec::len).sum::<usize>()));
    out.extend_from_slice(b"\x1bc");
    if !kept.is_empty() {
        for l in kept {
            out.extend_from_slice(l);
        }
        out.extend(std::iter::repeat_n(b'\n', rows.saturating_sub(1) as usize));
    }
    out.extend_from_slice(&tail);
    out
}

/// The last `n` scrollback lines (oldest first) as history lines.
pub fn tail_lines<CB: Callbacks>(parser: &mut Parser<CB>, n: usize) -> Vec<crate::Line> {
    let total = scrollback_count(parser);
    let n = n.min(total);
    let (rows, cols) = parser.screen().size();
    let mut out = Vec::with_capacity(n);
    let mut k = n;
    while k > 0 {
        parser.screen_mut().set_scrollback(k);
        let m = k.min(rows as usize);
        let screen = parser.screen();
        // vt100's own row formatter walks the cells directly (five times faster
        // than a cell() lookup per column, measured on a log flood). A row it
        // formats as the continuation of a soft-wrapped row may move the cursor
        // across rows, so those few rows use the column-by-column formatter.
        for (r, fast) in (0..m as u16).zip(screen.rows_formatted(0, cols)) {
            let wrapped = screen.row_wrapped(r);
            if r > 0 && screen.row_wrapped(r - 1) {
                let mut l = vec![];
                format_row(screen, r, cols, &mut l);
                out.push(crate::Line::new(l, wrapped));
            } else {
                let mut l = fast;
                if l.contains(&0x1b) {
                    l.extend_from_slice(b"\x1b[m");
                }
                out.push(crate::Line::new(l, wrapped));
            }
        }
        k -= m;
    }
    parser.screen_mut().set_scrollback(0);
    out
}

/// The screen's rows as plain text, trailing blanks trimmed.
pub fn screen_lines_text(screen: &Screen) -> Vec<String> {
    let (rows, cols) = screen.size();
    (0..rows)
        .map(|r| screen.contents_between(r, 0, r, cols).trim_end().to_string())
        .collect()
}

/// The OSC a history snapshot starts with (after `ESC c`): which history
/// lines it carries. Viewers that do not know it ignore it (an unknown OSC).
/// `first..end` are the lines written as scrollback; `start` is the oldest
/// line the runner still has (older pages: `pane.read` source `history`).
pub fn history_osc(start: u64, first: u64, end: u64, limit: usize) -> Vec<u8> {
    format!("\x1b]777;tmuxedo-history;start={start};first={first};end={end};limit={limit}\x07").into_bytes()
}

/// A snapshot whose scrollback comes from `history` (see the module docs;
/// the history OSC follows `ESC c`). The alternate screen gets the history
/// too: it is the main screen's past, there when the program leaves.
pub fn snapshot_history<CB: Callbacks>(
    parser: &mut Parser<CB>,
    history: &crate::History,
    max_bytes: usize,
    max_history_bytes: usize,
) -> Vec<u8> {
    parser.screen_mut().set_scrollback(0);
    let (rows, _) = parser.screen().size();
    let mut tail = vec![];
    if parser.screen().alternate_screen() {
        tail.extend_from_slice(b"\x1b[?1049h");
    }
    tail.extend_from_slice(&parser.screen().contents_formatted());
    tail.extend_from_slice(&parser.screen().input_mode_formatted());
    let osc_room = 120;
    let fixed = 2 + osc_room + tail.len() + rows as usize;
    let budget = max_bytes.saturating_sub(fixed).min(max_history_bytes);
    let page = history.page(None, usize::MAX, budget, |l| l.ansi.len() + 2);
    let mut out = Vec::with_capacity(fixed + budget.min(1 << 20));
    out.extend_from_slice(b"\x1bc");
    out.extend_from_slice(&history_osc(page.start, page.first, page.end, history.limit()));
    if !page.lines.is_empty() {
        for l in &page.lines {
            out.extend_from_slice(&l.ansi);
            out.extend_from_slice(b"\r\n");
        }
        out.extend(std::iter::repeat_n(b'\n', rows.saturating_sub(1) as usize));
    }
    out.extend_from_slice(&tail);
    out
}

/// History lines then the main screen's rows (trailing blank rows dropped),
/// each ending CRLF, newest `max_bytes` kept: fed into a fresh history
/// emulator it rebuilds the history (opt-in saved pane history).
pub fn history_replay<CB: Callbacks>(
    parser: &mut Parser<CB>,
    history: Option<&crate::History>,
    max_bytes: usize,
) -> Vec<u8> {
    parser.screen_mut().set_scrollback(0);
    let mut lines: Vec<Vec<u8>> = vec![];
    if !parser.screen().alternate_screen() {
        let (rows, cols) = parser.screen().size();
        for r in 0..rows {
            let mut l = vec![];
            format_row(parser.screen(), r, cols, &mut l);
            lines.push(l);
        }
        while lines.last().is_some_and(|l| l.is_empty()) {
            lines.pop();
        }
    }
    let mut used: usize = lines.iter().map(|l| l.len() + 2).sum();
    let mut older: Vec<&[u8]> = vec![];
    if let Some(h) = history {
        for l in h.iter().rev() {
            if used + l.ansi.len() + 2 > max_bytes {
                break;
            }
            used += l.ansi.len() + 2;
            older.push(&l.ansi);
        }
    }
    let mut out = Vec::with_capacity(used);
    for l in older.iter().rev() {
        out.extend_from_slice(l);
        out.extend_from_slice(b"\r\n");
    }
    for l in &lines {
        out.extend_from_slice(l);
        out.extend_from_slice(b"\r\n");
    }
    out
}

/// Every line, scrollback then screen, as plain text with trailing blanks trimmed.
pub fn all_lines_text<CB: Callbacks>(parser: &mut Parser<CB>) -> Vec<String> {
    let (_, cols) = parser.screen().size();
    let mut out = vec![];
    for_each_scrollback_row(parser, |screen, r| {
        out.push(screen.contents_between(r, 0, r, cols).trim_end().to_string());
    });
    let screen = parser.screen();
    let (rows, _) = screen.size();
    for r in 0..rows {
        out.push(screen.contents_between(r, 0, r, cols).trim_end().to_string());
    }
    out
}

/// Like [`all_lines_text`], but soft-wrapped rows are joined into one line.
pub fn all_lines_unwrapped<CB: Callbacks>(parser: &mut Parser<CB>) -> Vec<String> {
    let (_, cols) = parser.screen().size();
    let mut rows: Vec<(String, bool)> = vec![];
    for_each_scrollback_row(parser, |screen, r| {
        rows.push((screen.contents_between(r, 0, r, cols), screen.row_wrapped(r)));
    });
    let screen = parser.screen();
    let (n, _) = screen.size();
    for r in 0..n {
        rows.push((screen.contents_between(r, 0, r, cols), screen.row_wrapped(r)));
    }
    let mut out = vec![];
    let mut cur = String::new();
    for (text, wrapped) in rows {
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
    out
}
