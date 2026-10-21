//! The watchdog-linked memory warning (docs/design/machine-hud.md §6): when
//! the cgroup the memory watchdog guards stays at or above 85 % of its limit,
//! the pane the watchdog would stop at 90 % gets a needs-you item naming the
//! process, so the person can act first. One alert system: the item goes
//! through the needs path like any other (web pushes it and shows it in
//! Needs you, and the pane header draws its chip).
//!
//! At most once per [`QUIET`]; it stays open until memory falls below
//! [`CLEAR`] or the person dismisses it, and follows the victim if another
//! pane becomes the largest. `config.memoryWarn: false` turns it off.
//!
//! Only where the watchdog itself runs: a numeric cgroup `memory.max` (hosted
//! machines and containers with a limit). A machine without a limit has
//! nothing the watchdog would stop, so nothing to warn about.

use crate::daemon::Daemon;
use crate::watchdog::{self, Assessment};
use serde_json::json;
use std::time::{Duration, Instant};
use tmuxedo_protocol::needs::{NeedsResponse, NeedsYou, MAX_TITLE};

/// At or above [`watchdog::WARN`] this long before the item opens (a build's spike is not a warning).
pub const SUSTAIN: Duration = Duration::from_secs(10);
/// No new warning sooner than this after the last one opened (design: once per 10 min).
pub const QUIET: Duration = Duration::from_secs(600);
/// Below this share of the limit an open warning closes (a little under 85 %, so it does not flicker).
pub const CLEAR: f64 = 0.80;
/// Item ids start with this.
pub const ID_PREFIX: &str = "memory-";

/// The open warning: fixed when it opens (numbers do not tick in the item), the pane moves with the victim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Warning {
    pub id: String,
    pub pane: String,
    pub command: Option<String>,
    pub group_rss: u64,
    pub current: u64,
    pub max: u64,
    pub opened_ms: u64,
}

impl Warning {
    pub fn pct(&self) -> u64 {
        (self.current as f64 * 100.0 / self.max.max(1) as f64)
            .floor()
            .min(100.0) as u64
    }

    /// The needs-you item for this warning, on its pane.
    pub fn item(&self) -> NeedsYou {
        let what = self.command.as_deref().map_or_else(
            || "the largest process in this pane".to_string(),
            |c| format!("{c} in this pane"),
        );
        let stop = (watchdog::THRESHOLD * 100.0) as u64;
        let title = format!("Memory {}% full: {what} would be stopped at {stop}%", self.pct());
        let text = format!(
            "This machine is using {} of its {} memory limit. At {stop}% tmuxedo stops the process using the most \
             ({what}, {}) so the machine keeps running. Close something or save your work.",
            gib(self.current),
            gib(self.max),
            gib(self.group_rss),
        );
        let mut memory = json!({ "current": self.current, "max": self.max, "rss": self.group_rss, "stopAt": stop });
        if let Some(c) = &self.command {
            memory["process"] = json!(c);
        }
        NeedsYou {
            id: self.id.clone(),
            pane_id: self.pane.clone(),
            chat_id: None,
            plugin: crate::needs_files::CORE_PLUGIN.into(),
            kind: "error".into(),
            title: crate::needs::cut(&title, MAX_TITLE),
            detail: Some(json!({ "text": text, "memory": memory })),
            context: None,
            risk: "normal".into(),
            risk_class: None,
            why: None,
            responses: vec![NeedsResponse::new("dismiss", "dismiss", "Not now", "none")],
            answer_via: "none".into(),
            fingerprint: None,
            source: "core".into(),
            created_at: crate::util::iso(self.opened_ms),
            expires_at: None,
        }
    }
}

fn gib(b: u64) -> String {
    let g = b as f64 / (1u64 << 30) as f64;
    if g >= 1.0 {
        format!("{g:.1} GiB")
    } else {
        format!("{} MiB", b >> 20)
    }
}

/// The warning's state: pure over the watchdog's [`Assessment`] and the time.
#[derive(Debug)]
pub struct MemoryWarn {
    enabled: bool,
    above_since: Option<Instant>,
    last_opened: Option<Instant>,
    open: Option<Warning>,
}

impl MemoryWarn {
    pub fn new(enabled: bool) -> Self {
        MemoryWarn {
            enabled,
            above_since: None,
            last_opened: None,
            open: None,
        }
    }

