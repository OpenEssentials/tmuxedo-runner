//! Session recording and replay's `replay.*` methods
//! (docs/design/sharing-and-integrations.md §3, §7): the method names, the
//! limits and the parameter shapes. `packages/protocol/src/replay.ts` is the
//! same contract; both are held to `packages/protocol/fixtures/replay.json`.
//!
//! A request names a session (a pane) the runner records, never a file: the
//! runner keeps its recordings in its own folder. Recording is off until the
//! owner turns it on for one session.

use crate::review::valid_session_id;
use serde_json::Value;

/// The runner hello feature (`hello.runner.features`).
pub const FEATURE_REPLAY: &str = "replay";

/// Every `replay.*` method the runner serves.
pub const METHODS: &[&str] = &["replay.list", "replay.set", "replay.read", "replay.delete"];

/// One `replay.read` page at most (bytes of event data), and its default.
pub const READ_BYTES_MAX: usize = 512 * 1024;
pub const READ_BYTES_DEFAULT: usize = 256 * 1024;
/// Retention (decision D-SI4): 14 days, 1 GiB per machine, oldest first.
pub const RETENTION_DAYS: u64 = 14;
pub const MAX_BYTES: u64 = 1024 * 1024 * 1024;
/// A recording is kept in parts so retention can drop the oldest: a new part
/// starts at this size or age, each with a redraw of the screen first.
pub const SEGMENT_BYTES: u64 = 16 * 1024 * 1024;
pub const SEGMENT_MS: u64 = 6 * 60 * 60 * 1000;

/// Every `replay.*` name, served or not.
pub fn is_replay_method(method: &str) -> bool {
    method.starts_with("replay.")
}

/// Where a read starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum From {
    /// The oldest part kept.
    Start,
    /// The part holding this moment (from its start, so the screen is whole).
    At(u64),
    /// Where the previous page ended.
    Cursor(Cursor),
}

/// A position in a recording: the part (its start, ms), the byte offset in
/// its file and the time at that offset (ms since the epoch).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    pub part: u64,
    pub offset: u64,
    pub at: u64,
}

impl Cursor {
    pub fn parse(s: &str) -> Option<Cursor> {
        let mut it = s.split('.');
        let n = |x: Option<&str>| -> Option<u64> {
            let x = x?;
            if x.is_empty() || x.len() > 16 || !x.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            x.parse().ok()
        };
        let c = Cursor {
            part: n(it.next())?,
            offset: n(it.next())?,
            at: n(it.next())?,
        };
        it.next().is_none().then_some(c)
    }

    pub fn to_text(self) -> String {
        format!("{}.{}.{}", self.part, self.offset, self.at)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Params {
    List {
        pane: Option<String>,
    },
    Set {
        pane: String,
        on: bool,
    },
    Read {
        pane: String,
        from: From,
        limit: usize,
        markers_only: bool,
    },
    Delete {
        pane: String,
    },
}

pub type ParamError = (&'static str, String);

fn bad(why: &str) -> ParamError {
    ("invalid_params", why.to_string())
}

fn pane(p: &Value) -> Result<String, ParamError> {
    match p["pane"].as_str() {
        Some(s) if valid_session_id(s) => Ok(s.to_string()),
        _ => Err(bad("pane must be a session id")),
    }
}

fn whole(v: &Value, what: &str) -> Result<Option<u64>, ParamError> {
    if v.is_null() {
        return Ok(None);
    }
    v.as_u64()
        .map(Some)
        .ok_or_else(|| bad(&format!("{what} must be a whole number")))
}

/// Parse the parameters of one of [`METHODS`]. Unknown fields are ignored;
/// known ones must be exactly right.
pub fn parse_params(method: &str, p: &Value) -> Result<Params, ParamError> {
    if !p.is_object() {
        return Err(bad("params must be an object"));
    }
    match method {
        "replay.list" => Ok(Params::List {
            pane: if p["pane"].is_null() { None } else { Some(pane(p)?) },
        }),
        "replay.set" => Ok(Params::Set {
            pane: pane(p)?,
            on: p["on"].as_bool().ok_or_else(|| bad("on must be true or false"))?,
        }),
        "replay.read" => {
            let pane = pane(p)?;
            let from_ms = whole(&p["from_ms"], "from_ms")?;
            let cursor = match &p["cursor"] {
                Value::Null => None,
                v => Some(
                    v.as_str()
                        .and_then(Cursor::parse)
                        .ok_or_else(|| bad("cursor is not one this runner gave"))?,
                ),
            };
            let from = match (from_ms, cursor) {
                (Some(_), Some(_)) => return Err(bad("from_ms and cursor are exclusive")),
                (Some(t), None) => From::At(t),
                (None, Some(c)) => From::Cursor(c),
                (None, None) => From::Start,
            };
            let limit = match whole(&p["limit_bytes"], "limit_bytes")? {
                None => READ_BYTES_DEFAULT,
                Some(n) if (1..=READ_BYTES_MAX as u64).contains(&n) => n as usize,
                Some(_) => return Err(bad("limit_bytes is 1 to 524288")),
            };
            let markers_only = match &p["markers_only"] {
                Value::Null => false,
                Value::Bool(b) => *b,
                _ => return Err(bad("markers_only must be true or false")),
            };
            Ok(Params::Read {
                pane,
                from,
                limit,
                markers_only,
            })
        }
        "replay.delete" => Ok(Params::Delete { pane: pane(p)? }),
        _ => Err(bad("not a replay method")),
    }
}
