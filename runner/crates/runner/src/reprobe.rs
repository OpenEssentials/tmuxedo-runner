//! A version probe that did not answer is asked again, in the background.
//!
//! The start-up probe (`<agent.version_cmd>`, 5 s) decides whether login
//! tracking, the chat offer and the version gates are on. A slow disk or a
//! cold start must not turn them off for the runner's whole life: an unknown
//! result is retried on a backoff, and once when the CLI is first seen
//! starting in a pane. A program that is not there (ENOENT) is not a slow one:
//! it is looked for only at the backoff's cap and on that first sighting.

use crate::plugin::Probe;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The waits between probes of an unknown result; the last repeats.
pub const SCHEDULE: [Duration; 4] = [
    Duration::from_secs(30),
    Duration::from_secs(120),
    Duration::from_secs(600),
    Duration::from_secs(1800),
];

/// Knobs for tests.
#[derive(Debug, Clone)]
pub struct Timing {
    pub schedule: Vec<Duration>,
    pub limit: Duration,
    /// Replaces Claude's `version_cmd` (a fake CLI that is slow once).
    pub claude_argv: Option<Vec<String>>,
}

impl Default for Timing {
    fn default() -> Self {
        Timing {
            schedule: SCHEDULE.to_vec(),
            limit: crate::plugin::VERSION_PROBE_LIMIT,
            claude_argv: None,
        }
    }
}

struct Entry {
    absent: bool,
    in_flight: bool,
    /// Probes that found no version so far.
    attempts: usize,
    next_at: Instant,
    seen: bool,
}

pub type Done = Arc<Mutex<Vec<(String, Probe)>>>;

pub struct Reprobe {
    pub timing: Timing,
    /// CLIs whose version is still unknown.
    pending: BTreeMap<String, Entry>,
    pub done: Done,
}

impl Reprobe {
    pub fn new(timing: Timing) -> Reprobe {
        Reprobe {
            timing,
            pending: BTreeMap::new(),
            done: Done::default(),
        }
    }

    fn wait(&self, i: usize) -> Duration {
        let s = &self.timing.schedule;
        s.get(i).or(s.last()).copied().unwrap_or(SCHEDULE[3])
    }

    /// Record a probe's result; a version needs no more asking.
    pub fn note(&mut self, id: &str, p: &Probe, now: Instant) {
        if matches!(p, Probe::Version(_)) {
            self.pending.remove(id);
            return;
        }
        let (prev, seen) = self.pending.get(id).map_or((0, false), |e| (e.attempts, e.seen));
        let attempts = prev + 1;
        let absent = *p == Probe::Absent;
        let after = if absent {
            self.timing.schedule.last().copied().unwrap_or(SCHEDULE[3])
        } else {
            self.wait(attempts - 1)
        };
        self.pending.insert(
            id.to_string(),
            Entry {
                absent,
                in_flight: false,
                attempts,
                next_at: now + after,
                seen,
            },
        );
    }

    /// The CLI was seen starting in a pane: ask once now, if its version is unknown.
    pub fn seen_in_pane(&mut self, id: &str, now: Instant) {
        if let Some(e) = self.pending.get_mut(id) {
            if !e.seen {
                e.seen = true;
                if !e.in_flight {
                    e.next_at = now;
                }
            }
        }
    }

    /// The CLIs to probe now; each is marked in flight (one probe per CLI at a time).
    pub fn due(&mut self, now: Instant) -> Vec<String> {
        let mut out = vec![];
        for (id, e) in &mut self.pending {
            if !e.in_flight && e.next_at <= now {
                e.in_flight = true;
                out.push(id.clone());
            }
        }
        out
    }

    /// Finished probes since the last call.
    pub fn take_done(&mut self) -> Vec<(String, Probe)> {
        let v = self
            .done
            .lock()
            .map(|mut v| std::mem::take(&mut *v))
            .unwrap_or_default();
        for (id, _) in &v {
            if let Some(e) = self.pending.get_mut(id) {
                e.in_flight = false;
            }
        }
        v
    }

    pub fn is_pending(&self, id: &str) -> bool {
        self.pending.contains_key(id)
    }

