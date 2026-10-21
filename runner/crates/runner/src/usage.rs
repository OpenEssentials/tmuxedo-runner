//! AI usage (docs/design/agent-usage.md; docs/protocol.md "AI usage"): how
//! much of a plan's limits an agent CLI's account has used, read from what
//! the CLI itself reports and never asked of the vendor.
//!
//! - **Status line** (`hook --statusline`, D-U2): the agent runs this as its
//!   status-line command. It picks the numbers the manifest's `[usage]`
//!   source names out of the JSON on stdin, sends them to the runner, then
//!   runs the person's own status-line command with the same stdin and
//!   prints its output unchanged. Whatever fails here, their line still runs.
//! - **Session log** (`jsonl_tail`): the end of the log the agent's hook
//!   names, read only, the newest line the manifest selects.
//!
//! Only numbers, reset times and window ids leave this module: never the
//! vendor's JSON, a path, a prompt, a token or an account name. Logs name
//! the plugin and the source, nothing else.

use crate::plugin::manifest::{UsageSection, UsageSource, UsageWindowMap};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tmuxedo_protocol::usage::{
    usage_plan_ok, SessionUsage, SpentWindow, UsageContext, UsageRecord, UsageWindow, SESSION_TOKENS_MAX,
    USAGE_MIN_INTERVAL_MS, USAGE_WINDOWS_MAX,
};

/// The most status-line JSON read (a real one is a few KiB). More than this:
/// nothing is recorded and the person's command gets what was read.
pub const STDIN_MAX: u64 = 1024 * 1024;
/// The most of the person's status line passed on.
pub const OUTPUT_MAX: u64 = 64 * 1024;
/// How long the person's status-line command may run before it is stopped.
pub const USER_TIMEOUT: Duration = Duration::from_secs(5);
/// The most read from the end of a session log, and the longest line parsed.
pub const TAIL_MAX: u64 = 256 * 1024;
pub const LINE_MAX: usize = 64 * 1024;
/// The most settings file read when looking for the person's status line.
const SETTINGS_MAX: u64 = 1024 * 1024;
/// At most this many plugins' records are kept.
const ACCOUNTS_MAX: usize = 16;

/// What one source read: the numbers only.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reading {
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub windows: Vec<UsageWindow>,
    /// `[used percent, size in tokens]` of the context window.
    #[serde(default)]
    pub context: Option<(f64, Option<u64>)>,
    #[serde(default)]
    pub plan: Option<String>,
    /// The session's running token total, as the agent reported it.
    #[serde(default)]
    pub tokens: Option<u64>,
}

/// The value at a dotted field path.
pub fn at<'a>(v: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.').try_fold(v, |v, k| v.as_object()?.get(k))
}

/// A percentage a CLI reported: a number, not negative; above 100 is 100.
fn pct(v: Option<&Value>) -> Option<f64> {
    let n = v?.as_f64()?;
    (n.is_finite() && (0.0..=1000.0).contains(&n)).then(|| (n.min(100.0) * 10.0).round() / 10.0)
}

fn whole(v: Option<&Value>, min: u64, max: u64) -> Option<u64> {
    let v = v?;
    let n = v
        .as_u64()
        .or_else(|| v.as_f64().filter(|f| f.is_finite() && *f >= 0.0).map(|f| f as u64))?;
    (min..=max).contains(&n).then_some(n)
}

fn window_id(mins: u32) -> String {
    match mins {
        300 => "5h".into(),
        10_080 => "7d".into(),
        m => format!("{m}m"),
    }
}

