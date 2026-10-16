//! The file browser's `fs.*` methods (docs/design/file-browser.md §4): the
//! method names, the limits, the one lexical path rule and the parameter
//! shapes. `packages/protocol/src/fs.ts` is the same contract; both are held
//! to `packages/protocol/fixtures/fs.json`.
//!
//! Every field of a request is hostile until parsed here: a wrong type, an
//! over-long string or an unknown shape is `invalid_params`, and a path the
//! lexical rule refuses is `invalid_path`. Nothing is normalised: a path that
//! would need cleaning is refused.

use serde_json::Value;

/// The runner hello feature (`hello.runner.features`).
pub const FEATURE_FS: &str = "fs";

/// The methods that only read.
pub const READ_METHODS: &[&str] = &["fs.roots", "fs.list", "fs.stat", "fs.read", "fs.search"];
/// The methods that change something on the machine. They travel through
/// web only (never a client socket) and need control and a step-up.
pub const MUTATING_METHODS: &[&str] = &[
    "fs.write",
    "fs.mkdir",
    "fs.rename",
    "fs.delete",
    "fs.import",
    "fs.undo",
    "fs.root_add",
    "fs.root_remove",
];

/// The mutating methods this runner builds (slice A2). The rest of
/// [`MUTATING_METHODS`] (`fs.import`, `fs.root_add`, `fs.root_remove`)
/// still answer `not_supported`.
pub const WRITE_METHODS: &[&str] = &["fs.write", "fs.mkdir", "fs.rename", "fs.delete", "fs.undo"];

/// Parameters only web may set on a mutating method (design §3.3): the
/// gateway strips them from anything a client socket sends, and the runner
/// refuses a mutation without them.
pub const WEB_FLAGS: &[&str] = &["control", "stepup_at", "by"];

/// How old a step-up may be for a write (D-FB4), milliseconds.
pub const STEPUP_WINDOW_MS: u64 = 15 * 60 * 1000;
/// How far ahead of this clock a step-up time may be (machines drift).
pub const STEPUP_SKEW_MS: u64 = 5 * 60 * 1000;
/// An `op_id` at most (bytes).
pub const MAX_OP_ID: usize = 64;

/// The gateway's own method for a download from a root (`/gw/files/fs/…`):
/// one window of a file, as FILE frames. Not a reading method: a client
/// socket never reaches it (the gateway refuses every other `fs.*` name
/// there) and the local socket refuses it like every `fs.*` method.
pub const DOWNLOAD_METHOD: &str = "fs.download";

/// A request path at most (bytes).
pub const MAX_PATH_BYTES: usize = 4096;
/// Components in a request path at most.
pub const MAX_COMPONENTS: usize = 64;
/// One component at most (bytes).
pub const MAX_COMPONENT_BYTES: usize = 255;
/// A root id at most (bytes).
pub const MAX_ROOT_ID: usize = 64;
/// Entries in one `fs.list` page at most (and by default).
pub const LIST_MAX: usize = 500;
/// A list cursor at most (bytes): `d/` or `f/` and one name.
pub const MAX_CURSOR_BYTES: usize = MAX_COMPONENT_BYTES + 2;
/// A search query at most (characters).
pub const SEARCH_QUERY_MAX: usize = 100;
/// Search results at most (and by default).
pub const SEARCH_MAX_RESULTS: usize = 50;
/// Text `fs.read` shows at most (bytes of the file).
pub const TEXT_MAX: u64 = 2 * 1024 * 1024;
/// A file is editable up to this size (bytes), complete, valid UTF-8, not masked.
pub const EDITABLE_MAX: u64 = 512 * 1024;
/// The binary sniff looks for a NUL in this many leading bytes.
pub const SNIFF_BYTES: usize = 8192;

