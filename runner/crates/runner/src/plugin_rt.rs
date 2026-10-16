//! The plugin host at run time, past Claude's legacy paths (PLAN Phase 16;
//! docs/design/plugins.md §7, plugin-codex.md, plugin-cursor.md):
//!
//! - **login health for every plugin**: Claude's stays `hello.claudeAuth`
//!   (claude_auth.rs, untouched); every other plugin with an `[auth]`
//!   section gets the same state machine when its CLI is installed or its
//!   login file exists, reported in `hello.plugins[].auth` and as durable
//!   `auth_*` events with `data.plugin`. `command` probes (`codex login
//!   status`, `cursor-agent status --format json`) run in the background.
//! - **login links lifted from a screen** (`auth.lift`): a CLI that prints
//!   its sign-in link (Cursor with `NO_OPEN_BROWSER`, Codex's device code)
//!   gets it opened on the user's device, the one-time code beside it.
//! - **keystroke answers** (`permissions.mode = "keystroke"`, opt-in per
//!   user, D-12): a dialog on screen becomes a decidable prompt with a
//!   screen fingerprint; a decision re-reads the screen and sends one
//!   allow-listed key only when the fingerprint still matches (else
//!   `decision_stale`); no change within 2 s is reported `decision_unknown`.
//! - **release on input** (`permissions.release_on_input`): a held prompt
//!   (Codex shows no dialog while its hook is held) is given back with no
//!   decision as soon as anyone types into the pane.

use crate::claude_auth::{AuthHealth, Change};
use crate::daemon::Daemon;
use crate::log;
use crate::plugin::AgentPlugin;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tmuxedo_protocol::frames::{AgentPrompt, AgentStatus, RunnerFrame};

/// How long after a keystroke answer the dialog must be gone.
pub const KEY_SETTLE: Duration = Duration::from_secs(2);
/// How often a pane with new output is read for login messages, links and dialogs.
pub const SCAN_EVERY: Duration = Duration::from_millis(750);
/// Words a login message or link on screen contains (lower case); a pane
/// whose visible screen has none is not unwrapped and matched.
const LOGIN_WORDS: &[&str] = &["http", "token", "log in", "login", "logged", "sign in"];

/// A `command` probe's limit.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Finished `command` probes, shared with their tasks: (plugin, logged in?).
type ProbeResults = Arc<Mutex<Vec<(String, Option<bool>)>>>;

/// A dialog answered by keystroke.
#[derive(Debug, Clone)]
pub struct KeyPrompt {
    pub pane_id: String,
    pub plugin: String,
    pub kind: String,
    pub fingerprint: String,
    /// When a key went, for the settle check.
    pub sent: Option<Instant>,
}

#[derive(Default)]
pub struct PluginRt {
    /// Login health per plugin (not Claude's: that is `Daemon::auth`).
    pub auth: BTreeMap<String, AuthHealth>,
    next_probe: BTreeMap<String, Instant>,
    /// Finished `command` probes: (plugin, logged in?).
    probe_results: ProbeResults,
    probing: std::collections::BTreeSet<String>,
    /// Per pane: the end offset at the last scan, the screen rule last seen
    /// per plugin, and the last lifted (url, code).
    scanned: HashMap<String, u64>,
    /// When each pane was last scanned: a busy pane is read at most every
    /// [`SCAN_EVERY`] (a printing pane must not cost a screen walk per tick).
    scanned_at: HashMap<String, Instant>,
    screen_seen: HashMap<(String, String), bool>,
    lifted: HashMap<String, (String, Option<String>)>,
    pub key_prompts: HashMap<String, KeyPrompt>,
}

impl PluginRt {
    /// `hello.runner.plugins[].auth` for a plugin other than Claude.
    /// Read every pane's screen again on the next pass (an opt-in that
    /// makes a dialog already on screen decidable).
    pub fn rescan(&mut self) {
        self.scanned.clear();
    }

