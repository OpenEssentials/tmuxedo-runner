//! Recordings on disk: one folder per session, kept in parts.
//!
//! A part is an asciicast v3 file: a header line, then one event per line,
//! `[interval_seconds, code, data]` with code `o` (output), `r` (resize,
//! `COLSxROWS`) or `m` (marker, the event's label). The first event of every
//! part redraws the screen as it was, so each part plays on its own and the
//! oldest can be dropped. Bytes are masked before they get here (the pane's
//! stream masker); [`remask`] masks the files again when the set grows.

use crate::secrets::mask::MaskSet;
use serde_json::{json, Value};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tmuxedo_protocol::replay::{Cursor, From, MAX_BYTES, RETENTION_DAYS, SEGMENT_BYTES, SEGMENT_MS};

/// The folder's marker file: recording is on for this session.
const ON: &str = ".on";
const EXT: &str = "cast";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub segment_bytes: u64,
    pub segment_ms: u64,
    pub max_bytes: u64,
    pub max_age_ms: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            segment_bytes: SEGMENT_BYTES,
            segment_ms: SEGMENT_MS,
            max_bytes: MAX_BYTES,
            max_age_ms: RETENTION_DAYS * 24 * 3600 * 1000,
        }
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn mtime_ms(m: &fs::Metadata) -> u64 {
    m.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// A session id as a folder name: `:` becomes `~`, anything outside
/// `[A-Za-z0-9_-]` is `%xx`, so no id names `.`, `..` or a path.
pub fn dir_name(pane: &str) -> String {
    let mut s = String::with_capacity(pane.len());
    for b in pane.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-' => s.push(b as char),
            b':' => s.push('~'),
            _ => s.push_str(&format!("%{b:02x}")),
        }
    }
    s
}

pub fn pane_of(dir: &str) -> Option<String> {
    let b = dir.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'~' => out.push(b':'),
            b'%' => {
                let h = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
                out.push(u8::from_str_radix(h, 16).ok()?);
                i += 2;
            }
            c => out.push(c),
        }
        i += 1;
    }
    let p = String::from_utf8(out).ok()?;
    (dir_name(&p) == dir).then_some(p)
}

pub fn pane_dir(root: &Path, pane: &str) -> PathBuf {
    root.join(dir_name(pane))
}

pub fn is_on(root: &Path, pane: &str) -> bool {
    pane_dir(root, pane).join(ON).exists()
}

pub fn set_on(root: &Path, pane: &str, on: bool) -> io::Result<()> {
    let dir = pane_dir(root, pane);
    if on {
        fs::create_dir_all(&dir)?;
        File::create(dir.join(ON)).map(|_| ())
    } else {
        match fs::remove_file(dir.join(ON)) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
        let _ = fs::remove_dir(&dir); // only when empty
        Ok(())
    }
}

/// One part's file, by start.
#[derive(Debug, Clone)]
pub struct PartFile {
    pub start_ms: u64,
    pub path: PathBuf,
    pub bytes: u64,
    pub mtime_ms: u64,
}

/// A session's parts, oldest first.
pub fn parts(root: &Path, pane: &str) -> Vec<PartFile> {
    parts_in(&pane_dir(root, pane))
}

fn parts_in(dir: &Path) -> Vec<PartFile> {
    let mut v = vec![];
    let Ok(rd) = fs::read_dir(dir) else { return v };
    for e in rd.flatten() {
        let name = e.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(stem) = name.strip_suffix(".cast") else {
            continue;
        };
        let Ok(start_ms) = stem.parse::<u64>() else { continue };
        let Ok(m) = e.metadata() else { continue };
        if m.is_file() {
            v.push(PartFile {
                start_ms,
                path: e.path(),
                bytes: m.len(),
                mtime_ms: mtime_ms(&m),
            });
        }
    }
    v.sort_by_key(|p| p.start_ms);
    v
}

/// The screen as it is, for a part's first event.
pub struct Snap {
    pub ansi: Vec<u8>,
    pub cols: u16,
    pub rows: u16,
}

struct Part {
    start_ms: u64,
    path: PathBuf,
    out: BufWriter<File>,
    bytes: u64,
    last_ms: u64,
}