    pub fn open(&self) -> Option<&Warning> {
        self.open.as_ref()
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// On or off; answers the panes whose items changed (turning it off closes an open one).
    pub fn set_enabled(&mut self, on: bool) -> Vec<String> {
        self.enabled = on;
        if on {
            return vec![];
        }
        self.above_since = None;
        self.open.take().map(|w| w.pane).into_iter().collect()
    }

    /// The person said "Not now": close it (the next one waits out [`QUIET`] from this one's opening).
    pub fn dismiss(&mut self, id: &str) -> Option<String> {
        if self.open.as_ref().is_some_and(|w| w.id == id) {
            self.open.take().map(|w| w.pane)
        } else {
            None
        }
    }

    /// One watchdog look (None: no numeric limit). Answers the panes whose items changed.
    pub fn update(&mut self, a: Option<&Assessment>, now: Instant, now_ms: u64) -> Vec<String> {
        let Some(a) = a.filter(|_| self.enabled) else {
            self.above_since = None;
            return self.open.take().map(|w| w.pane).into_iter().collect();
        };
        if !watchdog::at_least(a.current, a.max, CLEAR) {
            self.above_since = None;
            return self.open.take().map(|w| w.pane).into_iter().collect();
        }
        if !watchdog::at_least(a.current, a.max, watchdog::WARN) {
            self.above_since = None; // in between: an open one stays, a new one waits
            return vec![];
        }
        let since = *self.above_since.get_or_insert(now);
        let Some(v) = a.victim.as_ref() else { return vec![] };
        if let Some(w) = self.open.as_mut() {
            if w.pane == v.pane && w.command == v.command {
                return vec![];
            }
            // Another pane (or job) is now the largest: the warning follows it.
            let old = std::mem::replace(&mut w.pane, v.pane.clone());
            w.command = v.command.clone();
            w.group_rss = v.group_rss;
            return if old == w.pane {
                vec![old]
            } else {
                vec![old, w.pane.clone()]
            };
        }
        if now.saturating_duration_since(since) < SUSTAIN
            || self
                .last_opened
                .is_some_and(|t| now.saturating_duration_since(t) < QUIET)
        {
            return vec![];
        }
        self.last_opened = Some(now);
        self.open = Some(Warning {
            id: format!("{ID_PREFIX}{now_ms}"),
            pane: v.pane.clone(),
            command: v.command.clone(),
            group_rss: v.group_rss,
            current: a.current,
            max: a.max,
            opened_ms: now_ms,
        });
        vec![v.pane.clone()]
    }
}

impl Daemon {
    /// After each watchdog look: bring the warning up to date and resend the panes it touched.
    pub(crate) async fn memory_warn_update(&mut self, a: Option<&Assessment>) {
        let was = self.memory_warn.open().map(|w| w.id.clone());
        let touched = self.memory_warn.update(a, Instant::now(), crate::util::now_ms());
        if let Some(w) = self.memory_warn.open().filter(|w| was.as_ref() != Some(&w.id)) {
            crate::log!(
                "memory {}/{} at {}%: warning that {} in {} would be stopped",
                w.current,
                w.max,
                w.pct(),
                w.command.as_deref().unwrap_or("its largest process"),
                w.pane
            );
        }
        for p in touched {
            self.needs_sync(&p).await;
        }
    }

    /// `config.memoryWarn`: the owner's setting, kept in config.json.
    pub(crate) async fn set_memory_warn(&mut self, on: bool) {
        let want = (!on).then_some(false);
        if self.launch.memory_warn != want {
            self.launch.memory_warn = want;
            if let Ok(b) = serde_json::to_vec_pretty(&self.launch) {
                let _ = crate::config::write_private(&self.opts.state_dir.join("config.json"), &b);
            }
            crate::log!("config: memory warning {}", if on { "on" } else { "off" });
        }
        for p in self.memory_warn.set_enabled(on) {
            self.needs_sync(&p).await;
        }
    }

    /// The warning's item when it is on `pane_id` (crate::needs).
    pub(crate) fn memory_warn_items(&self, pane_id: &str) -> Vec<NeedsYou> {
        self.memory_warn
            .open()
            .filter(|w| w.pane == pane_id && self.panes.contains_key(pane_id))
            .map(Warning::item)
            .filter(NeedsYou::is_valid)
            .into_iter()
            .collect()
    }

    /// "Not now" on the warning: Some(outcome) when the item is one.
    pub(crate) async fn memory_warn_respond(&mut self, item: &NeedsYou, r: &NeedsResponse) -> Option<&'static str> {
        if !(item.id.starts_with(ID_PREFIX) && item.source == "core" && item.kind == "error") {
            return None;
        }
        if r.kind != "dismiss" {
            return Some("stale");
        }
        let pane = self.memory_warn.dismiss(&item.id)?;
        self.needs.hint(&item.id, "remote", Some(r.id.clone()));
        crate::log!("memory: warning {} dismissed", item.id);
        self.needs_sync(&pane).await;
        Some("accepted")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::watchdog::Victim;

    const GIB: u64 = 1 << 30;

    fn look(pct: u64, pane: &str, cmd: &str) -> Assessment {
        let (current, max) = (pct * 40_000_000, 4_000_000_000);
        Assessment {
            current,
            max,
            victim: watchdog::at_least(current, max, watchdog::WARN).then(|| Victim {
                pane: pane.into(),
                pgid: 100,
                group_rss: 2 * GIB,
                pane_rss: 2 * GIB,
                current,
                max,
                command: Some(cmd.into()),
            }),
        }
    }

    fn secs(t0: Instant, s: u64) -> Instant {
        t0 + Duration::from_secs(s)
    }

