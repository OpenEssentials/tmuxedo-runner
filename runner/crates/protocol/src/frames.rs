//! JSON control frames (docs/protocol.md "Text frames"). Field names are the
//! wire names (camelCase); `t` is the frame type.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::PROTOCOL_VERSION;

// ── Shapes ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Workspace {
    pub id: String,
    pub name: String,
    pub cwd: String,
    pub position: u32,
    /// The workspace's active tab (shared focus; optional on the wire).
    #[serde(rename = "activeTab", default, skip_serializing_if = "Option::is_none")]
    pub active_tab: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Layout {
    Pane {
        pane: String,
    },
    Split {
        dir: SplitDir,
        ratio: f64,
        a: Box<Layout>,
        b: Box<Layout>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SplitDir {
    H,
    V,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tab {
    pub id: String,
    pub workspace_id: String,
    pub name: String,
    pub position: u32,
    pub layout: Layout,
    /// The tab's focused pane (shared focus; optional on the wire).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focused_pane: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Exited {
    pub code: Option<i32>,
    pub at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Pane {
    pub id: String,
    pub tab_id: String,
    pub cwd: String,
    pub command: String,
    pub title: String,
    pub cols: u16,
    pub rows: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exited: Option<Exited>,
    /// Always present on the wire, `null` when no agent.
    #[serde(default)]
    pub agent: Option<Agent>,
    /// The name a person gave the pane ([`crate::names`] rules); absent when none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentStatus {
    Working,
    Blocked,
    Done,
    Idle,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentSource {
    Reported,
    Screen,
    Heuristic,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentPrompt {
    pub tool: String,
    pub input: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggestions: Option<Value>,
    /// Files (Phase 15): the agent read this uploaded file earlier in the same
    /// turn, so whoever approves knows untrusted content came first.
    #[serde(rename = "afterUpload", default, skip_serializing_if = "Option::is_none")]
    pub after_upload: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Agent {
    pub kind: String,
    pub status: AgentStatus,
    pub since: String,
    pub source: AgentSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<AgentPrompt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// The agent session in this pane can be opened as a chat (its plugin
    /// has a chat mode and the login may use it); absent = it cannot, or a
    /// runner without the `chat_offer` feature (docs/protocol.md "Agent chat").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat: Option<AgentChat>,
    /// In-process sub-agents, "helpers" ([`crate::subagents`]); absent from
    /// older runners and when there are none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<crate::subagents::AgentChild>,
}

/// [`Agent::chat`]: how the pane's agent session relates to agent chat.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentChat {
    /// The chat that already holds this agent session (its history), if any:
    /// "Open as chat" continues that chat instead of starting a new one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

/// The hello feature of a runner that sends [`Agent::chat`] and keeps chats
/// bound to the agent session in a pane (SessionStart/SessionEnd).
pub const FEATURE_CHAT_OFFER: &str = "chat_offer";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunnerInfo {
    pub version: String,
    pub holder_version: String,
    pub os: String,
    pub arch: String,
    pub hostname: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_version: Option<String>,
    /// Claude login health `{state: ok|missing|expired|failed, reason, since}`
    /// (absent when the machine has no Claude Code).
    #[serde(rename = "claudeAuth", default, skip_serializing_if = "Option::is_none")]
    pub claude_auth: Option<serde_json::Value>,
    /// Protocol additions this runner serves (`["glance"]`); absent on older runners.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub features: Option<Vec<String>>,
    /// The runner's plugins ([`crate::plugins::PluginInfo`]), sent beside the
    /// legacy `claudeVersion` / `claudeAuth` (dual-write). A runner that sends
    /// it answers `plugin.*` over the gateway and sends `ui` frames.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<Vec<crate::plugins::PluginInfo>>,
    /// Machine-side consent to headless tasks (docs/design/headless-tasks.md §4):
    /// the labels its owner gave it. Absent = this machine takes no automation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub automation: Option<Automation>,
    /// Whether the runner keeps the machine awake while agents work
    /// ([`crate::wake::KeepAwake`]); absent on older runners.
    #[serde(rename = "keepAwake", default, skip_serializing_if = "Option::is_none")]
    pub keep_awake: Option<crate::wake::KeepAwake>,
    /// The interfaces this machine can be woken through
    /// ([`crate::wake::Network`]); absent on older runners.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<crate::wake::Network>,
}

/// `hello.runner.automation`: the machine's own automation labels (at most
/// [`MAX_AUTOMATION_LABELS`], each matching [`valid_automation_label`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Automation {
    pub labels: Vec<String>,
}

pub const MAX_AUTOMATION_LABELS: usize = 16;

/// A label: 1 to 32 of `a-z 0-9 -`, starting with a letter or digit.
pub fn valid_automation_label(l: &str) -> bool {
    let b = l.as_bytes();
    !b.is_empty()
        && b.len() <= 32
        && b[0].is_ascii_alphanumeric()
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

/// The hello feature for glance frames (docs/protocol.md "Glances").
pub const FEATURE_GLANCE: &str = "glance";
/// The hello feature for `login_callback` frames (docs/protocol.md "Login callback relay").
pub const FEATURE_LOGIN_CALLBACK: &str = "login_callback";
/// The hello feature of a runner that sends `Pane.label` and applies the
/// shared name rules on rename ([`crate::names`], docs/protocol.md "Names").
pub const FEATURE_NAMES: &str = "names";
/// Glance rate bounds and default (per pane, per second).
pub const GLANCE_HZ_MIN: f64 = 0.1;
pub const GLANCE_HZ_MAX: f64 = 1.0;
pub const GLANCE_HZ_DEFAULT: f64 = 0.5;

/// Clamp a requested glance rate (non-finite → default).
pub fn clamp_hz(hz: f64) -> f64 {
    if hz.is_finite() {
        hz.clamp(GLANCE_HZ_MIN, GLANCE_HZ_MAX)
    } else {
        GLANCE_HZ_DEFAULT
    }
}

/// A glance cursor position (0-based, in the glance's cropped grid).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    pub x: u16,
    pub y: u16,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub code: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

// ── runner → gateway ──────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "lowercase")]
pub enum RunnerFrame {
    Hello {
        v: u32,
        runner: RunnerInfo,
    },
    Topology {
        rev: u64,
        workspaces: Vec<Workspace>,
        tabs: Vec<Tab>,
        panes: Vec<Pane>,
        /// The machine's focused workspace (shared focus; optional).
        #[serde(rename = "focusedWorkspace", default, skip_serializing_if = "Option::is_none")]
        focused_workspace: Option<String>,
    },
    Status {
        #[serde(rename = "paneId")]
        pane_id: String,
        agent: Option<Agent>,
    },
    Event {
        seq: u64,
        #[serde(rename = "paneId")]
        pane_id: String,
        #[serde(rename = "type")]
        kind: String,
        at: String,
        data: Value,
    },
    Res {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<ErrorBody>,
    },
    Size {
        #[serde(rename = "paneId")]
        pane_id: String,
        cols: u16,
        rows: u16,
    },
    /// A pane's visible screen as styled runs (docs/protocol.md "Glances").
    /// `lines` is one array of runs `[text, fg?, bg?, attrs?]` per row.
    Glance {
        #[serde(rename = "paneId")]
        pane_id: String,
        rev: u64,
        cols: u16,
        rows: u16,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cursor: Option<Cursor>,
        lines: Value,
    },
    /// One machine stats sample (docs/protocol.md "Machine stats"); only while
    /// the gateway's last `stats` frame asked for a rate above 0.
    Stats(crate::stats::StatsSample),
    /// The newest AI usage record of one account (docs/protocol.md "AI
    /// usage"): on change, at most one per account every two seconds.
    Usage(crate::usage::UsageRecord),
    /// The answer to `chat_attach`, sent after the replayed CHAT frames:
    /// `ok` with the session's last seq, or the refusal code
    /// (`chat_not_found`). The gateway hands it to the requesting client.
    #[serde(rename = "chat_attached")]
    ChatAttached {
        req: u32,
        #[serde(rename = "sessionId")]
        session_id: String,
        ok: bool,
        #[serde(rename = "lastSeq", default)]
        last_seq: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<String>,
    },
    /// A plugin's declared surfaces; replaces the set for (plugin, scope,
    /// pane). Sent only after a `welcome` whose `features` lists `ui`.
    Ui {
        #[serde(rename = "pluginId")]
        plugin_id: String,
        /// `machine` or `pane`
        scope: String,
        #[serde(rename = "paneId", default, skip_serializing_if = "Option::is_none")]
        pane_id: Option<String>,
        items: Vec<crate::plugins::UiItem>,
    },
    /// A pane's open needs-you items (`""` = machine-level); replaces the
    /// set for that pane. Sent only after a `welcome` listing `needs`.
    Needs {
        #[serde(rename = "paneId")]
        pane_id: String,
        items: Vec<crate::needs::NeedsYou>,
    },
    Ping {
        at: u64,
    },
    Pong {
        at: u64,
    },
}

impl RunnerFrame {
    pub fn hello(runner: RunnerInfo) -> Self {
        RunnerFrame::Hello {
            v: PROTOCOL_VERSION,
            runner,
        }
    }
    pub fn ok(id: impl Into<String>, result: Value) -> Self {
        RunnerFrame::Res {
            id: id.into(),
            result: Some(result),
            error: None,
        }
    }
    pub fn err(id: impl Into<String>, code: &str, message: impl Into<String>) -> Self {
        RunnerFrame::Res {
            id: id.into(),
            result: None,
            error: Some(ErrorBody {
                code: code.into(),
                message: Some(message.into()),
            }),
        }
    }
    pub fn to_json(&self) -> String {
        // Serialising these types cannot fail (no maps with non-string keys).
        serde_json::to_string(self).unwrap_or_else(|_| "{}".into())
    }
}

// ── gateway → runner ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Behavior {
    Allow,
    Deny,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "lowercase")]
pub enum GatewayFrame {
    Welcome {
        #[serde(rename = "machineId")]
        machine_id: String,
        #[serde(rename = "userId")]
        user_id: String,
        /// Highest event seq web has stored for this machine (0 if none;
        /// absent from older gateways).
        #[serde(rename = "lastEventSeq", default)]
        last_event_seq: u64,
        /// What this gateway relays (`["ui"]`); absent from older gateways.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        features: Option<Vec<String>>,
    },
    Attach {
        req: u32,
        #[serde(rename = "paneId")]
        pane_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<u64>,
    },
    Detach {
        #[serde(rename = "paneId")]
        pane_id: String,
    },
    Req {
        id: String,
        method: String,
        #[serde(default)]
        params: Value,
    },
    Resize {
        #[serde(rename = "paneId")]
        pane_id: String,
        cols: u16,
        rows: u16,
    },
    Decision {
        #[serde(rename = "paneId")]
        pane_id: String,
        #[serde(rename = "promptId")]
        prompt_id: String,
        behavior: Behavior,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    /// Launch and shell configuration from web (docs/protocol.md `config`);
    /// kept as JSON, the runner validates each part.
    #[serde(rename = "config")]
    Config {
        #[serde(default)]
        claude: Option<Value>,
        /// Per plugin (`[{id, settings: {launch: {args}, skill: {enabled}}}]`),
        /// sent beside `claude` by newer webs; see [`crate::plugins::parse_config_plugins`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        plugins: Option<Value>,
        #[serde(default)]
        shell: Option<Value>,
        /// The folder new panes start in (`~/…` or absolute; `""` = the
        /// runner's default, `~/work`). Absent = unchanged.
        #[serde(default, rename = "workDir", skip_serializing_if = "Option::is_none")]
        work_dir: Option<Value>,
        /// Chat policy from web (docs/protocol.md "Agent chat"):
        /// `{subscription: "allow" | "deny"}`. Absent = unchanged.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        chat: Option<Value>,
        /// Project env (docs/protocol.md `config`): project key → {NAME:
        /// value | null}. Absent = unchanged.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        projects: Option<Box<Value>>,
        /// What each plugin gets on this machine, per project
        /// ([`crate::plugin_set::parse_plugin_set`]). Absent = unchanged.
        #[serde(default, rename = "pluginSet", skip_serializing_if = "Option::is_none")]
        plugin_set: Option<Box<Value>>,
        /// The roles effective on this machine, per project
        /// ([`crate::role_set::parse_role_set`]). Absent = unchanged. Boxed: the
        /// frame enum stays small.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        roles: Option<Box<Value>>,
        /// The projects whose search history this machine keeps
        /// (docs/protocol.md "Search history"): `{projects: [{key, days?}]}`.
        /// Absent = unchanged (an older web). Boxed: the frame enum stays small.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        history: Option<Box<Value>>,
        /// Keep this machine awake while agents work (`true` | `false`).
        /// Absent = unchanged.
        #[serde(default, rename = "keepAwake", skip_serializing_if = "Option::is_none")]
        keep_awake: Option<Box<Value>>,
        /// Warn before the memory watchdog stops a process (`true` | `false`;
        /// docs/protocol.md "Machine stats"). Absent = unchanged.
        #[serde(default, rename = "memoryWarn", skip_serializing_if = "Option::is_none")]
        memory_warn: Option<Box<Value>>,
    },
    /// Replay a chat's durable events from seq `from` (default 1) as CHAT
    /// frames tagged `req`, then `chat_attached`, then stream it live.
    #[serde(rename = "chat_attach")]
    ChatAttach {
        req: u32,
        #[serde(rename = "sessionId")]
        session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<u64>,
    },
    /// Nobody watches this chat any more.
    #[serde(rename = "chat_detach")]
    ChatDetach {
        #[serde(rename = "sessionId")]
        session_id: String,
    },
    /// Web refused these seqs (it holds different events under them): the
    /// runner renumbers them above `max(local, last_event_seq)` and resends.
    #[serde(rename = "events_rejected")]
    EventsRejected {
        seqs: Vec<u64>,
        #[serde(rename = "lastEventSeq", default)]
        last_event_seq: u64,
    },
    /// The union of every client's glance set (empty = stop) and the rate.
    Glance {
        #[serde(rename = "paneIds")]
        pane_ids: Vec<String>,
        #[serde(default = "default_hz")]
        hz: f64,
    },
    /// Sample the machine `hz` times a second and send `stats` frames; 0 = stop
    /// (docs/protocol.md "Machine stats"). Only to runners with the `stats` feature.
    Stats {
        #[serde(default)]
        hz: f64,
    },
    /// Deliver the address a user's browser ended on (a failed redirect to
    /// a loopback OAuth callback) to that callback, inside the machine
    /// (docs/protocol.md "Login callback relay"). Answered with `res {id}`.
    #[serde(rename = "login_callback")]
    LoginCallback {
        id: String,
        #[serde(rename = "paneId")]
        pane_id: String,
        url: String,
    },
    /// The answer to a runner's `secret_key` or `secret_req` (docs/protocol.md
    /// "Secrets"): `result` holds only ciphertext sealed to the runner's key.
    #[serde(rename = "secret_res")]
    SecretRes {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<ErrorBody>,
    },
    /// The machine's secret policy (names and bindings, no values), the
    /// sealed mask list, and names to forget; after `welcome` and on change.
    Secrets {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mask: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        policy: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        revoked: Option<Vec<String>>,
    },
    /// Every `event` with `seq <= up_to` is durably stored by web.
    #[serde(rename = "events_ack")]
    EventsAck {
        #[serde(rename = "upTo")]
        up_to: u64,
    },
    Ping {
        #[serde(default)]
        at: u64,
    },
    Pong {
        #[serde(default)]
        at: u64,
    },
}

fn default_hz() -> f64 {
    GLANCE_HZ_DEFAULT
}

/// Outcome of parsing a gateway text frame.
#[derive(Debug, Clone, PartialEq)]
pub enum Parsed {
    Frame(GatewayFrame),
    /// A well-formed object with a `t` we do not know: ignored (forward compatibility).
    Unknown(String),
    /// Junk: not JSON, not an object, or a known `t` with bad fields.
    Invalid(&'static str),
}

const KNOWN: &[&str] = &[
    "welcome",
    "attach",
    "detach",
    "req",
    "resize",
    "decision",
    "events_ack",
    "events_rejected",
    "config",
    "glance",
    "stats",
    "login_callback",
    "chat_attach",
    "chat_detach",
    "secret_res",
    "secrets",
    "ping",
    "pong",
];

/// Parse a gateway → runner text frame. Never panics.
pub fn parse_gateway(text: &str) -> Parsed {
    if text.len() > crate::MAX_FRAME_BYTES {
        return Parsed::Invalid("too_large");
    }
    let v: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return Parsed::Invalid("not_json"),
    };
    let Some(obj) = v.as_object() else {
        return Parsed::Invalid("not_object");
    };
    let t = match obj.get("t").and_then(Value::as_str) {
        Some(t) => t.to_owned(),
        None => return Parsed::Invalid("no_type"),
    };
    if !KNOWN.contains(&t.as_str()) {
        return Parsed::Unknown(t);
    }
    match serde_json::from_value::<GatewayFrame>(v) {
        Ok(f) => {
            if let GatewayFrame::Resize { cols, rows, .. } = &f {
                if !(1..=1000).contains(cols) || !(1..=1000).contains(rows) {
                    return Parsed::Invalid("bad_resize");
                }
            }
            if let GatewayFrame::LoginCallback { id, pane_id, url } = &f {
                if id.is_empty() || id.len() > 64 || pane_id.len() > 255 || url.is_empty() || url.len() > 8192 {
                    return Parsed::Invalid("bad_login_callback");
                }
            }
            if let GatewayFrame::ChatAttach { session_id, .. } | GatewayFrame::ChatDetach { session_id } = &f {
                if !crate::chat::valid_session_id(session_id) {
                    return Parsed::Invalid("bad_chat");
                }
            }
            if let GatewayFrame::Glance { pane_ids, hz } = f {
                if pane_ids.len() > 4096 || pane_ids.iter().any(|p| p.is_empty() || p.len() > 255) {
                    return Parsed::Invalid("bad_glance");
                }
                return Parsed::Frame(GatewayFrame::Glance {
                    pane_ids,
                    hz: clamp_hz(hz),
                });
            }
            if let GatewayFrame::Stats { hz } = f {
                // 0, negative or not a number all mean stop.
                return Parsed::Frame(GatewayFrame::Stats {
                    hz: crate::stats::stats_rate(hz).unwrap_or(0.0),
                });
            }
            Parsed::Frame(f)
        }
        Err(_) => Parsed::Invalid("bad_fields"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_agent() -> Agent {
        Agent {
            kind: "claude".into(),
            status: AgentStatus::Blocked,
            since: "2026-10-02T10:00:00Z".into(),
            source: AgentSource::Reported,
            session_id: Some("s1".into()),
            prompt_id: Some("p1".into()),
            prompt: Some(AgentPrompt {
                after_upload: None,
                tool: "Bash".into(),
                input: json!({"command": "ls"}),
                suggestions: None,
            }),
            summary: None,
            chat: Some(AgentChat { id: Some("c1".into()) }),
            children: Vec::new(),
        }
    }

    fn all_runner_frames() -> Vec<RunnerFrame> {
        vec![
            RunnerFrame::hello(RunnerInfo {
                version: "0.1.0".into(),
                holder_version: "0.1.0".into(),
                os: "linux".into(),
                arch: "x86_64".into(),
                hostname: "box".into(),
                claude_version: Some("2.1.288".into()),
                claude_auth: Some(
                    json!({"state": "ok", "reason": "credentials present", "since": "2026-10-03T00:00:00Z"}),
                ),
                features: Some(vec!["glance".into()]),
                plugins: None,
                automation: Some(Automation {
                    labels: vec!["automation".into()],
                }),
                keep_awake: Some(crate::wake::KeepAwake {
                    enabled: true,
                    available: true,
                    held: true,
                    reason: None,
                }),
                network: Some(crate::wake::Network {
                    interfaces: vec![crate::wake::Interface {
                        mac: "02:00:00:00:00:01".into(),
                        ipv4: "192.0.2.20".into(),
                        prefix: 24,
                    }],
                }),
            }),
            RunnerFrame::Topology {
                rev: 3,
                focused_workspace: Some("w1".into()),
                workspaces: vec![Workspace {
                    id: "w1".into(),
                    name: "default".into(),
                    cwd: "/home/u".into(),
                    position: 0,
                    active_tab: None,
                }],
                tabs: vec![Tab {
                    id: "w1:t1".into(),
                    workspace_id: "w1".into(),
                    name: "1".into(),
                    position: 0,
                    layout: Layout::Split {
                        dir: SplitDir::H,
                        ratio: 0.5,
                        a: Box::new(Layout::Pane { pane: "w1:p1".into() }),
                        b: Box::new(Layout::Pane { pane: "w1:p2".into() }),
                    },
                    focused_pane: None,
                }],
                panes: vec![
                    Pane {
                        id: "w1:p1".into(),
                        tab_id: "w1:t1".into(),
                        cwd: "/home/u".into(),
                        command: "/bin/bash -l".into(),
                        title: "bash".into(),
                        cols: 120,
                        rows: 40,
                        exited: None,
                        agent: None,
                        label: Some("build".into()),
                    },
                    Pane {
                        id: "w1:p2".into(),
                        tab_id: "w1:t1".into(),
                        cwd: "/home/u".into(),
                        command: "claude".into(),
                        title: "claude".into(),
                        cols: 80,
                        rows: 24,
                        exited: Some(Exited {
                            code: Some(0),
                            at: "2026-10-02T10:00:00Z".into(),
                        }),
                        agent: Some(sample_agent()),
                        label: None,
                    },
                ],
            },
            RunnerFrame::Status {
                pane_id: "w1:p1".into(),
                agent: Some(sample_agent()),
            },
            RunnerFrame::Status {
                pane_id: "w1:p1".into(),
                agent: None,
            },
            RunnerFrame::Event {
                seq: 42,
                pane_id: "w1:p1".into(),
                kind: "memory_stopped".into(),
                at: "2026-10-02T10:00:00Z".into(),
                data: json!({"rss": 1}),
            },
            RunnerFrame::ok("r1", json!({"type": "ok"})),
            RunnerFrame::err("r2", "not_supported", "nope"),
            RunnerFrame::Size {
                pane_id: "w1:p1".into(),
                cols: 80,
                rows: 24,
            },
            RunnerFrame::Ping { at: 1 },
            RunnerFrame::Pong { at: 2 },
            RunnerFrame::Glance {
                pane_id: "w1:p1".into(),
                rev: 3,
                cols: 80,
                rows: 2,
                cursor: Some(Cursor { x: 1, y: 0 }),
                lines: json!([[["hi"], ["!", 1, null, 1]], []]),
            },
            RunnerFrame::Glance {
                pane_id: "w1:p1".into(),
                rev: 4,
                cols: 80,
                rows: 1,
                cursor: None,
                lines: json!([[]]),
            },
        ]
    }

    #[test]
    fn runner_frames_round_trip() {
        for f in all_runner_frames() {
            let s = f.to_json();
            let back: RunnerFrame = serde_json::from_str(&s).unwrap();
            assert_eq!(back, f, "{s}");
        }
    }

    #[test]
    fn runner_frames_wire_names() {
        let f = all_runner_frames();
        let hello: Value = serde_json::from_str(&f[0].to_json()).unwrap();
        assert_eq!(hello["t"], "hello");
        assert_eq!(hello["v"], 1);
        assert_eq!(hello["runner"]["holderVersion"], "0.1.0");
        assert_eq!(hello["runner"]["claudeVersion"], "2.1.288");
        assert_eq!(hello["runner"]["features"], json!(["glance"]));
        let g: Value = serde_json::from_str(&f[10].to_json()).unwrap();
        assert_eq!(g["t"], "glance");
        assert_eq!(g["paneId"], "w1:p1");
        assert_eq!(g["cursor"], json!({"x": 1, "y": 0}));
        let g2: Value = serde_json::from_str(&f[11].to_json()).unwrap();
        assert!(g2.get("cursor").is_none());
        let topo: Value = serde_json::from_str(&f[1].to_json()).unwrap();
        assert_eq!(topo["tabs"][0]["workspaceId"], "w1");
        assert_eq!(topo["tabs"][0]["layout"]["dir"], "h");
        assert_eq!(topo["tabs"][0]["layout"]["a"]["pane"], "w1:p1");
        assert_eq!(topo["panes"][0]["tabId"], "w1:t1");
        assert!(topo["panes"][0]["agent"].is_null());
        assert!(topo["panes"][0].get("exited").is_none());
        assert_eq!(topo["panes"][1]["exited"]["code"], 0);
        assert_eq!(topo["panes"][1]["agent"]["promptId"], "p1");
        let status_none: Value = serde_json::from_str(&f[3].to_json()).unwrap();
        assert!(status_none["agent"].is_null());
        assert_eq!(status_none["paneId"], "w1:p1");
        let ev: Value = serde_json::from_str(&f[4].to_json()).unwrap();
        assert_eq!(ev["type"], "memory_stopped");
        let err: Value = serde_json::from_str(&f[6].to_json()).unwrap();
        assert_eq!(err["error"]["code"], "not_supported");
        assert!(err.get("result").is_none());
    }

    #[test]
    fn focus_fields_wire_names() {
        let f = RunnerFrame::Topology {
            rev: 1,
            workspaces: vec![Workspace {
                id: "w1".into(),
                name: "a".into(),
                cwd: "/".into(),
                position: 0,
                active_tab: Some("w1:t1".into()),
            }],
            tabs: vec![Tab {
                id: "w1:t1".into(),
                workspace_id: "w1".into(),
                name: "1".into(),
                position: 0,
                layout: Layout::Pane { pane: "w1:p1".into() },
                focused_pane: Some("w1:p1".into()),
            }],
            panes: vec![],
            focused_workspace: Some("w1".into()),
        };
        let v: Value = serde_json::from_str(&f.to_json()).unwrap();
        assert_eq!(v["focusedWorkspace"], "w1");
        assert_eq!(v["workspaces"][0]["activeTab"], "w1:t1");
        assert_eq!(v["tabs"][0]["focusedPane"], "w1:p1");
    }

    #[test]
    fn gateway_frames_parse() {
        let cases = [
            (
                r#"{"t":"welcome","machineId":"m","userId":"u"}"#,
                GatewayFrame::Welcome {
                    machine_id: "m".into(),
                    user_id: "u".into(),
                    last_event_seq: 0,
                    features: None,
                },
            ),
            (
                r#"{"t":"welcome","machineId":"m","userId":"u","lastEventSeq":41}"#,
                GatewayFrame::Welcome {
                    machine_id: "m".into(),
                    user_id: "u".into(),
                    last_event_seq: 41,
                    features: None,
                },
            ),
            (
                r#"{"t":"attach","req":7,"paneId":"w1:p1","from":4294967297}"#,
                GatewayFrame::Attach {
                    req: 7,
                    pane_id: "w1:p1".into(),
                    from: Some(4294967297),
                },
            ),
            (
                r#"{"t":"attach","req":1,"paneId":"w1:p1"}"#,
                GatewayFrame::Attach {
                    req: 1,
                    pane_id: "w1:p1".into(),
                    from: None,
                },
            ),
            (r#"{"t":"events_ack","upTo":12}"#, GatewayFrame::EventsAck { up_to: 12 }),
            (
                r#"{"t":"events_rejected","seqs":[2,3],"lastEventSeq":40}"#,
                GatewayFrame::EventsRejected {
                    seqs: vec![2, 3],
                    last_event_seq: 40,
                },
            ),
            (
                r#"{"t":"detach","paneId":"w1:p1"}"#,
                GatewayFrame::Detach {
                    pane_id: "w1:p1".into(),
                },
            ),
            (
                r#"{"t":"req","id":"a","method":"pane.list","params":{}}"#,
                GatewayFrame::Req {
                    id: "a".into(),
                    method: "pane.list".into(),
                    params: json!({}),
                },
            ),
            (
                r#"{"t":"resize","paneId":"w1:p1","cols":80,"rows":24}"#,
                GatewayFrame::Resize {
                    pane_id: "w1:p1".into(),
                    cols: 80,
                    rows: 24,
                },
            ),
            (
                r#"{"t":"decision","paneId":"w1:p1","promptId":"x","behavior":"deny","message":"no"}"#,
                GatewayFrame::Decision {
                    pane_id: "w1:p1".into(),
                    prompt_id: "x".into(),
                    behavior: Behavior::Deny,
                    message: Some("no".into()),
                },
            ),
            (
                r#"{"t":"glance","paneIds":["w1:p1","w1:p2"],"hz":0.5}"#,
                GatewayFrame::Glance {
                    pane_ids: vec!["w1:p1".into(), "w1:p2".into()],
                    hz: 0.5,
                },
            ),
            (
                r#"{"t":"glance","paneIds":[],"hz":1}"#,
                GatewayFrame::Glance {
                    pane_ids: vec![],
                    hz: 1.0,
                },
            ),
            (r#"{"t":"ping","at":5}"#, GatewayFrame::Ping { at: 5 }),
            (r#"{"t":"pong"}"#, GatewayFrame::Pong { at: 0 }),
        ];
        for (text, want) in cases {
            assert_eq!(parse_gateway(text), Parsed::Frame(want.clone()), "{text}");
            // And they round-trip through our own serialiser.
            let s = serde_json::to_string(&want).unwrap();
            assert_eq!(parse_gateway(&s), Parsed::Frame(want));
        }
    }

    #[test]
    fn gateway_junk_is_refused_not_panicked() {
        assert_eq!(parse_gateway("nope"), Parsed::Invalid("not_json"));
        assert_eq!(parse_gateway("[1]"), Parsed::Invalid("not_object"));
        assert_eq!(parse_gateway("{}"), Parsed::Invalid("no_type"));
        assert_eq!(
            parse_gateway(r#"{"t":"future_thing","x":1}"#),
            Parsed::Unknown("future_thing".into())
        );
        assert_eq!(
            parse_gateway(r#"{"t":"attach","req":-1,"paneId":"p"}"#),
            Parsed::Invalid("bad_fields")
        );
        assert_eq!(
            parse_gateway(r#"{"t":"resize","paneId":"p","cols":0,"rows":5}"#),
            Parsed::Invalid("bad_resize")
        );
        assert_eq!(
            parse_gateway(r#"{"t":"decision","paneId":"p","promptId":"x","behavior":"maybe"}"#),
            Parsed::Invalid("bad_fields")
        );
        assert_eq!(
            parse_gateway(r#"{"t":"glance","paneIds":[""],"hz":1}"#),
            Parsed::Invalid("bad_glance")
        );
        assert_eq!(parse_gateway(r#"{"t":"glance","hz":1}"#), Parsed::Invalid("bad_fields"));
    }

    #[test]
    fn glance_hz_is_clamped_and_defaulted() {
        assert_eq!(
            parse_gateway(r#"{"t":"glance","paneIds":["p"],"hz":50}"#),
            Parsed::Frame(GatewayFrame::Glance {
                pane_ids: vec!["p".into()],
                hz: 1.0
            })
        );
        assert_eq!(
            parse_gateway(r#"{"t":"glance","paneIds":["p"],"hz":0}"#),
            Parsed::Frame(GatewayFrame::Glance {
                pane_ids: vec!["p".into()],
                hz: 0.1
            })
        );
        assert_eq!(
            parse_gateway(r#"{"t":"glance","paneIds":["p"]}"#),
            Parsed::Frame(GatewayFrame::Glance {
                pane_ids: vec!["p".into()],
                hz: 0.5
            })
        );
        assert_eq!(clamp_hz(f64::NAN), GLANCE_HZ_DEFAULT);
    }
}
