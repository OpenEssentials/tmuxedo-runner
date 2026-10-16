//! Arrangement maths for `layout.arrange`: the built-in patterns as templates
//! over the split tree, the order panes take their slots in, fit checks on
//! whole cells, and the strict reader of a caller's tree. Pure: nothing here
//! touches a process or the machine's state.
//!
//! Rules a caller can rely on:
//! - **Slot order** is reading order: panes sorted by the centre of their
//!   current rectangle, top to bottom, then left to right (then by id). In a
//!   pattern with a main slot the focused pane takes it and the others keep
//!   their order.
//! - **Fewer panes than a pattern's slots:** the best pattern for the actual
//!   count is used (`best_for`).
//! - **More panes than slots:** the panes after the last slot, in slot order,
//!   are the extras. They are never closed: the caller moves them to a new tab
//!   or refuses.
//! - **A pattern that would leave a pane under the floor** (`FLOOR`, in whole
//!   cells) is replaced by `agent_wall`; if that does not fit either, panes
//!   leave from the end of the order, one at a time, until it does.

use crate::topology::{layout_panes, rects, Rect, MIN_RATIO};
use serde_json::Value;
use tmuxedo_protocol::frames::{Layout, SplitDir};

/// The smallest pane an arrangement may make: columns, rows.
pub const FLOOR: (u16, u16) = (20, 5);
/// Under this an agent's screen is hard to read; reported, never refused.
pub const READABLE: (u16, u16) = (60, 15);
/// Most panes one tree may name.
pub const MAX_LEAVES: usize = 64;
/// Deepest tree accepted from a caller.
pub const MAX_DEPTH: usize = 32;
/// Longest pane id accepted from a caller.
pub const MAX_ID: usize = 64;
/// Largest area a caller may ask the fit check to use, per side.
pub const MAX_SIDE: u64 = 1000;
/// Columns to rows of the pane `agent_wall` aims for (about 80x30).
const WALL_ASPECT: f64 = 2.7;

/// Every pattern name `layout.arrange` accepts. A closed set.
pub const PATTERNS: &[&str] = &[
    "even",
    "single",
    "side_by_side",
    "stacked",
    "main_side",
    "columns",
    "rows",
    "main_stack",
    "main_top",
    "main_wall",
    "focus_sidebar",
    "2_over_1",
    "3_over_1",
    "1_over_3",
    "3_over_2",
    "2_over_3",
    "4_over_2",
    "4_over_3",
    "grid_2x2",
    "grid_3x2",
    "grid_2x3",
    "grid_4x2",
    "agent_wall",
];

/// The pattern's name from the closed set, or None.
pub fn pattern_name(name: &str) -> Option<&'static str> {
    PATTERNS.iter().copied().find(|p| *p == name)
}

/// Panes per row, top to bottom, for the patterns made of equal rows.
fn row_counts(pattern: &str) -> Option<&'static [usize]> {
    Some(match pattern {
        "single" => &[1],
        "side_by_side" => &[2],
        "stacked" => &[1, 1],
        "2_over_1" => &[2, 1],
        "3_over_1" => &[3, 1],
        "1_over_3" => &[1, 3],
        "3_over_2" => &[3, 2],
        "2_over_3" => &[2, 3],
        "4_over_2" => &[4, 2],
        "4_over_3" => &[4, 3],
        "grid_2x2" => &[2, 2],
        "grid_3x2" => &[3, 3],
        "grid_2x3" => &[2, 2, 2],
        "grid_4x2" => &[4, 4],
        _ => return None,
    })
}

/// How many panes the pattern holds; None when it takes any number.
pub fn slots(pattern: &str) -> Option<usize> {
    match pattern {
        "main_side" => Some(2),
        p => row_counts(p).map(|r| r.iter().sum()),
    }
}

/// The focused pane takes the first slot.
pub fn has_main(pattern: &str) -> bool {
    matches!(
        pattern,
        "main_side" | "main_stack" | "main_top" | "main_wall" | "focus_sidebar"
    )
}

