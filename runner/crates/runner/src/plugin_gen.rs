//! The builders (docs/design/plugins.md §8, docs/protocol.md "Plugin
//! settings"): what the plugin set kept by [`crate::plugin_set`] becomes for
//! each agent CLI, without a byte written to the user's own configuration.
//!
//! For every plugin and every scope of the set (the machine's, and each
//! listed project's) the daemon writes `<state>/plugins/<id>/gen/<hash>/`
//! (0700, files 0600): the generated files, `argv.sh` (the extra launch
//! arguments, as one `set --` line of single-quoted words) and
//! `report.json` (what was built and what was left out, by name). `<hash>`
//! covers everything the directory is built from, so a changed set is a new
//! directory and an agent that is running keeps the one it started with.
//! `gen/index.json` says which directory a project gets; a plugin's shim
//! asks `tmuxedo-runner cli plugin-gen <root>` for the one of the folder it
//! is started in and sources its `argv.sh` (only while the set has
//! something for that plugin: a shim without a set is unchanged).
//!
//! - Claude Code: `--plugin-dir` (skills, sub-agents, commands),
//!   `--mcp-config`, `--append-system-prompt-file`.
//! - Codex: `-c mcp_servers.<name>.…`, and `-c developer_instructions=…`
//!   holding the instruction blocks and a line per skill (the skill's file
//!   is in the generated directory).
//! - Cursor: launch settings only; it has no layer for the rest yet.
//!
//! Roles (`config.roles`, crate::role_set) effective in a scope are offered
//! to each CLI whose `[multi_agent]` says it writes helpers: Claude Code
//! gets `agents/<name>.md` in the generated plugin, Codex gets
//! `-c agents.<name>.description` and, with a model, a role config file;
//! Cursor gets none. A role's text is never put into a shell line or a flag
//! position; names are cut to `a-z 0-9 -` (crate::role_launch); a role and
//! a sub-agent item with the same name: the role wins. Built-in roles
//! nobody copied are not offered.
//!
//! Secrets are named, never resolved here: a server whose environment names
//! one is started through `tmuxedo secret exec --mcp <plugin>/<server>`, so
//! the value goes from the runner to that process and into no file.

use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use tmuxedo_protocol::plugin_set::{PluginSet, PluginSetEntry, PluginSetItem};
use tmuxedo_protocol::role_set::{RoleSet, RoleSetEntry};
use tmuxedo_protocol::roles::ROLE_INSTRUCTIONS_MAX_BYTES;

use crate::log;
pub use crate::mcp_pin::{package_pin, PackagePin};
use crate::plugin::manifest::{MultiAgentSection, RoleFieldSupport};
use crate::plugin::{sh_quote, toml_str};
use crate::role_launch::{claude_tools_for_access, helper_name, model_for, safe_model};

/// Part of every hash: raise it when a builder's output changes.
pub const BUILDER: u32 = 3;
pub const INDEX: &str = "index.json";
pub const ARGV: &str = "argv.sh";
pub const REPORT: &str = "report.json";
/// Instruction text per launch: Codex reads 32 KiB; ours for the others.
pub const CODEX_INSTRUCTIONS_MAX: usize = 32 * 1024;
pub const INSTRUCTIONS_MAX: usize = 16 * 1024;
/// A directory no scope names any more is removed after this long: an
/// agent started from it may still be running.
pub const KEEP_STALE: Duration = Duration::from_secs(7 * 24 * 3600);
/// The plugins with a builder.
pub const PLUGINS: [&str; 3] = ["claude", "codex", "cursor"];

/// A plugin's settings schema (the same file web validates against).
pub fn settings_schema(plugin: &str) -> Option<&'static str> {
    match plugin {
        "claude" => Some(include_str!("../plugins/claude/settings.schema.json")),
        "codex" => Some(include_str!("../plugins/codex/settings.schema.json")),
        "cursor" => Some(include_str!("../plugins/cursor/settings.schema.json")),
        _ => None,
    }
}

/// What one scope of one plugin is built from, besides the set.
pub struct Ctx<'a> {
    /// The runner binary (`secret exec`).
    pub exe: &'a Path,
    /// The generated tmuxedo skill's text while it is on (Codex: it shares
    /// `developer_instructions`, where the last one given wins).
    pub skill_text: Option<&'a str>,
}

/// One scope's output: files relative to its directory, the arguments, and
/// what happened to each item.
#[derive(Debug, Default, PartialEq)]
pub struct Built {
    pub files: BTreeMap<String, String>,
    pub argv: Vec<String>,
    pub report: Vec<Value>,
}

impl Built {
    fn note(&mut self, item: &PluginSetItem, status: &str, reason: &str) {
        let mut v = json!({ "kind": item.kind, "name": item.name, "status": status });
        if !reason.is_empty() {
            v["reason"] = json!(reason);
        }
        self.report.push(v);
    }
    /// A local MCP server that fetches a package also says which, and
    /// whether that is one exact version (`package`); one that is not
    /// pinned is still built, with a `warning` saying so.
    fn built(&mut self, item: &PluginSetItem) {
        self.note(item, "built", "");
        if item.kind != "mcp" || item.definition["transport"] == "http" {
            return;
        }
        let Some(p) = package_pin(&strings(&item.definition["command"])) else {
            return;
        };
        let row = self.report.last_mut().expect("just noted");
        row["package"] = json!({
            "registry": p.registry, "name": p.name, "version": p.version, "pinned": p.pinned(),
        });
        if let Some(problem) = p.problem {
            row["warning"] = json!(format!(
                "not pinned to one version ({problem}): what runs can change between launches"
            ));
        }
    }
    fn skipped(&mut self, item: &PluginSetItem, reason: &str) {
        self.note(item, "skipped", reason);
    }
    fn flag(&mut self, flag: &str, value: String) {
        self.argv.push(flag.to_string());
        self.argv.push(value);
    }
}