fn window(m: &UsageWindowMap, v: &Value, now_s: u64) -> Option<UsageWindow> {
    let o = at(v, &m.at)?;
    let used_pct = pct(at(o, &m.pct))?;
    let window_mins = match &m.mins_field {
        Some(f) => whole(at(o, f), 1, 527_040).map(|n| n as u32),
        None => m.mins,
    };
    let id = m.id.clone().or_else(|| window_mins.map(window_id))?;
    // Unix seconds only: anything else (milliseconds, a date in words) is left out.
    let resets_at = m
        .resets
        .as_ref()
        .and_then(|f| whole(at(o, f), 1_000_000_000, 99_999_999_999));
    // A window whose reset has passed says nothing about now.
    if resets_at.is_some_and(|r| r <= now_s) {
        return None;
    }
    let w = UsageWindow {
        id,
        used_pct,
        resets_at,
        window_mins,
    };
    w.ok().then_some(w)
}

/// The numbers `source` names in `v`. A field that is missing, renamed or
/// not the kind of value expected is left out, never guessed.
pub fn extract(source: &UsageSource, v: &Value, now_s: u64) -> Reading {
    let mut r = Reading {
        source: source.wire_name().into(),
        ..Default::default()
    };
    for m in &source.windows {
        if let Some(w) = window(m, v, now_s) {
            if r.windows.len() < USAGE_WINDOWS_MAX && !r.windows.iter().any(|x| x.id == w.id) {
                r.windows.push(w);
            }
        }
    }
    if let Some(c) = &source.context {
        if let Some(p) = pct(at(v, &c.pct)) {
            let size = c.size.as_ref().and_then(|f| whole(at(v, f), 1, 1_000_000_000));
            r.context = Some((p, size));
        }
    }
    let parts: Vec<u64> = source
        .tokens
        .iter()
        .filter_map(|f| whole(at(v, f), 0, SESSION_TOKENS_MAX))
        .collect();
    r.tokens = (!parts.is_empty()).then(|| {
        parts
            .iter()
            .fold(0u64, |a, b| a.saturating_add(*b))
            .min(SESSION_TOKENS_MAX)
    });
    r.plan = source
        .plan
        .as_ref()
        .and_then(|f| at(v, f)?.as_str())
        .map(str::to_ascii_lowercase)
        .filter(|p| usage_plan_ok(p));
    r
}

// ── the status line (`hook --statusline`) ───────────────────────────────

/// Where the status-line command runs.
/// How long the status line waits for the runner to take a reading (it prints nothing until then).
const REPORT_WAIT: Duration = Duration::from_millis(300);

pub struct StatusEnv {
    pub plugin: String,
    pub socket: PathBuf,
    pub pane_id: Option<String>,
    /// The agent's config directory (`~/.claude`), where the person's own settings are.
    pub config_dir: Option<PathBuf>,
    pub cwd: Option<PathBuf>,
    pub timeout: Duration,
    pub now_s: u64,
}

/// The whole command: what to print (bytes, exactly) and the exit status.
pub fn run_statusline(stdin: &mut impl Read, env: &StatusEnv) -> (Vec<u8>, i32) {
    let mut input = Vec::new();
    let read_ok = stdin.take(STDIN_MAX + 1).read_to_end(&mut input).is_ok();
    let whole_input = read_ok && input.len() as u64 <= STDIN_MAX;
    input.truncate(STDIN_MAX as usize);
    // tmuxedo's part: nothing in it may stop the person's status line.
    let parsed = std::panic::catch_unwind(|| {
        let v: Value = whole_input.then(|| serde_json::from_slice(&input).ok()).flatten()?;
        let plugin = crate::plugin::Host::builtin().get(&env.plugin)?;
        let source = plugin.manifest().usage.as_ref()?.source("statusline")?.clone();
        let reading = extract(&source, &v, env.now_s);
        report(&env.socket, &env.plugin, env.pane_id.as_deref(), &reading);
        let model = source
            .model
            .as_ref()
            .and_then(|f| at(&v, f)?.as_str())
            .map(str::to_string);
        let project = at(&v, "workspace.project_dir")
            .and_then(Value::as_str)
            .map(PathBuf::from);
        Some((reading, model, project))
    })
    .ok()
    .flatten();
    let project = parsed
        .as_ref()
        .and_then(|p| p.2.clone())
        .filter(|p| p.is_absolute())
        .or_else(|| env.cwd.clone());
    match user_command(env.config_dir.as_deref(), project.as_deref()) {
        Some(cmd) => run_user(&cmd, &input, env.timeout),
        // No status line of their own: a short one, since an empty one would only hide the agent's hints.
        None => {
            let line = parsed.map(|(r, model, _)| minimal_line(&r, model.as_deref()));
            (line.unwrap_or_default().into_bytes(), 0)
        }
    }
}

