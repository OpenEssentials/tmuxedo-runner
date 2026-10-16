//! The runner API, served on the local NDJSON socket and for gateway `req`
//! frames. Names, parameters and result shapes are specified by
//! `runner/api/runner-api.schema.json`; the conformance tests validate every
//! method's request and response against it. Unknown methods answer
//! `unknown_method`.

use crate::agent::{parse_status, status_str};
use crate::daemon::Daemon;
use crate::events::{event_matches, Event, Filter, OutputMatcher};
use crate::topology::{export_layout, split_dir_name, split_dir_parse, Dir, TopoError};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};
use tmuxedo_protocol::frames::{AgentStatus, Layout, RunnerFrame, SplitDir};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;

#[derive(Debug, Clone, PartialEq)]
pub struct ApiErr {
    pub code: String,
    pub message: String,
}

pub fn err(code: &str, message: impl Into<String>) -> ApiErr {
    ApiErr {
        code: code.into(),
        message: message.into(),
    }
}

impl From<TopoError> for ApiErr {
    fn from(e: TopoError) -> Self {
        err(e.0, e.1)
    }
}

impl From<(String, String)> for ApiErr {
    fn from(e: (String, String)) -> Self {
        err(&e.0, e.1)
    }
}

pub type ApiResult = Result<Value, ApiErr>;

pub struct LocalReply {
    pub body: Value,
    /// For `events.subscribe`: further lines to stream on the connection.
    pub stream: Option<mpsc::UnboundedReceiver<String>>,
}

pub enum ReplyTo {
    Local(oneshot::Sender<LocalReply>),
    Gateway(String),
}

pub enum WaitKind {
    Output {
        pane: String,
        source: String,
        lines: Option<usize>,
        matcher: OutputMatcher,
    },
    Agent {
        pane: String,
        until: Vec<AgentStatus>,
        /// Only a change after this seq counts (agent.prompt --wait).
        after_seq: Option<u64>,
        prompted: bool,
    },
    Event {
        matcher: Value,
    },
}

pub struct Waiter {
    pub kind: WaitKind,
    pub deadline: Option<Instant>,
    pub reply: ReplyTo,
}

pub struct Subscriber {
    pub filters: Vec<Filter>,
    pub tx: mpsc::UnboundedSender<String>,
}

impl Subscriber {
    pub fn wants_lifecycle(&self, kind: &str) -> bool {
        self.filters
            .iter()
            .any(|f| matches!(f, Filter::Lifecycle(k) if k == kind))
    }
    pub fn wants_agent(&self, pane: &str, status: &str) -> bool {
        self.filters.iter().any(|f| {
            matches!(f, Filter::AgentStatus { pane: p, status: s } if p == pane && s.as_deref().is_none_or(|s| s == status))
        })
    }
}

enum Outcome {
    Now(ApiResult),
    Wait(WaitKind, Option<Duration>),
    Subscribe(Vec<Filter>),
}

/// Answer a request, wherever it came from.
pub fn respond(reply: ReplyTo, result: ApiResult, d: &mut Daemon) {
    match reply {
        ReplyTo::Local(tx) => {
            let body = match result {
                Ok(v) => json!({ "result": v }),
                Err(e) => json!({ "error": { "code": e.code, "message": e.message } }),
            };
            let _ = tx.send(LocalReply { body, stream: None });
        }
        ReplyTo::Gateway(id) => {
            let f = match result {
                Ok(v) => RunnerFrame::ok(id.clone(), v),
                Err(e) => RunnerFrame::err(id.clone(), &e.code, e.message),
            };
            let mut text = f.to_json();
            // Never send a frame the gateway would refuse by closing the
            // socket (audit P2-6): answer `too_large` instead.
            if text.len() > GW_FRAME_MAX {
                let msg = format!("the reply is {} bytes; the limit is {GW_FRAME_MAX}", text.len());
                text = RunnerFrame::err(id, "too_large", msg).to_json();
            }
            d.try_send_text(text);
        }
    }
}

/// `pane.read` / `agent.read`: at most this many lines …
pub const READ_MAX_LINES: usize = 10_000;
/// … and this many bytes of JSON-encoded text (the newest part is kept and
/// `truncated` is set), well under the gateway's frame limit (audit P2-6).
pub const READ_MAX_BYTES: usize = 512 * 1024;
/// Lines in one `pane.read` history page unless the caller asks for fewer or more.
pub const HISTORY_PAGE_LINES: usize = 2_000;
/// The largest text frame sent to the gateway (its `maxPayload` is 1 MiB).
pub const GW_FRAME_MAX: usize = 1024 * 1024 - 1024;

/// Bytes `c` takes inside a JSON string.
fn json_len(c: char) -> usize {
    match c {
        '"' | '\\' | '\n' | '\r' | '\t' | '\u{8}' | '\u{c}' => 2,
        c if (c as u32) < 0x20 => 6,
        c => c.len_utf8(),
    }
}

/// Keep the newest part of `text` whose JSON encoding fits in `max` bytes,
/// cut at a line start when there is one; true if anything was dropped.
pub fn cap_text(text: String, max: usize) -> (String, bool) {
    if text.chars().map(json_len).sum::<usize>() <= max {
        return (text, false);
    }
    let mut used = 0;
    let mut start = text.len();
    for (i, c) in text.char_indices().rev() {
        used += json_len(c);
        if used > max {
            break;
        }
        start = i;
    }
    let tail = &text[start..];
    let tail = match tail.find('\n') {
        Some(nl) if nl + 1 < tail.len() => &tail[nl + 1..],
        _ => tail,
    };
    (tail.to_string(), true)
}

fn s<'a>(p: &'a Value, k: &str) -> Option<&'a str> {
    p.get(k).and_then(Value::as_str).filter(|v| !v.is_empty())
}

fn req_str<'a>(p: &'a Value, k: &str) -> Result<&'a str, ApiErr> {
    s(p, k).ok_or_else(|| err("invalid_params", format!("{k} is required")))
}

/// The `label` of a workspace/tab rename, cleaned (docs/protocol.md "Names").
/// It must be present: a string, or null. `Ok(None)` (null, or a name that
/// cleans to nothing) means "restore the default name".
fn rename_label(p: &Value) -> Result<Option<String>, ApiErr> {
    match &p["label"] {
        Value::Null if p.get("label").is_some() => Ok(None),
        Value::String(l) => Ok(crate::topology::clean_label(Some(l.clone()))),
        _ => Err(err(
            "invalid_params",
            "label is required (a string, or null for the default name)",
        )),
    }
}

