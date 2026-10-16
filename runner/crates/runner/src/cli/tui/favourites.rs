//! Layout favourites in the TUI (docs/design/pane-layouts.md §7, §8.5): the
//! account's list (`GET /api/me/layouts`, read with the CLI token), read
//! strictly again here, the matcher and restore planner, and the words of the
//! restore preview. Pure. The Rust twin of `packages/layouts/src/favourites.ts`
//! (`planRestore`, `matchScore`, `restoreLayout`, `cwdSlot`) and of the
//! preview words in `web/src/lib/favourites.ts`; the shared fixture
//! `tests/fixtures/favourites.json` holds both sides to the same plans.
//!
//! A favourite never carries a command: a restore opens a plain shell by
//! splitting a terminal, starts an agent by its kind when the slot holds one,
//! then arranges with one `layout.arrange`. Nothing is ever closed: panes
//! with no slot move to a new tab "Extras".

use serde_json::{json, Value};
use tmuxedo_protocol::frames::{Layout, SplitDir};

pub const MAX_SLOTS: usize = 12;
pub const MAX_DEPTH: usize = 12;
pub const NAME_MAX: usize = 40;
pub const LABEL_MAX: usize = 64;
pub const PATH_MAX: usize = 256;
/// At most this many favourites are read from the list.
pub const LIST_MAX: usize = 24;

