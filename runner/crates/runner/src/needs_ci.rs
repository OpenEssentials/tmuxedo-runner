//! A pull request's CI, watched (docs/design/away-and-review.md §5): a PR
//! opened from the review is asked about once a minute with the machine's
//! own gh (`gh pr checks`), for a day at most. When a check fails, a
//! needs-you item says so (kind `review`, answered by nothing: open the
//! checks, open the session, or dismiss); when every check passes, the
//! watch ends quietly and a failure shown before goes away.
//!
//! Kept across a restart in `<state>/ci-watches.json`.

use crate::api::{err, respond, ReplyTo};
use crate::daemon::{Daemon, Internal};
use crate::log;
use crate::review::ship::{checks, pr_link, Checks};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::{Path, PathBuf};
use tmuxedo_protocol::needs::{NeedsResponse, NeedsYou, MAX_TITLE};

/// How long a pull request is watched.
pub const TTL_MS: u64 = 24 * 3600 * 1000;
/// How often its checks are asked about.
pub const EVERY_MS: u64 = 60 * 1000;
/// The most watched at once (the oldest go first).
pub const MAX_WATCHES: usize = 20;
const FILE: &str = "ci-watches.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CiFail {
    pub names: Vec<String>,
    pub total: usize,
    pub at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CiWatch {
    pub url: String,
    pub number: u64,
    pub dir: PathBuf,
    /// The pane the review was for; empty for a chat's (the machine's set).
    pub pane_id: String,
    pub created_ms: u64,
    #[serde(default)]
    pub next_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed: Option<CiFail>,
    /// A `gh pr checks` in flight.
    #[serde(skip)]
    pub busy: bool,
}

impl CiWatch {
    pub fn item_id(&self) -> String {
        let h = ring::digest::digest(&ring::digest::SHA256, self.url.as_bytes());
        format!("ci-{}-{}", self.number, hex::encode(&h.as_ref()[..4]))
    }

    /// The needs-you item, while a check is failing.
    pub fn item(&self) -> Option<NeedsYou> {
        let f = self.failed.as_ref()?;
        let names = f.names.join(", ");
        let title = format!(
            "CI failed on PR #{} · {} of {} checks: {names}",
            self.number,
            f.names.len(),
            f.total
        );
        let mut responses = vec![];
        if !self.pane_id.is_empty() {
            responses.push(NeedsResponse::new("open", "open", "Open session", "none"));
        }
        responses.push(NeedsResponse::new("dismiss", "dismiss", "Dismiss", "none"));
        Some(NeedsYou {
            id: self.item_id(),
            pane_id: self.pane_id.clone(),
            chat_id: None,
            plugin: crate::needs_files::CORE_PLUGIN.into(),
            kind: "review".into(),
            title: crate::needs::cut(&title, MAX_TITLE),
            detail: Some(json!({
                "text": crate::needs::cut(&names, 2000),
                "link": format!("{}/checks", self.url),
                "ci": { "number": self.number, "url": self.url, "failed": f.names.len(), "total": f.total },
            })),
            context: None,
            risk: "normal".into(),
            risk_class: None,
            why: None,
            responses,
            answer_via: "none".into(),
            fingerprint: None,
            source: "core".into(),
            created_at: crate::util::iso(f.at_ms),
            expires_at: Some(crate::util::iso(self.created_ms + TTL_MS)),
        })
        .filter(NeedsYou::is_valid)
    }
}

pub fn load(state_dir: &Path) -> Vec<CiWatch> {
    let list: Vec<CiWatch> = std::fs::read(state_dir.join(FILE))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    list.into_iter()
        .filter(|w| pr_link(&w.url).is_some_and(|(n, _)| n == w.number) && w.dir.is_absolute())
        .collect()
}

pub fn save(state_dir: &Path, watches: &[CiWatch]) {
    if watches.is_empty() {
        let _ = std::fs::remove_file(state_dir.join(FILE));
        return;
    }
    if let Ok(b) = serde_json::to_vec_pretty(watches) {
        let _ = crate::config::write_private(&state_dir.join(FILE), &b);
    }
}

/// Add (or restart) a watch; the oldest go past [`MAX_WATCHES`]. Answers
/// the watches dropped.
pub fn add(watches: &mut Vec<CiWatch>, new: CiWatch) -> Vec<CiWatch> {
    let mut gone = vec![];
    if let Some(i) = watches.iter().position(|w| w.url == new.url) {
        gone.push(watches.remove(i));
    }
    watches.push(new);
    while watches.len() > MAX_WATCHES {
        gone.push(watches.remove(0));
    }
    gone
}

