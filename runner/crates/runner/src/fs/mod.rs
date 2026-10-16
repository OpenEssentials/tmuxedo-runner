//! The file browser's reading methods (docs/design/file-browser.md §3, §4):
//! `fs.roots`, `fs.list`, `fs.stat`, `fs.read`, `fs.search`.
//!
//! ONE WAY IN. A request names a root (an id this runner gave) and a
//! relative path. The root is an open folder handle ([`crate::jail::Root`])
//! and every path is opened by the jail from that handle: nothing here
//! checks a path, joins one onto a folder or opens anything by name. What
//! the jail refuses is refused; this module only chooses the words.
//!
//! WHO. The methods answer on the gateway only. On the machine's local
//! socket (which anything in a pane can reach) every `fs.*` method is
//! `not_supported`, so an agent cannot browse (§3.1). The methods that
//! change files (`write.rs`) need web's control and step-up flags and a
//! writable root; `fs.import`, `fs.root_add` and `fs.root_remove` are not
//! built and answer `not_supported`.
//!
//! WHAT IS NEVER SAID. A secret-named entry is counted, never named
//! (`hidden`). A link is listed as a link, without its target, and cannot
//! be opened. An error names the root-relative path the caller sent at
//! most, and never says whether something exists outside the root.

mod roots;
#[cfg(test)]
mod tests;
pub mod write;
#[cfg(test)]
mod write_tests;

pub use roots::{Roots, Source};

use crate::api::{err, ApiErr, ApiResult};
use crate::chat::mask::Masker;
use crate::jail::{self, EntryKind, Refusal};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::cmp::Ordering;
use std::collections::VecDeque;
use std::io::Read;
use std::os::unix::fs::FileExt;
use std::os::unix::fs::MetadataExt;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tmuxedo_protocol::files::{FileFrame, FRAME_PAYLOAD, READ_WINDOW};
use tmuxedo_protocol::fs::{
    self as proto, Cursor, DownloadParams, ReadParams, Version, EDITABLE_MAX, LIST_MAX, SEARCH_MAX_RESULTS,
    SNIFF_BYTES, TEXT_MAX,
};

/// The methods this runner serves (the schema's list; all of them read).
pub const METHODS: &[&str] = proto::READ_METHODS;

/// Entries of one folder looked at, at most (a larger folder is `partial`).
pub const LIST_SCAN_MAX: usize = 20_000;
/// The text of one answer at most, as JSON (bytes). A `res` frame towards
/// the gateway is capped just under 1 MiB, so a preview is cut to fit it.
pub const REPLY_TEXT_MAX: usize = 880 * 1024;
/// Search: entries walked, time, depth (design §3.7).
pub const SEARCH_WALK_MAX: usize = 20_000;
pub const SEARCH_TIME: Duration = Duration::from_secs(2);
pub const SEARCH_DEPTH_MAX: usize = 12;
/// Requests being answered at once, at most (each holds a blocking thread).
pub const BUSY_MAX: usize = 4;

/// Folders listed dimmed and not searched: heavy, generated.
const HEAVY: &[&str] = &[
    "node_modules",
    "target",
    ".venv",
    "venv",
    "__pycache__",
    "dist",
    "build",
    ".next",
    ".cache",
    ".gradle",
    "Pods",
    "DerivedData",
];

/// What one request works on: the root's handle and what the answer needs.
#[derive(Clone)]
pub struct Ctx {
    pub id: String,
    pub root: Arc<jail::Root>,
    pub writable: bool,
    pub masker: Masker,
}

