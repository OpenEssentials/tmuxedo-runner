//! The plugin set from web (`config.pluginSet`, docs/protocol.md "Plugin
//! settings"): what each agent plugin gets on this machine, per project.
//!
//! This is the receiving half: the set is parsed, bounded and kept in the
//! state directory (`plugin-set.json`, mode 0600), where the plugins'
//! builders read it (crate::plugin_gen). It holds names of secrets, never
//! values.

use std::path::Path;

use serde_json::Value;
use tmuxedo_protocol::addon;
use tmuxedo_protocol::plugin_set::parse_plugin_set;
use tmuxedo_protocol::plugin_set::PluginSet;

use crate::log;

/// The file in the state directory.
pub const FILE: &str = "plugin-set.json";
/// One item's definition, serialised: web's limit with room to spare.
pub const MAX_DEFINITION_BYTES: usize = 96 * 1024;

/// Drop what is over the limits: an item whose definition is too large (and
/// every mention of it; an add-on may be up to `addon::MAX_BYTES`, and all
/// add-ons together up to `addon::MAX_TOTAL_BYTES`), and settings that
/// break the schema dialect's value limits. Returns what was dropped, by name.
pub fn bound(set: &mut PluginSet) -> Vec<String> {
    let mut dropped: Vec<String> = Vec::new();
    let mut gone: Vec<String> = Vec::new();
    let mut addon_bytes = 0usize;
    for i in &set.items {
        let len = i.definition.to_string().len();
        let over = if i.kind == addon::KIND {
            addon_bytes += len;
            len > addon::MAX_BYTES || addon_bytes > addon::MAX_TOTAL_BYTES
        } else {
            len > MAX_DEFINITION_BYTES
        };
        if over {
            if i.kind == addon::KIND {
                addon_bytes -= len;
            }
            gone.push(i.id.clone());
            dropped.push(format!("{} {}", i.kind, i.name));
        }
    }
    set.items.retain(|i| !gone.contains(&i.id));
    let machine = std::iter::once(&mut set.machine);
    for scope in machine.chain(set.projects.values_mut()) {
        for (plugin, entry) in scope.iter_mut() {
            entry.items.retain(|id| !gone.contains(id));
            let values = Value::Object(entry.values.clone());
            if !tmuxedo_schema::check_value_limits(&values).is_empty() {
                entry.values.clear();
                let name = format!("settings of {plugin}");
                if !dropped.contains(&name) {
                    dropped.push(name);
                }
            }
        }
    }
    dropped
}

/// `config.pluginSet` arrived: absent or null changes nothing; a set this
/// runner cannot read is refused and the previous one stays. Returns the
/// set now kept, when the frame carried a usable one.
pub fn apply(state_dir: &Path, received: Option<&Value>) -> Option<PluginSet> {
    let v = received.filter(|v| !v.is_null())?;
    let Some(mut set) = parse_plugin_set(v) else {
        log!("config: plugin set refused; keeping the previous one");
        return None;
    };
    let dropped = bound(&mut set);
    if !dropped.is_empty() {
        let names = dropped.join(", ");
        log!("config: plugin set: dropped {names} (over the limits)");
    }
    let bytes = serde_json::to_vec_pretty(&set).ok()?;
    let path = state_dir.join(FILE);
    if std::fs::read(&path).is_ok_and(|old| old == bytes) {
        return Some(set);
    }
    let items = set.items.len();
    let projects = set.projects.len();
    match crate::config::write_private(&path, &bytes) {
        Ok(()) => log!("config: plugin set: {items} items, {projects} projects"),
        Err(e) => log!("config: plugin set: {e}"),
    }
    Some(set)
}

