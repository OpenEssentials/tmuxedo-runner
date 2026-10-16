//! Agent login health (PLAN Phase 5 "Claude auth-health detection and
//! alert"; docs/design/plugins.md §7). The rules come from the plugin
//! manifest's `[auth]` section; this module is the state machine. The
//! runner never reads or stores credentials; it only looks for signals that
//! the machine's agent is (not) logged in:
//!
//! - probes: `env_present` (e.g. `ANTHROPIC_API_KEY` in the runner's
//!   environment counts as logged in) and `file_exists` (Claude's
//!   `~/.claude/.credentials.json` on Linux; on macOS the login lives in
//!   the Keychain, so the baseline there is "ok" unless a failure says
//!   otherwise);
//! - screen: a pane shows one of `auth.screen` (Claude's "Login expired" /
//!   "Please run /login"), matched case-insensitively on the detection text;
//! - hooks: `turn.failed` with an error in `auth.failure_errors`.
//!
//! Recovery: the credentials file changes after the failure (a fresh
//! `/login`), or a turn completes. Per decision 0001 the access token's
//! short `expiresAt` is NOT a signal: Claude refreshes it itself.
//!
//! State goes out as `hello.runner.claudeAuth` and every transition as a
//! durable event `auth_ok` / `auth_missing` / `auth_expired` / `auth_failed`.

use crate::plugin::{self, AgentPlugin, Ctx};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AuthState {
    Ok,
    /// Logged in, but the stored login is old enough that it may lapse
    /// soon (a `file_json_age` probe).
    Expiring,
    Missing,
    Expired,
    Failed,
}

impl AuthState {
    pub fn as_str(self) -> &'static str {
        match self {
            AuthState::Ok => "ok",
            AuthState::Expiring => "expiring",
            AuthState::Missing => "missing",
            AuthState::Expired => "expired",
            AuthState::Failed => "failed",
        }
    }
    /// The durable event type for a transition into this state.
    pub fn event(self) -> &'static str {
        match self {
            AuthState::Ok => "auth_ok",
            AuthState::Expiring => "auth_expiring",
            AuthState::Missing => "auth_missing",
            AuthState::Expired => "auth_expired",
            AuthState::Failed => "auth_failed",
        }
    }
}

/// Claude's screen rules (id + needle, case-insensitive): a compatibility
/// mirror of the manifest's `auth.screen`; a unit test keeps them equal.
pub const SCREEN_RULES: &[(&str, &str)] = &[("login_expired", "login expired"), ("run_login", "please run /login")];

/// The id of the first Claude screen rule matching `text`.
pub fn screen_rule(text: &str) -> Option<&'static str> {
    plugin::claude().auth_screen_rule(text)
}

/// The login state a Claude screen rule means (its manifest `state`;
/// `expired` unless it says `failed`).
pub fn screen_state(rule: &str) -> AuthState {
    let failed = plugin::claude()
        .manifest()
        .auth
        .as_ref()
        .and_then(|a| a.screen.iter().find(|r| r.id == rule))
        .is_some_and(|r| r.state == "failed");
    if failed {
        AuthState::Failed
    } else {
        AuthState::Expired
    }
}

/// The reason code for Claude's "your organization has disabled Claude
/// subscription access for Claude Code" (the manifest's screen rule id):
/// the signed-in account works, but its organization turned Claude Code off.
pub const ORG_DISABLED: &str = "org_disabled";

/// Claude `StopFailure.error_type` values that mean the login is unusable.
pub fn is_auth_error(error_type: &str) -> bool {
    plugin::claude().is_auth_error(error_type)
}

/// Claude's credentials file (the manifest's `file_exists` probe).
pub fn credentials_path(home: &str) -> PathBuf {
    file_probe(plugin::claude(), home, None).unwrap_or_else(|| Path::new(home).join(".claude/.credentials.json"))
}

