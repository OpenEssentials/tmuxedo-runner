//! The generic translator for agents that speak the Agent Client Protocol
//! (`transport = "acp"`, `translator = "acp"`; docs/design/agent-chat.md
//! §2.4, §3.2, §8.3, D-C16): JSON-RPC 2.0 over stdio, one JSON object per line.
//!
//! The runner is the ACP *client*: it asks (`initialize`, `session/new` or
//! `session/load`, `session/prompt`, `session/set_mode`), it tells
//! (`session/cancel`), the agent tells (`session/update`) and the agent asks
//! back (`session/request_permission`), which is how decisions travel. A turn
//! is one `session/prompt` request: it ends when that request is answered.
//!
//! Same contract as the Codex translator (codex.rs), so the host drives both
//! the same way: [`AState::line`] returns the events, the lines to write, the
//! decisions to open, the session id once known and a fatal reason; decisions
//! come back in the neutral words `accept | acceptForSession | decline | cancel`
//! and are turned into the option the agent offered.
//!
//! Pure: no I/O, no clock but `now_ms` (set by the host). Every line is
//! untrusted: a field of the wrong type is read as absent, sizes are capped,
//! an unknown method is counted and shown as a neutral notice, never a panic.
//! Text that is not masked yet (a message being streamed, a thought) is never
//! written to disk with the state.
//!
//! **The field names are this file's reading of the ACP specification
//! (protocol version 1), written from memory without the schema at hand and
//! without a real agent**: `tests/fixtures/acp/` and the fake agent repeat the
//! same reading; docs/handoff/chat-agents.md lists what to check.

use super::claude::{cut, cut_input, one_line, risk, subject_line, tail, tool_facts, Emit, Response};
use super::codex::{Ask, Out};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use tmuxedo_protocol::chat::{ChatBody, Diff, FileChange, Location, NeedsItem, PlanEntry, MAX_TOOL_OUTPUT};

/// A line from the agent longer than this is dropped (with a notice).
pub const MAX_FRAME: usize = super::codex::MAX_FRAME;
/// The protocol version this client speaks.
const PROTOCOL: u64 = 1;
/// Calls, requests and decisions remembered at most.
const MAX_OPEN: usize = 256;
/// The live output kept per call.
const LIVE_TAIL: usize = 16 * 1024;
/// A message or a thought being streamed is kept up to this (the event is cut to fit anyway).
const MAX_TEXT: usize = 512 * 1024;
/// Options of one permission request kept at most.
const MAX_OPTIONS: usize = 8;

/// A call that was announced and has no result yet.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ATool {
    /// The ACP kind (one of the nine the clients know).
    pub kind: String,
    /// The neutral tool name used for risk and facts ("Bash", "Edit", …), or empty.
    pub name: String,
    pub command: String,
    pub title: String,
    /// `(path, created | modified | deleted)` of the files the call touches.
    pub paths: Vec<(String, String)>,
    pub started_ms: u64,
    /// Output so far (not masked yet: never saved).
    #[serde(skip)]
    pub live: String,
}

/// An open permission request: the agent's request id, the call, the options offered.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Asked {
    pub rpc: Value,
    pub tool: String,
    /// `(optionId, kind)`.
    pub options: Vec<(String, String)>,
}

/// What the translator remembers between lines (persisted with the session).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AState {
    /// The agent's session, once it said so.
    pub session: Option<String>,
    pub model: Option<String>,
    /// The session to reopen, the folder and the native mode.
    pub resume: Option<String>,
    pub cwd: String,
    pub mode: Option<String>,
    /// The agent can reopen a session (`agentCapabilities.loadSession`).
    pub can_load: bool,
    /// A `session/load` is replaying the old conversation: its updates are not shown again.
    pub loading: bool,
    pub next_id: u64,
    /// Our requests without an answer yet: id → method.
    pub pending: BTreeMap<u64, String>,
    /// The `session/prompt` request of the running turn.
    pub prompt: Option<u64>,
    /// A message waiting for the session (never written to disk: it is not masked).
    #[serde(skip)]
    pub waiting: Option<String>,
    pub interrupting: bool,
    pub tools: BTreeMap<String, ATool>,
    /// Calls a person or a rule said no to.
    pub denied: BTreeSet<String>,
    /// Messages and thoughts numbered across processes (ids stay unique in the log).
    pub seq: u64,
    /// The message being streamed: (id, text so far).
    #[serde(skip)]
    pub msg: Option<(String, String)>,
    /// The thought being streamed: (since, text so far).
    #[serde(skip)]
    pub thought: Option<(u64, String)>,
    /// Lines this version could not show.
    pub unknown: u64,
    pub asked: Vec<Asked>,
    #[serde(skip)]
    pub now_ms: u64,
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}

