//! Per-pane runtime state in the runner: the emulator, the byte mirror,
//! live-output coalescing, the snapshot gate and the agent tracker.

use crate::agent::Tracker;
use crate::mirror::Mirror;
use crate::secrets::mask::StreamMasker;
use std::time::{Duration, Instant};
use tmuxedo_emu::{EmuEvent, Emulator, SnapshotGate};
use tmuxedo_holder::{HolderClient, DEFAULT_RING};
use tmuxedo_protocol::codec::{encode_parts, max_payload};
use tmuxedo_protocol::DataKind;

/// Coalescing window and size for live output (docs/protocol.md).
pub const COALESCE: Duration = Duration::from_millis(10);
pub const COALESCE_BYTES: usize = 32 * 1024;
/// Replay frames can be larger.
pub const REPLAY_CHUNK: usize = 256 * 1024;
/// History bytes an attach snapshot carries (decision 0009): about 1,200
/// lines of a Claude session at 120 columns (2,300 at a phone's 50), less
/// than the 2,000-line snapshots before it, so a phone opens quickly; older
/// lines come by `pane.read` source `history` when the viewer scrolls up.
pub const SNAPSHOT_HISTORY_BYTES: usize = 96 * 1024;

pub enum Out {
    Bin(Vec<u8>),
}

pub struct PaneRt {
    pub id: String,
    pub hid: String,
    pub base: u64,
    pub pid: u32,
    /// Next holder offset expected; None until attached.
    pub holder_pos: Option<u64>,
    /// Holder ring end when we attached: reached = the emulator has caught up.
    pub catchup: Option<u64>,
    pub emu: Emulator,
    pub mirror: Mirror,
    pub live: bool,
    pub pending: Vec<u8>,
    pub pending_start: u64,
    pub pending_since: Option<Instant>,
    pub gap_next: bool,
    pub gate: SnapshotGate,
    pub cols: u16,
    pub rows: u16,
    pub alive: bool,
    /// Being closed: its exit removes it instead of reporting `exited`.
    pub closing: bool,
    pub track: Tracker,
    /// Mirror end at the last output-waiter check.
    pub checked_at: u64,
    /// Mirror end at the last Claude login-message check, and whether the
    /// screen matched then (alert only on a new match).
    pub auth_checked_at: u64,
    pub auth_match: bool,
    /// Secret masking between the holder's raw bytes and everything else
    /// (secrets-and-env.md §5): the mirror, emulator and live output only
    /// ever see its output. Raw bytes it holds back are not in the mirror
    /// yet, so the mirror's end trails `base + holder_pos` by `mask.held()`.
    pub mask: StreamMasker,
    /// Re-reading the holder's ring after [`PaneRt::restart_from_ring`]:
    /// data from the old attachment is ignored until `Attached`.
    pub restarting: bool,
    /// The daemon's mask set (it changes as secrets are delivered).
    pub masks: crate::secrets::Shared,
    /// Opt-in saved history: when it was last written and the stream end then.
    pub history_saved: Option<(Instant, u64)>,
}

impl PaneRt {
    pub fn new(id: &str, hid: String, base: u64, cols: u16, rows: u16, masks: crate::secrets::Shared) -> Self {
        Self {
            id: id.into(),
            hid,
            base,
            pid: 0,
            holder_pos: None,
            catchup: None,
            emu: Self::emulator(rows, cols, crate::config::HistorySize::DEFAULT),
            mirror: Mirror::new(DEFAULT_RING, base),
            live: false,
            pending: vec![],
            pending_start: base,
            pending_since: None,
            gap_next: false,
            gate: SnapshotGate::default(),
            cols,
            rows,
            alive: true,
            closing: false,
            track: Tracker::default(),
            checked_at: 0,
            auth_checked_at: 0,
            auth_match: false,
            mask: StreamMasker::new(masks.get()),
            restarting: false,
            masks,
            history_saved: None,
        }
    }

    fn emulator(rows: u16, cols: u16, h: crate::config::HistorySize) -> Emulator {
        Emulator::with_history_bytes(rows, cols, h.lines, h.bytes)
    }

    /// This machine's history size (a fresh pane, nothing fed yet).
    pub fn with_history(mut self, h: crate::config::HistorySize) -> Self {
        self.emu = Self::emulator(self.rows, self.cols, h);
        self
    }

    pub fn end(&self) -> u64 {
        self.mirror.end()
    }

    pub fn output(&self, req: u32, gap: bool, offset: u64, payload: &[u8]) -> Out {
        Out::Bin(encode_parts(DataKind::Output, gap, req, &self.id, offset, payload).unwrap_or_default())
    }

    /// One live chunk of at most `max` bytes.
    pub fn take_chunk(&mut self, max: usize, out: &mut Vec<Out>) {
        if self.pending.is_empty() {
            return;
        }
        let n = self.pending.len().min(max);
        let gap = std::mem::take(&mut self.gap_next);
        let frame = self.output(0, gap, self.pending_start, &self.pending[..n]);
        out.push(frame);
        self.pending.drain(..n);
        self.pending_start += n as u64;
        self.pending_since = if self.pending.is_empty() {
            None
        } else {
            Some(Instant::now())
        };
    }

    /// Everything pending, now.
    pub fn flush(&mut self, out: &mut Vec<Out>) {
        while !self.pending.is_empty() {
            self.take_chunk(COALESCE_BYTES, out);
        }
    }

    pub fn snapshot(&mut self, req: u32, out: &mut Vec<Out>) {
        self.flush(out);
        let budget = max_payload(&self.id) - 64;
        let payload = self.emu.snapshot_with(budget, SNAPSHOT_HISTORY_BYTES);
        let f = encode_parts(DataKind::Snapshot, false, req, &self.id, self.end(), &payload).unwrap_or_default();
        out.push(Out::Bin(f));
    }

