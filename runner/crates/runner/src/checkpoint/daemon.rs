//! The daemon's side of checkpoints: the `checkpoint.*` requests and the
//! turn boundaries (a terminal agent's prompt submit, a chat's turn).

use super::Store;
use crate::api::{err, respond, ApiResult, ReplyTo};
use crate::daemon::{Daemon, Internal};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
pub(crate) use tmuxedo_protocol::checkpoint::Target as CkTarget;
use tmuxedo_protocol::checkpoint::{self as proto, Target};
use tmuxedo_protocol::frames::AgentStatus;

/// How long a turn waits for its checkpoint (within the hook shim's 10 s).
pub const TURN_BUDGET: Duration = Duration::from_secs(8);

impl Daemon {
    /// The work folder the runner records for a session, never a client's.
    fn checkpoint_folder(&self, t: &Target) -> Option<PathBuf> {
        let cwd = match t {
            // A headless command's pane goes when the command ends: its task keeps the folder.
            Target::Pane(id) => self.pane_folder_now(id).or_else(|| self.task_pane_cwd(id))?,
            Target::Chat(id) => self.chats.sessions.get(id).map(|s| s.meta.cwd.clone())?,
        };
        Some(PathBuf::from(cwd)).filter(|p| p.is_absolute())
    }

    /// The session's agent is working (a restore would race it).
    pub(crate) fn checkpoint_busy(&self, t: &Target) -> bool {
        match t {
            Target::Pane(id) => {
                self.task_runs_pane(id)
                    || self
                        .panes
                        .get(id)
                        .is_some_and(|rt| rt.track.effective == AgentStatus::Working)
            }
            Target::Chat(id) => self.chats.sessions.get(id).is_some_and(|s| s.meta.turn.is_some()),
        }
    }

    /// Every `checkpoint.*` request. The local socket is refused; the work
    /// runs in a task and answers through `Internal::Fs`.
    pub(crate) fn checkpoint_request(&mut self, method: &str, params: &Value, reply: ReplyTo) {
        if matches!(reply, ReplyTo::Local(_)) {
            let e = err(
                "not_supported",
                "Checkpoints are not available on the machine's local socket.",
            );
            return respond(reply, Err(e), self);
        }
        let p = match proto::parse_params(method, params) {
            Ok(p) => p,
            Err((code, why)) => return respond(reply, Err(err(code, format!("{method}: {why}"))), self),
        };
        let Some(cwd) = self.checkpoint_folder(&p.target) else {
            return respond(reply, Err(err("not_found", "No such session on this machine.")), self);
        };
        if method == proto::METHOD_RESTORE && self.checkpoint_busy(&p.target) {
            let e = err(
                "session_busy",
                "The agent is working: stop it or wait for its turn to end, then restore.",
            );
            return respond(reply, Err(e), self);
        }
        let (store, guard, tx) = (self.checkpoints.clone(), self.fs_guard(), self.internal_tx.clone());
        let key = p.target.key();
        let method = method.to_string();
        tokio::spawn(async move {
            let result: ApiResult = match method.as_str() {
                proto::METHOD_LIST => match store.list(&cwd, &guard, &key).await {
                    Ok(l) => Ok(json!({
                        "type": "checkpoint_list",
                        "available": true,
                        "checkpoints": l.iter().map(|i| i.json()).collect::<Vec<_>>(),
                    })),
                    Err(u) => Ok(
                        json!({ "type": "checkpoint_list", "available": false, "reason": u.reason, "message": u.message, "checkpoints": [] }),
                    ),
                },
                proto::METHOD_CREATE => store
                    .snapshot(&cwd, &guard, &key, proto::KIND_MANUAL, None)
                    .await
                    .map(|i| json!({ "type": "checkpoint_created", "checkpoint": i.map(|i| i.json()) })),
                proto::METHOD_PREVIEW => store.preview(&cwd, &guard, &key, p.id.unwrap_or(0)).await,
                _ => store.restore(&cwd, &guard, &key, p.id.unwrap_or(0)).await,
            };
            let _ = tx.send(Internal::Fs {
                reply,
                result,
                frames: vec![],
            });
        });
    }