fn env_name(s: &str) -> bool {
    let mut it = s.chars();
    it.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && s.len() <= 128
        && it.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn secret_of(v: &Value) -> Option<&str> {
    v.get("$secret")
        .and_then(Value::as_str)
        .filter(|s| tmuxedo_schema::is_secret_name(s))
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

/// A local server: program, arguments, plain environment.
pub type Stdio = (String, Vec<String>, Map<String, Value>);

/// A stdio server, wrapped in `secret exec` when its environment names
/// secrets. `Err` says why it cannot be built.
pub fn stdio_command(plugin: &str, item: &PluginSetItem, exe: &Path) -> Result<Stdio, String> {
    let command = strings(&item.definition["command"]);
    if command.is_empty() {
        return Err("it has no command".into());
    }
    let mut plain = Map::new();
    // variable → secret
    let mut secret: BTreeMap<String, String> = BTreeMap::new();
    if let Some(env) = item.definition.get("env").and_then(Value::as_object) {
        for (k, v) in env {
            if !env_name(k) {
                return Err("a variable's name is not allowed".into());
            }
            match (v.as_str(), secret_of(v)) {
                (Some(text), _) => {
                    plain.insert(k.clone(), json!(text));
                }
                (None, Some(name)) => {
                    secret.insert(k.clone(), name.to_string());
                }
                _ => return Err(format!("{k} is neither text nor a secret's name")),
            }
        }
    }
    if secret.is_empty() {
        return Ok((command[0].clone(), command[1..].to_vec(), plain));
    }
    let names: BTreeSet<&String> = secret.values().collect();
    let mut args: Vec<String> = ["cli", "secret", "exec", "--mcp"].map(String::from).to_vec();
    args.push(format!("{plugin}/{}", item.name));
    args.extend(names.iter().map(|n| n.to_string()));
    args.push("--".into());
    // The runner sets a secret under its own name; the server may want another.
    // Only checked names are written into the script, never a value.
    if secret.iter().any(|(var, name)| var != name) {
        let mut script = String::new();
        for (var, name) in secret.iter().filter(|(var, name)| var != name) {
            script.push_str(&format!("{var}=${name}; export {var}; "));
        }
        for name in names.iter().filter(|n| !secret.contains_key(n.as_str())) {
            script.push_str(&format!("unset {name}; "));
        }
        script.push_str("exec \"$@\"");
        args.extend(["/bin/sh".to_string(), "-c".to_string(), script, "sh".to_string()]);
    }
    args.extend(command);
    Ok((exe.to_string_lossy().into_owned(), args, plain))
}

/// A remote server as (url, headers). One that needs a secret has no layer
/// that keeps the value out of a file and out of the agent's environment.
fn http_server(item: &PluginSetItem) -> Result<(String, Map<String, Value>), String> {
    let d = &item.definition;
    let url = d["url"]
        .as_str()
        .filter(|u| u.starts_with("http://") || u.starts_with("https://"));
    let Some(url) = url else {
        return Err("it has no address".into());
    };
    let headers = d.get("headers").and_then(Value::as_object).cloned().unwrap_or_default();
    if d.get("bearer").is_some() || headers.values().any(|v| !v.is_string()) {
        return Err("a remote server that needs a secret is not supported yet".into());
    }
    Ok((url.to_string(), headers))
}

/// YAML front matter: every value a JSON string, which YAML reads as is.
fn front_matter(fields: &[(&str, Option<String>)]) -> String {
    let mut s = String::from("---\n");
    for (k, v) in fields {
        if let Some(v) = v {
            s.push_str(&format!("{k}: {}\n", Value::String(v.clone())));
        }
    }
    s.push_str("---\n\n");
    s
}

fn text(item: &PluginSetItem, key: &str) -> String {
    item.definition[key].as_str().unwrap_or_default().to_string()
}

fn opt(item: &PluginSetItem, key: &str) -> Option<String> {
    item.definition[key]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn skill_file(item: &PluginSetItem) -> String {
    let head = front_matter(&[
        ("name", Some(item.name.clone())),
        ("description", opt(item, "description")),
    ]);
    format!("{head}{}", text(item, "body"))
}

/// Add `block` to `into` unless the whole would pass `max`.
fn add_block(into: &mut String, block: &str, max: usize) -> bool {
    let block = block.trim_end();
    if block.is_empty() {
        return true;
    }
    if into.len() + block.len() + 2 > max {
        return false;
    }
    if !into.is_empty() {
        into.push_str("\n\n");
    }
    into.push_str(block);
    true
}

fn toml_list(items: &[String]) -> String {
    let quoted: Vec<String> = items.iter().map(|s| toml_str(s)).collect();
    format!("[{}]", quoted.join(", "))
}

fn toml_table(m: &Map<String, Value>) -> String {
    let pairs: Vec<String> = m
        .iter()
        .map(|(k, v)| format!("{} = {}", toml_str(k), toml_str(v.as_str().unwrap_or_default())))
        .collect();
    format!("{{ {} }}", pairs.join(", "))
}

// ── dangerous launch flags ───────────────────────────────────────────────

/// The long flags only a plugin's dangerous switch may add. The same list and
/// the same rule as `DANGEROUS_ARGS` / `dangerousArg` in
/// `packages/protocol/src/plugin-set.ts`; both read the cases in
/// `packages/protocol/fixtures/dangerous-args.json`.
fn dangerous_flags(plugin: &str) -> &'static [&'static str] {
    match plugin {
        "claude" => &["--dangerously-skip-permissions", "--allow-dangerously-skip-permissions"],
        "codex" => &["--dangerously-bypass-approvals-and-sandbox", "--yolo"],
        "cursor" => &["--force", "-f", "--yolo"],
        _ => &[],
    }
}

fn unquote(s: &str) -> &str {
    let s = s.strip_prefix(['"', '\'']).unwrap_or(s);
    s.strip_suffix(['"', '\'']).unwrap_or(s)
}

/// `^\s*(approval_policy|sandbox_mode)\s*=\s*["']?\s*(never|danger-full-access)\b`, any case.
fn codex_dangerous_config(v: &str) -> bool {
    let v = v.to_ascii_lowercase();
    let v = v.trim_start();
    let Some(rest) = v
        .strip_prefix("approval_policy")
        .or_else(|| v.strip_prefix("sandbox_mode"))
    else {
        return false;
    };
    let Some(rest) = rest.trim_start().strip_prefix('=') else {
        return false;
    };
    let rest = rest.trim_start();
    let rest = rest.strip_prefix(['"', '\'']).unwrap_or(rest).trim_start();
    ["never", "danger-full-access"].iter().any(|w| {
        rest.strip_prefix(w).is_some_and(|after| {
            !after
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
        })
    })
}

/// Whether the token at `i` (tokens are the whitespace-separated pieces of
/// the arguments) does what the dangerous switch does: `Some(true)` when its
/// value is the next token (that one goes with it), `Some(false)` when the
/// token stands alone, `None` when it is harmless.
fn dangerous_at(plugin: &str, toks: &[&str], i: usize) -> Option<bool> {
    let flags = dangerous_flags(plugin);
    if flags.is_empty() {
        return None;
    }
    let t = toks[i];
    let (head, inline) = match t.split_once('=') {
        Some((h, v)) => (h, Some(v)),
        None => (t, None),
    };
    let name = head.to_lowercase();
    let value = || unquote(inline.or(toks.get(i + 1).copied()).unwrap_or(""));
    let pair = inline.is_none();
    // An attached short-option value: `-anever`, `-c=x`, `-cx=y`.
    let attached = |letter: char| -> Option<&str> {
        let rest = t.strip_prefix('-')?.strip_prefix(letter)?;
        (!t.starts_with("--") && !rest.is_empty()).then(|| unquote(rest.strip_prefix('=').unwrap_or(rest)))
    };
    if name.starts_with("--") && flags.contains(&name.as_str()) {
        return Some(false);
    }
    if plugin == "claude" && name == "--permission-mode" && value().eq_ignore_ascii_case("bypasspermissions") {
        return Some(pair);
    }
    if plugin == "cursor"
        && !t.starts_with("--")
        && head.len() > 1
        && head.starts_with('-')
        && head[1..].chars().all(|c| c.is_ascii_alphabetic())
        && head.contains('f')
    {
        return Some(false);
    }
    if plugin == "codex" {
        if (name == "--config" || name == "-c") && codex_dangerous_config(value()) {
            return Some(pair);
        }
        if attached('c').is_some_and(codex_dangerous_config) {
            return Some(false);
        }
        if (name == "--ask-for-approval" || name == "-a") && value().eq_ignore_ascii_case("never") {
            return Some(pair);
        }
        if attached('a').is_some_and(|v| v.eq_ignore_ascii_case("never")) {
            return Some(false);
        }
        if (name == "--sandbox" || name == "-s") && value().eq_ignore_ascii_case("danger-full-access") {
            return Some(pair);
        }
        if attached('s').is_some_and(|v| v.eq_ignore_ascii_case("danger-full-access")) {
            return Some(false);
        }
    }
    None
}

/// The first token of `args` that does what the plugin's dangerous switch
/// does, or None (the Rust twin of `dangerousArg`).
pub fn dangerous_arg(plugin: &str, args: &[String]) -> Option<String> {
    let toks: Vec<&str> = args.iter().flat_map(|a| a.split_whitespace()).collect();
    (0..toks.len())
        .find(|&i| dangerous_at(plugin, &toks, i).is_some())
        .map(|i| toks[i].to_string())
}

/// `args` without the elements that carry a dangerous flag (and, for a
/// two-element spelling, its value), and the names of the flags dropped.
/// Non-string elements stay: the shape check refuses them.
fn without_dangerous(plugin: &str, args: &[Value]) -> (Vec<Value>, Vec<String>) {
    let mut toks: Vec<(usize, &str)> = vec![];
    for (n, a) in args.iter().enumerate() {
        if let Some(s) = a.as_str() {
            toks.extend(s.split_whitespace().map(|t| (n, t)));
        }
    }
    let words: Vec<&str> = toks.iter().map(|t| t.1).collect();
    let mut drop = BTreeSet::new();
    let mut names: Vec<String> = vec![];
    for i in 0..toks.len() {
        let Some(pair) = dangerous_at(plugin, &words, i) else {
            continue;
        };
        drop.insert(toks[i].0);
        if pair {
            drop.insert(toks[i + 1].0);
        }
        let t = toks[i].1;
        let head = t.split('=').next().unwrap_or(t);
        let name = if !t.starts_with("--") && plugin == "codex" {
            &t[..2]
        } else {
            head
        };
        if !names.iter().any(|n| n == name) {
            names.push(name.to_string());
        }
    }
    let kept = args
        .iter()
        .enumerate()
        .filter(|(n, _)| !drop.contains(n))
        .map(|(_, a)| a.clone())
        .collect();
    (kept, names)
}

/// The plugin's own settings, checked against its schema: (launch
/// arguments, problem). Values that do not fit are not used at all.
pub fn launch_values(plugin: &str, values: &Map<String, Value>) -> (Vec<String>, Option<String>) {
    let (out, problem, _) = launch_values_report(plugin, values);
    (out, problem)
}

/// As `launch_values`, plus the dangerous flags left out of `args` (names
/// only) because the plugin's dangerous switch is not on.
pub fn launch_values_report(plugin: &str, values: &Map<String, Value>) -> (Vec<String>, Option<String>, Vec<String>) {
    if values.is_empty() {
        return (vec![], None, vec![]);
    }
    let schema = settings_schema(plugin).and_then(|s| serde_json::from_str::<Value>(s).ok());
    let Some(schema) = schema else {
        return (vec![], Some("this plugin has no settings".into()), vec![]);
    };
    if !tmuxedo_schema::validate(&schema, &Value::Object(values.clone())).ok() {
        return (
            vec![],
            Some("the settings do not fit the plugin's schema".into()),
            vec![],
        );
    }
    let mut out = vec![];
    if let Some(m) = values.get("model").and_then(Value::as_str).filter(|m| !m.is_empty()) {
        out.extend(["--model".to_string(), m.to_string()]);
    }
    let danger = match plugin {
        "claude" => ("skipPermissions", "--dangerously-skip-permissions"),
        "codex" => ("bypassApprovals", "--dangerously-bypass-approvals-and-sandbox"),
        _ => ("force", "--force"),
    };
    let switch_on = values.get(danger.0).and_then(Value::as_bool) == Some(true);
    if switch_on {
        out.push(danger.1.to_string());
    }
    let mut problem = None;
    let mut dropped = vec![];
    let args = values.get("args").and_then(Value::as_array).filter(|a| !a.is_empty());
    if let Some(args) = args {
        // Only the switch may add these; with it on, its own flag covers them.
        let (kept, names) = without_dangerous(plugin, args);
        if !names.is_empty() && !switch_on {
            log!(
                "plugin set: {plugin}: dropped from the launch arguments: {}",
                names.join(" ")
            );
            dropped = names;
        }
        match crate::launch_config::validate_args(&Value::Array(kept)) {
            Ok(a) => out.extend(a),
            Err(e) => problem = Some(format!("the launch arguments were left out: {e}")),
        }
    }
    (out, problem, dropped)
}

fn build_claude(b: &mut Built, items: &[&PluginSetItem], helpers: &Helpers, ctx: &Ctx, dir: &Path) {
    let of = |kind: &'static str| items.iter().copied().filter(move |i| i.kind == kind);
    let at = |rel: &str| dir.join(rel).to_string_lossy().into_owned();
    let mut servers = Map::new();
    for i in of("mcp") {
        let server = if i.definition["transport"] == "http" {
            http_server(i).map(|(url, headers)| json!({ "type": "http", "url": url, "headers": headers }))
        } else {
            stdio_command("claude", i, ctx.exe)
                .map(|(command, args, env)| json!({ "command": command, "args": args, "env": env }))
        };
        match server {
            Ok(s) => {
                servers.insert(i.name.clone(), s);
                b.built(i);
            }
            Err(e) => b.skipped(i, &e),
        }
    }
    if !servers.is_empty() {
        let doc = json!({ "mcpServers": servers });
        let body = serde_json::to_string_pretty(&doc).unwrap_or_default() + "\n";
        b.files.insert("mcp.json".into(), body);
        b.flag("--mcp-config", at("mcp.json"));
    }
    let before = b.files.len();
    let roles = build_roles(b, "claude", helpers, dir);
    for i in of("skill") {
        b.files
            .insert(format!("plugin/skills/{}/SKILL.md", i.name), skill_file(i));
        b.built(i);
    }
    for i in of("subagent") {
        if roles.contains(&i.name) {
            b.skipped(i, "a role has this name; roles replace sub-agent items");
            continue;
        }
        let tools = strings(&i.definition["tools"]);
        let head = front_matter(&[
            ("name", Some(i.name.clone())),
            ("description", opt(i, "description")),
            ("model", opt(i, "model")),
            ("tools", (!tools.is_empty()).then(|| tools.join(", "))),
        ]);
        let body = format!("{head}{}", text(i, "instructions"));
        b.files.insert(format!("plugin/agents/{}.md", i.name), body);
        b.built(i);
    }
    for i in of("command") {
        let head = front_matter(&[("description", opt(i, "description"))]);
        let body = format!("{head}{}", text(i, "body"));
        b.files.insert(format!("plugin/commands/{}.md", i.name), body);
        b.built(i);
    }
    if b.files.len() > before {
        let manifest = json!({
            "name": "tmuxedo-items",
            "version": "1.0.0",
            "description": "Skills, sub-agents and commands from your tmuxedo settings.",
        });
        let body = serde_json::to_string_pretty(&manifest).unwrap_or_default() + "\n";
        b.files.insert("plugin/.claude-plugin/plugin.json".into(), body);
        b.flag("--plugin-dir", at("plugin"));
    }
    let mut doc = String::new();
    for i in of("instructions") {
        if add_block(&mut doc, &text(i, "body"), INSTRUCTIONS_MAX) {
            b.built(i);
        } else {
            b.skipped(i, "the instructions together are over 16 KiB");
        }
    }
    if !doc.is_empty() {
        b.files.insert("instructions.md".into(), doc + "\n");
        b.flag("--append-system-prompt-file", at("instructions.md"));
    }
}

fn build_codex(b: &mut Built, items: &[&PluginSetItem], helpers: &Helpers, ctx: &Ctx, dir: &Path) {
    let of = |kind: &'static str| items.iter().copied().filter(move |i| i.kind == kind);
    for i in of("mcp") {
        let key = format!("mcp_servers.{}", i.name);
        let mut pairs: Vec<String> = vec![];
        if i.definition["transport"] == "http" {
            match http_server(i) {
                Ok((url, headers)) => {
                    pairs.push(format!("{key}.url={}", toml_str(&url)));
                    if !headers.is_empty() {
                        pairs.push(format!("{key}.http_headers={}", toml_table(&headers)));
                    }
                }
                Err(e) => {
                    b.skipped(i, &e);
                    continue;
                }
            }
        } else {
            match stdio_command("codex", i, ctx.exe) {
                Ok((command, args, env)) => {
                    pairs.push(format!("{key}.command={}", toml_str(&command)));
                    pairs.push(format!("{key}.args={}", toml_list(&args)));
                    if !env.is_empty() {
                        pairs.push(format!("{key}.env={}", toml_table(&env)));
                    }
                }
                Err(e) => {
                    b.skipped(i, &e);
                    continue;
                }
            }
        }
        let tools = strings(&i.definition["tools"]);
        if !tools.is_empty() {
            pairs.push(format!("{key}.enabled_tools={}", toml_list(&tools)));
        }
        for p in pairs {
            b.flag("-c", p);
        }
        b.built(i);
    }
    let own = ctx.skill_text.unwrap_or_default().trim_end();
    let mut doc = own.to_string();
    for i in of("instructions") {
        if add_block(&mut doc, &text(i, "body"), CODEX_INSTRUCTIONS_MAX) {
            b.built(i);
        } else {
            b.skipped(i, "the instructions together are over 32 KiB");
        }
    }
    for i in of("skill") {
        let rel = format!("skills/{}/SKILL.md", i.name);
        let line = format!(
            "Skill \"{}\": {} Read {} before you use it.",
            i.name,
            opt(i, "description").unwrap_or_default(),
            dir.join(&rel).display()
        );
        if add_block(&mut doc, &line, CODEX_INSTRUCTIONS_MAX) {
            b.files.insert(rel, skill_file(i));
            b.built(i);
        } else {
            b.skipped(i, "the instructions together are over 32 KiB");
        }
    }
    // Only when the set added to it: the tmuxedo skill alone is the shim's own argument.
    if doc.len() > own.len() {
        b.flag("-c", format!("developer_instructions={}", toml_str(&doc)));
        b.files.insert("instructions.md".into(), doc + "\n");
    }
    build_roles(b, "codex", helpers, dir);
    for i in of("subagent").chain(of("command")) {
        b.skipped(i, "Codex has no layer for this yet");
    }
}

/// The roles one scope offers as helpers, and how the plugin hosts them.
#[derive(Default, Clone, Copy)]
pub struct Helpers<'a> {
    pub roles: &'a [&'a RoleSetEntry],
    pub multi_agent: Option<&'a MultiAgentSection>,
}

