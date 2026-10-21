//! Add-ons in the builders (docs/design/plugins.md §8.1, D-AO1..3): what an
//! enabled add-on (`kind: "addon"` in the plugin set, parsed by
//! [`tmuxedo_protocol::addon`]) adds to one scope of one agent plugin.
//!
//! - Environment: plain values become `export` lines of `argv.sh`; a value
//!   that names a secret is listed by name in `addon-env.json` and fetched
//!   at launch by `tmuxedo secret addon-env` (the same check as the agent's
//!   own secrets: an Agent binding for this plugin), so no value is ever
//!   written to a file.
//! - Files: `files/<add-on>/<path>` in the scope's directory (0700; a file
//!   marked `exec` 0700, others 0600). `{file:PATH}` in a value or command
//!   is that file's absolute path.
//! - Claude Code: hooks and the status line go into the scope's own copy of
//!   the plugin's hook settings (`claude-settings.json`), which the shim
//!   passes instead of the host's. The status line is chained: the add-on's
//!   command runs with `TMUXEDO_STATUSLINE_CHAIN` set to tmuxedo's status
//!   line command (which runs the person's own), and the person's output is
//!   what shows; the add-on's shows only when the person has none.
//! - Codex: `config` keys as `-c key=value` (D-AO1). A key `-c` cannot carry
//!   leaves Codex out of that add-on ("Not available for Codex"); a
//!   `config.toml` is never written. Hooks join tmuxedo's own `-c hooks.*`
//!   group per event.
//! - Cursor: hooks go into a plugin of their own (`cursor-addons/`, its
//!   `.cursor-plugin/plugin.json` and `hooks/hooks.json`), passed with one
//!   more `--plugin-dir`; nothing is written to `~/.cursor`.
//! - A variable or key the agent plugin or an earlier add-on already sets is
//!   left out and reported (D-AO2). An add-on whose service's own installer
//!   already set it up (its `probe`) is skipped.
//!
//! Off is absence: an add-on that is not in the scope's entry adds nothing,
//! from the next launch on.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};
use tmuxedo_protocol::addon::{parse_addon, Addon, AgentPart, EnvValue, Probe};
use tmuxedo_protocol::plugin_set::PluginSetItem;

use crate::plugin::{sh_quote, toml_str, DEFAULT_PLUGIN};
use crate::plugin_gen::Built;

/// The scope's copy of the Claude hook settings.
pub const CLAUDE_SETTINGS: &str = "claude-settings.json";
/// The chained status line.
pub const STATUSLINE: &str = "statusline.sh";
/// Names of the secrets the launch fetches (no values).
pub const ADDON_ENV: &str = "addon-env.json";
/// The variable the add-on's status line finds the next command in.
pub const CHAIN_ENV: &str = "TMUXEDO_STATUSLINE_CHAIN";
/// Cursor: the folder of the add-ons' own plugin in the scope.
pub const CURSOR_PLUGIN: &str = "cursor-addons";
const PROBE_READ_MAX: u64 = 1024 * 1024;

/// What the plugin itself brings, so an add-on can join it or stay clear of it.
#[derive(Debug, Default, Clone)]
pub struct AgentHost {
    /// The host's generated hook settings (Claude), as written.
    pub settings: Option<String>,
    /// The host's `-c hooks.*` arguments (Codex).
    pub hooks_argv: Vec<String>,
    /// Variables the plugin's shim sets (`launch.env`).
    pub env: Vec<String>,
    /// The `-c` keys the shim passes besides the set (its skill argument).
    pub config_keys: Vec<String>,
    /// The home folder, for `{"file": "~/…"}` probes.
    pub home: Option<PathBuf>,
    /// The agent's own configuration files, for `{"mentions": …}` probes.
    pub config_files: Vec<PathBuf>,
}