#[derive(Debug, Clone, PartialEq)]
pub enum Shape {
    Slot(usize),
    Split {
        dir: SplitDir,
        ratio: f64,
        a: Box<Shape>,
        b: Box<Shape>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct SlotCwd {
    /// `workspace` (path relative to the workspace's folder) or `absolute`.
    pub base: String,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Slot {
    /// `any`, `shell` or `agent`.
    pub holds: String,
    pub agent: Option<String>,
    pub label: Option<String>,
    pub cwd: Option<SlotCwd>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Favourite {
    pub id: String,
    pub name: String,
    pub shape: Shape,
    pub slots: Vec<Slot>,
}

/// A pane of the tab as the planner sees it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct OpenPane {
    pub id: String,
    pub agent: Option<String>,
    pub label: Option<String>,
    pub cwd: Option<SlotCwd>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RestorePlan {
    /// Per slot: the pane that fills it, or None when one is opened.
    pub assigned: Vec<Option<String>>,
    /// Slots nobody fills, ascending.
    pub missing: Vec<usize>,
    /// Panes with no slot, in the order given: moved, never closed.
    pub extras: Vec<String>,
}

fn only_keys(o: &serde_json::Map<String, Value>, keys: &[&str]) -> bool {
    o.keys().all(|k| keys.contains(&k.as_str()))
}

fn plain(s: &str, max: usize) -> bool {
    !s.is_empty() && s.chars().count() <= max && !s.chars().any(char::is_control)
}

fn shape(v: &Value, depth: usize, seen: &mut Vec<usize>) -> Option<Shape> {
    let o = v.as_object()?;
    if depth > MAX_DEPTH {
        return None;
    }
    if o.contains_key("slot") {
        let n = o["slot"].as_u64()? as usize;
        if !only_keys(o, &["slot"]) || n >= MAX_SLOTS || seen.contains(&n) {
            return None;
        }
        seen.push(n);
        return Some(Shape::Slot(n));
    }
    if !only_keys(o, &["dir", "ratio", "a", "b"]) {
        return None;
    }
    let dir = match o.get("dir")?.as_str()? {
        "h" => SplitDir::H,
        "v" => SplitDir::V,
        _ => return None,
    };
    let ratio = o.get("ratio")?.as_f64()?;
    if !(ratio.is_finite() && ratio > 0.0 && ratio < 1.0) {
        return None;
    }
    let a = shape(o.get("a")?, depth + 1, seen)?;
    let b = shape(o.get("b")?, depth + 1, seen)?;
    Some(Shape::Split {
        dir,
        ratio,
        a: Box::new(a),
        b: Box::new(b),
    })
}

fn slot(v: &Value) -> Option<Slot> {
    let o = v.as_object()?;
    if !only_keys(o, &["holds", "agent", "label", "cwd"]) {
        return None;
    }
    let holds = o.get("holds")?.as_str()?;
    if !matches!(holds, "any" | "shell" | "agent") {
        return None;
    }
    let agent = match o.get("agent") {
        None => None,
        Some(a) => Some(a.as_str().filter(|a| plain(a, LABEL_MAX))?.to_string()),
    };
    if (holds == "agent") != agent.is_some() {
        return None;
    }
    let label = match o.get("label") {
        None => None,
        Some(l) => Some(l.as_str().filter(|l| plain(l, LABEL_MAX))?.to_string()),
    };
    let cwd = match o.get("cwd") {
        None => None,
        Some(c) => {
            let c = c.as_object().filter(|c| only_keys(c, &["base", "path"]))?;
            let base = c
                .get("base")?
                .as_str()
                .filter(|b| *b == "workspace" || *b == "absolute")?;
            let path = c.get("path")?.as_str()?;
            if path.chars().count() > PATH_MAX || path.chars().any(char::is_control) {
                return None;
            }
            Some(SlotCwd {
                base: base.into(),
                path: path.into(),
            })
        }
    };
    Some(Slot {
        holds: holds.into(),
        agent,
        label,
        cwd,
    })
}

/// One row of the list, or None when it is not a favourite this TUI can trust.
pub fn parse_favourite(v: &Value) -> Option<Favourite> {
    let id = v["id"].as_str().filter(|id| {
        !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    })?;
    let name = v["name"].as_str().map(str::trim).filter(|n| plain(n, NAME_MAX))?;
    let mut seen = vec![];
    let shape = shape(&v["shape"], 0, &mut seen)?;
    let slots: Vec<Slot> = v["slots"].as_array()?.iter().map(slot).collect::<Option<_>>()?;
    // Slot numbers are exactly 0..n, each once.
    seen.sort_unstable();
    if slots.is_empty() || slots.len() > MAX_SLOTS || seen != (0..slots.len()).collect::<Vec<_>>() {
        return None;
    }
    Some(Favourite {
        id: id.into(),
        name: name.into(),
        shape,
        slots,
    })
}

/// `GET /api/me/layouts` (`{favourites: [...]}`): rows that do not read are left out.
pub fn parse_list(body: &Value) -> Vec<Favourite> {
    let mut out: Vec<Favourite> = vec![];
    for row in body["favourites"].as_array().into_iter().flatten().take(4 * LIST_MAX) {
        if out.len() >= LIST_MAX {
            break;
        }
        if let Some(f) = parse_favourite(row) {
            if !out.iter().any(|g| g.id == f.id) {
                out.push(f);
            }
        }
    }
    out
}

/// A pane's folder as a favourite keeps it (`cwdSlot`).
pub fn cwd_slot(pane_cwd: Option<&str>, workspace_cwd: Option<&str>) -> Option<SlotCwd> {
    let pane_cwd = pane_cwd.filter(|c| c.starts_with('/'))?;
    let clean = |p: &str| {
        if p.len() > 1 {
            p.trim_end_matches('/').to_string()
        } else {
            p.to_string()
        }
    };
    let cwd = clean(pane_cwd);
    let ws = workspace_cwd.filter(|w| w.starts_with('/')).map(clean);
    let got = match ws {
        Some(ws) if cwd == ws => SlotCwd {
            base: "workspace".into(),
            path: String::new(),
        },
        Some(ws) if ws != "/" && cwd.starts_with(&format!("{ws}/")) => SlotCwd {
            base: "workspace".into(),
            path: cwd[ws.len() + 1..].to_string(),
        },
        _ => SlotCwd {
            base: "absolute".into(),
            path: cwd,
        },
    };
    (got.path.chars().count() <= PATH_MAX && !got.path.chars().any(char::is_control)).then_some(got)
}

/// May `pane` fill `slot`, and how well (`matchScore`): an agent slot only a
/// pane running that agent, a shell slot only a pane running none; then the
/// same name +4, an agent slot +3, the same folder +1.
pub fn match_score(slot: &Slot, pane: &OpenPane) -> Option<u32> {
    let agent = pane.agent.as_deref().filter(|a| !a.is_empty());
    if slot.holds == "agent" && agent != slot.agent.as_deref() {
        return None;
    }
    if slot.holds == "shell" && agent.is_some() {
        return None;
    }
    let mut score = 0;
    if slot.label.is_some() && pane.label == slot.label {
        score += 4;
    }
    if slot.holds == "agent" {
        score += 3;
    }
    if let (Some(s), Some(p)) = (&slot.cwd, &pane.cwd) {
        if s == p {
            score += 1;
        }
    }
    Some(score)
}

/// Match the tab's panes (reading order) to the slots (`planRestore`): best
/// score first, ties by slot number, then by the panes' order.
pub fn plan_restore(slots: &[Slot], panes: &[OpenPane]) -> RestorePlan {
    let mut open: Vec<&OpenPane> = vec![];
    for p in panes {
        if !p.id.is_empty() && !open.iter().any(|o| o.id == p.id) {
            open.push(p);
        }
    }
    let mut pairs = vec![];
    for (si, s) in slots.iter().enumerate() {
        for (pi, p) in open.iter().enumerate() {
            if let Some(score) = match_score(s, p) {
                pairs.push((score, si, pi));
            }
        }
    }
    pairs.sort_by(|x, y| y.0.cmp(&x.0).then(x.1.cmp(&y.1)).then(x.2.cmp(&y.2)));
    let mut assigned: Vec<Option<String>> = vec![None; slots.len()];
    let mut taken = vec![false; open.len()];
    for (_, si, pi) in pairs {
        if assigned[si].is_some() || taken[pi] {
            continue;
        }
        assigned[si] = Some(open[pi].id.clone());
        taken[pi] = true;
    }
    RestorePlan {
        missing: (0..slots.len()).filter(|i| assigned[*i].is_none()).collect(),
        extras: open
            .iter()
            .zip(&taken)
            .filter(|(_, t)| !**t)
            .map(|(p, _)| p.id.clone())
            .collect(),
        assigned,
    }
}

pub fn shape_slots(s: &Shape) -> Vec<usize> {
    match s {
        Shape::Slot(n) => vec![*n],
        Shape::Split { a, b, .. } => [shape_slots(a), shape_slots(b)].concat(),
    }
}

/// The favourite's shape with a pane in every slot (`restoreLayout`); None
/// unless `filled` names one pane per slot, each once.
pub fn restore_layout(shape: &Shape, filled: &[Option<String>]) -> Option<Layout> {
    let ids: Vec<&String> = shape_slots(shape)
        .iter()
        .map(|s| filled.get(*s).and_then(Option::as_ref).filter(|id| !id.is_empty()))
        .collect::<Option<_>>()?;
    let mut uniq = ids.clone();
    uniq.sort();
    uniq.dedup();
    if uniq.len() != ids.len() {
        return None;
    }
    fn walk(n: &Shape, filled: &[Option<String>]) -> Layout {
        match n {
            Shape::Slot(s) => Layout::Pane {
                pane: filled[*s].clone().unwrap_or_default(),
            },
            Shape::Split { dir, ratio, a, b } => Layout::Split {
                dir: *dir,
                ratio: *ratio,
                a: Box::new(walk(a, filled)),
                b: Box::new(walk(b, filled)),
            },
        }
    }
    Some(walk(shape, filled))
}

/// A tree in the runner's `layout.arrange` wire format.
pub fn wire(l: &Layout) -> Value {
    match l {
        Layout::Pane { pane } => json!({ "type": "pane", "pane_id": pane }),
        Layout::Split { dir, ratio, a, b } => json!({
            "type": "split",
            "direction": if *dir == SplitDir::H { "right" } else { "down" },
            "ratio": ratio,
            "first": wire(a),
            "second": wire(b),
        }),
    }
}

/// An agent's kind as people say it.
pub fn agent_word(kind: &str) -> String {
    match kind {
        "claude" => "Claude".into(),
        "codex" => "Codex".into(),
        "cursor" | "cursor-agent" => "Cursor".into(),
        "gemini" => "Gemini".into(),
        k => k.to_string(),
    }
}

fn slot_word(s: &Slot) -> String {
    match s.holds.as_str() {
        "agent" => agent_word(s.agent.as_deref().unwrap_or("")),
        _ => "shell".into(),
    }
}

pub fn count(n: usize, one: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {one}s")
    }
}

/// What a restore of `f` on this tab will do (the preview), and the one
/// button's words ("Open 1 agent and arrange").
pub struct Preview {
    pub plan: RestorePlan,
    pub lines: Vec<String>,
    pub button: String,
}

pub fn preview(f: &Favourite, panes: &[OpenPane]) -> Preview {
    let plan = plan_restore(&f.slots, panes);
    let missing: Vec<&Slot> = plan.missing.iter().map(|i| &f.slots[*i]).collect();
    let agents = missing.iter().filter(|s| s.holds == "agent").count();
    let shells = missing.len() - agents;
    let reused = plan.assigned.len() - plan.missing.len();
    let mut head = vec![];
    if reused > 0 {
        head.push(format!("Reuses {}", count(reused, "terminal")));
    }
    if !missing.is_empty() {
        let words: Vec<String> = missing.iter().map(|s| slot_word(s)).collect();
        head.push(format!("opens {}: {}", missing.len(), words.join(", ")));
    }
    let head = head.join(" · ");
    let mut lines = vec![if head.is_empty() {
        "Nothing to open.".to_string()
    } else {
        let mut c = head.chars();
        c.next()
            .map(|f| f.to_uppercase().chain(c).collect())
            .unwrap_or_default()
    }];
    if !plan.extras.is_empty() {
        let n = plan.extras.len();
        lines.push(format!(
            "{} {} to a new tab “Extras”. Nothing is closed.",
            count(n, "other terminal"),
            if n == 1 { "moves" } else { "move" }
        ));
    }
    if agents > 0 {
        lines.push("Agents use your plan’s limits.".into());
    }
    let open: Vec<String> = [(agents, "agent"), (shells, "terminal")]
        .iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, w)| count(*n, w))
        .collect();
    let button = if open.is_empty() {
        "Arrange".to_string()
    } else {
        format!("Open {} and arrange", open.join(" and "))
    };
    Preview { plan, lines, button }
}

/// Every slot's rectangle as fractions of the whole, by slot number (the picture).
pub fn slot_boxes(shape: &Shape) -> Vec<(usize, [f64; 4])> {
    fn walk(n: &Shape, b: [f64; 4], out: &mut Vec<(usize, [f64; 4])>) {
        match n {
            Shape::Slot(s) => out.push((*s, b)),
            Shape::Split {
                dir,
                ratio,
                a,
                b: second,
            } => {
                let [x, y, w, h] = b;
                if *dir == SplitDir::H {
                    walk(a, [x, y, w * ratio, h], out);
                    walk(second, [x + w * ratio, y, w * (1.0 - ratio), h], out);
                } else {
                    walk(a, [x, y, w, h * ratio], out);
                    walk(second, [x, y + h * ratio, w, h * (1.0 - ratio)], out);
                }
            }
        }
    }
    let mut out = vec![];
    walk(shape, [0.0, 0.0, 1.0, 1.0], &mut out);
    out
}

/// A refusal from the runner, in the web's words (`arrangeProblem`).
pub fn problem(code: &str, undo: bool) -> &'static str {
    match code {
        "wont_fit" => "There is no room here for every terminal in that layout. Nothing was changed.",
        "would_close" if undo => "Undo would leave a terminal out, so nothing was changed.",
        "would_close" => "That layout would leave a terminal out, so nothing was changed.",
        "changed" if undo => "The layout has changed since, so there is nothing to undo.",
        "changed" => "The layout changed in another window. Pick again.",
        "forbidden_scope" => "You are watching. Take control to arrange.",
        "not_supported" | "unknown_method" => "This machine’s runner cannot arrange terminals yet. Update the runner.",
        "machine_offline" | "not_connected" | "disconnected" => "The machine is not connected.",
        "not_found" if undo => "A terminal from that layout is gone, so there is nothing to undo.",
        "not_found" => "That tab is gone.",
        _ if undo => "Undo did not work. Nothing was changed.",
        _ => "That did not work. Nothing was changed.",
    }
}