/// The one table of refusals (§4 "Errors"): the contract's code and words
/// that name nothing outside the root.
pub fn refusal(r: &Refusal, listing: bool) -> ApiErr {
    match r {
        Refusal::Invalid => err("invalid_path", "That path is not allowed."),
        Refusal::Outside => err("outside_root", "That is outside this folder."),
        Refusal::Symlink => err("symlink_refused", "Links are not followed."),
        Refusal::Secret(why) => err(
            "file_refused",
            format!("This may hold secrets ({why}), so tmuxedo does not show it."),
        ),
        Refusal::Linked => err(
            "linked_file",
            "This file has more than one name (a hard link), so tmuxedo does not open it.",
        ),
        Refusal::NotFound => err("not_found", "Nothing is there."),
        Refusal::NotAFile if listing => err("not_a_dir", "That is not a folder."),
        Refusal::NotAFile => err("not_a_file", "That is not a regular file."),
        Refusal::TooLarge { .. } => err("too_large", "That file is too large."),
        Refusal::RootGone => err("root_gone", "This folder was moved or replaced."),
        Refusal::Unreadable(_) => err("permission_denied", "tmuxedo cannot open that on this machine."),
    }
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

fn kind_str(k: EntryKind) -> &'static str {
    match k {
        EntryKind::File => "file",
        EntryKind::Dir => "dir",
        EntryKind::Link => "link",
        EntryKind::Other => "other",
    }
}

/// Names as people read them: digit runs by value, letters without case.
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (mut x, mut y) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (x.peek().copied(), y.peek().copied()) {
            (None, None) => return a.cmp(b),
            (None, _) => return Ordering::Less,
            (_, None) => return Ordering::Greater,
            (Some(c), Some(d)) if c.is_ascii_digit() && d.is_ascii_digit() => {
                let run = |it: &mut std::iter::Peekable<std::str::Chars>| {
                    let mut s = String::new();
                    while let Some(c) = it.peek().copied().filter(char::is_ascii_digit) {
                        s.push(c);
                        it.next();
                    }
                    s.trim_start_matches('0').to_string()
                };
                let (p, q) = (run(&mut x), run(&mut y));
                let o = p.len().cmp(&q.len()).then_with(|| p.cmp(&q));
                if o != Ordering::Equal {
                    return o;
                }
            }
            (Some(c), Some(d)) => {
                let (lc, ld) = (c.to_lowercase().next(), d.to_lowercase().next());
                if lc != ld {
                    return lc.cmp(&ld);
                }
                x.next();
                y.next();
            }
        }
    }
}

/// Folders first, then natural name order.
fn entry_cmp(a: (bool, &str), b: (bool, &str)) -> Ordering {
    b.0.cmp(&a.0).then_with(|| natural_cmp(a.1, b.1))
}

/// The folder at `path` (`""`: the root), as a readable handle from the jail.
fn open_folder(ctx: &Ctx, path: &str) -> Result<jail::Opened, Refusal> {
    if path.is_empty() {
        ctx.root.open_self()
    } else {
        ctx.root.open_dir(path)
    }
}

/// `fs.list`: one page of a folder. Secret-named entries are counted in
/// `hidden` and never named; names that are not text are counted in
/// `skipped` (they cannot be asked for); a link is `kind: link`.
pub fn list(ctx: &Ctx, path: &str, cursor: Option<&Cursor>, limit: usize) -> ApiResult {
    let dir = open_folder(ctx, path).map_err(|r| refusal(&r, true))?;
    let (raw, partial) = jail::read_entries(&dir.file, LIST_SCAN_MAX).map_err(|r| refusal(&r, true))?;
    let (mut hidden, mut skipped) = (0u64, 0u64);
    let mut shown = Vec::with_capacity(raw.len().min(4096));
    for e in raw {
        let Some(name) = e.name.to_str().map(str::to_string) else {
            skipped += 1;
            continue;
        };
        if jail::secret_reason(&join(&dir.rel, &name)).is_some() {
            hidden += 1;
            continue;
        }
        shown.push((name, e));
    }
    shown.sort_by(|a, b| entry_cmp((a.1.kind == EntryKind::Dir, &a.0), (b.1.kind == EntryKind::Dir, &b.0)));
    let start = match cursor {
        Some(c) => shown.partition_point(|(name, e)| {
            entry_cmp((e.kind == EntryKind::Dir, name), (c.dir, &c.name)) != Ordering::Greater
        }),
        None => 0,
    };
    let limit = limit.clamp(1, LIST_MAX);
    let page = &shown[start.min(shown.len())..];
    let next = (page.len() > limit).then(|| {
        let (name, e) = &page[limit - 1];
        Cursor {
            dir: e.kind == EntryKind::Dir,
            name: name.clone(),
        }
        .encode()
    });
    let entries: Vec<Value> = page
        .iter()
        .take(limit)
        .map(|(name, e)| {
            let mut v = json!({ "name": name, "kind": kind_str(e.kind), "size": e.size, "mtimeMs": e.mtime_ms });
            if e.kind == EntryKind::File {
                v["mime"] = json!(crate::chat::files::mime_of(name));
            }
            if e.kind == EntryKind::Dir && HEAVY.contains(&name.as_str()) {
                v["ignored"] = json!(true);
            }
            v
        })
        .collect();
    let mut out = json!({
        "type": "fs_list", "root": ctx.id, "path": path, "entries": entries,
        "hidden": hidden, "skipped": skipped, "partial": partial,
    });
    if let Some(n) = next {
        out["next"] = json!(n);
    }
    Ok(out)
}

