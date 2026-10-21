//! Search history (docs/design/navigation.md §6, D-N2, D-N3): one SQLite file
//! per project, `<state>/history/<project>.sqlite`, with an FTS5 trigram index
//! so a substring is found in milliseconds over a month of lines.
//!
//! - **Opt-in per project, off by default.** Web names the projects in the
//!   `config` frame (`history: {projects: [{key, days?}]}`); nothing is
//!   written for any other folder, and a project taken off the list loses its
//!   file at once ("forget this project").
//! - **Masked before the write.** [`Store::write`] runs every string through
//!   the chat masker (the machine's literals, then shapes) itself, so no
//!   caller can put a raw secret in the file. `secure_delete` zeroes what
//!   retention and the size cap remove.
//! - **Bounded.** Each project keeps [`DEFAULT_DAYS`] (1 … [`MAX_DAYS`]) and
//!   at most [`CAP_BYTES`]; the oldest rows go first.
//! - **`search`** ([`parse_query`], [`search`]): newest first over every
//!   opted-in project, at most [`LIMIT_MAX`] hits, stopped at the deadline
//!   (`partial: true`). The machine's local socket never reaches it: anything
//!   in a pane could read every project's history through it.

use crate::chat::mask::{strip_controls, Masker};
use rusqlite::{params, Connection, OpenFlags};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The request web sends each machine (docs/protocol.md "Search history").
pub const METHOD: &str = "search";
pub const DEFAULT_DAYS: u32 = 30;
pub const MAX_DAYS: u32 = 365;
/// Most bytes one project's file may hold.
pub const CAP_BYTES: u64 = 64 * 1024 * 1024;
/// Most projects web may name.
pub const MAX_PROJECTS: usize = 200;
/// Longest stored text (a chat message is cut here).
pub const TEXT_MAX: usize = 8 * 1024;
pub const QUERY_MAX: usize = 200;
pub const LIMIT_DEFAULT: usize = 20;
pub const LIMIT_MAX: usize = 50;
/// How long one `search` may take on this machine.
pub const TIME_BOX: Duration = Duration::from_secs(5);
/// The snippet around a match, in characters.
pub const SNIPPET_MAX: usize = 160;
/// How often retention and the cap run per open store.
const PRUNE_EVERY: Duration = Duration::from_secs(10 * 60);
const DAY_MS: i64 = 24 * 60 * 60 * 1000;

/// One opted-in project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    pub key: String,
    pub days: u32,
    pub cap_bytes: u64,
}

impl Policy {
    pub fn new(key: &str, days: u32) -> Policy {
        Policy {
            key: key.to_string(),
            days: days.clamp(1, MAX_DAYS),
            cap_bytes: CAP_BYTES,
        }
    }
}

pub type Policies = BTreeMap<String, Policy>;

/// `config.history`: `{projects: [{key, days?}]}`. Keys as `config.projects`
/// names them (≤ 512 printable bytes).
pub fn validate(v: &Value) -> Result<Policies, String> {
    let list = v
        .get("projects")
        .and_then(Value::as_array)
        .ok_or("history.projects must be a list")?;
    if list.len() > MAX_PROJECTS {
        return Err(format!("history.projects holds more than {MAX_PROJECTS} projects"));
    }
    let mut out = Policies::new();
    for p in list {
        let key = p.get("key").and_then(Value::as_str).unwrap_or("");
        if key.is_empty() || key.len() > 512 || !key.bytes().all(|b| (0x21..0x7f).contains(&b)) {
            return Err("history.projects[].key must be 1–512 printable characters".into());
        }
        let days = match p.get("days") {
            None | Some(Value::Null) => DEFAULT_DAYS,
            Some(d) => d
                .as_u64()
                .filter(|d| (1..=MAX_DAYS as u64).contains(d))
                .ok_or(format!("history.projects[].days must be 1–{MAX_DAYS}"))? as u32,
        };
        out.insert(key.to_string(), Policy::new(key, days));
    }
    Ok(out)
}

