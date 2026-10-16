//! The hook guard, the PostToolUse rewrite and the transcript scan
//! (secrets-and-env.md §4.3, §5.3; decisions/0005) through the real hook
//! binary and daemon.

use crate::common::{options, start_holder, FakeGateway, H};
use serde_json::{json, Value};
use std::io::Write;
use std::time::Duration;
use tmuxedo_runner::daemon;
use tmuxedo_runner::secrets::seal;

const S: Duration = Duration::from_secs(20);
const BIN: &str = env!("CARGO_BIN_EXE_tmuxedo-runner");

fn hook(sock: &std::path::Path, ev: Value) -> String {
    let mut child = std::process::Command::new(BIN)
        .arg("hook")
        .env("TMUXEDO_SOCKET_PATH", sock)
        .env("TMUXEDO_PANE_ID", "w1:p1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(ev.to_string().as_bytes())
        .unwrap();
    String::from_utf8_lossy(&child.wait_with_output().unwrap().stdout).into_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn guard_rewrite_and_transcript_scan() {
    let mut b = [0u8; 12];
    getrandom::getrandom(&mut b).unwrap();
    let value = format!("fake-guard-{}", hex::encode(b));
    let dir = crate::common::tempdir();
    let holder = start_holder(dir.path());
    let gw = FakeGateway::start().await;
    let o = options(dir.path(), &gw.url, holder);
    let sock = o.runner_socket.clone();
    let task = tokio::spawn(daemon::run(o));
    let mut c = gw.accept(S).await.unwrap();
    c.hello(S).await;
    let key = c.text(S, |v| v["t"] == "secret_key").await["publicKey"]
        .as_str()
        .unwrap()
        .to_string();
    let _h = H::attach_to(&sock).await;
    let pre = |cmd: &str| json!({"hook_event_name":"PreToolUse","session_id":"s1","tool_name":"Bash","tool_input":{"command": cmd}});

    // No secrets on the machine: the guard stays out of the way.
    assert_eq!(hook(&sock, pre("printenv")), "");
    let settings = dir.path().join("state/claude-settings.json");
    let before: Value = serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
    assert!(before["hooks"].get("PreToolUse").is_none());

    let mask = seal::seal(
        &key,
        json!({"values": [{"name": "GH_TOKEN", "value": value}]})
            .to_string()
            .as_bytes(),
        &seal::mask_context("m1"),
    )
    .unwrap();
    c.send(json!({"t":"secrets","mask": mask,"policy":{"names":[{"name":"GH_TOKEN","bindings":[{"kind":"exec","programs":["gh"]},{"kind":"agent","plugin":"claude"}]}],"offlineCache":false}}))
        .await;
    // The generated settings gain the guard's hook; the shim the agent-env line.
    let until = std::time::Instant::now() + S;
    loop {
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
        if v["hooks"].get("PreToolUse").is_some() {
            assert_eq!(v["hooks"]["PreToolUse"][0]["matcher"], "Bash");
            break;
        }
        assert!(std::time::Instant::now() < until, "guard hook never registered");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    loop {
        let shim = std::fs::read_to_string(dir.path().join("state/shim/claude")).unwrap_or_default();
        if shim.contains("cli secret agent-env") {
            break;
        }
        assert!(std::time::Instant::now() < until, "the shim never read agent env");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Env dumps and named reads are denied with a way forward; ordinary commands pass.
    for cmd in ["printenv", "env | sort", "echo $GH_TOKEN", "cat /proc/self/environ"] {
        let out: Value = serde_json::from_str(&hook(&sock, pre(cmd))).unwrap_or(Value::Null);
        assert_eq!(out["hookSpecificOutput"]["permissionDecision"], "deny", "{cmd}");
        assert!(out.to_string().contains("tmuxedo secret exec GH_TOKEN"));
    }
    assert_eq!(hook(&sock, pre("git status")), "");
    assert_eq!(hook(&sock, pre("tmuxedo secret exec GH_TOKEN -- gh api user")), "");

    // PostToolUse: output holding the value goes back to the model masked.
    let post = |stdout: &str| json!({"hook_event_name":"PostToolUse","session_id":"s1","tool_name":"Bash","tool_input":{"command":"x"},"tool_response":{"stdout": stdout,"stderr":"","interrupted":false}});
    let out: Value = serde_json::from_str(&hook(&sock, post(&format!("token is {value}\n")))).unwrap();
    let rewritten = &out["hookSpecificOutput"]["updatedToolOutput"];
    assert_eq!(out["hookSpecificOutput"]["hookEventName"], "PostToolUse");
    assert_eq!(rewritten["stdout"], format!("token is {}\n", "*".repeat(value.len())));
    assert_eq!(hook(&sock, post("nothing secret here")), "", "no identity rewrite");

    // A transcript that holds the value raises secret_exposed (names only) on the turn's end, once.
    let transcript = dir.path().join("t.jsonl");
    std::fs::write(&transcript, "{\"type\":\"user\"}\n").unwrap();
    let stop = json!({"hook_event_name":"Stop","session_id":"s1","transcript_path": transcript});
    hook(&sock, stop.clone());
    c.drain(Duration::from_millis(300)).await;
    assert!(!c.texts.iter().any(|v| v["type"] == "secret_exposed"));
    let b64 = {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(&value)
    };
    std::fs::OpenOptions::new()
        .append(true)
        .open(&transcript)
        .unwrap()
        .write_all(format!("{{\"type\":\"tool_result\",\"content\":\"{b64}\"}}\n").as_bytes())
        .unwrap();
    hook(&sock, stop.clone());
    let ev = c.text(S, |v| v["t"] == "event" && v["type"] == "secret_exposed").await;
    assert_eq!(ev["paneId"], "w1:p1");
    assert_eq!(ev["data"]["name"], "GH_TOKEN");
    assert_eq!(ev["data"]["plugin"], "claude");
    assert!(!ev.to_string().contains(&value) && !ev.to_string().contains(&b64));
    hook(&sock, stop);
    c.drain(Duration::from_millis(500)).await;
    assert!(!c.texts.iter().any(|v| v["type"] == "secret_exposed"), "reported once");

    // Guard off for the plugin: the hook registration goes away.
    c.send(json!({"t":"secrets","policy":{"names":[{"name":"GH_TOKEN","bindings":[]}],"guardOff":["claude"]}}))
        .await;
    let until = std::time::Instant::now() + S;
    loop {
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
        if v["hooks"].get("PreToolUse").is_none() {
            break;
        }
        assert!(std::time::Instant::now() < until);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(hook(&sock, pre("printenv")), "");
    let shim = std::fs::read_to_string(dir.path().join("state/shim/claude")).unwrap();
    assert!(!shim.contains("cli secret agent-env"), "no agent binding, no line");
    c.close(4003).await;
    let _ = tokio::time::timeout(S, task).await;
}