    #[test]
    fn opens_only_after_staying_above_and_names_the_victim() {
        let t0 = Instant::now();
        let mut m = MemoryWarn::new(true);
        assert!(m.update(Some(&look(70, "w1:p1", "cargo")), t0, 1).is_empty());
        assert!(
            m.update(Some(&look(86, "w1:p1", "cargo")), secs(t0, 1), 2).is_empty(),
            "not yet sustained"
        );
        // A dip below 85 % restarts the clock.
        assert!(m.update(Some(&look(83, "w1:p1", "cargo")), secs(t0, 5), 3).is_empty());
        assert!(m.update(Some(&look(87, "w1:p1", "cargo")), secs(t0, 6), 4).is_empty());
        assert!(m.update(Some(&look(87, "w1:p1", "cargo")), secs(t0, 15), 5).is_empty());
        assert_eq!(
            m.update(Some(&look(87, "w1:p1", "cargo")), secs(t0, 16), 6),
            vec!["w1:p1"]
        );
        let it = m.open().unwrap().item();
        assert!(it.is_valid(), "{it:?}");
        assert_eq!(it.id, "memory-6");
        assert_eq!(it.pane_id, "w1:p1");
        assert_eq!(it.title, "Memory 87% full: cargo in this pane would be stopped at 90%");
        assert_eq!(it.detail.as_ref().unwrap()["memory"]["process"], "cargo");
        assert!(it.detail.as_ref().unwrap()["text"]
            .as_str()
            .unwrap()
            .contains("2.0 GiB"));
        // Still above: nothing changes, the numbers do not tick.
        assert!(m.update(Some(&look(89, "w1:p1", "cargo")), secs(t0, 17), 7).is_empty());
        assert_eq!(m.open().unwrap().current, look(87, "", "").current);
    }

    #[test]
    fn closes_below_clear_and_waits_ten_minutes_for_the_next() {
        let t0 = Instant::now();
        let mut m = MemoryWarn::new(true);
        m.update(Some(&look(88, "a", "x")), t0, 1);
        assert_eq!(m.update(Some(&look(88, "a", "x")), secs(t0, 10), 2), vec!["a"]);
        assert!(
            m.update(Some(&look(82, "a", "x")), secs(t0, 11), 3).is_empty(),
            "between 80 and 85: stays"
        );
        assert!(m.open().is_some());
        assert_eq!(m.update(Some(&look(79, "a", "x")), secs(t0, 12), 4), vec!["a"]);
        assert!(m.open().is_none());
        // Up again at once: quiet for 10 minutes from the last opening.
        m.update(Some(&look(90, "a", "x")), secs(t0, 20), 5);
        assert!(m.update(Some(&look(90, "a", "x")), secs(t0, 600), 6).is_empty());
        assert_eq!(m.update(Some(&look(90, "a", "x")), secs(t0, 610), 7), vec!["a"]);
        // The limit going away (None) closes it too.
        assert_eq!(m.update(None, secs(t0, 611), 8), vec!["a"]);
    }

    #[test]
    fn follows_the_victim_and_can_be_dismissed_or_turned_off() {
        let t0 = Instant::now();
        let mut m = MemoryWarn::new(true);
        m.update(Some(&look(88, "a", "x")), t0, 1);
        m.update(Some(&look(88, "a", "x")), secs(t0, 10), 2);
        let id = m.open().unwrap().id.clone();
        assert_eq!(m.update(Some(&look(88, "b", "y")), secs(t0, 11), 3), vec!["a", "b"]);
        assert_eq!(
            (m.open().unwrap().pane.as_str(), m.open().unwrap().id.clone()),
            ("b", id.clone())
        );
        assert_eq!(
            m.update(Some(&look(88, "b", "z")), secs(t0, 12), 4),
            vec!["b"],
            "same pane, other job"
        );
        assert_eq!(m.dismiss("memory-other"), None);
        assert_eq!(m.dismiss(&id).as_deref(), Some("b"));
        assert!(
            m.update(Some(&look(88, "b", "z")), secs(t0, 100), 5).is_empty(),
            "quiet after Not now"
        );
        // Off: closes and stays closed; nothing opens.
        m.update(Some(&look(88, "b", "z")), secs(t0, 700), 6);
        m.update(Some(&look(88, "b", "z")), secs(t0, 710), 7);
        assert!(m.open().is_some());
        assert_eq!(m.set_enabled(false), vec!["b"]);
        assert!(m.update(Some(&look(95, "b", "z")), secs(t0, 2000), 8).is_empty());
        assert!(m.update(Some(&look(95, "b", "z")), secs(t0, 2100), 9).is_empty());
        assert!(m.set_enabled(true).is_empty());
    }

    #[test]
    fn an_unnamed_victim_still_gets_a_valid_item() {
        let w = Warning {
            id: "memory-1".into(),
            pane: "w1:p1".into(),
            command: None,
            group_rss: 300 << 20,
            current: 870,
            max: 1000,
            opened_ms: 1,
        };
        let it = w.item();
        assert!(it.is_valid());
        assert_eq!(
            it.title,
            "Memory 87% full: the largest process in this pane would be stopped at 90%"
        );
        assert!(it.detail.unwrap()["text"].as_str().unwrap().contains("300 MiB"));
    }
}