    pub fn auth_json(&self, id: &str) -> Option<Value> {
        self.auth.get(id).map(AuthHealth::json)
    }
}

/// A status command's answer: exit 0 = logged in, unless `json_field` names a
/// boolean in its JSON stdout (then that). None = it could not tell.
pub fn probe_answer(ok: bool, stdout: &str, json_field: &str) -> Option<bool> {
    if json_field.is_empty() {
        return Some(ok);
    }
    let v: Value = serde_json::from_str(stdout.trim()).ok()?;
    v.get(json_field)?.as_bool()
}

impl Daemon {
    /// At start: a login tracker for each enabled plugin (Claude aside) whose
    /// CLI answers its version command or whose login file exists.
    pub(crate) fn init_plugin_auth(&mut self) {
        for id in self.plugins.ids().into_iter().map(str::to_owned).collect::<Vec<_>>() {
            self.track_plugin_auth(&id);
        }
    }

    /// Start tracking one plugin's login if its CLI is installed (or its login
    /// file exists) and it is not tracked yet. True when tracking started.
    fn track_plugin_auth(&mut self, id: &str) -> bool {
        let os = if cfg!(target_os = "macos") { "darwin" } else { "linux" };
        if id == crate::plugin::DEFAULT_PLUGIN || !self.plugins.enabled(id) || self.prt.auth.contains_key(id) {
            return false;
        }
        let Some(p) = self.plugins.get(id) else { return false };
        if p.manifest().auth.is_none() {
            return false;
        }
        let installed = self.wire.agent_versions.get(id).is_some_and(Option::is_some);
        let h = AuthHealth::for_plugin(p.as_ref(), &self.opts.home, os, &|n| {
            std::env::var(n).is_ok_and(|v| !v.is_empty())
        });
        let file = crate::claude_auth::file_probe(p.as_ref(), &self.opts.home, Some(os));
        if !(installed || file.is_some_and(|f| f.exists())) {
            return false;
        }
        log!("{id} auth: {} ({})", h.state.as_str(), h.reason);
        self.prt.auth.insert(id.to_string(), h);
        self.prt
            .next_probe
            .insert(id.to_string(), Instant::now() + Duration::from_secs(5));
        true
    }

    /// Slow tick: the version probes that found nothing (start-up timed out,
    /// or the CLI was not there yet). Finished ones are applied; the due ones
    /// start, one in flight per CLI, in the background.
    pub(crate) async fn reprobe_tick(&mut self) {
        let now = Instant::now();
        for (id, probe) in self.reprobe.take_done() {
            self.reprobe.note(&id, &probe, now);
            if let crate::plugin::Probe::Version(v) = probe {
                self.version_found(&id, v).await;
            }
        }
        for id in self.reprobe.due(now) {
            let done = self.reprobe.done.clone();
            let limit = self.reprobe.timing.limit;
            let argv = (id == crate::plugin::DEFAULT_PLUGIN)
                .then(|| self.reprobe.timing.claude_argv.clone())
                .flatten();
            let Some(p) = self.plugins.get(&id) else {
                if let Ok(mut v) = done.lock() {
                    v.push((id, crate::plugin::Probe::Absent));
                }
                continue;
            };
            tokio::spawn(async move {
                let r = crate::plugin::probe_agent(p.as_ref(), argv.as_deref(), limit).await;
                if let Ok(mut v) = done.lock() {
                    v.push((id, r));
                }
            });
        }
    }