fn env_param(p: &Value) -> BTreeMap<String, String> {
    p["env"]
        .as_object()
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

fn aggregate(statuses: impl Iterator<Item = AgentStatus>) -> AgentStatus {
    let rank = |s: &AgentStatus| match s {
        AgentStatus::Blocked => 4,
        AgentStatus::Working => 3,
        AgentStatus::Done => 2,
        AgentStatus::Idle => 1,
        AgentStatus::Unknown => 0,
    };
    statuses.max_by_key(rank).unwrap_or(AgentStatus::Unknown)
}

impl Daemon {
    pub(crate) fn try_send_text(&mut self, text: String) {
        self.queue_msg(Message::Text(text.into()));
    }

    // ── info builders ──────────────────────────────────────

    fn pane_status(&self, id: &str) -> AgentStatus {
        self.panes
            .get(id)
            .map(|p| p.track.effective)
            .unwrap_or(AgentStatus::Unknown)
    }

    pub(crate) fn pane_info(&self, id: &str) -> Value {
        let rec = self.state.pane(id);
        let (ws, tab) = self.state.workspace_of(id).unwrap_or_default();
        let rt = self.panes.get(id);
        let kind = rt.and_then(|r| r.track.kind().map(str::to_owned));
        let title = rt.map(|r| r.emu.signals().title.clone()).filter(|t| !t.is_empty());
        let meta = rec.map(|r| r.meta.clone()).unwrap_or_default();
        let mut v = json!({
            "pane_id": id,
            "terminal_id": rec.map(|r| r.holder_id()).unwrap_or_default(),
            "workspace_id": ws,
            "tab_id": tab,
            "focused": self.state.is_focused_pane(id),
            "agent": kind,
            "agent_status": status_str(self.pane_status(id)),
            "revision": rt.map(|r| r.end()).unwrap_or(0),
            "cwd": rec.map(|r| r.pane.cwd.clone()),
            "label": rec.and_then(|r| r.pane.label.clone()),
            "title": meta.title.clone().or_else(|| rec.map(|r| r.pane.title.clone())),
            "terminal_title": title,
            "terminal_title_stripped": title.as_deref().map(|t| t.trim().to_string()),
            "display_agent": meta.display_agent,
            "state_labels": meta.state_labels,
            "tokens": meta.tokens,
        });
        if let (Some(sid), Some(k)) = (meta.session_id.as_deref(), kind.as_deref().or(meta.kind.as_deref())) {
            v["agent_session"] = json!({ "source": "tmuxedo", "agent": k, "kind": "id", "value": sid });
        }
        v
    }

    fn agent_info(&self, id: &str) -> Value {
        let mut v = self.pane_info(id);
        let rt = self.panes.get(id);
        let meta = self.state.pane(id).map(|r| r.meta.clone()).unwrap_or_default();
        v["name"] = json!(meta.name);
        v["interactive_ready"] = json!(rt.is_some_and(|r| r.alive));
        v["launch_pending"] = json!(false);
        v["screen_detection_skipped"] = json!(false);
        v["state_change_seq"] = json!(rt.map(|r| r.track.state_change_seq).unwrap_or(0));
        v["completion_seq"] = json!(rt.and_then(|r| r.track.completion_seq));
        v
    }

    pub(crate) fn tab_info(&self, tab: &str) -> Value {
        let t = self.state.tab(tab);
        let panes = self.state.panes_of_tab(tab);
        let focused = self.state.focused_tab().is_some_and(|f| f.id == tab);
        json!({
            "tab_id": tab,
            "workspace_id": t.map(|t| t.workspace_id.clone()).unwrap_or_default(),
            "number": t.map(|t| t.position + 1).unwrap_or(0),
            "label": t.map(|t| t.name.clone()).unwrap_or_default(),
            "focused": focused,
            "pane_count": panes.len(),
            "agent_status": status_str(aggregate(panes.iter().map(|p| self.pane_status(p)))),
        })
    }

    pub(crate) fn workspace_info(&self, ws: &str) -> Value {
        let w = self.state.workspace(ws);
        let panes = self.state.panes_of_ws(ws);
        let focused = self.state.focused_workspace().is_some_and(|f| f.id == ws);
        json!({
            "workspace_id": ws,
            "number": w.map(|w| w.position + 1).unwrap_or(0),
            "label": w.map(|w| w.name.clone()).unwrap_or_default(),
            "focused": focused,
            "pane_count": panes.len(),
            "tab_count": self.state.tabs_of(ws).len(),
            "active_tab_id": w.map(|w| w.active_tab.clone()).unwrap_or_default(),
            "agent_status": status_str(aggregate(panes.iter().map(|p| self.pane_status(p)))),
            "worktree": w.and_then(|w| w.worktree.as_ref()).map(|m| m.info()),
        })
    }

    fn layout_snapshot(&self, tab: &str) -> Value {
        let t = self.state.tab(tab);
        let (panes, splits) = self.state.tab_rects(tab);
        let focused = t.map(|t| t.focused_pane.clone()).unwrap_or_default();
        json!({
            "workspace_id": t.map(|t| t.workspace_id.clone()).unwrap_or_default(),
            "tab_id": tab,
            "zoomed": t.is_some_and(|t| t.zoomed),
            "area": {"x": 0, "y": 0, "width": self.state.area.0, "height": self.state.area.1},
            "focused_pane_id": focused,
            "panes": panes.iter().map(|(p, r)| json!({"pane_id": p, "focused": *p == focused, "rect": r.json()})).collect::<Vec<_>>(),
            "splits": splits.iter().map(|(id, d, ratio, r)| json!({"id": id, "direction": split_dir_name(*d), "ratio": ratio, "rect": r.json()})).collect::<Vec<_>>(),
        })
    }

    fn layout_description(&self, tab: &str) -> Value {
        let t = self.state.tab(tab).cloned();
        let mut d = json!({
            "workspace_id": t.as_ref().map(|t| t.workspace_id.clone()).unwrap_or_default(),
            "tab_id": tab,
            "zoomed": t.as_ref().is_some_and(|t| t.zoomed),
            "focused_pane_id": t.as_ref().map(|t| t.focused_pane.clone()).unwrap_or_default(),
            "root": t.as_ref().map(|t| export_layout(&self.state, &t.layout)).unwrap_or(Value::Null),
        });
        if let Some(pattern) = t.as_ref().and_then(|t| t.pattern.clone()) {
            d["pattern"] = json!(pattern);
        }
        d
    }

    fn read_result(&mut self, id: &str, source: &str, format: &str, lines: Option<usize>) -> ApiResult {
        let lines = lines.map(|n| n.min(READ_MAX_LINES));
        let (ws, tab) = self.state.workspace_of(id).unwrap_or_default();
        let rt = self
            .panes
            .get_mut(id)
            .ok_or_else(|| err("pane_not_found", format!("pane {id} is not running")))?;
        let (text, truncated) = if source == "visible" && format == "ansi" {
            (
                String::from_utf8_lossy(&rt.emu.screen().contents_formatted()).into_owned(),
                false,
            )
        } else {
            rt.read_text(source, lines)
                .ok_or_else(|| err("invalid_params", format!("unknown source {source}")))?
        };
        let (text, cut) = cap_text(text, READ_MAX_BYTES);
        let truncated = truncated || cut;
        let source = source.replace('-', "_");
        Ok(json!({
            "pane_id": id, "workspace_id": ws, "tab_id": tab,
            "source": source, "format": if format == "ansi" { "ansi" } else { "text" },
            "text": text, "revision": rt.end(), "truncated": truncated,
        }))
    }

    /// `pane.read` source `history` (decision 0009): the lines before index
    /// `before` (default: the newest), at most `lines` (default 2,000), in
    /// one reply under [`READ_MAX_BYTES`]. `format` `ansi` (default) gives each
    /// line as the ANSI that draws it followed by CRLF, ready to write into a
    /// terminal; `text` gives plain lines joined by `\n`.
    fn history_result(&mut self, id: &str, p: &Value) -> ApiResult {
        let (ws, tab) = self.state.workspace_of(id).unwrap_or_default();
        let rt = self
            .panes
            .get_mut(id)
            .ok_or_else(|| err("pane_not_found", format!("pane {id} is not running")))?;
        let before = match &p["before"] {
            Value::Null => None,
            v => Some(
                v.as_u64()
                    .ok_or_else(|| err("invalid_params", "before must be a line index (a non-negative integer)"))?,
            ),
        };
        let lines = p["lines"]
            .as_u64()
            .map_or(HISTORY_PAGE_LINES, |n| (n as usize).clamp(1, READ_MAX_LINES));
        let ansi = s(p, "format").unwrap_or("ansi") != "text";
        // Room for the envelope; each line costs its JSON-encoded size plus CRLF.
        let page = rt
            .emu
            .history_page(before, lines, READ_MAX_BYTES - 4096)
            .ok_or_else(|| err("not_supported", "this pane keeps no history"))?;
        let text = if ansi {
            let mut out = String::new();
            for l in &page.lines {
                out.push_str(&String::from_utf8_lossy(&l.ansi));
                out.push_str("\r\n");
            }
            out
        } else {
            page.lines.iter().map(|l| l.text()).collect::<Vec<_>>().join("\n")
        };
        let limit = rt.emu.history().map_or(0, |h| h.limit());
        Ok(json!({
            "pane_id": id, "workspace_id": ws, "tab_id": tab,
            "source": "history", "format": if ansi { "ansi" } else { "text" },
            "text": text, "revision": rt.end(), "truncated": page.more,
            "history": {
                "first": page.first, "count": page.lines.len(), "start": page.start,
                "end": page.end, "limit": limit, "more": page.more,
            },
        }))
    }

    // ── targets ───────────────────────────────────────────────────────────

    fn pane_target(&self, p: &Value, key: &str) -> Result<String, ApiErr> {
        let id = s(p, key)
            .map(str::to_owned)
            .or_else(|| self.state.focused_pane().map(|p| p.pane.id.clone()))
            .ok_or_else(|| err("pane_not_found", "no panes"))?;
        if self.state.pane(&id).is_none() {
            return Err(err("pane_not_found", format!("no pane {id}")));
        }
        Ok(id)
    }

    /// Agent targets: a unique agent name, or a pane id hosting an agent.
    fn agent_target(&self, p: &Value) -> Result<String, ApiErr> {
        let t = req_str(p, "target")?;
        if self.state.pane(t).is_some() {
            return Ok(t.to_string());
        }
        let named: Vec<&str> = self
            .state
            .panes
            .iter()
            .filter(|r| r.meta.name.as_deref() == Some(t))
            .map(|r| r.pane.id.as_str())
            .collect();
        match named.as_slice() {
            [one] => Ok(one.to_string()),
            [] => Err(err("agent_not_found", format!("no agent {t}"))),
            _ => Err(err("ambiguous_target", format!("several agents are named {t}"))),
        }
    }

    /// Focus a pane (its tab and workspace too), emit the
    /// `workspace_focused` / `tab_focused` / `pane_focused` for whatever
    /// changed, and publish the topology (focus is per machine, shared by
    /// every client). Returns whether the pane focus changed.
    async fn focus_and_emit(&mut self, id: &str) -> Result<bool, ApiErr> {
        let (ws, tab) = self
            .state
            .workspace_of(id)
            .ok_or_else(|| err("pane_not_found", format!("no pane {id}")))?;
        let ws_was = self.state.focused_workspace().map(|w| w.id.clone());
        let tab_was = self.state.focused_tab().map(|t| t.id.clone());
        let changed = self.state.focus_pane(id)?;
        if ws_was.as_deref() != Some(ws.as_str()) {
            self.emit(Event::new("workspace_focused", json!({ "workspace_id": ws })));
        }
        if tab_was.as_deref() != Some(tab.as_str()) {
            self.emit(Event::new("tab_focused", json!({ "tab_id": tab, "workspace_id": ws })));
        }
        if changed {
            self.emit(Event::new("pane_focused", json!({ "pane_id": id, "workspace_id": ws })));
            self.commit_topology().await;
        }
        Ok(changed)
    }

    fn ws_target(&self, p: &Value) -> Result<String, ApiErr> {
        let id = req_str(p, "workspace_id")?;
        self.state
            .workspace(id)
            .ok_or_else(|| err("workspace_not_found", format!("no workspace {id}")))?;
        Ok(id.into())
    }

    fn tab_target(&self, p: &Value) -> Result<String, ApiErr> {
        let id = req_str(p, "tab_id")?;
        self.state
            .tab(id)
            .ok_or_else(|| err("tab_not_found", format!("no tab {id}")))?;
        Ok(id.into())
    }

    pub(crate) fn hid(&self, pane: &str) -> Result<String, ApiErr> {
        self.panes
            .get(pane)
            .filter(|r| r.alive)
            .map(|r| r.hid.clone())
            .ok_or_else(|| err("pane_not_running", format!("pane {pane} has no running process")))
    }

    // ── request entry ─────────────────────────────────────────────────────

    pub(crate) async fn request(&mut self, method: &str, params: Value, reply: ReplyTo) {
        let params = if params.is_null() { json!({}) } else { params };
        if crate::chat::METHODS.contains(&method) {
            return self.chat_request(method, &params, reply).await;
        }
        if crate::files::METHODS.contains(&method) {
            return self.files_request(method, &params, reply);
        }
        if tmuxedo_protocol::fs::is_fs_method(method) {
            return self.fs_request(method, &params, reply);
        }
        if tmuxedo_protocol::checkpoint::is_checkpoint_method(method) {
            return self.checkpoint_request(method, &params, reply);
        }
        if tmuxedo_protocol::review::is_review_method(method) {
            return self.review_request(method, &params, reply);
        }
        if crate::needs::METHODS.contains(&method) {
            return self.needs_request(method, &params, reply).await;
        }
        if crate::queued::METHODS.contains(&method) {
            return self.queued_request(method, &params, reply).await;
        }
        if crate::task::METHODS.contains(&method) {
            let r = self.task_request(method, &params).await;
            return respond(reply, r, self);
        }
        match self.dispatch(method, &params).await {
            Outcome::Now(r) => respond(reply, r, self),
            Outcome::Wait(kind, timeout) => {
                let w = Waiter {
                    kind,
                    deadline: timeout.map(|t| Instant::now() + t),
                    reply,
                };
                self.waiters.push(w);
                self.check_all_waiters();
            }
            Outcome::Subscribe(filters) => match reply {
                ReplyTo::Local(tx) => {
                    let (stx, srx) = mpsc::unbounded_channel();
                    self.subscribers.push(Subscriber { filters, tx: stx });
                    let _ = tx.send(LocalReply {
                        body: json!({ "result": { "type": "subscription_started" } }),
                        stream: Some(srx),
                    });
                }
                r @ ReplyTo::Gateway(_) => respond(
                    r,
                    Err(err(
                        "not_supported",
                        "events.subscribe streams on the local socket only",
                    )),
                    self,
                ),
            },
        }
    }

    async fn dispatch(&mut self, method: &str, p: &Value) -> Outcome {
        match self.dispatch_inner(method, p).await {
            Ok(o) => o,
            Err(e) => Outcome::Now(Err(e)),
        }
    }

    async fn dispatch_inner(&mut self, method: &str, p: &Value) -> Result<Outcome, ApiErr> {
        let now = |v: Value| Ok(Outcome::Now(Ok(v)));
        match method {
            "ping" => now(json!({
                "type": "pong", "protocol": tmuxedo_protocol::PROTOCOL_VERSION, "version": crate::VERSION, "capabilities": null,
            })),
            "server.agent_manifests" => {
                now(json!({ "type": "agent_manifest_status", "manifests": tmuxedo_detect::catalog_rows() }))
            }

            // ── workspaces ──
            "workspace.list" => {
                let ids: Vec<String> = self.state.workspaces_sorted().iter().map(|w| w.id.clone()).collect();
                now(
                    json!({ "type": "workspace_list", "workspaces": ids.iter().map(|w| self.workspace_info(w)).collect::<Vec<_>>() }),
                )
            }
            "workspace.get" => {
                let ws = self.ws_target(p)?;
                now(json!({ "type": "workspace_info", "workspace": self.workspace_info(&ws) }))
            }
            "workspace.create" => {
                let cwd = s(p, "cwd").map(str::to_owned).unwrap_or_else(|| self.work_dir());
                if !std::path::Path::new(&cwd).is_dir() {
                    return Err(err("invalid_cwd", format!("{cwd} is not a directory")));
                }
                let argv = self.default_argv();
                let (ws, tab, pane) =
                    self.state
                        .create_workspace(&cwd, s(p, "label").map(str::to_owned), argv, env_param(p));
                if p["focus"].as_bool() == Some(true) {
                    let _ = self.state.focus_pane(&pane);
                }
                self.spawn_pane(&pane, None);
                self.emit(Event::new(
                    "workspace_created",
                    json!({ "workspace": self.workspace_info(&ws) }),
                ));
                self.emit(Event::new("tab_created", json!({ "tab": self.tab_info(&tab) })));
                self.emit(Event::new("pane_created", json!({ "pane": self.pane_info(&pane) })));
                self.commit_topology().await;
                now(
                    json!({ "type": "workspace_created", "workspace": self.workspace_info(&ws), "tab": self.tab_info(&tab), "root_pane": self.pane_info(&pane) }),
                )
            }
            "workspace.focus" => {
                let ws = self.ws_target(p)?;
                if self.state.focus_workspace(&ws)? {
                    self.emit(Event::new("workspace_focused", json!({ "workspace_id": ws })));
                    self.commit_topology().await;
                }
                now(json!({ "type": "workspace_info", "workspace": self.workspace_info(&ws) }))
            }
            "workspace.rename" => {
                let ws = self.ws_target(p)?;
                // `label` must be given; null or a name that cleans to nothing restores the default.
                let label = rename_label(p)?.unwrap_or_else(|| crate::topology::default_workspace_name(&ws));
                self.state.workspace_mut(&ws).expect("checked").name = label.clone();
                self.emit(Event::new(
                    "workspace_renamed",
                    json!({ "workspace_id": ws, "label": label }),
                ));
                self.commit_topology().await;
                now(json!({ "type": "workspace_info", "workspace": self.workspace_info(&ws) }))
            }
            "workspace.move" => {
                let ws = self.ws_target(p)?;
                let idx = p["insert_index"]
                    .as_u64()
                    .ok_or_else(|| err("invalid_params", "insert_index is required"))?
                    as usize;
                self.state.move_workspace(&ws, idx)?;
                let all: Vec<Value> = self
                    .state
                    .workspaces_sorted()
                    .iter()
                    .map(|w| self.workspace_info(&w.id))
                    .collect();
                self.emit(Event::new(
                    "workspace_moved",
                    json!({ "workspace_id": ws, "insert_index": idx, "workspaces": all }),
                ));
                self.commit_topology().await;
                now(json!({ "type": "workspace_info", "workspace": self.workspace_info(&ws) }))
            }
            "workspace.close" => {
                let ws = self.ws_target(p)?;
                let closed = self.state.close_workspace(&ws)?;
                self.kill_panes(&closed.panes);
                self.emit_closed(&closed);
                if self.state.workspaces.is_empty() {
                    self.ensure_a_pane();
                }
                self.commit_topology().await;
                now(json!({ "type": "ok" }))
            }

            // ── tabs ──
            "tab.list" => {
                let tabs: Vec<String> = match s(p, "workspace_id") {
                    Some(ws) => self.state.tabs_of(ws).iter().map(|t| t.id.clone()).collect(),
                    None => self.state.protocol_tabs().into_iter().map(|t| t.id).collect(),
                };
                now(json!({ "type": "tab_list", "tabs": tabs.iter().map(|t| self.tab_info(t)).collect::<Vec<_>>() }))
            }
            "tab.get" => {
                let t = self.tab_target(p)?;
                now(json!({ "type": "tab_info", "tab": self.tab_info(&t) }))
            }
            "tab.create" => {
                let ws = match s(p, "workspace_id") {
                    Some(w) => w.to_string(),
                    None => self
                        .state
                        .focused_workspace()
                        .map(|w| w.id.clone())
                        .ok_or_else(|| err("workspace_not_found", "no workspace"))?,
                };
                let cwd = match s(p, "cwd") {
                    Some(c) => c.to_owned(),
                    None => self.new_pane_cwd(self.state.workspace(&ws).map(|w| w.cwd.clone())),
                };
                let argv = self.default_argv();
                let (tab, pane) =
                    self.state
                        .create_tab(&ws, &cwd, s(p, "label").map(str::to_owned), argv, env_param(p))?;
                if p["focus"].as_bool() == Some(true) {
                    let _ = self.state.focus_pane(&pane);
                }
                self.spawn_pane(&pane, None);
                self.emit(Event::new("tab_created", json!({ "tab": self.tab_info(&tab) })));
                self.emit(Event::new("pane_created", json!({ "pane": self.pane_info(&pane) })));
                self.commit_topology().await;
                now(json!({ "type": "tab_created", "tab": self.tab_info(&tab), "root_pane": self.pane_info(&pane) }))
            }
            "tab.focus" => {
                let t = self.tab_target(p)?;
                if self.state.focus_tab(&t)? {
                    let ws = self.state.tab(&t).map(|t| t.workspace_id.clone()).unwrap_or_default();
                    self.emit(Event::new("tab_focused", json!({ "tab_id": t, "workspace_id": ws })));
                    self.commit_topology().await;
                }
                now(json!({ "type": "tab_info", "tab": self.tab_info(&t) }))
            }
            "tab.rename" => {
                let t = self.tab_target(p)?;
                let label = rename_label(p)?.unwrap_or_else(|| crate::topology::default_tab_name(&t));
                let tr = self.state.tab_mut(&t).expect("checked");
                tr.name = label.clone();
                let ws = tr.workspace_id.clone();
                self.emit(Event::new(
                    "tab_renamed",
                    json!({ "tab_id": t, "workspace_id": ws, "label": label }),
                ));
                self.commit_topology().await;
                now(json!({ "type": "tab_info", "tab": self.tab_info(&t) }))
            }
            "tab.move" => {
                let t = self.tab_target(p)?;
                let idx = p["insert_index"]
                    .as_u64()
                    .ok_or_else(|| err("invalid_params", "insert_index is required"))?
                    as usize;
                self.state.move_tab(&t, idx)?;
                let ws = self.state.tab(&t).map(|t| t.workspace_id.clone()).unwrap_or_default();
                let tabs: Vec<Value> = self.state.tabs_of(&ws).iter().map(|x| self.tab_info(&x.id)).collect();
                self.emit(Event::new(
                    "tab_moved",
                    json!({ "tab_id": t, "workspace_id": ws, "insert_index": idx, "tabs": tabs }),
                ));
                self.commit_topology().await;
                now(json!({ "type": "tab_info", "tab": self.tab_info(&t) }))
            }
            "tab.close" => {
                let t = self.tab_target(p)?;
                let closed = self.state.close_tab(&t)?;
                self.kill_panes(&closed.panes);
                self.emit_closed(&closed);
                if self.state.workspaces.is_empty() {
                    self.ensure_a_pane();
                }
                self.commit_topology().await;
                now(json!({ "type": "ok" }))
            }

            // ── panes ──
            "pane.list" => {
                let ws = s(p, "workspace_id");
                let ids: Vec<String> = match ws {
                    Some(w) => self.state.panes_of_ws(w),
                    None => self
                        .state
                        .workspaces_sorted()
                        .iter()
                        .flat_map(|w| self.state.panes_of_ws(&w.id))
                        .collect(),
                };
                now(json!({ "type": "pane_list", "panes": ids.iter().map(|i| self.pane_info(i)).collect::<Vec<_>>() }))
            }
            "pane.get" => {
                let id = self.pane_target(p, "pane_id")?;
                now(json!({ "type": "pane_info", "pane": self.pane_info(&id) }))
            }
            "pane.focus" => {
                let id = self.pane_target(p, "pane_id")?;
                self.focus_and_emit(&id).await?;
                now(json!({ "type": "pane_info", "pane": self.pane_info(&id) }))
            }
            "pane.focus_direction" => {
                let src = self.pane_target(p, "pane_id")?;
                let d = Dir::parse(req_str(p, "direction")?)
                    .ok_or_else(|| err("invalid_params", "direction must be left, right, up or down"))?;
                let target = self.state.neighbor(&src, d);
                if let Some(t) = &target {
                    self.focus_and_emit(t).await?;
                }
                let tab = self.state.workspace_of(&src).map(|w| w.1).unwrap_or_default();
                let focused = self.state.tab(&tab).map(|t| t.focused_pane.clone());
                now(json!({
                    "type": "pane_focus_direction",
                    "focus": {
                        "changed": target.is_some(),
                        "source_pane_id": src,
                        "focused_pane_id": focused,
                        "layout": self.layout_snapshot(&tab),
                        "reason": if target.is_none() { json!("no_neighbor") } else { Value::Null },
                    },
                }))
            }
            "pane.current" => {
                let id = self.pane_target(p, "caller_pane_id")?;
                now(json!({ "type": "pane_current", "pane": self.pane_info(&id) }))
            }
            "pane.split" => {
                let target = match s(p, "target_pane_id") {
                    Some(t) => self.pane_target(&json!({ "pane_id": t }), "pane_id")?,
                    None => match s(p, "workspace_id") {
                        Some(ws) => {
                            let tab = self
                                .state
                                .workspace(ws)
                                .ok_or_else(|| err("workspace_not_found", format!("no workspace {ws}")))?
                                .active_tab
                                .clone();
                            self.state
                                .tab(&tab)
                                .map(|t| t.focused_pane.clone())
                                .ok_or_else(|| err("pane_not_found", "no pane"))?
                        }
                        None => self.pane_target(p, "target_pane_id")?,
                    },
                };
                let dir = split_dir_parse(req_str(p, "direction")?)
                    .ok_or_else(|| err("invalid_params", "direction must be right or down"))?;
                if let Some(c) = s(p, "cwd") {
                    if !std::path::Path::new(c).is_dir() {
                        return Err(err("invalid_cwd", format!("{c} is not a directory")));
                    }
                }
                let argv = self.default_argv();
                let new = self
                    .state
                    .split(&target, dir, p["ratio"].as_f64(), s(p, "cwd"), argv, env_param(p))?;
                if p["focus"].as_bool() == Some(true) {
                    let _ = self.state.focus_pane(&new);
                }
                self.spawn_pane(&new, None);
                self.emit(Event::new("pane_created", json!({ "pane": self.pane_info(&new) })));
                self.emit_layout(&new);
                self.commit_topology().await;
                now(json!({ "type": "pane_info", "pane": self.pane_info(&new) }))
            }
            "pane.close" => {
                let id = self.pane_target(p, "pane_id")?;
                let closed = self.state.close_pane(&id)?;
                self.kill_panes(&closed.panes);
                self.emit_closed(&closed);
                if self.state.workspaces.is_empty() {
                    self.ensure_a_pane();
                }
                self.commit_topology().await;
                now(json!({ "type": "ok" }))
            }
            "pane.rename" => {
                let id = self.pane_target(p, "pane_id")?;
                // Absent, null or a name that cleans to nothing clears it (the title shows again).
                self.state.pane_mut(&id).expect("checked").pane.label =
                    crate::topology::clean_label(s(p, "label").map(str::to_owned));
                self.emit(Event::new("pane_updated", json!({ "pane": self.pane_info(&id) })));
                self.commit_topology().await;
                now(json!({ "type": "pane_info", "pane": self.pane_info(&id) }))
            }
            "pane.read" if s(p, "source") == Some("history") => {
                let id = self.pane_target(p, "pane_id")?;
                let r = self.history_result(&id, p)?;
                now(json!({ "type": "pane_read", "read": r }))
            }
            "pane.read" => {
                let id = self.pane_target(p, "pane_id")?;
                let r = self.read_result(
                    &id,
                    s(p, "source").unwrap_or("visible"),
                    s(p, "format").unwrap_or("text"),
                    p["lines"].as_u64().map(|n| n as usize),
                )?;
                now(json!({ "type": "pane_read", "read": r }))
            }
            "pane.send_text" => {
                let id = self.pane_target(p, "pane_id")?;
                let text = p["text"]
                    .as_str()
                    .ok_or_else(|| err("invalid_params", "text is required"))?;
                let hid = self.hid(&id)?;
                self.release_on_input(&id);
                self.holder.input(&hid, text.as_bytes());
                now(json!({ "type": "ok" }))
            }
            "pane.send_keys" | "pane.send_input" => {
                let id = self.pane_target(p, "pane_id")?;
                let mut bytes = p["text"].as_str().unwrap_or_default().as_bytes().to_vec();
                for k in p["keys"].as_array().cloned().unwrap_or_default() {
                    let name = k
                        .as_str()
                        .ok_or_else(|| err("invalid_params", "keys must be strings"))?;
                    bytes.extend(
                        crate::keys::key_bytes(name)
                            .ok_or_else(|| err("invalid_key", format!("unknown key {name}")))?,
                    );
                }
                let hid = self.hid(&id)?;
                self.release_on_input(&id);
                self.holder.input(&hid, &bytes);
                now(json!({ "type": "ok" }))
            }
            "pane.resize" => {
                let id = self.pane_target(p, "pane_id")?;
                if let (Some(c), Some(r)) = (p["cols"].as_u64(), p["rows"].as_u64()) {
                    // tmuxedo extension: resize the PTY itself.
                    if !(1..=1000).contains(&c) || !(1..=1000).contains(&r) {
                        return Err(err("invalid_params", "cols and rows must be 1..1000"));
                    }
                    self.resize(&id, c as u16, r as u16).await;
                    return now(json!({ "type": "ok" }));
                }
                let d = Dir::parse(req_str(p, "direction")?)
                    .ok_or_else(|| err("invalid_params", "direction must be left, right, up or down"))?;
                let changed = self.state.resize_toward(&id, d, p["amount"].as_f64())?;
                if changed {
                    self.emit_layout(&id);
                    self.commit_topology().await;
                }
                let (_, tab) = self.state.workspace_of(&id).unwrap_or_default();
                let focused = self.state.tab(&tab).map(|t| t.focused_pane.clone()).unwrap_or_default();
                now(json!({ "type": "pane_resize", "resize": {
                    "changed": changed, "pane_id": id, "focused_pane_id": focused,
                    "layout": self.layout_snapshot(&tab), "reason": if changed { Value::Null } else { json!("unchanged") },
                }}))
            }
            "pane.zoom" => {
                let id = self.pane_target(p, "pane_id")?;
                let before = self.state.focused_pane().map(|p| p.pane.id.clone());
                let (changed, zoomed, reason) = self.state.zoom(&id, s(p, "mode").unwrap_or("toggle"))?;
                let (_, tab) = self.state.workspace_of(&id).unwrap_or_default();
                let focused = self.state.tab(&tab).map(|t| t.focused_pane.clone()).unwrap_or_default();
                if changed {
                    self.emit_layout(&id);
                    self.commit_topology().await;
                }
                now(json!({ "type": "pane_zoom", "zoom": {
                    "changed": changed, "zoom_changed": changed, "focus_changed": before.as_deref() != Some(focused.as_str()) && changed,
                    "pane_id": id, "focused_pane_id": focused, "zoomed": zoomed, "layout": self.layout_snapshot(&tab), "reason": reason,
                }}))
            }
            "pane.swap" => {
                let src = match s(p, "source_pane_id") {
                    Some(x) => self.pane_target(&json!({ "pane_id": x }), "pane_id")?,
                    None => self.pane_target(p, "pane_id")?,
                };
                let target = match (s(p, "target_pane_id"), s(p, "direction").and_then(Dir::parse)) {
                    (Some(t), _) => Some(t.to_string()),
                    (None, Some(d)) => self.state.neighbor(&src, d),
                    (None, None) => return Err(err("invalid_params", "give target_pane_id or direction")),
                };
                let reason = match &target {
                    None => Some("no_neighbor"),
                    Some(t) => self.state.swap(&src, t)?.err(),
                };
                let changed = reason.is_none();
                if changed {
                    self.emit_layout(&src);
                    self.commit_topology().await;
                }
                let (_, tab) = self.state.workspace_of(&src).unwrap_or_default();
                let focused = self.state.tab(&tab).map(|t| t.focused_pane.clone()).unwrap_or_default();
                now(json!({ "type": "pane_swap", "swap": {
                    "changed": changed, "source_pane_id": src, "target_pane_id": target,
                    "focused_pane_id": focused, "layout": self.layout_snapshot(&tab), "reason": reason,
                }}))
            }
            "pane.move" => Ok(Outcome::Now(self.pane_move(p).await)),
            "pane.wait_for_output" => {
                let id = self.pane_target(p, "pane_id")?;
                let matcher = OutputMatcher::parse(&p["match"])?;
                Ok(Outcome::Wait(
                    WaitKind::Output {
                        pane: id,
                        source: s(p, "source").unwrap_or("recent").to_string(),
                        lines: p["lines"].as_u64().map(|n| n as usize),
                        matcher,
                    },
                    p["timeout_ms"].as_u64().map(Duration::from_millis),
                ))
            }
            "pane.report_agent" => {
                let id = self.pane_target(p, "pane_id")?;
                let kind = req_str(p, "agent")?.to_string();
                let source = req_str(p, "source")?.to_string();
                let state = parse_status(req_str(p, "state")?)
                    .ok_or_else(|| err("invalid_params", "state must be idle, working, blocked or unknown"))?;
                self.record_session(&id, &kind, p);
                if let Some(rt) = self.panes.get_mut(&id) {
                    rt.track.report(&kind, state, &source);
                }
                self.agent_update(&id, false).await;
                now(json!({ "type": "ok" }))
            }
            "pane.report_agent_session" => {
                let id = self.pane_target(p, "pane_id")?;
                let kind = req_str(p, "agent")?.to_string();
                req_str(p, "source")?;
                self.record_session(&id, &kind, p);
                if let Some(rt) = self.panes.get_mut(&id) {
                    rt.track.reported_kind = Some(kind);
                }
                self.agent_update(&id, true).await;
                now(json!({ "type": "ok" }))
            }
            "pane.report_metadata" => {
                let id = self.pane_target(p, "pane_id")?;
                req_str(p, "source")?;
                let rec = self.state.pane_mut(&id).expect("checked");
                let m = &mut rec.meta;
                if p["clear_title"].as_bool() == Some(true) {
                    m.title = None;
                }
                if let Some(t) = s(p, "title") {
                    m.title = Some(t.into());
                }
                if p["clear_display_agent"].as_bool() == Some(true) {
                    m.display_agent = None;
                }
                if let Some(t) = s(p, "display_agent") {
                    m.display_agent = Some(t.into());
                }
                if p["clear_state_labels"].as_bool() == Some(true) {
                    m.state_labels.clear();
                }
                if let Some(o) = p["state_labels"].as_object() {
                    for (k, v) in o {
                        if let Some(v) = v.as_str() {
                            m.state_labels.insert(k.clone(), v.into());
                        }
                    }
                }
                if let Some(o) = p["tokens"].as_object() {
                    for (k, v) in o {
                        match v.as_str() {
                            Some(v) => m.tokens.insert(k.clone(), v.into()),
                            None => m.tokens.remove(k),
                        };
                    }
                }
                self.emit(Event::new("pane_updated", json!({ "pane": self.pane_info(&id) })));
                now(json!({ "type": "ok" }))
            }
            "pane.process_info" => {
                let id = self.pane_target(p, "pane_id")?;
                let pid = self.panes.get(&id).map(|r| r.pid).filter(|p| *p > 0);
                let job = pid.and_then(tmuxedo_detect::platform::foreground_job);
                let tty = pid.and_then(tmuxedo_detect::platform::tty);
                now(json!({ "type": "pane_process_info", "process_info": {
                    "pane_id": id, "shell_pid": pid, "tty": tty,
                    "foreground_process_group_id": job.as_ref().map(|j| j.pgid),
                    "foreground_processes": job.map(|j| j.processes.into_iter().map(|pr| {
                        let argv = (!pr.argv.is_empty()).then_some(&pr.argv);
                        json!({
                            "pid": pr.pid, "name": pr.name, "argv0": pr.argv.first(), "argv": argv,
                            "cmdline": argv.map(|a| a.join(" ")), "cwd": tmuxedo_detect::platform::cwd(pr.pid),
                        })
                    }).collect::<Vec<_>>()).unwrap_or_default(),
                }}))
            }
            "pane.clear" => {
                let id = self.pane_target(p, "pane_id")?;
                let hid = self.hid(&id)?;
                // Forget the emulator's history, then ask the program to repaint (Ctrl-L).
                if let Some(rt) = self.panes.get_mut(&id) {
                    rt.emu.reset();
                }
                self.holder.input(&hid, b"\x0c");
                now(json!({ "type": "ok" }))
            }

            // ── agents ──
            "agent.list" => {
                let ids: Vec<String> = self
                    .state
                    .workspaces_sorted()
                    .iter()
                    .flat_map(|w| self.state.panes_of_ws(&w.id))
                    .filter(|p| self.panes.get(p).is_some_and(|r| r.track.kind().is_some()))
                    .collect();
                now(
                    json!({ "type": "agent_list", "agents": ids.iter().map(|i| self.agent_info(i)).collect::<Vec<_>>() }),
                )
            }
            "agent.get" => {
                let id = self.agent_target(p)?;
                now(json!({ "type": "agent_info", "agent": self.agent_info(&id) }))
            }
            "agent.read" => {
                let id = self.agent_target(p)?;
                let r = self.read_result(
                    &id,
                    s(p, "source").unwrap_or("recent"),
                    s(p, "format").unwrap_or("text"),
                    p["lines"].as_u64().map(|n| n as usize),
                )?;
                now(json!({ "type": "pane_read", "read": r }))
            }
            "agent.send_keys" => {
                let id = self.agent_target(p)?;
                let mut q = p.clone();
                q["pane_id"] = json!(id);
                Box::pin(self.dispatch_inner("pane.send_keys", &q)).await
            }
            "agent.rename" => {
                let id = self.agent_target(p)?;
                self.state.pane_mut(&id).expect("checked").meta.name = s(p, "name").map(str::to_owned);
                self.dirty_persist();
                now(json!({ "type": "agent_info", "agent": self.agent_info(&id) }))
            }
            "agent.prompt" => {
                let id = self.agent_target(p)?;
                let text = req_str(p, "text")?;
                let hid = self.hid(&id)?;
                // Bracketed paste for the body, then Enter.
                let mut bytes = b"\x1b[200~".to_vec();
                bytes.extend_from_slice(text.as_bytes());
                bytes.extend_from_slice(b"\x1b[201~\r");
                self.holder.input(&hid, &bytes);
                if p["wait"].is_object() {
                    let until = parse_until(&p["wait"]["until"])?;
                    let seq = self.panes.get(&id).map(|r| r.track.state_change_seq);
                    return Ok(Outcome::Wait(
                        WaitKind::Agent {
                            pane: id,
                            until,
                            after_seq: seq,
                            prompted: true,
                        },
                        p["wait"]["timeout_ms"].as_u64().map(Duration::from_millis),
                    ));
                }
                now(json!({ "type": "agent_prompted", "agent": self.agent_info(&id) }))
            }
            "agent.wait" => {
                let id = self.agent_target(p)?;
                let until = parse_until(&p["until"])?;
                Ok(Outcome::Wait(
                    WaitKind::Agent {
                        pane: id,
                        until,
                        after_seq: None,
                        prompted: false,
                    },
                    p["timeout_ms"].as_u64().map(Duration::from_millis),
                ))
            }
            "agent.start" => {
                let id = self.pane_target(p, "pane_id")?;
                let kind = req_str(p, "kind")?.to_string();
                let name = req_str(p, "name")?.to_string();
                let agent = tmuxedo_detect::Agent::lookup(&kind)
                    .ok_or_else(|| err("unknown_agent_kind", format!("unknown agent kind {kind}")))?;
                let mut argv = vec![agent.launch().to_string()];
                // A role (`config.roles`): its arguments go before the caller's own.
                let role = match p.get("role").filter(|v| !v.is_null()) {
                    Some(r) => {
                        let want = r
                            .as_str()
                            .ok_or_else(|| err("invalid_params", "role is a role id (a string)"))?;
                        let (args, info) = self.role_launch(&id, agent.id(), want)?;
                        argv.extend(args);
                        Some(info)
                    }
                    None => None,
                };
                argv.extend(
                    p["args"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default()
                        .iter()
                        .filter_map(|a| a.as_str().map(str::to_owned)),
                );
                let hid = self.hid(&id)?;
                let line = argv.iter().map(|a| shell_word(a)).collect::<Vec<_>>().join(" ");
                self.holder.input(&hid, format!("{line}\r").as_bytes());
                let rec = self.state.pane_mut(&id).expect("checked");
                rec.meta.name = Some(name);
                rec.meta.kind = Some(agent.id().to_string());
                if let Some(rt) = self.panes.get_mut(&id) {
                    rt.track.reported_kind = Some(agent.id().to_string());
                }
                self.agent_update(&id, true).await;
                self.dirty_persist();
                let mut out = json!({ "type": "agent_started", "agent": self.agent_info(&id), "argv": argv });
                if let Some(info) = role {
                    out["role"] = info;
                }
                now(out)
            }
            "agent.explain" => {
                let id = self.agent_target(p)?;
                let rt = self
                    .panes
                    .get_mut(&id)
                    .ok_or_else(|| err("pane_not_running", "pane not running"))?;
                let kind = rt.track.kind().map(str::to_owned);
                let sig = rt.emu.signals().clone();
                let text = rt.emu.detection_text();
                let input = tmuxedo_detect::Input {
                    screen: &text,
                    title: &sig.title,
                    progress: &sig.progress,
                };
                let explain = tmuxedo_detect::explain_label(kind.as_deref(), &input).to_json();
                let src = format!("{:?}", rt.track.source).to_lowercase();
                let mut explain = explain;
                explain["tmuxedo"] = json!({
                    "effective_status": status_str(rt.track.effective), "source": src,
                    "reported": rt.track.reported.as_ref().map(|r| json!({"status": status_str(r.status), "source": r.source})),
                    "screen": rt.track.screen.map(status_str), "heuristic": status_str(rt.track.heuristic),
                    "process_kind": rt.track.process_kind,
                });
                // Same object under the pre-rename key (deprecated; for scripts).
                explain["aimux"] = explain["tmuxedo"].clone();
                now(json!({ "type": "agent_explain", "explain": explain }))
            }

            // ── events ──
            "events.subscribe" => {
                let subs = p["subscriptions"]
                    .as_array()
                    .ok_or_else(|| err("invalid_params", "subscriptions is required"))?;
                let filters = subs.iter().map(Filter::parse).collect::<Result<Vec<_>, _>>()?;
                Ok(Outcome::Subscribe(filters))
            }
            "events.wait" => {
                let m = p["match_event"].clone();
                if !m["event"].is_string() {
                    return Err(err("invalid_params", "match_event.event is required"));
                }
                Ok(Outcome::Wait(
                    WaitKind::Event { matcher: m },
                    p["timeout_ms"].as_u64().map(Duration::from_millis),
                ))
            }

            // ── layout ──
            "layout.export" => {
                let tab = self.layout_tab(p)?;
                now(json!({ "type": "layout_export", "layout": self.layout_description(&tab) }))
            }
            "layout.set_split_ratio" => {
                let tab = self.layout_tab(p)?;
                let path: Vec<bool> = p["path"]
                    .as_array()
                    .ok_or_else(|| err("invalid_params", "path is required"))?
                    .iter()
                    .map(|b| b.as_bool().unwrap_or(false))
                    .collect();
                let ratio = p["ratio"]
                    .as_f64()
                    .ok_or_else(|| err("invalid_params", "ratio is required"))?;
                self.state.set_split_ratio(&tab, &path, ratio)?;
                let snap = self.layout_snapshot(&tab);
                self.emit(Event::new("layout_updated", json!({ "layout": snap })));
                self.commit_topology().await;
                now(json!({ "type": "layout_split_ratio_set", "layout": self.layout_description(&tab) }))
            }
            "layout.apply" => Ok(Outcome::Now(self.layout_apply(p).await)),
            "layout.arrange" => Ok(Outcome::Now(self.layout_arrange(p).await)),
            "worktree.list" | "worktree.create" | "worktree.open" | "worktree.remove" => {
                Ok(Outcome::Now(self.worktree_method(method, p).await))
            }

            // Plugins (crate::plugin_api): the local socket answers them in
            // daemon::on_local; over the gateway web classifies them
            // (server/api-v1/methods.ts) and a client's `plugin_action`
            // arrives here as `plugin.action.invoke`.
            "plugin.list" | "plugin.enable" | "plugin.disable" => {
                Ok(Outcome::Now(self.plugin_request(method, p).await))
            }
            "plugin.action.list" | "plugin.action.invoke" | "plugin.log.list" => {
                Ok(Outcome::Now(self.plugin_request(method, p).await))
            }
            m => Err(err("unknown_method", format!("unknown method: {m}"))),
        }
    }

    fn dirty_persist(&mut self) {
        self.persist();
    }

    fn record_session(&mut self, id: &str, kind: &str, p: &Value) {
        let rec = self.state.pane_mut(id).expect("checked");
        rec.meta.kind = Some(kind.into());
        if let Some(sid) = s(p, "agent_session_id") {
            rec.meta.session_id = Some(sid.into());
            rec.meta.cwd_was_repo = crate::inject::is_repo(&rec.pane.cwd);
        }
        if let Some(path) = s(p, "agent_session_path") {
            rec.meta.session_path = Some(path.into());
        }
        if let Some(a) = p["resume_argv"].as_array() {
            rec.meta.resume_argv = Some(a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect());
        }
    }

    fn layout_tab(&self, p: &Value) -> Result<String, ApiErr> {
        if let Some(t) = s(p, "tab_id") {
            return self.tab_target(&json!({ "tab_id": t }));
        }
        if let Some(pane) = s(p, "pane_id") {
            return self
                .state
                .workspace_of(pane)
                .map(|w| w.1)
                .ok_or_else(|| err("pane_not_found", format!("no pane {pane}")));
        }
        self.state
            .focused_tab()
            .map(|t| t.id.clone())
            .ok_or_else(|| err("tab_not_found", "no tab"))
    }

    fn emit_layout(&mut self, pane: &str) {
        if let Some((_, tab)) = self.state.workspace_of(pane) {
            let snap = self.layout_snapshot(&tab);
            self.emit(Event::new("layout_updated", json!({ "layout": snap })));
        }
    }

    async fn pane_move(&mut self, p: &Value) -> ApiResult {
        let id = self.pane_target(p, "pane_id")?;
        let (prev_ws, prev_tab) = self.state.workspace_of(&id).unwrap_or_default();
        let dest = &p["destination"];
        let mut created_tab = Value::Null;
        let mut created_ws = Value::Null;
        let closed = match dest["type"].as_str() {
            Some("tab") => {
                let tab = req_str(dest, "tab_id")?;
                let dir = split_dir_parse(req_str(dest, "split")?)
                    .ok_or_else(|| err("invalid_params", "split must be right or down"))?;
                match self
                    .state
                    .move_pane_to_tab(&id, tab, s(dest, "target_pane_id"), dir, dest["ratio"].as_f64())
                {
                    Err(TopoError("same_tab", _)) => {
                        let snap = self.layout_snapshot(&prev_tab);
                        return Ok(json!({ "type": "pane_move", "move_result": {
                            "changed": false, "previous_pane_id": id, "previous_workspace_id": prev_ws, "previous_tab_id": prev_tab,
                            "pane": self.pane_info(&id), "target_layout": snap, "focused_pane_id": id, "reason": "same_tab",
                        }}));
                    }
                    other => other?,
                }
            }
            Some("new_tab") => {
                let (tab, closed) = self.state.move_pane_to_new_tab(
                    &id,
                    s(dest, "workspace_id"),
                    s(dest, "label").map(str::to_owned),
                )?;
                created_tab = self.tab_info(&tab);
                closed
            }
            Some("new_workspace") => {
                let (ws, _tab, closed) = self.state.move_pane_to_new_workspace(
                    &id,
                    s(dest, "label").map(str::to_owned),
                    s(dest, "tab_label").map(str::to_owned),
                )?;
                created_ws = self.workspace_info(&ws);
                closed
            }
            _ => {
                return Err(err(
                    "invalid_params",
                    "destination.type must be tab, new_tab or new_workspace",
                ))
            }
        };
        if p["focus"].as_bool() == Some(true) {
            let _ = self.state.focus_pane(&id);
        }
        let (_, new_tab) = self.state.workspace_of(&id).unwrap_or_default();
        let target_layout = self.layout_snapshot(&new_tab);
        let source_layout = if self.state.tab(&prev_tab).is_some() {
            self.layout_snapshot(&prev_tab)
        } else {
            Value::Null
        };
        let result = json!({
            "changed": true, "previous_pane_id": id, "previous_workspace_id": prev_ws, "previous_tab_id": prev_tab,
            "pane": self.pane_info(&id), "target_layout": target_layout, "source_layout": source_layout,
            "focused_pane_id": self.state.focused_pane().map(|p| p.pane.id.clone()).unwrap_or_default(),
            "closed_tab_id": closed.tabs.first().map(|t| t.0.clone()), "closed_workspace_id": closed.workspaces.first(),
            "created_tab": created_tab, "created_workspace": created_ws,
        });
        let mut ev = result.clone();
        ev.as_object_mut().expect("object").retain(|k, _| {
            matches!(
                k.as_str(),
                "previous_pane_id"
                    | "previous_workspace_id"
                    | "previous_tab_id"
                    | "pane"
                    | "closed_tab_id"
                    | "closed_workspace_id"
                    | "created_tab"
                    | "created_workspace"
            )
        });
        self.emit(Event::new("pane_moved", ev));
        for (t, w) in &closed.tabs {
            self.emit(Event::new("tab_closed", json!({ "tab_id": t, "workspace_id": w })));
        }
        for w in &closed.workspaces {
            self.emit(Event::new(
                "workspace_closed",
                json!({ "workspace_id": w, "workspace": null }),
            ));
        }
        self.commit_topology().await;
        Ok(json!({ "type": "pane_move", "move_result": result }))
    }

    /// Rearrange a tab's panes (runner API `layout.arrange`) by a caller's tree
    /// (`root`) or a built-in `pattern`. It never closes a pane and never
    /// starts or stops a process: a pane with no place either moves to a new
    /// tab (`extras: "new_tab"`) or the request is refused. All or nothing:
    /// every refusal leaves the topology as it was.
    async fn layout_arrange(&mut self, p: &Value) -> ApiResult {
        use crate::arrange;
        let bad = |m: &str| err("invalid_params", m);
        let present = |k: &str| p.get(k).filter(|v| !v.is_null());
        let tab = self.tab_target(p)?;
        let new_tab = match present("extras").map(|v| v.as_str()) {
            None | Some(Some("refuse")) => false,
            Some(Some("new_tab")) => true,
            Some(_) => return Err(bad("extras must be refuse or new_tab")),
        };
        let dry_run = match present("dry_run") {
            None => false,
            Some(v) => v.as_bool().ok_or_else(|| bad("dry_run must be true or false"))?,
        };
        if let Some(v) = present("expect_rev") {
            let rev = v.as_u64().ok_or_else(|| bad("expect_rev must be a whole number"))?;
            if rev != self.state.rev {
                return Err(err(
                    "changed",
                    format!("the topology is at rev {}, not {rev}", self.state.rev),
                ));
            }
        }
        let pattern = match present("pattern") {
            None => None,
            Some(v) => Some(
                v.as_str()
                    .and_then(arrange::pattern_name)
                    .ok_or_else(|| bad("pattern is not one of the built-in patterns"))?,
            ),
        };
        let area = match present("area") {
            None => self.state.area,
            Some(a) => {
                let side = |k: &str| {
                    a[k].as_u64()
                        .filter(|n| (1..=arrange::MAX_SIDE).contains(n))
                        .map(|n| n as u16)
                        .ok_or_else(|| bad("area needs a width and a height from 1 to 1000"))
                };
                (side("width")?, side("height")?)
            }
        };
        let focus = match present("focus") {
            None => None,
            Some(v) => Some(
                v.as_str()
                    .filter(|f| !f.is_empty() && f.len() <= arrange::MAX_ID)
                    .ok_or_else(|| bad("focus must be a pane id"))?,
            ),
        };
        let t = self.state.tab(&tab).expect("checked").clone();
        let (used, layout, extras) = match (present("root"), pattern) {
            (Some(root), hint) => {
                let layout = arrange::parse_root(root).map_err(|(c, m)| err(c, m))?;
                let named = arrange::leaves(&layout);
                let mut extras = arrange::reading_order(&t.layout, area);
                extras.retain(|p| !named.contains(p));
                (hint, layout, extras)
            }
            (None, Some(pattern)) => {
                let main = focus.unwrap_or(&t.focused_pane);
                let plan = arrange::plan(pattern, &t.layout, main, area);
                (Some(plan.pattern), plan.layout, plan.extras)
            }
            (None, None) => return Err(bad("root or pattern is required")),
        };
        if !extras.is_empty() && !new_tab {
            let (code, why) = match present("root") {
                Some(_) => ("would_close", "the layout leaves out"),
                None => ("wont_fit", "there is no room here for"),
            };
            return Err(err(
                code,
                format!(
                    "{why} {}: pass extras \"new_tab\" to move them to a new tab",
                    extras.join(", ")
                ),
            ));
        }
        let below_readable = arrange::below(&layout, area, arrange::READABLE);
        let undo_root = export_layout(&self.state, &t.layout);
        let extras_layout = arrange::extras_layout(&extras, area);
        // On a copy: a refusal part-way cannot leave half an arrangement.
        let mut next = self.state.clone();
        let done = next.arrange_tab(&tab, layout, &extras, extras_layout, focus, used)?;
        let before = std::mem::replace(&mut self.state, next);
        let mut result = json!({
            "type": "layout_arrange",
            "layout": self.layout_description(&tab),
            "pattern": used,
            "opened": [],
            "extras": extras,
            "extras_tab": done.extras_tab,
            "below_readable": below_readable,
            "dry_run": dry_run,
            "undo": null,
        });
        if dry_run {
            self.state = before;
            return Ok(result);
        }
        let snap = self.layout_snapshot(&tab);
        self.emit(Event::new("layout_updated", json!({ "layout": snap })));
        if let Some(x) = &done.extras_tab {
            self.emit(Event::new("tab_created", json!({ "tab": self.tab_info(x) })));
        }
        for other in &done.touched {
            let snap = self.layout_snapshot(other);
            self.emit(Event::new("layout_updated", json!({ "layout": snap })));
        }
        for (t, w) in &done.closed.tabs {
            self.emit(Event::new("tab_closed", json!({ "tab_id": t, "workspace_id": w })));
        }
        self.commit_topology().await;
        result["undo"] = json!({ "root": undo_root, "rev": self.state.rev });
        Ok(result)
    }

    /// What `layout.apply` is about to do to a tab, checked before it changes
    /// anything: the tree's size, a pane named twice, and (with
    /// `close_unnamed: false`) the panes it would close.
    fn layout_apply_check(&self, p: &Value) -> Result<(), ApiErr> {
        fn walk(node: &Value, depth: usize, leaves: &mut usize, named: &mut Vec<String>) -> Result<(), ApiErr> {
            if depth > crate::arrange::MAX_DEPTH {
                return Err(err("invalid_params", "root is nested too deeply"));
            }
            if node["type"].as_str() == Some("split") {
                walk(&node["first"], depth + 1, leaves, named)?;
                return walk(&node["second"], depth + 1, leaves, named);
            }
            *leaves += 1;
            if *leaves > crate::arrange::MAX_LEAVES {
                return Err(err("invalid_params", "root names too many panes"));
            }
            if let Some(id) = node["pane_id"].as_str() {
                if named.iter().any(|n| n == id) {
                    return Err(err("invalid_params", "root names a pane twice"));
                }
                named.push(id.to_string());
            }
            Ok(())
        }
        let mut named = vec![];
        walk(&p["root"], 0, &mut 0, &mut named)?;
        let close = match p.get("close_unnamed").filter(|v| !v.is_null()) {
            None => true,
            Some(v) => v
                .as_bool()
                .ok_or_else(|| err("invalid_params", "close_unnamed must be true or false"))?,
        };
        if let (false, Some(tab)) = (close, s(p, "tab_id")) {
            let mut left_out = self.state.panes_of_tab(tab);
            left_out.retain(|id| !named.contains(id));
            if !left_out.is_empty() {
                return Err(err(
                    "would_close",
                    format!("the layout leaves out {}", left_out.join(", ")),
                ));
            }
        }
        Ok(())
    }

    /// Build a tab from a `LayoutNode` (runner API): into `tab_id` (replacing its
    /// layout) or a new tab.
    ///
    /// DESTRUCTIVE: applied to an existing tab, every pane the tree does not
    /// name is closed and its process killed. `close_unnamed: false` refuses
    /// instead (`would_close`, nothing changes); the answer lists what was
    /// `closed`. To rearrange without closing anything use `layout.arrange`.
    async fn layout_apply(&mut self, p: &Value) -> ApiResult {
        self.layout_apply_check(p)?;
        let root = &p["root"];
        let (ws, tab, fresh) = match s(p, "tab_id") {
            Some(t) => {
                let t = self.tab_target(&json!({ "tab_id": t }))?;
                let ws = self.state.tab(&t).map(|x| x.workspace_id.clone()).unwrap_or_default();
                (ws, t, false)
            }
            None => {
                let ws = match s(p, "workspace_id") {
                    Some(w) => self.ws_target(&json!({ "workspace_id": w }))?,
                    None => self
                        .state
                        .focused_workspace()
                        .map(|w| w.id.clone())
                        .ok_or_else(|| err("workspace_not_found", "no workspace"))?,
                };
                (ws, String::new(), true)
            }
        };
        let cwd_default = self.new_pane_cwd(self.state.workspace(&ws).map(|w| w.cwd.clone()));
        let argv = self.default_argv();
        // Create the tab with a placeholder root pane, then rebuild its layout.
        let (tab, placeholder) = if fresh {
            let (t, p0) = self.state.create_tab(
                &ws,
                &cwd_default,
                s(p, "tab_label").map(str::to_owned),
                argv.clone(),
                BTreeMap::new(),
            )?;
            (t, Some(p0))
        } else {
            (tab, None)
        };
        let old: Vec<String> = self.state.panes_of_tab(&tab);
        let mut keep = vec![];
        let mut new_panes = vec![];
        let mut first_leaf = true;
        let layout = self.build_layout(
            root,
            &ws,
            &tab,
            &cwd_default,
            &argv,
            placeholder.as_deref(),
            &mut first_leaf,
            &mut keep,
            &mut new_panes,
        )?;
        let t = self.state.tab_mut(&tab).expect("tab");
        t.layout = layout;
        let mut leaves = vec![];
        crate::topology::layout_panes(&t.layout, &mut leaves);
        if !leaves.contains(&t.focused_pane) {
            t.focused_pane = leaves.first().cloned().unwrap_or_default();
        }
        // Panes the new layout does not name close (layout_apply_check let it).
        let dropped: Vec<String> = old.into_iter().filter(|o| !leaves.contains(o)).collect();
        if !dropped.is_empty() {
            crate::log!("layout.apply on {tab} closes {}", dropped.join(", "));
        }
        for d in &dropped {
            self.state.panes.retain(|r| &r.pane.id != d);
        }
        self.kill_panes(&dropped);
        for np in &new_panes {
            self.spawn_pane(np, None);
        }
        if let Some(p0) = placeholder.filter(|p0| leaves.contains(p0)) {
            self.spawn_pane(&p0, None);
        }
        if p["focus"].as_bool() == Some(true) {
            let _ = self.state.focus_tab(&tab);
        }
        let snap = self.layout_snapshot(&tab);
        self.emit(Event::new("layout_updated", json!({ "layout": snap })));
        self.commit_topology().await;
        Ok(json!({ "type": "layout_apply", "layout": self.layout_description(&tab), "closed": dropped }))
    }

    #[allow(clippy::too_many_arguments)]
    fn build_layout(
        &mut self,
        node: &Value,
        ws: &str,
        tab: &str,
        cwd_default: &str,
        argv: &[String],
        placeholder: Option<&str>,
        first_leaf: &mut bool,
        keep: &mut Vec<String>,
        new_panes: &mut Vec<String>,
    ) -> Result<Layout, ApiErr> {
        match node["type"].as_str() {
            Some("pane") => {
                if let Some(id) = s(node, "pane_id") {
                    let (_, t) = self
                        .state
                        .workspace_of(id)
                        .ok_or_else(|| err("pane_not_found", format!("no pane {id}")))?;
                    if t != tab {
                        return Err(err("cross_tab", format!("pane {id} is in another tab")));
                    }
                    keep.push(id.to_string());
                    return Ok(Layout::Pane { pane: id.into() });
                }
                let cwd = s(node, "cwd").unwrap_or(cwd_default).to_string();
                let cmd: Option<Vec<String>> = node["command"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect());
                if *first_leaf {
                    *first_leaf = false;
                    if let Some(p0) = placeholder {
                        let rec = self.state.pane_mut(p0).expect("placeholder");
                        rec.pane.cwd = cwd;
                        rec.pane.label = crate::topology::clean_label(s(node, "label").map(str::to_owned));
                        if let Some(c) = cmd {
                            rec.argv = c;
                        }
                        return Ok(Layout::Pane { pane: p0.into() });
                    }
                }
                // A fresh pane record: split the tab's first pane then detach it from the tree
                // (the caller installs the whole tree at once).
                let id = {
                    let w = self.state.workspace_mut(ws).expect("ws");
                    let id = format!("{}:p{}", w.id, w.next_pane);
                    w.next_pane += 1;
                    id
                };
                let (cols, rows) = self.state.area;
                self.state.panes.push(crate::topology::PaneRecord {
                    pane: tmuxedo_protocol::frames::Pane {
                        id: id.clone(),
                        tab_id: tab.into(),
                        cwd,
                        command: cmd.clone().unwrap_or_else(|| argv.to_vec()).join(" "),
                        title: argv.first().cloned().unwrap_or_default(),
                        cols,
                        rows,
                        exited: None,
                        agent: None,
                        label: crate::topology::clean_label(s(node, "label").map(str::to_owned)),
                    },
                    argv: cmd.unwrap_or_else(|| argv.to_vec()),
                    gen: 1,
                    base: 0,
                    high_water: 0,
                    env: env_param(node),
                    meta: Default::default(),
                });
                new_panes.push(id.clone());
                Ok(Layout::Pane { pane: id })
            }
            Some("split") => {
                let dir = split_dir_parse(node["direction"].as_str().unwrap_or("")).unwrap_or(SplitDir::H);
                let ratio = node["ratio"]
                    .as_f64()
                    .unwrap_or(0.5)
                    .clamp(crate::topology::MIN_RATIO, 1.0 - crate::topology::MIN_RATIO);
                let a = self.build_layout(
                    &node["first"],
                    ws,
                    tab,
                    cwd_default,
                    argv,
                    placeholder,
                    first_leaf,
                    keep,
                    new_panes,
                )?;
                let b = self.build_layout(
                    &node["second"],
                    ws,
                    tab,
                    cwd_default,
                    argv,
                    placeholder,
                    first_leaf,
                    keep,
                    new_panes,
                )?;
                Ok(Layout::Split {
                    dir,
                    ratio,
                    a: Box::new(a),
                    b: Box::new(b),
                })
            }
            _ => Err(err("invalid_params", "layout node type must be pane or split")),
        }
    }

    // ── waiters ───────────────────────────────────────────────────────────

    fn check_all_waiters(&mut self) {
        self.check_output_waiters();
        let panes: Vec<String> = self.panes.keys().cloned().collect();
        for p in panes {
            self.check_agent_waiters(&p);
        }
    }

    pub(crate) fn check_event_waiters(&mut self, ev: &Event) {
        let (done, keep): (Vec<Waiter>, Vec<Waiter>) = std::mem::take(&mut self.waiters)
            .into_iter()
            .partition(|w| matches!(&w.kind, WaitKind::Event { matcher } if event_matches(matcher, ev)));
        self.waiters = keep;
        for w in done {
            respond(
                w.reply,
                Ok(json!({ "type": "wait_matched", "event": ev.envelope() })),
                self,
            );
        }
    }

    pub(crate) fn check_agent_waiters(&mut self, pane: &str) {
        let Some(rt) = self.panes.get(pane) else { return };
        let (status, seq) = (rt.track.effective, rt.track.state_change_seq);
        let (done, keep): (Vec<Waiter>, Vec<Waiter>) =
            std::mem::take(&mut self.waiters)
                .into_iter()
                .partition(|w| match &w.kind {
                    WaitKind::Agent {
                        pane: p,
                        until,
                        after_seq,
                        ..
                    } => p == pane && until.contains(&status) && after_seq.is_none_or(|a| seq > a),
                    _ => false,
                });
        self.waiters = keep;
        for w in done {
            let prompted = matches!(w.kind, WaitKind::Agent { prompted: true, .. });
            let info = self.agent_info(pane);
            let body = if prompted {
                json!({ "type": "agent_prompted", "agent": info })
            } else {
                json!({ "type": "agent_info", "agent": info })
            };
            respond(w.reply, Ok(body), self);
        }
    }

    pub(crate) fn check_output_waiters(&mut self) {
        let mut done = vec![];
        let mut waiters = std::mem::take(&mut self.waiters);
        let mut i = 0;
        while i < waiters.len() {
            let hit = match &waiters[i].kind {
                WaitKind::Output {
                    pane,
                    source,
                    lines,
                    matcher,
                } => {
                    let found = self
                        .panes
                        .get_mut(pane)
                        .and_then(|rt| rt.read_text(source, *lines))
                        .and_then(|(text, _)| matcher.find(&text));
                    found.map(|l| (pane.clone(), source.clone(), *lines, l))
                }
                _ => None,
            };
            match hit {
                Some(h) => done.push((waiters.remove(i), h)),
                None => i += 1,
            }
        }
        self.waiters = waiters;
        for (w, (pane, source, lines, line)) in done {
            let read = self.read_result(&pane, &source, "text", lines);
            let res = read.map(|r| json!({ "type": "output_matched", "pane_id": pane, "revision": r["revision"], "read": r, "matched_line": line }));
            respond(w.reply, res, self);
        }
        // Output subscriptions.
        let mut lines_out = vec![];
        let mut subs = std::mem::take(&mut self.subscribers);
        for sub in subs.iter_mut() {
            for f in sub.filters.iter_mut() {
                if let Filter::Output {
                    pane,
                    source,
                    lines,
                    matcher,
                    last,
                } = f
                {
                    let Some(rt) = self.panes.get_mut(pane.as_str()) else {
                        continue;
                    };
                    let Some((text, _)) = rt.read_text(source, *lines) else {
                        continue;
                    };
                    if let Some(l) = matcher.find(&text) {
                        if last.as_deref() != Some(l.as_str()) {
                            *last = Some(l.clone());
                            lines_out.push((sub.tx.clone(), pane.clone(), source.clone(), *lines, l));
                        }
                    }
                }
            }
        }
        self.subscribers = subs;
        for (tx, pane, source, lines, l) in lines_out {
            if let Ok(read) = self.read_result(&pane, &source, "text", lines) {
                let _ = tx.send(json!({ "event": "pane.output_matched", "data": { "pane_id": pane, "matched_line": l, "read": read } }).to_string());
            }
        }
    }

    pub(crate) fn expire_waiters(&mut self, now: Instant) {
        if !self.waiters.iter().any(|w| w.deadline.is_some_and(|d| d <= now)) {
            return;
        }
        let (done, keep): (Vec<Waiter>, Vec<Waiter>) = std::mem::take(&mut self.waiters)
            .into_iter()
            .partition(|w| w.deadline.is_some_and(|d| d <= now));
        self.waiters = keep;
        for w in done {
            respond(w.reply, Err(err("timeout", "timed out waiting")), self);
        }
    }

    pub(crate) fn fail_pane_waiters(&mut self, pane: &str) {
        let (done, keep): (Vec<Waiter>, Vec<Waiter>) =
            std::mem::take(&mut self.waiters)
                .into_iter()
                .partition(|w| match &w.kind {
                    WaitKind::Output { pane: p, .. } | WaitKind::Agent { pane: p, .. } => p == pane,
                    WaitKind::Event { .. } => false,
                });
        self.waiters = keep;
        for w in done {
            respond(
                w.reply,
                Err(err("agent_not_running", format!("pane {pane} closed"))),
                self,
            );
        }
    }
}

fn parse_until(v: &Value) -> Result<Vec<AgentStatus>, ApiErr> {
    let list: Vec<AgentStatus> = match v.as_array() {
        Some(a) => a
            .iter()
            .map(|x| {
                x.as_str()
                    .and_then(parse_status)
                    .ok_or_else(|| err("invalid_params", "until holds agent statuses"))
            })
            .collect::<Result<_, _>>()?,
        None => vec![],
    };
    Ok(if list.is_empty() {
        vec![AgentStatus::Idle, AgentStatus::Done]
    } else {
        list
    })
}

impl Daemon {
    /// `agent.start {role}`: the role `want` means for the pane's folder
    /// (`config.roles`, nearest scope already resolved by web), checked
    /// against the agent, as launch arguments (crate::role_launch) and what
    /// the reply says about it. Its instructions file is written here.
    fn role_launch(&self, pane: &str, plugin: &str, want: &str) -> Result<(Vec<String>, Value), ApiErr> {
        let unknown = || {
            let shown: String = want.chars().take(40).collect();
            err(
                "unknown_role",
                format!("No role {shown:?} here: it is not one of the roles this machine has for this folder."),
            )
        };
        let set = crate::role_set::load(&self.opts.state_dir).ok_or_else(unknown)?;
        let cwd = self.state.pane(pane).map(|p| p.pane.cwd.clone()).unwrap_or_default();
        let keys = crate::role_set::keys(&set);
        let project = self
            .projects
            .project_for(std::path::Path::new(&cwd), &self.delivery.machine, &keys);
        let e = set.find(project.as_deref(), want).ok_or_else(unknown)?;
        if !crate::role_launch::runs_on(&e.role, plugin) {
            return Err(err(
                "role_cli_mismatch",
                format!("Role {:?} is written for {}, not {plugin}.", e.role.id, e.role.cli),
            ));
        }
        let l = crate::role_launch::pane_launch(plugin, &e.role, &self.opts.state_dir.join("roles"));
        if let Some((path, text)) = &l.file {
            crate::config::write_private(path, text.as_bytes()).map_err(|m| {
                err(
                    "role_unavailable",
                    format!("The role's instructions could not be written: {m}"),
                )
            })?;
        }
        let info = json!({
            "id": e.role.id, "ref": e.r#ref, "version": e.version,
            "applied": l.applied, "notApplied": l.not_applied,
        });
        Ok((l.args, info))
    }
}

fn shell_word(a: &str) -> String {
    if a.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=:@".contains(c)) && !a.is_empty() {
        a.to_string()
    } else {
        format!("'{}'", a.replace('\'', "'\\''"))
    }
}