/// A session being recorded. Every write goes to the open part; a part is
/// opened (with a redraw) on the first write after none was open.
pub struct Recorder {
    root: PathBuf,
    pane: String,
    limits: Limits,
    part: Option<Part>,
    /// Stream offset recorded through: bytes before it are not taken again
    /// (a re-read of the holder's ring after the mask set grew).
    pub upto: u64,
    carry: Vec<u8>,
    /// A write failed: recording stopped until it is turned on again.
    pub failed: Option<String>,
    /// The disk is nearly full: nothing is written; a part opens again after.
    pub paused: bool,
}

fn interval(ms: u64) -> String {
    format!("{}.{:03}", ms / 1000, ms % 1000)
}

impl Recorder {
    pub fn new(root: &Path, pane: &str, limits: Limits) -> Self {
        Recorder {
            root: root.to_path_buf(),
            pane: pane.to_string(),
            limits,
            part: None,
            upto: 0,
            carry: vec![],
            failed: None,
            paused: false,
        }
    }

    pub fn open_path(&self) -> Option<&Path> {
        self.part.as_ref().map(|p| p.path.as_path())
    }

    pub fn is_open(&self) -> bool {
        self.part.is_some()
    }

    fn writable(&self) -> bool {
        self.failed.is_none() && !self.paused
    }

    fn fail(&mut self, e: io::Error) {
        self.failed = Some(e.to_string());
        self.part = None;
    }

    /// Open a part now (recording was turned on) with the screen as it is.
    pub fn start(&mut self, now: u64, snap: impl FnOnce() -> Snap) {
        if self.writable() && self.part.is_none() {
            if let Err(e) = self.open(now, snap()) {
                self.fail(e);
            }
        }
    }

    fn open(&mut self, now: u64, snap: Snap) -> io::Result<()> {
        let dir = pane_dir(&self.root, &self.pane);
        fs::create_dir_all(&dir)?;
        let mut start = now;
        let file = loop {
            let path = dir.join(format!("{start:016}.{EXT}"));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(f) => break (f, path),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => start += 1,
                Err(e) => return Err(e),
            }
        };
        let header = json!({
            "version": 3,
            "term": { "cols": snap.cols, "rows": snap.rows },
            "timestamp": start / 1000,
            "tmuxedo": { "pane": self.pane, "startMs": start, "snapshot": true, "masked": true },
        });
        let mut part = Part {
            start_ms: start,
            path: file.1,
            out: BufWriter::new(file.0),
            bytes: 0,
            last_ms: start,
        };
        let line = format!("{header}\n");
        part.out.write_all(line.as_bytes())?;
        part.bytes += line.len() as u64;
        self.carry.clear();
        self.part = Some(part);
        let text = String::from_utf8_lossy(&snap.ansi).into_owned();
        self.event(start, "o", &text)
    }

    fn event(&mut self, now: u64, code: &str, data: &str) -> io::Result<()> {
        let Some(p) = self.part.as_mut() else { return Ok(()) };
        let at = now.max(p.last_ms);
        let line = format!("[{}, {}, {}]\n", interval(at - p.last_ms), json!(code), json!(data));
        p.out.write_all(line.as_bytes())?;
        p.bytes += line.len() as u64;
        p.last_ms = at;
        Ok(())
    }

    fn due(&self, now: u64) -> bool {
        self.part.as_ref().is_some_and(|p| {
            p.bytes >= self.limits.segment_bytes || now.saturating_sub(p.start_ms) >= self.limits.segment_ms
        })
    }

    /// Masked output at stream offset `at` (the caller takes each byte once).
    pub fn output(&mut self, at: u64, bytes: &[u8], now: u64, snap: impl FnOnce() -> Snap) {
        let end = at + bytes.len() as u64;
        if end <= self.upto {
            return;
        }
        self.upto = end;
        if !self.writable() {
            self.close();
            return;
        }
        if self.due(now) {
            self.close();
        }
        if self.part.is_none() {
            if let Err(e) = self.open(now, snap()) {
                return self.fail(e);
            }
        }
        // Whole characters only; a sequence split across reads waits for its end.
        let mut buf = std::mem::take(&mut self.carry);
        buf.extend_from_slice(bytes);
        let keep = incomplete_tail(&buf);
        self.carry = buf.split_off(buf.len() - keep);
        if buf.is_empty() {
            return;
        }
        let text = String::from_utf8_lossy(&buf).into_owned();
        if let Err(e) = self.event(now, "o", &text) {
            self.fail(e);
        }
    }

    pub fn resize(&mut self, cols: u16, rows: u16, now: u64) {
        if let Err(e) = self.event(now, "r", &format!("{cols}x{rows}")) {
            self.fail(e);
        }
    }

    /// An agent event, as a marker. Written only into an open part: a
    /// marker alone does not start one (the next output does).
    pub fn marker(&mut self, label: &str, now: u64) {
        if !self.writable() {
            return;
        }
        if let Err(e) = self.event(now, "m", label) {
            self.fail(e);
        }
    }

    pub fn flush(&mut self) {
        if let Some(p) = self.part.as_mut() {
            if let Err(e) = p.out.flush() {
                self.fail(e);
            }
        }
    }

    /// End the open part (the next output opens another, with a redraw).
    pub fn close(&mut self) {
        if let Some(mut p) = self.part.take() {
            if !self.carry.is_empty() {
                let text = String::from_utf8_lossy(&std::mem::take(&mut self.carry)).into_owned();
                let line = format!("[{}, \"o\", {}]\n", interval(0), json!(text));
                let _ = p.out.write_all(line.as_bytes());
            }
            let _ = p.out.flush();
        }
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.close();
    }
}