/// Send a reading to the runner and wait a moment for its answer: a busy or absent runner loses
/// it. The runner checks the caller by walking its process ancestry, so this process must still be
/// alive when the request is read; one that wrote and exited was always refused as not in the pane.
fn report(socket: &Path, plugin: &str, pane_id: Option<&str>, reading: &Reading) {
    let Ok(mut s) = std::os::unix::net::UnixStream::connect(socket) else {
        return;
    };
    let _ = s.set_write_timeout(Some(Duration::from_millis(200)));
    let _ = s.set_read_timeout(Some(REPORT_WAIT));
    let req = json!({
        "id": "usage",
        "method": "usage.report",
        "params": { "plugin": plugin, "paneId": pane_id, "reading": reading },
    });
    if let Ok(mut line) = serde_json::to_vec(&req) {
        line.push(b'\n');
        if s.write_all(&line).is_ok() {
            let _ = s.read(&mut [0u8; 64]);
        }
    }
}

fn read_capped(path: &Path, max: u64) -> Option<Vec<u8>> {
    let f = std::fs::File::open(path).ok()?;
    if !f.metadata().ok()?.is_file() {
        return None;
    }
    let mut buf = Vec::new();
    f.take(max + 1).read_to_end(&mut buf).ok()?;
    (buf.len() as u64 <= max).then_some(buf)
}

/// The person's own status-line command: the first of the project's local
/// settings, the project's settings and their user settings that sets one.
/// Read only; the files are never written. tmuxedo's own command is skipped.
pub fn user_command(config_dir: Option<&Path>, project: Option<&Path>) -> Option<String> {
    let mut files = vec![];
    if let Some(p) = project {
        files.push(p.join(".claude/settings.local.json"));
        files.push(p.join(".claude/settings.json"));
    }
    if let Some(c) = config_dir {
        files.push(c.join("settings.json"));
    }
    files.iter().find_map(|f| {
        let v: Value = serde_json::from_slice(&read_capped(f, SETTINGS_MAX)?).ok()?;
        let s = v.get("statusLine")?;
        if s.get("type").and_then(Value::as_str).is_some_and(|t| t != "command") {
            return None;
        }
        let cmd = s.get("command")?.as_str()?.trim();
        (!cmd.is_empty() && cmd.len() <= 8192 && !cmd.contains("hook --statusline")).then(|| cmd.to_string())
    })
}

/// Run the person's command with the same stdin; its output byte for byte and
/// its exit status. Stopped (with everything it started) after `timeout`:
/// then nothing is printed.
fn run_user(cmd: &str, input: &[u8], timeout: Duration) -> (Vec<u8>, i32) {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    let Ok(mut child) = Command::new("/bin/sh")
        .arg("-c")
        .arg(cmd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .process_group(0)
        .spawn()
    else {
        return (vec![], 0);
    };
    if let Some(mut si) = child.stdin.take() {
        let bytes = input.to_vec();
        std::thread::spawn(move || {
            let _ = si.write_all(&bytes);
        });
    }
    let (tx, rx) = std::sync::mpsc::channel();
    if let Some(so) = child.stdout.take() {
        std::thread::spawn(move || {
            let mut out = Vec::new();
            let _ = so.take(OUTPUT_MAX).read_to_end(&mut out);
            let _ = tx.send(out);
        });
    }
    let deadline = Instant::now() + timeout;
    let out = rx.recv_timeout(timeout).ok();
    let mut code = None;
    while out.is_some() && Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(st)) => {
                code = Some(st.code().unwrap_or(1));
                break;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(_) => break,
        }
    }
    if code.is_none() {
        // SAFETY: the child is the leader of its own process group (process_group(0)); the
        // signal goes to that group only.
        unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
        let _ = child.wait();
        return (vec![], 0);
    }
    (out.unwrap_or_default(), code.unwrap_or(0))
}