/// What a file is, from its open handle: read once, at most [`TEXT_MAX`].
struct Facts {
    size: u64,
    mtime_ms: u64,
    version: Option<String>,
    kind: &'static str,
    /// The text to show (masked, cut to fit an answer), for a text file.
    text: Option<String>,
    truncated: bool,
    masked: bool,
    eol: &'static str,
    bom: bool,
    final_newline: bool,
    editable: bool,
    why: Option<&'static str>,
}

fn has_private_key(b: &[u8]) -> bool {
    const BEGIN: &[u8] = b"-----BEGIN ";
    const END: &[u8] = b"PRIVATE KEY-----";
    let mut from = 0;
    while let Some(i) = b[from..].windows(BEGIN.len()).position(|w| w == BEGIN) {
        let label = &b[from + i + BEGIN.len()..];
        let line = &label[..label.iter().position(|c| *c == b'\n').unwrap_or(label.len()).min(64)];
        if line.windows(END.len()).any(|w| w == END) {
            return true;
        }
        from += i + BEGIN.len();
    }
    false
}

/// Cut `s` so that it is at most `max` bytes as a JSON string.
fn fit_json(s: &str, max: usize) -> &str {
    let mut used = 2usize;
    for (i, c) in s.char_indices() {
        used += match c {
            '"' | '\\' | '\n' | '\r' | '\t' | '\u{8}' | '\u{c}' => 2,
            c if (c as u32) < 0x20 => 6,
            c => c.len_utf8(),
        };
        if used > max {
            return &s[..i];
        }
    }
    s
}

