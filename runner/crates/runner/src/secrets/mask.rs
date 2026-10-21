//! The terminal masker (docs/design/secrets-and-env.md §5): the single choke
//! point between the holder's raw ring and everything the runner derives from
//! a pane (mirror, emulator, OUTPUT frames, snapshots, `pane.read`, glances,
//! the history file, screen detection, events and push).
//!
//! - **Literals, not shapes.** The patterns are the values of secrets this
//!   machine knows (delivered from the vault, or secret-named variables in the
//!   user's shell settings) and their common encodings ([`forms`]). Shape
//!   rules would garble ordinary terminal output and stay out of the stream.
//! - **Same length.** Every byte of a match becomes `*`, so stream offsets
//!   (docs/protocol.md "Principles") are unchanged.
//! - **Streaming Aho-Corasick.** The automaton keeps its state across reads.
//!   The bytes that could still be the start of a match (the automaton's
//!   depth) are held back until the next read decides them, or until
//!   [`HOLDBACK`] passes with no new byte, when they are flushed as they are:
//!   a secret split by a longer pause leaks its tail (documented, §5.2).
//! - **Overlapping matches** are all masked (the union of their bytes).

use base64::Engine;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Values shorter than this are never masked (they would hit ordinary text);
/// web refuses to store them (`MIN_MASKABLE`).
pub const MIN_LEN: usize = 8;
/// How long a possible partial match is held back waiting for more bytes.
pub const HOLDBACK: Duration = Duration::from_millis(30);
/// Longest pattern kept; longer encodings are masked line by line instead.
pub const MAX_PATTERN: usize = 16 * 1024;
pub const MASK_BYTE: u8 = b'*';

/// The encodings a value is masked in (§5.2): raw; base64 standard and
/// URL-safe, padded and not, plus the run of characters that depends only on
/// the value when it sits at byte offset 1 or 2 of a larger encoded blob
/// (`printf 'user:%s' "$TOKEN" | base64`); percent-encoding (upper and lower
/// case hex); JSON string escaping. A multi-line value (a key file) also
/// gives each of its lines.
pub fn forms(value: &[u8]) -> Vec<Vec<u8>> {
    let mut out: BTreeSet<Vec<u8>> = BTreeSet::new();
    let v = trim(value);
    if v.len() < MIN_LEN {
        return vec![];
    }
    let mut add = |f: Vec<u8>| {
        if f.len() >= MIN_LEN && f.len() <= MAX_PATTERN {
            out.insert(f);
        }
    };
    add(v.to_vec());
    for engine in [
        &base64::engine::general_purpose::STANDARD,
        &base64::engine::general_purpose::URL_SAFE,
    ] {
        let full = engine.encode(v);
        add(full.as_bytes().to_vec());
        add(full.trim_end_matches('=').as_bytes().to_vec());
        for shift in 0..3usize {
            add(b64_core(engine, v, shift));
        }
    }
    let pct_upper = percent(v, true);
    add(pct_upper.clone());
    add(percent(v, false));
    // Every byte escaped (`od | sed`-style encoders), and form encoding.
    add(v.iter().flat_map(|b| format!("%{b:02X}").into_bytes()).collect());
    add(v.iter().flat_map(|b| format!("%{b:02x}").into_bytes()).collect());
    if v.contains(&b' ') {
        add(String::from_utf8_lossy(&pct_upper).replace("%20", "+").into_bytes());
    }
    add(json_escape(v, false));
    add(json_escape(v, true));
    if v.contains(&b'\n') {
        for line in v.split(|&b| b == b'\n') {
            let line = trim(line);
            if line.len() >= MIN_LEN && !line.starts_with(b"-----") {
                for f in forms(line) {
                    add(f);
                }
            }
        }
    }
    out.into_iter().collect()
}

fn trim(v: &[u8]) -> &[u8] {
    let start = v.iter().position(|b| !b.is_ascii_whitespace()).unwrap_or(v.len());
    let end = v
        .iter()
        .rposition(|b| !b.is_ascii_whitespace())
        .map_or(start, |e| e + 1);
    &v[start..end]
}

