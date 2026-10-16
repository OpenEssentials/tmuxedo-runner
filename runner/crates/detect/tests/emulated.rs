//! Detection on what tmuxedo's terminal emulator actually produces: agent
//! state transitions, the alternate screen, wide characters, resize, and
//! every byte prefix of the recorded agent sessions (partial frames).

mod common;

use common::screens;
use std::collections::BTreeMap;
use std::path::Path;
use tmuxedo_detect::{read, Agent, Input, Status};
use tmuxedo_emu::Emulator;

/// Draw a screen the way a full-screen TUI does: home, clear, lines with CRLF,
/// then the title (OSC 0) and progress (OSC 9;4).
fn draw(emu: &mut Emulator, screen: &str, title: &str, progress: &str) {
    let mut out = String::from("\x1b[H\x1b[2J\x1b[3J");
    out.push_str(&screen.replace('\n', "\r\n"));
    out.push_str(&format!("\x1b]0;{title}\x07"));
    if !progress.is_empty() {
        out.push_str(&format!("\x1b]9;{progress}\x07"));
    }
    emu.feed(out.as_bytes());
}

fn reading(emu: &mut Emulator, agent: Agent) -> Status {
    let text = emu.detection_text();
    let sig = emu.signals().clone();
    read(
        agent,
        &Input {
            screen: &text,
            title: &sig.title,
            progress: &sig.progress,
        },
    )
}

#[test]
fn every_agent_moves_through_its_states_in_the_emulator() {
    for agent in Agent::all() {
        let all = screens(agent.id());
        let pick = |s: Status| all.iter().find(|x| x.want == s);
        // idle → working → blocked → working → idle, skipping states the agent has no screen for.
        let path: Vec<_> = [
            Status::Idle,
            Status::Working,
            Status::Blocked,
            Status::Working,
            Status::Idle,
        ]
        .into_iter()
        .filter_map(pick)
        .collect();
        assert!(!path.is_empty(), "{}", agent.id());
        let mut emu = Emulator::new(40, 120, 2000);
        let mut seen = vec![];
        for s in path {
            draw(&mut emu, &s.screen, &s.title, &s.progress);
            let got = reading(&mut emu, agent);
            assert_eq!(got, s.want, "{} after drawing {}", agent.id(), s.path.display());
            seen.push(got);
        }
        // An agent that shows more than one status moved between them (signal-less
        // agents, such as Oh My Pi, only ever read idle).
        if all.iter().any(|s| s.want != Status::Idle) {
            assert!(
                seen.windows(2).any(|w| w[0] != w[1]),
                "{}: no transition: {seen:?}",
                agent.id()
            );
        }
    }
}

#[test]
fn the_alternate_screen_is_read_while_it_is_up() {
    let claude = Agent::lookup("claude").unwrap();
    let blocked = screens("claude")
        .into_iter()
        .find(|s| s.want == Status::Blocked)
        .unwrap();
    let mut emu = Emulator::new(40, 120, 2000);
    emu.feed(b"$ claude\r\n");
    emu.feed(b"\x1b[?1049h");
    draw(&mut emu, &blocked.screen, "", "");
    assert_eq!(reading(&mut emu, claude), Status::Blocked);
    // The agent exits: back to the shell's screen, nothing agent-like on it.
    emu.feed(b"\x1b[?1049l\x1b]0;\x07$ ");
    assert_eq!(reading(&mut emu, claude), Status::Idle);
}

#[test]
fn wide_characters_read_without_padding() {
    let qwen = Agent::lookup("qwen").unwrap();
    let mut emu = Emulator::new(40, 120, 2000);
    draw(&mut emu, "  ⠏ 等待用户确认...\n", "", "");
    assert!(
        emu.detection_text().contains("等待用户确认..."),
        "{:?}",
        emu.detection_text()
    );
    assert_eq!(reading(&mut emu, qwen), Status::Blocked);
    let kimi = Agent::lookup("kimi").unwrap();
    draw(&mut emu, "  Reading\n  🌗\n", "", "");
    assert_eq!(reading(&mut emu, kimi), Status::Working);
}

