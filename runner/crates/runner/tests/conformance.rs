//! Runner API conformance: one test per implemented method. Each request is
//! validated against tmuxedo's runner API schema (`#/schemas/request`), each
//! answer against `#/schemas/success_response` (or `error_response`), and
//! streamed events against `#/schemas/event` / `#/schemas/subscription_event`
//! (runner/api/runner-api.schema.json).

mod common;

use common::{next_json, schema_check, subscribe, H};
use serde_json::json;

macro_rules! conformance {
    ($($name:ident => $body:expr;)*) => {$(
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn $name() {
            let h = H::start().await;
            let f: &dyn Fn(&H) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + '_>> = &$body;
            f(&h).await;
        }
    )*};
}

async fn report_claude(h: &H, pane: &str, state: &str) {
    h.ok(
        "pane.report_agent",
        json!({"pane_id": pane, "source": "test", "agent": "claude", "state": state}),
    )
    .await;
}

/// A git repository with one commit at `<h.dir>/repo`, opened as workspace w2.
async fn git_repo(h: &H) -> std::path::PathBuf {
    let repo = h.dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let git = |args: &[&str]| {
        let st = std::process::Command::new("git")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.com",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .current_dir(&repo)
            .output()
            .unwrap();
        assert!(
            st.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&st.stderr)
        );
    };
    git(&["init", "-q"]);
    std::fs::write(repo.join("README"), "hi\n").unwrap();
    git(&["add", "README"]);
    git(&["commit", "-q", "-m", "init"]);
    let r = h.ok("workspace.create", json!({"cwd": repo.to_string_lossy()})).await;
    assert_eq!(r["workspace"]["workspace_id"], "w2");
    repo
}

