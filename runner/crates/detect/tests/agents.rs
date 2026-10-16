//! Every agent file against its stand-in screens:
//!
//! - each screen reads as the status in its file name;
//! - every agent has a screen for every status its signals can produce, and for the default;
//! - every signal decides at least one screen (no dead signals).

mod common;

use common::{screens, screens_dir};
use std::collections::{BTreeMap, BTreeSet};
use tmuxedo_detect::{explain, read, Agent, Input, Status};

#[test]
fn every_screen_reads_as_its_status() {
    let mut failures = vec![];
    for agent in Agent::all() {
        for s in screens(agent.id()) {
            let got = read(agent, &s.input());
            if got != s.want {
                let e = explain(agent, &s.input());
                failures.push(format!(
                    "{}: want {}, got {} (signal {:?})",
                    s.path.display(),
                    s.want.as_str(),
                    got.as_str(),
                    e.signal
                ));
            }
        }
    }
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}

#[test]
fn every_status_and_every_signal_is_exercised() {
    let mut missing = vec![];
    for agent in Agent::all() {
        let all = screens(agent.id());
        let probe = explain(agent, &Input::default());
        let has: BTreeSet<Status> = all.iter().map(|s| s.want).collect();
        let mut can: BTreeSet<Status> = probe.signals.iter().map(|s| s.status).collect();
        // The default (idle when no signal matches) needs a screen too, unless a catch-all signal decides
        // even an empty screen.
        if probe.signal.is_none() {
            can.insert(Status::Idle);
        }
        for status in can.difference(&has) {
            missing.push(format!("{}: no {} screen", agent.id(), status.as_str()));
        }
        let mut decided: BTreeMap<String, usize> = BTreeMap::new();
        let mut defaulted = false;
        for s in &all {
            match explain(agent, &s.input()).signal {
                Some(sig) => *decided.entry(sig).or_default() += 1,
                None => defaulted = true,
            }
        }
        for sig in &probe.signals {
            if !decided.contains_key(&sig.name) {
                missing.push(format!("{}: signal {} decides no screen", agent.id(), sig.name));
            }
        }
        if probe.signal.is_none() && !defaulted {
            missing.push(format!("{}: no screen reads idle by default", agent.id()));
        }
    }
    assert!(missing.is_empty(), "\n{}", missing.join("\n"));
}

#[test]
fn screens_exist_only_for_known_agents() {
    let mut dirs = 0;
    for e in std::fs::read_dir(screens_dir()).unwrap().flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let agent = Agent::lookup(&name).unwrap_or_else(|| panic!("tests/screens/{name}: no such agent"));
        assert_eq!(agent.id(), name, "directories use canonical ids");
        dirs += 1;
    }
    assert_eq!(dirs, Agent::all().count(), "every agent has screens");
}
