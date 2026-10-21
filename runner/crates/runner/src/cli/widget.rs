//! `tmuxedo widget usage|machine|agents|needs` (docs/design/widgets.md): a small
//! full-screen text dashboard for a pane. It reads this machine only: agents and
//! needs-you items from the runner's local socket (read-only methods), machine
//! numbers from the same /proc and cgroup files the runner's sampler reads. Never
//! the network. Six size tiers; every value is shown whole or not at all.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;
use std::time::{Duration, Instant};

/// The widget kinds, in picker order.
pub const KINDS: &[&str] = &["usage", "machine", "agents", "needs"];

/// One line per kind for `tmuxedo widget` with no name.
pub const LIST: &str = "\
Widgets (run one in a pane: tmuxedo widget <name>):
  1  usage    Plan limits for your AI accounts
  2  machine  CPU, memory and disk of this machine
  3  agents   Every agent on this machine and what it is doing
  4  needs    What is waiting for you
";

/// After this long with no key and no resize, a widget stops reading until a key is pressed
/// (outside tmuxedo it cannot tell whether anyone is looking; `--keep-alive` turns this off).
pub const IDLE_STOP: Duration = Duration::from_secs(30 * 60);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier {
    Micro,
    Strip,
    Tiny,
    Small,
    Medium,
    Large,
}

impl Tier {
    pub fn name(self) -> &'static str {
        match self {
            Tier::Micro => "micro",
            Tier::Strip => "strip",
            Tier::Tiny => "tiny",
            Tier::Small => "small",
            Tier::Medium => "medium",
            Tier::Large => "large",
        }
    }

    /// How often the data is read at this tier (design §9: strip/tiny 0.2 Hz, small and up 0.5 Hz).
    pub fn interval(self) -> Duration {
        match self {
            Tier::Micro | Tier::Strip | Tier::Tiny => Duration::from_secs(5),
            _ => Duration::from_secs(2),
        }
    }
}

/// The largest tier whose minimum fits both dimensions; 1–2 rows is always a strip (when 12 wide).
pub fn tier(cols: u16, rows: u16) -> Tier {
    if rows <= 2 {
        return if cols >= 12 { Tier::Strip } else { Tier::Micro };
    }
    if cols >= 120 && rows >= 36 {
        Tier::Large
    } else if cols >= 72 && rows >= 18 {
        Tier::Medium
    } else if cols >= 40 && rows >= 8 {
        Tier::Small
    } else if cols >= 20 {
        Tier::Tiny
    } else {
        Tier::Micro
    }
}