/// Which of the role's parts the plugin's helper format does not carry
/// (they are the parent pane's), by field name.
fn not_carried(plugin: &str, e: &RoleSetEntry, ma: &MultiAgentSection) -> Vec<&'static str> {
    let r = &e.role;
    let has = |f: &str| match f {
        "instructions" => !r.instructions.trim().is_empty(),
        "model" => model_for(r, plugin).is_some(),
        "tools" => !r.tools_allow.is_empty(),
        "access" => r.access != "full",
        "mcp" => !r.mcp.is_empty(),
        "skills" => !r.skills.is_empty(),
        _ => false,
    };
    let mut out: Vec<&'static str> = ["instructions", "model", "tools", "access", "mcp", "skills"]
        .into_iter()
        // A field only some versions carry is not written: it is the pane's.
        .filter(|f| has(f) && ma.role_field(f) != RoleFieldSupport::Yes)
        .collect();
    if model_for(r, plugin).is_some_and(|m| safe_model(m).is_none()) && !out.contains(&"model") {
        out.push("model");
    }
    if !r.tools_deny.is_empty() {
        out.push("tools.deny");
    }
    if !r.secrets.is_empty() {
        // Helpers share the parent's process: secrets only through MCP servers (D-28).
        out.push("secrets");
    }
    out
}

