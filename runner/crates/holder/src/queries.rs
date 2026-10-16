//! Stateless terminal queries the holder answers itself, so a TUI never waits
//! for a reply while no viewer (or no runner) is attached (decision 0001).
//! The runner answers DSR `\e[6n` from its emulator cursor; everything here
//! has an answer that does not depend on screen state.
//!
//! Viewers (xterm.js, the TUI) must therefore NOT forward their own automatic
//! replies to these queries, or the program sees two answers.

use tmuxedo_emu::Matcher;

const PATTERNS: &[&[u8]] = &[
    b"\x1b[c",          // 0 DA1
    b"\x1b[0c",         // 1 DA1
    b"\x1b[>c",         // 2 DA2
    b"\x1b[>0c",        // 3 DA2
    b"\x1b[>q",         // 4 XTVERSION
    b"\x1b[>0q",        // 5 XTVERSION
    b"\x1b[?u",         // 6 kitty keyboard flags
    b"\x1b[?2026$p",    // 7 DECRQM synchronized output
    b"\x1b[?1016$p",    // 8 DECRQM SGR-pixels mouse
    b"\x1b[16t",        // 9 cell size in pixels
    b"\x1b]10;?\x07",   // 10 OSC 10 foreground, BEL
    b"\x1b]10;?\x1b\\", // 11 OSC 10 foreground, ST
    b"\x1b]11;?\x07",   // 12 OSC 11 background, BEL
    b"\x1b]11;?\x1b\\", // 13 OSC 11 background, ST
    b"\x1b[?2026h",     // 14 (tracked, no answer)
    b"\x1b[?2026l",     // 15 (tracked, no answer)
];

pub struct QueryAnswerer {
    m: Matcher,
    sync_on: bool,
}

impl Default for QueryAnswerer {
    fn default() -> Self {
        Self {
            m: Matcher::new(PATTERNS),
            sync_on: false,
        }
    }
}

impl QueryAnswerer {
    /// Scan program output; returns the bytes to write back to the program.
    pub fn scan(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut out = vec![];
        for &b in bytes {
            let Some(i) = self.m.push(b) else { continue };
            match i {
                0 | 1 => out.extend_from_slice(b"\x1b[?62;22c"),
                2 | 3 => out.extend_from_slice(b"\x1b[>1;10;0c"),
                4 | 5 => {
                    out.extend_from_slice(b"\x1bP>|tmuxedo ");
                    out.extend_from_slice(crate::VERSION.as_bytes());
                    out.extend_from_slice(b"\x1b\\");
                }
                6 => out.extend_from_slice(b"\x1b[?0u"),
                7 => out.extend_from_slice(if self.sync_on {
                    b"\x1b[?2026;1$y"
                } else {
                    b"\x1b[?2026;2$y"
                }),
                8 => out.extend_from_slice(b"\x1b[?1016;0$y"),
                9 => out.extend_from_slice(b"\x1b[6;20;10t"),
                10 => out.extend_from_slice(b"\x1b]10;rgb:dddd/dddd/dddd\x07"),
                11 => out.extend_from_slice(b"\x1b]10;rgb:dddd/dddd/dddd\x1b\\"),
                12 => out.extend_from_slice(b"\x1b]11;rgb:1111/1111/1111\x07"),
                13 => out.extend_from_slice(b"\x1b]11;rgb:1111/1111/1111\x1b\\"),
                14 => self.sync_on = true,
                15 => self.sync_on = false,
                _ => {}
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_the_stateless_set() {
        let mut q = QueryAnswerer::default();
        assert_eq!(q.scan(b"x\x1b[c"), b"\x1b[?62;22c");
        assert_eq!(q.scan(b"\x1b[>0c"), b"\x1b[>1;10;0c");
        assert!(q.scan(b"\x1b[>q").starts_with(b"\x1bP>|tmuxedo "));
        assert_eq!(q.scan(b"\x1b[?u"), b"\x1b[?0u");
        assert_eq!(q.scan(b"\x1b[?2026$p"), b"\x1b[?2026;2$y");
        q.scan(b"\x1b[?2026h");
        assert_eq!(q.scan(b"\x1b[?2026$p"), b"\x1b[?2026;1$y");
        assert_eq!(q.scan(b"\x1b[?1016$p"), b"\x1b[?1016;0$y");
        assert_eq!(q.scan(b"\x1b[16t"), b"\x1b[6;20;10t");
        assert_eq!(q.scan(b"\x1b]11;?\x1b\\"), b"\x1b]11;rgb:1111/1111/1111\x1b\\");
        assert_eq!(q.scan(b"\x1b]10;?\x07"), b"\x1b]10;rgb:dddd/dddd/dddd\x07");
        // Split across reads.
        assert!(q.scan(b"\x1b[").is_empty());
        assert_eq!(q.scan(b"c"), b"\x1b[?62;22c");
        // DSR is the runner's job.
        assert!(q.scan(b"\x1b[6n").is_empty());
    }
}