/// Bytes at the end of `b` that start a UTF-8 sequence not yet complete.
fn incomplete_tail(b: &[u8]) -> usize {
    for back in 1..=3.min(b.len()) {
        let c = b[b.len() - back];
        if c & 0xC0 == 0x80 {
            continue; // continuation byte
        }
        let need = match c {
            0xC0..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF7 => 4,
            _ => return 0,
        };
        return if need > back { back } else { 0 };
    }
    0
}

// ── reading ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    pub pane: String,
    pub start_ms: u64,
    pub end_ms: u64,
    pub bytes: u64,
    pub segments: usize,
}

/// Every session with something recorded (or recording on), by id.
pub fn list(root: &Path) -> Vec<(Summary, bool)> {
    let mut v = vec![];
    let Ok(rd) = fs::read_dir(root) else { return v };
    for e in rd.flatten() {
        let name = e.file_name();
        let Some(pane) = name.to_str().and_then(pane_of) else {
            continue;
        };
        let ps = parts_in(&e.path());
        let on = e.path().join(ON).exists();
        if ps.is_empty() && !on {
            continue;
        }
        v.push((
            Summary {
                start_ms: ps.first().map(|p| p.start_ms).unwrap_or(0),
                end_ms: ps.iter().map(|p| p.mtime_ms.max(p.start_ms)).max().unwrap_or(0),
                bytes: ps.iter().map(|p| p.bytes).sum(),
                segments: ps.len(),
                pane,
            },
            on,
        ));
    }
    v.sort_by(|a, b| a.0.pane.cmp(&b.0.pane));
    v
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ev {
    pub at: u64,
    /// `s` (a part's redraw), `o`, `r` or `m`.
    pub k: &'static str,
    pub d: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    pub start_ms: u64,
    pub end_ms: u64,
    pub cols: u16,
    pub rows: u16,
    pub events: Vec<Ev>,
    /// Where the next read continues (the end, when nothing is left yet).
    pub next: Cursor,
    pub more: bool,
}

fn header(path: &Path) -> io::Result<(Value, u64)> {
    let mut r = BufReader::new(File::open(path)?);
    let mut line = String::new();
    let n = r.read_line(&mut line)?;
    if !line.ends_with('\n') {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "no header yet"));
    }
    let v: Value = serde_json::from_str(&line).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok((v, n as u64))
}

fn parse_interval(v: &Value) -> Option<u64> {
    // Written as `s.mmm`: read the text, not a float, so times add up exactly.
    let t = v.as_number()?.to_string();
    let (s, ms) = t.split_once('.').unwrap_or((&t, "0"));
    let ms = format!("{ms:0<3}");
    Some(s.parse::<u64>().ok()? * 1000 + ms.get(..3)?.parse::<u64>().ok()?)
}

