//! `config.pluginSet` (docs/protocol.md "Plugin settings"): what each agent
//! plugin gets on this machine besides the CLI's own configuration (MCP
//! servers, skills, sub-agents, commands, instruction blocks) and the
//! plugin's settings, already resolved by web per project.
//!
//! The set is additive to the `config` frame: a web that does not send it
//! leaves the runner as it was, and a runner that does not know it ignores
//! it. Parsing drops junk rows instead of refusing the set. Secrets appear
//! by name only (`{"$secret": "NAME"}`); a definition is kept as JSON for
//! the plugin's builders. The TypeScript side is
//! `packages/protocol/src/plugin-set.ts`; both check
//! `packages/protocol/fixtures/plugin-set.json`.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::plugins::valid_plugin_id;

/// Items in one set.
pub const MAX_ITEMS: usize = 200;
/// Projects in one set.
pub const MAX_PROJECTS: usize = 200;
/// The largest version (a whole number both sides represent exactly).
pub const MAX_VERSION: u64 = 9_007_199_254_740_991;
/// The kinds of item: the user's (MCP servers, skills, sub-agents,
/// commands, instruction blocks) and curated add-ons (`crate::addon`).
pub const KINDS: [&str; 6] = ["mcp", "skill", "subagent", "command", "instructions", "addon"];

/// One item, sent once however many plugins and projects get it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PluginSetItem {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub version: u64,
    pub definition: Value,
}

/// What one plugin gets in one place: item ids and its settings.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PluginSetEntry {
    pub items: Vec<String>,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub values: Map<String, Value>,
}

/// Plugin id → what it gets.
pub type PluginSetScope = BTreeMap<String, PluginSetEntry>;

/// The whole set for a machine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PluginSet {
    pub v: u64,
    pub items: Vec<PluginSetItem>,
    /// For a folder that is in no listed project.
    pub machine: PluginSetScope,
    /// Project key → what each plugin gets there.
    pub projects: BTreeMap<String, PluginSetScope>,
}

impl PluginSet {
    /// What `plugin` gets in `project`; a project that is not listed gets the machine's.
    pub fn entry(&self, plugin: &str, project: Option<&str>) -> Option<&PluginSetEntry> {
        let listed = project.and_then(|p| self.projects.get(p));
        listed.unwrap_or(&self.machine).get(plugin)
    }

    /// The item with this id.
    pub fn item(&self, id: &str) -> Option<&PluginSetItem> {
        self.items.iter().find(|i| i.id == id)
    }
}

/// 1 to 64 of `A-Z a-z 0-9 _ -`.
pub fn valid_item_id(s: &str) -> bool {
    let ok = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
    !s.is_empty() && s.len() <= 64 && s.chars().all(ok)
}

/// 1 to 40 of `a-z 0-9 -`, not starting with `-`.
pub fn valid_item_name(s: &str) -> bool {
    let ok = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-';
    !s.is_empty() && s.len() <= 40 && !s.starts_with('-') && s.chars().all(ok)
}

fn parse_item(x: &Value) -> Option<PluginSetItem> {
    let text = |key: &str| x.get(key).and_then(Value::as_str);
    let id = text("id").filter(|s| valid_item_id(s))?;
    let name = text("name").filter(|s| valid_item_name(s))?;
    let kind = text("kind").filter(|s| KINDS.contains(s))?;
    let version = x.get("version").and_then(Value::as_u64)?;
    let definition = x.get("definition").filter(|d| d.is_object())?;
    if !(1..=MAX_VERSION).contains(&version) {
        return None;
    }
    Some(PluginSetItem {
        id: id.to_string(),
        kind: kind.to_string(),
        name: name.to_string(),
        version,
        definition: definition.clone(),
    })
}

fn parse_scope(v: &Value, known: &BTreeSet<String>) -> PluginSetScope {
    let mut out = PluginSetScope::new();
    let Some(o) = v.as_object() else { return out };
    for (plugin, e) in o {
        let Some(list) = e.get("items").and_then(Value::as_array) else {
            continue;
        };
        if !valid_plugin_id(plugin) {
            continue;
        }
        let mut items: Vec<String> = Vec::new();
        for i in list {
            let Some(id) = i.as_str() else { continue };
            if known.contains(id) && !items.iter().any(|x| x == id) {
                items.push(id.to_string());
            }
        }
        let values = e.get("values").and_then(Value::as_object);
        let values = values.cloned().unwrap_or_default();
        out.insert(plugin.clone(), PluginSetEntry { items, values });
    }
    out
}

/// `config.pluginSet` → the valid part. `None` when it is not an object or
/// of a version this runner does not know: the caller keeps what it had.
/// An entry never names an item that is not in `items`.
pub fn parse_plugin_set(v: &Value) -> Option<PluginSet> {
    let o = v.as_object()?;
    if o.get("v").and_then(Value::as_u64) != Some(1) {
        return None;
    }
    let mut items: Vec<PluginSetItem> = Vec::new();
    let mut known: BTreeSet<String> = BTreeSet::new();
    let listed = o.get("items").and_then(Value::as_array);
    for x in listed.into_iter().flatten() {
        if items.len() >= MAX_ITEMS {
            break;
        }
        let Some(item) = parse_item(x) else { continue };
        if known.insert(item.id.clone()) {
            items.push(item);
        }
    }
    let mut projects = BTreeMap::new();
    // Keys come in order (serde_json's map is sorted), as the other side sorts them.
    if let Some(p) = o.get("projects").and_then(Value::as_object) {
        for (k, s) in p.iter().take(MAX_PROJECTS) {
            let bad = |c: char| c < ' ' || c == '\u{7f}';
            if k.is_empty() || k.len() > 512 || k.chars().any(bad) || !s.is_object() {
                continue;
            }
            projects.insert(k.clone(), parse_scope(s, &known));
        }
    }
    let machine = o.get("machine").map(|m| parse_scope(m, &known));
    Some(PluginSet {
        v: 1,
        items,
        machine: machine.unwrap_or_default(),
        projects,
    })
}