fn role_note(b: &mut Built, e: &RoleSetEntry, name: &str, status: &str, reason: &str, not_applied: &[&str]) {
    let mut v = json!({ "kind": "role", "name": name, "roleId": e.role.id, "status": status });
    if !reason.is_empty() {
        v["reason"] = json!(reason);
    }
    if !not_applied.is_empty() {
        v["notApplied"] = json!(not_applied);
    }
    b.report.push(v);
}

/// Write each role of the scope as the plugin's own helper; returns the
/// names written. Roles go in id order and the first keeps a name.
fn build_roles(b: &mut Built, plugin: &str, h: &Helpers, dir: &Path) -> BTreeSet<String> {
    let mut names: BTreeSet<String> = BTreeSet::new();
    let mut roles: Vec<&RoleSetEntry> = h.roles.iter().copied().filter(|e| !e.builtin).collect();
    roles.sort_by(|a, b| a.role.id.cmp(&b.role.id));
    for e in roles {
        let r = &e.role;
        let Some(name) = helper_name(&r.id) else {
            role_note(b, e, "", "skipped", "nothing of its id is usable as a helper name", &[]);
            continue;
        };
        let Some(ma) = h.multi_agent.filter(|m| m.native()) else {
            let why = "this agent cannot run helpers from definitions; the role runs only as a pane of its own";
            role_note(b, e, &name, "skipped", why, &[]);
            continue;
        };
        if !ma.hosts_role(&r.cli) {
            let why = format!("written for {}; a helper runs in its parent's agent", r.cli);
            role_note(b, e, &name, "skipped", &why, &[]);
            continue;
        }
        if names.contains(&name) {
            role_note(b, e, &name, "skipped", "another role has this helper name", &[]);
            continue;
        }
        if r.instructions.len() > ROLE_INSTRUCTIONS_MAX_BYTES {
            role_note(b, e, &name, "skipped", "the instructions are over 16 KiB", &[]);
            continue;
        }
        let description = if r.purpose.is_empty() {
            r.name.clone()
        } else {
            r.purpose.clone()
        };
        let model = model_for(r, plugin).and_then(safe_model).map(str::to_string);
        let missing = not_carried(plugin, e, ma);
        match plugin {
            "claude" => {
                let tools = if r.tools_allow.is_empty() {
                    claude_tools_for_access(&r.access).map(str::to_string)
                } else {
                    Some(r.tools_allow.join(", "))
                };
                let head = front_matter(&[
                    ("name", Some(name.clone())),
                    ("description", Some(description)),
                    ("model", model),
                    ("tools", tools),
                ]);
                let body = format!("{head}{}", r.instructions);
                b.files.insert(format!("plugin/agents/{name}.md"), body);
            }
            "codex" => {
                let key = format!("agents.{name}");
                b.flag("-c", format!("{key}.description={}", toml_str(&description)));
                if let Some(m) = model {
                    let rel = format!("agents/{name}.toml");
                    b.files.insert(rel.clone(), format!("model = {}\n", toml_str(&m)));
                    let at = dir.join(&rel).to_string_lossy().into_owned();
                    b.flag("-c", format!("{key}.config_file={}", toml_str(&at)));
                }
            }
            _ => {
                role_note(b, e, &name, "skipped", "this agent has no builder for helpers yet", &[]);
                continue;
            }
        }
        role_note(b, e, &name, "built", "", &missing);
        names.insert(name);
    }
    names
}

