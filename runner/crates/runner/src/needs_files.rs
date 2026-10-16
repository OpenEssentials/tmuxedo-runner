//! `tmuxedo files request` as a needs-you item (Phase 15, docs/protocol.md
//! "Needs you" and "Files"): one model with permissions, questions and logins.
//!
//! When an agent (or anything in a pane) asks for a file, the files worker
//! raises its durable `file_requested` event as before, and the daemon also
//! opens a `file` item in that pane's needs set (the machine's set when the
//! request came from outside a pane):
//!
//! - `answerVia: upload`, `detail.upload {project, requestId}`, responses
//!   `upload` ("Upload a file") and `dismiss` ("Not now"), both `requires:
//!   none` (an upload needs its own ticket: an MFA session or a `write`
//!   token, checked by web);
//! - an upload that names the request (`request_id` on `files.upload_init`)
//!   and lands in the same project closes it (`needs_resolved by: remote,
//!   responseId: upload`); `needs.respond` with `dismiss` closes it
//!   (`responseId: dismiss`); after [`TTL_MS`] it closes `by: expired`;
//!   when its pane goes, `by: agent`;
//! - at most [`MAX_PER_PANE`] per pane (the oldest goes); the same words
//!   again in the same pane refresh the open one instead of adding another;
//! - kept in `<state>/file-requests.json`, so a runner restart (an update)
//!   keeps them.

use crate::api::{err, respond, ReplyTo};
use crate::daemon::Daemon;
use crate::log;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::Path;
use tmuxedo_protocol::needs::{valid_file_request_id, NeedsResponse, NeedsYou, MAX_TITLE};

/// A request stays open this long.
pub const TTL_MS: u64 = 24 * 3600 * 1000;
/// Open requests per pane, at most.
pub const MAX_PER_PANE: usize = 5;
const FILE: &str = "file-requests.json";
/// The plugin id of a request that no agent made (a shell, a script).
pub const CORE_PLUGIN: &str = "tmuxedo";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileAsk {
    pub request_id: String,
    /// "" = the machine (not asked from a pane).
    pub pane_id: String,
    pub project: String,
    pub what: String,
    pub plugin: String,
    /// "Claude", "Codex"…; None when no agent runs in the pane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    pub created_ms: u64,
}

impl FileAsk {
    pub fn item_id(&self) -> String {
        format!("file-{}", self.request_id)
    }

    /// The needs-you item for it.
    pub fn item(&self) -> NeedsYou {
        let who = self.agent.as_deref().unwrap_or("An agent");
        NeedsYou {
            id: self.item_id(),
            pane_id: self.pane_id.clone(),
            chat_id: None,
            plugin: self.plugin.clone(),
            kind: "file".into(),
            title: crate::needs::cut(&format!("{who} wants a file: {}", self.what), MAX_TITLE),
            detail: Some(json!({
                "text": self.what,
                "upload": { "project": self.project, "requestId": self.request_id },
            })),
            context: None,
            risk: "normal".into(),
            risk_class: None,
            why: None,
            responses: vec![
                NeedsResponse::new("upload", "upload", "Upload a file", "none"),
                NeedsResponse::new("dismiss", "dismiss", "Not now", "none"),
            ],
            answer_via: "upload".into(),
            fingerprint: None,
            source: "core".into(),
            created_at: crate::util::iso(self.created_ms),
            expires_at: Some(crate::util::iso(self.created_ms + TTL_MS)),
        }
    }
}

pub fn load(state_dir: &Path) -> Vec<FileAsk> {
    let list: Vec<FileAsk> = std::fs::read(state_dir.join(FILE))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    list.into_iter()
        .filter(|a| valid_file_request_id(&a.request_id) && a.item().is_valid())
        .collect()
}

pub fn save(state_dir: &Path, asks: &[FileAsk]) {
    if asks.is_empty() {
        let _ = std::fs::remove_file(state_dir.join(FILE));
        return;
    }
    if let Ok(b) = serde_json::to_vec_pretty(asks) {
        let _ = crate::config::write_private(&state_dir.join(FILE), &b);
    }
}

