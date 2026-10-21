//! "Needs you" through the plugin contract (PLAN Phase 13,
//! docs/design/multi-agent-workflows.md §2, docs/protocol.md "Needs you").
//!
//! The runner owns the open items, one set per pane, derived from what it
//! already knows:
//!
//! - a **held permission prompt** (a verified agent hook) → `permission`,
//!   answered through the hook (`answerVia: hook`), id = the promptId;
//! - an agent **blocked on its own dialog** with no held prompt (a question,
//!   a choice list, an MCP elicitation; from a hook or from the screen) →
//!   `question`. When the plugin's `[keystroke]` allows questions and the
//!   person has not turned it off, it is answered by typing into the pane
//!   (`keystroke`): the screen's prompt region is fingerprinted when the
//!   item opens and again just before sending; a mismatch answers `stale`
//!   and sends nothing; only allowlisted keys go, and the region must change
//!   within [`KEYS_SETTLE`] or the outcome is `unknown`. Otherwise the item
//!   only opens the terminal (`none`);
//! - a **login** that needs doing → `login` (machine-level, `action`).
//!
//! The set goes out in a `needs` frame (to a gateway that relays them), and
//! each open and close as a durable `needs_opened` / `needs_resolved` event
//! (push, the event log). Answers arrive as `needs.respond` (over the
//! gateway only: the local socket answers `not_supported`, as anything in a
//! pane could call it). A chat's items stay the chat's (crate::chat); a
//! `needs.respond` naming a chat is handed to `chat.respond`.

use crate::api::{err, respond, ApiResult, ReplyTo};
use crate::daemon::Daemon;
use crate::log;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};
use tmuxedo_protocol::frames::{AgentStatus, Behavior, RunnerFrame};
use tmuxedo_protocol::needs::{
    forbidden_text_char, parse_respond, NeedsOption, NeedsResponse, NeedsYou, RespondParams, FEATURE_NEEDS, MAX_TITLE,
};

/// Runner API methods answered here.
pub const METHODS: &[&str] = &["needs.list", "needs.respond"];
/// A keystroke answer must change the prompt region within this long.
pub const KEYS_SETTLE: Duration = Duration::from_secs(2);
/// A question's screen must be this quiet before its fingerprint is taken.
pub const SCREEN_QUIET: Duration = Duration::from_millis(300);
/// How often a pending keystroke answer looks at the screen.
const KEYS_POLL: Duration = Duration::from_millis(100);
/// The machine-level set's pane id.
pub const MACHINE: &str = "";

