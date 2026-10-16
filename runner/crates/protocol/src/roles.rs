//! Roles: reusable agent definitions (docs/design/agent-team-builder.md
//! §2.1), the Rust twin of packages/protocol/src/roles.ts. A role can start a
//! pane of its own or be offered to another agent as a helper.
//!
//! [`parse_role`] is strict (every problem is reported with the path of its
//! field) and [`Role::canonical`] is the one text a role's content hash is
//! taken over: JSON, keys sorted, no spaces, `cli` and `access` always
//! written, empty optional parts left out. The hash is the SHA-256 of that
//! text, taken by the caller. Checked against
//! packages/protocol/fixtures/roles.json, which pins each hash.
//!
//! Everything in a role is text a person (or a repository) wrote.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

pub const ROLE_SCHEMA: u32 = 1;
/// `cli` of a role that runs on whichever CLI hosts it.
pub const ROLE_ANY_CLI: &str = "any";
/// Access presets, least first.
pub const ROLE_ACCESS: &[&str] = &["read-only", "edit", "edit+shell", "full"];
/// The avatar palette.
pub const ROLE_COLORS: &[&str] = &["slate", "red", "orange", "amber", "green", "teal", "blue", "purple"];

pub const ROLE_NAME_MAX: usize = 60;
pub const ROLE_PURPOSE_MAX: usize = 200;
/// Instructions, in UTF-8 bytes.
pub const ROLE_INSTRUCTIONS_MAX_BYTES: usize = 16 * 1024;
pub const ROLE_GLYPH_MAX: usize = 2;
pub const ROLE_MODEL_MAX: usize = 80;
pub const ROLE_MODELS_MAX: usize = 16;
pub const ROLE_TOOL_MAX: usize = 200;
pub const ROLE_TOOLS_MAX: usize = 64;
/// `mcp`, `skills` and `secrets`: entries each.
pub const ROLE_LIST_MAX: usize = 32;
pub const ROLE_DELEGATES_MAX: usize = 16;
pub const ROLE_MAX_SUBAGENTS: u64 = 16;
pub const ROLE_MAX_DEPTH: u64 = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleAvatar {
    pub glyph: String,
    pub color: String,
}

/// A role in normal form: defaults filled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Role {
    pub id: String,
    pub name: String,
    pub avatar: Option<RoleAvatar>,
    /// One line; becomes the helper's description, which the parent agent reads.
    pub purpose: String,
    pub instructions: String,
    /// A plugin id, or [`ROLE_ANY_CLI`].
    pub cli: String,
    /// Model per plugin id.
    pub model: BTreeMap<String, String>,
    /// One of [`ROLE_ACCESS`].
    pub access: String,
    pub tools_allow: Vec<String>,
    pub tools_deny: Vec<String>,
    pub mcp: Vec<String>,
    pub skills: Vec<String>,
    /// Secret names only, never values.
    pub secrets: Vec<String>,
    /// Ids of the roles it may call as helpers.
    pub delegates: Vec<String>,
    pub max_subagents: Option<u64>,
    pub max_depth: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleProblem {
    /// The field, dotted (`tools.allow.1`); empty for the document itself.
    pub path: String,
    pub message: String,
}

const KEYS: &[&str] = &[
    "schema",
    "id",
    "name",
    "avatar",
    "purpose",
    "instructions",
    "cli",
    "model",
    "access",
    "tools",
    "mcp",
    "skills",
    "secrets",
    "delegates",
    "limits",
];

/// `[a-z0-9-]{2,32}`: a role id, and a plugin id.
pub fn role_id_ok(s: &str) -> bool {
    (2..=32).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// `[A-Za-z0-9][A-Za-z0-9._-]{0,63}`: an MCP server or skill name.
fn item_name_ok(s: &str) -> bool {
    let b = s.as_bytes();
    (1..=64).contains(&b.len())
        && b[0].is_ascii_alphanumeric()
        && b.iter()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
}

/// `[A-Za-z_][A-Za-z0-9_]{0,127}`: a secret name.
fn secret_name_ok(s: &str) -> bool {
    let b = s.as_bytes();
    (1..=128).contains(&b.len())
        && (b[0].is_ascii_alphabetic() || b[0] == b'_')
        && b.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'_')
}

