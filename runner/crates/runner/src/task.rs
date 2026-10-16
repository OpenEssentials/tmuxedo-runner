//! Headless tasks (docs/design/headless-tasks.md): a command or an agent the
//! server starts for an automation client, inside a policy its owner set.
//!
//! The server decides the policy; this side is the machine's own consent and
//! a second check of what reaches it:
//! - nothing runs unless the machine's owner gave it automation labels
//!   (`runner.toml` `automation_labels`, or `--automation-label`), and the
//!   request names one of them;
//! - a command runs directly (no shell), its program given by absolute path
//!   and resolved (symlinks followed) to one of the allowed programs;
//! - the work folder, resolved, lies under one of the allowed roots;
//! - an agent never runs in Auto-approve; a permission it asks for fails the
//!   task with what it needed (nobody is there to answer it).
//!
//! A task is an ordinary pane or chat: people see it, watch it and can take
//! it over. Tasks are kept in memory; a runner restart forgets them (the
//! server then reports them as lost).

use crate::api::{err, ApiErr, ApiResult};
use crate::daemon::Daemon;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tmuxedo_protocol::frames::{valid_automation_label, Automation, MAX_AUTOMATION_LABELS};

/// Method names (schema, guardrails and web's method table).
pub const METHODS: &[&str] = &["task.start", "task.get", "task.list", "task.cancel"];
/// The most bytes of a result (output tail or the agent's last message).
pub const RESULT_MAX: usize = 64 * 1024;
/// Lines of a command's output kept as its result.
const TAIL_LINES: usize = 200;
/// Finished tasks remembered for `task.get` (the oldest go first).
const KEEP_FINISHED: usize = 200;
/// Tasks running at once on this machine, whatever the server allows.
pub const MAX_RUNNING: usize = 32;
/// The longest a task may run (24 h) and the default (1 h).
pub const MAX_TIMEOUT_MS: u64 = 24 * 3600 * 1000;
const DEFAULT_TIMEOUT_MS: u64 = 3600 * 1000;
/// Agent modes a task may use: never Auto-approve.
const AGENT_MODES: &[&str] = &["ask_first", "plan_first", "auto_edit"];
const MAX_ARGS: usize = 256;
const MAX_ARG_BYTES: usize = 4096;
const MAX_LIST: usize = 64;