/// The first-choice pattern for a count.
pub fn best_for(n: usize) -> &'static str {
    match n {
        0 | 1 => "single",
        2 => "side_by_side",
        3 => "columns",
        4 => "grid_2x2",
        5 => "3_over_2",
        6 => "grid_3x2",
        7 => "4_over_3",
        8 => "grid_4x2",
        _ => "agent_wall",
    }
}

fn leaf(id: &str) -> Layout {
    Layout::Pane { pane: id.to_string() }
}

fn split(dir: SplitDir, ratio: f64, a: Layout, b: Layout) -> Layout {
    Layout::Split {
        dir,
        ratio: ratio.clamp(MIN_RATIO, 1.0 - MIN_RATIO),
        a: Box::new(a),
        b: Box::new(b),
    }
}

/// `items` side by side (H) or stacked (V), each an equal share.
fn chain(dir: SplitDir, mut items: Vec<Layout>) -> Layout {
    let n = items.len();
    let first = items.remove(0);
    if items.is_empty() {
        return first;
    }
    split(dir, 1.0 / n as f64, first, chain(dir, items))
}

fn line(dir: SplitDir, panes: &[String]) -> Layout {
    chain(dir, panes.iter().map(|p| leaf(p)).collect())
}

fn rows_of(panes: &[String], counts: &[usize]) -> Layout {
    let mut at = 0;
    let mut rows = vec![];
    for c in counts {
        rows.push(line(SplitDir::H, &panes[at..at + c]));
        at += c;
    }
    chain(SplitDir::V, rows)
}

/// Rows of equal panes whose shape is as near `WALL_ASPECT` as the area
/// allows; a short last row is spread over the full width.
fn wall(panes: &[String], area: (u16, u16)) -> Layout {
    let n = panes.len();
    // (under the floor, distance from the aspect, columns): a column count
    // that keeps every pane at the floor beats one that does not.
    let mut best = (true, f64::MAX, 1);
    for cols in 1..=n {
        let rows = n.div_ceil(cols);
        let (w, h) = (area.0 as usize / cols, area.1 as usize / rows);
        let small = w < FLOOR.0 as usize || h < FLOOR.1 as usize;
        let miss = (w.max(1) as f64 / h.max(1) as f64 - WALL_ASPECT).abs();
        if (small, miss) < (best.0, best.1) {
            best = (small, miss, cols);
        }
    }
    let cols = best.2;
    let mut counts = vec![cols; n / cols];
    let short = n % cols;
    if short > 0 {
        counts.push(short);
    }
    rows_of(panes, &counts)
}

/// The tree for `pattern` over exactly these panes (already in slot order).
/// The caller has matched the count to the pattern (`plan` does).
fn tree(pattern: &str, panes: &[String], area: (u16, u16)) -> Layout {
    if let Some(counts) = row_counts(pattern) {
        return rows_of(panes, counts);
    }
    let main = || leaf(&panes[0]);
    match pattern {
        "columns" => line(SplitDir::H, panes),
        "rows" => line(SplitDir::V, panes),
        "main_side" => split(SplitDir::H, 2.0 / 3.0, main(), line(SplitDir::V, &panes[1..])),
        "main_stack" => split(SplitDir::H, 0.5, main(), line(SplitDir::V, &panes[1..])),
        "main_top" => split(SplitDir::V, 0.5, main(), line(SplitDir::H, &panes[1..])),
        "focus_sidebar" => split(SplitDir::H, 0.7, main(), line(SplitDir::V, &panes[1..])),
        "main_wall" => split(SplitDir::H, 0.5, main(), wall(&panes[1..], (area.0 / 2, area.1))),
        _ => wall(panes, area),
    }
}