/// Read a session's recording from `from`, at most `limit` bytes of event
/// data per page (one event at least). None: nothing recorded.
pub fn read(root: &Path, pane: &str, from: &From, limit: usize, markers_only: bool) -> io::Result<Option<Page>> {
    let ps = parts(root, pane);
    let (Some(first), Some(last)) = (ps.first(), ps.last()) else {
        return Ok(None);
    };
    let (start_ms, end_ms) = (
        first.start_ms,
        ps.iter().map(|p| p.mtime_ms.max(p.start_ms)).max().unwrap_or(0),
    );
    // (part index, offset in it or None for its start, time at that offset)
    let (mut idx, mut pos): (usize, Option<(u64, u64)>) = match from {
        From::Start => (0, None),
        From::At(t) => (ps.iter().rposition(|p| p.start_ms <= *t).unwrap_or(0), None),
        From::Cursor(c) => match ps.iter().position(|p| p.start_ms >= c.part) {
            Some(i) if ps[i].start_ms == c.part => (i, Some((c.offset, c.at))),
            Some(i) => (i, None), // that part was dropped: the next one
            None => {
                let next = Cursor {
                    part: last.start_ms,
                    offset: last.bytes,
                    at: c.at,
                };
                return Ok(Some(Page {
                    start_ms,
                    end_ms,
                    cols: 0,
                    rows: 0,
                    events: vec![],
                    next,
                    more: false,
                }));
            }
        },
    };
    let mut events = vec![];
    let mut used = 0usize;
    let (mut cols, mut rows) = (0u16, 0u16);
    loop {
        let p = &ps[idx];
        let (h, hlen) = match header(&p.path) {
            Ok(h) => h,
            Err(_) if idx + 1 < ps.len() => {
                idx += 1;
                pos = None;
                continue;
            }
            Err(_) => {
                let next = Cursor {
                    part: p.start_ms,
                    offset: 0,
                    at: p.start_ms,
                };
                return Ok(Some(Page {
                    start_ms,
                    end_ms,
                    cols,
                    rows,
                    events,
                    next,
                    more: false,
                }));
            }
        };
        let size = (
            h["term"]["cols"].as_u64().unwrap_or(80) as u16,
            h["term"]["rows"].as_u64().unwrap_or(24) as u16,
        );
        if cols == 0 {
            (cols, rows) = size;
        }
        let (mut off, mut at) = pos.unwrap_or((hlen, p.start_ms));
        if off < hlen {
            off = hlen;
        }
        let mut f = File::open(&p.path)?;
        f.seek(SeekFrom::Start(off))?;
        let mut r = BufReader::new(f);
        let mut line = String::new();
        loop {
            line.clear();
            let n = r.read_line(&mut line)?;
            if n == 0 || !line.ends_with('\n') {
                break; // the end, or a line still being written
            }
            let first_event = off == hlen;
            let v: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
            off += n as u64;
            let (Some(i), Some(code), Some(d)) = (parse_interval(&v[0]), v[1].as_str(), v[2].as_str()) else {
                continue;
            };
            at += i;
            let k = match code {
                "o" if first_event => "s",
                "o" => "o",
                "r" => "r",
                "m" => "m",
                _ => continue,
            };
            if markers_only && k != "m" {
                continue;
            }
            if k == "s" {
                // A part's redraw is drawn at the part's size: say it first.
                events.push(Ev {
                    at,
                    k: "r",
                    d: format!("{}x{}", size.0, size.1),
                });
            }
            used += d.len();
            events.push(Ev {
                at,
                k,
                d: d.to_string(),
            });
            if used >= limit {
                let next = Cursor {
                    part: p.start_ms,
                    offset: off,
                    at,
                };
                let more = off < p.bytes || idx + 1 < ps.len();
                return Ok(Some(Page {
                    start_ms,
                    end_ms,
                    cols,
                    rows,
                    events,
                    next,
                    more,
                }));
            }
        }
        if idx + 1 < ps.len() {
            idx += 1;
            pos = None;
            continue;
        }
        let next = Cursor {
            part: p.start_ms,
            offset: off,
            at,
        };
        return Ok(Some(Page {
            start_ms,
            end_ms,
            cols,
            rows,
            events,
            next,
            more: false,
        }));
    }
}