/// A C0 or C1 control character other than the ones in `allowed`.
fn has_control(s: &str, allowed: &str) -> bool {
    s.chars().any(|c| {
        let n = c as u32;
        (n < 0x20 || (0x7f..=0x9f).contains(&n)) && !allowed.contains(c)
    })
}

fn edge_space(s: &str) -> bool {
    s.starts_with(' ') || s.ends_with(' ')
}

/// One line of 1..=`max` characters with no space at either end.
fn line_ok(s: &str, max: usize) -> bool {
    !s.is_empty() && s.chars().count() <= max && !has_control(s, "") && !edge_space(s)
}

/// A whole number in `0..=max` (a JSON `3.0` counts, as it does in JavaScript).
fn whole(v: &Value, max: u64) -> Option<u64> {
    let n = match v.as_u64() {
        Some(n) => n,
        None => {
            let f = v.as_f64()?;
            if f < 0.0 || f.fract() != 0.0 || f > max as f64 {
                return None;
            }
            f as u64
        }
    };
    (n <= max).then_some(n)
}

struct Check {
    problems: Vec<RoleProblem>,
}

impl Check {
    fn bad(&mut self, path: impl Into<String>, message: impl Into<String>) {
        self.problems.push(RoleProblem {
            path: path.into(),
            message: message.into(),
        });
    }

    fn unknown(&mut self, o: &Map<String, Value>, known: &[&str], at: &str) {
        let mut keys: Vec<&String> = o.keys().collect();
        keys.sort();
        for k in keys {
            if !known.contains(&k.as_str()) {
                self.bad(format!("{at}{k}"), "Unknown field.");
            }
        }
    }

    fn list(
        &mut self,
        v: Option<&Value>,
        path: &str,
        max: usize,
        ok: impl Fn(&str) -> bool,
        what: &str,
    ) -> Vec<String> {
        let Some(v) = v else { return Vec::new() };
        let items = match v.as_array() {
            Some(a) if a.len() <= max => a,
            _ => {
                self.bad(path, format!("{what} (at most {max})."));
                return Vec::new();
            }
        };
        let mut out: Vec<String> = Vec::new();
        for (i, item) in items.iter().enumerate() {
            match item.as_str() {
                Some(s) if ok(s) => {
                    if out.iter().any(|o| o == s) {
                        self.bad(format!("{path}.{i}"), "Listed twice.");
                    } else {
                        out.push(s.to_string());
                    }
                }
                _ => self.bad(format!("{path}.{i}"), format!("{what}.")),
            }
        }
        out
    }
}

