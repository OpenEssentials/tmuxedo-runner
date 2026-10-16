//! The built-in translator for Codex's app-server (`codex app-server`,
//! JSON-RPC over stdio, one JSON object per line; docs/design/agent-chat.md
//! §2.2, §8.3).
//!
//! Unlike Claude's structured mode, the channel goes both ways: the runner
//! asks (`initialize`, `thread/start | resume`, `turn/start`,
//! `turn/interrupt`), the server tells (`turn/*`, `item/*` notifications) and
//! the server asks back (`item/*/requestApproval`), which is how decisions
//! travel: no hooks. So [`CState::line`] returns, besides the normalised
//! events, the lines to write to the child and the decisions to open.
//!
//! Pure: no I/O, no clock but `now_ms` (set by the host). Every line from the
//! child is untrusted: a field of the wrong type is read as absent, an
//! unknown method is counted and shown as a neutral notice, never a panic.
//!
//! **The shapes here are from memory of the app-server's v2 protocol, not
//! from a capture of a real CLI** (the design lists the method names; the
//! field names are this file's reading of them). `tests/fixtures/codex/`
//! holds the same shapes; docs/handoff/chat-agents.md lists what to check.

use super::claude::{cut, cut_input, one_line, risk, tail, tool_facts, Emit, Response};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use tmuxedo_protocol::chat::{ChatBody, Diff, FileChange, Location, NeedsItem, MAX_TOOL_OUTPUT};

/// A line from the app-server longer than this is dropped (with a notice).
pub const MAX_FRAME: usize = 4 * 1024 * 1024;
/// Calls, requests and decisions remembered at most (a child cannot grow the state without bound).
const MAX_OPEN: usize = 256;
/// The live tail of a running command kept per call.
const LIVE_TAIL: usize = 16 * 1024;

/// A call that was announced and has no result yet.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OpenItem {
    /// The neutral tool name it was shown under ("Bash", "Edit", …).
    pub name: String,
    pub command: String,
    /// `(path, add | delete | update)` of a file change.
    pub paths: Vec<(String, String)>,
    pub started_ms: u64,
    /// Output so far (its tail), from the output deltas.
    pub live: String,
}

/// What the translator remembers between lines (persisted with the session).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CState {
    /// The thread of this app-server process, once it said so.
    pub thread: Option<String>,
    pub model: Option<String>,
    /// The thread to reopen, the folder and the approval policy for the thread request.
    pub resume: Option<String>,
    pub cwd: String,
    pub policy: Option<String>,
    pub next_id: u64,
    /// Our requests without an answer yet: id → method.
    pub pending: BTreeMap<u64, String>,
    /// The server's turn id while a turn runs.
    pub turn: Option<String>,
    /// A message waiting for the thread (never written to disk: it is not masked).
    #[serde(skip)]
    pub waiting: Option<String>,
    pub interrupting: bool,
    pub tools: BTreeMap<String, OpenItem>,
    /// Calls a person or a rule said no to.
    pub denied: BTreeSet<String>,
    /// When the reasoning item being streamed began.
    pub think_start: Option<u64>,
    /// Lines this version could not show.
    pub unknown: u64,
    /// Approvals the server waits for: (its request id, the item asked about).
    pub asked: Vec<(Value, String)>,
    #[serde(skip)]
    pub now_ms: u64,
}

/// A decision the server asks for.
#[derive(Debug, Clone, PartialEq)]
pub struct Ask {
    /// The JSON-RPC id to answer (a number or a string, as the server sent it).
    pub rpc: Value,
    pub method: String,
    pub params: Value,
}

/// What one line leads to.
#[derive(Debug, Default)]
pub struct Out {
    pub emits: Vec<Emit>,
    /// Lines for the child's stdin.
    pub writes: Vec<Vec<u8>>,
    pub asks: Vec<Ask>,
    /// The thread id, when this line told it (the handshake is then over).
    pub thread: Option<String>,
    /// The child cannot go on (the handshake was refused): stop it.
    pub fatal: Option<String>,
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}

fn frame(v: Value) -> Vec<u8> {
    let mut b = serde_json::to_vec(&v).unwrap_or_default();
    b.push(b'\n');
    b
}

fn top(body: ChatBody) -> Emit {
    Emit { body, agent_path: None }
}

