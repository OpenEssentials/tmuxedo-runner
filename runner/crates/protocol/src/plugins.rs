//! Plugins on the wire (docs/protocol.md "Plugins"): the Rust twin of
//! packages/protocol/src/plugins.ts. `hello.runner.plugins[]` (beside the
//! legacy `claudeVersion` / `claudeAuth`), `config.plugins[]` (beside
//! `config.claude.args`), the `ui` frame's items and `plugin_action`.
//! Checked against packages/protocol/fixtures/plugins.json.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The plugin the legacy hello / config fields describe.
pub const LEGACY_PLUGIN: &str = "claude";
/// The welcome feature of a gateway that relays `ui` frames.
pub const FEATURE_UI: &str = "ui";
pub const UI_MAX_ITEMS: usize = 20;
pub const UI_MAX_TEXT: usize = 280;
pub const UI_MAX_LABEL: usize = 40;
pub const UI_MAX_BANNER_ACTIONS: usize = 3;

pub fn valid_plugin_id(s: &str) -> bool {
    (2..=32).contains(&s.len())
        && s.bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
}

/// A `ui` item key: `[a-z0-9][a-z0-9-]{0,39}`.
pub fn valid_ui_key(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 40
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

/// An action id: a manifest action, or a client action (`dismiss`, `settings.open`).
pub fn valid_action_id(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 64
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"._-".contains(c))
}

/// Agent login health (plugins.md §7.1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentAuth {
    /// `ok | expiring | missing | expired | failed | unknown`
    pub state: String,
    pub reason: String,
    pub since: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
}

/// One `hello.runner.plugins[]` row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginInfo {
    pub id: String,
    pub version: String,
    pub contract: String,
    /// `enabled | disabled | incompatible | failed`
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AgentAuth>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// What `config` asks of one plugin. `config.claude.args` (legacy) and
/// `config.plugins[claude].settings.launch.args` mean the same thing; when
/// both come, `plugins` wins.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PluginConfig {
    /// `Some(args)` when this config sets launch args (validated by the runner).
    pub launch_args: Option<Value>,
    /// The generated tmuxedo skill: `Some(false)` off, `Some(true)` on.
    pub skill: Option<bool>,
    /// Answering this plugin's prompts by keys (D-12, Phase 13): `None` =
    /// the plugin's default (`[keystroke] default_on`).
    pub keystroke: Option<bool>,
    /// AI usage (`settings.usage.enabled`): `Some(true)` = read this agent's
    /// plan numbers, which for a status-line source means installing the
    /// runner's status-line command. Absent (an older web, which has no
    /// switch to turn it off again) = off.
    pub usage: Option<bool>,
    /// Which usage alerts are on (`settings.usage.alerts`, a subset of
    /// [`crate::usage::USAGE_ALERT_LEVELS`]); `None` = all of them.
    pub usage_alerts: Option<Vec<u8>>,
}

/// `config.plugins` → (id, what it asks), junk dropped. `None` when absent
/// (an older web): the caller keeps reading `config.claude`.
pub fn parse_config_plugins(v: &Value) -> Option<Vec<(String, PluginConfig)>> {
    let list = v.as_array()?;
    let mut out: Vec<(String, PluginConfig)> = vec![];
    for x in list.iter().take(32) {
        let Some(id) = x.get("id").and_then(Value::as_str).filter(|i| valid_plugin_id(i)) else {
            continue;
        };
        let st = &x["settings"];
        let launch_args = st["launch"].get("args").cloned();
        // Present settings without `skill` = on (a web that sends plugins knows the skill).
        let skill = Some(st["skill"]["enabled"].as_bool().unwrap_or(true));
        let keystroke = st["keystroke"]["enabled"].as_bool();
        let usage = st["usage"]["enabled"].as_bool();
        let usage_alerts = crate::usage::parse_alert_levels(&st["usage"]["alerts"]);
        out.push((
            id.to_string(),
            PluginConfig {
                launch_args,
                skill,
                keystroke,
                usage,
                usage_alerts,
            },
        ));
    }
    Some(out)
}

/// A surface's action: a manifest action id or a client action.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UiAction {
    pub label: String,
    pub action: String,
}