impl AgentHost {
    /// What the built-in plugin `plugin` brings on this machine.
    pub fn of(plugin: &str, state_dir: &Path, hook_exe: &Path, home: Option<&Path>) -> AgentHost {
        let mut out = AgentHost {
            home: home.map(Path::to_path_buf),
            ..Default::default()
        };
        if let Some(h) = home {
            let env_dir = |var: &str, dflt: &str| {
                std::env::var_os(var)
                    .filter(|v| !v.is_empty())
                    .map(PathBuf::from)
                    .unwrap_or_else(|| h.join(dflt))
            };
            out.config_files = match plugin {
                "claude" => {
                    let d = env_dir("CLAUDE_CONFIG_DIR", ".claude");
                    vec![d.join("settings.json"), d.join("settings.local.json")]
                }
                "codex" => {
                    let d = env_dir("CODEX_HOME", ".codex");
                    vec![d.join("config.toml"), d.join("hooks.json")]
                }
                "cursor" => vec![h.join(".cursor/hooks.json"), h.join(".cursor/cli-config.json")],
                _ => vec![],
            };
        }
        let host = crate::plugin::Host::builtin();
        let Some(p) = host.get(plugin) else { return out };
        let m = p.manifest();
        let gen = crate::plugin::gen_paths(p.as_ref(), state_dir);
        for (name, g) in &m.generate {
            if g.builder == "hook-settings" {
                out.settings = gen.get(name).and_then(|f| std::fs::read_to_string(f).ok());
            }
        }
        out.hooks_argv = p.hooks_argv(hook_exe);
        if let Some(l) = &m.launch {
            out.env = l.env.keys().cloned().collect();
        }
        if let Some(sk) = &m.skill {
            for a in &sk.argv {
                if let Some((k, _)) = a.split_once('=') {
                    if !k.starts_with('-') {
                        out.config_keys.push(k.to_string());
                    }
                }
            }
        }
        out
    }
}

/// The add-ons of `items`, in order.
pub fn addons<'a>(items: &[&'a PluginSetItem]) -> Vec<&'a PluginSetItem> {
    items
        .iter()
        .copied()
        .filter(|i| i.kind == tmuxedo_protocol::addon::KIND)
        .collect()
}

fn read_capped(p: &Path) -> Option<String> {
    use std::io::Read;
    let f = std::fs::File::open(p).ok()?;
    let mut s = String::new();
    f.take(PROBE_READ_MAX).read_to_string(&mut s).ok()?;
    Some(s)
}

/// True when the service's own installer already set the add-on up here.
pub fn probed(a: &Addon, host: &AgentHost) -> bool {
    a.probe.iter().any(|p| match p {
        Probe::File(rel) => host.home.as_ref().is_some_and(|h| h.join(rel).exists()),
        Probe::Mentions(t) => host
            .config_files
            .iter()
            .any(|f| read_capped(f).is_some_and(|s| s.contains(t.as_str()))),
    })
}

/// What goes into the scope's hash for its add-ons: everything the output
/// depends on besides the items (the host's settings and hooks, and which
/// add-ons a probe skips).
pub fn hash_input(items: &[&PluginSetItem], host: Option<&AgentHost>) -> Option<Value> {
    let list = addons(items);
    if list.is_empty() {
        return None;
    }
    let skipped: Vec<&str> = list
        .iter()
        .filter(|i| {
            let a = parse_addon(&i.definition).ok();
            a.zip(host).is_some_and(|(a, h)| probed(&a, h))
        })
        .map(|i| i.id.as_str())
        .collect();
    Some(json!({
        "settings": host.and_then(|h| h.settings.as_deref()),
        "hooksArgv": host.map(|h| h.hooks_argv.clone()),
        "env": host.map(|h| h.env.clone()),
        "configKeys": host.map(|h| h.config_keys.clone()),
        "probed": skipped,
    }))
}

fn agent_label(plugin: &str) -> &str {
    match plugin {
        "claude" => "Claude Code",
        "codex" => "Codex",
        "cursor" => "Cursor",
        other => other,
    }
}

/// `{file:PATH}` → the file's absolute path, through `quote`.
fn resolve_files(s: &str, files_dir: &Path, quote: bool) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find("{file:") {
        let after = &rest[i + 6..];
        let Some(j) = after.find('}') else { break };
        out.push_str(&rest[..i]);
        let path = files_dir.join(&after[..j]).to_string_lossy().into_owned();
        out.push_str(&if quote { sh_quote(&path) } else { path });
        rest = &after[j + 1..];
    }
    out.push_str(rest);
    out
}