/// The project's file name: a readable slug and a hash of the whole key
/// (two keys never share a file; the key itself is in the file's `meta`).
pub fn file_name(key: &str) -> String {
    use sha2::{Digest, Sha256};
    let tail = key.rsplit(['/', ':']).find(|s| !s.is_empty()).unwrap_or("project");
    let slug: String = tail
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .take(40)
        .collect();
    let h = hex::encode(&Sha256::digest(key.as_bytes())[..6]);
    format!("{slug}-{h}.sqlite")
}

/// One thing to remember: a terminal line or a chat message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub at_ms: i64,
    /// `terminal` | `chat`.
    pub kind: &'static str,
    /// The pane (`w1:p3`) or the chat session id.
    pub session: String,
    /// The session's name as the owner sees it.
    pub title: String,
    /// `claude`, `codex`, … or `shell`.
    pub agent: String,
    /// The chat turn, for a chat message.
    pub turn: Option<String>,
    /// The scrollback line number, for a terminal line.
    pub line: Option<u64>,
    pub text: String,
}

/// One project's file.
pub struct Store {
    conn: Connection,
    policy: Policy,
    pruned: Option<Instant>,
}

fn configure(conn: &Connection) -> rusqlite::Result<()> {
    conn.busy_timeout(Duration::from_secs(2))?;
    conn.pragma_update(None, "secure_delete", "ON")?;
    Ok(())
}

impl Store {
    /// Open (or create, `0600`) the project's file under `dir`.
    pub fn open(dir: &Path, policy: &Policy) -> rusqlite::Result<Store> {
        let _ = std::fs::create_dir_all(dir);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        }
        let path = dir.join(file_name(&policy.key));
        if !path.exists() {
            // Created private before SQLite opens it.
            let mut o = std::fs::OpenOptions::new();
            o.write(true).create(true).truncate(false);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                o.mode(0o600);
            }
            let _ = o.open(&path);
        }
        let conn = Connection::open(&path)?;
        configure(&conn)?;
        conn.pragma_update(None, "auto_vacuum", "INCREMENTAL")?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS meta(k TEXT PRIMARY KEY, v TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS entries(
               id INTEGER PRIMARY KEY, at INTEGER NOT NULL, kind TEXT NOT NULL,
               session TEXT NOT NULL, title TEXT NOT NULL, agent TEXT NOT NULL,
               turn TEXT, line INTEGER, text TEXT NOT NULL);
             CREATE INDEX IF NOT EXISTS entries_at ON entries(at);
             CREATE VIRTUAL TABLE IF NOT EXISTS entries_fts USING fts5(
               text, content='entries', content_rowid='id', tokenize='trigram');
             CREATE TRIGGER IF NOT EXISTS entries_ai AFTER INSERT ON entries BEGIN
               INSERT INTO entries_fts(rowid, text) VALUES (new.id, new.text); END;
             CREATE TRIGGER IF NOT EXISTS entries_ad AFTER DELETE ON entries BEGIN
               INSERT INTO entries_fts(entries_fts, rowid, text) VALUES ('delete', old.id, old.text); END;",
        )?;
        conn.execute(
            "INSERT OR REPLACE INTO meta(k, v) VALUES ('project', ?1)",
            params![policy.key],
        )?;
        let mut s = Store {
            conn,
            policy: policy.clone(),
            pruned: None,
        };
        s.prune(crate::util::now_ms() as i64)?;
        Ok(s)
    }

    pub fn set_policy(&mut self, policy: &Policy) {
        if *policy != self.policy {
            self.policy = policy.clone();
            self.pruned = None;
        }
    }

    /// Mask, then write, in one transaction. Returns the rows written.
    pub fn write(&mut self, entries: &[Entry], masker: &Masker) -> rusqlite::Result<usize> {
        let tx = self.conn.transaction()?;
        let mut n = 0;
        {
            let mut ins = tx.prepare_cached(
                "INSERT INTO entries(at, kind, session, title, agent, turn, line, text) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?;
            for e in entries {
                let text = masked(masker, &e.text, TEXT_MAX);
                if text.trim().is_empty() {
                    continue;
                }
                let title = masked(masker, &e.title, 200);
                ins.execute(params![
                    e.at_ms,
                    e.kind,
                    e.session,
                    title,
                    e.agent,
                    e.turn,
                    e.line.map(|l| l as i64),
                    text
                ])?;
                n += 1;
            }
        }
        tx.commit()?;
        if self.pruned.is_none_or(|t| t.elapsed() >= PRUNE_EVERY) {
            self.prune(crate::util::now_ms() as i64)?;
        }
        Ok(n)
    }

    /// Retention, then the size cap (oldest first). Returns the rows removed.
    pub fn prune(&mut self, now_ms: i64) -> rusqlite::Result<usize> {
        self.pruned = Some(Instant::now());
        let cutoff = now_ms - self.policy.days as i64 * DAY_MS;
        let mut removed = self
            .conn
            .execute("DELETE FROM entries WHERE at < ?1", params![cutoff])?;
        for _ in 0..20 {
            if self.size_bytes()? <= self.policy.cap_bytes {
                break;
            }
            let rows: i64 = self.conn.query_row("SELECT count(*) FROM entries", [], |r| r.get(0))?;
            if rows == 0 {
                break;
            }
            let drop = (rows / 5).max(1);
            removed += self.conn.execute(
                "DELETE FROM entries WHERE id IN (SELECT id FROM entries ORDER BY at, id LIMIT ?1)",
                params![drop],
            )?;
            self.compact()?;
        }
        if removed > 0 {
            self.compact()?;
        }
        Ok(removed)
    }

    /// After a delete: the index merged into one segment (its old pages freed,
    /// and zeroed by `secure_delete`), the free pages returned to the disk.
    /// (FTS5's own 'secure-delete' option is not used: with it, 'optimize'
    /// no longer shrinks the index.)
    fn compact(&self) -> rusqlite::Result<()> {
        self.conn
            .execute("INSERT INTO entries_fts(entries_fts) VALUES ('optimize')", [])?;
        // One page per step: stepped to the end, or only the first page goes.
        let mut st = self.conn.prepare("PRAGMA incremental_vacuum")?;
        let mut rows = st.query([])?;
        while rows.next()?.is_some() {}
        let _ = self.conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()));
        Ok(())
    }

    /// Pages in use (what the cap counts).
    pub fn size_bytes(&self) -> rusqlite::Result<u64> {
        let pages: i64 = self.conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;
        let free: i64 = self.conn.query_row("PRAGMA freelist_count", [], |r| r.get(0))?;
        let size: i64 = self.conn.query_row("PRAGMA page_size", [], |r| r.get(0))?;
        Ok(((pages - free).max(0) * size) as u64)
    }

    pub fn count(&self) -> rusqlite::Result<i64> {
        self.conn.query_row("SELECT count(*) FROM entries", [], |r| r.get(0))
    }
}

