//! AI usage (docs/protocol.md "AI usage"): how much of a plan's limits an
//! agent CLI's account has used, as that CLI itself reported it. Numbers,
//! reset times and an opaque account key only: no tokens, keys, prompts,
//! paths, account names or vendor JSON. A value the CLI did not report is
//! left out, never guessed. Shared fixtures:
//! `packages/protocol/fixtures/usage.json`.

use serde::{Deserialize, Serialize};

/// The hello feature of a runner that sends `usage`.
pub const FEATURE_USAGE: &str = "usage";
/// At most this many windows in one record.
pub const USAGE_WINDOWS_MAX: usize = 8;
/// The runner sends at most one record per account in this many milliseconds.
pub const USAGE_MIN_INTERVAL_MS: u64 = 2000;
/// Where a record was read from.
pub const USAGE_SOURCES: &[&str] = &["statusline", "jsonl"];
pub const USAGE_MODES: &[&str] = &["plan", "metered"];
/// The usage alerts (percent of a window): near it, nearly there, and the
/// limit itself (which offers to resume at the reset). Each can be turned off
/// (`config.plugins[].settings.usage.alerts`); absent = all on.
pub const USAGE_ALERT_LEVELS: &[u8] = &[80, 95, 100];

/// `settings.usage.alerts`: the known levels in it, in order, once each.
/// Not a list = `None` (all on); an empty list = none.
pub fn parse_alert_levels(v: &serde_json::Value) -> Option<Vec<u8>> {
    let list = v.as_array()?;
    Some(
        USAGE_ALERT_LEVELS
            .iter()
            .copied()
            .filter(|l| list.iter().any(|x| x.as_u64() == Some(u64::from(*l))))
            .collect(),
    )
}

/// One limit window: `id` is `5h`, `7d` or `<minutes>m`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageWindow {
    pub id: String,
    /// Percent of the window used, 0..=100.
    #[serde(rename = "usedPct")]
    pub used_pct: f64,
    /// When the window resets, Unix seconds.
    #[serde(rename = "resetsAt", default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<u64>,
    #[serde(rename = "windowMins", default, skip_serializing_if = "Option::is_none")]
    pub window_mins: Option<u32>,
}

/// How full one pane's context window is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageContext {
    #[serde(rename = "paneId")]
    pub pane_id: String,
    #[serde(rename = "usedPct")]
    pub used_pct: f64,
    /// The context window's size in tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

/// The newest record for one account of one plugin on this machine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageRecord {
    pub plugin: String,
    /// Opaque: `h:<hex>` (a salted hash of the account) or `m:<hex>` (this
    /// machine's own key, when the account could not be told).
    #[serde(rename = "accountKey")]
    pub account_key: String,
    pub mode: String,
    /// The plan's name as the CLI reports it (`plus`); display only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    #[serde(default)]
    pub windows: Vec<UsageWindow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<UsageContext>,
    /// The id of a window that is used up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reached: Option<String>,
    pub source: String,
    /// Unix milliseconds.
    #[serde(rename = "observedAt")]
    pub observed_at: u64,
}

fn word(s: &str, max: usize, extra: &[u8]) -> bool {
    !s.is_empty()
        && s.len() <= max
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || extra.contains(&b))
}

fn pct(v: f64) -> bool {
    v.is_finite() && (0.0..=100.0).contains(&v)
}

/// A window id: `5h`, `7d`, `43200m`.
pub fn usage_id_ok(s: &str) -> bool {
    word(s, 24, b":_-")
}

/// A plan name: `max`, `plus`.
pub fn usage_plan_ok(s: &str) -> bool {
    word(s, 24, b"_-")
}

