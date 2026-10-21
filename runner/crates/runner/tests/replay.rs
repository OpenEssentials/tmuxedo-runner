//! Session recording and replay through the gateway, in process
//! (docs/design/sharing-and-integrations.md §3): a real holder, the runner
//! daemon and a fake gateway. The owner turns recording on for one session;
//! what it prints (masked), its agent events (markers) and its size changes
//! are recorded and read back by time, and a replay of the events redraws
//! the screen the session shows. Values that become secret later are masked
//! in the files too.

mod common;

use common::{options, start_holder, FakeGateway, H};
use serde_json::{json, Value};
use std::time::{Duration, Instant};
use tmuxedo_runner::daemon;

const S: Duration = Duration::from_secs(20);

/// A generated fake value (never a real credential).
fn fake_value(tag: &str) -> String {
    let mut b = [0u8; 12];
    getrandom::getrandom(&mut b).unwrap();
    format!("{tag}{}", hex::encode(b))
}

async fn wait_file_has(path: &std::path::Path, needle: &str) {
    let until = Instant::now() + S;
    while !std::fs::read_to_string(path).is_ok_and(|s| s.contains(needle)) {
        assert!(Instant::now() < until, "{} never held {needle:?}", path.display());
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn outputs(page: &Value) -> String {
    page["result"]["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["k"] == "o" || e["k"] == "s")
        .map(|e| e["d"].as_str().unwrap())
        .collect()
}

fn recorded_files(dir: &std::path::Path) -> String {
    let mut all = String::new();
    for d in std::fs::read_dir(dir.join("recordings")).unwrap().flatten() {
        for f in std::fs::read_dir(d.path()).unwrap().flatten() {
            all.push_str(&std::fs::read_to_string(f.path()).unwrap_or_default());
        }
    }
    all
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_recorded_session_replays_masked_with_its_markers() {
    let secret = fake_value("rec");
    let later = fake_value("late");
    let dir = common::tempdir();
    let holder = start_holder(dir.path());
    let gw = FakeGateway::start().await;
    let o = options(dir.path(), &gw.url, holder);
    let sock = o.runner_socket.clone();
    let task = tokio::spawn(daemon::run(o));
    let mut c = gw.accept(S).await.expect("runner dials out");
    let hello = c.hello(S).await;
    assert!(hello.to_string().contains("\"replay\""), "the feature is advertised");
    c.text_t(S, "topology").await;
    c.send(json!({"t":"welcome","machineId":"m1","userId":"u1"})).await;
    c.send(json!({"t":"config","shell":{"aliases":{},"env":{"API_SECRET": secret},"rc":""}}))
        .await;
    wait_file_has(&dir.path().join("state/config.json"), "API_SECRET").await;
    let h = H::attach_to(&sock).await;
    let pane = "w1:p1";
    h.wait_alive(pane).await;
    h.ok(
        "pane.send_text",
        json!({"pane_id": pane, "text": "echo BEFORE-$((40+2))\n"}),
    )
    .await;
    h.wait_text(pane, "BEFORE-42").await;

    // Off by default: nothing listed, nothing to read.
    c.send(json!({"t":"req","id":"l0","method":"replay.list","params":{}}))
        .await;
    assert_eq!(c.res("l0").await["result"]["recordings"], json!([]));
    c.send(json!({"t":"req","id":"r0","method":"replay.read","params":{"pane": pane}}))
        .await;
    assert_eq!(c.res("r0").await["error"]["code"], "not_found");

    // The owner turns it on.
    c.send(json!({"t":"req","id":"on","method":"replay.set","params":{"pane": pane, "on": true}}))
        .await;
    let on = c.res("on").await;
    assert_eq!(on["result"]["recording"], true, "{on}");
    let t_on = on["result"]["startMs"].as_u64().expect("a part opened at once");

    h.ok(
        "pane.report_agent",
        json!({"pane_id": pane, "source": "test", "agent": "claude", "state": "working"}),
    )
    .await;
    h.ok(
        "pane.send_text",
        json!({"pane_id": pane, "text": format!("echo 'tok {secret}'; echo ONE-$((40+2))\n")}),
    )
    .await;
    h.wait_text(pane, "ONE-42").await;
    h.ok("pane.resize", json!({"pane_id": pane, "cols": 100, "rows": 30}))
        .await;
    h.ok(
        "pane.report_agent",
        json!({"pane_id": pane, "source": "test", "agent": "claude", "state": "idle"}),
    )
    .await;
    h.ok(
        "pane.send_text",
        json!({"pane_id": pane, "text": format!("echo 'v {later}'; echo TWO-$((40+2))\n")}),
    )
    .await;
    h.wait_text(pane, "TWO-42").await;

    // Read it back: the first event redraws the screen, then what was printed.
    let until = Instant::now() + S;
    let page = loop {
        c.send(json!({"t":"req","id":"r1","method":"replay.read","params":{"pane": pane}}))
            .await;
        let p = c.res("r1").await;
        if outputs(&p).contains("TWO-42\r\n") || Instant::now() > until {
            break p;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let r = &page["result"];
    assert_eq!(r["type"], "replay_page");
    let events = r["events"].as_array().unwrap();
    assert_eq!(events[0]["k"], "r", "a part opens with its size");
    assert_eq!(events[1]["k"], "s", "then a redraw");
    assert!(
        events[1]["d"].as_str().unwrap().contains("BEFORE-42"),
        "the screen before recording, as it was"
    );
    let out = outputs(&page);
    assert!(out.contains("ONE-42") && out.contains("tok ****"), "printed, masked");
    assert!(!out.contains(&secret), "the secret is not recorded");
    assert!(out.contains(&later), "not secret yet");
    assert_eq!(
        out.matches("BEFORE-42\r\n").count(),
        1,
        "only the redraw holds what came before"
    );
    let labels: Vec<&str> = events
        .iter()
        .filter(|e| e["k"] == "m")
        .map(|e| e["d"].as_str().unwrap())
        .collect();
    assert!(labels.contains(&"status:working"), "{labels:?}");
    assert!(labels.contains(&"idle"), "{labels:?}");
    assert!(events.iter().any(|e| e["k"] == "r" && e["d"] == "100x30"));
    assert!(events.windows(2).all(|w| w[0]["at"].as_u64() <= w[1]["at"].as_u64()));
    assert!(events[0]["at"].as_u64().unwrap() >= t_on);

    // A replay of the events into a fresh screen shows what the session shows.
    let mut emu = tmuxedo_emu::Emulator::new(
        r["rows"].as_u64().unwrap() as u16,
        r["cols"].as_u64().unwrap() as u16,
        1000,
    );
    for e in events {
        match e["k"].as_str().unwrap() {
            "s" | "o" => emu.feed(e["d"].as_str().unwrap().as_bytes()),
            "r" => {
                let (c, rr) = e["d"].as_str().unwrap().split_once('x').unwrap();
                emu.resize(rr.parse().unwrap(), c.parse().unwrap());
            }
            _ => {}
        }
    }
    let live = h.ok("pane.read", json!({"pane_id": pane, "source": "visible"})).await;
    let live = live["read"]["text"].as_str().unwrap().to_string();
    let replayed = emu.visible_text();
    let trim = |s: &str| {
        s.lines()
            .map(str::trim_end)
            .collect::<Vec<_>>()
            .join("\n")
            .trim_end()
            .to_string()
    };
    assert_eq!(trim(&replayed), trim(&live), "the replay ends on the live screen");

    // Markers alone, and reading from a moment (the part holding it, from its start).
    c.send(json!({"t":"req","id":"m","method":"replay.read","params":{"pane": pane, "markers_only": true}}))
        .await;
    let m = c.res("m").await;
    assert!(m["result"]["events"].as_array().unwrap().iter().all(|e| e["k"] == "m"));
    let mid = events[events.len() / 2]["at"].as_u64().unwrap();
    c.send(json!({"t":"req","id":"t","method":"replay.read","params":{"pane": pane, "from_ms": mid}}))
        .await;
    let t = c.res("t").await;
    assert_eq!(t["result"]["events"][1]["k"], "s");
    // Small pages follow their cursors to the same events.
    let mut cursor: Option<String> = None;
    let mut paged = vec![];
    for i in 0..200 {
        let mut p = json!({"pane": pane, "limit_bytes": 64});
        if let Some(cur) = &cursor {
            p["cursor"] = json!(cur);
        }
        let id = format!("p{i}");
        c.send(json!({"t":"req","id": id,"method":"replay.read","params": p}))
            .await;
        let pg = c.res(&id).await;
        paged.extend(pg["result"]["events"].as_array().unwrap().iter().cloned());
        cursor = pg["result"]["next"].as_str().map(str::to_string);
        if pg["result"]["more"] != true {
            break;
        }
    }
    assert_eq!(paged.len(), events.len());

    // Listed for the owner.
    c.send(json!({"t":"req","id":"l1","method":"replay.list","params":{}}))
        .await;
    let l = c.res("l1").await;
    assert_eq!(l["result"]["recordings"][0]["pane"], pane, "{l}");
    assert_eq!(l["result"]["recordings"][0]["recording"], true);
    assert_eq!(l["result"]["retention"]["days"], 14);

    // `later` becomes secret: the files are masked again.
    c.send(json!({"t":"config","shell":{"aliases":{},"env":{"API_SECRET": secret, "OTHER_TOKEN": later},"rc":""}}))
        .await;
    let until = Instant::now() + S;
    loop {
        let files = recorded_files(dir.path());
        if !files.contains(&later) {
            assert!(files.contains("v ****"), "masked in place");
            break;
        }
        assert!(Instant::now() < until, "the recording was masked again");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(!recorded_files(dir.path()).contains(&secret));
    // Recording goes on after (a new part opens on the next output).
    h.ok(
        "pane.send_text",
        json!({"pane_id": pane, "text": "echo THREE-$((40+2))\n"}),
    )
    .await;
    h.wait_text(pane, "THREE-42").await;
    let until = Instant::now() + S;
    loop {
        c.send(json!({"t":"req","id":"r3","method":"replay.read","params":{"pane": pane, "limit_bytes": 524288}}))
            .await;
        let p = c.res("r3").await;
        let o = outputs(&p);
        assert!(!o.contains(&later) && !o.contains(&secret));
        if o.contains("THREE-42\r\n") {
            break;
        }
        assert!(Instant::now() < until, "recording continues after the mask grew");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Bad params, an unknown session, and the gone recording.
    c.send(json!({"t":"req","id":"b","method":"replay.read","params":{"pane": pane, "cursor": "x"}}))
        .await;
    assert_eq!(c.res("b").await["error"]["code"], "invalid_params");
    c.send(json!({"t":"req","id":"u","method":"replay.set","params":{"pane": "w9:p9", "on": true}}))
        .await;
    assert_eq!(c.res("u").await["error"]["code"], "not_found");
    c.send(json!({"t":"req","id":"off","method":"replay.set","params":{"pane": pane, "on": false}}))
        .await;
    assert_eq!(c.res("off").await["result"]["recording"], false);
    c.send(json!({"t":"req","id":"d","method":"replay.delete","params":{"pane": pane}}))
        .await;
    let d = c.res("d").await;
    assert!(d["result"]["bytes"].as_u64().unwrap() > 0, "{d}");
    c.send(json!({"t":"req","id":"r4","method":"replay.read","params":{"pane": pane}}))
        .await;
    assert_eq!(c.res("r4").await["error"]["code"], "not_found");
    c.send(json!({"t":"req","id":"l2","method":"replay.list","params":{}}))
        .await;
    assert_eq!(c.res("l2").await["result"]["recordings"], json!([]));
    task.abort();
}
