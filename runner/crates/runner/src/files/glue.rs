//! The daemon's side of files: which caller may use which method, the
//! project a pane or a folder belongs to, and what the worker sends back.

use super::worker::{Msg, Out};
use super::{project_key, repo_of, INTERNAL_METHODS, LOCAL_METHODS};
use crate::api::{err, respond, ReplyTo};
use crate::daemon::Daemon;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use tokio_tungstenite::tungstenite::Message;

/// The daemon's handle on the files worker.
pub struct Files {
    pub root: PathBuf,
    pub tx: tokio::sync::mpsc::Sender<Msg>,
    /// The disk level the worker last reported (`ok`, `low`, `critical`, `alert`).
    pub level: &'static str,
    ticks: u32,
    /// Pane → the uploaded file its agent read in the current turn (design §2 measure 5).
    pub read_uploads: std::collections::HashMap<String, String>,
}

impl Files {
    pub fn new(root: PathBuf, tx: tokio::sync::mpsc::Sender<Msg>) -> Self {
        Files {
            root,
            tx,
            level: "ok",
            ticks: 0,
            read_uploads: Default::default(),
        }
    }

    /// Send without waiting (the daemon never blocks on the worker).
    pub fn send(&self, m: Msg) {
        match self.tx.try_send(m) {
            Ok(()) => {}
            Err(tokio::sync::mpsc::error::TrySendError::Full(m)) => {
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    let _ = tx.send(m).await;
                });
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {}
        }
    }

    /// The pane's project folder, made if missing (`TMUXEDO_FILES_DIR`).
    pub fn dir_for(&self, cwd: &Path) -> PathBuf {
        let key = project_key(cwd);
        let dir = self.root.join(&key);
        for d in [dir.clone(), dir.join("inbox"), dir.join("outbox")] {
            if !d.exists() && std::fs::create_dir_all(&d).is_ok() {
                super::set_mode(&d, 0o700);
            }
        }
        dir
    }
}

/// The work folder a pane's agent may share files from: its repository, else its folder.
pub fn work_folder(cwd: &Path) -> PathBuf {
    repo_of(cwd).map(|(r, _)| r).unwrap_or_else(|| cwd.to_path_buf())
}

impl Daemon {
    /// A `files.*` method, from the gateway (web, a client socket) or the local socket.
    pub(crate) fn files_request(&mut self, method: &str, params: &Value, reply: ReplyTo) {
        let Some(files) = self.files.as_ref() else {
            return respond(reply, Err(err("not_supported", "Files are off on this machine.")), self);
        };
        let remote = matches!(reply, ReplyTo::Gateway(_));
        if remote && LOCAL_METHODS.contains(&method) {
            return respond(
                reply,
                Err(err(
                    "not_supported",
                    format!("{method} is available on the machine's local socket only"),
                )),
                self,
            );
        }
        if !remote && INTERNAL_METHODS.contains(&method) {
            return respond(
                reply,
                Err(err(
                    "not_supported",
                    format!("{method} is used by tmuxedo's servers only"),
                )),
                self,
            );
        }
        let mut p = if params.is_object() { params.clone() } else { json!({}) };
        // The project: given, else the pane's (gateway: `pane_id`), else the caller's folder (local: `cwd`).
        // A pane's folder, or (agent chat, the composer's 📎) the chat's work folder.
        let pane_cwd = p["pane_id"]
            .as_str()
            .and_then(|id| self.state.pane(id))
            .map(|r| PathBuf::from(&r.pane.cwd))
            .or_else(|| {
                p["session_id"]
                    .as_str()
                    .and_then(|sid| self.chats.sessions.get(sid))
                    .map(|s| PathBuf::from(&s.meta.cwd))
                    .filter(|c| c.is_absolute())
            });
        let local_cwd = (!remote)
            .then(|| p["cwd"].as_str().map(PathBuf::from))
            .flatten()
            .filter(|c| c.is_absolute());
        let cwd = pane_cwd.or(local_cwd);
        if p["project"].as_str().is_none() {
            match &cwd {
                Some(c) => p["project"] = json!(project_key(c)),
                None if matches!(
                    method,
                    "files.quota"
                        | "files.upload_status"
                        | "files.upload_complete"
                        | "files.upload_abort"
                        | "files.read"
                ) => {}
                None => {
                    return respond(
                        reply,
                        Err(err(
                            "invalid_params",
                            "say which project: project, pane_id or (local) cwd",
                        )),
                        self,
                    )
                }
            }
        }
        if method == "files.share" {
            // Where a shared file may come from: the session's work folder (never the caller's say-so alone).
            let allowed: Vec<String> = cwd.iter().map(|c| work_folder(c).display().to_string()).collect();
            p["allowed"] = json!(allowed);
        } else if let Some(o) = p.as_object_mut() {
            o.remove("allowed");
        }
        files.send(Msg::Call {
            method: method.to_string(),
            params: p,
            reply,
        });
    }