fn local_hhmm(at_s: u64) -> Option<String> {
    // `time_t` inferred: the named alias is deprecated on musl.
    let t = at_s as _;
    // SAFETY: `tm` is plain data that localtime_r fills; both pointers are valid for the call.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let ok = unsafe { !libc::localtime_r(&t, &mut tm).is_null() };
    ok.then(|| format!("{:02}:{:02}", tm.tm_hour, tm.tm_min))
}

/// `Opus · ctx 34% · 5h 62% (14:10)`: what is known, nothing else.
pub fn minimal_line(r: &Reading, model: Option<&str>) -> String {
    let mut parts = vec![];
    if let Some(m) = model {
        let m: String = m.chars().filter(|c| !c.is_control()).take(32).collect();
        if !m.trim().is_empty() {
            parts.push(m.trim().to_string());
        }
    }
    if let Some((p, _)) = r.context {
        parts.push(format!("ctx {p:.0}%"));
    }
    for w in &r.windows {
        let reset = w
            .resets_at
            .filter(|_| w.window_mins.is_some_and(|m| m <= 1440))
            .and_then(local_hhmm);
        parts.push(match reset {
            Some(t) => format!("{} {:.0}% ({t})", w.id, w.used_pct),
            None => format!("{} {:.0}%", w.id, w.used_pct),
        });
    }
    parts.join(" · ")
}

// ── the session log (`jsonl_tail`) ───────────────────────────────────────

/// The newest line of the log at `path` that `source` selects, as numbers.
/// `path` must be a `.jsonl` file under `root` (after links are resolved).
/// Reads at most [`TAIL_MAX`] from the end; a line over [`LINE_MAX`], not
/// JSON or not selected is skipped. None when nothing usable is there.
pub fn read_tail(source: &UsageSource, path: &Path, root: &Path, now_s: u64) -> Option<Reading> {
    use std::io::{Seek, SeekFrom};
    let (path, root) = (path.canonicalize().ok()?, root.canonicalize().ok()?);
    if !path.starts_with(&root) || path.extension().is_none_or(|e| e != "jsonl") {
        return None;
    }
    let mut f = std::fs::File::open(&path).ok()?;
    let meta = f.metadata().ok()?;
    if !meta.is_file() {
        return None;
    }
    let start = meta.len().saturating_sub(TAIL_MAX);
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    f.take(TAIL_MAX).read_to_end(&mut buf).ok()?;
    let mut lines: Vec<&[u8]> = buf.split(|b| *b == b'\n').collect();
    if start > 0 {
        lines.remove(0); // the rest of a line that began before the tail
    }
    lines.iter().rev().find_map(|line| {
        if line.is_empty() || line.len() > LINE_MAX {
            return None;
        }
        let v: Value = serde_json::from_slice(line).ok()?;
        let selected = source
            .select
            .iter()
            .all(|(k, want)| at(&v, k).and_then(Value::as_str) == Some(want));
        if !selected {
            return None;
        }
        let r = extract(source, &v, now_s);
        (!r.windows.is_empty()).then_some(r)
    })
}

// ── what the runner keeps ────────────────────────────────────────────────

struct Entry {
    record: UsageRecord,
    dirty: bool,
    sent: Option<Instant>,
}

/// Sessions whose use is remembered (the oldest is let go), and steps kept per session.
pub const SPENT_PANES_MAX: usize = 64;
pub const SPENT_EVENTS_MAX: usize = 256;

/// One step in what a session used: a rise in a window, or tokens.
struct Step {
    at_ms: u64,
    window: Option<(String, Option<u32>, f64)>,
    tokens: u64,
}

