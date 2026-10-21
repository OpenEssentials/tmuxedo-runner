//! The restart-time lazy resume (docs/design/agent-usage.md, "Resume and
//! limits", D-U7): when the runner is about to resume agent sessions after a
//! holder loss (pod restart), an agent whose plan's session window is at
//! 80 % or more, or whose plan has any window at its limit, is not resumed at
//! once. Its pane comes back empty and the agent resumes when the pane is
//! first opened or typed into, or when "Resume all now" is chosen on the
//! machine-level item. Below that its sessions resume as before.

use super::alerts::window_label;
use crate::daemon::Daemon;
use crate::log;
use serde_json::json;
use tmuxedo_protocol::needs::{NeedsResponse, NeedsYou};
use tmuxedo_protocol::usage::{UsageRecord, UsageWindow};

/// The session window at this much or more: resume lazily (D-U7).
pub const LAZY_RESUME_PCT: f64 = 80.0;

/// Why a plugin's agents wait: the window and how full it was.
#[derive(Debug, Clone, PartialEq)]
pub struct Why {
    pub window: String,
    pub window_mins: Option<u32>,
    pub used_pct: f64,
    pub resets_at: Option<u64>,
}

/// One pane whose agent waits to be resumed.
#[derive(Debug, Clone)]
pub struct Waiting {
    pub plugin: String,
    pub kind: String,
    pub session: String,
    pub argv: Vec<String>,
    pub why: Why,
}

fn minutes(w: &UsageWindow) -> Option<u32> {
    w.window_mins.or(match w.id.as_str() {
        "5h" => Some(300),
        "7d" => Some(10_080),
        _ => None,
    })
}

/// The window that makes a restart resume lazily, if any: a window at its
/// limit, else the session window (the shortest one) at 80 % or more.
/// Windows whose reset has passed do not count.
pub fn lazy_window(r: &UsageRecord, now_s: u64) -> Option<Why> {
    let live: Vec<&UsageWindow> = r
        .windows
        .iter()
        .filter(|w| w.resets_at.is_none_or(|t| t > now_s))
        .collect();
    let reached = live.iter().find(|w| w.used_pct >= 100.0);
    let session = live.iter().min_by_key(|w| minutes(w).unwrap_or(u32::MAX));
    let w = reached.or(session.filter(|w| w.used_pct >= LAZY_RESUME_PCT))?;
    Some(Why {
        window: w.id.clone(),
        window_mins: w.window_mins,
        used_pct: w.used_pct,
        resets_at: w.resets_at,
    })
}

fn limit_words(why: &Why) -> String {
    let label = window_label(&why.window, why.window_mins);
    if why.used_pct >= 100.0 {
        format!("{label} limit reached")
    } else {
        format!("{label} limit at {:.0}%", why.used_pct.floor())
    }
}

/// The sentence sent with `resume_waiting`: "Claude Code 5-hour limit at 86%: resumes when you open it".
pub fn reason(name: &str, why: &Why) -> String {
    format!("{name} {}: resumes when you open it", limit_words(why))
}

/// The needs-you item id for a plugin's waiting agents.
pub fn item_id(plugin: &str) -> String {
    format!("usage-lazy-{plugin}")
}

/// The machine-level item: how many agents wait, why, and "Resume all now".
pub fn item(plugin: &str, name: &str, n: usize, why: &Why, created_at: &str) -> NeedsYou {
    let title = if n == 1 {
        format!("1 {name} agent resumes when you open it ({})", limit_words(why))
    } else {
        format!("{n} {name} agents resume when you open them ({})", limit_words(why))
    };
    let level = if why.used_pct >= 100.0 { 100 } else { 80 };
    let mut usage = json!({ "window": why.window, "level": level, "usedPct": why.used_pct, "lazy": n });
    if let Some(t) = why.resets_at {
        usage["resetsAt"] = json!(t);
    }
    NeedsYou {
        id: item_id(plugin),
        pane_id: String::new(),
        chat_id: None,
        plugin: plugin.to_string(),
        kind: "limit".into(),
        title: crate::needs::cut(&title, tmuxedo_protocol::needs::MAX_TITLE),
        detail: Some(json!({ "usage": usage })),
        context: None,
        risk: "normal".into(),
        risk_class: None,
        why: None,
        responses: vec![
            NeedsResponse::new("resume_now", "approve", "Resume all now", "control"),
            NeedsResponse::new("dismiss", "dismiss", "OK", "none"),
        ],
        answer_via: "none".into(),
        fingerprint: None,
        source: "core".into(),
        created_at: created_at.to_string(),
        expires_at: None,
    }
}