    /// A later probe found `id`'s version: what start-up does with one (login
    /// tracking, the chat offer's and the skill's version gates), now.
    async fn version_found(&mut self, id: &str, version: String) {
        log!("{id} version {version} (found by a later probe)");
        self.wire.agent_versions.insert(id.to_string(), Some(version.clone()));
        if id == crate::plugin::DEFAULT_PLUGIN {
            self.info.claude_version = Some(version);
            if self.auth.is_none() && self.opts.claude_auth != Some(false) {
                let a = AuthHealth::new(
                    &self.opts.home,
                    cfg!(target_os = "linux"),
                    std::env::var("ANTHROPIC_API_KEY").is_ok_and(|k| !k.is_empty()),
                );
                self.info.claude_auth = Some(a.json());
                let data = json!({ "state": a.state, "reason": a.reason, "source": "probe" });
                let ev = a.state.event();
                self.auth = Some(a);
                self.send_event("", ev, data).await;
                self.needs_sync(crate::needs::MACHINE).await;
            }
        } else if self.track_plugin_auth(id) {
            if let Some(a) = self.prt.auth.get(id) {
                let data = json!({ "plugin": id, "state": a.state, "reason": a.reason, "source": "probe" });
                let ev = a.state.event();
                self.send_event("", ev, data).await;
            }
        }
        self.info.plugins = Some(self.plugin_infos());
        self.sync_skills();
    }

    /// A plugin was enabled: look for its CLI again (installed since the
    /// runner started?) and start tracking its login; the clients hear of it
    /// through an `auth_*` event.
    pub(crate) async fn plugin_recheck(&mut self, id: &str) {
        let Some(p) = self.plugins.get(id) else { return };
        if id != crate::plugin::DEFAULT_PLUGIN {
            let r = crate::plugin::probe_agent(p.as_ref(), None, self.reprobe.timing.limit).await;
            self.reprobe.note(id, &r, Instant::now());
            self.wire.agent_versions.insert(id.to_string(), r.version());
        }
        if self.track_plugin_auth(id) {
            self.info.plugins = Some(self.plugin_infos());
            if let Some(a) = self.prt.auth.get(id) {
                let data = json!({ "plugin": id, "state": a.state, "reason": a.reason, "source": "enable" });
                let ev = a.state.event();
                self.send_event("", ev, data).await;
            }
        }
    }

    /// Report one plugin's login transition (hello + durable `auth_*`).
    pub(crate) async fn plugin_auth_changed(&mut self, plugin: &str, c: Option<Change>) {
        let Some(c) = c else { return };
        log!("{plugin} auth: {} ({}, via {})", c.state.as_str(), c.reason, c.source);
        self.info.plugins = Some(self.plugin_infos());
        let data = json!({ "plugin": plugin, "state": c.state, "reason": c.reason, "source": c.source });
        self.send_event(c.pane.as_deref().unwrap_or(""), c.state.event(), data)
            .await;
    }

