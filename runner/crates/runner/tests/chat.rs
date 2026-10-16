//! Agent chat end to end (Phase 21 C1, docs/design/agent-chat.md), with a
//! scripted stand-in for Claude Code's structured mode
//! (tests/fixtures/fake_claude.py) driving the real `tmuxedo-runner hook`:
//! streaming, tool events, a permission round trip, a question with
//! "Other…", a plan, interrupt, queueing, masking, history replay and the
//! 30-day prune, chat → terminal → chat on one session, the per-login-kind
//! gate, park/resume, and the abuse cases. The runner restart mid-turn is in
//! `chat_restart.rs` (real processes).

mod common;

use base64::Engine;
use common::{options, start_holder, Conn, FakeGateway, H};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use tmuxedo_protocol::chat::{ChatFrame, MAX_EVENT_BYTES, MAX_TOOL_OUTPUT};
use tmuxedo_runner::daemon::{self, RunOptions};

const S: Duration = Duration::from_secs(20);
const KEY: &str = "sk-ant-api03-TESTKEYtestkey0123456789abcdefABCDEF";

/// Not shaped like a real key: only that it is set, and masked, matters.
const OPENAI: &str = "codex-test-value-0123456789abcdefABCDEF";

/// The fake `claude` on PATH, once per test binary (PATH is process-wide).
fn fake_bin() -> &'static Path {
    static D: OnceLock<PathBuf> = OnceLock::new();
    D.get_or_init(|| {
        let d = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("tmuxedo-fake-claude-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let src = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/fake_claude.py");
        std::fs::copy(src, d.join("claude")).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(d.join("claude"), std::fs::Permissions::from_mode(0o755)).unwrap();
        // The fake `codex` (its app-server) beside it.
        let src = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/fake_codex.py");
        std::fs::copy(src, d.join("codex")).unwrap();
        std::fs::set_permissions(d.join("codex"), std::fs::Permissions::from_mode(0o755)).unwrap();
        // The fake ACP agent, under Cursor's two names (so no real Cursor CLI is ever run).
        let src = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/fake_acp.py");
        for name in ["agent", "cursor-agent"] {
            std::fs::copy(src, d.join(name)).unwrap();
            std::fs::set_permissions(d.join(name), std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::env::set_var(
            "PATH",
            format!("{}:{}", d.display(), std::env::var("PATH").unwrap_or_default()),
        );
        d
    })
}

struct Chat {
    dir: tempfile::TempDir,
    c: Conn,
    h: H,
    _task: tokio::task::JoinHandle<Result<daemon::ExitReason, String>>,
    req: u32,
}

#[derive(Default)]
struct Setup {
    api_key: bool,
    /// An OpenAI API key in the shell env (Codex's `api_key` login kind).
    openai: bool,
    /// A Cursor API key in the shell env (the ACP agent's `api_key` login kind).
    cursor: bool,
    /// The fake ACP agent does not advertise `session/load`.
    acp_no_load: bool,
    policy: Option<&'static str>,
    /// Web's Auto-approve policy (`chat.autoApprove`), absent = not sent.
    auto: Option<&'static str>,
    tweak: Option<fn(&mut RunOptions)>,
}

impl Chat {
    async fn start(s: Setup) -> Chat {
        fake_bin();
        let dir = common::tempdir();
        let d = dir.path();
        std::fs::create_dir_all(d.join("log")).unwrap();
        std::fs::create_dir_all(d.join("work")).unwrap();
        let holder = start_holder(d);
        let gw = FakeGateway::start().await;
        let mut o = options(d, &gw.url, holder);
        o.home = d.display().to_string();
        o.default_work_dir = d.join("work").display().to_string();
        if let Some(f) = s.tweak {
            f(&mut o);
        }
        let sock = o.runner_socket.clone();
        let task = tokio::spawn(daemon::run(o));
        let mut c = gw.accept(S).await.expect("runner connects");
        let hello = c.hello(S).await;
        assert!(
            hello["runner"]["features"].as_array().unwrap().contains(&json!("chat")),
            "hello lists chat: {hello}"
        );
        let mut env = json!({ "FAKE_LOG": d.join("log").display().to_string() });
        if s.api_key {
            env["ANTHROPIC_API_KEY"] = json!(KEY);
        }
        if s.openai {
            env["OPENAI_API_KEY"] = json!(OPENAI);
        }
        if s.cursor {
            env["CURSOR_API_KEY"] = json!(OPENAI);
        }
        if s.acp_no_load {
            env["FAKE_ACP_NO_LOAD"] = json!("1");
        }
        let mut config = json!({ "t": "config", "shell": { "aliases": {}, "env": env, "rc": "" } });
        if let Some(p) = s.policy {
            config["chat"]["subscription"] = json!(p);
        }
        if let Some(a) = s.auto {
            config["chat"]["autoApprove"] = json!(a);
        }
        c.send(config).await;
        let h = H::attach_to(&sock).await;
        // The config is applied once the runner reports the env (same frame as the chat policy).
        let until = Instant::now() + S;
        loop {
            let v = h.raw("runner.config", json!({})).await;
            if v["result"]["shell"]["env"]["FAKE_LOG"].is_string() {
                break;
            }
            assert!(Instant::now() < until, "config never applied: {v}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Chat {
            dir,
            c,
            h,
            _task: task,
            req: 0,
        }
    }

    fn log_dir(&self) -> PathBuf {
        self.dir.path().join("log")
    }

    fn argv_log(&self) -> Vec<Vec<String>> {
        std::fs::read_to_string(self.log_dir().join("argv.log"))
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    /// The argv log once it has `n` starts (the fake writes it as it starts).
    async fn argv_wait(&self, n: usize) -> Vec<Vec<String>> {
        let until = Instant::now() + S;
        loop {
            let a = self.argv_log();
            if a.len() >= n {
                return a;
            }
            assert!(Instant::now() < until, "only {} agent starts: {a:?}", a.len());
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn start_chat(&mut self, params: Value) -> String {
        let r = self.h.raw("chat.start", params).await;
        let sid = r["result"]["session"]["id"]
            .as_str()
            .unwrap_or_else(|| panic!("chat.start failed: {r}"))
            .to_string();
        self.attach(&sid, 1).await;
        sid
    }

    async fn attach(&mut self, sid: &str, from: u64) -> u32 {
        self.req += 1;
        let req = self.req;
        self.c
            .send(json!({ "t": "chat_attach", "req": req, "sessionId": sid, "from": from }))
            .await;
        let a = self
            .c
            .text(S, move |v| v["t"] == "chat_attached" && v["req"] == req)
            .await;
        assert_eq!(a["ok"], true, "{a}");
        req
    }

    async fn send(&self, sid: &str, text: &str) -> Value {
        let r = self
            .h
            .raw("chat.send", json!({ "session_id": sid, "text": text }))
            .await;
        assert!(r.get("result").is_some(), "chat.send failed: {r}");
        r["result"].clone()
    }

    /// Durable events of `sid` seen on the socket so far (req 0 and replays), by seq.
    fn events(&self, sid: &str) -> Vec<Value> {
        let mut seen = std::collections::BTreeMap::new();
        for f in self.c.chats.iter().filter(|f| f.session_id == sid && !f.ephemeral) {
            seen.insert(f.seq, serde_json::from_slice::<Value>(&f.payload).unwrap());
        }
        seen.into_values().collect()
    }

    /// The first durable event matching `pred` (waits for it).
    async fn wait(&mut self, sid: &str, pred: impl Fn(&Value) -> bool) -> Value {
        let s = sid.to_string();
        let p = |fs: &[ChatFrame]| {
            fs.iter().any(|f| {
                f.session_id == s && !f.ephemeral && serde_json::from_slice::<Value>(&f.payload).is_ok_and(|v| pred(&v))
            })
        };
        self.c.chats_until(S, p).await;
        self.events(sid).into_iter().find(|v| pred(v)).unwrap()
    }

    async fn wait_type(&mut self, sid: &str, t: &str, n: usize) -> Vec<Value> {
        let s = sid.to_string();
        let t2 = t.to_string();
        self.c
            .chats_until(S, move |fs| {
                fs.iter()
                    .filter(|f| {
                        f.session_id == s
                            && !f.ephemeral
                            && serde_json::from_slice::<Value>(&f.payload).is_ok_and(|v| v["type"] == t2.as_str())
                    })
                    .count()
                    >= n
            })
            .await;
        self.events(sid).into_iter().filter(|v| v["type"] == t).collect()
    }

    async fn need(&mut self, sid: &str) -> Value {
        let n = self.wait_type(sid, "needs.opened", 1).await;
        n.last().unwrap()["item"].clone()
    }

    async fn respond(&self, sid: &str, need: &str, response: Value) -> Value {
        self.h
            .raw(
                "chat.respond",
                json!({ "session_id": sid, "need_id": need, "response": response, "by": "test" }),
            )
            .await
    }
}

fn assert_contiguous(events: &[Value]) {
    for (i, e) in events.iter().enumerate() {
        assert_eq!(
            e["seq"].as_u64().unwrap(),
            i as u64 + 1,
            "seqs 1..n with no hole: {events:#?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn streaming_deltas_then_the_message_and_a_clean_log() {
    let mut t = Chat::start(Setup {
        api_key: true,
        ..Default::default()
    })
    .await;
    let sid = t.start_chat(json!({ "title": "greeting" })).await;
    let opened = t.wait(&sid, |v| v["type"] == "session.opened").await;
    assert_eq!(opened["plugin"], "claude");
    assert_eq!(opened["login"], "api_key");
    assert_eq!(opened["mode"], "ask_first");
    let sent = t.send(&sid, "hello").await;
    assert_eq!(sent["queued"], false);
    let done = t.wait(&sid, |v| v["type"] == "message.done").await;
    assert_eq!(done["text"], "Hello there");
    let turn = t.wait(&sid, |v| v["type"] == "turn.done").await;
    assert_eq!(turn["status"], "completed");
    assert_eq!(turn["usage"]["outputTokens"], 5);
    // Deltas arrived first, ephemeral, numbered with the last durable seq, same message id.
    let deltas: Vec<&ChatFrame> = t.c.chats.iter().filter(|f| f.ephemeral).collect();
    assert_eq!(deltas.len(), 3, "three text deltas");
    let joined: String = deltas
        .iter()
        .map(|f| {
            serde_json::from_slice::<Value>(&f.payload).unwrap()["text"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(joined, "Hello there");
    for f in &deltas {
        let v: Value = serde_json::from_slice(&f.payload).unwrap();
        assert_eq!(v["type"], "message.delta");
        assert_eq!(v["messageId"], done["messageId"]);
        assert!(f.seq < done["seq"].as_u64().unwrap());
        assert_eq!(v["seq"], 0);
    }
    let all = t.events(&sid);
    assert_contiguous(&all);
    let types: Vec<&str> = all.iter().map(|v| v["type"].as_str().unwrap()).collect();
    assert_eq!(
        types,
        [
            "session.opened",
            "message.user",
            "turn.started",
            "message.done",
            "turn.done"
        ]
    );
    // Every event of the turn carries its turn id; the user's message too.
    assert!(all[1..].iter().all(|v| v["turnId"] == "t1"), "{all:#?}");
    // The log holds exactly the durable events (no deltas), and chat.read serves them.
    let r = t.h.ok("chat.read", json!({ "session_id": sid, "from_seq": 1 })).await;
    assert_eq!(r["events"].as_array().unwrap().len(), all.len());
    assert_eq!(r["lastSeq"], all.len());
    let log = std::fs::read_to_string(t.dir.path().join(format!("state/chat/{sid}/events.ndjson"))).unwrap();
    assert!(!log.contains("message.delta"));
    // The structured mode argv: the real binary (not the shim), stream-json both ways, the chat hooks.
    let argv = &t.argv_log()[0];
    for a in [
        "-p",
        "--input-format",
        "--output-format",
        "--include-partial-messages",
        "--session-id",
    ] {
        assert!(argv.contains(&a.to_string()), "{a} in {argv:?}");
    }
    let settings = &argv[argv.iter().position(|a| a == "--settings").unwrap() + 1];
    let s: Value = serde_json::from_str(&std::fs::read_to_string(settings).unwrap()).unwrap();
    assert_eq!(s["hooks"]["PreToolUse"][0]["matcher"], "AskUserQuestion");
    assert_eq!(s["hooks"]["PermissionRequest"][0]["hooks"][0]["timeout"], 600);
    // chat.list / chat.get.
    let l = t.h.ok("chat.list", json!({})).await;
    assert_eq!(l["sessions"][0]["id"], sid.as_str());
    assert_eq!(l["sessions"][0]["status"], "idle");
    assert_eq!(l["sessions"][0]["preview"], "Hello there");
    // Who wrote the previewed message: the agent's reply came last.
    assert_eq!(l["sessions"][0]["previewBy"], "agent");
    let g = t.h.ok("chat.get", json!({ "session_id": sid })).await;
    assert_eq!(g["session"]["title"], "greeting");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn permission_round_trip_with_tool_events_and_stale_answers() {
    let mut t = Chat::start(Setup {
        api_key: true,
        ..Default::default()
    })
    .await;
    let sid = t.start_chat(json!({})).await;
    t.send(&sid, "tool").await;
    let item = t.need(&sid).await;
    assert_eq!(item["kind"], "permission");
    assert_eq!(item["risk"], "normal");
    assert_eq!(item["title"], "Run `echo hi`");
    assert_eq!(item["options"], json!(["allow_once", "allow_session", "deny"]));
    assert_eq!(item["input"]["command"], "echo hi");
    let id = item["id"].as_str().unwrap().to_string();
    // The tool row is there before the decision.
    let started = t.wait(&sid, |v| v["type"] == "tool.started").await;
    assert_eq!(started["kind"], "execute");
    assert_eq!(started["title"], "Ran `echo hi`");
    // A durable machine event for push (web's `chat_needs`).
    let ev = t.c.text(S, |v| v["t"] == "event" && v["type"] == "chat_needs").await;
    assert_eq!(ev["data"]["needId"], id.as_str());
    assert_eq!(ev["data"]["sessionId"], sid.as_str());
    assert_eq!(ev["paneId"], "");
    // chat.get lists it; status says needs_you.
    let g = t.h.ok("chat.get", json!({ "session_id": sid })).await;
    assert_eq!(g["needs"][0]["id"], id.as_str());
    assert_eq!(g["session"]["status"], "needs_you");
    // A wrong kind of answer is refused, not delivered.
    let bad = t.respond(&sid, &id, json!({ "kind": "approve_plan" })).await;
    assert_eq!(bad["error"]["code"], "invalid_response");
    let r = t.respond(&sid, &id, json!({ "kind": "allow_session" })).await;
    assert_eq!(r["result"]["outcome"], "accepted", "{r}");
    let msg = t
        .wait(&sid, |v| {
            v["type"] == "message.done" && v["text"].as_str().unwrap_or("").starts_with("Ran")
        })
        .await;
    assert_eq!(msg["text"], "Ran it (for this session)");
    let resolved = t.wait(&sid, |v| v["type"] == "needs.resolved").await;
    assert_eq!(resolved["id"], id.as_str());
    assert_eq!(resolved["outcome"], "answered");
    assert_eq!(resolved["by"], "test");
    let done = t.wait(&sid, |v| v["type"] == "tool.done").await;
    assert_eq!(done["status"], "completed");
    assert_eq!(done["output"], "hi");
    t.wait(&sid, |v| v["type"] == "turn.done").await;
    // Answered once: the same answer again, or an id that never existed, is stale.
    let again = t.respond(&sid, &id, json!({ "kind": "allow_once" })).await;
    assert_eq!(again["result"]["outcome"], "stale");
    let never = t.respond(&sid, "nope", json!({ "kind": "deny" })).await;
    assert_eq!(never["result"]["outcome"], "stale");

    // Deny with a message ("Tell it what to do instead").
    t.send(&sid, "tool").await;
    let n = t.wait_type(&sid, "needs.opened", 2).await;
    let id2 = n[1]["item"]["id"].as_str().unwrap().to_string();
    let r = t
        .respond(&sid, &id2, json!({ "kind": "deny", "message": "use printf instead" }))
        .await;
    assert_eq!(r["result"]["outcome"], "accepted");
    t.wait(&sid, |v| {
        v["type"] == "message.done" && v["text"] == "Not running it: use printf instead"
    })
    .await;
    // A call a person said no to did not run: `denied`, not `failed`.
    let refused = t.wait_type(&sid, "tool.done", 2).await;
    assert_eq!(refused[0]["status"], "completed");
    assert_eq!(refused[1]["status"], "denied");
    assert_eq!(refused[1]["summary"], "Permission denied: use printf instead");
    assert_contiguous(&t.events(&sid));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn question_answered_with_other_and_multi_select() {
    let mut t = Chat::start(Setup {
        api_key: true,
        ..Default::default()
    })
    .await;
    let sid = t.start_chat(json!({})).await;
    t.send(&sid, "ask").await;
    let item = t.need(&sid).await;
    assert_eq!(item["kind"], "question");
    assert_eq!(item["title"], "Answer 2 questions");
    assert_eq!(item["questions"][0]["text"], "Where should the notes go?");
    assert_eq!(item["questions"][0]["header"], "Notes");
    assert_eq!(item["questions"][0]["options"][1]["label"], "PR description");
    assert_eq!(item["questions"][1]["multi"], true);
    let id = item["id"].as_str().unwrap().to_string();
    // Every question needs an answer.
    let partial = t
        .respond(
            &sid,
            &id,
            json!({ "kind": "answers", "answers": { "Where should the notes go?": "x" } }),
        )
        .await;
    assert_eq!(partial["error"]["code"], "invalid_response", "{partial}");
    // "Other…": free text is the answer; by question text or by index; multi-select as a list.
    let r = t
        .respond(
            &sid,
            &id,
            json!({ "kind": "answers", "answers": { "Where should the notes go?": "In the team wiki", "1": ["lint", "tests"] } }),
        )
        .await;
    assert_eq!(r["result"]["outcome"], "accepted", "{r}");
    let msg = t
        .wait(&sid, |v| {
            v["type"] == "message.done" && v["text"].as_str().unwrap_or("").starts_with("You chose")
        })
        .await;
    let text = msg["text"].as_str().unwrap();
    assert!(
        text.contains(r#""Where should the notes go?": "In the team wiki""#),
        "{text}"
    );
    assert!(text.contains(r#""Which checks?": "lint, tests""#), "{text}");

    // Skip: a general reply instead.
    t.send(&sid, "ask").await;
    let n = t.wait_type(&sid, "needs.opened", 2).await;
    let id2 = n[1]["item"]["id"].as_str().unwrap().to_string();
    t.respond(&sid, &id2, json!({ "kind": "deny", "message": "decide yourself" }))
        .await;
    t.wait(&sid, |v| {
        v["type"] == "message.done" && v["text"] == "Skipped: decide yourself"
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn plan_keep_planning_then_approve() {
    let mut t = Chat::start(Setup {
        api_key: true,
        ..Default::default()
    })
    .await;
    let sid = t.start_chat(json!({ "mode": "plan_first" })).await;
    let argv = &t.argv_wait(1).await[0];
    let i = argv.iter().position(|a| a == "--permission-mode").unwrap();
    assert_eq!(argv[i + 1], "plan");
    t.send(&sid, "plan").await;
    let item = t.need(&sid).await;
    assert_eq!(item["kind"], "plan");
    assert_eq!(item["options"], json!(["approve_plan", "keep_planning"]));
    assert_eq!(item["markdown"], "# Plan\n- step one\n- step two");
    let id = item["id"].as_str().unwrap().to_string();
    t.respond(&sid, &id, json!({ "kind": "keep_planning", "message": "add tests" }))
        .await;
    t.wait(&sid, |v| {
        v["type"] == "message.done" && v["text"] == "Still planning: Keep planning: add tests"
    })
    .await;
    t.send(&sid, "plan").await;
    let n = t.wait_type(&sid, "needs.opened", 2).await;
    let id2 = n[1]["item"]["id"].as_str().unwrap().to_string();
    let r = t.respond(&sid, &id2, json!({ "kind": "approve_plan" })).await;
    assert_eq!(r["result"]["outcome"], "accepted");
    t.wait(&sid, |v| v["type"] == "message.done" && v["text"] == "Plan approved")
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn high_risk_needs_a_fresh_step_up_and_has_no_session_allow() {
    let mut t = Chat::start(Setup {
        api_key: true,
        ..Default::default()
    })
    .await;
    let sid = t.start_chat(json!({})).await;
    t.send(&sid, "push").await;
    let item = t.need(&sid).await;
    assert_eq!(item["risk"], "high");
    assert_eq!(item["why"], "Pushes to a remote");
    assert_eq!(item["options"], json!(["allow_once", "deny"]));
    let id = item["id"].as_str().unwrap().to_string();
    let no = t.respond(&sid, &id, json!({ "kind": "allow_once" })).await;
    assert_eq!(no["error"]["code"], "stepup_required");
    let sess = t.respond(&sid, &id, json!({ "kind": "allow_session" })).await;
    assert_eq!(sess["error"]["code"], "invalid_response");
    let ok =
        t.h.raw(
            "chat.respond",
            json!({ "session_id": sid, "need_id": id, "response": { "kind": "allow_once" }, "fresh_stepup": true }),
        )
        .await;
    assert_eq!(ok["result"]["outcome"], "accepted", "{ok}");
    t.wait(&sid, |v| v["type"] == "message.done" && v["text"] == "push allow")
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn interrupt_ends_the_turn_and_queued_messages_follow() {
    let mut t = Chat::start(Setup {
        api_key: true,
        ..Default::default()
    })
    .await;
    let sid = t.start_chat(json!({})).await;
    // Nothing to interrupt yet.
    let r = t.h.ok("chat.interrupt", json!({ "session_id": sid })).await;
    assert_eq!(r["interrupted"], false);
    t.send(&sid, "slow 60").await;
    t.c.chats_until(S, |fs| fs.iter().filter(|f| f.ephemeral).count() >= 3)
        .await;
    // A message during the turn is queued (shown at once, delivered after).
    let q = t.send(&sid, "hello").await;
    assert_eq!(q["queued"], true);
    let shown = t
        .wait(&sid, |v| v["type"] == "message.user" && v["text"] == "hello")
        .await;
    assert_eq!(shown["queued"], true);
    // The summary's preview is now yours, and says so.
    let g = t.h.ok("chat.get", json!({ "session_id": sid })).await;
    assert_eq!(g["session"]["preview"], "hello");
    assert_eq!(g["session"]["previewBy"], "you");
    let r = t.h.ok("chat.interrupt", json!({ "session_id": sid })).await;
    assert_eq!(r["interrupted"], true);
    let done = t.wait(&sid, |v| v["type"] == "turn.done").await;
    assert_eq!(done["status"], "interrupted");
    // The queued message runs next, as its own turn.
    let hello = t
        .wait(&sid, |v| v["type"] == "message.done" && v["text"] == "Hello there")
        .await;
    assert_eq!(hello["turnId"], "t2");
    let turns = t.wait_type(&sid, "turn.done", 2).await;
    assert_eq!(turns[1]["status"], "completed");
    assert_contiguous(&t.events(&sid));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn secrets_are_masked_in_the_log_the_frames_and_push() {
    let mut t = Chat::start(Setup {
        api_key: true,
        ..Default::default()
    })
    .await;
    let sid = t.start_chat(json!({ "text": format!("hello, my key is {KEY}") })).await;
    t.wait(&sid, |v| v["type"] == "turn.done").await;
    t.send(&sid, "secret").await;
    t.wait(&sid, |v| {
        v["type"] == "message.done" && v["text"].as_str().unwrap_or("").starts_with("The key")
    })
    .await;
    t.send(&sid, "leak").await;
    let item = t.need(&sid).await;
    let id = item["id"].as_str().unwrap().to_string();
    let push = t.c.text(S, |v| v["t"] == "event" && v["type"] == "chat_needs").await;
    t.respond(&sid, &id, json!({ "kind": "deny" })).await;
    t.wait_type(&sid, "turn.done", 3).await;
    let b64 = base64::engine::general_purpose::STANDARD.encode(KEY);
    let log = std::fs::read_to_string(t.dir.path().join(format!("state/chat/{sid}/events.ndjson"))).unwrap();
    let meta = std::fs::read_to_string(t.dir.path().join(format!("state/chat/{sid}/meta.json"))).unwrap();
    let frames: String =
        t.c.chats
            .iter()
            .map(|f| String::from_utf8_lossy(&f.payload).into_owned())
            .collect();
    let read = t.h.ok("chat.read", json!({ "session_id": sid })).await.to_string();
    let list = t.h.ok("chat.list", json!({})).await.to_string();
    for (what, hay) in [
        ("log", &log),
        ("meta", &meta),
        ("frames", &frames),
        ("read", &read),
        ("list", &list),
        ("push", &push.to_string()),
    ] {
        assert!(!hay.contains(KEY), "the key in the {what}");
        assert!(!hay.contains(&b64), "the base64 key in the {what}");
        assert!(!hay.contains("TESTKEYtestkey"), "a piece of the key in the {what}");
    }
    assert!(log.contains("***"), "masked, not dropped");
    assert!(frames.contains("The key is ***"), "{frames}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn big_output_files_plans_and_unknown_lines() {
    let mut t = Chat::start(Setup {
        api_key: true,
        ..Default::default()
    })
    .await;
    let sid = t.start_chat(json!({})).await;
    t.send(&sid, "big").await;
    let done = t.wait(&sid, |v| v["type"] == "tool.done").await;
    let out = done["output"].as_str().unwrap();
    assert!(out.len() <= MAX_TOOL_OUTPUT + 8, "the tail only: {}", out.len());
    assert!(out.ends_with("END"));
    assert_eq!(done["outputKept"], "tail", "a command: its end");
    assert_eq!(done["outputBytes"], 600 * 1024 + 3, "the size of all of it");
    t.wait(&sid, |v| v["type"] == "turn.done").await;
    assert!(t.c.chats.iter().all(|f| f.payload.len() <= MAX_EVENT_BYTES));

    t.send(&sid, "write notes.md").await;
    let files = t.wait(&sid, |v| v["type"] == "files.changed").await;
    let path = t.dir.path().join("work/notes.md").display().to_string();
    assert_eq!(files["files"][0]["path"], path.as_str());
    assert_eq!(files["files"][0]["change"], "created");
    assert_eq!(files["files"][0]["source"], "tool");
    let diff = t.wait(&sid, |v| v["type"] == "tool.updated").await;
    assert_eq!(diff["diff"]["newText"], "# Notes\n");
    t.wait_type(&sid, "turn.done", 2).await;

    t.send(&sid, "todo").await;
    let plan = t.wait(&sid, |v| v["type"] == "plan.updated").await;
    assert_eq!(
        plan["entries"],
        json!([{ "text": "First", "status": "done" }, { "text": "Second", "status": "in_progress" }])
    );
    t.wait_type(&sid, "turn.done", 3).await;

    // CLI drift: a non-JSON line and an unknown event become neutral notices; the chat goes on.
    t.send(&sid, "junk").await;
    let n = t.wait_type(&sid, "notice", 2).await;
    assert_eq!(n[0]["code"], "agent_output");
    assert_eq!(n[1]["code"], "unknown_event");
    assert_eq!(n[1]["native"], "brand_new_event");
    t.wait_type(&sid, "turn.done", 4).await;
    t.send(&sid, "hello").await;
    t.wait(&sid, |v| v["type"] == "message.done" && v["text"] == "Hello there")
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_crash_says_why_and_the_next_message_resumes_the_same_session() {
    let mut t = Chat::start(Setup {
        api_key: true,
        ..Default::default()
    })
    .await;
    let sid = t.start_chat(json!({})).await;
    t.send(&sid, "hello").await;
    t.wait(&sid, |v| v["type"] == "turn.done").await;
    t.send(&sid, "exit").await;
    let failed = t.wait_type(&sid, "turn.done", 2).await;
    assert_eq!(failed[1]["status"], "failed");
    assert!(
        failed[1]["error"].as_str().unwrap().contains("Invalid API key"),
        "the agent's own last stderr line: {}",
        failed[1]
    );
    let s = t.h.ok("chat.get", json!({ "session_id": sid })).await;
    assert_eq!(s["session"]["status"], "parked");
    t.send(&sid, "hello").await;
    t.wait_type(&sid, "turn.done", 3).await;
    let argv = t.argv_log();
    assert_eq!(argv.len(), 2);
    let vendor = s["session"]["vendorSessionId"].as_str().unwrap();
    let i = argv[0].iter().position(|a| a == "--session-id").unwrap();
    assert_eq!(argv[0][i + 1], vendor);
    let j = argv[1].iter().position(|a| a == "--resume").unwrap();
    assert_eq!(argv[1][j + 1], vendor, "resumed the same session");
    let opened = t.wait_type(&sid, "session.opened", 2).await;
    assert_eq!(opened[1]["resumedFrom"], vendor);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn chat_to_terminal_and_back_on_one_session() {
    let mut t = Chat::start(Setup {
        api_key: true,
        ..Default::default()
    })
    .await;
    let sid = t.start_chat(json!({ "title": "handoff" })).await;
    // Not before the CLI has the session.
    let early =
        t.h.raw("chat.handoff", json!({ "session_id": sid, "to": "terminal" }))
            .await;
    assert_eq!(early["error"]["code"], "chat_not_started");
    t.send(&sid, "hello").await;
    t.wait(&sid, |v| v["type"] == "turn.done").await;
    let vendor = t.h.ok("chat.get", json!({ "session_id": sid })).await["session"]["vendorSessionId"]
        .as_str()
        .unwrap()
        .to_string();
    // Mid-turn it waits (or stops the turn when asked).
    // Deltas of the turns before this one do not count: wait for five of the slow turn's own.
    let before = t.c.chats.iter().filter(|f| f.ephemeral).count();
    // The turn must still be running when the handoff asks: only `stop` ends it, so a starved
    // test thread cannot outlast it (a short one finishes by itself under load).
    t.send(&sid, "slow 400").await;
    t.c.chats_until(S, |fs| fs.iter().filter(|f| f.ephemeral).count() >= before + 5)
        .await;
    let busy =
        t.h.raw("chat.handoff", json!({ "session_id": sid, "to": "terminal" }))
            .await;
    assert_eq!(busy["error"]["code"], "turn_running");
    let r =
        t.h.ok(
            "chat.handoff",
            json!({ "session_id": sid, "to": "terminal", "stop": true }),
        )
        .await;
    let pane = r["paneId"].as_str().unwrap().to_string();
    assert_eq!(r["session"]["status"], "terminal");
    let ho = t.wait(&sid, |v| v["type"] == "session.handoff").await;
    assert_eq!(ho["to"], "terminal");
    assert_eq!(ho["paneId"], pane.as_str());
    let tdone = t.wait_type(&sid, "turn.done", 2).await;
    assert_eq!(tdone[1]["status"], "interrupted");
    // The pane resumes the same session in the terminal (through the shim, no -p).
    t.h.wait_text(&pane, &format!("FAKE TUI resumed {vendor}")).await;
    let last = t.argv_log().last().unwrap().clone();
    assert!(!last.contains(&"-p".to_string()), "{last:?}");
    assert!(last.contains(&vendor));
    // The chat refuses messages while the terminal drives it.
    let refused = t.h.raw("chat.send", json!({ "session_id": sid, "text": "hi" })).await;
    assert_eq!(refused["error"]["code"], "chat_in_terminal");
    // The pane's agent reported idle (SessionStart): back to chat.
    let until = Instant::now() + S;
    let back = loop {
        let r =
            t.h.raw("chat.handoff", json!({ "session_id": sid, "to": "chat" }))
                .await;
        if r.get("result").is_some() {
            break r["result"].clone();
        }
        assert_eq!(r["error"]["code"], "agent_busy", "{r}");
        assert!(Instant::now() < until, "the pane never went idle");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(back["session"]["status"], "parked");
    let hb = t.wait_type(&sid, "session.handoff", 2).await;
    assert_eq!(hb[1]["to"], "chat");
    t.wait(&sid, |v| v["type"] == "notice" && v["code"] == "terminal_turns")
        .await;
    // The pane is gone.
    let panes = t.h.ok("pane.list", json!({})).await;
    assert!(!panes.to_string().contains(&format!("\"{pane}\"")), "{panes}");
    t.send(&sid, "hello").await;
    t.wait_type(&sid, "turn.done", 3).await;
    let last = t.argv_log().last().unwrap().clone();
    assert!(last.contains(&"-p".to_string()));
    let j = last.iter().position(|a| a == "--resume").unwrap();
    assert_eq!(last[j + 1], vendor, "the chat resumed the same session");
    assert_contiguous(&t.events(&sid));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_terminal_session_opens_as_a_chat() {
    let mut t = Chat::start(Setup {
        api_key: true,
        ..Default::default()
    })
    .await;
    // A pane running the agent, started in the terminal (not from a chat).
    t.h.ok(
        "pane.send_text",
        json!({ "pane_id": "w1:p1", "text": "claude --resume abc-123\n" }),
    )
    .await;
    t.h.wait_text("w1:p1", "FAKE TUI resumed abc-123").await;
    let until = Instant::now() + S;
    let r = loop {
        let r =
            t.h.raw("chat.handoff", json!({ "pane_id": "w1:p1", "to": "chat" }))
                .await;
        if r.get("result").is_some() {
            break r["result"].clone();
        }
        assert!(Instant::now() < until, "{r}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let sid = r["session"]["id"].as_str().unwrap().to_string();
    assert_eq!(r["session"]["vendorSessionId"], "abc-123");
    t.attach(&sid, 1).await;
    t.send(&sid, "hello").await;
    t.wait(&sid, |v| v["type"] == "message.done").await;
    let last = t.argv_log().last().unwrap().clone();
    let j = last.iter().position(|a| a == "--resume").unwrap();
    assert_eq!(last[j + 1], "abc-123");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_subscription_login_is_refused_unless_web_allows_it() {
    // A public user (web's policy: subscription chat not allowed) without an API key.
    let mut t = Chat::start(Setup {
        policy: Some("deny"),
        ..Default::default()
    })
    .await;
    let r = t.h.raw("chat.start", json!({ "text": "hello" })).await;
    assert_eq!(r["error"]["code"], "chat_login_not_allowed", "{r}");
    assert!(r["error"]["message"].as_str().unwrap().contains("terminal"), "{r}");
    assert!(t.argv_log().is_empty(), "nothing was started");
    assert_eq!(t.h.ok("chat.list", json!({})).await["sessions"], json!([]));
    // Nor by adopting a terminal session.
    let r =
        t.h.raw("chat.handoff", json!({ "pane_id": "w1:p1", "to": "chat" }))
            .await;
    assert!(r.get("error").is_some());
    // The terminal path keeps working for the same agent (needs-you through the pane's hooks).
    t.h.ok(
        "pane.send_text",
        json!({ "pane_id": "w1:p1", "text": "claude --resume sub-1\n" }),
    )
    .await;
    t.h.wait_text("w1:p1", "FAKE TUI resumed sub-1").await;
    let st =
        t.c.text(S, |v| v["t"] == "status" && v["agent"]["sessionId"] == "sub-1")
            .await;
    assert_eq!(st["agent"]["kind"], "claude");

    // An admin (policy allow) may chat on the subscription login.
    let mut a = Chat::start(Setup {
        policy: Some("allow"),
        ..Default::default()
    })
    .await;
    let sid = a.start_chat(json!({})).await;
    let opened = a.wait(&sid, |v| v["type"] == "session.opened").await;
    assert_eq!(opened["login"], "subscription");
    // The policy can be withdrawn: the next start is refused.
    a.c.send(json!({ "t": "config", "chat": { "subscription": "deny" } }))
        .await;
    let until = Instant::now() + S;
    loop {
        let r = a.h.raw("chat.start", json!({})).await;
        if r["error"]["code"] == "chat_login_not_allowed" {
            break;
        }
        assert!(Instant::now() < until, "{r}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replay_from_a_seq_and_abuse_cases() {
    let mut t = Chat::start(Setup {
        api_key: true,
        ..Default::default()
    })
    .await;
    let sid = t.start_chat(json!({})).await;
    t.send(&sid, "hello").await;
    t.wait(&sid, |v| v["type"] == "turn.done").await;
    let all = t.events(&sid);
    // Replay from seq 3: exactly 3..n, tagged with the request, then chat_attached.
    t.c.chats.clear();
    let req = t.attach(&sid, 3).await;
    let replay: Vec<&ChatFrame> = t.c.chats.iter().filter(|f| f.req == req).collect();
    let seqs: Vec<u64> = replay.iter().map(|f| f.seq).collect();
    assert_eq!(seqs, (3..=all.len() as u64).collect::<Vec<_>>());
    assert!(replay.iter().all(|f| !f.gap && !f.ephemeral));
    // From beyond the end: nothing, still ok with lastSeq.
    t.req += 1;
    let r2 = t.req;
    t.c.send(json!({ "t": "chat_attach", "req": r2, "sessionId": sid, "from": 999_999 }))
        .await;
    let a = t.c.text(S, move |v| v["t"] == "chat_attached" && v["req"] == r2).await;
    assert_eq!(a["lastSeq"], all.len());
    // Another machine's (or a made-up) session id: refused, nothing leaks.
    t.c.send(json!({ "t": "chat_attach", "req": 77, "sessionId": "c-0000000000notmine", "from": 1 }))
        .await;
    let a = t.c.text(S, |v| v["t"] == "chat_attached" && v["req"] == 77).await;
    assert_eq!(a["ok"], false);
    assert_eq!(a["code"], "chat_not_found");
    for m in ["chat.get", "chat.read", "chat.send", "chat.respond", "chat.close"] {
        let r = t
            .h
            .raw(m, json!({ "session_id": "c-0000000000notmine", "text": "x", "need_id": "n", "response": {"kind": "deny"} }))
            .await;
        assert_eq!(r["error"]["code"], "chat_not_found", "{m}: {r}");
    }
    // A junk session id never reaches a session (refused by the frame parser).
    t.c.send(json!({ "t": "chat_attach", "req": 78, "sessionId": "../../etc", "from": 1 }))
        .await;
    t.c.drain(Duration::from_millis(300)).await;
    assert!(!t.c.texts.iter().any(|v| v["req"] == 78));
    // Oversized message.
    let big = "x".repeat(tmuxedo_protocol::chat::MAX_SEND_BYTES + 1);
    let r = t.h.raw("chat.send", json!({ "session_id": sid, "text": big })).await;
    assert_eq!(r["error"]["code"], "too_large");
    let r = t.h.raw("chat.send", json!({ "session_id": sid, "text": "   " })).await;
    assert_eq!(r["error"]["code"], "invalid_params");
    // A resent message (same clientMsgId) is not sent twice.
    let a1 =
        t.h.ok(
            "chat.send",
            json!({ "session_id": sid, "text": "hello", "client_msg_id": "m-1" }),
        )
        .await;
    let a2 =
        t.h.ok(
            "chat.send",
            json!({ "session_id": sid, "text": "hello", "client_msg_id": "m-1" }),
        )
        .await;
    assert_eq!(a1["seq"], a2["seq"]);
    assert_eq!(a2["duplicate"], true);
    t.wait_type(&sid, "turn.done", 2).await;
    t.c.drain(Duration::from_millis(500)).await;
    let users: Vec<Value> = t
        .events(&sid)
        .into_iter()
        .filter(|v| v["type"] == "message.user" && v["clientMsgId"] == "m-1")
        .collect();
    assert_eq!(users.len(), 1);
    // Bad parameters.
    let r = t.h.raw("chat.start", json!({ "mode": "yolo" })).await;
    assert_eq!(r["error"]["code"], "invalid_mode");
    let r = t.h.raw("chat.start", json!({ "agent": "nope" })).await;
    assert_eq!(r["error"]["code"], "unknown_agent_kind");
    let r = t.h.raw("chat.start", json!({ "cwd": "/no/such/dir" })).await;
    assert_eq!(r["error"]["code"], "invalid_cwd");
    let r =
        t.h.raw(
            "chat.respond",
            json!({ "session_id": sid, "need_id": "n", "response": "allow" }),
        )
        .await;
    assert_eq!(r["error"]["code"], "invalid_params");
    // Close: the agent stops, the history stays readable, sends are refused.
    t.h.ok("chat.close", json!({ "session_id": sid })).await;
    t.wait(&sid, |v| v["type"] == "session.closed").await;
    let r = t.h.raw("chat.send", json!({ "session_id": sid, "text": "hi" })).await;
    assert_eq!(r["error"]["code"], "chat_closed");
    assert!(
        t.h.ok("chat.read", json!({ "session_id": sid })).await["events"]
            .as_array()
            .unwrap()
            .len()
            > 3
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn history_is_pruned_at_30_days() {
    fake_bin();
    let dir = common::tempdir();
    let d = dir.path();
    let day = 24 * 3600 * 1000u64;
    let now = tmuxedo_runner::util::now_ms();
    let write = |id: &str, ages: &[u64]| {
        let sd = d.join("state/chat").join(id);
        std::fs::create_dir_all(&sd).unwrap();
        let meta = json!({
            "id": id, "plugin": "claude", "cwd": d.display().to_string(), "mode": "ask_first", "login": "api_key",
            "title": "old", "createdAt": "", "lastAt": "", "preview": "", "vendorSession": "v", "started": true,
            "incarnation": 1, "status": "parked", "offset": 0,
        });
        std::fs::write(sd.join("meta.json"), meta.to_string()).unwrap();
        let mut lines = String::new();
        for (i, age) in ages.iter().enumerate() {
            let ev = json!({ "seq": i + 1, "at": tmuxedo_runner::util::iso(now - age * day), "sessionId": id,
                             "type": "notice", "code": "x", "text": format!("event {}", i + 1) });
            lines.push_str(&format!("{ev}\n"));
        }
        std::fs::write(sd.join("events.ndjson"), lines).unwrap();
    };
    write("c-oldoldoldold01", &[45, 40, 31]);
    write("c-mixedmixed0002", &[40, 35, 29, 1]);
    let holder = start_holder(d);
    let gw = FakeGateway::start().await;
    let mut o = options(d, &gw.url, holder);
    o.home = d.display().to_string();
    let sock = o.runner_socket.clone();
    let _task = tokio::spawn(daemon::run(o));
    let mut c = gw.accept(S).await.unwrap();
    c.hello(S).await;
    let h = H::attach_to(&sock).await;
    let l = h.ok("chat.list", json!({})).await;
    let ids: Vec<&str> = l["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        ["c-mixedmixed0002"],
        "a chat with nothing newer than 30 days is gone"
    );
    assert!(!d.join("state/chat/c-oldoldoldold01").exists());
    // A record written before `previewBy` was kept: the field is left out, not guessed.
    assert!(l["sessions"][0].get("previewBy").is_none());
    let r = h.ok("chat.read", json!({ "session_id": "c-mixedmixed0002" })).await;
    assert_eq!(r["firstSeq"], 3);
    assert_eq!(r["gap"], true);
    assert_eq!(r["events"].as_array().unwrap().len(), 2);
    // A replay from the start is marked GAP on its first frame.
    c.send(json!({ "t": "chat_attach", "req": 5, "sessionId": "c-mixedmixed0002", "from": 1 }))
        .await;
    c.text(S, |v| v["t"] == "chat_attached" && v["req"] == 5).await;
    let frames: Vec<&ChatFrame> = c.chats.iter().filter(|f| f.req == 5).collect();
    assert_eq!(frames.iter().map(|f| f.seq).collect::<Vec<_>>(), [3, 4]);
    assert!(frames[0].gap && !frames[1].gap);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn idle_chats_park_and_held_decisions_expire() {
    let mut t = Chat::start(Setup {
        api_key: true,
        tweak: Some(|o| {
            o.chat_park_after = Duration::from_secs(1);
            o.prompt_hold = Duration::from_secs(2);
        }),
        ..Default::default()
    })
    .await;
    let sid = t.start_chat(json!({})).await;
    // A decision nobody answers expires; the agent hears no answer.
    t.send(&sid, "tool").await;
    let item = t.need(&sid).await;
    let r = t.wait(&sid, |v| v["type"] == "needs.resolved").await;
    assert_eq!(r["outcome"], "expired");
    assert_eq!(r["id"], item["id"]);
    t.wait(&sid, |v| {
        v["type"] == "message.done" && v["text"] == "Not running it: no answer"
    })
    .await;
    t.wait(&sid, |v| v["type"] == "turn.done").await;
    // Nobody watching and nothing to do: it parks (the agent stops) …
    t.c.send(json!({ "t": "chat_detach", "sessionId": sid })).await;
    let until = Instant::now() + S;
    loop {
        let g = t.h.ok("chat.get", json!({ "session_id": sid })).await;
        if g["session"]["status"] == "parked" {
            break;
        }
        assert!(Instant::now() < until, "never parked: {g}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    // … and the next message resumes it.
    t.attach(&sid, 1).await;
    t.send(&sid, "hello").await;
    t.wait(&sid, |v| v["type"] == "message.done" && v["text"] == "Hello there")
        .await;
    assert_eq!(t.argv_log().len(), 2);
    assert!(t.argv_log()[1].contains(&"--resume".to_string()));
}

/// Auto-approve (Phase 21, design §11 "Bypass permissions"): opt-in, needs
/// web's policy and a fresh step-up, runs the CLI in its bypass mode with
/// the guard hook, and the "never" list still blocks.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auto_approve_is_gated_audited_and_the_never_list_still_blocks() {
    let mut t = Chat::start(Setup {
        api_key: true,
        auto: Some("allow"),
        ..Default::default()
    })
    .await;
    let sid = t.start_chat(json!({})).await;
    // No fresh step-up (a token, a socket client): refused.
    let no =
        t.h.raw("chat.mode", json!({ "session_id": sid, "mode": "auto_approve" }))
            .await;
    assert_eq!(no["error"]["code"], "stepup_required", "{no}");
    let no =
        t.h.raw(
            "chat.mode",
            json!({ "session_id": sid, "mode": "auto_approve", "fresh_stepup": "yes" }),
        )
        .await;
    assert_eq!(
        no["error"]["code"], "stepup_required",
        "only a boolean true counts: {no}"
    );
    let ok =
        t.h.ok(
            "chat.mode",
            json!({ "session_id": sid, "mode": "auto_approve", "fresh_stepup": true }),
        )
        .await;
    assert_eq!(ok["mode"], "auto_approve");
    let m = t.wait(&sid, |v| v["type"] == "session.mode").await;
    assert_eq!(m["mode"], "auto_approve");

    // A normal action runs without asking (the CLI's bypass mode, the guard said go on).
    t.send(&sid, "tool").await;
    t.wait(&sid, |v| v["type"] == "message.done" && v["text"] == "Ran it (bypass)")
        .await;
    assert!(
        t.events(&sid).iter().all(|v| v["type"] != "needs.opened"),
        "nothing asked"
    );
    // The first start (Ask first) may be stopped by the mode change before it logs its argv.
    let argv = t.argv_wait(1).await;
    let last = argv.last().unwrap();
    let i = last.iter().position(|a| a == "--permission-mode").expect("a mode flag");
    assert_eq!(last[i + 1], "bypassPermissions");
    let settings_path = &last[last.iter().position(|a| a == "--settings").unwrap() + 1];
    let settings: Value = serde_json::from_str(&std::fs::read_to_string(settings_path).unwrap()).unwrap();
    let pre = settings["hooks"]["PreToolUse"].as_array().unwrap();
    assert!(
        pre.iter().any(|e| e.get("matcher").is_none()
            && e["hooks"][0]["command"].as_str().unwrap().ends_with("hook --guard")),
        "the guard runs on every tool: {settings}"
    );
    // Audited: one machine event for what the turn let through.
    let ev = t.c.text(S, |v| v["t"] == "event" && v["type"] == "chat_auto").await;
    assert_eq!(ev["data"]["action"], "approved", "{ev}");
    assert_eq!(ev["data"]["count"], 1);
    assert_eq!(ev["data"]["sessionId"], sid.as_str());

    // The never list: a push is blocked, not asked, not run; the chat and the audit say so.
    t.send(&sid, "push").await;
    let said = t
        .wait(&sid, |v| {
            v["type"] == "message.done" && v["text"].as_str().unwrap_or("").starts_with("push ")
        })
        .await;
    assert!(
        said["text"]
            .as_str()
            .unwrap()
            .starts_with("push deny: tmuxedo blocked this"),
        "{said}"
    );
    let n = t
        .wait(&sid, |v| v["type"] == "notice" && v["code"] == "auto_blocked")
        .await;
    assert!(n["text"].as_str().unwrap().contains("pushes to a remote"), "{n}");
    // The call a rule said no to is `denied` (not run), not `failed`.
    let refused = t
        .wait(&sid, |v| v["type"] == "tool.done" && v["status"] != "completed")
        .await;
    assert_eq!(refused["status"], "denied", "{refused}");
    let blocked =
        t.c.text(S, |v| {
            v["t"] == "event" && v["type"] == "chat_auto" && v["data"]["action"] == "blocked"
        })
        .await;
    assert_eq!(blocked["data"]["why"], "Pushes to a remote");
    assert!(
        t.events(&sid).iter().all(|v| v["type"] != "needs.opened"),
        "never asked either"
    );
    // A question is still a person's (its own hook holds it).
    t.send(&sid, "ask").await;
    let item = t.need(&sid).await;
    assert_eq!(item["kind"], "question");
    let id = item["id"].as_str().unwrap().to_string();
    t.respond(
        &sid,
        &id,
        json!({ "kind": "answers", "answers": { "0": "PR description", "1": ["lint"] } }),
    )
    .await;
    t.wait(&sid, |v| {
        v["type"] == "message.done" && v["text"].as_str().unwrap_or("").starts_with("You chose")
    })
    .await;
    t.wait_type(&sid, "turn.done", 3).await;

    // An admin forbids it: back to Ask first, said in the chat; turning it on again is refused.
    t.c.send(json!({ "t": "config", "chat": { "autoApprove": "deny" } }))
        .await;
    let back = t
        .wait(&sid, |v| v["type"] == "session.mode" && v["mode"] == "ask_first")
        .await;
    assert!(back["seq"].as_u64().unwrap() > m["seq"].as_u64().unwrap());
    t.wait(&sid, |v| v["type"] == "notice" && v["code"] == "auto_approve_off")
        .await;
    let no =
        t.h.raw(
            "chat.mode",
            json!({ "session_id": sid, "mode": "auto_approve", "fresh_stepup": true }),
        )
        .await;
    assert_eq!(no["error"]["code"], "auto_approve_forbidden", "{no}");
    let no =
        t.h.raw("chat.start", json!({ "mode": "auto_approve", "fresh_stepup": true }))
            .await;
    assert_eq!(no["error"]["code"], "auto_approve_forbidden", "{no}");
    // The next turn asks again.
    t.send(&sid, "tool").await;
    let items = t.wait_type(&sid, "needs.opened", 2).await;
    let item = &items[1]["item"];
    assert_eq!(item["kind"], "permission");
    let id = item["id"].as_str().unwrap().to_string();
    t.respond(&sid, &id, json!({ "kind": "deny" })).await;
    t.wait_type(&sid, "turn.done", 4).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auto_approve_starts_only_with_policy_and_step_up() {
    // Web never said (an older web): not allowed.
    let mut t = Chat::start(Setup {
        api_key: true,
        ..Default::default()
    })
    .await;
    let no =
        t.h.raw("chat.start", json!({ "mode": "auto_approve", "fresh_stepup": true }))
            .await;
    assert_eq!(no["error"]["code"], "auto_approve_forbidden", "{no}");
    t.c.send(json!({ "t": "config", "chat": { "autoApprove": "allow" } }))
        .await;
    let until = Instant::now() + S;
    loop {
        let r = t.h.raw("chat.start", json!({ "mode": "auto_approve" })).await;
        if r["error"]["code"] == "stepup_required" {
            break;
        }
        assert!(Instant::now() < until, "policy never applied: {r}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let sid = t
        .start_chat(json!({ "mode": "auto_approve", "fresh_stepup": true, "text": "tool" }))
        .await;
    t.wait(&sid, |v| v["type"] == "message.done" && v["text"] == "Ran it (bypass)")
        .await;
    let opened = t.wait(&sid, |v| v["type"] == "session.opened").await;
    assert_eq!(opened["mode"], "auto_approve");
}

/// The Files panel's reads (design §7): confined to the work folder,
/// secret files refused, masked, with what this chat changed; files a
/// shell command wrote are found at the end of the turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn files_are_confined_masked_and_shell_writes_are_found() {
    let mut t = Chat::start(Setup {
        api_key: true,
        ..Default::default()
    })
    .await;
    let work = t.dir.path().join("work");
    let vcs = |args: &[&str]| {
        let o = std::process::Command::new("git")
            .arg("-C")
            .arg(&work)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
            .output()
            .unwrap();
        assert!(o.status.success(), "{args:?}: {}", String::from_utf8_lossy(&o.stderr));
    };
    std::fs::write(work.join("README.md"), "# Readme\nline\n").unwrap();
    std::fs::write(work.join("tracked.txt"), "one\n").unwrap();
    vcs(&["init", "-q"]);
    vcs(&["add", "."]);
    vcs(&["commit", "-q", "-m", "init"]);
    std::fs::write(work.join("leaky.md"), format!("the key: {KEY}\n")).unwrap();
    std::fs::write(t.dir.path().join("outside.txt"), "outside\n").unwrap();
    std::os::unix::fs::symlink(t.dir.path().join("outside.txt"), work.join("escape.md")).unwrap();
    let sid = t.start_chat(json!({})).await;
    let file = |path: &str| json!({ "session_id": sid, "path": path });

    // A document the agent wrote: text, Markdown, "new" in this chat.
    t.send(&sid, "write MIGRATION.md").await;
    t.wait(&sid, |v| v["type"] == "files.changed").await;
    t.wait(&sid, |v| v["type"] == "turn.done").await;
    let f = t.h.ok("chat.file", file("MIGRATION.md")).await;
    assert_eq!(f["text"], "# Notes\n");
    assert_eq!(f["mime"], "text/markdown");
    assert_eq!(f["path"], "MIGRATION.md");
    assert_eq!(f["changes"]["kind"], "new");
    // Absolute inside the folder works too.
    let abs = work.join("MIGRATION.md").display().to_string();
    assert_eq!(t.h.ok("chat.file", file(&abs)).await["text"], "# Notes\n");
    // Checkpoints (Phase 23): the folder was recorded before the turn, and
    // undoing it would take the agent's file away.
    t.c.send(json!({"t":"req","id":"cl","method":"checkpoint.list","params":{"chat": sid}}))
        .await;
    let cl = t.c.res("cl").await;
    assert_eq!(cl["result"]["checkpoints"][0]["kind"], "turn", "{cl}");
    let first = cl["result"]["checkpoints"][0]["id"].clone();
    t.c.send(json!({"t":"req","id":"cp","method":"checkpoint.preview","params":{"chat": sid, "id": first}}))
        .await;
    let cp = t.c.res("cp").await;
    assert_eq!(
        cp["result"]["files"],
        json!([{"path": "MIGRATION.md", "change": "removed"}]),
        "{cp}"
    );

    // An edit by a tool (allowed once): its diffs.
    t.send(&sid, "edit README.md").await;
    let item = t.need(&sid).await;
    let id = item["id"].as_str().unwrap().to_string();
    t.respond(&sid, &id, json!({ "kind": "allow_once" })).await;
    t.wait_type(&sid, "turn.done", 2).await;
    let f = t.h.ok("chat.file", file("README.md")).await;
    assert_eq!(f["changes"]["kind"], "tool", "{f}");
    assert_eq!(f["changes"]["diffs"][0]["oldText"], "# Readme\nline\n");

    // A shell command's file: the turn-end scan reports it (source watch); a tracked change shows the diff.
    std::fs::write(work.join("tracked.txt"), "one\ntwo\n").unwrap();
    t.send(&sid, "shellwrite report.txt").await;
    let items = t.wait_type(&sid, "needs.opened", 2).await;
    let id = items[1]["item"]["id"].as_str().unwrap().to_string();
    t.respond(&sid, &id, json!({ "kind": "allow_once" })).await;
    let w = t
        .wait(&sid, |v| {
            v["type"] == "files.changed" && v["files"][0]["source"] == "watch"
        })
        .await;
    let paths: Vec<&str> = w["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["path"].as_str().unwrap())
        .collect();
    let report = work.join("report.txt").display().to_string();
    assert!(paths.contains(&report.as_str()), "{w}");
    assert!(
        !paths.iter().any(|p| p.ends_with("README.md")),
        "a tool already said that one: {w}"
    );
    let f = t.h.ok("chat.file", file("tracked.txt")).await;
    assert_eq!(f["changes"]["kind"], "git");
    assert!(f["changes"]["patch"].as_str().unwrap().contains("+two"), "{f}");

    // Masked: a secret the chat's CLI can see never leaves the machine.
    let f = t.h.ok("chat.file", file("leaky.md")).await;
    let text = f["text"].as_str().unwrap();
    assert!(!text.contains(KEY) && text.contains("***"), "{text}");

    // Refused: secrets (even when the agent wrote them), outside, symlink escapes, junk.
    t.send(&sid, "write .env").await;
    t.wait_type(&sid, "turn.done", 4).await;
    for (p, code) in [
        (".env", "file_refused"),
        ("../outside.txt", "file_refused"),
        ("/etc/passwd", "file_refused"),
        ("escape.md", "file_refused"),
        (".git/config", "file_refused"),
        ("nope.md", "file_not_found"),
        ("", "invalid_path"),
    ] {
        let r = t.h.raw("chat.file", file(p)).await;
        assert_eq!(r["error"]["code"], code, "{p}: {r}");
        let s = r.to_string();
        assert!(
            !s.contains("outside\\n") && !s.contains("sk-ant"),
            "{p}: no content: {r}"
        );
    }
    let r = t.h.raw("chat.file", file(".env")).await;
    assert!(
        r["error"]["message"].as_str().unwrap().contains("may hold secrets"),
        "{r}"
    );
    let r =
        t.h.raw("chat.file", json!({ "session_id": "c-nope-nope-nope", "path": "a.md" }))
            .await;
    assert_eq!(r["error"]["code"], "chat_not_found");
}

/// How often `needle` is in the pane's recent text (soft wraps joined).
async fn count_text(h: &H, pane: &str, needle: &str) -> usize {
    let r = h
        .raw("pane.read", json!({"pane_id": pane, "source": "recent_unwrapped"}))
        .await;
    r["result"]["read"]["text"]
        .as_str()
        .unwrap_or("")
        .matches(needle)
        .count()
}

/// Type `cmd` into the shell a handoff pane leaves after its agent exits. That shell is a login
/// shell (`run_then_shell`), and this test box's /etc/profile resets PATH as Debian's does; the
/// hosted image puts the pane's PATH back in /etc/profile.d (provisioner/image/profile-tmuxedo.sh).
fn retype(cmd: &str) -> String {
    format!("export PATH=\"$TMUXEDO_PANE_PATH:$PATH\"; {cmd}\n")
}

/// Wait until `needle` is in the pane `n` times.
async fn wait_count(h: &H, pane: &str, needle: &str, n: usize) {
    let until = Instant::now() + Duration::from_secs(30);
    while count_text(h, pane, needle).await < n {
        if Instant::now() >= until {
            let r = h
                .raw("pane.read", json!({"pane_id": pane, "source": "recent_unwrapped"}))
                .await;
            panic!(
                "{needle:?} never appeared {n} times in {pane}: {}",
                r["result"]["read"]["text"]
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The next status frame for `pane` whose agent matches `pred` (Agent.chat, design §5.1).
async fn status_for(t: &mut Chat, pane: &str, pred: impl Fn(&Value) -> bool) -> Value {
    let p = pane.to_string();
    t.c.text(S, move |v| {
        v["t"] == "status" && v["paneId"] == p.as_str() && pred(&v["agent"])
    })
    .await["agent"]
        .clone()
}

/// The chat's session as the runner has it now.
async fn chat_status(t: &Chat, sid: &str) -> Value {
    t.h.ok("chat.get", json!({ "session_id": sid })).await["session"].clone()
}

/// `chat.handoff {pane_id, to: chat}` once the pane's agent is idle.
async fn open_as_chat(t: &Chat, pane: &str) -> Value {
    let until = Instant::now() + S;
    loop {
        let r = t.h.raw("chat.handoff", json!({ "pane_id": pane, "to": "chat" })).await;
        if r.get("result").is_some() {
            return r["result"].clone();
        }
        assert_eq!(r["error"]["code"], "agent_busy", "{r}");
        assert!(Instant::now() < until, "{r}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The owner, 2026-10-09: "if I exit claude when I have chat available, chat goes away … when I go back in
/// to claude is it possible for tmuxedo to detect that and bring the chat option back?" A chat that
/// continued in a terminal follows its session: exiting the agent there gives the chat back;
/// `--resume <id>` and `--continue` bind it to the pane again (its history kept); a new session
/// there is offered as a new chat; and every pane's agent says so (`Agent.chat`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_chat_follows_its_session_out_of_the_terminal_and_back() {
    let mut t = Chat::start(Setup {
        api_key: true,
        ..Default::default()
    })
    .await;
    let sid = t.start_chat(json!({ "title": "reattach" })).await;
    t.send(&sid, "hello").await;
    t.wait(&sid, |v| v["type"] == "turn.done").await;
    let vendor = chat_status(&t, &sid).await["vendorSessionId"]
        .as_str()
        .unwrap()
        .to_string();
    let r =
        t.h.ok("chat.handoff", json!({ "session_id": sid, "to": "terminal" }))
            .await;
    let pane = r["paneId"].as_str().unwrap().to_string();
    t.h.wait_text(&pane, &format!("FAKE TUI resumed {vendor}")).await;
    // The pane's agent says which chat holds its session.
    let s = sid.clone();
    status_for(&mut t, &pane, move |a| a["chat"]["id"] == s.as_str()).await;

    // 1. The agent exits in the terminal: the chat is back (it drives the session again).
    t.h.ok("pane.send_text", json!({ "pane_id": pane, "text": "bye\n" }))
        .await;
    let back = t.wait_type(&sid, "session.handoff", 2).await;
    assert_eq!(back[1]["to"], "chat");
    assert_eq!(back[1]["paneId"], pane.as_str());
    t.wait(&sid, |v| v["type"] == "notice" && v["code"] == "terminal_ended")
        .await;
    let st = chat_status(&t, &sid).await;
    assert_eq!(st["status"], "parked", "{st}");
    assert!(st["paneId"].is_null(), "{st}");

    // 2. `claude --resume <id>` in the same terminal: the chat follows it there, history kept.
    t.h.ok(
        "pane.send_text",
        json!({ "pane_id": pane, "text": retype(&format!("claude --resume {vendor}")) }),
    )
    .await;
    wait_count(&t.h, &pane, &format!("FAKE TUI resumed {vendor}"), 2).await;
    let again = t.wait_type(&sid, "session.handoff", 3).await;
    assert_eq!(again[2]["to"], "terminal");
    assert_eq!(again[2]["paneId"], pane.as_str());
    assert_eq!(chat_status(&t, &sid).await["status"], "terminal");
    let refused = t.h.raw("chat.send", json!({ "session_id": sid, "text": "hi" })).await;
    assert_eq!(refused["error"]["code"], "chat_in_terminal", "one driver at a time");
    assert!(
        t.events(&sid).iter().any(|v| v["type"] == "message.done"),
        "the history is still there"
    );

    // 3. Exit, then `claude --continue` (the last session, the same one): bound again.
    t.h.ok("pane.send_text", json!({ "pane_id": pane, "text": "bye\n" }))
        .await;
    t.wait_type(&sid, "session.handoff", 4).await;
    t.h.ok(
        "pane.send_text",
        json!({ "pane_id": pane, "text": retype("claude --continue") }),
    )
    .await;
    t.h.wait_text(&pane, &format!("FAKE TUI continued {vendor}")).await;
    let cont = t.wait_type(&sid, "session.handoff", 5).await;
    assert_eq!(cont[4]["to"], "terminal");
    let s = sid.clone();
    status_for(&mut t, &pane, move |a| a["chat"]["id"] == s.as_str()).await;

    // 4. Exit, then a brand-new `claude`: the old chat stays free; the pane offers a new chat.
    t.h.ok("pane.send_text", json!({ "pane_id": pane, "text": "bye\n" }))
        .await;
    t.wait_type(&sid, "session.handoff", 6).await;
    t.h.ok("pane.send_text", json!({ "pane_id": pane, "text": retype("claude") }))
        .await;
    let started = t.h.wait_text(&pane, "FAKE TUI started ").await;
    let fresh = started
        .split("FAKE TUI started ")
        .last()
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .to_string();
    let f = fresh.clone();
    let offer = status_for(&mut t, &pane, move |a| {
        a["sessionId"] == f.as_str() && a["chat"].is_object()
    })
    .await;
    assert!(
        offer["chat"].get("id").is_none(),
        "no chat holds the new session yet: {offer}"
    );
    assert_eq!(
        chat_status(&t, &sid).await["status"],
        "parked",
        "the old chat was not taken over"
    );
    // "Open as chat" on it: a new chat on the new session, not the old one.
    let opened = open_as_chat(&t, &pane).await;
    assert_ne!(opened["session"]["id"], sid.as_str());
    assert_eq!(opened["session"]["vendorSessionId"], fresh.as_str());

    // 5. The old chat's session reopened in a terminal while the chat drives it (two writers):
    // the chat pauses, says so, and the terminal has it.
    t.send(&sid, "slow 40").await;
    let until = Instant::now() + S;
    while chat_status(&t, &sid).await["status"] != "working" {
        assert!(Instant::now() < until, "{}", chat_status(&t, &sid).await);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(t.send(&sid, "queued while working").await["queued"], true);
    t.h.ok(
        "pane.send_text",
        json!({ "pane_id": "w1:p1", "text": format!("claude --resume {vendor}\n") }),
    )
    .await;
    let n = t
        .wait(&sid, |v| v["type"] == "notice" && v["code"] == "opened_elsewhere")
        .await;
    assert!(
        n["text"].as_str().unwrap().contains("1 waiting message was not sent"),
        "{n}"
    );
    let ho = t
        .wait(&sid, |v| v["type"] == "session.handoff" && v["paneId"] == "w1:p1")
        .await;
    assert_eq!(ho["to"], "terminal");
    t.wait(&sid, |v| v["type"] == "turn.done" && v["status"] == "interrupted")
        .await;
    let until = Instant::now() + S;
    while chat_status(&t, &sid).await["status"] != "terminal" {
        assert!(Instant::now() < until, "{}", chat_status(&t, &sid).await);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let refused = t.h.raw("chat.send", json!({ "session_id": sid, "text": "hi" })).await;
    assert_eq!(refused["error"]["code"], "chat_in_terminal");
    // "Open as chat" there continues the same chat (history), not a new one.
    let back = open_as_chat(&t, "w1:p1").await;
    assert_eq!(back["session"]["id"], sid.as_str());
    assert_contiguous(&t.events(&sid));
}

/// "Your organization has disabled Claude subscription access for Claude Code" is a login state:
/// `failed` with reason `org_disabled`, from a pane's screen (wrapped or not) and from a chat's failed
/// turn; a turn that works clears it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_organization_that_turned_claude_off_is_a_login_state() {
    let mut t = Chat::start(Setup {
        api_key: true,
        // Login tracking on by option: left to the runner it is on only if `claude --version` answers
        // within 5 s at start-up, and the fake (a Python script) does not on a loaded host, so no
        // pane screen would ever be read for a login message.
        tweak: Some(|o| o.claude_auth = Some(true)),
        ..Default::default()
    })
    .await;
    // On a pane's screen, wrapped over two lines as on a narrow terminal.
    t.h.ok(
        "pane.send_text",
        json!({ "pane_id": "w1:p1", "text": "printf 'Your organization has disabled Claude\\nsubscription access for Claude Code\\n'\n" }),
    )
    .await;
    let e = t.c.text(S, |v| v["t"] == "event" && v["type"] == "auth_failed").await;
    assert_eq!(e["data"]["reason"], "org_disabled", "{e}");
    assert_eq!(e["data"]["source"], "screen");
    // A chat turn that works clears it.
    let sid = t.start_chat(json!({})).await;
    t.send(&sid, "hello").await;
    t.wait(&sid, |v| v["type"] == "turn.done").await;
    let ok = t.c.text(S, |v| v["t"] == "event" && v["type"] == "auth_ok").await;
    assert_eq!(ok["data"]["reason"], "turn completed");
    // From the chat's own failed turn.
    t.send(&sid, "orgoff").await;
    let done = t.wait_type(&sid, "turn.done", 2).await;
    assert_eq!(done[1]["status"], "failed");
    let e = t.c.text(S, |v| v["t"] == "event" && v["type"] == "auth_failed").await;
    assert_eq!(e["data"]["reason"], "org_disabled", "{e}");
    assert_eq!(e["data"]["source"], "chat");
    // A reply that only quotes the words is not a login problem; a working turn clears it.
    t.send(&sid, "hello").await;
    t.wait_type(&sid, "turn.done", 3).await;
    t.c.text(S, |v| v["t"] == "event" && v["type"] == "auth_ok").await;
}

/// The start-up `--version` probe timing out must not turn login tracking off for good: a later
/// probe (the fake CLI is slow once, quick after) switches it on, and a login message on a pane's
/// screen is then a login state, as in the test above.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_version_probe_that_timed_out_at_start_is_retried_and_turns_login_tracking_on() {
    static SCRIPT: OnceLock<Vec<String>> = OnceLock::new();
    SCRIPT.get_or_init(|| {
        let d = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("tmuxedo-slow-once-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let script = d.join("claude-version");
        let marker = d.join("seen");
        let _ = std::fs::remove_file(&marker);
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nif [ ! -e '{m}' ]; then : > '{m}'; exec sleep 30; fi\necho '2.1.300 (fake)'\n",
                m = marker.display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        vec![script.display().to_string(), "--version".into()]
    });
    let mut t = Chat::start(Setup {
        api_key: true,
        tweak: Some(|o| {
            o.probe.limit = Duration::from_millis(500);
            o.probe.schedule = vec![Duration::from_millis(500)];
            o.probe.claude_argv = SCRIPT.get().cloned();
        }),
        ..Default::default()
    })
    .await;
    // The first probe timed out, so nothing is tracked yet; the retry (next slow tick) finds the version.
    let e =
        t.c.text(S, |v| v["t"] == "event" && v["data"]["source"] == "probe")
            .await;
    assert!(e["type"].as_str().unwrap().starts_with("auth_"), "{e}");
    t.h.ok(
        "pane.send_text",
        json!({ "pane_id": "w1:p1", "text": "printf 'Your organization has disabled Claude\\nsubscription access for Claude Code\\n'\n" }),
    )
    .await;
    let e = t.c.text(S, |v| v["t"] == "event" && v["type"] == "auth_failed").await;
    assert_eq!(e["data"]["reason"], "org_disabled", "{e}");
    assert_eq!(e["data"]["source"], "screen");
}

/// D-C4 "guarded (b)" (the owner, 2026-10-10): a subscription login chats while someone watches, but
/// Auto-approve with nobody watching needs an API key, unless web exempts the owner (admins).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unattended_auto_approve_on_a_subscription_needs_a_key_unless_exempt() {
    let mut t = Chat::start(Setup {
        policy: Some("allow"),
        auto: Some("allow"),
        tweak: Some(|o| o.chat_unwatched_grace = Duration::from_secs(1)),
        ..Default::default()
    })
    .await;
    t.c.send(
        json!({ "t": "config", "chat": { "subscription": "allow", "autoApprove": "allow", "unattended": "deny" } }),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let sid = t.start_chat(json!({})).await;
    let mode = |m: &str| json!({ "session_id": sid, "mode": m, "fresh_stepup": true });
    // Watching: allowed.
    t.h.ok("chat.mode", mode("auto_approve")).await;
    // Nobody watches past the grace: back to Ask first, with the reason.
    t.c.send(json!({ "t": "chat_detach", "sessionId": sid })).await;
    let until = Instant::now() + S;
    while t.h.ok("chat.get", json!({ "session_id": sid })).await["session"]["mode"] != "ask_first" {
        assert!(Instant::now() < until, "never went back to Ask first");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let refused = t.h.raw("chat.mode", mode("auto_approve")).await;
    assert_eq!(refused["error"]["code"], "unattended_needs_api_key", "{refused}");
    // Watching again (the history replays the reason): allowed again.
    t.attach(&sid, 1).await;
    let n = t
        .wait(&sid, |v| {
            v["type"] == "notice" && v["code"] == "unattended_needs_api_key"
        })
        .await;
    assert!(n["text"].as_str().unwrap().contains("API key"), "{n}");
    t.wait(&sid, |v| v["type"] == "session.mode" && v["mode"] == "ask_first")
        .await;
    t.h.ok("chat.mode", mode("auto_approve")).await;

    // An exempt owner (an admin): unattended is allowed.
    t.c.send(
        json!({ "t": "config", "chat": { "subscription": "allow", "autoApprove": "allow", "unattended": "allow" } }),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    t.c.send(json!({ "t": "chat_detach", "sessionId": sid })).await;
    tokio::time::sleep(Duration::from_secs(7)).await;
    let s = t.h.ok("chat.get", json!({ "session_id": sid })).await;
    assert_eq!(s["session"]["mode"], "auto_approve", "{s}");
    let notices = t
        .events(&sid)
        .into_iter()
        .filter(|v| v["code"] == "unattended_needs_api_key")
        .count();
    assert_eq!(notices, 1);
}

/// The tool-call facts of docs/design/chat-tools.md §14, end to end: what a
/// call is, how much it gave back and which end was kept, how it ended, how
/// long a thought took; and the 30-day log holds exactly those events.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tool_events_say_what_ran_what_came_back_and_how_it_ended() {
    let mut t = Chat::start(Setup {
        api_key: true,
        ..Default::default()
    })
    .await;
    let sid = t.start_chat(json!({})).await;
    let turn = |n: usize| move |v: &Value| v["type"] == "turn.done" && v["turnId"] == format!("t{n}").as_str();

    // A file search with the CLI's structured result, and an MCP tool.
    t.send(&sid, "glob").await;
    t.wait(&sid, turn(1)).await;
    let ev = t.events(&sid);
    let started: Vec<&Value> = ev.iter().filter(|v| v["type"] == "tool.started").collect();
    assert_eq!(
        (
            &started[0]["kind"],
            &started[0]["name"],
            &started[0]["subject"],
            &started[0]["variant"]
        ),
        (&json!("search"), &json!("Glob"), &json!("**/*.test.ts"), &json!("glob"))
    );
    assert_eq!(
        started[0]["title"], "Looked for `**/*.test.ts`",
        "the old line stays for old readers"
    );
    assert_eq!(
        (
            &started[1]["kind"],
            &started[1]["name"],
            &started[1]["server"],
            &started[1]["variant"]
        ),
        (
            &json!("other"),
            &json!("create_issue"),
            &json!("tracker"),
            &json!("mcp")
        )
    );
    assert!(started[1].get("subject").is_none());
    let done: Vec<&Value> = ev.iter().filter(|v| v["type"] == "tool.done").collect();
    assert_eq!(
        (&done[0]["resultCount"], &done[0]["durationMs"]),
        (&json!(2), &json!(31))
    );
    assert_eq!(done[0]["outputBytes"], 19);
    assert!(done[0].get("outputKept").is_none(), "nothing was cut");
    assert!(done[1].get("resultCount").is_none() && done[1].get("durationMs").is_none());

    // A long read: the start of the file is kept, and the event says so.
    t.send(&sid, "readbig").await;
    t.wait(&sid, turn(2)).await;
    let read = t
        .wait(&sid, |v| v["type"] == "tool.done" && v["outputKept"] == "head")
        .await;
    let out = read["output"].as_str().unwrap();
    assert!(out.starts_with("HEADxxx") && !out.contains("END"), "the head only");
    assert!(out.len() <= MAX_TOOL_OUTPUT + 8);
    assert_eq!(read["outputBytes"], 600 * 1024 + 7);
    let started = t
        .wait(&sid, |v| v["type"] == "tool.started" && v["name"] == "Read")
        .await;
    assert_eq!(started["locations"][0]["line"], 40);
    assert!(started["subject"].as_str().unwrap().ends_with("/work/big.txt"));

    // A command that failed: its exit code.
    t.send(&sid, "fail").await;
    t.wait(&sid, turn(3)).await;
    let failed = t
        .wait(&sid, |v| v["type"] == "tool.done" && v["status"] == "failed")
        .await;
    assert_eq!(
        (&failed["exitCode"], &failed["summary"]),
        (&json!(1), &json!("Exit code 1…"))
    );

    // An image in a result: what it is and its size, never its bytes.
    t.send(&sid, "image").await;
    t.wait(&sid, turn(4)).await;
    let img = t
        .wait(&sid, |v| v["type"] == "tool.done" && v.get("attachments").is_some())
        .await;
    assert_eq!(img["attachments"], json!([{ "mime": "image/png", "bytes": 3000 }]));
    assert_eq!(img["output"], "a screenshot\n[image]");

    // A new file is a creation; writing it again is not. (The file appears only once the
    // person allows the write, so the runner has seen the call before the file exists.)
    for n in 1..=2 {
        t.send(&sid, "create made.md").await;
        let writes = t.wait_type(&sid, "tool.started", 5 + n).await;
        let needs = t.wait_type(&sid, "needs.opened", n).await;
        let id = needs[n - 1]["item"]["id"].as_str().unwrap().to_string();
        t.respond(&sid, &id, json!({ "kind": "allow_once" })).await;
        t.wait(&sid, turn(4 + n)).await;
        let w = writes.last().unwrap();
        assert_eq!(w["name"], "Write");
        if n == 1 {
            assert_eq!(w["variant"], "create");
        } else {
            assert!(w.get("variant").is_none(), "{w}");
        }
    }

    // Thoughts: the time between the block's start and stop; a hidden one says it is hidden.
    t.send(&sid, "think").await;
    t.wait(&sid, turn(7)).await;
    let thoughts: Vec<Value> = t
        .events(&sid)
        .into_iter()
        .filter(|v| v["type"] == "thought.done")
        .collect();
    assert_eq!(thoughts.len(), 2);
    assert_eq!(thoughts[0]["text"], "Check the tests first.");
    let ms = thoughts[0]["durationMs"].as_u64().unwrap();
    // The CLI's stream lines carry no timestamps, so the runner times a thought from when it
    // reads the block's start and stop lines. Under load it can read both together and report
    // 0 ms: only that the duration is present and sane is guaranteed.
    assert!(ms < 10_000, "a duration, not a runaway clock: {ms}");
    assert!(thoughts[0].get("redacted").is_none());
    assert_eq!(
        (&thoughts[1]["text"], &thoughts[1]["redacted"]),
        (&json!(""), &json!(true))
    );
    assert!(thoughts[1]["durationMs"].as_u64().is_some());

    // The log on the machine is these events and nothing more: line for line what was sent
    // and what `chat.read` gives, with the facts in it and no image bytes.
    let log = std::fs::read_to_string(t.dir.path().join(format!("state/chat/{sid}/events.ndjson"))).unwrap();
    let logged: Vec<Value> = log
        .lines()
        .map(|l| {
            let mut v: Value = serde_json::from_str(l).unwrap();
            // The log's own note of where in the agent's output the event came from.
            v.as_object_mut().unwrap().remove("o");
            v
        })
        .collect();
    let sent = t.events(&sid);
    assert_eq!(logged, sent, "the log holds exactly the events");
    let read = t.h.ok("chat.read", json!({ "session_id": sid })).await;
    assert_eq!(read["events"].as_array().unwrap(), &sent);
    for fact in [
        "\"subject\":\"**/*.test.ts\"",
        "\"outputKept\":\"head\"",
        "\"outputBytes\":614407",
        "\"exitCode\":1",
        "\"resultCount\":2",
        "\"attachments\":[{",
        "\"redacted\":true",
        "\"variant\":\"create\"",
    ] {
        assert!(log.contains(fact), "{fact} is not in the log");
    }
    assert!(!log.contains("QUJDQUJD"), "an image's bytes in the log");
    assert!(
        log.len() < 200 * 1024,
        "the log keeps the cut, not the output: {}",
        log.len()
    );
    assert_contiguous(&sent);
}

/// A call that was refused is `denied`; a call still open when its turn is
/// interrupted is `cancelled` (also when the agent exits without a word).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn refused_calls_are_denied_and_interrupted_calls_are_cancelled() {
    let mut t = Chat::start(Setup {
        api_key: true,
        ..Default::default()
    })
    .await;
    let sid = t.start_chat(json!({})).await;

    // The hook names the call (`tool_use_id`): that call is the one denied, whatever its input.
    t.send(&sid, "toolid").await;
    let id = t.need(&sid).await["id"].as_str().unwrap().to_string();
    t.respond(&sid, &id, json!({ "kind": "deny" })).await;
    let done = t.wait(&sid, |v| v["type"] == "tool.done").await;
    assert_eq!(done["status"], "denied", "{done}");
    assert!(done.get("exitCode").is_none());
    t.wait(&sid, |v| v["type"] == "turn.done").await;
    // Allowed, the same call completes.
    t.send(&sid, "toolid").await;
    let n = t.wait_type(&sid, "needs.opened", 2).await;
    let id = n[1]["item"]["id"].as_str().unwrap().to_string();
    t.respond(&sid, &id, json!({ "kind": "allow_once" })).await;
    let done = t.wait_type(&sid, "tool.done", 2).await;
    assert_eq!(done[1]["status"], "completed");
    t.wait_type(&sid, "turn.done", 2).await;

    // Three calls open (one of them a helper's) when the person stops the turn.
    for (n, word) in [(3, "hang"), (4, "hangexit")] {
        t.send(&sid, word).await;
        let turn = format!("t{n}");
        let mine = |v: &Value| v["turnId"] == turn.as_str();
        t.c.chats_until(S, |fs| {
            fs.iter()
                .filter(|f| {
                    serde_json::from_slice::<Value>(&f.payload).is_ok_and(|v| v["type"] == "tool.started" && mine(&v))
                })
                .count()
                >= 3
        })
        .await;
        let r = t.h.ok("chat.interrupt", json!({ "session_id": sid })).await;
        assert_eq!(r["interrupted"], true);
        let end = t.wait(&sid, |v| v["type"] == "turn.done" && mine(v)).await;
        assert_eq!(end["status"], "interrupted", "{word}: {end}");
        let ev: Vec<Value> = t.events(&sid).into_iter().filter(|v| mine(v)).collect();
        let started: Vec<&Value> = ev.iter().filter(|v| v["type"] == "tool.started").collect();
        let done: Vec<&Value> = ev.iter().filter(|v| v["type"] == "tool.done").collect();
        assert_eq!(done.len(), 3, "{word}: every open call is closed: {done:?}");
        for d in &done {
            assert_eq!(d["status"], "cancelled", "{word}");
            assert!(d["seq"].as_u64() < end["seq"].as_u64(), "{word}: before the turn's end");
            // Each under the helper it was started under.
            let s = started.iter().find(|s| s["toolId"] == d["toolId"]).unwrap();
            assert_eq!(s.get("agentPath"), d.get("agentPath"), "{word}");
        }
        assert_eq!(
            done.iter().filter(|d| d.get("agentPath").is_some()).count(),
            1,
            "{word}"
        );
        assert_eq!(started.iter().filter(|s| s["variant"] == "agent").count(), 1);
    }
    // The chat goes on (the agent that exited is started again on the same session).
    t.send(&sid, "hello").await;
    let hello = t
        .wait(&sid, |v| v["type"] == "message.done" && v["text"] == "Hello there")
        .await;
    assert_eq!(hello["turnId"], "t5");
    // Nothing is cancelled twice, and a turn that just ends cancels nothing.
    t.wait(&sid, |v| v["type"] == "turn.done" && v["turnId"] == "t5").await;
    let all = t.events(&sid);
    assert_eq!(all.iter().filter(|v| v["status"] == "cancelled").count(), 6);
    assert_contiguous(&all);
}

/// Terminal control sequences in what a tool took and gave back never reach
/// an event, and they go before masking: a secret with a colour code in its
/// middle is masked whole (docs/design/chat-tools.md §15).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn control_sequences_are_stripped_from_tool_text_before_masking() {
    let mut t = Chat::start(Setup {
        api_key: true,
        ..Default::default()
    })
    .await;
    let sid = t.start_chat(json!({})).await;
    t.send(&sid, "ansi").await;
    t.wait(&sid, |v| v["type"] == "turn.done").await;
    let started = t.wait(&sid, |v| v["type"] == "tool.started").await;
    assert_eq!(started["subject"], "printf 'red'⏎echo ***");
    assert_eq!(started["input"]["command"], "printf 'red'\necho ***");
    assert_eq!(started["title"], "Ran `printf 'red'…`");
    let done = t.wait(&sid, |v| v["type"] == "tool.done").await;
    assert_eq!(done["output"], "red\n***\n50%\n100%\nclick\n");
    assert!(
        done["outputBytes"].as_u64().unwrap() > 100,
        "the size is of what the tool gave"
    );

    let log = std::fs::read_to_string(t.dir.path().join(format!("state/chat/{sid}/events.ndjson"))).unwrap();
    let frames: String =
        t.c.chats
            .iter()
            .map(|f| String::from_utf8_lossy(&f.payload).into_owned())
            .collect();
    let read = t.h.ok("chat.read", json!({ "session_id": sid })).await.to_string();
    let half = KEY.len() / 2;
    for (what, hay) in [("log", &log), ("frames", &frames), ("read", &read)] {
        for (name, bad) in [
            ("an escape", "\\u001b"),
            ("a raw escape", "\u{1b}"),
            ("a bell", "\\u0007"),
            ("a carriage return", "\\r"),
            ("the title a tool tried to set", "✓ done"),
            ("the link a tool tried to make", "evil.example"),
            ("the key", KEY),
            ("the first half of the key", &KEY[..half]),
            ("the second half of the key", &KEY[half..]),
        ] {
            assert!(!hay.contains(bad), "{name} in the {what}");
        }
    }
    assert!(log.contains("***"), "masked, not dropped");
}

// ── Codex through its app-server (Phase 21 C4; tests/fixtures/fake_codex.py) ──

impl Chat {
    async fn codex() -> (Chat, String) {
        let mut t = Chat::start(Setup {
            openai: true,
            ..Default::default()
        })
        .await;
        let sid = t.start_chat(json!({ "agent": "codex" })).await;
        (t, sid)
    }

    /// What the fake app-server read so far: requests, notifications and answers.
    fn rpc_log(&self) -> Vec<Value> {
        std::fs::read_to_string(self.log_dir().join("codex-rpc.log"))
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn codex_starts(&self) -> usize {
        std::fs::read_to_string(self.log_dir().join("codex-argv.log"))
            .unwrap_or_default()
            .lines()
            .count()
    }

    async fn turn_done(&mut self, sid: &str, n: usize) -> Value {
        self.wait_type(sid, "turn.done", n).await.last().unwrap().clone()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn codex_streams_an_answer_on_a_thread_it_names() {
    let (mut t, sid) = Chat::codex().await;
    t.send(&sid, "hello").await;
    assert_eq!(t.turn_done(&sid, 1).await["status"], "completed");
    let ev = t.events(&sid);
    assert_contiguous(&ev);
    let types: Vec<&str> = ev.iter().map(|v| v["type"].as_str().unwrap()).collect();
    assert_eq!(
        types,
        [
            "session.opened",
            "message.user",
            "turn.started",
            "message.done",
            "turn.done"
        ]
    );
    assert_eq!(ev[0]["plugin"], "codex");
    assert_eq!(ev[0]["login"], "api_key");
    assert_eq!(ev[0]["mode"], "ask_first");
    assert!(ev[0].get("resumedFrom").is_none(), "{}", ev[0]);
    assert_eq!(ev[3]["text"], "Hello from Codex");
    // The deltas were relayed live with the message's id, and are not in the log.
    let deltas: Vec<Value> =
        t.c.chats
            .iter()
            .filter(|f| f.session_id == sid && f.ephemeral)
            .map(|f| serde_json::from_slice(&f.payload).unwrap())
            .collect();
    assert_eq!(deltas.len(), 3);
    assert!(deltas.iter().all(|d| d["messageId"] == ev[3]["messageId"]));
    // Started from the manifest's argv, in the session's folder, with the handshake in order.
    assert_eq!(
        std::fs::read_to_string(t.log_dir().join("codex-argv.log"))
            .unwrap()
            .trim(),
        r#"["app-server"]"#
    );
    let rpc = t.rpc_log();
    let methods: Vec<&str> = rpc.iter().map(|m| m["method"].as_str().unwrap_or("")).collect();
    assert_eq!(methods, ["initialize", "initialized", "thread/start", "turn/start"]);
    assert_eq!(rpc[0]["params"]["clientInfo"]["name"], "tmuxedo");
    let work = t.dir.path().join("work").canonicalize().unwrap();
    assert_eq!(rpc[2]["params"]["cwd"], work.display().to_string());
    assert_eq!(rpc[2]["params"]["approvalPolicy"], "untrusted");
    // The chat is named after the agent's own thread.
    let info = chat_status(&t, &sid).await;
    let thread = info["vendorSessionId"].as_str().unwrap().to_string();
    assert!(thread.starts_with("thr-"), "{info}");
    assert_eq!(rpc[3]["params"]["threadId"], thread.as_str());
    assert_eq!(info["status"], "idle");
    // A second message: the same process and thread, no second handshake.
    t.send(&sid, "think").await;
    assert_eq!(t.turn_done(&sid, 2).await["status"], "completed");
    let thought = t.wait(&sid, |v| v["type"] == "thought.done").await;
    assert_eq!(thought["text"], "Weighing the options.");
    assert_eq!(t.codex_starts(), 1);
    assert_eq!(t.rpc_log().len(), 5);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn codex_approvals_are_answered_by_request_id() {
    let (mut t, sid) = Chat::codex().await;
    // 1. Approved once: the command runs and its output comes back.
    t.send(&sid, "tool").await;
    let item = t.need(&sid).await;
    assert_eq!(item["kind"], "permission");
    assert_eq!(item["risk"], "normal");
    assert_eq!(item["title"], "Run `echo hi`");
    assert_eq!(item["tool"], "Bash");
    assert_eq!(item["options"], json!(["allow_once", "allow_session", "deny"]));
    assert_eq!(item["input"]["command"], "echo hi");
    let started = t.wait(&sid, |v| v["type"] == "tool.started").await;
    assert_eq!(
        (&started["kind"], &started["name"], &started["subject"]),
        (&json!("execute"), &json!("Bash"), &json!("echo hi"))
    );
    assert_eq!(chat_status(&t, &sid).await["status"], "needs_you");
    let id = item["id"].as_str().unwrap().to_string();
    // An answer that is not one of the item's is refused; the item stays open.
    let bad = t.respond(&sid, &id, json!({ "kind": "approve_plan" })).await;
    assert_eq!(bad["error"]["code"], "invalid_response", "{bad}");
    let r = t.respond(&sid, &id, json!({ "kind": "allow_once" })).await;
    assert_eq!(r["result"]["outcome"], "accepted", "{r}");
    let done = t.wait(&sid, |v| v["type"] == "tool.done").await;
    assert_eq!(done["toolId"], started["toolId"]);
    assert_eq!(
        (&done["status"], &done["output"], &done["durationMs"]),
        (&json!("completed"), &json!("hi\n"), &json!(12))
    );
    assert_eq!(t.turn_done(&sid, 1).await["status"], "completed");
    // Answered already: stale, and nothing more is written to the agent.
    let again = t.respond(&sid, &id, json!({ "kind": "deny" })).await;
    assert_eq!(again["result"]["outcome"], "stale");

    // 2. Denied: the call is `denied` (not failed) and the turn goes on.
    t.send(&sid, "tool").await;
    let item = t.wait_type(&sid, "needs.opened", 2).await[1]["item"].clone();
    let r = t
        .respond(&sid, item["id"].as_str().unwrap(), json!({ "kind": "deny" }))
        .await;
    assert_eq!(r["result"]["outcome"], "accepted", "{r}");
    assert_eq!(t.turn_done(&sid, 2).await["status"], "completed");
    let dones = t.wait_type(&sid, "tool.done", 2).await;
    assert_eq!(dones[1]["status"], "denied");

    // 3. For this session: the server's own word for it.
    t.send(&sid, "tool").await;
    let item = t.wait_type(&sid, "needs.opened", 3).await[2]["item"].clone();
    t.respond(&sid, item["id"].as_str().unwrap(), json!({ "kind": "allow_session" }))
        .await;
    assert_eq!(t.turn_done(&sid, 3).await["status"], "completed");

    // 4. High risk: Allow once / Deny only, and a fresh step-up to allow.
    t.send(&sid, "risky").await;
    let item = t.wait_type(&sid, "needs.opened", 4).await[3]["item"].clone();
    assert_eq!(item["risk"], "high");
    assert_eq!(item["options"], json!(["allow_once", "deny"]));
    let nid = item["id"].as_str().unwrap();
    let r = t.respond(&sid, nid, json!({ "kind": "allow_once" })).await;
    assert_eq!(r["error"]["code"], "stepup_required", "{r}");
    let r =
        t.h.raw(
            "chat.respond",
            json!({ "session_id": sid, "need_id": nid, "response": { "kind": "allow_once" }, "fresh_stepup": true }),
        )
        .await;
    assert_eq!(r["result"]["outcome"], "accepted", "{r}");
    assert_eq!(t.turn_done(&sid, 4).await["status"], "completed");

    // 5. A file change: asked with its path, and reported once it happened.
    t.send(&sid, "edit made.txt").await;
    let item = t.wait_type(&sid, "needs.opened", 5).await[4]["item"].clone();
    let path = t.dir.path().join("work").canonicalize().unwrap().join("made.txt");
    assert_eq!(item["title"], format!("Edit `{}`", path.display()));
    assert_eq!(item["why"], "write outside the sandbox");
    t.respond(&sid, item["id"].as_str().unwrap(), json!({ "kind": "allow_once" }))
        .await;
    assert_eq!(t.turn_done(&sid, 5).await["status"], "completed");
    let changed = t.wait(&sid, |v| v["type"] == "files.changed").await;
    assert_eq!(changed["files"][0]["change"], "created");
    assert!(path.exists());

    // Each decision went back once, on the id of the request that asked.
    let answers: Vec<Value> = t.rpc_log().into_iter().filter(|m| m.get("method").is_none()).collect();
    let said: Vec<(u64, &str)> = answers
        .iter()
        .map(|m| (m["id"].as_u64().unwrap(), m["result"]["decision"].as_str().unwrap()))
        .collect();
    assert_eq!(
        said,
        [
            (101, "accept"),
            (102, "decline"),
            (103, "acceptForSession"),
            (104, "accept"),
            (105, "accept")
        ]
    );
    let resolved = t.wait_type(&sid, "needs.resolved", 5).await;
    assert!(resolved
        .iter()
        .all(|r| r["outcome"] == "answered" && r["by"] == "test" || r["by"].is_null()));
    assert_contiguous(&t.events(&sid));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn codex_interrupt_ends_the_turn_and_the_agent_stays() {
    let (mut t, sid) = Chat::codex().await;
    // A running command and an open approval: both end with the turn.
    t.send(&sid, "tool").await;
    let item = t.need(&sid).await;
    let r = t.h.raw("chat.interrupt", json!({ "session_id": sid })).await;
    assert_eq!(r["result"]["interrupted"], true, "{r}");
    assert_eq!(t.turn_done(&sid, 1).await["status"], "interrupted");
    let resolved = t.wait(&sid, |v| v["type"] == "needs.resolved").await;
    assert_eq!(
        (&resolved["id"], &resolved["outcome"]),
        (&item["id"], &json!("cancelled"))
    );
    let done = t.wait(&sid, |v| v["type"] == "tool.done").await;
    assert_eq!(done["status"], "cancelled");
    let stale = t
        .respond(&sid, item["id"].as_str().unwrap(), json!({ "kind": "allow_once" }))
        .await;
    assert_eq!(stale["result"]["outcome"], "stale");
    // Mid-stream, with a message queued behind it.
    t.send(&sid, "slow 40").await;
    t.c.chats_until(S, |fs| fs.iter().any(|f| f.ephemeral)).await;
    let queued = t.send(&sid, "hello").await;
    assert_eq!(queued["queued"], true, "{queued}");
    t.h.raw("chat.interrupt", json!({ "session_id": sid })).await;
    assert_eq!(t.turn_done(&sid, 2).await["status"], "interrupted");
    assert_eq!(t.turn_done(&sid, 3).await["status"], "completed");
    let last = t.wait_type(&sid, "message.done", 1).await;
    assert_eq!(last.last().unwrap()["text"], "Hello from Codex");
    // The app-server was asked, not signalled: it is the same process still.
    assert_eq!(t.codex_starts(), 1);
    let rpc = t.rpc_log();
    assert_eq!(rpc.iter().filter(|m| m["method"] == "turn/interrupt").count(), 2);
    assert!(rpc
        .iter()
        .any(|m| m["id"] == 101 && m["result"]["decision"] == "cancel"));
    assert!(!t.events(&sid).iter().any(|v| v["type"] == "notice"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn codex_failed_turns_bad_frames_and_a_child_that_dies_mid_turn() {
    let (mut t, sid) = Chat::codex().await;
    // 1. A turn the agent fails: its words, and the chat goes on.
    t.send(&sid, "fail").await;
    let done = t.turn_done(&sid, 1).await;
    assert_eq!(
        (&done["status"], &done["error"]),
        (&json!("failed"), &json!("model overloaded"))
    );
    // 2. Lines that are not the protocol: each is dropped with a notice, none stops the turn.
    t.send(&sid, "junk").await;
    assert_eq!(t.turn_done(&sid, 2).await["status"], "completed");
    let ev = t.events(&sid);
    let mut codes: Vec<&str> = ev
        .iter()
        .filter(|v| v["type"] == "notice")
        .map(|v| v["code"].as_str().unwrap())
        .collect();
    // The 5 MiB line is told once however many reads it came in (the holder's ring may add one).
    codes.dedup();
    assert_eq!(
        codes,
        [
            "agent_error",
            "agent_output",
            "unknown_event",
            "answer_in_terminal",
            "output_lost",
            "unknown_event"
        ]
    );
    assert!(ev
        .iter()
        .any(|v| v["type"] == "message.done" && v["text"] == "after junk"));
    assert!(ev.iter().all(|v| v.to_string().len() < MAX_EVENT_BYTES));
    // The question it cannot carry was refused on its own id.
    assert!(t
        .rpc_log()
        .iter()
        .any(|m| m["id"] == "q-1" && m["error"]["code"] == -32601));
    let thread = chat_status(&t, &sid).await["vendorSessionId"]
        .as_str()
        .unwrap()
        .to_string();
    // 3. The process dies mid-turn: the turn fails with its last words.
    t.send(&sid, "exit").await;
    let done = t.turn_done(&sid, 3).await;
    assert_eq!(done["status"], "failed");
    assert!(done["error"].as_str().unwrap().contains("lost its mind"), "{done}");
    let n = t
        .wait(&sid, |v| v["type"] == "notice" && v["code"] == "agent_exited")
        .await;
    assert!(n["text"].as_str().unwrap().contains("Send a message to continue"));
    assert_eq!(chat_status(&t, &sid).await["status"], "parked");
    // 4. The next message starts it again and reopens the same thread.
    t.send(&sid, "hello").await;
    assert_eq!(t.turn_done(&sid, 4).await["status"], "completed");
    assert_eq!(t.codex_starts(), 2);
    let rpc = t.rpc_log();
    let resume = rpc.iter().find(|m| m["method"] == "thread/resume").expect("resumed");
    assert_eq!(resume["params"]["threadId"], thread.as_str());
    assert_eq!(rpc.iter().filter(|m| m["method"] == "thread/start").count(), 1);
    let opened = t.wait_type(&sid, "session.opened", 2).await;
    assert_eq!(opened[1]["resumedFrom"], thread.as_str());
    assert_eq!(chat_status(&t, &sid).await["vendorSessionId"], thread.as_str());
    assert_contiguous(&t.events(&sid));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn codex_chat_follows_the_login_kind() {
    // A ChatGPT sign-in (no key in the env): refused unless web's policy allows it.
    let t = Chat::start(Setup::default()).await;
    let r = t.h.raw("chat.start", json!({ "agent": "codex" })).await;
    assert_eq!(r["error"]["code"], "chat_login_not_allowed", "{r}");
    let mut t = Chat::start(Setup {
        policy: Some("allow"),
        ..Default::default()
    })
    .await;
    let sid = t.start_chat(json!({ "agent": "codex" })).await;
    t.send(&sid, "hello").await;
    assert_eq!(t.turn_done(&sid, 1).await["status"], "completed");
    assert_eq!(t.events(&sid)[0]["login"], "subscription");
    // A mode the manifest does not map is refused before anything starts.
    let r =
        t.h.raw("chat.start", json!({ "agent": "codex", "mode": "plan_first" }))
            .await;
    assert_eq!(r["error"]["code"], "invalid_mode", "{r}");
}

// ── ACP agents through the generic translator (Phase 21 C4; tests/fixtures/fake_acp.py,
// started as Cursor's `agent acp`) ──

impl Chat {
    async fn acp(s: Setup) -> (Chat, String) {
        let mut t = Chat::start(Setup { cursor: true, ..s }).await;
        let sid = t.start_chat(json!({ "agent": "cursor" })).await;
        (t, sid)
    }

    /// What the fake ACP agent read so far: requests, notifications and answers.
    fn acp_log(&self) -> Vec<Value> {
        std::fs::read_to_string(self.log_dir().join("acp-rpc.log"))
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn acp_starts(&self) -> usize {
        std::fs::read_to_string(self.log_dir().join("acp-argv.log"))
            .unwrap_or_default()
            .lines()
            .count()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acp_streams_an_answer_on_a_session_it_names() {
    let (mut t, sid) = Chat::acp(Setup::default()).await;
    t.send(&sid, "hello").await;
    assert_eq!(t.turn_done(&sid, 1).await["status"], "completed");
    let ev = t.events(&sid);
    assert_contiguous(&ev);
    let types: Vec<&str> = ev.iter().map(|v| v["type"].as_str().unwrap()).collect();
    assert_eq!(
        types,
        [
            "session.opened",
            "message.user",
            "turn.started",
            "message.done",
            "turn.done"
        ]
    );
    assert_eq!(ev[0]["plugin"], "cursor");
    assert_eq!(ev[0]["login"], "api_key");
    assert_eq!(ev[3]["text"], "Hello from ACP");
    let deltas: Vec<Value> =
        t.c.chats
            .iter()
            .filter(|f| f.session_id == sid && f.ephemeral)
            .map(|f| serde_json::from_slice(&f.payload).unwrap())
            .collect();
    assert_eq!(deltas.len(), 3);
    assert!(deltas.iter().all(|d| d["messageId"] == ev[3]["messageId"]));
    // Started from the manifest's argv, in the session's folder; JSON-RPC 2.0 on every line.
    assert_eq!(
        std::fs::read_to_string(t.log_dir().join("acp-argv.log"))
            .unwrap()
            .trim(),
        r#"["acp"]"#
    );
    let rpc = t.acp_log();
    let methods: Vec<&str> = rpc.iter().map(|m| m["method"].as_str().unwrap_or("")).collect();
    assert_eq!(
        methods,
        ["initialize", "session/new", "session/set_mode", "session/prompt"]
    );
    assert!(rpc.iter().all(|m| m["jsonrpc"] == "2.0"), "{rpc:?}");
    let work = t.dir.path().join("work").canonicalize().unwrap();
    assert_eq!(rpc[1]["params"]["cwd"], work.display().to_string());
    assert_eq!(rpc[2]["params"]["modeId"], "agent");
    let info = chat_status(&t, &sid).await;
    let session = info["vendorSessionId"].as_str().unwrap().to_string();
    assert!(session.starts_with("acp-sess-"), "{info}");
    assert_eq!(rpc[3]["params"]["sessionId"], session.as_str());
    assert_eq!(info["status"], "idle");
    // More turns on the same process: a thought, a plan.
    t.send(&sid, "think").await;
    assert_eq!(t.turn_done(&sid, 2).await["status"], "completed");
    let thought = t.wait(&sid, |v| v["type"] == "thought.done").await;
    assert_eq!(thought["text"], "Weighing the options.");
    t.send(&sid, "plan").await;
    assert_eq!(t.turn_done(&sid, 3).await["status"], "completed");
    let plan = t.wait(&sid, |v| v["type"] == "plan.updated").await;
    assert_eq!(
        plan["entries"],
        json!([
            { "text": "Read the code", "status": "done" },
            { "text": "Change it", "status": "in_progress" },
            { "text": "Test it", "status": "pending" }
        ])
    );
    assert_eq!(t.acp_starts(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acp_permissions_are_answered_with_the_agents_options() {
    let (mut t, sid) = Chat::acp(Setup::default()).await;
    // 1. Allowed once: the command runs and its output comes back.
    t.send(&sid, "tool").await;
    let item = t.need(&sid).await;
    assert_eq!(
        (&item["kind"], &item["risk"], &item["title"], &item["tool"]),
        (
            &json!("permission"),
            &json!("normal"),
            &json!("Run `echo hi`"),
            &json!("Bash")
        )
    );
    assert_eq!(item["options"], json!(["allow_once", "allow_session", "deny"]));
    let started = t.wait(&sid, |v| v["type"] == "tool.started").await;
    assert_eq!(
        (&started["kind"], &started["name"], &started["subject"]),
        (&json!("execute"), &json!("Bash"), &json!("echo hi"))
    );
    assert_eq!(chat_status(&t, &sid).await["status"], "needs_you");
    let id = item["id"].as_str().unwrap().to_string();
    let r = t.respond(&sid, &id, json!({ "kind": "allow_once" })).await;
    assert_eq!(r["result"]["outcome"], "accepted", "{r}");
    let done = t.wait(&sid, |v| v["type"] == "tool.done").await;
    assert_eq!(done["toolId"], started["toolId"]);
    assert_eq!(
        (&done["status"], &done["output"], &done["exitCode"]),
        (&json!("completed"), &json!("hi\n"), &json!(0))
    );
    assert_eq!(t.turn_done(&sid, 1).await["status"], "completed");
    let again = t.respond(&sid, &id, json!({ "kind": "deny" })).await;
    assert_eq!(again["result"]["outcome"], "stale");

    // 2. Denied: the call is `denied`, and the turn goes on.
    t.send(&sid, "tool").await;
    let item = t.wait_type(&sid, "needs.opened", 2).await[1]["item"].clone();
    t.respond(&sid, item["id"].as_str().unwrap(), json!({ "kind": "deny" }))
        .await;
    assert_eq!(t.turn_done(&sid, 2).await["status"], "completed");
    assert_eq!(t.wait_type(&sid, "tool.done", 2).await[1]["status"], "denied");

    // 3. For this session: the agent's own "always" option.
    t.send(&sid, "tool").await;
    let item = t.wait_type(&sid, "needs.opened", 3).await[2]["item"].clone();
    t.respond(&sid, item["id"].as_str().unwrap(), json!({ "kind": "allow_session" }))
        .await;
    assert_eq!(t.turn_done(&sid, 3).await["status"], "completed");

    // 4. High risk: Allow once / Deny only, and a fresh step-up to allow.
    t.send(&sid, "risky").await;
    let item = t.wait_type(&sid, "needs.opened", 4).await[3]["item"].clone();
    assert_eq!(item["risk"], "high");
    assert_eq!(item["options"], json!(["allow_once", "deny"]));
    let nid = item["id"].as_str().unwrap();
    let r = t.respond(&sid, nid, json!({ "kind": "allow_once" })).await;
    assert_eq!(r["error"]["code"], "stepup_required", "{r}");
    let r =
        t.h.raw(
            "chat.respond",
            json!({ "session_id": sid, "need_id": nid, "response": { "kind": "allow_once" }, "fresh_stepup": true }),
        )
        .await;
    assert_eq!(r["result"]["outcome"], "accepted", "{r}");
    assert_eq!(t.turn_done(&sid, 4).await["status"], "completed");

    // 5. A new file, the agent offering no "always": asked with its path, reported once made.
    t.send(&sid, "edit made.txt").await;
    let item = t.wait_type(&sid, "needs.opened", 5).await[4]["item"].clone();
    let path = t.dir.path().join("work").canonicalize().unwrap().join("made.txt");
    assert_eq!(item["title"], format!("Edit `{}`", path.display()));
    assert_eq!(item["options"], json!(["allow_once", "deny"]));
    t.respond(&sid, item["id"].as_str().unwrap(), json!({ "kind": "allow_once" }))
        .await;
    assert_eq!(t.turn_done(&sid, 5).await["status"], "completed");
    let diff = t
        .wait(&sid, |v| v["type"] == "tool.updated" && v["diff"].is_object())
        .await;
    assert_eq!(diff["diff"]["newText"], "new\n");
    let changed = t.wait(&sid, |v| v["type"] == "files.changed").await;
    assert_eq!(changed["files"][0]["change"], "created");
    assert!(path.exists());

    // Each decision went back once, on the id of the request that asked, as the option it offered.
    let said: Vec<(u64, String)> = t
        .acp_log()
        .into_iter()
        .filter(|m| m.get("method").is_none())
        .map(|m| {
            (
                m["id"].as_u64().unwrap(),
                m["result"]["outcome"]["optionId"].as_str().unwrap_or("").to_string(),
            )
        })
        .collect();
    let want: Vec<(u64, String)> = [
        (101, "allow"),
        (102, "reject"),
        (103, "always"),
        (104, "allow"),
        (105, "yes"),
    ]
    .iter()
    .map(|(i, o)| (*i, o.to_string()))
    .collect();
    assert_eq!(said, want);
    assert_contiguous(&t.events(&sid));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acp_cancel_ends_the_turn_and_the_agent_stays() {
    let (mut t, sid) = Chat::acp(Setup::default()).await;
    // An open permission request: answered `cancelled`, the turn ends interrupted.
    t.send(&sid, "tool").await;
    let item = t.need(&sid).await;
    let r = t.h.raw("chat.interrupt", json!({ "session_id": sid })).await;
    assert_eq!(r["result"]["interrupted"], true, "{r}");
    assert_eq!(t.turn_done(&sid, 1).await["status"], "interrupted");
    let resolved = t.wait(&sid, |v| v["type"] == "needs.resolved").await;
    assert_eq!(
        (&resolved["id"], &resolved["outcome"]),
        (&item["id"], &json!("cancelled"))
    );
    assert_eq!(t.wait(&sid, |v| v["type"] == "tool.done").await["status"], "cancelled");
    let stale = t
        .respond(&sid, item["id"].as_str().unwrap(), json!({ "kind": "allow_once" }))
        .await;
    assert_eq!(stale["result"]["outcome"], "stale");
    // Mid-stream, with a message queued behind it.
    t.send(&sid, "slow 40").await;
    t.c.chats_until(S, |fs| fs.iter().any(|f| f.ephemeral)).await;
    let queued = t.send(&sid, "hello").await;
    assert_eq!(queued["queued"], true, "{queued}");
    t.h.raw("chat.interrupt", json!({ "session_id": sid })).await;
    assert_eq!(t.turn_done(&sid, 2).await["status"], "interrupted");
    assert_eq!(t.turn_done(&sid, 3).await["status"], "completed");
    let last = t.wait_type(&sid, "message.done", 1).await;
    assert_eq!(last.last().unwrap()["text"], "Hello from ACP");
    // Told, not signalled: the same process still.
    assert_eq!(t.acp_starts(), 1);
    let rpc = t.acp_log();
    assert_eq!(rpc.iter().filter(|m| m["method"] == "session/cancel").count(), 2);
    assert!(rpc
        .iter()
        .any(|m| m["id"] == 101 && m["result"]["outcome"]["outcome"] == "cancelled"));
    assert!(!t.events(&sid).iter().any(|v| v["type"] == "notice"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acp_failed_turns_bad_frames_and_a_child_that_dies_mid_turn() {
    let (mut t, sid) = Chat::acp(Setup::default()).await;
    // 1. A prompt answered with an error: its words, and the chat goes on.
    t.send(&sid, "fail").await;
    let done = t.turn_done(&sid, 1).await;
    assert_eq!(
        (&done["status"], &done["error"]),
        (&json!("failed"), &json!("model overloaded"))
    );
    // 2. Lines that are not the protocol: each dropped with a notice, none stops the turn.
    t.send(&sid, "junk").await;
    assert_eq!(t.turn_done(&sid, 2).await["status"], "completed");
    let ev = t.events(&sid);
    let mut codes: Vec<&str> = ev
        .iter()
        .filter(|v| v["type"] == "notice")
        .map(|v| v["code"].as_str().unwrap())
        .collect();
    codes.dedup();
    assert_eq!(
        codes,
        [
            "agent_output",
            "unknown_event",
            "answer_in_terminal",
            "output_lost",
            "unknown_event"
        ]
    );
    assert!(ev
        .iter()
        .any(|v| v["type"] == "message.done" && v["text"] == "after junk"));
    assert!(ev.iter().all(|v| v.to_string().len() < MAX_EVENT_BYTES));
    assert!(t
        .acp_log()
        .iter()
        .any(|m| m["id"] == "q-1" && m["error"]["code"] == -32601));
    let session = chat_status(&t, &sid).await["vendorSessionId"]
        .as_str()
        .unwrap()
        .to_string();
    // 3. The process dies mid-turn: the turn fails with its last words.
    t.send(&sid, "exit").await;
    let done = t.turn_done(&sid, 3).await;
    assert_eq!(done["status"], "failed");
    assert!(done["error"].as_str().unwrap().contains("lost its mind"), "{done}");
    t.wait(&sid, |v| v["type"] == "notice" && v["code"] == "agent_exited")
        .await;
    assert_eq!(chat_status(&t, &sid).await["status"], "parked");
    // 4. The next message starts it again and loads the same session; its replay is not shown again.
    t.send(&sid, "hello").await;
    assert_eq!(t.turn_done(&sid, 4).await["status"], "completed");
    assert_eq!(t.acp_starts(), 2);
    let rpc = t.acp_log();
    let load = rpc.iter().find(|m| m["method"] == "session/load").expect("loaded");
    assert_eq!(load["params"]["sessionId"], session.as_str());
    assert_eq!(rpc.iter().filter(|m| m["method"] == "session/new").count(), 1);
    let opened = t.wait_type(&sid, "session.opened", 2).await;
    assert_eq!(opened[1]["resumedFrom"], session.as_str());
    assert_eq!(chat_status(&t, &sid).await["vendorSessionId"], session.as_str());
    assert!(!t.events(&sid).iter().any(|v| v.to_string().contains("old answer")));
    assert_contiguous(&t.events(&sid));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acp_without_load_starts_a_new_session_and_says_so() {
    let (mut t, sid) = Chat::acp(Setup {
        acp_no_load: true,
        ..Default::default()
    })
    .await;
    t.send(&sid, "hello").await;
    assert_eq!(t.turn_done(&sid, 1).await["status"], "completed");
    let first = chat_status(&t, &sid).await["vendorSessionId"]
        .as_str()
        .unwrap()
        .to_string();
    t.send(&sid, "exit").await;
    assert_eq!(t.turn_done(&sid, 2).await["status"], "failed");
    t.send(&sid, "hello").await;
    assert_eq!(t.turn_done(&sid, 3).await["status"], "completed");
    let n = t
        .wait(&sid, |v| v["type"] == "notice" && v["code"] == "resume_unsupported")
        .await;
    assert!(n["text"].as_str().unwrap().contains("new one"), "{n}");
    let rpc = t.acp_log();
    assert_eq!(rpc.iter().filter(|m| m["method"] == "session/new").count(), 2);
    assert!(!rpc.iter().any(|m| m["method"] == "session/load"));
    let second = chat_status(&t, &sid).await["vendorSessionId"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(first, second);
    assert_contiguous(&t.events(&sid));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acp_chat_follows_the_login_kind() {
    // A Cursor sign-in (no key in the env): refused unless web's policy allows it.
    let t = Chat::start(Setup::default()).await;
    let r = t.h.raw("chat.start", json!({ "agent": "cursor" })).await;
    assert_eq!(r["error"]["code"], "chat_login_not_allowed", "{r}");
    // A mode the manifest does not map is refused before anything starts.
    let t = Chat::start(Setup {
        cursor: true,
        ..Default::default()
    })
    .await;
    let r =
        t.h.raw("chat.start", json!({ "agent": "cursor", "mode": "plan_first" }))
            .await;
    assert_eq!(r["error"]["code"], "invalid_mode", "{r}");
    assert_eq!(t.acp_starts(), 0);
}

// ── queued prompts (docs/protocol.md "Queued prompts") ──────────────────

fn queued_seal(key: &str, id: &str, body: Value) -> String {
    tmuxedo_runner::secrets::seal::seal(
        key,
        body.to_string().as_bytes(),
        &tmuxedo_protocol::queued::context("m1", id),
    )
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn queued_prompt_reaches_a_chat_once_and_a_pane_and_refuses_the_rest() {
    let mut t = Chat::start(Setup {
        api_key: true,
        ..Setup::default()
    })
    .await;
    let key = t.c.text(S, |v| v["t"] == "secret_key").await["publicKey"]
        .as_str()
        .unwrap()
        .to_string();
    let sid = t.start_chat(json!({})).await;
    let id = "qtest0001";
    let sealed = queued_seal(
        &key,
        id,
        json!({ "v": 1, "text": "queued hello", "target": { "chat": sid } }),
    );
    let r = t.h.raw("queued.deliver", json!({ "id": id, "sealed": sealed })).await;
    assert_eq!(r["result"]["type"], "queued_delivered", "{r}");
    assert_eq!(r["result"]["to"], "chat");
    let m = t
        .wait(&sid, |v| v["type"] == "message.user" && v["text"] == "queued hello")
        .await;
    assert_eq!(m["clientMsgId"], "q:qtest0001");
    // A resend (the answer was lost) delivers nothing more.
    let again = t.h.raw("queued.deliver", json!({ "id": id, "sealed": sealed })).await;
    assert_eq!(again["result"]["duplicate"], true, "{again}");
    assert_eq!(again["result"]["seq"], r["result"]["seq"]);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let n = t
        .events(&sid)
        .iter()
        .filter(|v| v["type"] == "message.user" && v["text"] == "queued hello")
        .count();
    assert_eq!(n, 1, "delivered exactly once");

    // A terminal pane: typed in with Enter.
    let pid = "qtest0002";
    let s2 = queued_seal(
        &key,
        pid,
        json!({ "v": 1, "text": "echo QUEUED-$((40+2))", "target": { "pane": "w1:p1" } }),
    );
    let r = t.h.raw("queued.deliver", json!({ "id": pid, "sealed": s2 })).await;
    assert_eq!(r["result"]["to"], "pane", "{r}");
    t.h.wait_text("w1:p1", "QUEUED-42").await;

    let code = |r: &Value| r["error"]["code"].as_str().unwrap_or_default().to_string();
    // Ran out before the machine was back.
    let s3 = queued_seal(
        &key,
        "qtest0003",
        json!({ "v": 1, "text": "late", "target": { "chat": sid }, "expiresAt": 1 }),
    );
    assert_eq!(
        code(
            &t.h.raw("queued.deliver", json!({ "id": "qtest0003", "sealed": s3 }))
                .await
        ),
        "queued_expired"
    );
    // Sealed under another id: web cannot move a blob between prompts.
    let s4 = queued_seal(
        &key,
        "qtest0004",
        json!({ "v": 1, "text": "moved", "target": { "chat": sid } }),
    );
    assert_eq!(
        code(
            &t.h.raw("queued.deliver", json!({ "id": "qtest0005", "sealed": s4 }))
                .await
        ),
        "queued_unreadable"
    );
    // The session is gone.
    let s6 = queued_seal(
        &key,
        "qtest0006",
        json!({ "v": 1, "text": "gone", "target": { "chat": "nosuchchat1" } }),
    );
    assert_eq!(
        code(
            &t.h.raw("queued.deliver", json!({ "id": "qtest0006", "sealed": s6 }))
                .await
        ),
        "session_ended"
    );
    let s7 = queued_seal(
        &key,
        "qtest0007",
        json!({ "v": 1, "text": "gone", "target": { "pane": "w9:p9" } }),
    );
    assert_eq!(
        code(
            &t.h.raw("queued.deliver", json!({ "id": "qtest0007", "sealed": s7 }))
                .await
        ),
        "session_ended"
    );
    // Caps.
    let huge = "x".repeat(tmuxedo_protocol::queued::MAX_SEALED_BYTES + 1);
    assert_eq!(
        code(
            &t.h.raw("queued.deliver", json!({ "id": "qtest0008", "sealed": huge }))
                .await
        ),
        "invalid_params"
    );
    assert_eq!(
        code(
            &t.h.raw("queued.deliver", json!({ "id": "bad", "sealed": "x1.a.b.c" }))
                .await
        ),
        "invalid_params"
    );
    // None of the refused ones reached the chat.
    assert!(!t
        .events(&sid)
        .iter()
        .any(|v| ["late", "moved", "gone"].contains(&v["text"].as_str().unwrap_or(""))));
}

// ── headless tasks (docs/design/headless-tasks.md) ──

fn automation(o: &mut RunOptions) {
    o.automation_labels = vec!["ci".into()];
}

async fn task_wait(h: &H, id: &str) -> Value {
    let until = Instant::now() + Duration::from_secs(30);
    loop {
        let t = h.ok("task.get", json!({ "task_id": id })).await["task"].clone();
        if t["status"] != "running" {
            return t;
        }
        assert!(Instant::now() < until, "task never ended: {t}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_agent_task_runs_unattended_and_returns_its_answer() {
    let t = Chat::start(Setup {
        api_key: true,
        tweak: Some(automation),
        ..Default::default()
    })
    .await;
    let work = t.dir.path().join("work").display().to_string();
    let p = |id: &str, mode: &str| {
        json!({ "task_id": id, "kind": "agent", "labels": ["ci"], "cwd": work, "roots": [work],
            "agent": "claude", "allowed_agents": ["claude"], "mode": mode, "text": "hello" })
    };
    // Auto-approve is never a task's mode; an agent off the list is refused.
    assert_eq!(
        t.h.fails("task.start", p("a0", "auto_approve")).await["code"],
        "mode_not_allowed"
    );
    let mut other = p("a0", "ask_first");
    other["allowed_agents"] = json!(["codex"]);
    assert_eq!(t.h.fails("task.start", other).await["code"], "agent_not_allowed");
    let r = t.h.ok("task.start", p("a1", "ask_first")).await;
    assert_eq!(r["task"]["kind"], "agent");
    let sid = r["task"]["session_id"].as_str().unwrap().to_string();
    // An ordinary chat: people see it.
    let g = t.h.ok("chat.get", json!({ "session_id": sid })).await;
    assert_eq!(g["session"]["id"], sid.as_str());
    let done = task_wait(&t.h, "a1").await;
    assert_eq!(done["status"], "done", "{done}");
    assert_eq!(done["output"], "Hello there");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_agent_task_that_needs_a_permission_fails_saying_which() {
    let t = Chat::start(Setup {
        api_key: true,
        tweak: Some(automation),
        ..Default::default()
    })
    .await;
    let work = t.dir.path().join("work").display().to_string();
    let r =
        t.h.ok(
            "task.start",
            json!({ "task_id": "a2", "kind": "agent", "labels": ["ci"], "cwd": work, "roots": [work],
                "agent": "claude", "allowed_agents": ["claude"], "text": "tool" }),
        )
        .await;
    let sid = r["task"]["session_id"].as_str().unwrap().to_string();
    let done = task_wait(&t.h, "a2").await;
    assert_eq!(done["status"], "failed");
    assert_eq!(done["reason_code"], "permission_needed");
    assert_eq!(done["reason"], "needed permission: Run `echo hi`");
    // Nobody is left waiting: the chat closed with the task.
    let g = t.h.ok("chat.get", json!({ "session_id": sid })).await;
    assert_eq!(g["session"]["status"], "closed", "{g}");
}