/// The machine's consent as the hello carries it: valid labels only, none = no automation.
pub fn consent(labels: &[String]) -> Option<Automation> {
    let mut out: Vec<String> = vec![];
    for l in labels {
        let l = l.trim().to_ascii_lowercase();
        if valid_automation_label(&l) && !out.contains(&l) && out.len() < MAX_AUTOMATION_LABELS {
            out.push(l);
        }
    }
    (!out.is_empty()).then_some(Automation { labels: out })
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Kind {
    Command { pane_id: String },
    Agent { session_id: String },
}

#[derive(Debug, Clone)]
pub struct Task {
    id: String,
    kind: Kind,
    label: String,
    cwd: String,
    started_at: String,
    deadline: Instant,
    /// `running` | `done` | `failed` | `cancelled` | `timed_out`.
    status: &'static str,
    reason_code: Option<&'static str>,
    reason: Option<String>,
    exit_code: Option<i32>,
    signal: Option<String>,
    output: Option<String>,
    output_truncated: bool,
    ended_at: Option<String>,
    finished_seq: u64,
}

impl Task {
    fn running(&self) -> bool {
        self.status == "running"
    }

    fn info(&self) -> Value {
        let (kind, pane, session) = match &self.kind {
            Kind::Command { pane_id } => ("command", Some(pane_id.as_str()), None),
            Kind::Agent { session_id } => ("agent", None, Some(session_id.as_str())),
        };
        json!({
            "task_id": self.id, "kind": kind, "label": self.label, "cwd": self.cwd,
            "status": self.status, "reason_code": self.reason_code, "reason": self.reason,
            "exit_code": self.exit_code, "signal": self.signal,
            "output": self.output, "output_truncated": self.output_truncated,
            "pane_id": pane, "session_id": session,
            "started_at": self.started_at, "ended_at": self.ended_at,
        })
    }

    fn finish(&mut self, status: &'static str, code: Option<&'static str>, reason: Option<String>, seq: u64) {
        if !self.running() {
            return;
        }
        self.status = status;
        self.reason_code = code;
        self.reason = reason;
        self.ended_at = Some(crate::util::iso_now());
        self.finished_seq = seq;
    }
}

#[derive(Default)]
pub struct Tasks {
    map: BTreeMap<String, Task>,
    seq: u64,
}

impl Tasks {
    fn running(&self) -> usize {
        self.map.values().filter(|t| t.running()).count()
    }

    fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    /// Forget the oldest finished tasks beyond [`KEEP_FINISHED`].
    fn trim(&mut self) {
        let mut done: Vec<(u64, String)> = self
            .map
            .values()
            .filter(|t| !t.running())
            .map(|t| (t.finished_seq, t.id.clone()))
            .collect();
        if done.len() <= KEEP_FINISHED {
            return;
        }
        done.sort();
        for (_, id) in done.iter().take(done.len() - KEEP_FINISHED) {
            self.map.remove(id);
        }
    }
}

fn valid_task_id(id: &str) -> bool {
    (1..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn str_list(p: &Value, key: &str, max: usize) -> Result<Vec<String>, ApiErr> {
    let a = p[key]
        .as_array()
        .ok_or_else(|| err("invalid_params", format!("{key} must be a list of strings")))?;
    if a.len() > max {
        return Err(err("invalid_params", format!("{key} holds more than {max} items")));
    }
    a.iter()
        .map(|v| {
            v.as_str()
                .filter(|s| !s.contains('\0') && s.len() <= MAX_ARG_BYTES)
                .map(str::to_owned)
                .ok_or_else(|| err("invalid_params", format!("{key} must be a list of strings")))
        })
        .collect()
}

/// An absolute path, resolved (symlinks followed); None when it does not exist.
fn resolve(p: &str) -> Option<PathBuf> {
    let path = Path::new(p);
    if !path.is_absolute() {
        return None;
    }
    path.canonicalize().ok()
}

/// The program `argv0` names, resolved, if it is one of `allowed` (also resolved).
pub fn allowed_program(argv0: &str, allowed: &[String]) -> Result<PathBuf, ApiErr> {
    if !Path::new(argv0).is_absolute() {
        return Err(err(
            "command_not_allowed",
            "the program must be given by its absolute path",
        ));
    }
    let real = resolve(argv0).filter(|r| r.is_file()).ok_or_else(|| {
        err(
            "command_not_allowed",
            format!("{argv0} is not a program on this machine"),
        )
    })?;
    if allowed.iter().filter_map(|a| resolve(a)).any(|a| a == real) {
        Ok(real)
    } else {
        Err(err(
            "command_not_allowed",
            format!("{argv0} is not on the account's command list"),
        ))
    }
}

/// The work folder, resolved, if it lies under one of `roots` (also resolved).
pub fn allowed_cwd(cwd: &str, roots: &[String]) -> Result<PathBuf, ApiErr> {
    let real = resolve(cwd)
        .filter(|r| r.is_dir())
        .ok_or_else(|| err("cwd_not_allowed", format!("{cwd} is not a folder on this machine")))?;
    if roots.iter().filter_map(|r| resolve(r)).any(|r| real.starts_with(&r)) {
        Ok(real)
    } else {
        Err(err(
            "cwd_not_allowed",
            format!("{cwd} is outside the account's work roots"),
        ))
    }
}

impl Daemon {
    pub(crate) async fn task_request(&mut self, method: &str, p: &Value) -> ApiResult {
        match method {
            "task.start" => self.task_start(p).await,
            "task.get" => {
                let id = p["task_id"].as_str().unwrap_or("");
                self.tasks
                    .map
                    .get(id)
                    .map(|t| json!({ "type": "task_info", "task": t.info() }))
                    .ok_or_else(|| err("task_not_found", format!("no task {id}")))
            }
            "task.list" => {
                let tasks: Vec<Value> = self.tasks.map.values().map(Task::info).collect();
                Ok(json!({ "type": "task_list", "tasks": tasks }))
            }
            "task.cancel" => {
                let id = p["task_id"].as_str().unwrap_or("").to_string();
                if !self.tasks.map.contains_key(&id) {
                    return Err(err("task_not_found", format!("no task {id}")));
                }
                self.task_stop(&id, "cancelled", None, None).await;
                Ok(json!({ "type": "task_info", "task": self.tasks.map[&id].info() }))
            }
            _ => Err(err("method_not_found", format!("unknown method {method}"))),
        }
    }

    async fn task_start(&mut self, p: &Value) -> ApiResult {
        // The machine's own consent comes first: the server cannot turn it on.
        let Some(own) = consent(&self.opts.automation_labels) else {
            return Err(err(
                "automation_disabled",
                "this machine's owner has not turned automation on",
            ));
        };
        // Observe-only means nothing new runs here, from anyone.
        if self.opts.guardrails.observe_only {
            return Err(err("observe_only", "this machine is observe-only"));
        }
        let asked = str_list(p, "labels", MAX_AUTOMATION_LABELS)?;
        if !asked.iter().any(|l| own.labels.contains(l)) {
            return Err(err(
                "automation_not_allowed",
                "none of the policy's labels is one this machine's owner gave it",
            ));
        }
        let id = p["task_id"].as_str().unwrap_or("").to_string();
        if !valid_task_id(&id) {
            return Err(err("invalid_params", "task_id must be 1 to 64 of A-Z a-z 0-9 - _"));
        }
        if self.tasks.map.contains_key(&id) {
            return Err(err("task_exists", format!("task {id} exists")));
        }
        if self.tasks.running() >= MAX_RUNNING {
            return Err(err("limit_reached", format!("{MAX_RUNNING} tasks already run here")));
        }
        let timeout_ms = match &p["timeout_ms"] {
            Value::Null => DEFAULT_TIMEOUT_MS,
            v => v
                .as_u64()
                .filter(|t| (1000..=MAX_TIMEOUT_MS).contains(t))
                .ok_or_else(|| err("invalid_params", "timeout_ms must be 1000 to 86400000"))?,
        };
        let roots = str_list(p, "roots", MAX_LIST)?;
        let cwd = p["cwd"]
            .as_str()
            .ok_or_else(|| err("invalid_params", "cwd is required"))?;
        let cwd = allowed_cwd(cwd, &roots)?.to_string_lossy().into_owned();
        let label = p["label"]
            .as_str()
            .map(|l| l.chars().filter(|c| !c.is_control()).take(60).collect::<String>())
            .filter(|l| !l.trim().is_empty())
            .unwrap_or_else(|| format!("Task {}", &id[..id.len().min(8)]));
        let kind = match p["kind"].as_str() {
            Some("command") => {
                let argv = str_list(p, "argv", MAX_ARGS)?;
                let first = argv
                    .first()
                    .ok_or_else(|| err("invalid_params", "argv must name a program"))?;
                let allowed = str_list(p, "allowed_commands", MAX_LIST)?;
                let program = allowed_program(first, &allowed)?;
                let mut argv = argv;
                argv[0] = program.to_string_lossy().into_owned();
                let (ws, tab, pane) = self
                    .state
                    .create_workspace(&cwd, Some(label.clone()), argv, BTreeMap::new());
                self.spawn_pane(&pane, None);
                self.emit(crate::events::Event::new(
                    "workspace_created",
                    json!({ "workspace": self.workspace_info(&ws) }),
                ));
                self.emit(crate::events::Event::new(
                    "tab_created",
                    json!({ "tab": self.tab_info(&tab) }),
                ));
                self.emit(crate::events::Event::new(
                    "pane_created",
                    json!({ "pane": self.pane_info(&pane) }),
                ));
                self.commit_topology().await;
                Kind::Command { pane_id: pane }
            }
            Some("agent") => {
                let agent = p["agent"]
                    .as_str()
                    .ok_or_else(|| err("invalid_params", "agent is required"))?;
                let allowed = str_list(p, "allowed_agents", MAX_LIST)?;
                if !allowed.iter().any(|a| a == agent) {
                    return Err(err(
                        "agent_not_allowed",
                        format!("{agent} is not on the account's agent list"),
                    ));
                }
                let mode = p["mode"].as_str().unwrap_or("ask_first");
                if !AGENT_MODES.contains(&mode) {
                    return Err(err(
                        "mode_not_allowed",
                        format!("a task's agent runs in {}", AGENT_MODES.join(", ")),
                    ));
                }
                let text = p["text"]
                    .as_str()
                    .filter(|t| !t.trim().is_empty())
                    .ok_or_else(|| err("invalid_params", "text (the prompt) is required"))?;
                let r = self
                    .chat_start(&json!({
                        "agent": agent, "mode": mode, "cwd": cwd, "text": text, "title": label,
                    }))
                    .await?;
                let sid = r["session"]["id"]
                    .as_str()
                    .ok_or_else(|| err("internal", "the chat has no id"))?
                    .to_string();
                Kind::Agent { session_id: sid }
            }
            _ => return Err(err("invalid_params", "kind must be command or agent")),
        };
        crate::log!("task {id}: started ({})", p["kind"].as_str().unwrap_or(""));
        let t = Task {
            id: id.clone(),
            kind,
            label,
            cwd,
            started_at: crate::util::iso_now(),
            deadline: Instant::now() + Duration::from_millis(timeout_ms),
            status: "running",
            reason_code: None,
            reason: None,
            exit_code: None,
            signal: None,
            output: None,
            output_truncated: false,
            ended_at: None,
            finished_seq: 0,
        };
        self.tasks.map.insert(id.clone(), t);
        Ok(json!({ "type": "task_info", "task": self.tasks.map[&id].info() }))
    }

    /// End a running task: stop its pane or close its chat.
    async fn task_stop(&mut self, id: &str, status: &'static str, code: Option<&'static str>, reason: Option<String>) {
        let Some(t) = self.tasks.map.get(id) else { return };
        if !t.running() {
            return;
        }
        let kind = t.kind.clone();
        let seq = self.tasks.next_seq();
        if let Some(t) = self.tasks.map.get_mut(id) {
            t.finish(status, code, reason, seq);
        }
        match kind {
            Kind::Command { pane_id } => {
                if let Some((text, cut)) = self.task_tail(&pane_id) {
                    if let Some(t) = self.tasks.map.get_mut(id) {
                        t.output = Some(text);
                        t.output_truncated = cut;
                    }
                }
                if let Ok(closed) = self.state.close_pane(&pane_id) {
                    self.kill_panes(&closed.panes);
                    self.emit_closed(&closed);
                    if self.state.workspaces.is_empty() {
                        self.ensure_a_pane();
                    }
                    self.commit_topology().await;
                }
            }
            Kind::Agent { session_id } => {
                let _ = self.chat_close(&json!({ "session_id": session_id })).await;
            }
        }
        crate::log!("task {id}: {status}");
        self.tasks.trim();
    }

    /// The last lines a command printed, masked like every pane.
    fn task_tail(&mut self, pane_id: &str) -> Option<(String, bool)> {
        let rt = self.panes.get_mut(pane_id)?;
        let (text, more) = rt.read_text("recent", Some(TAIL_LINES))?;
        let text = self.secrets.set().mask_str(&text);
        let text = crate::chat::mask::mask_shapes(&text);
        let cut = text.len() > RESULT_MAX;
        let text = if cut {
            let mut i = text.len() - RESULT_MAX;
            while !text.is_char_boundary(i) {
                i += 1;
            }
            text[i..].to_string()
        } else {
            text
        };
        Some((text, more || cut))
    }

    /// Whether a running task's command lives in this pane.
    pub(crate) fn task_runs_pane(&self, pane_id: &str) -> bool {
        self.tasks
            .map
            .values()
            .any(|t| t.running() && matches!(&t.kind, Kind::Command { pane_id: p } if p == pane_id))
    }

    /// Handle a task pane's exit a moment later, so output still in flight is read first.
    pub(crate) fn task_exit_later(
        &self,
        pane_id: String,
        hid: String,
        code: Option<i32>,
        signal: Option<String>,
        tries: u32,
    ) {
        let tx = self.internal_tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            let _ = tx.send(crate::daemon::Internal::TaskExit {
                pane_id,
                hid,
                code,
                signal,
                tries,
            });
        });
    }

    /// A pane's process exited: if it was a task's command, keep its result.
    pub(crate) fn task_pane_exited(&mut self, pane_id: &str, code: Option<i32>, signal: Option<String>) {
        let Some(id) = self
            .tasks
            .map
            .values()
            .find(|t| matches!(&t.kind, Kind::Command { pane_id: p } if p == pane_id))
            .map(|t| t.id.clone())
        else {
            return;
        };
        let tail = self.task_tail(pane_id);
        let seq = self.tasks.next_seq();
        let Some(t) = self.tasks.map.get_mut(&id) else { return };
        if t.output.is_none() {
            if let Some((text, cut)) = tail {
                t.output = Some(text);
                t.output_truncated = cut;
            }
        }
        t.exit_code = code;
        t.signal = signal;
        if t.running() {
            let (status, rc) = if code == Some(0) {
                ("done", None)
            } else {
                ("failed", Some("exit_status"))
            };
            t.finish(status, rc, None, seq);
            crate::log!("task {id}: {status} (code {code:?})");
        }
        self.tasks.trim();
    }

    /// Every few seconds: time-outs, agents that asked for a permission or finished.
    pub(crate) async fn task_tick(&mut self) {
        let now = Instant::now();
        let mut stops: Vec<Stop> = vec![];
        let mut done: Vec<Done> = vec![];
        for t in self.tasks.map.values().filter(|t| t.running()) {
            if now >= t.deadline {
                stops.push((t.id.clone(), "timed_out", Some("timeout"), None));
                continue;
            }
            match &t.kind {
                Kind::Command { pane_id } => {
                    // The process could not start: the pane went without an exit.
                    if self.state.pane(pane_id).is_none() && !self.panes.contains_key(pane_id) {
                        done.push((
                            t.id.clone(),
                            "failed",
                            Some("spawn_failed"),
                            Some("the program could not start".into()),
                            String::new(),
                        ));
                    }
                }
                Kind::Agent { session_id } => {
                    let Some(s) = self.chats.sessions.get(session_id) else {
                        done.push((t.id.clone(), "failed", Some("lost"), None, String::new()));
                        continue;
                    };
                    if let Some(n) = s.needs.values().next() {
                        // Nobody answers for an automation: deny and fail, saying what it needed.
                        stops.push((
                            t.id.clone(),
                            "failed",
                            Some("permission_needed"),
                            Some(format!("needed permission: {}", n.item.title)),
                        ));
                        continue;
                    }
                    match s.status() {
                        "terminal" => done.push((
                            t.id.clone(),
                            "failed",
                            Some("taken_over"),
                            Some("a person took the session to a terminal".into()),
                            String::new(),
                        )),
                        "closed" => done.push((t.id.clone(), "failed", Some("closed"), None, String::new())),
                        "idle" | "parked" if s.meta.turns > 0 && s.meta.turn.is_none() => {
                            done.push((t.id.clone(), "done", None, None, s.last_reply.clone()))
                        }
                        _ => {}
                    }
                }
            }
        }
        for (id, status, code, reason, output) in done {
            let seq = self.tasks.next_seq();
            if let Some(t) = self.tasks.map.get_mut(&id) {
                if matches!(t.kind, Kind::Agent { .. }) && status == "done" {
                    t.output = Some(output);
                }
                t.finish(status, code, reason, seq);
                crate::log!("task {id}: {status}");
            }
        }
        for (id, status, code, reason) in stops {
            self.task_stop(&id, status, code, reason).await;
        }
        self.tasks.trim();
    }
}

/// A task to stop: id, status, reason code, reason.
type Stop = (String, &'static str, Option<&'static str>, Option<String>);
/// A task already over: the same, plus its output.
type Done = (String, &'static str, Option<&'static str>, Option<String>, String);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consent_keeps_valid_labels_only() {
        assert_eq!(consent(&[]), None);
        assert_eq!(consent(&["".into(), "Bad Label".into()]), None);
        let c = consent(&["CI".into(), "ci".into(), "macos".into()]).unwrap();
        assert_eq!(c.labels, vec!["ci".to_string(), "macos".to_string()]);
    }

    #[test]
    fn task_ids_are_short_and_plain() {
        assert!(valid_task_id("a1-b_2"));
        assert!(!valid_task_id(""));
        assert!(!valid_task_id("../x"));
        assert!(!valid_task_id(&"a".repeat(65)));
    }

    #[test]
    fn programs_resolve_through_symlinks_to_the_list() {
        let d = tempfile::tempdir().unwrap();
        let real = d.path().join("tool");
        std::fs::write(&real, "#!/bin/sh\n").unwrap();
        let other = d.path().join("other");
        std::fs::write(&other, "#!/bin/sh\n").unwrap();
        let link = d.path().join("link");
        std::os::unix::fs::symlink(&other, &link).unwrap();
        let allowed = vec![real.to_string_lossy().into_owned()];
        assert!(allowed_program(real.to_str().unwrap(), &allowed).is_ok());
        // A link to another program is that program.
        assert_eq!(
            allowed_program(link.to_str().unwrap(), &allowed).unwrap_err().code,
            "command_not_allowed"
        );
        // A relative path is never resolved against anything.
        assert_eq!(
            allowed_program("tool", &allowed).unwrap_err().code,
            "command_not_allowed"
        );
        // A link that names an allowed program is that program.
        let good = d.path().join("good");
        std::os::unix::fs::symlink(&real, &good).unwrap();
        assert_eq!(
            allowed_program(good.to_str().unwrap(), &allowed).unwrap(),
            real.canonicalize().unwrap()
        );
    }

    #[test]
    fn work_folders_stay_under_a_root() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("root");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::create_dir_all(d.path().join("rootless")).unwrap();
        let roots = vec![root.to_string_lossy().into_owned()];
        assert!(allowed_cwd(root.join("sub").to_str().unwrap(), &roots).is_ok());
        // `..` and a sibling whose name starts like the root are outside.
        let up = format!("{}/sub/../../rootless", root.display());
        assert_eq!(allowed_cwd(&up, &roots).unwrap_err().code, "cwd_not_allowed");
        let sib = d.path().join("rootless");
        assert_eq!(
            allowed_cwd(sib.to_str().unwrap(), &roots).unwrap_err().code,
            "cwd_not_allowed"
        );
        // A link inside the root that leads out is outside.
        let esc = root.join("esc");
        std::os::unix::fs::symlink(d.path().join("rootless"), &esc).unwrap();
        assert_eq!(
            allowed_cwd(esc.to_str().unwrap(), &roots).unwrap_err().code,
            "cwd_not_allowed"
        );
    }
}
