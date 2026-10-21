//! Add-ons (docs/design/plugins.md §8.1): a curated, data-only bundle that
//! wires a service into the agents tmuxedo launches: environment variables,
//! hooks, a chained status line, Codex config keys and a few packaged files.
//! An add-on travels as an item of kind `addon` in `config.pluginSet`.
//!
//! An add-on never runs an installer and never writes the agent's own
//! configuration; the runner generates everything per launch. The
//! TypeScript side is `packages/protocol/src/addon.ts`; both check
//! `packages/protocol/fixtures/addons.json`.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

pub const KIND: &str = "addon";
pub const AGENTS: [&str; 3] = ["claude", "codex", "cursor"];
pub const MAX_BYTES: usize = 256 * 1024;
/// Every add-on of one plugin set together, serialised (web's
/// `MAX_ADDONS_TOTAL_BYTES`): what is past it is dropped, in item order.
pub const MAX_TOTAL_BYTES: usize = 384 * 1024;
pub const MAX_FILES: usize = 16;
pub const MAX_FILE_BYTES: usize = 64 * 1024;
pub const MAX_ORIGINS: usize = 16;
pub const MAX_PROBES: usize = 8;
pub const MAX_ENV: usize = 64;
pub const MAX_EVENTS: usize = 16;
pub const MAX_HOOKS: usize = 8;
pub const MAX_CONFIG: usize = 64;
const MAX_LINE: usize = 4096;
/// Codex keys an add-on may not set: hooks go in `hooks`, and the dangerous switch is the plugin's.
const CODEX_REFUSED: [&str; 3] = ["hooks", "approval_policy", "sandbox_mode"];

