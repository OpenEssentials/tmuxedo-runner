//! `tmuxedo widget …` (docs/design/widgets.md §10): golden snapshots per widget at the
//! design's odd sizes (every tier, both glyph sets), sizes from 1×1 to 500×200 never
//! panic or draw past the edge, resizes leave nothing behind, no colour when asked,
//! and the mapping from the runner's local answers. No runner is started here.

use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::Terminal;
use serde_json::json;
use std::path::PathBuf;
use tmuxedo_runner::cli::widget::{self, Data, Glyphs, MachineData, Opts, Tier};

const SIZES: &[(u16, u16)] = &[
    (1, 1),
    (7, 2),
    (12, 1),
    (20, 1),
    (20, 3),
    (33, 5),
    (40, 8),
    (44, 30),
    (80, 24),
    (120, 40),
    (300, 80),
];

const NOW: u64 = 1_791_882_000_000; // 2026-10-13T09:00:00Z

fn opts(glyphs: Glyphs, color: bool) -> Opts {
    Opts {
        glyphs,
        color,
        paused: false,
    }
}

fn machine() -> Data {
    let mut m = MachineData::default();
    let cpu = [5.0, 12.0, 30.0, 55.0, 23.0, 18.0, 64.0, 80.0, 40.0, 23.0];
    let mem = [52.0, 54.0, 55.0, 57.0, 58.0, 60.0, 61.0, 60.0, 59.0, 60.0];
    for i in 0..40 {
        m.cpu_hist.push(cpu[i % cpu.len()]);
        m.mem_hist.push(mem[i % mem.len()]);
    }
    m.cpu = Some(23.0);
    m.cores = Some(2);
    m.mem = Some((2_576_980_378, 4_294_967_296, true)); // 2.4 of 4.0 GiB
    m.disk = Some((51_539_607_552, 107_374_182_400)); // 48 free of 100
    Data::Machine(m)
}

fn agent_list() -> serde_json::Value {
    json!({ "type": "agent_list", "agents": [
        { "pane_id": "w1:p1", "name": "api-agent", "agent": "claude", "agent_status": "working", "cwd": "/home/dev/work/api" },
        { "pane_id": "w1:p2", "name": "release", "agent": "claude", "agent_status": "blocked", "cwd": "/home/dev/work/site" },
        { "pane_id": "w2:p1", "label": "tests", "agent": "codex", "agent_status": "done", "cwd": "/home/dev/work/api/" },
        { "pane_id": "w2:p4", "name": "a-name-that-is-far-too-long-for-any-column", "agent": "cursor", "agent_status": "idle", "cwd": "/" },
        { "pane_id": "w3:p1", "name": "レビュー", "agent": "claude", "agent_status": "", "cwd": "/home/dev" }
    ]})
}

fn usage_list() -> serde_json::Value {
    let s = NOW / 1000;
    json!({ "type": "usage_list", "enabled": true, "records": [
        { "plugin": "claude", "name": "Claude Code", "observedAt": NOW, "windows": [
            { "id": "5h", "label": "5-hour", "usedPct": 81.4, "resetsAt": s + 2 * 3600 + 15 * 60 },
            { "id": "7d", "label": "weekly", "usedPct": 34.0, "resetsAt": s + 3 * 86_400 }
        ] },
        { "plugin": "codex", "name": "Codex", "observedAt": NOW, "windows": [
            { "id": "5h", "label": "5-hour", "usedPct": 100.0, "resetsAt": s + 40 * 60 }
        ] }
    ] })
}

fn needs_list() -> serde_json::Value {
    json!({ "type": "needs_list", "items": [
        { "id": "n2", "pane_id": "w1:p2", "plugin": "claude", "kind": "permission", "title": "Run a command",
          "detail": { "tool": "Bash", "input": { "command": "git push origin main" } },
          "risk": "high", "responses": [], "answer_via": "hook", "source": "hook", "created_at": "2026-10-13T08:55:00.000Z" },
        { "id": "n1", "pane_id": "w1:p1", "plugin": "claude", "kind": "permission", "title": "wants to run: npm test -- --watch=false",
          "detail": { "tool": "Bash", "input": { "command": "npm test -- --watch=false" } },
          "risk": "normal", "responses": [], "answer_via": "hook", "source": "hook", "created_at": "2026-10-13T08:58:00.000Z" },
        { "id": "n3", "pane_id": "w2:p1", "plugin": "codex", "kind": "question", "title": "Which branch should the release notes go on? There are three candidates.",
          "risk": "normal", "responses": [], "answer_via": "keys", "source": "screen", "created_at": "2026-10-12T21:00:00Z" }
    ]})
}

