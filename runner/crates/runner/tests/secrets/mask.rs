//! The terminal masker end to end (docs/design/secrets-and-env.md §5; PLAN
//! Phase 14 gate, runner half): a pane prints a secret raw, base64 and
//! URL-encoded, and no trace of it reaches OUTPUT frames, a snapshot,
//! `pane.read`, a glance, the history file, a text frame or an event.
//!
//! The secret here comes from the user's shell settings (a secret-named
//! variable in `config.shell.env`); vault deliveries feed the same set
//! (tests/secrets_delivery.rs).

use crate::common::{options, start_holder, FakeGateway, H};
use base64::Engine;
use serde_json::json;
use std::time::Duration;
use tmuxedo_protocol::DataKind;
use tmuxedo_runner::daemon;

const S: Duration = Duration::from_secs(20);

/// A generated fake (never a real credential), with characters that
/// percent-encoding changes.
fn fake_secret() -> String {
    let mut b = [0u8; 12];
    getrandom::getrandom(&mut b).unwrap();
    format!("fake/{}+x=@1", hex::encode(b))
}

/// Every form a leak could take (what the assertions look for).
fn forms(v: &str) -> Vec<String> {
    let pct_canonical: String = v
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect();
    let pct_all: String = v.bytes().map(|b| format!("%{b:02x}")).collect();
    vec![
        v.to_string(),
        // The part of the value that is random: a leak of it is a leak.
        v[5..29].to_string(),
        base64::engine::general_purpose::STANDARD.encode(v),
        base64::engine::general_purpose::STANDARD.encode(v)[..20].to_string(),
        pct_canonical,
        pct_all,
    ]
}

