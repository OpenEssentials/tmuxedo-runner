//! `config.roles` (docs/protocol.md "Roles"): the roles web resolved for one
//! machine, sent beside `config.pluginSet` and shaped like it. Each role is
//! sent once (`roles[]`, keyed by `ref`, the library's id); the machine's
//! scope and each listed project's hold the refs that are effective there,
//! already resolved by the nearest-scope rule (project, machine, account,
//! built-in). A project that is not listed gets the machine's.
//!
//! Additive to the `config` frame: a web that does not send it leaves the
//! runner as it was. Parsing drops junk instead of refusing the set; every
//! role is checked again with [`crate::roles::parse_role`], since the text
//! in it is what a person (or a repository) wrote. The TypeScript side is
//! `RoleSet` in packages/protocol/src/roles.ts.

use std::collections::BTreeMap;

use serde_json::{json, Map, Value};

use crate::roles::{parse_role, role_id_ok, Role};

/// Roles in one set (web's per-account limit).
pub const MAX_ROLES: usize = 200;
/// Listed projects in one set.
pub const MAX_PROJECTS: usize = 200;
/// The set as JSON text: larger is refused whole (web keeps under it).
pub const MAX_BYTES: usize = 512 * 1024;
/// The largest version (a whole number both sides represent exactly).
pub const MAX_VERSION: u64 = 9_007_199_254_740_991;

/// One role as the set carries it.
#[derive(Debug, Clone, PartialEq)]
pub struct RoleSetEntry {
    /// The library's id: a row id, or `builtin:<roleId>`.
    pub r#ref: String,
    pub version: u64,
    /// SHA-256 (hex) of the canonical text, as web computed it.
    pub hash: String,
    /// A built-in starter role (never offered as a helper on its own).
    pub builtin: bool,
    pub role: Role,
}

/// The whole set for a machine.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RoleSet {
    pub v: u64,
    pub roles: Vec<RoleSetEntry>,
    /// Refs effective in a folder that is in no listed project.
    pub machine: Vec<String>,
    /// Project key → refs effective there.
    pub projects: BTreeMap<String, Vec<String>>,
}

/// `[A-Za-z0-9:_-]{1,64}`: a ref.
pub fn ref_ok(s: &str) -> bool {
    (1..=64).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b':' || b == b'_' || b == b'-')
}