impl Daemon {
    fn usage_plugin_name(&self, id: &str) -> String {
        self.plugins
            .get(id)
            .map_or_else(|| id.to_string(), |p| p.manifest().name.clone())
    }

    /// Restore of pane `id` with an agent session to resume: true when it
    /// waits instead (the pane is registered, nothing is started).
    pub(crate) fn usage_lazy_hold(&mut self, id: &str, kind: &str, session: &str, argv: &[String]) -> bool {
        let Some(plugin) = self.plugins.by_label(kind).map(|p| p.id().to_string()) else {
            return false;
        };
        if !self.plugins.usage_enabled(&plugin) {
            return false;
        }
        let now_s = crate::util::now_ms() / 1000;
        let Some(why) = self
            .usage
            .records()
            .iter()
            .find(|r| r.plugin == plugin)
            .and_then(|r| lazy_window(r, now_s))
        else {
            return false;
        };
        let name = self.usage_plugin_name(&plugin);
        let reason = reason(&name, &why);
        let cwd = self.state.pane(id).map(|p| p.pane.cwd.clone()).unwrap_or_default();
        log!("{id}: {kind} session {session} waits to be opened ({reason})");
        self.queue_event(
            id,
            "resume_waiting",
            json!({ "kind": kind, "sessionId": session, "cwd": cwd, "window": why.window,
                    "usedPct": why.used_pct, "reason": reason }),
        );
        self.register(id);
        if self.opts.pane_history {
            self.load_history(id);
        }
        self.usage_lazy.insert(
            id.to_string(),
            Waiting {
                plugin,
                kind: kind.to_string(),
                session: session.to_string(),
                argv: argv.to_vec(),
                why,
            },
        );
        true
    }

    /// The pane was opened or typed into: resume its agent if it waits.
    pub(crate) fn usage_lazy_wake(&mut self, id: &str) {
        let Some(w) = self.usage_lazy.remove(id) else { return };
        let Some(rec) = self.state.pane(id) else { return };
        let cwd = rec.pane.cwd.clone();
        let hid = rec.holder_id();
        if self.panes.get(id).is_none_or(|rt| rt.hid != hid) {
            return;
        }
        log!("{id}: resuming {} session {} in {cwd} (opened)", w.kind, w.session);
        self.queue_event(
            id,
            "resumed",
            json!({ "kind": w.kind, "sessionId": w.session, "cwd": cwd, "lazy": true }),
        );
        self.start_spawn(id, &hid, w.argv);
        self.usage_lazy_flush = true;
    }

    /// Keep the waiting set to panes that still exist; true when the items change.
    pub(crate) fn usage_lazy_tick(&mut self) -> bool {
        let gone: Vec<String> = self
            .usage_lazy
            .keys()
            .filter(|p| self.state.pane(p).is_none())
            .cloned()
            .collect();
        for p in gone {
            self.usage_lazy.remove(&p);
        }
        let counts = self.usage_lazy_counts();
        let changed = counts != self.usage_lazy_shown;
        self.usage_lazy_shown = counts;
        changed
    }

    fn usage_lazy_counts(&self) -> Vec<(String, usize)> {
        let mut m: std::collections::BTreeMap<String, usize> = Default::default();
        for w in self.usage_lazy.values() {
            *m.entry(w.plugin.clone()).or_default() += 1;
        }
        m.into_iter().collect()
    }

