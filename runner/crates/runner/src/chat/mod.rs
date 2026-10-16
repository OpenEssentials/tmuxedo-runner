//! The structured-session host: agent chat (docs/design/agent-chat.md §3,
//! docs/protocol.md "Agent chat").
//!
//! A chat session drives an agent CLI in its structured mode as a holder
//! **pipe child** (so a runner restart or self-update never kills a chat
//! mid-turn), translates its stdout with the plugin's translator into the
//! normalised event model, masks every event, numbers it and appends it to
//! the session's log, and relays it to whoever watches as CHAT frames.
//!
//! Decisions (permission requests, questions, plans) arrive through the
//! plugin's held hooks, tagged with the chat (`TMUXEDO_CHAT_ID`), become
//! needs-you items (`answerVia: session`) and are answered by id with
//! `chat.respond`: exact, never keystrokes; an item that is gone answers
//! `stale`.
//!
//! One driver per agent session: `chat.handoff` moves the same session to a
//! terminal pane (`claude --resume <id>`) and back.
//!
//! The runner API methods (`chat.*`) are the surface for the gateway, web's
//! REST twin (`/api/v1/machines/{id}/chats…`) and the Phase 18 headless
//! task API.

pub mod acp;
pub mod claude;
pub mod codex;
pub mod files;
pub mod log;
pub mod mask;

use crate::api::{err, respond, ApiErr, ApiResult, ReplyTo};
use crate::daemon::Daemon;
use crate::local::LocalReq;
use crate::plugin::manifest::AUTO_APPROVE;
use crate::plugin::AgentPlugin;
use crate::util::iso_now;
use log::EventLog;
use mask::Masker;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tmuxedo_protocol::chat::{
    valid_session_id, ChatBody, ChatEvent, ChatFrame, NeedsItem, MAX_EVENT_BYTES, MAX_SEND_BYTES,
};
use tmuxedo_protocol::frames::RunnerFrame;
use tokio_tungstenite::tungstenite::Message;

/// An idle chat nobody watches parks after this (design §5.3).
pub const PARK_AFTER: Duration = Duration::from_secs(30 * 60);
/// Why a subscription login may not run unattended (D-C4 "guarded (b)", 2026-10-10).
const UNATTENDED_WHY: &str = "On a Claude subscription login, runs with nobody watching need an API key \
     (ANTHROPIC_API_KEY or CLAUDE_CODE_OAUTH_TOKEN in Settings → Secrets).";
/// A chat nobody watches for this long is unattended (a page reload or a reconnect is not).
pub const UNWATCHED_GRACE: Duration = Duration::from_secs(60);
/// A stdout line longer than this is dropped (with a notice).
const MAX_LINE: usize = 16 * 1024 * 1024;
/// How long a held decision waits (the hook's own timeout is 600 s).
const HOLD: Duration = Duration::from_secs(540);
/// Replayed events per `chat.read` call at most.
const READ_MAX: usize = 1000;
/// `clientMsgId`s remembered per session (a resent message is not sent twice).
const RECENT_IDS: usize = 256;
/// A json-rpc child that has not opened its conversation after this is stopped.
const RPC_HANDSHAKE: Duration = Duration::from_secs(30);
/// A json-rpc child that has not ended an interrupted turn after this is stopped.
const RPC_INTERRUPT: Duration = Duration::from_secs(10);
/// The transport whose child is asked and answers on its stdio (Codex's app-server).
const JSON_RPC: &str = "json-rpc";
/// The Agent Client Protocol: also asked and answered on stdio, by the generic translator.
const ACP: &str = "acp";

/// Call the same method of whichever stdio translator the session uses
/// (`acp` for an ACP agent, `cx` for Codex's app-server): both keep one contract.
macro_rules! rpc_eng {
    ($meta:expr, $($call:tt)+) => {
        if $meta.engine == ACP {
            $meta.acp.$($call)+
        } else {
            $meta.cx.$($call)+
        }
    };
}

/// The session record on disk (`meta.json`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Meta {
    pub id: String,
    pub plugin: String,
    pub cwd: String,
    pub mode: String,
    pub login: String,
    pub title: String,
    pub created_at: String,
    pub last_at: String,
    pub preview: String,
    /// Who wrote the message `preview` is cut from: `agent` | `you`. Empty
    /// for a record written before this was kept (the summary then omits
    /// `previewBy`, and a client draws the preview as the agent's).
    pub preview_by: String,
    /// The CLI's own session id (Claude: a UUID given with `--session-id`).
    pub vendor_session: String,
    /// The CLI has recorded the session (later starts use `resume`).
    pub started: bool,
    /// Holder incarnation (`chat-<id>-<n>`).
    pub incarnation: u32,
    /// `idle` | `working` | `needs_you` | `parked` | `terminal` | `closed`.
    pub status: String,
    /// Stdout offset (in this incarnation) after the last line handled.
    pub offset: u64,
    #[serde(default)]
    pub turn: Option<String>,
    #[serde(default)]
    pub turns: u64,
    /// The pane the session continues in after a handoff to the terminal.
    #[serde(default)]
    pub pane_id: Option<String>,
    /// Needs-you items open when this was written (closed as `expired`
    /// after a runner restart: their hooks went with the old runner).
    #[serde(default)]
    pub open_needs: Vec<String>,
    #[serde(default)]
    pub tr: claude::TState,
    /// The chat transport when it is not stream-json (`json-rpc`); set at each start.
    #[serde(default)]
    pub engine: String,
    /// The json-rpc translator's state (Codex's app-server).
    #[serde(default)]
    pub cx: codex::CState,
    /// The ACP translator's state (any agent with `transport = "acp"`).
    #[serde(default)]
    pub acp: acp::AState,
}

pub(crate) struct OpenNeed {
    pub item: NeedsItem,
    pub native: Value,
    pub reply: Option<ReplyTo>,
    pub deadline: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stop {
    Park,
    Close,
    Handoff,
}

pub(crate) struct Session {
    pub meta: Meta,
    pub log: EventLog,
    pub masker: Masker,
    dir: PathBuf,
    buf: Vec<u8>,
    /// Offset after the last stdout byte received in this incarnation.
    recv_end: u64,
    /// Drop bytes up to the next newline (after a gap or an oversized line).
    resync: bool,
    pub alive: bool,
    /// The agent child's pid (0 = not known): its hooks must come from below it (crate::ancestry).
    pub pid: u32,
    pub attached: bool,
    pub needs: BTreeMap<String, OpenNeed>,
    queue: VecDeque<String>,
    idle_since: Instant,
    recent: VecDeque<(String, u64)>,
    stopping: Option<Stop>,
    /// A `chat.handoff` waiting for the agent to stop.
    pending: Option<ReplyTo>,
    /// Restore `mode` after the current turn (a change asked for mid-turn).
    mode_after_turn: bool,
    /// Since when nobody watches (None while someone does): unattended after [`UNWATCHED_GRACE`].
    unwatched_since: Option<Instant>,
    /// Hook calls from this chat's agent (diagnostics: the CLI's hooks reach the runner).
    hooks_seen: u64,
    /// What Auto-approve let through in the current turn (audited at its end).
    auto_approved: Vec<String>,
    /// The work folder's git status when the current turn started (the
    /// turn-end scan finds files a shell command wrote), if it is a repo.
    scan_base: Option<files::Snapshot>,
    /// Paths (relative to the work folder) tool events reported this turn.
    turn_files: BTreeSet<String>,
    /// A json-rpc child must have answered by then (what it was asked: `start` | `stop`).
    rpc_deadline: Option<(Instant, &'static str)>,
    /// The agent's last finished message, masked (a headless task's result).
    pub last_reply: String,
}

impl Session {
    /// Driven over JSON-RPC (requests both ways on stdio) rather than stream-json and hooks.
    fn rpc(&self) -> bool {
        self.meta.engine == JSON_RPC || self.meta.engine == ACP
    }
    fn hid(&self) -> String {
        format!("chat-{}-{}", self.meta.id, self.meta.incarnation)
    }
    /// The log's source offset for a stdout line ending at `end`.
    fn src(&self, end: u64) -> u64 {
        (u64::from(self.meta.incarnation) << 40) | (end & ((1 << 40) - 1))
    }
    pub(crate) fn status(&self) -> &'static str {
        match self.meta.status.as_str() {
            "closed" => "closed",
            "terminal" => "terminal",
            _ if !self.needs.is_empty() => "needs_you",
            _ if self.meta.turn.is_some() => "working",
            _ if self.alive => "idle",
            _ => "parked",
        }
    }
    fn save(&mut self) {
        self.meta.status = self.status().into();
        self.meta.open_needs = self.needs.keys().cloned().collect();
        if let Ok(b) = serde_json::to_vec_pretty(&self.meta) {
            if let Err(e) = crate::config::write_private(&self.dir.join("meta.json"), &b) {
                crate::log!("chat {}: saving meta: {e}", self.meta.id);
            }
        }
    }
    fn info(&self) -> Value {
        let m = &self.meta;
        let mut v = json!({
            "id": m.id, "plugin": m.plugin, "cwd": m.cwd, "mode": m.mode, "login": m.login,
            "title": m.title, "status": self.status(), "createdAt": m.created_at, "lastAt": m.last_at,
            "preview": m.preview, "firstSeq": self.log.first_seq, "lastSeq": self.log.last_seq,
            "needs": self.needs.len(), "queued": self.queue.len(), "paneId": m.pane_id,
            "vendorSessionId": m.vendor_session, "model": m.tr.model, "hooksSeen": self.hooks_seen,
        });
        // Additive: only the two known writers are ever sent; a record from
        // before `preview_by` was kept sends nothing.
        if matches!(m.preview_by.as_str(), "agent" | "you") {
            v["previewBy"] = json!(m.preview_by);
        }
        v
    }
}

/// Every chat on this machine.
pub struct Chats {
    dir: PathBuf,
    pub(crate) sessions: BTreeMap<String, Session>,
    /// Web's policy: may a subscription login chat? (admins only by default;
    /// unknown until the first `config` = no).
    pub subscription_allowed: bool,
    /// Web's policy: may this machine's owner turn on Auto-approve? (an admin
    /// may forbid it; unknown until the first `config` = no).
    pub auto_approve_allowed: bool,
    /// Web's policy (D-C4 "guarded (b)", 2026-10-10): may this machine's owner run
    /// a subscription login unattended (Auto-approve with nobody watching; later
    /// headless and scheduled runs)? Admins yes; everyone else needs an API key.
    /// Absent from an older web = allowed (that web had no such rule).
    pub unattended_allowed: bool,
    /// How long a chat may go unwatched (a reload, a reconnect) before it counts as unattended.
    pub unwatched_grace: Duration,
    pub park_after: Duration,
    last_prune: Option<Instant>,
}

fn uuid_v4() -> String {
    let mut b = crate::util::random_bytes(16);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = hex::encode(b);
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

fn new_session_id() -> String {
    format!("c-{}", &crate::util::random_id()[..20])
}

/// The first line of a text, one line, cut (a preview or a title).
fn preview(t: &str, max: usize) -> String {
    let flat: String = t.split_whitespace().collect::<Vec<_>>().join(" ");
    claude::cut(&flat, max)
}

/// Who wrote a previewed message (the summary's `previewBy`).
fn preview_by(kind: &str) -> &'static str {
    match kind {
        "message.user" => "you",
        _ => "agent",
    }
}

/// Make a serialized event fit [`MAX_EVENT_BYTES`]: halve the longest
/// string until it does, and say so.
fn fit(v: &mut Value) {
    for _ in 0..40 {
        let len = serde_json::to_vec(v).map(|b| b.len()).unwrap_or(0);
        if len <= MAX_EVENT_BYTES {
            return;
        }
        fn longest(v: &mut Value) -> Option<&mut String> {
            let mut best: Option<&mut String> = None;
            let mut stack: Vec<&mut Value> = vec![v];
            while let Some(x) = stack.pop() {
                match x {
                    Value::String(s) => {
                        if best.as_ref().is_none_or(|b| s.len() > b.len()) {
                            best = Some(s);
                        }
                    }
                    Value::Array(a) => stack.extend(a.iter_mut()),
                    Value::Object(o) => stack.extend(o.values_mut()),
                    _ => {}
                }
            }
            best
        }
        match longest(v) {
            Some(s) if s.len() > 64 => {
                let keep = s.len() / 2;
                *s = claude::cut(s, keep);
            }
            _ => return,
        }
        v["truncated"] = json!(true);
    }
}

/// The real agent binary on PATH, never the shim.
fn find_agent(bin: &str, shim: Option<&Path>) -> Option<PathBuf> {
    let path = std::env::var("PATH").unwrap_or_default();
    for d in path.split(':').filter(|d| !d.is_empty()) {
        let dir = Path::new(d);
        if shim.is_some_and(|s| dir == s || dir.canonicalize().ok().as_deref() == s.canonicalize().ok().as_deref()) {
            continue;
        }
        let p = dir.join(bin);
        if let Ok(m) = std::fs::metadata(&p) {
            use std::os::unix::fs::PermissionsExt;
            if m.is_file() && m.permissions().mode() & 0o111 != 0 {
                return Some(p);
            }
        }
    }
    None
}

fn str_param<'a>(p: &'a Value, k: &str) -> Option<&'a str> {
    p.get(k).and_then(Value::as_str)
}

