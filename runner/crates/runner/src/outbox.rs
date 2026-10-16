//! Durable `event` outbox (docs/protocol.md, `events_ack`).
//!
//! Every event is appended to `events.jsonl` in the state directory and
//! fsynced before it is sent, so it survives a runner crash or a holder
//! restart. Events stay queued until the gateway acknowledges them with
//! `events_ack {upTo}`; the runner resends everything unacknowledged after
//! every `welcome` (web ignores duplicates by `(machineId, seq)`). An ack
//! compacts the file: the remaining events are written to a temporary file,
//! fsynced, and renamed over it. A torn last line (crash mid-append) is
//! skipped on load.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use tmuxedo_protocol::frames::RunnerFrame;

/// More than this many unacknowledged events (a gateway that never acks)
/// drops the oldest, so the file cannot grow without bound.
pub const MAX_QUEUED: usize = 10_000;

pub struct Outbox {
    path: PathBuf,
    queue: VecDeque<(u64, RunnerFrame)>,
    /// Highest seq sent on the current connection (0 = none yet).
    sent_through: u64,
}

impl Outbox {
    /// Load the queue from `path` (missing file = empty).
    pub fn open(path: &Path) -> Self {
        let mut queue = VecDeque::new();
        if let Ok(f) = File::open(path) {
            for line in BufReader::new(f).lines() {
                let Ok(line) = line else { break };
                match serde_json::from_str::<RunnerFrame>(&line) {
                    Ok(f @ RunnerFrame::Event { .. }) => {
                        let RunnerFrame::Event { seq, .. } = &f else {
                            unreachable!()
                        };
                        let seq = *seq;
                        if queue.back().is_none_or(|(s, _)| *s < seq) {
                            queue.push_back((seq, f));
                        }
                    }
                    _ => crate::log!("events outbox: skipped an unreadable line"),
                }
            }
        }
        Self {
            path: path.to_path_buf(),
            queue,
            sent_through: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.queue.len()
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Highest seq on disk (so a restarted runner never reuses one).
    pub fn max_seq(&self) -> u64 {
        self.queue.back().map_or(0, |(s, _)| *s)
    }

    /// Append an event and fsync it before it may be sent.
    pub fn push(&mut self, frame: RunnerFrame) {
        let RunnerFrame::Event { seq, .. } = &frame else {
            return;
        };
        let seq = *seq;
        let line = format!("{}\n", serde_json::to_string(&frame).unwrap_or_default());
        let appended = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .and_then(|mut f| {
                f.write_all(line.as_bytes())?;
                f.sync_data()
            });
        if let Err(e) = appended {
            crate::log!("events outbox: append failed: {e}");
        }
        self.queue.push_back((seq, frame));
        if self.queue.len() > MAX_QUEUED {
            let drop = self.queue.len() - MAX_QUEUED;
            crate::log!("events outbox: {drop} unacknowledged events dropped (over {MAX_QUEUED})");
            self.queue.drain(..drop);
            self.compact();
        }
    }

    /// A new connection: nothing has been sent on it yet.
    pub fn reset_sent(&mut self) {
        self.sent_through = 0;
    }

    /// Events not yet sent on this connection; marks them sent.
    pub fn take_unsent(&mut self) -> Vec<RunnerFrame> {
        let from = self.sent_through;
        let out: Vec<RunnerFrame> = self
            .queue
            .iter()
            .filter(|(s, _)| *s > from)
            .map(|(_, f)| f.clone())
            .collect();
        if let Some((s, _)) = self.queue.back() {
            self.sent_through = self.sent_through.max(*s);
        }
        out
    }

    /// `welcome.lastEventSeq`: if any queued event has `seq <= floor` (web
    /// already stored that seq, from a runner that lost its state), renumber
    /// the whole queue in order from `next()` and rewrite the file. Events
    /// above `floor` cannot be stored yet, so renumbering them is harmless
    /// and keeps their order. Returns how many events were renumbered.
    pub fn renumber_at_or_below(&mut self, floor: u64, mut next: impl FnMut() -> u64) -> usize {
        if self.queue.front().is_none_or(|(s, _)| *s > floor) {
            return 0;
        }
        for (seq, f) in self.queue.iter_mut() {
            let n = next();
            *seq = n;
            if let RunnerFrame::Event { seq, .. } = f {
                *seq = n;
            }
        }
        self.sent_through = 0;
        self.compact();
        self.queue.len()
    }

    /// `events_rejected`: move the queued events with these seqs to the end
    /// of the queue under new seqs from `next()` (in their original order)
    /// and rewrite the file; they are sent again on this connection. An ack
    /// for the old numbers then no longer touches them. Returns how many.
    pub fn renumber_rejected(&mut self, seqs: &[u64], mut next: impl FnMut() -> u64) -> usize {
        let (rejected, keep): (Vec<_>, Vec<_>) = self.queue.drain(..).partition(|(s, _)| seqs.contains(s));
        self.queue = keep.into();
        let n = rejected.len();
        for (_, mut f) in rejected {
            let s = next();
            if let RunnerFrame::Event { seq, .. } = &mut f {
                *seq = s;
            }
            self.queue.push_back((s, f));
        }
        if n > 0 {
            self.compact();
        }
        n
    }

    /// Drop every event with `seq <= up_to` and compact the file.
    pub fn ack(&mut self, up_to: u64) {
        let before = self.queue.len();
        while self.queue.front().is_some_and(|(s, _)| *s <= up_to) {
            self.queue.pop_front();
        }
        if self.queue.len() != before {
            self.compact();
        }
    }

    fn compact(&self) {
        let tmp = self.path.with_extension("jsonl.tmp");
        let written = (|| -> std::io::Result<()> {
            let mut f = File::create(&tmp)?;
            for (_, fr) in &self.queue {
                writeln!(f, "{}", serde_json::to_string(fr).unwrap_or_default())?;
            }
            f.sync_all()?;
            std::fs::rename(&tmp, &self.path)?;
            if let Some(dir) = self.path.parent() {
                File::open(dir)?.sync_all()?;
            }
            Ok(())
        })();
        if let Err(e) = written {
            crate::log!("events outbox: compaction failed: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ev(seq: u64) -> RunnerFrame {
        RunnerFrame::Event {
            seq,
            pane_id: "w1:p1".into(),
            kind: "done".into(),
            at: "2026-10-03T00:00:00Z".into(),
            data: json!({"kind": "claude"}),
        }
    }

    fn seqs(v: &[RunnerFrame]) -> Vec<u64> {
        v.iter()
            .map(|f| match f {
                RunnerFrame::Event { seq, .. } => *seq,
                _ => 0,
            })
            .collect()
    }

    #[test]
    fn survives_reopen_resends_after_reconnect_and_ack_trims_the_file() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("events.jsonl");
        let mut o = Outbox::open(&p);
        for s in 1..=4 {
            o.push(ev(s));
        }
        assert_eq!(seqs(&o.take_unsent()), [1, 2, 3, 4]);
        assert!(o.take_unsent().is_empty(), "sent once per connection");
        o.reset_sent();
        assert_eq!(
            seqs(&o.take_unsent()),
            [1, 2, 3, 4],
            "unacked events resend after welcome"
        );

        // Crash: a fresh process reads the same queue back.
        let mut o = Outbox::open(&p);
        assert_eq!(o.max_seq(), 4);
        o.ack(2);
        assert_eq!(seqs(&o.take_unsent()), [3, 4]);
        assert_eq!(std::fs::read_to_string(&p).unwrap().lines().count(), 2, "ack compacts");
        o.ack(2); // duplicate ack is harmless
        o.ack(9);
        assert!(o.is_empty());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "");
        assert!(Outbox::open(&p).is_empty());
    }

    #[test]
    fn renumbers_the_queue_above_web_s_last_seq() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("events.jsonl");
        let mut o = Outbox::open(&p);
        for s in [2, 3, 8] {
            o.push(ev(s));
        }
        let mut local = 8;
        assert_eq!(o.renumber_at_or_below(1, || unreachable!()), 0, "nothing collides");
        let n = o.renumber_at_or_below(5, || {
            local += 1;
            local
        });
        assert_eq!(n, 3);
        assert_eq!(seqs(&o.take_unsent()), [9, 10, 11], "order kept");
        assert_eq!(seqs(&Outbox::open(&p).take_unsent()), [9, 10, 11], "file rewritten");
    }

    #[test]
    fn rejected_seqs_move_to_the_end_under_new_numbers() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("events.jsonl");
        let mut o = Outbox::open(&p);
        for s in 1..=4 {
            o.push(ev(s));
        }
        assert_eq!(seqs(&o.take_unsent()), [1, 2, 3, 4]);
        let mut local = 10;
        let n = o.renumber_rejected(&[2, 3, 99], || {
            local += 1;
            local
        });
        assert_eq!(n, 2);
        assert_eq!(
            seqs(&o.take_unsent()),
            [11, 12],
            "only the renumbered ones are sent again"
        );
        o.ack(4); // an ack covering the old numbers keeps them
        assert_eq!(o.len(), 2);
        assert_eq!(Outbox::open(&p).max_seq(), 12, "file rewritten");
    }

    #[test]
    fn torn_last_line_is_skipped() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("events.jsonl");
        let mut o = Outbox::open(&p);
        o.push(ev(1));
        o.push(ev(2));
        let mut f = OpenOptions::new().append(true).open(&p).unwrap();
        f.write_all(br#"{"t":"event","seq":3,"pa"#).unwrap();
        let mut o = Outbox::open(&p);
        assert_eq!(seqs(&o.take_unsent()), [1, 2]);
    }

    #[test]
    fn caps_the_queue() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("events.jsonl");
        let mut o = Outbox::open(&p);
        for s in 1..=(MAX_QUEUED as u64 + 5) {
            o.queue.push_back((s, ev(s)));
        }
        o.push(ev(MAX_QUEUED as u64 + 6));
        assert_eq!(o.len(), MAX_QUEUED);
        assert_eq!(seqs(&o.take_unsent())[0], 7);
        assert_eq!(Outbox::open(&p).len(), MAX_QUEUED);
    }
}