/// The error codes an `fs.*` method answers (design §4).
pub const ERROR_CODES: &[&str] = &[
    "invalid_params",
    "invalid_path",
    "root_unknown",
    "root_gone",
    "root_refused",
    "outside_root",
    "symlink_refused",
    "linked_file",
    "file_refused",
    "not_found",
    "not_a_file",
    "not_a_dir",
    "exists",
    "not_empty",
    "too_large",
    "binary_file",
    "not_utf8",
    "masked_file",
    "conflict",
    "read_only",
    "control_required",
    "stepup_required",
    "confirm_pending",
    "insufficient_storage",
    "permission_denied",
    "rate_limited",
    "not_supported",
];

pub fn is_fs_method(method: &str) -> bool {
    method.starts_with("fs.")
}

/// A Windows device name, with or without an extension (`CON`, `nul.txt`,
/// `COM1`). Refused everywhere so a path means the same on every machine.
fn reserved_device(component: &str) -> bool {
    let stem = component.split('.').next().unwrap_or("").trim_end_matches(' ');
    let up = stem.to_ascii_uppercase();
    if matches!(up.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$") {
        return true;
    }
    if up.starts_with("COM") || up.starts_with("LPT") {
        let tail = &up[3..];
        return matches!(tail, "¹" | "²" | "³") || (tail.len() == 1 && matches!(tail.as_bytes()[0], b'1'..=b'9'));
    }
    false
}

/// One path component the rule accepts.
pub fn valid_component(p: &str) -> bool {
    !(p.is_empty()
        || p.len() > MAX_COMPONENT_BYTES
        || p == "."
        || p == ".."
        || p.chars().any(|c| c.is_control() || c == '\\' || c == '/' || c == ':')
        || p.ends_with(['.', ' '])
        || reserved_device(p))
}

/// THE lexical rule (design §3.4 step 1): the components of a relative,
/// `/`-separated request path, or None. Refused, never cleaned: an empty
/// path, an empty component, `.` and `..`, a leading or trailing `/`, `\`,
/// `:`, any control character, a component ending in a dot or a space, a
/// Windows device name, more than 4096 bytes, 64 components or 255 bytes a
/// component. The runner's jail resolves exactly what this accepts.
pub fn path_components(rel: &str) -> Option<Vec<&str>> {
    if rel.is_empty() || rel.len() > MAX_PATH_BYTES {
        return None;
    }
    let parts: Vec<&str> = rel.split('/').collect();
    (parts.len() <= MAX_COMPONENTS && parts.iter().all(|p| valid_component(p))).then_some(parts)
}

/// A root id as the runner gives them: 1 to 64 of `A-Z a-z 0-9 _ -`.
pub fn valid_root_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= MAX_ROOT_ID && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Where a list page starts: after this entry (a folder or not, and its name).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor {
    pub dir: bool,
    pub name: String,
}

impl Cursor {
    /// `d/<name>` or `f/<name>`. The name need not pass the path rule (a
    /// listing shows names the jail would not open), but it is one name.
    pub fn parse(s: &str) -> Option<Cursor> {
        if s.len() > MAX_CURSOR_BYTES {
            return None;
        }
        let (kind, name) = s.split_once('/')?;
        let dir = match kind {
            "d" => true,
            "f" => false,
            _ => return None,
        };
        if name.is_empty() || name.len() > MAX_COMPONENT_BYTES || name.contains(['/', '\0']) {
            return None;
        }
        Some(Cursor { dir, name: name.into() })
    }

    pub fn encode(&self) -> String {
        format!("{}/{}", if self.dir { "d" } else { "f" }, self.name)
    }
}

/// The parameters of a reading method, checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadParams {
    Roots,
    List {
        root: String,
        /// `""` is the root itself.
        path: String,
        cursor: Option<Cursor>,
        limit: usize,
    },
    Stat {
        root: String,
        path: String,
    },
    Read {
        root: String,
        path: String,
    },
    Search {
        root: String,
        query: String,
        limit: usize,
    },
}

/// Why parameters are refused: the error code and a reason for the message.
pub type ParamError = (&'static str, &'static str);

const BAD: &str = "invalid_params";