/// Check a role document. Unknown fields, wrong types and broken limits are
/// all problems; nothing is dropped or repaired.
pub fn parse_role(input: &Value) -> Result<Role, Vec<RoleProblem>> {
    let Some(o) = input.as_object() else {
        return Err(vec![RoleProblem {
            path: String::new(),
            message: "A role is a table of fields.".into(),
        }]);
    };
    let mut c = Check { problems: Vec::new() };
    c.unknown(o, KEYS, "");

    if let Some(s) = o.get("schema") {
        if whole(s, u64::from(u32::MAX)) != Some(u64::from(ROLE_SCHEMA)) {
            c.bad("schema", format!("Only schema {ROLE_SCHEMA} is understood."));
        }
    }

    let id = o.get("id").and_then(Value::as_str).unwrap_or("").to_string();
    if !role_id_ok(&id) {
        c.bad("id", "An id is 2 to 32 of a-z, 0-9 and \"-\".");
    }

    let name = o.get("name").and_then(Value::as_str).unwrap_or("").to_string();
    if !line_ok(&name, ROLE_NAME_MAX) {
        c.bad(
            "name",
            format!("A name is 1 to {ROLE_NAME_MAX} characters on one line."),
        );
    }

    let mut avatar = None;
    if let Some(a) = o.get("avatar") {
        match a.as_object() {
            None => c.bad("avatar", "An avatar is a glyph and a colour."),
            Some(a) => {
                c.unknown(a, &["glyph", "color"], "avatar.");
                let glyph = a.get("glyph").and_then(Value::as_str).unwrap_or("");
                let color = a.get("color").and_then(Value::as_str).unwrap_or("");
                let glyph_ok = !glyph.is_empty()
                    && glyph.chars().count() <= ROLE_GLYPH_MAX
                    && !has_control(glyph, "")
                    && !glyph.contains(' ');
                let color_ok = ROLE_COLORS.contains(&color);
                if !glyph_ok {
                    c.bad("avatar.glyph", format!("A glyph is 1 or {ROLE_GLYPH_MAX} characters."));
                }
                if !color_ok {
                    c.bad(
                        "avatar.color",
                        format!("A colour is one of: {}.", ROLE_COLORS.join(", ")),
                    );
                }
                if glyph_ok && color_ok {
                    avatar = Some(RoleAvatar {
                        glyph: glyph.to_string(),
                        color: color.to_string(),
                    });
                }
            }
        }
    }

    let mut purpose = String::new();
    if let Some(v) = o.get("purpose") {
        match v.as_str() {
            Some(s) if s.chars().count() <= ROLE_PURPOSE_MAX && !has_control(s, "") => purpose = s.to_string(),
            _ => c.bad(
                "purpose",
                format!("A purpose is one line of at most {ROLE_PURPOSE_MAX} characters."),
            ),
        }
    }

    let mut instructions = String::new();
    if let Some(v) = o.get("instructions") {
        match v.as_str() {
            Some(s) if s.len() <= ROLE_INSTRUCTIONS_MAX_BYTES && !has_control(s, "\n\t") => {
                instructions = s.to_string()
            }
            _ => c.bad("instructions", "Instructions are plain text of at most 16 KiB."),
        }
    }

    let mut cli = ROLE_ANY_CLI.to_string();
    if let Some(v) = o.get("cli") {
        match v.as_str() {
            Some(s) if role_id_ok(s) => cli = s.to_string(),
            _ => c.bad("cli", "cli is \"any\" or the id of an agent plugin."),
        }
    }

    let mut model = BTreeMap::new();
    if let Some(v) = o.get("model") {
        match v.as_object() {
            Some(m) if m.len() <= ROLE_MODELS_MAX => {
                let mut keys: Vec<&String> = m.keys().collect();
                keys.sort();
                for k in keys {
                    if !role_id_ok(k) || k == ROLE_ANY_CLI {
                        c.bad(format!("model.{k}"), "A model is set per agent plugin id.");
                        continue;
                    }
                    match m[k].as_str() {
                        Some(s) if line_ok(s, ROLE_MODEL_MAX) => {
                            model.insert(k.clone(), s.to_string());
                        }
                        _ => c.bad(
                            format!("model.{k}"),
                            format!("A model name is 1 to {ROLE_MODEL_MAX} characters."),
                        ),
                    }
                }
            }
            _ => c.bad(
                "model",
                format!("model names a model per agent plugin (at most {ROLE_MODELS_MAX})."),
            ),
        }
    }

    let mut access = ROLE_ACCESS[0].to_string();
    if let Some(v) = o.get("access") {
        match v.as_str() {
            Some(s) if ROLE_ACCESS.contains(&s) => access = s.to_string(),
            _ => c.bad("access", format!("access is one of: {}.", ROLE_ACCESS.join(", "))),
        }
    }

    let (mut tools_allow, mut tools_deny) = (Vec::new(), Vec::new());
    if let Some(v) = o.get("tools") {
        match v.as_object() {
            None => c.bad("tools", "tools is an allow list and a deny list."),
            Some(t) => {
                c.unknown(t, &["allow", "deny"], "tools.");
                let ok = |s: &str| line_ok(s, ROLE_TOOL_MAX);
                tools_allow = c.list(
                    t.get("allow"),
                    "tools.allow",
                    ROLE_TOOLS_MAX,
                    ok,
                    "A list of tool rules",
                );
                tools_deny = c.list(t.get("deny"), "tools.deny", ROLE_TOOLS_MAX, ok, "A list of tool rules");
            }
        }
    }

    let mcp = c.list(
        o.get("mcp"),
        "mcp",
        ROLE_LIST_MAX,
        item_name_ok,
        "A list of MCP server names",
    );
    let skills = c.list(
        o.get("skills"),
        "skills",
        ROLE_LIST_MAX,
        item_name_ok,
        "A list of skill names",
    );
    let secrets = c.list(
        o.get("secrets"),
        "secrets",
        ROLE_LIST_MAX,
        secret_name_ok,
        "A list of secret names",
    );
    let delegates = c.list(
        o.get("delegates"),
        "delegates",
        ROLE_DELEGATES_MAX,
        |s| role_id_ok(s) && s != id,
        "A list of other role ids",
    );

    let (mut max_subagents, mut max_depth) = (None, None);
    if let Some(v) = o.get("limits") {
        match v.as_object() {
            None => c.bad("limits", "limits is max_subagents and max_depth."),
            Some(l) => {
                c.unknown(l, &["max_subagents", "max_depth"], "limits.");
                if let Some(v) = l.get("max_subagents") {
                    max_subagents = whole(v, ROLE_MAX_SUBAGENTS);
                    if max_subagents.is_none() {
                        c.bad(
                            "limits.max_subagents",
                            format!("A whole number from 0 to {ROLE_MAX_SUBAGENTS}."),
                        );
                    }
                }
                if let Some(v) = l.get("max_depth") {
                    max_depth = whole(v, ROLE_MAX_DEPTH);
                    if max_depth.is_none() {
                        c.bad(
                            "limits.max_depth",
                            format!("A whole number from 0 to {ROLE_MAX_DEPTH}."),
                        );
                    }
                }
            }
        }
    }

    if !c.problems.is_empty() {
        return Err(c.problems);
    }
    Ok(Role {
        id,
        name,
        avatar,
        purpose,
        instructions,
        cli,
        model,
        access,
        tools_allow,
        tools_deny,
        mcp,
        skills,
        secrets,
        delegates,
        max_subagents,
        max_depth,
    })
}