impl Chats {
    /// Load every session record under `dir` (`<state>/chat`).
    pub fn load(dir: &Path, park_after: Duration) -> Chats {
        let mut c = Chats {
            dir: dir.to_path_buf(),
            sessions: BTreeMap::new(),
            subscription_allowed: false,
            auto_approve_allowed: false,
            unattended_allowed: true,
            unwatched_grace: UNWATCHED_GRACE,
            park_after,
            last_prune: None,
        };
        let Ok(rd) = std::fs::read_dir(dir) else { return c };
        for e in rd.flatten() {
            let d = e.path();
            let Some(id) = d.file_name().and_then(|n| n.to_str()).map(str::to_owned) else {
                continue;
            };
            if !valid_session_id(&id) {
                continue;
            }
            let Some(meta) = std::fs::read(d.join("meta.json"))
                .ok()
                .and_then(|b| serde_json::from_slice::<Meta>(&b).ok())
                .filter(|m| m.id == id)
            else {
                continue;
            };
            let log = EventLog::open(&d);
            c.sessions.insert(id, Session::new(meta, log, d));
        }
        c
    }

    fn session_dir(&self, id: &str) -> PathBuf {
        self.dir.join(id)
    }
}

impl Session {
    fn new(meta: Meta, log: EventLog, dir: PathBuf) -> Session {
        Session {
            meta,
            log,
            masker: Masker::default(),
            dir,
            buf: vec![],
            recv_end: 0,
            resync: false,
            alive: false,
            pid: 0,
            attached: false,
            needs: BTreeMap::new(),
            queue: VecDeque::new(),
            idle_since: Instant::now(),
            recent: VecDeque::new(),
            stopping: None,
            pending: None,
            mode_after_turn: false,
            unwatched_since: Some(Instant::now()),
            hooks_seen: 0,
            auto_approved: vec![],
            scan_base: None,
            turn_files: BTreeSet::new(),
            rpc_deadline: None,
            last_reply: String::new(),
        }
    }
}

// ── the host, on the daemon ─────────────────────────────────────────────

/// Method names (for docs, the gateway's read-only table and web's method table).
pub const METHODS: &[&str] = &[
    "chat.start",
    "chat.list",
    "chat.get",
    "chat.read",
    "chat.send",
    "chat.interrupt",
    "chat.mode",
    "chat.respond",
    "chat.handoff",
    "chat.close",
    "chat.file",
];

impl Daemon {
    fn chat_mut(&mut self, p: &Value) -> Result<&mut Session, ApiErr> {
        let id = str_param(p, "session_id").ok_or_else(|| err("invalid_params", "session_id is required"))?;
        self.chats
            .sessions
            .get_mut(id)
            .ok_or_else(|| err("chat_not_found", format!("no chat {id}")))
    }

    fn chat_plugin(&self, id: &str) -> Result<std::sync::Arc<dyn AgentPlugin>, ApiErr> {
        let p = self
            .plugins
            .get(id)
            .filter(|_| self.plugins.enabled(id))
            .ok_or_else(|| err("unknown_agent_kind", format!("no enabled agent plugin {id}")))?;
        if p.manifest().chat.is_none() {
            return Err(err(
                "chat_not_supported",
                format!("{} has no chat mode; use the terminal", p.manifest().name),
            ));
        }
        Ok(p)
    }

    /// The structured mode needs a new enough agent CLI (`chat.min_agent`): an older one gets
    /// no chat and no "Open as chat", and whoever asks is told why. A version that could not
    /// be read is not refused (design §8.4: an untested version still chats).
    fn chat_agent_ok(&self, plugin: &dyn AgentPlugin) -> Result<(), ApiErr> {
        let min = plugin
            .manifest()
            .chat
            .as_ref()
            .map(|c| c.min_agent.as_str())
            .unwrap_or("");
        let have = self.wire.agent_versions.get(plugin.id()).cloned().flatten();
        match agent_too_old(have.as_deref(), min) {
            true => Err(err(
                "chat_agent_too_old",
                format!(
                    "Chat needs {} {min} or newer; this machine has {}. Update it, or use the terminal.",
                    plugin.manifest().name,
                    have.unwrap_or_default()
                ),
            )),
            false => Ok(()),
        }
    }

    /// The environment a chat child gets on top of the holder's own.
    fn chat_env(&self, sid: &str, plugin: &dyn AgentPlugin) -> std::collections::BTreeMap<String, String> {
        let sock = self.opts.runner_socket.to_string_lossy().into_owned();
        let mut env = std::collections::BTreeMap::new();
        for p in ["TMUXEDO", "AIMUX"] {
            env.insert(format!("{p}_CHAT_ID"), sid.to_string());
            env.insert(format!("{p}_SOCKET_PATH"), sock.clone());
        }
        for (k, v) in &self.launch.shell.env {
            env.entry(k.clone()).or_insert_with(|| v.clone());
        }
        env.retain(|k, _| !plugin.scrubs(k));
        env
    }

    /// Which login a chat child would use: the first `chat.login_env` kind
    /// with a non-empty variable, else `subscription`.
    fn chat_login(&self, plugin: &dyn AgentPlugin, env: &std::collections::BTreeMap<String, String>) -> String {
        let Some(c) = &plugin.manifest().chat else {
            return "subscription".into();
        };
        let set = |k: &str| {
            env.get(k)
                .cloned()
                .or_else(|| std::env::var(k).ok())
                .is_some_and(|v| !v.trim().is_empty() && v != "0")
        };
        for kind in ["api_key", "cloud"] {
            if c.login_env.get(kind).is_some_and(|names| names.iter().any(|n| set(n))) {
                return kind.into();
            }
        }
        "subscription".into()
    }

    /// Per-login-kind gate (D-C4): a kind in `chat.logins`, or a subscription
    /// login when web's policy allows it for this machine's owner.
    fn chat_login_allowed(&self, plugin: &dyn AgentPlugin, login: &str) -> Result<(), ApiErr> {
        let c = plugin.manifest().chat.as_ref().expect("checked");
        if c.logins.iter().any(|l| l == login) || (login == "subscription" && self.chats.subscription_allowed) {
            return Ok(());
        }
        Err(err(
            "chat_login_not_allowed",
            format!(
                "Chat with {} on a {} login is not available for this account. Use the terminal instead: \
                 its permission prompts still come to you as buttons. (An API-key login can chat.)",
                plugin.manifest().name,
                if login == "subscription" { "subscription" } else { login }
            ),
        ))
    }

    pub(crate) async fn chat_apply_policy(&mut self, v: &Value) {
        if let Some(s) = v.get("subscription").and_then(Value::as_str) {
            let allow = s == "allow";
            if allow != self.chats.subscription_allowed {
                crate::log!(
                    "chat policy: subscription chat {}",
                    if allow { "allowed" } else { "not allowed" }
                );
            }
            self.chats.subscription_allowed = allow;
        }
        // Unattended runs on a subscription login (D-C4 guarded (b)): absent (an older web) = allowed.
        self.chats.unattended_allowed = v.get("unattended").and_then(Value::as_str) != Some("deny");
        // Absent (an older web) = not allowed: Auto-approve is opt-in on both sides.
        let allow = v.get("autoApprove").and_then(Value::as_str) == Some("allow");
        if allow != self.chats.auto_approve_allowed {
            crate::log!(
                "chat policy: Auto-approve {}",
                if allow { "allowed" } else { "not allowed" }
            );
        }
        self.chats.auto_approve_allowed = allow;
        // The subscription policy decides which panes may open as a chat.
        self.chat_offers_changed().await;
        if !allow {
            // Forbidden now: every chat in Auto-approve goes back to "Ask first" (at its next turn if busy).
            let on: Vec<String> = self
                .chats
                .sessions
                .values()
                .filter(|s| s.meta.mode == AUTO_APPROVE && s.meta.status != "closed")
                .map(|s| s.meta.id.clone())
                .collect();
            for sid in on {
                self.chat_set_mode(&sid, "ask_first").await;
                self.chat_notice(
                    &sid,
                    "auto_approve_off",
                    "Auto-approve was turned off for this account by an admin. The agent asks before it acts again."
                        .into(),
                )
                .await;
                self.chat_save(&sid);
            }
        }
    }

    /// Write the chat hook settings: the plugin's hooks plus `chat.hooks`,
    /// and in Auto-approve the guard (`hook --guard` on every tool).
    fn chat_settings(&self, plugin: &dyn AgentPlugin, auto: bool) -> Result<PathBuf, ApiErr> {
        let mut v = plugin
            .hook_settings(&self.opts.hook_exe)
            .unwrap_or_else(|| json!({ "hooks": {} }));
        let command = format!(
            "{} hook{}",
            crate::plugin::sh_quote(&self.opts.hook_exe.to_string_lossy()),
            if plugin.id() == crate::plugin::DEFAULT_PLUGIN {
                String::new()
            } else {
                format!(" --plugin {}", plugin.id())
            }
        );
        for r in &plugin.manifest().chat.as_ref().expect("checked").hooks {
            let mut hk = json!({ "type": "command", "command": command });
            if let Some(t) = r.timeout {
                hk["timeout"] = json!(t);
            }
            let mut entry = json!({ "hooks": [hk] });
            if let Some(m) = &r.matcher {
                entry["matcher"] = json!(m);
            }
            let list = v["hooks"][&r.event].as_array().cloned().unwrap_or_default();
            let mut list = list;
            list.push(entry);
            v["hooks"][&r.event] = Value::Array(list);
        }
        if auto {
            // No matcher: every tool. PreToolUse still runs in the CLI's bypass mode, and a
            // deny there stops the call; the guard fails closed when the runner cannot answer.
            let guard = json!({ "hooks": [{
                "type": "command",
                "command": format!("{} hook --guard", crate::plugin::sh_quote(&self.opts.hook_exe.to_string_lossy())),
                "timeout": 30,
            }] });
            let mut list = v["hooks"]["PreToolUse"].as_array().cloned().unwrap_or_default();
            list.insert(0, guard);
            v["hooks"]["PreToolUse"] = Value::Array(list);
        }
        let path = self.opts.state_dir.join(format!(
            "{}-chat-settings{}.json",
            plugin.id(),
            if auto { "-auto" } else { "" }
        ));
        let bytes = serde_json::to_vec_pretty(&v).map_err(|e| err("internal", e.to_string()))?;
        crate::config::write_private(&path, &bytes).map_err(|e| err("internal", e))?;
        Ok(path)
    }