    /// `session.summary` (the away digest's facts): the folder's changes and
    /// the chat turns since `since` from the checkpoints, and a chat's last
    /// agent line (masked when it was logged). Refused on the local socket,
    /// as the checkpoints are.
    pub(crate) fn session_summary(&mut self, params: &Value, reply: ReplyTo) {
        if matches!(reply, ReplyTo::Local(_)) {
            let e = err(
                "not_supported",
                "Session summaries are not available on the machine's local socket.",
            );
            return respond(reply, Err(e), self);
        }
        let p = match proto::parse_summary_params(params) {
            Ok(p) => p,
            Err((code, why)) => {
                return respond(reply, Err(err(code, format!("{}: {why}", proto::METHOD_SUMMARY))), self)
            }
        };
        let Some(cwd) = self.checkpoint_folder(&p.target) else {
            return respond(reply, Err(err("not_found", "No such session on this machine.")), self);
        };
        // The agent's own last line comes from a chat's log: the chat itself, or the one
        // a pane continues after a handoff to the terminal. A plain terminal has none.
        let chat = match &p.target {
            Target::Chat(id) => self.chats.sessions.get(id),
            Target::Pane(id) => self
                .chats
                .sessions
                .values()
                .find(|s| s.meta.pane_id.as_deref() == Some(id.as_str())),
        };
        let (line, turn) = chat.map_or((None, None), |s| {
            let from_log = (!s.last_reply.is_empty()).then_some(s.last_reply.as_str());
            let from_meta = (s.meta.preview_by == "agent").then_some(s.meta.preview.as_str());
            let line = from_log.or(from_meta).and_then(proto::summary_line);
            let turn = from_log.and(s.last_reply_turn.clone());
            (line, turn)
        });
        // What the pane's agent used of its plan (AI usage), from its own readings.
        let usage = match &p.target {
            Target::Pane(id) => self.usage.spent(id, p.since.unwrap_or(0)),
            Target::Chat(_) => None,
        };
        let (store, guard, tx) = (self.checkpoints.clone(), self.fs_guard(), self.internal_tx.clone());
        let key = p.target.key();
        tokio::spawn(async move {
            let mut v = match store.summary(&cwd, &guard, &key, p.since).await {
                Ok(mut v) => {
                    v["available"] = json!(true);
                    v
                }
                Err(u) => json!({ "turns": 0, "files": null, "available": false, "reason": u.reason }),
            };
            v["type"] = json!("session_summary");
            if let Some(l) = line {
                v["line"] = json!(l);
            }
            if let Some(t) = turn {
                v["turn"] = json!(t);
            }
            if let Some(u) = usage {
                v["usage"] = json!(u);
            }
            let _ = tx.send(Internal::Fs {
                reply,
                result: Ok(v),
                frames: vec![],
            });
        });
    }

    /// Start recording a session before its agent's turn: the handle ends
    /// when the checkpoint is written, or is given up after [`TURN_BUDGET`]
    /// (then nothing is recorded). None when checkpoints are off or the
    /// session has no folder.
    pub(crate) fn checkpoint_turn(&self, t: Target) -> Option<impl std::future::Future<Output = ()> + Send + 'static> {
        if !self.checkpoints.enabled() {
            return None;
        }
        let cwd = self.checkpoint_folder(&t)?;
        // Not in a repository: nothing to record, and no detour for the turn.
        crate::files::enclosing_git_tree(&cwd)?;
        // A chat's checkpoint names the turn it comes before ("Restore to here" on a message).
        let turn = match &t {
            Target::Chat(id) => self.chats.sessions.get(id).and_then(|s| s.meta.turn.clone()),
            Target::Pane(_) => None,
        };
        let (store, guard) = (self.checkpoints.clone(), self.fs_guard());
        Some(turn_snapshot(store, guard, cwd, t.key(), turn))
    }
}

async fn turn_snapshot(store: Store, guard: crate::jail::Guard, cwd: PathBuf, key: String, turn: Option<String>) {
    let abandon = Arc::new(AtomicBool::new(false));
    let a = abandon.clone();
    let task = tokio::spawn(async move {
        store
            .snapshot_turn(&cwd, &guard, &key, proto::KIND_TURN, turn.as_deref(), Some(a))
            .await
    });
    match tokio::time::timeout(TURN_BUDGET, task).await {
        Ok(Ok(Err(e))) if !matches!(e.code.as_str(), "not_a_repo" | "root_refused" | "folder_gone" | "off") => {
            crate::log!("checkpoints: a turn was not recorded: {}", e.message);
        }
        Err(_) => {
            abandon.store(true, Ordering::Relaxed);
            crate::log!("checkpoints: a turn took too long to record; skipped");
        }
        _ => {}
    }
}
