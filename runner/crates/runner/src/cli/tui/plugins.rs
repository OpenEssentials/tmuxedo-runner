//! Plugin surfaces in the TUI (PLAN Phase 12 UI primitives; plugins.md §6).
//! The runner's `ui` frames (relayed by the gateway) are kept per machine and
//! each item's `when` is evaluated with the Rust evaluator
//! (tmuxedo_protocol::when, the same fixtures as web and the app):
//!
//! - banner: the status line says it (nothing is inserted above the panes,
//!   ui-patterns.md §5), with "prefix :" for its actions;
//! - toast: the status line, once, when its condition becomes true;
//! - badge: `[text]` after the machine (machine target) or the agent (agent
//!   target) in the sidebar;
//! - quick_action: the `prefix :` palette (with the banners' actions);
//! - status_line: the right of the status line, for the focused pane.
//!
//! An action runs on the machine through the `plugin_action` frame (control
//! scope); `dismiss` hides an item until the TUI restarts.

use super::app::{App, Machine};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use tmuxedo_protocol::plugins::{parse_ui_items, UiItem};

/// What a machine's plugins have said.
#[derive(Debug, Clone, Default)]
pub struct PluginState {
    /// slot (`plugin/scope[/pane]`) → (plugin id, pane, items)
    pub ui: BTreeMap<String, (String, Option<String>, Vec<UiItem>)>,
    /// plugin → (state, reason)
    pub auth: BTreeMap<String, (String, String)>,
    /// plugin → agent label (`hello.runner.plugins[].agent`)
    pub labels: BTreeMap<String, String>,
    /// Toasts already said (key) and items dismissed (key).
    pub toasted: BTreeSet<String>,
    pub dismissed: BTreeSet<String>,
}

/// A surface's place: its pane (None = the machine) and that pane's agent (kind, status).
type Target<'a> = (Option<String>, Option<(&'a str, String)>);

/// One item to draw, its text filled in.
#[derive(Debug, Clone, PartialEq)]
pub struct Shown {
    pub plugin: String,
    pub pane: Option<String>,
    pub item: UiItem,
    pub text: String,
}

impl Shown {
    pub fn key(&self) -> String {
        match &self.pane {
            Some(p) => format!("{}/{}/{p}", self.plugin, self.item.key),
            None => format!("{}/{}", self.plugin, self.item.key),
        }
    }
}

impl PluginState {
    /// A runner hello (forwarded in `welcome` / `machine`): plugins[] first, else claudeAuth.
    fn hello(&mut self, h: &Value) {
        let r = if h.get("runner").is_some() { &h["runner"] } else { h };
        if let Some(list) = r["plugins"].as_array() {
            self.auth.clear();
            for p in list {
                let Some(id) = p["id"].as_str() else { continue };
                self.labels
                    .insert(id.to_string(), p["agent"].as_str().unwrap_or(id).to_string());
                if let Some(s) = p["auth"]["state"].as_str() {
                    let reason = p["auth"]["reason"].as_str().unwrap_or("").to_string();
                    self.auth.insert(id.to_string(), (s.to_string(), reason));
                }
            }
        } else if let Some(s) = r["claudeAuth"]["state"].as_str() {
            let reason = r["claudeAuth"]["reason"].as_str().unwrap_or("").to_string();
            self.auth.insert("claude".into(), (s.to_string(), reason));
        }
    }

    /// Every frame of the machine's socket passes here first.
    pub fn observe(&mut self, v: &Value) {
        match v["t"].as_str().unwrap_or("") {
            "welcome" => self.hello(&v["machine"]["hello"]),
            "machine" => self.hello(&v["hello"]),
            "event" => {
                let t = v["type"].as_str().unwrap_or("");
                if let Some(state) = t.strip_prefix("auth_") {
                    let plugin = v["data"]["plugin"].as_str().unwrap_or("claude").to_string();
                    let reason = v["data"]["reason"].as_str().unwrap_or("").to_string();
                    self.auth.insert(plugin, (state.to_string(), reason));
                }
            }
            "ui" => {
                let (Some(plugin), Some(scope)) = (v["pluginId"].as_str(), v["scope"].as_str()) else {
                    return;
                };
                let pane = v["paneId"].as_str().filter(|_| scope == "pane").map(str::to_owned);
                let slot = match &pane {
                    Some(p) => format!("{plugin}/pane/{p}"),
                    None => format!("{plugin}/machine"),
                };
                match parse_ui_items(&v["items"]) {
                    Some(items) if !items.is_empty() => {
                        self.ui.insert(slot, (plugin.to_string(), pane, items));
                    }
                    Some(_) => {
                        self.ui.remove(&slot);
                    }
                    None => {}
                }
            }
            _ => {}
        }
    }
}

fn ctx(m: &Machine, plugin: &str, agent: Option<(&str, String)>) -> Value {
    let auth = m
        .plugins
        .auth
        .get(plugin)
        .map(|(s, r)| json!({ "state": s, "reason": r }))
        .unwrap_or(Value::Null);
    let agent = agent
        .map(|(k, s)| json!({ "kind": k, "status": s }))
        .unwrap_or(Value::Null);
    json!({ "auth": auth, "agent": agent, "machine": { "online": m.online, "name": m.name }, "plugin": { "id": plugin } })
}

