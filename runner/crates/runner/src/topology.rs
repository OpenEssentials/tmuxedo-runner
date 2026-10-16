//! The machine's topology: workspaces → tabs (each a split tree) → panes.
//! Pure data and operations, no processes: the daemon applies the returned
//! effects (spawn, kill, events). Ids are `w<n>`, `w<n>:t<n>`, `w<n>:p<n>` and never reused:
//! `w3`, `w3:t2`, `w3:p7` (a pane moved to another workspace keeps its id).
//!
//! Split directions map runner API ↔ gateway protocol: `right` = `h` (side by side,
//! `a` left), `down` = `v` (stacked, `a` on top). Ratio is the share of `a`.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use tmuxedo_protocol::frames::{Layout, Pane, SplitDir, Tab, Workspace};

pub const MIN_RATIO: f64 = 0.05;

pub use tmuxedo_protocol::names::{clean_name, default_tab_name, default_workspace_name};

/// A name given at creation or rename, cleaned (docs/protocol.md "Names");
/// None when nothing is left, so the caller uses the default name.
pub fn clean_label(label: Option<String>) -> Option<String> {
    label.map(|l| clean_name(&l)).filter(|l| !l.is_empty())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceRec {
    pub id: String,
    pub name: String,
    pub cwd: String,
    pub position: u32,
    pub active_tab: String,
    pub next_tab: u32,
    pub next_pane: u32,
    /// Set for workspaces opened on a git worktree (worktree.create/open).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<crate::worktree::Membership>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TabRec {
    pub id: String,
    pub workspace_id: String,
    pub name: String,
    pub position: u32,
    pub layout: Layout,
    pub focused_pane: String,
    #[serde(default)]
    pub zoomed: bool,
    /// The pattern `layout.arrange` last applied here (a hint for clients).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
}

/// Agent facts that outlive the runner process (resume needs them).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentMeta {
    pub kind: Option<String>,
    pub session_id: Option<String>,
    pub session_path: Option<String>,
    pub resume_argv: Option<Vec<String>>,
    /// The cwd was a git checkout when the session was recorded (resume guard).
    pub cwd_was_repo: bool,
    pub name: Option<String>,
    pub display_agent: Option<String>,
    pub title: Option<String>,
    pub state_labels: BTreeMap<String, String>,
    pub tokens: BTreeMap<String, String>,
    /// The agent's helpers (crate::subagents), kept so a runner restart still
    /// shows the ones running in the agent's process.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub helpers: Vec<tmuxedo_protocol::subagents::AgentChild>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PaneRecord {
    #[serde(flatten)]
    pub pane: Pane,
    pub argv: Vec<String>,
    pub gen: u64,
    pub base: u64,
    pub high_water: u64,
    // The pane's name is `pane.label` (protocol Pane, sent in the topology).
    // State files from before kept it under the same `label` key, which the
    // flattened `pane` now reads.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub meta: AgentMeta,
}

