//! Usage alerts: 80 %, 95 % and the limit, once per window, each one off on
//! request; the limit offers to resume at the reset.

use super::*;
use tmuxedo_protocol::usage::{UsageRecord, UsageWindow};

const NOW: u64 = 1_900_000_000;
const RESET: u64 = NOW + 3600;
const ALL: &[u8] = &[80, 95, 100];

fn rec(plugin: &str, pct: f64, resets_at: Option<u64>) -> UsageRecord {
    UsageRecord {
        plugin: plugin.into(),
        account_key: "m:0123456789abcdef0123456789abcdef".into(),
        mode: "plan".into(),
        plan: None,
        windows: vec![UsageWindow {
            id: "5h".into(),
            used_pct: pct,
            resets_at,
            window_mins: Some(300),
        }],
        context: None,
        reached: (pct >= 100.0).then(|| "5h".into()),
        source: "statusline".into(),
        observed_at: NOW * 1000,
    }
}

fn levels(a: &[Alert]) -> Vec<u8> {
    a.iter().map(|x| x.level).collect()
}

fn all(_: &str) -> Vec<u8> {
    ALL.to_vec()
}

#[test]
fn nothing_below_80() {
    let mut a = Alerts::default();
    assert!(a.update(&[rec("claude", 79.9, Some(RESET))], all, NOW).is_empty());
}

#[test]
fn eighty_then_ninety_five_then_the_limit_each_once() {
    let mut a = Alerts::default();
    let at80 = a.update(&[rec("claude", 81.0, Some(RESET))], all, NOW);
    assert_eq!(levels(&at80), vec![80]);
    assert_eq!(at80[0].window, "5h");
    assert_eq!(at80[0].resets_at, Some(RESET));
    assert!(at80[0].fresh, "a new alert is fresh");
    // The same window again: the same alert, no longer fresh.
    let again = a.update(&[rec("claude", 85.0, Some(RESET))], all, NOW + 10);
    assert_eq!(levels(&again), vec![80]);
    assert!(!again[0].fresh);
    assert_eq!(again[0].used_pct, 85.0);
    // 95 replaces 80: one alert per window.
    assert_eq!(
        levels(&a.update(&[rec("claude", 96.0, Some(RESET))], all, NOW + 20)),
        vec![95]
    );
    assert_eq!(
        levels(&a.update(&[rec("claude", 100.0, Some(RESET))], all, NOW + 30)),
        vec![100]
    );
    // Falling back (an estimate wobble) never re-alerts a lower level.
    assert_eq!(
        levels(&a.update(&[rec("claude", 97.0, Some(RESET))], all, NOW + 40)),
        vec![100]
    );
}

#[test]
fn a_jump_alerts_only_the_highest_level() {
    let mut a = Alerts::default();
    assert_eq!(
        levels(&a.update(&[rec("claude", 97.0, Some(RESET))], all, NOW)),
        vec![95]
    );
}

#[test]
fn a_new_window_starts_over_and_the_old_alert_closes_at_its_reset() {
    let mut a = Alerts::default();
    a.update(&[rec("claude", 90.0, Some(RESET))], all, NOW);
    // The reset passed: the alert is gone even with no new reading.
    assert!(a.update(&[rec("claude", 90.0, Some(RESET))], all, RESET).is_empty());
    // The next window reaches 80 again: a new alert.
    let next = RESET + 5 * 3600;
    let at = a.update(&[rec("claude", 82.0, Some(next))], all, RESET + 60);
    assert_eq!(levels(&at), vec![80]);
    assert!(at[0].fresh);
}

#[test]
fn a_level_that_is_off_is_skipped() {
    let mut a = Alerts::default();
    let only95 = |_: &str| vec![95, 100];
    assert!(a.update(&[rec("claude", 85.0, Some(RESET))], only95, NOW).is_empty());
    assert_eq!(
        levels(&a.update(&[rec("claude", 95.0, Some(RESET))], only95, NOW)),
        vec![95]
    );
    let none = |_: &str| vec![];
    let mut b = Alerts::default();
    assert!(b.update(&[rec("claude", 100.0, Some(RESET))], none, NOW).is_empty());
}

#[test]
fn turning_a_level_off_closes_its_open_alert() {
    let mut a = Alerts::default();
    a.update(&[rec("claude", 85.0, Some(RESET))], all, NOW);
    assert!(a
        .update(&[rec("claude", 85.0, Some(RESET))], |_| vec![95, 100], NOW)
        .is_empty());
}

#[test]
fn per_plugin_and_per_window() {
    let mut a = Alerts::default();
    let mut c = rec("claude", 81.0, Some(RESET));
    c.windows.push(UsageWindow {
        id: "7d".into(),
        used_pct: 96.0,
        resets_at: Some(NOW + 86_400),
        window_mins: Some(10_080),
    });
    let out = a.update(&[c, rec("codex", 99.0, Some(RESET))], all, NOW);
    let got: Vec<(String, String, u8)> = out
        .iter()
        .map(|x| (x.plugin.clone(), x.window.clone(), x.level))
        .collect();
    assert_eq!(
        got,
        vec![
            ("claude".into(), "5h".into(), 80),
            ("claude".into(), "7d".into(), 95),
            ("codex".into(), "5h".into(), 95)
        ]
    );
}

#[test]
fn a_window_without_a_reset_time_rearms_once_it_drops_below_80() {
    let mut a = Alerts::default();
    assert_eq!(levels(&a.update(&[rec("claude", 81.0, None)], all, NOW)), vec![80]);
    assert!(a.update(&[rec("claude", 10.0, None)], all, NOW + 1).is_empty());
    assert!(a.update(&[rec("claude", 81.0, None)], all, NOW + 2)[0].fresh);
}

