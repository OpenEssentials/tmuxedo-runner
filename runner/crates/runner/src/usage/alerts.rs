//! Usage alerts (docs/design/agent-usage.md §5): a needs-you item, and so a
//! push, when a window of a plan reaches 80 %, 95 % and its limit. Once per
//! window: a level already told stays told until the window resets (kept in
//! `<state>/usage-alerts.json`, so a runner restart does not tell it again).
//! Each level can be turned off (`config.plugins[].settings.usage.alerts`).
//!
//! At the limit the item offers "Resume at reset": the agents of that plugin
//! that were stopped get `continue` typed into their pane a moment after the
//! window resets, once. Nothing else is typed, and only into a pane whose
//! agent is idle with nothing open for a decision (the daemon checks).

use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use tmuxedo_protocol::needs::{NeedsResponse, NeedsYou};
use tmuxedo_protocol::usage::{UsageRecord, USAGE_ALERT_LEVELS};

/// How long after the reset a planned resume waits (the vendor's clock and ours differ a little).
pub const RESUME_AFTER_S: u64 = 30;
/// A planned resume this long past its time is dropped (the machine was off): it would surprise.
pub const STALE_RESUME_S: u64 = 6 * 3600;
/// Two reset times this close are the same window (a vendor may round differently per report).
const SAME_WINDOW_S: u64 = 15 * 60;
/// What is typed into a resumed agent's pane.
pub const RESUME_TEXT: &str = "continue";
/// At most this many windows are remembered (plugins × windows).
const SENT_MAX: usize = 64;

/// One open alert: the highest level a window reached this time round.
#[derive(Debug, Clone, PartialEq)]
pub struct Alert {
    pub plugin: String,
    pub window: String,
    pub window_mins: Option<u32>,
    /// 80, 95 or 100 (the limit).
    pub level: u8,
    pub used_pct: f64,
    pub resets_at: Option<u64>,
    /// Opened by this update (the caller logs it; the needs-you set does the rest).
    pub fresh: bool,
    /// At the limit with a resume planned: how many agents.
    pub resume: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Sent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    resets_at: Option<u64>,
    level: u8,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Resume {
    at: u64,
    panes: Vec<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Saved {
    #[serde(default)]
    sent: BTreeMap<String, Sent>,
    #[serde(default)]
    resume: BTreeMap<String, Resume>,
}

#[derive(Debug, Default)]
pub struct Alerts {
    path: Option<PathBuf>,
    saved: Saved,
    /// The windows whose alert was opened by this process (a restart shows none again).
    shown: BTreeSet<String>,
}

fn key(plugin: &str, window: &str) -> String {
    format!("{plugin} {window}")
}

fn same_window(a: Option<u64>, b: Option<u64>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a.abs_diff(b) <= SAME_WINDOW_S,
        (None, None) => true,
        _ => false,
    }
}

impl Alerts {
    pub fn load(state_dir: &Path) -> Alerts {
        let path = state_dir.join("usage-alerts.json");
        let saved = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Alerts {
            path: Some(path),
            saved,
            shown: BTreeSet::new(),
        }
    }

    fn save(&self) {
        if let (Some(p), Ok(b)) = (&self.path, serde_json::to_vec(&self.saved)) {
            let _ = crate::config::write_private(p, &b);
        }
    }

    /// Bring the alerts up to date with the newest records and return the
    /// open ones (one per window at most). `levels(plugin)`: the levels on.
    pub fn update(&mut self, records: &[UsageRecord], levels: impl Fn(&str) -> Vec<u8>, now_s: u64) -> Vec<Alert> {
        let before = serde_json::to_vec(&self.saved).ok();
        let mut out = vec![];
        let mut live = BTreeSet::new();
        for r in records {
            let on = levels(&r.plugin);
            for w in &r.windows {
                if w.resets_at.is_some_and(|t| t <= now_s) {
                    continue;
                }
                let k = key(&r.plugin, &w.id);
                live.insert(k.clone());
                // A new window (another reset time, or one without a reset time that fell back) starts over.
                let stale = self.saved.sent.get(&k).is_some_and(|s| {
                    !same_window(s.resets_at, w.resets_at) || (w.resets_at.is_none() && w.used_pct < 80.0)
                });
                if stale {
                    self.saved.sent.remove(&k);
                    self.shown.remove(&k);
                }
                let reached = USAGE_ALERT_LEVELS
                    .iter()
                    .copied()
                    .filter(|l| on.contains(l) && w.used_pct >= f64::from(*l))
                    .max();
                let told = self.saved.sent.get(&k).map(|s| s.level);
                let mut fresh = false;
                if let Some(l) = reached.filter(|l| told.is_none_or(|t| *l > t)) {
                    if told.is_some() || self.saved.sent.len() < SENT_MAX {
                        self.saved.sent.insert(
                            k.clone(),
                            Sent {
                                resets_at: w.resets_at,
                                level: l,
                            },
                        );
                        self.shown.insert(k.clone());
                        fresh = true;
                    }
                }
                let Some(level) = self.saved.sent.get(&k).map(|s| s.level) else {
                    continue;
                };
                if !self.shown.contains(&k) || !on.contains(&level) {
                    continue;
                }
                let resume = (level == 100)
                    .then(|| self.saved.resume.get(&r.plugin).map(|p| p.panes.len()))
                    .flatten();
                out.push(Alert {
                    plugin: r.plugin.clone(),
                    window: w.id.clone(),
                    window_mins: w.window_mins,
                    level,
                    used_pct: w.used_pct,
                    resets_at: w.resets_at,
                    fresh,
                    resume,
                });
            }
        }
        // A window that is no longer reported (its reset passed, or usage went off) is done.
        self.saved
            .sent
            .retain(|k, s| live.contains(k) && s.resets_at.is_none_or(|t| t > now_s));
        self.shown.retain(|k| self.saved.sent.contains_key(k));
        if serde_json::to_vec(&self.saved).ok() != before {
            self.save();
        }
        out
    }