fn pane_rects(l: &Layout, area: (u16, u16)) -> Vec<(String, Rect)> {
    let mut panes = vec![];
    let mut splits = vec![];
    let whole = Rect {
        x: 0,
        y: 0,
        w: area.0,
        h: area.1,
    };
    rects(l, whole, &mut panes, &mut splits, "");
    panes
}

/// The panes `l` gives fewer than `min` whole cells at `area`.
pub fn below(l: &Layout, area: (u16, u16), min: (u16, u16)) -> Vec<String> {
    pane_rects(l, area)
        .into_iter()
        .filter(|(_, r)| r.w < min.0 || r.h < min.1)
        .map(|(p, _)| p)
        .collect()
}

pub fn fits(l: &Layout, area: (u16, u16)) -> bool {
    below(l, area, FLOOR).is_empty()
}

/// The tab's panes in reading order (module doc).
pub fn reading_order(l: &Layout, area: (u16, u16)) -> Vec<String> {
    let mut panes = pane_rects(l, area);
    let key = |r: &Rect| (2 * r.y as u32 + r.h as u32, 2 * r.x as u32 + r.w as u32);
    panes.sort_by(|(a, ra), (b, rb)| key(ra).cmp(&key(rb)).then_with(|| a.cmp(b)));
    panes.into_iter().map(|(p, _)| p).collect()
}