fn draw(data: &Data, cols: u16, rows: u16, o: &Opts) -> Buffer {
    let mut buf = Buffer::empty(Rect::new(0, 0, cols, rows));
    widget::render(data, buf.area, &mut buf, o);
    buf
}

fn snapshot(name: &str, got: &str) {
    let path = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/snapshots/widget")).join(name);
    if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, got).unwrap();
        return;
    }
    let want = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{}: {e} (run with UPDATE_SNAPSHOTS=1)", path.display()));
    assert_eq!(
        got, want,
        "{name} changed; run with UPDATE_SNAPSHOTS=1 and review the diff"
    );
}

fn sheet(data: &Data, g: Glyphs) -> String {
    let mut out = String::new();
    for &(c, r) in SIZES {
        let t = widget::tier(c, r);
        let port = if widget::portrait(c, r) && t >= Tier::Small {
            ", portrait"
        } else {
            ""
        };
        out.push_str(&format!("=== {c}x{r} ({}{port}) ===\n", t.name()));
        out.push_str(&widget::text(&draw(data, c, r, &opts(g, true))));
    }
    out
}

fn all_data() -> Vec<(&'static str, Data)> {
    vec![
        ("machine", machine()),
        ("agents", Data::Agents(widget::agents_from(&agent_list()))),
        ("needs", Data::Needs(widget::needs_from(&needs_list(), NOW))),
        ("usage", Data::Usage(widget::usage_from(&usage_list(), NOW))),
        (
            "usage-off",
            Data::Usage(widget::usage_from(
                &json!({"type": "usage_list", "enabled": false, "records": []}),
                NOW,
            )),
        ),
        (
            "usage-empty",
            Data::Usage(widget::usage_from(
                &json!({"type": "usage_list", "enabled": true, "records": []}),
                NOW,
            )),
        ),
        ("no-runner", Data::NoRunner),
        ("agents-empty", Data::Agents(vec![])),
        ("needs-empty", Data::Needs(vec![])),
        ("machine-empty", Data::Machine(MachineData::default())),
    ]
}

#[test]
fn snapshots_at_odd_sizes_for_every_widget_and_tier() {
    for (name, data) in all_data() {
        snapshot(&format!("{name}-blocks.txt"), &sheet(&data, Glyphs::Blocks));
        if ["machine", "agents", "needs", "usage"].contains(&name) {
            snapshot(&format!("{name}-ascii.txt"), &sheet(&data, Glyphs::Ascii));
        }
    }
}

#[test]
fn usage_shows_each_window_as_a_gauge_with_its_reset() {
    let d = Data::Usage(widget::usage_from(&usage_list(), NOW));
    let t = widget::text(&draw(&d, 72, 10, &opts(Glyphs::Blocks, false)));
    assert!(t.contains("Claude Code"), "{t}");
    assert!(
        t.contains("5-hour") && t.contains("81%") && t.contains("resets in 2 h 15 m"),
        "{t}"
    );
    assert!(t.contains("weekly") && t.contains("34%"), "{t}");
    assert!(
        t.contains("Codex") && t.contains("100%") && t.contains("resets in 40 m"),
        "{t}"
    );
    assert!(t.contains('█'), "a gauge: {t}");
    let off = Data::Usage(widget::usage_from(&json!({"enabled": false, "records": []}), NOW));
    let t = widget::text(&draw(&off, 72, 10, &opts(Glyphs::Blocks, false)));
    assert!(t.contains("AI usage is off"), "{t}");
    // One row: the fullest window.
    let t = widget::text(&draw(&d, 40, 1, &opts(Glyphs::Blocks, false)));
    assert!(t.contains("100%"), "{t}");
}

#[test]
fn the_sizes_cover_all_six_tiers_and_the_portrait_variant() {
    let tiers: std::collections::BTreeSet<Tier> = SIZES.iter().map(|&(c, r)| widget::tier(c, r)).collect();
    assert_eq!(tiers.len(), 6);
    assert_eq!(widget::tier(1, 1), Tier::Micro);
    assert_eq!(widget::tier(11, 2), Tier::Micro);
    assert_eq!(widget::tier(500, 2), Tier::Strip, "1–2 rows is a strip however wide");
    assert_eq!(widget::tier(19, 50), Tier::Micro);
    assert_eq!(widget::tier(20, 3), Tier::Tiny);
    assert_eq!(widget::tier(40, 8), Tier::Small);
    assert_eq!(widget::tier(80, 24), Tier::Medium);
    assert_eq!(widget::tier(120, 36), Tier::Large);
    assert!(widget::portrait(44, 30));
    assert!(!widget::portrait(80, 24));
    assert!(!widget::portrait(40, 8));
}