    pub fn is_absent(&self, id: &str) -> bool {
        self.pending.get(id).is_some_and(|e| e.absent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(secs: u64) -> Duration {
        Duration::from_secs(secs)
    }

    fn finish(r: &mut Reprobe, id: &str, p: Probe, now: Instant) {
        r.done.lock().unwrap().push((id.into(), p.clone()));
        assert_eq!(r.take_done().len(), 1);
        r.note(id, &p, now);
    }

    #[test]
    fn an_unknown_result_backs_off_and_caps() {
        let mut r = Reprobe::new(Timing::default());
        let t0 = Instant::now();
        r.note("claude", &Probe::Unknown, t0);
        assert!(r.due(t0 + t(29)).is_empty());
        assert_eq!(r.due(t0 + t(30)), ["claude"]);
        // One in flight: not asked again meanwhile.
        assert!(r.due(t0 + t(99999)).is_empty());
        let mut now = t0 + t(30);
        for want in [120, 600, 1800, 1800, 1800] {
            finish(&mut r, "claude", Probe::Unknown, now);
            assert!(r.due(now + t(want - 1)).is_empty(), "{want}");
            assert_eq!(r.due(now + t(want)), ["claude"], "{want}");
            now += t(want);
        }
    }

    #[test]
    fn a_version_ends_it_and_a_missing_program_waits_for_the_cap() {
        let mut r = Reprobe::new(Timing::default());
        let t0 = Instant::now();
        r.note("claude", &Probe::Unknown, t0);
        r.note("claude", &Probe::Version("2.1.0".into()), t0);
        assert!(!r.is_pending("claude"));
        assert!(r.due(t0 + t(99999)).is_empty());
        r.note("codex", &Probe::Absent, t0);
        assert!(r.is_absent("codex"));
        assert!(r.due(t0 + t(1799)).is_empty());
        assert_eq!(r.due(t0 + t(1800)), ["codex"]);
    }

    #[test]
    fn first_sighting_in_a_pane_asks_once() {
        let mut r = Reprobe::new(Timing::default());
        let t0 = Instant::now();
        r.note("codex", &Probe::Absent, t0);
        r.seen_in_pane("codex", t0 + t(1));
        assert_eq!(r.due(t0 + t(1)), ["codex"]);
        finish(&mut r, "codex", Probe::Absent, t0 + t(2));
        r.seen_in_pane("codex", t0 + t(3));
        assert!(r.due(t0 + t(4)).is_empty(), "a second sighting is not a hot loop");
    }

    /// A fake CLI built in a temp dir: slow on the first call, quick after.
    fn slow_once(dir: &std::path::Path) -> Vec<String> {
        use std::os::unix::fs::PermissionsExt;
        let script = dir.join("fake-cli");
        let marker = dir.join("seen");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nif [ ! -e '{m}' ]; then : > '{m}'; exec sleep 5; fi\necho '2.1.300 (fake)'\n",
                m = marker.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        vec![script.display().to_string(), "--version".into()]
    }

    #[tokio::test]
    async fn a_slow_first_call_is_unknown_not_absent_and_it_costs_only_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        let argv = slow_once(dir.path());
        let claude = crate::plugin::claude();
        let limit = Duration::from_millis(300);
        let t0 = Instant::now();
        let first = crate::plugin::probe_agent(claude, Some(&argv), limit).await;
        assert_eq!(first, Probe::Unknown);
        assert!(
            t0.elapsed() < Duration::from_secs(3),
            "start-up pays the limit, not the sleep"
        );
        let second = crate::plugin::probe_agent(claude, Some(&argv), limit).await;
        assert_eq!(second, Probe::Version("2.1.300".into()));
    }

    #[tokio::test]
    async fn a_missing_program_is_absent_and_is_not_asked_in_a_hot_loop() {
        let dir = tempfile::tempdir().unwrap();
        let argv = vec![dir.path().join("nope").display().to_string(), "--version".into()];
        let p = crate::plugin::probe_agent(crate::plugin::claude(), Some(&argv), Duration::from_secs(1)).await;
        assert_eq!(p, Probe::Absent);
        let mut r = Reprobe::new(Timing::default());
        let t0 = Instant::now();
        r.note("claude", &p, t0);
        // A thousand slow ticks (5 s apart) over the first 80 minutes: two probes at most.
        let mut asked = 0;
        for i in 1..=1000u64 {
            let now = t0 + t(5 * i);
            for id in r.due(now) {
                asked += 1;
                finish(&mut r, &id, Probe::Absent, now);
            }
        }
        assert!(asked <= 3, "asked {asked} times");
    }

    #[test]
    fn a_sighting_during_a_probe_in_flight_does_not_start_another() {
        let mut r = Reprobe::new(Timing::default());
        let t0 = Instant::now();
        r.note("claude", &Probe::Unknown, t0);
        assert_eq!(r.due(t0 + t(30)).len(), 1);
        r.seen_in_pane("claude", t0 + t(31));
        assert!(r.due(t0 + t(32)).is_empty());
    }
}