fn masked(masker: &Masker, s: &str, max: usize) -> String {
    // Control sequences out first, so a colour code inside a secret cannot hide it.
    let clean = strip_controls(s);
    let clean: String = clean.chars().take(max).collect();
    masker.text(&clean)
}

// ── the writer ─────────────────────────────────────────────────────────────

enum Job {
    Write {
        policy: Policy,
        entries: Vec<Entry>,
        masker: Masker,
    },
    Forget {
        key: String,
    },
}

/// The daemon's side: which projects keep history, where each pane's
/// scrollback was read up to, and a writer thread that owns the files.
pub struct Recorder {
    pub dir: PathBuf,
    /// None until web says (an older web never does: nothing is kept).
    pub policies: Option<Policies>,
    /// The policies as a `Projects` map, for [`crate::project::Resolver`].
    pub keys: crate::project::Projects,
    /// Pane → the next scrollback line to read.
    pub marks: HashMap<String, u64>,
    tx: Option<std::sync::mpsc::SyncSender<Job>>,
    pub dropped: u64,
    /// Searches running now (at most two at once).
    pub searching: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl Recorder {
    pub fn new(state_dir: &Path) -> Recorder {
        Recorder {
            dir: state_dir.join("history"),
            policies: None,
            keys: Default::default(),
            marks: HashMap::new(),
            tx: None,
            dropped: 0,
            searching: Default::default(),
        }
    }

    pub fn is_on(&self) -> bool {
        self.policies.as_ref().is_some_and(|p| !p.is_empty())
    }

    pub fn policy(&self, key: &str) -> Option<&Policy> {
        self.policies.as_ref()?.get(key)
    }

    /// New policies from web. Projects taken off the list lose their file.
    pub fn apply(&mut self, next: Policies) {
        let gone = existing_files(&self.dir)
            .into_iter()
            .filter(|(_, key)| !next.contains_key(key))
            .map(|(_, key)| key)
            .collect::<Vec<_>>();
        for key in gone {
            self.send(Job::Forget { key });
        }
        self.keys = next.keys().map(|k| (k.clone(), BTreeMap::new())).collect();
        if next.is_empty() {
            self.marks.clear();
        }
        self.policies = Some(next);
    }

    /// Queue entries for a project (masked by the writer before the write).
    pub fn record(&mut self, key: &str, entries: Vec<Entry>, masker: Masker) {
        let Some(policy) = self.policy(key).cloned() else {
            return;
        };
        if entries.is_empty() {
            return;
        }
        self.send(Job::Write {
            policy,
            entries,
            masker,
        });
    }

    fn send(&mut self, job: Job) {
        if self.tx.is_none() {
            let (tx, rx) = std::sync::mpsc::sync_channel::<Job>(256);
            let dir = self.dir.clone();
            let spawned = std::thread::Builder::new()
                .name("history".into())
                .spawn(move || writer(dir, rx));
            if spawned.is_err() {
                return;
            }
            self.tx = Some(tx);
        }
        if let Some(tx) = &self.tx {
            if tx.try_send(job).is_err() {
                self.dropped += 1;
            }
        }
    }
}

fn writer(dir: PathBuf, rx: std::sync::mpsc::Receiver<Job>) {
    let mut stores: HashMap<String, Store> = HashMap::new();
    while let Ok(job) = rx.recv() {
        match job {
            Job::Write {
                policy,
                entries,
                masker,
            } => {
                if !stores.contains_key(&policy.key) {
                    match Store::open(&dir, &policy) {
                        Ok(s) => {
                            stores.insert(policy.key.clone(), s);
                        }
                        Err(e) => {
                            crate::log!("history: opening a project's store: {e}");
                            continue;
                        }
                    }
                }
                let Some(s) = stores.get_mut(&policy.key) else { continue };
                s.set_policy(&policy);
                if let Err(e) = s.write(&entries, &masker) {
                    crate::log!("history: writing: {e}");
                }
            }
            Job::Forget { key } => {
                stores.remove(&key);
                forget(&dir, &key);
            }
        }
    }
}

/// Remove a project's file (and SQLite's side files).
pub fn forget(dir: &Path, key: &str) {
    let base = dir.join(file_name(key));
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let mut p = base.clone().into_os_string();
        p.push(suffix);
        let _ = std::fs::remove_file(PathBuf::from(p));
    }
}

