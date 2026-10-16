//! Stops the local terminal's automatic replies from reaching a remote pane.
//!
//! The holder already answers terminal queries on the machine (decision
//! 0001), so when `tmuxedo attach` writes a program's `\e[c` to the local
//! terminal, the local terminal's own reply must not be typed into the pane
//! as well. Replies are recognisable: keyboards never produce DA/DECRPM/
//! kitty-flag/window-report CSIs, OSC or DCS strings. The one ambiguous
//! reply, a cursor position report (`\e[r;cR`, which looks like a modified
//! F3), is dropped only while a `\e[6n` seen in the output is unanswered.

use tmuxedo_emu::Matcher;

const DSR: &[&[u8]] = &[b"\x1b[6n"];

pub struct ReplyFilter {
    dsr_pending: u32,
    carry: Vec<u8>,
    out: Matcher,
}

impl Default for ReplyFilter {
    fn default() -> Self {
        Self {
            dsr_pending: 0,
            carry: vec![],
            out: Matcher::new(DSR),
        }
    }
}

enum Seq {
    Keep(usize),
    Drop(usize),
    Incomplete,
}

impl ReplyFilter {
    /// Feed pane output (what the local terminal is about to see).
    pub fn observe_output(&mut self, bytes: &[u8]) {
        for _ in self.out.scan(bytes) {
            self.dsr_pending = (self.dsr_pending + 1).min(16);
        }
    }

    pub fn has_carry(&self) -> bool {
        !self.carry.is_empty()
    }

    /// A lone ESC (or other incomplete prefix) that nothing completed: it was a key.
    pub fn flush(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.carry)
    }

    fn classify(&mut self, b: &[u8]) -> Seq {
        // b[0] == ESC
        let Some(&k) = b.get(1) else { return Seq::Incomplete };
        match k {
            b'[' => {
                let mut i = 2;
                while i < b.len() && (0x30..=0x3f).contains(&b[i]) {
                    i += 1;
                }
                let params = &b[2..i];
                let inter_start = i;
                while i < b.len() && (0x20..=0x2f).contains(&b[i]) {
                    i += 1;
                }
                let Some(&fin) = b.get(i) else { return Seq::Incomplete };
                if !(0x40..=0x7e).contains(&fin) {
                    return Seq::Keep(i);
                }
                let len = i + 1;
                let inter = &b[inter_start..i];
                let private = params.first().copied().filter(|c| b"?<=>".contains(c));
                let reply = match fin {
                    b'c' => matches!(private, Some(b'?') | Some(b'>') | Some(b'=')),
                    b'u' => private == Some(b'?'),
                    b'y' => inter == b"$",
                    b't' => private.is_none() && !params.is_empty(),
                    b'n' => private.is_none() || private == Some(b'?'),
                    b'R' if private == Some(b'?') => true,
                    b'R' if self.dsr_pending > 0 && params.contains(&b';') => {
                        self.dsr_pending -= 1;
                        true
                    }
                    _ => false,
                };
                if reply {
                    Seq::Drop(len)
                } else {
                    Seq::Keep(len)
                }
            }
            b']' | b'P' | b'_' | b'^' => {
                // String: ends with BEL (OSC) or ST.
                let mut i = 2;
                while i < b.len() {
                    if b[i] == 0x07 && k == b']' {
                        return Seq::Drop(i + 1);
                    }
                    if b[i] == 0x1b {
                        match b.get(i + 1) {
                            Some(b'\\') => return Seq::Drop(i + 2),
                            None => return Seq::Incomplete,
                            Some(_) => {}
                        }
                    }
                    i += 1;
                }
                Seq::Incomplete
            }
            _ => Seq::Keep(1),
        }
    }

    /// Filter local input; may hold an incomplete escape sequence back.
    pub fn filter_input(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut buf = std::mem::take(&mut self.carry);
        buf.extend_from_slice(bytes);
        let mut out = Vec::with_capacity(buf.len());
        let mut i = 0;
        while i < buf.len() {
            if buf[i] != 0x1b {
                out.push(buf[i]);
                i += 1;
                continue;
            }
            match self.classify(&buf[i..]) {
                Seq::Keep(n) => {
                    out.extend_from_slice(&buf[i..i + n]);
                    i += n;
                }
                Seq::Drop(n) => i += n,
                Seq::Incomplete => {
                    self.carry = buf[i..].to_vec();
                    break;
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_terminal_replies_keeps_keys() {
        let mut f = ReplyFilter::default();
        assert_eq!(f.filter_input(b"ls\x1b[?62;22c -l\r"), b"ls -l\r");
        assert_eq!(f.filter_input(b"\x1b[>1;10;0c\x1b[?0u\x1b[?2026;2$y\x1b[6;20;10t"), b"");
        assert_eq!(f.filter_input(b"\x1b]11;rgb:1111/1111/1111\x1b\\x"), b"x");
        assert_eq!(f.filter_input(b"\x1b]10;rgb:dd/dd/dd\x07\x1bP>|xterm\x1b\\y"), b"y");
        // Arrow keys, alt-keys and F3 pass.
        assert_eq!(f.filter_input(b"\x1b[A\x1bx\x1b[1;2R"), b"\x1b[A\x1bx\x1b[1;2R");
        // A CPR is dropped only after a DSR in the output.
        f.observe_output(b"\x1b[6n");
        assert_eq!(f.filter_input(b"\x1b[12;40Rz"), b"z");
        assert_eq!(f.filter_input(b"\x1b[12;40R"), b"\x1b[12;40R");
        // Split replies are held until complete.
        assert_eq!(f.filter_input(b"a\x1b[?6"), b"a");
        assert!(f.has_carry());
        assert_eq!(f.filter_input(b"2;22cb"), b"b");
        // A lone ESC key is released by flush.
        assert_eq!(f.filter_input(b"\x1b"), b"");
        assert_eq!(f.flush(), b"\x1b");
    }
}