fn frame(mut v: Value) -> Vec<u8> {
    v["jsonrpc"] = json!("2.0");
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

fn error_text(e: &Value) -> Option<String> {
    let m = match e {
        Value::String(t) => t.as_str(),
        _ => s(e, "message"),
    };
    Some(cut(m.trim(), 2000)).filter(|m| !m.is_empty())
}

/// The ACP kind as one of the nine the clients know (`switch_mode` and anything new read as `other`).
fn kind_of(k: &str) -> &'static str {
    match k {
        "read" => "read",
        "edit" => "edit",
        "delete" => "delete",
        "move" => "move",
        "search" => "search",
        "execute" => "execute",
        "think" => "think",
        "fetch" => "fetch",
        _ => "other",
    }
}

/// The text parts of a content list (`[{type: content, content: {type: text, text}}]`), joined.
fn text_of(content: Option<&Value>) -> Option<String> {
    let items = content?.as_array()?;
    let parts: Vec<&str> = items
        .iter()
        .take(100)
        .filter(|c| s(c, "type") == "content")
        .filter_map(|c| c.get("content"))
        .filter(|c| s(c, "type") == "text")
        .map(|c| s(c, "text"))
        .collect();
    (!parts.is_empty()).then(|| parts.join("\n"))
}

/// The diff parts of a content list (`{type: diff, path, oldText?, newText}`).
fn diffs_of(content: Option<&Value>) -> Vec<(Diff, bool)> {
    content
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
        .iter()
        .filter(|c| s(c, "type") == "diff" && !s(c, "path").is_empty())
        .take(20)
        .map(|c| {
            let created = c.get("oldText").is_none_or(Value::is_null);
            (
                Diff {
                    path: cut(s(c, "path"), 4096),
                    old_text: cut(s(c, "oldText"), 64 * 1024),
                    new_text: cut(s(c, "newText"), 64 * 1024),
                    line: None,
                },
                created,
            )
        })
        .collect()
}

fn locations_of(v: &Value) -> Vec<Location> {
    v.get("locations")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
        .iter()
        .filter(|l| !s(l, "path").is_empty())
        .take(20)
        .map(|l| Location {
            path: cut(s(l, "path"), 4096),
            line: l.get("line").and_then(Value::as_u64),
        })
        .collect()
}

/// A command from a call's raw input: a string, or an argv (`command` + `args`).
fn command_of(raw: &Value) -> String {
    let mut parts: Vec<String> = match raw.get("command").or_else(|| raw.get("cmd")) {
        Some(Value::String(c)) => vec![c.clone()],
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).map(str::to_owned).collect(),
        _ => vec![],
    };
    if let Some(Value::Array(a)) = raw.get("args") {
        parts.extend(a.iter().filter_map(Value::as_str).map(str::to_owned));
    }
    cut(&parts.join(" "), 8192)
}

/// The neutral tool name and input the shared rules (risk, facts) know a call by.
fn neutral(kind: &str, title: &str, raw: &Value, locations: &[Location]) -> (&'static str, Value) {
    let path = locations
        .first()
        .map(|l| l.path.clone())
        .or_else(|| {
            ["path", "file_path", "filePath", "abs_path"]
                .iter()
                .map(|k| s(raw, k))
                .find(|p| !p.is_empty())
                .map(str::to_owned)
        })
        .unwrap_or_default();
    match kind {
        "execute" => {
            let mut command = command_of(raw);
            if command.is_empty() {
                // No input: the title is what the agent shows for it (often the command).
                command = title.trim_matches('`').to_string();
            }
            ("Bash", json!({ "command": command }))
        }
        "read" => ("Read", json!({ "file_path": path })),
        "edit" | "delete" | "move" => ("Edit", json!({ "file_path": path })),
        "search" => (
            "Grep",
            json!({ "pattern": s(raw, "pattern").to_string() + s(raw, "query"), "path": path }),
        ),
        "fetch" => {
            let url = s(raw, "url");
            ("WebFetch", json!({ "url": if url.is_empty() { title } else { url } }))
        }
        _ => ("", json!({})),
    }
}

