//! The plugin manifest (`plugin.toml`, docs/design/plugins.md §5.2) and its
//! linter (§11 step 1). Contract major **1**.
//!
//! Everything in braces in a manifest string is a **host-resolved
//! placeholder** ([`PLACEHOLDERS`]); the host renders argv as a list and
//! execs it, never through a shell. Unknown fields are ignored (minor
//! versions add optional fields, §10).

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// The contract major this host implements.
pub const CONTRACT_MAJOR: u32 = 1;

/// Capabilities a manifest may declare (§9.3).
pub const CAPABILITIES: &[&str] = &[
    "launch.wrap",
    "hooks.receive",
    "prompt.hold",
    "auth.probe_file",
    "screen.read",
    "generate.files",
    "ui.surfaces",
    "secrets.inject",
    "prompt.keystroke",
    "usage.report",
];

/// Keys a `[keystroke]` allowlist may name (D-12): digits and letters one at
/// a time, and Escape. Never Tab, Shift+Tab or Enter on its own (they widen
/// permissions or confirm whatever is selected); Enter is sent only after a
/// free-text reply (`reply = true`).
pub fn keystroke_key_ok(k: &str) -> bool {
    k == "Escape" || (k.len() == 1 && k.bytes().all(|b| b.is_ascii_digit() || b.is_ascii_lowercase()))
}

/// Item kinds a plugin may let people answer by keys.
pub const KEYSTROKE_KINDS: &[&str] = &["question", "permission"];
/// The only keys a permission may be answered with (D-12).
pub const PERMISSION_KEYS: &[&str] = &["y", "n", "Escape"];

/// A `[links]` host pattern: a host name or `*.domain`.
pub fn host_pattern_ok(p: &str) -> bool {
    let h = p.strip_prefix("*.").unwrap_or(p);
    !h.is_empty()
        && h.len() <= 253
        && h.contains('.')
        && h.split('.').all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && l.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                && !l.starts_with('-')
                && !l.ends_with('-')
        })
}

/// Placeholders the host resolves. `{settings.launch.args...}` splats the
/// user's launch args; `{gen:<name>}` is a generated file's absolute path.
pub const PLACEHOLDERS: &[&str] = &[
    "shim",
    "agent",
    "session_id",
    "home",
    "message",
    "uuid",
    "settings.launch.args...",
    "skill.argv...",
    "hooks.argv...",
];

/// Built-in `generate` builders: `hook-settings` (a Claude-style settings
/// JSON file) and `cursor-plugin` (a Cursor `--plugin-dir` directory holding
/// `hooks/hooks.json` and, while the skill is on, the skill's rule).
pub const BUILDERS: &[&str] = &["hook-settings", "cursor-plugin"];
/// Auth probe kinds (§7.1).
pub const PROBE_KINDS: &[&str] = &["file_exists", "env_present", "file_json_age", "command"];

/// Native forms of the generated tmuxedo skill (`[skill] form`, crate::skill).
pub const SKILL_FORMS: &[&str] = &["claude-code-plugin", "agents-md", "cursor-rule"];
/// Client-side actions a `ui` surface may name besides the manifest's own.
pub const CLIENT_ACTIONS: &[&str] = &["dismiss", "settings.open"];

/// Normalised event names a hook row may `emit` (§5.3).
pub const EVENTS: &[&str] = &[
    "session.started",
    "turn.started",
    "turn.completed",
    "turn.failed",
    "attention.blocked",
    "attention.idle",
    "prompt.permission",
    "prompt.question",
    "prompt.resolved",
    "session.ended",
    "auth.state",
    "open_url",
    "usage",
    "log",
    "subagent.started",
    "subagent.completed",
];

pub const ACTION_KINDS: &[&str] = &["auth.login", "pane.run", "pane.send", "open_url", "settings.open"];

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    #[serde(default)]
    pub schema: u32,
    pub id: String,
    pub name: String,
    pub version: String,
    /// Contract major, e.g. `"1"` or `">=1.0, <2"` (only the major is read).
    #[serde(default)]
    pub contract: String,
    #[serde(default)]
    pub publisher: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub agent: AgentSection,
    #[serde(default)]
    pub env: EnvSection,
    #[serde(default)]
    pub launch: Option<LaunchSection>,
    #[serde(default)]
    pub generate: BTreeMap<String, Generate>,
    #[serde(default)]
    pub hooks: Option<HooksSection>,
    #[serde(default)]
    pub permissions: Option<Permissions>,
    #[serde(default)]
    pub resume: Option<Resume>,
    #[serde(default)]
    pub auth: Option<AuthSection>,
    #[serde(default)]
    pub actions: Vec<Action>,
    /// Agent chat (docs/design/agent-chat.md §3.2): how to drive the CLI in
    /// its structured mode. Absent = no chat; the terminal only.
    #[serde(default)]
    pub chat: Option<ChatSection>,
    /// Declarative surfaces (`[[ui]]`, plugins.md §6): sent to clients in
    /// `ui` frames; each may carry a `when` condition. Needs `ui.surfaces`.
    #[serde(default)]
    pub ui: Vec<tmuxedo_protocol::plugins::UiItem>,
    /// The generated tmuxedo skill in this CLI's native form (crate::skill).
    #[serde(default)]
    pub skill: Option<SkillSection>,
    /// Needs-you items answered by typing into the pane (D-12; crate::needs).
    /// Absent = never by keys.
    #[serde(default)]
    pub keystroke: Option<KeystrokeSection>,
    /// Where this plugin's links may go (not-yet-thought-of.md #1): an
    /// `open_url` from the agent to a listed host is verified; any other is
    /// shown as an unverified link, and only `login` hosts get the login
    /// callback relay.
    #[serde(default)]
    pub links: Option<LinksSection>,
    /// Roles and helpers (docs/design/agent-team-builder.md §7): which role
    /// fields this CLI can give a helper of its own, whose roles it can host
    /// in its process, and the CLI settings that cap helpers. Absent = the
    /// CLI has no helpers tmuxedo can define.
    #[serde(default)]
    pub multi_agent: Option<MultiAgentSection>,
    /// AI usage (docs/design/agent-usage.md §2): where this CLI reports how
    /// much of a plan's limits is used. Absent = no gauge.
    #[serde(default)]
    pub usage: Option<UsageSection>,
}

pub use super::manifest_usage::{
    UsageContextMap, UsageSection, UsageSource, UsageWindowMap, USAGE_CAPABILITY, USAGE_SOURCE_KINDS,
};

/// How a plugin's CLI runs helpers (`multi_agent.subagents`).
pub const SUBAGENT_MODES: &[&str] = &["native", "none"];
/// Role fields a CLI's own helper format may carry (`multi_agent.role_fields`).
/// A trailing `?` marks one that only some versions of the CLI carry. A field
/// that is not listed is applied to the parent pane instead, and clients say so.
pub const ROLE_FIELDS: &[&str] = &["purpose", "instructions", "model", "tools", "access", "mcp", "skills"];

/// How far a CLI's helper format carries one role field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoleFieldSupport {
    /// The helper gets it.
    Yes,
    /// Only on some versions of the CLI.
    SomeVersions,
    /// Not per helper: it applies to the whole pane.
    No,
}

/// `[multi_agent]`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MultiAgentSection {
    /// One of [`SUBAGENT_MODES`]: `native` = the CLI starts helpers in its
    /// own process from definitions tmuxedo can write; `none` = it cannot.
    pub subagents: String,
    /// [`ROLE_FIELDS`] entries, each optionally ending in `?`.
    #[serde(default)]
    pub role_fields: Vec<String>,
    /// Plugin ids whose roles this CLI can host as helpers (a role for `any`
    /// CLI is hosted by every `native` plugin). A role fixed to another CLI
    /// cannot be a helper here: it has to be a step of its own.
    #[serde(default)]
    pub hosts: Vec<String>,
    #[serde(default)]
    pub limits: MultiAgentLimits,
}

