//! `[usage]` in a plugin manifest (docs/design/agent-usage.md §2): where the
//! agent CLI itself reports how much of a plan's limits is used, and which
//! fields hold the numbers. The host knows the source kinds; the field names
//! are the plugin's. Parsed strictly: an unknown key or kind is refused.

use super::manifest::Manifest;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Capability a manifest with `[usage]` must declare.
pub const USAGE_CAPABILITY: &str = "usage.report";
/// Source kinds this host reads (a closed set).
pub const USAGE_SOURCE_KINDS: &[&str] = &["statusline", "jsonl_tail"];

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageSection {
    /// `plan` (limits and resets) and/or `metered` (tokens and cost).
    #[serde(default)]
    pub modes: Vec<String>,
    /// Where the vendor shows usage, for a CLI with no source to read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
    #[serde(default)]
    pub sources: Vec<UsageSource>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageSource {
    /// One of [`USAGE_SOURCE_KINDS`].
    pub kind: String,
    #[serde(default)]
    pub windows: Vec<UsageWindowMap>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<UsageContextMap>,
    /// Field with the plan's name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    /// Fields whose whole numbers add up to the session's running token
    /// total (what the agent says this session used so far; at most 4).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tokens: Vec<String>,
    /// `statusline`: field with the model's display name (for the short line).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// `jsonl_tail`: the hook field that names the session log.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path_field: Option<String>,
    /// `jsonl_tail`: the folder the log must be in (`{home}/…`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    /// `jsonl_tail`: field → the text it must equal for a line to count.
    #[serde(default)]
    pub select: BTreeMap<String, String>,
}

/// One limit window: the object at `at`, and the fields in it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageWindowMap {
    pub at: String,
    pub pct: String,
    /// Field with the reset time, Unix seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets: Option<String>,
    /// A fixed id (`5h`); without one it comes from the window's length.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The window's length in minutes: fixed, or read from `mins_field`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mins: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mins_field: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageContextMap {
    pub pct: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<String>,
}

impl UsageSource {
    /// The record's `source` for this kind.
    pub fn wire_name(&self) -> &'static str {
        match self.kind.as_str() {
            "jsonl_tail" => "jsonl",
            _ => "statusline",
        }
    }
}

impl UsageSection {
    pub fn source(&self, kind: &str) -> Option<&UsageSource> {
        self.sources.iter().find(|s| s.kind == kind)
    }

    pub fn source_by_wire(&self, wire: &str) -> Option<&UsageSource> {
        self.sources
            .iter()
            .find(|s| USAGE_SOURCE_KINDS.contains(&s.kind.as_str()) && s.wire_name() == wire)
    }
}

/// `a.b.c`: names of JSON object keys, nothing else.
fn field_ok(f: &str) -> bool {
    f.len() <= 120
        && f.split('.')
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'))
}

impl Manifest {
    pub(super) fn lint_usage(&self, e: &mut Vec<String>) {
        let Some(u) = &self.usage else { return };
        if !self.has(USAGE_CAPABILITY) {
            e.push(format!("[usage] needs capability {USAGE_CAPABILITY}"));
        }
        if u.modes.is_empty() {
            e.push("usage.modes is empty".into());
        }
        for m in &u.modes {
            if !tmuxedo_protocol::usage::USAGE_MODES.contains(&m.as_str()) {
                e.push(format!(
                    "usage.modes {m:?} is not one of {:?}",
                    tmuxedo_protocol::usage::USAGE_MODES
                ));
            }
        }
        if let Some(l) = &u.link {
            if !l.starts_with("https://") || l.len() > 200 || l.chars().any(|c| c.is_whitespace() || c.is_control()) {
                e.push("usage.link must be an https URL of at most 200 characters".into());
            }
        }
        if u.sources.is_empty() && u.link.is_none() {
            e.push("[usage] has no sources and no link".into());
        }
        for (i, s) in u.sources.iter().enumerate() {
            let w = format!("usage.sources[{i}]");
            if !USAGE_SOURCE_KINDS.contains(&s.kind.as_str()) {
                e.push(format!("{w}.kind {:?} is not one of {USAGE_SOURCE_KINDS:?}", s.kind));
                continue;
            }
            if u.sources[..i].iter().any(|o| o.kind == s.kind) {
                e.push(format!("{w}: a second {:?} source", s.kind));
            }
            let mut fields: Vec<(&str, &String)> = vec![];
            for (j, m) in s.windows.iter().enumerate() {
                fields.push(("windows.at", &m.at));
                fields.push(("windows.pct", &m.pct));
                fields.extend(m.resets.iter().map(|f| ("windows.resets", f)));
                fields.extend(m.mins_field.iter().map(|f| ("windows.mins_field", f)));
                if m.id.is_none() && m.mins.is_none() && m.mins_field.is_none() {
                    e.push(format!("{w}.windows[{j}] needs id, mins or mins_field"));
                }
                if m.id
                    .as_deref()
                    .is_some_and(|id| !tmuxedo_protocol::usage::usage_id_ok(id))
                {
                    e.push(format!("{w}.windows[{j}].id is not a window id"));
                }
                if m.mins.is_some_and(|n| !(1..=527_040).contains(&n)) {
                    e.push(format!("{w}.windows[{j}].mins is out of range"));
                }
            }
            if s.windows.len() > tmuxedo_protocol::usage::USAGE_WINDOWS_MAX {
                e.push(format!("{w}: more than 8 windows"));
            }
            if let Some(c) = &s.context {
                fields.push(("context.pct", &c.pct));
                fields.extend(c.size.iter().map(|f| ("context.size", f)));
            }
            fields.extend(s.plan.iter().map(|f| ("plan", f)));
            fields.extend(s.tokens.iter().map(|f| ("tokens", f)));
            if s.tokens.len() > 4 {
                e.push(format!("{w}: more than 4 token fields"));
            }
            fields.extend(s.model.iter().map(|f| ("model", f)));
            fields.extend(s.path_field.iter().map(|f| ("path_field", f)));
            fields.extend(s.select.keys().map(|f| ("select", f)));
            for (name, f) in fields {
                if !field_ok(f) {
                    e.push(format!("{w}.{name} {f:?} is not a field path"));
                }
            }
            if s.kind == "statusline" {
                if s.path_field.is_some() || s.root.is_some() || !s.select.is_empty() {
                    e.push(format!("{w}: path_field, root and select belong to jsonl_tail"));
                }
                if !self.generate.values().any(|g| g.builder == "hook-settings") {
                    e.push(format!("{w}: statusline needs a hook-settings generator"));
                }
            } else {
                if s.model.is_some() {
                    e.push(format!("{w}: model belongs to statusline"));
                }
                if s.path_field.is_none() || self.hooks.is_none() || !self.has("hooks.receive") {
                    e.push(format!(
                        "{w}: jsonl_tail needs path_field, [hooks] and capability hooks.receive"
                    ));
                }
                let root_ok = s.root.as_deref().is_some_and(|r| {
                    r.strip_prefix("{home}/")
                        .is_some_and(|rest| !rest.is_empty() && !rest.split('/').any(|p| p == ".." || p.is_empty()))
                });
                if !root_ok {
                    e.push(format!("{w}.root must be a folder under {{home}}/"));
                }
                if s.select.is_empty() {
                    e.push(format!("{w}: jsonl_tail needs select"));
                }
            }
        }
    }
}