fn notice(code: &str, text: String) -> Emit {
    top(ChatBody::Notice {
        code: code.into(),
        text,
        native: None,
    })
}

/// The answer to a server request: `accept | acceptForSession | decline | cancel`.
pub fn answer(rpc: &Value, decision: &str) -> Vec<u8> {
    frame(json!({ "id": rpc, "result": { "decision": decision } }))
}

/// The error message of a turn or an error notification, as one short text.
fn error_text(e: &Value) -> Option<String> {
    let m = match e {
        Value::String(t) => t.as_str(),
        _ => s(e, "message"),
    };
    Some(cut(m.trim(), 2000)).filter(|m| !m.is_empty())
}

/// A command as one line: a string, or an argv (older servers).
fn command_of(v: &Value) -> String {
    match v.get("command") {
        Some(Value::String(c)) => c.clone(),
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" "),
        _ => String::new(),
    }
}

/// `(path, add | delete | update, diff)` of a file-change item.
fn changes_of(item: &Value) -> Vec<(String, String, String)> {
    item.get("changes")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
        .iter()
        .take(100)
        .filter_map(|c| {
            let path = s(c, "path");
            let kind = match c.get("kind") {
                Some(Value::String(k)) => k.as_str(),
                Some(k) => s(k, "type"),
                None => "",
            };
            (!path.is_empty()).then(|| (path.to_string(), kind.to_string(), s(c, "diff").to_string()))
        })
        .collect()
}

fn done(tool_id: String, status: &str, raw: &str, exit_code: Option<i64>, duration_ms: Option<u64>) -> ChatBody {
    ChatBody::ToolDone {
        tool_id,
        status: status.into(),
        summary: None,
        output: Some(tail(raw, MAX_TOOL_OUTPUT)).filter(|t| !t.is_empty()),
        exit_code,
        duration_ms,
        output_bytes: Some(raw.len() as u64).filter(|n| *n > 0),
        output_kept: (raw.len() > MAX_TOOL_OUTPUT).then(|| "tail".to_string()),
        attachments: vec![],
        result_count: None,
    }
}

impl CState {
    fn request(&mut self, method: &str, params: Value) -> Vec<u8> {
        self.next_id += 1;
        if self.pending.len() >= MAX_OPEN {
            self.pending.pop_first();
        }
        self.pending.insert(self.next_id, method.to_string());
        frame(json!({ "id": self.next_id, "method": method, "params": params }))
    }

    /// A new app-server process: forget the last one's state and say hello.
    /// `resume`: the thread to reopen; `policy`: the native approval policy.
    pub fn begin(&mut self, resume: Option<&str>, cwd: &str, policy: Option<&str>, version: &str) -> Vec<u8> {
        *self = CState {
            model: self.model.take(),
            resume: resume.map(str::to_owned),
            cwd: cwd.to_string(),
            policy: policy.map(str::to_owned),
            unknown: self.unknown,
            ..CState::default()
        };
        self.request(
            "initialize",
            json!({ "clientInfo": { "name": "tmuxedo", "title": "tmuxedo", "version": version } }),
        )
    }

    fn turn_start(&mut self, text: &str) -> Vec<u8> {
        let thread = self.thread.clone().unwrap_or_default();
        self.request(
            "turn/start",
            json!({ "threadId": thread, "input": [{ "type": "text", "text": text }] }),
        )
    }

    /// A person's message: a turn now, or once the thread is open.
    pub fn send(&mut self, text: &str) -> Vec<Vec<u8>> {
        self.interrupting = false;
        if self.thread.is_some() {
            vec![self.turn_start(text)]
        } else {
            self.waiting = Some(text.to_string());
            vec![]
        }
    }

    /// Stop the running turn (it ends with `turn/completed`, status
    /// `interrupted`). Nothing to write while the server has not named the turn.
    pub fn interrupt(&mut self) -> Vec<Vec<u8>> {
        self.interrupting = true;
        self.waiting = None;
        match (self.thread.clone(), self.turn.clone()) {
            (Some(thread), Some(turn)) => {
                vec![self.request("turn/interrupt", json!({ "threadId": thread, "turnId": turn }))]
            }
            _ => vec![],
        }
    }

    /// The server named the running turn (an interrupt asked for before that goes out now).
    fn turn_known(&mut self, t: &str, out: &mut Out) {
        if t.is_empty() || self.turn.is_some() {
            return;
        }
        self.turn = Some(cut(t, 200));
        if self.interrupting {
            out.writes.extend(self.interrupt());
        }
    }

