//! tmuxedo-detect on real agent CLIs' startup screens (raw PTY bytes at
//! 120x40, through tmuxedo's emulator): each reads as idle.
//!
//! - `claude.bin`: a Claude Code session at its idle prompt box.
//! - `codex.bin`: Codex CLI 0.160.0, `codex --no-daemon` in a trusted git dir
//!   with a dummy API key stored by `codex login --with-api-key` (no account).
//! - `gemini.bin`: Gemini CLI 0.62.0 with `GEMINI_API_KEY=dummy` and API-key
//!   auth preselected in settings (no account).
//! - `opencode.bin`: opencode 1.18.34 with no provider configured.
//!
//! Recorded in `node:24-alpine` with a Python pty recorder that answers the
//! usual terminal queries (DSR, DA, OSC 10/11), ~15–20 s each.

use tmuxedo_detect::{explain, read, Agent, Input, Status};
use tmuxedo_emu::Emulator;

/// `signal`: the signal that decides the screen, or `None` when the agent's
/// idle reading is the default (Gemini and opencode show no idle chrome we key on).
fn check(file: &str, agent: &str, signal: Option<&str>) {
    let agent = Agent::lookup(agent).unwrap();
    let path = format!("{}/../../tests/fixtures/{file}", env!("CARGO_MANIFEST_DIR"));
    let bytes = std::fs::read(&path).unwrap();
    let mut emu = Emulator::new(40, 120, 2000);
    emu.feed(&bytes);
    let text = emu.detection_text();
    let sig = emu.signals().clone();
    let input = Input {
        screen: &text,
        title: &sig.title,
        progress: &sig.progress,
    };
    let e = explain(agent, &input);
    assert_eq!(
        read(agent, &input),
        Status::Idle,
        "{file}: {:?}\nscreen:\n{text}",
        e.signal
    );
    assert_eq!(e.signal.as_deref(), signal, "{file}\nscreen:\n{text}");
}

#[test]
fn claude_recording_reads_as_idle() {
    check("claude.bin", "claude", Some("prompt-box"));
}

#[test]
fn codex_recording_reads_as_idle() {
    check("codex.bin", "codex", Some("title-idle"));
}

#[test]
fn gemini_recording_reads_as_idle() {
    check("gemini.bin", "gemini", None);
}

#[test]
fn opencode_recording_reads_as_idle() {
    check("opencode.bin", "opencode", None);
}
