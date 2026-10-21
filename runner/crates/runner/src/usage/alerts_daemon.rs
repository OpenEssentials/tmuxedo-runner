//! The daemon side of usage alerts (crate::usage::alerts): bring them up to
//! date about once a second, show them as machine-level needs-you items,
//! answer "Resume at reset" / "Don't resume", and type the resume when due.

use super::alerts::{self, RESUME_TEXT};
use crate::daemon::Daemon;
use crate::log;
use std::time::{Duration, Instant};
use tmuxedo_protocol::frames::AgentStatus;
use tmuxedo_protocol::needs::NeedsYou;

const EVERY: Duration = Duration::from_secs(1);

impl Daemon {
    /// The plugin id of the agent running in `pane`, if any.
    fn usage_pane_plugin(&self, pane: &str) -> Option<String> {
        let kind = self.panes.get(pane)?.track.kind()?.to_string();
        self.plugins.by_label(&kind).map(|p| p.id().to_string())
    }

    fn usage_pane_status(&self, pane: &str) -> Option<AgentStatus> {
        self.panes.get(pane).map(|rt| rt.track.effective)
    }

    /// Free to be typed into: no decision open, nothing held.
    fn usage_pane_free(&self, pane: &str) -> bool {
        self.needs.open.get(pane).is_none_or(|v| v.is_empty()) && !self.prompts.values().any(|h| h.pane_id == pane)
    }

    pub(crate) async fn usage_alerts_tick(&mut self, now: Instant) {
        if self.usage_alerts_at.is_some_and(|t| now.duration_since(t) < EVERY) {
            return;
        }
        self.usage_alerts_at = Some(now);
        let now_s = crate::util::now_ms() / 1000;
        let records = self.usage.records();
        let levels = self.wire.usage_alert_levels.clone();
        let open = self
            .usage_alerts
            .update(&records, |p| crate::plugin_wire::alert_levels(&levels, p), now_s);
        for a in open.iter().filter(|a| a.fresh) {
            log!(
                "usage: {} {} at {}% ({:.0}% used)",
                a.plugin,
                a.window,
                a.level,
                a.used_pct
            );
        }
        let changed = open != self.usage_open || self.usage_lazy_tick();
        self.usage_open = open;
        for (plugin, panes) in self.usage_alerts.resume_due(now_s) {
            self.usage_resume(&plugin, &panes);
        }
        if changed {
            self.needs_sync(crate::needs::MACHINE).await;
        }
    }

    /// Type `continue` into each planned pane still running that plugin's agent, idle and free.
    fn usage_resume(&mut self, plugin: &str, panes: &[String]) {
        let mut n = 0;
        for pane in panes {
            let ok = self.usage_pane_plugin(pane).as_deref() == Some(plugin)
                && !matches!(
                    self.usage_pane_status(pane),
                    Some(AgentStatus::Working | AgentStatus::Blocked) | None
                )
                && self.usage_pane_free(pane);
            if !ok {
                continue;
            }
            let Ok(hid) = self.hid(pane) else { continue };
            let mut bytes = RESUME_TEXT.as_bytes().to_vec();
            bytes.push(b'\r');
            self.holder.input(&hid, &bytes);
            n += 1;
        }
        log!(
            "usage: resumed {n} of {} {plugin} agent(s) after the reset",
            panes.len()
        );
    }

    /// The machine-level items for the open alerts (crate::needs), keeping each one's first time.
    pub(crate) fn usage_alert_items(&self, existing: &[NeedsYou]) -> Vec<NeedsYou> {
        let name = |id: &str| {
            self.plugins
                .get(id)
                .map_or_else(|| id.to_string(), |p| p.manifest().name.clone())
        };
        let mut out = alerts::items(&self.usage_open, name, "");
        for it in &mut out {
            it.created_at = existing
                .iter()
                .find(|i| i.id == it.id)
                .map_or_else(crate::util::iso_now, |i| i.created_at.clone());
        }
        out.extend(self.usage_lazy_items(existing));
        out
    }

    /// `usage.list`: per plugin with AI usage on, its name and windows (no account key).
    pub(crate) fn usage_list(&self) -> serde_json::Value {
        let records: Vec<serde_json::Value> = self
            .usage
            .records()
            .into_iter()
            .filter(|r| self.plugins.usage_enabled(&r.plugin))
            .map(|r| {
                let name = self
                    .plugins
                    .get(&r.plugin)
                    .map_or_else(|| r.plugin.clone(), |p| p.manifest().name.clone());
                let windows: Vec<serde_json::Value> = r
                    .windows
                    .iter()
                    .map(|w| {
                        let mut v = serde_json::json!({
                            "id": w.id,
                            "label": alerts::window_label(&w.id, w.window_mins),
                            "usedPct": w.used_pct,
                        });
                        if let Some(t) = w.resets_at {
                            v["resetsAt"] = t.into();
                        }
                        v
                    })
                    .collect();
                serde_json::json!({ "plugin": r.plugin, "name": name, "windows": windows, "observedAt": r.observed_at })
            })
            .collect();
        let on = self.plugins.active().any(|p| self.plugins.usage_enabled(p.id()));
        serde_json::json!({ "type": "usage_list", "enabled": on, "records": records })
    }

    /// "Resume at reset" or "Don't resume" on a limit item: Some(outcome).
    pub(crate) async fn usage_alert_respond(&mut self, item: &NeedsYou, response: &str) -> Option<&'static str> {
        if item.kind != "limit" || !item.id.starts_with("usage-") {
            return None;
        }
        if let Some(outcome) = self.usage_lazy_respond(item, response) {
            self.usage_alerts_at = None;
            self.usage_alerts_tick(Instant::now()).await;
            return Some(outcome);
        }
        let a = self
            .usage_open
            .iter()
            .find(|a| a.level == 100 && super::alerts::item_id(a) == item.id)
            .cloned();
        let outcome = match (response, a) {
            ("resume", Some(a)) => {
                let Some(at) = a.resets_at else { return Some("stale") };
                let panes: Vec<String> = self
                    .panes
                    .keys()
                    .filter(|p| {
                        self.usage_pane_plugin(p).as_deref() == Some(a.plugin.as_str())
                            && self.usage_pane_status(p) != Some(AgentStatus::Working)
                    })
                    .cloned()
                    .collect();
                if self.usage_alerts.plan_resume(&a.plugin, at, panes) {
                    "accepted"
                } else {
                    "stale"
                }
            }
            ("keep", Some(a)) => {
                self.usage_alerts.cancel_resume(&a.plugin);
                "accepted"
            }
            _ => "stale",
        };
        // Show the new state now, not on the next tick.
        self.usage_alerts_at = None;
        self.usage_alerts_tick(Instant::now()).await;
        Some(outcome)
    }
}