#[test]
fn any_size_from_1x1_to_500x200_draws_inside_the_area_and_is_never_blank() {
    let mut cols: Vec<u16> = (1..=60).collect();
    cols.extend([71, 72, 73, 99, 119, 120, 121, 200, 300, 500]);
    let mut rows: Vec<u16> = (1..=20).collect();
    rows.extend([29, 30, 35, 36, 37, 60, 100, 200]);
    for (name, data) in all_data() {
        for g in [Glyphs::Blocks, Glyphs::Ascii] {
            for &c in &cols {
                for &r in &rows {
                    let o = opts(g, true);
                    let lines = widget::lines(&data, c, r, &o);
                    assert!(lines.len() <= r as usize, "{name} {c}x{r}: {} lines", lines.len());
                    for l in &lines {
                        assert!(
                            l.width() <= c as usize,
                            "{name} {c}x{r}: {:?} is wider than the pane",
                            l.to_string()
                        );
                    }
                    let first = lines.first().map(|l| l.to_string()).unwrap_or_default();
                    assert!(
                        !first.trim().is_empty() || r > 2,
                        "{name} {c}x{r}: the first line is blank"
                    );
                    let b = draw(&data, c, r, &o);
                    assert!(!widget::text(&b).trim().is_empty(), "{name} {c}x{r} drew nothing");
                }
            }
        }
    }
}

#[test]
fn a_value_shows_whole_or_not_at_all() {
    let data = machine();
    for c in 1..=60u16 {
        for r in [1u16, 2, 3, 5, 8] {
            let t = widget::text(&draw(&data, c, r, &opts(Glyphs::Blocks, false)));
            for cut in ["2.4 of 4.", "48 GiB fre", "of 2 cor"] {
                assert!(
                    !t.lines().any(|l| l.trim_end().ends_with(cut)),
                    "{c}x{r}: a value was cut: {t}"
                );
            }
            for (whole, part) in [("60%", "6"), ("23%", "2")] {
                for l in t.lines() {
                    // A percentage is never shown without its sign at the end of a line.
                    assert!(!l.ends_with(&format!(" {part}")) || l.contains(whole), "{c}x{r}: {l}");
                }
            }
        }
    }
}

#[test]
fn a_resize_leaves_no_cells_from_the_size_before() {
    let data = Data::Needs(widget::needs_from(&needs_list(), NOW));
    let o = opts(Glyphs::Blocks, true);
    let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
    for (c, r) in [
        (120, 40),
        (80, 24),
        (33, 5),
        (300, 80),
        (12, 1),
        (44, 30),
        (1, 1),
        (80, 24),
    ] {
        term.backend_mut().resize(c, r);
        term.draw(|f| widget::render(&data, f.area(), f.buffer_mut(), &o))
            .unwrap();
        let fresh = widget::text(&draw(&data, c, r, &o));
        assert_eq!(widget::text(term.backend().buffer()), fresh, "{c}x{r}");
    }
}

#[test]
fn no_colour_means_no_colour_anywhere() {
    for (name, data) in all_data() {
        for &(c, r) in SIZES {
            let b = draw(&data, c, r, &opts(Glyphs::Blocks, false));
            for cell in b.content() {
                assert_eq!(cell.fg, Color::Reset, "{name} {c}x{r}");
                assert_eq!(cell.bg, Color::Reset, "{name} {c}x{r}");
            }
        }
    }
    // With colour, the blocked agent is red.
    let b = draw(
        &Data::Agents(widget::agents_from(&agent_list())),
        80,
        24,
        &opts(Glyphs::Blocks, true),
    );
    assert!(b.content().iter().any(|c| c.fg == Color::Red));
}

#[test]
fn colour_and_glyphs_follow_the_environment() {
    let env = |pairs: &'static [(&'static str, &'static str)]| {
        move |k: &str| pairs.iter().find(|(a, _)| *a == k).map(|(_, v)| v.to_string())
    };
    assert!(widget::color_on(&env(&[("TERM", "xterm-256color")])));
    assert!(widget::color_on(&env(&[])));
    assert!(!widget::color_on(&env(&[("NO_COLOR", "")])));
    assert!(!widget::color_on(&env(&[("NO_COLOR", "1"), ("TERM", "xterm")])));
    assert!(!widget::color_on(&env(&[("TERM", "dumb")])));
    assert_eq!(Glyphs::choose(None, &env(&[("LANG", "en_US.UTF-8")])), Glyphs::Blocks);
    assert_eq!(Glyphs::choose(None, &env(&[("LANG", "C")])), Glyphs::Ascii);
    assert_eq!(Glyphs::choose(None, &env(&[])), Glyphs::Ascii);
    assert_eq!(
        Glyphs::choose(None, &env(&[("LC_ALL", "C"), ("LANG", "en_US.UTF-8")])),
        Glyphs::Ascii
    );
    assert_eq!(
        Glyphs::choose(
            None,
            &env(&[("LANG", "en_US.UTF-8"), ("TMUXEDO_WIDGET_GLYPHS", "ascii")])
        ),
        Glyphs::Ascii
    );
    assert_eq!(Glyphs::choose(Some("blocks"), &env(&[("LANG", "C")])), Glyphs::Blocks);
}