/// One declared surface (the `kind` decides which fields it carries).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UiItem {
    pub key: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<String>,
    /// `machine` (default) or `agent` (each pane running this plugin's agent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tone: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// banner: up to three; toast: at most one (`action`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actions: Option<Vec<UiAction>>,
    /// toast: one action object; quick_action: the action id (a string).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dismissible: Option<bool>,
}

const TONES: &[&str] = &["info", "success", "warning", "danger"];
pub const UI_KINDS: &[&str] = &["banner", "toast", "badge", "quick_action", "status_line"];

fn text_ok(t: &Option<String>, max: usize) -> bool {
    t.as_ref().is_some_and(|t| !t.is_empty() && t.chars().count() <= max)
}

fn action_ok(a: &UiAction) -> bool {
    !a.label.is_empty() && a.label.chars().count() <= UI_MAX_LABEL && valid_action_id(&a.action)
}

impl UiItem {
    /// Why this item breaks a rule (the TS parser drops the same items).
    pub fn problem(&self) -> Option<String> {
        if !valid_ui_key(&self.key) {
            return Some(format!("bad key {:?}", self.key));
        }
        if !UI_KINDS.contains(&self.kind.as_str()) {
            return Some(format!("unknown kind {:?}", self.kind));
        }
        if let Some(w) = self.when.as_deref().filter(|w| !w.is_empty()) {
            if let Err(e) = crate::when::parse(w) {
                return Some(format!("when {w:?}: {e}"));
            }
        }
        if self.target.as_deref().is_some_and(|t| t != "machine" && t != "agent") {
            return Some("target must be machine or agent".into());
        }
        let ok = match self.kind.as_str() {
            "banner" => {
                text_ok(&self.text, UI_MAX_TEXT)
                    && self
                        .actions
                        .as_ref()
                        .is_none_or(|a| a.len() <= UI_MAX_BANNER_ACTIONS && a.iter().all(action_ok))
            }
            "toast" => {
                text_ok(&self.text, UI_MAX_TEXT)
                    && self
                        .action
                        .as_ref()
                        .is_none_or(|a| serde_json::from_value::<UiAction>(a.clone()).is_ok_and(|a| action_ok(&a)))
            }
            "badge" => text_ok(&self.text, UI_MAX_LABEL),
            "quick_action" => {
                text_ok(&self.label, UI_MAX_LABEL)
                    && self
                        .action
                        .as_ref()
                        .and_then(Value::as_str)
                        .is_some_and(valid_action_id)
            }
            _ => text_ok(&self.text, UI_MAX_TEXT),
        };
        (!ok).then(|| format!("{} {:?} breaks the {} rules", self.kind, self.key, self.kind))
    }

    /// The item as the TS parser normalises it (tone defaults to info).
    pub fn normalised(mut self) -> Self {
        if matches!(self.kind.as_str(), "banner" | "toast" | "badge") {
            if !self.tone.as_deref().is_some_and(|t| TONES.contains(&t)) {
                self.tone = Some("info".into());
            }
        } else {
            self.tone = None;
        }
        if self.when.as_deref() == Some("") {
            self.when = None;
        }
        if self.target.as_deref() == Some("machine") {
            self.target = None;
        }
        if self.dismissible != Some(true) {
            self.dismissible = None;
        }
        if self.actions.as_ref().is_some_and(Vec::is_empty) {
            self.actions = None;
        }
        self
    }

    /// The text a client shows (banner/toast/badge/status_line text, quick_action label).
    pub fn shown_text(&self, ctx: &Value) -> String {
        let t = self.text.as_deref().or(self.label.as_deref()).unwrap_or("");
        crate::when::render_text(t, ctx)
    }
}

/// A `ui` frame's items from JSON: each bad item dropped, duplicates by key dropped.
pub fn parse_ui_items(v: &Value) -> Option<Vec<UiItem>> {
    let list = v.as_array().filter(|l| l.len() <= UI_MAX_ITEMS)?;
    let mut out: Vec<UiItem> = vec![];
    for x in list {
        let Ok(it) = serde_json::from_value::<UiItem>(x.clone()) else {
            continue;
        };
        if it.problem().is_none() && !out.iter().any(|o| o.key == it.key) {
            out.push(it.normalised());
        }
    }
    Some(out)
}