/// An environment value: text (may hold `{file:REL}`) or a secret by name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvValue {
    Text(String),
    Secret(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
    /// A path under the home folder, without the leading `~/`.
    File(String),
    /// Text the user's own agent configuration mentions.
    Mentions(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddonFile {
    pub body: String,
    pub exec: bool,
}

/// What one agent takes from an add-on.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AgentPart {
    pub env: BTreeMap<String, EnvValue>,
    pub hooks: BTreeMap<String, Vec<String>>,
    pub statusline: Option<String>,
    pub config: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Addon {
    pub description: Option<String>,
    pub service: Option<String>,
    pub net: Vec<String>,
    pub probe: Vec<Probe>,
    pub files: BTreeMap<String, AddonFile>,
    pub agents: BTreeMap<String, AgentPart>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    pub path: String,
    pub code: &'static str,
}

/// Names the shell, the loader or tmuxedo itself owns.
pub fn reserved_env_name(name: &str) -> bool {
    let u = name.to_ascii_uppercase();
    ["PATH", "HOME", "SHELL", "USER", "IFS", "ENV", "BASH_ENV"].contains(&u.as_str())
        || ["TMUXEDO_", "AIMUX_", "LD_", "DYLD_"].iter().any(|p| u.starts_with(p))
}

fn env_name(s: &str) -> bool {
    let mut c = s.chars();
    let first = c.next().is_some_and(|f| f.is_ascii_alphabetic() || f == '_');
    first && s.len() <= 128 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn secret_name(s: &str) -> bool {
    env_name(s) && s.len() <= 64
}

fn one_line(s: &str) -> bool {
    !s.chars().any(|c| c < ' ' || c == '\u{7f}')
}

fn clean_text(s: &str) -> bool {
    !s.chars()
        .any(|c| (c < ' ' && c != '\t' && c != '\n' && c != '\r') || c == '\u{7f}')
}

fn line(v: &Value, max: usize) -> Option<&str> {
    let s = v.as_str()?;
    (!s.trim().is_empty() && s.len() <= max && one_line(s)).then_some(s)
}

/// A relative path of up to four plain segments: never absolute, never `.` or `..`.
pub fn valid_file_path(p: &str) -> bool {
    let seg = |s: &str| {
        let ok = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-';
        let first = s.chars().next().is_some_and(|f| f.is_ascii_alphanumeric() || f == '_');
        first && s.len() <= 64 && s.chars().all(ok)
    };
    p.len() <= 128 && p.split('/').count() <= 4 && p.split('/').all(seg)
}

/// A probe's path under the home folder: dot-folders allowed, never `.` or `..`.
pub fn valid_probe_path(p: &str) -> bool {
    let seg = |s: &str| {
        let ok = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-';
        !s.is_empty() && s.len() <= 64 && s != "." && s != ".." && s.chars().all(ok)
    };
    p.len() <= 256 && p.split('/').count() <= 6 && p.split('/').all(seg)
}

fn origin(s: &str) -> bool {
    let rest = s.strip_prefix("https://").or_else(|| s.strip_prefix("http://"));
    let Some(rest) = rest else { return false };
    let (host, port) = match rest.split_once(':') {
        Some((h, p)) => (h, Some(p)),
        None => (rest, None),
    };
    let host_ok = host.len() <= 253
        && host
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && host
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-');
    let port_ok = port.is_none_or(|p| (1..=5).contains(&p.len()) && p.chars().all(|c| c.is_ascii_digit()));
    host_ok && port_ok
}

fn event_name(s: &str) -> bool {
    s.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        && s.len() <= 40
        && s.chars().all(|c| c.is_ascii_alphanumeric())
}

fn depth(v: &Value, d: usize) -> usize {
    match v {
        Value::Array(a) => a.iter().map(|x| depth(x, d + 1)).max().unwrap_or(d).max(d),
        Value::Object(o) => o.values().map(|x| depth(x, d + 1)).max().unwrap_or(d).max(d),
        _ => d,
    }
}

/// The `{file:REL}` placeholders in a line.
pub fn file_refs(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = s;
    while let Some(i) = rest.find("{file:") {
        let after = &rest[i + 6..];
        let Some(j) = after.find('}') else { break };
        out.push(&after[..j]);
        rest = &after[j + 1..];
    }
    out
}

fn cut(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

struct Check {
    issues: Vec<Issue>,
}

impl Check {
    fn add(&mut self, path: impl Into<String>, code: &'static str) {
        self.issues.push(Issue {
            path: path.into(),
            code,
        });
    }

    fn unknown(&mut self, o: &Map<String, Value>, allowed: &[&str], prefix: &str) {
        for k in o.keys() {
            if !allowed.contains(&k.as_str()) {
                self.add(format!("{prefix}{k}"), "unknown");
            }
        }
    }

    fn refs(&mut self, at: &str, s: &str, files: &BTreeMap<String, AddonFile>) {
        if file_refs(s).iter().any(|r| !files.contains_key(*r)) {
            self.add(at, "file");
        }
    }
}

/// Which keys each agent may take.
fn agent_keys(agent: &str) -> Option<&'static [&'static str]> {
    match agent {
        "claude" => Some(&["env", "hooks", "statusline"]),
        "codex" => Some(&["env", "config", "hooks"]),
        "cursor" => Some(&["hooks"]),
        _ => None,
    }
}

fn parse_part(c: &mut Check, at: &str, o: &Map<String, Value>, files: &BTreeMap<String, AddonFile>) -> AgentPart {
    let mut part = AgentPart::default();
    if let Some(env) = o.get("env") {
        match env.as_object().filter(|e| e.len() <= MAX_ENV) {
            None => c.add(format!("{at}.env"), "invalid"),
            Some(env) => {
                for (k, x) in env {
                    let p = format!("{at}.env.{}", cut(k, 128));
                    if !env_name(k) || reserved_env_name(k) {
                        c.add(p, "name");
                    } else if let Some(s) = x.as_str() {
                        if s.len() > MAX_LINE || !one_line(s) {
                            c.add(p, "invalid");
                        } else {
                            c.refs(&p, s, files);
                            part.env.insert(k.clone(), EnvValue::Text(s.to_string()));
                        }
                    } else {
                        let name = x.as_object().filter(|m| m.len() == 1).and_then(|m| m.get("$secret"));
                        match name.and_then(Value::as_str).filter(|n| secret_name(n)) {
                            Some(n) => {
                                part.env.insert(k.clone(), EnvValue::Secret(n.to_string()));
                            }
                            None => c.add(p, "secret"),
                        }
                    }
                }
            }
        }
    }
    if let Some(hooks) = o.get("hooks") {
        match hooks.as_object().filter(|h| h.len() <= MAX_EVENTS) {
            None => c.add(format!("{at}.hooks"), "invalid"),
            Some(hooks) => {
                for (e, cmds) in hooks {
                    let p = format!("{at}.hooks.{}", cut(e, 40));
                    if !event_name(e) {
                        c.add(p, "name");
                        continue;
                    }
                    let list = cmds.as_array().filter(|l| !l.is_empty() && l.len() <= MAX_HOOKS);
                    let lines: Option<Vec<&str>> = list.and_then(|l| l.iter().map(|x| line(x, MAX_LINE)).collect());
                    match lines {
                        None => c.add(p, "invalid"),
                        Some(lines) => {
                            for l in &lines {
                                c.refs(&p, l, files);
                            }
                            part.hooks
                                .insert(e.clone(), lines.into_iter().map(String::from).collect());
                        }
                    }
                }
            }
        }
    }
    if let Some(s) = o.get("statusline") {
        match line(s, MAX_LINE) {
            None => c.add(format!("{at}.statusline"), "invalid"),
            Some(s) => {
                c.refs(&format!("{at}.statusline"), s, files);
                part.statusline = Some(s.to_string());
            }
        }
    }
    if let Some(config) = o.get("config") {
        match config.as_object().filter(|m| m.len() <= MAX_CONFIG) {
            None => c.add(format!("{at}.config"), "invalid"),
            Some(config) => {
                for (k, x) in config {
                    let p = format!("{at}.config.{}", cut(k, 128));
                    let head = k.split('.').next().unwrap_or("").trim();
                    if line(&Value::String(k.clone()), 128).is_none() {
                        c.add(p, "name");
                    } else if CODEX_REFUSED.contains(&head) {
                        c.add(p, "refused");
                    } else if depth(x, 0) > 4 || x.to_string().len() > MAX_LINE {
                        c.add(p, "invalid");
                    } else {
                        part.config.insert(k.clone(), x.clone());
                    }
                }
            }
        }
    }
    part
}

/// Check an add-on and return its parts; unknown keys are refused, so a typo
/// never silently does nothing. The issues name the same paths as the
/// TypeScript side.
pub fn parse_addon(v: &Value) -> Result<Addon, Vec<Issue>> {
    let mut c = Check { issues: Vec::new() };
    let Some(o) = v.as_object() else {
        return Err(vec![Issue {
            path: String::new(),
            code: "invalid",
        }]);
    };
    c.unknown(o, &["description", "service", "net", "probe", "files", "agents"], "");
    let mut out = Addon::default();
    if let Some(d) = o.get("description") {
        match line(d, 1024) {
            Some(d) => out.description = Some(d.to_string()),
            None => c.add("description", "invalid"),
        }
    }
    if let Some(s) = o.get("service") {
        match line(s, 64) {
            Some(s) => out.service = Some(s.to_string()),
            None => c.add("service", "invalid"),
        }
    }
    if let Some(net) = o.get("net") {
        match net.as_array().filter(|l| l.len() <= MAX_ORIGINS) {
            None => c.add("net", "invalid"),
            Some(l) => {
                for (i, x) in l.iter().enumerate() {
                    match x.as_str().filter(|s| origin(s)) {
                        Some(s) => out.net.push(s.to_string()),
                        None => c.add(format!("net.{i}"), "origin"),
                    }
                }
            }
        }
    }
    if let Some(probe) = o.get("probe") {
        match probe.as_array().filter(|l| l.len() <= MAX_PROBES) {
            None => c.add("probe", "invalid"),
            Some(l) => {
                for (i, x) in l.iter().enumerate() {
                    let one = x.as_object().filter(|m| m.len() == 1);
                    let file = one.and_then(|m| m.get("file")).and_then(Value::as_str);
                    let file = file.and_then(|f| f.strip_prefix("~/")).filter(|f| valid_probe_path(f));
                    let mentions = one.and_then(|m| m.get("mentions")).and_then(|m| line(m, 256));
                    match (file, mentions) {
                        (Some(f), _) => out.probe.push(Probe::File(f.to_string())),
                        (_, Some(m)) => out.probe.push(Probe::Mentions(m.to_string())),
                        _ => c.add(format!("probe.{i}"), "invalid"),
                    }
                }
            }
        }
    }
    if let Some(files) = o.get("files") {
        match files.as_object().filter(|m| m.len() <= MAX_FILES) {
            None => c.add("files", "invalid"),
            Some(m) => {
                for (p, f) in m {
                    let at = format!("files.{}", cut(p, 128));
                    if !valid_file_path(p) {
                        c.add(at, "path");
                        continue;
                    }
                    let f = f.as_object().filter(|f| f.keys().all(|k| k == "body" || k == "exec"));
                    let body = f.and_then(|f| f.get("body")).and_then(Value::as_str);
                    let body = body.filter(|b| clean_text(b) && b.len() <= MAX_FILE_BYTES);
                    let exec = f.and_then(|f| f.get("exec"));
                    let exec = match exec {
                        None => Some(false),
                        Some(e) => e.as_bool(),
                    };
                    match (body, exec) {
                        (Some(body), Some(exec)) => {
                            out.files.insert(
                                p.clone(),
                                AddonFile {
                                    body: body.to_string(),
                                    exec,
                                },
                            );
                        }
                        _ => c.add(at, "invalid"),
                    }
                }
            }
        }
    }
    match o.get("agents").and_then(Value::as_object).filter(|m| !m.is_empty()) {
        None => c.add("agents", "invalid"),
        Some(agents) => {
            for (agent, part) in agents {
                let at = format!("agents.{}", cut(agent, 40));
                let Some(keys) = agent_keys(agent) else {
                    c.add(at, "unknown");
                    continue;
                };
                let Some(p) = part.as_object().filter(|p| !p.is_empty()) else {
                    c.add(at, "invalid");
                    continue;
                };
                c.unknown(p, keys, &format!("{at}."));
                let parsed = parse_part(&mut c, &at, p, &out.files);
                out.agents.insert(agent.clone(), parsed);
            }
        }
    }
    if c.issues.is_empty() && v.to_string().len() > MAX_BYTES {
        c.add("", "too_large");
    }
    if c.issues.is_empty() {
        Ok(out)
    } else {
        Err(c.issues)
    }
}