/// The plugin's `file_exists` probe path for `home`, if one applies to `os`
/// (None = any OS).
pub(crate) fn file_probe(plugin: &dyn AgentPlugin, home: &str, os: Option<&str>) -> Option<PathBuf> {
    let a = plugin.manifest().auth.as_ref()?;
    a.probes
        .iter()
        .find(|p| p.kind == "file_exists" && os.is_none_or(|o| p.os.is_empty() || p.os.iter().any(|x| x == o)))
        .map(|p| {
            PathBuf::from(plugin::resolve(
                &p.path,
                &Ctx {
                    home: Some(home),
                    ..Ctx::default()
                },
            ))
        })
}

/// A transition to report.
#[derive(Debug, Clone, PartialEq)]
pub struct Change {
    pub state: AuthState,
    pub reason: String,
    pub source: &'static str,
    pub pane: Option<String>,
}

#[derive(Debug)]
pub struct AuthHealth {
    pub state: AuthState,
    pub reason: String,
    pub since: String,
    creds: PathBuf,
    /// The credentials file's mtime when the current failure was recorded.
    creds_at_failure: Option<SystemTime>,
    check_file: bool,
    api_key: bool,
    api_key_reason: String,
    unprobed_reason: String,
    /// `file_json_age`: (file, field, warn_days).
    age: Option<(PathBuf, String, u32)>,
    /// The last `command` probe's answer: once it has spoken, the file's
    /// presence alone no longer moves the state (a keyring login has no
    /// file; the command is the ground truth).
    command_says: Option<bool>,
    /// The login file's mtime when the command last spoke: a file that
    /// appears or changes after that (a fresh `login`) counts again.
    creds_at_command: Option<SystemTime>,
}

impl AuthHealth {
    /// Claude's login health. `check_file`: whether the credentials file is
    /// the login store (Linux); `api_key`: `ANTHROPIC_API_KEY` is set.
    pub fn new(home: &str, check_file: bool, api_key: bool) -> Self {
        Self::build(plugin::claude(), credentials_path(home), check_file, api_key)
    }

    /// A plugin's login health from its manifest probes: `env_present`
    /// probes are checked with `env`; the `file_exists` probe for `os`
    /// (`linux` / `darwin`) becomes the credentials file.
    pub fn for_plugin(plugin: &dyn AgentPlugin, home: &str, os: &str, env: &dyn Fn(&str) -> bool) -> Self {
        let a = plugin.manifest().auth.clone().unwrap_or_default();
        let api_key = a.probes.iter().any(|p| p.kind == "env_present" && env(&p.name));
        let creds = file_probe(plugin, home, Some(os));
        let mut h = Self::build(plugin, creds.clone().unwrap_or_default(), creds.is_some(), api_key);
        h.age = a
            .probes
            .iter()
            .find(|p| p.kind == "file_json_age" && (p.os.is_empty() || p.os.iter().any(|x| x == os)))
            .map(|p| {
                let path = plugin::resolve(
                    &p.path,
                    &Ctx {
                        home: Some(home),
                        ..Ctx::default()
                    },
                );
                (PathBuf::from(path), p.field.clone(), p.warn_days)
            });
        if h.state == AuthState::Ok {
            if let Some(why) = h.aging() {
                h.state = AuthState::Expiring;
                h.reason = why;
            }
        }
        h
    }

    /// `file_json_age`: "last refreshed N days ago" when older than the
    /// threshold. Only the age is kept; the file's other contents are never
    /// looked at past the one field.
    fn aging(&self) -> Option<String> {
        let (path, field, warn) = self.age.as_ref()?;
        let text = std::fs::read_to_string(path).ok()?;
        let v: serde_json::Value = serde_json::from_str(&text).ok()?;
        let at = v.get(field)?.as_str()?;
        let days = age_days(at)?;
        (days > i64::from(*warn)).then(|| format!("last refreshed {days} days ago"))
    }

