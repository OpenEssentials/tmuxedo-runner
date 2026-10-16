//! PLAN Phase 18, first slice: helpers (the sub-agents an agent runs in its
//! own process) through a real daemon and holder with a fake gateway. The
//! test plays the agent's hooks on the runner socket, as tests/events.rs does.
//!
//! - A helper starting and finishing shows in `Agent.children` on `status`
//!   frames and as durable `subagent_started` / `subagent_completed` events;
//!   the pane's own status does not move.
//! - A helper that never reports its end stays until the session ends.
//! - The list is bounded.
//! - With `config.roles`: `agent.start {role}` adds the role's launch
//!   arguments (or refuses an unknown role), and a helper whose name is a
//!   role's carries `roleId`.

mod common;

use common::{options, start_holder, Conn, FakeGateway, H};
use serde_json::{json, Value};
use std::time::Duration;
use tmuxedo_protocol::subagents::MAX_AGENT_CHILDREN;
use tmuxedo_runner::daemon;

const S: Duration = Duration::from_secs(20);

struct Rig {
    state: std::path::PathBuf,
    _dir: tempfile::TempDir,
    c: Conn,
    h: H,
    task: tokio::task::JoinHandle<Result<daemon::ExitReason, String>>,
}

async fn rig() -> Rig {
    let _dir = common::tempdir();
    let holder = start_holder(_dir.path());
    let gw = FakeGateway::start().await;
    let o = options(_dir.path(), &gw.url, holder);
    let state = o.state_dir.clone();
    let sock = o.runner_socket.clone();
    let task = tokio::spawn(daemon::run(o));
    let mut c = gw.accept(S).await.unwrap();
    c.hello(S).await;
    c.text_t(S, "topology").await;
    let h = H::attach_to(&sock).await;
    Rig {
        state,
        _dir,
        c,
        h,
        task,
    }
}

impl Rig {
    async fn hook(&self, name: &str, extra: Value) {
        let mut ev = json!({"hook_event_name": name, "session_id": "s-1"});
        for (k, v) in extra.as_object().unwrap() {
            ev[k] = v.clone();
        }
        let res = self.h.raw("hook.event", json!({"paneId": "w1:p1", "event": ev})).await;
        assert_eq!(res["result"], json!({}), "{name}");
    }

    /// The next status frame whose helper list satisfies `pred`.
    async fn children(&mut self, pred: impl Fn(&[Value]) -> bool) -> Value {
        let none = vec![];
        let kids = |v: &Value| pred(v["agent"]["children"].as_array().unwrap_or(&none));
        self.c.text(S, |v| v["t"] == "status" && kids(v)).await
    }

    async fn status(&mut self, s: &str) -> Value {
        let s = s.to_string();
        self.c
            .text(S, move |v| v["t"] == "status" && v["agent"]["status"] == s.as_str())
            .await
    }

    async fn event(&mut self, t: &str) -> Value {
        let t = t.to_string();
        self.c
            .text(S, move |v| v["t"] == "event" && v["type"] == t.as_str())
            .await
    }
}