fn inspect(ctx: &Ctx, opened: &mut jail::Opened, name: &str, want_text: bool) -> Result<Facts, ApiErr> {
    let unreadable = |_| err("permission_denied", "tmuxedo cannot read that on this machine.");
    let meta_size = opened.meta.len();
    let mut bytes = Vec::with_capacity(meta_size.min(TEXT_MAX) as usize + 1);
    (&mut opened.file)
        .take(TEXT_MAX + 1)
        .read_to_end(&mut bytes)
        .map_err(unreadable)?;
    let after = opened.file.metadata().map_err(unreadable)?;
    let complete = bytes.len() as u64 <= TEXT_MAX;
    let size = if complete {
        bytes.len() as u64
    } else {
        after.len().max(bytes.len() as u64)
    };
    let mtime_ns = (after.mtime().max(0) as u64)
        .saturating_mul(1_000_000_000)
        .saturating_add(after.mtime_nsec().max(0) as u64);
    // The hash decides a version, so only a file read to its end has one.
    let version = complete.then(|| {
        Version {
            sha256: hex::encode(Sha256::digest(&bytes)),
            size,
            mtime_ns,
        }
        .encode()
    });
    bytes.truncate(TEXT_MAX as usize);
    // Key material is refused whatever the file is called (§3.5).
    if has_private_key(&bytes) {
        return Err(err(
            "file_refused",
            "This may hold secrets (key material), so tmuxedo does not show it.",
        ));
    }
    let mut f = Facts {
        size,
        mtime_ms: mtime_ns / 1_000_000,
        version,
        kind: "text",
        text: None,
        truncated: !complete,
        masked: false,
        eol: "lf",
        bom: false,
        final_newline: false,
        editable: false,
        why: None,
    };
    let mime = crate::chat::files::mime_of(name);
    if crate::chat::files::is_raster_image(mime) || mime == "application/pdf" {
        f.kind = if mime == "application/pdf" { "binary" } else { "image" };
        f.why = Some("binary");
        return Ok(f);
    }
    // The sniff is the first 8 KiB (design §3.7); a NUL anywhere in what was read is no text either.
    if bytes.iter().take(SNIFF_BYTES).any(|b| *b == 0) || bytes.contains(&0) {
        f.kind = "binary";
        f.why = Some("binary");
        return Ok(f);
    }
    let text = match String::from_utf8(bytes) {
        Ok(s) => s,
        // A cut may split a character: drop the partial tail; anything else is not text.
        Err(e) if !complete && e.as_bytes().len() - e.utf8_error().valid_up_to() < 4 => {
            let valid = e.utf8_error().valid_up_to();
            String::from_utf8_lossy(&e.as_bytes()[..valid]).into_owned()
        }
        Err(_) => {
            f.kind = "binary";
            f.why = Some("not_utf8");
            return Ok(f);
        }
    };
    f.bom = text.starts_with('\u{feff}');
    let body = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let crlf = body.matches("\r\n").count();
    let lf = body.matches('\n').count();
    f.eol = match (crlf, lf - crlf) {
        (0, _) => "lf",
        (_, 0) => "crlf",
        _ => "mixed",
    };
    f.final_newline = body.ends_with('\n');
    let shown = ctx.masker.text(body);
    f.masked = shown != body;
    let cut = fit_json(&shown, REPLY_TEXT_MAX);
    if cut.len() < shown.len() {
        f.truncated = true;
    }
    f.why = if f.truncated && size > EDITABLE_MAX {
        Some("too_large")
    } else if f.truncated {
        Some("truncated")
    } else if size > EDITABLE_MAX {
        Some("too_large")
    } else if f.masked {
        Some("masked")
    } else if !ctx.writable {
        Some("read_only_root")
    } else {
        None
    };
    f.editable = f.why.is_none();
    if want_text {
        f.text = Some(cut.to_string());
    }
    Ok(f)
}

fn last_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// `fs.stat`: one file or folder, and whether it could be edited.
pub fn stat(ctx: &Ctx, path: &str) -> ApiResult {
    let name = last_name(path);
    match ctx.root.open_file(path, jail::Limits::default()) {
        Ok(mut o) => {
            let f = inspect(ctx, &mut o, name, false)?;
            let mut out = json!({
                "type": "fs_stat",
                "entry": { "name": name, "kind": "file", "size": f.size, "mtimeMs": f.mtime_ms, "mime": crate::chat::files::mime_of(name) },
                "editable": f.editable,
            });
            if let Some(v) = f.version {
                out["version"] = json!(v);
            }
            if let Some(w) = f.why {
                out["why"] = json!(w);
            }
            Ok(out)
        }
        Err(Refusal::NotAFile) => {
            // Not a regular file: a folder is described, anything else is refused as before.
            let d = ctx.root.open_dir(path).map_err(|r| refusal(&r, false))?;
            let ms = (d.meta.mtime().max(0) as u64)
                .saturating_mul(1000)
                .saturating_add(d.meta.mtime_nsec().max(0) as u64 / 1_000_000);
            Ok(json!({
                "type": "fs_stat",
                "entry": { "name": name, "kind": "dir", "size": 0, "mtimeMs": ms },
                "editable": false,
            }))
        }
        Err(r) => Err(refusal(&r, false)),
    }
}

/// `fs.read`: a file's text (masked, cut to fit), or what it is instead.
pub fn read(ctx: &Ctx, path: &str) -> ApiResult {
    let name = last_name(path);
    let mut o = ctx
        .root
        .open_file(path, jail::Limits::default())
        .map_err(|r| refusal(&r, false))?;
    let f = inspect(ctx, &mut o, name, true)?;
    let mut out = json!({
        "type": "fs_file", "root": ctx.id, "path": path, "name": name,
        "mime": crate::chat::files::mime_of(name), "size": f.size, "mtimeMs": f.mtime_ms,
        "kind": f.kind, "truncated": f.truncated, "masked": f.masked, "eol": f.eol, "bom": f.bom,
        "finalNewline": f.final_newline, "editable": f.editable,
    });
    if let Some(v) = f.version {
        out["version"] = json!(v);
    }
    if let Some(t) = f.text {
        out["text"] = json!(t);
    }
    if let Some(w) = f.why {
        out["why"] = json!(w);
    }
    Ok(out)
}