conformance! {
    ping => |h| Box::pin(async move {
        let r = h.ok("ping", json!({})).await;
        assert_eq!(r["type"], "pong");
    });
    server_agent_manifests => |h| Box::pin(async move {
        let r = h.ok("server.agent_manifests", json!({})).await;
        let rows = r["manifests"].as_array().unwrap();
        assert_eq!(rows.len(), 24);
        assert_eq!(rows[0]["agent"], "claude");
        assert_eq!(rows[0]["source"], "built-in");
        assert!(rows.iter().all(|m| m["warning"].is_null()));
    });
    workspace_list => |h| Box::pin(async move {
        let r = h.ok("workspace.list", json!({})).await;
        assert_eq!(r["workspaces"][0]["workspace_id"], "w1");
        assert_eq!(r["workspaces"][0]["active_tab_id"], "w1:t1");
    });
    workspace_create => |h| Box::pin(async move {
        let r = h.ok("workspace.create", json!({"cwd": "/tmp", "label": "two", "focus": true})).await;
        assert_eq!(r["workspace"]["workspace_id"], "w2");
        assert_eq!(r["workspace"]["label"], "two");
        assert_eq!(r["root_pane"]["pane_id"], "w2:p1");
        assert_eq!(r["root_pane"]["cwd"], "/tmp");
        h.wait_alive("w2:p1").await;
        assert!(h.fails("workspace.create", json!({"cwd": "/no/such/dir"})).await["code"] == "invalid_cwd");
    });
    workspace_get => |h| Box::pin(async move {
        assert_eq!(h.ok("workspace.get", json!({"workspace_id": "w1"})).await["workspace"]["pane_count"], 1);
        assert_eq!(h.fails("workspace.get", json!({"workspace_id": "w9"})).await["code"], "workspace_not_found");
    });
    workspace_focus => |h| Box::pin(async move {
        h.ok("workspace.create", json!({})).await;
        let r = h.ok("workspace.focus", json!({"workspace_id": "w2"})).await;
        assert_eq!(r["workspace"]["focused"], true);
    });
    workspace_rename => |h| Box::pin(async move {
        assert_eq!(h.ok("workspace.rename", json!({"workspace_id": "w1", "label": "main"})).await["workspace"]["label"], "main");
    });
    workspace_move => |h| Box::pin(async move {
        h.ok("workspace.create", json!({})).await;
        h.ok("workspace.move", json!({"workspace_id": "w2", "insert_index": 0})).await;
        assert_eq!(h.ok("workspace.list", json!({})).await["workspaces"][0]["workspace_id"], "w2");
    });
    workspace_close => |h| Box::pin(async move {
        h.ok("workspace.create", json!({})).await;
        assert_eq!(h.ok("workspace.close", json!({"workspace_id": "w1"})).await["type"], "ok");
        let list = h.ok("workspace.list", json!({})).await;
        assert_eq!(list["workspaces"].as_array().unwrap().len(), 1);
    });
    tab_list => |h| Box::pin(async move {
        assert_eq!(h.ok("tab.list", json!({"workspace_id": "w1"})).await["tabs"][0]["tab_id"], "w1:t1");
    });
    tab_create => |h| Box::pin(async move {
        let r = h.ok("tab.create", json!({"workspace_id": "w1", "label": "logs", "focus": true})).await;
        assert_eq!(r["tab"]["tab_id"], "w1:t2");
        assert_eq!(r["root_pane"]["pane_id"], "w1:p2");
    });
    tab_get => |h| Box::pin(async move {
        assert_eq!(h.ok("tab.get", json!({"tab_id": "w1:t1"})).await["tab"]["pane_count"], 1);
    });
    tab_focus => |h| Box::pin(async move {
        h.ok("tab.create", json!({})).await;
        assert_eq!(h.ok("tab.focus", json!({"tab_id": "w1:t1"})).await["tab"]["focused"], true);
    });
    tab_rename => |h| Box::pin(async move {
        assert_eq!(h.ok("tab.rename", json!({"tab_id": "w1:t1", "label": "x"})).await["tab"]["label"], "x");
    });
    tab_move => |h| Box::pin(async move {
        h.ok("tab.create", json!({})).await;
        h.ok("tab.move", json!({"tab_id": "w1:t2", "insert_index": 0})).await;
        assert_eq!(h.ok("tab.list", json!({"workspace_id": "w1"})).await["tabs"][0]["tab_id"], "w1:t2");
    });
    tab_close => |h| Box::pin(async move {
        h.ok("tab.create", json!({})).await;
        h.ok("tab.close", json!({"tab_id": "w1:t2"})).await;
        assert_eq!(h.ok("tab.list", json!({})).await["tabs"].as_array().unwrap().len(), 1);
    });
    pane_split => |h| Box::pin(async move {
        let r = h.ok("pane.split", json!({"target_pane_id": "w1:p1", "direction": "right", "ratio": 0.4})).await;
        assert_eq!(r["pane"]["pane_id"], "w1:p2");
        h.wait_alive("w1:p2").await;
        let l = h.ok("layout.export", json!({"tab_id": "w1:t1"})).await;
        assert_eq!(l["layout"]["root"]["type"], "split");
        assert_eq!(l["layout"]["root"]["direction"], "right");
    });
    pane_list => |h| Box::pin(async move {
        let r = h.ok("pane.list", json!({})).await;
        assert_eq!(r["panes"][0]["pane_id"], "w1:p1");
        assert_eq!(r["panes"][0]["terminal_id"], "w1:p1#1");
    });
    pane_get => |h| Box::pin(async move {
        assert_eq!(h.ok("pane.get", json!({"pane_id": "w1:p1"})).await["pane"]["focused"], true);
        assert_eq!(h.fails("pane.get", json!({"pane_id": "w1:p9"})).await["code"], "pane_not_found");
    });
    pane_current => |h| Box::pin(async move {
        assert_eq!(h.ok("pane.current", json!({})).await["pane"]["pane_id"], "w1:p1");
    });
    pane_read => |h| Box::pin(async move {
        h.ok("pane.send_text", json!({"pane_id": "w1:p1", "text": "echo read-$((2*3))\n"})).await;
        h.wait_text("w1:p1", "read-6").await;
        for source in ["visible", "recent", "recent_unwrapped", "detection"] {
            let r = h.ok("pane.read", json!({"pane_id": "w1:p1", "source": source, "lines": 50})).await;
            assert!(r["read"]["text"].as_str().unwrap().contains("read-6"), "{source}: {r}");
        }
        let r = h.ok("pane.read", json!({"pane_id": "w1:p1", "source": "visible", "format": "ansi"})).await;
        assert_eq!(r["read"]["format"], "ansi");
    });
    pane_send_text => |h| Box::pin(async move {
        h.ok("pane.send_text", json!({"pane_id": "w1:p1", "text": "echo st-$((7*6))\n"})).await;
        h.wait_text("w1:p1", "st-42").await;
    });
    pane_send_keys => |h| Box::pin(async move {
        // `k''9` echoes as typed but runs as `k9`, so the needle only appears
        // once the shell executed it, wherever its prompt landed.
        h.ok("pane.send_keys", json!({"pane_id": "w1:p1", "keys": ["e","c","h","o","Space","k","'","'","9","Enter"]})).await;
        h.wait_text("w1:p1", "k9").await;
        assert_eq!(h.fails("pane.send_keys", json!({"pane_id": "w1:p1", "keys": ["NoSuchKey"]})).await["code"], "invalid_key");
    });
    pane_send_input => |h| Box::pin(async move {
        h.ok("pane.send_input", json!({"pane_id": "w1:p1", "text": "echo si-$((3+4))", "keys": ["Enter"]})).await;
        h.wait_text("w1:p1", "si-7").await;
    });
    pane_resize => |h| Box::pin(async move {
        h.ok("pane.split", json!({"target_pane_id": "w1:p1", "direction": "down"})).await;
        let r = h.ok("pane.resize", json!({"pane_id": "w1:p1", "direction": "down", "amount": 0.1})).await;
        assert_eq!(r["resize"]["changed"], true);
        let r = h.ok("pane.resize", json!({"pane_id": "w1:p1", "direction": "left"})).await;
        assert_eq!(r["resize"]["changed"], false);
    });
    pane_zoom => |h| Box::pin(async move {
        assert_eq!(h.ok("pane.zoom", json!({"pane_id": "w1:p1"})).await["zoom"]["reason"], "single_pane");
        h.ok("pane.split", json!({"target_pane_id": "w1:p1", "direction": "right"})).await;
        let r = h.ok("pane.zoom", json!({"pane_id": "w1:p2", "mode": "on"})).await;
        assert_eq!(r["zoom"]["zoomed"], true);
        assert_eq!(r["zoom"]["layout"]["panes"].as_array().unwrap().len(), 1);
    });
    pane_move => |h| Box::pin(async move {
        h.ok("pane.split", json!({"target_pane_id": "w1:p1", "direction": "right"})).await;
        let r = h.ok("pane.move", json!({"pane_id": "w1:p2", "destination": {"type": "new_tab", "label": "moved"}})).await;
        assert_eq!(r["move_result"]["changed"], true);
        assert_eq!(r["move_result"]["created_tab"]["label"], "moved");
        let r = h.ok("pane.move", json!({"pane_id": "w1:p2", "destination": {"type": "new_workspace"}})).await;
        assert_eq!(r["move_result"]["created_workspace"]["workspace_id"], "w2");
        assert_eq!(r["move_result"]["closed_tab_id"], "w1:t2");
        let r = h.ok("pane.move", json!({"pane_id": "w1:p2", "destination": {"type": "tab", "tab_id": "w1:t1", "split": "down"}})).await;
        assert_eq!(r["move_result"]["closed_workspace_id"], "w2");
    });
    pane_swap => |h| Box::pin(async move {
        h.ok("pane.split", json!({"target_pane_id": "w1:p1", "direction": "right"})).await;
        let r = h.ok("pane.swap", json!({"source_pane_id": "w1:p1", "target_pane_id": "w1:p2"})).await;
        assert_eq!(r["swap"]["changed"], true);
        let r = h.ok("pane.swap", json!({"pane_id": "w1:p2", "direction": "right"})).await;
        assert_eq!(r["swap"]["changed"], true);
        let r = h.ok("pane.swap", json!({"pane_id": "w1:p2", "direction": "up"})).await;
        assert_eq!(r["swap"]["reason"], "no_neighbor");
    });
    pane_close => |h| Box::pin(async move {
        h.ok("pane.split", json!({"target_pane_id": "w1:p1", "direction": "right"})).await;
        h.ok("pane.close", json!({"pane_id": "w1:p2"})).await;
        assert_eq!(h.ok("pane.list", json!({})).await["panes"].as_array().unwrap().len(), 1);
    });
    pane_rename => |h| Box::pin(async move {
        assert_eq!(h.ok("pane.rename", json!({"pane_id": "w1:p1", "label": "build"})).await["pane"]["label"], "build");
    });
    pane_wait_for_output => |h| Box::pin(async move {
        // The shell must be running before the timed wait starts: the pane answers reads
        // as soon as it exists, and on a loaded host the shell can take most of the wait's
        // budget just to start, so the wait itself would be measuring shell start-up.
        h.ok("pane.send_text", json!({"pane_id": "w1:p1", "text": "echo wo-up-$((3+4))\n"})).await;
        h.wait_text("w1:p1", "wo-up-7").await;
        h.ok("pane.send_text", json!({"pane_id": "w1:p1", "text": "sleep 0.3; echo wo-$((5+5))\n"})).await;
        // 25 s, not 5: with ~67 PTY tests in parallel on a loaded host the shell
        // has taken over 15 s to print. It stays under the harness's 30 s reply
        // timeout, and a match still returns as soon as it appears.
        let r = h.ok("pane.wait_for_output", json!({"pane_id": "w1:p1", "source": "recent", "match": {"type": "regex", "value": "^wo-1[0-9]$"}, "timeout_ms": 25000})).await;
        assert_eq!(r["type"], "output_matched");
        assert_eq!(r["matched_line"], "wo-10");
        let e = h.fails("pane.wait_for_output", json!({"pane_id": "w1:p1", "source": "recent", "match": {"type": "substring", "value": "never-printed"}, "timeout_ms": 200})).await;
        assert_eq!(e["code"], "timeout");
        let e = h.fails("pane.wait_for_output", json!({"pane_id": "w1:p1", "source": "recent", "match": {"type": "regex", "value": "("}})).await;
        assert_eq!(e["code"], "invalid_regex");
    });
    pane_report_agent => |h| Box::pin(async move {
        report_claude(h, "w1:p1", "blocked").await;
        let p = h.ok("pane.get", json!({"pane_id": "w1:p1"})).await;
        assert_eq!(p["pane"]["agent"], "claude");
        assert_eq!(p["pane"]["agent_status"], "blocked");
    });
    pane_report_agent_session => |h| Box::pin(async move {
        h.ok("pane.report_agent_session", json!({"pane_id": "w1:p1", "source": "test", "agent": "claude", "agent_session_id": "s-1"})).await;
        let p = h.ok("pane.get", json!({"pane_id": "w1:p1"})).await;
        assert_eq!(p["pane"]["agent_session"]["value"], "s-1");
    });
    pane_report_metadata => |h| Box::pin(async move {
        h.ok("pane.report_metadata", json!({"pane_id": "w1:p1", "source": "test", "title": "T", "display_agent": "Claude", "state_labels": {"working": "thinking"}})).await;
        let p = h.ok("pane.get", json!({"pane_id": "w1:p1"})).await;
        assert_eq!(p["pane"]["title"], "T");
        assert_eq!(p["pane"]["state_labels"]["working"], "thinking");
    });
    pane_process_info => |h| Box::pin(async move {
        let r = h.ok("pane.process_info", json!({"pane_id": "w1:p1"})).await;
        assert!(r["process_info"]["shell_pid"].as_u64().unwrap() > 0);
        assert!(!r["process_info"]["foreground_processes"].as_array().unwrap().is_empty());
    });
    pane_clear => |h| Box::pin(async move {
        assert_eq!(h.ok("pane.clear", json!({"pane_id": "w1:p1"})).await["type"], "ok");
    });
    agent_list => |h| Box::pin(async move {
        assert!(h.ok("agent.list", json!({})).await["agents"].as_array().unwrap().is_empty());
        report_claude(h, "w1:p1", "idle").await;
        assert_eq!(h.ok("agent.list", json!({})).await["agents"][0]["pane_id"], "w1:p1");
    });
    agent_get => |h| Box::pin(async move {
        report_claude(h, "w1:p1", "working").await;
        assert_eq!(h.ok("agent.get", json!({"target": "w1:p1"})).await["agent"]["agent_status"], "working");
    });
    agent_read => |h| Box::pin(async move {
        report_claude(h, "w1:p1", "idle").await;
        h.ok("agent.read", json!({"target": "w1:p1", "source": "recent"})).await;
    });
    agent_rename => |h| Box::pin(async move {
        report_claude(h, "w1:p1", "idle").await;
        assert_eq!(h.ok("agent.rename", json!({"target": "w1:p1", "name": "rev"})).await["agent"]["name"], "rev");
        assert_eq!(h.ok("agent.get", json!({"target": "rev"})).await["agent"]["pane_id"], "w1:p1");
    });
    agent_send_keys => |h| Box::pin(async move {
        report_claude(h, "w1:p1", "idle").await;
        h.ok("agent.send_keys", json!({"target": "w1:p1", "keys": ["e","c","h","o","Space","a","k","Enter"]})).await;
        h.wait_text("w1:p1", "\nak").await;
    });
    agent_prompt => |h| Box::pin(async move {
        report_claude(h, "w1:p1", "idle").await;
        assert_eq!(h.ok("agent.prompt", json!({"target": "w1:p1", "text": "hello"})).await["type"], "agent_prompted");
        // With wait: answered when the agent next reaches a wanted status.
        let sock = h.sock.clone();
        let waiting = tokio::spawn(async move {
            common::call_once(&sock, &json!({"id": "w", "method": "agent.prompt", "params": {"target": "w1:p1", "text": "go", "wait": {"until": ["blocked"], "timeout_ms": 5000}}})).await
        });
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        report_claude(h, "w1:p1", "blocked").await;
        let r = waiting.await.unwrap();
        schema_check("success_response", &r).unwrap();
        assert_eq!(r["result"]["agent"]["agent_status"], "blocked");
    });
    agent_wait => |h| Box::pin(async move {
        report_claude(h, "w1:p1", "working").await;
        let sock = h.sock.clone();
        let waiting = tokio::spawn(async move {
            common::call_once(&sock, &json!({"id": "w", "method": "agent.wait", "params": {"target": "w1:p1", "until": ["idle"], "timeout_ms": 5000}})).await
        });
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        report_claude(h, "w1:p1", "idle").await;
        let r = waiting.await.unwrap();
        schema_check("success_response", &r).unwrap();
        assert_eq!(r["result"]["type"], "agent_info");
        let e = h.fails("agent.wait", json!({"target": "w1:p1", "until": ["blocked"], "timeout_ms": 100})).await;
        assert_eq!(e["code"], "timeout");
    });
    agent_start => |h| Box::pin(async move {
        let r = h.ok("agent.start", json!({"name": "c1", "kind": "claude", "pane_id": "w1:p1"})).await;
        assert_eq!(r["argv"][0], "claude");
        assert_eq!(r["agent"]["name"], "c1");
        assert_eq!(h.fails("agent.start", json!({"name": "x", "kind": "nope", "pane_id": "w1:p1"})).await["code"], "unknown_agent_kind");
        // No config.roles on this machine: every role is unknown.
        assert_eq!(h.fails("agent.start", json!({"name": "x", "kind": "claude", "pane_id": "w1:p1", "role": "reviewer"})).await["code"], "unknown_role");
    });
    agent_explain => |h| Box::pin(async move {
        report_claude(h, "w1:p1", "idle").await;
        let r = h.ok("agent.explain", json!({"target": "w1:p1"})).await;
        assert_eq!(r["explain"]["tmuxedo"]["effective_status"], "idle");
        assert_eq!(r["explain"]["aimux"], r["explain"]["tmuxedo"], "deprecated alias");
        // The reported status wins; the screen explanation is still there.
        assert_eq!(r["explain"]["agent"], "claude");
        assert!(r["explain"]["signals"].as_array().unwrap().len() > 10, "{r}");
        assert_eq!(r["explain"]["tmuxedo"]["source"], "reported");
        // A pane with no agent explains as unknown.
        h.ok("pane.split", json!({"target_pane_id": "w1:p1", "direction": "down"})).await;
        h.wait_alive("w1:p2").await;
        let none = h.ok("agent.explain", json!({"target": "w1:p2"})).await;
        assert_eq!(none["explain"]["agent"], serde_json::Value::Null, "{none}");
        assert_eq!(none["explain"]["status"], "unknown");
        assert_eq!(none["explain"]["signals"], json!([]));
        assert_eq!(none["explain"]["tmuxedo"]["process_kind"], serde_json::Value::Null);
    });
    events_subscribe => |h| Box::pin(async move {
        let mut lines = subscribe(&h.sock, json!([{"type": "workspace.created"}, {"type": "pane.agent_status_changed", "pane_id": "w1:p1"}])).await;
        h.ok("workspace.create", json!({})).await;
        let ev = next_json(&mut lines).await;
        schema_check("event", &ev).unwrap_or_else(|e| panic!("{e}: {ev}"));
        assert_eq!(ev["event"], "workspace_created");
        report_claude(h, "w1:p1", "blocked").await;
        let ev = next_json(&mut lines).await;
        schema_check("subscription_event", &ev).unwrap_or_else(|e| panic!("{e}: {ev}"));
        assert_eq!(ev["data"]["agent_status"], "blocked");
    });
    events_wait => |h| Box::pin(async move {
        let sock = h.sock.clone();
        let waiting = tokio::spawn(async move {
            common::call_once(&sock, &json!({"id": "w", "method": "events.wait", "params": {"match_event": {"event": "tab_created", "workspace_id": "w1"}, "timeout_ms": 5000}})).await
        });
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        h.ok("tab.create", json!({"workspace_id": "w1"})).await;
        let r = waiting.await.unwrap();
        schema_check("success_response", &r).unwrap_or_else(|e| panic!("{e}: {r}"));
        assert_eq!(r["result"]["event"]["event"], "tab_created");
        assert_eq!(h.fails("events.wait", json!({"match_event": {"event": "pane_closed", "pane_id": "w1:p1"}, "timeout_ms": 100})).await["code"], "timeout");
    });
    layout_export => |h| Box::pin(async move {
        let r = h.ok("layout.export", json!({"pane_id": "w1:p1"})).await;
        assert_eq!(r["layout"]["root"]["pane_id"], "w1:p1");
    });
    layout_set_split_ratio => |h| Box::pin(async move {
        h.ok("pane.split", json!({"target_pane_id": "w1:p1", "direction": "down"})).await;
        let r = h.ok("layout.set_split_ratio", json!({"tab_id": "w1:t1", "path": [], "ratio": 0.7})).await;
        assert!((r["layout"]["root"]["ratio"].as_f64().unwrap() - 0.7).abs() < 1e-9);
    });
    layout_apply => |h| Box::pin(async move {
        let root = json!({"type": "split", "direction": "right", "ratio": 0.5,
            "first": {"type": "pane", "cwd": "/tmp"},
            "second": {"type": "split", "direction": "down", "ratio": 0.5, "first": {"type": "pane"}, "second": {"type": "pane", "label": "logs"}}});
        let r = h.ok("layout.apply", json!({"workspace_id": "w1", "root": root, "tab_label": "dev"})).await;
        assert_eq!(r["layout"]["root"]["type"], "split");
        let tab = r["layout"]["tab_id"].as_str().unwrap().to_string();
        assert_eq!(h.ok("tab.get", json!({"tab_id": tab})).await["tab"]["pane_count"], 3);
        // Re-apply onto the same tab keeping one pane: the others close.
        let keep = r["layout"]["root"]["first"]["pane_id"].as_str().unwrap().to_string();
        // Guarded: with close_unnamed false it refuses and nothing closes.
        let guarded = json!({"tab_id": tab, "root": {"type": "pane", "pane_id": keep}, "close_unnamed": false});
        assert_eq!(h.fails("layout.apply", guarded).await["code"], "would_close");
        assert_eq!(h.fails("layout.apply", json!({"tab_id": tab, "root": {"type": "pane", "pane_id": keep}, "close_unnamed": "no"})).await["code"], "invalid_params");
        let twice = json!({"type": "split", "direction": "right", "ratio": 0.5, "first": {"type": "pane", "pane_id": keep}, "second": {"type": "pane", "pane_id": keep}});
        assert_eq!(h.fails("layout.apply", json!({"tab_id": tab, "root": twice})).await["code"], "invalid_params");
        assert_eq!(h.ok("tab.get", json!({"tab_id": tab})).await["tab"]["pane_count"], 3);
        let r = h.ok("layout.apply", json!({"tab_id": tab, "root": {"type": "pane", "pane_id": keep}})).await;
        assert_eq!(r["closed"].as_array().unwrap().len(), 2, "{r}");
        assert_eq!(h.ok("tab.get", json!({"tab_id": tab})).await["tab"]["pane_count"], 1);
    });
    layout_arrange => |h| Box::pin(async move {
        for _ in 0..4 {
            h.ok("pane.split", json!({"target_pane_id": "w1:p1", "direction": "right"})).await;
        }
        let ids = |v: &serde_json::Value| {
            let mut ids: Vec<String> = v["panes"].as_array().unwrap().iter().map(|p| p["pane_id"].as_str().unwrap().to_string()).collect();
            ids.sort();
            ids
        };
        let panes = ids(&h.ok("pane.list", json!({})).await);
        assert_eq!(panes.len(), 5);
        let start = h.ok("layout.export", json!({"tab_id": "w1:t1"})).await["layout"]["root"].clone();

        // A dry run answers with the arrangement and changes nothing.
        let dry = h.ok("layout.arrange", json!({"tab_id": "w1:t1", "pattern": "3_over_2", "dry_run": true})).await;
        assert_eq!(dry["dry_run"], true);
        assert_eq!(dry["layout"]["root"]["direction"], "down");
        assert!(dry["undo"].is_null());
        assert_eq!(h.ok("layout.export", json!({"tab_id": "w1:t1"})).await["layout"]["root"], start);

        // Refusals change nothing: a pattern with too few slots, a tree that leaves
        // panes out, a pane twice, a leaf that would open a pane, unknown names.
        let one = json!({"type": "pane", "pane_id": "w1:p1"});
        let cases = [
            (json!({"pattern": "grid_2x2"}), "wont_fit"),
            (json!({"root": one}), "would_close"),
            (json!({"root": {"type": "split", "direction": "right", "ratio": 0.5, "first": one, "second": one}}), "invalid_params"),
            (json!({"root": {"type": "new", "command": ["sh"]}}), "not_supported"),
            (json!({"pattern": "spiral"}), "invalid_params"),
            (json!({"pattern": "columns", "extras": "close"}), "invalid_params"),
            (json!({"pattern": "columns", "area": {"width": 0, "height": 40}}), "invalid_params"),
            (json!({"pattern": "columns", "focus": "w1:p99"}), "invalid_params"),
            (json!({"pattern": "columns", "expect_rev": 0}), "changed"),
            (json!({}), "invalid_params"),
        ];
        for (mut params, code) in cases {
            params["tab_id"] = json!("w1:t1");
            assert_eq!(h.fails("layout.arrange", params.clone()).await["code"], code, "{params}");
        }
        assert_eq!(h.fails("layout.arrange", json!({"tab_id": "w1:t9", "pattern": "columns"})).await["code"], "tab_not_found");
        assert_eq!(h.ok("layout.export", json!({"tab_id": "w1:t1"})).await["layout"]["root"], start);
        assert_eq!(h.ok("tab.list", json!({})).await["tabs"].as_array().unwrap().len(), 1);

        // Five panes into four slots: the fifth moves to a new tab. No pane closes.
        let r = h.ok("layout.arrange", json!({"tab_id": "w1:t1", "pattern": "grid_2x2", "extras": "new_tab"})).await;
        assert_eq!(r["pattern"], "grid_2x2");
        assert_eq!(r["layout"]["pattern"], "grid_2x2");
        assert_eq!(r["extras"].as_array().unwrap().len(), 1);
        assert_eq!(r["opened"].as_array().unwrap().len(), 0);
        let extras_tab = r["extras_tab"].as_str().unwrap().to_string();
        assert_eq!(h.ok("tab.get", json!({"tab_id": "w1:t1"})).await["tab"]["pane_count"], 4);
        assert_eq!(h.ok("tab.get", json!({"tab_id": extras_tab})).await["tab"]["pane_count"], 1);
        assert_eq!(ids(&h.ok("pane.list", json!({})).await), panes);
        for p in &panes {
            h.wait_alive(p).await;
        }

        // Undo: the answer's root and rev put everything back; a stale rev is refused.
        let undo = r["undo"].clone();
        let stale = json!({"tab_id": "w1:t1", "root": undo["root"], "expect_rev": undo["rev"].as_u64().unwrap() - 1});
        assert_eq!(h.fails("layout.arrange", stale).await["code"], "changed");
        let back = h.ok("layout.arrange", json!({"tab_id": "w1:t1", "root": undo["root"], "expect_rev": undo["rev"]})).await;
        assert_eq!(back["layout"]["root"], start);
        assert_eq!(h.ok("tab.list", json!({})).await["tabs"].as_array().unwrap().len(), 1);
        assert_eq!(ids(&h.ok("pane.list", json!({})).await), panes);
        for p in &panes {
            h.wait_alive(p).await;
        }
    });
    worktree_list => |h| Box::pin(async move {
        let repo = git_repo(h).await;
        let r = h.ok("worktree.list", json!({"workspace_id": "w2"})).await;
        assert_eq!(r["source"]["repo_name"], "repo");
        assert_eq!(r["source"]["source_workspace_id"], "w2");
        assert_eq!(r["worktrees"].as_array().unwrap().len(), 1);
        assert_eq!(r["worktrees"][0]["branch"], "main");
        assert_eq!(r["worktrees"][0]["is_linked_worktree"], false);
        assert_eq!(r["worktrees"][0]["open_workspace_id"], "w2");
        let by_cwd = h.ok("worktree.list", json!({"cwd": repo.to_string_lossy()})).await;
        assert_eq!(by_cwd["source"]["source_workspace_id"], "w2");
        assert_eq!(h.fails("worktree.list", json!({"cwd": "/"})).await["code"], "not_git_worktree");
        assert_eq!(h.fails("worktree.list", json!({"cwd": "relative/dir"})).await["code"], "invalid_request");
        assert_eq!(h.fails("worktree.list", json!({"workspace_id": "w2", "cwd": "/"})).await["code"], "invalid_request");
    });
    worktree_create => |h| Box::pin(async move {
        git_repo(h).await;
        let mut lines = subscribe(&h.sock, json!([{"type": "worktree.created"}])).await;
        let r = h.ok("worktree.create", json!({"workspace_id": "w2", "branch": "feat/Login_UI", "focus": true})).await;
        let want = h.dir.path().join("worktrees/repo/feat-login-ui");
        assert_eq!(r["worktree"]["path"], want.to_string_lossy().as_ref(), "<dir>/<repo>/<branch-slug>");
        assert_eq!(r["worktree"]["branch"], "feat/Login_UI");
        assert_eq!(r["worktree"]["is_linked_worktree"], true);
        assert_eq!(r["worktree"]["open_workspace_id"], "w3");
        assert_eq!(r["workspace"]["worktree"]["is_linked_worktree"], true);
        assert_eq!(r["workspace"]["worktree"]["repo_name"], "repo");
        assert!(want.join("README").is_file());
        h.wait_alive(r["root_pane"]["pane_id"].as_str().unwrap()).await;
        let ev = next_json(&mut lines).await;
        schema_check("event", &ev).unwrap_or_else(|e| panic!("{e}: {ev}"));
        assert_eq!(ev["event"], "worktree_created");
        // A generated branch when none is given.
        let g = h.ok("worktree.create", json!({"workspace_id": "w2"})).await;
        assert!(g["worktree"]["branch"].as_str().unwrap().starts_with("worktree/"));
        assert_eq!(h.fails("worktree.create", json!({"workspace_id": "w3", "branch": "x"})).await["code"], "linked_worktree_source");
        assert_eq!(h.fails("worktree.create", json!({"workspace_id": "w2", "branch": "  "})).await["code"], "invalid_request");
        assert_eq!(h.fails("worktree.create", json!({"workspace_id": "w2", "branch": "y", "path": "rel"})).await["code"], "invalid_request");
        assert_eq!(h.fails("worktree.create", json!({"workspace_id": "w2", "branch": "z", "base": "no-such-ref"})).await["code"], "worktree_create_failed");
        assert_eq!(h.fails("worktree.create", json!({"workspace_id": "w9"})).await["code"], "workspace_not_found");
    });
    worktree_open => |h| Box::pin(async move {
        git_repo(h).await;
        let c = h.ok("worktree.create", json!({"workspace_id": "w2", "branch": "feat"})).await;
        let path = c["worktree"]["path"].as_str().unwrap().to_string();
        let mut lines = subscribe(&h.sock, json!([{"type": "worktree.opened"}])).await;
        let r = h.ok("worktree.open", json!({"workspace_id": "w2", "branch": "feat"})).await;
        assert_eq!(r["already_open"], true);
        assert_eq!(r["workspace"]["workspace_id"], "w3");
        let ev = next_json(&mut lines).await;
        schema_check("event", &ev).unwrap_or_else(|e| panic!("{e}: {ev}"));
        assert_eq!(ev["data"]["already_open"], true);
        h.ok("workspace.close", json!({"workspace_id": "w3"})).await;
        let r = h.ok("worktree.open", json!({"workspace_id": "w2", "path": path, "label": "feat-ws"})).await;
        assert_eq!(r["already_open"], false);
        assert_eq!(r["workspace"]["label"], "feat-ws");
        assert_eq!(r["workspace"]["worktree"]["checkout_path"], path.as_str());
        assert_eq!(h.fails("worktree.open", json!({"workspace_id": "w2", "branch": "nope"})).await["code"], "worktree_not_found");
        assert_eq!(h.fails("worktree.open", json!({"workspace_id": "w2"})).await["code"], "invalid_request");
    });
    worktree_remove => |h| Box::pin(async move {
        let repo = git_repo(h).await;
        let c = h.ok("worktree.create", json!({"workspace_id": "w2", "branch": "gone"})).await;
        let ws = c["workspace"]["workspace_id"].as_str().unwrap().to_string();
        let path = std::path::PathBuf::from(c["worktree"]["path"].as_str().unwrap());
        assert_eq!(h.fails("worktree.remove", json!({"workspace_id": "w2"})).await["code"], "not_linked_worktree");
        std::fs::write(path.join("scratch.txt"), "x").unwrap();
        assert_eq!(h.fails("worktree.remove", json!({"workspace_id": ws})).await["code"], "dirty_worktree_requires_force");
        assert!(path.is_dir(), "a refused remove keeps the checkout");
        let mut lines = subscribe(&h.sock, json!([{"type": "worktree.removed"}])).await;
        let r = h.ok("worktree.remove", json!({"workspace_id": ws, "force": true})).await;
        assert_eq!(r["forced"], true);
        assert_eq!(r["path"], path.to_string_lossy().as_ref());
        assert!(!path.exists());
        let ev = next_json(&mut lines).await;
        schema_check("event", &ev).unwrap_or_else(|e| panic!("{e}: {ev}"));
        assert_eq!(ev["data"]["workspace_id"], ws.as_str());
        assert_eq!(h.fails("workspace.get", json!({"workspace_id": ws})).await["code"], "workspace_not_found");
        let branches = std::process::Command::new("git").args(["branch", "--list", "gone"]).current_dir(&repo).output().unwrap();
        assert!(String::from_utf8_lossy(&branches.stdout).contains("gone"), "the branch is kept");
        assert_eq!(h.fails("worktree.remove", json!({"workspace_id": "w9"})).await["code"], "workspace_not_found");
    });
    pane_focus => |h| Box::pin(async move {
        h.ok("pane.split", json!({"target_pane_id": "w1:p1", "direction": "right", "focus": true})).await;
        assert_eq!(h.ok("pane.get", json!({"pane_id": "w1:p2"})).await["pane"]["focused"], true);
        let mut lines = subscribe(&h.sock, json!([{"type": "pane.focused"}])).await;
        let r = h.ok("pane.focus", json!({"pane_id": "w1:p1"})).await;
        assert_eq!(r["pane"]["pane_id"], "w1:p1");
        assert_eq!(r["pane"]["focused"], true);
        let ev = next_json(&mut lines).await;
        schema_check("event", &ev).unwrap_or_else(|e| panic!("{e}: {ev}"));
        assert_eq!((ev["event"].as_str(), ev["data"]["pane_id"].as_str()), (Some("pane_focused"), Some("w1:p1")));
        assert_eq!(h.ok("pane.get", json!({"pane_id": "w1:p2"})).await["pane"]["focused"], false);
        // Across workspaces: the workspace and tab follow.
        h.ok("workspace.create", json!({})).await;
        h.ok("pane.focus", json!({"pane_id": "w2:p1"})).await;
        assert_eq!(h.ok("workspace.get", json!({"workspace_id": "w2"})).await["workspace"]["focused"], true);
        assert_eq!(h.fails("pane.focus", json!({"pane_id": "w1:p9"})).await["code"], "pane_not_found");
    });
    pane_focus_direction => |h| Box::pin(async move {
        h.ok("pane.split", json!({"target_pane_id": "w1:p1", "direction": "right", "focus": true})).await;
        h.ok("pane.split", json!({"target_pane_id": "w1:p2", "direction": "down", "focus": true})).await;
        // w1:p1 | (w1:p2 / w1:p3), focus on w1:p3.
        let r = h.ok("pane.focus_direction", json!({"direction": "up"})).await;
        assert_eq!(r["focus"]["changed"], true);
        assert_eq!(r["focus"]["source_pane_id"], "w1:p3");
        assert_eq!(r["focus"]["focused_pane_id"], "w1:p2");
        let r = h.ok("pane.focus_direction", json!({"pane_id": "w1:p2", "direction": "left"})).await;
        assert_eq!(r["focus"]["focused_pane_id"], "w1:p1");
        let r = h.ok("pane.focus_direction", json!({"direction": "left"})).await;
        assert_eq!(r["focus"]["changed"], false);
        assert_eq!(r["focus"]["reason"], "no_neighbor");
        assert_eq!(r["focus"]["focused_pane_id"], "w1:p1");
        assert_eq!(h.fails("pane.focus_direction", json!({"direction": "sideways"})).await["code"], "invalid_params");
    });
    pane_read_is_capped => |h| Box::pin(async move {
        // A wide pane with ~800 KB of history: the reply keeps the newest
        // part within READ_MAX_BYTES of JSON and says truncated (audit P2-6).
        // pane.resize's PTY-size form ({cols, rows}).
        assert_eq!(h.ok("pane.resize", json!({"pane_id": "w1:p1", "cols": 1000, "rows": 40})).await["type"], "ok");
        h.ok("pane.send_text", json!({"pane_id": "w1:p1", "text": "yes \"$(printf '%0999d' 7)\" | head -n 800; echo DONE-$((40+2))\n"})).await;
        h.wait_text("w1:p1", "DONE-42").await;
        let r = h.ok("pane.read", json!({"pane_id": "w1:p1", "source": "recent", "lines": 100000})).await;
        let text = r["read"]["text"].as_str().unwrap();
        assert_eq!(r["read"]["truncated"], true);
        let encoded = serde_json::to_string(text).unwrap().len();
        assert!(encoded <= tmuxedo_runner::api::READ_MAX_BYTES + 2, "{encoded}");
        assert!(encoded > tmuxedo_runner::api::READ_MAX_BYTES / 2, "kept a useful amount: {encoded}");
        assert!(text.contains("DONE-42"), "the newest part is kept");
        assert!(text.lines().next().unwrap().len() >= 999 || text.starts_with('$'), "cut at a line start");
        // Escapes count: ESC is 6 bytes in JSON.
        let (t, cut) = tmuxedo_runner::api::cap_text("\x1b[0m\n".repeat(100_000), 64 * 1024);
        assert!(cut && serde_json::to_string(&t).unwrap().len() <= 64 * 1024 + 2);
        assert_eq!(tmuxedo_runner::api::cap_text("short".into(), 10), ("short".to_string(), false));
    });
    pane_env_vars => |h| Box::pin(async move {
        // Every pane gets tmuxedo's environment, plus the deprecated AIMUX_* copies (docs/protocol.md "Methods").
        h.ok("pane.split", json!({"target_pane_id": "w1:p1", "direction": "down"})).await;
        h.wait_alive("w1:p2").await;
        let sock = h.sock.to_string_lossy().to_string();
        for (pane, tab) in [("w1:p1", "w1:t1"), ("w1:p2", "w1:t1")] {
            h.ok("pane.send_text", json!({"pane_id": pane, "text": "env | grep -E '^[A-Z]+_(PANE_ID|WORKSPACE_ID|TAB_ID|SOCKET_PATH)=' | sort; echo ENV-$((1+1))-END\n"})).await;
            h.wait_text(pane, "ENV-2-END").await;
            let r = h.ok("pane.read", json!({"pane_id": pane, "source": "recent_unwrapped", "lines": 60})).await;
            let text = r["read"]["text"].as_str().unwrap().to_string();
            // The shell prompt can land in front of the first output line (typed-ahead input).
            let lines: Vec<&str> = text.lines().map(|l| l.trim_end().trim_start_matches("$ ")).collect();
            for want in [
                format!("TMUXEDO_PANE_ID={pane}"),
                "TMUXEDO_WORKSPACE_ID=w1".to_string(),
                format!("TMUXEDO_TAB_ID={tab}"),
                format!("TMUXEDO_SOCKET_PATH={sock}"),
                format!("AIMUX_PANE_ID={pane}"),
                "AIMUX_WORKSPACE_ID=w1".to_string(),
                format!("AIMUX_TAB_ID={tab}"),
                format!("AIMUX_SOCKET_PATH={sock}"),
            ] {
                assert!(lines.contains(&want.as_str()), "{pane}: {want} missing in {text}");
            }
            // Only tmuxedo's names (and the deprecated AIMUX_* aliases) carry pane ids.
            for l in lines.iter().filter(|l| l.contains('=') && !l.contains(" ")) {
                assert!(l.starts_with("TMUXEDO_") || l.starts_with("AIMUX_"), "{pane}: unexpected {l} in {text}");
            }
        }
    });
    // ── plugins (runner API plugin.*, D-13) ──
    plugin_list => |h| Box::pin(async move {
        let r = h.ok("plugin.list", json!({})).await;
        assert_eq!(r["type"], "plugin_list");
        let claude = &r["plugins"][0];
        assert_eq!(
            (claude["plugin_id"].as_str(), claude["enabled"].as_bool(), claude["plugin_root"].as_str()),
            (Some("claude"), Some(true), Some("builtin:claude"))
        );
        assert_eq!(claude["actions"][0]["id"], "login");
        assert_eq!(claude["warnings"], json!([]));
        let one = h.ok("plugin.list", json!({"plugin_id": "claude"})).await;
        assert_eq!(one["plugins"].as_array().unwrap().len(), 1);
        assert_eq!(h.fails("plugin.list", json!({"plugin_id": "nope"})).await["code"], "plugin_not_found");
    });
    plugin_enable => |h| Box::pin(async move {
        let shim = h.dir.path().join("state/shim/claude");
        h.ok("plugin.disable", json!({"plugin_id": "claude"})).await;
        assert!(!shim.exists());
        let r = h.ok("plugin.enable", json!({"plugin_id": "claude"})).await;
        assert_eq!((r["type"].as_str(), r["plugin"]["enabled"].as_bool()), (Some("plugin_enabled"), Some(true)));
        assert!(shim.is_file(), "enabling writes the shim back");
        assert_eq!(h.fails("plugin.enable", json!({"plugin_id": "nope"})).await["code"], "plugin_not_found");
        assert_eq!(h.fails("plugin.enable", json!({})).await["code"], "invalid_params");
    });
    plugin_disable => |h| Box::pin(async move {
        let r = h.ok("plugin.disable", json!({"plugin_id": "claude"})).await;
        assert_eq!((r["type"].as_str(), r["plugin"]["enabled"].as_bool()), (Some("plugin_disabled"), Some(false)));
        assert!(!h.dir.path().join("state/shim/claude").exists(), "claude runs unwrapped");
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(h.dir.path().join("state/plugins.json")).unwrap()).unwrap();
        assert_eq!(saved["disabled"], json!(["claude"]));
        // Its hooks are ignored: no status, `{}` at once even for a hold.
        let r = h
            .raw("hook.event", json!({"paneId": "w1:p1", "plugin": "claude",
                "event": {"hook_event_name": "PermissionRequest", "session_id": "s", "tool_name": "Bash"}}))
            .await;
        assert_eq!(r["result"], json!({}));
        let a = h.ok("agent.list", json!({})).await;
        assert_eq!(a["agents"], json!([]), "{a}");
        assert_eq!(
            h.fails("plugin.action.invoke", json!({"plugin_id": "claude", "action_id": "login"})).await["code"],
            "plugin_disabled"
        );
    });
    plugin_action_list => |h| Box::pin(async move {
        let r = h.ok("plugin.action.list", json!({})).await;
        assert_eq!(r["type"], "plugin_action_list");
        let a = &r["actions"][0];
        assert_eq!(
            (a["plugin_id"].as_str(), a["action_id"].as_str(), a["title"].as_str()),
            (Some("claude"), Some("login"), Some("Sign in to Claude"))
        );
        assert_eq!(a["command"], json!(["auth.login"]));
        let none = h.ok("plugin.action.list", json!({"plugin_id": "claude"})).await;
        assert_eq!(none["actions"].as_array().unwrap().len(), 1);
    });
    plugin_action_invoke => |h| Box::pin(async move {
        // `login`: a new tab running the shim's `claude /login`, then a shell.
        let real = h.dir.path().join("real-bin");
        std::fs::create_dir_all(&real).unwrap();
        let r = h
            .ok("plugin.action.invoke", json!({"plugin_id": "claude", "action_id": "login",
                "context": {"workspace_id": "w1", "invocation_source": "test"}}))
            .await;
        assert_eq!(r["type"], "plugin_action_invoked");
        assert_eq!(r["action"]["action_id"], "login");
        assert_eq!(r["context"]["invocation_source"], "test");
        assert_eq!(r["log"]["status"], "succeeded", "{r}");
        let shim = h.dir.path().join("state/shim/claude");
        assert_eq!(r["log"]["command"], json!(["auth.login", shim.to_string_lossy(), "/login"]));
        assert_eq!(r["log"]["stdout"], "opened w1:p2");
        h.wait_alive("w1:p2").await;
        let tab = h.ok("tab.get", json!({"tab_id": "w1:t2"})).await;
        assert_eq!(tab["tab"]["label"], "Sign in to Claude");
        // No claude on this PATH: the shim says so, and a shell is left behind.
        h.wait_text("w1:p2", "claude is not installed").await;
        assert_eq!(
            h.fails("plugin.action.invoke", json!({"action_id": "nope"})).await["code"],
            "action_not_found"
        );
    });
    needs_list => |h| Box::pin(async move {
        // Nothing needs anyone in a fresh shell pane.
        let r = h.ok("needs.list", json!({})).await;
        assert_eq!(r["type"], "needs_list");
        assert_eq!(r["items"], json!([]));
    });
    needs_respond => |h| Box::pin(async move {
        // Answers come from people through tmuxedo, never from the local socket
        // (anything in a pane can reach it); over the gateway: tests/needs.rs.
        let e = h.fails("needs.respond", json!({"itemId": "p-1", "responseId": "approve"})).await;
        assert_eq!(e["code"], "not_supported");
    });
    plugin_log_list => |h| Box::pin(async move {
        h.ok("plugin.disable", json!({"plugin_id": "claude"})).await;
        h.ok("plugin.enable", json!({"plugin_id": "claude"})).await;
        let r = h.ok("plugin.log.list", json!({"plugin_id": "claude", "limit": 1})).await;
        assert_eq!(r["type"], "plugin_log_list");
        assert_eq!(r["logs"].as_array().unwrap().len(), 1);
        assert_eq!(r["logs"][0]["event"], "plugin.enabled", "newest first");
        let all = h.ok("plugin.log.list", json!({})).await;
        assert_eq!(all["logs"].as_array().unwrap().len(), 2);
    });
    // Agent chat: the happy paths (with a scripted agent) are in tests/chat.rs,
    // schema-checked there too; here, the shapes with no session and no API key.
    chat_start => |h| Box::pin(async move {
        // No API key and web has not allowed subscription chat: refused, nothing created.
        assert_eq!(h.fails("chat.start", json!({})).await["code"], "chat_login_not_allowed");
        assert_eq!(h.fails("chat.start", json!({"mode": "yolo"})).await["code"], "invalid_mode");
        assert_eq!(h.fails("chat.start", json!({"agent": "nope"})).await["code"], "unknown_agent_kind");
    });
    chat_list => |h| Box::pin(async move {
        let r = h.ok("chat.list", json!({})).await;
        assert_eq!(r["type"], "chat_list");
        assert_eq!(r["sessions"], json!([]));
    });
    chat_get => |h| Box::pin(async move {
        assert_eq!(h.fails("chat.get", json!({"session_id": "c-00000000"})).await["code"], "chat_not_found");
        assert_eq!(h.fails("chat.get", json!({})).await["code"], "invalid_params");
    });
    chat_read => |h| Box::pin(async move {
        assert_eq!(h.fails("chat.read", json!({"session_id": "c-00000000", "from_seq": 1})).await["code"], "chat_not_found");
    });
    chat_send => |h| Box::pin(async move {
        assert_eq!(h.fails("chat.send", json!({"session_id": "c-00000000", "text": "hi"})).await["code"], "chat_not_found");
        assert_eq!(h.fails("chat.send", json!({"session_id": "c-00000000"})).await["code"], "invalid_params");
    });
    chat_interrupt => |h| Box::pin(async move {
        assert_eq!(h.fails("chat.interrupt", json!({"session_id": "c-00000000"})).await["code"], "chat_not_found");
    });
    chat_mode => |h| Box::pin(async move {
        assert_eq!(h.fails("chat.mode", json!({"session_id": "c-00000000", "mode": "plan_first"})).await["code"], "chat_not_found");
    });
    chat_respond => |h| Box::pin(async move {
        let p = json!({"session_id": "c-00000000", "need_id": "n1", "response": {"kind": "deny"}});
        assert_eq!(h.fails("chat.respond", p).await["code"], "chat_not_found");
    });
    chat_handoff => |h| Box::pin(async move {
        assert_eq!(h.fails("chat.handoff", json!({"to": "sideways"})).await["code"], "invalid_params");
        assert_eq!(h.fails("chat.handoff", json!({"to": "terminal", "session_id": "c-00000000"})).await["code"], "chat_not_found");
        // A pane with no agent session cannot become a chat.
        assert_eq!(h.fails("chat.handoff", json!({"to": "chat", "pane_id": "w1:p1"})).await["code"], "no_agent_session");
    });
    chat_close => |h| Box::pin(async move {
        assert_eq!(h.fails("chat.close", json!({"session_id": "c-00000000"})).await["code"], "chat_not_found");
    });
    chat_file => |h| Box::pin(async move {
        assert_eq!(h.fails("chat.file", json!({"session_id": "c-00000000", "path": "a.md"})).await["code"], "chat_not_found");
        assert_eq!(h.fails("chat.file", json!({"session_id": "c-00000000"})).await["code"], "invalid_params");
        // Auto-approve without web's policy and a fresh step-up: refused, nothing started.
        assert_eq!(h.fails("chat.start", json!({"mode": "auto_approve"})).await["code"], "auto_approve_forbidden");
    });
    files_list => |h| Box::pin(async move {
        let r = h.ok("files.list", json!({"project": "p1"})).await;
        assert_eq!(r["files"], json!([]));
        let root = h.dir.path().join("files").canonicalize().unwrap();
        std::fs::create_dir_all(root.join("p1/inbox")).unwrap();
        std::fs::write(root.join("p1/inbox/a.txt"), "a").unwrap();
        let r = h.ok("files.list", json!({"project": "p1", "dir": "inbox"})).await;
        assert_eq!(r["files"][0]["name"], "a.txt");
        // Locally, the caller's folder names the project.
        let r = h.ok("files.list", json!({"cwd": h.dir.path().display().to_string()})).await;
        assert!(r["project"].as_str().unwrap().starts_with("path_"), "{r}");
        assert_eq!(h.fails("files.list", json!({})).await["code"], "invalid_params");
    });
    files_stat => |h| Box::pin(async move {
        let root = h.dir.path().join("files").canonicalize().unwrap();
        std::fs::create_dir_all(root.join("p1/outbox")).unwrap();
        std::fs::write(root.join("p1/outbox/r.pdf"), "pdf").unwrap();
        let r = h.ok("files.stat", json!({"project": "p1", "dir": "outbox", "name": "r.pdf"})).await;
        assert_eq!(r["size"], 3);
        assert_eq!(h.fails("files.stat", json!({"project": "p1", "dir": "outbox", "name": "../x"})).await["code"], "invalid_params");
    });
    files_delete => |h| Box::pin(async move {
        let root = h.dir.path().join("files").canonicalize().unwrap();
        std::fs::create_dir_all(root.join("p1/inbox")).unwrap();
        std::fs::write(root.join("p1/inbox/x.txt"), "x").unwrap();
        h.ok("files.delete", json!({"project": "p1", "dir": "inbox", "name": "x.txt"})).await;
        assert!(!root.join("p1/inbox/x.txt").exists());
        assert_eq!(h.fails("files.delete", json!({"project": "p1", "dir": "inbox", "name": "x.txt"})).await["code"], "file_not_found");
    });
    files_pin => |h| Box::pin(async move {
        let root = h.dir.path().join("files").canonicalize().unwrap();
        std::fs::create_dir_all(root.join("p1/inbox")).unwrap();
        std::fs::write(root.join("p1/inbox/k.txt"), "k").unwrap();
        let r = h.ok("files.pin", json!({"project": "p1", "dir": "inbox", "name": "k.txt", "pinned": true})).await;
        assert_eq!(r["pinned"], true);
    });
    files_quota => |h| Box::pin(async move {
        let r = h.ok("files.quota", json!({})).await;
        assert!(r["quotaBytes"].as_u64().unwrap() > 0);
    });
    files_share => |h| Box::pin(async move {
        let work = h.dir.path().join("work-share");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(work.join("out.md"), "# done").unwrap();
        let r = h
            .ok("files.share", json!({"path": work.join("out.md").display().to_string(), "cwd": work.display().to_string(), "note": "here"}))
            .await;
        assert_eq!(r["name"], "out.md");
        assert_eq!(
            h.fails("files.share", json!({"path": "/etc/hostname", "cwd": work.display().to_string()})).await["code"],
            "file_refused"
        );
        // The jail: no link is followed (even one that stays inside), no hard link, no secret, no `..`.
        let share = |name: &str| json!({"path": work.join(name).display().to_string(), "cwd": work.display().to_string()});
        std::fs::create_dir_all(work.join("sub")).unwrap();
        std::fs::write(work.join("sub/deep.md"), "# deep").unwrap();
        std::fs::write(work.join(".env"), "KEY=1").unwrap();
        std::fs::write(h.dir.path().join("elsewhere.txt"), "not yours").unwrap();
        std::os::unix::fs::symlink(h.dir.path().join("elsewhere.txt"), work.join("link.txt")).unwrap();
        std::os::unix::fs::symlink(work.join("sub"), work.join("alias")).unwrap();
        std::fs::hard_link(h.dir.path().join("elsewhere.txt"), work.join("hard.txt")).unwrap();
        assert_eq!(h.ok("files.share", share("sub/deep.md")).await["name"], "deep.md");
        for (name, code) in [
            ("link.txt", "file_refused"),
            ("alias/deep.md", "file_refused"),
            ("hard.txt", "file_refused"),
            (".env", "file_refused"),
            (".ENV", "file_refused"),
            ("sub", "file_refused"),
            ("nope.md", "file_not_found"),
            ("sub/../out.md", "invalid_params"),
        ] {
            assert_eq!(h.fails("files.share", share(name)).await["code"], code, "{name}");
        }
    });
    files_request => |h| Box::pin(async move {
        let r = h.ok("files.request", json!({"what": "the logo as SVG", "cwd": h.dir.path().display().to_string()})).await;
        assert!(r["inbox"].as_str().unwrap().ends_with("/inbox"));
    });
    files_upload_init => |h| Box::pin(async move {
        // Web's methods, through the gateway only.
        assert_eq!(h.fails("files.upload_init", json!({"upload_id": "up_conform1", "name": "a", "size": 1, "chunk_size": 262144})).await["code"], "not_supported");
    });
    files_upload_status => |h| Box::pin(async move {
        assert_eq!(h.fails("files.upload_status", json!({"upload_id": "up_conform1"})).await["code"], "not_supported");
    });
    files_upload_complete => |h| Box::pin(async move {
        assert_eq!(h.fails("files.upload_complete", json!({"upload_id": "up_conform1"})).await["code"], "not_supported");
    });
    files_upload_abort => |h| Box::pin(async move {
        assert_eq!(h.fails("files.upload_abort", json!({"upload_id": "up_conform1"})).await["code"], "not_supported");
    });
    files_read => |h| Box::pin(async move {
        assert_eq!(h.fails("files.read", json!({"xfer": "dl_conform", "name": "a"})).await["code"], "not_supported");
    });
    // The file browser (docs/design/file-browser.md §3.1, T20): never on the local socket, which
    // anything in a pane can reach. Each method is refused before its parameters are even read.
    fs_roots => |h| Box::pin(async move {
        assert_eq!(h.fails("fs.roots", json!({})).await["code"], "not_supported");
    });
    fs_list => |h| Box::pin(async move {
        assert_eq!(h.fails("fs.list", json!({"root": "r0123456789abcdef", "path": ""})).await["code"], "not_supported");
    });
    fs_stat => |h| Box::pin(async move {
        assert_eq!(h.fails("fs.stat", json!({"root": "r0123456789abcdef", "path": "README.md"})).await["code"], "not_supported");
    });
    fs_download => |h| Box::pin(async move {
        // The gateway's download method: refused on the local socket like every fs name.
        let p = json!({"root": "r0123456789abcdef", "path": "README.md", "xfer": "dl_0123456789ab"});
        assert_eq!(h.fails("fs.download", p).await["code"], "not_supported");
    });
    fs_read => |h| Box::pin(async move {
        assert_eq!(h.fails("fs.read", json!({"root": "r0123456789abcdef", "path": "README.md"})).await["code"], "not_supported");
    });
    fs_search => |h| Box::pin(async move {
        assert_eq!(h.fails("fs.search", json!({"root": "r0123456789abcdef", "query": "readme"})).await["code"], "not_supported");
        // The methods that change files are refused the same way, and so is any other fs name.
        for m in tmuxedo_protocol::fs::MUTATING_METHODS.iter().chain(&["fs.anything"]) {
            let r = h.raw(m, json!({"root": "r0123456789abcdef", "path": "x", "text": "y"})).await;
            assert_eq!(r["error"]["code"], "not_supported", "{m}: {r}");
        }
    });
    // The changing methods (slice A2): never on the local socket either, even with every flag web
    // would set (an agent in a pane must not write through the browser's path).
    fs_write => |h| Box::pin(async move {
        let p = json!({"root": "r0123456789abcdef", "path": "a.md", "text": "x", "create": true, "op_id": "o1", "control": true, "stepup_at": 1});
        assert_eq!(h.fails("fs.write", p).await["code"], "not_supported");
    });
    fs_mkdir => |h| Box::pin(async move {
        let p = json!({"root": "r0123456789abcdef", "path": "d", "op_id": "o1", "control": true, "stepup_at": 1});
        assert_eq!(h.fails("fs.mkdir", p).await["code"], "not_supported");
    });
    fs_rename => |h| Box::pin(async move {
        let p = json!({"root": "r0123456789abcdef", "from": "a", "to": "b", "op_id": "o1", "control": true, "stepup_at": 1});
        assert_eq!(h.fails("fs.rename", p).await["code"], "not_supported");
    });
    fs_delete => |h| Box::pin(async move {
        let p = json!({"root": "r0123456789abcdef", "path": "d", "op_id": "o1", "control": true, "stepup_at": 1});
        assert_eq!(h.fails("fs.delete", p).await["code"], "not_supported");
    });
    fs_undo => |h| Box::pin(async move {
        let p = json!({"undoId": "u0123456789abcdef", "op_id": "o1", "control": true, "stepup_at": 1});
        assert_eq!(h.fails("fs.undo", p).await["code"], "not_supported");
    });
    // Review and ship (docs/design/away-and-review.md): never on the local socket either.
    git_status => |h| Box::pin(async move {
        assert_eq!(h.fails("git.status", json!({"pane": "w1:p1"})).await["code"], "not_supported");
    });
    git_diff => |h| Box::pin(async move {
        assert_eq!(h.fails("git.diff", json!({"pane": "w1:p1", "path": "README.md"})).await["code"], "not_supported");
        for m in tmuxedo_protocol::review::MUTATING_METHODS.iter().chain(&["git.anything"]) {
            let r = h.raw(m, json!({"pane": "w1:p1", "message": "x"})).await;
            assert_eq!(r["error"]["code"], "not_supported", "{m}: {r}");
        }
    });
    // Checkpoints (Phase 23): never on the local socket; the agent cannot undo its own turn.
    checkpoint_list => |h| Box::pin(async move {
        assert_eq!(h.fails("checkpoint.list", json!({"pane": "w1:p1"})).await["code"], "not_supported");
    });
    checkpoint_preview => |h| Box::pin(async move {
        assert_eq!(h.fails("checkpoint.preview", json!({"pane": "w1:p1", "id": 1})).await["code"], "not_supported");
    });
    checkpoint_restore => |h| Box::pin(async move {
        assert_eq!(h.fails("checkpoint.restore", json!({"pane": "w1:p1", "id": 1})).await["code"], "not_supported");
    });
    checkpoint_create => |h| Box::pin(async move {
        assert_eq!(h.fails("checkpoint.create", json!({"chat": "c1"})).await["code"], "not_supported");
    });
    queued_deliver => |h| Box::pin(async move {
        // Delivery itself (sealed to the runner key) is in tests/chat.rs; here the
        // refusals: missing fields, and a prompt that does not open on this machine.
        assert_eq!(h.fails("queued.deliver", json!({"sealed": "x"})).await["code"], "invalid_params");
        assert_eq!(h.fails("queued.deliver", json!({"id": "q_AAAAAAAAAAAAAAAAAAAAAAAA"})).await["code"], "invalid_params");
        let r = h.fails("queued.deliver", json!({"id": "q_AAAAAAAAAAAAAAAAAAAAAAAA", "sealed": "bm90LXNlYWxlZA"})).await;
        assert_eq!(r["code"], "queued_unreadable", "{r}");
    });
    unsupported_and_unknown => |h| Box::pin(async move {
        // Methods the runner does not have, whatever their name.
        assert_eq!(h.fails("plugin.link", json!({"path": "/x"})).await["code"], "unknown_method");
        assert_eq!(h.fails("server.stop", json!({})).await["code"], "unknown_method");
        assert_eq!(h.fails("no.such", json!({})).await["code"], "unknown_method");
    });
    task_start => |h| Box::pin(async move {
        // The machine's owner has not turned automation on: refused, whatever the server says.
        let p = json!({"task_id": "t1", "kind": "command", "labels": ["ci"], "cwd": "/", "roots": ["/"],
            "argv": ["/bin/sh", "-c", "true"], "allowed_commands": ["/bin/sh"]});
        assert_eq!(h.fails("task.start", p.clone()).await["code"], "automation_disabled");
        let h = H::start_with(|o| o.automation_labels = vec!["ci".into()]).await;
        let root = h.dir.path().to_string_lossy().into_owned();
        let mut p = p;
        p["cwd"] = json!(root);
        p["roots"] = json!([root]);
        p["argv"] = json!(["/bin/sh", "-c", "echo task-out; exit 3"]);
        let r = h.ok("task.start", p.clone()).await;
        assert_eq!(r["type"], "task_info");
        assert_eq!(r["task"]["status"], "running");
        assert_eq!(h.fails("task.start", p).await["code"], "task_exists");
    });
    task_get => |h| Box::pin(async move {
        assert_eq!(h.fails("task.get", json!({"task_id": "nope"})).await["code"], "task_not_found");
    });
    task_list => |h| Box::pin(async move {
        let r = h.ok("task.list", json!({})).await;
        assert_eq!(r["type"], "task_list");
        assert_eq!(r["tasks"], json!([]));
    });
    task_cancel => |h| Box::pin(async move {
        assert_eq!(h.fails("task.cancel", json!({"task_id": "nope"})).await["code"], "task_not_found");
    });
    every_schema_method_is_implemented_and_tested => |h| Box::pin(async move {
        // The schema lists exactly the methods the runner dispatches, each has its own
        // schema-validated test above, and none answers unknown_method.
        let methods = common::schema_methods();
        assert!(methods.len() > 55, "only {} methods", methods.len());
        let mut constants = common::schema_method_constants();
        constants.sort();
        assert_eq!(constants, methods, "every method has a request schema");
        for m in &methods {
            let test = format!("\n    {} => |h|", m.replace('.', "_"));
            assert!(include_str!("conformance.rs").contains(&test), "{m} has no conformance test");
            let r = h.raw(m, json!({})).await;
            assert_ne!(r["error"]["code"], "unknown_method", "{m}: {r}");
        }
    });
}

#[test]
fn the_validator_actually_rejects_bad_shapes() {
    assert!(schema_check("success_response", &json!({"id": "x", "result": {"type": "pane_info"}})).is_err());
    assert!(schema_check(
        "success_response",
        &json!({"id": "x", "result": {"type": "pane_list", "panes": [{"pane_id": "p"}]}})
    )
    .is_err());
    assert!(schema_check("request", &json!({"id": "x", "method": "pane.split", "params": {}})).is_err());
    assert!(schema_check(
        "request",
        &json!({"id": "x", "method": "pane.split", "params": {"direction": "left"}})
    )
    .is_err());
    assert!(schema_check(
        "event",
        &json!({"event": "pane_closed", "data": {"type": "pane_closed"}})
    )
    .is_err());
    assert!(schema_check("error_response", &json!({"id": "x", "error": {"code": "c"}})).is_err());
    assert!(schema_check("success_response", &json!({"id": "x", "result": {"type": "ok"}})).is_ok());
}