#[test]
fn resize_keeps_reading_the_live_rows() {
    let claude = Agent::lookup("claude").unwrap();
    let working = screens("claude")
        .into_iter()
        .find(|s| s.path.ends_with("working-status-line.txt"))
        .unwrap();
    let mut emu = Emulator::new(40, 120, 2000);
    draw(&mut emu, &working.screen, "", "");
    assert_eq!(reading(&mut emu, claude), Status::Working);
    emu.resize(20, 100);
    assert_eq!(
        reading(&mut emu, claude),
        Status::Working,
        "shorter: the status line is still among the rows read"
    );
    emu.resize(60, 200);
    assert_eq!(reading(&mut emu, claude), Status::Working);
    // The agent redraws for the new size.
    draw(&mut emu, &working.screen, "", "");
    assert_eq!(reading(&mut emu, claude), Status::Working);
}

#[test]
fn a_tiny_pane_still_reads_the_recent_lines() {
    let codex = Agent::lookup("codex").unwrap();
    let mut emu = Emulator::new(5, 40, 2000);
    draw(&mut emu, "› fix it\n\n• Working (3s • esc to interrupt)\n\n› ", "", "");
    assert_eq!(reading(&mut emu, codex), Status::Working);
}

/// The previous engine's readings of every byte prefix of every recording, run-length encoded:
/// `{"<file>@<rows>x<cols>": {"<agent>": [[prefix_len, "status"], …]}}` (a status holds until the next entry).
#[test]
#[cfg_attr(
    coverage,
    ignore = "a regression check, not coverage: ~800,000 readings take over half an hour under coverage \
              instrumentation; it runs in every normal test run"
)]
fn recordings_read_like_the_golden_record_at_every_byte() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let golden: BTreeMap<String, BTreeMap<String, Vec<(usize, String)>>> =
        serde_json::from_str(&std::fs::read_to_string(dir.join("tests/golden/recordings.json")).unwrap()).unwrap();
    assert!(!golden.is_empty());
    // The recording × size combinations are independent: a few threads.
    let jobs: Vec<_> = golden.iter().collect();
    let next = std::sync::atomic::AtomicUsize::new(0);
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get()).min(4);
    let failures: Vec<String> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                scope.spawn(|| {
                    let mut out = vec![];
                    while let Some((key, per_agent)) = jobs.get(next.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
                    {
                        out.extend(check_recording(dir, key, per_agent));
                    }
                    out
                })
            })
            .collect();
        workers.into_iter().flat_map(|w| w.join().unwrap()).collect()
    });
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}

/// One recording at one size against its golden runs; up to 20 failures.
fn check_recording(dir: &Path, key: &str, per_agent: &BTreeMap<String, Vec<(usize, String)>>) -> Vec<String> {
    let mut failures = vec![];
    let (file, size) = key.split_once('@').unwrap();
    let (rows, cols) = size.split_once('x').unwrap();
    let bytes = std::fs::read(dir.join("../../tests/fixtures").join(file)).unwrap();
    let mut emu = Emulator::new(rows.parse().unwrap(), cols.parse().unwrap(), 2000);
    let agents: Vec<(Agent, &Vec<(usize, String)>)> = per_agent
        .iter()
        .map(|(id, runs)| (Agent::lookup(id).unwrap(), runs))
        .collect();
    let mut cursor = vec![0usize; agents.len()];
    let mut last_key = String::new();
    let mut last: Vec<Status> = vec![];
    for (n, b) in bytes.iter().enumerate() {
        emu.feed(std::slice::from_ref(b));
        let text = emu.detection_text();
        let sig = emu.signals().clone();
        let k = format!("{text}\0{}\0{}", sig.title, sig.progress);
        if k != last_key {
            let input = Input {
                screen: &text,
                title: &sig.title,
                progress: &sig.progress,
            };
            last = agents.iter().map(|(a, _)| read(*a, &input)).collect();
            last_key = k;
        }
        for (i, (agent, runs)) in agents.iter().enumerate() {
            while cursor[i] + 1 < runs.len() && runs[cursor[i] + 1].0 <= n + 1 {
                cursor[i] += 1;
            }
            let want = Status::parse(&runs[cursor[i]].1).unwrap();
            if last[i] != want && failures.len() < 20 {
                failures.push(format!(
                    "{key} prefix {}: {} reads {} (golden {})",
                    n + 1,
                    agent.id(),
                    last[i].as_str(),
                    want.as_str()
                ));
            }
        }
    }
    failures
}