/// `fs.search`: file and folder names holding `query`, within one root.
/// Bounded in entries, time, depth and results; skips links, secret-named
/// entries (counted), heavy folders and other filesystems.
pub fn search(ctx: &Ctx, query: &str, limit: usize) -> ApiResult {
    search_within(ctx, query, limit, SEARCH_WALK_MAX, SEARCH_TIME)
}

pub(crate) fn search_within(ctx: &Ctx, query: &str, limit: usize, walk_max: usize, time: Duration) -> ApiResult {
    let needle = jail::fold(query.trim());
    let limit = limit.clamp(1, SEARCH_MAX_RESULTS);
    let deadline = Instant::now() + time;
    let mut queue: VecDeque<(String, usize)> = VecDeque::from([(String::new(), 0)]);
    let (mut walked, mut hidden, mut partial) = (0usize, 0u64, false);
    let mut matches = Vec::new();
    'walk: while let Some((dir, depth)) = queue.pop_front() {
        let Ok(handle) = open_folder(ctx, &dir) else {
            continue;
        };
        let room = walk_max.saturating_sub(walked);
        let Ok((mut entries, more)) = jail::read_entries(&handle.file, room) else {
            continue;
        };
        partial |= more;
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        for e in entries {
            walked += 1;
            if Instant::now() >= deadline {
                partial = true;
                break 'walk;
            }
            let Some(name) = e.name.to_str() else {
                continue;
            };
            let rel = join(&handle.rel, name);
            if jail::secret_reason(&rel).is_some() {
                hidden += 1;
                continue;
            }
            // Only what could be opened is offered: a regular file or a folder, with a name the jail takes.
            if !matches!(e.kind, EntryKind::File | EntryKind::Dir) || !proto::valid_component(name) {
                continue;
            }
            if jail::fold(name).contains(&needle) {
                if matches.len() >= limit {
                    partial = true;
                    break 'walk;
                }
                matches.push(json!({ "path": rel, "kind": kind_str(e.kind) }));
            }
            if e.kind == EntryKind::Dir && !HEAVY.contains(&name) && e.dev == ctx.root.dev() {
                if depth + 1 >= SEARCH_DEPTH_MAX {
                    partial = true;
                } else {
                    queue.push_back((rel, depth + 1));
                }
            }
        }
        if walked >= walk_max {
            partial |= !queue.is_empty();
            break;
        }
    }
    Ok(json!({ "type": "fs_found", "matches": matches, "partial": partial, "hidden": hidden }))
}

/// Would the masker change this text? Only for what looks like text (no NUL
/// in it, valid UTF-8 but for a cut character at the end): a binary is sent
/// as it is (design §3.6, "Downloaded as-is").
fn masks(ctx: &Ctx, b: &[u8]) -> bool {
    if b.contains(&0) {
        return false;
    }
    let text = match std::str::from_utf8(b) {
        Ok(t) => t,
        Err(e) if b.len() - e.valid_up_to() < 4 => std::str::from_utf8(&b[..e.valid_up_to()]).unwrap_or(""),
        Err(_) => return false,
    };
    ctx.masker.text(text) != text
}

