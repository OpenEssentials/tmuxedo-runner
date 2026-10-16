//! The runner's copy of each pane's recent bytes, indexed by relay offset, so
//! an `attach {from}` inside it is answered by replay rather than snapshot.
//! It mirrors the holder ring (same capacity): after a runner restart it is
//! refilled from the holder.

use std::collections::VecDeque;

pub struct Mirror {
    buf: VecDeque<u8>,
    end: u64,
    cap: usize,
}

impl Mirror {
    pub fn new(cap: usize, at: u64) -> Self {
        Self {
            buf: VecDeque::new(),
            end: at,
            cap,
        }
    }
    pub fn start(&self) -> u64 {
        self.end - self.buf.len() as u64
    }
    pub fn end(&self) -> u64 {
        self.end
    }
    /// Discard everything; the next byte has offset `at`.
    pub fn reset_at(&mut self, at: u64) {
        self.buf.clear();
        self.end = at;
    }
    pub fn push(&mut self, b: &[u8]) {
        self.buf.extend(b);
        self.end += b.len() as u64;
        let over = self.buf.len().saturating_sub(self.cap);
        if over > 0 {
            self.buf.drain(..over);
        }
    }
    pub fn contains(&self, from: u64) -> bool {
        from >= self.start() && from <= self.end
    }
    /// Up to `max` bytes starting at `from` (which must be in range).
    pub fn read(&self, from: u64, max: usize) -> Vec<u8> {
        if !self.contains(from) {
            return vec![];
        }
        let skip = (from - self.start()) as usize;
        let n = ((self.end - from) as usize).min(max);
        self.buf.range(skip..skip + n).copied().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_and_reads_by_offset() {
        let mut m = Mirror::new(8, 100);
        m.push(b"abcdef");
        assert_eq!((m.start(), m.end()), (100, 106));
        m.push(b"ghij");
        assert_eq!((m.start(), m.end()), (102, 110));
        assert_eq!(m.read(102, 100), b"cdefghij");
        assert_eq!(m.read(105, 2), b"fg");
        assert_eq!(m.read(110, 5), b"");
        assert!(!m.contains(101));
        assert!(m.read(101, 5).is_empty());
        m.reset_at(500);
        assert_eq!((m.start(), m.end()), (500, 500));
    }
}