/// Remove a session's recording (every part but `open`). Returns bytes removed.
pub fn delete(root: &Path, pane: &str, open: Option<&Path>) -> u64 {
    let mut gone = 0;
    for p in parts(root, pane) {
        if Some(p.path.as_path()) != open && fs::remove_file(&p.path).is_ok() {
            gone += p.bytes;
        }
    }
    let _ = fs::remove_dir(pane_dir(root, pane));
    gone
}

/// Retention (D-SI4): parts older than the age limit go, then the oldest
/// until the machine's total fits. Open parts stay. Returns bytes removed.
pub fn prune(root: &Path, limits: &Limits, now: u64, open: &[PathBuf]) -> u64 {
    let mut all: Vec<PartFile> = vec![];
    if let Ok(rd) = fs::read_dir(root) {
        for e in rd.flatten() {
            if e.file_name().to_str().and_then(pane_of).is_some() {
                all.extend(parts_in(&e.path()));
            }
        }
    }
    all.sort_by_key(|p| (p.mtime_ms, p.start_ms));
    let mut total: u64 = all.iter().map(|p| p.bytes).sum();
    let mut gone = 0;
    for p in &all {
        if open.contains(&p.path) {
            continue;
        }
        let old = now.saturating_sub(p.mtime_ms) > limits.max_age_ms;
        if (old || total > limits.max_bytes) && fs::remove_file(&p.path).is_ok() {
            total -= p.bytes;
            gone += p.bytes;
            if let Some(d) = p.path.parent() {
                let _ = fs::remove_dir(d); // when empty (no `.on`, no parts)
            }
        }
    }
    gone
}

/// Mask every closed part again with `set` (it grew): the output events of
/// a part are joined, masked (same length) and split back, so a value split
/// across two events is found too. Returns the parts rewritten.
pub fn remask(root: &Path, set: &MaskSet, open: &[PathBuf]) -> usize {
    let mut n = 0;
    let Ok(rd) = fs::read_dir(root) else { return 0 };
    for e in rd.flatten() {
        if e.file_name().to_str().and_then(pane_of).is_none() {
            continue;
        }
        for p in parts_in(&e.path()) {
            if !open.contains(&p.path) && remask_part(&p.path, set).unwrap_or(false) {
                n += 1;
            }
        }
    }
    n
}

fn remask_part(path: &Path, set: &MaskSet) -> io::Result<bool> {
    let text = fs::read_to_string(path)?;
    let mut lines: Vec<Value> = vec![];
    let mut head = None;
    for l in text.lines() {
        if head.is_none() {
            head = Some(l.to_string());
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(l) {
            lines.push(v);
        }
    }
    let Some(head) = head else { return Ok(false) };
    let mut joined: Vec<u8> = vec![];
    let mut lens = vec![];
    for v in &lines {
        if v[1] == "o" {
            let d = v[2].as_str().unwrap_or("").as_bytes();
            lens.push(d.len());
            joined.extend_from_slice(d);
        }
    }
    if set.mask_in_place(&mut joined) == 0 {
        return Ok(false);
    }
    let mut out = String::with_capacity(text.len());
    out.push_str(&head);
    out.push('\n');
    let mut at = 0;
    let mut li = 0;
    for v in &mut lines {
        if v[1] == "o" {
            let n = lens[li];
            li += 1;
            v[2] = json!(String::from_utf8_lossy(&joined[at..at + n]).into_owned());
            at += n;
        }
        // Keep the interval as written (`s.mmm`).
        let iv = v[0]
            .as_number()
            .map(|n| n.to_string())
            .unwrap_or_else(|| "0.000".into());
        out.push_str(&format!("[{iv}, {}, {}]\n", v[1], v[2]));
    }
    let mtime = fs::metadata(path)?.modified().ok();
    let tmp = path.with_extension("remask");
    {
        let mut f = File::create(&tmp)?;
        f.write_all(out.as_bytes())?;
        f.sync_all()?;
        if let Some(t) = mtime {
            let _ = f.set_modified(t);
        }
    }
    fs::rename(&tmp, path)?;
    Ok(true)
}

/// For tests: age a part.
pub fn set_mtime(path: &Path, ms: u64) -> io::Result<()> {
    File::options()
        .write(true)
        .open(path)?
        .set_modified(UNIX_EPOCH + Duration::from_millis(ms))
}

#[cfg(test)]
mod tests;