/// What one answer does to a watch: `true` when it ends.
pub fn apply(w: &mut CiWatch, c: &Checks, now: u64) -> bool {
    match c {
        Checks::Passed => true,
        Checks::Failed { names, total } => {
            if w.failed.as_ref().is_none_or(|f| f.names != *names || f.total != *total) {
                w.failed = Some(CiFail {
                    names: names.clone(),
                    total: *total,
                    at_ms: now,
                });
            }
            false
        }
        Checks::Pending | Checks::Unknown => false,
    }
}

impl Daemon {
    fn ci_save(&self) {
        save(&self.opts.state_dir, &self.needs.ci);
    }

    pub(crate) fn ci_items(&self, pane_id: &str) -> Vec<NeedsYou> {
        self.needs
            .ci
            .iter()
            .filter(|w| w.pane_id == pane_id)
            .filter_map(CiWatch::item)
            .collect()
    }

    /// A pull request was opened from the review: watch its checks.
    pub(crate) async fn ci_watch(&mut self, url: String, number: u64, dir: PathBuf, pane: Option<String>) {
        let pane_id = pane
            .filter(|p| self.panes.contains_key(p))
            .unwrap_or_else(|| crate::needs::MACHINE.to_string());
        let now = crate::util::now_ms();
        let w = CiWatch {
            url,
            number,
            dir,
            pane_id,
            created_ms: now,
            next_ms: now + EVERY_MS,
            failed: None,
            busy: false,
        };
        log!("ci: watching PR #{} for a day", w.number);
        let gone = add(&mut self.needs.ci, w);
        self.ci_save();
        for g in gone {
            if g.failed.is_some() {
                self.needs.hint(&g.item_id(), "expired", None);
                self.needs_sync(&g.pane_id.clone()).await;
            }
        }
    }

    /// Once a minute per watch: ask gh; end the old ones.
    pub(crate) async fn ci_tick(&mut self) {
        let now = crate::util::now_ms();
        let mut touched = vec![];
        let mut keep = vec![];
        for mut w in std::mem::take(&mut self.needs.ci) {
            if w.created_ms + TTL_MS <= now {
                if w.failed.is_some() {
                    self.needs.hint(&w.item_id(), "expired", None);
                    touched.push(w.pane_id.clone());
                }
                continue;
            }
            // Its pane closed: a failure stays, on the machine's list.
            if !w.pane_id.is_empty() && !self.panes.contains_key(&w.pane_id) {
                if w.failed.is_some() {
                    self.needs.hint(&w.item_id(), "agent", None);
                    touched.push(w.pane_id.clone());
                    touched.push(crate::needs::MACHINE.to_string());
                }
                w.pane_id = crate::needs::MACHINE.to_string();
            }
            if !w.busy && w.next_ms <= now {
                w.busy = true;
                w.next_ms = now + EVERY_MS;
                let tx = self.internal_tx.clone();
                let (url, dir) = (w.url.clone(), w.dir.clone());
                tokio::spawn(async move {
                    let outcome = checks(Path::new("gh"), &dir, &url, &[]).await;
                    let _ = tx.send(Internal::CiChecked { url, outcome });
                });
            }
            keep.push(w);
        }
        self.needs.ci = keep;
        if touched.is_empty() {
            return;
        }
        self.ci_save();
        touched.sort();
        touched.dedup();
        for p in touched {
            self.needs_sync(&p).await;
        }
    }

    /// gh answered about one pull request.
    pub(crate) async fn ci_checked(&mut self, url: &str, outcome: Checks) {
        let Some(i) = self.needs.ci.iter().position(|w| w.url == url) else {
            return;
        };
        let now = crate::util::now_ms();
        let w = &mut self.needs.ci[i];
        w.busy = false;
        let before = w.failed.clone();
        let ends = apply(w, &outcome, now);
        let (pane, id, number, after) = (w.pane_id.clone(), w.item_id(), w.number, w.failed.clone());
        if ends {
            self.needs.ci.remove(i);
            log!("ci: PR #{number} passed; watch ended");
            if before.is_some() {
                self.needs.hint(&id, "agent", None);
            }
        } else if after != before {
            log!("ci: PR #{number} has failing checks");
        } else {
            return;
        }
        self.ci_save();
        self.needs_sync(&pane).await;
    }