/// What one pane's agent used, step by step (the away digest asks "since when").
#[derive(Default)]
struct Spent {
    plugin: String,
    last_ms: u64,
    tokens_now: Option<u64>,
    steps: std::collections::VecDeque<Step>,
}

impl Spent {
    fn push(&mut self, s: Step) {
        if self.steps.len() >= SPENT_EVENTS_MAX {
            self.steps.pop_front();
        }
        self.steps.push_back(s);
    }
}

/// Credit `pane` with what this reading shows it used: the rise of each
/// window since the newest reading from any pane (a window whose reset
/// passed starts again from 0; a first reading on this machine is only
/// where the account stood), and the change in its running token total
/// (a smaller total is a new agent session, counted whole).
fn credit(
    spent: &mut BTreeMap<String, Spent>,
    plugin: &str,
    pane: &str,
    old: &[UsageWindow],
    new: &[UsageWindow],
    tokens: Option<u64>,
    now_ms: u64,
) {
    let now_s = now_ms / 1000;
    if !spent.contains_key(pane) && spent.len() >= SPENT_PANES_MAX {
        let oldest = spent.iter().min_by_key(|(_, s)| s.last_ms).map(|(k, _)| k.clone());
        if let Some(k) = oldest {
            spent.remove(&k);
        }
    }
    let s = spent.entry(pane.to_string()).or_default();
    if s.plugin != plugin {
        *s = Spent {
            plugin: plugin.to_string(),
            ..Default::default()
        };
    }
    s.last_ms = now_ms;
    for w in new {
        let Some(o) = old.iter().find(|o| o.id == w.id) else {
            continue;
        };
        let rise = if o.resets_at.is_some_and(|t| t <= now_s) {
            w.used_pct
        } else {
            w.used_pct - o.used_pct
        };
        if rise > 0.0 {
            s.push(Step {
                at_ms: now_ms,
                window: Some((w.id.clone(), w.window_mins, rise)),
                tokens: 0,
            });
        }
    }
    if let Some(t) = tokens {
        let used = match s.tokens_now {
            Some(prev) if t >= prev => t - prev,
            _ => t,
        };
        s.tokens_now = Some(t);
        if used > 0 {
            s.push(Step {
                at_ms: now_ms,
                window: None,
                tokens: used,
            });
        }
    }
}

/// A window with no reset time is trusted from disk for this long.
const NO_RESET_KEEP_MS: u64 = 5 * 3600 * 1000;

/// The records saved by an earlier run, with every window whose reset has
/// passed left out (a record with none left is dropped). Each is sent again.
fn load_latest(path: &Path, now_ms: u64) -> BTreeMap<String, Entry> {
    let now_s = now_ms / 1000;
    let saved: Vec<UsageRecord> = std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let mut out = BTreeMap::new();
    for mut r in saved {
        let fresh = now_ms.saturating_sub(r.observed_at) < NO_RESET_KEEP_MS;
        r.windows.retain(|w| w.resets_at.map_or(fresh, |t| t > now_s));
        r.reached = r.windows.iter().find(|w| w.used_pct >= 100.0).map(|w| w.id.clone());
        r.context = None;
        if r.windows.is_empty() || !r.ok() || out.len() >= ACCOUNTS_MAX || out.contains_key(&r.plugin) {
            continue;
        }
        out.insert(
            r.plugin.clone(),
            Entry {
                record: r,
                dirty: true,
                sent: None,
            },
        );
    }
    out
}

/// The newest record per plugin, and when each may be sent again.
pub struct Store {
    salt_file: PathBuf,
    /// The newest records, kept on disk so a restart still knows how full
    /// each plan is (the gauge, and the restart-time lazy resume).
    latest_file: PathBuf,
    salt: Option<String>,
    entries: BTreeMap<String, Entry>,
    tailed: BTreeMap<String, Instant>,
    /// Per pane: what its session used (in memory only; a restart starts afresh).
    spent: BTreeMap<String, Spent>,
}