fn assert_clean(what: &str, hay: &[u8], secret: &str) {
    let hay = String::from_utf8_lossy(hay);
    for (i, f) in forms(secret).iter().enumerate() {
        // The index says which form (never the value itself).
        assert!(!hay.contains(f.as_str()), "{what} holds the secret in form #{i}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_printed_secret_reaches_no_artifact() {
    let secret = fake_secret();
    let dir = crate::common::tempdir();
    let holder = start_holder(dir.path());
    let gw = FakeGateway::start().await;
    let mut o = options(dir.path(), &gw.url, holder);
    o.pane_history = true;
    // Saved on every persist here (a real runner waits 30 s between saves of a busy pane).
    o.history_save_every = Duration::ZERO;
    let sock = o.runner_socket.clone();
    let task = tokio::spawn(daemon::run(o));
    let mut c = gw.accept(S).await.expect("runner dials out");
    c.hello(S).await;
    c.text_t(S, "topology").await;
    // The user's shell settings hold a secret-named variable.
    c.send(json!({"t":"config","shell":{"aliases":{},"env":{"GH_TOKEN": secret},"rc":""}}))
        .await;
    // Applied (config.json is written while the mask set is updated) before the pane starts.
    crate::wait_file_has(&dir.path().join("state/config.json"), "GH_TOKEN").await;
    let h = H::attach_to(&sock).await;
    // A new pane gets it in its environment.
    let split = h
        .ok("pane.split", json!({"target_pane_id":"w1:p1","direction":"right"}))
        .await;
    let pane = split["pane"]["pane_id"].as_str().unwrap().to_string();
    h.wait_alive(&pane).await;
    c.send(json!({"t":"attach","req":7,"paneId": pane})).await;
    c.send(json!({"t":"glance","paneIds":[pane],"hz":2})).await;
    // Raw, base64 (alone and inside a larger blob), canonical and fully
    // escaped percent-encoding; the canonical form is also typed, so the
    // shell's echo of the command line carries it.
    let canonical = &forms(&secret)[4];
    c.input(
        &pane,
        &format!(
            "printf 'raw=%s\\n' \"$GH_TOKEN\"; printf %s \"$GH_TOKEN\" | base64; printf 'user:%s' \"$GH_TOKEN\" | base64; \
             printf %s \"$GH_TOKEN\" | od -An -v -tx1 | tr -d ' \\n' | sed 's/../%&/g'; echo; echo 'url={canonical}'; echo DONE-PRINT-$((40+2))\n"
        ),
    )
    .await;
    let frames = c.output_until(0, "DONE-PRINT-42\r\n", S).await;
    let live: Vec<u8> = frames.iter().flat_map(|f| f.payload.clone()).collect();
    assert_clean("OUTPUT frames", &live, &secret);
    let text = String::from_utf8_lossy(&live);
    assert!(text.contains("raw=****"), "the raw print is masked in place");
    // Same length: offsets are contiguous across every frame.
    let (start, end, bytes) = crate::common::contiguous(&frames);
    assert_eq!(end - start, bytes.len() as u64);

    // pane.read, every source.
    for source in ["visible", "recent", "recent_unwrapped", "detection", "history"] {
        let r = h.ok("pane.read", json!({"pane_id": pane, "source": source})).await;
        assert_clean(&format!("pane.read {source}"), r.to_string().as_bytes(), &secret);
    }
    // A fresh viewer gets a snapshot.
    c.send(json!({"t":"attach","req":8,"paneId": pane})).await;
    let snap = c.bin(S, |f| f.req == 8 && f.kind == DataKind::Snapshot).await;
    assert_clean("SNAPSHOT", &snap.payload, &secret);
    assert!(String::from_utf8_lossy(&snap.payload).contains("DONE-PRINT"));
    // Replay from offset 0 (a reconnecting viewer).
    c.send(json!({"t":"attach","req":9,"paneId": pane, "from": 0})).await;
    let replay = c.output_until(9, "DONE-PRINT", S).await;
    let replay: Vec<u8> = replay.iter().flat_map(|f| f.payload.clone()).collect();
    assert_clean("OUTPUT replay", &replay, &secret);
    // Glances.
    let g = c
        .text(S, |v| v["t"] == "glance" && v.to_string().contains("DONE-PRINT"))
        .await;
    assert_clean("glance", g.to_string().as_bytes(), &secret);

    // Text frames: a pane name and an open_url event that carry the value.
    h.ok(
        "pane.rename",
        json!({"pane_id": pane, "label": format!("deploy {secret}")}),
    )
    .await;
    let topo = c
        .text(S, |v| v["t"] == "topology" && v.to_string().contains("deploy "))
        .await;
    assert_clean("topology", topo.to_string().as_bytes(), &secret);
    c.input(&pane, &format!("\"$BROWSER\" 'https://example.com/x?t={canonical}'\n"))
        .await;
    let ev = c.text(S, |v| v["t"] == "event" && v["type"] == "open_url").await;
    assert_clean("open_url event", ev.to_string().as_bytes(), &secret);
    // Durable events are masked before they reach the on-disk outbox.
    let outbox = std::fs::read(dir.path().join("state/events.jsonl")).unwrap_or_default();
    assert_clean("event outbox", &outbox, &secret);

    // The history file (saved on persist; a pane rename persists).
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut seen_history = false;
    for e in std::fs::read_dir(dir.path().join("state/history")).unwrap() {
        let b = std::fs::read(e.unwrap().path()).unwrap();
        assert_clean("history file", &b, &secret);
        seen_history |= String::from_utf8_lossy(&b).contains("DONE-PRINT");
    }
    assert!(seen_history, "the history file was written");
    for f in c.bins.iter() {
        assert_clean("other binary frames", &f.payload, &secret);
    }
    for t in c.texts.iter() {
        assert_clean("other text frames", t.to_string().as_bytes(), &secret);
    }
    c.close(4003).await;
    let _ = tokio::time::timeout(S, task).await;
}

/// A value printed before it became secret is masked once it is: the
/// runner re-reads the pane from the holder ring (mirror, screen, history).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_value_printed_before_it_was_secret_is_masked_afterwards() {
    let secret = fake_secret();
    let dir = crate::common::tempdir();
    let holder = start_holder(dir.path());
    let gw = FakeGateway::start().await;
    let mut o = options(dir.path(), &gw.url, holder);
    o.pane_history = true;
    let sock = o.runner_socket.clone();
    let task = tokio::spawn(daemon::run(o));
    let mut c = gw.accept(S).await.expect("runner dials out");
    c.hello(S).await;
    let h = H::attach_to(&sock).await;
    h.ok(
        "pane.send_text",
        json!({"pane_id":"w1:p1","text": format!("echo 'early {secret}'; echo EARLY-END-$((40+2))\n")}),
    )
    .await;
    let t = h.wait_text("w1:p1", "EARLY-END-42").await;
    assert!(t.contains(&secret), "not masked before it is a secret");
    c.send(json!({"t":"config","shell":{"aliases":{},"env":{"API_SECRET": secret},"rc":""}}))
        .await;
    let until = std::time::Instant::now() + S;
    loop {
        let r = h
            .ok("pane.read", json!({"pane_id":"w1:p1","source":"recent_unwrapped"}))
            .await;
        let text = r["read"]["text"].as_str().unwrap_or("").to_string();
        if text.contains("EARLY-END-42") && !text.contains(&secret) {
            assert!(text.contains("early ****"), "{}", text.len());
            break;
        }
        assert!(std::time::Instant::now() < until, "never re-masked");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    c.send(json!({"t":"attach","req":3,"paneId":"w1:p1"})).await;
    let snap = c.bin(S, |f| f.req == 3 && f.kind == DataKind::Snapshot).await;
    assert_clean("SNAPSHOT after re-mask", &snap.payload, &secret);
    // The long history is rebuilt from the masked ring too (decision 0009).
    let r = h
        .ok("pane.read", json!({"pane_id":"w1:p1","source":"history","lines":10000}))
        .await;
    assert_clean("history after re-mask", r.to_string().as_bytes(), &secret);
    c.close(4003).await;
    let _ = tokio::time::timeout(S, task).await;
}
