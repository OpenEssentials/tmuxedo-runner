//! Session recording and replay (Phase 27; docs/design/sharing-and-integrations.md §3).
//!
//! Off by default: the owner turns recording on for one session
//! (`replay.set`). The masked byte stream (the same bytes the mirror and live
//! viewers get) is written to `<recordings_dir>/<session>/` in parts, with
//! the session's agent events as timestamped markers. Retention (D-SI4): 14
//! days and 1 GiB per machine, oldest first. Recordings never leave the
//! machine except as `replay.read` pages to web, for the owner; every
//! `replay.*` method is refused on the local socket.

pub mod store;

use crate::api::{err, respond, ReplyTo};
use crate::daemon::Daemon;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use store::{Limits, Recorder, Snap};
use tmuxedo_protocol::replay::{self as proto, Params};

/// Retention runs this often (and at the first slow tick).
pub const PRUNE_EVERY: Duration = Duration::from_secs(600);
/// A part's redraw: the screen and this much of the history above it.
const SNAP_MAX: usize = 1024 * 1024;

#[derive(Default)]
pub struct Rt {
    pub limits: Limits,
    pruned: Option<Instant>,
    /// Masking the files again after the set grew (a thread).
    remasking: Option<std::thread::JoinHandle<usize>>,
}

pub fn snap(emu: &mut tmuxedo_emu::Emulator, cols: u16, rows: u16) -> Snap {
    Snap {
        ansi: emu.snapshot_with(SNAP_MAX, crate::pane_rt::SNAPSHOT_HISTORY_BYTES),
        cols,
        rows,
    }
}

impl Daemon {
    fn recordings(&self) -> PathBuf {
        self.opts.recordings_dir.clone()
    }

    fn open_parts(&self) -> Vec<PathBuf> {
        self.panes
            .values()
            .filter_map(|p| p.rec.as_ref()?.open_path().map(|p| p.to_path_buf()))
            .collect()
    }

    /// A pane registers: recording resumes if the owner turned it on.
    pub(crate) fn replay_register(&mut self, pane_id: &str) {
        let root = self.recordings();
        let limits = self.replay.limits;
        if let Some(rt) = self.panes.get_mut(pane_id) {
            if rt.rec.is_none() && store::is_on(&root, pane_id) {
                rt.rec = Some(Recorder::new(&root, pane_id, limits));
            }
        }
    }

    /// The pane is gone: its recording stays (retention ends it), the flag goes.
    pub(crate) fn replay_pane_gone(&mut self, pane_id: &str) {
        let _ = store::set_on(&self.recordings(), pane_id, false);
    }

    /// An agent event (its durable kind, or `status:<status>`), as a marker.
    pub(crate) fn replay_marker(&mut self, pane_id: &str, label: &str) {
        if let Some(rec) = self.panes.get_mut(pane_id).and_then(|p| p.rec.as_mut()) {
            rec.marker(label, store::now_ms());
        }
    }

    pub(crate) fn replay_resize(&mut self, pane_id: &str, cols: u16, rows: u16) {
        if let Some(rec) = self.panes.get_mut(pane_id).and_then(|p| p.rec.as_mut()) {
            rec.resize(cols, rows, store::now_ms());
        }
    }

    fn replay_flush(&mut self) {
        for p in self.panes.values_mut() {
            if let Some(r) = p.rec.as_mut() {
                r.flush();
            }
        }
    }

    /// The slow tick: flush, pause on a nearly full disk, retention.
    pub(crate) fn replay_tick(&mut self) {
        let low = !self.disk_allows_history();
        for p in self.panes.values_mut() {
            if let Some(r) = p.rec.as_mut() {
                r.paused = low;
                r.flush();
            }
        }
        if self.replay.remasking.as_ref().is_some_and(|h| h.is_finished()) {
            if let Some(Ok(n)) = self.replay.remasking.take().map(|h| h.join()) {
                if n > 0 {
                    crate::log!("recordings: masked {n} part(s) again");
                }
            }
        }
        if self.replay.pruned.is_some_and(|t| t.elapsed() < PRUNE_EVERY) || self.replay.remasking.is_some() {
            return;
        }
        self.replay.pruned = Some(Instant::now());
        let gone = store::prune(
            &self.recordings(),
            &self.replay.limits,
            store::now_ms(),
            &self.open_parts(),
        );
        if gone > 0 {
            crate::log!("recordings: retention removed {gone} bytes");
        }
    }

    /// The mask set grew: every open part ends (the next opens with a
    /// redraw of the re-read screen) and the files are masked again.
    pub(crate) fn replay_mask_grew(&mut self) {
        for p in self.panes.values_mut() {
            if let Some(r) = p.rec.as_mut() {
                r.close();
            }
        }
        let (root, set) = (self.recordings(), self.secrets.set());
        if !root.is_dir() {
            return;
        }
        // One at a time: a later growth waits for the earlier pass to end.
        if let Some(h) = self.replay.remasking.take() {
            let _ = h.join();
        }
        self.replay.remasking = Some(std::thread::spawn(move || store::remask(&root, &set, &[])));
    }