/// Add a request: the same words in the same pane replace the open one (the
/// agent holds the newest request id); the oldest of a pane goes past
/// [`MAX_PER_PANE`]. → (the request now open, the ones it pushed out).
pub fn add(asks: &mut Vec<FileAsk>, new: FileAsk) -> (FileAsk, Vec<FileAsk>) {
    let mut gone = vec![];
    if let Some(i) = asks
        .iter()
        .position(|a| a.pane_id == new.pane_id && a.project == new.project && a.what == new.what)
    {
        gone.push(asks.remove(i));
    }
    asks.push(new.clone());
    while asks.iter().filter(|a| a.pane_id == new.pane_id).count() > MAX_PER_PANE {
        let i = asks
            .iter()
            .position(|a| a.pane_id == new.pane_id)
            .expect("counted above");
        gone.push(asks.remove(i));
    }
    (new, gone)
}

impl Daemon {
    fn file_asks_save(&self) {
        save(&self.opts.state_dir, &self.needs.files);
    }

    /// The file items of a set ("" = the machine's).
    pub(crate) fn file_items(&self, pane_id: &str) -> Vec<NeedsYou> {
        self.needs
            .files
            .iter()
            .filter(|a| a.pane_id == pane_id)
            .map(FileAsk::item)
            .collect()
    }

    /// The files worker raised `file_requested`: open its item. Returns the item id.
    pub(crate) async fn file_ask_opened(&mut self, pane_id: &str, data: &Value) -> Option<String> {
        let id = data["requestId"].as_str().filter(|s| valid_file_request_id(s))?;
        let project = data["project"].as_str().filter(|p| !p.is_empty())?;
        let what = data["what"].as_str().unwrap_or("").to_string();
        // A pane this runner knows, else the machine's set.
        let pane = if self.panes.contains_key(pane_id) {
            pane_id
        } else {
            crate::needs::MACHINE
        };
        let agent_label = self.panes.get(pane).and_then(|rt| rt.track.kind().map(str::to_owned));
        let plugin = agent_label
            .as_deref()
            .and_then(|l| self.plugins.by_label(l))
            .map(|p| p.manifest().id.clone())
            .filter(|p| tmuxedo_protocol::plugins::valid_plugin_id(p))
            .unwrap_or_else(|| CORE_PLUGIN.to_string());
        let agent = agent_label
            .as_deref()
            .map(|l| tmuxedo_detect::Agent::lookup(l).map_or(l.to_string(), |a| a.title().to_string()));
        let ask = FileAsk {
            request_id: id.to_string(),
            pane_id: pane.to_string(),
            project: project.to_string(),
            what,
            plugin,
            agent,
            created_ms: crate::util::now_ms(),
        };
        let (open, gone) = add(&mut self.needs.files, ask);
        for g in &gone {
            // Asked again in the same words: replaced by the agent; else pushed out by newer ones.
            self.needs.hint(
                &g.item_id(),
                if g.what == open.what { "agent" } else { "expired" },
                None,
            );
        }
        self.file_asks_save();
        log!(
            "needs you: file request {} in {}",
            open.item_id(),
            if pane.is_empty() { "the machine" } else { pane }
        );
        self.needs_sync(pane).await;
        Some(open.item_id())
    }

    /// An upload landed: if it names an open request in the same project, close it.
    pub(crate) async fn file_ask_uploaded(&mut self, data: &Value) {
        let Some(rid) = data["requestId"].as_str() else { return };
        let project = data["project"].as_str().unwrap_or("");
        let Some(i) = self
            .needs
            .files
            .iter()
            .position(|a| a.request_id == rid && a.project == project)
        else {
            return;
        };
        let a = self.needs.files.remove(i);
        self.needs.hint(&a.item_id(), "remote", Some("upload".into()));
        self.file_asks_save();
        log!("needs you: file request {} answered by an upload", a.item_id());
        self.needs_sync(&a.pane_id).await;
    }

    /// `needs.respond` on a file item: `dismiss` closes it; `upload` is an upload, not an answer.
    pub(crate) async fn file_ask_respond(&mut self, item: &NeedsYou, r: &NeedsResponse, reply: ReplyTo) {
        if r.kind != "dismiss" {
            let e = err(
                "invalid_params",
                "upload the file (POST …/uploads with this requestId); the request closes when it lands",
            );
            return respond(reply, Err(e), self);
        }
        let Some(i) = self.needs.files.iter().position(|a| a.item_id() == item.id) else {
            return respond(reply, Ok(json!({ "type": "needs_respond", "outcome": "stale" })), self);
        };
        let a = self.needs.files.remove(i);
        self.needs.hint(&a.item_id(), "remote", Some(r.id.clone()));
        self.file_asks_save();
        log!("needs you: file request {} dismissed", a.item_id());
        self.needs_sync(&a.pane_id).await;
        respond(
            reply,
            Ok(json!({ "type": "needs_respond", "outcome": "accepted" })),
            self,
        )
    }

