//! The roles from web (`config.roles`, docs/protocol.md "Roles"): the
//! receiving half. The set is parsed, bounded and kept in the state
//! directory (`roles.json`, mode 0600); the builders (crate::plugin_gen),
//! `agent.start {role}` (crate::role_launch) and the helper list
//! (`children[].roleId`) read it from there.

use std::path::Path;

use serde_json::Value;
use tmuxedo_protocol::role_set::{parse_role_set, RoleSet};

use crate::log;

/// The file in the state directory.
pub const FILE: &str = "roles.json";

/// `config.roles` arrived: absent or null changes nothing; a set this
/// runner cannot read is refused and the previous one stays. Returns the
/// set now kept, when the frame carried a usable one.
pub fn apply(state_dir: &Path, received: Option<&Value>) -> Option<RoleSet> {
    let v = received.filter(|v| !v.is_null())?;
    let Some(set) = parse_role_set(v) else {
        log!("config: roles refused; keeping the previous ones");
        return None;
    };
    let bytes = serde_json::to_vec_pretty(&set.to_value()).ok()?;
    let path = state_dir.join(FILE);
    if std::fs::read(&path).is_ok_and(|old| old == bytes) {
        return Some(set);
    }
    let (n, projects) = (set.roles.len(), set.projects.len());
    match crate::config::write_private(&path, &bytes) {
        Ok(()) => log!("config: roles: {n} roles, {projects} projects"),
        Err(e) => log!("config: roles: {e}"),
    }
    Some(set)
}

/// The set kept by [`apply`], checked again on the way in.
pub fn load(state_dir: &Path) -> Option<RoleSet> {
    let text = std::fs::read_to_string(state_dir.join(FILE)).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    parse_role_set(&v)
}

/// The project keys the set lists (for crate::project::Resolver).
pub fn keys(set: &RoleSet) -> crate::project::Projects {
    set.projects.keys().map(|k| (k.clone(), Default::default())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn set() -> Value {
        json!({
            "v": 1,
            "roles": [{ "ref": "r1", "version": 2, "hash": "b".repeat(64),
                        "role": { "schema": 1, "id": "reviewer", "name": "Reviewer" } }],
            "machine": ["r1"], "projects": {},
        })
    }

    #[test]
    fn kept_refused_and_absent() {
        let dir = tempfile::tempdir().unwrap();
        assert!(apply(dir.path(), None).is_none());
        assert!(apply(dir.path(), Some(&Value::Null)).is_none());
        let kept = apply(dir.path(), Some(&set())).unwrap();
        assert_eq!(load(dir.path()).unwrap(), kept);
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.path().join(FILE)).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        // Junk keeps the previous set.
        assert!(apply(dir.path(), Some(&json!({ "v": 9 }))).is_none());
        assert_eq!(load(dir.path()).unwrap(), kept);
    }
}