#[derive(Default)]
pub struct Needs {
    /// pane id ("" = machine) → its open items, oldest first.
    pub open: BTreeMap<String, Vec<NeedsYou>>,
    /// item id → who closed it, set by whoever closes it (`needs_resolved.by`).
    hints: HashMap<String, (&'static str, Option<String>)>,
    /// pane id → the last `needs` frame sent on this connection (no resends).
    sent: HashMap<String, String>,
    /// Keystroke answers waiting for the screen to move.
    pending: Vec<PendingKeys>,
    next_poll: Option<Instant>,
    /// Web's per-plugin wish for keystroke answers (`config.plugins[].settings.keystroke`).
    pub keystroke: BTreeMap<String, bool>,
    /// Panes whose agent's hook said it is asking a question (`[keystroke]
    /// asks`); cleared when the agent moves on. Only these get keys.
    pub asking: std::collections::HashSet<String>,
    /// pane id → the agent's state-change number when its question was
    /// answered by keys (or refused as stale): no new question item until
    /// the agent's state moves on, so an answer does not re-open itself.
    settled: HashMap<String, u64>,
    /// Open file requests (`tmuxedo files request`, crate::needs_files).
    pub files: Vec<crate::needs_files::FileAsk>,
    /// Pull requests opened from the review, their checks watched (crate::needs_ci).
    pub ci: Vec<crate::needs_ci::CiWatch>,
}

struct PendingKeys {
    pane: String,
    item: String,
    before: String,
    rows: usize,
    deadline: Instant,
    reply: ReplyTo,
}

/// Web's last keystroke wishes (`<state>/keystroke.json`, `{"claude": false}`).
pub fn load_keystroke_wishes(state_dir: &std::path::Path) -> BTreeMap<String, bool> {
    std::fs::read(state_dir.join("keystroke.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

pub fn save_keystroke_wishes(state_dir: &std::path::Path, w: &BTreeMap<String, bool>) {
    if let Ok(b) = serde_json::to_vec_pretty(w) {
        let _ = crate::config::write_private(&state_dir.join("keystroke.json"), &b);
    }
}

impl Needs {
    /// Forget what was sent (a new gateway connection gets every set again).
    pub fn reset_sent(&mut self) {
        self.sent.clear();
    }

    pub fn item(&self, id: &str) -> Option<&NeedsYou> {
        self.open.values().flatten().find(|i| i.id == id)
    }

    /// Note who closes `id` (the next sync reports it).
    pub fn hint(&mut self, id: &str, by: &'static str, response: Option<String>) {
        self.hints.insert(id.to_string(), (by, response));
    }
}

// ── the screen's prompt region ───────────────────────────────────────────

/// Box-drawing and pointer characters around a dialog's text.
fn trim_frame(l: &str) -> &str {
    l.trim_matches(|c: char| c.is_whitespace() || "│┃|╭╮╰╯─━┌┐└┘║═".contains(c))
}

/// The last `rows` non-blank lines of `screen`, whitespace collapsed.
pub fn region(screen: &str, rows: usize) -> Vec<String> {
    let lines: Vec<String> = screen
        .lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|l| !l.is_empty())
        .collect();
    let skip = lines.len().saturating_sub(rows);
    lines[skip..].to_vec()
}

/// sha-256 (hex) of a region.
pub fn fingerprint(region: &[String]) -> String {
    let d = ring::digest::digest(&ring::digest::SHA256, region.join("\n").as_bytes());
    d.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

/// Numbered choices in a region (`1. Red`, `❯ 2) Blue`), in order, at most 9.
pub fn options(region: &[String]) -> Vec<NeedsOption> {
    use regex::Regex;
    use std::sync::LazyLock;
    static RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^(?:[❯>›▶→*]\s*)?([1-9])[.)]\s+(\S.{0,199})$").expect("option rule"));
    let mut out: Vec<NeedsOption> = vec![];
    for l in region {
        if let Some(c) = RE.captures(trim_frame(l)) {
            let id = c[1].to_string();
            if !out.iter().any(|o| o.id == id) {
                out.push(NeedsOption {
                    id,
                    label: crate::chat::mask::mask_shapes(c[2].trim()),
                });
            }
        }
    }
    out.truncate(9);
    out
}

/// The question in a region: its last line ending in `?`.
pub fn question(region: &[String]) -> Option<String> {
    region
        .iter()
        .rev()
        .map(|l| trim_frame(l))
        .find(|l| l.ends_with('?') && l.chars().count() > 3)
        .map(|l| cut(&crate::chat::mask::mask_shapes(l), MAX_TITLE))
}

pub(crate) fn cut(s: &str, max: usize) -> String {
    let one: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() <= max {
        one
    } else {
        let mut t: String = one.chars().take(max - 1).collect();
        t.push('…');
        t
    }
}

/// "Run `npm test`", "Edit src/app.ts", "Fetch https://…": a permission's one line.
pub fn permission_title(tool: &str, input: &Value) -> String {
    let s = |k: &str| input.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let t = match tool {
        "Bash" | "shell" | "Shell" | "exec" | "run_shell_command" if !s("command").is_empty() => {
            format!("Run {}", s("command"))
        }
        "Write" | "Edit" | "MultiEdit" | "NotebookEdit" if !s("file_path").is_empty() => {
            format!("Edit {}", s("file_path"))
        }
        "Read" if !s("file_path").is_empty() => format!("Read {}", s("file_path")),
        "WebFetch" if !s("url").is_empty() => format!("Fetch {}", s("url")),
        "" => "Permission needed".to_string(),
        t => format!("Use {t}"),
    };
    cut(&crate::chat::mask::mask_shapes(&t), MAX_TITLE)
}

fn resp(id: &str, kind: &str, label: &str, requires: &str) -> NeedsResponse {
    NeedsResponse::new(id, kind, label, requires)
}

/// The bytes a key name stands for.
fn key_bytes(k: &str) -> Vec<u8> {
    if k == "Escape" {
        vec![0x1b]
    } else {
        k.as_bytes().to_vec()
    }
}

/// What a keystroke answer types, or why it may not.
pub fn keystroke_bytes(
    item: &NeedsYou,
    r: &NeedsResponse,
    p: &RespondParams,
    keys: &[String],
    reply_ok: bool,
) -> Result<Vec<u8>, (&'static str, String)> {
    let allowed = |k: &str| keys.iter().any(|x| x == k);
    match r.kind.as_str() {
        "choose" => {
            let id = p
                .option_id
                .as_deref()
                .ok_or(("invalid_params", "optionId is required to choose".to_string()))?;
            let known = r.options.as_ref().is_some_and(|o| o.iter().any(|x| x.id == id));
            if !known || !allowed(id) {
                return Err(("key_not_allowed", format!("{id} is not a choice this item offers")));
            }
            Ok(key_bytes(id))
        }
        "reply" => {
            let t = p.text.as_deref().unwrap_or("").trim();
            if !reply_ok || item.kind != "question" {
                return Err(("key_not_allowed", "this item cannot be answered with text".into()));
            }
            if t.is_empty() || t.chars().any(forbidden_text_char) {
                return Err(("invalid_params", "the reply is empty or has control characters".into()));
            }
            // One line: a newline would submit early in most agent inputs.
            let mut b = t.replace(['\n', '\t'], " ").into_bytes();
            b.push(b'\r');
            Ok(b)
        }
        "deny" if allowed("Escape") => Ok(key_bytes("Escape")),
        "approve" if allowed("y") && item.kind == "permission" => Ok(key_bytes("y")),
        "deny" if allowed("n") && item.kind == "permission" => Ok(key_bytes("n")),
        _ => Err(("key_not_allowed", format!("{} is not answered by keys", r.kind))),
    }
}

impl Daemon {
    /// A verified hook for a pane: is its agent asking a question now (keys may answer), or has it moved on?
    pub(crate) fn needs_note_hook(
        &mut self,
        pane_id: &str,
        plugin: &dyn crate::plugin::AgentPlugin,
        native: &Value,
        out: &crate::plugin::HookOutcome,
    ) {
        use crate::plugin::AgentEvent as E;
        let m = plugin.manifest();
        let field = m.hooks.as_ref().map_or("hook_event_name", |h| h.event_field.as_str());
        if m.keystroke.as_ref().is_some_and(|k| k.asked_by(field, native)) {
            self.needs.asking.insert(pane_id.to_string());
        } else if out.events.iter().any(|e| {
            matches!(
                e,
                E::TurnStarted
                    | E::TurnCompleted
                    | E::TurnFailed { .. }
                    | E::PromptResolved
                    | E::PromptPermission { .. }
                    | E::SessionEnded
                    | E::SessionStarted { .. }
            )
        }) {
            self.needs.asking.remove(pane_id);
        }
    }

    pub(crate) fn gateway_relays_needs(&self) -> bool {
        self.wire.gateway_features.iter().any(|f| f == FEATURE_NEEDS)
    }

    /// The plugin for an agent kind, with its keystroke policy for `kind` items:
    /// (plugin id, keys, reply allowed, rows) when answering by keys is on.
    fn keystroke_policy(&self, agent: &str, kind: &str) -> (String, Option<(Vec<String>, bool, usize)>) {
        let Some(p) = self.plugins.by_label(agent) else {
            return (agent.to_string(), None);
        };
        let m = p.manifest();
        let on = m.keystroke.as_ref().filter(|k| {
            k.kinds.iter().any(|x| x == kind)
                && m.has("prompt.keystroke")
                && self.needs.keystroke.get(&m.id).copied().unwrap_or(k.default_on)
        });
        (
            m.id.clone(),
            on.map(|k| (k.keys.clone(), k.reply, usize::from(k.rows.clamp(1, 40)))),
        )
    }

    /// What should be open for a pane now: its agent's items, then the
    /// file requests made in it (crate::needs_files).
    fn needs_desired(&mut self, pane_id: &str) -> Vec<NeedsYou> {
        let mut v = self.needs_desired_agent(pane_id);
        v.extend(self.file_items(pane_id));
        v.extend(self.ci_items(pane_id));
        v.extend(self.memory_warn_items(pane_id));
        v.truncate(tmuxedo_protocol::needs::MAX_ITEMS);
        v
    }

    /// The agent's items for a pane (existing items keep their id, time and
    /// fingerprint).
    fn needs_desired_agent(&mut self, pane_id: &str) -> Vec<NeedsYou> {
        let existing = self.needs.open.get(pane_id).cloned().unwrap_or_default();
        let cwd = self.state.pane(pane_id).map(|p| p.pane.cwd.clone()).unwrap_or_default();
        let Some(rt) = self.panes.get(pane_id).filter(|rt| rt.alive) else {
            return vec![];
        };
        let Some(agent) = rt.track.kind().map(str::to_owned) else {
            return vec![];
        };
        let now = crate::util::iso_now();
        if let Some((pid, prompt)) = rt.track.prompt.clone() {
            if let Some(it) = existing.iter().find(|i| i.id == pid) {
                return vec![it.clone()];
            }
            let deadline = self.prompts.get(&pid).map(|h| h.deadline);
            let (plugin, _) = self.keystroke_policy(&agent, "permission");
            let risk = crate::risk::classify(&prompt.tool, &prompt.input, &cwd);
            let expires = deadline.map(|d| {
                let left = d.saturating_duration_since(Instant::now()).as_millis() as u64;
                crate::util::iso(crate::util::now_ms() + left)
            });
            let mut detail = json!({ "tool": prompt.tool, "input": prompt.input });
            // Files (Phase 15): it read an upload just before asking (whoever approves should know).
            let context = prompt.after_upload.as_ref().map(|u| {
                detail["afterUpload"] = json!(u);
                vec![cut(&format!("read upload {u}"), 200)]
            });
            if let Some(s) = prompt.suggestions.clone().filter(|s| s.is_array()) {
                detail["suggestions"] = s;
            }
            // A dialog the plugin answers by keys (Cursor's, crate::plugin_rt), not a held hook.
            let keys_fp = self.is_key_prompt(&pid);
            return vec![NeedsYou {
                id: pid,
                pane_id: pane_id.to_string(),
                chat_id: None,
                plugin,
                kind: "permission".into(),
                title: permission_title(&prompt.tool, &prompt.input),
                detail: Some(detail),
                context,
                risk: if risk.is_some() { "high" } else { "normal" }.into(),
                risk_class: risk.map(|r| r.class.to_string()),
                why: risk.map(|r| r.why.to_string()),
                responses: vec![
                    resp("approve", "approve", "Approve", "stepup"),
                    resp("deny", "deny", "Deny", "stepup"),
                ],
                answer_via: if keys_fp.is_some() { "keystroke" } else { "hook" }.into(),
                fingerprint: keys_fp.clone().map(|f| fingerprint(&[f])),
                source: if keys_fp.is_some() { "screen" } else { "hook" }.into(),
                created_at: now,
                expires_at: expires,
            }];
        }
        if rt.track.effective != AgentStatus::Blocked {
            return vec![];
        }
        if let Some(it) = existing.iter().find(|i| i.kind == "question") {
            return vec![it.clone()];
        }
        if self.needs.settled.get(pane_id) == Some(&rt.track.state_change_seq) {
            return vec![];
        }
        // The dialog is still being drawn: take its fingerprint once the screen is quiet.
        if rt.track.last_output.is_some_and(|t| t.elapsed() < SCREEN_QUIET) || rt.emu.in_sync() {
            return vec![];
        }
        let source = match rt.track.reported.as_ref().map(|r| r.source.as_str()) {
            Some("hook") => "hook",
            Some(_) => "report",
            None => "screen",
        };
        let (plugin, policy) = self.keystroke_policy(&agent, "question");
        // Keys answer only a question the agent's own (verified) hook said it is asking.
        let policy = policy.filter(|_| source == "hook" && self.needs.asking.contains(pane_id));
        let name = tmuxedo_detect::Agent::lookup(&agent).map_or(agent.as_str(), |a| a.title());
        let rows = policy.as_ref().map_or(12, |p| p.2);
        let Some(rt) = self.panes.get(pane_id) else {
            return vec![];
        };
        let reg = region(&rt.emu.visible_text(), rows);
        let title = question(&reg).unwrap_or_else(|| cut(&format!("{name} is waiting for an answer"), MAX_TITLE));
        let id = format!("q-{}", crate::util::random_id());
        let open = resp("open", "open", "Open terminal", "none");
        let item =
            |answer_via: &str, fp: Option<String>, responses: Vec<NeedsResponse>, detail: Option<Value>| NeedsYou {
                id: id.clone(),
                pane_id: pane_id.to_string(),
                chat_id: None,
                plugin: plugin.clone(),
                kind: "question".into(),
                title: title.clone(),
                detail,
                context: None,
                risk: "normal".into(),
                risk_class: None,
                why: None,
                responses,
                answer_via: answer_via.into(),
                fingerprint: fp,
                source: source.into(),
                created_at: now.clone(),
                expires_at: None,
            };
        // A report (pane.report_agent) can never be answered remotely.
        match policy.filter(|_| source != "report") {
            Some((keys, reply, _)) => {
                let mut responses = vec![];
                let opts: Vec<NeedsOption> = options(&reg).into_iter().filter(|o| keys.contains(&o.id)).collect();
                if !opts.is_empty() {
                    let mut r = resp("choose", "choose", "Choose", "stepup");
                    r.options = Some(opts);
                    responses.push(r);
                }
                if reply {
                    responses.push(resp("reply", "reply", "Reply", "stepup"));
                }
                if keys.iter().any(|k| k == "Escape") {
                    responses.push(resp("cancel", "deny", "Cancel", "stepup"));
                }
                responses.push(open);
                let text = question(&reg).map(|q| json!({ "text": q }));
                vec![item("keystroke", Some(fingerprint(&reg)), responses, text)]
            }
            None => vec![item("none", None, vec![open], None)],
        }
    }

    /// The machine-level items: every chat's open items (one list with the
    /// terminal's, agent-chat.md §3.7), then a login that needs doing.
    fn needs_desired_machine(&self) -> Vec<NeedsYou> {
        let existing = self.needs.open.get(MACHINE).cloned().unwrap_or_default();
        let mut out: Vec<NeedsYou> = vec![];
        for (sid, s) in &self.chats.sessions {
            for (id, n) in &s.needs {
                let created = existing
                    .iter()
                    .find(|i| i.id == *id)
                    .map_or_else(crate::util::iso_now, |i| i.created_at.clone());
                let it = tmuxedo_protocol::needs::from_chat_item(sid, &s.meta.plugin, &n.item, &created);
                if it.is_valid() {
                    out.push(it);
                }
            }
        }
        out.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        let mut files = self.file_items(MACHINE);
        files.extend(self.ci_items(MACHINE));
        out.truncate(tmuxedo_protocol::needs::MAX_ITEMS.saturating_sub(1 + files.len()));
        out.extend(files);
        out.extend(self.needs_login_item(&existing));
        out.extend(self.usage_alert_items(&existing));
        out.truncate(tmuxedo_protocol::needs::MAX_ITEMS);
        out
    }

    fn needs_login_item(&self, existing: &[NeedsYou]) -> Vec<NeedsYou> {
        let Some(a) = self.auth.as_ref() else { return vec![] };
        let state = a.state.as_str();
        if !matches!(state, "expired" | "failed") || !self.plugins.enabled(crate::plugin::DEFAULT_PLUGIN) {
            return vec![];
        }
        let Some(p) = self.plugins.get(crate::plugin::DEFAULT_PLUGIN) else {
            return vec![];
        };
        let m = p.manifest();
        let Some(action) = m.actions.iter().find(|x| x.kind == "auth.login") else {
            return vec![];
        };
        let id = format!("login-{}", m.id);
        let existing = existing.iter().find(|i| i.id == id);
        let mut login = resp("login", "action", "Sign in", "control");
        login.action_id = Some(action.id.clone());
        vec![NeedsYou {
            id: id.clone(),
            pane_id: MACHINE.into(),
            chat_id: None,
            plugin: m.id.clone(),
            kind: "login".into(),
            title: cut(&format!("Sign in to {}", m.name), MAX_TITLE),
            detail: Some(json!({ "message": crate::chat::mask::mask_shapes(&a.reason) })),
            context: None,
            risk: "normal".into(),
            risk_class: None,
            why: None,
            responses: vec![login, resp("dismiss", "dismiss", "Not now", "none")],
            answer_via: "action".into(),
            fingerprint: None,
            source: "core".into(),
            created_at: existing
                .filter(|i| i.id == id)
                .map_or_else(crate::util::iso_now, |i| i.created_at.clone()),
            expires_at: None,
        }]
    }

    /// Bring a pane's set up to date: events for what opened and closed, and
    /// the `needs` frame when the set changed.
    pub(crate) async fn needs_sync(&mut self, pane_id: &str) {
        let want = if pane_id == MACHINE {
            self.needs_desired_machine()
        } else {
            self.needs_desired(pane_id)
        };
        let had = self.needs.open.get(pane_id).cloned().unwrap_or_default();
        if want == had {
            return self.needs_send(pane_id).await;
        }
        // The events are part of the needs contract: only for a gateway (and
        // so a web) that knows it. An older one hears what it always heard.
        let tell = self.gateway_relays_needs();
        for old in had
            .iter()
            .filter(|o| !want.iter().any(|w| w.id == o.id) && o.chat_id.is_none())
        {
            let (by, response) = self.needs.hints.remove(&old.id).unwrap_or_else(|| {
                let alive = self
                    .panes
                    .get(pane_id)
                    .is_some_and(|r| r.alive && r.track.kind().is_some());
                (
                    if pane_id == MACHINE || alive {
                        "terminal"
                    } else {
                        "agent"
                    },
                    None,
                )
            });
            let mut d = json!({ "id": old.id, "kind": old.kind, "plugin": old.plugin, "by": by });
            if let Some(r) = response {
                d["responseId"] = json!(r);
            }
            if tell {
                self.send_event(pane_id, "needs_resolved", d).await;
            }
        }
        // A chat's items are pushed and logged by the chat itself (`chat_needs`).
        for new in want
            .iter()
            .filter(|w| !had.iter().any(|h| h.id == w.id) && w.chat_id.is_none())
        {
            log!(
                "needs you: {} {} in {} ({}, {})",
                new.kind,
                new.id,
                if pane_id.is_empty() { "the machine" } else { pane_id },
                new.answer_via,
                new.risk
            );
            if tell {
                self.send_event(pane_id, "needs_opened", json!({ "item": new })).await;
            }
        }
        if want.is_empty() {
            self.needs.open.remove(pane_id);
        } else {
            self.needs.open.insert(pane_id.to_string(), want);
        }
        self.needs_send(pane_id).await;
    }

    /// The pane's set to the gateway, if it relays them and it changed.
    async fn needs_send(&mut self, pane_id: &str) {
        if !self.gateway_relays_needs() {
            return;
        }
        let items = self.needs.open.get(pane_id).cloned().unwrap_or_default();
        let f = RunnerFrame::Needs {
            pane_id: pane_id.to_string(),
            items,
        };
        let text = f.to_json();
        if self.needs.sent.get(pane_id) == Some(&text) {
            return;
        }
        // An empty set nobody was told about needs no frame.
        if !self.needs.sent.contains_key(pane_id) && !self.needs.open.contains_key(pane_id) {
            return;
        }
        self.needs.sent.insert(pane_id.to_string(), text);
        self.send_frame(f).await;
    }

    /// After `welcome`: every open set again (a gateway keeps the last per
    /// pane), and the machine's set always, even empty: it tells web this
    /// machine's needs-you items come through the contract (web then leaves
    /// the older `blocked` event out of push).
    pub(crate) async fn needs_resend(&mut self) {
        self.needs.reset_sent();
        if !self.gateway_relays_needs() {
            return;
        }
        self.needs_sync(MACHINE).await;
        let panes: Vec<String> = self.needs.open.keys().filter(|p| !p.is_empty()).cloned().collect();
        for p in panes {
            self.needs_send(&p).await;
        }
        if !self.needs.sent.contains_key(MACHINE) {
            let f = RunnerFrame::Needs {
                pane_id: MACHINE.into(),
                items: vec![],
            };
            self.needs.sent.insert(MACHINE.into(), f.to_json());
            self.send_frame(f).await;
        }
    }

    /// Panes gone from the runner: close their items.
    pub(crate) async fn needs_gc(&mut self) {
        let gone: Vec<String> = self
            .needs
            .open
            .keys()
            .filter(|p| !p.is_empty() && !self.panes.contains_key(*p))
            .cloned()
            .collect();
        for p in gone {
            self.needs_sync(&p).await;
            self.needs.sent.remove(&p);
        }
    }

    /// `needs.list` / `needs.respond` (gateway or local).
    pub(crate) async fn needs_request(&mut self, method: &str, p: &Value, reply: ReplyTo) {
        match method {
            "needs.list" => {
                let items: Vec<&NeedsYou> = self.needs.open.values().flatten().collect();
                let v = json!({ "type": "needs_list", "items": items });
                respond(reply, Ok(v), self)
            }
            "needs.respond" => {
                if matches!(reply, ReplyTo::Local(_)) {
                    // Anything in a pane can reach the local socket; answers come from people, remotely.
                    let e = err(
                        "not_supported",
                        "needs.respond is answered from tmuxedo (web, app, TUI), not the local socket",
                    );
                    return respond(reply, Err(e), self);
                }
                match parse_respond(p) {
                    Ok(params) => self.needs_respond(params, reply).await,
                    Err(why) => respond(reply, Err(err("invalid_params", format!("needs.respond: {why}"))), self),
                }
            }
            _ => respond(reply, Err(err("unknown_method", method.to_string())), self),
        }
    }

    async fn needs_respond(&mut self, p: RespondParams, reply: ReplyTo) {
        let stale = |reply: ReplyTo, d: &mut Daemon| {
            respond(reply, Ok(json!({ "type": "needs_respond", "outcome": "stale" })), d)
        };
        if let Some(chat) = p.chat_id.clone() {
            let r = self.needs_chat_respond(&chat, &p).await;
            return respond(reply, r, self);
        }
        let Some(item) = self.needs.item(&p.item_id).cloned() else {
            return stale(reply, self);
        };
        if p.pane_id.as_deref().is_some_and(|x| x != item.pane_id) {
            return stale(reply, self);
        }
        let Some(r) = item.response(&p.response_id).cloned() else {
            let e = err("invalid_params", format!("this item has no response {}", p.response_id));
            return respond(reply, Err(e), self);
        };
        if item.risk == "high" && r.kind == "approve" && p.fresh_stepup != Some(true) {
            let e = err(
                "stepup_required",
                "a high-risk action needs a fresh confirmation (within 5 minutes); confirm it's you and answer again",
            );
            return respond(reply, Err(e), self);
        }
        if let Some(outcome) = self.memory_warn_respond(&item, &r).await {
            return respond(reply, Ok(json!({ "type": "needs_respond", "outcome": outcome })), self);
        }
        if let Some(outcome) = self.usage_alert_respond(&item, &r.id).await {
            return respond(reply, Ok(json!({ "type": "needs_respond", "outcome": outcome })), self);
        }
        let pane = item.pane_id.clone();
        match item.answer_via.as_str() {
            "hook" => {
                let behavior = match r.kind.as_str() {
                    "approve" => Behavior::Allow,
                    "deny" => Behavior::Deny,
                    _ => {
                        let e = err("invalid_params", "a held prompt is answered with approve or deny");
                        return respond(reply, Err(e), self);
                    }
                };
                let held = self.prompts.get(&item.id).is_some_and(|h| h.pane_id == pane);
                if !held {
                    return stale(reply, self);
                }
                let message = p.text.clone().filter(|t| !t.trim().is_empty());
                self.needs.hint(&item.id, "remote", Some(r.id.clone()));
                log!("needs you: {} answered remotely ({})", item.id, r.id);
                self.on_decision(pane, item.id.clone(), behavior, message).await;
                respond(
                    reply,
                    Ok(json!({ "type": "needs_respond", "outcome": "accepted" })),
                    self,
                )
            }
            "keystroke" if item.kind == "permission" && self.is_key_prompt(&item.id).is_some() => {
                let allow = match r.kind.as_str() {
                    "approve" => true,
                    "deny" => false,
                    _ => {
                        let e = err("invalid_params", "a dialog is answered with approve or deny");
                        return respond(reply, Err(e), self);
                    }
                };
                let before = self.prt.key_prompts.len();
                self.key_decision(&pane, &item.id, allow).await;
                // key_decision drops a stale one; a sent key stays until the screen moves.
                let sent = self.prt.key_prompts.len() == before;
                self.needs.hint(&item.id, "remote", Some(r.id.clone()));
                let outcome = if sent { "accepted" } else { "stale" };
                respond(reply, Ok(json!({ "type": "needs_respond", "outcome": outcome })), self)
            }
            "keystroke" => self.needs_keys(item, r, p, reply).await,
            "upload" => self.file_ask_respond(&item, &r, reply).await,
            "none" if item.id.starts_with("ci-") && item.source == "core" => self.ci_respond(&item, &r, reply).await,
            "action" => {
                let Some(action) = r.action_id.clone().filter(|_| r.kind == "action") else {
                    let e = err("invalid_params", "this response is not an action");
                    return respond(reply, Err(e), self);
                };
                let params = json!({ "plugin_id": item.plugin, "action_id": action, "context": {} });
                match self.plugin_request("plugin.action.invoke", &params).await {
                    Ok(_) => {
                        log!("needs you: {} ran action {action}", item.id);
                        respond(
                            reply,
                            Ok(json!({ "type": "needs_respond", "outcome": "accepted" })),
                            self,
                        )
                    }
                    Err(e) => respond(reply, Err(e), self),
                }
            }
            _ => {
                let e = err("answer_in_terminal", "this one is answered in the terminal");
                respond(reply, Err(e), self)
            }
        }
    }

    /// A keystroke answer: same screen, allowed keys, then watch the screen move.
    async fn needs_keys(&mut self, item: NeedsYou, r: NeedsResponse, p: RespondParams, reply: ReplyTo) {
        let agent = self
            .panes
            .get(&item.pane_id)
            .and_then(|rt| rt.track.kind().map(str::to_owned));
        let policy = agent.and_then(|a| self.keystroke_policy(&a, &item.kind).1);
        let Some((keys, reply_ok, rows)) = policy else {
            let e = err(
                "keystroke_off",
                "answering by keys is off for this agent (Settings → Terminal)",
            );
            return respond(reply, Err(e), self);
        };
        let bytes = match keystroke_bytes(&item, &r, &p, &keys, reply_ok) {
            Ok(b) => b,
            Err((code, msg)) => return respond(reply, Err(err(code, msg)), self),
        };
        let Some(rt) = self.panes.get(&item.pane_id).filter(|rt| rt.alive) else {
            return respond(reply, Ok(json!({ "type": "needs_respond", "outcome": "stale" })), self);
        };
        let now_fp = fingerprint(&region(&rt.emu.visible_text(), rows));
        if Some(&now_fp) != item.fingerprint.as_ref() {
            log!(
                "needs you: {} refused: the screen changed since it was asked (nothing typed)",
                item.id
            );
            self.needs.hint(&item.id, "screen_changed", None);
            self.needs
                .settled
                .insert(item.pane_id.clone(), rt.track.state_change_seq);
            if let Some(v) = self.needs.open.get_mut(&item.pane_id) {
                v.retain(|i| i.id != item.id);
            }
            // The set changed (the item went); re-derive, so a still-blocked pane gets a fresh item.
            self.needs_close_removed(&item).await;
            return respond(reply, Ok(json!({ "type": "needs_respond", "outcome": "stale" })), self);
        }
        let hid = rt.hid.clone();
        let seq = rt.track.state_change_seq;
        self.holder.input(&hid, &bytes);
        self.needs.settled.insert(item.pane_id.clone(), seq);
        log!(
            "needs you: {} answered by keys ({}, {} bytes)",
            item.id,
            r.id,
            bytes.len()
        );
        self.needs.hint(&item.id, "remote", Some(r.id.clone()));
        let now = Instant::now();
        self.needs.pending.push(PendingKeys {
            pane: item.pane_id.clone(),
            item: item.id.clone(),
            before: now_fp,
            rows,
            deadline: now + KEYS_SETTLE,
            reply,
        });
        self.needs.next_poll = Some(now + KEYS_POLL);
    }

    /// An item removed by hand (a stale keystroke): report it and send the set.
    async fn needs_close_removed(&mut self, item: &NeedsYou) {
        let (by, response) = self.needs.hints.remove(&item.id).unwrap_or(("terminal", None));
        let mut d = json!({ "id": item.id, "kind": item.kind, "plugin": item.plugin, "by": by });
        if let Some(r) = response {
            d["responseId"] = json!(r);
        }
        if self.gateway_relays_needs() {
            self.send_event(&item.pane_id, "needs_resolved", d).await;
        }
        if self.needs.open.get(&item.pane_id).is_some_and(Vec::is_empty) {
            self.needs.open.remove(&item.pane_id);
        }
        self.needs_send(&item.pane_id).await;
    }

    /// Pending keystroke answers: the region moved (accepted) or did not in time (unknown).
    pub(crate) async fn needs_tick(&mut self, now: Instant) {
        if self.needs.pending.is_empty() || self.needs.next_poll.is_some_and(|t| now < t) {
            return;
        }
        self.needs.next_poll = Some(now + KEYS_POLL);
        let pending = std::mem::take(&mut self.needs.pending);
        let mut keep = vec![];
        for k in pending {
            let moved = self
                .panes
                .get(&k.pane)
                .is_none_or(|rt| !rt.alive || fingerprint(&region(&rt.emu.visible_text(), k.rows)) != k.before);
            if moved {
                respond(
                    k.reply,
                    Ok(json!({ "type": "needs_respond", "outcome": "accepted" })),
                    self,
                );
                // The question is answered: close it now (a later screen change may open another).
                if let Some(it) = self.needs.item(&k.item).cloned() {
                    if let Some(v) = self.needs.open.get_mut(&k.pane) {
                        v.retain(|i| i.id != k.item);
                    }
                    self.needs_close_removed(&it).await;
                }
            } else if now >= k.deadline {
                log!("needs you: {} typed, but the screen did not change within 2 s", k.item);
                self.needs.hints.remove(&k.item);
                respond(
                    k.reply,
                    Ok(json!({ "type": "needs_respond", "outcome": "unknown" })),
                    self,
                );
            } else {
                keep.push(k);
            }
        }
        self.needs.pending.extend(keep);
        if self.needs.pending.is_empty() {
            self.needs.next_poll = None;
        }
    }

    /// `needs.respond` for a chat's item: `chat.respond` with the same words.
    async fn needs_chat_respond(&mut self, chat: &str, p: &RespondParams) -> ApiResult {
        let kind = match p.response_id.as_str() {
            "allow_once" | "allow_session" | "deny" | "answers" | "approve_plan" | "keep_planning" => {
                p.response_id.clone()
            }
            "approve" => "allow_once".into(),
            other => return Err(err("invalid_params", format!("a chat item has no response {other}"))),
        };
        let mut response = json!({ "kind": kind });
        if let Some(t) = &p.text {
            response["message"] = json!(t);
        }
        if let Some(a) = &p.answers {
            response["answers"] = Value::Object(a.clone());
        }
        let mut params = json!({ "session_id": chat, "need_id": p.item_id, "response": response });
        if let Some(b) = &p.by {
            params["by"] = json!(b);
        }
        if p.fresh_stepup == Some(true) {
            params["fresh_stepup"] = json!(true);
        }
        let v = self.chat_respond(&params).await?;
        Ok(json!({ "type": "needs_respond", "outcome": v["outcome"] }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen() -> &'static str {
        "some output\n\n? Which colour should the button be?\n  1. Red\n❯ 2. Blue\n  3) Green\n\nEsc to cancel\n"
    }

    #[test]
    fn region_fingerprint_and_options() {
        let r = region(screen(), 12);
        assert_eq!(r.len(), 6);
        assert_eq!(r[1], "? Which colour should the button be?");
        let o = options(&r);
        assert_eq!(
            o.iter().map(|x| (x.id.as_str(), x.label.as_str())).collect::<Vec<_>>(),
            [("1", "Red"), ("2", "Blue"), ("3", "Green")]
        );
        assert_eq!(question(&r).as_deref(), Some("? Which colour should the button be?"));
        let fp = fingerprint(&r);
        assert_eq!(fp.len(), 64);
        // Spacing alone does not change it; a changed line does.
        assert_eq!(fp, fingerprint(&region(&screen().replace("1. Red", "1.   Red"), 12)));
        assert_ne!(fp, fingerprint(&region(&format!("{}more\n", screen()), 12)));
        assert_eq!(region("a\nb\nc\n", 2), ["b", "c"]);
    }

    #[test]
    fn titles() {
        assert_eq!(
            permission_title("Bash", &json!({"command": "npm  test"})),
            "Run npm test"
        );
        assert_eq!(
            permission_title("Write", &json!({"file_path": "/w/a.md"})),
            "Edit /w/a.md"
        );
        assert_eq!(permission_title("mcp__x", &json!({})), "Use mcp__x");
        let long = permission_title("Bash", &json!({"command": "x".repeat(500)}));
        assert_eq!(long.chars().count(), MAX_TITLE);
        assert!(long.ends_with('…'));
    }

    fn q() -> NeedsYou {
        let mut choose = resp("choose", "choose", "Choose", "stepup");
        choose.options = Some(vec![
            NeedsOption {
                id: "1".into(),
                label: "Red".into(),
            },
            NeedsOption {
                id: "2".into(),
                label: "Blue".into(),
            },
        ]);
        NeedsYou {
            id: "q-1".into(),
            pane_id: "w1:p1".into(),
            chat_id: None,
            plugin: "claude".into(),
            kind: "question".into(),
            title: "t".into(),
            detail: None,
            context: None,
            risk: "normal".into(),
            risk_class: None,
            why: None,
            responses: vec![
                choose,
                resp("reply", "reply", "Reply", "stepup"),
                resp("cancel", "deny", "Cancel", "stepup"),
            ],
            answer_via: "keystroke".into(),
            fingerprint: Some("0".repeat(64)),
            source: "hook".into(),
            created_at: String::new(),
            expires_at: None,
        }
    }

    fn params(v: Value) -> RespondParams {
        parse_respond(&v).unwrap()
    }

    #[test]
    fn keystroke_answers_send_only_allowed_keys() {
        let it = q();
        let keys: Vec<String> = ["1", "2", "Escape"].iter().map(|s| s.to_string()).collect();
        let choose = it.response("choose").unwrap();
        let p = params(json!({"itemId": "q-1", "responseId": "choose", "optionId": "2"}));
        assert_eq!(keystroke_bytes(&it, choose, &p, &keys, true).unwrap(), b"2");
        // A choice the item does not offer, or a key not on the list.
        let p = params(json!({"itemId": "q-1", "responseId": "choose", "optionId": "3"}));
        assert_eq!(
            keystroke_bytes(&it, choose, &p, &keys, true).unwrap_err().0,
            "key_not_allowed"
        );
        let only1 = vec!["1".to_string()];
        let p = params(json!({"itemId": "q-1", "responseId": "choose", "optionId": "2"}));
        assert_eq!(
            keystroke_bytes(&it, choose, &p, &only1, true).unwrap_err().0,
            "key_not_allowed"
        );
        // Text: one line, then Enter; never when replies are off.
        let reply = it.response("reply").unwrap();
        let p = params(json!({"itemId": "q-1", "responseId": "reply", "text": "Blue\nplease"}));
        assert_eq!(keystroke_bytes(&it, reply, &p, &keys, true).unwrap(), b"Blue please\r");
        assert_eq!(
            keystroke_bytes(&it, reply, &p, &keys, false).unwrap_err().0,
            "key_not_allowed"
        );
        // Cancel is Escape, only when Escape is allowed.
        let cancel = it.response("cancel").unwrap();
        let p = params(json!({"itemId": "q-1", "responseId": "cancel"}));
        assert_eq!(keystroke_bytes(&it, cancel, &p, &keys, true).unwrap(), b"\x1b");
        assert!(keystroke_bytes(&it, cancel, &p, &only1, true).is_err());
    }
}
