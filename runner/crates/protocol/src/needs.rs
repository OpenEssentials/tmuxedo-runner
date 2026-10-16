//! "Needs you" on the wire (docs/protocol.md "Needs you", PLAN Phase 13):
//! the Rust twin of packages/protocol/src/needs.ts. Items the runner owns,
//! one set per pane in the `needs` frame, `needs.respond` params and the
//! link allowlist rule. Checked against packages/protocol/fixtures/needs.json.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Hello feature (runner sends `needs`, answers `needs.respond`) and welcome
/// feature (the gateway relays `needs`).
pub const FEATURE_NEEDS: &str = "needs";
pub const MAX_ITEMS: usize = 20;
pub const MAX_TITLE: usize = 120;
pub const MAX_TEXT: usize = 2000;
pub const MAX_OPTIONS: usize = 9;

/// `file`: an agent asked for a file (`tmuxedo files request`), answered by an upload.
pub const KINDS: &[&str] = &[
    "permission",
    "question",
    "plan",
    "login",
    "error",
    "limit",
    "review",
    "file",
];
pub const RESPONSE_KINDS: &[&str] = &[
    "approve",
    "deny",
    "reply",
    "choose",
    "answers",
    "approve_plan",
    "action",
    "open",
    "dismiss",
    "upload",
];
pub const REQUIRES: &[&str] = &["none", "control", "stepup"];
/// `upload`: a file request, answered by an upload naming `detail.upload.requestId`.
pub const ANSWER_VIA: &[&str] = &["hook", "keystroke", "action", "session", "none", "upload"];
pub const SOURCES: &[&str] = &["hook", "screen", "report", "core", "chat"];
pub const RISK_CLASSES: &[&str] = &[
    "git_push",
    "install",
    "admin",
    "delete_outside",
    "edit_outside",
    "network",
    "credentials",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NeedsOption {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NeedsResponse {
    pub id: String,
    pub kind: String,
    pub label: String,
    pub requires: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<Vec<NeedsOption>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action_id: Option<String>,
}

impl NeedsResponse {
    pub fn new(id: &str, kind: &str, label: &str, requires: &str) -> Self {
        NeedsResponse {
            id: id.into(),
            kind: kind.into(),
            label: label.into(),
            requires: requires.into(),
            options: None,
            action_id: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NeedsYou {
    pub id: String,
    pub pane_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_id: Option<String>,
    pub plugin: String,
    pub kind: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<Vec<String>>,
    pub risk: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub risk_class: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
    pub responses: Vec<NeedsResponse>,
    pub answer_via: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    pub source: String,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
}

/// A file request's id (`requestId` on an upload; needs.ts FILE_REQUEST_ID).
pub fn valid_file_request_id(s: &str) -> bool {
    (8..=64).contains(&s.len()) && s.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

pub fn valid_item_id(s: &str) -> bool {
    (1..=128).contains(&s.len()) && s.bytes().all(|c| c.is_ascii_alphanumeric() || b"_.:-".contains(&c))
}

pub fn valid_response_id(s: &str) -> bool {
    let b = s.as_bytes();
    (1..=32).contains(&b.len())
        && b[0].is_ascii_lowercase()
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"_.-".contains(c))
}

pub fn valid_option_id(s: &str) -> bool {
    (1..=32).contains(&s.len()) && s.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

fn valid_chat_id(s: &str) -> bool {
    (8..=64).contains(&s.len()) && s.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

fn valid_fingerprint(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

impl NeedsResponse {
    fn is_valid(&self) -> bool {
        valid_response_id(&self.id)
            && RESPONSE_KINDS.contains(&self.kind.as_str())
            && !self.label.is_empty()
            && self.label.chars().count() <= 40
            && REQUIRES.contains(&self.requires.as_str())
            && self.options.as_ref().is_none_or(|o| {
                o.len() <= MAX_OPTIONS
                    && o.iter()
                        .all(|x| valid_option_id(&x.id) && x.label.chars().count() <= 200)
            })
            && (self.kind != "choose" || self.options.as_ref().is_some_and(|o| !o.is_empty()))
            && self.action_id.as_deref().is_none_or(crate::plugins::valid_action_id)
            && (self.kind != "action" || self.action_id.is_some())
    }
}

impl NeedsYou {
    /// The same rules as needs.ts `parseNeedsItem` (the runner checks what it sends).
    pub fn is_valid(&self) -> bool {
        let mut ids = std::collections::BTreeSet::new();
        valid_item_id(&self.id)
            && self.pane_id.len() <= 255
            && self.chat_id.as_deref().is_none_or(valid_chat_id)
            && crate::plugins::valid_plugin_id(&self.plugin)
            && KINDS.contains(&self.kind.as_str())
            && !self.title.is_empty()
            && self.title.chars().count() <= MAX_TITLE
            && (self.risk == "normal" || self.risk == "high")
            && self.risk_class.as_deref().is_none_or(|c| RISK_CLASSES.contains(&c))
            && ANSWER_VIA.contains(&self.answer_via.as_str())
            && SOURCES.contains(&self.source.as_str())
            && self.created_at.len() <= 64
            && self.expires_at.as_ref().is_none_or(|e| e.len() <= 64)
            && self.fingerprint.as_deref().is_none_or(valid_fingerprint)
            && (self.answer_via != "keystroke" || self.fingerprint.is_some())
            && (self.answer_via != "session" || self.chat_id.is_some())
            && ((self.kind == "file") == (self.answer_via == "upload"))
            && (self.answer_via != "upload" || self.file_request().is_some())
            && (self.answer_via == "upload" || self.responses.iter().all(|r| r.kind != "upload"))
            && self.responses.len() <= 8
            && self.responses.iter().all(|r| r.is_valid() && ids.insert(r.id.clone()))
            && (self.source != "report" || self.responses.iter().all(|r| r.kind == "open" || r.kind == "dismiss"))
    }

    /// A `file` item's (project, request id): needs.ts `fileRequestOf`.
    pub fn file_request(&self) -> Option<(&str, &str)> {
        if self.kind != "file" || self.answer_via != "upload" {
            return None;
        }
        let u = self.detail.as_ref()?.get("upload")?;
        let id = u.get("requestId")?.as_str().filter(|s| valid_file_request_id(s))?;
        let project = u
            .get("project")?
            .as_str()
            .filter(|p| !p.is_empty() && p.chars().count() <= 200)?;
        Some((project, id))
    }

    pub fn response(&self, id: &str) -> Option<&NeedsResponse> {
        self.responses.iter().find(|r| r.id == id)
    }
}

/// A chat's item (chat.rs `NeedsItem`) as a `NeedsYou` (`answerVia: session`,
/// machine-level): the same mapping as needs.ts `fromChatItem`.
pub fn from_chat_item(session_id: &str, plugin: &str, it: &crate::chat::NeedsItem, created_at: &str) -> NeedsYou {
    let has = |o: &str| it.options.iter().any(|x| x == o);
    let mut responses = vec![];
    for (opt, kind, label) in [
        ("allow_once", "approve", "Allow once"),
        ("allow_session", "approve", "Allow for this chat"),
        ("answers", "answers", "Answer"),
        ("approve_plan", "approve_plan", "Approve plan"),
        ("keep_planning", "deny", "Keep planning"),
        ("deny", "deny", "Deny"),
    ] {
        if has(opt) {
            responses.push(NeedsResponse::new(opt, kind, label, "stepup"));
        }
    }
    responses.push(NeedsResponse::new("open", "open", "Open chat", "none"));
    responses.truncate(8);
    let mut detail = serde_json::Map::new();
    if let Some(t) = &it.tool {
        detail.insert("tool".into(), Value::String(t.clone()));
    }
    if let Some(i) = &it.input {
        detail.insert("input".into(), i.clone());
    }
    if !it.questions.is_empty() {
        detail.insert(
            "questions".into(),
            serde_json::to_value(&it.questions).unwrap_or(Value::Null),
        );
    }
    if let Some(m) = &it.markdown {
        detail.insert("markdown".into(), Value::String(m.clone()));
    }
    let title: String = if it.title.chars().count() > MAX_TITLE {
        let mut t: String = it.title.chars().take(MAX_TITLE - 1).collect();
        t.push('…');
        t
    } else if it.title.is_empty() {
        "Needs you".into()
    } else {
        it.title.clone()
    };
    NeedsYou {
        id: it.id.clone(),
        pane_id: String::new(),
        chat_id: Some(session_id.to_string()),
        plugin: if crate::plugins::valid_plugin_id(plugin) {
            plugin
        } else {
            "claude"
        }
        .to_string(),
        kind: match it.kind.as_str() {
            "question" => "question",
            "plan" => "plan",
            _ => "permission",
        }
        .into(),
        title,
        detail: (!detail.is_empty()).then_some(Value::Object(detail)),
        context: None,
        risk: if it.risk == "normal" { "normal" } else { "high" }.into(),
        risk_class: None,
        why: it.why.as_ref().map(|w| w.chars().take(200).collect()),
        responses,
        answer_via: "session".into(),
        fingerprint: None,
        source: "chat".into(),
        created_at: created_at.to_string(),
        expires_at: Some(it.expires_at.clone()),
    }
}

/// Parse one item; None for anything that breaks a rule (never panics).
pub fn parse_item(v: &Value) -> Option<NeedsYou> {
    let it: NeedsYou = serde_json::from_value(v.clone()).ok()?;
    it.is_valid().then_some(it)
}

/// `needs.respond` params (the same rules as needs.ts `parseRespondParams`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RespondParams {
    pub item_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_id: Option<String>,
    pub response_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub option_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answers: Option<serde_json::Map<String, Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    #[serde(rename = "fresh_stepup", default, skip_serializing_if = "Option::is_none")]
    pub fresh_stepup: Option<bool>,
}

/// A character never allowed in text that may be typed into a pane: C0
/// controls other than `\t`/`\n`, DEL and the C1 CSI (escape sequences,
/// carriage returns that would submit early, bracketed-paste ends).
pub fn forbidden_text_char(c: char) -> bool {
    (c < ' ' && c != '\t' && c != '\n') || c == '\u{7f}' || c == '\u{9b}'
}

pub fn parse_respond(v: &Value) -> Result<RespondParams, &'static str> {
    if !v.is_object() {
        return Err("not_an_object");
    }
    if !v["itemId"].as_str().is_some_and(valid_item_id) {
        return Err("bad_item_id");
    }
    if !v["responseId"].as_str().is_some_and(valid_response_id) {
        return Err("bad_response_id");
    }
    let p: RespondParams = serde_json::from_value(v.clone()).map_err(|_| "bad_params")?;
    if p.pane_id.as_ref().is_some_and(|s| s.len() > 255) {
        return Err("bad_pane_id");
    }
    if p.chat_id.as_deref().is_some_and(|c| !valid_chat_id(c)) {
        return Err("bad_chat_id");
    }
    if p.text
        .as_deref()
        .is_some_and(|t| t.chars().count() > MAX_TEXT || t.chars().any(forbidden_text_char))
    {
        return Err("bad_text");
    }
    if p.option_id.as_deref().is_some_and(|o| !valid_option_id(o)) {
        return Err("bad_option");
    }
    if p.by.as_ref().is_some_and(|b| b.chars().count() > 120) {
        return Err("bad_by");
    }
    Ok(p)
}

/// Whether `host` is on an allowlist: exact hosts, or `*.domain` for any
/// subdomain (not the bare domain). Case-insensitive, trailing dot ignored;
/// anything but `[a-z0-9.-]` never matches (userinfo, IPv6, unicode).
pub fn host_allowed(host: &str, allow: &[String]) -> bool {
    let h = host.to_ascii_lowercase();
    let h = h.strip_suffix('.').unwrap_or(&h);
    if h.is_empty()
        || !h
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'.' || c == b'-')
    {
        return false;
    }
    allow.iter().any(|raw| {
        let a = raw.to_ascii_lowercase();
        let a = a.strip_suffix('.').unwrap_or(&a);
        match a.strip_prefix("*.") {
            Some(base) => !base.is_empty() && h.len() > base.len() + 1 && h.ends_with(&format!(".{base}")),
            None => a == h,
        }
    })
}

/// The host of an http(s) URL as a browser would read it (after any
/// userinfo, before the port), lower-cased; None when it is not one.
pub fn url_host(url: &str) -> Option<String> {
    let l = url.trim();
    let rest = l
        .get(..8)
        .filter(|p| p.eq_ignore_ascii_case("https://"))
        .map(|_| &l[8..])
        .or_else(|| {
            l.get(..7)
                .filter(|p| p.eq_ignore_ascii_case("http://"))
                .map(|_| &l[7..])
        })?;
    let auth = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host_port = auth.rsplit('@').next().unwrap_or("");
    let host = if let Some(v6) = host_port.strip_prefix('[') {
        format!("[{}", v6.split(']').next().unwrap_or(""))
    } else {
        host_port.split(':').next().unwrap_or("").to_string()
    };
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_hosts() {
        assert_eq!(url_host("https://claude.ai/x").as_deref(), Some("claude.ai"));
        assert_eq!(url_host("HTTPS://Claude.AI:443/x").as_deref(), Some("claude.ai"));
        assert_eq!(
            url_host("https://claude.ai@evil.example/x").as_deref(),
            Some("evil.example")
        );
        assert_eq!(url_host("http://[::1]:8080/").as_deref(), Some("[::1"));
        assert_eq!(url_host("javascript:alert(1)"), None);
        assert_eq!(url_host("https:///x"), None);
    }

    #[test]
    fn text_typed_into_a_pane_has_no_controls() {
        assert!(forbidden_text_char('\r'));
        assert!(forbidden_text_char('\u{1b}'));
        assert!(forbidden_text_char('\u{9b}'));
        assert!(!forbidden_text_char('\n'));
        assert!(!forbidden_text_char('é'));
    }
}
