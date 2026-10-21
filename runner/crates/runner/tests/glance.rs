//! Glances (docs/protocol.md "Glances") against the real daemon and holder:
//! the hello lists the feature; a glance set yields a first glance and then
//! one per change, never more than `hz` a second; an unchanged screen sends
//! nothing; scrollback never appears; an empty set stops all glances.

mod common;

use common::{options, start_holder, Conn, FakeGateway};
use serde_json::{json, Value};
use std::time::Duration;
use tmuxedo_runner::daemon;

const S: Duration = Duration::from_secs(10);

fn text_of(g: &Value) -> String {
    g["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            row.as_array()
                .unwrap()
                .iter()
                .map(|r| r[0].as_str().unwrap().to_string())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn take_glances(c: &mut Conn) -> Vec<Value> {
    let (g, rest): (Vec<Value>, Vec<Value>) = std::mem::take(&mut c.texts)
        .into_iter()
        .partition(|v| v["t"] == "glance");
    c.texts = rest;
    g
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn glances_follow_changes_at_most_hz() {
    let dir = common::tempdir();
    let holder = start_holder(dir.path());
    let gw = FakeGateway::start().await;
    let _task = tokio::spawn(daemon::run(options(dir.path(), &gw.url, holder)));
    let mut c = gw.accept(S).await.expect("runner dials out");
    let hello = c.hello(S).await;
    assert_eq!(
        hello["runner"]["features"],
        json!([
            "glance",
            "login_callback",
            "names",
            "chat",
            "chat_offer",
            "files",
            "needs",
            "fs",
            "stats",
            "usage",
            "review",
            "queued_prompts",
            "checkpoints",
            "session_summary",
            "replay"
        ])
    );
    c.text_t(S, "topology").await;

    // Nothing before a glance set.
    c.input("w1:p1", "echo before\n").await;
    c.drain(Duration::from_millis(1500)).await;
    assert!(take_glances(&mut c).is_empty(), "no glances without a set");

    // A set: a first glance at once.
    c.send(json!({"t":"glance","paneIds":["w1:p1"],"hz":1})).await;
    let first = c.text(S, |v| v["t"] == "glance").await;
    assert_eq!(first["paneId"], "w1:p1");
    assert_eq!((first["cols"].as_u64(), first["rows"].as_u64()), (Some(80), Some(24)));
    assert!(text_of(&first).contains("before"), "{}", text_of(&first));
    let mut last_rev = first["rev"].as_u64().unwrap();

    // A line scrolled into scrollback is never in a glance.
    c.input("w1:p1", "printf 'SECRET-%s\\n' top; seq 1 60\n").await;
    let end = loop {
        let g = c.text(S, |v| v["t"] == "glance").await;
        let rev = g["rev"].as_u64().unwrap();
        assert!(rev > last_rev, "rev increases");
        last_rev = rev;
        if text_of(&g).lines().any(|l| l.trim() == "60") {
            break g;
        }
    };
    assert!(
        !text_of(&end).contains("SECRET-top"),
        "scrollback leaked: {}",
        text_of(&end)
    );
    assert!(text_of(&end).lines().count() <= 24);

    // Unchanged screen: nothing more.
    c.drain(Duration::from_millis(2500)).await;
    let idle = take_glances(&mut c);
    assert!(
        idle.len() <= 1,
        "an idle screen sends at most the prompt redraw: {}",
        idle.len()
    );
    c.drain(Duration::from_millis(2500)).await;
    assert!(take_glances(&mut c).is_empty(), "an unchanged screen sends nothing");

    // Continuous printing for ~3 s at hz 1: at most one glance a second.
    c.input("w1:p1", "for i in $(seq 1 30); do echo tick-$i; sleep 0.1; done\n")
        .await;
    c.drain(Duration::from_millis(3500)).await;
    let busy = take_glances(&mut c);
    assert!((2..=5).contains(&busy.len()), "{} glances in 3.5 s at 1 Hz", busy.len());

    // Empty set: everything stops.
    c.drain(Duration::from_millis(1500)).await;
    c.send(json!({"t":"glance","paneIds":[],"hz":1})).await;
    c.drain(Duration::from_millis(300)).await;
    take_glances(&mut c);
    c.input("w1:p1", "echo after-stop\n").await;
    c.drain(Duration::from_millis(2500)).await;
    assert!(take_glances(&mut c).is_empty(), "no glances after an empty set");

    // Unknown panes in a set are ignored; a reconnect clears the set (the gateway resends it).
    c.send(json!({"t":"glance","paneIds":["w9:p9","w1:p1"],"hz":0.5})).await;
    let g = c.text(S, |v| v["t"] == "glance").await;
    assert_eq!(g["paneId"], "w1:p1");
    assert!(text_of(&g).contains("after-stop"));
    c.close(1001).await;
    let mut c2 = gw.accept(S).await.expect("runner reconnects");
    c2.hello(S).await;
    c2.input("w1:p1", "echo after-reconnect\n").await;
    c2.drain(Duration::from_millis(2500)).await;
    assert!(take_glances(&mut c2).is_empty(), "the set does not survive a reconnect");
}
