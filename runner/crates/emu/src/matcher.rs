//! Byte-level matcher for a fixed set of escape sequences. State carries
//! across calls, so a sequence split over two PTY reads is still recognised.

pub struct Matcher {
    patterns: &'static [&'static [u8]],
    buf: Vec<u8>,
}

impl Matcher {
    /// Every pattern must start with ESC.
    pub fn new(patterns: &'static [&'static [u8]]) -> Self {
        debug_assert!(patterns.iter().all(|p| p.first() == Some(&crate::ESC)));
        Self {
            patterns,
            buf: Vec::with_capacity(16),
        }
    }

    fn is_prefix(&self) -> bool {
        self.patterns.iter().any(|p| p.starts_with(&self.buf))
    }

    /// Feed one byte. Returns the index of the pattern that completes at this byte.
    #[inline]
    pub fn push(&mut self, b: u8) -> Option<usize> {
        if self.buf.is_empty() && b != crate::ESC {
            return None;
        }
        self.buf.push(b);
        if let Some(i) = self.patterns.iter().position(|p| *p == self.buf.as_slice()) {
            self.buf.clear();
            return Some(i);
        }
        if !self.is_prefix() {
            self.buf.clear();
            if b == crate::ESC {
                self.buf.push(b);
            }
        }
        None
    }

    /// Feed a slice, returning every completed pattern as (index of the final byte, pattern).
    pub fn scan(&mut self, bytes: &[u8]) -> Vec<(usize, usize)> {
        let mut out = vec![];
        for (i, &b) in bytes.iter().enumerate() {
            if let Some(p) = self.push(b) {
                out.push((i, p));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: &[&[u8]] = &[b"\x1b[c", b"\x1b]11;?\x1b\\", b"\x1b]11;?\x07"];

    #[test]
    fn finds_whole_and_split_sequences() {
        let mut m = Matcher::new(P);
        assert_eq!(m.scan(b"ab\x1b[cd"), vec![(4, 0)]);
        assert_eq!(m.scan(b"x\x1b["), vec![]);
        assert_eq!(m.scan(b"c"), vec![(0, 0)]);
        // ESC inside a pattern (ST terminator) does not restart the match.
        assert_eq!(m.scan(b"\x1b]11;?\x1b\\"), vec![(7, 1)]);
        assert_eq!(m.scan(b"\x1b]11;?\x07"), vec![(6, 2)]);
        // A broken sequence followed by a real one.
        assert_eq!(m.scan(b"\x1b[1\x1b[c"), vec![(5, 0)]);
        assert_eq!(m.scan(b"\x1b\x1b[c"), vec![(3, 0)]);
        assert_eq!(m.scan(b"\x1b[0c"), vec![]);
    }
}