impl PaneRecord {
    pub fn holder_id(&self) -> String {
        format!("{}#{}", self.pane.id, self.gen)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct State {
    pub rev: u64,
    pub event_seq: u64,
    #[serde(default = "one")]
    pub next_ws: u32,
    #[serde(default)]
    pub focused_workspace: Option<String>,
    pub workspaces: Vec<WorkspaceRec>,
    pub tabs: Vec<TabRec>,
    pub panes: Vec<PaneRecord>,
    /// Size new tabs are laid out in (cells).
    #[serde(default = "default_area")]
    pub area: (u16, u16),
}

fn one() -> u32 {
    1
}
fn default_area() -> (u16, u16) {
    (120, 40)
}

/// Gap left between incarnations' offset ranges.
pub const INCARNATION_GAP: u64 = 1 << 24;

#[derive(Debug, Clone, PartialEq)]
pub struct TopoError(pub &'static str, pub String);

pub type TResult<T> = Result<T, TopoError>;

fn not_found(kind: &'static str, id: &str) -> TopoError {
    TopoError(kind, format!("no {} {id}", kind.trim_end_matches("_not_found")))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Left,
    Right,
    Up,
    Down,
}

impl Dir {
    pub fn parse(s: &str) -> Option<Dir> {
        match s {
            "left" => Some(Dir::Left),
            "right" => Some(Dir::Right),
            "up" => Some(Dir::Up),
            "down" => Some(Dir::Down),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

impl Rect {
    pub fn json(&self) -> Value {
        json!({"x": self.x, "y": self.y, "width": self.w, "height": self.h})
    }
}

pub fn split_dir_name(d: SplitDir) -> &'static str {
    match d {
        SplitDir::H => "right",
        SplitDir::V => "down",
    }
}

pub fn split_dir_parse(s: &str) -> Option<SplitDir> {
    match s {
        "right" | "h" => Some(SplitDir::H),
        "down" | "v" => Some(SplitDir::V),
        _ => None,
    }
}

// ── layout tree helpers ───────────────────────────────────────────────────

pub fn layout_panes(l: &Layout, out: &mut Vec<String>) {
    match l {
        Layout::Pane { pane } => out.push(pane.clone()),
        Layout::Split { a, b, .. } => {
            layout_panes(a, out);
            layout_panes(b, out);
        }
    }
}

fn contains(l: &Layout, id: &str) -> bool {
    match l {
        Layout::Pane { pane } => pane == id,
        Layout::Split { a, b, .. } => contains(a, id) || contains(b, id),
    }
}

/// Replace leaf `id` by `f(leaf)`.
fn replace_leaf(l: &mut Layout, id: &str, f: &mut dyn FnMut(Layout) -> Layout) -> bool {
    match l {
        Layout::Pane { pane } if pane == id => {
            let old = std::mem::replace(l, Layout::Pane { pane: String::new() });
            *l = f(old);
            true
        }
        Layout::Pane { .. } => false,
        Layout::Split { a, b, .. } => replace_leaf(a, id, f) || replace_leaf(b, id, f),
    }
}

/// Remove leaf `id`; its sibling takes the parent's place. None if `l` was that leaf.
fn remove_leaf(l: Layout, id: &str) -> Option<Layout> {
    match l {
        Layout::Pane { ref pane } if pane == id => None,
        Layout::Pane { .. } => Some(l),
        Layout::Split { dir, ratio, a, b } => match (remove_leaf(*a, id), remove_leaf(*b, id)) {
            (Some(a), Some(b)) => Some(Layout::Split {
                dir,
                ratio,
                a: Box::new(a),
                b: Box::new(b),
            }),
            (Some(x), None) | (None, Some(x)) => Some(x),
            (None, None) => None,
        },
    }
}

fn rename_leaf(l: &mut Layout, from: &str, to: &str) {
    match l {
        Layout::Pane { pane } => {
            if pane == from {
                *pane = to.to_string();
            }
        }
        Layout::Split { a, b, .. } => {
            rename_leaf(a, from, to);
            rename_leaf(b, from, to);
        }
    }
}

pub type PaneRect = (String, Rect);
pub type SplitRect = (String, SplitDir, f64, Rect);

/// Rectangles of every pane and split for `area`.
pub fn rects(
    l: &Layout,
    area: Rect,
    panes: &mut Vec<(String, Rect)>,
    splits: &mut Vec<(String, SplitDir, f64, Rect)>,
    path: &str,
) {
    match l {
        Layout::Pane { pane } => panes.push((pane.clone(), area)),
        Layout::Split { dir, ratio, a, b } => {
            splits.push((
                if path.is_empty() {
                    "root".into()
                } else {
                    path.to_string()
                },
                *dir,
                *ratio,
                area,
            ));
            let (ra, rb) = match dir {
                SplitDir::H => {
                    let wa = ((area.w as f64) * ratio).round().clamp(1.0, (area.w.max(2) - 1) as f64) as u16;
                    (
                        Rect { w: wa, ..area },
                        Rect {
                            x: area.x + wa,
                            w: area.w.saturating_sub(wa).max(1),
                            ..area
                        },
                    )
                }
                SplitDir::V => {
                    let ha = ((area.h as f64) * ratio).round().clamp(1.0, (area.h.max(2) - 1) as f64) as u16;
                    (
                        Rect { h: ha, ..area },
                        Rect {
                            y: area.y + ha,
                            h: area.h.saturating_sub(ha).max(1),
                            ..area
                        },
                    )
                }
            };
            rects(a, ra, panes, splits, &format!("{path}0"));
            rects(b, rb, panes, splits, &format!("{path}1"));
        }
    }
}

/// The split at `path` (false = first/a, true = second/b).
fn split_at_path<'a>(l: &'a mut Layout, path: &[bool]) -> Option<&'a mut Layout> {
    match path.split_first() {
        None => matches!(l, Layout::Split { .. }).then_some(l),
        Some((&second, rest)) => match l {
            Layout::Split { a, b, .. } => split_at_path(if second { b } else { a }, rest),
            Layout::Pane { .. } => None,
        },
    }
}

/// Path (as booleans) of the nearest ancestor split of `id` along `axis`, and whether `id` is in `a`.
fn ancestor_split(l: &Layout, id: &str, axis: SplitDir, path: &mut Vec<bool>) -> Option<(Vec<bool>, bool)> {
    match l {
        Layout::Pane { .. } => None,
        Layout::Split { dir, a, b, .. } => {
            let in_a = contains(a, id);
            if !in_a && !contains(b, id) {
                return None;
            }
            path.push(!in_a);
            let deeper = ancestor_split(if in_a { a } else { b }, id, axis, path);
            path.pop();
            if deeper.is_some() {
                return deeper;
            }
            (*dir == axis).then(|| (path.clone(), in_a))
        }
    }
}

// ── State ─────────────────────────────────────────────────────────────────

/// What `State::arrange_tab` did besides setting the tab's tree.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Arranged {
    /// The new tab holding the panes that got no slot.
    pub extras_tab: Option<String>,
    /// Other tabs that gave a pane to the arrangement and still exist.
    pub touched: Vec<String>,
    /// Tabs left empty by that (tabs only: no pane closes).
    pub closed: Closed,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Closed {
    pub panes: Vec<String>,
    pub tabs: Vec<(String, String)>,
    pub workspaces: Vec<String>,
}

/// `state.json` format version. Older files are migrated forward on load,
/// never discarded: `rev` and `eventSeq` must survive every upgrade (a reset
/// seq collides with events web already stored).
///
/// - 1: runner r000001 (Phase 2): protocol-shaped workspaces and tabs, no
///   `version` field.
/// - 2: Phase 3 topology (`activeTab`, `nextTab`, `nextPane`, `focusedPane`,
///   `nextWs`, `focusedWorkspace`, `area`). Files written before the
///   `version` field existed are recognised by shape.
pub const STATE_VERSION: u64 = 2;

/// What [`State::load`] found.
#[derive(Debug)]
pub enum Loaded {
    Missing,
    Ok(State),
    /// Not usable even after migration (corrupt, or from a newer runner).
    /// The file is kept as `backup`; the counters are carried forward.
    Unreadable {
        rev: u64,
        event_seq: u64,
        backup: Option<std::path::PathBuf>,
        why: String,
    },
}

impl Loaded {
    pub fn state(self) -> Option<State> {
        match self {
            Loaded::Ok(s) => Some(s),
            _ => None,
        }
    }
}

fn id_num(id: &str) -> u32 {
    let last = id.rsplit(':').next().unwrap_or(id);
    last.trim_start_matches(|c: char| c.is_ascii_alphabetic())
        .parse()
        .unwrap_or(0)
}

fn first_leaf(layout: &serde_json::Value) -> Option<String> {
    if let Some(p) = layout.get("pane").and_then(|p| p.as_str()) {
        return Some(p.to_string());
    }
    first_leaf(layout.get("a")?).or_else(|| first_leaf(layout.get("b")?))
}

/// Bring a parsed `state.json` to [`STATE_VERSION`].
pub fn migrate_state(mut v: serde_json::Value) -> Result<serde_json::Value, String> {
    use serde_json::json;
    let looks_v1 = v["workspaces"]
        .as_array()
        .is_some_and(|ws| ws.iter().any(|w| w.get("activeTab").is_none()));
    let version = v["version"].as_u64().unwrap_or(if looks_v1 { 1 } else { 2 });
    if version > STATE_VERSION {
        return Err(format!("written by a newer runner (state version {version})"));
    }
    if version < 2 {
        let tabs = v["tabs"].as_array().cloned().unwrap_or_default();
        let panes = v["panes"].as_array().cloned().unwrap_or_default();
        let mut new_tabs = vec![];
        for mut t in tabs.clone() {
            t["focusedPane"] = json!(first_leaf(&t["layout"]).unwrap_or_default());
            t["zoomed"] = json!(false);
            new_tabs.push(t);
        }
        let mut ws_list = v["workspaces"].as_array().cloned().unwrap_or_default();
        ws_list.sort_by_key(|w| w["position"].as_u64().unwrap_or(0));
        for w in ws_list.iter_mut() {
            let wid = w["id"].as_str().unwrap_or("").to_string();
            let mut mine: Vec<&serde_json::Value> = tabs
                .iter()
                .filter(|t| t["workspaceId"].as_str() == Some(&wid))
                .collect();
            mine.sort_by_key(|t| t["position"].as_u64().unwrap_or(0));
            w["activeTab"] = json!(mine.first().and_then(|t| t["id"].as_str()).unwrap_or(""));
            let max_tab = mine
                .iter()
                .filter_map(|t| t["id"].as_str())
                .map(id_num)
                .max()
                .unwrap_or(0);
            let prefix = format!("{wid}:");
            let max_pane = panes
                .iter()
                .filter_map(|p| p["id"].as_str())
                .filter(|id| id.starts_with(&prefix))
                .map(id_num)
                .max()
                .unwrap_or(0);
            w["nextTab"] = json!(max_tab + 1);
            w["nextPane"] = json!(max_pane + 1);
        }
        let max_ws = ws_list
            .iter()
            .filter_map(|w| w["id"].as_str())
            .map(id_num)
            .max()
            .unwrap_or(0);
        v["focusedWorkspace"] = json!(ws_list.first().and_then(|w| w["id"].as_str()));
        v["nextWs"] = json!(max_ws + 1);
        v["workspaces"] = json!(ws_list);
        v["tabs"] = json!(new_tabs);
    }
    v["version"] = json!(STATE_VERSION);
    Ok(v)
}

impl State {
    pub fn load(path: &std::path::Path) -> Loaded {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Loaded::Missing;
        };
        let raw: serde_json::Value = serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
        let parsed = if raw.is_null() {
            Err("not JSON".to_string())
        } else {
            migrate_state(raw.clone()).and_then(|v| serde_json::from_value::<State>(v).map_err(|e| e.to_string()))
        };
        match parsed {
            Ok(s) => Loaded::Ok(s),
            Err(why) => {
                let backup =
                    path.with_extension(format!("json.unreadable-{}", crate::util::iso_now().replace(':', "")));
                let backup = std::fs::copy(path, &backup).ok().map(|_| backup);
                Loaded::Unreadable {
                    rev: raw["rev"].as_u64().unwrap_or(0),
                    event_seq: raw["eventSeq"].as_u64().unwrap_or(0),
                    backup,
                    why,
                }
            }
        }
    }

    pub fn save(&self, path: &std::path::Path) -> Result<(), String> {
        let mut v = serde_json::to_value(self).map_err(|e| e.to_string())?;
        v["version"] = serde_json::json!(STATE_VERSION);
        let text = serde_json::to_vec_pretty(&v).map_err(|e| e.to_string())?;
        crate::config::write_private(path, &text)
    }

    pub fn new_empty(cols: u16, rows: u16) -> State {
        State {
            rev: 0,
            event_seq: 0,
            next_ws: 1,
            focused_workspace: None,
            workspaces: vec![],
            tabs: vec![],
            panes: vec![],
            area: (cols, rows),
        }
    }

    /// The Phase 2 starting point: one workspace, one tab, one pane in `cwd`.
    pub fn default_topology(cwd: &str, argv: Vec<String>, cols: u16, rows: u16) -> State {
        let mut s = State::new_empty(cols, rows);
        s.create_workspace(cwd, None, argv, BTreeMap::new());
        s.rev = 1;
        s
    }

    // ── lookups ──

    pub fn pane(&self, id: &str) -> Option<&PaneRecord> {
        self.panes.iter().find(|p| p.pane.id == id)
    }
    pub fn pane_mut(&mut self, id: &str) -> Option<&mut PaneRecord> {
        self.panes.iter_mut().find(|p| p.pane.id == id)
    }
    pub fn tab(&self, id: &str) -> Option<&TabRec> {
        self.tabs.iter().find(|t| t.id == id)
    }
    pub fn tab_mut(&mut self, id: &str) -> Option<&mut TabRec> {
        self.tabs.iter_mut().find(|t| t.id == id)
    }
    pub fn workspace(&self, id: &str) -> Option<&WorkspaceRec> {
        self.workspaces.iter().find(|w| w.id == id)
    }
    pub fn workspace_mut(&mut self, id: &str) -> Option<&mut WorkspaceRec> {
        self.workspaces.iter_mut().find(|w| w.id == id)
    }
    pub fn tabs_of(&self, ws: &str) -> Vec<&TabRec> {
        let mut v: Vec<&TabRec> = self.tabs.iter().filter(|t| t.workspace_id == ws).collect();
        v.sort_by_key(|t| t.position);
        v
    }
    pub fn panes_of_tab(&self, tab: &str) -> Vec<String> {
        let mut out = vec![];
        if let Some(t) = self.tab(tab) {
            layout_panes(&t.layout, &mut out);
        }
        out
    }
    pub fn panes_of_ws(&self, ws: &str) -> Vec<String> {
        self.tabs_of(ws).iter().flat_map(|t| self.panes_of_tab(&t.id)).collect()
    }
    pub fn workspaces_sorted(&self) -> Vec<&WorkspaceRec> {
        let mut v: Vec<&WorkspaceRec> = self.workspaces.iter().collect();
        v.sort_by_key(|w| w.position);
        v
    }
    /// (workspace id, tab id) of a pane.
    pub fn workspace_of(&self, pane_id: &str) -> Option<(String, String)> {
        let p = self.pane(pane_id)?;
        let t = self.tab(&p.pane.tab_id)?;
        Some((t.workspace_id.clone(), t.id.clone()))
    }
    pub fn focused_workspace(&self) -> Option<&WorkspaceRec> {
        self.focused_workspace
            .as_deref()
            .and_then(|w| self.workspace(w))
            .or_else(|| self.workspaces_sorted().first().copied())
    }
    pub fn focused_tab(&self) -> Option<&TabRec> {
        self.focused_workspace().and_then(|w| self.tab(&w.active_tab))
    }
    pub fn focused_pane(&self) -> Option<&PaneRecord> {
        self.focused_tab().and_then(|t| self.pane(&t.focused_pane))
    }
    pub fn is_focused_pane(&self, id: &str) -> bool {
        self.focused_pane().is_some_and(|p| p.pane.id == id)
    }

    // ── protocol projection ──

    pub fn protocol_workspaces(&self) -> Vec<Workspace> {
        self.workspaces_sorted()
            .into_iter()
            .map(|w| Workspace {
                id: w.id.clone(),
                name: w.name.clone(),
                cwd: w.cwd.clone(),
                position: w.position,
                active_tab: Some(w.active_tab.clone()).filter(|t| !t.is_empty()),
            })
            .collect()
    }
    pub fn protocol_tabs(&self) -> Vec<Tab> {
        let mut v: Vec<&TabRec> = self.tabs.iter().collect();
        v.sort_by(|a, b| (a.workspace_id.as_str(), a.position).cmp(&(b.workspace_id.as_str(), b.position)));
        v.into_iter()
            .map(|t| Tab {
                id: t.id.clone(),
                workspace_id: t.workspace_id.clone(),
                name: t.name.clone(),
                position: t.position,
                layout: t.layout.clone(),
                focused_pane: Some(t.focused_pane.clone()).filter(|p| !p.is_empty()),
            })
            .collect()
    }

    // ── creation ──

    #[allow(clippy::too_many_arguments)]
    fn new_pane_record(
        &mut self,
        ws: &str,
        tab: &str,
        cwd: &str,
        argv: Vec<String>,
        env: BTreeMap<String, String>,
        cols: u16,
        rows: u16,
    ) -> String {
        let w = self.workspace_mut(ws).expect("workspace exists");
        let id = format!("{}:p{}", w.id, w.next_pane);
        w.next_pane += 1;
        let title = argv
            .first()
            .map(|a| a.rsplit('/').next().unwrap_or(a).to_string())
            .unwrap_or_default();
        self.panes.push(PaneRecord {
            pane: Pane {
                id: id.clone(),
                tab_id: tab.to_string(),
                cwd: cwd.to_string(),
                command: argv.join(" "),
                title,
                cols: cols.max(2),
                rows: rows.max(2),
                exited: None,
                agent: None,
                label: None,
            },
            argv,
            gen: 1,
            base: 0,
            high_water: 0,
            env,
            meta: AgentMeta::default(),
        });
        id
    }

    /// New workspace with one tab and one pane. Returns (workspace, tab, pane).
    pub fn create_workspace(
        &mut self,
        cwd: &str,
        label: Option<String>,
        argv: Vec<String>,
        env: BTreeMap<String, String>,
    ) -> (String, String, String) {
        let n = self.next_ws;
        self.next_ws += 1;
        let ws = format!("w{n}");
        let position = self.workspaces.iter().map(|w| w.position + 1).max().unwrap_or(0);
        self.workspaces.push(WorkspaceRec {
            id: ws.clone(),
            name: clean_label(label).unwrap_or_else(|| default_workspace_name(&ws)),
            cwd: cwd.into(),
            position,
            active_tab: String::new(),
            next_tab: 1,
            next_pane: 1,
            worktree: None,
        });
        let (tab, pane) = self
            .create_tab(&ws, cwd, None, argv, env)
            .expect("workspace just created");
        if self.focused_workspace.is_none() {
            self.focused_workspace = Some(ws.clone());
        }
        (ws, tab, pane)
    }

    pub fn create_tab(
        &mut self,
        ws: &str,
        cwd: &str,
        label: Option<String>,
        argv: Vec<String>,
        env: BTreeMap<String, String>,
    ) -> TResult<(String, String)> {
        let w = self
            .workspace_mut(ws)
            .ok_or_else(|| not_found("workspace_not_found", ws))?;
        let tab = format!("{}:t{}", w.id, w.next_tab);
        let number = w.next_tab;
        w.next_tab += 1;
        if w.active_tab.is_empty() {
            w.active_tab = tab.clone();
        }
        let position = self
            .tabs
            .iter()
            .filter(|t| t.workspace_id == ws)
            .map(|t| t.position + 1)
            .max()
            .unwrap_or(0);
        let (cols, rows) = self.area;
        let pane = self.new_pane_record(ws, &tab, cwd, argv, env, cols, rows);
        self.tabs.push(TabRec {
            id: tab.clone(),
            workspace_id: ws.into(),
            name: clean_label(label).unwrap_or_else(|| number.to_string()),
            position,
            layout: Layout::Pane { pane: pane.clone() },
            focused_pane: pane.clone(),
            zoomed: false,
            pattern: None,
        });
        Ok((tab, pane))
    }

    /// Split `target` (its tab) in `dir`; the new pane is `b`. Returns the new pane id.
    pub fn split(
        &mut self,
        target: &str,
        dir: SplitDir,
        ratio: Option<f64>,
        cwd: Option<&str>,
        argv: Vec<String>,
        env: BTreeMap<String, String>,
    ) -> TResult<String> {
        let tp = self
            .pane(target)
            .ok_or_else(|| not_found("pane_not_found", target))?
            .clone();
        let (ws, tab) = self
            .workspace_of(target)
            .ok_or_else(|| not_found("pane_not_found", target))?;
        let ratio = ratio.unwrap_or(0.5).clamp(MIN_RATIO, 1.0 - MIN_RATIO);
        let (cols, rows) = match dir {
            SplitDir::H => (((tp.pane.cols as f64) * (1.0 - ratio)) as u16, tp.pane.rows),
            SplitDir::V => (tp.pane.cols, ((tp.pane.rows as f64) * (1.0 - ratio)) as u16),
        };
        let cwd = cwd.map(str::to_owned).unwrap_or(tp.pane.cwd.clone());
        let new = self.new_pane_record(&ws, &tab, &cwd, argv, env, cols, rows);
        let t = self.tab_mut(&tab).expect("tab exists");
        let leaf = Layout::Pane { pane: new.clone() };
        replace_leaf(&mut t.layout, target, &mut |old| Layout::Split {
            dir,
            ratio,
            a: Box::new(old),
            b: Box::new(leaf.clone()),
        });
        t.zoomed = false;
        Ok(new)
    }

    /// Remove a pane from its tab (collapsing the split); empty tabs and
    /// workspaces close too. The pane record is dropped.
    pub fn close_pane(&mut self, id: &str) -> TResult<Closed> {
        let (ws, tab) = self.workspace_of(id).ok_or_else(|| not_found("pane_not_found", id))?;
        let mut closed = Closed {
            panes: vec![id.to_string()],
            ..Default::default()
        };
        self.detach_pane_from_tab(id, &tab, &ws, &mut closed);
        self.panes.retain(|p| p.pane.id != id);
        Ok(closed)
    }

    fn detach_pane_from_tab(&mut self, id: &str, tab: &str, ws: &str, closed: &mut Closed) {
        let t = self.tab_mut(tab).expect("tab exists");
        let layout = std::mem::replace(&mut t.layout, Layout::Pane { pane: String::new() });
        match remove_leaf(layout, id) {
            Some(rest) => {
                t.layout = rest;
                if t.focused_pane == id {
                    let mut ps = vec![];
                    layout_panes(&t.layout, &mut ps);
                    t.focused_pane = ps.first().cloned().unwrap_or_default();
                }
                t.zoomed = false;
            }
            None => {
                self.tabs.retain(|t| t.id != tab);
                closed.tabs.push((tab.to_string(), ws.to_string()));
                self.renumber_tabs(ws);
                let remaining = self.tabs_of(ws).first().map(|t| t.id.clone());
                match remaining {
                    Some(next) => {
                        let w = self.workspace_mut(ws).expect("workspace exists");
                        if w.active_tab == tab {
                            w.active_tab = next;
                        }
                    }
                    None => {
                        self.workspaces.retain(|w| w.id != ws);
                        closed.workspaces.push(ws.to_string());
                        self.renumber_workspaces();
                        if self.focused_workspace.as_deref() == Some(ws) {
                            self.focused_workspace = self.workspaces_sorted().first().map(|w| w.id.clone());
                        }
                    }
                }
            }
        }
    }

    pub fn close_tab(&mut self, tab: &str) -> TResult<Closed> {
        let panes = self.panes_of_tab(tab);
        if self.tab(tab).is_none() {
            return Err(not_found("tab_not_found", tab));
        }
        let mut all = Closed::default();
        for p in panes {
            let c = self.close_pane(&p)?;
            all.panes.extend(c.panes);
            all.tabs.extend(c.tabs);
            all.workspaces.extend(c.workspaces);
        }
        Ok(all)
    }

    pub fn close_workspace(&mut self, ws: &str) -> TResult<Closed> {
        if self.workspace(ws).is_none() {
            return Err(not_found("workspace_not_found", ws));
        }
        let mut all = Closed::default();
        for t in self.tabs_of(ws).iter().map(|t| t.id.clone()).collect::<Vec<_>>() {
            let c = self.close_tab(&t)?;
            all.panes.extend(c.panes);
            all.tabs.extend(c.tabs);
            all.workspaces.extend(c.workspaces);
        }
        Ok(all)
    }

    fn renumber_tabs(&mut self, ws: &str) {
        let mut ids: Vec<(u32, String)> = self
            .tabs
            .iter()
            .filter(|t| t.workspace_id == ws)
            .map(|t| (t.position, t.id.clone()))
            .collect();
        ids.sort();
        for (i, (_, id)) in ids.into_iter().enumerate() {
            if let Some(t) = self.tab_mut(&id) {
                t.position = i as u32;
            }
        }
    }

    fn renumber_workspaces(&mut self) {
        let mut ids: Vec<(u32, String)> = self.workspaces.iter().map(|w| (w.position, w.id.clone())).collect();
        ids.sort();
        for (i, (_, id)) in ids.into_iter().enumerate() {
            if let Some(w) = self.workspace_mut(&id) {
                w.position = i as u32;
            }
        }
    }

    // ── focus, rename, move ──

    pub fn focus_workspace(&mut self, ws: &str) -> TResult<bool> {
        self.workspace(ws).ok_or_else(|| not_found("workspace_not_found", ws))?;
        let changed = self.focused_workspace.as_deref() != Some(ws);
        self.focused_workspace = Some(ws.into());
        Ok(changed)
    }

    pub fn focus_tab(&mut self, tab: &str) -> TResult<bool> {
        let ws = self
            .tab(tab)
            .ok_or_else(|| not_found("tab_not_found", tab))?
            .workspace_id
            .clone();
        let w = self.workspace_mut(&ws).expect("workspace exists");
        let changed = w.active_tab != tab;
        w.active_tab = tab.into();
        self.focused_workspace = Some(ws);
        Ok(changed)
    }

    pub fn focus_pane(&mut self, id: &str) -> TResult<bool> {
        let (_, tab) = self.workspace_of(id).ok_or_else(|| not_found("pane_not_found", id))?;
        let was = self.is_focused_pane(id);
        self.focus_tab(&tab)?;
        self.tab_mut(&tab).expect("tab").focused_pane = id.into();
        Ok(!was)
    }

    pub fn move_workspace(&mut self, ws: &str, insert_index: usize) -> TResult<()> {
        self.workspace(ws).ok_or_else(|| not_found("workspace_not_found", ws))?;
        let mut order: Vec<String> = self.workspaces_sorted().iter().map(|w| w.id.clone()).collect();
        order.retain(|w| w != ws);
        order.insert(insert_index.min(order.len()), ws.into());
        for (i, id) in order.iter().enumerate() {
            self.workspace_mut(id).expect("ws").position = i as u32;
        }
        Ok(())
    }

    pub fn move_tab(&mut self, tab: &str, insert_index: usize) -> TResult<()> {
        let ws = self
            .tab(tab)
            .ok_or_else(|| not_found("tab_not_found", tab))?
            .workspace_id
            .clone();
        let mut order: Vec<String> = self.tabs_of(&ws).iter().map(|t| t.id.clone()).collect();
        order.retain(|t| t != tab);
        order.insert(insert_index.min(order.len()), tab.into());
        for (i, id) in order.iter().enumerate() {
            self.tab_mut(id).expect("tab").position = i as u32;
        }
        Ok(())
    }

    // ── zoom, swap, move pane, ratios ──

    /// Returns (changed, zoomed, reason).
    pub fn zoom(&mut self, id: &str, mode: &str) -> TResult<(bool, bool, Option<&'static str>)> {
        let (_, tab) = self.workspace_of(id).ok_or_else(|| not_found("pane_not_found", id))?;
        let single = self.panes_of_tab(&tab).len() == 1;
        let t = self.tab_mut(&tab).expect("tab");
        let target = match mode {
            "on" => true,
            "off" => false,
            _ => !(t.zoomed && t.focused_pane == id),
        };
        if single && target {
            return Ok((false, false, Some("single_pane")));
        }
        if target == t.zoomed && (!target || t.focused_pane == id) {
            return Ok((
                false,
                t.zoomed,
                Some(if target { "already_zoomed" } else { "already_unzoomed" }),
            ));
        }
        t.zoomed = target;
        if target {
            t.focused_pane = id.into();
        }
        Ok((true, target, None))
    }

    /// Swap two panes' places. Same tab only (`cross_tab` is refused).
    pub fn swap(&mut self, a: &str, b: &str) -> TResult<Result<(), &'static str>> {
        if a == b {
            return Ok(Err("same_pane"));
        }
        let (_, ta) = self.workspace_of(a).ok_or_else(|| not_found("pane_not_found", a))?;
        let (_, tb) = match self.workspace_of(b) {
            Some(x) => x,
            None => return Ok(Err("not_found")),
        };
        if ta != tb {
            return Ok(Err("cross_tab"));
        }
        let t = self.tab_mut(&ta).expect("tab");
        rename_leaf(&mut t.layout, a, "\u{0}swap");
        rename_leaf(&mut t.layout, b, a);
        rename_leaf(&mut t.layout, "\u{0}swap", b);
        Ok(Ok(()))
    }

    /// The pane next to `id` in direction `d`, by rectangles in the tab's area.
    pub fn neighbor(&self, id: &str, d: Dir) -> Option<String> {
        let (_, tab) = self.workspace_of(id)?;
        let (panes, _) = self.tab_rects(&tab);
        let me = panes.iter().find(|(p, _)| p == id)?.1;
        let overlap = |a0: u16, a1: u16, b0: u16, b1: u16| a1.min(b1) as i32 - a0.max(b0) as i32;
        panes
            .iter()
            .filter(|(p, _)| p != id)
            .filter_map(|(p, r)| {
                let ok = match d {
                    Dir::Left => r.x + r.w == me.x,
                    Dir::Right => me.x + me.w == r.x,
                    Dir::Up => r.y + r.h == me.y,
                    Dir::Down => me.y + me.h == r.y,
                };
                let ov = match d {
                    Dir::Left | Dir::Right => overlap(me.y, me.y + me.h, r.y, r.y + r.h),
                    Dir::Up | Dir::Down => overlap(me.x, me.x + me.w, r.x, r.x + r.w),
                };
                (ok && ov > 0).then(|| (ov, p.clone()))
            })
            .max_by_key(|(ov, _)| *ov)
            .map(|(_, p)| p)
    }

    pub fn tab_rects(&self, tab: &str) -> (Vec<PaneRect>, Vec<SplitRect>) {
        let mut panes = vec![];
        let mut splits = vec![];
        if let Some(t) = self.tab(tab) {
            let area = Rect {
                x: 0,
                y: 0,
                w: self.area.0,
                h: self.area.1,
            };
            if t.zoomed {
                panes.push((t.focused_pane.clone(), area));
            } else {
                rects(&t.layout, area, &mut panes, &mut splits, "");
            }
        }
        (panes, splits)
    }

    pub fn set_split_ratio(&mut self, tab: &str, path: &[bool], ratio: f64) -> TResult<()> {
        let t = self.tab_mut(tab).ok_or_else(|| not_found("tab_not_found", tab))?;
        match split_at_path(&mut t.layout, path) {
            Some(Layout::Split { ratio: r, .. }) => {
                *r = ratio.clamp(MIN_RATIO, 1.0 - MIN_RATIO);
                Ok(())
            }
            _ => Err(TopoError("split_not_found", "no split at that path".into())),
        }
    }

    /// Grow `id` towards `d` by `amount` (fraction of the split, default 0.05). Returns whether anything changed.
    pub fn resize_toward(&mut self, id: &str, d: Dir, amount: Option<f64>) -> TResult<bool> {
        let (_, tab) = self.workspace_of(id).ok_or_else(|| not_found("pane_not_found", id))?;
        let axis = match d {
            Dir::Left | Dir::Right => SplitDir::H,
            Dir::Up | Dir::Down => SplitDir::V,
        };
        let t = self.tab_mut(&tab).expect("tab");
        let Some((path, _in_a)) = ancestor_split(&t.layout, id, axis, &mut vec![]) else {
            return Ok(false);
        };
        let amount = amount.unwrap_or(0.05).abs();
        let amount = if amount > 1.0 { amount / 100.0 } else { amount };
        if let Some(Layout::Split { ratio, .. }) = split_at_path(&mut t.layout, &path) {
            // The divider moves in direction `d` (right/down grows `a`).
            let grow_a = matches!(d, Dir::Right | Dir::Down);
            let new = (*ratio + if grow_a { amount } else { -amount }).clamp(MIN_RATIO, 1.0 - MIN_RATIO);
            let changed = (new - *ratio).abs() > f64::EPSILON;
            *ratio = new;
            return Ok(changed);
        }
        Ok(false)
    }

    /// Move a pane next to `target` in another tab (split `dir`), or into a
    /// new tab / workspace. Returns what closed as a side effect.
    pub fn move_pane_to_tab(
        &mut self,
        id: &str,
        tab: &str,
        target: Option<&str>,
        dir: SplitDir,
        ratio: Option<f64>,
    ) -> TResult<Closed> {
        let (ws, from_tab) = self.workspace_of(id).ok_or_else(|| not_found("pane_not_found", id))?;
        let dest = self.tab(tab).ok_or_else(|| not_found("tab_not_found", tab))?.clone();
        if from_tab == tab {
            return Err(TopoError("same_tab", "the pane is already in that tab".into()));
        }
        let target = match target {
            Some(t) if contains(&dest.layout, t) => t.to_string(),
            Some(t) => return Err(not_found("pane_not_found", t)),
            None => dest.focused_pane.clone(),
        };
        let mut closed = Closed::default();
        self.detach_pane_from_tab(id, &from_tab, &ws, &mut closed);
        let ratio = ratio.unwrap_or(0.5).clamp(MIN_RATIO, 1.0 - MIN_RATIO);
        let t = self.tab_mut(tab).expect("tab");
        let leaf = Layout::Pane { pane: id.into() };
        replace_leaf(&mut t.layout, &target, &mut |old| Layout::Split {
            dir,
            ratio,
            a: Box::new(old),
            b: Box::new(leaf.clone()),
        });
        t.zoomed = false;
        self.pane_mut(id).expect("pane").pane.tab_id = tab.into();
        Ok(closed)
    }

    pub fn move_pane_to_new_tab(
        &mut self,
        id: &str,
        ws: Option<&str>,
        label: Option<String>,
    ) -> TResult<(String, Closed)> {
        let (from_ws, from_tab) = self.workspace_of(id).ok_or_else(|| not_found("pane_not_found", id))?;
        let ws = ws.map(str::to_owned).unwrap_or(from_ws.clone());
        let w = self
            .workspace_mut(&ws)
            .ok_or_else(|| not_found("workspace_not_found", &ws))?;
        let tab = format!("{}:t{}", w.id, w.next_tab);
        let number = w.next_tab;
        w.next_tab += 1;
        let mut closed = Closed::default();
        self.detach_pane_from_tab(id, &from_tab, &from_ws, &mut closed);
        if closed.workspaces.contains(&ws) {
            return Err(TopoError("workspace_not_found", format!("workspace {ws} closed")));
        }
        let position = self
            .tabs
            .iter()
            .filter(|t| t.workspace_id == ws)
            .map(|t| t.position + 1)
            .max()
            .unwrap_or(0);
        self.tabs.push(TabRec {
            id: tab.clone(),
            workspace_id: ws.clone(),
            name: clean_label(label).unwrap_or_else(|| number.to_string()),
            position,
            layout: Layout::Pane { pane: id.into() },
            focused_pane: id.into(),
            zoomed: false,
            pattern: None,
        });
        self.pane_mut(id).expect("pane").pane.tab_id = tab.clone();
        Ok((tab, closed))
    }

    pub fn move_pane_to_new_workspace(
        &mut self,
        id: &str,
        label: Option<String>,
        tab_label: Option<String>,
    ) -> TResult<(String, String, Closed)> {
        let cwd = self
            .pane(id)
            .ok_or_else(|| not_found("pane_not_found", id))?
            .pane
            .cwd
            .clone();
        let n = self.next_ws;
        self.next_ws += 1;
        let ws = format!("w{n}");
        let position = self.workspaces.iter().map(|w| w.position + 1).max().unwrap_or(0);
        self.workspaces.push(WorkspaceRec {
            id: ws.clone(),
            name: clean_label(label).unwrap_or_else(|| n.to_string()),
            cwd,
            position,
            active_tab: String::new(),
            next_tab: 1,
            next_pane: 1,
            worktree: None,
        });
        let (tab, closed) = self.move_pane_to_new_tab(id, Some(&ws), tab_label)?;
        self.workspace_mut(&ws).expect("ws").active_tab = tab.clone();
        Ok((ws, tab, closed))
    }

    /// Start a new incarnation for `pane_id`: returns (gen, base).
    pub fn next_incarnation(&mut self, pane_id: &str) -> Option<(u64, u64)> {
        let p = self.pane_mut(pane_id)?;
        p.gen += 1;
        // A new process: the old one's helpers went with it.
        p.meta.helpers.clear();
        p.base = p.high_water.max(p.base) + INCARNATION_GAP;
        p.high_water = p.base;
        Some((p.gen, p.base))
    }

    /// A stable fingerprint of a tab's layout (pane ids and split shape), for history replay.
    /// Replace `tab`'s tree with `layout` without closing a pane.
    ///
    /// Every pane of the tab must be in `layout` or in `extras`, else
    /// `would_close`. `extras` move to a new tab laid out as `extras_layout`.
    /// A leaf from another tab of the same workspace moves in (a tab left
    /// empty closes). Everything is checked before anything changes: on an
    /// error the state is as it was.
    pub fn arrange_tab(
        &mut self,
        tab: &str,
        layout: Layout,
        extras: &[String],
        extras_layout: Option<Layout>,
        focus: Option<&str>,
        pattern: Option<&str>,
    ) -> TResult<Arranged> {
        let bad = |m: &str| TopoError("invalid_params", m.to_string());
        let t = self.tab(tab).ok_or_else(|| not_found("tab_not_found", tab))?;
        let ws = t.workspace_id.clone();
        let mut current = vec![];
        layout_panes(&t.layout, &mut current);
        let mut leaves = vec![];
        layout_panes(&layout, &mut leaves);
        let mut seen = std::collections::BTreeSet::new();
        for id in &leaves {
            if !seen.insert(id.as_str()) {
                return Err(bad("the layout names a pane twice"));
            }
            match self.workspace_of(id) {
                None => return Err(not_found("pane_not_found", id)),
                Some((w, _)) if w != ws => {
                    return Err(TopoError(
                        "cross_workspace",
                        format!("pane {id} is in another workspace"),
                    ))
                }
                Some(_) => {}
            }
        }
        for id in extras {
            if !current.contains(id) || !seen.insert(id.as_str()) {
                return Err(bad("extras must be panes of the tab that the layout leaves out"));
            }
        }
        let left_out: Vec<&str> = current
            .iter()
            .map(String::as_str)
            .filter(|id| !seen.contains(id))
            .collect();
        if !left_out.is_empty() {
            return Err(TopoError(
                "would_close",
                format!("the layout leaves out {}", left_out.join(", ")),
            ));
        }
        let mut in_extras_layout = vec![];
        if let Some(l) = &extras_layout {
            layout_panes(l, &mut in_extras_layout);
        }
        let sorted = |v: &[String]| {
            let mut v = v.to_vec();
            v.sort();
            v
        };
        if sorted(&in_extras_layout) != sorted(extras) {
            return Err(bad("the extras layout must name each extra once"));
        }
        if focus.is_some_and(|f| !leaves.iter().any(|l| l == f)) {
            return Err(bad("focus must be a pane of the layout"));
        }
        if self.workspace(&ws).is_none() {
            return Err(not_found("workspace_not_found", &ws));
        }

        // Checked: from here nothing fails.
        let mut out = Arranged::default();
        for id in leaves.iter().filter(|id| !current.contains(id)) {
            let from = self.pane(id).expect("checked").pane.tab_id.clone();
            self.detach_pane_from_tab(id, &from, &ws, &mut out.closed);
            self.pane_mut(id).expect("checked").pane.tab_id = tab.to_string();
            if !out.touched.contains(&from) {
                out.touched.push(from);
            }
        }
        out.touched.retain(|t| self.tab(t).is_some());
        let t = self.tab_mut(tab).expect("checked");
        t.layout = layout;
        t.zoomed = false;
        t.pattern = pattern.map(str::to_owned);
        match focus {
            Some(f) => t.focused_pane = f.to_string(),
            None if !leaves.contains(&t.focused_pane) => t.focused_pane = leaves[0].clone(),
            None => {}
        }
        if let Some(extras_layout) = extras_layout {
            let w = self.workspace_mut(&ws).expect("checked");
            let id = format!("{}:t{}", w.id, w.next_tab);
            w.next_tab += 1;
            let position = self
                .tabs
                .iter()
                .filter(|t| t.workspace_id == ws)
                .map(|t| t.position + 1)
                .max()
                .unwrap_or(0);
            self.tabs.push(TabRec {
                id: id.clone(),
                workspace_id: ws.clone(),
                name: "Extras".into(),
                position,
                layout: extras_layout,
                focused_pane: extras[0].clone(),
                zoomed: false,
                pattern: None,
            });
            for e in extras {
                self.pane_mut(e).expect("checked").pane.tab_id = id.clone();
            }
            out.extras_tab = Some(id);
        }
        Ok(out)
    }

    pub fn layout_fingerprint(&self, tab: &str) -> String {
        fn walk(l: &Layout, out: &mut String) {
            match l {
                Layout::Pane { pane } => out.push_str(pane),
                Layout::Split { dir, a, b, .. } => {
                    out.push_str(if *dir == SplitDir::H { "(h " } else { "(v " });
                    walk(a, out);
                    out.push(' ');
                    walk(b, out);
                    out.push(')');
                }
            }
        }
        let mut s = String::new();
        if let Some(t) = self.tab(tab) {
            walk(&t.layout, &mut s);
        }
        s
    }
}

/// A tab as a `LayoutNode` (runner API).
pub fn export_layout(s: &State, l: &Layout) -> Value {
    match l {
        Layout::Pane { pane } => {
            let p = s.pane(pane);
            json!({
                "type": "pane",
                "pane_id": pane,
                "cwd": p.map(|p| p.pane.cwd.clone()),
                "label": p.and_then(|p| p.pane.label.clone()),
                "command": p.map(|p| p.argv.clone()),
            })
        }
        Layout::Split { dir, ratio, a, b } => json!({
            "type": "split",
            "direction": split_dir_name(*dir),
            "ratio": ratio,
            "first": export_layout(s, a),
            "second": export_layout(s, b),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh() -> Vec<String> {
        vec!["sh".into()]
    }

    fn st() -> State {
        State::default_topology("/home/u", sh(), 120, 40)
    }

    #[test]
    fn a_pane_label_from_an_older_state_file_is_kept_and_sent_in_the_topology() {
        // Before queue 3c the label was PaneRecord's own field, under the same key.
        let mut s = st();
        let mut old = serde_json::to_value(&s.panes[0]).unwrap();
        old.as_object_mut().unwrap().remove("label");
        old["label"] = json!("build");
        let rec: PaneRecord = serde_json::from_value(old).unwrap();
        assert_eq!(rec.pane.label.as_deref(), Some("build"));
        s.panes[0] = rec;
        // Round trip: written once, under `label`, and in the protocol pane.
        let v = serde_json::to_value(&s.panes[0]).unwrap();
        assert_eq!(v["label"], "build");
        let back: PaneRecord = serde_json::from_value(v).unwrap();
        assert_eq!(back, s.panes[0]);
        let wire = serde_json::to_value(&s.panes[0].pane).unwrap();
        assert_eq!(wire["label"], "build");
        // No label: no key on the wire (older gateways and web see the same shape as before).
        s.panes[0].pane.label = None;
        assert!(serde_json::to_value(&s.panes[0].pane).unwrap().get("label").is_none());
    }

    #[test]
    fn names_given_at_creation_are_cleaned_and_empty_means_the_default() {
        let mut s = st();
        let (ws, tab, _) = s.create_workspace(
            "/tmp",
            Some("  api\u{1b}\t server \u{202e}".into()),
            sh(),
            BTreeMap::new(),
        );
        assert_eq!(s.workspace(&ws).unwrap().name, "api server");
        assert_eq!(s.tab(&tab).unwrap().name, "1");
        let (ws3, _, _) = s.create_workspace("/tmp", Some(" \u{7} ".into()), sh(), BTreeMap::new());
        assert_eq!(s.workspace(&ws3).unwrap().name, "3");
        let (t2, _) = s
            .create_tab(&ws, "/tmp", Some("x".repeat(80)), sh(), BTreeMap::new())
            .unwrap();
        assert_eq!(s.tab(&t2).unwrap().name, "x".repeat(64));
        let (t3, _) = s
            .create_tab(&ws, "/tmp", Some("\n".into()), sh(), BTreeMap::new())
            .unwrap();
        assert_eq!(s.tab(&t3).unwrap().name, "3");
        assert_eq!(clean_label(Some("  ".into())), None);
        assert_eq!(clean_label(None), None);
    }

    #[test]
    fn ids_are_numbered_and_never_reused() {
        let mut s = st();
        assert_eq!(s.panes[0].pane.id, "w1:p1");
        assert_eq!(s.tabs[0].id, "w1:t1");
        let p2 = s
            .split("w1:p1", SplitDir::H, None, None, sh(), BTreeMap::new())
            .unwrap();
        assert_eq!(p2, "w1:p2");
        s.close_pane(&p2).unwrap();
        let p3 = s
            .split("w1:p1", SplitDir::V, Some(0.3), None, sh(), BTreeMap::new())
            .unwrap();
        assert_eq!(p3, "w1:p3");
        let (ws, tab, pane) = s.create_workspace("/tmp", Some("two".into()), sh(), BTreeMap::new());
        assert_eq!((ws.as_str(), tab.as_str(), pane.as_str()), ("w2", "w2:t1", "w2:p1"));
        let (t2, p) = s.create_tab("w2", "/tmp", None, sh(), BTreeMap::new()).unwrap();
        assert_eq!((t2.as_str(), p.as_str()), ("w2:t2", "w2:p2"));
    }

    #[test]
    fn split_close_collapses_and_cascades() {
        let mut s = st();
        let p2 = s
            .split("w1:p1", SplitDir::H, Some(0.25), None, sh(), BTreeMap::new())
            .unwrap();
        assert_eq!(
            s.tabs[0].layout,
            Layout::Split {
                dir: SplitDir::H,
                ratio: 0.25,
                a: Box::new(Layout::Pane { pane: "w1:p1".into() }),
                b: Box::new(Layout::Pane { pane: p2.clone() })
            }
        );
        let (panes, splits) = s.tab_rects("w1:t1");
        assert_eq!(
            panes[0].1,
            Rect {
                x: 0,
                y: 0,
                w: 30,
                h: 40
            }
        );
        assert_eq!(
            panes[1].1,
            Rect {
                x: 30,
                y: 0,
                w: 90,
                h: 40
            }
        );
        assert_eq!(splits.len(), 1);
        assert_eq!(s.neighbor("w1:p1", Dir::Right), Some(p2.clone()));
        assert_eq!(s.neighbor("w1:p1", Dir::Left), None);
        let c = s.close_pane("w1:p1").unwrap();
        assert_eq!(c.panes, vec!["w1:p1"]);
        assert!(c.tabs.is_empty());
        assert_eq!(s.tabs[0].layout, Layout::Pane { pane: p2.clone() });
        assert_eq!(s.tabs[0].focused_pane, p2);
        // Closing the last pane closes the tab and the workspace.
        let (ws, _, _) = s.create_workspace("/", None, sh(), BTreeMap::new());
        let c = s.close_pane(&p2).unwrap();
        assert_eq!(c.tabs, vec![("w1:t1".to_string(), "w1".to_string())]);
        assert_eq!(c.workspaces, vec!["w1"]);
        assert_eq!(s.focused_workspace().unwrap().id, ws);
        assert_eq!(s.workspaces[0].position, 0);
    }

    #[test]
    fn zoom_swap_ratio_resize_move() {
        let mut s = st();
        assert_eq!(s.zoom("w1:p1", "toggle").unwrap(), (false, false, Some("single_pane")));
        let p2 = s
            .split("w1:p1", SplitDir::V, None, None, sh(), BTreeMap::new())
            .unwrap();
        assert_eq!(s.zoom(&p2, "toggle").unwrap(), (true, true, None));
        assert_eq!(s.tab_rects("w1:t1").0.len(), 1);
        assert_eq!(s.zoom(&p2, "on").unwrap().2, Some("already_zoomed"));
        assert_eq!(s.zoom(&p2, "off").unwrap(), (true, false, None));
        s.swap("w1:p1", &p2).unwrap().unwrap();
        let mut order = vec![];
        layout_panes(&s.tabs[0].layout, &mut order);
        assert_eq!(order, vec![p2.clone(), "w1:p1".to_string()]);
        s.set_split_ratio("w1:t1", &[], 0.7).unwrap();
        assert!(s.set_split_ratio("w1:t1", &[true], 0.7).is_err());
        assert!(s.resize_toward("w1:p1", Dir::Down, Some(0.1)).unwrap());
        assert!(matches!(s.tabs[0].layout, Layout::Split { ratio, .. } if (ratio - 0.8).abs() < 1e-9));
        assert!(
            !s.resize_toward("w1:p1", Dir::Left, None).unwrap(),
            "no horizontal split to move"
        );
        // Move into a new tab, then into another workspace's tab.
        let (t2, closed) = s.move_pane_to_new_tab(&p2, None, Some("moved".into())).unwrap();
        assert!(closed.tabs.is_empty());
        assert_eq!(s.pane(&p2).unwrap().pane.tab_id, t2);
        let (ws2, t3, _) = s.create_workspace("/", None, sh(), BTreeMap::new());
        let c = s.move_pane_to_tab(&p2, &t3, None, SplitDir::H, None).unwrap();
        assert_eq!(c.tabs, vec![(t2.clone(), "w1".to_string())]);
        assert_eq!(s.workspace_of(&p2).unwrap(), (ws2, t3));
        assert_eq!(s.swap("w1:p1", &p2).unwrap(), Err("cross_tab"));
    }

    #[test]
    fn arrange_never_closes_a_pane_and_changes_nothing_when_it_refuses() {
        use crate::arrange::{extras_layout, leaves, plan};
        let mut s = st();
        for _ in 0..4 {
            s.split("w1:p1", SplitDir::H, None, None, sh(), BTreeMap::new())
                .unwrap();
        }
        let (other, _) = s.create_tab("w1", "/tmp", None, sh(), BTreeMap::new()).unwrap();
        let before = s.clone();
        let panes = |s: &State| {
            let mut v: Vec<String> = s.panes.iter().map(|p| p.pane.id.clone()).collect();
            v.sort();
            v
        };
        let pane = |id: &str| Layout::Pane { pane: id.into() };
        let two = |a: &str, b: &str| Layout::Split {
            dir: SplitDir::V,
            ratio: 0.5,
            a: Box::new(pane(a)),
            b: Box::new(pane(b)),
        };

        // Refusals: a pane left out, a pane twice, an unknown pane, a bad focus,
        // extras that are not the left-out panes. Nothing changes.
        let refused = [
            (two("w1:p1", "w1:p2"), vec![], None, None, "would_close"),
            (two("w1:p1", "w1:p1"), vec![], None, None, "invalid_params"),
            (two("w1:p1", "w1:p99"), vec![], None, None, "pane_not_found"),
            (
                pane("w1:p1"),
                vec!["w1:p2".to_string()],
                Some(pane("w1:p2")),
                None,
                "would_close",
            ),
            (
                pane("w1:p1"),
                vec!["w1:p6".to_string()],
                Some(pane("w1:p6")),
                None,
                "invalid_params",
            ),
        ];
        for (layout, extras, xl, focus, code) in refused {
            let e = s.arrange_tab("w1:t1", layout, &extras, xl, focus, None).unwrap_err();
            assert_eq!(e.0, code, "{e:?}");
            assert_eq!(s, before);
        }
        let all = s.tab("w1:t1").unwrap().layout.clone();
        let e = s
            .arrange_tab("w1:t1", all.clone(), &[], None, Some("w1:p6"), None)
            .unwrap_err();
        assert_eq!(e.0, "invalid_params");
        let e = s
            .arrange_tab("w1:t1", all.clone(), &[], Some(pane("w1:p2")), None, None)
            .unwrap_err();
        assert_eq!(e.0, "invalid_params");
        assert_eq!(
            s.arrange_tab("w1:t9", all, &[], None, None, None).unwrap_err().0,
            "tab_not_found"
        );
        assert_eq!(s, before);

        // A pattern with fewer slots: the extras go to a new tab, in order.
        let t = s.tab("w1:t1").unwrap().clone();
        let p = plan("grid_2x2", &t.layout, &t.focused_pane, s.area);
        assert_eq!(p.extras.len(), 1);
        let xl = extras_layout(&p.extras, s.area);
        let a = s
            .arrange_tab("w1:t1", p.layout.clone(), &p.extras, xl, None, Some(p.pattern))
            .unwrap();
        let extras_tab = a.extras_tab.clone().unwrap();
        assert_eq!(extras_tab, "w1:t3");
        assert_eq!(panes(&s), panes(&before), "no pane closed, none opened");
        for (now, was) in s.panes.iter().zip(&before.panes) {
            assert_eq!((now.gen, now.holder_id()), (was.gen, was.holder_id()), "same process");
        }
        assert_eq!(s.tab("w1:t1").unwrap().layout, p.layout);
        assert_eq!(s.tab("w1:t1").unwrap().pattern.as_deref(), Some("grid_2x2"));
        assert_eq!(s.tab("w1:t1").unwrap().focused_pane, t.focused_pane);
        assert_eq!(s.panes_of_tab(&extras_tab), p.extras);
        assert_eq!(s.pane(&p.extras[0]).unwrap().pane.tab_id, extras_tab);
        assert_eq!(s.tab(&extras_tab).unwrap().name, "Extras");
        assert!(a.closed.tabs.is_empty() && a.touched.is_empty());

        // Undo: the old tree names the pane now in the extras tab. It moves
        // back and the emptied tab closes; the other tab is untouched.
        let a = s.arrange_tab("w1:t1", t.layout.clone(), &[], None, None, None).unwrap();
        assert_eq!(a.closed.tabs, vec![(extras_tab, "w1".to_string())]);
        assert!(a.closed.panes.is_empty() && a.closed.workspaces.is_empty());
        assert_eq!(s.tab("w1:t1").unwrap().layout, t.layout);
        assert_eq!(panes(&s), panes(&before));
        assert!(s.panes.iter().all(|p| s.tab(&p.pane.tab_id).is_some()));
        assert_eq!(leaves(&s.tab(&other).unwrap().layout).len(), 1);

        // A pane of another workspace is refused.
        let (_, _, foreign) = s.create_workspace("/tmp", None, sh(), BTreeMap::new());
        let kept = s.clone();
        let e = s
            .arrange_tab("w1:t1", two("w1:p1", &foreign), &[], None, None, None)
            .unwrap_err();
        assert_eq!(e.0, "cross_workspace");
        assert_eq!(s, kept);
    }

    #[test]
    fn focus_and_ordering() {
        let mut s = st();
        let (ws2, t, p) = s.create_workspace("/", None, sh(), BTreeMap::new());
        assert_eq!(s.focused_pane().unwrap().pane.id, "w1:p1");
        assert!(s.focus_pane(&p).unwrap());
        assert_eq!(s.focused_workspace().unwrap().id, ws2);
        assert_eq!(s.focused_tab().unwrap().id, t);
        s.move_workspace(&ws2, 0).unwrap();
        assert_eq!(s.workspaces_sorted()[0].id, ws2);
        let (t2, _) = s.create_tab("w1", "/", None, sh(), BTreeMap::new()).unwrap();
        s.move_tab(&t2, 0).unwrap();
        assert_eq!(s.tabs_of("w1")[0].id, t2);
        assert_eq!(s.protocol_tabs().len(), 3);
    }

    #[test]
    fn export_and_persist() {
        let mut s = st();
        s.split("w1:p1", SplitDir::H, None, Some("/srv"), sh(), BTreeMap::new())
            .unwrap();
        let e = export_layout(&s, &s.tabs[0].layout);
        assert_eq!(e["type"], "split");
        assert_eq!(e["direction"], "right");
        assert_eq!(e["second"]["cwd"], "/srv");
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("state.json");
        s.save(&path).unwrap();
        assert_eq!(State::load(&path).state().unwrap(), s);
        assert_eq!(s.layout_fingerprint("w1:t1"), "(h w1:p1 w1:p2)");
    }

    #[test]
    fn r000001_state_migrates_and_keeps_counters() {
        let p = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fixtures/state-r000001.json");
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("state.json");
        std::fs::copy(p, &path).unwrap();
        let s = State::load(&path).state().expect("v1 migrates");
        assert_eq!((s.rev, s.event_seq), (5, 3));
        assert_eq!(s.workspaces[0].active_tab, "w1:t1");
        assert_eq!(
            (s.workspaces[0].next_tab, s.workspaces[0].next_pane, s.next_ws),
            (2, 2, 2)
        );
        assert_eq!(s.tabs[0].focused_pane, "w1:p1");
        assert_eq!(s.focused_workspace.as_deref(), Some("w1"));
        assert_eq!(s.panes[0].high_water, 12345);
        assert_eq!(s.panes[0].argv, vec!["/bin/zsh".to_string(), "-l".into()]);
        // Saved as v2; v2 loads unchanged.
        s.save(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"version\": 2"), "{text}");
        assert_eq!(State::load(&path).state().unwrap(), s);
    }

    #[test]
    fn unreadable_state_keeps_counters_and_a_backup() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("state.json");
        std::fs::write(&path, r#"{"version": 99, "rev": 40, "eventSeq": 17, "workspaces": []}"#).unwrap();
        match State::load(&path) {
            Loaded::Unreadable {
                rev,
                event_seq,
                backup,
                why,
            } => {
                assert_eq!((rev, event_seq), (40, 17));
                assert!(why.contains("newer"), "{why}");
                assert!(backup.unwrap().exists());
            }
            other => panic!("{other:?}"),
        }
        std::fs::write(&path, "{ torn").unwrap();
        assert!(matches!(State::load(&path), Loaded::Unreadable { rev: 0, .. }));
        assert!(matches!(State::load(&d.path().join("nope.json")), Loaded::Missing));
    }
}