/// Build one scope of one plugin, for the directory `dir`.
pub fn build(plugin: &str, set: &PluginSet, entry: &PluginSetEntry, ctx: &Ctx, dir: &Path) -> Built {
    build_scope(plugin, set, entry, &Helpers::default(), ctx, dir)
}

/// As [`build`], with the roles the scope offers as helpers.
pub fn build_scope(
    plugin: &str,
    set: &PluginSet,
    entry: &PluginSetEntry,
    helpers: &Helpers,
    ctx: &Ctx,
    dir: &Path,
) -> Built {
    let mut b = Built::default();
    let items: Vec<&PluginSetItem> = entry.items.iter().filter_map(|id| set.item(id)).collect();
    match plugin {
        "claude" => build_claude(&mut b, &items, helpers, ctx, dir),
        "codex" => build_codex(&mut b, &items, helpers, ctx, dir),
        _ => {
            for i in &items {
                b.skipped(i, "this agent has no layer for it yet");
            }
            build_roles(&mut b, plugin, helpers, dir);
        }
    }
    let (args, problem, dropped) = launch_values_report(plugin, &entry.values);
    b.argv.extend(args);
    for name in dropped {
        b.report.push(json!({
            "kind": "launch-argument", "name": name, "status": "skipped",
            "reason": "only the plugin's dangerous switch may add this; it was left out of the arguments",
        }));
    }
    if let Some(p) = problem {
        b.report
            .push(json!({ "kind": "settings", "name": plugin, "status": "skipped", "reason": p }));
    }
    b
}