fn quoted(s: &str) -> String {
    serde_json::to_string(s).expect("a string always serialises")
}

fn strings(list: &[String]) -> String {
    format!("[{}]", list.iter().map(|s| quoted(s)).collect::<Vec<_>>().join(","))
}

/// `{"k":v,…}` from members already in key order; `v` is JSON text.
fn object(members: &[(&str, String)]) -> String {
    let body: Vec<String> = members.iter().map(|(k, v)| format!("{}:{v}", quoted(k))).collect();
    format!("{{{}}}", body.join(","))
}

impl Role {
    /// The canonical text (see the module comment). Two documents that mean
    /// the same role give the same text, whatever order their fields came in.
    pub fn canonical(&self) -> String {
        // Members in key order: access, avatar, cli, delegates, id,
        // instructions, limits, mcp, model, name, purpose, schema, secrets,
        // skills, tools.
        let mut m: Vec<(&str, String)> = vec![("access", quoted(&self.access))];
        if let Some(a) = &self.avatar {
            m.push((
                "avatar",
                object(&[("color", quoted(&a.color)), ("glyph", quoted(&a.glyph))]),
            ));
        }
        m.push(("cli", quoted(&self.cli)));
        if !self.delegates.is_empty() {
            m.push(("delegates", strings(&self.delegates)));
        }
        m.push(("id", quoted(&self.id)));
        if !self.instructions.is_empty() {
            m.push(("instructions", quoted(&self.instructions)));
        }
        let mut limits: Vec<(&str, String)> = Vec::new();
        if let Some(n) = self.max_depth {
            limits.push(("max_depth", n.to_string()));
        }
        if let Some(n) = self.max_subagents {
            limits.push(("max_subagents", n.to_string()));
        }
        if !limits.is_empty() {
            m.push(("limits", object(&limits)));
        }
        if !self.mcp.is_empty() {
            m.push(("mcp", strings(&self.mcp)));
        }
        if !self.model.is_empty() {
            let members: Vec<(&str, String)> = self.model.iter().map(|(k, v)| (k.as_str(), quoted(v))).collect();
            m.push(("model", object(&members)));
        }
        m.push(("name", quoted(&self.name)));
        if !self.purpose.is_empty() {
            m.push(("purpose", quoted(&self.purpose)));
        }
        m.push(("schema", ROLE_SCHEMA.to_string()));
        if !self.secrets.is_empty() {
            m.push(("secrets", strings(&self.secrets)));
        }
        if !self.skills.is_empty() {
            m.push(("skills", strings(&self.skills)));
        }
        let mut tools: Vec<(&str, String)> = Vec::new();
        if !self.tools_allow.is_empty() {
            tools.push(("allow", strings(&self.tools_allow)));
        }
        if !self.tools_deny.is_empty() {
            tools.push(("deny", strings(&self.tools_deny)));
        }
        if !tools.is_empty() {
            m.push(("tools", object(&tools)));
        }
        object(&m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_smallest_role_gets_the_least_access_on_any_cli() {
        let r = parse_role(&json!({"id": "ab", "name": "A"})).unwrap();
        assert_eq!((r.cli.as_str(), r.access.as_str()), ("any", "read-only"));
        assert_eq!(
            r.canonical(),
            r#"{"access":"read-only","cli":"any","id":"ab","name":"A","schema":1}"#
        );
    }

    #[test]
    fn the_canonical_members_are_in_key_order() {
        let r = parse_role(&json!({
            "id": "ab", "name": "A", "avatar": {"glyph": "A", "color": "red"}, "purpose": "p", "instructions": "i",
            "model": {"codex": "m", "claude": "n"}, "tools": {"allow": ["Read"], "deny": ["Write"]}, "mcp": ["a"],
            "skills": ["b"], "secrets": ["C"], "delegates": ["dd"], "limits": {"max_subagents": 1, "max_depth": 2}
        }))
        .unwrap();
        let text = r.canonical();
        let keys = [
            "access",
            "avatar",
            "cli",
            "delegates",
            "id",
            "instructions",
            "limits",
            "mcp",
            "model",
            "name",
            "purpose",
            "schema",
            "secrets",
            "skills",
            "tools",
        ];
        let at: Vec<usize> = keys.iter().map(|k| text.find(&format!("\"{k}\":")).expect(k)).collect();
        assert!(at.windows(2).all(|w| w[0] < w[1]), "{text}");
        assert!(text.contains(r#""model":{"claude":"n","codex":"m"}"#), "{text}");
        assert!(text.contains(r#""limits":{"max_depth":2,"max_subagents":1}"#), "{text}");
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parse_role(&v).unwrap(), r);
    }

    #[test]
    fn a_whole_float_counts_as_a_whole_number() {
        let r = parse_role(&json!({"id": "ab", "name": "A", "schema": 1.0, "limits": {"max_depth": 2.0}})).unwrap();
        assert_eq!(r.max_depth, Some(2));
        assert!(parse_role(&json!({"id": "ab", "name": "A", "limits": {"max_depth": 2.5}})).is_err());
    }
}