#[test]
fn agent_list_maps_to_rows_needing_you_first() {
    let rows = widget::agents_from(&agent_list());
    let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names[0], "release", "blocked first");
    assert_eq!(names[1], "api-agent");
    assert_eq!(rows[2].name, "tests", "a label when there is no name");
    assert_eq!(rows[2].place, "api", "the folder's last part");
    assert_eq!(rows[3].place, "/");
    assert_eq!(rows.last().unwrap().status, "");
    assert!(widget::agents_from(&json!({})).is_empty());
    assert!(widget::agents_from(&json!({ "agents": "nope" })).is_empty());
    let odd = widget::agents_from(&json!({ "agents": [{ "pane_id": "w1:p9", "name": "a\u{1b}[31mb\nc" }] }));
    assert_eq!(odd[0].name, "a [31mb c", "control characters never reach the terminal");
}

#[test]
fn needs_list_maps_to_rows_oldest_first_with_age_and_risk() {
    let rows = widget::needs_from(&needs_list(), NOW);
    assert_eq!(
        rows.iter().map(|r| r.pane.as_str()).collect::<Vec<_>>(),
        ["w2:p1", "w1:p2", "w1:p1"]
    );
    assert_eq!(rows[0].age_ms, Some(12 * 3600 * 1000));
    assert!(rows[1].high);
    assert_eq!(rows[1].command.as_deref(), Some("git push origin main"));
    assert_eq!(rows[2].command, None, "a command already in the title is not repeated");
    assert!(widget::needs_from(&json!({ "items": [{}] }), NOW)[0].age_ms.is_none());
}

#[test]
fn iso_times_parse_like_the_runner_writes_them() {
    for ms in [0u64, 951_782_400_123, NOW, 4_102_444_800_000] {
        assert_eq!(widget::iso_ms(&tmuxedo_runner::util::iso(ms)), Some(ms));
    }
    assert_eq!(widget::iso_ms("2026-10-13T09:00:00Z"), Some(NOW));
    for bad in [
        "",
        "yesterday",
        "2026-10-13 09:00:00Z",
        "2026-10-13T09:00:00+02:00",
        "2026-1x-13T09:00:00Z",
    ] {
        assert_eq!(widget::iso_ms(bad), None, "{bad}");
    }
}

#[test]
fn widget_names_and_picker_numbers() {
    assert_eq!(widget::KINDS, ["usage", "machine", "agents", "needs"]);
    assert_eq!(widget::kind_of(None), Ok(None));
    assert_eq!(widget::kind_of(Some("2")), Ok(Some("machine")));
    assert_eq!(widget::kind_of(Some("needs")), Ok(Some("needs")));
    assert!(widget::kind_of(Some("5")).is_err());
    assert!(widget::kind_of(Some("0")).is_err());
    assert!(widget::kind_of(Some("clock")).is_err());
    for k in widget::KINDS {
        assert!(widget::LIST.contains(k));
    }
    assert!(tmuxedo_runner::cli::TOP_COMMANDS.contains(&"widget"));
    assert!(tmuxedo_runner::cli::names_a_command(&[
        "widget".into(),
        "machine".into()
    ]));
}

#[test]
fn data_is_read_at_the_tier_rate() {
    assert_eq!(Tier::Strip.interval().as_secs(), 5);
    assert_eq!(Tier::Tiny.interval().as_secs(), 5);
    assert_eq!(Tier::Small.interval().as_secs(), 2);
    assert_eq!(Tier::Large.interval().as_secs(), 2);
}

#[test]
fn the_paused_note_shows_in_the_footer() {
    let o = Opts {
        glyphs: Glyphs::Blocks,
        color: false,
        paused: true,
    };
    let t = widget::text(&draw(&machine(), 80, 24, &o));
    assert!(t.contains("paused · any key wakes · q quit"), "{t}");
}