/// The base64 characters determined only by `v` when it starts `shift` bytes
/// into the encoded input (whatever precedes or follows it).
fn b64_core(engine: &base64::engine::GeneralPurpose, v: &[u8], shift: usize) -> Vec<u8> {
    let mut input = vec![0u8; shift];
    input.extend_from_slice(v);
    let enc = engine.encode(&input);
    let enc = enc.trim_end_matches('=').as_bytes();
    // Character i covers bits [6i, 6i+6); the value's bits are [8s, 8(s+n)).
    let first = (8 * shift).div_ceil(6);
    let last_bit = 8 * (shift + v.len());
    let end = last_bit / 6; // characters fully inside
    if first >= end || end > enc.len() {
        return vec![];
    }
    enc[first..end].to_vec()
}

fn percent(v: &[u8], upper: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 3);
    for &b in v {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b);
        } else if upper {
            out.extend_from_slice(format!("%{b:02X}").as_bytes());
        } else {
            out.extend_from_slice(format!("%{b:02x}").as_bytes());
        }
    }
    out
}

fn json_escape(v: &[u8], slash: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() + 8);
    for &b in v {
        match b {
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'/' if slash => out.extend_from_slice(b"\\/"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\t' => out.extend_from_slice(b"\\t"),
            0..=0x1f => out.extend_from_slice(format!("\\u{b:04x}").as_bytes()),
            _ => out.push(b),
        }
    }
    out
}

/// An Aho-Corasick automaton over the mask patterns. Built once per set;
/// shared (`Arc`) by every pane's [`StreamMasker`].
#[derive(Debug, Default)]
pub struct MaskSet {
    /// Per node: sorted (byte, child) edges.
    edges: Vec<Vec<(u8, u32)>>,
    fail: Vec<u32>,
    depth: Vec<u32>,
    /// Lengths of every pattern that ends at this node (its own and those
    /// reachable through failure links).
    out: Vec<Vec<u32>>,
    patterns: usize,
    /// Identity of the value set (to tell a changed set from the same one).
    fingerprint: [u8; 32],
}