fn root_of(p: &Value) -> Result<String, ParamError> {
    match p.get("root").and_then(Value::as_str) {
        Some(r) if valid_root_id(r) => Ok(r.into()),
        _ => Err((BAD, "root must be a root id")),
    }
}

/// `fs.download`'s parameters, checked. `length` 0 asks for the size and the
/// refusals only (the gateway's first call); `xfer` names the FILE frames.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadParams {
    pub root: String,
    pub path: String,
    pub xfer: String,
    pub n: u32,
    pub offset: u64,
    pub length: u64,
}

/// Parse `fs.download {root, path, xfer, n?, offset?, length?}`. `length` is
/// capped at `window` (never an error: a larger ask is a smaller answer).
pub fn parse_download_params(p: &Value, window: u64) -> Result<DownloadParams, ParamError> {
    if !p.is_object() {
        return Err((BAD, "params must be an object"));
    }
    let xfer = match p.get("xfer").and_then(Value::as_str) {
        Some(x) if crate::files::valid_transfer_id(x) => x.to_string(),
        _ => return Err((BAD, "xfer must be a transfer id")),
    };
    let num = |k: &str| -> Result<u64, ParamError> {
        match p.get(k) {
            None | Some(Value::Null) => Ok(0),
            Some(v) => v.as_u64().ok_or((BAD, "offset, length and n must be whole numbers")),
        }
    };
    let n = num("n")?;
    if n > u32::MAX as u64 {
        return Err((BAD, "n is too large"));
    }
    Ok(DownloadParams {
        root: root_of(p)?,
        path: path_of(p, false)?,
        xfer,
        n: n as u32,
        offset: num("offset")?,
        length: num("length")?.min(window),
    })
}

/// `path`: a string. `""` (the root) only where `root_ok`.
fn path_of(p: &Value, root_ok: bool) -> Result<String, ParamError> {
    let Some(s) = p.get("path").and_then(Value::as_str) else {
        return Err((BAD, "path must be a string"));
    };
    if (s.is_empty() && root_ok) || path_components(s).is_some() {
        return Ok(s.into());
    }
    Err(("invalid_path", "path is not a relative path inside the root"))
}

/// An optional whole number from 1 to `max` (absent: `max`).
fn limit_of(p: &Value, max: usize) -> Result<usize, ParamError> {
    match p.get("limit") {
        None | Some(Value::Null) => Ok(max),
        Some(v) => match v.as_u64() {
            Some(n) if n >= 1 && n <= max as u64 => Ok(n as usize),
            _ => Err((BAD, "limit is out of range")),
        },
    }
}

/// Parse the parameters of one of [`READ_METHODS`]. Unknown fields are
/// ignored (a newer client may send more); known ones must be exactly right.
pub fn parse_read_params(method: &str, p: &Value) -> Result<ReadParams, ParamError> {
    if !p.is_object() {
        return Err((BAD, "params must be an object"));
    }
    match method {
        "fs.roots" => Ok(ReadParams::Roots),
        "fs.list" => {
            let cursor = match p.get("cursor") {
                None | Some(Value::Null) => None,
                Some(v) => Some(
                    v.as_str()
                        .and_then(Cursor::parse)
                        .ok_or((BAD, "cursor is not one this runner gave"))?,
                ),
            };
            Ok(ReadParams::List {
                root: root_of(p)?,
                path: path_of(p, true)?,
                cursor,
                limit: limit_of(p, LIST_MAX)?,
            })
        }
        "fs.stat" => Ok(ReadParams::Stat {
            root: root_of(p)?,
            path: path_of(p, false)?,
        }),
        "fs.read" => Ok(ReadParams::Read {
            root: root_of(p)?,
            path: path_of(p, false)?,
        }),
        "fs.search" => {
            let query = match p.get("query").and_then(Value::as_str) {
                Some(q)
                    if !q.trim().is_empty()
                        && q.chars().count() <= SEARCH_QUERY_MAX
                        && !q.chars().any(char::is_control) =>
                {
                    q.to_string()
                }
                _ => return Err((BAD, "query must be 1 to 100 characters")),
            };
            Ok(ReadParams::Search {
                root: root_of(p)?,
                query,
                limit: limit_of(p, SEARCH_MAX_RESULTS)?,
            })
        }
        _ => Err((BAD, "not a reading fs method")),
    }
}