/// What a machine's plugins show now.
pub fn shown(m: &Machine) -> Vec<Shown> {
    let mut out = vec![];
    let panes: Vec<(String, Option<(String, String)>)> = m
        .topo
        .iter()
        .flat_map(|t| t.panes.iter())
        .map(|p| {
            let a = p.agent.as_ref().map(|a| {
                let s = serde_json::to_value(a.status)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .unwrap_or_default();
                (a.kind.clone(), s)
            });
            (p.id.clone(), a)
        })
        .collect();
    for (plugin, slot_pane, items) in m.plugins.ui.values() {
        let label = m.plugins.labels.get(plugin).cloned().unwrap_or_else(|| plugin.clone());
        for it in items {
            let targets: Vec<Target> = if slot_pane.is_some() || it.target.as_deref() != Some("agent") {
                let a = slot_pane
                    .as_ref()
                    .and_then(|p| panes.iter().find(|(id, _)| id == p))
                    .and_then(|(_, a)| a.as_ref().map(|(k, s)| (k.as_str(), s.clone())));
                vec![(slot_pane.clone(), a)]
            } else {
                panes
                    .iter()
                    .filter_map(|(id, a)| {
                        a.as_ref()
                            .filter(|(k, _)| *k == label)
                            .map(|(k, s)| (Some(id.clone()), Some((k.as_str(), s.clone()))))
                    })
                    .collect()
            };
            for (pane, agent) in targets {
                let c = ctx(m, plugin, agent);
                if !tmuxedo_protocol::when::eval(it.when.as_deref(), &c) {
                    continue;
                }
                let s = Shown {
                    plugin: plugin.clone(),
                    pane,
                    text: it.shown_text(&c),
                    item: it.clone(),
                };
                if !m.plugins.dismissed.contains(&s.key()) {
                    out.push(s);
                }
            }
        }
    }
    out
}

/// The actions the `prefix :` palette offers: (label, plugin, action id, pane).
pub fn palette(m: &Machine) -> Vec<(String, String, String, Option<String>)> {
    let mut out = vec![];
    for s in shown(m) {
        match s.item.kind.as_str() {
            "quick_action" => {
                if let Some(a) = s.item.action.as_ref().and_then(Value::as_str) {
                    out.push((s.text.clone(), s.plugin.clone(), a.to_string(), s.pane.clone()));
                }
            }
            "banner" => {
                for a in s.item.actions.iter().flatten() {
                    out.push((a.label.clone(), s.plugin.clone(), a.action.clone(), s.pane.clone()));
                }
                if s.item.dismissible == Some(true) {
                    out.push((
                        format!("Dismiss: {}", s.text),
                        s.plugin.clone(),
                        "dismiss".into(),
                        s.pane.clone(),
                    ));
                }
            }
            _ => {}
        }
    }
    out.truncate(9);
    out
}

impl App {
    /// After a frame: a toast whose condition just became true goes to the status line.
    pub(crate) fn plugin_toasts(&mut self, mi: usize, selected: bool) {
        let fresh: Vec<Shown> = shown(&self.machines[mi])
            .into_iter()
            .filter(|s| s.item.kind == "toast" && !self.machines[mi].plugins.toasted.contains(&s.key()))
            .collect();
        for s in &fresh {
            self.machines[mi].plugins.toasted.insert(s.key());
        }
        if let (true, Some(t)) = (selected, fresh.last()) {
            self.note = Some(format!("{}: {}", t.plugin, t.text));
        }
    }

    /// The palette's line on the status bar.
    pub fn palette_line(&self) -> String {
        let items = self.machine().map(palette).unwrap_or_default();
        if items.is_empty() {
            return "no plugin actions here (Esc)".into();
        }
        let list: Vec<String> = items
            .iter()
            .enumerate()
            .map(|(i, (l, ..))| format!("{} {l}", i + 1))
            .collect();
        format!("actions: {}  (Esc cancels)", list.join("  "))
    }

    /// A digit in the palette: run that action.
    pub(crate) fn palette_pick(&mut self, n: usize) {
        let Some(m) = self.machine() else { return };
        let Some((label, plugin, action, pane)) = palette(m).into_iter().nth(n) else {
            return;
        };
        if action == "dismiss" {
            let key = shown(m)
                .into_iter()
                .find(|s| s.plugin == plugin && s.pane == pane && s.item.kind == "banner")
                .map(|s| s.key());
            let sel = self.sel;
            if let Some(k) = key {
                self.machines[sel].plugins.dismissed.insert(k);
            }
            return;
        }
        if action == "settings.open" {
            self.note = Some("plugin settings are on the web: Settings → Plugins".into());
            return;
        }
        let pane = pane.or_else(|| self.focus.clone());
        self.plugin_action(&label, &plugin, &action, pane);
    }

    /// The banner the status line says (the first), as "text · prefix : actions".
    pub fn banner_line(&self) -> Option<String> {
        let m = self.machine()?;
        if super::view::org_disabled(m.auth.as_ref()) {
            return Some(super::view::ORG_DISABLED_LINE.to_string());
        }
        let b = shown(m).into_iter().find(|s| s.item.kind == "banner")?;
        Some(format!("{} · {} : actions", b.text, self.keymap.prefix_label()))
    }

    /// The focused pane's status line (agent target), or the machine's.
    pub fn plugin_status(&self) -> Option<String> {
        let m = self.machine()?;
        let focus = self.focus.clone();
        let all = shown(m);
        all.iter()
            .find(|s| s.item.kind == "status_line" && s.pane.is_some() && s.pane == focus)
            .or_else(|| all.iter().find(|s| s.item.kind == "status_line" && s.pane.is_none()))
            .map(|s| s.text.clone())
    }
}

/// Badges for the sidebar: machine-level (pane None) or one pane's.
pub fn badges(m: &Machine, pane: Option<&str>) -> Vec<(String, String)> {
    shown(m)
        .into_iter()
        .filter(|s| s.item.kind == "badge" && s.pane.as_deref() == pane)
        .map(|s| (s.text, s.item.tone.unwrap_or_else(|| "info".into())))
        .collect()
}