    fn build(plugin: &dyn AgentPlugin, creds: PathBuf, check_file: bool, api_key: bool) -> Self {
        let a = plugin.manifest().auth.clone().unwrap_or_default();
        let api_key_reason = a
            .probes
            .iter()
            .find(|p| p.kind == "env_present")
            .map(|p| p.name.clone())
            .unwrap_or_else(|| "api key".into());
        let mut h = Self {
            state: AuthState::Ok,
            reason: String::new(),
            since: crate::util::iso_now(),
            creds,
            creds_at_failure: None,
            check_file,
            api_key,
            api_key_reason,
            unprobed_reason: if a.unprobed_reason.is_empty() {
                "not inspected".into()
            } else {
                a.unprobed_reason
            },
            age: None,
            command_says: None,
            creds_at_command: None,
        };
        if let Some((s, why)) = h.baseline() {
            h.state = s;
            h.reason = why;
        }
        h
    }

    fn creds_mtime(&self) -> Option<SystemTime> {
        std::fs::metadata(&self.creds).and_then(|m| m.modified()).ok()
    }

    fn baseline(&self) -> Option<(AuthState, String)> {
        if self.api_key {
            return Some((AuthState::Ok, self.api_key_reason.clone()));
        }
        if !self.check_file {
            return Some((AuthState::Ok, self.unprobed_reason.clone()));
        }
        Some(match self.creds_mtime() {
            Some(_) => (AuthState::Ok, "credentials present".into()),
            None => (AuthState::Missing, format!("no {}", self.creds.display())),
        })
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::json!({ "state": self.state, "reason": self.reason, "since": self.since })
    }

    fn set(&mut self, state: AuthState, reason: String, source: &'static str, pane: Option<String>) -> Option<Change> {
        if state == self.state {
            return None;
        }
        self.state = state;
        self.reason = reason.clone();
        self.since = crate::util::iso_now();
        self.creds_at_failure = matches!(state, AuthState::Expired | AuthState::Failed)
            .then(|| self.creds_mtime())
            .flatten();
        Some(Change {
            state,
            reason,
            source,
            pane,
        })
    }

    /// Periodic file check: appearance/disappearance, or a fresh login
    /// after a failure (the file changed since).
    pub fn check_credentials(&mut self) -> Option<Change> {
        if !self.check_file || self.api_key {
            return None;
        }
        let now = self.creds_mtime();
        // Present and logged in: fresh or ageing (`file_json_age`).
        if now.is_some() && matches!(self.state, AuthState::Ok | AuthState::Expiring) && self.age.is_some() {
            return match (self.state, self.aging()) {
                (AuthState::Ok, Some(why)) => self.set(AuthState::Expiring, why, "credentials", None),
                (AuthState::Expiring, None) => {
                    self.set(AuthState::Ok, "credentials renewed".into(), "credentials", None)
                }
                _ => None,
            };
        }
        if self.command_says.is_some()
            && now == self.creds_at_command
            && matches!((self.state, now), (AuthState::Missing, Some(_)) | (AuthState::Ok, None))
        {
            return None; // the status command decides presence
        }
        match (self.state, now) {
            (AuthState::Missing, Some(_)) => self.set(AuthState::Ok, "credentials present".into(), "credentials", None),
            (AuthState::Ok, None) => {
                let why = format!("no {}", self.creds.display());
                self.set(AuthState::Missing, why, "credentials", None)
            }
            (AuthState::Expired | AuthState::Failed, Some(t)) if Some(t) != self.creds_at_failure => {
                self.set(AuthState::Ok, "credentials renewed".into(), "credentials", None)
            }
            (AuthState::Expired | AuthState::Failed, None) if self.command_says.is_none() => {
                let why = format!("no {}", self.creds.display());
                self.set(AuthState::Missing, why, "credentials", None)
            }
            _ => None,
        }
    }