/// A file version: `v1.<sha256 hex>.<size>.<mtimeNs>`. The hash decides;
/// size and mtime only let `fs.stat` answer without reading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub sha256: String,
    pub size: u64,
    pub mtime_ns: u64,
}

fn decimal(s: &str) -> Option<u64> {
    if s.is_empty() || s.len() > 20 || (s.len() > 1 && s.starts_with('0')) || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

impl Version {
    pub fn parse(s: &str) -> Option<Version> {
        let mut it = s.split('.');
        let (v, sha, size, mtime) = (it.next()?, it.next()?, it.next()?, it.next()?);
        if it.next().is_some() || v != "v1" || sha.len() != 64 {
            return None;
        }
        if !sha.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
            return None;
        }
        Some(Version {
            sha256: sha.into(),
            size: decimal(size)?,
            mtime_ns: decimal(mtime)?,
        })
    }

    pub fn encode(&self) -> String {
        format!("v1.{}.{}.{}", self.sha256, self.size, self.mtime_ns)
    }

    /// The same content (the hash and the size; the time is not compared).
    pub fn same_content(&self, other: &Version) -> bool {
        self.sha256 == other.sha256 && self.size == other.size
    }
}

/// What web vouches for on a mutating call. Absent fields are not a parse
/// error: the runner answers `control_required` / `stepup_required`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WebFlags {
    pub control: bool,
    /// When the caller last confirmed it was them (milliseconds since the epoch).
    pub stepup_at: Option<u64>,
    /// Who, for the machine's own journal (an opaque label web chooses).
    pub by: Option<String>,
}

impl WebFlags {
    /// `Ok` when control is held and the step-up is fresh at `now_ms`;
    /// else the contract's code.
    pub fn check(&self, now_ms: u64) -> Result<(), &'static str> {
        if !self.control {
            return Err("control_required");
        }
        match self.stepup_at {
            Some(t) if t <= now_ms.saturating_add(STEPUP_SKEW_MS) && now_ms.saturating_sub(t) <= STEPUP_WINDOW_MS => {
                Ok(())
            }
            _ => Err("stepup_required"),
        }
    }
}

/// One mutating operation, checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteOp {
    /// Replace the file at `path` whose version is `base`, or create it
    /// (`base: None`, i.e. `create: true`, never replacing anything).
    Write {
        root: String,
        path: String,
        text: String,
        base: Option<Version>,
    },
    /// One folder; the parent must exist.
    Mkdir { root: String, path: String },
    /// Within one root; never replaces. `base` checks a file's version.
    Rename {
        root: String,
        from: String,
        to: String,
        base: Option<Version>,
    },
    /// A file (its version must be `base`) or an empty folder (no `base`).
    Delete {
        root: String,
        path: String,
        base: Option<Version>,
    },
    /// Put back what one earlier operation changed.
    Undo { undo_id: String },
}

impl WriteOp {
    /// The method's short name (`write`, `mkdir`…), as the journal records it.
    pub fn name(&self) -> &'static str {
        match self {
            WriteOp::Write { .. } => "write",
            WriteOp::Mkdir { .. } => "mkdir",
            WriteOp::Rename { .. } => "rename",
            WriteOp::Delete { .. } => "delete",
            WriteOp::Undo { .. } => "undo",
        }
    }
}

/// A mutating request: the operation, its retry key and web's flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteRequest {
    pub op: WriteOp,
    pub op_id: String,
    pub flags: WebFlags,
}