/// The directory name of one scope: everything it is built from.
pub fn scope_hash(plugin: &str, set: &PluginSet, entry: &PluginSetEntry, ctx: &Ctx) -> String {
    scope_hash_with(plugin, set, entry, &Helpers::default(), ctx)
}

/// As [`scope_hash`], with the roles the scope offers as helpers.
pub fn scope_hash_with(plugin: &str, set: &PluginSet, entry: &PluginSetEntry, helpers: &Helpers, ctx: &Ctx) -> String {
    let items: Vec<&PluginSetItem> = entry.items.iter().filter_map(|id| set.item(id)).collect();
    let mut roles: Vec<String> = helpers
        .roles
        .iter()
        .filter(|e| !e.builtin)
        .map(|e| e.role.canonical())
        .collect();
    roles.sort();
    let mut doc = json!({
        "builder": BUILDER, "plugin": plugin, "items": items, "values": entry.values,
        "exe": ctx.exe.to_string_lossy(), "skill": ctx.skill_text,
    });
    if !roles.is_empty() {
        doc["roles"] = json!(roles);
        doc["multiAgent"] = json!(helpers.multi_agent);
    }
    hex::encode(&Sha256::digest(doc.to_string().as_bytes())[..12])
}

/// The one line of `argv.sh`: the arguments go before the user's own.
pub fn argv_line(argv: &[String]) -> String {
    let mut line = String::from("set --");
    for a in argv {
        line.push(' ');
        line.push_str(&sh_quote(a));
    }
    line.push_str(" \"$@\"\n");
    line
}

fn write_file(path: &Path, body: &str) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(parent)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(body.as_bytes())
}