/// The history files under `dir` and the project each holds.
pub fn existing_files(dir: &Path) -> Vec<(PathBuf, String)> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return vec![];
    };
    let mut out = vec![];
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("sqlite") {
            continue;
        }
        let Ok(c) = Connection::open_with_flags(&p, OpenFlags::SQLITE_OPEN_READ_ONLY) else {
            continue;
        };
        if let Ok(k) = c.query_row("SELECT v FROM meta WHERE k = 'project'", [], |r| r.get::<_, String>(0)) {
            out.push((p, k));
        }
    }
    out
}

/// Scrollback lines `from..` of a pane's history as logical lines (soft
/// wraps joined, so a value split by a wrap is masked whole). Returns the
/// lines (first line number, text) and where to read from next: a wrapped
/// last row waits for its continuation.
pub fn scrollback(h: &tmuxedo_emu::history::History, from: u64, max: usize) -> (Vec<(u64, String)>, u64) {
    let mut out = vec![];
    let mut i = from.max(h.start());
    let mut next = i;
    let (mut cur, mut first) = (String::new(), i);
    while i < h.end() && out.len() < max {
        let Some(l) = h.get(i) else { break };
        if cur.is_empty() {
            first = i;
        }
        cur.push_str(&l.text_untrimmed());
        i += 1;
        if !l.wrapped || cur.len() > TEXT_MAX {
            let t = cur.trim_end().to_string();
            if !t.trim().is_empty() {
                out.push((first, t));
            }
            cur.clear();
            next = i;
        }
    }
    (out, next)
}