/// The CLI's own setting keys behind a role's `limits`; empty = the CLI has none.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MultiAgentLimits {
    /// The setting that caps helpers running at once (`limits.max_subagents`).
    #[serde(default)]
    pub max_concurrency: String,
    /// The setting that caps helpers of helpers (`limits.max_depth`).
    #[serde(default)]
    pub max_depth: String,
}

impl MultiAgentSection {
    pub fn native(&self) -> bool {
        self.subagents == "native"
    }

    /// How far this CLI's helper format carries `field` (one of [`ROLE_FIELDS`]).
    pub fn role_field(&self, field: &str) -> RoleFieldSupport {
        if !self.native() {
            return RoleFieldSupport::No;
        }
        for f in &self.role_fields {
            match f.strip_suffix('?') {
                Some(name) if name == field => return RoleFieldSupport::SomeVersions,
                None if f == field => return RoleFieldSupport::Yes,
                _ => {}
            }
        }
        RoleFieldSupport::No
    }

    /// Whether a role written for `cli` (a plugin id or `any`) can run as a
    /// helper inside this CLI.
    pub fn hosts_role(&self, cli: &str) -> bool {
        self.native() && (cli == tmuxedo_protocol::roles::ROLE_ANY_CLI || self.hosts.iter().any(|h| h == cli))
    }
}

/// `[keystroke]`: which items people may answer by keys, with which keys.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct KeystrokeSection {
    /// Item kinds ([`KEYSTROKE_KINDS`]).
    pub kinds: Vec<String>,
    /// The per-plugin allowlist ([`keystroke_key_ok`]); a permission only `y`/`n`/`Escape`.
    pub keys: Vec<String>,
    /// A free-text reply may be typed and sent with Enter (questions only).
    #[serde(default)]
    pub reply: bool,
    /// On until the person turns it off (questions); otherwise opt-in.
    #[serde(default)]
    pub default_on: bool,
    /// The prompt region: the last `rows` non-blank screen rows (fingerprint and options).
    #[serde(default = "default_keystroke_rows")]
    pub rows: u16,
    /// The hook signals that mean "the agent is asking a question" (the only
    /// time keys may answer): a native event, optionally one field's values.
    /// A wait read from the screen alone, or any other dialog (a permission
    /// whose hold expired), is never answered by keys.
    #[serde(default)]
    pub asks: Vec<AskSignal>,
}

/// One `[keystroke] asks` row.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AskSignal {
    pub event: String,
    #[serde(default)]
    pub field: String,
    #[serde(default)]
    pub values: Vec<String>,
}

impl KeystrokeSection {
    /// Whether a native hook payload (with the manifest's event field) is a question being asked.
    pub fn asked_by(&self, event_field: &str, native: &serde_json::Value) -> bool {
        let name = native[event_field].as_str().unwrap_or("");
        self.asks.iter().any(|a| {
            a.event == name
                && (a.field.is_empty()
                    || native[a.field.as_str()]
                        .as_str()
                        .is_some_and(|v| a.values.iter().any(|x| x == v)))
        })
    }
}

fn default_keystroke_rows() -> u16 {
    12
}

/// `[links]`: host allowlists (exact hosts or `*.domain`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LinksSection {
    #[serde(default)]
    pub open_url: Vec<String>,
    #[serde(default)]
    pub login: Vec<String>,
}

/// `[skill]`: how the generated tmuxedo skill reaches this agent.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SkillSection {
    /// One of [`SKILL_FORMS`].
    pub form: String,
    /// Launch tokens added where `launch.prepend` says `{skill.argv...}`
    /// while the skill is on (`["--plugin-dir", "{gen:skill}"]`).
    #[serde(default)]
    pub argv: Vec<String>,
    /// The oldest agent CLI version that understands `argv`; an older or
    /// unknown version launches without it.
    #[serde(default)]
    pub min_agent: String,
}

/// Chat transports and translators a manifest may name.
pub const CHAT_TRANSPORTS: &[&str] = &["stream-json", "json-rpc", "acp"];
/// Login kinds (`chat.logins`, `auth.state`'s kinds).
pub const LOGIN_KINDS: &[&str] = &["api_key", "cloud", "subscription"];
/// The plain mode names every plugin maps to its own (design §6.5).
/// `auto_approve` is the opt-in "Auto-approve" (the CLI's bypass mode): it
/// needs a fresh step-up, web's policy may forbid it, and the runner's guard
/// hook still blocks the high-risk "never" list (docs/protocol.md "Agent chat").
pub const CHAT_MODES: &[&str] = &["ask_first", "plan_first", "auto_edit", "auto_approve"];
/// The Auto-approve mode id.
pub const AUTO_APPROVE: &str = "auto_approve";
/// Chat features the clients know (a missing one hides its control).
pub const CHAT_FEATURES: &[&str] = &[
    "text_deltas",
    "tool_calls",
    "diffs",
    "permissions",
    "questions",
    "plan",
    "interrupt",
    "steer",
    "queue",
    "mode_switch",
    "model_switch",
    "images",
    "subagents",
    "usage",
    "live_terminal_attach",
];

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ChatMode {
    /// One of [`CHAT_MODES`].
    pub id: String,
    /// The CLI's own name for it (passed to `mode_flag`).
    pub native: String,
}

/// A native hook the chat child must hold for an answer (besides the
/// permission hold): `event` and, optionally, the tool it is about.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ChatHold {
    pub event: String,
    #[serde(default)]
    pub tool: String,
    /// What it becomes: `permission` | `question`.
    pub item: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ChatSection {
    /// One of [`CHAT_TRANSPORTS`].
    pub transport: String,
    /// `builtin:<id>` (a Rust translator compiled into the runner) or `acp`.
    pub translator: String,
    /// The structured-mode command. `{agent}` is the real binary (found on
    /// PATH, never the shim), `{gen:chat-settings}` the generated hook
    /// settings with the chat holds, `{settings.launch.args...}` the
    /// user's launch args.
    pub argv: Vec<String>,
    /// Appended for a new session (`{session_id}` pre-assigned).
    #[serde(default)]
    pub new_session: Vec<String>,
    /// Appended to reopen a session (`{session_id}`).
    #[serde(default)]
    pub resume: Vec<String>,
    /// The flag that takes a mode's native name (`--permission-mode`).
    #[serde(default)]
    pub mode_flag: String,
    #[serde(default)]
    pub modes: Vec<ChatMode>,
    /// Login kinds that may always use chat (D-C4). `subscription` not
    /// listed: allowed only when web's chat policy says so (admins by default).
    #[serde(default)]
    pub logins: Vec<String>,
    /// Env names whose presence (non-empty) makes the login that kind,
    /// checked in this order; none = `subscription`.
    #[serde(default)]
    pub login_env: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub features: Vec<String>,
    /// Extra hooks registered for the chat child (`hooks.register` rows).
    #[serde(default)]
    pub hooks: Vec<Register>,
    #[serde(default)]
    pub holds: Vec<ChatHold>,
    /// The format of the agent's own transcript (the file its hooks name), read
    /// to show turns taken in the terminal and to know whether the session has
    /// a conversation yet. `claude-jsonl` or empty (unknown: not read).
    #[serde(default)]
    pub transcript: String,
    /// The oldest agent CLI version that has this structured mode; an older one gets no chat
    /// (the terminal only, with the reason). Empty or an unknown version: no check.
    #[serde(default)]
    pub min_agent: String,
}

