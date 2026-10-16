//! Checkpoints and undo (Phase 23, docs/design/away-and-review.md §3, §5;
//! docs/protocol.md "Checkpoints"): the method names, the limits and the
//! parameter shapes. `packages/protocol/src/checkpoint.ts` holds the same
//! names.
//!
//! A request names a session the runner already knows (a pane, or a chat),
//! never a folder: the runner finds the work folder in its own records, as
//! the review's `git.*` methods do. A checkpoint is named by its number in
//! that session.

use crate::review::valid_session_id;
use serde_json::Value;

/// The runner hello feature (`hello.runner.features`).
pub const FEATURE_CHECKPOINTS: &str = "checkpoints";

pub const METHOD_LIST: &str = "checkpoint.list";
pub const METHOD_PREVIEW: &str = "checkpoint.preview";
pub const METHOD_RESTORE: &str = "checkpoint.restore";
pub const METHOD_CREATE: &str = "checkpoint.create";

/// Every `checkpoint.*` method this protocol knows.
pub const METHODS: &[&str] = &[METHOD_LIST, METHOD_PREVIEW, METHOD_RESTORE, METHOD_CREATE];
/// The methods a client socket may send (with control of the owner's machine).
/// `checkpoint.restore` changes files: it travels through web only.
pub const SOCKET_METHODS: &[&str] = &[METHOD_LIST, METHOD_PREVIEW, METHOD_CREATE];

/// Checkpoints kept per session (oldest pruned first).
pub const KEEP: usize = 50;
/// Checkpoints older than this are pruned (milliseconds): 7 days.
pub const KEEP_MS: u64 = 7 * 24 * 60 * 60 * 1000;
/// A file larger than this is not in a checkpoint (and is never touched by a restore).
pub const LARGE_FILE: u64 = 50 * 1024 * 1024;
/// Files one preview lists at most (the rest is counted, `truncated`).
pub const PREVIEW_FILES_MAX: usize = 500;

/// What made a checkpoint.
pub const KIND_TURN: &str = "turn";
pub const KIND_RESTORE: &str = "restore";
pub const KIND_MANUAL: &str = "manual";

/// Every `checkpoint.*` name: known or not.
pub fn is_checkpoint_method(method: &str) -> bool {
    method.starts_with("checkpoint.")
}

/// Whose work folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Pane(String),
    Chat(String),
}

impl Target {
    /// The session key checkpoints are filed under (`pane:<id>` / `chat:<id>`).
    pub fn key(&self) -> String {
        match self {
            Target::Pane(p) => format!("pane:{p}"),
            Target::Chat(c) => format!("chat:{c}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Params {
    pub target: Target,
    /// The checkpoint (preview, restore).
    pub id: Option<u64>,
}

pub type ParamError = (&'static str, String);

fn bad(why: &str) -> ParamError {
    ("invalid_params", why.to_string())
}

/// Parse a `checkpoint.*` request's parameters: `{pane | chat, id?}`.
/// `id` is required by preview and restore, refused nowhere else.
pub fn parse_params(method: &str, p: &Value) -> Result<Params, ParamError> {
    if !p.is_object() {
        return Err(bad("params must be an object"));
    }
    let target = match (&p["pane"], &p["chat"]) {
        (Value::String(s), Value::Null) if valid_session_id(s) => Target::Pane(s.clone()),
        (Value::Null, Value::String(s)) if valid_session_id(s) => Target::Chat(s.clone()),
        _ => return Err(bad("exactly one of pane or chat (a session id) is required")),
    };
    let id = match &p["id"] {
        Value::Null => None,
        v => Some(
            v.as_u64()
                .filter(|n| *n >= 1 && *n < 1_000_000)
                .ok_or_else(|| bad("id must be a checkpoint number"))?,
        ),
    };
    let needs_id = matches!(method, METHOD_PREVIEW | METHOD_RESTORE);
    if needs_id && id.is_none() {
        return Err(bad("id is required"));
    }
    if !METHODS.contains(&method) {
        return Err(("unknown_method", format!("unknown method {method}")));
    }
    Ok(Params { target, id })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn params_name_one_session_and_a_number() {
        let p = parse_params(METHOD_LIST, &json!({"pane": "w1:p1"})).unwrap();
        assert_eq!(p.target, Target::Pane("w1:p1".into()));
        assert_eq!(p.target.key(), "pane:w1:p1");
        let r = parse_params(METHOD_RESTORE, &json!({"chat": "c1", "id": 3})).unwrap();
        assert_eq!((r.target, r.id), (Target::Chat("c1".into()), Some(3)));
        for (m, bad) in [
            (METHOD_LIST, json!({})),
            (METHOD_LIST, json!({"pane": "a", "chat": "b"})),
            (METHOD_LIST, json!({"pane": "has space"})),
            (METHOD_PREVIEW, json!({"pane": "a"})),
            (METHOD_RESTORE, json!({"pane": "a", "id": 0})),
            (METHOD_RESTORE, json!({"pane": "a", "id": "1"})),
            (METHOD_LIST, json!([])),
        ] {
            assert_eq!(parse_params(m, &bad).unwrap_err().0, "invalid_params", "{bad}");
        }
        assert_eq!(
            parse_params("checkpoint.nope", &json!({"pane": "a"})).unwrap_err().0,
            "unknown_method"
        );
        assert!(is_checkpoint_method("checkpoint.x") && !is_checkpoint_method("git.status"));
        assert!(!SOCKET_METHODS.contains(&METHOD_RESTORE));
    }
}