/// `fs.download` (the gateway's, for `/gw/files/fs/…`): one window of a file
/// as FILE frames. Every call opens the path through the jail again (a
/// secret name, a link, a hard link or a path outside the root is refused
/// whatever an earlier call found) and looks at the file's first 2 MiB and at
/// the window itself: key material is refused, and so is text the masker
/// would change (a download is the bytes as they are, so a file with hidden
/// values is not offered at all: `masked_file`).
pub fn download(ctx: &Ctx, d: &DownloadParams) -> Result<(Value, Vec<Vec<u8>>), ApiErr> {
    let o = ctx
        .root
        .open_file(&d.path, jail::Limits::default())
        .map_err(|r| refusal(&r, false))?;
    let unreadable = |_| err("permission_denied", "tmuxedo cannot read that on this machine.");
    let size = o.meta.len();
    let pread = |at: u64, want: u64| -> Result<Vec<u8>, ApiErr> {
        let mut out = vec![0u8; want as usize];
        let mut got = 0usize;
        while got < out.len() {
            let k = o.file.read_at(&mut out[got..], at + got as u64).map_err(unreadable)?;
            if k == 0 {
                break;
            }
            got += k;
        }
        out.truncate(got);
        Ok(out)
    };
    let head = pread(0, size.min(TEXT_MAX))?;
    let secret = || {
        err(
            "file_refused",
            "This may hold secrets (key material), so tmuxedo does not show it.",
        )
    };
    if has_private_key(&head) {
        return Err(secret());
    }
    let text = !head.iter().take(SNIFF_BYTES).any(|b| *b == 0);
    if text && masks(ctx, &head) {
        return Err(err(
            "masked_file",
            "This file holds values tmuxedo hides, so it is not downloaded.",
        ));
    }
    let start = d.offset.min(size);
    let end = size.min(start.saturating_add(d.length.min(READ_WINDOW)));
    let window = if end > start {
        pread(start, end - start)?
    } else {
        vec![]
    };
    if has_private_key(&window) {
        return Err(secret());
    }
    if text && start + window.len() as u64 > TEXT_MAX && masks(ctx, &window) {
        return Err(err(
            "masked_file",
            "This file holds values tmuxedo hides, so it is not downloaded.",
        ));
    }
    let mut frames = vec![];
    for (i, part) in window.chunks(FRAME_PAYLOAD).enumerate() {
        let fr = FileFrame {
            id: d.xfer.clone(),
            req: d.n,
            offset: start + (i * FRAME_PAYLOAD) as u64,
            payload: part.to_vec(),
        };
        frames.push(fr.encode().map_err(|_| err("io_error", "frame"))?);
    }
    let bytes = window.len() as u64;
    let name = last_name(&d.path);
    Ok((
        json!({ "type": "fs_download", "name": name, "size": size, "offset": d.offset, "bytes": bytes,
                "eof": start + bytes >= size, "mtimeMs": (o.meta.mtime().max(0) as u64).saturating_mul(1000) }),
        frames,
    ))
}

/// Run one parsed reading request against its root.
pub fn run(ctx: &Ctx, params: &ReadParams) -> ApiResult {
    match params {
        ReadParams::Roots => Err(err("invalid_params", "fs.roots takes no root")),
        ReadParams::List {
            path, cursor, limit, ..
        } => list(ctx, path, cursor.as_ref(), *limit),
        ReadParams::Stat { path, .. } => stat(ctx, path),
        ReadParams::Read { path, .. } => read(ctx, path),
        ReadParams::Search { query, limit, .. } => search(ctx, query, *limit),
    }
}

/// The limits a client should know (`fs_roots.limits`).
pub fn limits() -> Value {
    json!({
        "textBytes": TEXT_MAX, "replyTextBytes": REPLY_TEXT_MAX, "editableBytes": EDITABLE_MAX,
        "listPage": LIST_MAX, "listScan": LIST_SCAN_MAX, "searchResults": SEARCH_MAX_RESULTS,
        "searchEntries": SEARCH_WALK_MAX, "searchMs": SEARCH_TIME.as_millis() as u64, "searchDepth": SEARCH_DEPTH_MAX,
        "pathBytes": proto::MAX_PATH_BYTES, "nameBytes": proto::MAX_COMPONENT_BYTES,
    })
}

// ── the daemon's side ────────────────────────────────────────────────────

use crate::api::{respond, ReplyTo};
use crate::daemon::{Daemon, Internal};
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

/// The daemon's file-browser state: the roots and the work in flight.
#[derive(Default)]
pub struct Rt {
    pub roots: Roots,
    busy: Arc<AtomicUsize>,
    store: std::sync::OnceLock<Arc<std::sync::Mutex<write::Store>>>,
}