    /// Slow tick: login files, `command` probes (results from the last run
    /// first, then any that are due).
    pub(crate) async fn plugin_slow_tick(&mut self) {
        let ids: Vec<String> = self.prt.auth.keys().cloned().collect();
        for id in &ids {
            if !self.plugins.enabled(id) {
                continue;
            }
            let c = self.prt.auth.get_mut(id).and_then(AuthHealth::check_credentials);
            if c.is_some() {
                // The login file moved: let the status command confirm soon.
                self.prt.next_probe.insert(id.clone(), Instant::now());
            }
            self.plugin_auth_changed(id, c).await;
        }
        let done: Vec<(String, Option<bool>)> = self
            .prt
            .probe_results
            .lock()
            .map(|mut v| std::mem::take(&mut *v))
            .unwrap_or_default();
        for (id, answer) in done {
            self.prt.probing.remove(&id);
            let c = self.prt.auth.get_mut(&id).and_then(|a| a.command_result(answer));
            self.plugin_auth_changed(&id, c).await;
        }
        let now = Instant::now();
        for id in ids {
            if self.prt.probing.contains(&id) || self.prt.next_probe.get(&id).is_some_and(|t| *t > now) {
                continue;
            }
            let Some(p) = self.plugins.get(&id).filter(|_| self.plugins.enabled(&id)) else {
                continue;
            };
            let Some(probe) = p
                .manifest()
                .auth
                .as_ref()
                .and_then(|a| a.probes.iter().find(|p| p.kind == "command").cloned())
            else {
                self.prt.next_probe.remove(&id);
                continue;
            };
            self.prt
                .next_probe
                .insert(id.clone(), now + Duration::from_secs(probe.interval_secs.max(60)));
            self.prt.probing.insert(id.clone());
            let results = self.prt.probe_results.clone();
            let scrub: Vec<String> = std::env::vars_os()
                .filter_map(|(k, _)| k.to_str().filter(|k| p.scrubs(k)).map(str::to_owned))
                .collect();
            tokio::spawn(async move {
                let mut cmd = tokio::process::Command::new(&probe.argv[0]);
                cmd.args(&probe.argv[1..])
                    .stdin(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .kill_on_drop(true);
                for k in scrub {
                    cmd.env_remove(k);
                }
                let answer = match tokio::time::timeout(PROBE_TIMEOUT, cmd.output()).await {
                    Ok(Ok(out)) => probe_answer(
                        out.status.success(),
                        &String::from_utf8_lossy(&out.stdout),
                        &probe.json_field,
                    ),
                    _ => None,
                };
                if let Ok(mut v) = results.lock() {
                    v.push((id, answer));
                }
            });
        }
    }

    /// After the detection pass: each pane's screen against every plugin's
    /// login rules and lift rules, and keystroke dialogs.
    pub(crate) async fn plugin_detect(&mut self) {
        let ids: Vec<String> = self.panes.keys().cloned().collect();
        let active: Vec<std::sync::Arc<dyn AgentPlugin>> = self
            .plugins
            .active()
            .filter(|p| p.id() != crate::plugin::DEFAULT_PLUGIN)
            .cloned()
            .collect();
        if active.is_empty() {
            return;
        }
        for id in ids {
            let (text, joined, kind, blocked, end) = {
                let Some(rt) = self.panes.get_mut(&id) else { continue };
                if !rt.alive || rt.pid == 0 || rt.emu.in_sync() {
                    continue;
                }
                if self.prt.scanned.get(&id) == Some(&rt.end()) {
                    continue;
                }
                if self.prt.scanned_at.get(&id).is_some_and(|t| t.elapsed() < SCAN_EVERY) {
                    continue;
                }
                self.prt.scanned_at.insert(id.clone(), Instant::now());
                // The visible screen only (cheap); dialogs keep its layout (their fingerprint).
                let text = rt.emu.visible_text();
                let lower = text.to_lowercase();
                // Login messages and links are matched with soft-wrapped rows
                // joined (a long line wraps mid-word on a narrow pane), only
                // when the screen has a word one would contain.
                let joined = if LOGIN_WORDS.iter().any(|w| lower.contains(w)) {
                    let rows = usize::from(rt.rows).max(24);
                    rt.read_text("recent_unwrapped", Some(rows))
                        .map(|t| t.0)
                        .unwrap_or_default()
                } else {
                    String::new()
                };
                (
                    text,
                    joined,
                    rt.track.kind().map(str::to_owned),
                    rt.track.screen == Some(AgentStatus::Blocked),
                    rt.end(),
                )
            };
            self.prt.scanned.insert(id.clone(), end);
            for p in &active {
                let pid = p.id().to_string();
                // Login messages (a new match only).
                if self.prt.auth.contains_key(&pid) {
                    let hit = p.auth_screen_hit(&joined).map(|(r, s)| (r.to_string(), s.to_string()));
                    let key = (id.clone(), pid.clone());
                    let was = self.prt.screen_seen.get(&key).copied().unwrap_or(false);
                    self.prt.screen_seen.insert(key, hit.is_some());
                    if let (false, Some((rule, state))) = (was, hit) {
                        let c = self
                            .prt
                            .auth
                            .get_mut(&pid)
                            .and_then(|a| a.screen_hit(&id, &rule, &state));
                        self.plugin_auth_changed(&pid, c).await;
                    }
                }
                // Login links printed instead of opened.
                if p.manifest().auth.as_ref().is_some_and(|a| !a.lift.is_empty()) {
                    if let Some(l) = p.lift(&joined) {
                        let now = (l.url.clone(), l.code.clone());
                        if self.prt.lifted.get(&id) != Some(&now) {
                            self.prt.lifted.insert(id.clone(), now);
                            self.lift_open_url(&id, &pid, &l);
                        }
                    }
                }
            }
            // An agent whose dialogs show only on screen: blocked there wins.
            if let Some(k) = &kind {
                let sb = self
                    .plugins
                    .by_label(k)
                    .is_some_and(|p| p.manifest().permissions.as_ref().is_some_and(|x| x.mode == "keystroke"));
                let changed = self.panes.get_mut(&id).is_some_and(|rt| {
                    let c = rt.track.screen_blocks != sb;
                    rt.track.screen_blocks = sb;
                    c
                });
                if changed {
                    self.agent_update(&id, false).await;
                }
            }
            // Keystroke dialogs: only for the pane's own agent's plugin.
            if let Some(kind) = kind {
                self.key_prompt_scan(&id, &kind, &text, blocked).await;
            }
        }
        self.key_settle_check().await;
    }

    /// Tell the user's device to open a lifted login link (a non-durable
    /// `open_url` event, `source: "screen"`, the code beside it).
    fn lift_open_url(&mut self, pane: &str, plugin: &str, l: &crate::plugin::Lifted) {
        log!("open_url from {pane} ({plugin} screen, {}): {}", l.rule, l.url);
        let mut data = json!({ "url": l.url, "source": "screen", "program": plugin });
        if self.opts.verify_hook_callers {
            let allow = self
                .plugins
                .get(plugin)
                .and_then(|p| p.manifest().links.as_ref().map(|x| x.open_url.clone()))
                .unwrap_or_default();
            let host = tmuxedo_protocol::needs::url_host(&l.url).unwrap_or_default();
            let ok = tmuxedo_protocol::needs::host_allowed(&host, &allow);
            data["verified"] = json!(ok);
            data["plugin"] = json!(plugin);
            if !ok {
                data["reason"] = json!("host_not_allowed");
            }
        }
        if let Some(c) = &l.code {
            data["code"] = json!(c);
        }
        self.queue_msg(tokio_tungstenite::tungstenite::Message::Text(
            RunnerFrame::Event {
                seq: 0,
                pane_id: pane.to_string(),
                kind: "open_url".into(),
                at: crate::util::iso_now(),
                data,
            }
            .to_json()
            .into(),
        ));
    }

    /// The plugin that answers this pane's permission dialogs by keystroke:
    /// its `[keystroke]` lists permissions (always opt-in, Phase 13) and the
    /// person turned it on (`config.plugins[].settings.keystroke`).
    fn keystroke_plugin(&self, kind: &str) -> Option<std::sync::Arc<dyn AgentPlugin>> {
        let p = self.plugins.by_label(kind)?;
        let m = p.manifest();
        let k = m.keystroke.as_ref()?;
        let on = k.kinds.iter().any(|x| x == "permission")
            && m.has("prompt.keystroke")
            && self.needs.keystroke.get(&m.id).copied().unwrap_or(k.default_on);
        let mode = m.permissions.as_ref().map(|x| x.mode.as_str());
        (on && mode == Some("keystroke")).then_some(p)
    }

    /// Whether `prompt_id` is a dialog answered by keystroke (not a held hook).
    pub(crate) fn is_key_prompt(&self, prompt_id: &str) -> Option<String> {
        self.prt.key_prompts.get(prompt_id).map(|k| k.fingerprint.clone())
    }

    async fn key_prompt_scan(&mut self, pane: &str, kind: &str, text: &str, blocked: bool) {
        let Some(p) = self.keystroke_plugin(kind) else { return };
        if blocked && p.prompt_fingerprint(text).is_none() {
            log!(
                "{pane}: blocked, but no {} dialog this plugin answers is on screen",
                p.id()
            );
        }
        let fp = if blocked { p.prompt_fingerprint(text) } else { None };
        let current = self
            .panes
            .get(pane)
            .and_then(|rt| rt.track.prompt.as_ref().map(|x| x.0.clone()));
        let ours = current.as_ref().and_then(|id| self.prt.key_prompts.get(id).cloned());
        match (fp, ours) {
            // The same dialog is still up (or a key is on its way).
            (Some((_, f)), Some(k)) if k.fingerprint == f => {}
            (Some((tool, f)), _) => {
                if current.is_some() && !self.prt.key_prompts.contains_key(current.as_deref().unwrap_or("")) {
                    return; // a hook-held prompt has the pane; leave it be
                }
                if let Some(old) = &current {
                    self.prt.key_prompts.remove(old);
                }
                let prompt_id = crate::util::random_id();
                log!(
                    "{pane}: {} dialog on screen, answerable by keystroke ({prompt_id})",
                    p.id()
                );
                self.prt.key_prompts.insert(
                    prompt_id.clone(),
                    KeyPrompt {
                        pane_id: pane.to_string(),
                        plugin: p.id().to_string(),
                        kind: kind.to_string(),
                        fingerprint: f.clone(),
                        sent: None,
                    },
                );
                if let Some(rt) = self.panes.get_mut(pane) {
                    rt.track.prompt = Some((
                        prompt_id,
                        AgentPrompt {
                            tool,
                            input: json!({ "command": f, "answer": "keystroke" }),
                            suggestions: None,
                            after_upload: None,
                        },
                    ));
                }
                self.agent_update(pane, true).await;
            }
            // The dialog went (answered at the terminal, or by our key).
            (None, Some(_)) => {
                if let Some(old) = current {
                    self.prt.key_prompts.remove(&old);
                }
                if let Some(rt) = self.panes.get_mut(pane) {
                    rt.track.prompt = None;
                }
                self.agent_update(pane, true).await;
            }
            (None, None) => {}
        }
    }

    /// A decision for a keystroke prompt: true when it was one (answered or
    /// refused as stale here).
    pub(crate) async fn key_decision(&mut self, pane_id: &str, prompt_id: &str, allow: bool) -> bool {
        let Some(k) = self.prt.key_prompts.get(prompt_id).cloned() else {
            return false;
        };
        let p = self.plugins.get(&k.plugin);
        let now_fp = self.panes.get_mut(pane_id).and_then(|rt| {
            let text = rt.emu.detection_text();
            p.as_ref().and_then(|p| p.prompt_fingerprint(&text)).map(|x| x.1)
        });
        let keys = p
            .as_ref()
            .and_then(|p| p.manifest().permissions.clone())
            .map(|x| x.keys)
            .unwrap_or_default();
        let key = if allow { keys.allow } else { keys.deny };
        let fresh = k.pane_id == pane_id && k.sent.is_none() && now_fp.as_deref() == Some(k.fingerprint.as_str());
        if !fresh || !crate::plugin::manifest::ANSWER_KEYS.contains(&key.as_str()) {
            log!(
                "keystroke answer for {pane_id} refused: {}",
                if fresh { "key not allowed" } else { "the screen changed" }
            );
            self.prt.key_prompts.remove(prompt_id);
            if let Some(rt) = self.panes.get_mut(pane_id) {
                if rt.track.prompt.as_ref().is_some_and(|x| x.0 == prompt_id) {
                    rt.track.prompt = None;
                }
            }
            self.send_event(pane_id, "decision_stale", json!({ "promptId": prompt_id }))
                .await;
            self.agent_update(pane_id, true).await;
            return true;
        }
        if let Some(rt) = self.panes.get(pane_id) {
            if rt.alive {
                self.holder.input(&rt.hid, key.as_bytes());
            }
        }
        log!(
            "keystroke answer for {pane_id}: {} ({})",
            if allow { "allow" } else { "deny" },
            k.plugin
        );
        if let Some(x) = self.prt.key_prompts.get_mut(prompt_id) {
            x.sent = Some(Instant::now());
        }
        if let Some(rt) = self.panes.get_mut(pane_id) {
            rt.track.prompt = None;
            rt.track.report(&k.kind, AgentStatus::Working, "keystroke");
        }
        self.agent_update(pane_id, true).await;
        true
    }

    /// A key went KEY_SETTLE ago and the same dialog is still on screen:
    /// the outcome is unknown (the key may have landed elsewhere).
    async fn key_settle_check(&mut self) {
        let now = Instant::now();
        let due: Vec<(String, KeyPrompt)> = self
            .prt
            .key_prompts
            .iter()
            .filter(|(_, k)| k.sent.is_some_and(|t| now.duration_since(t) >= KEY_SETTLE))
            .map(|(id, k)| (id.clone(), k.clone()))
            .collect();
        for (id, k) in due {
            self.prt.key_prompts.remove(&id);
            let still = self.panes.get_mut(&k.pane_id).and_then(|rt| {
                let text = rt.emu.detection_text();
                self.plugins
                    .get(&k.plugin)
                    .and_then(|p| p.prompt_fingerprint(&text))
                    .map(|x| x.1)
            });
            if still.as_deref() == Some(k.fingerprint.as_str()) {
                log!("keystroke answer for {}: the dialog is still there", k.pane_id);
                self.send_event(&k.pane_id, "decision_unknown", json!({ "promptId": id }))
                    .await;
            }
        }
    }

    /// Someone typed into `pane_id`: give back any prompt held for a plugin
    /// whose agent shows no dialog while held (`permissions.release_on_input`).
    pub(crate) fn release_on_input(&mut self, pane_id: &str) {
        let held: Vec<String> = self
            .prompts
            .iter()
            .filter(|(_, p)| {
                p.pane_id == pane_id
                    && self
                        .plugins
                        .by_label(&p.kind)
                        .and_then(|pl| pl.manifest().permissions.clone())
                        .is_some_and(|x| x.release_on_input)
            })
            .map(|(k, _)| k.clone())
            .collect();
        if held.is_empty() {
            return;
        }
        for k in held {
            if let Some(p) = self.prompts.remove(&k) {
                log!("{pane_id}: typing gave the held prompt back to the terminal");
                crate::api::respond(p.reply, Ok(json!({})), self);
            }
        }
        if let Some(rt) = self.panes.get_mut(pane_id) {
            rt.track.prompt = None;
        }
    }

    /// The plugin's hook is for a pane whose detected agent belongs to a
    /// different plugin (Cursor runs a project's Claude hooks too): ignore it.
    pub(crate) fn hook_for_other_agent(&self, pane_id: &str, plugin: &dyn AgentPlugin) -> bool {
        let Some(kind) = self.panes.get(pane_id).and_then(|rt| rt.track.process_kind.clone()) else {
            return false;
        };
        if plugin.manifest().agent.labels.contains(&kind) {
            return false;
        }
        self.plugins.by_label(&kind).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_answers() {
        assert_eq!(probe_answer(true, "Logged in using ChatGPT", ""), Some(true));
        assert_eq!(probe_answer(false, "Not logged in", ""), Some(false));
        assert_eq!(
            probe_answer(
                true,
                r#"{"status":"authenticated","isAuthenticated":true}"#,
                "isAuthenticated"
            ),
            Some(true)
        );
        assert_eq!(
            probe_answer(
                false,
                r#"{"status":"unauthenticated","isAuthenticated":false}"#,
                "isAuthenticated"
            ),
            Some(false)
        );
        assert_eq!(probe_answer(true, "not json", "isAuthenticated"), None);
        assert_eq!(
            probe_answer(true, r#"{"isAuthenticated":"yes"}"#, "isAuthenticated"),
            None
        );
    }
}