/// `h:` or `m:` and 16 to 64 hex digits.
pub fn account_key_ok(s: &str) -> bool {
    s.strip_prefix("h:").or_else(|| s.strip_prefix("m:")).is_some_and(|h| {
        (16..=64).contains(&h.len()) && h.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

impl UsageWindow {
    pub fn ok(&self) -> bool {
        usage_id_ok(&self.id)
            && pct(self.used_pct)
            && self.resets_at.is_none_or(|r| r > 0 && r < 100_000_000_000)
            && self.window_mins.is_none_or(|m| (1..=527_040).contains(&m))
    }
}

impl UsageRecord {
    /// The rules both codecs apply (`parseUsage` in usage.ts).
    pub fn ok(&self) -> bool {
        (2..=32).contains(&self.plugin.len())
            && word(&self.plugin, 32, b"-")
            && account_key_ok(&self.account_key)
            && USAGE_MODES.contains(&self.mode.as_str())
            && self.plan.as_deref().is_none_or(usage_plan_ok)
            && self.windows.len() <= USAGE_WINDOWS_MAX
            && self.windows.iter().all(UsageWindow::ok)
            && self.context.as_ref().is_none_or(|c| {
                !c.pane_id.is_empty()
                    && c.pane_id.len() <= 64
                    && c.pane_id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"_:-".contains(&b))
                    && pct(c.used_pct)
                    && c.size.is_none_or(|s| s <= 1_000_000_000)
            })
            && self.reached.as_deref().is_none_or(usage_id_ok)
            && USAGE_SOURCES.contains(&self.source.as_str())
    }
}

/// One limit window's share that a session used (`session.summary`'s `usage`):
/// the rise in the account's percentage that this session's own readings
/// showed, in percentage points. An estimate: another session or device on
/// the same account between two readings is counted here too.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpentWindow {
    pub id: String,
    #[serde(rename = "spentPct")]
    pub spent_pct: f64,
    #[serde(rename = "windowMins", default, skip_serializing_if = "Option::is_none")]
    pub window_mins: Option<u32>,
}

/// What one session used since a time: the windows' shares and the tokens
/// its agent reported (the agent's own running total; a restart counts anew).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionUsage {
    pub plugin: String,
    #[serde(default)]
    pub windows: Vec<SpentWindow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<u64>,
}

/// The most tokens a session summary carries (anything above is not believed).
pub const SESSION_TOKENS_MAX: u64 = 1_000_000_000_000;

impl SessionUsage {
    pub fn ok(&self) -> bool {
        word(&self.plugin, 32, b"-")
            && self.plugin.len() >= 2
            && self.windows.len() <= USAGE_WINDOWS_MAX
            && self.windows.iter().all(|w| {
                usage_id_ok(&w.id)
                    && w.spent_pct.is_finite()
                    && w.spent_pct > 0.0
                    && w.spent_pct <= 100.0 * 1000.0
                    && w.window_mins.is_none_or(|m| (1..=527_040).contains(&m))
            })
            && self.tokens.is_none_or(|t| t > 0 && t <= SESSION_TOKENS_MAX)
            && (!self.windows.is_empty() || self.tokens.is_some())
    }
}

#[cfg(test)]
mod session_usage_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_session_usage_is_numbers_only_and_never_empty() {
        let u: SessionUsage = serde_json::from_value(json!({
            "plugin": "claude", "windows": [{"id": "5h", "spentPct": 7.5, "windowMins": 300}], "tokens": 19755
        }))
        .unwrap();
        assert!(u.ok());
        assert_eq!(
            serde_json::to_value(&u).unwrap(),
            json!({"plugin": "claude", "windows": [{"id": "5h", "spentPct": 7.5, "windowMins": 300}], "tokens": 19755})
        );
        let empty = SessionUsage {
            plugin: "claude".into(),
            windows: vec![],
            tokens: None,
        };
        assert!(!empty.ok());
        let mut bad = u.clone();
        bad.windows[0].spent_pct = 0.0;
        assert!(!bad.ok());
        let mut bad = u.clone();
        bad.windows[0].id = "5 H".into();
        assert!(!bad.ok());
        let mut bad = u.clone();
        bad.tokens = Some(0);
        assert!(!bad.ok());
        let mut bad = u;
        bad.plugin = "Claude".into();
        assert!(!bad.ok());
    }
}