fn helper(id: &str, kind: &str) -> Value {
    json!({"agent_id": id, "agent_type": kind})
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn helpers_show_on_the_agent_and_as_durable_events() {
    let mut r = rig().await;
    r.hook("UserPromptSubmit", json!({"prompt": "x"})).await;
    let st = r.status("working").await;
    assert!(st["agent"].get("children").is_none(), "no helpers, no field: {st}");

    // A helper starts: on the agent, and a durable event.
    r.hook("SubagentStart", helper("a1", "Explore")).await;
    let ev = r.event("subagent_started").await;
    assert_eq!(ev["paneId"], "w1:p1");
    let a1 = json!({"kind": "claude", "childId": "a1", "name": "Explore"});
    assert_eq!(ev["data"], a1);
    let st = r.children(|c| c.len() == 1).await;
    let child = &st["agent"]["children"][0];
    assert_eq!(child["id"], "a1");
    assert_eq!(child["name"], "Explore");
    assert_eq!(child["status"], "working");
    assert_eq!(child["since"], child["startedAt"]);
    // The pane's own status is still the root agent's.
    assert_eq!(st["agent"]["kind"], "claude");
    assert_eq!(st["agent"]["status"], "working");

    // The same start again changes nothing; a second helper joins.
    r.hook("SubagentStart", helper("a1", "Explore")).await;
    r.hook("SubagentStart", helper("a2", "tester")).await;
    let ev = r.event("subagent_started").await;
    assert_eq!(ev["data"]["childId"], "a2", "the repeated start raised no event");
    r.children(|c| c.len() == 2).await;

    // The first finishes: done on the list, the other still at work.
    r.hook("SubagentStop", helper("a1", "Explore")).await;
    let ev = r.event("subagent_completed").await;
    assert_eq!(ev["data"], a1);
    let st = r.children(|c| c.len() == 2 && c[0]["status"] == "done").await;
    assert_eq!(st["agent"]["children"][1]["status"], "working");
    assert_eq!(st["agent"]["children"][0]["startedAt"], child["startedAt"]);
    // A stop nobody saw start says nothing.
    r.hook("SubagentStop", helper("zz", "ghost")).await;

    // The turn ends with a helper that never stopped: it stays on the agent.
    r.hook("Stop", json!({})).await;
    let st = r.status("done").await;
    assert_eq!(st["agent"]["children"][1]["id"], "a2");
    assert_eq!(st["agent"]["children"][1]["status"], "working");

    // The session ends: the list ends with it.
    r.hook("SessionEnd", json!({})).await;
    let st = r.children(|c| c.is_empty()).await;
    assert!(st["agent"].get("children").is_none(), "{st}");
    let ghost = r.c.texts.iter().any(|v| v["type"] == "subagent_completed");
    assert!(!ghost, "the ghost stop raised no event");
    r.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_helper_list_is_bounded() {
    let mut r = rig().await;
    r.hook("UserPromptSubmit", json!({"prompt": "x"})).await;
    let over = MAX_AGENT_CHILDREN + 4;
    for i in 0..over {
        r.hook("SubagentStart", helper(&format!("k{i}"), "x")).await;
    }
    let last = format!("k{}", over - 1);
    let newest = |c: &[Value]| c.last().is_some_and(|k| k["id"] == last.as_str());
    let st = r.children(newest).await;
    let kids = st["agent"]["children"].as_array().unwrap();
    assert_eq!(kids.len(), MAX_AGENT_CHILDREN);
    assert_eq!(kids[0]["id"], "k4", "the oldest made room");
    r.task.abort();
}

fn roles_config() -> Value {
    let role = |r: &str, doc: Value| json!({ "ref": r, "version": 3, "hash": "d".repeat(64), "role": doc });
    json!({ "t": "config", "roles": {
        "v": 1,
        "roles": [
            role("u-rev", json!({ "schema": 1, "id": "reviewer", "name": "Reviewer", "purpose": "Reviews.",
                "instructions": "Review only.\n", "model": { "claude": "opus" } })),
            role("u-cdx", json!({ "schema": 1, "id": "porter", "name": "Porter", "cli": "codex" })),
            role("u-full", json!({ "schema": 1, "id": "boss", "name": "Boss", "access": "full",
                "model": { "claude": "--dangerously-skip-permissions" } })),
        ],
        "machine": ["u-rev", "u-cdx", "u-full"], "projects": {},
    } })
}

impl Rig {
    async fn with_roles(&mut self) {
        self.c.send(roles_config()).await;
        let file = self.state.join("roles.json");
        for _ in 0..200 {
            if file.is_file() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("config.roles was not kept");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_role_starts_a_pane_with_its_launch_values_and_never_a_dangerous_one() {
    let mut r = rig().await;
    let unknown =
        r.h.raw(
            "agent.start",
            json!({"pane_id": "w1:p1", "kind": "claude", "name": "x", "role": "reviewer"}),
        )
        .await;
    assert_eq!(unknown["error"]["code"], "unknown_role", "no roles yet: {unknown}");
    r.with_roles().await;
    let ok =
        r.h.raw(
            "agent.start",
            json!({"pane_id": "w1:p1", "kind": "claude", "name": "rev", "role": "reviewer", "args": ["-p", "go"]}),
        )
        .await;
    let res = &ok["result"];
    let argv: Vec<&str> = res["argv"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a.as_str().unwrap())
        .collect();
    assert_eq!(argv[..3], ["claude", "--model", "opus"], "{ok}");
    assert_eq!(argv[3], "--append-system-prompt-file");
    assert_eq!(
        argv[5..],
        ["--permission-mode", "plan", "-p", "go"],
        "the role's before the caller's"
    );
    assert_eq!(std::fs::read_to_string(argv[4]).unwrap(), "Review only.\n");
    assert_eq!(res["role"]["id"], "reviewer");
    assert_eq!(res["role"]["version"], 3);
    assert_eq!(res["role"]["applied"], json!(["model", "instructions", "access"]));

    let full =
        r.h.raw(
            "agent.start",
            json!({"pane_id": "w1:p1", "kind": "claude", "name": "b", "role": "boss"}),
        )
        .await;
    let argv = full["result"]["argv"].to_string();
    assert!(!argv.contains("dangerously") && !argv.contains("--model"), "{full}");
    assert_eq!(full["result"]["role"]["notApplied"], json!(["model"]));

    let other =
        r.h.raw(
            "agent.start",
            json!({"pane_id": "w1:p1", "kind": "claude", "name": "p", "role": "porter"}),
        )
        .await;
    assert_eq!(other["error"]["code"], "role_cli_mismatch", "{other}");
    let junk =
        r.h.raw(
            "agent.start",
            json!({"pane_id": "w1:p1", "kind": "claude", "name": "p", "role": "../x; rm"}),
        )
        .await;
    assert_eq!(junk["error"]["code"], "unknown_role");
    let not_text =
        r.h.raw(
            "agent.start",
            json!({"pane_id": "w1:p1", "kind": "claude", "name": "p", "role": 7}),
        )
        .await;
    assert_eq!(not_text["error"]["code"], "invalid_params");
    r.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_helper_named_like_a_role_carries_its_role_id() {
    let mut r = rig().await;
    r.with_roles().await;
    r.hook("UserPromptSubmit", json!({"prompt": "x"})).await;
    r.hook("SubagentStart", helper("a1", "reviewer")).await;
    let ev = r.event("subagent_started").await;
    assert_eq!(ev["data"]["roleId"], "reviewer", "{ev}");
    r.hook("SubagentStart", helper("a2", "tmuxedo-items:reviewer")).await;
    r.hook("SubagentStart", helper("a3", "Explore")).await;
    // Fixed to Codex: never a Claude helper, so not matched.
    r.hook("SubagentStart", helper("a4", "porter")).await;
    let st = r.children(|c| c.len() == 4).await;
    let kids = st["agent"]["children"].as_array().unwrap();
    let roles: Vec<&Value> = kids.iter().map(|k| &k["roleId"]).collect();
    assert_eq!(
        roles,
        [&json!("reviewer"), &json!("reviewer"), &Value::Null, &Value::Null],
        "{st}"
    );
    r.task.abort();
}
