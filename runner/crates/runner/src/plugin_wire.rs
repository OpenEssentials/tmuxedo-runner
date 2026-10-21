//! The plugin host on the wire (docs/protocol.md "Plugins", PLAN Phase 12):
//!
//! - `hello.runner.plugins[]`, sent beside the legacy `claudeVersion` /
//!   `claudeAuth` (dual-write; those stay byte-for-byte as they were);
//! - `config.plugins[]`: the launch args per plugin (`config.claude.args` is
//!   still read; `plugins` wins when both come) and the generated skill;
//! - `ui` frames: every enabled plugin's declared surfaces, sent after a
//!   `welcome` whose `features` lists `ui`, and again when a plugin is
//!   enabled or disabled;
//! - the generated tmuxedo skill (crate::skill), written per plugin and
//!   passed to the agent through the manifest's `{skill.argv...}`.

use crate::daemon::Daemon;
use crate::log;
use crate::plugin::AgentPlugin;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use tmuxedo_protocol::frames::RunnerFrame;
use tmuxedo_protocol::plugins::{parse_config_plugins, AgentAuth, PluginInfo, FEATURE_UI, LEGACY_PLUGIN};

/// Per-connection and per-process plugin wire state.
#[derive(Default)]
pub struct Wire {
    /// What the current gateway relays (`welcome.features`).
    pub gateway_features: Vec<String>,
    /// Each plugin's agent CLI version (from `agent.version_cmd`), probed at start.
    pub agent_versions: BTreeMap<String, Option<String>>,
    /// Web asked for the skill per plugin (`config.plugins[].settings.skill`);
    /// absent = web never said (an older web): off.
    pub skill_wanted: BTreeMap<String, bool>,
    /// Web asked for AI usage per plugin (`config.plugins[].settings.usage`);
    /// only the ones that are on. Kept in `<state>/usage.json`.
    pub usage_wanted: BTreeMap<String, bool>,
    /// The usage alert levels on per plugin (`settings.usage.alerts`); a
    /// plugin not here has all of them. Kept in `<state>/usage-alert-levels.json`.
    pub usage_alert_levels: BTreeMap<String, Vec<u8>>,
    /// A hosted machine (the provisioner sets TMUXEDO_CONFIG_SOURCE).
    pub hosted: bool,
}

impl Wire {
    pub fn new(agent_versions: BTreeMap<String, Option<String>>) -> Wire {
        let hosted = ["TMUXEDO_CONFIG_SOURCE", "AIMUX_CONFIG_SOURCE"]
            .iter()
            .any(|k| std::env::var_os(k).is_some_and(|v| !v.is_empty()));
        Wire {
            agent_versions,
            hosted,
            ..Wire::default()
        }
    }
}

/// `config.plugins` (if present) resolved for the legacy fields: the claude
/// launch args to validate, and the skill wishes per plugin.
pub struct ConfigPlugins {
    pub claude_args: Option<Value>,
    pub skill: BTreeMap<String, bool>,
    /// Keystroke answers per plugin (absent = the plugin's default).
    pub keystroke: BTreeMap<String, bool>,
    /// AI usage per plugin (only the ones web turned on; absent = off).
    pub usage: BTreeMap<String, bool>,
    /// Usage alert levels per plugin, only where web said (absent = all on).
    pub usage_alerts: BTreeMap<String, Vec<u8>>,
}

pub fn read_config_plugins(plugins: Option<&Value>) -> Option<ConfigPlugins> {
    let list = parse_config_plugins(plugins?)?;
    let mut out = ConfigPlugins {
        claude_args: None,
        skill: BTreeMap::new(),
        keystroke: BTreeMap::new(),
        usage: BTreeMap::new(),
        usage_alerts: BTreeMap::new(),
    };
    for (id, c) in list {
        if id == LEGACY_PLUGIN {
            out.claude_args = c.launch_args.clone();
        }
        if c.usage == Some(true) {
            out.usage.insert(id.clone(), true);
        }
        if let Some(l) = c.usage_alerts.clone() {
            out.usage_alerts.insert(id.clone(), l);
        }
        if let Some(on) = c.keystroke {
            out.keystroke.insert(id.clone(), on);
        }
        if let Some(on) = c.skill {
            out.skill.insert(id, on);
        }
    }
    Some(out)
}