// ── search ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    pub q: String,
    pub session: Option<String>,
    pub agent: Option<String>,
    pub since_ms: Option<i64>,
    pub project: Option<String>,
    pub limit: usize,
}

/// `search {q, session?, agent?, since?, project?, limit?}`.
pub fn parse_query(p: &Value) -> Result<Query, String> {
    let q = p.get("q").and_then(Value::as_str).ok_or("q is required")?;
    let q: String = q.chars().filter(|c| !c.is_control()).collect();
    let q = q.trim().to_string();
    if q.is_empty() || q.chars().count() > QUERY_MAX {
        return Err(format!("q must be 1–{QUERY_MAX} characters"));
    }
    let opt = |name: &str, max: usize| -> Result<Option<String>, String> {
        match p.get(name) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) if !s.is_empty() && s.len() <= max => Ok(Some(s.clone())),
            _ => Err(format!("{name} must be a string of 1–{max} characters")),
        }
    };
    let since_ms = match p.get("since") {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.as_i64().filter(|n| *n >= 0).ok_or("since must be a time in ms")?),
    };
    let limit = match p.get("limit") {
        None | Some(Value::Null) => LIMIT_DEFAULT,
        Some(v) => v
            .as_u64()
            .filter(|n| (1..=LIMIT_MAX as u64).contains(n))
            .ok_or(format!("limit must be 1–{LIMIT_MAX}"))? as usize,
    };
    Ok(Query {
        q,
        session: opt("session", 256)?,
        agent: opt("agent", 64)?,
        since_ms,
        project: opt("project", 512)?,
        limit,
    })
}

/// Search every opted-in project's file, newest first. Stops at `deadline`
/// with what it has (`partial: true`).
pub fn search(dir: &Path, policies: &Policies, q: &Query, deadline: Instant) -> Value {
    let mut hits: Vec<(i64, Value)> = vec![];
    let mut partial = false;
    let mut searched = 0;
    for p in policies.values() {
        if q.project.as_ref().is_some_and(|k| k != &p.key) {
            continue;
        }
        if Instant::now() >= deadline {
            partial = true;
            break;
        }
        let path = dir.join(file_name(&p.key));
        if !path.exists() {
            searched += 1;
            continue;
        }
        match search_one(&path, &p.key, q, deadline) {
            Ok((h, cut)) => {
                searched += 1;
                partial |= cut;
                hits.extend(h);
            }
            Err(e) => {
                if Instant::now() >= deadline {
                    partial = true;
                } else {
                    crate::log!("history: searching: {e}");
                }
            }
        }
    }
    hits.sort_by_key(|h| std::cmp::Reverse(h.0));
    let more = hits.len() > q.limit;
    hits.truncate(q.limit);
    json!({
        "type": "search_hits",
        "hits": hits.into_iter().map(|(_, h)| h).collect::<Vec<_>>(),
        "partial": partial,
        "more": more,
        "projects": searched,
    })
}

