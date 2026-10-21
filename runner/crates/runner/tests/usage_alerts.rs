//! Usage alerts end to end (docs/design/agent-usage.md §5): a status-line
//! reading at 80 % opens a machine-level `limit` item, a higher one replaces
//! it, a level turned off is skipped, and at the limit "Resume at reset"
//! plans the stopped agent's resume ("Don't resume" calls it off).

mod common;

use common::{options, start_holder, FakeGateway, H};
use serde_json::{json, Value};
use std::time::Duration;
use tmuxedo_runner::daemon;

const S: Duration = Duration::from_secs(20);

fn limits(v: &Value) -> Vec<Value> {
    v["items"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|i| i["kind"] == "limit")
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn usage_alerts_open_once_per_level_and_offer_to_resume() {
    let dir = common::tempdir();
    let holder = start_holder(dir.path());
    let gw = FakeGateway::start().await;
    let o = options(dir.path(), &gw.url, holder);
    let sock = o.runner_socket.clone();
    let _task = tokio::spawn(daemon::run(o));
    let mut c = gw.accept(S).await.unwrap();
    c.text_t(S, "hello").await;
    c.send(json!({"t": "welcome", "machineId": "m1", "userId": "u1", "lastEventSeq": 0, "features": ["ui", "needs"]}))
        .await;
    c.text_t(S, "topology").await;
    // AI usage on, the 95 % alert off.
    c.send(json!({"t": "config", "plugins": [{"id": "claude", "settings": {"usage": {"enabled": true, "alerts": [80, 100]}}}]}))
        .await;
    let h = H::attach_to(&sock).await;
    let hook = |name: &str| json!({"paneId": "w1:p1", "plugin": "claude", "event": {"hook_event_name": name, "session_id": "s1"}});
    h.raw("hook.event", hook("SessionStart")).await;
    h.raw("hook.event", hook("Stop")).await;

    let reset = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3600;
    let report = |pct: f64| {
        json!({"paneId": "w1:p1", "plugin": "claude", "reading": {"source": "statusline",
            "windows": [{"id": "5h", "usedPct": pct, "resetsAt": reset, "windowMins": 300}]}})
    };
    // Usage turns on asynchronously: report until the first alert shows.
    let mut at80 = None;
    for _ in 0..40 {
        h.raw("usage.report", report(81.0)).await;
        c.drain(Duration::from_millis(500)).await;
        if let Some(f) = c
            .texts
            .iter()
            .rev()
            .find(|v| v["t"] == "needs" && !limits(v).is_empty())
        {
            at80 = Some(f.clone());
            break;
        }
    }
    let at80 = at80.expect("an 80 % alert");
    let it = &limits(&at80)[0];
    assert_eq!(it["id"], "usage-claude-5h-80", "{it}");
    assert_eq!(it["paneId"], "");
    assert_eq!(it["title"], "Claude Code 5-hour limit at 80%");

    // 96 %: the 95 % alert is off, so the 80 % one stays.
    c.texts.clear();
    h.raw("usage.report", report(96.0)).await;
    c.drain(Duration::from_millis(2500)).await;
    assert!(
        !c.texts
            .iter()
            .any(|v| v["t"] == "needs" && limits(v).iter().any(|i| i["id"] != "usage-claude-5h-80")),
        "{:?}",
        c.texts
    );

    // The TUI widget reads the same numbers locally (no account key).
    let l = h.ok("usage.list", json!({})).await;
    assert_eq!(l["enabled"], true);
    assert_eq!(l["records"][0]["name"], "Claude Code");
    assert_eq!(l["records"][0]["windows"][0]["label"], "5-hour");
    assert_eq!(l["records"][0]["windows"][0]["usedPct"], 96.0);
    assert!(l["records"][0].get("accountKey").is_none());

    // The limit: a new item offering to resume.
    h.raw("usage.report", report(100.0)).await;
    let f = c
        .text(S, |v| {
            v["t"] == "needs" && limits(v).iter().any(|i| i["id"] == "usage-claude-5h-100")
        })
        .await;
    let it = limits(&f)
        .into_iter()
        .find(|i| i["id"] == "usage-claude-5h-100")
        .unwrap();
    assert_eq!(it["title"], "Claude Code 5-hour limit reached");
    let ids: Vec<&str> = it["responses"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["resume", "dismiss"]);

    c.send(json!({"t": "req", "id": "r1", "method": "needs.respond", "params": {"itemId": "usage-claude-5h-100", "responseId": "resume"}}))
        .await;
    let r = c.res("r1").await;
    assert_eq!(r["result"]["outcome"], "accepted", "{r}");
    let f = c
        .text(S, |v| {
            v["t"] == "needs" && limits(v).iter().any(|i| i["detail"]["usage"]["resume"] == 1)
        })
        .await;
    let it = &limits(&f)[0];
    assert_eq!(
        it["title"],
        "Claude Code 5-hour limit reached · 1 agent resumes at the reset"
    );

    c.send(json!({"t": "req", "id": "r2", "method": "needs.respond", "params": {"itemId": "usage-claude-5h-100", "responseId": "keep"}}))
        .await;
    assert_eq!(c.res("r2").await["result"]["outcome"], "accepted");
    let f = c
        .text(S, |v| {
            v["t"] == "needs"
                && limits(v)
                    .iter()
                    .any(|i| i["title"] == "Claude Code 5-hour limit reached")
        })
        .await;
    assert!(limits(&f)[0]["detail"]["usage"].get("resume").is_none());
}

/// The away digest asks what a session used: `session.summary` carries the
/// rise in each window that the pane's own readings showed, and its tokens.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_summary_says_what_the_session_used() {
    let dir = common::tempdir();
    let holder = start_holder(dir.path());
    let gw = FakeGateway::start().await;
    let o = options(dir.path(), &gw.url, holder);
    let sock = o.runner_socket.clone();
    let _task = tokio::spawn(daemon::run(o));
    let mut c = gw.accept(S).await.unwrap();
    c.text_t(S, "hello").await;
    c.send(json!({"t": "welcome", "machineId": "m1", "userId": "u1", "lastEventSeq": 0, "features": ["ui", "needs"]}))
        .await;
    c.text_t(S, "topology").await;
    c.send(
        json!({"t": "config", "plugins": [{"id": "claude", "settings": {"usage": {"enabled": true, "alerts": []}}}]}),
    )
    .await;
    let h = H::attach_to(&sock).await;
    h.raw(
        "hook.event",
        json!({"paneId": "w1:p1", "plugin": "claude", "event": {"hook_event_name": "SessionStart", "session_id": "s1"}}),
    )
    .await;
    let reset = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3600;
    let report = |pct: f64, tokens: u64| {
        json!({"paneId": "w1:p1", "plugin": "claude", "reading": {"source": "statusline", "tokens": tokens,
            "windows": [{"id": "5h", "usedPct": pct, "resetsAt": reset, "windowMins": 300}]}})
    };
    // Usage turns on asynchronously: report until the runner has the first reading.
    let mut on = false;
    for _ in 0..40 {
        h.raw("usage.report", report(20.0, 1000)).await;
        if h.ok("usage.list", json!({})).await["records"][0]["windows"][0]["usedPct"] == 20.0 {
            on = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(on, "usage never turned on");
    h.raw("usage.report", report(26.5, 4000)).await;

    c.send(json!({"t": "req", "id": "s", "method": "session.summary", "params": {"pane": "w1:p1", "since": 0}}))
        .await;
    let sm = c.res("s").await;
    assert_eq!(
        sm["result"]["usage"],
        json!({"plugin": "claude", "windows": [{"id": "5h", "spentPct": 6.5, "windowMins": 300}], "tokens": 4000}),
        "{sm}"
    );
    common::schema_check("success_response", &json!({"id": "s", "result": sm["result"]})).unwrap();
    // Nothing after `since`: no usage at all.
    c.send(json!({"t": "req", "id": "s2", "method": "session.summary", "params": {"pane": "w1:p1", "since": 99_999_999_999_999u64}}))
        .await;
    let sm = c.res("s2").await;
    assert!(sm["result"].get("usage").is_none(), "{sm}");
}