fn hash_ok(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn parse_entry(x: &Value) -> Option<RoleSetEntry> {
    let r = x.get("ref").and_then(Value::as_str).filter(|s| ref_ok(s))?;
    let version = x.get("version").and_then(Value::as_u64)?;
    let hash = x.get("hash").and_then(Value::as_str).filter(|s| hash_ok(s))?;
    if !(1..=MAX_VERSION).contains(&version) {
        return None;
    }
    let role = parse_role(x.get("role")?).ok()?;
    Some(RoleSetEntry {
        r#ref: r.to_string(),
        version,
        hash: hash.to_ascii_lowercase(),
        builtin: x.get("builtin").and_then(Value::as_bool) == Some(true),
        role,
    })
}

/// Refs that name a kept role, once each and one per role id (the first wins).
fn parse_refs(v: &Value, roles: &[RoleSetEntry]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut ids: Vec<&str> = Vec::new();
    for r in v.as_array().into_iter().flatten().filter_map(Value::as_str) {
        let Some(e) = roles.iter().find(|e| e.r#ref == r) else {
            continue;
        };
        if out.len() >= MAX_ROLES || ids.contains(&e.role.id.as_str()) {
            continue;
        }
        ids.push(&e.role.id);
        out.push(r.to_string());
    }
    out
}

/// `config.roles` → the valid part. `None` when it is not an object with
/// `v: 1`, or is over [`MAX_BYTES`]. Roles that do not parse, repeated refs
/// and refs to nothing are dropped; past [`MAX_ROLES`] and
/// [`MAX_PROJECTS`] the rest is dropped.
pub fn parse_role_set(v: &Value) -> Option<RoleSet> {
    let o = v.as_object()?;
    if o.get("v").and_then(Value::as_u64) != Some(1) || v.to_string().len() > MAX_BYTES {
        return None;
    }
    let mut roles: Vec<RoleSetEntry> = Vec::new();
    for x in o.get("roles").and_then(Value::as_array).into_iter().flatten() {
        if roles.len() >= MAX_ROLES {
            break;
        }
        if let Some(e) = parse_entry(x) {
            if !roles.iter().any(|k| k.r#ref == e.r#ref) {
                roles.push(e);
            }
        }
    }
    let machine = parse_refs(o.get("machine").unwrap_or(&Value::Null), &roles);
    let mut projects = BTreeMap::new();
    for (k, refs) in o.get("projects").and_then(Value::as_object).into_iter().flatten() {
        if projects.len() >= MAX_PROJECTS {
            break;
        }
        let key_ok = (3..=512).contains(&k.len()) && k.bytes().all(|b| (0x21..=0x7e).contains(&b));
        if key_ok {
            projects.insert(k.clone(), parse_refs(refs, &roles));
        }
    }
    Some(RoleSet {
        v: 1,
        roles,
        machine,
        projects,
    })
}

impl RoleSet {
    /// The roles effective in `project` (a listed one, else the machine's), in the order sent.
    pub fn effective(&self, project: Option<&str>) -> Vec<&RoleSetEntry> {
        let refs = project.and_then(|p| self.projects.get(p)).unwrap_or(&self.machine);
        refs.iter()
            .filter_map(|r| self.roles.iter().find(|e| &e.r#ref == r))
            .collect()
    }

    /// The role `id` means in `project`, if any.
    pub fn find(&self, project: Option<&str>, id: &str) -> Option<&RoleSetEntry> {
        if !role_id_ok(id) {
            return None;
        }
        self.effective(project).into_iter().find(|e| e.role.id == id)
    }

    /// Back to the wire form (what [`parse_role_set`] reads).
    pub fn to_value(&self) -> Value {
        let roles: Vec<Value> = self
            .roles
            .iter()
            .map(|e| {
                let role: Value = serde_json::from_str(&e.role.canonical()).unwrap_or(Value::Null);
                let mut v = json!({ "ref": e.r#ref, "version": e.version, "hash": e.hash, "role": role });
                if e.builtin {
                    v["builtin"] = json!(true);
                }
                v
            })
            .collect();
        let projects: Map<String, Value> = self.projects.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
        json!({ "v": 1, "roles": roles, "machine": self.machine, "projects": projects })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn role(id: &str) -> Value {
        json!({ "schema": 1, "id": id, "name": id, "purpose": "p" })
    }
    fn entry(r: &str, id: &str) -> Value {
        json!({ "ref": r, "version": 1, "hash": "a".repeat(64), "role": role(id) })
    }

    #[test]
    fn the_nearest_scope_is_already_resolved_and_a_project_not_listed_gets_the_machines() {
        let s = parse_role_set(&json!({
            "v": 1,
            "roles": [entry("r1", "reviewer"), entry("r2", "reviewer"), entry("builtin:judge", "judge")],
            "machine": ["r1", "builtin:judge"],
            "projects": { "github.com/o/r": ["r2"] },
        }))
        .unwrap();
        let ids = |p: Option<&str>| s.effective(p).iter().map(|e| e.r#ref.clone()).collect::<Vec<_>>();
        assert_eq!(ids(None), ["r1", "builtin:judge"]);
        assert_eq!(ids(Some("github.com/o/r")), ["r2"]);
        assert_eq!(ids(Some("github.com/o/other")), ["r1", "builtin:judge"]);
        assert_eq!(s.find(Some("github.com/o/r"), "reviewer").unwrap().r#ref, "r2");
        assert!(s.find(Some("github.com/o/r"), "judge").is_none());
        assert_eq!(parse_role_set(&s.to_value()).unwrap(), s);
    }

    #[test]
    fn junk_is_dropped_not_refused() {
        let s = parse_role_set(&json!({
            "v": 1,
            "roles": [
                entry("r1", "a1"), entry("r1", "a2"), entry("bad ref", "a3"),
                { "ref": "r4", "version": 0, "hash": "a".repeat(64), "role": role("a4") },
                { "ref": "r5", "version": 1, "hash": "nothex", "role": role("a5") },
                { "ref": "r6", "version": 1, "hash": "a".repeat(64), "role": { "id": "a6", "name": "x", "evil": 1 } },
                entry("r7", "a1"),
            ],
            "machine": ["r1", "r7", "nobody", 3, "r1"],
            "projects": { "x": ["r1"], "github.com/o/r": "junk" },
        }))
        .unwrap();
        let refs: Vec<_> = s.roles.iter().map(|e| e.r#ref.as_str()).collect();
        assert_eq!(refs, ["r1", "r7"]);
        assert_eq!(s.machine, ["r1"], "one ref per role id, the first wins");
        assert_eq!(s.projects.keys().collect::<Vec<_>>(), ["github.com/o/r"]);
        assert!(s.projects["github.com/o/r"].is_empty());
    }

    #[test]
    fn the_set_is_bounded() {
        assert!(parse_role_set(&json!({ "v": 2, "roles": [] })).is_none());
        assert!(parse_role_set(&json!([])).is_none());
        let big = "x".repeat(MAX_BYTES);
        assert!(parse_role_set(&json!({ "v": 1, "roles": [], "pad": big })).is_none());
        let many: Vec<Value> = (0..MAX_ROLES + 5)
            .map(|i| entry(&format!("r{i}"), &format!("id{i}")))
            .collect();
        let refs: Vec<String> = (0..MAX_ROLES + 5).map(|i| format!("r{i}")).collect();
        let s = parse_role_set(&json!({ "v": 1, "roles": many, "machine": refs })).unwrap();
        assert_eq!((s.roles.len(), s.machine.len()), (MAX_ROLES, MAX_ROLES));
    }
}