    /// What the worker produced.
    pub(crate) async fn on_files(&mut self, out: Out) {
        match out {
            Out::Text(t) => self.queue_msg(Message::Text(t.into())),
            Out::Reply { reply, result, frames } => {
                if matches!(reply, ReplyTo::Gateway(_)) {
                    for f in frames {
                        self.queue_msg(Message::Binary(f.into()));
                    }
                }
                respond(reply, result, self);
            }
            Out::Event(mut n) => {
                // Phase 13's one model: a file request is also a needs-you item; an upload naming it closes it.
                match n.kind {
                    "file_requested" => {
                        if let Some(item) = self.file_ask_opened(&n.pane_id, &n.data).await {
                            // Web pushes the item (needs_opened), not this event, when it came through needs.
                            if self.gateway_relays_needs() {
                                n.data["itemId"] = json!(item);
                            }
                        }
                    }
                    "file_uploaded" => self.file_ask_uploaded(&n.data).await,
                    _ => {}
                }
                self.send_event(&n.pane_id, n.kind, n.data).await
            }
            Out::Level(l) => {
                if let Some(f) = self.files.as_mut() {
                    if f.level != l {
                        crate::log!("disk level {} → {l}", f.level);
                    }
                    f.level = l;
                }
            }
        }
    }

    /// Every slow tick (5 s): the worker checks the disk each minute and sweeps.
    pub(crate) fn files_tick(&mut self) {
        if let Some(f) = self.files.as_mut() {
            f.ticks += 1;
            if f.ticks % 12 == 1 {
                f.send(Msg::Tick);
            }
        }
    }

    /// A hook event: a read of an uploaded file is remembered for the pane until
    /// the next prompt from the person; returns the file read in this turn, if any.
    pub(crate) fn note_upload_read(&mut self, pane_id: &str, ev: &serde_json::Value) -> Option<String> {
        let files = self.files.as_mut()?;
        let event = ev["hook_event_name"].as_str().unwrap_or("");
        if event == "UserPromptSubmit" || event == "SessionStart" {
            files.read_uploads.remove(pane_id);
            return None;
        }
        if event == "PostToolUse"
            && matches!(
                ev["tool_name"].as_str(),
                Some("Read" | "read_file" | "View" | "NotebookRead")
            )
        {
            let path = ev["tool_input"]["file_path"]
                .as_str()
                .or_else(|| ev["tool_input"]["path"].as_str())
                .or_else(|| ev["tool_input"]["notebook_path"].as_str());
            if let Some(name) = path.and_then(|p| super::upload_name(&files.root, p)) {
                files.read_uploads.insert(pane_id.to_string(), name);
            }
        }
        files.read_uploads.get(pane_id).cloned()
    }

    /// Pane history is opt-in convenience: it stops when the disk is nearly full.
    pub(crate) fn disk_allows_history(&self) -> bool {
        !matches!(self.files.as_ref().map(|f| f.level), Some("critical" | "alert"))
    }
}
