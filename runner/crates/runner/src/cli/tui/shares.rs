//! Shared sessions in the TUI (Phase 27).
//!
//! - Your own sessions that someone can see wear a `⇄` mark in the sidebar
//!   (`GET /api/shares?shared=byme`, token scope `read`).
//! - "Shared with me" lists the sessions others shared with you
//!   (`GET /api/shares?shared=me`), only when there is one. Enter opens the
//!   session in the browser: a guest's terminal is a signed-in browser's
//!   (or the phone app's) only, never an API token's, so the TUI does not
//!   dial it itself.
//!
//! Both are read with the machines every 15 s; a server without them (an
//! older one) or a token without `read` simply shows nothing.

use serde_json::Value;
use std::collections::HashSet;

/// One session shared with you.
#[derive(Debug, Clone, PartialEq)]
pub struct WithMe {
    pub id: String,
    pub session: String,
    pub owner: String,
    /// `watch` or `type`.
    pub role: String,
    /// Milliseconds since the epoch (None: unreadable).
    pub until_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Shares {
    /// `(machine, pane)` of your own live shares.
    pub by_me: HashSet<(String, String)>,
    pub with_me: Vec<WithMe>,
}

impl Shares {
    /// From the two answers' `shares` arrays; anything malformed is skipped.
    pub fn from_json(by_me: &Value, with_me: &Value) -> Shares {
        let list = |v: &Value| v["shares"].as_array().cloned().unwrap_or_default();
        let by_me = list(by_me)
            .iter()
            .filter_map(|s| Some((s["machineId"].as_str()?.to_string(), s["paneId"].as_str()?.to_string())))
            .collect();
        let with_me = list(with_me)
            .iter()
            .filter_map(|s| {
                Some(WithMe {
                    id: s["id"].as_str()?.to_string(),
                    session: s["sessionName"].as_str().unwrap_or("session").to_string(),
                    owner: s["owner"]["name"].as_str().unwrap_or("someone").to_string(),
                    role: s["role"].as_str().unwrap_or("watch").to_string(),
                    until_ms: s["expiresAt"].as_str().and_then(crate::chat::log::parse_iso_ms),
                })
            })
            .collect();
        Shares { by_me, with_me }
    }

    pub fn is_shared(&self, machine: &str, pane: &str) -> bool {
        self.by_me.contains(&(machine.to_string(), pane.to_string()))
    }
}

/// "45 min left", "3 h left", "2 days left", "ended" (the web's words, rounded up).
pub fn time_left(until_ms: Option<u64>, now_ms: u64) -> String {
    let Some(until) = until_ms else { return String::new() };
    if until <= now_ms {
        return "ended".into();
    }
    let min = (until - now_ms).div_ceil(60_000);
    if min < 60 {
        return format!("{min} min left");
    }
    let h = min.div_ceil(60);
    if h < 48 {
        return format!("{h} h left");
    }
    format!("{} days left", h.div_ceil(24))
}

/// The sidebar row: "build · Owner · watch only · 3 h left".
pub fn row_text(s: &WithMe, now_ms: u64) -> String {
    let how = if s.role == "type" { "you can type" } else { "watch only" };
    let left = time_left(s.until_ms, now_ms);
    if left.is_empty() {
        format!("{} · {} · {how}", s.session, s.owner)
    } else {
        format!("{} · {} · {how} · {left}", s.session, s.owner)
    }
}

/// The guest page for a share on the web app.
pub fn open_url(web_url: &str, id: &str) -> String {
    let safe: String = id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    format!("{}/shared/{safe}", web_url.trim_end_matches('/'))
}

/// Both lists, with the CLI's token. An answer other than 200 reads as empty (older server, no `read`).
pub fn fetch(cfg: &super::super::CliConfig) -> Result<Shares, String> {
    let get = |path: &str| match super::super::api(cfg, "GET", path, None) {
        Ok((200, v)) => Ok(v),
        Ok(_) => Ok(Value::Null),
        Err(e) => Err(format!("offline: {e}")),
    };
    Ok(Shares::from_json(
        &get("/api/shares?shared=byme")?,
        &get("/api/shares?shared=me")?,
    ))
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_both_lists_and_skips_malformed_rows() {
        let s = Shares::from_json(
            &json!({ "shares": [{ "machineId": "m1", "paneId": "w1:p2" }, { "machineId": "m1" }] }),
            &json!({ "shares": [
                { "id": "s1", "sessionName": "build", "owner": { "name": "Owner" }, "role": "type", "expiresAt": "2026-10-17T12:00:00.000Z" },
                { "sessionName": "no id" }
            ] }),
        );
        assert!(s.is_shared("m1", "w1:p2"));
        assert!(!s.is_shared("m1", "w1:p3"));
        assert_eq!(s.with_me.len(), 1);
        assert_eq!(s.with_me[0].owner, "Owner");
        let now = crate::chat::log::parse_iso_ms("2026-10-17T09:00:00.000Z").unwrap();
        assert_eq!(row_text(&s.with_me[0], now), "build · Owner · you can type · 3 h left");
        // An older server (404 → null) shows nothing.
        assert_eq!(Shares::from_json(&Value::Null, &Value::Null), Shares::default());
    }

    #[test]
    fn time_left_rounds_up_and_ends() {
        assert_eq!(time_left(Some(60_000 * 45), 0), "45 min left");
        assert_eq!(time_left(Some(60_000 * 61), 0), "2 h left");
        assert_eq!(time_left(Some(60_000 * 60 * 72), 0), "3 days left");
        assert_eq!(time_left(Some(5), 10), "ended");
        assert_eq!(time_left(None, 10), "");
    }

    #[test]
    fn opens_the_guest_page_with_a_clean_id() {
        assert_eq!(
            open_url("https://app.example.test/", "s_1"),
            "https://app.example.test/shared/s_1"
        );
        assert_eq!(
            open_url("https://app.example.test", "../x?y"),
            "https://app.example.test/shared/xy"
        );
    }
}