/// Web's last skill wishes (`<state>/skill.json`, `{"claude": true}`), kept
/// apart from config.json (whose shape older runners read).
pub fn load_skill_wishes(state_dir: &std::path::Path) -> BTreeMap<String, bool> {
    std::fs::read(state_dir.join("skill.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

pub fn save_skill_wishes(state_dir: &std::path::Path, w: &BTreeMap<String, bool>) {
    if let Ok(b) = serde_json::to_vec_pretty(w) {
        let _ = crate::config::write_private(&state_dir.join("skill.json"), &b);
    }
}

/// Web's last AI usage wishes (`<state>/usage.json`, `{"claude": true}`): the
/// plugins whose usage the person turned on. No file = none, so a runner
/// that has never heard from a web with the switch installs no status line.
pub fn load_usage_wishes(state_dir: &std::path::Path) -> BTreeMap<String, bool> {
    std::fs::read(state_dir.join("usage.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

pub fn save_usage_wishes(state_dir: &std::path::Path, w: &BTreeMap<String, bool>) {
    if let Ok(b) = serde_json::to_vec_pretty(w) {
        let _ = crate::config::write_private(&state_dir.join("usage.json"), &b);
    }
}

/// Web's last usage alert levels (`<state>/usage-alert-levels.json`, `{"claude": [95, 100]}`).
pub fn load_usage_alert_levels(state_dir: &std::path::Path) -> BTreeMap<String, Vec<u8>> {
    std::fs::read(state_dir.join("usage-alert-levels.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

pub fn save_usage_alert_levels(state_dir: &std::path::Path, l: &BTreeMap<String, Vec<u8>>) {
    if let Ok(b) = serde_json::to_vec_pretty(l) {
        let _ = crate::config::write_private(&state_dir.join("usage-alert-levels.json"), &b);
    }
}

/// The usage alert levels on for `plugin`.
pub fn alert_levels(l: &BTreeMap<String, Vec<u8>>, plugin: &str) -> Vec<u8> {
    l.get(plugin)
        .cloned()
        .unwrap_or_else(|| tmuxedo_protocol::usage::USAGE_ALERT_LEVELS.to_vec())
}

impl Daemon {
    /// `hello.runner.plugins[]`: every plugin the host holds.
    pub(crate) fn plugin_infos(&self) -> Vec<PluginInfo> {
        self.plugins
            .ids()
            .into_iter()
            .filter_map(|id| self.plugins.loaded(id))
            .map(|l| {
                let m = l.plugin.manifest();
                let enabled = l.enabled;
                // Login health: the Claude plugin's is the legacy claudeAuth.
                let auth = if m.id == LEGACY_PLUGIN {
                    enabled.then_some(self.info.claude_auth.clone()).flatten()
                } else {
                    enabled.then(|| self.prt.auth_json(&m.id)).flatten()
                }
                .and_then(|a| serde_json::from_value::<AgentAuth>(a).ok());
                PluginInfo {
                    id: m.id.clone(),
                    version: m.version.clone(),
                    contract: m.contract.clone(),
                    state: if enabled { "enabled" } else { "disabled" }.into(),
                    agent: Some(m.agent_label().to_string()),
                    agent_version: self.wire.agent_versions.get(&m.id).cloned().flatten(),
                    auth,
                    reason: None,
                }
            })
            .collect()
    }

    /// The hello this runner sends now (legacy fields untouched, plugins added).
    pub(crate) fn hello_info(&mut self) -> tmuxedo_protocol::frames::RunnerInfo {
        self.info.plugins = Some(self.plugin_infos());
        self.info.keep_awake = Some(self.awake.wire());
        self.info.network = Some(crate::wake::discover());
        self.info.clone()
    }

    pub(crate) fn gateway_relays_ui(&self) -> bool {
        self.wire.gateway_features.iter().any(|f| f == FEATURE_UI)
    }

    /// One plugin's `ui` frame: its declared surfaces while enabled, none when not.
    fn ui_frame(&self, id: &str) -> Option<RunnerFrame> {
        let l = self.plugins.loaded(id)?;
        let items = if l.enabled {
            l.plugin.manifest().ui.clone()
        } else {
            vec![]
        };
        Some(RunnerFrame::Ui {
            plugin_id: id.to_string(),
            scope: "machine".into(),
            pane_id: None,
            items,
        })
    }

    /// After `welcome`: every plugin's surfaces (only to a gateway that relays them).
    pub(crate) async fn send_ui_frames(&mut self) {
        if !self.gateway_relays_ui() {
            return;
        }
        let ids: Vec<String> = self.plugins.ids().into_iter().map(String::from).collect();
        for id in ids {
            let declares = self.plugins.get(&id).is_some_and(|p| !p.manifest().ui.is_empty());
            if let (true, Some(f)) = (declares, self.ui_frame(&id)) {
                self.send_frame(f).await;
            }
        }
    }

    /// A plugin was enabled or disabled: its surfaces follow.
    pub(crate) async fn plugin_ui_changed(&mut self, id: &str) {
        if self.gateway_relays_ui() && self.plugins.get(id).is_some_and(|p| !p.manifest().ui.is_empty()) {
            if let Some(f) = self.ui_frame(id) {
                self.send_frame(f).await;
            }
        }
    }

    /// The facts the skill is built from, for one plugin.
    pub(crate) fn skill_facts(&self, plugin: &dyn AgentPlugin) -> crate::skill::Facts {
        let work = self
            .launch
            .work_dir
            .clone()
            .unwrap_or_else(|| crate::launch_config::DEFAULT_WORK_DIR.to_string());
        let mut f = crate::skill::facts(
            plugin.manifest(),
            &self.info.features.clone().unwrap_or_default(),
            self.wire.hosted,
            &work,
            &self.opts.state_dir.display().to_string(),
        );
        // Phase 14: names only, from the policy web sent.
        f.secret_names = self.delivery.policy.names.keys().cloned().collect();
        f.ssh_keys = !self.ssh.keys.is_empty();
        f
    }

    /// Bring every plugin's skill in line with web's wish and the agent's
    /// version: write it (on) or remove it (off). True when a shim must change.
    pub(crate) fn sync_skills(&mut self) -> bool {
        let mut changed = false;
        let ids: Vec<String> = self.plugins.ids().into_iter().map(String::from).collect();
        for id in ids {
            let Some(p) = self.plugins.get(&id) else { continue };
            let Some(sk) = p.manifest().skill.clone() else { continue };
            let wanted = self.wire.skill_wanted.get(&id).copied().unwrap_or(false);
            let version = self.wire.agent_versions.get(&id).cloned().flatten();
            let supported = sk.argv.is_empty() || crate::skill::version_at_least(version.as_deref(), &sk.min_agent);
            let on = wanted && supported && self.plugins.enabled(&id);
            let dir = crate::plugin::skill_path(&self.opts.state_dir, &id);
            if on {
                let facts = self.skill_facts(p.as_ref());
                if let Err(e) = crate::skill::write(&dir, &sk.form, &facts, crate::VERSION) {
                    log!("skill {id}: {e}");
                    continue;
                }
            } else if dir.exists() {
                let _ = std::fs::remove_dir_all(&dir);
            }
            if wanted && !supported {
                log!(
                    "skill {id}: {} {} is older than {}; launching without it",
                    p.manifest().name,
                    version.as_deref().unwrap_or("(unknown version)"),
                    sk.min_agent
                );
            }
            if self.plugins.skill_enabled(&id) != on {
                self.plugins.set_skill(&id, on);
                changed = true;
            }
        }
        changed
    }

    /// `runner.skill {plugin_id?}` (local socket; `tmuxedo skill`): what the
    /// agent is told, and whether it is on.
    pub(crate) fn skill_view(&self, p: &Value) -> Result<Value, crate::api::ApiErr> {
        let id = p["plugin_id"].as_str().unwrap_or(LEGACY_PLUGIN);
        let plugin = self
            .plugins
            .get(id)
            .ok_or_else(|| crate::api::err("plugin_not_found", format!("no plugin {id}")))?;
        let facts = self.skill_facts(plugin.as_ref());
        let form = plugin.manifest().skill.as_ref().map(|s| s.form.clone());
        Ok(json!({
            "plugin_id": id,
            "enabled": self.plugins.skill_enabled(id),
            "form": form,
            "dir": crate::plugin::skill_path(&self.opts.state_dir, id),
            "markdown": crate::skill::markdown(&facts),
        }))
    }
}
