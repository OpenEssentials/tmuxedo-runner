//! The runner API's `plugin.*` methods over tmuxedo's plugins (docs/design/
//! plugins.md §13, D-13): `plugin.list`, `plugin.enable` / `plugin.disable`,
//! `plugin.action.list` / `plugin.action.invoke` and `plugin.log.list`, as specified by
//! runner/api/runner-api.schema.json.
//!
//! They answer on the machine's local socket (`daemon::on_local`) and over
//! the gateway (`api.rs`), where web's method table classifies them (list
//! and log `read`, the rest `terminal`) and a client's `plugin_action` frame
//! arrives as `plugin.action.invoke`. Actions run only what the manifest the
//! runner holds declares: a caller names an action, never argv.

use crate::api::{err, ApiErr, ApiResult};
use crate::daemon::Daemon;
use crate::events::Event;
use crate::plugin::{self, LogEntry};
use serde_json::{json, Value};

/// The `plugin.*` methods answered on the local socket.
pub const METHODS: &[&str] = &[
    "plugin.list",
    "plugin.enable",
    "plugin.disable",
    "plugin.action.list",
    "plugin.action.invoke",
    "plugin.log.list",
];

fn opt_str<'a>(p: &'a Value, k: &str) -> Option<&'a str> {
    p.get(k).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn not_found(id: &str) -> ApiErr {
    err("plugin_not_found", format!("no plugin {id}"))
}

impl Daemon {
    pub(crate) async fn plugin_request(&mut self, method: &str, p: &Value) -> ApiResult {
        match method {
            "plugin.list" => {
                let id = opt_str(p, "plugin_id");
                if let Some(i) = id {
                    self.plugins.loaded(i).ok_or_else(|| not_found(i))?;
                }
                Ok(json!({ "type": "plugin_list", "plugins": self.plugins.infos(id) }))
            }
            "plugin.enable" | "plugin.disable" => {
                let id = opt_str(p, "plugin_id")
                    .ok_or_else(|| err("invalid_params", "plugin_id is required"))?
                    .to_string();
                let on = method == "plugin.enable";
                self.plugins.loaded(&id).ok_or_else(|| not_found(&id))?;
                if self.plugins.enabled(&id) != on {
                    self.plugins
                        .set_enabled(&id, on)
                        .map_err(|e| err("plugin_not_found", e))?;
                    self.sync_skills();
                    self.plugin_toggled(&id, on);
                    if on {
                        self.plugin_recheck(&id).await;
                    } else {
                        self.prt.auth.remove(&id);
                    }
                    self.plugin_ui_changed(&id).await;
                }
                let info = self.plugins.infos(Some(&id)).remove(0);
                let t = if on { "plugin_enabled" } else { "plugin_disabled" };
                Ok(json!({ "type": t, "plugin": info }))
            }
            "plugin.action.list" => {
                let id = opt_str(p, "plugin_id");
                if let Some(i) = id {
                    self.plugins.loaded(i).ok_or_else(|| not_found(i))?;
                }
                Ok(json!({ "type": "plugin_action_list", "actions": self.plugins.action_infos(id) }))
            }
            "plugin.action.invoke" => self.plugin_invoke(p).await,
            "plugin.log.list" => {
                let limit = p["limit"].as_u64().map(|n| n as usize);
                let logs: Vec<&LogEntry> = self.plugins.logs(opt_str(p, "plugin_id"), limit);
                Ok(json!({ "type": "plugin_log_list", "logs": logs }))
            }
            m => Err(err("unknown_method", format!("unknown method {m}"))),
        }
    }

    /// Enabling writes the plugin's shim and generated files; disabling
    /// removes its shim (its agent then runs unwrapped and its hooks are
    /// ignored; core detection, `open_url` and `pane.report_agent` stay).
    fn plugin_toggled(&mut self, id: &str, on: bool) {
        let dir = self.opts.state_dir.clone();
        let res = match (on, self.plugins.get(id), self.shim_dir.is_some()) {
            (true, Some(p), true) => {
                let skill = self.plugins.skill_dir(id, &dir);
                plugin::install_plugin(
                    p.as_ref(),
                    &dir,
                    &self.opts.hook_exe,
                    &self.launch.claude_args,
                    skill.as_deref(),
                )
            }
            (false, _, true) => self.plugins.remove_shim(id, &dir),
            _ => Ok(()),
        };
        if let Err(e) = res {
            crate::log!("plugin {id}: {e}");
        }
        crate::log!("plugin {id} {}", if on { "enabled" } else { "disabled" });
    }