/// Same shape, every row and column shared equally: in `h(A, h(B, C))` the
/// outer split becomes a third and the inner a half.
pub fn even_out(l: &Layout) -> Layout {
    fn run(l: &Layout, dir: SplitDir) -> usize {
        match l {
            Layout::Split { dir: d, a, b, .. } if *d == dir => run(a, dir) + run(b, dir),
            _ => 1,
        }
    }
    match l {
        Layout::Pane { .. } => l.clone(),
        Layout::Split { dir, a, b, .. } => {
            let (na, nb) = (run(a, *dir), run(b, *dir));
            split(*dir, na as f64 / (na + nb) as f64, even_out(a), even_out(b))
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    /// The pattern used, which is not the one asked for when the count or the
    /// floor ruled that out.
    pub pattern: &'static str,
    pub layout: Layout,
    /// Panes with no slot, in slot order.
    pub extras: Vec<String>,
}

/// Arrange `current`'s panes by `pattern` (a member of `PATTERNS`) at `area`.
pub fn plan(pattern: &'static str, current: &Layout, focused: &str, area: (u16, u16)) -> Plan {
    if pattern == "even" {
        return Plan {
            pattern,
            layout: even_out(current),
            extras: vec![],
        };
    }
    let mut order = reading_order(current, area);
    let n = order.len();
    let mut pattern = match slots(pattern) {
        Some(k) if n < k => best_for(n),
        None if n < 2 => "single",
        _ => pattern,
    };
    if has_main(pattern) {
        if let Some(i) = order.iter().position(|p| p == focused) {
            let main = order.remove(i);
            order.insert(0, main);
        }
    }
    let mut take = slots(pattern).unwrap_or(n).min(n);
    let mut layout = tree(pattern, &order[..take], area);
    if take > 1 && !fits(&layout, area) {
        pattern = "agent_wall";
        take = n;
        loop {
            layout = tree(pattern, &order[..take], area);
            if take == 1 || fits(&layout, area) {
                break;
            }
            take -= 1;
        }
        if take == 1 {
            pattern = "single";
        }
    }
    Plan {
        pattern,
        layout,
        extras: order.split_off(take),
    }
}

/// The tree for panes that got no slot (the new tab).
pub fn extras_layout(extras: &[String], area: (u16, u16)) -> Option<Layout> {
    (!extras.is_empty()).then(|| wall(extras, area))
}

/// The words for a pattern, as `PATTERN_LABELS` in `packages/layouts` (held
/// to it by `tests/fixtures/offered.json`).
pub fn label(pattern: &str) -> &'static str {
    match pattern {
        "even" => "Even out",
        "single" => "Single",
        "side_by_side" => "Side by side",
        "stacked" => "Stacked",
        "main_side" => "Main + side",
        "columns" => "Columns",
        "rows" => "Rows",
        "main_stack" => "Main + stack",
        "main_top" => "Main on top",
        "main_wall" => "Main + wall",
        "focus_sidebar" => "Focus + sidebar",
        "2_over_1" => "2 over 1",
        "3_over_1" => "3 over 1",
        "1_over_3" => "1 over 3",
        "3_over_2" => "3 over 2",
        "2_over_3" => "2 over 3",
        "4_over_2" => "4 over 2",
        "4_over_3" => "4 over 3",
        "grid_2x2" => "Grid 2x2",
        "grid_3x2" => "Grid 3x2",
        "grid_2x3" => "Grid 2x3",
        "grid_4x2" => "Grid 4x2",
        "agent_wall" => "Agent wall",
        _ => "Layout",
    }
}

/// What `pattern` would look like for `count` panes at `area` (the
/// TypeScript `thumbnail`): the pattern really used, one whole-cell rectangle
/// per pane that gets a slot (the first is the main slot), and how many panes
/// are left over. From the same code that builds the tree.
pub fn thumbnail(pattern: &'static str, count: usize, area: (u16, u16)) -> (&'static str, Vec<Rect>, usize) {
    let ids: Vec<String> = (1..=count.max(1)).map(|i| format!("p{i}")).collect();
    let p = plan(pattern, &line(SplitDir::H, &ids), &ids[0], area);
    let rects = pane_rects(&p.layout, area).into_iter().map(|(_, r)| r).collect();
    (p.pattern, rects, p.extras.len())
}

/// The first view per count, best first, then "More layouts" (design:
/// pane-layouts §5.2). Same table as `offered()` in `packages/layouts`.
type Offer = (&'static [&'static str], &'static [&'static str]);
fn offer_table(count: usize) -> Offer {
    match count {
        2 => (&["side_by_side", "stacked", "main_side"], &[]),
        3 => (
            &["columns", "main_stack", "2_over_1", "main_top"],
            &["rows", "focus_sidebar"],
        ),
        4 => (
            &["grid_2x2", "3_over_1", "main_stack", "columns"],
            &["1_over_3", "rows", "focus_sidebar"],
        ),
        5 => (
            &["3_over_2", "main_stack", "2_over_3", "columns"],
            &["main_top", "focus_sidebar", "agent_wall"],
        ),
        6 => (&["grid_3x2", "main_stack", "4_over_2"], &["grid_2x3", "agent_wall"]),
        7 => (&["4_over_3", "main_wall", "agent_wall"], &["focus_sidebar"]),
        8 => (&["grid_4x2", "main_wall", "agent_wall"], &["focus_sidebar"]),
        _ => (&["agent_wall", "main_wall"], &[]),
    }
}

/// The patterns to offer for `count` panes at `area`: at most four in the
/// first view, then the rest. A pattern is hidden when it would not be used
/// as itself (a pane under the floor, or panes left over). One pane has
/// nothing to arrange. The Rust twin of `offered()` in `packages/layouts`.
pub fn offered(count: usize, area: (u16, u16)) -> (Vec<&'static str>, Vec<&'static str>) {
    if count < 2 {
        return (vec![], vec![]);
    }
    let (first, more) = offer_table(count);
    let ok = |p: &&&'static str| {
        let (used, _, extras) = thumbnail(p, count, area);
        used == **p && extras == 0
    };
    let first: Vec<&'static str> = first.iter().filter(ok).take(4).copied().collect();
    let more = more.iter().filter(ok).copied().collect();
    (first, more)
}