impl MaskSet {
    /// From secret values: each is expanded to its [`forms`].
    pub fn from_values<'a>(values: impl IntoIterator<Item = &'a [u8]>) -> MaskSet {
        let mut pats: BTreeSet<Vec<u8>> = BTreeSet::new();
        for v in values {
            pats.extend(forms(v));
        }
        Self::from_patterns(pats)
    }

    pub fn from_patterns(pats: BTreeSet<Vec<u8>>) -> MaskSet {
        use sha2::Digest;
        let mut h = sha2::Sha256::new();
        let mut m = MaskSet {
            edges: vec![vec![]],
            fail: vec![0],
            depth: vec![0],
            out: vec![vec![]],
            patterns: 0,
            fingerprint: [0; 32],
        };
        for p in &pats {
            if p.is_empty() {
                continue;
            }
            h.update((p.len() as u64).to_be_bytes());
            h.update(p);
            m.patterns += 1;
            let mut s = 0u32;
            for &b in p {
                s = match m.child(s, b) {
                    Some(c) => c,
                    None => {
                        let c = m.edges.len() as u32;
                        m.edges.push(vec![]);
                        m.fail.push(0);
                        m.depth.push(m.depth[s as usize] + 1);
                        m.out.push(vec![]);
                        let e = &mut m.edges[s as usize];
                        let at = e.partition_point(|(x, _)| *x < b);
                        e.insert(at, (b, c));
                        c
                    }
                };
            }
            m.out[s as usize].push(p.len() as u32);
        }
        m.fingerprint = h.finalize().into();
        // Breadth-first failure links.
        let mut queue = std::collections::VecDeque::new();
        for &(_, c) in &m.edges[0].clone() {
            m.fail[c as usize] = 0;
            queue.push_back(c);
        }
        while let Some(s) = queue.pop_front() {
            for (b, c) in m.edges[s as usize].clone() {
                let mut f = m.fail[s as usize];
                let target = loop {
                    if let Some(t) = m.child(f, b) {
                        break t;
                    }
                    if f == 0 {
                        break 0;
                    }
                    f = m.fail[f as usize];
                };
                m.fail[c as usize] = target;
                let inherited = m.out[target as usize].clone();
                m.out[c as usize].extend(inherited);
                queue.push_back(c);
            }
        }
        m
    }

    fn child(&self, s: u32, b: u8) -> Option<u32> {
        let e = &self.edges[s as usize];
        e.binary_search_by_key(&b, |(x, _)| *x).ok().map(|i| e[i].1)
    }

    fn step(&self, mut s: u32, b: u8) -> u32 {
        loop {
            if let Some(c) = self.child(s, b) {
                return c;
            }
            if s == 0 {
                return 0;
            }
            s = self.fail[s as usize];
        }
    }

    pub fn is_empty(&self) -> bool {
        self.patterns == 0
    }

    pub fn pattern_count(&self) -> usize {
        self.patterns
    }

    pub fn fingerprint(&self) -> [u8; 32] {
        self.fingerprint
    }

    /// Mask a complete buffer in place (no holdback).
    pub fn mask_in_place(&self, buf: &mut [u8]) -> usize {
        if self.is_empty() {
            return 0;
        }
        let cover = self.coverage(buf);
        let mut n = 0;
        for (b, c) in buf.iter_mut().zip(cover) {
            if c {
                *b = MASK_BYTE;
                n += 1;
            }
        }
        n
    }

    fn coverage(&self, buf: &[u8]) -> Vec<bool> {
        let mut cover = vec![false; buf.len()];
        let mut s = 0u32;
        for (i, &b) in buf.iter().enumerate() {
            s = self.step(s, b);
            for &l in &self.out[s as usize] {
                let start = i + 1 - l as usize;
                cover[start..=i].iter_mut().for_each(|c| *c = true);
            }
        }
        cover
    }

    /// Replace each masked run with `with` (for text where length does not
    /// matter: chat events), keeping the rest of `s`.
    pub fn redact_str(&self, s: &str, with: &str) -> String {
        if self.is_empty() || !self.contains(s.as_bytes()) {
            return s.to_string();
        }
        let b = s.as_bytes();
        let cover = self.coverage(b);
        let mut out = Vec::with_capacity(b.len());
        let mut i = 0;
        while i < b.len() {
            if cover[i] {
                out.extend_from_slice(with.as_bytes());
                while i < b.len() && cover[i] {
                    i += 1;
                }
            } else {
                out.push(b[i]);
                i += 1;
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    pub fn mask(&self, buf: &[u8]) -> Vec<u8> {
        let mut v = buf.to_vec();
        self.mask_in_place(&mut v);
        v
    }

    /// Mask a string (lossless for valid UTF-8: only whole matches change,
    /// and patterns are whole values or ASCII encodings).
    pub fn mask_str(&self, s: &str) -> String {
        if self.is_empty() {
            return s.to_string();
        }
        String::from_utf8_lossy(&self.mask(s.as_bytes())).into_owned()
    }

    /// Whether `buf` holds any pattern.
    pub fn contains(&self, buf: &[u8]) -> bool {
        if self.is_empty() {
            return false;
        }
        let mut s = 0u32;
        for &b in buf {
            s = self.step(s, b);
            if !self.out[s as usize].is_empty() {
                return true;
            }
        }
        false
    }
}

/// One pane's stream through a [`MaskSet`].
#[derive(Debug)]
pub struct StreamMasker {
    set: Arc<MaskSet>,
    state: u32,
    /// Raw bytes not yet released, and whether each is inside a match.
    held: Vec<u8>,
    cover: Vec<bool>,
    held_since: Option<Instant>,
}

impl Default for StreamMasker {
    fn default() -> Self {
        Self::new(Arc::new(MaskSet::default()))
    }
}

impl StreamMasker {
    pub fn new(set: Arc<MaskSet>) -> Self {
        Self {
            set,
            state: 0,
            held: vec![],
            cover: vec![],
            held_since: None,
        }
    }

    pub fn set(&self) -> &Arc<MaskSet> {
        &self.set
    }

    /// Bytes held back right now.
    pub fn held(&self) -> usize {
        self.held.len()
    }

    /// The held bytes have waited [`HOLDBACK`].
    pub fn due(&self, now: Instant) -> bool {
        self.held_since.is_some_and(|t| now.duration_since(t) >= HOLDBACK)
    }

    /// Feed raw bytes; returns the masked bytes that are decided (possibly
    /// fewer than given: the rest is held until the next feed or flush).
    pub fn feed(&mut self, bytes: &[u8], now: Instant) -> Vec<u8> {
        if self.set.is_empty() && self.held.is_empty() {
            return bytes.to_vec();
        }
        for &b in bytes {
            self.push(b);
        }
        let keep = self.set.depth[self.state as usize] as usize;
        let out = self.release(self.held.len() - keep.min(self.held.len()));
        // The wait counts from the last byte that kept a match going: a value
        // echoed a few bytes at a time (a line editor echoing typed input) is
        // not split by the wait however long it is in all.
        self.held_since = if self.held.is_empty() {
            None
        } else if bytes.is_empty() {
            self.held_since.or(Some(now))
        } else {
            Some(now)
        };
        out
    }

    fn push(&mut self, b: u8) {
        self.state = self.set.step(self.state, b);
        self.held.push(b);
        self.cover.push(false);
        let n = self.held.len();
        for &l in &self.set.out[self.state as usize] {
            let l = (l as usize).min(n);
            self.cover[n - l..].iter_mut().for_each(|c| *c = true);
        }
    }

    fn release(&mut self, n: usize) -> Vec<u8> {
        let mut out: Vec<u8> = self.held.drain(..n).collect();
        for (b, c) in out.iter_mut().zip(self.cover.drain(..n)) {
            if c {
                *b = MASK_BYTE;
            }
        }
        out
    }

    /// Release everything held (complete matches masked, a partial match
    /// released as it is) and start over at the root.
    pub fn flush(&mut self) -> Vec<u8> {
        self.state = 0;
        self.held_since = None;
        let n = self.held.len();
        self.release(n)
    }

    /// Drop held bytes (the stream jumped: a gap or a re-attach).
    pub fn reset(&mut self) {
        self.state = 0;
        self.held.clear();
        self.cover.clear();
        self.held_since = None;
    }

    /// Switch to a new set; held bytes are re-scanned with it (matches found
    /// by the old set stay masked).
    pub fn set_mask(&mut self, set: Arc<MaskSet>) {
        if Arc::ptr_eq(&set, &self.set) {
            return;
        }
        let held = std::mem::take(&mut self.held);
        let old = std::mem::take(&mut self.cover);
        self.set = set;
        self.state = 0;
        for &b in &held {
            self.push(b);
        }
        for (c, o) in self.cover.iter_mut().zip(old) {
            *c |= o;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(vals: &[&str]) -> Arc<MaskSet> {
        Arc::new(MaskSet::from_values(vals.iter().map(|v| v.as_bytes())))
    }

    fn b64(v: &str) -> String {
        base64::engine::general_purpose::STANDARD.encode(v)
    }

    #[test]
    fn masks_raw_and_encodings_same_length() {
        let s = set(&["tok-0123456789abcdef"]);
        for text in [
            "x tok-0123456789abcdef y".to_string(),
            format!("b64 {}\n", b64("tok-0123456789abcdef")),
            format!("b64 {}\n", b64("user:tok-0123456789abcdef")),
            format!("b64 {}\n", b64("ab:tok-0123456789abcdef:zz")),
            "url tok-0123456789abcdef".to_string(),
        ] {
            let out = s.mask(text.as_bytes());
            assert_eq!(out.len(), text.len());
            assert!(out.contains(&MASK_BYTE), "{text}");
            assert!(!String::from_utf8_lossy(&out).contains("0123456789abcdef"), "{text}");
        }
        let pct = set(&["p@ss w/rd+secret!"]);
        let out = pct.mask(b"q=p%40ss%20w%2Frd%2Bsecret%21&x=1");
        assert_eq!(String::from_utf8_lossy(&out), format!("q={}&x=1", "*".repeat(27)));
        let out = pct.mask(b"q=p%40ss%20w%2frd%2bsecret%21");
        assert!(!String::from_utf8_lossy(&out).contains("secret"));
        let out = pct.mask(br#"{"v":"p@ss w\/rd+secret!"}"#);
        assert!(!String::from_utf8_lossy(&out).contains("secret"));
    }

    #[test]
    fn short_values_and_empty_sets_pass_through() {
        assert!(set(&["short"]).is_empty());
        let mut m = StreamMasker::default();
        assert_eq!(m.feed(b"anything", Instant::now()), b"anything");
        assert_eq!(m.held(), 0);
    }

    #[test]
    fn overlapping_and_nested_values_are_masked_whole() {
        let s = set(&["abcdefgh12", "gh12345678"]);
        let out = s.mask(b"..abcdefgh12345678..");
        assert_eq!(out, b"..****************..");
        let s = set(&["valueXYZ12345", "valueXYZ1234567"]);
        let mut m = StreamMasker::new(s);
        let now = Instant::now();
        let mut out = m.feed(b"a valueXYZ12345", now);
        assert_eq!(m.held(), 13, "a complete match that a longer one extends is held");
        out.extend(m.feed(b"67 b", now));
        assert_eq!(out, b"a *************** b");
    }

    #[test]
    fn split_reads_are_held_back_and_flushed() {
        let s = set(&["sk-split-secret-value"]);
        let mut m = StreamMasker::new(s);
        let t0 = Instant::now();
        let a = m.feed(b"hello sk-split-", t0);
        assert_eq!(a, b"hello ");
        assert_eq!(m.held(), 9);
        assert!(!m.due(t0));
        assert!(m.due(t0 + HOLDBACK));
        let b = m.feed(b"secret-value done", t0);
        let all = [a, b].concat();
        assert_eq!(all, b"hello ********************* done");
        // A prefix never completed is released by flush, unmasked (the
        // documented leak for > 30 ms gaps).
        let c = m.feed(b"sk-split", t0);
        assert!(c.is_empty());
        assert_eq!(m.flush(), b"sk-split");
    }

    #[test]
    fn a_match_echoed_a_byte_at_a_time_is_masked_whole() {
        // A line editor echoes typed input a byte at a time: the wait restarts
        // with each byte, so the value is masked however long it takes in all.
        let mut m = StreamMasker::new(set(&["sk-split-secret-value"]));
        let mut t = Instant::now();
        let mut out = vec![];
        for &b in b"sk-split-secret-value!" {
            t += HOLDBACK / 2;
            assert!(!m.due(t));
            out.extend(m.feed(&[b], t));
        }
        assert_eq!(out, b"*********************!");
        // A pause longer than the wait still releases a prefix as it is.
        assert!(m.feed(b"sk-split", t).is_empty());
        assert!(m.due(t + HOLDBACK));
        assert_eq!(m.flush(), b"sk-split");
    }

    #[test]
    fn a_new_set_rescans_held_bytes() {
        let mut m = StreamMasker::new(set(&["zzzzzzzzzz-other"]));
        let t = Instant::now();
        let out = m.feed(b"zzzzzzzzzz", t);
        assert!(out.is_empty());
        m.set_mask(set(&["zzzzzzzzzz-other", "zzzzzzzzzz"]));
        assert_eq!(m.feed(b"!", t), b"**********!");
    }

    #[test]
    fn b64_cores_cover_every_alignment() {
        let v = "Zx9-secret-QQ";
        for pre in ["", "a", "ab", "abc", "abcd"] {
            for post in ["", "!", "!!"] {
                let text = b64(&format!("{pre}{v}{post}"));
                let out = set(&[v]).mask(text.as_bytes());
                // Every character fully determined by the value is masked.
                let masked = out.iter().filter(|&&b| b == MASK_BYTE).count();
                assert!(masked >= (v.len() * 4 / 3).saturating_sub(2), "{pre}|{post}: {text}");
            }
        }
    }

    #[test]
    fn multi_line_values_mask_each_line() {
        let key = concat!(
            "-----BEGIN OPENSSH ",
            "PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAABG5vbmU\nAAAAAAAAAAEAAAAzAAAAC3NzaC1lZDI1\n-----END OPENSSH ",
            "PRIVATE KEY-----\n"
        );
        let s = set(&[key]);
        let screen = key.replace('\n', "\r\n");
        let out = String::from_utf8(s.mask(screen.as_bytes())).unwrap();
        assert!(!out.contains("b3BlbnNzaC1rZXktdjEAAAAABG5vbmU"), "{out}");
        assert!(!out.contains("AAAAAAAAAAEAAAAzAAAAC3NzaC1lZDI1"), "{out}");
    }

    mod props {
        use super::super::*;
        use proptest::prelude::*;

        fn encode(kind: u8, prefix: &[u8], v: &[u8]) -> Vec<u8> {
            let mut input = prefix.to_vec();
            input.extend_from_slice(v);
            match kind % 6 {
                0 => v.to_vec(),
                1 => base64::engine::general_purpose::STANDARD.encode(&input).into_bytes(),
                2 => base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .encode(&input)
                    .into_bytes(),
                3 => percent(v, true),
                4 => percent(v, false),
                _ => json_escape(v, false),
            }
        }

        fn contains(hay: &[u8], needle: &[u8]) -> bool {
            needle.len() <= hay.len() && hay.windows(needle.len()).any(|w| w == needle)
        }

        proptest! {
            #![proptest_config(ProptestConfig { cases: 400, ..ProptestConfig::default() })]

            /// Any split of the stream into reads gives exactly the one-shot
            /// result: same length, every encoded secret gone.
            #[test]
            fn stream_equals_one_shot_across_any_chunking(
                secrets in proptest::collection::vec(proptest::collection::vec(0x21u8..0x7f, 8..40), 1..4),
                noise in proptest::collection::vec(proptest::collection::vec(any::<u8>(), 0..30), 1..6),
                kinds in proptest::collection::vec(any::<u8>(), 1..6),
                prefixes in proptest::collection::vec(proptest::collection::vec(0x21u8..0x7f, 0..3), 1..6),
                cuts in proptest::collection::vec(0usize..400, 0..12),
            ) {
                let set = Arc::new(MaskSet::from_values(secrets.iter().map(|v| v.as_slice())));
                let mut text = vec![];
                let mut planted = vec![];
                for (i, n) in noise.iter().enumerate() {
                    text.extend_from_slice(n);
                    let v = &secrets[i % secrets.len()];
                    let k = kinds[i % kinds.len()];
                    let pre = &prefixes[i % prefixes.len()];
                    text.extend_from_slice(&encode(k, pre, v));
                    planted.push((k % 6, pre.len(), v.clone()));
                }
                let one_shot = set.mask(&text);
                let mut cuts: Vec<usize> = cuts.into_iter().filter(|c| *c < text.len()).collect();
                cuts.sort_unstable();
                cuts.dedup();
                let mut m = StreamMasker::new(set.clone());
                let mut out = vec![];
                let mut at = 0;
                let t = Instant::now();
                for c in cuts.into_iter().chain([text.len()]) {
                    out.extend(m.feed(&text[at..c], t));
                    at = c;
                }
                out.extend(m.flush());
                prop_assert_eq!(out.len(), text.len());
                prop_assert_eq!(&out, &one_shot);
                for v in &secrets {
                    prop_assert!(!contains(&out, v), "raw value survived");
                }
                for (kind, plen, v) in planted {
                    // Raw, percent and JSON forms vanish whole; a base64 run
                    // keeps at most the characters it shares with its neighbours.
                    let core = match kind {
                        1 => b64_core(&base64::engine::general_purpose::STANDARD, &v, plen % 3),
                        2 => b64_core(&base64::engine::general_purpose::URL_SAFE, &v, plen % 3),
                        3 => percent(&v, true),
                        4 => percent(&v, false),
                        5 => json_escape(&v, false),
                        _ => v.clone(),
                    };
                    prop_assert!(!contains(&out, &core), "an encoded form survived (kind {})", kind);
                }
            }

            /// Masking never changes bytes outside matches.
            #[test]
            fn bytes_outside_matches_are_untouched(text in proptest::collection::vec(any::<u8>(), 0..300)) {
                let set = MaskSet::from_values([b"never-in-random-bytes-xyz".as_slice()]);
                prop_assert_eq!(set.mask(&text), text);
            }
        }
    }
}