/// Tall and narrow (phone portrait, 44 × 30): items stack instead of sitting side by side.
pub fn portrait(cols: u16, rows: u16) -> bool {
    cols < 48 && u32::from(rows) * 3 >= u32::from(cols) * 2
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Glyphs {
    Blocks,
    Ascii,
}

impl Glyphs {
    /// `--glyphs`, then `TMUXEDO_WIDGET_GLYPHS`, then ASCII unless the locale is UTF-8.
    pub fn choose(flag: Option<&str>, env: &dyn Fn(&str) -> Option<String>) -> Glyphs {
        let pick = flag.map(str::to_owned).or_else(|| env("TMUXEDO_WIDGET_GLYPHS"));
        match pick.as_deref() {
            Some("ascii") => return Glyphs::Ascii,
            Some("blocks") | Some("braille") => return Glyphs::Blocks,
            _ => {}
        }
        let loc = env("LC_ALL")
            .filter(|v| !v.is_empty())
            .or_else(|| env("LC_CTYPE").filter(|v| !v.is_empty()))
            .or_else(|| env("LANG"))
            .unwrap_or_default()
            .to_ascii_lowercase();
        if loc.contains("utf-8") || loc.contains("utf8") {
            Glyphs::Blocks
        } else {
            Glyphs::Ascii
        }
    }
}

/// Colour unless `NO_COLOR` is set (any value) or the terminal is dumb.
pub fn color_on(env: &dyn Fn(&str) -> Option<String>) -> bool {
    env("NO_COLOR").is_none() && env("TERM").is_none_or(|t| t != "dumb")
}

#[derive(Clone, Copy, Debug)]
pub struct Opts {
    pub glyphs: Glyphs,
    pub color: bool,
    /// Reading stopped (idle); the footer says how to wake it.
    pub paused: bool,
}

// ---- data ------------------------------------------------------------------------------------

#[derive(Clone, Debug, Default, PartialEq)]
pub struct MachineData {
    pub cpu: Option<f64>,
    pub cores: Option<u32>,
    pub quota: Option<f64>,
    /// used, limit, limited by a container
    pub mem: Option<(u64, u64, bool)>,
    /// free, total
    pub disk: Option<(u64, u64)>,
    pub cpu_hist: Vec<f64>,
    pub mem_hist: Vec<f64>,
}

const HIST: usize = 300;

impl MachineData {
    /// Take one sampler reading (crate::stats) and keep its percentages for the sparklines.
    pub fn push(&mut self, s: &tmuxedo_protocol::stats::StatsSample) {
        self.cpu = s.cpu.as_ref().and_then(|c| c.pct).filter(|p| p.is_finite());
        self.cores = s.cpu.as_ref().and_then(|c| c.cores);
        self.quota = s.cpu.as_ref().and_then(|c| c.quota).filter(|q| q.is_finite());
        self.mem = s
            .mem
            .as_ref()
            .filter(|m| m.limit > 0)
            .map(|m| (m.used, m.limit, m.limited));
        self.disk = s
            .disks
            .first()
            .filter(|d| d.total > 0)
            .map(|d| (d.free.min(d.total), d.total));
        if let Some(c) = self.cpu {
            self.cpu_hist.push(c.clamp(0.0, 100.0));
        }
        if let Some(p) = self.mem_pct() {
            self.mem_hist.push(p);
        }
        for h in [&mut self.cpu_hist, &mut self.mem_hist] {
            if h.len() > HIST {
                h.drain(..h.len() - HIST);
            }
        }
    }

    fn mem_pct(&self) -> Option<f64> {
        self.mem
            .map(|(u, l, _)| (u as f64 * 100.0 / l as f64).clamp(0.0, 100.0))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct AgentRow {
    pub pane: String,
    pub name: String,
    pub kind: String,
    pub status: String,
    pub place: String,
}

/// `agent.list`'s result → rows, the ones that need you first.
pub fn agents_from(result: &Value) -> Vec<AgentRow> {
    let mut rows: Vec<AgentRow> = result["agents"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .map(|a| {
            let st = |k: &str| clean(a[k].as_str().unwrap_or("")).trim().to_string();
            let pane = st("pane_id");
            let name = [st("name"), st("label"), pane.clone()]
                .into_iter()
                .find(|s| !s.is_empty())
                .unwrap_or_default();
            let cwd = st("cwd");
            let place = cwd.rsplit('/').find(|s| !s.is_empty()).unwrap_or(&cwd).to_string();
            AgentRow {
                pane,
                name,
                kind: st("agent"),
                status: st("agent_status"),
                place,
            }
        })
        .collect();
    rows.sort_by_key(|r| status_rank(&r.status));
    rows
}

fn status_rank(s: &str) -> u8 {
    match s {
        "blocked" => 0,
        "working" => 1,
        "done" => 2,
        "idle" => 3,
        _ => 4,
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct NeedRow {
    pub pane: String,
    pub plugin: String,
    pub title: String,
    pub command: Option<String>,
    pub high: bool,
    pub age_ms: Option<u64>,
}

/// `needs.list`'s result → rows, oldest first.
pub fn needs_from(result: &Value, now_ms: u64) -> Vec<NeedRow> {
    let mut rows: Vec<(u64, NeedRow)> = result["items"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .map(|n| {
            let st = |k: &str| clean(n[k].as_str().unwrap_or("")).trim().to_string();
            let at = n["created_at"].as_str().and_then(iso_ms);
            let command = n["detail"]["input"]["command"]
                .as_str()
                .map(clean)
                .filter(|c| !c.trim().is_empty());
            let title = st("title");
            let row = NeedRow {
                pane: st("pane_id"),
                plugin: st("plugin"),
                command: command.filter(|c| !title.contains(c.as_str())),
                title,
                high: n["risk"].as_str() == Some("high"),
                age_ms: at.map(|t| now_ms.saturating_sub(t)),
            };
            (at.unwrap_or(u64::MAX), row)
        })
        .collect();
    rows.sort_by_key(|(t, _)| *t);
    rows.into_iter().map(|(_, r)| r).collect()
}

/// Control characters out (names and titles come from agents).
fn clean(s: &str) -> String {
    s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect()
}

/// `2026-10-13T09:00:00.000Z` → ms since the epoch.
pub fn iso_ms(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[10] != b'T' || !s.ends_with('Z') {
        return None;
    }
    let n = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d, h, mi, se) = (n(0..4)?, n(5..7)?, n(8..10)?, n(11..13)?, n(14..16)?, n(17..19)?);
    let ms = if b[19] == b'.' { n(20..23).unwrap_or(0) } else { 0 };
    // Days from the civil date (the usual era arithmetic).
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * (if mo > 2 { mo - 3 } else { mo + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = u64::try_from(((days * 24 + h) * 60 + mi) * 60 + se).ok()?;
    Some(secs * 1000 + u64::try_from(ms).unwrap_or(0))
}

/// One plan window of one AI account (`usage.list`).
#[derive(Clone, Debug, PartialEq)]
pub struct UsageRow {
    pub name: String,
    /// "5-hour", "weekly".
    pub label: String,
    /// "5h", "7d".
    pub short: String,
    pub used: f64,
    pub resets_in_ms: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct UsageData {
    /// AI usage is on for some agent (Settings → AI usage).
    pub on: bool,
    pub rows: Vec<UsageRow>,
}

/// `usage.list`'s result → rows, in the runner's order.
pub fn usage_from(result: &Value, now_ms: u64) -> UsageData {
    let mut rows = vec![];
    for r in result["records"].as_array().map(Vec::as_slice).unwrap_or_default() {
        let name = clean(r["name"].as_str().or(r["plugin"].as_str()).unwrap_or(""))
            .trim()
            .to_string();
        for x in r["windows"].as_array().map(Vec::as_slice).unwrap_or_default() {
            let Some(used) = x["usedPct"].as_f64() else { continue };
            let short = clean(x["id"].as_str().unwrap_or("")).trim().to_string();
            rows.push(UsageRow {
                name: name.clone(),
                label: clean(x["label"].as_str().unwrap_or(&short)).trim().to_string(),
                short,
                used,
                resets_in_ms: x["resetsAt"].as_u64().map(|t| (t * 1000).saturating_sub(now_ms)),
            });
        }
    }
    UsageData {
        on: result["enabled"].as_bool().unwrap_or(false),
        rows,
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Data {
    /// AI usage (plan limits).
    Usage(UsageData),
    Machine(MachineData),
    Agents(Vec<AgentRow>),
    Needs(Vec<NeedRow>),
    /// The local socket did not answer.
    NoRunner,
}

// ---- drawing helpers -------------------------------------------------------------------------

type L = Line<'static>;

fn w(s: &str) -> usize {
    Line::from(s).width()
}

/// The widest candidate that fits, or none (an item is dropped whole, never cut).
fn fit<S: AsRef<str>>(cands: &[S], width: usize) -> Option<String> {
    cands
        .iter()
        .map(AsRef::as_ref)
        .find(|c| w(c) <= width)
        .map(str::to_owned)
}

fn plain(s: impl Into<String>) -> L {
    Line::from(s.into())
}

/// `left … right`; the right part is dropped first, then the left falls back to its short forms.
fn two(left: &[&str], right: &str, cols: usize) -> String {
    if !right.trim().is_empty() {
        for l in left {
            if w(l) + w(right) < cols {
                return format!("{l}{}{right}", " ".repeat(cols - w(l) - w(right)));
            }
        }
    }
    fit(left, cols).unwrap_or_default()
}

/// Items in priority order, each at its widest form that still fits; what does not fit is
/// dropped whole and counted in a `+n` marker.
fn strip(items: &[Vec<String>], cols: usize) -> String {
    const SEP: &str = "  ";
    let mut placed: Vec<String> = vec![];
    let mut dropped = 0usize;
    for it in items {
        let used = placed.iter().map(|p| w(p) + w(SEP)).sum::<usize>();
        match fit(it, cols.saturating_sub(used)) {
            Some(s) if dropped == 0 => placed.push(s),
            _ => dropped += 1,
        }
    }
    loop {
        let body = placed.join(SEP);
        if dropped == 0 {
            return body;
        }
        let mark = format!("+{dropped}");
        let full = if body.is_empty() {
            mark.clone()
        } else {
            format!("{body} {mark}")
        };
        if w(&full) <= cols {
            return full;
        }
        if placed.pop().is_none() {
            return fit(&[mark], cols).unwrap_or_default();
        }
        dropped += 1;
    }
}

/// Greedy word wrap; a word wider than the line is split (the whole value still shows).
fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = vec![];
    let mut cur = String::new();
    for word in text.split_whitespace() {
        let mut word = word.to_string();
        while w(&word) > width {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            let mut head = String::new();
            let mut rest = String::new();
            for c in word.chars() {
                if rest.is_empty() && w(&format!("{head}{c}")) <= width {
                    head.push(c);
                } else {
                    rest.push(c);
                }
            }
            if head.is_empty() {
                // One character wider than the line: nothing can show it.
                word = String::new();
                break;
            }
            out.push(head);
            word = rest;
        }
        if word.is_empty() {
            continue;
        }
        if cur.is_empty() {
            cur = word;
        } else if w(&cur) + 1 + w(&word) <= width {
            cur = format!("{cur} {word}");
        } else {
            out.push(std::mem::replace(&mut cur, word));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// At most `max` wrapped lines; when some are left out the last line says so with "…".
fn clipped(text: &str, width: usize, max: usize) -> Vec<String> {
    let mut v = wrap(text, width);
    if v.len() > max {
        v.truncate(max);
        if let Some(last) = v.last_mut() {
            if w(last) + 2 <= width {
                last.push_str(" …");
            }
        }
    }
    v
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Level {
    Calm,
    Busy,
    High,
}

impl Level {
    fn word(self) -> &'static str {
        match self {
            Level::Calm => "calm",
            Level::Busy => "busy",
            Level::High => "high",
        }
    }
    fn color(self) -> Color {
        match self {
            Level::Calm => Color::Green,
            Level::Busy => Color::Yellow,
            Level::High => Color::Red,
        }
    }
    fn glyph(self, g: Glyphs) -> &'static str {
        match (g, self) {
            (Glyphs::Blocks, Level::Calm) => "○",
            (Glyphs::Blocks, Level::Busy) => "◐",
            (Glyphs::Blocks, Level::High) => "●",
            (Glyphs::Ascii, Level::Calm) => ".",
            (Glyphs::Ascii, Level::Busy) => "o",
            (Glyphs::Ascii, Level::High) => "!",
        }
    }
}

/// Memory is "high" where the runner's watchdog acts (90 % of the limit).
fn level(pct: f64, busy: f64, high: f64) -> Level {
    if pct >= high {
        Level::High
    } else if pct >= busy {
        Level::Busy
    } else {
        Level::Calm
    }
}

fn bar(frac: f64, width: usize, g: Glyphs) -> String {
    let frac = if frac.is_finite() { frac.clamp(0.0, 1.0) } else { 0.0 };
    match g {
        Glyphs::Blocks => {
            let on = (frac * width as f64).round() as usize;
            format!("{}{}", "█".repeat(on), "░".repeat(width - on))
        }
        Glyphs::Ascii if width < 3 => "#".repeat(width),
        Glyphs::Ascii => {
            let inner = width - 2;
            let on = (frac * inner as f64).round() as usize;
            format!("[{}{}]", "#".repeat(on), "-".repeat(inner - on))
        }
    }
}

/// The last `width` points as a sparkline (0–100).
fn spark(vals: &[f64], width: usize, g: Glyphs) -> String {
    let set: &[&str] = match g {
        Glyphs::Blocks => &["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"],
        Glyphs::Ascii => &[".", ":", "-", "=", "+", "*", "#", "@"],
    };
    let tail = &vals[vals.len().saturating_sub(width)..];
    tail.iter()
        .map(|v| set[((v.clamp(0.0, 100.0) / 100.0) * 7.0).round() as usize])
        .collect()
}

/// A taller chart: `height` rows of columns (top row first).
fn chart(vals: &[f64], width: usize, height: usize, g: Glyphs) -> Vec<String> {
    let tail = &vals[vals.len().saturating_sub(width)..];
    let full = match g {
        Glyphs::Blocks => "█",
        Glyphs::Ascii => "#",
    };
    (0..height)
        .map(|row| {
            let floor = (height - 1 - row) as f64 / height as f64 * 100.0;
            tail.iter().map(|v| if *v > floor { full } else { " " }).collect()
        })
        .collect()
}

fn gib(b: u64) -> String {
    let g = b as f64 / (1u64 << 30) as f64;
    if g >= 100.0 {
        format!("{g:.0}")
    } else {
        format!("{g:.1}")
    }
}

fn pct(v: f64) -> String {
    format!("{:.0}%", v.clamp(0.0, 100.0))
}

fn ago(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        0..=59 => "now".into(),
        60..=3599 => format!("{} m ago", s / 60),
        3600..=86_399 => format!("{} h ago", s / 3600),
        _ => format!("{} d ago", s / 86_400),
    }
}

fn rule(cols: usize, g: Glyphs) -> String {
    let c = if g == Glyphs::Blocks { "─" } else { "-" };
    format!(" {}", c.repeat(cols.saturating_sub(2)))
}

fn footer(cols: usize, o: &Opts, more: &[&str]) -> String {
    let q = if o.paused {
        "paused · any key wakes · q quit"
    } else {
        "q quit"
    };
    let mut cands: Vec<String> = more.iter().map(|m| format!("{m} · {q}")).collect();
    cands.push(q.into());
    cands.push("q quit".into());
    let s = fit(&cands, cols.saturating_sub(1)).unwrap_or_default();
    format!("{}{s}", " ".repeat(cols.saturating_sub(w(&s) + 1)))
}

/// Header, rule, body (cut to the rows left), blank lines, footer.
fn framed(title: &str, right: &str, body: Vec<L>, cols: usize, rows: usize, o: &Opts, more: &[&str]) -> Vec<L> {
    let head = format!(" {title}");
    let mut out = vec![
        Line::from(two(&[&head], &format!("{right} "), cols)).style(Style::default().add_modifier(Modifier::BOLD)),
        plain(rule(cols, o.glyphs)),
    ];
    out.extend(body.into_iter().take(rows.saturating_sub(3)));
    while out.len() + 1 < rows {
        out.push(plain(""));
    }
    out.push(plain(footer(cols, o, more)));
    out
}

fn tone(o: &Opts, c: Color) -> Style {
    if o.color {
        Style::default().fg(c)
    } else {
        Style::default()
    }
}

/// Lines stacked one per item, each at its widest form that fits; never blank.
fn stacked(items: &[Vec<String>], last: &str, cols: usize, rows: usize) -> Vec<L> {
    let mut out: Vec<L> = items
        .iter()
        .filter_map(|c| fit(c, cols))
        .take(rows)
        .map(plain)
        .collect();
    if out.is_empty() {
        out.push(plain(fit(&[last], cols).unwrap_or_default()));
    }
    out
}

// ---- the widgets -----------------------------------------------------------------------------

/// Draw `data` into `area` of `buf` at the tier its size picks.
pub fn render(data: &Data, area: Rect, buf: &mut Buffer, o: &Opts) {
    let (cols, rows) = (area.width, area.height);
    if cols == 0 || rows == 0 {
        return;
    }
    for (i, l) in lines(data, cols, rows, o).iter().take(rows as usize).enumerate() {
        buf.set_line(area.x, area.y + i as u16, l, cols);
    }
}

/// The frame as lines (each already fits `cols`).
pub fn lines(data: &Data, cols: u16, rows: u16, o: &Opts) -> Vec<L> {
    let t = tier(cols, rows);
    let port = portrait(cols, rows);
    let (c, r) = (cols as usize, rows as usize);
    match data {
        Data::Usage(u) if !u.on => note(
            &[
                "AI usage is off · turn it on in tmuxedo settings",
                "AI usage is off",
                "usage off",
                "off",
                "-",
            ],
            "Plan limits",
            t,
            c,
            r,
            o,
        ),
        Data::Usage(u) if u.rows.is_empty() => note(
            &[
                "No reading yet · it shows after an agent's next reply",
                "No reading yet",
                "no reading",
                "-",
            ],
            "Plan limits",
            t,
            c,
            r,
            o,
        ),
        Data::Usage(u) => usage(&u.rows, t, c, r, o),
        Data::NoRunner => note(
            &[
                "tmuxedo-runner is not running on this machine",
                "tmuxedo-runner is not running",
                "runner not running",
                "no runner",
                "off",
                "x",
            ],
            "tmuxedo",
            t,
            c,
            r,
            o,
        ),
        Data::Machine(m) => machine(m, t, port, c, r, o),
        Data::Agents(a) => agents(a, t, c, r, o),
        Data::Needs(n) => needs(n, t, c, r, o),
    }
}

/// "2 h 15 m", "40 m", "3 d".
fn until(ms: u64) -> String {
    let m = ms / 60_000;
    match m {
        0 => "now".into(),
        1..=59 => format!("{m} m"),
        60..=1439 if m.is_multiple_of(60) => format!("{} h", m / 60),
        60..=1439 => format!("{} h {} m", m / 60, m % 60),
        _ if m % 1440 < 60 => format!("{} d", m / 1440),
        _ => format!("{} d {} h", m / 1440, m % 1440 / 60),
    }
}

type Detail<'a> = &'a dyn Fn(&UsageRow) -> Option<String>;

/// Plan limits: one gauge per window (green, then yellow at 80 %, red at 95 %).
fn usage(rows: &[UsageRow], t: Tier, c: usize, r: usize, o: &Opts) -> Vec<L> {
    let g = o.glyphs;
    let lv = |u: &UsageRow| level(u.used, 80.0, 95.0);
    let styled = |s: String, l: Level| Line::from(Span::styled(s, tone(o, l.color())));
    let mut fullest: Vec<&UsageRow> = rows.iter().collect();
    fullest.sort_by(|a, b| b.used.total_cmp(&a.used));
    let worst = fullest.first().map_or(Level::Calm, |u| lv(u));
    match t {
        Tier::Micro | Tier::Strip => {
            let items: Vec<Vec<String>> = fullest
                .iter()
                .map(|u| {
                    vec![
                        format!("{} {} {}", u.name, u.label, pct(u.used)),
                        format!("{} {}", u.short, pct(u.used)),
                        pct(u.used),
                    ]
                })
                .collect();
            if t == Tier::Micro {
                let mut v = stacked(&items, worst.glyph(g), c, r);
                if let Some(l) = v.first_mut() {
                    *l = l.clone().style(tone(o, worst.color()));
                }
                return v;
            }
            vec![styled(strip(&items, c).trim_end().to_string(), worst)]
        }
        _ => {
            let lw = rows
                .iter()
                .map(|u| w(&format!("{} {}", u.name, u.label)))
                .max()
                .unwrap_or(0)
                .min(c / 2)
                + 1;
            // Every row has the same columns: label, percent, a bar of one width, the reset.
            let room = c.saturating_sub(lw + 8);
            let long = |u: &UsageRow| u.resets_in_ms.map(|ms| format!("resets in {}", until(ms)));
            let short = |u: &UsageRow| u.resets_in_ms.map(until);
            let widest = |f: Detail| rows.iter().filter_map(f).map(|d| w(&d)).max();
            let detail: Option<(Detail, usize)> = [&long as Detail, &short as Detail]
                .into_iter()
                .find_map(|f| widest(f).filter(|d| room >= d + 8).map(|d| (f, d)));
            let n = detail.map_or(room, |(_, d)| room - d - 2).min(30);
            let line = |u: &UsageRow| -> String {
                let label = fit(
                    &[
                        format!("{} {}", u.name, u.label),
                        format!("{} {}", u.name, u.short),
                        u.short.clone(),
                    ],
                    lw,
                )
                .unwrap_or_default();
                let d = detail
                    .and_then(|(f, _)| f(u))
                    .map(|d| format!("  {d}"))
                    .unwrap_or_default();
                format!(" {label:<lw$}{:>4}  {}{d}", pct(u.used), bar(u.used / 100.0, n, g))
                    .trim_end()
                    .to_string()
            };
            let body: Vec<L> = rows.iter().map(|u| styled(line(u), lv(u))).collect();
            if t == Tier::Tiny {
                let mut v: Vec<L> = fullest.iter().map(|u| styled(line(u), lv(u))).collect();
                v.truncate(r);
                return v;
            }
            framed("Plan limits", "", body, c, r, o, &[])
        }
    }
}

fn note(cands: &[&str], title: &str, t: Tier, c: usize, r: usize, o: &Opts) -> Vec<L> {
    if t <= Tier::Tiny {
        let wrapped = wrap(cands[0], c);
        if wrapped.len() <= r && r > 1 && wrapped.iter().all(|l| l.split_whitespace().count() > 1 || w(l) >= 4) {
            return wrapped.into_iter().map(plain).collect();
        }
        return vec![plain(fit(cands, c).unwrap_or_default())];
    }
    let body = wrap(cands[0], c.saturating_sub(2))
        .into_iter()
        .map(|l| plain(format!(" {l}")))
        .collect();
    framed(title, "", body, c, r, o, &[])
}

fn machine(m: &MachineData, t: Tier, port: bool, c: usize, r: usize, o: &Opts) -> Vec<L> {
    let g = o.glyphs;
    let cpu = m.cpu.map(pct).unwrap_or_else(|| "-".into());
    let memp = m.mem_pct();
    let mem = memp.map(pct).unwrap_or_else(|| "-".into());
    let mem_lv = memp.map(|p| level(p, 80.0, 90.0));
    let cpu_lv = m.cpu.map(|p| level(p, 70.0, 90.0));
    let disk_used = m.disk.map(|(f, t)| 100.0 - f as f64 * 100.0 / t as f64);
    let disk_lv = disk_used.map(|u| level(u, 85.0, 95.0));
    let mem_amt = m.mem.map(|(u, l, _)| format!("{} of {} GiB", gib(u), gib(l)));
    let mem_short = m.mem.map(|(u, l, _)| format!("{}/{}G", gib(u), gib(l)));
    let disk_free = m
        .disk
        .map(|(f, _)| format!("{} GiB free", gib(f)))
        .unwrap_or_else(|| "-".into());
    let disk_short = m
        .disk
        .map(|(f, _)| format!("{}G", gib(f)))
        .unwrap_or_else(|| "-".into());
    let worst = [mem_lv, cpu_lv, disk_lv].into_iter().flatten().max();
    let styled =
        |s: String, lv: Option<Level>| Line::from(Span::styled(s, tone(o, lv.map_or(Color::Reset, Level::color))));
    match t {
        Tier::Micro => {
            let items = vec![
                vec![format!("MEM {mem}"), format!("M{mem}"), mem.clone()],
                vec![format!("CPU {cpu}"), format!("C{cpu}")],
                vec![format!("DSK {disk_short}"), format!("D{disk_short}")],
            ];
            let glyph = worst.map_or("-", |l| l.glyph(g));
            let mut v = stacked(&items, glyph, c, r);
            if let Some(l) = v.first_mut() {
                *l = l.clone().style(tone(o, worst.map_or(Color::Reset, Level::color)));
            }
            v
        }
        Tier::Strip => {
            let sp = spark(&m.cpu_hist, 8, g);
            let items = vec![
                vec![
                    format!("Memory {mem} · {}", mem_amt.clone().unwrap_or_default()),
                    format!("MEM {mem} {}", mem_short.clone().unwrap_or_default()),
                    format!("MEM {mem}"),
                    format!("M {mem}"),
                ],
                vec![format!("CPU {cpu} {sp}"), format!("CPU {cpu}"), format!("C {cpu}")],
                vec![
                    format!("Disk {disk_free}"),
                    format!("DISK {disk_short}"),
                    format!("D {disk_short}"),
                ],
            ];
            vec![styled(strip(&items, c).trim_end().to_string(), worst)]
        }
        Tier::Tiny => {
            let row = |label: &str, val: &str, hist: &[f64], extra: Option<String>| {
                let head = format!("{label:<4} {val:>4} ");
                let room = c.saturating_sub(w(&head));
                let mut cands = vec![];
                if let Some(e) = &extra {
                    if !hist.is_empty() && room > w(e) + 2 {
                        cands.push(format!("{head}{} {e}", spark(hist, room - w(e) - 1, g)));
                    }
                    cands.push(format!("{head}{e}"));
                }
                if !hist.is_empty() {
                    cands.push(format!("{head}{}", spark(hist, room, g)));
                }
                cands.push(head.trim_end().to_string());
                fit(&cands, c).unwrap_or_default()
            };
            let mut v = vec![
                styled(row("CPU", &cpu, &m.cpu_hist, None), cpu_lv),
                styled(row("MEM", &mem, &m.mem_hist, mem_short.clone()), mem_lv),
                styled(
                    fit(&[format!("DISK {disk_free}"), format!("DISK {disk_short}")], c).unwrap_or_default(),
                    disk_lv,
                ),
            ];
            v.truncate(r);
            v
        }
        _ => {
            const LW: usize = 8;
            let mut body = vec![];
            let metric =
                |label: &str, val: &str, graphic: &dyn Fn(usize) -> String, detail: &[String]| -> Vec<String> {
                    let head = format!(" {label:<LW$}{val:>4}  ");
                    let room = c.saturating_sub(w(&head) + 1);
                    if port {
                        let mut v = vec![format!("{head}{}", graphic(room)).trim_end().to_string()];
                        if let Some(d) = fit(detail, c.saturating_sub(LW + 2)) {
                            v.push(format!(" {}{d}", " ".repeat(LW)));
                        }
                        return v;
                    }
                    for d in detail {
                        if room >= w(d) + 10 {
                            return vec![format!("{head}{}  {d}", graphic(room - w(d) - 2))];
                        }
                    }
                    vec![format!("{head}{}", graphic(room)).trim_end().to_string()]
                };
            let cpu_detail: Vec<String> = match (m.cores, m.quota) {
                (_, Some(q)) => vec![format!("of {q:.1} cores (limit)"), format!("{q:.1} cores")],
                (Some(n), None) => vec![format!("of {n} cores"), format!("{n} cores")],
                _ => vec![],
            };
            for l in metric("CPU", &cpu, &|n| spark(&m.cpu_hist, n, g), &cpu_detail) {
                body.push(styled(l, cpu_lv));
            }
            let mem_detail: Vec<String> = match (&mem_amt, mem_lv) {
                (Some(a), Some(lv)) => vec![format!("{a} · {}", lv.word()), a.clone()],
                _ => vec![],
            };
            for l in metric("Memory", &mem, &|n| spark(&m.mem_hist, n, g), &mem_detail) {
                body.push(styled(l, mem_lv));
            }
            let disk_detail: Vec<String> = match m.disk {
                Some((f, tot)) => vec![
                    format!("{} GiB free of {}", gib(f), gib(tot)),
                    format!("{} GiB free", gib(f)),
                ],
                None => vec![],
            };
            let dpct = disk_used.map(pct).unwrap_or_else(|| "-".into());
            let gauge = |n: usize| disk_used.map_or(String::new(), |u| bar(u / 100.0, n.min(44), g));
            for l in metric("Disk", &dpct, &gauge, &disk_detail) {
                body.push(styled(l, disk_lv));
            }
            if t >= Tier::Medium && m.mem.is_some() {
                body.push(plain(""));
                for l in wrap("At 90 % of the memory limit tmuxedo stops the biggest process.", c - 2) {
                    body.push(plain(format!(" {l}")));
                }
            }
            if t == Tier::Large {
                let cw = c.saturating_sub(12);
                for (name, h) in [("CPU", &m.cpu_hist), ("Memory", &m.mem_hist)] {
                    body.push(plain(""));
                    body.push(plain(format!(" {name}, last {} samples", h.len().min(cw))));
                    for line in chart(h, cw, 6, g) {
                        body.push(plain(format!("           {line}").trim_end().to_string()));
                    }
                }
            }
            framed("machine", if o.paused { "paused" } else { "live" }, body, c, r, o, &[])
        }
    }
}

fn status_glyph(s: &str, g: Glyphs) -> (&'static str, Color) {
    let st = crate::agent::parse_status(s).unwrap_or(tmuxedo_protocol::frames::AgentStatus::Unknown);
    let (gl, col) = super::tui::view::glyph(Some(st));
    let gl = match (g, gl) {
        (Glyphs::Ascii, "●") => "*",
        (Glyphs::Ascii, "✓") => "v",
        (Glyphs::Ascii, "○") => "o",
        (_, x) => x,
    };
    (gl, col)
}

fn status_word(s: &str) -> &'static str {
    crate::agent::parse_status(s).map_or("unknown", super::tui::view::status_word)
}

fn agents(a: &[AgentRow], t: Tier, c: usize, r: usize, o: &Opts) -> Vec<L> {
    let g = o.glyphs;
    let blocked = a.iter().filter(|x| x.status == "blocked").count();
    let n = a.len();
    let s = if n == 1 { "" } else { "s" };
    let count: Vec<String> = if blocked > 0 {
        vec![
            format!(
                "{n} agent{s} · {blocked} need{} you",
                if blocked == 1 { "s" } else { "" }
            ),
            format!("{n} · {blocked}!"),
            format!("{blocked}!"),
            "!".into(),
        ]
    } else {
        vec![format!("{n} agent{s}"), format!("{n}")]
    };
    if a.is_empty() && t <= Tier::Tiny {
        let none = vec![
            "No agents on this machine".into(),
            "no agents".into(),
            "none".into(),
            "0".into(),
        ];
        return stacked(&[none], "0", c, r);
    }
    let row_style = |st: &str| tone(o, status_glyph(st, g).1);
    let cands = |x: &AgentRow| {
        let gl = status_glyph(&x.status, g).0;
        vec![
            format!("{gl} {} · {}", x.name, status_word(&x.status)),
            format!("{gl} {}", x.name),
            format!("{gl} {}", x.pane),
            gl.to_string(),
        ]
    };
    match t {
        Tier::Micro => {
            let mut v = stacked(std::slice::from_ref(&count), "!", c, 1);
            for x in a.iter().take(r.saturating_sub(1)) {
                if let Some(s) = fit(&cands(x)[1..], c) {
                    v.push(Line::from(s).style(row_style(&x.status)));
                }
            }
            v
        }
        Tier::Strip => {
            let items: Vec<Vec<String>> = a.iter().map(|x| cands(x)[1..].to_vec()).collect();
            vec![plain(strip(&items, c))]
        }
        Tier::Tiny => {
            let room = if a.len() > r { r - 1 } else { r };
            let mut v: Vec<L> = a
                .iter()
                .take(room)
                .map(|x| Line::from(fit(&cands(x), c).unwrap_or_default()).style(row_style(&x.status)))
                .collect();
            if a.len() > room {
                v.push(plain(format!("+{} more", a.len() - room)));
            }
            v
        }
        _ => {
            let mut body = vec![];
            if a.is_empty() {
                for l in wrap(
                    "No agents on this machine. Start one from tmuxedo, or type its name in a pane.",
                    c - 2,
                ) {
                    body.push(plain(format!(" {l}")));
                }
            }
            let room = r.saturating_sub(3);
            let shown = if a.len() > room {
                room.saturating_sub(1)
            } else {
                a.len()
            };
            // Columns: glyph, name, status word, then by room the agent, the folder, the pane.
            let name_w = a
                .iter()
                .map(|x| w(&x.name))
                .max()
                .unwrap_or(0)
                .clamp(4, 28.min(c.saturating_sub(15)).max(4));
            let kind_w = a.iter().map(|x| w(&x.kind)).max().unwrap_or(0);
            let place_w = a.iter().map(|x| w(&x.place)).max().unwrap_or(0).min(24);
            for x in a.iter().take(shown) {
                let gl = status_glyph(&x.status, g).0;
                // A name too long for its column shows as its pane (a declared short form).
                let name = if w(&x.name) <= name_w {
                    x.name.clone()
                } else {
                    x.pane.clone()
                };
                let pad = name_w.saturating_sub(w(&name));
                let mut line = format!(" {gl} {name}{}  {:<9}", " ".repeat(pad), status_word(&x.status));
                if w(&line) + kind_w + 2 <= c {
                    line = format!("{line}  {:<kind_w$}", x.kind);
                }
                if t >= Tier::Medium && w(&x.place) <= place_w && w(&line) + place_w + 2 <= c {
                    line = format!("{line}  {}{}", x.place, " ".repeat(place_w - w(&x.place)));
                }
                if t == Tier::Large && w(&line) + w(&x.pane) + 2 <= c {
                    line = format!("{line}  {}", x.pane);
                }
                let line = fit(
                    &[
                        line.trim_end().to_string(),
                        format!(" {gl} {name}"),
                        format!(" {gl} {}", x.pane),
                    ],
                    c,
                )
                .unwrap_or_default();
                body.push(Line::from(line).style(row_style(&x.status)));
            }
            if a.len() > shown {
                body.push(plain(format!(" +{} more", a.len() - shown)));
            }
            let right = fit(&count, c.saturating_sub(12)).unwrap_or_default();
            framed("Agents", &right, body, c, r, o, &[])
        }
    }
}

fn needs(n: &[NeedRow], t: Tier, c: usize, r: usize, o: &Opts) -> Vec<L> {
    let g = o.glyphs;
    let k = n.len();
    let ok = if g == Glyphs::Blocks { "✓" } else { "ok" };
    let mark = |x: &NeedRow| {
        if x.high {
            "!"
        } else if g == Glyphs::Blocks {
            "■"
        } else {
            "#"
        }
    };
    let style = |x: &NeedRow| tone(o, if x.high { Color::Red } else { Color::Yellow });
    let item = |x: &NeedRow| {
        vec![
            format!("{} {}: {}", mark(x), x.pane, x.title),
            format!("{} {}", mark(x), x.pane),
        ]
    };
    if k == 0 {
        let cands = vec![
            format!("Nothing needs you {ok}"),
            "nothing waiting".into(),
            ok.to_string(),
            "0".into(),
        ];
        return match t {
            Tier::Micro | Tier::Strip | Tier::Tiny => stacked(&[cands], "0", c, r),
            _ => framed(
                "Needs you",
                "",
                vec![plain(format!(" Nothing needs you {ok}"))],
                c,
                r,
                o,
                &[],
            ),
        };
    }
    let count = vec![
        format!("Needs you · {k}"),
        format!("{k} need you"),
        format!("{k}!"),
        format!("{k}"),
    ];
    match t {
        Tier::Micro => {
            let mut v = vec![Line::from(fit(&count, c).unwrap_or_else(|| "!".into())).style(tone(o, Color::Yellow))];
            for x in n.iter().take(r - 1) {
                if let Some(s) = fit(&[format!("{} {}", mark(x), x.pane), mark(x).to_string()], c) {
                    v.push(Line::from(s).style(style(x)));
                }
            }
            v
        }
        Tier::Strip => {
            // The count is always shown; items follow while they fit (the count already says how many).
            let head = fit(&count, c).unwrap_or_default();
            let items: Vec<Vec<String>> = n.iter().map(item).collect();
            let tail = strip(&items, c.saturating_sub(w(&head) + 2));
            let s = if tail.is_empty() || tail.starts_with('+') {
                head
            } else {
                format!("{head}  {tail}")
            };
            vec![Line::from(s).style(tone(o, Color::Yellow))]
        }
        Tier::Tiny => {
            let mut v = vec![
                Line::from(fit(&count, c).unwrap_or_default()).style(Style::default().add_modifier(Modifier::BOLD))
            ];
            let room = r - 1;
            let shown = if k > room { room - 1 } else { k };
            for x in n.iter().take(shown) {
                v.push(Line::from(fit(&item(x), c).unwrap_or_default()).style(style(x)));
            }
            if k > shown {
                v.push(plain(format!("+{} more", k - shown)));
            }
            v
        }
        _ => {
            let mut body: Vec<L> = vec![];
            let room = r.saturating_sub(3);
            let per = if t >= Tier::Medium { 3 } else { 2 };
            let tw = c.saturating_sub(6);
            for (i, x) in n.iter().enumerate() {
                let right = match (x.high, x.age_ms.map(ago)) {
                    (true, Some(a)) => format!("HIGH RISK · {a}"),
                    (true, None) => "HIGH RISK".into(),
                    (false, Some(a)) => a,
                    (false, None) => String::new(),
                };
                let who = format!(" {} {} · {}", mark(x), x.pane, x.plugin);
                let short = format!(" {} {}", mark(x), x.pane);
                let mut block: Vec<L> = vec![Line::from(two(&[&who, &short], &format!("{right} "), c)).style(style(x))];
                for text in std::iter::once(x.title.clone()).chain(x.command.as_ref().map(|c| format!("command: {c}")))
                {
                    for l in clipped(&text, tw, per) {
                        block.push(plain(format!("     {l}")));
                    }
                }
                let left = k - i;
                if body.len() + block.len() + usize::from(left > 1) > room {
                    body.push(plain(format!(" +{left} more")));
                    break;
                }
                body.append(&mut block);
                if t >= Tier::Medium && left > 1 {
                    body.push(plain(""));
                }
            }
            let place = if o.paused { "paused" } else { "this machine" };
            let more = ["answer in tmuxedo (web, app or TUI)", "answer in tmuxedo"];
            framed(&format!("Needs you · {k}"), place, body, c, r, o, &more)
        }
    }
}

/// The frame as plain text, trailing spaces trimmed (snapshots).
pub fn text(buf: &Buffer) -> String {
    let a = buf.area;
    let mut out = String::new();
    for y in 0..a.height {
        let mut line = String::new();
        let mut skip = 0;
        for x in 0..a.width {
            if skip > 0 {
                // The cell under the second half of a wide character.
                skip -= 1;
                continue;
            }
            let s = buf[(a.x + x, a.y + y)].symbol();
            skip = w(s).saturating_sub(1);
            line.push_str(s);
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

// ---- the live loop ---------------------------------------------------------------------------

/// Read the kind's data once (local only).
fn fetch(kind: &str, sampler: &mut crate::stats::Sampler, machine: &mut MachineData) -> Data {
    match kind {
        "usage" => match super::call_local("usage.list", serde_json::json!({})) {
            Ok(v) if v.get("result").is_some() => Data::Usage(usage_from(&v["result"], crate::util::now_ms())),
            _ => Data::NoRunner,
        },
        "machine" => {
            if let Some(s) = sampler.tick(Instant::now(), crate::util::now_ms()) {
                machine.push(&s);
            }
            Data::Machine(machine.clone())
        }
        "agents" => match super::call_local("agent.list", serde_json::json!({})) {
            Ok(v) if v.get("result").is_some() => Data::Agents(agents_from(&v["result"])),
            _ => Data::NoRunner,
        },
        _ => match super::call_local("needs.list", serde_json::json!({})) {
            Ok(v) if v.get("result").is_some() => Data::Needs(needs_from(&v["result"], crate::util::now_ms())),
            _ => Data::NoRunner,
        },
    }
}

/// `widget` with no name, a picker number or a kind.
pub fn kind_of(arg: Option<&str>) -> Result<Option<&'static str>, String> {
    let Some(a) = arg else { return Ok(None) };
    let by_number = a
        .parse::<usize>()
        .ok()
        .and_then(|n| n.checked_sub(1))
        .and_then(|i| KINDS.get(i));
    by_number
        .or_else(|| KINDS.iter().find(|k| **k == a))
        .map(|k| Some(*k))
        .ok_or_else(|| format!("no widget named {a}; one of: {}", KINDS.join(", ")))
}

pub fn run(rest: &[String]) -> Result<(), String> {
    use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
    let p = super::parse_args(rest);
    let Some(kind) = kind_of(p.pos.first().map(String::as_str))? else {
        print!("{LIST}");
        return Ok(());
    };
    let env = |k: &str| std::env::var(k).ok();
    let mut o = Opts {
        glyphs: Glyphs::choose(p.f("--glyphs").as_deref(), &env),
        color: color_on(&env),
        paused: false,
    };
    let keep_alive = p.has("--keep-alive");
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
    let mut sampler = crate::stats::Sampler::new(crate::stats::Roots::system(
        std::path::Path::new(&home),
        std::process::id(),
    ));
    sampler.set_rate(Some(tmuxedo_protocol::stats::STATS_HZ_MAX), Instant::now());
    let mut machine = MachineData::default();
    let mut term = ratatui::try_init().map_err(|e| e.to_string())?;
    let mut last_input = Instant::now();
    let mut next = Instant::now();
    let mut shown: Option<(Data, (u16, u16), bool)> = None;
    let mut data = Data::NoRunner;
    let r = loop {
        let now = Instant::now();
        let size = term.size().map(|s| (s.width, s.height)).unwrap_or((80, 24));
        o.paused = !keep_alive && now.duration_since(last_input) >= IDLE_STOP;
        if !o.paused && now >= next {
            data = fetch(kind, &mut sampler, &mut machine);
            next = now + tier(size.0, size.1).interval();
        }
        // Redraw only when what is shown would change (data, size or the paused note).
        let key = (data.clone(), size, o.paused);
        if shown.as_ref() != Some(&key) {
            if let Err(e) = term.draw(|f| render(&data, f.area(), f.buffer_mut(), &o)) {
                break Err(e.to_string());
            }
            shown = Some(key);
        }
        let wait = if o.paused {
            Duration::from_secs(3600)
        } else {
            next.saturating_duration_since(Instant::now())
                .min(IDLE_STOP.saturating_sub(last_input.elapsed()))
        };
        match event::poll(wait) {
            Ok(true) => match event::read() {
                Ok(Event::Key(k)) if k.kind != KeyEventKind::Release => {
                    let ctrl_c = k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c');
                    if ctrl_c || matches!(k.code, KeyCode::Char('q') | KeyCode::Esc) {
                        break Ok(());
                    }
                    if o.paused {
                        next = Instant::now();
                    }
                    last_input = Instant::now();
                }
                // SIGWINCH: the next turn re-picks the tier and redraws in the same frame.
                Ok(Event::Resize(..)) => last_input = Instant::now(),
                Ok(_) => {}
                Err(e) => break Err(e.to_string()),
            },
            Ok(false) => {}
            Err(e) => break Err(e.to_string()),
        }
    };
    ratatui::restore();
    r
}
