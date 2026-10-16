//! A chat's event log on the machine (D-C6): `<state>/chat/<session>/`
//!
//! - `events.ndjson`: append-only, one masked durable event per line, in
//!   seq order. Each line also carries `"o"`: the offset in the agent's
//!   stdout after the line that produced it (0 for the runner's own
//!   events), so a runner that restarts never logs an event twice.
//! - `meta.json`: the session record ([`super::Meta`]).
//!
//! History is kept for [`HISTORY_DAYS`]: older events are dropped from the
//! front of the file (a replay from before them is marked GAP), and a session
//! with nothing newer is removed entirely.

use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use tmuxedo_protocol::chat::HISTORY_DAYS;

pub const HISTORY_MS: u64 = HISTORY_DAYS * 24 * 3600 * 1000;

pub struct EventLog {
    path: PathBuf,
    /// Last seq written (0 = none).
    pub last_seq: u64,
    /// First seq still in the file (0 = none).
    pub first_seq: u64,
    /// Highest `o` written, and how many events carry it (one stdout line
    /// can make several events).
    pub last_offset: u64,
    pub last_offset_count: usize,
}

/// Milliseconds since the epoch of an ISO timestamp the runner wrote
/// (`2026-10-07T08:00:00.000Z`); None for anything else.
pub fn parse_iso_ms(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let n = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d, h, mi, se) = (n(0..4)?, n(5..7)?, n(8..10)?, n(11..13)?, n(14..16)?, n(17..19)?);
    let ms = if b.get(19) == Some(&b'.') {
        n(20..23).unwrap_or(0)
    } else {
        0
    };
    // Days from civil (Howard Hinnant's algorithm).
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = if y2 >= 0 { y2 } else { y2 - 399 } / 400;
    let yoe = y2 - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let t = ((days * 24 + h) * 60 + mi) * 60 + se;
    u64::try_from(t * 1000 + ms).ok()
}

impl EventLog {
    pub fn open(dir: &Path) -> EventLog {
        let path = dir.join("events.ndjson");
        let mut log = EventLog {
            path,
            last_seq: 0,
            first_seq: 0,
            last_offset: 0,
            last_offset_count: 0,
        };
        log.scan();
        log
    }

    fn scan(&mut self) {
        let (mut first, mut last) = (0, 0);
        self.last_offset = 0;
        self.last_offset_count = 0;
        for v in self.lines() {
            let seq = v["seq"].as_u64().unwrap_or(0);
            if first == 0 {
                first = seq;
            }
            last = last.max(seq);
            self.note_offset(v["o"].as_u64().unwrap_or(0));
        }
        self.first_seq = first;
        self.last_seq = last;
    }

    /// Every parsable line (a torn last line from a crash is skipped).
    fn lines(&self) -> impl Iterator<Item = Value> {
        let f = std::fs::File::open(&self.path).ok();
        f.into_iter()
            .flat_map(|f| BufReader::new(f).lines().map_while(Result::ok))
            .filter_map(|l| serde_json::from_str::<Value>(&l).ok())
    }

