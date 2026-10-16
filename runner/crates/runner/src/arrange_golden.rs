//! The golden fixtures `packages/layouts` is held to: every built-in pattern
//! at 1..=10 panes on four areas, a few current shapes, Even out, and the
//! floor fallback, each with the tree, the whole-cell rectangles, the extras
//! and the panes under the readable size.
//!
//! `tests/fixtures/arrange.json` is generated here. The test fails when the
//! file is stale; `TMUXEDO_UPDATE_FIXTURES=1 cargo test -p tmuxedo-runner
//! arrange::golden` rewrites it. The TypeScript twin reads the same file.

use super::*;
use serde_json::{json, Value};

const FILE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/arrange.json");
const AREAS: [(u16, u16); 4] = [(240, 60), (121, 41), (80, 24), (40, 10)];

fn ids(n: usize) -> Vec<String> {
    (1..=n).map(|i| format!("w1:p{i}")).collect()
}

fn tree(l: &Layout) -> Value {
    serde_json::to_value(l).unwrap()
}

fn case(name: String, pattern: &'static str, area: (u16, u16), current: &Layout, focused: &str) -> Value {
    let p = plan(pattern, current, focused, area);
    let sizes: Vec<Value> = pane_rects(&p.layout, area)
        .into_iter()
        .map(|(_, r)| json!([r.x, r.y, r.w, r.h]))
        .collect();
    json!({
        "name": name,
        "pattern": pattern,
        "area": [area.0, area.1],
        "current": tree(current),
        "focused": focused,
        "expect": {
            "pattern": p.pattern,
            "layout": tree(&p.layout),
            "extras": p.extras,
            "sizes": sizes,
            "below_readable": below(&p.layout, area, READABLE),
        },
    })
}

fn cases() -> Vec<Value> {
    let mut out = vec![];
    for pattern in PATTERNS {
        for n in 1..=10 {
            let panes = ids(n);
            let focused = panes[n / 2].clone();
            for area in AREAS {
                out.push(case(
                    format!("{pattern} {n} panes {}x{}", area.0, area.1),
                    pattern,
                    area,
                    &line(SplitDir::H, &panes),
                    &focused,
                ));
            }
        }
        // Other current shapes: reading order, not id order, decides the slots.
        for n in [3, 4, 5, 7] {
            let panes = ids(n);
            out.push(case(
                format!("{pattern} {n} panes stacked now"),
                pattern,
                (240, 60),
                &line(SplitDir::V, &panes),
                &panes[n - 1],
            ));
        }
        let q = ids(4);
        let quad = split(SplitDir::H, 0.5, line(SplitDir::V, &q[..2]), line(SplitDir::V, &q[2..]));
        out.push(case(
            format!("{pattern} 4 panes quad now"),
            pattern,
            (239, 59),
            &quad,
            &q[0],
        ));
    }
    // Even out: unequal ratios, chains and nesting; the order never changes.
    let p = ids(5);
    let uneven = [
        split(
            SplitDir::H,
            0.8,
            leaf(&p[0]),
            split(SplitDir::H, 0.3, leaf(&p[1]), leaf(&p[2])),
        ),
        split(SplitDir::V, 0.1, line(SplitDir::H, &p[..2]), line(SplitDir::H, &p[2..])),
        split(
            SplitDir::H,
            0.25,
            split(
                SplitDir::V,
                0.9,
                leaf(&p[0]),
                split(SplitDir::V, 0.2, leaf(&p[1]), leaf(&p[2])),
            ),
            split(SplitDir::H, 0.7, leaf(&p[3]), leaf(&p[4])),
        ),
        leaf(&p[0]),
    ];
    for (i, l) in uneven.iter().enumerate() {
        out.push(case(format!("even shape {i}"), "even", (240, 60), l, &p[0]));
    }
    out
}

fn render() -> String {
    let slot: serde_json::Map<String, Value> = PATTERNS.iter().map(|p| (p.to_string(), json!(slots(p)))).collect();
    let main: Vec<&&str> = PATTERNS.iter().filter(|p| has_main(p)).collect();
    let best: Vec<&str> = (0..=13).map(best_for).collect();
    let head = json!({
        "floor": [FLOOR.0, FLOOR.1],
        "readable": [READABLE.0, READABLE.1],
        "patterns": PATTERNS,
        "slots": slot,
        "has_main": main,
        "best_for": best,
    });
    let head = serde_json::to_string(&head).unwrap();
    let lines: Vec<String> = cases().iter().map(|c| serde_json::to_string(c).unwrap()).collect();
    // One case per line keeps a diff readable. The head's closing brace opens "cases".
    format!("{},\"cases\":[\n{}\n]}}\n", &head[..head.len() - 1], lines.join(",\n"))
}

#[test]
fn golden_fixtures_are_fresh() {
    let want = render();
    if std::env::var_os("TMUXEDO_UPDATE_FIXTURES").is_some() {
        std::fs::write(FILE, &want).unwrap();
    }
    let have = std::fs::read_to_string(FILE).unwrap_or_default();
    assert!(
        have == want,
        "tests/fixtures/arrange.json is stale: run TMUXEDO_UPDATE_FIXTURES=1 cargo test -p tmuxedo-runner arrange::golden"
    );
    serde_json::from_str::<Value>(&have).expect("the fixtures are valid JSON");
}

const OFFERED_FILE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/offered.json");
const OFFERED_AREAS: [(u16, u16); 7] = [(240, 60), (200, 50), (121, 41), (100, 30), (80, 24), (60, 20), (40, 10)];

/// What the Arrange picker offers per count and area, the pattern words, and
/// each offered picture's rectangles: `offered()`, `PATTERN_LABELS` and
/// `thumbnail()` in `packages/layouts` are held to this (the TUI uses the Rust).
fn render_offered() -> String {
    let labels: serde_json::Map<String, Value> = PATTERNS.iter().map(|p| (p.to_string(), json!(label(p)))).collect();
    let mut lines = vec![];
    for area in OFFERED_AREAS {
        for count in 1..=12 {
            let (first, more) = offered(count, area);
            let pictures: serde_json::Map<String, Value> = first
                .iter()
                .chain(more.iter())
                .map(|p| {
                    let (_, rects, _) = thumbnail(p, count, area);
                    (
                        p.to_string(),
                        json!(rects.iter().map(|r| [r.x, r.y, r.w, r.h]).collect::<Vec<_>>()),
                    )
                })
                .collect();
            let case =
                json!({ "count": count, "area": [area.0, area.1], "first": first, "more": more, "rects": pictures });
            lines.push(serde_json::to_string(&case).unwrap());
        }
    }
    let head = serde_json::to_string(&json!({ "labels": labels })).unwrap();
    format!("{},\"cases\":[\n{}\n]}}\n", &head[..head.len() - 1], lines.join(",\n"))
}

#[test]
fn offered_fixtures_are_fresh() {
    let want = render_offered();
    if std::env::var_os("TMUXEDO_UPDATE_FIXTURES").is_some() {
        std::fs::write(OFFERED_FILE, &want).unwrap();
    }
    let have = std::fs::read_to_string(OFFERED_FILE).unwrap_or_default();
    assert!(
        have == want,
        "tests/fixtures/offered.json is stale: run TMUXEDO_UPDATE_FIXTURES=1 cargo test -p tmuxedo-runner arrange::golden"
    );
    serde_json::from_str::<Value>(&have).expect("the fixtures are valid JSON");
}