    /// Wait for a pass of [`Daemon::replay_mask_grew`] (reads and deletes).
    fn replay_settle(&mut self) {
        if let Some(h) = self.replay.remasking.take() {
            let _ = h.join();
        }
    }

    /// Every `replay.*` request. Web reaches these through the gateway
    /// (`machine.call`); the local socket is refused.
    pub(crate) fn replay_request(&mut self, method: &str, params: &Value, reply: ReplyTo) {
        if matches!(reply, ReplyTo::Local(_)) {
            let e = err(
                "not_supported",
                "Recordings are not available on the machine's local socket.",
            );
            return respond(reply, Err(e), self);
        }
        if !proto::METHODS.contains(&method) {
            return respond(
                reply,
                Err(err("unknown_method", format!("unknown method {method}"))),
                self,
            );
        }
        let parsed = match proto::parse_params(method, params) {
            Ok(p) => p,
            Err((code, why)) => return respond(reply, Err(err(code, format!("{method}: {why}"))), self),
        };
        let r = self.replay_answer(parsed);
        respond(reply, r, self)
    }

    fn replay_answer(&mut self, p: Params) -> crate::api::ApiResult {
        let root = self.recordings();
        match p {
            Params::List { pane } => {
                self.replay_flush();
                let failed: Vec<(String, String)> = self
                    .panes
                    .iter()
                    .filter_map(|(id, p)| Some((id.clone(), p.rec.as_ref()?.failed.clone()?)))
                    .collect();
                let recordings: Vec<Value> = store::list(&root)
                    .into_iter()
                    .filter(|(s, _)| pane.as_deref().is_none_or(|p| p == s.pane))
                    .map(|(s, on)| {
                        let mut v = json!({
                            "pane": s.pane, "recording": on && self.panes.contains_key(&s.pane),
                            "startMs": s.start_ms, "endMs": s.end_ms, "bytes": s.bytes, "segments": s.segments,
                        });
                        if let Some((_, why)) = failed.iter().find(|(id, _)| *id == s.pane) {
                            v["failed"] = json!(why);
                        }
                        v
                    })
                    .collect();
                let total: u64 = recordings.iter().map(|r| r["bytes"].as_u64().unwrap_or(0)).sum();
                let l = self.replay.limits;
                Ok(json!({
                    "type": "replay_list", "recordings": recordings, "totalBytes": total,
                    "retention": { "days": l.max_age_ms / 86_400_000, "maxBytes": l.max_bytes },
                }))
            }
            Params::Set { pane, on } => {
                if !self.panes.get(&pane).is_some_and(|p| p.alive) {
                    return Err(err("not_found", "No such session on this machine."));
                }
                if let Err(e) = store::set_on(&root, &pane, on) {
                    return Err(err("internal", format!("could not save the setting: {}", e.kind())));
                }
                let limits = self.replay.limits;
                let rt = self.panes.get_mut(&pane).expect("checked");
                if on {
                    if rt.rec.as_ref().is_none_or(|r| r.failed.is_some()) {
                        rt.rec = Some(Recorder::new(&root, &pane, limits));
                    }
                    rt.record_start();
                } else {
                    rt.rec = None;
                }
                let mut v = json!({ "type": "replay_state", "pane": pane, "recording": on });
                if let Some(start) = store::parts(&root, &pane).first().map(|p| p.start_ms) {
                    v["startMs"] = json!(start);
                }
                Ok(v)
            }
            Params::Read {
                pane,
                from,
                limit,
                markers_only,
            } => {
                self.replay_settle();
                if let Some(r) = self.panes.get_mut(&pane).and_then(|p| p.rec.as_mut()) {
                    r.flush();
                }
                match store::read(&root, &pane, &from, limit, markers_only) {
                    Ok(Some(pg)) => Ok(json!({
                        "type": "replay_page", "pane": pane, "startMs": pg.start_ms, "endMs": pg.end_ms,
                        "cols": pg.cols, "rows": pg.rows,
                        "events": pg.events.iter().map(|e| json!({ "at": e.at, "k": e.k, "d": e.d })).collect::<Vec<_>>(),
                        "next": pg.next.to_text(), "more": pg.more,
                    })),
                    Ok(None) => Err(err("not_found", "Nothing recorded for this session.")),
                    Err(e) => Err(err("internal", format!("could not read the recording: {}", e.kind()))),
                }
            }
            Params::Delete { pane } => {
                self.replay_settle();
                // An open part ends first, so it goes too (recording, if on, starts a new one).
                if let Some(r) = self.panes.get_mut(&pane).and_then(|p| p.rec.as_mut()) {
                    r.close();
                }
                let had = !store::parts(&root, &pane).is_empty();
                let bytes = store::delete(&root, &pane, None);
                if !had {
                    return Err(err("not_found", "Nothing recorded for this session."));
                }
                Ok(json!({ "type": "replay_deleted", "pane": pane, "bytes": bytes }))
            }
        }
    }
}