    /// An approval was opened for the server's request `rpc` about `item`.
    pub fn ask_opened(&mut self, rpc: &Value, item: &str) {
        if self.asked.len() < MAX_OPEN {
            self.asked.push((rpc.clone(), cut(item, 200)));
        }
    }

    /// An approval the host opened as a needs-you item (the same call as the ACP translator's).
    pub fn opened(&mut self, ask: &Ask) {
        self.ask_opened(&ask.rpc, s(&ask.params, "itemId"));
    }

    /// The answer to one open approval (nothing when it is not open any more).
    pub fn answer_one(&mut self, rpc: &Value, decision: &str) -> Vec<Vec<u8>> {
        let Some(i) = self.asked.iter().position(|(r, _)| r == rpc) else {
            return vec![];
        };
        let (rpc, item) = self.asked.remove(i);
        if decision == "decline" {
            self.deny(&item);
        }
        vec![answer(&rpc, decision)]
    }

    /// The same answer to every open approval (an interrupt, a restart).
    pub fn answer_asked(&mut self, decision: &str) -> Vec<Vec<u8>> {
        let ids: Vec<Value> = self.asked.iter().map(|(r, _)| r.clone()).collect();
        ids.iter().flat_map(|r| self.answer_one(r, decision)).collect()
    }

    /// A call was refused: its item, when it completes, is `denied`.
    pub fn deny(&mut self, item: &str) {
        if !item.is_empty() && self.denied.len() < MAX_OPEN {
            self.denied.insert(item.to_string());
        }
    }

    /// The calls still open, closed as `status`.
    fn close_open(&mut self, status: &str) -> Vec<Emit> {
        self.denied.clear();
        std::mem::take(&mut self.tools)
            .into_keys()
            .map(|id| top(done(id, status, "", None, None)))
            .collect()
    }

    pub fn cancel_open(&mut self) -> Vec<Emit> {
        self.close_open("cancelled")
    }

    /// One line from the child (already parsed: a JSON object).
    pub fn line(&mut self, v: &Value) -> Out {
        let mut out = Out::default();
        let method = s(v, "method");
        let params = v.get("params").unwrap_or(&Value::Null);
        let id = v.get("id").filter(|i| i.is_u64() || i.is_string());
        match (method.is_empty(), id) {
            (false, Some(id)) => self.server_request(id, method, params, &mut out),
            (false, None) => self.notification(method, params, &mut out),
            (true, Some(id)) => self.response(id, v, &mut out),
            (true, None) => self.unknown(&mut out),
        }
        out
    }

    fn unknown(&mut self, out: &mut Out) {
        self.unknown += 1;
        // Only the first few are shown: a server that drifted says the same thing all turn.
        if self.unknown <= 3 {
            out.emits.push(notice(
                "unknown_event",
                "The agent did something this version of tmuxedo can't show.".into(),
            ));
        }
    }

    fn response(&mut self, id: &Value, v: &Value, out: &mut Out) {
        let Some(method) = id.as_u64().and_then(|i| self.pending.remove(&i)) else {
            return self.unknown(out);
        };
        let error = v.get("error").filter(|e| !e.is_null());
        let result = v.get("result").unwrap_or(&Value::Null);
        match (method.as_str(), error) {
            ("initialize", None) => {
                out.writes.push(frame(json!({ "method": "initialized" })));
                let mut p = json!({ "cwd": self.cwd });
                if let Some(a) = &self.policy {
                    p["approvalPolicy"] = json!(a);
                }
                let line = match self.resume.clone() {
                    Some(t) => {
                        p["threadId"] = json!(t);
                        self.request("thread/resume", p)
                    }
                    None => self.request("thread/start", p),
                };
                out.writes.push(line);
            }
            ("thread/start" | "thread/resume", None) => {
                let t = s(&result["thread"], "id");
                if t.is_empty() || t.len() > 200 {
                    out.fatal = Some("Codex did not open a conversation.".into());
                    return;
                }
                self.thread = Some(t.to_string());
                if let Some(m) = Some(s(result, "model")).filter(|m| !m.is_empty()) {
                    self.model = Some(cut(m, 100));
                }
                out.thread = self.thread.clone();
                if let Some(text) = self.waiting.take() {
                    out.writes.push(self.turn_start(&text));
                }
            }
            ("turn/start", None) => {
                let t = s(&result["turn"], "id").to_string();
                self.turn_known(&t, out);
            }
            ("turn/start", Some(e)) => {
                // The turn never began: it ends here.
                self.turn = None;
                out.emits.push(top(ChatBody::TurnDone {
                    status: "failed".into(),
                    error: error_text(e).or_else(|| Some("Codex refused the message.".into())),
                    usage: None,
                }));
            }
            ("turn/interrupt", _) => {}
            (m, Some(e)) => {
                out.fatal = Some(format!(
                    "Codex could not {}: {}",
                    if m == "initialize" {
                        "start"
                    } else {
                        "open the conversation"
                    },
                    error_text(e).unwrap_or_else(|| "no reason given".into())
                ));
            }
            _ => {}
        }
    }

