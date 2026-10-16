//! Glance encoder (docs/protocol.md "Glances"): a pane's VISIBLE screen as
//! styled text runs, for dashboard thumbnails.
//!
//! Built from the pane's emulator (the same one SNAPSHOT and `pane.read`
//! read, fed from the runner's single read of the holder, where the output
//! masker sits), at scrollback offset 0: scrollback never enters a glance.
//!
//! Wire: one array of runs `[text, fg?, bg?, attrs?]` per row, trailing
//! default values left out; colours `null` / palette index / `"#rrggbb"`;
//! attrs bit 0 bold, 1 dim, 2 italic, 3 underline, 4 inverse. Trailing blank
//! cells are dropped; a wide character's continuation cell is skipped.
//! Panes larger than [`MAX_COLS`] x [`MAX_ROWS`] are cropped to the bottom
//! rows and left columns. Golden fixtures: packages/protocol/fixtures/glance.json.

use serde_json::{json, Value};
use std::hash::{Hash, Hasher};
use tmuxedo_emu::Emulator;
use tmuxedo_protocol::frames::Cursor;
use vt100::Color;

pub const MAX_ROWS: u16 = 60;
pub const MAX_COLS: u16 = 200;

/// One glance, before it is numbered and sent.
#[derive(Debug, Clone, PartialEq)]
pub struct Glance {
    pub cols: u16,
    pub rows: u16,
    pub cursor: Option<Cursor>,
    pub lines: Value,
    /// Change detection: a hash of everything above.
    pub hash: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Style {
    fg: Color,
    bg: Color,
    attrs: u8,
}

impl Style {
    fn of(c: &vt100::Cell) -> Self {
        let attrs = (c.bold() as u8)
            | (c.dim() as u8) << 1
            | (c.italic() as u8) << 2
            | (c.underline() as u8) << 3
            | (c.inverse() as u8) << 4;
        Self {
            fg: c.fgcolor(),
            bg: c.bgcolor(),
            attrs,
        }
    }
    /// A blank cell in this style draws nothing (it may be trimmed at a line end).
    fn blank_invisible(&self) -> bool {
        self.bg == Color::Default && self.attrs & (8 | 16) == 0
    }
}

fn color(c: Color) -> Value {
    match c {
        Color::Default => Value::Null,
        Color::Idx(i) => json!(i),
        Color::Rgb(r, g, b) => json!(format!("#{r:02x}{g:02x}{b:02x}")),
    }
}

fn run(text: String, s: Style) -> Value {
    let mut v = vec![json!(text), color(s.fg), color(s.bg), json!(s.attrs)];
    while v.len() > 1 {
        let last = &v[v.len() - 1];
        if last.is_null() || last == &json!(0) {
            v.pop();
        } else {
            break;
        }
    }
    Value::Array(v)
}

/// Encode the visible screen of `emu` as a glance.
pub fn encode(emu: &mut Emulator) -> Glance {
    emu.parser_mut().screen_mut().set_scrollback(0);
    let screen = emu.screen();
    let (prows, pcols) = screen.size();
    let rows = prows.min(MAX_ROWS);
    let cols = pcols.min(MAX_COLS);
    let top = prows - rows;
    let mut lines = Vec::with_capacity(rows as usize);
    for r in top..prows {
        // (text, style, blank) per cell, wide continuations skipped.
        let mut cells: Vec<(&str, Style, bool)> = Vec::with_capacity(cols as usize);
        for c in 0..cols {
            let Some(cell) = screen.cell(r, c) else { break };
            if cell.is_wide_continuation() {
                continue;
            }
            let s = Style::of(cell);
            let text = cell.contents();
            let blank = text.is_empty() || text == " ";
            cells.push((if text.is_empty() { " " } else { text }, s, blank));
        }
        while cells.last().is_some_and(|(_, s, blank)| *blank && s.blank_invisible()) {
            cells.pop();
        }
        let mut runs = vec![];
        let mut cur: Option<(String, Style)> = None;
        for (text, s, _) in cells {
            match &mut cur {
                Some((t, cs)) if *cs == s => t.push_str(text),
                _ => {
                    if let Some((t, cs)) = cur.take() {
                        runs.push(run(t, cs));
                    }
                    cur = Some((text.to_string(), s));
                }
            }
        }
        if let Some((t, cs)) = cur {
            runs.push(run(t, cs));
        }
        lines.push(Value::Array(runs));
    }
    let cursor = if screen.hide_cursor() {
        None
    } else {
        let (cr, cc) = screen.cursor_position();
        (cr >= top && cc < cols).then(|| Cursor { x: cc, y: cr - top })
    };
    let lines = Value::Array(lines);
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (cols, rows, cursor.map(|c| (c.x, c.y))).hash(&mut h);
    lines.to_string().hash(&mut h);
    Glance {
        cols,
        rows,
        cursor,
        lines,
        hash: h.finish(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixtures() -> Value {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../packages/protocol/fixtures/glance.json"
        );
        let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
        serde_json::from_str(&text).expect("fixtures are JSON")
    }

    #[test]
    fn golden_fixtures_encode_exactly() {
        let f = fixtures();
        let cases = f["cases"].as_array().expect("cases[]");
        assert!(cases.len() >= 7);
        for c in cases {
            let name = c["name"].as_str().unwrap();
            let cols = c["cols"].as_u64().unwrap() as u16;
            let rows = c["rows"].as_u64().unwrap() as u16;
            let mut emu = Emulator::new(rows, cols, 100);
            emu.feed(c["input"].as_str().unwrap().as_bytes());
            let g = encode(&mut emu);
            let mut got = json!({ "cols": g.cols, "rows": g.rows, "lines": g.lines });
            if let Some(cur) = g.cursor {
                got["cursor"] = json!({ "x": cur.x, "y": cur.y });
            }
            assert_eq!(got, c["frame"], "{name}");
        }
    }

    #[test]
    fn scrollback_never_enters_even_when_scrolled_back() {
        let mut emu = Emulator::new(2, 10, 100);
        emu.feed(b"secret-old\r\ntwo\r\nthree");
        emu.parser_mut().screen_mut().set_scrollback(1);
        let g = encode(&mut emu);
        assert!(!g.lines.to_string().contains("secret"), "{}", g.lines);
        assert_eq!(g.lines, json!([[["two"]], [["three"]]]));
    }

    #[test]
    fn hash_follows_changes_only() {
        let mut emu = Emulator::new(3, 20, 10);
        emu.feed(b"abc");
        let a = encode(&mut emu);
        assert_eq!(encode(&mut emu).hash, a.hash);
        emu.feed(b"\x1b[1D"); // cursor moves: a change
        let b = encode(&mut emu);
        assert_ne!(b.hash, a.hash);
        emu.feed(b"\x1b[31m");
        assert_eq!(encode(&mut emu).hash, b.hash, "a pen change draws nothing");
        emu.feed(b"x");
        assert_ne!(encode(&mut emu).hash, b.hash);
    }

    /// A busy coloured 120x40 screen: the size behind the bytes/s bound in the design doc.
    #[test]
    fn busy_screen_size() {
        let mut emu = Emulator::new(40, 120, 100);
        for i in 0..200 {
            emu.feed(
                format!(
                    "-rwxr-xr-x 1 root root {:>8} Oct  3 12:{:02} \x1b[1;32mbinary-{i:04}\x1b[0m -> \x1b[36m/usr/lib/x86_64-linux-gnu/thing-{i}\x1b[0m\r\n",
                    i * 4099,
                    i % 60
                )
                .as_bytes(),
            );
        }
        let g = encode(&mut emu);
        let frame = tmuxedo_protocol::frames::RunnerFrame::Glance {
            pane_id: "w1:p1".into(),
            rev: 1,
            cols: g.cols,
            rows: g.rows,
            cursor: g.cursor,
            lines: g.lines,
        }
        .to_json();
        eprintln!("busy 120x40 glance: {} bytes", frame.len());
        assert!(frame.len() < 16 * 1024, "{}", frame.len());
    }
}