    /// Start (or resume) the session's structured child in the holder.
    async fn chat_spawn(&mut self, sid: &str) -> Result<(), ApiErr> {
        let (plugin_id, resume, vendor, cwd, mode) = {
            let s = self
                .chats
                .sessions
                .get(sid)
                .ok_or_else(|| err("chat_not_found", "no such chat"))?;
            if s.alive {
                return Ok(());
            }
            let m = &s.meta;
            (
                m.plugin.clone(),
                m.started,
                m.vendor_session.clone(),
                m.cwd.clone(),
                m.mode.clone(),
            )
        };
        let plugin = self.chat_plugin(&plugin_id)?;
        let c = plugin.manifest().chat.clone().expect("checked");
        let env = self.chat_env(sid, plugin.as_ref());
        let login = self.chat_login(plugin.as_ref(), &env);
        self.chat_login_allowed(plugin.as_ref(), &login)?;
        self.chat_agent_ok(plugin.as_ref())?;
        let rpc = c.transport == JSON_RPC || c.transport == ACP;
        // A json-rpc agent names its session itself: there is one to reopen once it has.
        let resume = resume && (!rpc || !vendor.is_empty());
        let bin = plugin
            .manifest()
            .agent
            .binaries
            .first()
            .cloned()
            .unwrap_or_else(|| plugin.id().into());
        let agent = find_agent(&bin, self.shim_dir.as_deref()).ok_or_else(|| {
            err(
                "agent_not_installed",
                format!(
                    "{} is not installed on this machine ({bin} is not on PATH)",
                    plugin.manifest().name
                ),
            )
        })?;
        if !Path::new(&cwd).is_dir() {
            return Err(err("invalid_cwd", format!("{cwd} is not a directory")));
        }
        // Auto-approve forbidden since it was turned on (or never known): start in "Ask first".
        let mode = if mode == AUTO_APPROVE && !self.chats.auto_approve_allowed {
            if let Some(s) = self.chats.sessions.get_mut(sid) {
                s.meta.mode = "ask_first".into();
            }
            "ask_first".to_string()
        } else {
            mode
        };
        let settings = if c.argv.iter().any(|t| t == "{gen:chat-settings}") {
            self.chat_settings(plugin.as_ref(), mode == AUTO_APPROVE)?
        } else {
            PathBuf::new()
        };
        let mut argv = vec![];
        for t in &c.argv {
            match t.as_str() {
                "{agent}" => argv.push(agent.to_string_lossy().into_owned()),
                "{gen:chat-settings}" => argv.push(settings.to_string_lossy().into_owned()),
                "{settings.launch.args...}" => argv.extend(self.launch.claude_args.iter().cloned()),
                other => argv.push(other.to_string()),
            }
        }
        for t in if resume { &c.resume } else { &c.new_session } {
            argv.push(t.replace("{session_id}", &vendor));
        }
        if let Some(native) = c.native_mode(&mode) {
            if !c.mode_flag.is_empty() {
                argv.push(c.mode_flag.clone());
                argv.push(native.to_string());
            }
        }
        // Literal secrets the child can see are masked in every event.
        let mut masker = Masker::from_env(
            std::env::vars()
                .collect::<Vec<_>>()
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str())),
        );
        for (k, v) in &env {
            if mask::is_secret_name(k) {
                masker.add(v);
            }
        }
        masker.machine = self.secrets.shared();
        let (hid, stderr) = {
            let s = self.chats.sessions.get_mut(sid).expect("checked");
            s.meta.incarnation += 1;
            s.meta.offset = 0;
            s.meta.login = login.clone();
            s.meta.engine = if rpc { c.transport.clone() } else { String::new() };
            s.recv_end = 0;
            s.buf.clear();
            s.resync = false;
            s.masker = masker;
            (s.hid(), s.dir.join("stderr.log"))
        };
        let spec = tmuxedo_holder::wire::SpawnSpec {
            id: hid.clone(),
            argv,
            cwd: Some(cwd.clone()),
            env,
            cols: 80,
            rows: 24,
        };
        let call = self
            .holder
            .spawn_pipe(spec, Some(stderr.to_string_lossy().into_owned()));
        let got = tokio::time::timeout(Duration::from_secs(5), call).await;
        let pid = match got {
            Ok(Ok(v)) if v["pipe"] == true => v["pid"].as_u64().unwrap_or(0) as u32,
            Ok(Ok(_)) => {
                // An older holder ran it in a terminal: stop it.
                self.holder.kill_nowait(&hid, libc::SIGKILL);
                return Err(err(
                    "holder_outdated",
                    "this machine's holder is too old for chat; restart the tmuxedo service (or the machine) to update it",
                ));
            }
            Ok(Err(e)) => return Err(err("spawn_failed", format!("could not start {}: {e}", plugin.manifest().name))),
            Err(_) => {
                return Err(err(
                    "holder_outdated",
                    "this machine's holder did not answer a chat start; restart the tmuxedo service (or the machine) to update it",
                ))
            }
        };
        self.holder.attach_nowait(&hid, 0);
        if rpc {
            // The handshake: hello, then the conversation (new, or the one this chat already has).
            let native = c.native_mode(&mode).map(str::to_owned);
            let s = self.chats.sessions.get_mut(sid).expect("checked");
            let hello = rpc_eng!(
                s.meta,
                begin(
                    resume.then_some(vendor.as_str()),
                    &cwd,
                    native.as_deref(),
                    env!("CARGO_PKG_VERSION"),
                )
            );
            s.rpc_deadline = Some((Instant::now() + RPC_HANDSHAKE, "start"));
            self.holder.input(&hid, &hello);
        }
        crate::log!(
            "chat {sid}: started {} ({}){}",
            plugin.id(),
            hid,
            if resume { " resuming" } else { "" }
        );
        let features = c.features.clone();
        let model = {
            let s = self.chats.sessions.get_mut(sid).expect("checked");
            s.alive = true;
            s.pid = pid;
            s.idle_since = Instant::now();
            s.meta.tr.msg = None;
            s.meta.tr.tools.clear();
            s.meta.tr.interrupting = false;
            s.meta.tr.model.clone()
        };
        self.chat_emit(
            sid,
            ChatBody::SessionOpened {
                plugin: plugin.id().into(),
                cwd,
                mode,
                login,
                model,
                features,
                resumed_from: resume.then_some(vendor),
            },
            None,
            0,
        )
        .await;
        Ok(())
    }

    /// Number, mask, log and relay one event.
    pub(crate) async fn chat_emit(&mut self, sid: &str, body: ChatBody, agent_path: Option<String>, src: u64) {
        let Some(s) = self.chats.sessions.get_mut(sid) else {
            return;
        };
        let kind = body.kind();
        let ephemeral = body.ephemeral();
        let seq = if ephemeral { 0 } else { s.log.last_seq + 1 };
        let ev = ChatEvent {
            seq,
            at: iso_now(),
            session_id: sid.to_string(),
            turn_id: s.meta.turn.clone(),
            agent_path,
            body,
        };
        let Ok(mut v) = serde_json::to_value(&ev) else { return };
        // What a tool took and gave back, a thought, an error and the CLI's stray output are
        // text chosen by programs: no terminal control sequence survives, and they go BEFORE
        // masking, so a secret split by one is masked whole (docs/design/chat-tools.md §15).
        if matches!(
            kind,
            "tool.started" | "tool.updated" | "tool.done" | "thought.done" | "turn.done" | "notice"
        ) {
            mask::strip_value(&mut v);
        }
        s.masker.value(&mut v);
        fit(&mut v);
        if !ephemeral {
            if let Err(e) = s.log.append(&v, src) {
                crate::log!("chat {sid}: writing the log: {e}");
            }
            s.meta.last_at = ev.at.clone();
            if matches!(kind, "message.done" | "message.user") {
                s.meta.preview = preview(v["text"].as_str().unwrap_or(""), 140);
                s.meta.preview_by = preview_by(kind).into();
                if kind == "message.done" {
                    s.last_reply = claude::cut(v["text"].as_str().unwrap_or(""), crate::task::RESULT_MAX);
                }
            }
            if kind == "files.changed" {
                let root = Path::new(&s.meta.cwd);
                for f in v["files"].as_array().map(Vec::as_slice).unwrap_or(&[]) {
                    let p = f["path"].as_str().unwrap_or("");
                    if let Ok(rel) = files::relative(root, p) {
                        s.turn_files.insert(rel.to_string_lossy().into_owned());
                    }
                }
            }
        }
        let frame = ChatFrame {
            gap: false,
            ephemeral,
            req: 0,
            session_id: sid.to_string(),
            seq: if ephemeral { s.log.last_seq } else { seq },
            payload: serde_json::to_vec(&v).unwrap_or_default(),
        };
        let attached = s.attached;
        if attached {
            if let Ok(b) = frame.encode() {
                self.queue_msg(Message::Binary(b.into()));
            }
        }
        // Login health (claude_auth): a chat's words about its login count like a pane's screen,
        // and a turn that completes proves the login works.
        if self.chats.sessions[sid].meta.plugin == crate::plugin::DEFAULT_PLUGIN && self.auth.is_some() {
            let text = match kind {
                "turn.done" if v["status"] == "failed" => v["error"].as_str(),
                // What the CLI printed outside its protocol, and its last words on exit (not the
                // conversation: a reply quoting such a message is not a login problem).
                "notice" if matches!(v["code"].as_str(), Some("agent_output" | "agent_exited")) => v["text"].as_str(),
                _ => None,
            };
            let c = match text.and_then(crate::claude_auth::screen_rule) {
                Some(rule) => {
                    let state = crate::claude_auth::screen_state(rule);
                    self.auth.as_mut().and_then(|a| a.text_hit("", rule, state, "chat"))
                }
                None if kind == "turn.done" && v["status"] == "completed" => {
                    self.auth.as_mut().and_then(|a| a.turn_completed(""))
                }
                None => None,
            };
            self.auth_changed(c).await;
        }
        // The machine's needs-you set carries every chat's open items (crate::needs).
        if kind == "needs.opened" || kind == "needs.resolved" {
            self.needs_sync(crate::needs::MACHINE).await;
        }
        // Push for a needs-you item (web: `chat_needs`, same notification group as a blocked agent).
        if kind == "needs.opened" {
            let it = &v["item"];
            let data = json!({
                "sessionId": sid, "needId": it["id"], "kind": it["kind"], "risk": it["risk"],
                "title": it["title"], "tool": it["tool"], "agent": self.chats.sessions[sid].meta.plugin,
            });
            self.send_event("", "chat_needs", data).await;
        }
    }

    /// The calls still open are `cancelled`: their turn is about to end as
    /// interrupted without the agent's own word (it exited, or the machine restarted).
    async fn chat_cancel_open(&mut self, sid: &str) {
        let Some(s) = self.chats.sessions.get_mut(sid) else {
            return;
        };
        let open = if s.rpc() {
            rpc_eng!(s.meta, cancel_open())
        } else {
            s.meta.tr.cancel_open()
        };
        for e in open {
            self.chat_emit(sid, e.body, e.agent_path, 0).await;
        }
    }

    async fn chat_notice(&mut self, sid: &str, code: &str, text: String) {
        self.chat_emit(
            sid,
            ChatBody::Notice {
                code: code.into(),
                text,
                native: None,
            },
            None,
            0,
        )
        .await;
    }

    fn chat_save(&mut self, sid: &str) {
        if let Some(s) = self.chats.sessions.get_mut(sid) {
            s.save();
        }
    }

    // ── holder events ────────────────────────────────────────────────────

    fn chat_by_hid(&self, hid: &str) -> Option<String> {
        let rest = hid.strip_prefix("chat-")?;
        let (id, n) = rest.rsplit_once('-')?;
        let s = self.chats.sessions.get(id)?;
        (n.parse::<u32>().ok() == Some(s.meta.incarnation)).then(|| id.to_string())
    }

    /// Whether a holder id is a chat child's (handled here, never a pane).
    pub(crate) fn chat_owns(&self, hid: &str) -> bool {
        hid.starts_with("chat-")
    }

    pub(crate) async fn chat_attached_holder(&mut self, hid: &str, from: u64, gap: bool) {
        let Some(sid) = self.chat_by_hid(hid) else { return };
        let s = self.chats.sessions.get_mut(&sid).expect("by hid");
        s.recv_end = from;
        s.buf.clear();
        if gap {
            s.resync = true;
            self.chat_notice(
                &sid,
                "output_lost",
                "Some of the agent's output was lost while the runner was away.".into(),
            )
            .await;
        }
    }

    pub(crate) async fn chat_data(&mut self, hid: &str, offset: u64, bytes: &[u8]) {
        let Some(sid) = self.chat_by_hid(hid) else { return };
        let mut lines: Vec<(Vec<u8>, u64)> = vec![];
        let mut lost = false;
        {
            let s = self.chats.sessions.get_mut(&sid).expect("by hid");
            if offset > s.recv_end {
                // The holder's ring wrapped past what we had.
                s.buf.clear();
                s.resync = true;
                s.recv_end = offset;
                lost = true;
            }
            let skip = (s.recv_end - offset) as usize;
            if skip >= bytes.len() {
                return;
            }
            let data = &bytes[skip..];
            let max = if s.rpc() { codex::MAX_FRAME } else { MAX_LINE };
            let mut pos = s.recv_end - s.buf.len() as u64;
            s.buf.extend_from_slice(data);
            s.recv_end += data.len() as u64;
            while let Some(nl) = s.buf.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = s.buf.drain(..=nl).collect();
                pos += line.len() as u64;
                if std::mem::take(&mut s.resync) {
                    s.meta.offset = pos;
                    continue;
                }
                if line.len() > max {
                    s.meta.offset = pos;
                    lost = true;
                    continue;
                }
                lines.push((line, pos));
            }
            if s.resync {
                // Still inside a line that is being dropped: nothing of it is kept (or reported twice).
                s.buf.clear();
            }
            if s.buf.len() > max {
                s.buf.clear();
                s.resync = true;
                lost = true;
            }
        }
        if lost {
            self.chat_notice(
                &sid,
                "output_lost",
                "Some of the agent's output was too large or lost and is not shown.".into(),
            )
            .await;
        }
        for (line, end) in lines {
            self.chat_line(&sid, &line, end).await;
        }
        self.chat_save(&sid);
    }

    async fn chat_line(&mut self, sid: &str, line: &[u8], end: u64) {
        let text = String::from_utf8_lossy(line);
        let text = text.trim();
        let mut side = None;
        let (emits, src) = {
            let s = self.chats.sessions.get_mut(sid).expect("checked");
            s.meta.offset = end;
            if text.is_empty() {
                return;
            }
            let src = s.src(end);
            s.meta.tr.now_ms = crate::util::now_ms();
            let cwd = s.meta.cwd.clone();
            let exists = move |p: &str| {
                let p = Path::new(p);
                if p.is_absolute() {
                    p.exists()
                } else {
                    Path::new(&cwd).join(p).exists()
                }
            };
            let rpc = s.rpc();
            let emits = match serde_json::from_str::<Value>(text) {
                Ok(v) if v.is_object() && rpc => {
                    rpc_eng!(s.meta, now_ms = crate::util::now_ms());
                    let o = rpc_eng!(s.meta, line(&v));
                    side = Some((o.writes, o.asks, o.thread, o.fatal));
                    o.emits
                }
                Ok(v) if v.is_object() => s.meta.tr.line(&v, &exists),
                _ => {
                    // Not JSON: the CLI printed something else on stdout.
                    vec![claude::Emit {
                        body: ChatBody::Notice {
                            code: "agent_output".into(),
                            text: claude::cut(text, 2000),
                            native: None,
                        },
                        agent_path: None,
                    }]
                }
            };
            (emits, src)
        };
        // A runner that restarted handles the last lines again: what is in the log is not sent twice.
        let mut skip = self.chats.sessions[sid].log.already_logged(src);
        if let Some((writes, asks, thread, fatal)) = side {
            self.chat_rpc_side(sid, writes, asks, thread, fatal).await;
        }
        for e in emits {
            let done = matches!(e.body, ChatBody::TurnDone { .. });
            if skip > 0 {
                if !e.body.ephemeral() {
                    skip -= 1;
                }
                if done {
                    if let Some(s) = self.chats.sessions.get_mut(sid) {
                        s.meta.turn = None;
                    }
                }
                continue;
            }
            if done {
                self.chat_rpc_turn_over(sid).await;
            }
            self.chat_emit(sid, e.body, e.agent_path, src).await;
            if done {
                self.chat_turn_ended(sid).await;
            }
        }
    }

    /// What a json-rpc line asks of the host besides its events: lines for the child, decisions
    /// to open (needs-you items answered by the request's id, never a hook), the session's
    /// name, or a handshake that failed.
    async fn chat_rpc_side(
        &mut self,
        sid: &str,
        writes: Vec<Vec<u8>>,
        asks: Vec<codex::Ask>,
        thread: Option<String>,
        fatal: Option<String>,
    ) {
        let Some(s) = self.chats.sessions.get_mut(sid) else {
            return;
        };
        let hid = s.hid();
        let cwd = s.meta.cwd.clone();
        let mut bound = false;
        if let Some(t) = thread {
            s.rpc_deadline = None;
            bound = s.meta.vendor_session != t;
            s.meta.vendor_session = t;
        }
        for w in &writes {
            self.holder.input(&hid, w);
        }
        for ask in asks {
            let id = crate::util::random_id();
            let deadline = Instant::now() + HOLD.min(self.opts.prompt_hold);
            let expires =
                crate::util::iso(crate::util::now_ms() + deadline.duration_since(Instant::now()).as_millis() as u64);
            let s = self.chats.sessions.get_mut(sid).expect("checked");
            if s.needs.len() >= 64 {
                // More open questions than a person can answer: the rest are refused.
                let no = if s.meta.engine == ACP {
                    acp::refuse(&ask)
                } else {
                    codex::answer(&ask.rpc, "decline")
                };
                self.holder.input(&hid, &no);
                continue;
            }
            let item = rpc_eng!(s.meta, needs_item(&ask, &id, &expires, &cwd));
            rpc_eng!(s.meta, opened(&ask));
            s.needs.insert(
                id.clone(),
                OpenNeed {
                    item: item.clone(),
                    native: json!({ "rpc": ask.rpc }),
                    reply: None,
                    deadline,
                },
            );
            crate::log!(
                "chat {sid}: needs you ({} {}, {})",
                item.kind,
                item.tool.as_deref().unwrap_or(""),
                item.risk
            );
            self.chat_save(sid);
            self.chat_emit(sid, ChatBody::NeedsOpened { item }, None, 0).await;
        }
        if let Some(why) = fatal {
            crate::log!("chat {sid}: the agent refused the handshake; stopping it");
            self.chat_notice(sid, "agent_failed", why).await;
            self.holder.kill_nowait(&hid, libc::SIGTERM);
        }
        if bound {
            self.chat_offers_changed().await;
        }
    }

    /// Answer one approval a json-rpc agent asked for (`rpc`: its request id).
    fn chat_rpc_answer(&mut self, sid: &str, rpc: &Value, decision: &str) {
        let Some(s) = self.chats.sessions.get_mut(sid).filter(|s| s.rpc() && s.alive) else {
            return;
        };
        let hid = s.hid();
        for w in rpc_eng!(s.meta, answer_one(rpc, decision)) {
            self.holder.input(&hid, &w);
        }
    }

    /// A json-rpc agent ended its turn: what it still asked is withdrawn (it will not read an answer).
    async fn chat_rpc_turn_over(&mut self, sid: &str) {
        let Some(s) = self.chats.sessions.get_mut(sid).filter(|s| s.rpc()) else {
            return;
        };
        s.rpc_deadline = None;
        let open: Vec<String> = std::mem::take(&mut s.needs).into_keys().collect();
        for id in open {
            self.chat_emit(
                sid,
                ChatBody::NeedsResolved {
                    id,
                    outcome: "cancelled".into(),
                    by: None,
                    response: Value::Null,
                },
                None,
                0,
            )
            .await;
        }
    }

    /// After `turn.done`: the next queued message, a mode change, or idle.
    async fn chat_turn_ended(&mut self, sid: &str) {
        self.chat_flush_auto(sid).await;
        self.chat_scan_changes(sid).await;
        let (next, park) = {
            let s = self.chats.sessions.get_mut(sid).expect("checked");
            s.meta.turn = None;
            s.idle_since = Instant::now();
            s.meta.started = true;
            let park = std::mem::take(&mut s.mode_after_turn);
            (if park { None } else { s.queue.pop_front() }, park)
        };
        if park {
            self.chat_stop(sid, Stop::Park);
        } else if let Some(text) = next {
            self.chat_deliver(sid, &text).await;
        }
        self.chat_save(sid);
    }

    pub(crate) async fn chat_exit(&mut self, hid: &str, code: Option<i32>, signal: Option<String>) {
        let holder = self.holder.clone();
        let h = hid.to_string();
        tokio::spawn(async move {
            let _ = holder.remove(&h).await;
        });
        let Some(sid) = self.chat_by_hid(hid) else { return };
        let (stopping, turn_open, interrupting, needs, stderr) = {
            let s = self.chats.sessions.get_mut(&sid).expect("by hid");
            s.alive = false;
            let needs: Vec<(String, Option<ReplyTo>)> = std::mem::take(&mut s.needs)
                .into_iter()
                .map(|(k, n)| (k, n.reply))
                .collect();
            let stderr = std::fs::read(s.dir.join("stderr.log")).ok().and_then(|b| {
                let t = String::from_utf8_lossy(&b[b.len().saturating_sub(2048)..]).into_owned();
                t.lines()
                    .rev()
                    .find(|l| !l.trim().is_empty())
                    .map(|l| claude::cut(l.trim(), 300))
            });
            (
                s.stopping.take(),
                s.meta.turn.is_some(),
                s.meta.tr.interrupting || s.meta.cx.interrupting || s.meta.acp.interrupting,
                needs,
                stderr,
            )
        };
        crate::log!("chat {sid}: agent exited (code {code:?}, signal {signal:?})");
        self.chat_flush_auto(&sid).await;
        for (id, reply) in needs {
            if let Some(r) = reply {
                respond(r, Ok(json!({})), self);
            }
            self.chat_emit(
                &sid,
                ChatBody::NeedsResolved {
                    id,
                    outcome: "cancelled".into(),
                    by: None,
                    response: Value::Null,
                },
                None,
                0,
            )
            .await;
        }
        let clean = code == Some(0) && signal.is_none();
        let why = || match &stderr {
            Some(l) => format!("The agent stopped: {l}"),
            None => format!(
                "The agent stopped ({}).",
                signal
                    .clone()
                    .unwrap_or_else(|| format!("exit code {}", code.unwrap_or(-1)))
            ),
        };
        if turn_open {
            let interrupted = interrupting || stopping.is_some();
            if interrupted {
                self.chat_cancel_open(&sid).await;
            }
            self.chat_emit(
                &sid,
                ChatBody::TurnDone {
                    status: if interrupted { "interrupted" } else { "failed" }.into(),
                    error: (!interrupted).then(why),
                    usage: None,
                },
                None,
                0,
            )
            .await;
        }
        {
            let s = self.chats.sessions.get_mut(&sid).expect("by hid");
            s.meta.turn = None;
            s.meta.tr.interrupting = false;
            s.meta.tr.tools.clear();
            s.meta.tr.edits.clear();
            // The app-server went, and with it its thread handle, its requests and what it asked.
            let model = s.meta.cx.model.take();
            s.meta.cx = codex::CState {
                model,
                ..Default::default()
            };
            s.meta.acp.reset();
            s.rpc_deadline = None;
        }
        match stopping {
            Some(Stop::Close) => {
                self.chat_emit(
                    &sid,
                    ChatBody::SessionClosed {
                        reason: "closed".into(),
                    },
                    None,
                    0,
                )
                .await;
                if let Some(s) = self.chats.sessions.get_mut(&sid) {
                    s.meta.status = "closed".into();
                }
            }
            Some(Stop::Handoff) => {
                let r = self.chat_to_terminal(&sid).await;
                let reply = self.chats.sessions.get_mut(&sid).and_then(|s| s.pending.take());
                if let Some(reply) = reply {
                    respond(reply, r, self);
                }
            }
            Some(Stop::Park) => {}
            None if !clean && !interrupting => {
                let t = format!("{} Send a message to continue.", why());
                self.chat_notice(&sid, "agent_exited", t).await;
            }
            None => {}
        }
        // A queued message still goes out (the agent resumes for it).
        let next = self
            .chats
            .sessions
            .get_mut(&sid)
            .filter(|s| !matches!(s.meta.status.as_str(), "closed" | "terminal"))
            .and_then(|s| s.queue.pop_front());
        if let Some(text) = next {
            match self.chat_spawn(&sid).await {
                Ok(()) => self.chat_deliver(&sid, &text).await,
                Err(e) => self.chat_notice(&sid, &e.code, e.message).await,
            }
        }
        self.chat_save(&sid);
    }

    /// Ask the child to stop (SIGTERM to its group); what follows is decided at its exit.
    fn chat_stop(&mut self, sid: &str, why: Stop) {
        let Some(s) = self.chats.sessions.get_mut(sid) else {
            return;
        };
        if !s.alive {
            return;
        }
        s.stopping = Some(why);
        let hid = s.hid();
        self.holder.kill_nowait(&hid, libc::SIGTERM);
    }

    // ── gateway ──────────────────────────────────────────────────────────

    /// `chat_attach {req, sessionId, from}`: replay, `chat_attached`, then live.
    pub(crate) fn chat_attach(&mut self, req: u32, sid: &str, from: Option<u64>) {
        let Some(s) = self.chats.sessions.get_mut(sid) else {
            let f = RunnerFrame::ChatAttached {
                req,
                session_id: sid.into(),
                ok: false,
                last_seq: 0,
                code: Some("chat_not_found".into()),
            };
            self.queue_msg(Message::Text(f.to_json().into()));
            return;
        };
        s.attached = true;
        s.unwatched_since = None;
        s.idle_since = Instant::now();
        let from = from.unwrap_or(1).max(1);
        let gap = s.log.first_seq > 0 && from < s.log.first_seq;
        let events = s.log.read(from, usize::MAX);
        let last_seq = s.log.last_seq;
        let mut out = vec![];
        for (i, ev) in events.iter().enumerate() {
            let f = ChatFrame {
                gap: gap && i == 0,
                ephemeral: false,
                req,
                session_id: sid.into(),
                seq: ev["seq"].as_u64().unwrap_or(0),
                payload: serde_json::to_vec(ev).unwrap_or_default(),
            };
            if let Ok(b) = f.encode() {
                out.push(Message::Binary(b.into()));
            }
        }
        for m in out {
            self.queue_msg(m);
        }
        let f = RunnerFrame::ChatAttached {
            req,
            session_id: sid.into(),
            ok: true,
            last_seq,
            code: None,
        };
        self.queue_msg(Message::Text(f.to_json().into()));
    }

    pub(crate) fn chat_detach(&mut self, sid: &str) {
        if let Some(s) = self.chats.sessions.get_mut(sid) {
            s.attached = false;
            s.idle_since = Instant::now();
            s.unwatched_since = Some(Instant::now());
        }
    }

    /// The gateway connection went away: nobody watches until it re-attaches.
    pub(crate) fn chat_gateway_reset(&mut self) {
        for s in self.chats.sessions.values_mut() {
            s.attached = false;
            s.unwatched_since.get_or_insert_with(Instant::now);
        }
    }

    // ── hooks ────────────────────────────────────────────────────────────

    /// A hook from a chat child (`chatId`): a hold becomes a needs-you item;
    /// anything else is answered at once (the stream carries the rest).
    pub(crate) async fn chat_hook(&mut self, req: LocalReq) {
        let sid = req.params["chatId"].as_str().unwrap_or("").to_string();
        let native = req.params["event"].clone();
        let guard = req.params["guard"] == true;
        let reply = ReplyTo::Local(req.reply);
        log_hook_shape(&native, guard);
        if guard {
            let r = self.chat_guard(&sid, &native).await;
            respond(reply, Ok(r), self);
            return;
        }
        let Some((plugin_id, cwd)) = self.chats.sessions.get_mut(&sid).filter(|s| s.alive).map(|s| {
            s.hooks_seen += 1;
            (s.meta.plugin.clone(), s.meta.cwd.clone())
        }) else {
            respond(reply, Ok(json!({})), self);
            return;
        };
        let plugin = match self.chat_plugin(&plugin_id) {
            Ok(p) => p,
            Err(_) => {
                respond(reply, Ok(json!({})), self);
                return;
            }
        };
        let holds = plugin
            .manifest()
            .chat
            .as_ref()
            .map(|c| c.holds.clone())
            .unwrap_or_default();
        let event = native["hook_event_name"].as_str().unwrap_or("");
        let tool = native["tool_name"].as_str().unwrap_or("");
        let held = holds
            .iter()
            .any(|h| h.event == event && (h.tool.is_empty() || h.tool == tool));
        let id = crate::util::random_id();
        let deadline = Instant::now() + HOLD.min(self.opts.prompt_hold);
        let expires =
            crate::util::iso(crate::util::now_ms() + deadline.duration_since(Instant::now()).as_millis() as u64);
        let item = if held {
            claude::needs_item(&native, &id, &expires, &cwd)
        } else {
            None
        };
        let Some(item) = item else {
            respond(reply, Ok(json!({})), self);
            return;
        };
        {
            let s = self.chats.sessions.get_mut(&sid).expect("checked");
            s.needs.insert(
                id.clone(),
                OpenNeed {
                    item: item.clone(),
                    native,
                    reply: Some(reply),
                    deadline,
                },
            );
        }
        crate::log!(
            "chat {sid}: needs you ({} {}, {})",
            item.kind,
            item.tool.as_deref().unwrap_or(""),
            item.risk
        );
        // On disk before anyone hears of it: a runner that dies now closes it at its restart.
        self.chat_save(&sid);
        self.chat_emit(&sid, ChatBody::NeedsOpened { item }, None, 0).await;
    }

    /// Auto-approve's guard (design §11 "Bypass permissions"): the CLI runs in
    /// its bypass mode and asks this, through `hook --guard`, before every
    /// tool. The high-risk classes (git push, package installs, `sudo`,
    /// deletes and edits outside the project, network use, credential reads)
    /// are the **never** list: blocked here whatever the mode, with the
    /// reason told to the agent and shown in the chat. Everything else goes
    /// on; it is logged in the chat (its tool rows) and audited as a
    /// `chat_auto` machine event (one per turn for what was approved, one per
    /// block). The hook fails closed when this cannot answer.
    async fn chat_guard(&mut self, sid: &str, native: &Value) -> Value {
        let deny = |why: String| {
            json!({ "output": { "hookSpecificOutput": {
                "hookEventName": "PreToolUse", "permissionDecision": "deny", "permissionDecisionReason": why,
            }}})
        };
        let tool = native["tool_name"].as_str().unwrap_or("").to_string();
        let input = native.get("tool_input").cloned().unwrap_or(Value::Null);
        let Some(s) = self.chats.sessions.get_mut(sid).filter(|s| s.alive) else {
            return deny("tmuxedo blocked this: the chat it belongs to is not running.".into());
        };
        s.hooks_seen += 1;
        // A question is held by its own hook: a person answers it.
        if tool == "AskUserQuestion" {
            return json!({ "allow": true });
        }
        let (cwd, plugin) = (s.meta.cwd.clone(), s.meta.plugin.clone());
        if s.meta.mode != AUTO_APPROVE || !self.chats.auto_approve_allowed {
            // Switched off mid-turn: the CLI is still in bypass until the turn ends; nothing goes unasked.
            s.meta.tr.deny(native);
            return deny(
                "tmuxedo blocked this: Auto-approve was turned off for this chat. Tell the user what you wanted to do; \
                 the next message runs in Ask first."
                    .into(),
            );
        }
        let (_, title) = claude::tool_kind_title(&tool, &input);
        let title = s.masker.text(&claude::cut(&title, 200));
        match claude::risk(&tool, &input, &cwd) {
            Some(why) => {
                crate::log!("chat {sid}: Auto-approve blocked {tool} ({why})");
                let text = format!(
                    "Auto-approve never allows this ({}): {title} was blocked. Turn Auto-approve off to approve it yourself.",
                    why.to_lowercase()
                );
                s.meta.tr.deny(native);
                self.chat_notice(sid, "auto_blocked", text).await;
                let data = json!({
                    "sessionId": sid, "action": "blocked", "tool": tool, "title": title, "why": why, "agent": plugin,
                });
                self.send_event("", "chat_auto", data).await;
                self.chat_save(sid);
                deny(format!(
                    "tmuxedo blocked this: Auto-approve never allows actions that are high risk ({}). \
                     Ask the user to turn Auto-approve off and approve it themselves.",
                    why.to_lowercase()
                ))
            }
            None => {
                let s = self.chats.sessions.get_mut(sid).expect("checked");
                s.auto_approved.push(format!("{tool}: {title}"));
                json!({ "allow": true })
            }
        }
    }

    /// The audit row for what Auto-approve let through in a turn (one machine event).
    async fn chat_flush_auto(&mut self, sid: &str) {
        let Some(s) = self.chats.sessions.get_mut(sid) else {
            return;
        };
        if s.auto_approved.is_empty() {
            return;
        }
        let list = std::mem::take(&mut s.auto_approved);
        let n = list.len();
        let data = json!({
            "sessionId": sid, "action": "approved", "count": n,
            "tools": list.into_iter().take(50).collect::<Vec<_>>(), "agent": s.meta.plugin,
        });
        self.send_event("", "chat_auto", data).await;
    }

    /// The turn-end scan (design §7.1's "watch", done cheaply): files git sees
    /// as new or changed since the turn started that no tool event named (a
    /// shell command's output, a build report) become `files.changed` with
    /// `source: watch`. Git's own ignore rules apply; a folder that is not a
    /// repository is not scanned.
    async fn chat_scan_changes(&mut self, sid: &str) {
        let Some(s) = self.chats.sessions.get_mut(sid) else {
            return;
        };
        let Some(before) = s.scan_base.take() else { return };
        let said = std::mem::take(&mut s.turn_files);
        let cwd = s.meta.cwd.clone();
        let Some(after) = files::git_snapshot(Path::new(&cwd)).await else {
            return;
        };
        let changes = files::snapshot_changes(&before, &after, &said);
        if changes.is_empty() {
            return;
        }
        let root = Path::new(&cwd);
        let list = changes
            .into_iter()
            .take(200)
            .map(|(rel, change)| tmuxedo_protocol::chat::FileChange {
                path: root.join(rel).to_string_lossy().into_owned(),
                change: change.into(),
                source: "watch".into(),
                tool_id: None,
            })
            .collect();
        self.chat_emit(sid, ChatBody::FilesChanged { files: list }, None, 0)
            .await;
    }

    /// `chat.file {session_id, path}`: one file of the chat's work folder for
    /// the Files panel (Preview · Raw · Changes), confined, secrets refused,
    /// masked (see [`files`]). `changes`: `{kind: new}` for a file this chat
    /// created, `{kind: tool, diffs}` from its edit tools, `{kind: git,
    /// patch}` for another change to a tracked file, else `{kind: none}`.
    async fn chat_file(&mut self, p: &Value) -> ApiResult {
        let path = str_param(p, "path")
            .ok_or_else(|| err("invalid_params", "path is required"))?
            .to_string();
        let (cwd, history) = {
            let s = self.chat_mut(p)?;
            (s.meta.cwd.clone(), s.log.read(1, usize::MAX))
        };
        let root = PathBuf::from(&cwd);
        let refused = |r: files::Refusal| {
            let (code, msg) = r.code();
            err(code, msg)
        };
        let rel = files::relative(&root, &path).map_err(refused)?;
        let rel_s = rel.to_string_lossy().into_owned();
        // The jail walks from the folder's open handle and follows no link (docs/design/file-browser.md §3.4).
        let opened = crate::jail::Root::open_trusted(&root)
            .and_then(|r| r.open_file(&rel_s, Default::default()))
            .map_err(|r| refused(r.into()))?;
        let (f, real) = (opened.file, opened.real);
        let name = rel
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut c = files::read_content(f, &name).map_err(|e| err("file_unreadable", e.to_string()))?;
        // What this chat did to it, from its own (masked) log.
        let same = |v: &Value| files::relative(&root, v.as_str().unwrap_or("")).is_ok_and(|r| r == rel);
        let created = history.iter().any(|e| {
            e["type"] == "files.changed"
                && e["files"]
                    .as_array()
                    .is_some_and(|l| l.iter().any(|f| f["change"] == "created" && same(&f["path"])))
        });
        let diffs: Vec<Value> = history
            .iter()
            .filter(|e| e["type"] == "tool.updated" && same(&e["diff"]["path"]))
            .map(|e| json!({ "oldText": e["diff"]["oldText"], "newText": e["diff"]["newText"] }))
            .collect();
        let patch = if created || !diffs.is_empty() {
            None
        } else {
            files::git_patch(&root, &rel).await
        };
        let s = self.chat_mut(p)?;
        let changes = if created {
            json!({ "kind": "new" })
        } else if !diffs.is_empty() {
            json!({ "kind": "tool", "diffs": diffs })
        } else if let Some(patch) = patch {
            json!({ "kind": "git", "patch": s.masker.text(&patch) })
        } else {
            json!({ "kind": "none" })
        };
        if let Some(t) = c.text.as_mut() {
            *t = s.masker.text(t);
        }
        crate::log!("chat file read: {} bytes", c.size);
        Ok(files::answer(&rel_s, &real, c, changes))
    }

    // ── timers ───────────────────────────────────────────────────────────

    /// Expire held decisions; park idle chats nobody watches; prune history hourly.
    pub(crate) async fn chat_tick(&mut self) {
        let now = Instant::now();
        let mut expired = vec![];
        let mut park = vec![];
        for (sid, s) in &self.chats.sessions {
            for (id, n) in &s.needs {
                if n.deadline <= now {
                    expired.push((sid.clone(), id.clone()));
                }
            }
            if s.alive
                && s.stopping.is_none()
                && s.meta.turn.is_none()
                && s.needs.is_empty()
                && s.queue.is_empty()
                && !s.attached
                && now.duration_since(s.idle_since) >= self.chats.park_after
            {
                park.push(sid.clone());
            }
        }
        for (sid, id) in expired {
            let n = self.chats.sessions.get_mut(&sid).and_then(|s| s.needs.remove(&id));
            if let Some(n) = &n {
                // Nobody answered: a json-rpc agent is told no (its turn goes on).
                self.chat_rpc_answer(&sid, &n.native["rpc"], "decline");
            }
            if let Some(reply) = n.and_then(|n| n.reply) {
                // Nothing printed: the CLI's own handling decides (structured mode: refused).
                respond(reply, Ok(json!({ "expired": true })), self);
            }
            self.chat_emit(
                &sid,
                ChatBody::NeedsResolved {
                    id,
                    outcome: "expired".into(),
                    by: None,
                    response: Value::Null,
                },
                None,
                0,
            )
            .await;
            self.chat_save(&sid);
        }
        let silent: Vec<(String, &'static str)> = self
            .chats
            .sessions
            .iter()
            .filter_map(|(sid, s)| {
                s.rpc_deadline
                    .filter(|(d, _)| *d <= now && s.alive)
                    .map(|(_, what)| (sid.clone(), what))
            })
            .collect();
        for (sid, what) in silent {
            crate::log!("chat {sid}: the agent did not answer ({what}); stopping it");
            let hid = {
                let s = self.chats.sessions.get_mut(&sid).expect("listed");
                s.rpc_deadline = None;
                s.hid()
            };
            let text = if what == "start" {
                "The agent did not start its conversation in time and was stopped."
            } else {
                "The agent did not stop when asked and was stopped."
            };
            self.chat_notice(&sid, "agent_silent", text.into()).await;
            self.holder.kill_nowait(&hid, libc::SIGKILL);
        }
        for sid in park {
            crate::log!("chat {sid}: idle; parking (it resumes on the next message)");
            self.chat_stop(&sid, Stop::Park);
        }
        // D-C4 "guarded (b)": Auto-approve on a subscription login with nobody watching goes back
        // to "Ask first" (at the end of a running turn) for owners web does not exempt.
        let unattended: Vec<String> = self
            .chats
            .sessions
            .values()
            .filter(|s| s.meta.mode == AUTO_APPROVE && s.meta.status != "closed" && !s.mode_after_turn)
            .map(|s| s.meta.id.clone())
            .filter(|sid| self.unattended_blocked(sid))
            .collect();
        for sid in unattended {
            crate::log!("chat {sid}: nobody watches a subscription chat in Auto-approve; back to Ask first");
            self.chat_set_mode(&sid, "ask_first").await;
            self.chat_notice(
                &sid,
                "unattended_needs_api_key",
                format!("{UNATTENDED_WHY} Auto-approve is off again; it asks before it acts. Turn it back on while you watch."),
            )
            .await;
            self.chat_save(&sid);
        }
        // A chat whose pane is gone (closed, its shell exited, a restart without it): nothing drives it now.
        let orphaned: Vec<String> = self
            .chats
            .sessions
            .values()
            .filter(|s| s.meta.status == "terminal")
            .filter(|s| s.meta.pane_id.as_deref().is_none_or(|p| self.state.pane(p).is_none()))
            .map(|s| s.meta.id.clone())
            .collect();
        for sid in orphaned {
            self.chat_release(&sid, "terminal_closed", "The terminal it continued in was closed.")
                .await;
        }
        if self
            .chats
            .last_prune
            .is_none_or(|t| now.duration_since(t) >= Duration::from_secs(3600))
        {
            self.chats.last_prune = Some(now);
            self.chat_prune(crate::util::now_ms());
        }
    }

    /// Drop chat history older than 30 days; remove sessions left with none.
    pub(crate) fn chat_prune(&mut self, now_ms: u64) {
        let ids: Vec<String> = self.chats.sessions.keys().cloned().collect();
        for sid in ids {
            let s = self.chats.sessions.get_mut(&sid).expect("listed");
            if s.alive || !s.needs.is_empty() {
                continue;
            }
            match s.log.prune(now_ms) {
                Ok((dropped, 0)) => {
                    let _ = dropped;
                    let dir = s.dir.clone();
                    self.chats.sessions.remove(&sid);
                    let _ = std::fs::remove_dir_all(&dir);
                    crate::log!("chat {sid}: history older than 30 days removed");
                }
                Ok((dropped, _)) if dropped > 0 => {
                    crate::log!("chat {sid}: {dropped} events older than 30 days removed")
                }
                Ok(_) => {}
                Err(e) => crate::log!("chat {sid}: pruning: {e}"),
            }
        }
    }

    /// At startup: reattach to chat children the holder kept; close the
    /// rest (parked), and needs-you items whose hooks died with the old runner.
    pub(crate) async fn chat_adopt(&mut self, held: &std::collections::HashMap<String, Value>) {
        self.chat_prune(crate::util::now_ms());
        let ids: Vec<String> = self.chats.sessions.keys().cloned().collect();
        for sid in ids {
            let (hid, offset, open, status) = {
                let s = &self.chats.sessions[&sid];
                (s.hid(), s.meta.offset, s.meta.open_needs.clone(), s.meta.status.clone())
            };
            for id in open {
                self.chat_emit(
                    &sid,
                    ChatBody::NeedsResolved {
                        id,
                        outcome: "expired".into(),
                        by: Some("runner restart".into()),
                        response: Value::Null,
                    },
                    None,
                    0,
                )
                .await;
            }
            match held.get(&hid) {
                Some(h) if h["exited"].is_null() && !matches!(status.as_str(), "closed" | "terminal") => {
                    crate::log!("chat {sid}: reattaching {hid} from {offset}");
                    let s = self.chats.sessions.get_mut(&sid).expect("listed");
                    s.alive = true;
                    s.pid = h["pid"].as_u64().unwrap_or(0) as u32;
                    // Masking literals come back from the same environment.
                    s.masker = Masker::from_env(
                        std::env::vars()
                            .collect::<Vec<_>>()
                            .iter()
                            .map(|(k, v)| (k.as_str(), v.as_str())),
                    );
                    for (k, v) in &self.launch.shell.env {
                        if mask::is_secret_name(k) {
                            s.masker.add(v);
                        }
                    }
                    s.masker.machine = self.secrets.shared();
                    s.recv_end = offset;
                    // What a json-rpc agent asked the old runner is answered no (its item was just closed as expired).
                    let declined = if s.rpc() {
                        rpc_eng!(s.meta, answer_asked("decline"))
                    } else {
                        vec![]
                    };
                    let _ = self.holder.attach(&hid, offset).await;
                    for w in &declined {
                        self.holder.input(&hid, w);
                    }
                }
                Some(h) => {
                    let code = h["exited"]["code"].as_i64().map(|c| c as i32);
                    let signal = h["exited"]["signal"].as_str().map(str::to_owned);
                    self.chats.sessions.get_mut(&sid).expect("listed").alive = true;
                    self.chat_exit(&hid, code, signal).await;
                }
                None => {
                    let turn_open = self.chats.sessions[&sid].meta.turn.is_some();
                    if turn_open {
                        self.chat_cancel_open(&sid).await;
                        self.chat_emit(
                            &sid,
                            ChatBody::TurnDone {
                                status: "interrupted".into(),
                                error: Some("The machine restarted.".into()),
                                usage: None,
                            },
                            None,
                            0,
                        )
                        .await;
                        self.chats.sessions.get_mut(&sid).expect("listed").meta.turn = None;
                    }
                }
            }
            self.chat_save(&sid);
        }
    }

    // ── methods ──────────────────────────────────────────────────────────

    pub(crate) async fn chat_request(&mut self, method: &str, p: &Value, reply: ReplyTo) {
        let r = match method {
            "chat.start" => self.chat_start(p).await,
            "chat.list" => Ok(self.chat_list()),
            "chat.get" => self.chat_mut(p).map(|s| {
                let needs: Vec<&NeedsItem> = s.needs.values().map(|n| &n.item).collect();
                json!({ "type": "chat_info", "session": s.info(), "needs": needs })
            }),
            "chat.read" => self.chat_read(p),
            "chat.send" => self.chat_send(p).await,
            "chat.interrupt" => self.chat_interrupt(p).await,
            "chat.mode" => self.chat_mode(p).await,
            "chat.respond" => self.chat_respond(p).await,
            "chat.handoff" => match self.chat_handoff(p, reply).await {
                Some((reply, r)) => return respond(reply, r, self),
                None => return, // answered when the agent has stopped
            },
            "chat.close" => self.chat_close(p).await,
            "chat.file" => self.chat_file(p).await,
            m => Err(err("unknown_method", format!("no method {m}"))),
        };
        respond(reply, r, self);
    }

    fn chat_list(&self) -> Value {
        let mut list: Vec<&Session> = self.chats.sessions.values().collect();
        list.sort_by(|a, b| b.meta.last_at.cmp(&a.meta.last_at));
        json!({ "type": "chat_list", "sessions": list.iter().map(|s| s.info()).collect::<Vec<_>>() })
    }

    fn chat_read(&mut self, p: &Value) -> ApiResult {
        let from = p.get("from_seq").and_then(Value::as_u64).unwrap_or(1).max(1);
        let limit = p
            .get("limit")
            .and_then(Value::as_u64)
            .map_or(200, |n| n.clamp(1, READ_MAX as u64)) as usize;
        let s = self.chat_mut(p)?;
        let events = s.log.read(from, limit);
        Ok(json!({
            "type": "chat_events", "events": events, "firstSeq": s.log.first_seq, "lastSeq": s.log.last_seq,
            "gap": s.log.first_seq > 0 && from < s.log.first_seq,
        }))
    }

    pub(crate) async fn chat_start(&mut self, p: &Value) -> ApiResult {
        let plugin_id = str_param(p, "agent")
            .unwrap_or(crate::plugin::DEFAULT_PLUGIN)
            .to_string();
        let plugin = self.chat_plugin(&plugin_id)?;
        let c = plugin.manifest().chat.clone().expect("checked");
        let mode = str_param(p, "mode").unwrap_or("ask_first").to_string();
        if c.native_mode(&mode).is_none() {
            return Err(err(
                "invalid_mode",
                format!("{mode} is not a mode of {}", plugin.manifest().name),
            ));
        }
        if mode == AUTO_APPROVE {
            self.auto_approve_gate(p)?;
        }
        let cwd = match str_param(p, "cwd") {
            // An existing folder (`~`, `~/…` or absolute): a chat never creates one.
            Some(c) => {
                let c = c.trim();
                let path = if c == "~" {
                    PathBuf::from(&self.opts.home)
                } else if let Some(rest) = c.strip_prefix("~/") {
                    Path::new(&self.opts.home).join(rest)
                } else {
                    PathBuf::from(c)
                };
                if !path.is_absolute() {
                    return Err(err("invalid_cwd", format!("{c} is not ~, ~/… or an absolute path")));
                }
                path.canonicalize()
                    .ok()
                    .filter(|p| p.is_dir())
                    .ok_or_else(|| err("invalid_cwd", format!("{c} is not a directory")))?
                    .to_string_lossy()
                    .into_owned()
            }
            None => self.work_dir(),
        };
        if !Path::new(&cwd).is_dir() {
            return Err(err("invalid_cwd", format!("{cwd} is not a directory")));
        }
        let text = str_param(p, "text").map(str::to_owned);
        if let Some(t) = &text {
            check_text(t)?;
        }
        // Refuse before anything is created: the login kind and the binary.
        let sid = new_session_id();
        let env = self.chat_env(&sid, plugin.as_ref());
        let login = self.chat_login(plugin.as_ref(), &env);
        self.chat_login_allowed(plugin.as_ref(), &login)?;
        let dir = self.chats.session_dir(&sid);
        std::fs::create_dir_all(&dir).map_err(|e| err("internal", e.to_string()))?;
        let _ = std::fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o700));
        let now = iso_now();
        let title = str_param(p, "title")
            .map(|t| preview(t, 80))
            .or_else(|| text.as_deref().map(|t| preview(t, 60)))
            .unwrap_or_default();
        let meta = Meta {
            id: sid.clone(),
            plugin: plugin_id,
            cwd,
            mode,
            login,
            title,
            created_at: now.clone(),
            last_at: now,
            // A json-rpc agent names its own session (the thread): known once it has started.
            vendor_session: if c.transport == JSON_RPC || c.transport == ACP {
                String::new()
            } else {
                uuid_v4()
            },
            status: "parked".into(),
            ..Meta::default()
        };
        let log = EventLog::open(&dir);
        let mut s = Session::new(meta, log, dir);
        s.save();
        self.chats.sessions.insert(sid.clone(), s);
        // The title is masked like every event string.
        if let Some(s) = self.chats.sessions.get_mut(&sid) {
            s.masker = Masker::from_env(env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
            s.masker.machine = self.secrets.shared();
            s.meta.title = s.masker.text(&s.meta.title);
        }
        if let Err(e) = self.chat_spawn(&sid).await {
            // Nothing was started: no session left behind.
            if let Some(s) = self.chats.sessions.remove(&sid) {
                let _ = std::fs::remove_dir_all(&s.dir);
            }
            return Err(e);
        }
        let mut sent = Value::Null;
        if let Some(t) = text {
            sent = self
                .chat_send_text(&sid, &t, str_param(p, "client_msg_id"), str_param(p, "by"))
                .await?;
        }
        self.chat_save(&sid);
        let s = &self.chats.sessions[&sid];
        Ok(json!({ "type": "chat_info", "session": s.info(), "sent": sent }))
    }

    async fn chat_send(&mut self, p: &Value) -> ApiResult {
        let text = str_param(p, "text")
            .ok_or_else(|| err("invalid_params", "text is required"))?
            .to_string();
        check_text(&text)?;
        let sid = self.chat_mut(p)?.meta.id.clone();
        let r = self
            .chat_send_text(&sid, &text, str_param(p, "client_msg_id"), str_param(p, "by"))
            .await;
        self.chat_save(&sid);
        r
    }

    /// A queued prompt (crate::queued): sent as the owner's message, deduplicated by
    /// `client_msg_id`, and saved at once.
    pub(crate) async fn chat_send_queued(&mut self, sid: &str, text: &str, client_msg_id: &str) -> ApiResult {
        check_text(text)?;
        let r = self.chat_send_text(sid, text, Some(client_msg_id), None).await;
        self.chat_save(sid);
        r
    }

    async fn chat_send_text(
        &mut self,
        sid: &str,
        text: &str,
        client_msg_id: Option<&str>,
        by: Option<&str>,
    ) -> ApiResult {
        let client_msg_id = client_msg_id.map(|c| claude::cut(c, 64));
        {
            let s = self
                .chats
                .sessions
                .get(sid)
                .ok_or_else(|| err("chat_not_found", "no such chat"))?;
            match s.meta.status.as_str() {
                "closed" => return Err(err("chat_closed", "this chat is closed")),
                "terminal" => {
                    return Err(err(
                        "chat_in_terminal",
                        "this conversation continues in a terminal; use \"Back to chat\" (chat.handoff to chat) first",
                    ))
                }
                _ => {}
            }
            if let Some(c) = &client_msg_id {
                if let Some((_, seq)) = s.recent.iter().find(|(id, _)| id == c) {
                    return Ok(json!({ "type": "chat_sent", "seq": seq, "duplicate": true }));
                }
            }
        }
        if !self.chats.sessions[sid].alive {
            self.chat_spawn(sid).await?;
        }
        let queued = {
            let s = &self.chats.sessions[sid];
            // A CLI that is stopping (a mode change restarts it) would lose the line: it goes
            // out from the queue once the agent has exited and started again.
            s.meta.turn.is_some() || !s.needs.is_empty() || !s.queue.is_empty() || s.stopping.is_some()
        };
        let message_id = format!("u{}", &crate::util::random_id()[..12]);
        if !queued {
            let s = self.chats.sessions.get_mut(sid).expect("checked");
            s.meta.turns += 1;
            s.meta.turn = Some(format!("t{}", s.meta.turns));
        }
        self.chat_emit(
            sid,
            ChatBody::MessageUser {
                message_id,
                text: text.to_string(),
                attachments: vec![],
                by: by.map(|b| claude::cut(b, 120)),
                client_msg_id: client_msg_id.clone(),
                queued,
            },
            None,
            0,
        )
        .await;
        let seq = self.chats.sessions[sid].log.last_seq;
        {
            let s = self.chats.sessions.get_mut(sid).expect("checked");
            if let Some(c) = client_msg_id {
                s.recent.push_back((c, seq));
                if s.recent.len() > RECENT_IDS {
                    s.recent.pop_front();
                }
            }
            if s.meta.title.is_empty() {
                s.meta.title = s.masker.text(&preview(text, 60));
            }
            if queued {
                s.queue.push_back(text.to_string());
            }
        }
        if !queued {
            self.chat_write_turn(sid, text).await;
        }
        Ok(json!({ "type": "chat_sent", "seq": seq, "queued": queued }))
    }

    /// A queued message's turn starts (its message.user was already sent).
    async fn chat_deliver(&mut self, sid: &str, text: &str) {
        {
            let Some(s) = self.chats.sessions.get_mut(sid) else {
                return;
            };
            s.meta.turns += 1;
            s.meta.turn = Some(format!("t{}", s.meta.turns));
        }
        self.chat_write_turn(sid, text).await;
    }

    async fn chat_write_turn(&mut self, sid: &str, text: &str) {
        // Before the agent sees the message: what the work folder looks like (the turn-end scan).
        let cwd = self.chats.sessions[sid].meta.cwd.clone();
        let base = files::git_snapshot(Path::new(&cwd)).await;
        let (hid, line) = {
            let s = self.chats.sessions.get_mut(sid).expect("checked");
            s.idle_since = Instant::now();
            s.scan_base = base;
            s.turn_files.clear();
            let lines = if s.rpc() {
                rpc_eng!(s.meta, send(text))
            } else {
                vec![claude::user_line(text, s.meta.tr.vendor_session.as_deref())]
            };
            (s.hid(), lines)
        };
        for l in &line {
            self.holder.input(&hid, l);
        }
        self.chat_emit(sid, ChatBody::TurnStarted {}, None, 0).await;
    }

    async fn chat_interrupt(&mut self, p: &Value) -> ApiResult {
        let s = self.chat_mut(p)?;
        let sid = s.meta.id.clone();
        if s.meta.turn.is_none() || !s.alive {
            return Ok(json!({ "type": "chat_interrupt", "interrupted": false }));
        }
        s.meta.tr.interrupting = true;
        let hid = s.hid();
        // A json-rpc agent is asked to stop its turn (what it asked is withdrawn first); it stays up.
        let asked = s.rpc().then(|| {
            let mut l = rpc_eng!(s.meta, answer_asked("cancel"));
            l.extend(rpc_eng!(s.meta, interrupt()));
            l
        });
        if asked.is_some() {
            s.rpc_deadline = Some((Instant::now() + RPC_INTERRUPT, "stop"));
        }
        let needs: Vec<(String, Option<ReplyTo>)> = std::mem::take(&mut s.needs)
            .into_iter()
            .map(|(k, n)| (k, n.reply))
            .collect();
        // SIGINT ends the turn (design §2.1.1); the CLI may also exit, and the
        // next message resumes the session.
        match asked {
            Some(lines) => lines.iter().for_each(|l| self.holder.input(&hid, l)),
            None => self.holder.kill_nowait(&hid, libc::SIGINT),
        }
        for (id, reply) in needs {
            if let Some(r) = reply {
                respond(r, Ok(json!({})), self);
            }
            self.chat_emit(
                &sid,
                ChatBody::NeedsResolved {
                    id,
                    outcome: "cancelled".into(),
                    by: None,
                    response: Value::Null,
                },
                None,
                0,
            )
            .await;
        }
        self.chat_save(&sid);
        Ok(json!({ "type": "chat_interrupt", "interrupted": true }))
    }

    async fn chat_mode(&mut self, p: &Value) -> ApiResult {
        let mode = str_param(p, "mode")
            .ok_or_else(|| err("invalid_params", "mode is required"))?
            .to_string();
        let (sid, plugin) = {
            let s = self.chat_mut(p)?;
            (s.meta.id.clone(), s.meta.plugin.clone())
        };
        let plugin = self.chat_plugin(&plugin)?;
        if plugin
            .manifest()
            .chat
            .as_ref()
            .and_then(|c| c.native_mode(&mode))
            .is_none()
        {
            return Err(err(
                "invalid_mode",
                format!("{mode} is not a mode of {}", plugin.manifest().name),
            ));
        }
        if mode == AUTO_APPROVE {
            self.auto_approve_gate(p)?;
            if let Some(e) = self.unattended_refusal(&sid) {
                return Err(e);
            }
        }
        let busy = self.chat_set_mode(&sid, &mode).await;
        self.chat_save(&sid);
        Ok(json!({ "type": "chat_mode", "mode": mode, "applies": if busy { "after_turn" } else { "next_message" } }))
    }

    /// D-C4 "guarded (b)": a subscription login may not run unattended unless web
    /// allows it for the owner (admins). Auto-approve counts as unattended when
    /// nobody watches the chat.
    fn unattended_blocked(&self, sid: &str) -> bool {
        let Some(s) = self.chats.sessions.get(sid) else {
            return false;
        };
        !self.chats.unattended_allowed
            && s.meta.login == "subscription"
            && !s.attached
            && s.unwatched_since
                .is_none_or(|t| t.elapsed() >= self.chats.unwatched_grace)
    }

    fn unattended_refusal(&self, sid: &str) -> Option<ApiErr> {
        self.unattended_blocked(sid).then(|| {
            err(
                "unattended_needs_api_key",
                format!("{UNATTENDED_WHY} Open the chat to turn it on while you watch, or add an API key."),
            )
        })
    }

    /// Auto-approve needs web's policy to allow it for the owner and a fresh
    /// step-up (`fresh_stepup`, set by web only: never a token, never a socket client).
    fn auto_approve_gate(&self, p: &Value) -> Result<(), ApiErr> {
        if !self.chats.auto_approve_allowed {
            return Err(err(
                "auto_approve_forbidden",
                "Auto-approve is turned off for this account by an admin.",
            ));
        }
        if p.get("fresh_stepup").and_then(Value::as_bool) != Some(true) {
            return Err(err(
                "stepup_required",
                "Auto-approve needs a fresh confirmation (within 5 minutes); confirm it's you and try again",
            ));
        }
        Ok(())
    }

    /// Change a chat's mode: `session.mode`, then the CLI restarts with it at
    /// the next turn boundary (it takes its mode at start). Returns whether a
    /// turn is running (the change applies after it).
    async fn chat_set_mode(&mut self, sid: &str, mode: &str) -> bool {
        let (changed, busy) = {
            let Some(s) = self.chats.sessions.get_mut(sid) else {
                return false;
            };
            let changed = s.meta.mode != mode;
            s.meta.mode = mode.to_string();
            (changed, s.meta.turn.is_some())
        };
        if changed {
            let model = self.chats.sessions[sid].meta.tr.model.clone();
            self.chat_emit(
                sid,
                ChatBody::SessionMode {
                    mode: mode.to_string(),
                    model,
                },
                None,
                0,
            )
            .await;
            if busy {
                self.chats.sessions.get_mut(sid).expect("checked").mode_after_turn = true;
            } else {
                self.chat_stop(sid, Stop::Park);
            }
            crate::log!("chat {sid}: mode {mode}");
        }
        busy
    }

    pub(crate) async fn chat_respond(&mut self, p: &Value) -> ApiResult {
        let need = str_param(p, "need_id")
            .ok_or_else(|| err("invalid_params", "need_id is required"))?
            .to_string();
        let response: claude::Response = serde_json::from_value(p.get("response").cloned().unwrap_or(Value::Null))
            .map_err(|_| err("invalid_params", "response must be {kind, message?, answers?}"))?;
        let by = str_param(p, "by").map(|b| claude::cut(b, 120));
        let fresh = p.get("fresh_stepup").and_then(Value::as_bool).unwrap_or(false);
        let sid = self.chat_mut(p)?.meta.id.clone();
        let s = self.chats.sessions.get_mut(&sid).expect("checked");
        let rpc = s.rpc();
        let Some(open) = s.needs.get(&need) else {
            return Ok(json!({ "type": "chat_respond", "outcome": "stale" }));
        };
        let out = if rpc {
            json!(codex::decision(&open.item, &response).map_err(|(c, m)| err(c, m))?)
        } else {
            claude::reply(&open.item, &open.native, &response).map_err(|(c, m)| err(c, m))?
        };
        if open.item.risk == "high" && !fresh && response.kind != "deny" {
            return Err(err(
                "stepup_required",
                "a high-risk action needs a fresh confirmation (within 5 minutes); confirm it's you and answer again",
            ));
        }
        let open = s.needs.remove(&need).expect("checked");
        if response.kind == "deny" {
            // The call this was about did not run: its result is `denied`, not `failed`.
            s.meta.tr.deny(&open.native);
        }
        if rpc {
            // The answer goes back on the channel the question came on, by its request id.
            self.chat_rpc_answer(&sid, &open.native["rpc"], out.as_str().unwrap_or("decline"));
        }
        if let Some(r) = open.reply {
            respond(r, Ok(json!({ "output": out })), self);
        }
        crate::log!("chat {sid}: answered {need} ({})", response.kind);
        // What was answered, without free text beyond a short form (masked anyway).
        let mut shown = json!({ "kind": response.kind });
        if let Some(m) = &response.message {
            shown["message"] = json!(claude::cut(m, 2000));
        }
        if let Some(a) = &response.answers {
            shown["answers"] = Value::Object(a.clone());
        }
        self.chat_emit(
            &sid,
            ChatBody::NeedsResolved {
                id: need,
                outcome: "answered".into(),
                by,
                response: shown,
            },
            None,
            0,
        )
        .await;
        self.chat_save(&sid);
        Ok(json!({ "type": "chat_respond", "outcome": "accepted" }))
    }

    pub(crate) async fn chat_close(&mut self, p: &Value) -> ApiResult {
        let s = self.chat_mut(p)?;
        let sid = s.meta.id.clone();
        if s.meta.status == "closed" {
            return Ok(json!({ "type": "ok" }));
        }
        s.queue.clear();
        if s.alive {
            self.chat_stop(&sid, Stop::Close);
        } else {
            self.chat_emit(
                &sid,
                ChatBody::SessionClosed {
                    reason: "closed".into(),
                },
                None,
                0,
            )
            .await;
            if let Some(s) = self.chats.sessions.get_mut(&sid) {
                s.meta.status = "closed".into();
            }
        }
        self.chat_save(&sid);
        self.chat_offers_changed().await;
        Ok(json!({ "type": "ok" }))
    }

    /// `chat.handoff {to: terminal | chat}` (design §5.1). Returns the reply
    /// to send now, or None when it is sent once the agent has stopped.
    async fn chat_handoff(&mut self, p: &Value, reply: ReplyTo) -> Option<(ReplyTo, ApiResult)> {
        match str_param(p, "to") {
            Some("terminal") => {
                let s = match self.chat_mut(p) {
                    Ok(s) => s,
                    Err(e) => return Some((reply, Err(e))),
                };
                let sid = s.meta.id.clone();
                if matches!(s.meta.status.as_str(), "closed" | "terminal") {
                    return Some((
                        reply,
                        Err(err("chat_not_active", format!("this chat is {}", s.meta.status))),
                    ));
                }
                if !s.meta.started {
                    return Some((
                        reply,
                        Err(err(
                            "chat_not_started",
                            "send a first message before opening the chat in a terminal",
                        )),
                    ));
                }
                if s.meta.turn.is_some() && p.get("stop").and_then(Value::as_bool) != Some(true) {
                    return Some((
                        reply,
                        Err(err(
                            "turn_running",
                            "available when it finishes (or stop the turn first: stop=true)",
                        )),
                    ));
                }
                if s.alive {
                    if s.meta.turn.is_some() {
                        s.meta.tr.interrupting = true;
                    }
                    s.pending = Some(reply);
                    self.chat_stop(&sid, Stop::Handoff);
                    return None;
                }
                let r = self.chat_to_terminal(&sid).await;
                Some((reply, r))
            }
            Some("chat") => Some((reply, self.chat_from_terminal(p).await)),
            _ => Some((reply, Err(err("invalid_params", "to must be terminal or chat")))),
        }
    }

    /// Open a pane resuming the session (`[resume]` of the plugin) and mark the chat as continuing there.
    async fn chat_to_terminal(&mut self, sid: &str) -> ApiResult {
        let (plugin, vendor, cwd, title) = {
            let s = self
                .chats
                .sessions
                .get(sid)
                .ok_or_else(|| err("chat_not_found", "no such chat"))?;
            (
                s.meta.plugin.clone(),
                s.meta.vendor_session.clone(),
                s.meta.cwd.clone(),
                s.meta.title.clone(),
            )
        };
        let kind = self.chat_plugin(&plugin)?.agent_label().to_string();
        let shim = self.shim_dir.clone().unwrap_or_default();
        let argv = crate::inject::resume_argv_with(&self.plugins, &self.shell, &shim, &kind, &vendor, None);
        let label = if title.is_empty() {
            None
        } else {
            Some(preview(&title, 40))
        };
        let (_, _, pane) = self.state.create_workspace(&cwd, label, argv, Default::default());
        if let Some(rec) = self.state.pane_mut(&pane) {
            rec.meta.kind = Some(kind);
            rec.meta.session_id = Some(vendor);
            rec.meta.cwd_was_repo = crate::inject::is_repo(&cwd);
        }
        self.spawn_pane(&pane, None);
        let _ = self.focus_pane_for_chat(&pane).await;
        self.commit_topology().await;
        {
            let s = self.chats.sessions.get_mut(sid).expect("checked");
            s.meta.status = "terminal".into();
            s.meta.pane_id = Some(pane.clone());
        }
        self.chat_emit(
            sid,
            ChatBody::SessionHandoff {
                to: "terminal".into(),
                pane_id: Some(pane.clone()),
            },
            None,
            0,
        )
        .await;
        self.chat_save(sid);
        crate::log!("chat {sid}: continues in the terminal ({pane})");
        let s = &self.chats.sessions[sid];
        Ok(json!({ "type": "chat_handoff", "paneId": pane, "session": s.info() }))
    }

    async fn focus_pane_for_chat(&mut self, pane: &str) -> Result<bool, ApiErr> {
        Ok(self.state.focus_pane(pane)?)
    }

    /// Terminal → chat: the pane's agent must be idle; the pane closes and
    /// the chat resumes the same session id at the next message.
    async fn chat_from_terminal(&mut self, p: &Value) -> ApiResult {
        let existing = str_param(p, "session_id").map(str::to_owned);
        let pane = match (str_param(p, "pane_id"), &existing) {
            (Some(pane), _) => pane.to_string(),
            (None, Some(sid)) => self
                .chats
                .sessions
                .get(sid)
                .ok_or_else(|| err("chat_not_found", format!("no chat {sid}")))?
                .meta
                .pane_id
                .clone()
                .ok_or_else(|| err("invalid_params", "this chat is not in a terminal; give pane_id"))?,
            (None, None) => return Err(err("invalid_params", "pane_id or session_id is required")),
        };
        let rec = self
            .state
            .pane(&pane)
            .ok_or_else(|| err("pane_not_found", format!("no pane {pane}")))?
            .clone();
        let vendor = rec
            .meta
            .session_id
            .clone()
            .ok_or_else(|| err("no_agent_session", "this pane has no agent session to open as a chat"))?;
        let kind = rec.meta.kind.clone().unwrap_or_default();
        let plugin = self
            .plugins
            .by_label(&kind)
            .ok_or_else(|| err("chat_not_supported", format!("{kind} has no chat mode")))?;
        let plugin_id = plugin.id().to_string();
        self.chat_plugin(&plugin_id)?;
        let status = self.panes.get(&pane).map(|rt| rt.track.effective);
        if matches!(
            status,
            Some(tmuxedo_protocol::frames::AgentStatus::Working | tmuxedo_protocol::frames::AgentStatus::Blocked)
        ) {
            return Err(err("agent_busy", "available when it finishes"));
        }
        // A session a chat already holds (it was a chat before, or `--resume` / `--continue`
        // reopened it in this pane): continue that chat, history and all.
        let existing = existing.or_else(|| self.chat_by_vendor(&plugin_id, &vendor));
        let sid = match existing {
            Some(sid) => {
                let s = self
                    .chats
                    .sessions
                    .get(&sid)
                    .ok_or_else(|| err("chat_not_found", format!("no chat {sid}")))?;
                if s.meta.vendor_session != vendor {
                    return Err(err("session_mismatch", "that pane runs a different agent session"));
                }
                sid
            }
            None => {
                // Adopt the pane's session as a new chat (login checked at its first start).
                let env = self.chat_env("x", plugin.as_ref());
                let login = self.chat_login(plugin.as_ref(), &env);
                self.chat_login_allowed(plugin.as_ref(), &login)?;
                let sid = new_session_id();
                let dir = self.chats.session_dir(&sid);
                std::fs::create_dir_all(&dir).map_err(|e| err("internal", e.to_string()))?;
                let now = iso_now();
                let meta = Meta {
                    id: sid.clone(),
                    plugin: plugin_id,
                    cwd: rec.pane.cwd.clone(),
                    mode: "ask_first".into(),
                    login,
                    title: rec.pane.label.clone().unwrap_or_default(),
                    created_at: now.clone(),
                    last_at: now,
                    vendor_session: vendor.clone(),
                    started: true,
                    status: "parked".into(),
                    ..Meta::default()
                };
                let mut s = Session::new(meta, EventLog::open(&dir), dir);
                s.save();
                self.chats.sessions.insert(sid.clone(), s);
                sid
            }
        };
        // The pane's agent ends; the CLI keeps the conversation under its id.
        if let Ok(closed) = self.state.close_pane(&pane) {
            self.kill_panes(&closed.panes);
            self.emit_closed(&closed);
            if self.state.workspaces.is_empty() {
                self.ensure_a_pane();
            }
            self.commit_topology().await;
        }
        {
            let s = self.chats.sessions.get_mut(&sid).expect("checked");
            s.meta.status = "parked".into();
            s.meta.pane_id = None;
            s.meta.started = true;
        }
        self.chat_emit(
            &sid,
            ChatBody::SessionHandoff {
                to: "chat".into(),
                pane_id: Some(pane),
            },
            None,
            0,
        )
        .await;
        self.chat_notice(
            &sid,
            "terminal_turns",
            "Turns taken in the terminal are not shown here; the agent remembers them.".into(),
        )
        .await;
        self.chat_save(&sid);
        self.chat_offers_changed().await;
        let s = &self.chats.sessions[&sid];
        Ok(json!({ "type": "chat_info", "session": s.info() }))
    }

    // ── a chat follows its agent session through panes (design §5.1) ──────
    //
    // The pane's hooks say which agent session runs there (`session.started`
    // with the session id and its source, `session.ended`). With that:
    //  - a chat bound to a pane is released when its session ends there (the
    //    agent exited, or a new session started in its place): the chat can
    //    drive it again;
    //  - a session that a chat holds, started in a pane (`--resume <id>`, or
    //    `--continue` landing on it), binds that chat to the pane: its history
    //    stays, "Back to chat" works once the agent is idle. If the chat was
    //    driving it (its child alive), that is two writers on one session: the
    //    chat pauses (its turn stops) and says so; the terminal carries on;
    //  - every pane's agent says whether it can open as a chat, and which chat
    //    holds its session (`Agent.chat`), so web, the app and the TUI can
    //    offer "Open as chat" again whenever an agent (re)starts in a pane.
    // Plugin-generic: any plugin whose hooks map `session.started` with the
    // session id (and whose manifest has `[chat]`) gets it.

    /// The chat that holds `vendor` (an agent session id of `plugin`), if any.
    fn chat_by_vendor(&self, plugin: &str, vendor: &str) -> Option<String> {
        self.chats
            .sessions
            .values()
            .filter(|s| s.meta.plugin == plugin && s.meta.vendor_session == vendor && s.meta.status != "closed")
            .max_by(|a, b| a.meta.last_at.cmp(&b.meta.last_at))
            .map(|s| s.meta.id.clone())
    }

    /// `Agent.chat` for a pane's agent of `kind` on agent session `vendor`.
    pub(crate) fn chat_offer(&self, kind: &str, vendor: Option<&str>) -> Option<tmuxedo_protocol::frames::AgentChat> {
        let vendor = vendor.filter(|v| !v.is_empty())?;
        let plugin = self.plugins.by_label(kind)?;
        let plugin = self.chat_plugin(plugin.id()).ok()?;
        if let Some(id) = self.chat_by_vendor(plugin.id(), vendor) {
            return Some(tmuxedo_protocol::frames::AgentChat { id: Some(id) });
        }
        self.chat_agent_ok(plugin.as_ref()).ok()?;
        let env = self.chat_env("x", plugin.as_ref());
        let login = self.chat_login(plugin.as_ref(), &env);
        self.chat_login_allowed(plugin.as_ref(), &login).ok()?;
        Some(tmuxedo_protocol::frames::AgentChat { id: None })
    }

    /// Chats changed (one bound, released, started or closed; the policy):
    /// every pane's `Agent.chat` is worked out again (a status frame where it changed).
    pub(crate) async fn chat_offers_changed(&mut self) {
        let panes: Vec<String> = self
            .state
            .panes
            .iter()
            .filter(|p| p.pane.agent.is_some())
            .map(|p| p.pane.id.clone())
            .collect();
        for p in panes {
            self.agent_update(&p, false).await;
        }
    }

    /// The pane `sid` continued in no longer runs its session: the chat drives it again.
    async fn chat_release(&mut self, sid: &str, code: &str, why: &str) {
        let pane = {
            let Some(s) = self.chats.sessions.get_mut(sid) else {
                return;
            };
            if s.meta.status != "terminal" {
                return;
            }
            s.meta.status = "parked".into();
            s.meta.pane_id.take()
        };
        crate::log!("chat {sid}: released from {} ({code})", pane.as_deref().unwrap_or("?"));
        self.chat_emit(
            sid,
            ChatBody::SessionHandoff {
                to: "chat".into(),
                pane_id: pane,
            },
            None,
            0,
        )
        .await;
        self.chat_notice(
            sid,
            code,
            format!("{why} You can carry on here: the agent remembers the conversation, turns taken in the terminal included."),
        )
        .await;
        self.chat_save(sid);
        self.chat_offers_changed().await;
    }

    /// `session.started` in a pane (plugin `plugin_id`, agent session `vendor`, `source` as the agent says).
    pub(crate) async fn chat_pane_started(&mut self, pane: &str, plugin_id: &str, vendor: &str, source: Option<&str>) {
        // A chat bound to this pane on another session: that session ended here.
        let replaced: Vec<String> = self
            .chats
            .sessions
            .values()
            .filter(|s| s.meta.status == "terminal" && s.meta.pane_id.as_deref() == Some(pane))
            .filter(|s| s.meta.vendor_session != vendor)
            .map(|s| s.meta.id.clone())
            .collect();
        for sid in replaced {
            self.chat_release(
                &sid,
                "terminal_new_session",
                "A new conversation started in the terminal.",
            )
            .await;
        }
        let Some(sid) = self.chat_by_vendor(plugin_id, vendor) else {
            return self.chat_offers_changed().await;
        };
        let (bound, alive, working) = {
            let s = &self.chats.sessions[&sid];
            (
                s.meta.status == "terminal" && s.meta.pane_id.as_deref() == Some(pane),
                s.alive,
                s.meta.turn.is_some(),
            )
        };
        if bound {
            // The pane a handoff opened (or the same pane again): nothing changes.
            return self.chat_offers_changed().await;
        }
        crate::log!(
            "chat {sid}: its session started in {pane} (source {}){}",
            source.unwrap_or("?"),
            if alive { "; the chat was driving it: paused" } else { "" }
        );
        let dropped = {
            let s = self.chats.sessions.get_mut(&sid).expect("found");
            s.meta.status = "terminal".into();
            s.meta.pane_id = Some(pane.to_string());
            if alive && working {
                s.meta.tr.interrupting = true;
            }
            std::mem::take(&mut s.queue).len()
        };
        if alive {
            // Two writers on one session (design §10.7): the terminal has it now; the chat's agent stops.
            let pending = {
                let s = self.chats.sessions.get_mut(&sid).expect("found");
                if s.stopping == Some(Stop::Handoff) {
                    s.stopping = None;
                    s.pending.take()
                } else {
                    None
                }
            };
            if let Some(reply) = pending {
                // A "to terminal" handoff was waiting for the agent: it is in this pane now.
                let info = self.chats.sessions[&sid].info();
                respond(
                    reply,
                    Ok(json!({ "type": "chat_handoff", "paneId": pane, "session": info })),
                    self,
                );
            }
            self.chat_stop(&sid, Stop::Park);
        }
        self.chat_emit(
            &sid,
            ChatBody::SessionHandoff {
                to: "terminal".into(),
                pane_id: Some(pane.to_string()),
            },
            None,
            0,
        )
        .await;
        if alive {
            let mut t = String::from(
                "This conversation was opened in a terminal, so the chat paused: one place drives it at a time. \
                 It continues there; use Back to chat when the agent is idle.",
            );
            if dropped > 0 {
                t.push_str(&format!(
                    " {dropped} waiting message{} not sent.",
                    if dropped == 1 { " was" } else { "s were" }
                ));
            }
            self.chat_notice(&sid, "opened_elsewhere", t).await;
        }
        self.chat_save(&sid);
        self.chat_offers_changed().await;
    }

    /// `session.ended` in a pane: a chat bound to it (on that session) is released.
    pub(crate) async fn chat_pane_ended(&mut self, pane: &str, vendor: Option<&str>) {
        let bound: Vec<String> = self
            .chats
            .sessions
            .values()
            .filter(|s| s.meta.status == "terminal" && s.meta.pane_id.as_deref() == Some(pane))
            .filter(|s| vendor.is_none_or(|v| s.meta.vendor_session == v))
            .map(|s| s.meta.id.clone())
            .collect();
        for sid in bound {
            self.chat_release(&sid, "terminal_ended", "The agent in the terminal stopped.")
                .await;
        }
    }
}