    fn server_request(&mut self, id: &Value, method: &str, params: &Value, out: &mut Out) {
        match method {
            "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => out.asks.push(Ask {
                rpc: id.clone(),
                method: method.to_string(),
                params: params.clone(),
            }),
            _ => {
                // A question, a permission grant or an MCP form: not carried yet. The server
                // gets a plain "no such method", and the person is told where to answer.
                out.writes.push(frame(json!({
                    "id": id, "error": { "code": -32601, "message": "not supported by this client" }
                })));
                out.emits.push(notice(
                    "answer_in_terminal",
                    "Codex asked something this chat cannot show yet. Open it in the terminal to answer.".into(),
                ));
            }
        }
    }

    fn notification(&mut self, method: &str, p: &Value, out: &mut Out) {
        let item = p.get("item").unwrap_or(&Value::Null);
        match method {
            "turn/started" => {
                let t = s(&p["turn"], "id").to_string();
                self.turn_known(&t, out);
            }
            "turn/completed" => {
                let turn = p.get("turn").unwrap_or(&Value::Null);
                let status = match s(turn, "status") {
                    "completed" => "completed",
                    "interrupted" => "interrupted",
                    _ if self.interrupting => "interrupted",
                    _ => "failed",
                };
                out.emits
                    .extend(self.close_open(if status == "failed" { "failed" } else { "cancelled" }));
                self.turn = None;
                self.interrupting = false;
                self.think_start = None;
                self.asked.clear();
                let error = (status == "failed")
                    .then(|| error_text(&turn["error"]).unwrap_or_else(|| "The turn failed.".into()));
                out.emits.push(top(ChatBody::TurnDone {
                    status: status.into(),
                    error,
                    usage: turn
                        .get("usage")
                        .or_else(|| p.get("usage"))
                        .filter(|u| u.is_object())
                        .cloned(),
                }));
            }
            "error" => {
                // Told again in `turn/completed` when it ends the turn; a retry goes on.
                if let Some(t) = error_text(p.get("error").unwrap_or(p)) {
                    out.emits.push(notice("agent_error", t));
                }
            }
            "item/started" => self.item_started(item, out),
            "item/completed" => self.item_completed(item, out),
            m if m.ends_with("agentMessage/delta") => {
                let (id, d) = (s(p, "itemId"), s(p, "delta"));
                if !id.is_empty() && !d.is_empty() {
                    out.emits.push(top(ChatBody::MessageDelta {
                        message_id: cut(id, 200),
                        text: d.to_string(),
                    }));
                }
            }
            m if m.ends_with("commandExecution/outputDelta") => {
                if let Some(t) = self.tools.get_mut(s(p, "itemId")) {
                    t.live.push_str(s(p, "delta"));
                    if t.live.len() > 2 * LIVE_TAIL {
                        t.live = tail(&t.live, LIVE_TAIL);
                    }
                    out.emits.push(top(ChatBody::ToolUpdated {
                        tool_id: s(p, "itemId").to_string(),
                        status: "in_progress".into(),
                        output: Some(tail(&t.live, LIVE_TAIL)),
                        diff: None,
                    }));
                }
            }
            // Known and not shown (yet): progress the events above already carry.
            "thread/started"
            | "thread/status/changed"
            | "thread/tokenUsage/updated"
            | "turn/diff/updated"
            | "turn/plan/updated"
            | "account/rateLimits/updated" => {}
            m if m.starts_with("item/") && m.ends_with("elta") => {}
            _ => self.unknown(out),
        }
    }

    fn item_started(&mut self, item: &Value, out: &mut Out) {
        let id = s(item, "id");
        if id.is_empty() || id.len() > 200 || self.tools.len() >= MAX_OPEN || self.tools.contains_key(id) {
            return;
        }
        let changes = changes_of(item);
        let (name, input, open) = match s(item, "type") {
            "reasoning" => {
                self.think_start = Some(self.now_ms);
                return;
            }
            "commandExecution" => {
                let command = command_of(item);
                let open = OpenItem {
                    command: command.clone(),
                    ..OpenItem::default()
                };
                ("Bash".to_string(), json!({ "command": command }), open)
            }
            "fileChange" => {
                let (path, kind) = changes.first().map(|c| (c.0.clone(), c.1.clone())).unwrap_or_default();
                let open = OpenItem {
                    paths: changes.iter().map(|(p, k, _)| (p.clone(), k.clone())).collect(),
                    ..OpenItem::default()
                };
                let name = if kind == "add" { "Write" } else { "Edit" };
                (name.to_string(), json!({ "file_path": path }), open)
            }
            "mcpToolCall" => (
                format!("mcp__{}__{}", s(item, "server"), s(item, "tool")),
                item.get("arguments").cloned().unwrap_or(Value::Null),
                OpenItem::default(),
            ),
            "webSearch" => (
                "WebSearch".to_string(),
                json!({ "query": s(item, "query") }),
                OpenItem::default(),
            ),
            // Messages are told when complete; anything else has no row.
            _ => return,
        };
        let facts = tool_facts(&name, &input, name == "Write");
        let locations = open
            .paths
            .iter()
            .map(|(p, _)| Location {
                path: p.clone(),
                line: None,
            })
            .collect();
        out.emits.push(top(ChatBody::ToolStarted {
            tool_id: id.to_string(),
            kind: facts.kind.into(),
            title: facts.title,
            input: cut_input(&input, 0),
            locations,
            name: Some(facts.name).filter(|n| !n.is_empty()),
            server: facts.server,
            subject: facts.subject,
            variant: facts.variant.map(str::to_owned),
        }));
        for (path, _, diff) in changes.into_iter().filter(|c| !c.2.is_empty()).take(20) {
            // Codex gives a unified diff, not the old and new text: it is shown as the new side.
            out.emits.push(top(ChatBody::ToolUpdated {
                tool_id: id.to_string(),
                status: "in_progress".into(),
                output: None,
                diff: Some(Diff {
                    path,
                    old_text: String::new(),
                    new_text: cut(&diff, 64 * 1024),
                    line: None,
                }),
            }));
        }
        self.tools.insert(
            id.to_string(),
            OpenItem {
                name,
                started_ms: self.now_ms,
                ..open
            },
        );
    }

    fn item_completed(&mut self, item: &Value, out: &mut Out) {
        let id = s(item, "id");
        match s(item, "type") {
            "agentMessage" => {
                let text = s(item, "text");
                if !id.is_empty() && !text.is_empty() {
                    out.emits.push(top(ChatBody::MessageDone {
                        message_id: cut(id, 200),
                        text: text.to_string(),
                    }));
                }
            }
            "reasoning" => {
                let part = |k: &str| match item.get(k) {
                    Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("\n\n"),
                    Some(Value::String(t)) => t.clone(),
                    _ => String::new(),
                };
                let text = Some(part("summary"))
                    .filter(|t| !t.is_empty())
                    .unwrap_or_else(|| part("content"));
                let began = self.think_start.take();
                out.emits.push(top(ChatBody::ThoughtDone {
                    redacted: text.is_empty(),
                    text: cut(&text, 64 * 1024),
                    duration_ms: began.map(|b| self.now_ms.saturating_sub(b)).filter(|d| *d > 0),
                }));
            }
            _ => {
                if !self.tools.contains_key(id) {
                    // Completed without a start (a short call): announce it first.
                    self.item_started(item, out);
                }
                let Some(open) = self.tools.remove(id) else { return };
                let refused = self.denied.remove(id) || s(item, "status") == "declined";
                let failed = matches!(s(item, "status"), "failed" | "declined")
                    || item.get("exitCode").and_then(Value::as_i64).is_some_and(|c| c != 0)
                    || item.get("error").is_some_and(|e| !e.is_null());
                let status = match (refused, failed) {
                    (true, _) => "denied",
                    (false, true) => "failed",
                    _ => "completed",
                };
                let raw = match item.get("aggregatedOutput").and_then(Value::as_str) {
                    Some(t) => t.to_string(),
                    None if !open.live.is_empty() => open.live.clone(),
                    None => match item.get("result").filter(|r| !r.is_null()) {
                        Some(Value::String(t)) => t.clone(),
                        Some(r) => r.to_string(),
                        None => error_text(item.get("error").unwrap_or(&Value::Null)).unwrap_or_default(),
                    },
                };
                let duration = item
                    .get("durationMs")
                    .and_then(Value::as_u64)
                    .or_else(|| Some(self.now_ms.saturating_sub(open.started_ms)).filter(|_| open.started_ms > 0));
                let code = item
                    .get("exitCode")
                    .and_then(Value::as_i64)
                    .filter(|_| failed && !refused);
                out.emits.push(top(done(id.to_string(), status, &raw, code, duration)));
                if status == "completed" && !open.paths.is_empty() {
                    let files = open.paths.iter().map(|(p, k)| FileChange {
                        path: p.clone(),
                        change: match k.as_str() {
                            "add" => "created",
                            "delete" => "deleted",
                            _ => "modified",
                        }
                        .into(),
                        source: "tool".into(),
                        tool_id: Some(id.to_string()),
                    });
                    out.emits.push(top(ChatBody::FilesChanged { files: files.collect() }));
                }
            }
        }
    }

    /// The needs-you item for an approval the server asked for. `id` and
    /// `expires_at` come from the host. Risk is the same rule set as every
    /// other engine's (crate::risk), on the same neutral tool names.
    pub fn needs_item(&self, ask: &Ask, id: &str, expires_at: &str, cwd: &str) -> NeedsItem {
        let open = self.tools.get(s(&ask.params, "itemId"));
        let (tool, input, title, why) = if ask.method.contains("commandExecution") {
            let mut command = command_of(&ask.params);
            if command.is_empty() {
                command = open.map(|o| o.command.clone()).unwrap_or_default();
            }
            let input = json!({ "command": command });
            let why = risk("Bash", &input, cwd);
            ("Bash", input, format!("Run `{}`", one_line(&command, 80)), why)
        } else {
            let paths: Vec<&str> = open
                .map(|o| o.paths.iter().map(|(p, _)| p.as_str()).collect())
                .unwrap_or_default();
            let why = paths.iter().find_map(|p| risk("Edit", &json!({ "file_path": p }), cwd));
            let first = paths.first().copied().unwrap_or("");
            let title = match paths.len() {
                0 => "Change files".to_string(),
                1 => format!("Edit `{first}`"),
                n => format!("Edit `{first}` and {} more", n - 1),
            };
            ("Edit", json!({ "file_path": first, "paths": paths }), title, why)
        };
        // "For this session" is the server's own `acceptForSession` (design §8.1); never for a high-risk call.
        let options: &[&str] = match why {
            Some(_) => &["allow_once", "deny"],
            None => &["allow_once", "allow_session", "deny"],
        };
        NeedsItem {
            id: id.to_string(),
            kind: "permission".into(),
            risk: if why.is_some() { "high" } else { "normal" }.into(),
            options: options.iter().map(|o| o.to_string()).collect(),
            title,
            tool: Some(tool.into()),
            input: Some(cut_input(&input, 0)),
            why: why
                .map(str::to_owned)
                .or_else(|| Some(cut(s(&ask.params, "reason"), 500)).filter(|r| !r.is_empty())),
            questions: vec![],
            markdown: None,
            expires_at: expires_at.to_string(),
        }
    }
}

/// The server's word for a person's answer to `item`, or why the answer does not fit.
pub fn decision(item: &NeedsItem, r: &Response) -> Result<&'static str, (&'static str, String)> {
    if !item.options.contains(&r.kind) {
        return Err((
            "invalid_response",
            format!("{} is not an answer to this item ({})", r.kind, item.options.join(", ")),
        ));
    }
    Ok(match r.kind.as_str() {
        "allow_once" => "accept",
        "allow_session" => "acceptForSession",
        _ => "decline",
    })
}

#[cfg(test)]
#[path = "codex_tests.rs"]
mod tests;