    /// The `command` probe answered: logged in or not (None = it could not
    /// tell, e.g. not installed or junk output: no change).
    pub fn command_result(&mut self, logged_in: Option<bool>) -> Option<Change> {
        let li = logged_in?;
        self.command_says = Some(li);
        self.creds_at_command = self.creds_mtime();
        if self.api_key {
            return None;
        }
        match (li, self.state) {
            (true, AuthState::Missing) => self.set(AuthState::Ok, "status: logged in".into(), "command", None),
            (true, AuthState::Expired | AuthState::Failed) => {
                // Only a login that changed since the failure clears it (a
                // status check may still say "logged in" with a dead token).
                if self.check_file && self.creds_mtime().is_some() && self.creds_mtime() != self.creds_at_failure {
                    self.set(AuthState::Ok, "credentials renewed".into(), "command", None)
                } else {
                    None
                }
            }
            (false, AuthState::Ok | AuthState::Expiring) => {
                self.set(AuthState::Missing, "status: not logged in".into(), "command", None)
            }
            _ => None,
        }
    }

    /// A pane newly shows a login message: `state` is `expired` or `failed`.
    pub fn screen_hit(&mut self, pane: &str, rule: &str, state: &str) -> Option<Change> {
        if state == "failed" {
            return self.set(AuthState::Failed, rule.to_string(), "screen", Some(pane.to_string()));
        }
        self.screen_expired(pane, rule)
    }

    /// A pane newly shows a login-expired message.
    pub fn screen_expired(&mut self, pane: &str, rule: &str) -> Option<Change> {
        if self.state == AuthState::Failed {
            return None; // a specific hook failure is more informative
        }
        self.set(AuthState::Expired, rule.to_string(), "screen", Some(pane.to_string()))
    }

    /// A pane (or a chat) newly shows the text of screen rule `rule`, which
    /// means `state`: `expired` as [`Self::screen_expired`]; `failed` (the
    /// login cannot be used, e.g. [`ORG_DISABLED`]) is set even over another
    /// failure, since the text says exactly what is wrong.
    pub fn text_hit(&mut self, pane: &str, rule: &str, state: AuthState, source: &'static str) -> Option<Change> {
        if state != AuthState::Failed {
            return self.screen_expired(pane, rule);
        }
        if self.state == AuthState::Failed && self.reason == rule {
            return None;
        }
        if self.state == AuthState::Failed {
            // Same state, a more precise reason: still a transition to report.
            self.state = AuthState::Ok;
        }
        self.set(AuthState::Failed, rule.to_string(), source, Some(pane.to_string()))
    }

    /// `turn.failed` with an auth error.
    pub fn hook_failure(&mut self, pane: &str, error_type: &str) -> Option<Change> {
        self.set(
            AuthState::Failed,
            error_type.to_string(),
            "hook",
            Some(pane.to_string()),
        )
    }

    /// A completed turn (`turn.completed`): the login works.
    pub fn turn_completed(&mut self, pane: &str) -> Option<Change> {
        if self.state == AuthState::Ok {
            return None;
        }
        self.set(AuthState::Ok, "turn completed".into(), "hook", Some(pane.to_string()))
    }
}