impl Store {
    pub fn new(state_dir: &Path) -> Self {
        let latest_file = state_dir.join("usage-latest.json");
        let entries = load_latest(&latest_file, crate::util::now_ms());
        Self {
            salt_file: state_dir.join("usage-salt"),
            latest_file,
            salt: None,
            entries,
            tailed: BTreeMap::new(),
            spent: BTreeMap::new(),
        }
    }

    fn save(&self) {
        if let Ok(b) = serde_json::to_vec(&self.records()) {
            let _ = crate::config::write_private(&self.latest_file, &b);
        }
    }

    /// This machine's random salt (made once, mode 0600): an account key
    /// tells nothing about the account or the machine.
    fn salt(&mut self) -> String {
        if let Some(s) = &self.salt {
            return s.clone();
        }
        let hex_ok = |s: &str| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit());
        let s = std::fs::read_to_string(&self.salt_file)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| hex_ok(s))
            .unwrap_or_else(|| {
                use std::os::unix::fs::OpenOptionsExt;
                let mut raw = [0u8; 32];
                let _ = getrandom::getrandom(&mut raw);
                let s: String = raw.iter().map(|b| format!("{b:02x}")).collect();
                let f = std::fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .mode(0o600)
                    .open(&self.salt_file);
                if let Ok(mut f) = f {
                    let _ = f.write_all(s.as_bytes());
                }
                s
            });
        self.salt = Some(s.clone());
        s
    }

    /// The account key for `plugin` on this machine (`m:` + 32 hex digits).
    pub fn account_key(&mut self, plugin: &str) -> String {
        let mut h = Sha256::new();
        h.update(b"tmuxedo-usage\0");
        h.update(self.salt().as_bytes());
        h.update(b"\0");
        h.update(plugin.as_bytes());
        let hex: String = h.finalize().iter().take(16).map(|b| format!("{b:02x}")).collect();
        format!("m:{hex}")
    }

    /// May the session log be read for `plugin` now (at most once per interval)?
    pub fn tail_due(&mut self, plugin: &str, now: Instant) -> bool {
        let every = Duration::from_millis(USAGE_MIN_INTERVAL_MS);
        if self.tailed.get(plugin).is_some_and(|t| now.duration_since(*t) < every) {
            return false;
        }
        if !self.tailed.contains_key(plugin) && self.tailed.len() >= ACCOUNTS_MAX {
            return false;
        }
        self.tailed.insert(plugin.to_string(), now);
        true
    }

    /// Take a reading in. Checked again here (the local socket is not trusted
    /// to send sound numbers). True when the record changed.
    pub fn note(&mut self, section: &UsageSection, plugin: &str, pane: Option<&str>, r: Reading, now_ms: u64) -> bool {
        if section.source_by_wire(&r.source).is_none() {
            return false;
        }
        if !self.entries.contains_key(plugin) && self.entries.len() >= ACCOUNTS_MAX {
            return false;
        }
        let now_s = now_ms / 1000;
        let mut windows: Vec<UsageWindow> = vec![];
        for w in r.windows.into_iter().take(USAGE_WINDOWS_MAX) {
            if w.ok() && w.resets_at.is_none_or(|t| t > now_s) && !windows.iter().any(|x| x.id == w.id) {
                windows.push(w);
            }
        }
        let account_key = self.account_key(plugin);
        let old = self.entries.get(plugin).map(|e| &e.record);
        // No windows in this reading (before a session's first reply): the last known ones stay
        // until their reset passes.
        if windows.is_empty() {
            if let Some(o) = old {
                windows = o
                    .windows
                    .iter()
                    .filter(|w| w.resets_at.is_some_and(|t| t > now_s))
                    .cloned()
                    .collect();
            }
        }
        let context = pane.zip(r.context).map(|(p, (used_pct, size))| UsageContext {
            pane_id: p.to_string(),
            used_pct,
            size,
        });
        let record = UsageRecord {
            plugin: plugin.to_string(),
            account_key,
            mode: "plan".into(),
            plan: r
                .plan
                .filter(|p| usage_plan_ok(p))
                .or_else(|| old.and_then(|o| o.plan.clone())),
            reached: windows.iter().find(|w| w.used_pct >= 100.0).map(|w| w.id.clone()),
            windows,
            context,
            source: r.source,
            observed_at: now_ms,
        };
        if !record.ok() {
            return false;
        }
        if let Some(p) = pane {
            let before = old.map_or(&[][..], |o| o.windows.as_slice());
            credit(&mut self.spent, plugin, p, before, &record.windows, r.tokens, now_ms);
        }
        let same = old.is_some_and(|o| {
            let mut o = o.clone();
            o.observed_at = now_ms;
            o == record
        });
        match self.entries.get_mut(plugin) {
            Some(e) => {
                e.record = record;
                e.dirty |= !same;
            }
            None => {
                self.entries.insert(
                    plugin.to_string(),
                    Entry {
                        record,
                        dirty: true,
                        sent: None,
                    },
                );
            }
        }
        if !same {
            self.save();
        }
        !same
    }

    /// What the session in `pane` used at or after `since_ms`; None when nothing is known.
    pub fn spent(&self, pane: &str, since_ms: u64) -> Option<SessionUsage> {
        let s = self.spent.get(pane)?;
        let mut windows: Vec<SpentWindow> = vec![];
        let mut tokens = 0u64;
        for st in s.steps.iter().filter(|st| st.at_ms >= since_ms) {
            tokens = tokens.saturating_add(st.tokens);
            if let Some((id, mins, rise)) = &st.window {
                if let Some(w) = windows.iter_mut().find(|w| &w.id == id) {
                    w.spent_pct += rise;
                    w.window_mins = mins.or(w.window_mins);
                } else if windows.len() < USAGE_WINDOWS_MAX {
                    windows.push(SpentWindow {
                        id: id.clone(),
                        spent_pct: *rise,
                        window_mins: *mins,
                    });
                }
            }
        }
        for w in &mut windows {
            w.spent_pct = (w.spent_pct * 10.0).round() / 10.0;
        }
        windows.retain(|w| w.spent_pct > 0.0);
        let u = SessionUsage {
            plugin: s.plugin.clone(),
            windows,
            tokens: (tokens > 0).then_some(tokens.min(SESSION_TOKENS_MAX)),
        };
        u.ok().then_some(u)
    }

    /// The records to send now: changed, and none sent for that account within the interval.
    pub fn due(&mut self, now: Instant) -> Vec<UsageRecord> {
        let every = Duration::from_millis(USAGE_MIN_INTERVAL_MS);
        let mut out = vec![];
        for e in self.entries.values_mut() {
            if e.dirty && e.sent.is_none_or(|t| now.duration_since(t) >= every) {
                e.dirty = false;
                e.sent = Some(now);
                out.push(e.record.clone());
            }
        }
        out
    }

    /// The newest record per account (usage alerts read these).
    pub fn records(&self) -> Vec<UsageRecord> {
        self.entries.values().map(|e| e.record.clone()).collect()
    }

    /// A new gateway connection knows nothing: every record goes again.
    pub fn resend(&mut self) {
        for e in self.entries.values_mut() {
            e.dirty = true;
            e.sent = None;
        }
    }

    /// Drop what was read for a plugin (its AI usage was turned off).
    pub fn forget(&mut self, plugin: &str) {
        self.tailed.remove(plugin);
        self.spent.retain(|_, s| s.plugin != plugin);
        if self.entries.remove(plugin).is_some() {
            self.save();
        }
    }

    pub fn has_dirty(&self) -> bool {
        self.entries.values().any(|e| e.dirty)
    }
}

/// The local and gateway methods (crate::api): the newest readings, for the TUI widget.
pub const METHODS: &[&str] = &["usage.list"];

pub mod alerts;
mod alerts_daemon;
pub mod lazy;

#[cfg(test)]
mod tests;
