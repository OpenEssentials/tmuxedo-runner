//! Phase 4 end to end with a scripted fake `claude`: the pane's PATH shim
//! runs it with tmuxedo's generated `--settings`, it drives the real
//! `tmuxedo-runner hook` binary through SessionStart → UserPromptSubmit →
//! PermissionRequest (answered by a `decision` frame from the gateway) →
//! a second PermissionRequest answered at the terminal (PostToolUse) →
//! Stop, and the runner reports working → blocked → done with prompt ids
//! and durable, monotonic events.

mod common;

use common::{options, start_holder, FakeGateway, H};
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;
use tmuxedo_runner::daemon;

const S: Duration = Duration::from_secs(15);

const FAKE_CLAUDE: &str = r#"#!/bin/sh
settings=""
while [ $# -gt 0 ]; do
  case "$1" in
    --settings) settings="$2"; shift 2 ;;
    *) shift ;;
  esac
done
[ -f "$settings" ] || { echo "FAKE: no settings"; exit 3; }
cmd=$(sed -n 's/.*"command": "\(.*\)",$/\1/p' "$settings" | head -n 1)
hook() { printf '%s' "$1" | eval "$cmd"; }
sid=fake-session-1
echo "FAKE CLAUDE up"
hook '{"hook_event_name":"SessionStart","session_id":"'$sid'","cwd":"'"$PWD"'","source":"startup"}'
printf 'prompt> '
read line
hook '{"hook_event_name":"UserPromptSubmit","session_id":"'$sid'","prompt":"x"}'
out=$(hook '{"hook_event_name":"PermissionRequest","session_id":"'$sid'","tool_name":"Bash","tool_input":{"command":"touch x"},"permission_suggestions":[]}')
echo "DECISION1:$out"
hook '{"hook_event_name":"PostToolUse","session_id":"'$sid'","tool_name":"Bash"}'
( out2=$(hook '{"hook_event_name":"PermissionRequest","session_id":"'$sid'","tool_name":"Bash","tool_input":{"command":"rm y"}}'); echo "DECISION2:[$out2]" ) &
sleep 1
hook '{"hook_event_name":"PostToolUse","session_id":"'$sid'","tool_name":"Bash"}'
wait
hook '{"hook_event_name":"Stop","session_id":"'$sid'"}'
echo "FAKE DONE"
read line2
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fake_claude_full_cycle_through_hooks() {
    let dir = common::tempdir();
    let real = dir.path().join("real-bin");
    std::fs::create_dir_all(&real).unwrap();
    std::fs::write(real.join("claude"), FAKE_CLAUDE).unwrap();
    std::fs::set_permissions(real.join("claude"), std::fs::Permissions::from_mode(0o755)).unwrap();
    // Panes inherit the runner's PATH behind the shim.
    std::env::set_var(
        "PATH",
        format!("{}:{}", real.display(), std::env::var("PATH").unwrap_or_default()),
    );

    let holder = start_holder(dir.path());
    let gw = FakeGateway::start().await;
    let o = options(dir.path(), &gw.url, holder);
    let sock = o.runner_socket.clone();
    let task = tokio::spawn(daemon::run(o));
    let mut c = gw.accept(S).await.unwrap();
    c.hello(S).await;
    c.text_t(S, "topology").await;
    // The generated settings register every hook event, PermissionRequest with 600 s.
    let settings: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.path().join("state/claude-settings.json")).unwrap()).unwrap();
    assert_eq!(settings["hooks"]["PermissionRequest"][0]["hooks"][0]["timeout"], 600);
    assert!(
        std::fs::metadata(dir.path().join("state/shim/claude"))
            .unwrap()
            .permissions()
            .mode()
            & 0o111
            != 0
    );

    let h = H::attach_to(&sock).await;
    h.ok("pane.send_text", json!({"pane_id": "w1:p1", "text": "claude\n"}))
        .await;
    h.wait_text("w1:p1", "prompt>").await;
    // The process tier may publish a heuristic idle first; SessionStart's
    // reported idle must still reach the gateway (same status, new source).
    let st = c
        .text(S, |v| {
            v["t"] == "status" && v["agent"]["status"] == "idle" && v["agent"]["source"] == "reported"
        })
        .await;
    assert_eq!(st["agent"]["kind"], "claude");
    assert_eq!(st["agent"]["sessionId"], "fake-session-1");

    h.ok("pane.send_text", json!({"pane_id": "w1:p1", "text": "go\n"}))
        .await;
    c.text(S, |v| v["t"] == "status" && v["agent"]["status"] == "working")
        .await;
    let blocked = c
        .text(S, |v| v["t"] == "status" && v["agent"]["status"] == "blocked")
        .await;
    let prompt_id = blocked["agent"]["promptId"].as_str().unwrap().to_string();
    assert_eq!(blocked["agent"]["prompt"]["tool"], "Bash");
    assert_eq!(blocked["agent"]["prompt"]["input"]["command"], "touch x");
    let ev_blocked = c.text(S, |v| v["t"] == "event" && v["type"] == "blocked").await;
    assert_eq!(ev_blocked["data"]["promptId"], prompt_id.as_str());

    // The runner API sees the same state.
    let a = h.ok("agent.get", json!({"target": "w1:p1"})).await;
    assert_eq!(a["agent"]["agent_status"], "blocked");

    // A stale decision is refused; the right one is delivered to the hook.
    c.send(json!({"t": "decision", "paneId": "w1:p1", "promptId": "nope", "behavior": "deny"}))
        .await;
    c.text(S, |v| v["t"] == "event" && v["type"] == "decision_stale").await;
    c.send(json!({"t": "decision", "paneId": "w1:p1", "promptId": prompt_id, "behavior": "allow"}))
        .await;
    let text = h.wait_text("w1:p1", "DECISION1:").await.replace('\n', "");
    assert!(text.contains(r#""behavior":"allow""#), "{text}");
    assert!(text.contains(r#""hookEventName":"PermissionRequest""#), "{text}");

    // The second prompt is answered at the terminal (PostToolUse): the hook returns no decision.
    let second = c
        .text(S, |v| v["t"] == "status" && v["agent"]["status"] == "blocked")
        .await;
    assert_ne!(second["agent"]["promptId"].as_str(), Some(prompt_id.as_str()));
    let text = h.wait_text("w1:p1", "DECISION2:").await;
    assert!(text.contains("DECISION2:[]"), "{text}");

    // Stop → done; the agent wait API agrees.
    let done = c
        .text(S, |v| v["t"] == "status" && v["agent"]["status"] == "done")
        .await;
    assert!(done["agent"]["promptId"].is_null());
    let ev_done = c.text(S, |v| v["t"] == "event" && v["type"] == "done").await;
    assert!(
        ev_done["seq"].as_u64() > ev_blocked["seq"].as_u64(),
        "event seq is monotonic"
    );
    let w = h
        .ok(
            "agent.wait",
            json!({"target": "w1:p1", "until": ["done"], "timeout_ms": 3000}),
        )
        .await;
    assert_eq!(w["agent"]["agent_status"], "done");
    h.wait_text("w1:p1", "FAKE DONE").await;
    // The session id is kept for resume.
    let p = h.ok("pane.get", json!({"pane_id": "w1:p1"})).await;
    assert_eq!(p["pane"]["agent_session"]["value"], "fake-session-1");

    c.close(4003).await;
    let _ = tokio::time::timeout(S, task).await;
}