fn search_one(path: &Path, key: &str, q: &Query, deadline: Instant) -> rusqlite::Result<(Vec<(i64, Value)>, bool)> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
    configure(&conn)?;
    conn.progress_handler(1000, Some(move || Instant::now() >= deadline))?;
    let filters = "AND (?2 IS NULL OR e.session = ?2 OR e.title = ?2) AND (?3 IS NULL OR e.agent = ?3) AND (?4 IS NULL OR e.at >= ?4)";
    let cols = "e.at, e.kind, e.session, e.title, e.agent, e.turn, e.line, e.text";
    // Three characters or more: the trigram index (a phrase is a substring match).
    // Shorter: a scan, bounded by the deadline like everything else.
    let sql = if q.q.chars().count() >= 3 {
        format!("SELECT {cols} FROM entries_fts f JOIN entries e ON e.id = f.rowid WHERE entries_fts MATCH ?1 {filters} ORDER BY e.at DESC LIMIT ?5")
    } else {
        format!("SELECT {cols} FROM entries e WHERE instr(lower(e.text), lower(?1)) > 0 {filters} ORDER BY e.at DESC LIMIT ?5")
    };
    let needle = if q.q.chars().count() >= 3 {
        format!("\"{}\"", q.q.replace('"', "\"\""))
    } else {
        q.q.clone()
    };
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(
        params![needle, q.session, q.agent, q.since_ms, (q.limit + 1) as i64],
        |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, Option<i64>>(6)?,
                r.get::<_, String>(7)?,
            ))
        },
    )?;
    let mut out = vec![];
    let mut cut = false;
    for row in rows {
        match row {
            Ok((at, kind, session, title, agent, turn, line, text)) => {
                let (snippet, m) = snippet(&text, &q.q);
                let mut h = json!({
                    "project": key, "kind": kind, "session": session, "title": title, "agent": agent,
                    "at": crate::util::iso(at.max(0) as u64), "atMs": at, "snippet": snippet, "match": m,
                });
                if let Some(t) = turn {
                    h["turn"] = json!(t);
                }
                if let Some(l) = line {
                    h["line"] = json!(l);
                }
                out.push((at, h));
            }
            Err(rusqlite::Error::SqliteFailure(e, _)) if e.code == rusqlite::ErrorCode::OperationInterrupted => {
                cut = true;
                break;
            }
            Err(e) => return Err(e),
        }
    }
    Ok((out, cut))
}

/// Up to [`SNIPPET_MAX`] characters around the first match, and the match's
/// place in the snippet in UTF-16 units (what a browser's string counts), or
/// null when the match cannot be placed.
pub fn snippet(text: &str, q: &str) -> (String, Value) {
    let chars: Vec<char> = text.chars().collect();
    let needle: Vec<char> = q.chars().flat_map(char::to_lowercase).collect();
    let lower: Vec<char> = chars.iter().map(|c| c.to_lowercase().next().unwrap_or(*c)).collect();
    let pos = if needle.is_empty() || needle.len() > lower.len() {
        None
    } else {
        (0..=lower.len() - needle.len()).find(|&i| lower[i..i + needle.len()] == needle[..])
    };
    let Some(pos) = pos else {
        let s: String = chars.iter().take(SNIPPET_MAX).collect();
        return (s, Value::Null);
    };
    let len = needle.len();
    let before = SNIPPET_MAX.saturating_sub(len) / 3;
    let start = pos.saturating_sub(before);
    let end = (start + SNIPPET_MAX).min(chars.len());
    let start = end.saturating_sub(SNIPPET_MAX).min(start);
    let lead = if start > 0 { "…" } else { "" };
    let tail = if end < chars.len() { "…" } else { "" };
    let body: String = chars[start..end].iter().collect();
    let u16len = |cs: &[char]| cs.iter().map(|c| c.len_utf16()).sum::<usize>();
    let m0 = lead.encode_utf16().count() + u16len(&chars[start..pos]);
    let m1 = m0 + u16len(&chars[pos..(pos + len).min(end)]);
    (format!("{lead}{body}{tail}"), json!([m0, m1]))
}

// ── the daemon's side ──────────────────────────────────────────────────────

impl crate::daemon::Daemon {
    /// `config.history` from web.
    pub(crate) fn history_apply(&mut self, v: &Value) {
        match validate(v) {
            Ok(p) => {
                crate::log!("history: kept for {} project(s)", p.len());
                self.history.apply(p);
            }
            Err(e) => crate::log!("config: history: {e}"),
        }
    }

    /// The machine's masker plus a project's own secret-named settings.
    fn history_masker(&self, key: &str) -> Masker {
        let vars = self.launch.projects.get(key).into_iter().flatten();
        let mut m = Masker::from_env(vars.filter_map(|(k, v)| Some((k.as_str(), v.as_deref()?))));
        m.machine = self.secrets.shared();
        m
    }