/// The answer to a permission request: the option picked, or `cancelled`.
fn outcome(rpc: &Value, option: Option<&str>) -> Vec<u8> {
    let o = match option {
        Some(id) => json!({ "outcome": "selected", "optionId": id }),
        None => json!({ "outcome": "cancelled" }),
    };
    frame(json!({ "id": rpc, "result": { "outcome": o } }))
}

/// The options of a permission request: `(optionId, kind)`, known kinds only.
fn options_of(params: &Value) -> Vec<(String, String)> {
    params
        .get("options")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
        .iter()
        .filter(|o| {
            let id = s(o, "optionId");
            !id.is_empty() && id.len() <= 200
        })
        .filter(|o| ["allow_once", "allow_always", "reject_once", "reject_always"].contains(&s(o, "kind")))
        .take(MAX_OPTIONS)
        .map(|o| (s(o, "optionId").to_string(), s(o, "kind").to_string()))
        .collect()
}

/// The option for a neutral decision: `accept` → allow once, `acceptForSession` →
/// allow always, `decline` → reject (once, else always); `cancel` → none.
fn pick<'a>(options: &'a [(String, String)], decision: &str) -> Option<&'a str> {
    let kinds: &[&str] = match decision {
        "accept" => &["allow_once"],
        "acceptForSession" => &["allow_always"],
        "decline" => &["reject_once", "reject_always"],
        _ => &[],
    };
    kinds
        .iter()
        .find_map(|k| options.iter().find(|(_, ok)| ok == k))
        .map(|(id, _)| id.as_str())
}

/// The answer that refuses a request nobody will see (too many open): reject, else cancel.
pub fn refuse(ask: &Ask) -> Vec<u8> {
    let options = options_of(&ask.params);
    outcome(&ask.rpc, pick(&options, "decline"))
}

fn done(
    tool_id: String,
    status: &str,
    raw: &str,
    head: bool,
    exit_code: Option<i64>,
    duration_ms: Option<u64>,
) -> ChatBody {
    let over = raw.len() > MAX_TOOL_OUTPUT;
    ChatBody::ToolDone {
        tool_id,
        status: status.into(),
        summary: None,
        output: Some(if head {
            cut(raw, MAX_TOOL_OUTPUT)
        } else {
            tail(raw, MAX_TOOL_OUTPUT)
        })
        .filter(|t| !t.is_empty()),
        exit_code,
        duration_ms,
        output_bytes: Some(raw.len() as u64).filter(|n| *n > 0),
        output_kept: over.then(|| if head { "head" } else { "tail" }.to_string()),
        attachments: vec![],
        result_count: None,
    }
}

impl AState {
    fn request(&mut self, method: &str, params: Value) -> Vec<u8> {
        self.next_id += 1;
        if self.pending.len() >= MAX_OPEN {
            self.pending.pop_first();
        }
        self.pending.insert(self.next_id, method.to_string());
        frame(json!({ "id": self.next_id, "method": method, "params": params }))
    }