    /// The machine-level items, one per plugin with agents waiting.
    pub(crate) fn usage_lazy_items(&self, existing: &[NeedsYou]) -> Vec<NeedsYou> {
        self.usage_lazy_counts()
            .into_iter()
            .filter_map(|(plugin, n)| {
                let why = self.usage_lazy.values().find(|w| w.plugin == plugin)?.why.clone();
                let id = item_id(&plugin);
                let created = existing
                    .iter()
                    .find(|i| i.id == id)
                    .map_or_else(crate::util::iso_now, |i| i.created_at.clone());
                let it = item(&plugin, &self.usage_plugin_name(&plugin), n, &why, &created);
                it.is_valid().then_some(it)
            })
            .collect()
    }

    /// "Resume all now" (or OK) on a lazy item: Some(outcome).
    pub(crate) fn usage_lazy_respond(&mut self, item: &NeedsYou, response: &str) -> Option<&'static str> {
        let plugin = item.id.strip_prefix("usage-lazy-")?;
        if item.kind != "limit" {
            return None;
        }
        let panes: Vec<String> = self
            .usage_lazy
            .iter()
            .filter(|(_, w)| w.plugin == plugin)
            .map(|(p, _)| p.clone())
            .collect();
        if panes.is_empty() {
            return Some("stale");
        }
        if response == "resume_now" {
            for p in panes {
                self.usage_lazy_wake(&p);
            }
        }
        Some("accepted")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(windows: &[(&str, f64, Option<u32>)]) -> UsageRecord {
        UsageRecord {
            plugin: "claude".into(),
            account_key: format!("m:{}", "a".repeat(32)),
            mode: "plan".into(),
            plan: None,
            windows: windows
                .iter()
                .map(|(id, pct, mins)| UsageWindow {
                    id: (*id).into(),
                    used_pct: *pct,
                    resets_at: Some(2_000),
                    window_mins: *mins,
                })
                .collect(),
            context: None,
            reached: None,
            source: "statusline".into(),
            observed_at: 0,
        }
    }

    #[test]
    fn the_session_window_at_80_or_any_window_at_its_limit_waits() {
        assert_eq!(lazy_window(&rec(&[("5h", 79.9, Some(300))]), 1_000), None);
        let w = lazy_window(&rec(&[("5h", 80.0, Some(300)), ("7d", 10.0, Some(10_080))]), 1_000).unwrap();
        assert_eq!((w.window.as_str(), w.used_pct), ("5h", 80.0));
        // The weekly window high but the session window low: resume as before.
        assert_eq!(
            lazy_window(&rec(&[("5h", 20.0, None), ("7d", 90.0, None)]), 1_000),
            None
        );
        // Any window used up waits.
        let w = lazy_window(&rec(&[("5h", 20.0, None), ("7d", 100.0, None)]), 1_000).unwrap();
        assert_eq!(w.window, "7d");
        // A window whose reset passed does not count.
        assert_eq!(lazy_window(&rec(&[("5h", 95.0, Some(300))]), 3_000), None);
    }

    #[test]
    fn words() {
        let why = lazy_window(&rec(&[("5h", 86.4, Some(300))]), 1_000).unwrap();
        assert_eq!(
            reason("Claude Code", &why),
            "Claude Code 5-hour limit at 86%: resumes when you open it"
        );
        let it = item("claude", "Claude Code", 2, &why, "2026-10-19T00:00:00Z");
        assert!(it.is_valid());
        assert_eq!(
            it.title,
            "2 Claude Code agents resume when you open them (5-hour limit at 86%)"
        );
        let full = lazy_window(&rec(&[("7d", 100.0, None)]), 1_000).unwrap();
        assert_eq!(
            item("claude", "Claude Code", 1, &full, "").title,
            "1 Claude Code agent resumes when you open it (weekly limit reached)"
        );
    }
}