/// Whether an agent CLI of version `have` is older than the `min` its chat mode needs.
fn agent_too_old(have: Option<&str>, min: &str) -> bool {
    !min.is_empty() && have.is_some_and(|v| !crate::skill::version_at_least(Some(v), min))
}

fn check_text(t: &str) -> Result<(), ApiErr> {
    if t.trim().is_empty() {
        return Err(err("invalid_params", "text is empty"));
    }
    if t.len() > MAX_SEND_BYTES {
        return Err(err(
            "too_large",
            format!("a message is at most {MAX_SEND_BYTES} bytes; attach a file instead"),
        ));
    }
    Ok(())
}

/// How many hook-shape lines one runner process writes (see [`log_hook_shape`]).
const HOOK_SHAPE_LINES: u32 = 40;
static HOOK_SHAPES_LOGGED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// What a chat child's permission hook carries, by NAME only: the payload's
/// top-level keys, the keys of `tool_input`, and whether `tool_use_id` is
/// there. Never a value: a command or a file's text can hold a secret.
/// It answers, from a real CLI, whether a decision can be tied to its call by
/// id (docs/handoff/chat-tools.md, "the toolId finding").
fn hook_shape(native: &Value) -> Option<String> {
    let event = native["hook_event_name"].as_str().unwrap_or("");
    if event != "PermissionRequest" && event != "PreToolUse" {
        return None;
    }
    let names = |v: Option<&Value>| -> String {
        let mut k: Vec<&str> = v
            .and_then(Value::as_object)
            .map(|o| o.keys().map(String::as_str).collect())
            .unwrap_or_default();
        k.sort_unstable();
        k.join(",")
    };
    let has_id = native["tool_use_id"].as_str().is_some_and(|i| !i.is_empty());
    let tool: String = native["tool_name"]
        .as_str()
        .unwrap_or("")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .take(64)
        .collect();
    Some(format!(
        "chat hook shape: event={event} tool={tool} tool_use_id={} keys=[{}] input_keys=[{}]",
        if has_id { "yes" } else { "no" },
        names(Some(native)),
        names(native.get("tool_input")),
    ))
}