    /// Append one event (already masked and numbered), with its source offset.
    pub fn append(&mut self, event: &Value, offset: u64) -> std::io::Result<()> {
        let mut line = event.clone();
        if offset > 0 {
            line["o"] = Value::from(offset);
        }
        let mut bytes = serde_json::to_vec(&line)?;
        bytes.push(b'\n');
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&self.path)?;
        f.write_all(&bytes)?;
        f.sync_data()?;
        let seq = event["seq"].as_u64().unwrap_or(0);
        if self.first_seq == 0 {
            self.first_seq = seq;
        }
        self.last_seq = self.last_seq.max(seq);
        self.note_offset(offset);
        Ok(())
    }

    fn note_offset(&mut self, o: u64) {
        if o == 0 {
            return;
        }
        if o == self.last_offset {
            self.last_offset_count += 1;
        } else if o > self.last_offset {
            self.last_offset = o;
            self.last_offset_count = 1;
        }
    }

    /// How many durable events of the stdout line `src` are already logged
    /// (a runner that restarted handles the line again): all of an earlier
    /// line's, the logged part of the last one, none of a new one.
    pub fn already_logged(&self, src: u64) -> usize {
        match src.cmp(&self.last_offset) {
            std::cmp::Ordering::Less => usize::MAX,
            std::cmp::Ordering::Equal => self.last_offset_count,
            std::cmp::Ordering::Greater => 0,
        }
    }

    /// Events with `seq >= from`, at most `limit`, without the internal `o`.
    pub fn read(&self, from: u64, limit: usize) -> Vec<Value> {
        self.lines()
            .filter(|v| v["seq"].as_u64().unwrap_or(0) >= from)
            .take(limit)
            .map(|mut v| {
                if let Some(o) = v.as_object_mut() {
                    o.remove("o");
                }
                v
            })
            .collect()
    }

    /// The `at` of the newest event, in ms.
    pub fn last_at_ms(&self) -> Option<u64> {
        self.lines().filter_map(|v| parse_iso_ms(v["at"].as_str()?)).max()
    }

    /// Drop events older than the history window. Returns (dropped, left).
    pub fn prune(&mut self, now_ms: u64) -> std::io::Result<(usize, usize)> {
        let cutoff = now_ms.saturating_sub(HISTORY_MS);
        let all: Vec<Value> = self.lines().collect();
        let keep: Vec<&Value> = all
            .iter()
            .filter(|v| v["at"].as_str().and_then(parse_iso_ms).is_none_or(|t| t >= cutoff))
            .collect();
        let dropped = all.len() - keep.len();
        if dropped > 0 {
            let tmp = self.path.with_extension("ndjson.tmp");
            let mut f = std::fs::File::create(&tmp)?;
            for v in &keep {
                let mut b = serde_json::to_vec(v)?;
                b.push(b'\n');
                f.write_all(&b)?;
            }
            f.sync_all()?;
            std::fs::rename(&tmp, &self.path)?;
            self.first_seq = keep.first().and_then(|v| v["seq"].as_u64()).unwrap_or(0);
        }
        Ok((dropped, keep.len()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn iso_parse_matches_the_runner_clock() {
        assert_eq!(parse_iso_ms("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(parse_iso_ms("2026-10-07T08:00:00.123Z"), Some(1_791_360_000_123));
        let now = crate::util::now_ms();
        assert_eq!(parse_iso_ms(&crate::util::iso(now)), Some(now));
        assert_eq!(parse_iso_ms("yesterday"), None);
    }

    #[test]
    fn append_read_reopen_and_prune() {
        let d = tempfile::tempdir().unwrap();
        let mut log = EventLog::open(d.path());
        assert_eq!((log.first_seq, log.last_seq), (0, 0));
        let day = 24 * 3600 * 1000;
        let now = 100 * day;
        for (seq, t) in [(1, now - 40 * day), (2, now - 31 * day), (3, now - 29 * day), (4, now)] {
            let ev = json!({ "seq": seq, "at": crate::util::iso(t), "sessionId": "c-abcdefgh", "type": "notice" });
            log.append(&ev, seq * 10).unwrap();
        }
        // A torn last line (a crash mid-write) is skipped.
        std::fs::OpenOptions::new()
            .append(true)
            .open(d.path().join("events.ndjson"))
            .unwrap()
            .write_all(b"{\"seq\":5,\"at")
            .unwrap();
        let log2 = EventLog::open(d.path());
        assert_eq!((log2.first_seq, log2.last_seq, log2.last_offset), (1, 4, 40));
        assert_eq!(log2.already_logged(30), usize::MAX);
        assert_eq!(log2.already_logged(40), 1);
        assert_eq!(log2.already_logged(50), 0);
        let r = log2.read(2, 2);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0]["seq"], 2);
        assert!(r[0].get("o").is_none(), "the source offset is internal");
        let mut log3 = log2;
        assert_eq!(log3.prune(now).unwrap(), (2, 2));
        assert_eq!(log3.first_seq, 3);
        assert_eq!(log3.read(0, 10).len(), 2);
        assert_eq!(log3.last_at_ms(), Some(now));
    }
}