    pub fn attach(&mut self, req: u32, from: Option<u64>, out: &mut Vec<Out>) {
        self.flush(out);
        match from {
            Some(f) if self.mirror.contains(f) => {
                let mut pos = f;
                while pos < self.end() {
                    let chunk = self.mirror.read(pos, REPLAY_CHUNK);
                    let frame = self.output(req, false, pos, &chunk);
                    out.push(frame);
                    pos += chunk.len() as u64;
                }
                if f == self.end() {
                    // Nothing to replay: an empty frame still tells the viewer where it stands.
                    let frame = self.output(req, false, f, &[]);
                    out.push(frame);
                }
            }
            _ => {
                if self.gate.request(req, Instant::now(), self.emu.in_sync()) {
                    self.snapshot(req, out);
                }
            }
        }
        self.live = true;
    }

    pub fn detach(&mut self) {
        self.live = false;
        self.pending.clear();
        self.pending_since = None;
        self.gate.cancel_all();
    }

    /// Feed holder bytes at holder offset `offset`. Returns true if a gap was seen.
    pub fn ingest(&mut self, offset: u64, bytes: &[u8], holder: &HolderClient, out: &mut Vec<Out>) -> bool {
        if self.restarting {
            return false;
        }
        let expected = self.holder_pos.unwrap_or(offset);
        let mut bytes = bytes;
        let mut gap = false;
        if offset > expected {
            gap = true;
            self.flush(out);
            self.mask.reset();
            self.emu.reset_screen();
            self.mirror.reset_at(self.base + offset);
            self.pending_start = self.base + offset;
            self.gap_next = true;
        } else if offset < expected {
            let skip = (expected - offset) as usize;
            if skip >= bytes.len() {
                return false;
            }
            bytes = &bytes[skip..];
        }
        self.holder_pos = Some(offset.max(expected) + bytes.len() as u64);
        if !bytes.is_empty() {
            self.track.on_output(Instant::now());
        }
        self.mask.set_mask(self.masks.get());
        let ready = self.mask.feed(bytes, Instant::now());
        self.process(&ready, holder, out);
        gap
    }

    /// Release bytes the masker held back for [`crate::secrets::mask::HOLDBACK`]
    /// (a possible secret prefix that never completed). Returns whether any were.
    pub fn release_held(&mut self, now: Instant, holder: &HolderClient, out: &mut Vec<Out>) -> bool {
        if !self.mask.due(now) {
            return false;
        }
        let tail = self.mask.flush();
        self.process(&tail, holder, out);
        true
    }

    /// Masked bytes into the emulator, the mirror and (when viewed) live output.
    fn process(&mut self, bytes: &[u8], holder: &HolderClient, out: &mut Vec<Out>) {
        let mut bytes = bytes;
        while !bytes.is_empty() {
            let (n, ev) = self.emu.feed_until_event(bytes);
            let seg = &bytes[..n];
            if self.live {
                if self.pending.is_empty() {
                    self.pending_start = self.end();
                    self.pending_since = Some(Instant::now());
                }
                self.pending.extend_from_slice(seg);
            }
            self.mirror.push(seg);
            bytes = &bytes[n..];
            match ev {
                Some(EmuEvent::SyncEnd) => {
                    for req in self.gate.sync_ended() {
                        self.snapshot(req, out);
                    }
                }
                Some(EmuEvent::CursorQuery) => holder.input(&self.hid, &self.emu.cursor_report()),
                _ => {}
            }
        }
    }

    /// Start the pane's derived state over from the holder's ring (the mask
    /// set grew: what is already in the mirror and emulator may hold a value
    /// that is now secret). The caller re-attaches from the ring's start.
    pub fn restart_from_ring(&mut self) {
        self.mask.reset();
        self.emu.reset();
        self.pending.clear();
        self.pending_since = None;
        self.holder_pos = None;
        self.restarting = true;
    }
}

impl PaneRt {
    /// Prepend saved history (opt-in pane history): it becomes the start of
    /// this incarnation's stream, and holder offsets shift past it.
    pub fn preload(&mut self, bytes: &[u8]) {
        // Saved under an older set, perhaps: masked again with today's.
        let masked = self.masks.get().mask(bytes);
        let bytes = masked.as_slice();
        self.emu.feed(bytes);
        self.mirror.push(bytes);
        self.base += bytes.len() as u64;
        self.pending_start = self.end();
    }

    /// Text of the pane for `pane.read` sources.
    pub fn read_text(&mut self, source: &str, lines: Option<usize>) -> Option<(String, bool)> {
        match source {
            "visible" => {
                let t = self.emu.visible_text();
                Some(match lines {
                    Some(n) => {
                        let all: Vec<&str> = t.lines().collect();
                        let skip = all.len().saturating_sub(n);
                        (all[skip..].join("\n"), skip > 0)
                    }
                    None => (t, false),
                })
            }
            "recent" | "recent_unwrapped" | "recent-unwrapped" => {
                let n = lines.unwrap_or(1000);
                // One more than asked tells whether there was more.
                let mut got = if source == "recent" {
                    self.emu.recent_lines(n.saturating_add(1))
                } else {
                    self.emu.recent_unwrapped(n.saturating_add(1))
                };
                let more = got.len() > n;
                if more {
                    got.remove(0);
                }
                Some((got.join("\n"), more))
            }
            "detection" => Some((self.emu.detection_text(), false)),
            _ => None,
        }
    }
}