type Refusal = (&'static str, String);

/// A caller's `LayoutNode` as a tree of existing panes. Every field is checked:
/// depth, leaf count, id length, direction, a finite ratio, no pane twice.
/// A leaf that would open a pane is refused (`not_supported`).
pub fn parse_root(node: &Value) -> Result<Layout, Refusal> {
    fn bad(m: &str) -> Refusal {
        ("invalid_params", m.to_string())
    }
    fn walk(node: &Value, depth: usize, seen: &mut Vec<String>) -> Result<Layout, Refusal> {
        if depth > MAX_DEPTH {
            return Err(bad("root is nested too deeply"));
        }
        match node["type"].as_str() {
            Some("pane") => {
                let Some(id) = node["pane_id"].as_str() else {
                    return Err((
                        "not_supported",
                        "layout.arrange places existing panes: every leaf needs a pane_id".into(),
                    ));
                };
                if id.is_empty() || id.len() > MAX_ID {
                    return Err(bad("pane_id is empty or too long"));
                }
                if seen.iter().any(|s| s == id) {
                    return Err(bad("root names a pane twice"));
                }
                if seen.len() >= MAX_LEAVES {
                    return Err(bad("root names too many panes"));
                }
                seen.push(id.to_string());
                Ok(leaf(id))
            }
            Some("split") => {
                let dir = match node["direction"].as_str() {
                    Some("right") => SplitDir::H,
                    Some("down") => SplitDir::V,
                    _ => return Err(bad("direction must be right or down")),
                };
                let ratio = node["ratio"].as_f64().filter(|r| r.is_finite());
                let ratio = ratio.ok_or_else(|| bad("ratio must be a number"))?;
                let a = walk(&node["first"], depth + 1, seen)?;
                let b = walk(&node["second"], depth + 1, seen)?;
                Ok(split(dir, ratio, a, b))
            }
            Some("new") => Err(("not_supported", "layout.arrange does not open panes yet".into())),
            _ => Err(bad("layout node type must be pane or split")),
        }
    }
    walk(node, 0, &mut vec![])
}

/// Pane ids named by `l`.
pub fn leaves(l: &Layout) -> Vec<String> {
    let mut out = vec![];
    layout_panes(l, &mut out);
    out
}

#[cfg(test)]
#[path = "arrange_golden.rs"]
mod golden;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const AREA: (u16, u16) = (240, 60);

    fn ids(n: usize) -> Vec<String> {
        (1..=n).map(|i| format!("w1:p{i}")).collect()
    }

    /// n panes in a row, in id order.
    fn row(n: usize) -> Layout {
        line(SplitDir::H, &ids(n))
    }

    fn sizes(l: &Layout, area: (u16, u16)) -> Vec<(u16, u16, u16, u16)> {
        pane_rects(l, area)
            .into_iter()
            .map(|(_, r)| (r.x, r.y, r.w, r.h))
            .collect()
    }

    /// The rectangles tile the area exactly: whole cells, no gap, no overlap.
    fn tiles(l: &Layout, area: (u16, u16)) {
        let rs = pane_rects(l, area);
        let cells: u32 = rs.iter().map(|(_, r)| r.w as u32 * r.h as u32).sum();
        assert_eq!(cells, area.0 as u32 * area.1 as u32, "{l:?}");
        for (i, (_, a)) in rs.iter().enumerate() {
            assert!(a.w >= 1 && a.h >= 1 && a.x + a.w <= area.0 && a.y + a.h <= area.1);
            for (_, b) in &rs[i + 1..] {
                let apart = a.x + a.w <= b.x || b.x + b.w <= a.x || a.y + a.h <= b.y || b.y + b.h <= a.y;
                assert!(apart, "{a:?} overlaps {b:?}");
            }
        }
    }

    #[test]
    fn every_pattern_at_every_count_keeps_every_pane_once() {
        for pattern in PATTERNS {
            for n in 1..=12 {
                for area in [AREA, (239, 59), (121, 41), (80, 24)] {
                    let p = plan(pattern, &row(n), "w1:p1", area);
                    assert!(pattern_name(p.pattern).is_some(), "{pattern} at {n}: {}", p.pattern);
                    tiles(&p.layout, area);
                    let placed = leaves(&p.layout);
                    let mut all = placed.clone();
                    all.extend(p.extras.clone());
                    all.sort();
                    let mut want = ids(n);
                    want.sort();
                    assert_eq!(all, want, "{pattern} at {n} on {area:?}");
                    if *pattern != "even" && placed.len() > 1 {
                        assert!(fits(&p.layout, area), "{pattern} at {n} on {area:?}: {:?}", p.layout);
                    }
                    if let Some(k) = slots(p.pattern) {
                        assert_eq!(placed.len(), k, "{pattern} at {n} on {area:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn the_shapes_are_the_pictures() {
        let p = plan("3_over_1", &row(4), "w1:p1", AREA);
        assert_eq!(
            sizes(&p.layout, AREA),
            vec![(0, 0, 80, 30), (80, 0, 80, 30), (160, 0, 80, 30), (0, 30, 240, 30)]
        );
        let p = plan("grid_2x2", &row(4), "w1:p1", AREA);
        assert_eq!(
            sizes(&p.layout, AREA),
            vec![(0, 0, 120, 30), (120, 0, 120, 30), (0, 30, 120, 30), (120, 30, 120, 30)]
        );
        let p = plan("3_over_2", &row(5), "w1:p1", AREA);
        assert_eq!(sizes(&p.layout, AREA)[3..], [(0, 30, 120, 30), (120, 30, 120, 30)]);
        let p = plan("main_stack", &row(4), "w1:p1", AREA);
        assert_eq!(
            sizes(&p.layout, AREA),
            vec![
                (0, 0, 120, 60),
                (120, 0, 120, 20),
                (120, 20, 120, 20),
                (120, 40, 120, 20)
            ]
        );
        let p = plan("main_side", &row(2), "w1:p1", AREA);
        assert_eq!(sizes(&p.layout, AREA), vec![(0, 0, 160, 60), (160, 0, 80, 60)]);
        let p = plan("focus_sidebar", &row(3), "w1:p1", AREA);
        assert_eq!(sizes(&p.layout, AREA)[0], (0, 0, 168, 60));
        let p = plan("main_top", &row(3), "w1:p1", AREA);
        assert_eq!(
            sizes(&p.layout, AREA),
            vec![(0, 0, 240, 30), (0, 30, 120, 30), (120, 30, 120, 30)]
        );
        let p = plan("main_wall", &row(5), "w1:p1", AREA);
        assert_eq!(sizes(&p.layout, AREA)[0], (0, 0, 120, 60));
        let p = plan("columns", &row(5), "w1:p1", AREA);
        assert!(sizes(&p.layout, AREA).iter().all(|r| r.2 == 48 && r.3 == 60));
        let p = plan("rows", &row(4), "w1:p1", AREA);
        assert!(sizes(&p.layout, AREA).iter().all(|r| r.2 == 240 && r.3 == 15));
    }

    #[test]
    fn odd_sizes_land_on_whole_cells_and_differ_by_at_most_one() {
        for (w, n) in [(100u16, 3usize), (121, 4), (239, 5), (97, 3), (83, 4)] {
            let p = plan("columns", &row(n), "w1:p1", (w, 41));
            tiles(&p.layout, (w, 41));
            let ws: Vec<u16> = sizes(&p.layout, (w, 41)).iter().map(|r| r.2).collect();
            assert!(
                ws.iter().max().unwrap() - ws.iter().min().unwrap() <= 1,
                "{w}/{n}: {ws:?}"
            );
        }
    }

    #[test]
    fn slot_order_is_reading_order_and_the_focused_pane_takes_the_main_slot() {
        // p1 tall on the left, p2 over p3 on the right: p2's centre is highest.
        let l = split(
            SplitDir::H,
            0.5,
            leaf("w1:p1"),
            split(SplitDir::V, 0.5, leaf("w1:p2"), leaf("w1:p3")),
        );
        assert_eq!(reading_order(&l, AREA), ["w1:p2", "w1:p1", "w1:p3"]);
        assert_eq!(reading_order(&row(3), AREA), ids(3));
        let p = plan("main_stack", &l, "w1:p3", AREA);
        assert_eq!(leaves(&p.layout), ["w1:p3", "w1:p2", "w1:p1"]);
        // No main slot: focus changes nothing.
        let p = plan("columns", &l, "w1:p3", AREA);
        assert_eq!(leaves(&p.layout), ["w1:p2", "w1:p1", "w1:p3"]);
        // Applying a pattern to its own result changes nothing.
        for pattern in PATTERNS {
            for n in 1..=9 {
                let once = plan(pattern, &row(n), "w1:p1", AREA);
                if once.extras.is_empty() {
                    let twice = plan(once.pattern, &once.layout, "w1:p1", AREA);
                    assert_eq!(once.layout, twice.layout, "{pattern} at {n}");
                }
            }
        }
    }

    #[test]
    fn fewer_panes_than_slots_use_the_best_pattern_for_the_count() {
        assert_eq!(plan("grid_2x2", &row(3), "w1:p1", AREA).pattern, "columns");
        assert_eq!(plan("4_over_3", &row(2), "w1:p1", AREA).pattern, "side_by_side");
        assert_eq!(plan("main_wall", &row(1), "w1:p1", AREA).pattern, "single");
        assert_eq!(plan("grid_2x2", &row(1), "w1:p1", AREA).layout, leaf("w1:p1"));
        for n in 1..=12 {
            assert!(slots(best_for(n)).is_none_or(|k| k == n), "{n}");
        }
    }

    #[test]
    fn more_panes_than_slots_are_extras_in_slot_order() {
        let p = plan("grid_2x2", &row(7), "w1:p1", AREA);
        assert_eq!(p.pattern, "grid_2x2");
        assert_eq!(leaves(&p.layout), ids(4));
        assert_eq!(p.extras, ["w1:p5", "w1:p6", "w1:p7"]);
        // The focused pane is first in a main pattern, so it is never an extra.
        let p = plan("main_side", &row(4), "w1:p4", AREA);
        assert_eq!(leaves(&p.layout), ["w1:p4", "w1:p1"]);
        assert_eq!(p.extras, ["w1:p2", "w1:p3"]);
        let x = extras_layout(&p.extras, AREA).unwrap();
        assert_eq!(leaves(&x), p.extras);
        assert_eq!(extras_layout(&[], AREA), None);
    }

    #[test]
    fn a_pattern_under_the_floor_becomes_a_wall_and_then_sheds_panes() {
        // Six columns of 80 are 13 wide: under the floor. The wall fits.
        let p = plan("columns", &row(6), "w1:p1", (80, 24));
        assert_eq!(p.pattern, "agent_wall");
        assert!(p.extras.is_empty() && fits(&p.layout, (80, 24)));
        // 12 panes cannot fit 40x12 at 20x5: the last ones leave, in order.
        let p = plan("agent_wall", &row(12), "w1:p1", (40, 12));
        assert_eq!(p.pattern, "agent_wall");
        assert_eq!(leaves(&p.layout), ids(4));
        assert_eq!(p.extras, ids(12)[4..]);
        assert!(fits(&p.layout, (40, 12)));
        // Nothing fits two panes in 30x8: one stays.
        let p = plan("side_by_side", &row(2), "w1:p1", (30, 8));
        assert_eq!((p.pattern, p.extras.len()), ("single", 1));
        // The floor is exact.
        assert_eq!(plan("side_by_side", &row(2), "w1:p1", (40, 5)).pattern, "side_by_side");
        assert_eq!(plan("side_by_side", &row(2), "w1:p1", (39, 4)).pattern, "single");
        assert_eq!(below(&row(2), (100, 20), READABLE), ids(2));
    }

    #[test]
    fn the_wall_follows_the_area() {
        let wide = plan("agent_wall", &row(8), "w1:p1", (320, 60));
        assert_eq!(sizes(&wide.layout, (320, 60))[0], (0, 0, 80, 30));
        let tall = plan("agent_wall", &row(8), "w1:p1", (100, 240));
        assert_eq!(sizes(&tall.layout, (100, 240))[0], (0, 0, 100, 30));
        // A short last row is spread over the full width.
        let seven = plan("agent_wall", &row(7), "w1:p1", (320, 60));
        assert_eq!(
            sizes(&seven.layout, (320, 60))[4..],
            [(0, 30, 107, 30), (107, 30, 107, 30), (214, 30, 106, 30)]
        );
    }

    #[test]
    fn even_out_keeps_the_shape_and_the_order() {
        let l = split(
            SplitDir::H,
            0.8,
            leaf("a"),
            split(
                SplitDir::H,
                0.2,
                leaf("b"),
                split(SplitDir::V, 0.9, leaf("c"), leaf("d")),
            ),
        );
        let e = even_out(&l);
        assert_eq!(leaves(&e), ["a", "b", "c", "d"]);
        let s = sizes(&e, AREA);
        assert_eq!((s[0].2, s[1].2, s[2].2), (80, 80, 80));
        assert_eq!((s[2].3, s[3].3), (30, 30));
        assert_eq!(even_out(&e), e);
        assert_eq!(plan("even", &l, "a", AREA).layout, e);
    }

    #[test]
    fn a_callers_tree_is_read_strictly() {
        fn pane(id: &str) -> Value {
            json!({"type": "pane", "pane_id": id})
        }
        fn sp(a: Value, b: Value) -> Value {
            json!({"type": "split", "direction": "right", "ratio": 0.5, "first": a, "second": b})
        }
        fn balanced(ids: &[String]) -> Value {
            if ids.len() == 1 {
                return pane(&ids[0]);
            }
            let (a, b) = ids.split_at(ids.len() / 2);
            sp(balanced(a), balanced(b))
        }
        let code = |v: Value| parse_root(&v).unwrap_err().0;
        assert_eq!(
            parse_root(&sp(pane("a"), pane("b"))).unwrap(),
            split(SplitDir::H, 0.5, leaf("a"), leaf("b"))
        );
        assert_eq!(code(json!(null)), "invalid_params");
        assert_eq!(code(json!({"type": "tab"})), "invalid_params");
        assert_eq!(code(sp(pane("a"), pane("a"))), "invalid_params");
        assert_eq!(code(pane("")), "invalid_params");
        assert_eq!(code(pane(&"x".repeat(MAX_ID + 1))), "invalid_params");
        assert_eq!(code(json!({"type": "pane", "pane_id": 7})), "not_supported");
        assert_eq!(code(json!({"type": "pane", "cwd": "/tmp"})), "not_supported");
        assert_eq!(code(json!({"type": "new", "command": ["sh"]})), "not_supported");
        let mut odd = sp(pane("a"), pane("b"));
        odd["direction"] = json!("up");
        assert_eq!(code(odd.clone()), "invalid_params");
        odd["direction"] = json!("down");
        odd["ratio"] = json!("half");
        assert_eq!(code(odd.clone()), "invalid_params");
        // A ratio out of range is clamped, as everywhere else.
        odd["ratio"] = json!(7.0);
        assert_eq!(
            parse_root(&odd).unwrap(),
            split(SplitDir::V, 1.0 - MIN_RATIO, leaf("a"), leaf("b"))
        );
        let mut deep = pane("z");
        for i in 0..=MAX_DEPTH {
            deep = sp(pane(&format!("p{i}")), deep);
        }
        assert_eq!(code(deep), "invalid_params");
        let many = |n: usize| balanced(&(0..n).map(|i| format!("p{i}")).collect::<Vec<_>>());
        assert!(parse_root(&many(MAX_LEAVES)).is_ok());
        assert_eq!(code(many(MAX_LEAVES + 1)), "invalid_params");
    }
}
