//! A role as an agent launch (docs/design/agent-team-builder.md §2.2, §2.4):
//! the arguments `agent.start {role}` adds for a pane of its own, the name a
//! builder gives a role offered as a helper (crate::plugin_gen), and the
//! match from a helper back to its role (`children[].roleId`).
//!
//! Everything in a role is text someone wrote, so nothing from it reaches a
//! shell or a flag position: a model is a single word that cannot start
//! with `-`, instructions go into a file named by its hash, and no role ever
//! adds a plugin's dangerous switch or a permission wider than the user's
//! own (the access presets only narrow).

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use tmuxedo_protocol::role_set::{RoleSet, RoleSetEntry};
use tmuxedo_protocol::roles::{Role, ROLE_ANY_CLI, ROLE_INSTRUCTIONS_MAX_BYTES};

use crate::plugin::manifest::MultiAgentSection;

/// The Claude Code plugin the builder writes helpers into; Claude may name a
/// plugin's helper `<plugin>:<agent>`.
pub const CLAUDE_PLUGIN: &str = "tmuxedo-items";
/// Longest helper name a builder writes.
pub const HELPER_NAME_MAX: usize = 32;

/// The name a builder gives the role `id` as a helper: `a-z 0-9 -` (what
/// every CLI's helper format and a file name accept), no leading or trailing
/// `-`, at most [`HELPER_NAME_MAX`]. A valid role id is its own name. None
/// when nothing usable is left.
pub fn helper_name(id: &str) -> Option<String> {
    let mapped: String = id
        .chars()
        .map(|c| c.to_ascii_lowercase())
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() {
                c
            } else {
                '-'
            }
        })
        .collect();
    let mut out = mapped.trim_start_matches('-').to_string();
    out.truncate(HELPER_NAME_MAX);
    let out = out.trim_end_matches('-').to_string();
    (out.len() >= 2).then_some(out)
}

/// A model name safe as one argument: `A-Z a-z 0-9 . _ : / @ + -`, not
/// starting with `-`, at most 80.
pub fn safe_model(s: &str) -> Option<&str> {
    let ok = |c: char| c.is_ascii_alphanumeric() || "._:/@+-".contains(c);
    let fine = !s.is_empty() && s.len() <= 80 && !s.starts_with('-') && s.chars().all(ok);
    fine.then_some(s)
}

/// The role's model for `plugin`.
pub fn model_for<'a>(role: &'a Role, plugin: &str) -> Option<&'a str> {
    role.model.get(plugin).map(String::as_str)
}

/// Whether a role may run on `plugin` at all (its own pane).
pub fn runs_on(role: &Role, plugin: &str) -> bool {
    role.cli == ROLE_ANY_CLI || role.cli == plugin
}

/// Claude Code helper tools for an access preset when the role lists none:
/// read-only reads, edit adds editing, edit+shell adds the shell, full
/// leaves the list out (the helper has what its parent has).
pub fn claude_tools_for_access(access: &str) -> Option<&'static str> {
    match access {
        "read-only" => Some("Read, Grep, Glob"),
        "edit" => Some("Read, Grep, Glob, Edit, Write"),
        "edit+shell" => Some("Read, Grep, Glob, Edit, Write, Bash"),
        _ => None,
    }
}

/// What `agent.start {role}` adds, and what of the role it could not apply.
#[derive(Debug, Default, PartialEq)]
pub struct PaneLaunch {
    pub args: Vec<String>,
    /// Instructions to write before the launch: (file, text).
    pub file: Option<(PathBuf, String)>,
    pub applied: Vec<&'static str>,
    pub not_applied: Vec<&'static str>,
}

/// The arguments a pane of its own gets from `role` on `plugin`, before the
/// user's own. `dir` is where an instructions file goes (named by the hash
/// of its text). Only narrowing settings are used: read-only is Claude's
/// plan mode and Codex's read-only sandbox; edit is Codex's workspace
/// sandbox; nothing else changes the user's permission prompts.
pub fn pane_launch(plugin: &str, role: &Role, dir: &Path) -> PaneLaunch {
    let mut l = PaneLaunch::default();
    if let Some(m) = model_for(role, plugin) {
        match safe_model(m) {
            Some(m) => {
                l.args.extend(["--model".to_string(), m.to_string()]);
                l.applied.push("model");
            }
            None => l.not_applied.push("model"),
        }
    }
    let instructions = role.instructions.trim_end();
    if !instructions.is_empty() {
        if plugin == "claude" && instructions.len() <= ROLE_INSTRUCTIONS_MAX_BYTES {
            let digest = hex::encode(&Sha256::digest(instructions.as_bytes())[..12]);
            let path = dir.join(format!("{digest}.md"));
            l.args.push("--append-system-prompt-file".into());
            l.args.push(path.to_string_lossy().into_owned());
            l.file = Some((path, format!("{instructions}\n")));
            l.applied.push("instructions");
        } else {
            // Codex shares one developer_instructions with tmuxedo's own (the last wins).
            l.not_applied.push("instructions");
        }
    }
    let access = match (plugin, role.access.as_str()) {
        ("claude", "read-only") => Some(["--permission-mode", "plan"]),
        ("codex", "read-only") => Some(["--sandbox", "read-only"]),
        ("codex", "edit" | "edit+shell") => Some(["--sandbox", "workspace-write"]),
        (_, "full") => None,
        _ => {
            l.not_applied.push("access");
            None
        }
    };
    if let Some(a) = access {
        l.args.extend(a.iter().map(|s| s.to_string()));
        l.applied.push("access");
    }
    if !role.tools_allow.is_empty() || !role.tools_deny.is_empty() {
        l.not_applied.push("tools");
    }
    if !role.mcp.is_empty() {
        l.not_applied.push("mcp");
    }
    if !role.skills.is_empty() {
        l.not_applied.push("skills");
    }
    if !role.secrets.is_empty() {
        l.not_applied.push("secrets");
    }
    // A role never adds what only the plugin's dangerous switch may.
    if crate::plugin_gen::dangerous_arg(plugin, &l.args).is_some() {
        l.args.clear();
        l.file = None;
        l.applied.clear();
        l.not_applied = vec!["model", "instructions", "access"];
    }
    l
}