/// Whole days since an RFC 3339 / ISO 8601 UTC time (`2026-10-01T12:00:00Z`,
/// fractional seconds and `+00:00` accepted). None when unparsable.
pub fn age_days(at: &str) -> Option<i64> {
    let (date, rest) = at.split_once('T')?;
    let mut d = date.split('-').map(|x| x.parse::<i64>());
    let (y, m, dd) = (d.next()?.ok()?, d.next()?.ok()?, d.next()?.ok()?);
    let hms: String = rest.chars().take(8).collect();
    let mut t = hms.split(':').map(|x| x.parse::<i64>());
    let (h, mi, s) = (t.next()?.ok()?, t.next()?.ok()?, t.next()?.ok()?);
    // Days from civil (Howard Hinnant's algorithm).
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + dd - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    let secs = days * 86400 + h * 3600 + mi * 60 + s;
    let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).ok()?.as_secs() as i64;
    Some((now - secs).div_euclid(86400))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_and_error_types() {
        assert_eq!(
            screen_rule("  ✗ Login expired · Please run /login"),
            Some("login_expired")
        );
        assert_eq!(screen_rule("API Error: 401 · please run /login"), Some("run_login"));
        assert_eq!(screen_rule("> what next?"), None);
        assert!(is_auth_error("oauth_org_not_allowed") && is_auth_error("account_on_hold"));
        assert!(!is_auth_error("rate_limit") && !is_auth_error("billing_error"));
    }

    #[test]
    fn lifecycle_from_missing_to_ok_to_failed_and_back() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().to_str().unwrap();
        let mut h = AuthHealth::new(home, true, false);
        assert_eq!(h.state, AuthState::Missing);
        assert_eq!(h.check_credentials(), None, "no change, no event");
        std::fs::create_dir_all(d.path().join(".claude")).unwrap();
        // Contents are never read: a short expiresAt is not an alert.
        std::fs::write(credentials_path(home), r#"{"claudeAiOauth":{"expiresAt":1}}"#).unwrap();
        assert_eq!(h.check_credentials().unwrap().state, AuthState::Ok);
        let c = h.screen_expired("w1:p1", "login_expired").unwrap();
        assert_eq!(
            (c.state, c.source, c.pane.as_deref()),
            (AuthState::Expired, "screen", Some("w1:p1"))
        );
        assert_eq!(h.check_credentials(), None, "same file: still expired");
        let c = h.hook_failure("w1:p1", "authentication_failed").unwrap();
        assert_eq!(
            (c.state, c.reason.as_str()),
            (AuthState::Failed, "authentication_failed")
        );
        assert_eq!(h.screen_expired("w1:p1", "run_login"), None, "failed is kept");
        assert_eq!(h.turn_completed("w1:p1").unwrap().state, AuthState::Ok);
        // A re-login after a failure is seen through the file's mtime.
        h.hook_failure("w1:p1", "account_on_hold");
        std::thread::sleep(std::time::Duration::from_millis(20));
        let f = std::fs::File::options()
            .write(true)
            .open(credentials_path(home))
            .unwrap();
        f.set_modified(SystemTime::now() + std::time::Duration::from_secs(5))
            .unwrap();
        assert_eq!(h.check_credentials().unwrap().reason, "credentials renewed");
        std::fs::remove_file(credentials_path(home)).unwrap();
        assert_eq!(h.check_credentials().unwrap().state, AuthState::Missing);
    }

    #[test]
    fn codex_probes_age_command_and_failed_screen_rules() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().to_str().unwrap();
        let codex = crate::plugin::Host::builtin().get("codex").unwrap();
        let mut h = AuthHealth::for_plugin(codex.as_ref(), home, "linux", &|_| false);
        assert_eq!(
            (h.state, h.reason.as_str()),
            (AuthState::Missing, format!("no {home}/.codex/auth.json").as_str())
        );
        std::fs::create_dir_all(d.path().join(".codex")).unwrap();
        let old = "2020-01-01T00:00:00.000000Z";
        std::fs::write(
            d.path().join(".codex/auth.json"),
            format!(r#"{{"last_refresh":"{old}","tokens":{{"x":1}}}}"#),
        )
        .unwrap();
        let c = h.check_credentials().unwrap();
        assert_eq!(c.state, AuthState::Ok, "present first");
        let c = h.check_credentials().unwrap();
        assert_eq!(c.state, AuthState::Expiring);
        assert!(
            c.reason.starts_with("last refreshed ") && c.reason.ends_with(" days ago"),
            "{}",
            c.reason
        );
        let now = crate::util::iso_now();
        std::fs::write(
            d.path().join(".codex/auth.json"),
            format!(r#"{{"last_refresh":"{now}"}}"#),
        )
        .unwrap();
        assert_eq!(h.check_credentials().unwrap().reason, "credentials renewed");
        // A reused refresh token is a failure with its own reason.
        let (rule, state) = codex
            .auth_screen_hit("refresh token was already used. Please log out and sign in again.")
            .unwrap();
        let c = h.screen_hit("w1:p1", rule, state).unwrap();
        assert_eq!((c.state, c.reason.as_str()), (AuthState::Failed, "refresh_reused"));
        // `codex login status` saying "logged in" does not clear a failure on the same file.
        assert_eq!(h.command_result(Some(true)), None);
        assert_eq!(h.command_result(None), None);
        // Fresh from a fresh one.
        let mut fresh = AuthHealth::for_plugin(codex.as_ref(), home, "linux", &|_| false);
        assert_eq!(fresh.state, AuthState::Ok);
        assert_eq!(fresh.command_result(Some(false)).unwrap().state, AuthState::Missing);
        assert_eq!(fresh.check_credentials(), None, "the command decides presence now");
        // A login written after the command spoke counts at once (no wait for the next probe).
        let mut signed_out = AuthHealth::for_plugin(
            codex.as_ref(),
            d.path().join("other").to_str().unwrap(),
            "linux",
            &|_| false,
        );
        assert_eq!(signed_out.state, AuthState::Missing);
        assert_eq!(signed_out.command_result(Some(false)), None);
        std::fs::create_dir_all(d.path().join("other/.codex")).unwrap();
        std::fs::write(d.path().join("other/.codex/auth.json"), "{}").unwrap();
        assert_eq!(signed_out.check_credentials().unwrap().state, AuthState::Ok);
        assert_eq!(fresh.command_result(Some(true)).unwrap().state, AuthState::Ok);
        let key = AuthHealth::for_plugin(codex.as_ref(), home, "linux", &|n| n == "OPENAI_API_KEY");
        assert_eq!((key.state, key.reason.as_str()), (AuthState::Ok, "OPENAI_API_KEY"));
    }

    #[test]
    fn cursor_on_macos_trusts_the_status_command() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().to_str().unwrap();
        let cursor = crate::plugin::Host::builtin().get("cursor").unwrap();
        let mut mac = AuthHealth::for_plugin(cursor.as_ref(), home, "darwin", &|_| false);
        assert_eq!(
            (mac.state, mac.reason.as_str()),
            (AuthState::Ok, "keychain (not inspected)")
        );
        assert_eq!(mac.command_result(Some(false)).unwrap().state, AuthState::Missing);
        assert_eq!(mac.command_result(Some(true)).unwrap().state, AuthState::Ok);
        let linux = AuthHealth::for_plugin(cursor.as_ref(), home, "linux", &|_| false);
        assert_eq!(linux.state, AuthState::Missing);
    }

    #[test]
    fn age_in_days() {
        assert_eq!(age_days("nope"), None);
        let a = age_days("1970-01-02T00:00:00Z").unwrap();
        let b = age_days("1970-01-01T00:00:00+00:00").unwrap();
        assert_eq!(b - a, 1);
        assert_eq!(age_days(&crate::util::iso_now()), Some(0));
    }

    #[test]
    fn api_key_and_keychain_baselines() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().to_str().unwrap();
        assert_eq!(AuthHealth::new(home, true, true).state, AuthState::Ok);
        let mac = AuthHealth::new(home, false, false);
        assert_eq!(
            (mac.state, mac.reason.as_str()),
            (AuthState::Ok, "keychain (not inspected)")
        );
        // The same, driven by the manifest probes.
        let p = plugin::claude();
        let linux = AuthHealth::for_plugin(p, home, "linux", &|_| false);
        assert_eq!(linux.state, AuthState::Missing);
        assert_eq!(linux.reason, format!("no {home}/.claude/.credentials.json"));
        let mac = AuthHealth::for_plugin(p, home, "darwin", &|_| false);
        assert_eq!(mac.reason, "keychain (not inspected)");
        let key = AuthHealth::for_plugin(p, home, "linux", &|n| n == "ANTHROPIC_API_KEY");
        assert_eq!((key.state, key.reason.as_str()), (AuthState::Ok, "ANTHROPIC_API_KEY"));
    }
}