/// An undo id as the runner gives them: `u` and 16 lower-case hex digits.
pub fn valid_undo_id(id: &str) -> bool {
    id.len() == 17
        && id.starts_with('u')
        && id[1..]
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn op_id_of(p: &Value) -> Result<String, ParamError> {
    match p.get("op_id").and_then(Value::as_str) {
        Some(s)
            if !s.is_empty()
                && s.len() <= MAX_OP_ID
                && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') =>
        {
            Ok(s.into())
        }
        _ => Err((BAD, "op_id must be 1 to 64 of A-Z a-z 0-9 _ -")),
    }
}

fn flags_of(p: &Value) -> Result<WebFlags, ParamError> {
    let control = match p.get("control") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        _ => return Err((BAD, "control must be a boolean")),
    };
    let stepup_at = match p.get("stepup_at") {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.as_u64().ok_or((BAD, "stepup_at must be a time in milliseconds"))?),
    };
    let by = match p.get("by") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if s.len() <= 200 && !s.chars().any(char::is_control) => Some(s.clone()),
        _ => return Err((BAD, "by must be a short label")),
    };
    Ok(WebFlags { control, stepup_at, by })
}

fn base_of(p: &Value) -> Result<Option<Version>, ParamError> {
    match p.get("base") {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_str()
            .and_then(Version::parse)
            .map(Some)
            .ok_or((BAD, "base must be a version")),
    }
}

/// A path field other than `path` (`from`, `to`), never the root itself.
fn named_path(p: &Value, key: &str) -> Result<String, ParamError> {
    let Some(s) = p.get(key).and_then(Value::as_str) else {
        return Err((BAD, "from and to must be strings"));
    };
    if path_components(s).is_some() {
        return Ok(s.into());
    }
    Err(("invalid_path", "path is not a relative path inside the root"))
}

/// Parse the parameters of one of [`WRITE_METHODS`]. Text is at most
/// [`EDITABLE_MAX`] bytes (`too_large`) and holds no NUL (`binary_file`);
/// a write names exactly one of `base` and `create: true`. Unknown fields
/// are ignored; web's flags are read but never required here.
pub fn parse_write_params(method: &str, p: &Value) -> Result<WriteRequest, ParamError> {
    if !p.is_object() {
        return Err((BAD, "params must be an object"));
    }
    let op = match method {
        "fs.write" => {
            let Some(Value::String(text)) = p.get("text") else {
                return Err((BAD, "text must be a string"));
            };
            if text.len() as u64 > EDITABLE_MAX {
                return Err(("too_large", "text is larger than a file the browser may write"));
            }
            if text.contains('\0') {
                return Err(("binary_file", "text must not hold a NUL"));
            }
            let create = match p.get("create") {
                None | Some(Value::Null) | Some(Value::Bool(false)) => false,
                Some(Value::Bool(true)) => true,
                _ => return Err((BAD, "create must be a boolean")),
            };
            let base = base_of(p)?;
            if create == base.is_some() {
                return Err((BAD, "a write names exactly one of base and create"));
            }
            WriteOp::Write {
                root: root_of(p)?,
                path: path_of(p, false)?,
                text: text.clone(),
                base,
            }
        }
        "fs.mkdir" => WriteOp::Mkdir {
            root: root_of(p)?,
            path: path_of(p, false)?,
        },
        "fs.rename" => WriteOp::Rename {
            root: root_of(p)?,
            from: named_path(p, "from")?,
            to: named_path(p, "to")?,
            base: base_of(p)?,
        },
        "fs.delete" => WriteOp::Delete {
            root: root_of(p)?,
            path: path_of(p, false)?,
            base: base_of(p)?,
        },
        "fs.undo" => match p.get("undoId").and_then(Value::as_str) {
            Some(u) if valid_undo_id(u) => WriteOp::Undo { undo_id: u.into() },
            _ => return Err((BAD, "undoId must be an undo id")),
        },
        _ => return Err((BAD, "not a writing fs method")),
    };
    Ok(WriteRequest {
        op,
        op_id: op_id_of(p)?,
        flags: flags_of(p)?,
    })
}