/// Write one scope under `root`; returns its directory name, or None when
/// the scope gives the plugin nothing.
fn write_scope(
    root: &Path,
    plugin: &str,
    set: &PluginSet,
    entry: &PluginSetEntry,
    helpers: &Helpers,
    ctx: &Ctx,
) -> std::io::Result<Option<String>> {
    let hash = scope_hash_with(plugin, set, entry, helpers, ctx);
    let dir = root.join(&hash);
    let built = build_scope(plugin, set, entry, helpers, ctx, &dir);
    if built.argv.is_empty() && built.report.is_empty() {
        return Ok(None);
    }
    if dir.join(ARGV).is_file() {
        return Ok(Some(hash));
    }
    let tmp = root.join(format!(".{hash}.tmp{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    for (rel, body) in &built.files {
        write_file(&tmp.join(rel), body)?;
    }
    write_file(&tmp.join(ARGV), &argv_line(&built.argv))?;
    let report = json!({ "v": 1, "plugin": plugin, "items": built.report });
    let report = serde_json::to_string_pretty(&report).unwrap_or_default() + "\n";
    write_file(&tmp.join(REPORT), &report)?;
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::rename(&tmp, &dir)?;
    Ok(Some(hash))
}

pub fn root(state_dir: &Path, plugin: &str) -> PathBuf {
    state_dir.join("plugins").join(plugin).join("gen")
}

/// Build every scope of `plugin` and write the index. True when the set has
/// something for the plugin (its shim then asks at each launch).
pub fn rebuild(state_dir: &Path, plugin: &str, set: &PluginSet, machine: &str, ctx: &Ctx) -> std::io::Result<bool> {
    rebuild_with(state_dir, plugin, set, None, None, machine, ctx)
}

/// As [`rebuild`], with the roles (`config.roles`) and the plugin's
/// `[multi_agent]`. A project the roles list and the set does not gets the
/// machine's entry of the set.
pub fn rebuild_with(
    state_dir: &Path,
    plugin: &str,
    set: &PluginSet,
    roles: Option<&RoleSet>,
    multi_agent: Option<&MultiAgentSection>,
    machine: &str,
    ctx: &Ctx,
) -> std::io::Result<bool> {
    let root = root(state_dir, plugin);
    let mut default: Option<String> = None;
    let mut projects: BTreeMap<String, Option<String>> = BTreeMap::new();
    let mut live: BTreeSet<String> = BTreeSet::new();
    let mut keys: BTreeSet<&String> = set.projects.keys().collect();
    keys.extend(roles.into_iter().flat_map(|r| r.projects.keys()));
    let empty = PluginSetEntry::default();
    for project in std::iter::once(None).chain(keys.into_iter().map(Some)) {
        let scope = project.and_then(|k| set.projects.get(k)).unwrap_or(&set.machine);
        let offered: Vec<&RoleSetEntry> = roles
            .map(|r| r.effective(project.map(String::as_str)))
            .unwrap_or_default()
            .into_iter()
            .filter(|e| !e.builtin)
            .collect();
        let helpers = Helpers {
            roles: &offered,
            multi_agent,
        };
        let hash = match (scope.get(plugin), offered.is_empty()) {
            (Some(entry), _) => write_scope(&root, plugin, set, entry, &helpers, ctx)?,
            (None, false) => write_scope(&root, plugin, set, &empty, &helpers, ctx)?,
            (None, true) => None,
        };
        live.extend(hash.clone());
        match project {
            None => default = hash,
            // Listed even when empty: a listed project never falls back to the machine's.
            Some(k) => {
                projects.insert(k.clone(), hash);
            }
        }
    }
    if live.is_empty() {
        let _ = std::fs::remove_file(root.join(INDEX));
        prune(&root, &live);
        return Ok(false);
    }
    let index = json!({ "v": 1, "machine": machine, "default": default, "projects": projects });
    let bytes = serde_json::to_vec_pretty(&index)?;
    crate::config::write_private(&root.join(INDEX), &bytes).map_err(std::io::Error::other)?;
    prune(&root, &live);
    Ok(true)
}

/// Remove directories no scope names that are older than [`KEEP_STALE`].
fn prune(root: &Path, live: &BTreeSet<String>) {
    let Ok(rd) = std::fs::read_dir(root) else { return };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| SystemTime::now().duration_since(t).ok())
            .is_some_and(|age| age > KEEP_STALE);
        if old && e.path().is_dir() && !live.contains(&name) {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

/// The `argv.sh` an agent started in `cwd` gets, from `root`'s index.
pub fn lookup(root: &Path, cwd: &Path) -> Option<PathBuf> {
    let index = std::fs::read_to_string(root.join(INDEX)).ok()?;
    let index: Value = serde_json::from_str(&index).ok()?;
    let listed = index["projects"].as_object()?;
    let keys: crate::project::Projects = listed.keys().map(|k| (k.clone(), BTreeMap::new())).collect();
    let machine = index["machine"].as_str().unwrap_or_default();
    let project = crate::project::Resolver::default().project_for(cwd, machine, &keys);
    let hash = match project {
        Some(k) => listed[&k].as_str(),
        None => index["default"].as_str(),
    }?;
    // A name of ours only: never a path out of the index.
    if hash.is_empty() || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let argv = root.join(hash).join(ARGV);
    argv.is_file().then_some(argv)
}

/// The line a shim runs before it starts the agent.
pub fn shim_line(exe: &Path, root: &Path) -> String {
    format!(
        "    tmuxedo_set=$({} cli plugin-gen {} 2>/dev/null) && [ -r \"$tmuxedo_set\" ] && . \"$tmuxedo_set\"\n",
        sh_quote(&exe.to_string_lossy()),
        sh_quote(&root.to_string_lossy())
    )
}

/// The shim lines of what is already built (a daemon that starts before
/// web's next `config`).
pub fn existing_lines(state_dir: &Path, exe: &Path) -> BTreeMap<String, String> {
    PLUGINS
        .iter()
        .filter(|p| root(state_dir, p).join(INDEX).is_file())
        .map(|p| (p.to_string(), shim_line(exe, &root(state_dir, p))))
        .collect()
}

/// Rebuild every plugin with a builder; returns the shim line of each that
/// has something (the caller hands them to the host and rewrites the shims).
pub fn rebuild_all(
    state_dir: &Path,
    set: &PluginSet,
    machine: &str,
    exe: &Path,
    skill_text: impl Fn(&str) -> Option<String>,
) -> BTreeMap<String, String> {
    rebuild_all_with(state_dir, set, None, machine, exe, skill_text, |_| None)
}

/// As [`rebuild_all`], with the roles and each plugin's `[multi_agent]`.
pub fn rebuild_all_with(
    state_dir: &Path,
    set: &PluginSet,
    roles: Option<&RoleSet>,
    machine: &str,
    exe: &Path,
    skill_text: impl Fn(&str) -> Option<String>,
    multi_agent: impl Fn(&str) -> Option<MultiAgentSection>,
) -> BTreeMap<String, String> {
    let mut lines = BTreeMap::new();
    for plugin in PLUGINS {
        let skill = skill_text(plugin);
        let ctx = Ctx {
            exe,
            skill_text: skill.as_deref(),
        };
        let ma = multi_agent(plugin);
        match rebuild_with(state_dir, plugin, set, roles, ma.as_ref(), machine, &ctx) {
            Ok(true) => {
                lines.insert(plugin.to_string(), shim_line(exe, &root(state_dir, plugin)));
            }
            Ok(false) => {}
            Err(e) => log!("plugin set: {plugin}: {e}"),
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vals(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn a_dangerous_flag_is_dropped_and_harmless_neighbours_stay() {
        let (a, p, d) = launch_values_report(
            "claude",
            &vals(json!({ "args": ["--verbose", "--Dangerously-Skip-Permissions", "--add-dir", "/x"] })),
        );
        assert_eq!(
            (a, p),
            (vec!["--verbose".into(), "--add-dir".into(), "/x".into()], None)
        );
        assert_eq!(d, ["--Dangerously-Skip-Permissions"]);
    }

    #[test]
    fn a_flag_and_its_value_go_together() {
        let (a, _, d) = launch_values_report(
            "claude",
            &vals(json!({ "args": ["--permission-mode", "bypassPermissions", "--verbose"] })),
        );
        assert_eq!(
            (a, d),
            (vec!["--verbose".to_string()], vec!["--permission-mode".to_string()])
        );
        let (a, _, d) = launch_values_report(
            "codex",
            &vals(
                json!({ "args": ["-c", "approval_policy=never", "-a", "never", "-s", "danger-full-access", "--model", "o3"] }),
            ),
        );
        assert_eq!(a, ["--model", "o3"]);
        assert_eq!(d, ["-c", "-a", "-s"]);
        let (a, _, d) = launch_values_report(
            "codex",
            &vals(json!({ "args": ["--config=sandbox_mode=danger-full-access", "-anever"] })),
        );
        assert!(a.is_empty());
        assert_eq!(d, ["--config", "-a"]);
    }

    #[test]
    fn with_the_switch_on_the_flag_comes_once_and_nothing_is_reported() {
        let (a, p, d) = launch_values_report(
            "claude",
            &vals(json!({ "skipPermissions": true, "args": ["--dangerously-skip-permissions", "--verbose"] })),
        );
        assert_eq!(
            (a, p),
            (vec!["--dangerously-skip-permissions".into(), "--verbose".into()], None)
        );
        assert!(d.is_empty());
    }

    #[test]
    fn harmless_look_alikes_are_kept() {
        for (plugin, args) in [
            ("claude", json!(["--permission-mode", "plan"])),
            (
                "codex",
                json!(["-c", "approval_policy=on-request", "-a", "on-request", "--yolo-mode"]),
            ),
            ("cursor", json!(["--force-color", "-p"])),
        ] {
            let n = args.as_array().unwrap().len();
            let (a, p, d) = launch_values_report(plugin, &vals(json!({ "args": args })));
            assert_eq!((a.len(), p, d.len()), (n, None, 0), "{plugin}");
        }
    }

    #[test]
    fn the_skip_is_in_the_report_by_name_only() {
        let set = tmuxedo_protocol::plugin_set::parse_plugin_set(&json!({
            "v": 1, "items": [],
            "machine": { "claude": { "items": [], "values": { "args": ["--permission-mode", "bypassPermissions"] } } },
            "projects": {}
        }))
        .unwrap();
        let ctx = Ctx {
            exe: Path::new("/x"),
            skill_text: None,
        };
        let b = build(
            "claude",
            &set,
            set.entry("claude", None).unwrap(),
            &ctx,
            Path::new("/g"),
        );
        assert!(b.argv.is_empty());
        let r = b.report.iter().find(|r| r["kind"] == "launch-argument").unwrap();
        assert_eq!(
            (r["name"].as_str(), r["status"].as_str()),
            (Some("--permission-mode"), Some("skipped"))
        );
        assert!(!r.to_string().contains("bypass"));
    }
}