    /// "Resume at reset": the panes to resume once `at` (Unix seconds) has passed.
    pub fn plan_resume(&mut self, plugin: &str, at: u64, panes: Vec<String>) -> bool {
        if panes.is_empty() {
            return false;
        }
        self.saved.resume.insert(plugin.to_string(), Resume { at, panes });
        self.save();
        true
    }

    pub fn cancel_resume(&mut self, plugin: &str) {
        if self.saved.resume.remove(plugin).is_some() {
            self.save();
        }
    }

    pub fn resume_planned(&self, plugin: &str) -> bool {
        self.saved.resume.contains_key(plugin)
    }

    /// The resumes whose time has come (each once); one far past its time is dropped.
    pub fn resume_due(&mut self, now_s: u64) -> Vec<(String, Vec<String>)> {
        let due: Vec<String> = self
            .saved
            .resume
            .iter()
            .filter(|(_, r)| now_s >= r.at + RESUME_AFTER_S)
            .map(|(k, _)| k.clone())
            .collect();
        if due.is_empty() {
            return vec![];
        }
        let mut out = vec![];
        for k in due {
            if let Some(r) = self.saved.resume.remove(&k) {
                if now_s <= r.at + STALE_RESUME_S {
                    out.push((k, r.panes));
                }
            }
        }
        self.save();
        out
    }
}

/// `5h` → "5-hour", `7d` → "weekly", `43200m` → "30-day".
pub fn window_label(id: &str, mins: Option<u32>) -> String {
    let mins = mins.or(match id {
        "5h" => Some(300),
        "7d" => Some(10_080),
        _ => None,
    });
    match mins {
        Some(10_080) => "weekly".into(),
        Some(m) if m % 1440 == 0 => format!("{}-day", m / 1440),
        Some(m) if m % 60 == 0 => format!("{}-hour", m / 60),
        Some(m) => format!("{m}-minute"),
        None => id.to_string(),
    }
}

/// An alert's needs-you item id: one per plugin, window and level.
pub fn item_id(a: &Alert) -> String {
    format!("usage-{}-{}-{}", a.plugin, a.window, a.level).replace(' ', "_")
}

/// The machine-level needs-you items for the open alerts. `name(plugin)`:
/// the agent's name ("Claude").
pub fn items(alerts: &[Alert], name: impl Fn(&str) -> String, created_at: &str) -> Vec<NeedsYou> {
    alerts
        .iter()
        .map(|a| {
            let what = format!("{} {} limit", name(&a.plugin), window_label(&a.window, a.window_mins));
            let title = match (a.level, a.resume) {
                (100, Some(1)) => format!("{what} reached · 1 agent resumes at the reset"),
                (100, Some(n)) => format!("{what} reached · {n} agents resume at the reset"),
                (100, None) => format!("{what} reached"),
                (l, _) => format!("{what} at {l}%"),
            };
            let responses = match (a.level, a.resume, a.resets_at) {
                (100, Some(_), _) => vec![
                    NeedsResponse::new("keep", "deny", "Don't resume", "control"),
                    NeedsResponse::new("dismiss", "dismiss", "OK", "none"),
                ],
                (100, None, Some(_)) => vec![
                    NeedsResponse::new("resume", "approve", "Resume at reset", "control"),
                    NeedsResponse::new("dismiss", "dismiss", "Not now", "none"),
                ],
                _ => vec![NeedsResponse::new("dismiss", "dismiss", "OK", "none")],
            };
            let mut usage = json!({ "window": a.window, "level": a.level, "usedPct": a.used_pct });
            if let Some(t) = a.resets_at {
                usage["resetsAt"] = json!(t);
            }
            if let Some(n) = a.resume {
                usage["resume"] = json!(n);
            }
            NeedsYou {
                id: item_id(a),
                pane_id: String::new(),
                chat_id: None,
                plugin: a.plugin.clone(),
                kind: "limit".into(),
                title: crate::needs::cut(&title, tmuxedo_protocol::needs::MAX_TITLE),
                detail: Some(json!({ "usage": usage })),
                context: None,
                risk: "normal".into(),
                risk_class: None,
                why: None,
                responses,
                answer_via: "none".into(),
                fingerprint: None,
                source: "core".into(),
                created_at: created_at.to_string(),
                expires_at: a.resets_at.map(|t| crate::util::iso(t * 1000)),
            }
        })
        .filter(NeedsYou::is_valid)
        .collect()
}

#[cfg(test)]
#[path = "alerts_tests.rs"]
mod tests;