    /// Dismiss: the item goes and the watch ends.
    pub(crate) async fn ci_respond(&mut self, item: &NeedsYou, r: &NeedsResponse, reply: ReplyTo) {
        if r.kind != "dismiss" {
            let e = err(
                "invalid_params",
                "open the checks or the session in the app; this one is only dismissed here",
            );
            return respond(reply, Err(e), self);
        }
        let Some(i) = self.needs.ci.iter().position(|w| w.item_id() == item.id) else {
            return respond(reply, Ok(json!({ "type": "needs_respond", "outcome": "stale" })), self);
        };
        let w = self.needs.ci.remove(i);
        self.needs.hint(&item.id, "remote", Some(r.id.clone()));
        self.ci_save();
        log!("ci: PR #{} dismissed", w.number);
        self.needs_sync(&w.pane_id).await;
        respond(
            reply,
            Ok(json!({ "type": "needs_respond", "outcome": "accepted" })),
            self,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn watch(n: u64) -> CiWatch {
        CiWatch {
            url: format!("https://github.example/o/r/pull/{n}"),
            number: n,
            dir: PathBuf::from("/srv/work"),
            pane_id: "w1:p1".into(),
            created_ms: 1_759_000_000_000,
            next_ms: 0,
            failed: None,
            busy: false,
        }
    }

    #[test]
    fn a_failure_is_an_item_and_a_pass_ends_the_watch() {
        let mut w = watch(7);
        assert!(w.item().is_none(), "nothing to show while pending");
        assert!(!apply(&mut w, &Checks::Pending, 5));
        assert!(!apply(
            &mut w,
            &Checks::Failed {
                names: vec!["e2e".into(), "lint".into()],
                total: 5
            },
            9
        ));
        let it = w.item().expect("an item");
        assert!(it.is_valid(), "{it:?}");
        assert_eq!(it.kind, "review");
        assert_eq!(it.answer_via, "none");
        assert_eq!(it.title, "CI failed on PR #7 · 2 of 5 checks: e2e, lint");
        assert_eq!(
            it.detail.as_ref().unwrap()["link"],
            "https://github.example/o/r/pull/7/checks"
        );
        assert!(
            it.id.starts_with("ci-7-") && it.id.len() == "ci-7-".len() + 8,
            "{}",
            it.id
        );
        assert_eq!(
            it.responses.iter().map(|r| r.kind.as_str()).collect::<Vec<_>>(),
            vec!["open", "dismiss"]
        );
        // The same failure again keeps its time; a pass ends it.
        assert!(!apply(
            &mut w,
            &Checks::Failed {
                names: vec!["e2e".into(), "lint".into()],
                total: 5
            },
            20
        ));
        assert_eq!(w.failed.as_ref().unwrap().at_ms, 9);
        assert!(!apply(&mut w, &Checks::Unknown, 30));
        assert!(apply(&mut w, &Checks::Passed, 40));
        let mut m = watch(8);
        m.pane_id = String::new();
        apply(
            &mut m,
            &Checks::Failed {
                names: vec!["x".repeat(400)],
                total: 1,
            },
            1,
        );
        let it = m.item().unwrap();
        assert!(it.is_valid() && it.title.chars().count() <= MAX_TITLE);
        assert_eq!(it.responses.len(), 1, "no session to open from the machine's list");
    }

    #[test]
    fn kept_across_a_restart_capped_and_junk_dropped() {
        let d = tempfile::tempdir().unwrap();
        let mut v = vec![];
        for n in 1..=(MAX_WATCHES as u64 + 2) {
            add(&mut v, watch(n));
        }
        assert_eq!(v.len(), MAX_WATCHES);
        assert_eq!(v[0].number, 3, "the oldest went");
        let gone = add(&mut v, watch(5));
        assert_eq!(gone.len(), 1);
        assert_eq!(v.last().unwrap().number, 5);
        save(d.path(), &v);
        assert_eq!(load(d.path()), v);
        std::fs::write(
            d.path().join(FILE),
            r#"[{"url":"http://x/o/r/pull/1","number":1,"dir":"/a","paneId":"","createdMs":1}]"#,
        )
        .unwrap();
        assert!(load(d.path()).is_empty());
        save(d.path(), &[]);
        assert!(!d.path().join(FILE).exists());
    }
}