    /// Every few seconds: the scrollback lines panes in opted-in projects
    /// have added since the last look. A pane seen for the first time starts
    /// at its end (what it replayed after a restart is not new).
    pub(crate) fn history_tick(&mut self) {
        if !self.history.is_on() {
            return;
        }
        let machine = self.delivery.machine.clone();
        let panes = &self.panes;
        self.history.marks.retain(|id, _| panes.contains_key(id));
        let ids: Vec<String> = self.panes.keys().cloned().collect();
        let mut work: Vec<(String, Vec<Entry>)> = vec![];
        for id in ids {
            let Some(rec) = self.state.pane(&id) else { continue };
            let p = &rec.pane;
            let key = self
                .projects
                .project_for(Path::new(&p.cwd), &machine, &self.history.keys);
            let Some(key) = key else {
                self.history.marks.remove(&id);
                continue;
            };
            let Some(h) = self.panes.get(&id).and_then(|rt| rt.emu.history()) else {
                continue;
            };
            let Some(&from) = self.history.marks.get(&id) else {
                self.history.marks.insert(id, h.end());
                continue;
            };
            let (lines, next) = scrollback(h, from, 4000);
            self.history.marks.insert(id.clone(), next);
            if lines.is_empty() {
                continue;
            }
            let now = crate::util::now_ms() as i64;
            let title = p
                .label
                .clone()
                .filter(|l| !l.is_empty())
                .unwrap_or_else(|| p.title.clone());
            let agent = p
                .agent
                .as_ref()
                .map(|a| a.kind.clone())
                .unwrap_or_else(|| "shell".into());
            let entries = lines
                .into_iter()
                .map(|(n, text)| Entry {
                    at_ms: now,
                    kind: "terminal",
                    session: id.clone(),
                    title: title.clone(),
                    agent: agent.clone(),
                    turn: None,
                    line: Some(n),
                    text,
                })
                .collect();
            work.push((key, entries));
        }
        for (key, entries) in work {
            let masker = self.history_masker(&key);
            self.history.record(&key, entries, masker);
        }
    }

    /// A chat message, for its folder's project when that keeps history.
    pub(crate) fn history_chat(&mut self, cwd: &str, entry: Entry, mut masker: Masker) {
        let machine = self.delivery.machine.clone();
        let Some(key) = self.projects.project_for(Path::new(cwd), &machine, &self.history.keys) else {
            return;
        };
        if let Some(vars) = self.launch.projects.get(&key) {
            for (k, v) in vars {
                if let Some(v) = v.as_deref().filter(|_| crate::chat::mask::is_secret_name(k)) {
                    masker.add(v);
                }
            }
        }
        self.history.record(&key, vec![entry], masker);
    }

    /// `search`: web only, answered from a blocking thread.
    pub(crate) fn search_request(&mut self, params: &Value, reply: crate::api::ReplyTo) {
        use crate::api::{err, respond, ReplyTo};
        if matches!(reply, ReplyTo::Local(_)) {
            let e = err(
                "not_supported",
                "Search is not available on the machine's local socket.",
            );
            return respond(reply, Err(e), self);
        }
        let q = match parse_query(params) {
            Ok(q) => q,
            Err(why) => return respond(reply, Err(err("invalid_params", format!("search: {why}"))), self),
        };
        let Some(policies) = self.history.policies.clone().filter(|p| !p.is_empty()) else {
            let r =
                json!({"type": "search_hits", "hits": [], "partial": false, "more": false, "projects": 0, "off": true});
            return respond(reply, Ok(r), self);
        };
        if self.history.searching.load(std::sync::atomic::Ordering::Relaxed) >= 2 {
            return respond(
                reply,
                Err(err("rate_limited", "A search is already running; try again.")),
                self,
            );
        }
        let (dir, tx, busy) = (
            self.history.dir.clone(),
            self.internal_tx.clone(),
            self.history.searching.clone(),
        );
        busy.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let deadline = Instant::now() + TIME_BOX;
        tokio::task::spawn_blocking(move || {
            let r = search(&dir, &policies, &q, deadline);
            busy.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            let _ = tx.send(crate::daemon::Internal::Fs {
                reply,
                result: Ok(r),
                frames: vec![],
            });
        });
    }
}