impl Daemon {
    /// Where the roots come from: the runner's own records, never a client.
    fn fs_sources(&self) -> Vec<Source> {
        let mut out: Vec<Source> = self
            .state
            .workspaces_sorted()
            .iter()
            .map(|w| Source {
                kind: "workspace",
                label: w.name.clone(),
                path: w.cwd.clone().into(),
            })
            .collect();
        for s in self.chats.sessions.values() {
            let cwd = std::path::PathBuf::from(&s.meta.cwd);
            if !cwd.is_absolute() {
                continue;
            }
            let top = crate::files::repo_of(&cwd).map_or(cwd, |(root, _)| root);
            let label = top
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            out.push(Source {
                kind: "project",
                label,
                path: top,
            });
        }
        out
    }

    pub(crate) fn fs_guard(&self) -> jail::Guard {
        let real = |p: std::path::PathBuf| p.canonicalize().unwrap_or(p);
        let mut own = vec![real(self.opts.state_dir.clone()), real(crate::paths::config_dir())];
        if let Some(f) = &self.files {
            own.push(real(f.root.clone()));
        }
        jail::Guard {
            home: Some(real(self.opts.home.clone().into())),
            own,
        }
    }

    /// Every `fs.*` request. Local callers and the methods that change
    /// files are refused here; a reading method is parsed, bound to its
    /// root's handle and answered from a blocking thread.
    pub(crate) fn fs_request(&mut self, method: &str, params: &Value, reply: ReplyTo) {
        if matches!(reply, ReplyTo::Local(_)) {
            let e = err(
                "not_supported",
                "The file browser is not available on the machine's local socket.",
            );
            return respond(reply, Err(e), self);
        }
        if method == proto::DOWNLOAD_METHOD {
            return self.fs_download(params, reply);
        }
        if proto::WRITE_METHODS.contains(&method) {
            return self.fs_write(method, params, reply);
        }
        if !METHODS.contains(&method) {
            let e = if proto::MUTATING_METHODS.contains(&method) {
                err("not_supported", "This runner does not change files from the browser.")
            } else {
                err("unknown_method", format!("unknown method {method}"))
            };
            return respond(reply, Err(e), self);
        }
        let parsed = match proto::parse_read_params(method, params) {
            Ok(p) => p,
            Err((code, why)) => return respond(reply, Err(err(code, format!("{method}: {why}"))), self),
        };
        let (sources, guard) = (self.fs_sources(), self.fs_guard());
        self.fs.roots.sync(&sources, &guard);
        let root_id = match &parsed {
            ReadParams::Roots => {
                let mut r = self.fs.roots.describe();
                if self.opts.guardrails.observe_only {
                    r["canWrite"] = json!(false);
                    r["why"] = json!("observe_only");
                }
                return respond(reply, Ok(r), self);
            }
            ReadParams::List { root, .. }
            | ReadParams::Stat { root, .. }
            | ReadParams::Read { root, .. }
            | ReadParams::Search { root, .. } => root.clone(),
        };
        let (root, writable) = match self.fs.roots.get(&root_id) {
            Ok(r) => r,
            Err(e) => return respond(reply, Err(e), self),
        };
        if self.fs.busy.load(AtomicOrdering::Relaxed) >= BUSY_MAX {
            let e = err("rate_limited", "Too many file requests at once; try again.");
            return respond(reply, Err(e), self);
        }
        // No chat's own values here: what the machine masks (deliveries, shell settings), then shapes.
        let mut masker = Masker::from_env(std::iter::empty::<(&str, &str)>());
        masker.machine = self.secrets.shared();
        let ctx = Ctx {
            id: root_id,
            root,
            writable,
            masker,
        };
        let (busy, tx) = (self.fs.busy.clone(), self.internal_tx.clone());
        busy.fetch_add(1, AtomicOrdering::Relaxed);
        tokio::task::spawn_blocking(move || {
            let result = run(&ctx, &parsed);
            busy.fetch_sub(1, AtomicOrdering::Relaxed);
            let _ = tx.send(Internal::Fs {
                reply,
                result,
                frames: vec![],
            });
        });
    }