/// The set kept by [`apply`], checked again on the way in.
pub fn load(state_dir: &Path) -> Option<PluginSet> {
    let text = std::fs::read_to_string(state_dir.join(FILE)).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    parse_plugin_set(&v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fx() -> Value {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../packages/protocol/fixtures/plugin-set.json"
        );
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn a_received_set_is_kept_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let f = fx();
        let full = &f["config"]["full"]["pluginSet"];
        assert!(load(dir.path()).is_none());
        let kept = apply(dir.path(), Some(full)).unwrap();
        assert_eq!(serde_json::to_value(&kept).unwrap(), f["parsed"]["full"]);
        assert_eq!(load(dir.path()), Some(kept));
    }

    #[cfg(unix)]
    #[test]
    fn the_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let f = fx();
        apply(dir.path(), Some(&f["config"]["full"]["pluginSet"])).unwrap();
        let meta = std::fs::metadata(dir.path().join(FILE)).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn absent_null_and_unreadable_sets_keep_the_previous_one() {
        let dir = tempfile::tempdir().unwrap();
        let f = fx();
        let kept = apply(dir.path(), Some(&f["config"]["full"]["pluginSet"]));
        assert!(kept.is_some());
        assert!(apply(dir.path(), None).is_none());
        assert!(apply(dir.path(), Some(&Value::Null)).is_none());
        for name in ["future", "not_object"] {
            let received = &f["config"][name]["pluginSet"];
            assert!(apply(dir.path(), Some(received)).is_none(), "{name}");
        }
        assert_eq!(load(dir.path()), kept);
    }

    #[test]
    fn an_empty_set_replaces_the_previous_one() {
        let dir = tempfile::tempdir().unwrap();
        let f = fx();
        apply(dir.path(), Some(&f["config"]["full"]["pluginSet"])).unwrap();
        let empty = apply(dir.path(), Some(&f["config"]["empty"]["pluginSet"])).unwrap();
        assert!(empty.items.is_empty());
        assert_eq!(load(dir.path()), Some(empty));
    }

    #[test]
    fn junk_rows_are_dropped_before_the_set_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let f = fx();
        let kept = apply(dir.path(), Some(&f["config"]["junk"]["pluginSet"])).unwrap();
        assert_eq!(serde_json::to_value(&kept).unwrap(), f["parsed"]["junk"]);
    }

    #[test]
    fn what_is_over_the_limits_is_dropped_by_name() {
        let big = "x".repeat(MAX_DEFINITION_BYTES);
        let deep = json!({ "a": { "b": { "c": {} } } });
        let received = json!({
            "v": 1,
            "items": [
                { "id": "big", "kind": "instructions", "name": "huge", "version": 1, "definition": { "body": big } },
                { "id": "ok", "kind": "command", "name": "ship", "version": 1, "definition": { "body": "Ship." } }
            ],
            "machine": {
                "claude": { "items": ["big", "ok"], "values": deep },
                "codex": { "items": ["big"], "values": { "launch": { "args": ["-v"] } } }
            },
            "projects": { "p": { "claude": { "items": ["ok", "big"], "values": deep } } }
        });
        let mut set = parse_plugin_set(&received).unwrap();
        let dropped = bound(&mut set);
        assert_eq!(dropped, ["instructions huge", "settings of claude"]);
        assert!(set.item("big").is_none());
        let claude = set.entry("claude", None).unwrap();
        assert_eq!(claude.items, ["ok"]);
        assert!(claude.values.is_empty());
        let in_project = set.entry("claude", Some("p")).unwrap();
        assert_eq!(in_project.items, ["ok"]);
        let codex = set.entry("codex", None).unwrap();
        assert!(codex.items.is_empty());
        assert_eq!(codex.values["launch"]["args"][0], "-v");
    }

    #[test]
    fn an_addon_may_be_larger_than_an_item_but_all_addons_have_a_total() {
        let def = |n: usize| json!({ "agents": { "claude": { "env": { "A": "1" } } }, "description": "x".repeat(n) });
        let item =
            |id: &str, n: usize| json!({ "id": id, "kind": "addon", "name": id, "version": 1, "definition": def(n) });
        let received = json!({
            "v": 1,
            "items": [
                item("a1", 200 * 1024),
                item("a2", 200 * 1024),
                item("a3", 100 * 1024),
                item("a4", 300 * 1024)
            ],
            "machine": { "claude": { "items": ["a1", "a2", "a3", "a4"], "values": {} } },
            "projects": {}
        });
        let mut set = parse_plugin_set(&received).unwrap();
        let dropped = bound(&mut set);
        // a1 (200 KiB, over an item's 96) stays; a2 would take the add-ons
        // past 384 KiB; a3 still fits; a4 is over one add-on's 256 KiB.
        assert_eq!(dropped, ["addon a2", "addon a4"]);
        assert_eq!(set.entry("claude", None).unwrap().items, ["a1", "a3"]);
    }
}