    async fn plugin_invoke(&mut self, p: &Value) -> ApiResult {
        let action_id = opt_str(p, "action_id").ok_or_else(|| err("invalid_params", "action_id is required"))?;
        let want = opt_str(p, "plugin_id");
        let mut found: Vec<(String, crate::plugin::manifest::Action)> = vec![];
        for id in self.plugins.ids() {
            if want.is_some_and(|w| w != id) {
                continue;
            }
            if let Some(a) = self
                .plugins
                .get(id)
                .and_then(|pl| pl.manifest().actions.iter().find(|a| a.id == action_id).cloned())
            {
                found.push((id.to_string(), a));
            }
        }
        if let Some(w) = want {
            self.plugins.loaded(w).ok_or_else(|| not_found(w))?;
        }
        let (pid, action) = match found.len() {
            0 => return Err(err("action_not_found", format!("no plugin action {action_id}"))),
            1 => found.remove(0),
            _ => {
                return Err(err(
                    "invalid_params",
                    format!("action {action_id} is ambiguous; pass plugin_id"),
                ))
            }
        };
        if !self.plugins.enabled(&pid) {
            return Err(err("plugin_disabled", format!("plugin {pid} is disabled")));
        }
        let plugin = self.plugins.get(&pid).expect("listed");
        let ctx = p
            .get("context")
            .cloned()
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({}));
        let started = crate::util::now_ms();
        let shim_dir = self
            .shim_dir
            .clone()
            .unwrap_or_else(|| self.opts.state_dir.join("shim"));
        let mut command = vec![action.kind.clone()];
        let outcome: Result<String, String> = match action.kind.as_str() {
            "auth.login" | "pane.run" => {
                let argv = if action.kind == "auth.login" {
                    plugin.login_argv(&shim_dir)
                } else {
                    let shim = plugin.manifest().launch.as_ref().map(|l| shim_dir.join(&l.shim));
                    let ctx = plugin::Ctx {
                        shim: shim.as_deref(),
                        ..plugin::Ctx::default()
                    };
                    Some(action.argv.iter().map(|t| plugin::resolve(t, &ctx)).collect())
                };
                match argv.filter(|a| !a.is_empty()) {
                    Some(argv) => {
                        command.extend(argv.iter().cloned());
                        self.plugin_run_in_new_tab(&ctx, &action.title, &argv).await
                    }
                    None => Err("the plugin declares no command for this action".into()),
                }
            }
            "pane.send" => {
                command.push(action.text.clone());
                match opt_str(&ctx, "focused_pane_id").map(str::to_owned) {
                    Some(pane) => match self.panes.get(&pane).map(|rt| rt.hid.clone()) {
                        Some(hid) => {
                            self.holder.input(&hid, action.text.as_bytes());
                            Ok(format!("sent to {pane}"))
                        }
                        None => Err(format!("no pane {pane}")),
                    },
                    None => Err("pane.send needs context.focused_pane_id".into()),
                }
            }
            k => Err(format!("action kind {k} is handled by the clients, not the runner")),
        };
        let finished = crate::util::now_ms();
        let (status, stdout, error) = match &outcome {
            Ok(s) => ("succeeded", Some(s.clone()), None),
            Err(e) => ("failed", None, Some(e.clone())),
        };
        let mut log = self.plugins.push_log(LogEntry {
            log_id: String::new(),
            plugin_id: pid.clone(),
            command: command.clone(),
            status: status.into(),
            started_unix_ms: started,
            finished_unix_ms: Some(finished),
            action_id: Some(action.id.clone()),
            event: None,
            error,
        });
        log.status = status.into();
        let mut log_v = serde_json::to_value(&log).unwrap_or_default();
        if let Some(s) = stdout {
            log_v["stdout"] = json!(s);
        }
        let info = self
            .plugins
            .action_infos(Some(&pid))
            .into_iter()
            .find(|a| a["action_id"] == action.id.as_str())
            .unwrap_or_default();
        Ok(json!({ "type": "plugin_action_invoked", "action": info, "context": ctx, "log": log_v }))
    }

    /// A new tab (in the context's workspace, else the focused one) running
    /// `argv`, then a login shell. argv comes from the manifest only.
    async fn plugin_run_in_new_tab(&mut self, ctx: &Value, label: &str, argv: &[String]) -> Result<String, String> {
        let ws = match opt_str(ctx, "workspace_id") {
            Some(w) => w.to_string(),
            None => self
                .state
                .focused_workspace()
                .map(|w| w.id.clone())
                .ok_or("no workspace")?,
        };
        let cwd = match opt_str(ctx, "focused_pane_cwd") {
            Some(c) => c.to_owned(),
            None => self.new_pane_cwd(self.state.workspace(&ws).map(|w| w.cwd.clone())),
        };
        let cmd = argv.iter().map(|a| plugin::sh_quote(a)).collect::<Vec<_>>().join(" ");
        let run = plugin::run_then_shell(&self.shell, &cmd);
        let default = self.default_argv();
        let (tab, pane) = self
            .state
            .create_tab(
                &ws,
                &cwd,
                Some(label.chars().take(40).collect()),
                default,
                Default::default(),
            )
            .map_err(|e| e.1)?;
        let _ = self.state.focus_pane(&pane);
        self.spawn_pane(&pane, Some(run));
        self.emit(Event::new("tab_created", json!({ "tab": self.tab_info(&tab) })));
        self.emit(Event::new("pane_created", json!({ "pane": self.pane_info(&pane) })));
        self.commit_topology().await;
        Ok(format!("opened {pane}"))
    }
}