#[test]
fn a_plugin_that_stops_reporting_has_no_alert() {
    let mut a = Alerts::default();
    a.update(&[rec("claude", 81.0, Some(RESET))], all, NOW);
    assert!(a.update(&[], all, NOW + 1).is_empty());
}

#[test]
fn sent_alerts_survive_a_restart_without_alerting_again() {
    let dir = tempfile::tempdir().unwrap();
    let mut a = Alerts::load(dir.path());
    assert!(a.update(&[rec("claude", 81.0, Some(RESET))], all, NOW)[0].fresh);
    let mut b = Alerts::load(dir.path());
    // Already told about this window before the restart: not shown again.
    assert!(b.update(&[rec("claude", 81.0, Some(RESET))], all, NOW + 5).is_empty());
    // A higher level is still news.
    assert_eq!(
        levels(&b.update(&[rec("claude", 96.0, Some(RESET))], all, NOW + 6)),
        vec![95]
    );
}

#[test]
fn resume_at_the_reset() {
    let dir = tempfile::tempdir().unwrap();
    let mut a = Alerts::load(dir.path());
    a.update(&[rec("claude", 100.0, Some(RESET))], all, NOW);
    assert!(a.plan_resume("claude", RESET, vec!["w1:p1".into(), "w1:p2".into()]));
    let out = a.update(&[rec("claude", 100.0, Some(RESET))], all, NOW + 1);
    assert_eq!(out[0].resume, Some(2));
    assert!(a.resume_due(RESET - 1).is_empty(), "not before the reset");
    // Kept across a restart.
    let mut b = Alerts::load(dir.path());
    assert!(
        b.resume_due(RESET).is_empty(),
        "a moment after it, so the window has really reset"
    );
    let due = b.resume_due(RESET + RESUME_AFTER_S);
    assert_eq!(
        due,
        vec![("claude".to_string(), vec!["w1:p1".to_string(), "w1:p2".to_string()])]
    );
    assert!(b.resume_due(RESET + 999).is_empty(), "once");
}

#[test]
fn a_resume_can_be_called_off() {
    let mut a = Alerts::default();
    assert!(a.plan_resume("claude", RESET, vec!["w1:p1".into()]));
    a.cancel_resume("claude");
    assert!(a.resume_due(RESET + 999).is_empty());
    assert!(!a.plan_resume("claude", RESET, vec![]), "nothing to resume");
}

#[test]
fn a_resume_older_than_a_day_is_dropped() {
    let mut a = Alerts::default();
    a.plan_resume("claude", RESET, vec!["w1:p1".into()]);
    assert!(a.resume_due(RESET + STALE_RESUME_S + 1).is_empty());
}

// ── the needs-you items ──────────────────────────────────────────────────

fn name(_: &str) -> String {
    "Claude".into()
}

#[test]
fn items_follow_the_contract_and_say_it_plainly() {
    let mut a = Alerts::default();
    let at80 = a.update(&[rec("claude", 81.0, Some(RESET))], all, NOW);
    let it = &items(&at80, name, "2026-10-18T00:00:00Z")[0];
    assert!(it.is_valid(), "{it:?}");
    assert_eq!(it.id, "usage-claude-5h-80");
    assert_eq!(it.pane_id, "");
    assert_eq!(it.kind, "limit");
    assert_eq!(it.source, "core");
    assert_eq!(it.answer_via, "none");
    assert_eq!(it.title, "Claude 5-hour limit at 80%");
    let d = it.detail.as_ref().unwrap();
    assert_eq!(d["usage"]["usedPct"], 81.0);
    assert_eq!(d["usage"]["resetsAt"], RESET);
    assert_eq!(d["usage"]["level"], 80);
    assert_eq!(
        it.responses.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
        vec!["dismiss"]
    );

    let full = a.update(&[rec("claude", 100.0, Some(RESET))], all, NOW + 1);
    let it = &items(&full, name, "t")[0];
    assert!(it.is_valid(), "{it:?}");
    assert_eq!(it.title, "Claude 5-hour limit reached");
    let ids: Vec<(&str, &str, &str)> = it
        .responses
        .iter()
        .map(|r| (r.id.as_str(), r.kind.as_str(), r.requires.as_str()))
        .collect();
    assert_eq!(
        ids,
        vec![("resume", "approve", "control"), ("dismiss", "dismiss", "none")]
    );

    a.plan_resume("claude", RESET, vec!["w1:p1".into(), "w1:p2".into()]);
    let planned = a.update(&[rec("claude", 100.0, Some(RESET))], all, NOW + 2);
    let it = &items(&planned, name, "t")[0];
    assert_eq!(it.id, "usage-claude-5h-100", "the same item, so no second push");
    assert_eq!(it.title, "Claude 5-hour limit reached · 2 agents resume at the reset");
    assert_eq!(it.detail.as_ref().unwrap()["usage"]["resume"], 2);
    let ids: Vec<&str> = it.responses.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(ids, vec!["keep", "dismiss"]);
}

#[test]
fn the_limit_without_a_reset_time_offers_no_resume() {
    let mut a = Alerts::default();
    let out = a.update(&[rec("claude", 100.0, None)], all, NOW);
    let it = &items(&out, name, "t")[0];
    assert!(it.responses.iter().all(|r| r.id != "resume"));
}

#[test]
fn window_labels() {
    assert_eq!(window_label("5h", None), "5-hour");
    assert_eq!(window_label("7d", None), "weekly");
    assert_eq!(window_label("43200m", Some(43_200)), "30-day");
    assert_eq!(window_label("120m", Some(120)), "2-hour");
    assert_eq!(window_label("45m", Some(45)), "45-minute");
    assert_eq!(window_label("model:x", None), "model:x");
}
