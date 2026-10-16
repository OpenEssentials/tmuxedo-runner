//! Names for workspaces, tabs and panes (queue 3c, docs/protocol.md "Names"),
//! end to end through a real holder and daemon:
//!
//! - over the gateway (`req` frames from a fake gateway): every rename lands
//!   in the next topology frame, a pane's name as `label`; hello lists the
//!   `names` feature;
//! - the shared name rules apply (control characters, bidi overrides,
//!   whitespace, the 64-character cap) and an empty name restores the
//!   default (`default` / the number; a pane's label disappears);
//! - two clients renaming at once: the runner applies them in arrival order,
//!   every caller gets an answer, and the last one is what everyone sees;
//! - a missing `label` is still refused (unchanged API).

mod common;

use common::{options, start_holder, FakeGateway, H};
use serde_json::{json, Value};
use std::time::Duration;
use tmuxedo_runner::daemon;

const S: Duration = Duration::from_secs(10);

/// The newest topology received so far. The runner sends it before the `res`
/// of the call that changed it, so right after `res` it reflects that call.
fn latest(c: &mut common::Conn) -> Value {
    let t = c
        .texts
        .iter()
        .filter(|v| v["t"] == "topology")
        .max_by_key(|v| v["rev"].as_u64().unwrap_or(0))
        .cloned()
        .expect("a topology frame");
    c.texts.retain(|v| v["t"] != "topology");
    t
}

fn pane<'a>(topo: &'a Value, id: &str) -> &'a Value {
    topo["panes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == id)
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn renames_over_the_gateway_reach_the_topology() {
    let dir = common::tempdir();
    let holder = start_holder(dir.path());
    let gw = FakeGateway::start().await;
    let task = tokio::spawn(daemon::run(options(dir.path(), &gw.url, holder)));
    let mut c = gw.accept(S).await.expect("runner dials out");
    let hello = c.text_t(S, "hello").await;
    let features: Vec<&str> = hello["runner"]["features"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|f| f.as_str())
        .collect();
    assert!(features.contains(&"names"), "{features:?}");
    let first = c.text_t(S, "topology").await;
    assert!(
        pane(&first, "w1:p1").get("label").is_none(),
        "no label key until one is given"
    );
    c.send(json!({"t":"welcome","machineId":"m1","userId":"u1"})).await;

    // Workspace, tab and pane, with junk the rules remove.
    c.send(json!({"t":"req","id":"r1","method":"workspace.rename","params":{"workspace_id":"w1","label":"  api\u{1b}[31m \u{202e}server  "}})).await;
    let r1 = c.res("r1").await;
    assert_eq!(r1["result"]["workspace"]["label"], "api[31m server", "{r1}");
    let t = latest(&mut c);
    assert_eq!(t["workspaces"][0]["name"], "api[31m server");

    c.send(json!({"t":"req","id":"r2","method":"tab.rename","params":{"tab_id":"w1:t1","label":"logs\n\ttail"}}))
        .await;
    assert_eq!(c.res("r2").await["result"]["tab"]["label"], "logs tail");
    assert_eq!(latest(&mut c)["tabs"][0]["name"], "logs tail");

    let long = format!("{}😀tail", "p".repeat(63));
    c.send(json!({"t":"req","id":"r3","method":"pane.rename","params":{"pane_id":"w1:p1","label": long}}))
        .await;
    let want = format!("{}😀", "p".repeat(63));
    assert_eq!(c.res("r3").await["result"]["pane"]["label"], want.as_str());
    let t = latest(&mut c);
    assert_eq!(pane(&t, "w1:p1")["label"], want.as_str());
    assert_eq!(pane(&t, "w1:p1")["title"], "sh", "the title is untouched");

    // Empty restores the defaults; a pane's label goes away entirely.
    c.send(
        json!({"t":"req","id":"r4","method":"workspace.rename","params":{"workspace_id":"w1","label":" \u{200b} "}}),
    )
    .await;
    assert_eq!(c.res("r4").await["result"]["workspace"]["label"], "default");
    c.send(json!({"t":"req","id":"r5","method":"tab.rename","params":{"tab_id":"w1:t1","label":null}}))
        .await;
    assert_eq!(c.res("r5").await["result"]["tab"]["label"], "1");
    c.send(json!({"t":"req","id":"r6","method":"pane.rename","params":{"pane_id":"w1:p1","label":""}}))
        .await;
    assert!(c.res("r6").await["result"]["pane"]["label"].is_null());
    let t = latest(&mut c);
    assert!(pane(&t, "w1:p1").get("label").is_none(), "{t}");
    assert_eq!(t["tabs"][0]["name"], "1");
    assert_eq!(t["workspaces"][0]["name"], "default");

    // Unknown targets, a missing label and a label that is not a string are refused.
    c.send(json!({"t":"req","id":"r7","method":"tab.rename","params":{"tab_id":"w9:t9","label":"x"}}))
        .await;
    assert_eq!(c.res("r7").await["error"]["code"], "tab_not_found");
    c.send(json!({"t":"req","id":"r8","method":"workspace.rename","params":{"workspace_id":"w1"}}))
        .await;
    assert_eq!(c.res("r8").await["error"]["code"], "invalid_params");
    c.send(json!({"t":"req","id":"r9","method":"tab.rename","params":{"tab_id":"w1:t1","label":7}}))
        .await;
    assert_eq!(c.res("r9").await["error"]["code"], "invalid_params");

    task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_clients_renaming_at_once_end_on_the_last_name() {
    let h = std::sync::Arc::new(H::start().await);
    // Two local clients race 20 renames each on the same tab and pane.
    let mut tasks = Vec::new();
    for who in ["a", "b"] {
        let h = h.clone();
        tasks.push(tokio::spawn(async move {
            let mut answers = Vec::new();
            for i in 0..20 {
                let r = h
                    .raw("tab.rename", json!({"tab_id": "w1:t1", "label": format!("{who}-{i}")}))
                    .await;
                answers.push(r["result"]["tab"]["label"].as_str().unwrap().to_string());
                let r = h
                    .raw(
                        "pane.rename",
                        json!({"pane_id": "w1:p1", "label": format!("{who}-{i}")}),
                    )
                    .await;
                answers.push(r["result"]["pane"]["label"].as_str().unwrap().to_string());
            }
            answers
        }));
    }
    for t in tasks {
        assert_eq!(t.await.unwrap().len(), 40, "every rename was answered");
    }
    // Both finished: the stored names are one client's final rename, and
    // every read agrees.
    let tab = h.ok("tab.get", json!({"tab_id": "w1:t1"})).await;
    let label = tab["tab"]["label"].as_str().unwrap().to_string();
    assert!(label == "a-19" || label == "b-19", "{label}");
    let pane = h.ok("pane.get", json!({"pane_id": "w1:p1"})).await;
    let pl = pane["pane"]["label"].as_str().unwrap().to_string();
    assert!(pl == "a-19" || pl == "b-19", "{pl}");
    let list = h.ok("tab.list", json!({})).await;
    assert_eq!(list["tabs"][0]["label"], label.as_str());
}
