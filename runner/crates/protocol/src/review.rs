//! Review and ship's `git.*` methods (docs/design/away-and-review.md §3, §4.2,
//! §5): the method names, the limits and the parameter shapes.
//! `packages/protocol/src/review.ts` is the same contract; both are held to
//! `packages/protocol/fixtures/review.json`.
//!
//! A request names a session the runner already knows (a pane, or a chat),
//! never a folder: the runner finds the work folder in its own records. A
//! file is named relative to the repository's top and must be one the
//! runner's own status listed; the lexical rule is the file browser's.

use crate::fs::path_components;
use serde_json::Value;

/// The runner hello feature (`hello.runner.features`).
pub const FEATURE_REVIEW: &str = "review";

/// The methods that only read.
pub const READ_METHODS: &[&str] = &["git.status", "git.diff"];
/// The methods that change the repository or reach a remote. Not served yet:
/// they will travel through web only, with control and a fresh step-up.
pub const MUTATING_METHODS: &[&str] = &["git.commit", "git.push", "git.discard"];

/// A session id at most (bytes).
pub const MAX_SESSION_ID: usize = 128;
/// Files one `git.status` lists at most (the rest is counted, `truncated`).
pub const STATUS_FILES_MAX: usize = 1000;
/// One file's patch at most (bytes, before masking); the rest is cut.
pub const PATCH_MAX: usize = 512 * 1024;
/// A file whose patch changes more lines than this is shown collapsed.
pub const LARGE_LINES: u64 = 2000;

/// Every `git.*` name: the read ones, the mutating ones, and any other.
pub fn is_review_method(method: &str) -> bool {
    method.starts_with("git.")
}

/// Whose work folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Pane(String),
    Chat(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadParams {
    Status { target: Target },
    Diff { target: Target, path: String },
}

pub type ParamError = (&'static str, String);

fn bad(why: &str) -> ParamError {
    ("invalid_params", why.to_string())
}

/// A session id: 1..=128 bytes of `[A-Za-z0-9_:.-]`.
pub fn valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_SESSION_ID
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b':' | b'.' | b'-'))
}

fn target(p: &Value) -> Result<Target, ParamError> {
    let (pane, chat) = (&p["pane"], &p["chat"]);
    match (pane.is_null(), chat.is_null()) {
        (false, true) => match pane.as_str() {
            Some(s) if valid_session_id(s) => Ok(Target::Pane(s.to_string())),
            _ => Err(bad("pane must be a session id")),
        },
        (true, false) => match chat.as_str() {
            Some(s) if valid_session_id(s) => Ok(Target::Chat(s.to_string())),
            _ => Err(bad("chat must be a session id")),
        },
        _ => Err(bad("exactly one of pane or chat is required")),
    }
}

/// Parse the parameters of one of [`READ_METHODS`]. Unknown fields are
/// ignored; known ones must be exactly right.
pub fn parse_read_params(method: &str, p: &Value) -> Result<ReadParams, ParamError> {
    if !p.is_object() {
        return Err(bad("params must be an object"));
    }
    match method {
        "git.status" => Ok(ReadParams::Status { target: target(p)? }),
        "git.diff" => {
            let target = target(p)?;
            let path = p["path"].as_str().ok_or_else(|| bad("path is required"))?;
            if path.is_empty() || path_components(path).is_none() {
                return Err(("invalid_path", "That path is not allowed.".into()));
            }
            Ok(ReadParams::Diff {
                target,
                path: path.to_string(),
            })
        }
        _ => Err(bad("not a reading git method")),
    }
}
