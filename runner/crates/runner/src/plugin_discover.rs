//! "Not managed by tmuxedo" (`plugins.discover`, gateway only;
//! docs/design/plugin-settings-ux.md §3): the NAMES of the MCP servers,
//! skills, sub-agents and commands the person set up in each agent CLI's own
//! files, so the Plugins page can say "github is also in your own
//! ~/.claude.json" instead of leaving two servers of the same name unexplained.
//!
//! Only names are read out. Commands, URLs, environment values and file
//! bodies are never returned. The files are read, never written.

use std::collections::BTreeSet;
use std::path::Path;

use serde_json::{json, Map, Value};

pub const METHOD: &str = "plugins.discover";
/// Names per list.
pub const MAX_NAMES: usize = 100;
/// A config file larger than this is not read.
const MAX_FILE: u64 = 16 * 1024 * 1024;

fn ok_name(n: &str) -> bool {
    !n.is_empty() && n.chars().count() <= 64 && !n.chars().any(char::is_control)
}

fn read(p: &Path) -> Option<String> {
    let m = std::fs::metadata(p).ok()?;
    if !m.is_file() || m.len() > MAX_FILE {
        return None;
    }
    std::fs::read_to_string(p).ok()
}

fn keys(v: Option<&Value>, out: &mut BTreeSet<String>) {
    if let Some(o) = v.and_then(Value::as_object) {
        out.extend(o.keys().filter(|k| ok_name(k)).cloned());
    }
}

/// Folders under `dir` that hold a `SKILL.md`.
fn skill_dirs(dir: &Path, out: &mut BTreeSet<String>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten().take(1000) {
        let name = e.file_name().to_string_lossy().into_owned();
        if ok_name(&name) && !name.starts_with('.') && e.path().join("SKILL.md").is_file() {
            out.insert(name);
        }
    }
}

/// `*.md` files under `dir`, by stem.
fn md_files(dir: &Path, out: &mut BTreeSet<String>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten().take(1000) {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) == Some("md") && p.is_file() {
            if let Some(stem) = p.file_stem().and_then(|s| s.to_str()).filter(|s| ok_name(s)) {
                out.insert(stem.to_string());
            }
        }
    }
}

fn mcp_json(p: &Path, out: &mut BTreeSet<String>) {
    if let Some(v) = read(p).and_then(|t| serde_json::from_str::<Value>(&t).ok()) {
        keys(v.get("mcpServers"), out);
    }
}

fn lists(pairs: Vec<(&str, BTreeSet<String>)>) -> Value {
    let mut m = Map::new();
    for (k, set) in pairs {
        if !set.is_empty() {
            m.insert(k.into(), json!(set.into_iter().take(MAX_NAMES).collect::<Vec<_>>()));
        }
    }
    Value::Object(m)
}

/// What the person set up themselves, per plugin and kind, names only.
pub fn discover(home: &Path, codex_home: &Path) -> Value {
    // Claude Code: ~/.claude.json (user and per-project servers), ~/.claude/{skills,agents,commands}.
    let mut c_mcp = BTreeSet::new();
    if let Some(v) = read(&home.join(".claude.json")).and_then(|t| serde_json::from_str::<Value>(&t).ok()) {
        keys(v.get("mcpServers"), &mut c_mcp);
        if let Some(projects) = v.get("projects").and_then(Value::as_object) {
            for p in projects.values() {
                keys(p.get("mcpServers"), &mut c_mcp);
            }
        }
    }
    let (mut c_skill, mut c_agent, mut c_cmd) = (BTreeSet::new(), BTreeSet::new(), BTreeSet::new());
    skill_dirs(&home.join(".claude/skills"), &mut c_skill);
    md_files(&home.join(".claude/agents"), &mut c_agent);
    md_files(&home.join(".claude/commands"), &mut c_cmd);

    // Codex: config.toml [mcp_servers], skills in ~/.agents/skills and CODEX_HOME/skills.
    let mut x_mcp = BTreeSet::new();
    if let Some(t) = read(&codex_home.join("config.toml")).and_then(|t| t.parse::<toml::Table>().ok()) {
        if let Some(servers) = t.get("mcp_servers").and_then(toml::Value::as_table) {
            x_mcp.extend(servers.keys().filter(|k| ok_name(k)).cloned());
        }
    }
    let mut x_skill = BTreeSet::new();
    skill_dirs(&home.join(".agents/skills"), &mut x_skill);
    skill_dirs(&codex_home.join("skills"), &mut x_skill);

    // Cursor: ~/.cursor/mcp.json.
    let mut u_mcp = BTreeSet::new();
    mcp_json(&home.join(".cursor/mcp.json"), &mut u_mcp);

    let mut plugins = Map::new();
    for (plugin, v) in [
        (
            "claude",
            lists(vec![
                ("mcp", c_mcp),
                ("skill", c_skill),
                ("subagent", c_agent),
                ("command", c_cmd),
            ]),
        ),
        ("codex", lists(vec![("mcp", x_mcp), ("skill", x_skill)])),
        ("cursor", lists(vec![("mcp", u_mcp)])),
    ] {
        if v.as_object().is_some_and(|o| !o.is_empty()) {
            plugins.insert(plugin.into(), v);
        }
    }
    json!({ "type": "plugins_discover", "plugins": plugins })
}

impl crate::daemon::Daemon {
    /// `plugins.discover {}` from the gateway.
    pub(crate) fn plugins_discover_request(&mut self, reply: crate::api::ReplyTo) {
        let tx = self.internal_tx.clone();
        tokio::task::spawn_blocking(move || {
            let home = std::env::var_os("HOME")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| "/".into());
            let codex = std::env::var_os("CODEX_HOME")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| home.join(".codex"));
            let result = Ok(discover(&home, &codex));
            let _ = tx.send(crate::daemon::Internal::Fs {
                reply,
                result,
                frames: vec![],
            });
        });
    }
}