/// Two `-c` keys that set the same thing: equal, or one inside the other.
fn keys_overlap(a: &str, b: &str) -> bool {
    a == b || a.starts_with(&format!("{b}.")) || b.starts_with(&format!("{a}."))
}

/// A bare TOML key segment, which is all `-c` splits on.
fn bare_key(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// A JSON value as an inline TOML value; None when TOML has no such value (null).
pub fn toml_value(v: &Value) -> Option<String> {
    Some(match v {
        Value::Null => return None,
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => toml_str(s),
        Value::Array(a) => {
            let parts: Option<Vec<String>> = a.iter().map(toml_value).collect();
            format!("[{}]", parts?.join(", "))
        }
        Value::Object(o) => {
            let mut parts = vec![];
            for (k, x) in o {
                let key = if bare_key(k) { k.clone() } else { toml_str(k) };
                parts.push(format!("{key} = {}", toml_value(x)?));
            }
            format!("{{ {} }}", parts.join(", "))
        }
    })
}

/// Why a Codex key cannot go through `-c`, or None when it can.
fn not_for_c(key: &str, v: &Value) -> Option<String> {
    if !key.split('.').all(bare_key) {
        return Some(format!("the key \"{key}\" cannot be passed with -c"));
    }
    if toml_value(v).is_none() {
        return Some(format!("the value of \"{key}\" has no TOML form"));
    }
    None
}

/// The `-c` keys already in `argv`.
fn c_keys(argv: &[String]) -> Vec<String> {
    let mut out = vec![];
    let mut it = argv.iter().peekable();
    while let Some(a) = it.next() {
        let v = if a == "-c" || a == "--config" {
            it.next().map(String::as_str)
        } else {
            a.strip_prefix("--config=").or_else(|| a.strip_prefix("-c"))
        };
        if let Some((k, _)) = v.and_then(|v| v.split_once('=')) {
            out.push(k.trim().to_string());
        }
    }
    out
}

/// One add-on's row in `report.json`.
fn row(item: &PluginSetItem, a: Option<&Addon>, status: &str, reason: &str) -> Value {
    let mut v = json!({ "kind": item.kind, "name": item.name, "status": status });
    if !reason.is_empty() {
        v["reason"] = json!(reason);
    }
    if let Some(a) = a {
        if !a.net.is_empty() {
            v["sendsTo"] = json!(a.net);
        }
    }
    v
}

/// What the add-ons have set so far in this scope (D-AO2).
struct Taken {
    env: BTreeMap<String, String>,
    keys: Vec<(String, String)>,
}

impl Taken {
    fn env_owner(&self, name: &str) -> Option<&str> {
        self.env.get(name).map(String::as_str)
    }
    fn key_owner(&self, key: &str) -> Option<&str> {
        self.keys
            .iter()
            .find(|(k, _)| keys_overlap(k, key))
            .map(|(_, o)| o.as_str())
    }
}

/// Apply the scope's enabled add-ons to `b` (after the plugin's own build).
pub fn apply(b: &mut Built, plugin: &str, items: &[&PluginSetItem], host: Option<&AgentHost>, exe: &Path, dir: &Path) {
    let list = addons(items);
    if list.is_empty() {
        return;
    }
    let none = AgentHost::default();
    let host = host.unwrap_or(&none);
    let by_plugin = format!("the {plugin} plugin");
    let mut taken = Taken {
        env: host.env.iter().map(|k| (k.clone(), by_plugin.clone())).collect(),
        keys: host
            .config_keys
            .iter()
            .cloned()
            .chain(c_keys(&b.argv))
            .chain(
                c_keys(&host.hooks_argv)
                    .into_iter()
                    .filter(|k| !k.starts_with("hooks.")),
            )
            .map(|k| (k, by_plugin.clone()))
            .collect(),
    };
    taken
        .env
        .insert(crate::plugin_gen::SOCKET_ENV.to_string(), "tmuxedo".to_string());
    // Claude: hook groups and status lines to put in the settings copy.
    let mut claude_hooks: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    let mut status_lines: Vec<String> = vec![];
    // Codex: handler groups per event, joined with tmuxedo's at the end.
    let mut codex_hooks: BTreeMap<String, Vec<String>> = BTreeMap::new();
    // Cursor: hooks per event, in a second `--plugin-dir` of the scope.
    let mut cursor_hooks: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for item in list {
        let a = match parse_addon(&item.definition) {
            Ok(a) => a,
            Err(_) => {
                b.report
                    .push(row(item, None, "skipped", "this add-on's definition is not valid"));
                continue;
            }
        };
        if probed(&a, host) {
            let by = a.service.clone().unwrap_or_else(|| item.name.clone());
            let reason = format!("Already set up on this machine by {by}'s installer");
            b.report.push(row(item, Some(&a), "skipped", &reason));
            continue;
        }
        let label = agent_label(plugin);
        let part = match (plugin, a.agents.get(plugin)) {
            ("claude" | "codex" | "cursor", Some(p)) => p,
            _ => {
                let reason = format!("Not available for {label}");
                b.report.push(row(item, Some(&a), "skipped", &reason));
                continue;
            }
        };
        if plugin == "codex" {
            if let Some(why) = part.config.iter().find_map(|(k, v)| not_for_c(k, v)) {
                let reason = format!("Not available for Codex: {why}");
                b.report.push(row(item, Some(&a), "skipped", &reason));
                continue;
            }
        }
        let files_dir = dir.join("files").join(&item.name);
        let mut refused: Vec<Value> = vec![];
        add_env(b, item, part, &files_dir, &mut taken, &mut refused);
        if plugin == "cursor" {
            for (event, cmds) in &part.hooks {
                for c in cmds {
                    let command = resolve_files(c, &files_dir, true);
                    cursor_hooks
                        .entry(event.clone())
                        .or_default()
                        .push(json!({ "command": command }));
                }
            }
        } else if plugin == "codex" {
            for (k, v) in &part.config {
                if let Some(owner) = taken.key_owner(k) {
                    refused.push(json!({ "key": k, "reason": format!("already set by {owner}") }));
                    continue;
                }
                let value = toml_value(v).expect("checked above");
                b.argv.push("-c".into());
                b.argv.push(format!("{k}={value}"));
                taken.keys.push((k.clone(), format!("the add-on {}", item.name)));
            }
            for (event, cmds) in &part.hooks {
                for c in cmds {
                    let command = resolve_files(c, &files_dir, true);
                    let group = format!("{{hooks=[{{type=\"command\",command={}}}]}}", toml_str(&command));
                    codex_hooks.entry(event.clone()).or_default().push(group);
                }
            }
        } else {
            for (event, cmds) in &part.hooks {
                for c in cmds {
                    let command = resolve_files(c, &files_dir, true);
                    let group = json!({ "hooks": [{ "type": "command", "command": command }] });
                    claude_hooks.entry(event.clone()).or_default().push(group);
                }
            }
            if let Some(s) = &part.statusline {
                status_lines.push(resolve_files(s, &files_dir, true));
            }
        }
        // Only files something of this agent uses would matter, but the
        // whole set is small (16 files at most) and kept together.
        for (rel, f) in &a.files {
            let key = format!("files/{}/{rel}", item.name);
            b.files.insert(key.clone(), f.body.clone());
            if f.exec {
                b.exec.insert(key);
            }
        }
        let mut r = row(item, Some(&a), "built", "");
        if !refused.is_empty() {
            r["refused"] = json!(refused);
        }
        b.report.push(r);
    }
    if !codex_hooks.is_empty() {
        codex_join_hooks(b, host, codex_hooks);
    }
    if !cursor_hooks.is_empty() {
        cursor_plugin(b, dir, cursor_hooks);
    }
    if !claude_hooks.is_empty() || !status_lines.is_empty() {
        claude_settings(b, plugin, host, exe, dir, claude_hooks, &status_lines);
    }
}

fn add_env(
    b: &mut Built,
    item: &PluginSetItem,
    part: &AgentPart,
    files_dir: &Path,
    taken: &mut Taken,
    refused: &mut Vec<Value>,
) {
    for (k, v) in &part.env {
        if let Some(owner) = taken.env_owner(k) {
            refused.push(json!({ "key": k, "reason": format!("already set by {owner}") }));
            continue;
        }
        match v {
            EnvValue::Text(t) => {
                b.env.insert(k.clone(), resolve_files(t, files_dir, false));
            }
            EnvValue::Secret(s) => {
                b.secret_env.insert(k.clone(), s.clone());
            }
        }
        taken.env.insert(k.clone(), format!("the add-on {}", item.name));
    }
}

/// `-c hooks.<event>=[tmuxedo's group, the add-ons' groups]`: given later on
/// the command line, it takes the place of tmuxedo's own for that event.
fn codex_join_hooks(b: &mut Built, host: &AgentHost, groups: BTreeMap<String, Vec<String>>) {
    for (event, add) in groups {
        let prefix = format!("hooks.{event}=");
        let own = host
            .hooks_argv
            .iter()
            .find_map(|a| a.strip_prefix(&prefix))
            .and_then(|v| v.strip_prefix('[')?.strip_suffix(']'))
            .filter(|v| !v.is_empty());
        let all: Vec<&str> = own.into_iter().chain(add.iter().map(String::as_str)).collect();
        b.argv.push("-c".into());
        b.argv.push(format!("{prefix}[{}]", all.join(",")));
    }
}

/// Cursor: the add-ons' hooks as a plugin of their own in the scope
/// ([`CURSOR_PLUGIN`]), given with one more `--plugin-dir` (the flag may be
/// given more than once; decision 0003). Nothing is written under
/// `~/.cursor`, so no managed block and no opt-in.
fn cursor_plugin(b: &mut Built, dir: &Path, hooks: BTreeMap<String, Vec<Value>>) {
    let manifest = json!({
        "name": "tmuxedo-addons",
        "version": "1.0.0",
        "description": "Generated by tmuxedo-runner: the hooks of the add-ons turned on for this session.",
        "hooks": "hooks/hooks.json",
    });
    b.files.insert(
        format!("{CURSOR_PLUGIN}/.cursor-plugin/plugin.json"),
        serde_json::to_string_pretty(&manifest).unwrap_or_default() + "\n",
    );
    b.files.insert(
        format!("{CURSOR_PLUGIN}/hooks/hooks.json"),
        serde_json::to_string_pretty(&json!({ "version": 1, "hooks": hooks })).unwrap_or_default() + "\n",
    );
    b.argv.push("--plugin-dir".into());
    b.argv.push(dir.join(CURSOR_PLUGIN).to_string_lossy().into_owned());
}

/// The scope's copy of the host's hook settings with the add-ons' hooks
/// added and the chained status line.
fn claude_settings(
    b: &mut Built,
    plugin: &str,
    host: &AgentHost,
    exe: &Path,
    dir: &Path,
    hooks: BTreeMap<String, Vec<Value>>,
    status_lines: &[String],
) {
    let mut doc = host
        .settings
        .as_deref()
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    if !doc["hooks"].is_object() {
        doc["hooks"] = Value::Object(Map::new());
    }
    for (event, groups) in hooks {
        let list = &mut doc["hooks"][&event];
        if !list.is_array() {
            *list = json!([]);
        }
        if let Some(l) = list.as_array_mut() {
            l.extend(groups);
        }
    }
    if !status_lines.is_empty() {
        let mut chain = format!("{} hook --statusline", sh_quote(&exe.to_string_lossy()));
        if plugin != DEFAULT_PLUGIN {
            chain.push_str(&format!(" --plugin {plugin}"));
        }
        b.files
            .insert(STATUSLINE.into(), statusline_script(&chain, status_lines));
        b.exec.insert(STATUSLINE.into());
        let command = sh_quote(&dir.join(STATUSLINE).to_string_lossy());
        doc["statusLine"] = json!({ "type": "command", "command": command });
    }
    let body = serde_json::to_string_pretty(&doc).unwrap_or_default() + "\n";
    b.files.insert(CLAUDE_SETTINGS.into(), body);
    b.hook_settings = Some(CLAUDE_SETTINGS.into());
}

/// The chained status line: each add-on's command, with the next command
/// in [`CHAIN_ENV`]; the person's line (through tmuxedo's) is what shows,
/// the first add-on's only when the person has none.
pub fn statusline_script(chain: &str, commands: &[String]) -> String {
    let mut s = String::from(
        "#!/bin/sh\n\
         # tmuxedo: the add-ons' status lines, then the person's own (through\n\
         # tmuxedo's, which runs it). Generated by tmuxedo-runner.\n",
    );
    s.push_str(&format!("{CHAIN_ENV}={}; export {CHAIN_ENV}\n", sh_quote(chain)));
    s.push_str("tmuxedo_in=$(cat)\n");
    for (i, c) in commands.iter().enumerate() {
        if i == 0 {
            s.push_str(&format!(
                "tmuxedo_addon=$(printf '%s\\n' \"$tmuxedo_in\" | {{ {c}\n}} 2>/dev/null)\n"
            ));
        } else {
            s.push_str(&format!(
                "printf '%s\\n' \"$tmuxedo_in\" | {{ {c}\n}} >/dev/null 2>&1\n"
            ));
        }
    }
    s.push_str(&format!(
        "tmuxedo_user=$(printf '%s\\n' \"$tmuxedo_in\" | /bin/sh -c \"${CHAIN_ENV}\" 2>/dev/null)\n"
    ));
    s.push_str(
        "if [ -n \"$tmuxedo_user\" ]; then\n  printf '%s\\n' \"$tmuxedo_user\"\nelif [ -n \"$tmuxedo_addon\" ]; then\n  printf '%s\\n' \"$tmuxedo_addon\"\nfi\n",
    );
    s
}

/// The lines `argv.sh` runs before its `set --` line: plain variables, the
/// secrets by name (fetched at launch, never written), and the settings
/// copy the shim passes.
pub fn prelude(b: &Built, exe: &Path, dir: &Path) -> String {
    let mut s = String::new();
    for (k, v) in &b.env {
        s.push_str(&format!("{k}={}; export {k}\n", sh_quote(v)));
    }
    if !b.secret_env.is_empty() {
        s.push_str(&format!(
            "eval \"$({} cli secret addon-env {} 2>/dev/null)\"\n",
            sh_quote(&exe.to_string_lossy()),
            sh_quote(&dir.join(ADDON_ENV).to_string_lossy())
        ));
    }
    if let Some(f) = &b.hook_settings {
        s.push_str(&format!(
            "tmuxedo_hook_settings={}\n",
            sh_quote(&dir.join(f).to_string_lossy())
        ));
    }
    s
}

/// `addon-env.json`: which variable takes which secret (names only).
pub fn addon_env_file(plugin: &str, b: &Built) -> Option<String> {
    if b.secret_env.is_empty() {
        return None;
    }
    let doc = json!({ "v": 1, "plugin": plugin, "env": b.secret_env });
    Some(serde_json::to_string_pretty(&doc).unwrap_or_default() + "\n")
}

/// The secret names an `addon-env.json` asks for, each once.
pub fn addon_env_names(doc: &Value) -> BTreeSet<String> {
    doc["env"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(_, v)| v.as_str().map(String::from))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toml_values_and_keys() {
        let v = json!({ "a": 1, "b c": [true, "x\"y"], "d": { "e": 1.5 } });
        assert_eq!(
            toml_value(&v).unwrap(),
            r#"{ a = 1, "b c" = [true, "x\"y"], d = { e = 1.5 } }"#
        );
        assert!(toml_value(&json!([1, null])).is_none());
        assert!(not_for_c("otel.exporter", &json!("x")).is_none());
        assert!(not_for_c("profile name", &json!("x")).is_some());
        assert!(not_for_c("a.\"b\"", &json!("x")).is_some());
        assert!(not_for_c("a", &Value::Null).is_some());
        assert!(keys_overlap("otel", "otel.exporter") && keys_overlap("a.b", "a") && !keys_overlap("ab", "a"));
        let argv: Vec<String> = ["-c", "a.b=1", "--config=c=2", "-cd=3", "--x"].map(String::from).into();
        assert_eq!(c_keys(&argv), ["a.b", "c", "d"]);
    }

    #[test]
    fn file_placeholders() {
        let d = Path::new("/s/files/x");
        assert_eq!(resolve_files("{file:bin/h} go", d, true), "'/s/files/x/bin/h' go");
        assert_eq!(resolve_files("p={file:a}", d, false), "p=/s/files/x/a");
        assert_eq!(resolve_files("no {file:", d, false), "no {file:");
    }
}