    /// A changing method: from the gateway only, with web's flags, on a
    /// writable root; one at a time (the store's lock).
    fn fs_write(&mut self, method: &str, params: &Value, reply: ReplyTo) {
        let req = match proto::parse_write_params(method, params) {
            Ok(r) => r,
            Err((code, why)) => return respond(reply, Err(err(code, format!("{method}: {why}"))), self),
        };
        if let Err(code) = req.flags.check(write::now_ms()) {
            let msg = if code == "control_required" {
                "Changing files needs control of this machine."
            } else {
                "Confirm it's you first."
            };
            return respond(reply, Err(err(code, msg)), self);
        }
        let store = self
            .fs
            .store
            .get_or_init(|| Arc::new(std::sync::Mutex::new(write::Store::new(&self.opts.state_dir))))
            .clone();
        let root_id = match &req.op {
            proto::WriteOp::Write { root, .. }
            | proto::WriteOp::Mkdir { root, .. }
            | proto::WriteOp::Rename { root, .. }
            | proto::WriteOp::Delete { root, .. } => root.clone(),
            proto::WriteOp::Undo { undo_id } => match store.lock().ok().and_then(|s| s.root_of(undo_id)) {
                Some(r) => r,
                None => return respond(reply, Err(err("not_found", "That can no longer be undone.")), self),
            },
        };
        let (sources, guard) = (self.fs_sources(), self.fs_guard());
        self.fs.roots.sync(&sources, &guard);
        let (root, writable) = match self.fs.roots.get(&root_id) {
            Ok(r) => r,
            Err(e) => return respond(reply, Err(e), self),
        };
        if self.fs.busy.load(AtomicOrdering::Relaxed) >= BUSY_MAX {
            let e = err("rate_limited", "Too many file requests at once; try again.");
            return respond(reply, Err(e), self);
        }
        let mut masker = Masker::from_env(std::iter::empty::<(&str, &str)>());
        masker.machine = self.secrets.shared();
        let ctx = Ctx {
            id: root_id,
            root,
            writable,
            masker,
        };
        let (busy, tx) = (self.fs.busy.clone(), self.internal_tx.clone());
        busy.fetch_add(1, AtomicOrdering::Relaxed);
        tokio::task::spawn_blocking(move || {
            let result = match store.lock() {
                Ok(mut s) => write::run(&mut s, &ctx, &req, write::now_ms()),
                Err(_) => Err(err("permission_denied", "The machine refused that change.")),
            };
            busy.fetch_sub(1, AtomicOrdering::Relaxed);
            let _ = tx.send(Internal::Fs {
                reply,
                result,
                frames: vec![],
            });
        });
    }

    /// `fs.download`: from the gateway only (the local socket was refused above).
    fn fs_download(&mut self, params: &Value, reply: ReplyTo) {
        let d = match proto::parse_download_params(params, READ_WINDOW) {
            Ok(d) => d,
            Err((code, why)) => return respond(reply, Err(err(code, format!("fs.download: {why}"))), self),
        };
        let (sources, guard) = (self.fs_sources(), self.fs_guard());
        self.fs.roots.sync(&sources, &guard);
        let (root, writable) = match self.fs.roots.get(&d.root) {
            Ok(r) => r,
            Err(e) => return respond(reply, Err(e), self),
        };
        if self.fs.busy.load(AtomicOrdering::Relaxed) >= BUSY_MAX {
            let e = err("rate_limited", "Too many file requests at once; try again.");
            return respond(reply, Err(e), self);
        }
        let mut masker = Masker::from_env(std::iter::empty::<(&str, &str)>());
        masker.machine = self.secrets.shared();
        let ctx = Ctx {
            id: d.root.clone(),
            root,
            writable,
            masker,
        };
        let (busy, tx) = (self.fs.busy.clone(), self.internal_tx.clone());
        busy.fetch_add(1, AtomicOrdering::Relaxed);
        tokio::task::spawn_blocking(move || {
            let (result, frames) = match download(&ctx, &d) {
                Ok((v, f)) => (Ok(v), f),
                Err(e) => (Err(e), vec![]),
            };
            busy.fetch_sub(1, AtomicOrdering::Relaxed);
            let _ = tx.send(Internal::Fs { reply, result, frames });
        });
    }
}