    /// A new agent process: forget the last one's state and say hello.
    /// `resume`: the session to reopen; `mode`: the native mode to switch to.
    pub fn begin(&mut self, resume: Option<&str>, cwd: &str, mode: Option<&str>, version: &str) -> Vec<u8> {
        *self = AState {
            model: self.model.take(),
            resume: resume.map(str::to_owned),
            cwd: cwd.to_string(),
            mode: mode.map(str::to_owned),
            seq: self.seq,
            unknown: self.unknown,
            ..AState::default()
        };
        // No file system and no terminal are offered: the agent uses its own, behind permission requests.
        self.request(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL,
                "clientCapabilities": { "fs": { "readTextFile": false, "writeTextFile": false }, "terminal": false },
                "clientInfo": { "name": "tmuxedo", "title": "tmuxedo", "version": version },
            }),
        )
    }

    /// The process went: what belonged to it goes too (the counters stay).
    pub fn reset(&mut self) {
        *self = AState {
            model: self.model.take(),
            seq: self.seq,
            unknown: self.unknown,
            ..AState::default()
        };
    }

    fn prompt(&mut self, text: &str) -> Vec<u8> {
        let session = self.session.clone().unwrap_or_default();
        let line = self.request(
            "session/prompt",
            json!({ "sessionId": session, "prompt": [{ "type": "text", "text": text }] }),
        );
        self.prompt = Some(self.next_id);
        line
    }

    /// A person's message: a turn now, or once the session is open.
    pub fn send(&mut self, text: &str) -> Vec<Vec<u8>> {
        self.interrupting = false;
        if self.session.is_some() && !self.loading {
            vec![self.prompt(text)]
        } else {
            self.waiting = Some(text.to_string());
            vec![]
        }
    }

    /// Stop the running turn: `session/cancel` (a notification); the turn ends when the
    /// agent answers the prompt (`stopReason: cancelled`). Nothing to write before a prompt went out.
    pub fn interrupt(&mut self) -> Vec<Vec<u8>> {
        self.interrupting = true;
        self.waiting = None;
        match (&self.session, self.prompt) {
            (Some(sid), Some(_)) => vec![frame(
                json!({ "method": "session/cancel", "params": { "sessionId": sid } }),
            )],
            _ => vec![],
        }
    }

    /// A permission request became a needs-you item.
    pub fn opened(&mut self, ask: &Ask) {
        if self.asked.len() < MAX_OPEN {
            self.asked.push(Asked {
                rpc: ask.rpc.clone(),
                tool: cut(s(&ask.params["toolCall"], "toolCallId"), 200),
                options: options_of(&ask.params),
            });
        }
    }

    /// The answer to one open request (nothing when it is not open any more).
    pub fn answer_one(&mut self, rpc: &Value, decision: &str) -> Vec<Vec<u8>> {
        let Some(i) = self.asked.iter().position(|a| &a.rpc == rpc) else {
            return vec![];
        };
        let a = self.asked.remove(i);
        if decision == "decline" {
            self.deny(&a.tool);
        }
        let option = pick(&a.options, decision);
        vec![outcome(&a.rpc, option)]
    }

    /// The same answer to every open request (an interrupt, a restart).
    pub fn answer_asked(&mut self, decision: &str) -> Vec<Vec<u8>> {
        let ids: Vec<Value> = self.asked.iter().map(|a| a.rpc.clone()).collect();
        ids.iter().flat_map(|r| self.answer_one(r, decision)).collect()
    }

    /// A call was refused: when it ends, it is `denied`.
    pub fn deny(&mut self, tool: &str) {
        if !tool.is_empty() && self.denied.len() < MAX_OPEN {
            self.denied.insert(tool.to_string());
        }
    }

    fn close_open(&mut self, status: &str) -> Vec<Emit> {
        self.denied.clear();
        std::mem::take(&mut self.tools)
            .into_keys()
            .map(|id| top(done(id, status, "", false, None, None)))
            .collect()
    }

    pub fn cancel_open(&mut self) -> Vec<Emit> {
        let mut e = self.flush();
        e.extend(self.close_open("cancelled"));
        e
    }

    /// The message and the thought being streamed end (something else began).
    fn flush(&mut self) -> Vec<Emit> {
        let mut e = vec![];
        if let Some((since, text)) = self.thought.take() {
            e.push(top(ChatBody::ThoughtDone {
                redacted: text.trim().is_empty(),
                text,
                duration_ms: Some(self.now_ms.saturating_sub(since)).filter(|d| *d > 0 && since > 0),
            }));
        }
        if let Some((id, text)) = self.msg.take() {
            if !text.is_empty() {
                e.push(top(ChatBody::MessageDone { message_id: id, text }));
            }
        }
        e
    }

    /// One line from the child (already parsed: a JSON object).
    pub fn line(&mut self, v: &Value) -> Out {
        let mut out = Out::default();
        let method = s(v, "method");
        let params = v.get("params").unwrap_or(&Value::Null);
        let id = v.get("id").filter(|i| i.is_u64() || i.is_string());
        match (method.is_empty(), id) {
            (false, Some(id)) => self.agent_request(id, method, params, &mut out),
            (false, None) => self.notification(method, params, &mut out),
            (true, Some(id)) => self.response(id, v, &mut out),
            (true, None) => self.unknown(&mut out),
        }
        out
    }

    fn unknown(&mut self, out: &mut Out) {
        self.unknown += 1;
        if self.unknown <= 3 {
            out.emits.push(notice(
                "unknown_event",
                "The agent did something this version of tmuxedo can't show.".into(),
            ));
        }
    }

    fn open_session(&mut self, out: &mut Out) {
        let p = json!({ "cwd": self.cwd, "mcpServers": [] });
        out.writes.push(self.request("session/new", p));
    }

    fn opened_session(&mut self, sid: String, result: &Value, out: &mut Out) {
        self.session = Some(sid);
        self.loading = false;
        let current = result["models"]["currentModelId"].as_str().unwrap_or("");
        if !current.is_empty() {
            self.model = Some(cut(current, 100));
        }
        // The native mode, when the agent lists it and is not in it already.
        let modes = &result["modes"];
        if let Some(m) = self.mode.clone() {
            let listed = modes["availableModes"]
                .as_array()
                .is_some_and(|a| a.iter().any(|x| s(x, "id") == m));
            if listed && s(modes, "currentModeId") != m {
                let sid = self.session.clone().unwrap_or_default();
                out.writes
                    .push(self.request("session/set_mode", json!({ "sessionId": sid, "modeId": m })));
            }
        }
        out.thread = self.session.clone();
        if let Some(text) = self.waiting.take() {
            out.writes.push(self.prompt(&text));
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
                if let Some(p) = result.get("protocolVersion").and_then(Value::as_u64) {
                    if p != PROTOCOL {
                        out.fatal = Some(format!(
                            "The agent speaks version {p} of the Agent Client Protocol; this tmuxedo speaks {PROTOCOL}."
                        ));
                        return;
                    }
                }
                self.can_load = result["agentCapabilities"]["loadSession"].as_bool().unwrap_or(false);
                match self.resume.clone() {
                    Some(r) if self.can_load => {
                        self.loading = true;
                        let p = json!({ "sessionId": r, "cwd": self.cwd, "mcpServers": [] });
                        out.writes.push(self.request("session/load", p));
                    }
                    Some(_) => {
                        out.emits.push(notice(
                            "resume_unsupported",
                            "This agent can't reopen the earlier conversation, so a new one started.".into(),
                        ));
                        self.open_session(out);
                    }
                    None => self.open_session(out),
                }
            }
            ("session/new", None) => {
                let sid = s(result, "sessionId");
                if sid.is_empty() || sid.len() > 200 {
                    out.fatal = Some("The agent did not open a conversation.".into());
                    return;
                }
                self.opened_session(sid.to_string(), result, out);
            }
            ("session/load", None) => {
                let sid = self.resume.clone().unwrap_or_default();
                self.opened_session(sid, result, out);
            }
            ("session/load", Some(e)) => {
                // Gone (or refused): a new conversation, said plainly.
                self.loading = false;
                out.emits.push(notice(
                    "resume_failed",
                    format!(
                        "The agent could not reopen the earlier conversation ({}), so a new one started.",
                        error_text(e).unwrap_or_else(|| "no reason given".into())
                    ),
                ));
                self.open_session(out);
            }
            ("session/prompt", r) => {
                if self.prompt != id.as_u64() {
                    return;
                }
                self.prompt = None;
                out.emits.extend(self.flush());
                let reason = s(result, "stopReason");
                let status = match (r, reason) {
                    (_, "cancelled") => "interrupted",
                    (Some(_), _) if self.interrupting => "interrupted",
                    (Some(_), _) | (None, "refusal") => "failed",
                    _ => "completed",
                };
                let error = match (status, r) {
                    ("failed", Some(e)) => Some(error_text(e).unwrap_or_else(|| "The turn failed.".into())),
                    ("failed", None) => Some("The agent refused to go on.".into()),
                    _ => None,
                };
                if matches!(reason, "max_tokens" | "max_turn_requests") {
                    out.emits.push(notice(
                        "agent_limit",
                        "The agent stopped at its limit for one turn. Send a message to continue.".into(),
                    ));
                }
                out.emits
                    .extend(self.close_open(if status == "failed" { "failed" } else { "cancelled" }));
                self.interrupting = false;
                self.asked.clear();
                out.emits.push(top(ChatBody::TurnDone {
                    status: status.into(),
                    error,
                    usage: result.get("usage").filter(|u| u.is_object()).cloned(),
                }));
            }
            ("session/set_mode", Some(e)) => {
                out.emits.push(notice(
                    "mode_not_set",
                    format!(
                        "The agent kept its own mode: {}",
                        error_text(e).unwrap_or_else(|| "no reason given".into())
                    ),
                ));
            }
            ("session/set_mode", None) => {}
            (m, Some(e)) => {
                let why = error_text(e).unwrap_or_else(|| "no reason given".into());
                // `auth_required` (-32000): the agent wants a sign-in this chat cannot do.
                out.fatal = Some(if e["code"].as_i64() == Some(-32000) {
                    format!("The agent needs you to sign in first. Sign in in a terminal, then try again ({why}).")
                } else {
                    format!(
                        "The agent could not {}: {why}",
                        if m == "initialize" {
                            "start"
                        } else {
                            "open the conversation"
                        }
                    )
                });
            }
            _ => {}
        }
    }

    fn agent_request(&mut self, id: &Value, method: &str, params: &Value, out: &mut Out) {
        match method {
            "session/request_permission" if !self.loading => {
                let call = params.get("toolCall").unwrap_or(&Value::Null);
                let tid = s(call, "toolCallId");
                if !tid.is_empty() && !self.tools.contains_key(tid) {
                    // Asked about a call it did not announce: announce it.
                    out.emits.extend(self.flush());
                    self.tool_started(call, out);
                }
                out.asks.push(Ask {
                    rpc: id.clone(),
                    method: method.to_string(),
                    params: params.clone(),
                });
            }
            _ => {
                // Files and terminals were not offered; questions and plans are not carried yet.
                out.writes.push(frame(json!({
                    "id": id, "error": { "code": -32601, "message": "not supported by this client" }
                })));
                if method.starts_with("fs/") || method.starts_with("terminal/") {
                    self.unknown(out);
                } else {
                    out.emits.push(notice(
                        "answer_in_terminal",
                        "The agent asked something this chat cannot show yet. Open it in the terminal to answer."
                            .into(),
                    ));
                }
            }
        }
    }

    fn notification(&mut self, method: &str, p: &Value, out: &mut Out) {
        match method {
            "session/update" => {
                if self.loading {
                    // The reopened conversation replays itself: it is in the log already.
                    return;
                }
                if self.session.as_deref().is_some_and(|sid| sid != s(p, "sessionId")) {
                    return;
                }
                let u = p.get("update").unwrap_or(&Value::Null);
                self.update(u, out);
            }
            // Cursor's extensions (design §8.3) are a later slice: known, not shown.
            m if m.starts_with("cursor/") => {}
            _ => self.unknown(out),
        }
    }

    fn update(&mut self, u: &Value, out: &mut Out) {
        let text = || s(&u["content"], "text");
        match s(u, "sessionUpdate") {
            "agent_message_chunk" => {
                let t = text();
                if t.is_empty() {
                    return;
                }
                if self.thought.is_some() {
                    let f = self.flush_thought();
                    out.emits.extend(f);
                }
                if self.msg.is_none() {
                    self.seq += 1;
                    self.msg = Some((format!("acp-m{}", self.seq), String::new()));
                }
                let (id, all) = self.msg.as_mut().expect("set");
                if all.len() < MAX_TEXT {
                    all.push_str(t);
                }
                out.emits.push(top(ChatBody::MessageDelta {
                    message_id: id.clone(),
                    text: t.to_string(),
                }));
            }
            "agent_thought_chunk" => {
                let t = text();
                if let Some((id, m)) = self.msg.take() {
                    if !m.is_empty() {
                        out.emits.push(top(ChatBody::MessageDone {
                            message_id: id,
                            text: m,
                        }));
                    }
                }
                let now = self.now_ms;
                let (_, all) = self.thought.get_or_insert_with(|| (now, String::new()));
                if all.len() < MAX_TEXT {
                    all.push_str(t);
                }
            }
            "tool_call" => {
                out.emits.extend(self.flush());
                self.tool_started(u, out);
                // A call announced with content (a diff, output) or already finished.
                if u.get("content").is_some_and(Value::is_array)
                    || matches!(s(u, "status"), "completed" | "failed" | "cancelled")
                {
                    self.tool_updated(u, out);
                }
            }
            "tool_call_update" => {
                out.emits.extend(self.flush());
                if !self.tools.contains_key(s(u, "toolCallId")) {
                    self.tool_started(u, out);
                }
                self.tool_updated(u, out);
            }
            "plan" => {
                let entries = u
                    .get("entries")
                    .and_then(Value::as_array)
                    .map(Vec::as_slice)
                    .unwrap_or(&[])
                    .iter()
                    .take(100)
                    .filter(|e| !s(e, "content").is_empty())
                    .map(|e| PlanEntry {
                        text: cut(s(e, "content"), 2000),
                        status: match s(e, "status") {
                            "in_progress" => "in_progress",
                            "completed" => "done",
                            _ => "pending",
                        }
                        .into(),
                    })
                    .collect();
                out.emits.push(top(ChatBody::PlanUpdated { entries }));
            }
            // Known and not shown (yet).
            "user_message_chunk"
            | "available_commands_update"
            | "current_mode_update"
            | "session_info_update"
            | "config_option_update"
            | "usage_update" => {}
            _ => self.unknown(out),
        }
    }

    fn flush_thought(&mut self) -> Vec<Emit> {
        let msg = self.msg.take();
        let e = self.flush();
        self.msg = msg;
        e
    }

    fn tool_started(&mut self, u: &Value, out: &mut Out) {
        let id = s(u, "toolCallId");
        if id.is_empty() || id.len() > 200 || self.tools.len() >= MAX_OPEN || self.tools.contains_key(id) {
            return;
        }
        let kind = kind_of(s(u, "kind"));
        let title = cut(s(u, "title").trim(), 300);
        let raw = u.get("rawInput").unwrap_or(&Value::Null);
        let locations = locations_of(u);
        let (name, input) = neutral(kind, &title, raw, &locations);
        let (fact_title, fact_name, subject, variant) = if name.is_empty() {
            (String::new(), None, None, None)
        } else {
            let f = tool_facts(name, &input, false);
            (f.title, Some(f.name), f.subject, f.variant)
        };
        let subject = subject.or_else(|| subject_line(&title));
        let shown = if !title.is_empty() {
            title.clone()
        } else if !fact_title.is_empty() {
            fact_title
        } else {
            "Used a tool".to_string()
        };
        let paths = locations
            .iter()
            .map(|l| {
                let c = if kind == "delete" { "deleted" } else { "modified" };
                (l.path.clone(), c.to_string())
            })
            .collect();
        out.emits.push(top(ChatBody::ToolStarted {
            tool_id: id.to_string(),
            kind: kind.into(),
            title: shown,
            input: cut_input(if raw.is_null() { &input } else { raw }, 0),
            locations,
            name: fact_name.filter(|n| !n.is_empty()),
            server: None,
            subject,
            variant: variant.map(str::to_owned),
        }));
        self.tools.insert(
            id.to_string(),
            ATool {
                kind: kind.into(),
                name: name.into(),
                command: s(&input, "command").to_string(),
                title,
                paths,
                started_ms: self.now_ms,
                live: String::new(),
            },
        );
    }

    fn tool_updated(&mut self, u: &Value, out: &mut Out) {
        let id = s(u, "toolCallId").to_string();
        let Some(open) = self.tools.get_mut(&id) else { return };
        let content = u.get("content").filter(|c| c.is_array());
        // Content replaces what the call showed so far.
        if let Some(t) = text_of(content) {
            open.live = tail(&t, 2 * LIVE_TAIL);
        }
        for l in locations_of(u) {
            if !open.paths.iter().any(|(p, _)| *p == l.path) && open.paths.len() < 20 {
                open.paths.push((l.path, "modified".into()));
            }
        }
        let diffs = diffs_of(content);
        for (d, created) in &diffs {
            let change = if *created { "created" } else { "modified" };
            match open.paths.iter().position(|(p, _)| *p == d.path) {
                Some(i) => open.paths[i].1 = change.into(),
                None if open.paths.len() < 20 => open.paths.push((d.path.clone(), change.into())),
                None => {}
            }
        }
        let status = s(u, "status");
        let finished = matches!(status, "completed" | "failed" | "cancelled");
        for (d, _) in diffs {
            out.emits.push(top(ChatBody::ToolUpdated {
                tool_id: id.clone(),
                status: "in_progress".into(),
                output: None,
                diff: Some(d),
            }));
        }
        if !finished {
            if content.is_some() && !open.live.is_empty() {
                out.emits.push(top(ChatBody::ToolUpdated {
                    tool_id: id.clone(),
                    status: "in_progress".into(),
                    output: Some(tail(&open.live, LIVE_TAIL)),
                    diff: None,
                }));
            }
            return;
        }
        let open = self.tools.remove(&id).expect("checked");
        let raw_out = u.get("rawOutput").filter(|r| !r.is_null());
        let refused = self.denied.remove(&id);
        let status = match (refused, status) {
            (true, _) => "denied",
            (false, "failed") => "failed",
            (false, "cancelled") => "cancelled",
            _ => "completed",
        };
        let raw = if !open.live.is_empty() {
            open.live.clone()
        } else {
            match raw_out {
                Some(Value::String(t)) => t.clone(),
                Some(r) => {
                    let o = s(r, "output");
                    if o.is_empty() {
                        cut(&r.to_string(), 4 * MAX_TOOL_OUTPUT)
                    } else {
                        o.to_string()
                    }
                }
                None => String::new(),
            }
        };
        let code = raw_out
            .and_then(|r| r.get("exitCode").or_else(|| r.get("exit_code")))
            .and_then(Value::as_i64)
            .filter(|_| status != "denied");
        let duration = Some(self.now_ms.saturating_sub(open.started_ms)).filter(|_| open.started_ms > 0);
        let head = matches!(open.kind.as_str(), "read" | "search" | "fetch");
        out.emits
            .push(top(done(id.clone(), status, &raw, head, code, duration)));
        if status == "completed" && matches!(open.kind.as_str(), "edit" | "delete" | "move") && !open.paths.is_empty() {
            let files = open.paths.iter().map(|(p, c)| FileChange {
                path: p.clone(),
                change: c.clone(),
                source: "tool".into(),
                tool_id: Some(id.clone()),
            });
            out.emits.push(top(ChatBody::FilesChanged { files: files.collect() }));
        }
    }

    /// The needs-you item for a permission request. `id` and `expires_at` come from
    /// the host. Risk is the same rule set as every other engine's (crate::risk), on
    /// the neutral tool names; "for this session" only where the agent offers
    /// `allow_always`, and never for a high-risk call.
    pub fn needs_item(&self, ask: &Ask, id: &str, expires_at: &str, cwd: &str) -> NeedsItem {
        let call = ask.params.get("toolCall").unwrap_or(&Value::Null);
        let open = self.tools.get(s(call, "toolCallId"));
        let kind = match s(call, "kind") {
            "" => open.map(|o| o.kind.as_str()).unwrap_or("other"),
            k => kind_of(k),
        };
        let mut title_raw = s(call, "title").trim().to_string();
        if title_raw.is_empty() {
            title_raw = open.map(|o| o.title.clone()).unwrap_or_default();
        }
        let mut locations = locations_of(call);
        if locations.is_empty() {
            locations = open
                .map(|o| {
                    o.paths
                        .iter()
                        .map(|(p, _)| Location {
                            path: p.clone(),
                            line: None,
                        })
                        .collect()
                })
                .unwrap_or_default();
        }
        let raw = call.get("rawInput").unwrap_or(&Value::Null);
        let (tool, mut input) = neutral(kind, &title_raw, raw, &locations);
        if tool == "Bash" && raw.is_null() {
            if let Some(o) = open.filter(|o| !o.command.is_empty()) {
                input = json!({ "command": o.command });
            }
        }
        let why = if tool.is_empty() { None } else { risk(tool, &input, cwd) };
        let path = s(&input, "file_path");
        let title = match tool {
            "Bash" => format!("Run `{}`", one_line(s(&input, "command"), 80)),
            "Edit" if !path.is_empty() => match kind {
                "delete" => format!("Delete `{path}`"),
                "move" => format!("Move `{path}`"),
                _ => format!("Edit `{path}`"),
            },
            "Read" if !path.is_empty() => format!("Read `{path}`"),
            "WebFetch" => format!("Fetch {}", one_line(s(&input, "url"), 80)),
            _ if !title_raw.is_empty() => one_line(&title_raw, 120),
            _ => "Use a tool".to_string(),
        };
        let offered = options_of(&ask.params);
        let has = |k: &str| offered.iter().any(|(_, ok)| ok == k);
        let mut options = vec![];
        if has("allow_once") {
            options.push("allow_once");
        }
        if has("allow_always") && why.is_none() {
            options.push("allow_session");
        }
        options.push("deny");
        NeedsItem {
            id: id.to_string(),
            kind: "permission".into(),
            risk: if why.is_some() { "high" } else { "normal" }.into(),
            options: options.iter().map(|o| o.to_string()).collect(),
            title,
            tool: Some(if tool.is_empty() { "other" } else { tool }.into()),
            input: Some(cut_input(if raw.is_null() { &input } else { raw }, 0)),
            why: why.map(str::to_owned),
            questions: vec![],
            markdown: None,
            expires_at: expires_at.to_string(),
        }
    }
}

/// The neutral word for a person's answer (the same as Codex's; see [`AState::answer_one`]).
pub fn decision(item: &NeedsItem, r: &Response) -> Result<&'static str, (&'static str, String)> {
    super::codex::decision(item, r)
}

#[cfg(test)]
#[path = "acp_tests.rs"]
mod tests;