/// Whether `plugin` (its `[multi_agent]`) can run `e` as a helper: the
/// plugin writes helpers, the role is written for it (or for any CLI), and
/// it is not a built-in nobody copied.
pub fn hosted(ma: Option<&MultiAgentSection>, e: &RoleSetEntry) -> bool {
    !e.builtin && ma.is_some_and(|m| m.hosts_role(&e.role.cli))
}

/// The role a helper named `name` (as its CLI reports it) was built from,
/// among the roles effective in `project`. Claude may prefix a plugin's
/// helper with the plugin's name.
pub fn role_of_helper(
    set: &RoleSet,
    project: Option<&str>,
    plugin: &str,
    ma: Option<&MultiAgentSection>,
    name: &str,
) -> Option<String> {
    let name = name.trim();
    let bare = match plugin {
        "claude" => name
            .strip_prefix(CLAUDE_PLUGIN)
            .and_then(|r| r.strip_prefix(':'))
            .unwrap_or(name),
        _ => name,
    };
    set.effective(project)
        .into_iter()
        .filter(|e| hosted(ma, e))
        .find(|e| helper_name(&e.role.id).as_deref() == Some(bare))
        .map(|e| e.role.id.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tmuxedo_protocol::roles::parse_role;

    fn role(v: serde_json::Value) -> Role {
        let mut doc = json!({ "schema": 1, "id": "reviewer", "name": "Reviewer" });
        for (k, x) in v.as_object().unwrap() {
            doc[k] = x.clone();
        }
        parse_role(&doc).unwrap()
    }

    #[test]
    fn helper_names_keep_to_the_safe_charset() {
        assert_eq!(helper_name("reviewer").as_deref(), Some("reviewer"));
        assert_eq!(helper_name("../../etc/passwd").as_deref(), Some("etc-passwd"));
        assert_eq!(helper_name("Re View;rm -rf ~").as_deref(), Some("re-view-rm--rf"));
        assert_eq!(helper_name("a--b").as_deref(), Some("a--b"));
        assert_eq!(helper_name(&"a".repeat(80)).unwrap().len(), HELPER_NAME_MAX);
        assert_eq!(helper_name("$(x)"), None);
        assert_eq!(helper_name("-"), None);
    }

    #[test]
    fn a_model_is_one_safe_word() {
        assert_eq!(safe_model("gpt-5-codex"), Some("gpt-5-codex"));
        assert_eq!(safe_model("claude-opus-4@2025"), Some("claude-opus-4@2025"));
        assert_eq!(safe_model("--dangerously-skip-permissions"), None);
        assert_eq!(safe_model("a b"), None);
        assert_eq!(safe_model("x;y"), None);
        assert_eq!(safe_model("$(id)"), None);
    }

    #[test]
    fn a_pane_gets_model_instructions_and_narrowing_access_only() {
        let dir = Path::new("/state/roles");
        let r = role(
            json!({ "model": { "claude": "opus", "codex": "gpt-5" }, "instructions": "Review.\n",
                             "tools": { "deny": ["Bash"] }, "mcp": ["github"] }),
        );
        let l = pane_launch("claude", &r, dir);
        assert_eq!(
            l.args[..4],
            ["--model", "opus", "--append-system-prompt-file", l.args[3].as_str()]
        );
        assert!(l.args[3].starts_with("/state/roles/") && l.args[3].ends_with(".md"));
        assert_eq!(l.args[4..], ["--permission-mode", "plan"]);
        assert_eq!(l.file.as_ref().unwrap().1, "Review.\n");
        assert_eq!(l.not_applied, ["tools", "mcp"]);
        let l = pane_launch("codex", &r, dir);
        assert_eq!(l.args, ["--model", "gpt-5", "--sandbox", "read-only"]);
        assert!(l.not_applied.contains(&"instructions"));
        let l = pane_launch("cursor", &r, dir);
        assert!(l.args.is_empty() && l.not_applied.contains(&"access"));
    }

    #[test]
    fn a_role_never_widens_permissions() {
        let dir = Path::new("/s");
        for access in ["full", "edit+shell", "edit"] {
            let r = role(json!({ "access": access, "model": { "claude": "-p", "codex": "--yolo" } }));
            for plugin in ["claude", "codex", "cursor"] {
                let l = pane_launch(plugin, &r, dir);
                assert!(
                    crate::plugin_gen::dangerous_arg(plugin, &l.args).is_none(),
                    "{plugin} {access}"
                );
                assert!(!l
                    .args
                    .iter()
                    .any(|a| a.contains("bypass") || a.contains("danger") || a == "--yolo"));
                assert!(
                    !l.args.contains(&"--model".to_string()),
                    "a flag-shaped model is not used"
                );
            }
        }
        assert!(pane_launch("claude", &role(json!({ "access": "full" })), dir)
            .args
            .is_empty());
    }
}