/// The first [`HOOK_SHAPE_LINES`] permission hooks of a runner process, logged by shape.
fn log_hook_shape(native: &Value, guard: bool) {
    use std::sync::atomic::Ordering;
    let Some(line) = hook_shape(native) else { return };
    if HOOK_SHAPES_LOGGED.fetch_add(1, Ordering::Relaxed) < HOOK_SHAPE_LINES {
        crate::log!("{line} guard={guard}");
    }
}

#[cfg(test)]
mod hook_shape_tests {
    use super::hook_shape;
    use serde_json::json;

    #[test]
    fn names_only_never_a_value() {
        let line = hook_shape(&json!({
            "hook_event_name": "PermissionRequest",
            "tool_name": "Bash",
            "tool_use_id": "toolu_1",
            "session_id": "s-secret",
            "tool_input": {"command": "deploy --token SECRET-VALUE x", "description": "d"},
        }))
        .unwrap();
        assert_eq!(
            line,
            "chat hook shape: event=PermissionRequest tool=Bash tool_use_id=yes \
             keys=[hook_event_name,session_id,tool_input,tool_name,tool_use_id] input_keys=[command,description]"
        );
        assert!(!line.contains("SECRET") && !line.contains("toolu_1") && !line.contains("s-secret"));
    }

    #[test]
    fn no_id_other_events_and_odd_names() {
        let line =
            hook_shape(&json!({"hook_event_name":"PreToolUse","tool_name":"mcp__x y;rm","tool_input":"text"})).unwrap();
        assert!(
            line.contains("tool=mcp__xyrm tool_use_id=no ") && line.ends_with("input_keys=[]"),
            "{line}"
        );
        assert!(hook_shape(&json!({"hook_event_name":"Stop","tool_name":"Bash"})).is_none());
        assert!(hook_shape(&json!("junk")).is_none());
    }
}
