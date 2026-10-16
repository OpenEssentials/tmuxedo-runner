//! Machine stats (docs/protocol.md "Machine stats") against the real daemon:
//! the hello lists the feature; nothing is sent until the gateway asks; a
//! rate yields a first sample at once and then one per interval; 0 stops
//! them; a reconnect forgets the rate. A sample holds numbers and the word
//! `home`, nothing else.

mod common;

use common::{options, start_holder, Conn, FakeGateway};
use serde_json::{json, Value};
use std::time::Duration;
use tmuxedo_runner::daemon;

const S: Duration = Duration::from_secs(10);

fn take_stats(c: &mut Conn) -> Vec<Value> {
    let (s, rest): (Vec<Value>, Vec<Value>) = std::mem::take(&mut c.texts)
        .into_iter()
        .partition(|v| v["t"] == "stats");
    c.texts = rest;
    s
}

fn strings(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(s) => out.push(s.clone()),
        Value::Array(a) => a.iter().for_each(|x| strings(x, out)),
        Value::Object(o) => o.values().for_each(|x| strings(x, out)),
        _ => {}
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stats_are_sent_only_while_asked() {
    let dir = common::tempdir();
    let holder = start_holder(dir.path());
    let gw = FakeGateway::start().await;
    let _task = tokio::spawn(daemon::run(options(dir.path(), &gw.url, holder)));
    let mut c = gw.accept(S).await.expect("runner dials out");
    let hello = c.hello(S).await;
    let features = hello["runner"]["features"].as_array().unwrap();
    assert!(features.contains(&json!("stats")), "{features:?}");
    c.text_t(S, "topology").await;

    // Nobody asked: nothing.
    c.drain(Duration::from_millis(2500)).await;
    assert!(take_stats(&mut c).is_empty(), "no stats before a rate is set");

    // A rate: a first sample at once, then about one every 2 s.
    c.send(json!({"t":"stats","hz":0.5})).await;
    let first = c.text(Duration::from_millis(1500), |v| v["t"] == "stats").await;
    assert!(first["at"].as_u64().unwrap() > 1_600_000_000_000);
    assert_eq!(first["disks"][0]["id"], "home");
    assert!(first["disks"][0]["free"].as_u64().unwrap() <= first["disks"][0]["total"].as_u64().unwrap());
    assert!(first.to_string().len() < 512, "{first}");
    let mut found = vec![];
    strings(&first, &mut found);
    found.sort();
    assert_eq!(found, ["home", "stats"], "a sample holds no other text");
    take_stats(&mut c);
    c.drain(Duration::from_millis(5000)).await;
    let n = take_stats(&mut c).len();
    assert!((1..=3).contains(&n), "{n} samples in 5 s at 0.5 Hz");

    // A rate far above the most allowed is still one every 2 s.
    c.send(json!({"t":"stats","hz":100})).await;
    c.drain(Duration::from_millis(3000)).await;
    let n = take_stats(&mut c).len();
    assert!(n <= 2, "{n} samples in 3 s");

    // Stop: nothing more.
    c.send(json!({"t":"stats","hz":0})).await;
    c.drain(Duration::from_millis(300)).await;
    take_stats(&mut c);
    c.drain(Duration::from_millis(4500)).await;
    assert!(take_stats(&mut c).is_empty(), "no stats after a stop");

    // A reconnect forgets the rate (the gateway asks again if anyone still looks).
    c.send(json!({"t":"stats","hz":0.5})).await;
    c.text(S, |v| v["t"] == "stats").await;
    c.close(1001).await;
    let mut c2 = gw.accept(S).await.expect("runner reconnects");
    c2.hello(S).await;
    c2.drain(Duration::from_millis(4500)).await;
    assert!(take_stats(&mut c2).is_empty(), "the rate does not survive a reconnect");
}
