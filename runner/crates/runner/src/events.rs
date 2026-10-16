//! Runner API events: the envelope stream behind `events.subscribe`
//! and `events.wait` on the local socket.
//!
//! Lifecycle subscriptions (`workspace.created`, `pane.closed`, …) deliver
//! envelopes `{"event":"pane_closed","data":{"type":…}}`;
//! `pane.agent_status_changed` and `pane.output_matched` deliver
//! `SubscriptionEventEnvelope`s `{"event":"pane.agent_status_changed","data":{…}}`.

use regex::Regex;
use serde_json::{json, Value};

#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    /// `workspace_created`, `pane_agent_status_changed`, …
    pub kind: String,
    /// The event's data (has `type` = kind).
    pub data: Value,
}

impl Event {
    pub fn new(kind: &str, mut data: Value) -> Self {
        data["type"] = json!(kind);
        Self {
            kind: kind.into(),
            data,
        }
    }
    pub fn envelope(&self) -> Value {
        json!({ "event": self.kind, "data": self.data })
    }
}

/// Look a field up in event data: top level, then inside workspace/tab/pane.
pub fn field<'a>(data: &'a Value, name: &str) -> Option<&'a Value> {
    data.get(name)
        .or_else(|| data.get("workspace").and_then(|w| w.get(name)))
        .or_else(|| data.get("tab").and_then(|w| w.get(name)))
        .or_else(|| data.get("pane").and_then(|w| w.get(name)))
        .filter(|v| !v.is_null())
}

/// An event match: `event` names the kind; every other non-null field must equal.
pub fn event_matches(m: &Value, ev: &Event) -> bool {
    let Some(kind) = m["event"].as_str() else { return false };
    if kind != ev.kind {
        return false;
    }
    let Some(obj) = m.as_object() else { return false };
    obj.iter()
        .filter(|(k, v)| k.as_str() != "event" && !v.is_null())
        .all(|(k, v)| match k.as_str() {
            "min_revision" => field(&ev.data, "revision").and_then(Value::as_u64) >= v.as_u64(),
            _ => field(&ev.data, k) == Some(v),
        })
}

#[derive(Debug, Clone)]
pub enum OutputMatcher {
    Substring(String),
    Regex(Regex),
}

impl OutputMatcher {
    pub fn parse(v: &Value) -> Result<Self, (String, String)> {
        let value = v["value"].as_str().unwrap_or_default().to_string();
        match v["type"].as_str() {
            Some("substring") => Ok(Self::Substring(value)),
            Some("regex") => Regex::new(&value)
                .map(Self::Regex)
                .map_err(|e| ("invalid_regex".into(), e.to_string())),
            _ => Err(("invalid_params".into(), "match.type must be substring or regex".into())),
        }
    }
    /// The first matching line.
    pub fn find(&self, text: &str) -> Option<String> {
        text.lines()
            .find(|l| match self {
                Self::Substring(s) => l.contains(s.as_str()),
                Self::Regex(r) => r.is_match(l),
            })
            .map(str::to_owned)
            .or_else(|| match self {
                // A regex may span lines.
                Self::Regex(r) => r.find(text).map(|m| m.as_str().to_string()),
                Self::Substring(_) => None,
            })
    }
}

#[derive(Debug, Clone)]
pub enum Filter {
    /// `workspace.created` → event kind `workspace_created`.
    Lifecycle(String),
    AgentStatus {
        pane: String,
        status: Option<String>,
    },
    Output {
        pane: String,
        source: String,
        lines: Option<usize>,
        matcher: OutputMatcher,
        /// Last matched line, so the same screen does not fire twice.
        last: Option<String>,
    },
    /// Accepted, never fires (tmuxedo has no viewer-side scroll state).
    Scroll,
}

impl Filter {
    pub fn parse(v: &Value) -> Result<Filter, (String, String)> {
        let t = v["type"].as_str().unwrap_or_default();
        match t {
            "pane.agent_status_changed" => Ok(Filter::AgentStatus {
                pane: v["pane_id"].as_str().unwrap_or_default().into(),
                status: v["agent_status"].as_str().map(str::to_owned),
            }),
            "pane.output_matched" => Ok(Filter::Output {
                pane: v["pane_id"].as_str().unwrap_or_default().into(),
                source: v["source"].as_str().unwrap_or("recent").into(),
                lines: v["lines"].as_u64().map(|n| n as usize),
                matcher: OutputMatcher::parse(&v["match"])?,
                last: None,
            }),
            "pane.scroll_changed" => Ok(Filter::Scroll),
            t if t.contains('.') => Ok(Filter::Lifecycle(t.replace('.', "_"))),
            _ => Err(("invalid_params".into(), format!("unknown subscription {t:?}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matching() {
        let ev = Event::new(
            "pane_agent_status_changed",
            json!({"pane_id":"w1:p1","workspace_id":"w1","agent_status":"blocked"}),
        );
        assert_eq!(ev.envelope()["data"]["type"], "pane_agent_status_changed");
        assert!(event_matches(
            &json!({"event":"pane_agent_status_changed","pane_id":"w1:p1","agent_status":"blocked"}),
            &ev
        ));
        assert!(!event_matches(
            &json!({"event":"pane_agent_status_changed","pane_id":"w1:p1","agent_status":"done"}),
            &ev
        ));
        assert!(!event_matches(&json!({"event":"pane_closed","pane_id":"w1:p1"}), &ev));
        let ws = Event::new("workspace_created", json!({"workspace":{"workspace_id":"w2"}}));
        assert!(event_matches(
            &json!({"event":"workspace_created","workspace_id":"w2"}),
            &ws
        ));
        assert!(event_matches(
            &json!({"event":"workspace_created","workspace_id":null}),
            &ws
        ));
        let m = OutputMatcher::parse(&json!({"type":"regex","value":"^n[0-9]+$"})).unwrap();
        assert_eq!(m.find("a\nn12\nb"), Some("n12".into()));
        assert!(OutputMatcher::parse(&json!({"type":"regex","value":"("})).is_err());
        assert!(
            matches!(Filter::parse(&json!({"type":"tab.closed"})).unwrap(), Filter::Lifecycle(k) if k == "tab_closed")
        );
    }
}