impl ChatSection {
    /// The native name of a plain mode id, if this plugin maps it.
    pub fn native_mode(&self, id: &str) -> Option<&str> {
        self.modes.iter().find(|m| m.id == id).map(|m| m.native.as_str())
    }
    pub fn has(&self, feature: &str) -> bool {
        self.features.iter().any(|f| f == feature)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AgentSection {
    /// Agent labels this plugin owns (tmuxedo-detect ids); the first is the agent `kind`.
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub binaries: Vec<String>,
    /// Run with a 5 s timeout and the scrubbed env; first word = version.
    #[serde(default)]
    pub version_cmd: Vec<String>,
    /// `builtin:<agent>` (an agent file in tmuxedo-detect) or a path.
    #[serde(default)]
    pub detect_manifest: String,
    /// Tested agent CLI range (informational).
    #[serde(default)]
    pub tested: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EnvSection {
    /// Variable names (or `PREFIX_*` globs) removed from every pane.
    #[serde(default)]
    pub scrub: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LaunchSection {
    /// PATH stand-in name; execs the next `shim` on PATH.
    pub shim: String,
    /// Tokens put before the user's own arguments.
    #[serde(default)]
    pub prepend: Vec<String>,
    /// Further names the same shim answers to (`cursor-agent` beside `agent`).
    #[serde(default)]
    pub aliases: Vec<String>,
    /// Exported by the shim before the agent runs (`NO_OPEN_BROWSER = "1"`).
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// The real binary's resolved path must contain this, or the shim runs
    /// it untouched (a generic name like `agent` may be another program).
    #[serde(default)]
    pub target_contains: String,
    /// A first argument in this list runs the real binary untouched
    /// (`login`, `status`, `--version`: subcommands that take no session flags).
    #[serde(default)]
    pub passthrough: Vec<String>,
    /// Added after `prepend` on hosted machines only (no self-update there:
    /// the image pins the agent CLI, D-21).
    #[serde(default)]
    pub hosted_prepend: Vec<String>,
    /// Files (Phase 15, docs/design/files-and-uploads.md §2): in a pane with
    /// a files folder (`TMUXEDO_FILES_DIR`), the flag that lets the CLI read it
    /// without asking and the flag that hands it the files instructions file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files: Option<LaunchFiles>,
    /// A system-prompt flag pair the CLI reads only the last of (Claude:
    /// `--append-system-prompt` and `--append-system-prompt-file`, each of
    /// which only its last copy counts). The shim joins every copy into one
    /// text flag, so tmuxedo's files note, the plugin set's instructions, a
    /// role's and the person's own all reach the model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub join_prompt: Option<JoinPrompt>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct JoinPrompt {
    /// The flag whose value is the text, e.g. `--append-system-prompt`.
    pub text: String,
    /// The flag whose value is a file holding the text, e.g. `--append-system-prompt-file`.
    pub file: String,
}

impl JoinPrompt {
    /// The shim's lines that rewrite `"$@"`: every `text` / `file` flag
    /// (also as `--flag=value`, never after `--`) becomes part of one `text`
    /// flag at the front, joined by a blank line in the order given. A file
    /// that cannot be read keeps its flag. Empty when a flag is not
    /// `--[a-z-]+` (never interpolated).
    pub fn shell_fragment(&self) -> String {
        let ok =
            |s: &str| s.len() > 2 && s.starts_with("--") && s[2..].bytes().all(|b| b.is_ascii_lowercase() || b == b'-');
        if !ok(&self.text) || !ok(&self.file) || self.text == self.file {
            return String::new();
        }
        let (t, f) = (&self.text, &self.file);
        format!(
            r#"    tmuxedo_p=; tmuxedo_w=; tmuxedo_e=; tmuxedo_n=1; tmuxedo_nl='
'
    for tmuxedo_a in "$@"; do
      [ "$tmuxedo_n" = 1 ] && set -- && tmuxedo_n=
      if [ -z "$tmuxedo_w$tmuxedo_e" ]; then
        case $tmuxedo_a in
          --) tmuxedo_e=1 ;;
          {f}) tmuxedo_w=f; continue ;;
          {t}) tmuxedo_w=t; continue ;;
          {f}=*) tmuxedo_w=f; tmuxedo_a=${{tmuxedo_a#*=}} ;;
          {t}=*) tmuxedo_w=t; tmuxedo_a=${{tmuxedo_a#*=}} ;;
        esac
      fi
      if [ "$tmuxedo_w" = f ]; then
        if tmuxedo_v=$(cat -- "$tmuxedo_a" 2>/dev/null); then tmuxedo_w=t; tmuxedo_a=$tmuxedo_v
        else set -- "$@" {f} "$tmuxedo_a"; tmuxedo_w=; continue; fi
      fi
      if [ "$tmuxedo_w" = t ]; then
        tmuxedo_p=${{tmuxedo_p:+$tmuxedo_p$tmuxedo_nl$tmuxedo_nl}}$tmuxedo_a; tmuxedo_w=; continue
      fi
      set -- "$@" "$tmuxedo_a"
    done
    [ "$tmuxedo_n" = 1 ] && set --
    case $tmuxedo_w in f) set -- "$@" {f} ;; t) set -- "$@" {t} ;; esac
    [ -n "$tmuxedo_p" ] && set -- {t} "$tmuxedo_p" "$@"
"#
        )
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LaunchFiles {
    /// e.g. `--add-dir` (Claude, Cursor).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub add_dir: Option<String>,
    /// e.g. `--append-system-prompt-file` (Claude).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

impl LaunchFiles {
    /// The shim's fragment: empty in a pane without a files folder. Flags are
    /// `--[a-z-]+` only (anything else is dropped, never interpolated).
    pub fn shell_fragment(&self) -> String {
        let ok = |f: &Option<String>| {
            f.as_deref()
                .filter(|s| {
                    s.len() > 2 && s.starts_with("--") && s[2..].bytes().all(|b| b.is_ascii_lowercase() || b == b'-')
                })
                .map(str::to_owned)
        };
        let mut words = vec![];
        if let Some(f) = ok(&self.add_dir) {
            words.push(format!("{f} \"$TMUXEDO_FILES_DIR\""));
        }
        if let Some(f) = ok(&self.instructions) {
            words.push(format!(
                "{f} \"$TMUXEDO_FILES_ROOT/{}\"",
                crate::files::INSTRUCTIONS_FILE
            ));
        }
        if words.is_empty() {
            return String::new();
        }
        format!("${{TMUXEDO_FILES_DIR:+{}}} ", words.join(" "))
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Generate {
    /// A built-in builder (`hook-settings`) …
    #[serde(default)]
    pub builder: String,
    /// … or a template file in the package.
    #[serde(default)]
    pub template: String,
    /// Output file name under the plugin's state dir.
    #[serde(default)]
    pub file: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HooksSection {
    /// `command` (the agent runs `tmuxedo-runner hook --plugin <id>`).
    pub transport: String,
    #[serde(default = "default_event_field")]
    pub event_field: String,
    #[serde(default)]
    pub session_field: String,
    #[serde(default)]
    pub cwd_field: String,
    #[serde(default)]
    pub source_field: String,
    /// Where a hook names the session's transcript file (an absolute path,
    /// kept as the pane's `session_path`; Claude: `transcript_path`).
    #[serde(default)]
    pub transcript_field: String,
    /// Native events to register with the agent (the `hook-settings` builder).
    #[serde(default)]
    pub register: Vec<Register>,
    #[serde(default)]
    pub map: Vec<HookMap>,
    /// The agent's own hook timeout when a `register` row sets none (Codex
    /// 600 s): a hold must end before it.
    #[serde(default)]
    pub default_timeout: Option<u32>,
    /// A system config file that, when it already registers this plugin's
    /// hook (`hook --plugin <id>`), makes `{hooks.argv...}` empty: managed
    /// hooks need no per-launch flags and no trust step (Codex on hosted
    /// machines, `/etc/codex/config.toml`).
    #[serde(default)]
    pub managed_file: String,
    /// `{hooks.argv...}`: the flag that carries one hook definition per
    /// registered event (`-c` for Codex: `-c 'hooks.<Event>=[…]'`).
    #[serde(default)]
    pub config_flag: String,
    /// Where a `subagent.*` row finds the helper in the native payload.
    #[serde(default)]
    pub subagent: SubagentFields,
}

/// Payload fields of the agent's sub-agent hooks (`[hooks.subagent]`). All
/// optional: a helper without an id gets one from the core, and its stop is
/// paired by name, then newest first (crate::subagents).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SubagentFields {
    /// The helper's id, the same in its start and its stop.
    #[serde(default)]
    pub id_field: String,
    /// What the agent calls the helper (its type or role name).
    #[serde(default)]
    pub name_field: String,
    /// The helper that started this one (nested helpers).
    #[serde(default)]
    pub parent_field: String,
    #[serde(default)]
    pub model_field: String,
}

fn default_event_field() -> String {
    "hook_event_name".into()
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Register {
    pub event: String,
    #[serde(default)]
    pub timeout: Option<u32>,
    #[serde(default)]
    pub matcher: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HookMap {
    pub native: String,
    pub emit: String,
    /// Field → allowed values; every entry must match.
    #[serde(default)]
    pub when: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub hold: bool,
    /// For `turn.failed`: the field carrying the error type.
    #[serde(default)]
    pub error_field: String,
    /// For `attention.*`: a fixed reason.
    #[serde(default)]
    pub reason: String,
    /// A prompt held in the pane is over (Codex `Interrupt`: Esc killed the hook).
    #[serde(default)]
    pub release: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Permissions {
    /// `hook_hold` | `keystroke` | `none`.
    pub mode: String,
    #[serde(default)]
    pub hold_secs: u64,
    #[serde(default)]
    pub prompt: PromptFields,
    #[serde(default)]
    pub reply: ReplyTemplates,
    /// Release a held prompt (no decision) as soon as anyone types into the
    /// pane: the agent shows no dialog while its hook is held (Codex), so
    /// typing means the person at the terminal wants it back.
    #[serde(default)]
    pub release_on_input: bool,
    /// `keystroke` mode: the single keys that answer the agent's own dialog.
    #[serde(default)]
    pub keys: KeyAnswers,
    /// `keystroke` mode: the dialogs that can be answered, by a lower-case
    /// needle in the screen text; the needle's line and the lines after it
    /// (`fingerprint_lines`) are the prompt's fingerprint.
    #[serde(default)]
    pub screen: Vec<PromptRule>,
    #[serde(default)]
    pub fingerprint_lines: u32,
}

/// Keys sent for a decision (plugin-cursor.md §4.1: single keys only).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct KeyAnswers {
    #[serde(default)]
    pub allow: String,
    #[serde(default)]
    pub deny: String,
}

/// Keys a keystroke answer may ever send: `y`, `n` and Esc; never Tab,
/// Shift+Tab or Enter after text (they widen permissions or submit text).
pub const ANSWER_KEYS: &[&str] = &["y", "n", "\u{1b}"];

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PromptRule {
    /// Lower-case substring of the screen text.
    pub contains: String,
    /// What the item calls it (`Bash`, `Write`, …).
    #[serde(default)]
    pub tool: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PromptFields {
    #[serde(default)]
    pub tool: String,
    #[serde(default)]
    pub input: String,
    #[serde(default)]
    pub suggestions: String,
}

/// JSON documents printed to the agent for a decision; a string leaf equal
/// to `{message}` is replaced by the decision message or dropped with its
/// key when there is none.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ReplyTemplates {
    #[serde(default)]
    pub allow: String,
    #[serde(default)]
    pub deny: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Resume {
    pub argv: Vec<String>,
    #[serde(default)]
    pub assign_session_id: Vec<String>,
    /// Start the session under the given id when it has no conversation yet
    /// (nothing to resume: Claude writes its transcript from the first turn).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub new_argv: Vec<String>,
    /// A session recorded in `$HOME` itself resumes in the runner's default
    /// working folder (`config.workDir`, `~/work`) instead. For an agent that
    /// finds its sessions by id from any folder but asks again about a
    /// folder it will not remember (Claude and `$HOME`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub leave_home: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AuthSection {
    #[serde(default)]
    pub probes: Vec<Probe>,
    #[serde(default)]
    pub screen: Vec<ScreenRule>,
    #[serde(default)]
    pub failure_errors: Vec<String>,
    #[serde(default)]
    pub recover_on: Vec<String>,
    /// Reason reported when no probe applies on this OS.
    #[serde(default)]
    pub unprobed_reason: String,
    #[serde(default)]
    pub login: Option<Login>,
    /// Login links (and codes) lifted from a pane's screen to the user's
    /// device: the CLI prints them instead of opening a browser.
    #[serde(default)]
    pub lift: Vec<LiftRule>,
}

/// A login link on screen → an `open_url` event (with the code, if any).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LiftRule {
    pub id: String,
    /// Regex for the URL (the whole match; http(s) only).
    pub url: String,
    /// Regex for a one-time code shown with it (capture group 1, or the match).
    #[serde(default)]
    pub code: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Probe {
    /// One of [`PROBE_KINDS`].
    pub kind: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub name: String,
    /// `file_json_age`: the JSON field holding an RFC 3339 time (only the
    /// age leaves the runner); older than `warn_days` = `expiring`.
    #[serde(default)]
    pub field: String,
    #[serde(default)]
    pub warn_days: u32,
    /// `command`: run (scrubbed env, 10 s timeout) every `interval_secs`;
    /// logged in = exit 0, or `json_field` true in its JSON stdout.
    #[serde(default)]
    pub argv: Vec<String>,
    #[serde(default)]
    pub json_field: String,
    #[serde(default)]
    pub interval_secs: u64,
    /// OS names (`linux`, `darwin`) the probe applies to; empty = all.
    #[serde(default)]
    pub os: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ScreenRule {
    pub id: String,
    /// Case-insensitive substring of the detection text (runs of white space,
    /// line breaks included, match one space: the text may wrap).
    pub contains: String,
    /// `expired` (default) or `failed` (the login is unusable, e.g. a
    /// refresh token reused on another machine).
    #[serde(default)]
    pub state: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Login {
    /// `in_pane` | `device_code` | `localhost_callback` | `api_key`.
    pub kind: String,
    #[serde(default)]
    pub argv: Vec<String>,
    #[serde(default)]
    pub label: String,
}

/// A user-invokable action (§6.2); `plugin.action.*` lists and runs them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Action {
    pub id: String,
    pub title: String,
    /// One of [`ACTION_KINDS`].
    pub kind: String,
    #[serde(default)]
    pub argv: Vec<String>,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub description: String,
    /// Contexts: global, workspace, tab, pane, selection.
    #[serde(default)]
    pub contexts: Vec<String>,
}

impl Manifest {
    pub fn parse(text: &str) -> Result<Manifest, String> {
        toml::from_str(text).map_err(|e| format!("plugin.toml: {}", e.message()))
    }

    /// The contract major the plugin targets (`"1"`, `">=1.0, <2"`, `"1.2"`).
    pub fn contract_major(&self) -> Option<u32> {
        let s = self.contract.trim().trim_start_matches(['>', '=', '^', '~']).trim();
        s.split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()
    }

    pub fn has(&self, capability: &str) -> bool {
        self.capabilities.iter().any(|c| c == capability)
    }

    pub fn agent_label(&self) -> &str {
        self.agent.labels.first().map(String::as_str).unwrap_or(&self.id)
    }

    /// Lint (§11 step 1): schema, placeholders, capabilities, limits.
    /// Returns every problem, so a report shows them all at once.
    pub fn lint(&self) -> Vec<String> {
        let mut e = vec![];
        let ok_id = self.id.len() >= 2
            && self.id.len() <= 32
            && self
                .id
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        if !ok_id {
            e.push(format!("id {:?} must match [a-z0-9-]{{2,32}}", self.id));
        }
        if self.schema != 1 {
            e.push(format!("schema {} is not 1", self.schema));
        }
        if self.name.trim().is_empty() {
            e.push("name is required".into());
        }
        if !semver_ok(&self.version) {
            e.push(format!("version {:?} is not semver", self.version));
        }
        match self.contract_major() {
            Some(CONTRACT_MAJOR) => {}
            Some(m) => e.push(format!("contract major {m} is not {CONTRACT_MAJOR}")),
            None => e.push(format!("contract {:?} names no major version", self.contract)),
        }
        for c in &self.capabilities {
            if !CAPABILITIES.contains(&c.as_str()) {
                e.push(format!("unknown capability {c:?}"));
            }
        }
        if self.agent.labels.is_empty() {
            e.push("agent.labels needs at least one agent label".into());
        }
        for l in &self.agent.labels {
            if tmuxedo_detect::Agent::lookup(l).is_none() {
                e.push(format!("agent label {l:?} is not a supported agent"));
            }
        }
        if let Some(v) = self.agent.detect_manifest.strip_prefix("builtin:") {
            if tmuxedo_detect::Agent::lookup(v).is_none() {
                e.push(format!("detect_manifest builtin:{v} names no built-in agent file"));
            }
        }
        for s in &self.env.scrub {
            let base = s.strip_suffix('*').unwrap_or(s);
            if base.is_empty()
                || !base
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
            {
                e.push(format!("env.scrub entry {s:?} is not an env name or PREFIX_*"));
            }
        }
        // Launch.
        if let Some(l) = &self.launch {
            if !self.has("launch.wrap") {
                e.push("launch needs capability launch.wrap".into());
            }
            if !self.agent.binaries.contains(&l.shim) {
                e.push(format!("launch.shim {:?} is not in agent.binaries", l.shim));
            }
            for t in l.prepend.iter().chain(&l.hosted_prepend) {
                self.check_placeholders("launch.prepend", t, &mut e);
            }
            for a in &l.aliases {
                if !self.agent.binaries.contains(a) {
                    e.push(format!("launch.aliases {a:?} is not in agent.binaries"));
                }
            }
            for k in l.env.keys() {
                if k.is_empty()
                    || !k
                        .chars()
                        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
                {
                    e.push(format!("launch.env name {k:?} is not an env name"));
                }
            }
            for v in l.env.values() {
                if v.chars().any(char::is_control) {
                    e.push("launch.env values must be plain text".into());
                }
            }
            for a in &l.passthrough {
                if a.is_empty() || a.chars().any(|c| c.is_whitespace() || "'\"$`\\".contains(c)) {
                    e.push(format!("launch.passthrough {a:?} must be a plain word"));
                }
            }
            if l.target_contains
                .chars()
                .any(|c| c.is_whitespace() || "'\"$`\\*?[".contains(c))
            {
                e.push("launch.target_contains must be a plain path fragment".into());
            }
            if l.prepend.iter().any(|t| t == "{hooks.argv...}")
                && self
                    .hooks
                    .as_ref()
                    .is_none_or(|h| h.config_flag.is_empty() || h.register.is_empty())
            {
                e.push("{hooks.argv...} needs hooks.config_flag and hooks.register".into());
            }
        }
        if !self.generate.is_empty() && !self.has("generate.files") {
            e.push("generate needs capability generate.files".into());
        }
        for (name, g) in &self.generate {
            if g.builder.is_empty() == g.template.is_empty() {
                e.push(format!("generate.{name}: exactly one of builder or template"));
            }
            if !g.builder.is_empty() && !BUILDERS.contains(&g.builder.as_str()) {
                e.push(format!("generate.{name}: unknown builder {:?}", g.builder));
            }
            if !g.builder.is_empty() && self.hooks.as_ref().is_none_or(|h| h.register.is_empty()) {
                e.push(format!("generate.{name}: {} needs hooks.register", g.builder));
            }
            if g.file.contains('/') || g.file.starts_with('.') {
                e.push(format!("generate.{name}.file {:?} must be a plain file name", g.file));
            }
        }
        // Hooks.
        if let Some(h) = &self.hooks {
            if !self.has("hooks.receive") {
                e.push("hooks needs capability hooks.receive".into());
            }
            if h.transport != "command" {
                e.push(format!("hooks.transport {:?} is not supported (command)", h.transport));
            }
            if h.event_field.is_empty() {
                e.push("hooks.event_field is required".into());
            }
            let registered: Vec<&str> = h.register.iter().map(|r| r.event.as_str()).collect();
            for (i, m) in h.map.iter().enumerate() {
                if !EVENTS.contains(&m.emit.as_str()) {
                    e.push(format!("hooks.map[{i}] emits unknown event {:?}", m.emit));
                }
                if !h.register.is_empty() && !registered.contains(&m.native.as_str()) {
                    e.push(format!("hooks.map[{i}] native {:?} is not in hooks.register", m.native));
                }
                if m.release && m.hold {
                    e.push(format!("hooks.map[{i}]: a row cannot both hold and release"));
                }
                if m.hold && m.emit != "prompt.permission" {
                    e.push(format!("hooks.map[{i}]: only prompt.permission can hold"));
                }
                if m.hold && !self.has("prompt.hold") {
                    e.push(format!("hooks.map[{i}] holds but capability prompt.hold is missing"));
                }
                if m.emit == "turn.failed" && m.error_field.is_empty() {
                    e.push(format!("hooks.map[{i}]: turn.failed needs error_field"));
                }
            }
            let has_hold = h.map.iter().any(|m| m.hold);
            if has_hold && self.permissions.as_ref().is_none_or(|p| p.mode != "hook_hold") {
                e.push("a hold row needs permissions.mode = \"hook_hold\"".into());
            }
            if let Some(p) = self.permissions.as_ref().filter(|p| p.mode == "hook_hold") {
                for row in h.map.iter().filter(|m| m.hold) {
                    let timeout = h
                        .register
                        .iter()
                        .find(|r| r.event == row.native)
                        .and_then(|r| r.timeout)
                        .or(h.default_timeout);
                    match timeout {
                        Some(t) if u64::from(t) > p.hold_secs => {}
                        Some(t) => e.push(format!(
                            "permissions.hold_secs {} must be below the {} hook timeout {t}",
                            p.hold_secs, row.native
                        )),
                        None => e.push(format!("held hook {} needs a timeout in hooks.register", row.native)),
                    }
                }
            }
        }
        if let Some(c) = &self.chat {
            if !CHAT_TRANSPORTS.contains(&c.transport.as_str()) {
                e.push(format!("chat.transport {:?} is unknown", c.transport));
            }
            if c.translator != "acp" && c.translator != format!("builtin:{}", self.id) {
                e.push(format!(
                    "chat.translator {:?} must be acp or builtin:{}",
                    c.translator, self.id
                ));
            }
            // The generic translator reads ACP only, and ACP is read by it only.
            if (c.transport == "acp") != (c.translator == "acp") {
                e.push("chat.transport acp goes with chat.translator acp, and only with it".into());
            }
            if c.argv.first().map(String::as_str) != Some("{agent}") {
                e.push("chat.argv must start with {agent}".into());
            }
            for t in c.argv.iter().chain(&c.new_session).chain(&c.resume) {
                if t != "{gen:chat-settings}" {
                    self.check_placeholders("chat.argv", t, &mut e);
                }
            }
            for l in c.logins.iter().chain(c.login_env.keys()) {
                if !LOGIN_KINDS.contains(&l.as_str()) {
                    e.push(format!("chat login kind {l:?} is unknown"));
                }
            }
            for m in &c.modes {
                if !CHAT_MODES.contains(&m.id.as_str()) || m.native.is_empty() {
                    e.push(format!(
                        "chat.modes entry {:?} is not a known mode with a native name",
                        m.id
                    ));
                }
            }
            if !c.min_agent.is_empty() && !semver_ok(&c.min_agent) {
                e.push(format!("chat.min_agent {:?} is not semver", c.min_agent));
            }
            // A json-rpc transport passes the native mode in its own requests, not as a flag.
            if !c.modes.is_empty() && c.mode_flag.is_empty() && c.transport == "stream-json" {
                e.push("chat.modes needs chat.mode_flag".into());
            }
            for f in &c.features {
                if !CHAT_FEATURES.contains(&f.as_str()) {
                    e.push(format!("chat feature {f:?} is unknown"));
                }
            }
            for h in &c.holds {
                if !["permission", "question"].contains(&h.item.as_str()) {
                    e.push(format!("chat.holds item {:?} is not permission or question", h.item));
                }
            }
        }
        if let Some(p) = &self.permissions {
            if !["hook_hold", "keystroke", "none"].contains(&p.mode.as_str()) {
                e.push(format!("permissions.mode {:?} is unknown", p.mode));
            }
            if p.mode == "keystroke" {
                if p.screen.is_empty() {
                    e.push("permissions.screen is required for keystroke".into());
                }
                for r in &p.screen {
                    if r.contains.is_empty() || r.contains != r.contains.to_lowercase() {
                        e.push(format!(
                            "permissions.screen {:?} must be a lower-case needle",
                            r.contains
                        ));
                    }
                }
                for (which, k) in [("allow", &p.keys.allow), ("deny", &p.keys.deny)] {
                    let shown = k.escape_default().to_string();
                    if !ANSWER_KEYS.contains(&k.as_str()) {
                        e.push(format!(
                            "permissions.keys.{which} {shown:?} is not an allowed answer key (y, n, Esc)"
                        ));
                    }
                }
                if !self.has("screen.read") {
                    e.push("keystroke answers need capability screen.read".into());
                }
            }
            if p.release_on_input && p.mode != "hook_hold" {
                e.push("permissions.release_on_input needs mode hook_hold".into());
            }
            if p.mode == "hook_hold" {
                if p.hold_secs == 0 {
                    e.push("permissions.hold_secs is required for hook_hold".into());
                }
                if p.prompt.tool.is_empty() || p.prompt.input.is_empty() {
                    e.push("permissions.prompt needs tool and input fields".into());
                }
                for (which, t) in [("allow", &p.reply.allow), ("deny", &p.reply.deny)] {
                    match serde_json::from_str::<Value>(t) {
                        Ok(Value::Object(_)) => {}
                        _ => e.push(format!("permissions.reply.{which} is not a JSON object")),
                    }
                    self.check_placeholders(&format!("permissions.reply.{which}"), t, &mut e);
                }
            }
        }
        if let Some(r) = &self.resume {
            if r.argv.is_empty() {
                e.push("resume.argv is empty".into());
            }
            for t in r.argv.iter().chain(&r.assign_session_id).chain(&r.new_argv) {
                self.check_placeholders("resume", t, &mut e);
            }
        }
        if let Some(a) = &self.auth {
            for (i, p) in a.probes.iter().enumerate() {
                match p.kind.as_str() {
                    "file_exists" => {
                        if p.path.is_empty() {
                            e.push(format!("auth.probes[{i}] file_exists needs path"));
                        }
                        if !self.has("auth.probe_file") {
                            e.push(format!("auth.probes[{i}] needs capability auth.probe_file"));
                        }
                        self.check_placeholders("auth.probes", &p.path, &mut e);
                    }
                    "env_present" => {
                        if p.name.is_empty() {
                            e.push(format!("auth.probes[{i}] env_present needs name"));
                        }
                    }
                    "file_json_age" => {
                        if p.path.is_empty() || p.field.is_empty() || p.warn_days == 0 {
                            e.push(format!(
                                "auth.probes[{i}] file_json_age needs path, field and warn_days"
                            ));
                        }
                        if !self.has("auth.probe_file") {
                            e.push(format!("auth.probes[{i}] needs capability auth.probe_file"));
                        }
                        self.check_placeholders("auth.probes", &p.path, &mut e);
                    }
                    "command" => {
                        if p.argv.is_empty() || !self.agent.binaries.contains(&p.argv[0]) {
                            e.push(format!("auth.probes[{i}] command must run one of agent.binaries"));
                        }
                        if p.interval_secs < 60 {
                            e.push(format!("auth.probes[{i}] command needs interval_secs >= 60"));
                        }
                    }
                    k => e.push(format!("auth.probes[{i}] kind {k:?} is unknown")),
                }
                for os in &p.os {
                    if !["linux", "darwin"].contains(&os.as_str()) {
                        e.push(format!("auth.probes[{i}] os {os:?} is unknown"));
                    }
                }
            }
            if !a.screen.is_empty() && !self.has("screen.read") {
                e.push("auth.screen needs capability screen.read".into());
            }
            for (i, s) in a.screen.iter().enumerate() {
                if s.id.is_empty() || s.contains.is_empty() {
                    e.push(format!("auth.screen[{i}] needs id and contains"));
                }
                if s.contains != s.contains.to_lowercase() {
                    e.push(format!(
                        "auth.screen[{i}].contains must be lower case (matched case-insensitively)"
                    ));
                }
                if !["", "expired", "failed"].contains(&s.state.as_str()) {
                    e.push(format!("auth.screen[{i}].state {:?} is not expired or failed", s.state));
                }
            }
            for (i, l) in a.lift.iter().enumerate() {
                if l.id.is_empty() || regex::Regex::new(&l.url).is_err() {
                    e.push(format!("auth.lift[{i}] needs id and a valid url regex"));
                }
                if !l.code.is_empty() && regex::Regex::new(&l.code).is_err() {
                    e.push(format!("auth.lift[{i}].code is not a valid regex"));
                }
                if !self.has("screen.read") {
                    e.push(format!("auth.lift[{i}] needs capability screen.read"));
                }
            }
            for r in &a.recover_on {
                if r != "probe_changed" && !EVENTS.contains(&r.as_str()) {
                    e.push(format!("auth.recover_on {r:?} is not an event or probe_changed"));
                }
            }
            if let Some(l) = &a.login {
                if !["in_pane", "device_code", "localhost_callback", "api_key"].contains(&l.kind.as_str()) {
                    e.push(format!("auth.login.kind {:?} is unknown", l.kind));
                }
                for t in &l.argv {
                    self.check_placeholders("auth.login.argv", t, &mut e);
                }
            }
        }
        let mut seen = std::collections::BTreeSet::new();
        for a in &self.actions {
            if !seen.insert(a.id.as_str()) {
                e.push(format!("duplicate action id {:?}", a.id));
            }
            if a.id.is_empty() || a.title.is_empty() {
                e.push("every action needs id and title".into());
            }
            if a.title.chars().count() > 80 {
                e.push(format!("action {} title is longer than 80 characters", a.id));
            }
            match a.kind.as_str() {
                "auth.login" => {
                    if self.auth.as_ref().and_then(|a| a.login.as_ref()).is_none() {
                        e.push(format!("action {} is auth.login but auth.login is not declared", a.id));
                    }
                }
                "pane.run" => {
                    if a.argv.is_empty() {
                        e.push(format!("action {} (pane.run) needs argv", a.id));
                    }
                }
                "pane.send" => {
                    if a.text.is_empty() {
                        e.push(format!("action {} (pane.send) needs text", a.id));
                    }
                }
                "open_url" => {
                    if !crate::open_url::valid_url(&a.url) {
                        e.push(format!("action {} (open_url) needs an http(s) url", a.id));
                    }
                }
                "settings.open" => {}
                k => e.push(format!("action {} kind {k:?} is unknown", a.id)),
            }
            for t in &a.argv {
                self.check_placeholders(&format!("action {}", a.id), t, &mut e);
            }
            for c in &a.contexts {
                if !["global", "workspace", "tab", "pane", "selection"].contains(&c.as_str()) {
                    e.push(format!("action {} context {c:?} is unknown", a.id));
                }
            }
        }
        self.lint_ui(&mut e);
        self.lint_skill(&mut e);
        self.lint_needs(&mut e);
        self.lint_multi_agent(&mut e);
        self.lint_usage(&mut e);
        e
    }

    fn lint_ui(&self, e: &mut Vec<String>) {
        if self.ui.is_empty() {
            return;
        }
        if !self.has("ui.surfaces") {
            e.push("ui surfaces need capability ui.surfaces".into());
        }
        if self.ui.len() > tmuxedo_protocol::plugins::UI_MAX_ITEMS {
            e.push(format!("at most {} ui items", tmuxedo_protocol::plugins::UI_MAX_ITEMS));
        }
        let mut seen = std::collections::BTreeSet::new();
        for it in &self.ui {
            if !seen.insert(it.key.as_str()) {
                e.push(format!("duplicate ui key {:?}", it.key));
            }
            if let Some(p) = it.problem() {
                e.push(format!("ui {}: {p}", it.key));
            }
            let mut names: Vec<&str> = it.actions.iter().flatten().map(|a| a.action.as_str()).collect();
            match (it.kind.as_str(), &it.action) {
                ("quick_action", Some(Value::String(a))) => names.push(a),
                ("toast", Some(a)) => names.extend(a["action"].as_str()),
                _ => {}
            }
            for n in names {
                if !CLIENT_ACTIONS.contains(&n) && !self.actions.iter().any(|a| a.id == n) {
                    e.push(format!("ui {}: action {n:?} is not declared in [[actions]]", it.key));
                }
            }
        }
    }

    /// `[multi_agent]` (PLAN Phase 18).
    fn lint_multi_agent(&self, e: &mut Vec<String>) {
        let Some(m) = &self.multi_agent else { return };
        if !SUBAGENT_MODES.contains(&m.subagents.as_str()) {
            e.push(format!(
                "multi_agent.subagents {:?} is not one of {SUBAGENT_MODES:?}",
                m.subagents
            ));
        }
        let mut seen: Vec<&str> = Vec::new();
        for f in &m.role_fields {
            let name = f.strip_suffix('?').unwrap_or(f);
            if !ROLE_FIELDS.contains(&name) {
                e.push(format!("multi_agent.role_fields {f:?} is not one of {ROLE_FIELDS:?}"));
            } else if seen.contains(&name) {
                e.push(format!("multi_agent.role_fields lists {name:?} twice"));
            }
            seen.push(name);
        }
        for (i, h) in m.hosts.iter().enumerate() {
            if !tmuxedo_protocol::roles::role_id_ok(h) || h == tmuxedo_protocol::roles::ROLE_ANY_CLI {
                e.push(format!("multi_agent.hosts {h:?} is not a plugin id"));
            } else if m.hosts[..i].contains(h) {
                e.push(format!("multi_agent.hosts lists {h:?} twice"));
            }
        }
        if m.subagents == "native" && !m.hosts.contains(&self.id) {
            e.push(format!(
                "multi_agent.hosts must name this plugin ({:?}): a CLI with helpers hosts its own roles",
                self.id
            ));
        }
        if m.subagents == "none"
            && !(m.role_fields.is_empty() && m.hosts.is_empty() && m.limits == MultiAgentLimits::default())
        {
            e.push("multi_agent.subagents = \"none\" takes no role_fields, hosts or limits".into());
        }
        for (which, key) in [
            ("max_concurrency", &m.limits.max_concurrency),
            ("max_depth", &m.limits.max_depth),
        ] {
            let ok = key.len() <= 128
                && key
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
            if !ok {
                e.push(format!("multi_agent.limits.{which} {key:?} is not a setting key"));
            }
        }
    }

    /// `[keystroke]` and `[links]` (PLAN Phase 13).
    fn lint_needs(&self, e: &mut Vec<String>) {
        if let Some(k) = &self.keystroke {
            if !self.has("prompt.keystroke") {
                e.push("[keystroke] needs capability prompt.keystroke".into());
            }
            if !self.has("screen.read") {
                e.push("[keystroke] needs capability screen.read (the screen fingerprint)".into());
            }
            if k.kinds.is_empty() {
                e.push("keystroke.kinds is empty".into());
            }
            for kind in &k.kinds {
                if !KEYSTROKE_KINDS.contains(&kind.as_str()) {
                    e.push(format!("keystroke.kinds {kind:?} is not one of {KEYSTROKE_KINDS:?}"));
                }
            }
            for key in &k.keys {
                if !keystroke_key_ok(key) {
                    e.push(format!(
                        "keystroke.keys {key:?} is not allowed (single digits or letters, Escape)"
                    ));
                }
            }
            let mut seen = std::collections::BTreeSet::new();
            for key in &k.keys {
                if !seen.insert(key) {
                    e.push(format!("keystroke.keys lists {key:?} twice"));
                }
            }
            if k.kinds.iter().any(|x| x == "permission") {
                if let Some(bad) = k.keys.iter().find(|x| !PERMISSION_KEYS.contains(&x.as_str())) {
                    e.push(format!(
                        "keystroke.keys {bad:?}: a permission is answered with y, n or Escape only"
                    ));
                }
                if k.reply {
                    e.push("keystroke.reply: a permission is never answered with typed text".into());
                }
                if k.default_on {
                    e.push("keystroke.default_on: answering permissions by keys is always opt-in".into());
                }
            }
            if !(1..=40).contains(&k.rows) {
                e.push(format!("keystroke.rows {} is not 1..40", k.rows));
            }
            if k.asks.is_empty() {
                e.push("keystroke.asks is empty: keys answer only a question a hook says is being asked".into());
            }
            let registered = |ev: &str| {
                self.hooks
                    .as_ref()
                    .is_some_and(|h| h.register.iter().any(|r| r.event == ev))
            };
            for a in &k.asks {
                if !registered(&a.event) {
                    e.push(format!("keystroke.asks event {:?} is not in hooks.register", a.event));
                }
                if a.field.is_empty() != a.values.is_empty() {
                    e.push(format!("keystroke.asks {:?}: field and values go together", a.event));
                }
            }
        }
        if let Some(l) = &self.links {
            for (which, list) in [("open_url", &l.open_url), ("login", &l.login)] {
                for h in list {
                    if !host_pattern_ok(h) {
                        e.push(format!("links.{which} {h:?} is not a host name or *.domain"));
                    }
                }
            }
            for h in &l.login {
                if !tmuxedo_protocol::needs::host_allowed(h.trim_start_matches("*."), &l.open_url)
                    && !l.open_url.contains(h)
                {
                    e.push(format!("links.login {h:?} is not in links.open_url"));
                }
            }
        }
    }

    fn lint_skill(&self, e: &mut Vec<String>) {
        let uses = self
            .launch
            .as_ref()
            .is_some_and(|l| l.prepend.iter().any(|t| t == "{skill.argv...}"));
        let Some(s) = &self.skill else {
            if uses {
                e.push("launch.prepend uses {skill.argv...} but [skill] is not declared".into());
            }
            return;
        };
        if !SKILL_FORMS.contains(&s.form.as_str()) {
            e.push(format!("skill.form {:?} is not one of {SKILL_FORMS:?}", s.form));
        }
        if !self.has("generate.files") {
            e.push("[skill] needs capability generate.files".into());
        }
        if !s.argv.is_empty() && !uses {
            e.push("skill.argv is set but launch.prepend has no {skill.argv...}".into());
        }
        if !s.argv.is_empty() && !semver_ok(&s.min_agent) {
            e.push(format!(
                "skill.min_agent {:?} is not semver (needed with skill.argv)",
                s.min_agent
            ));
        }
        for t in &s.argv {
            if t != "{gen:skill}" && !t.contains("{skill.toml:") {
                self.check_placeholders("skill.argv", t, e);
            }
        }
    }

    fn check_placeholders(&self, where_: &str, text: &str, e: &mut Vec<String>) {
        for ph in placeholders(text) {
            let known =
                PLACEHOLDERS.contains(&ph) || ph.strip_prefix("gen:").is_some_and(|g| self.generate.contains_key(g));
            if !known {
                e.push(format!("{where_}: unresolvable placeholder {{{ph}}}"));
            }
        }
    }
}

/// `{name}` tokens in a manifest string.
pub fn placeholders(text: &str) -> Vec<&str> {
    // JSON braces (`{"a":…}`) never match: a placeholder is `{word…}`.
    static RE: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"\{([A-Za-z0-9_][A-Za-z0-9_.:-]*)\}").expect("valid regex"));
    RE.captures_iter(text)
        .filter_map(|c| c.get(1))
        .map(|m| m.as_str())
        .collect()
}

fn semver_ok(v: &str) -> bool {
    let parts: Vec<&str> = v.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
schema = 1
id = "demo"
name = "Demo"
version = "0.1.0"
contract = "1"
[agent]
labels = ["codex"]
"#;

    #[test]
    fn minimal_manifest_lints_clean() {
        let m = Manifest::parse(MINIMAL).unwrap();
        assert_eq!(m.lint(), Vec::<String>::new());
        assert_eq!(m.agent_label(), "codex");
        assert_eq!(m.contract_major(), Some(1));
        assert!(Manifest::parse("schema = 1\nid = 'x'")
            .unwrap_err()
            .contains("plugin.toml"));
    }

    #[test]
    fn lint_reports_every_problem() {
        let text = MINIMAL
            .replace("id = \"demo\"", "id = \"Bad!\"")
            .replace("contract = \"1\"", "contract = \"2\"\ncapabilities = [\"flying\"]")
            + r#"
[launch]
shim = "codex"
prepend = ["{nope}", "--settings", "{gen:missing}"]
[hooks]
transport = "socket"
map = [{ native = "X", emit = "nothing", hold = true }]
[[actions]]
id = "a"
title = "A"
kind = "pane.run"
"#;
        let m = Manifest::parse(&text).unwrap();
        let e = m.lint().join("\n");
        for want in [
            "id \"Bad!\"",
            "contract major 2",
            "unknown capability \"flying\"",
            "launch needs capability launch.wrap",
            "launch.shim \"codex\" is not in agent.binaries",
            "unresolvable placeholder {nope}",
            "unresolvable placeholder {gen:missing}",
            "hooks needs capability hooks.receive",
            "hooks.transport \"socket\"",
            "emits unknown event \"nothing\"",
            "only prompt.permission can hold",
            "capability prompt.hold is missing",
            "permissions.mode = \"hook_hold\"",
            "action a (pane.run) needs argv",
        ] {
            assert!(e.contains(want), "missing {want:?} in:\n{e}");
        }
    }

    #[test]
    fn multi_agent_says_what_a_helper_can_carry() {
        let text = MINIMAL.to_string()
            + r#"
[multi_agent]
subagents = "native"
role_fields = ["purpose", "instructions", "model", "mcp?"]
hosts = ["demo"]
limits = { max_concurrency = "agents.max_threads", max_depth = "agents.max_depth" }
"#;
        let m = Manifest::parse(&text).unwrap();
        assert_eq!(m.lint(), Vec::<String>::new());
        let ma = m.multi_agent.as_ref().unwrap();
        assert_eq!(ma.role_field("purpose"), RoleFieldSupport::Yes);
        assert_eq!(ma.role_field("mcp"), RoleFieldSupport::SomeVersions);
        assert_eq!(ma.role_field("skills"), RoleFieldSupport::No);
        assert!(ma.hosts_role("demo") && ma.hosts_role("any"));
        assert!(!ma.hosts_role("other"));
        assert_eq!(ma.limits.max_depth, "agents.max_depth");

        // No section: nothing to say, and nothing to lint.
        assert!(Manifest::parse(MINIMAL).unwrap().multi_agent.is_none());

        // A CLI without helpers hosts nobody and carries nothing.
        let none = Manifest::parse(&(MINIMAL.to_string() + "[multi_agent]\nsubagents = \"none\"\n")).unwrap();
        assert_eq!(none.lint(), Vec::<String>::new());
        let ma = none.multi_agent.as_ref().unwrap();
        assert!(!ma.hosts_role("any") && !ma.hosts_role("demo"));
        assert_eq!(ma.role_field("purpose"), RoleFieldSupport::No);
    }

    #[test]
    fn multi_agent_lint_reports_every_problem() {
        let text = MINIMAL.to_string()
            + r#"
[multi_agent]
subagents = "sometimes"
role_fields = ["purpose", "purpose?", "secrets"]
hosts = ["Other!", "any", "codex", "codex"]
limits = { max_concurrency = "has space" }
"#;
        let e = Manifest::parse(&text).unwrap().lint().join("\n");
        for want in [
            "multi_agent.subagents \"sometimes\"",
            "role_fields lists \"purpose\" twice",
            "role_fields \"secrets\" is not one of",
            "hosts \"Other!\" is not a plugin id",
            "hosts \"any\" is not a plugin id",
            "hosts lists \"codex\" twice",
            "limits.max_concurrency \"has space\"",
        ] {
            assert!(e.contains(want), "missing {want:?} in:\n{e}");
        }
        let native = MINIMAL.to_string() + "[multi_agent]\nsubagents = \"native\"\nhosts = [\"codex\"]\n";
        let e = Manifest::parse(&native).unwrap().lint().join("\n");
        assert!(e.contains("multi_agent.hosts must name this plugin (\"demo\")"), "{e}");
        let none = MINIMAL.to_string() + "[multi_agent]\nsubagents = \"none\"\nhosts = [\"demo\"]\n";
        let e = Manifest::parse(&none).unwrap().lint().join("\n");
        assert!(e.contains("takes no role_fields, hosts or limits"), "{e}");
    }

    #[test]
    fn placeholder_scan() {
        assert_eq!(
            placeholders("{shim} --x {gen:hook-settings} '{\"a\":1}' {settings.launch.args...}"),
            ["shim", "gen:hook-settings", "settings.launch.args..."]
        );
        assert!(placeholders(r#"{"hookSpecificOutput":{"decision":{"behavior":"allow"}}}"#).is_empty());
        assert_eq!(placeholders(r#"{"m":"{message}"}"#), ["message"]);
    }

    #[test]
    fn contract_majors() {
        for (s, want) in [
            (">=1.0, <2", Some(1)),
            ("1", Some(1)),
            ("^1.2", Some(1)),
            ("2.0", Some(2)),
            ("", None),
        ] {
            let m = Manifest {
                contract: s.into(),
                ..Manifest::default()
            };
            assert_eq!(m.contract_major(), want, "{s}");
        }
    }

    /// The shim's prompt join, run by /bin/sh: every copy of either flag
    /// (also `--flag=value`) becomes one text flag at the front, in order; a
    /// file that cannot be read keeps its flag; nothing after `--` is touched.
    /// Found by the Phase 17 gate run: Claude Code reads only the last of each.
    #[test]
    fn the_prompt_join_keeps_every_copy_in_order() {
        let j = JoinPrompt {
            text: "--append-system-prompt".into(),
            file: "--append-system-prompt-file".into(),
        };
        let dir = std::env::temp_dir().join(format!("tmuxedo-join-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.md"), "FILE A\n").unwrap();
        std::fs::write(dir.join("b.md"), "FILE 'B' $HOME\n\n").unwrap();
        let script = format!("{}for x in \"$@\"; do printf '<%s>' \"$x\"; done\n", j.shell_fragment());
        let run = |args: &[&str]| {
            let out = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(&script)
                .arg("sh")
                .args(args)
                .current_dir(&dir)
                .output()
                .unwrap();
            String::from_utf8(out.stdout).unwrap()
        };
        assert_eq!(
            run(&[
                "--append-system-prompt-file",
                "a.md",
                "--settings",
                "s",
                "--append-system-prompt",
                "in line",
                "-p",
                "hi there",
                "--append-system-prompt-file=b.md"
            ]),
            "<--append-system-prompt><FILE A\n\nin line\n\nFILE 'B' $HOME><--settings><s><-p><hi there>"
        );
        assert_eq!(
            run(&[
                "--append-system-prompt-file",
                "missing.md",
                "--",
                "--append-system-prompt",
                "x"
            ]),
            "<--append-system-prompt-file><missing.md><--><--append-system-prompt><x>"
        );
        assert_eq!(run(&[]), "");
        assert_eq!(run(&["", "a b"]), "<><a b>");
        assert_eq!(run(&["-p", "--append-system-prompt"]), "<-p><--append-system-prompt>");
        // One flag alone is still handed over as text: same prompt, one flag.
        assert_eq!(run(&["--append-system-prompt", "x"]), "<--append-system-prompt><x>");
        let _ = std::fs::remove_dir_all(&dir);
        // A flag name that could break out of the script is never used.
        let bad = JoinPrompt {
            text: "--x;id".into(),
            file: "--y".into(),
        };
        assert_eq!(bad.shell_fragment(), "");
    }
}