    /// Requests past their time, or whose pane is gone, close.
    pub(crate) async fn file_asks_tick(&mut self) {
        let now = crate::util::now_ms();
        let mut touched = vec![];
        let mut keep = vec![];
        for a in std::mem::take(&mut self.needs.files) {
            let pane_gone = !a.pane_id.is_empty() && !self.panes.contains_key(&a.pane_id);
            if a.created_ms + TTL_MS <= now || pane_gone {
                self.needs
                    .hint(&a.item_id(), if pane_gone { "agent" } else { "expired" }, None);
                touched.push(a.pane_id.clone());
            } else {
                keep.push(a);
            }
        }
        self.needs.files = keep;
        if touched.is_empty() {
            return;
        }
        self.file_asks_save();
        touched.sort();
        touched.dedup();
        for p in touched {
            self.needs_sync(&p).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ask(pane: &str, what: &str, at: u64) -> FileAsk {
        FileAsk {
            request_id: format!("req{at:08}"),
            pane_id: pane.into(),
            project: "github.com_org_repo".into(),
            what: what.into(),
            plugin: "claude".into(),
            agent: Some("Claude".into()),
            created_ms: at,
        }
    }

    #[test]
    fn the_item_follows_the_contract() {
        let it = ask("w1:p1", "the logo as SVG", 1_759_000_000_000).item();
        assert!(it.is_valid(), "{it:?}");
        assert_eq!(it.kind, "file");
        assert_eq!(it.answer_via, "upload");
        assert_eq!(it.title, "Claude wants a file: the logo as SVG");
        assert_eq!(it.file_request(), Some(("github.com_org_repo", "req1759000000000")));
        assert_eq!(
            it.responses
                .iter()
                .map(|r| (r.id.as_str(), r.requires.as_str()))
                .collect::<Vec<_>>(),
            vec![("upload", "none"), ("dismiss", "none")]
        );
        assert!(it.expires_at.is_some());
        // No agent: still a valid item, said plainly.
        let mut a = ask("", "a CSV", 1);
        a.agent = None;
        a.plugin = CORE_PLUGIN.into();
        let it = a.item();
        assert!(it.is_valid());
        assert_eq!(it.title, "An agent wants a file: a CSV");
        // A long request is cut to the title limit.
        let it = ask("w1:p1", &"x".repeat(400), 2).item();
        assert!(it.is_valid() && it.title.chars().count() <= MAX_TITLE);
    }

    #[test]
    fn the_same_words_refresh_and_a_pane_keeps_five() {
        let mut v = vec![];
        let (a, gone) = add(&mut v, ask("w1:p1", "logo", 1));
        assert!(gone.is_empty());
        // Asked again: the new request (its id is what the agent was told) replaces the old.
        let (b, gone) = add(&mut v, ask("w1:p1", "logo", 9));
        assert_eq!(gone, vec![a]);
        assert_eq!((v.len(), b.created_ms), (1, 9));
        assert_eq!(v[0], b);
        for i in 0..6 {
            add(&mut v, ask("w1:p1", &format!("f{i}"), 10 + i));
        }
        add(&mut v, ask("w2:p1", "other pane", 30));
        assert_eq!(v.iter().filter(|a| a.pane_id == "w1:p1").count(), MAX_PER_PANE);
        assert!(!v.iter().any(|a| a.what == "logo"), "the oldest went");
        assert_eq!(v.iter().filter(|a| a.pane_id == "w2:p1").count(), 1);
    }

    #[test]
    fn kept_across_a_restart_and_junk_dropped() {
        let d = tempfile::tempdir().unwrap();
        save(d.path(), &[ask("w1:p1", "logo", 5)]);
        assert_eq!(load(d.path()), vec![ask("w1:p1", "logo", 5)]);
        std::fs::write(
            d.path().join(FILE),
            r#"[{"requestId":"bad id!","paneId":"","project":"p","what":"x","plugin":"claude","createdMs":1}]"#,
        )
        .unwrap();
        assert!(load(d.path()).is_empty());
        save(d.path(), &[]);
        assert!(!d.path().join(FILE).exists());
    }
}
