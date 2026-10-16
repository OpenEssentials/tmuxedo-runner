//! The public API on the process-wide catalogue.

use tmuxedo_detect::{catalog_rows, explain, explain_label, identify, read, Agent, Input, Job, Process, Status};

#[test]
fn statuses_round_trip() {
    for s in Status::ALL {
        assert_eq!(Status::parse(s.as_str()), Some(s));
    }
    assert_eq!(Status::parse("done"), None);
    let names: Vec<_> = Status::ALL.iter().map(|s| s.as_str()).collect();
    assert_eq!(names, ["working", "blocked", "idle", "unknown", "hold"]);
}

#[test]
fn input_from_a_screen() {
    let i = Input::screen("abc");
    assert_eq!((i.screen, i.title, i.progress), ("abc", "", ""));
    let d = Input::default();
    assert_eq!((d.screen, d.title, d.progress), ("", "", ""));
}

#[test]
fn every_supported_agent() {
    let ids: Vec<_> = Agent::all().map(|a| a.id()).collect();
    assert_eq!(
        ids,
        [
            "claude",
            "codex",
            "gemini",
            "cursor",
            "copilot",
            "opencode",
            "amp",
            "devin",
            "agy",
            "cline",
            "kilo",
            "kimi",
            "kiro",
            "droid",
            "grok",
            "hermes",
            "letta",
            "maki",
            "muse",
            "qodercli",
            "qwen",
            "pi",
            "omp",
            "mastracode",
        ]
    );
    for (i, a) in Agent::all().enumerate() {
        assert_eq!(a.index(), i);
        assert!(!a.title().is_empty());
        assert_eq!(Agent::lookup(a.id()), Some(a));
        assert_eq!(Agent::lookup(a.launch()), Some(a), "{} launches {}", a.id(), a.launch());
    }
    let cursor = Agent::lookup("cursor").unwrap();
    assert_eq!((cursor.title(), cursor.launch()), ("Cursor CLI", "cursor-agent"));
    assert_eq!(Agent::lookup("kiro").unwrap().launch(), "kiro-cli");
    assert_eq!(Agent::lookup("nope"), None);
}

#[test]
fn read_explain_and_unknown_labels() {
    let claude = Agent::lookup("claude").unwrap();
    let busy = Input {
        screen: "",
        title: "⠂ task",
        progress: "",
    };
    assert_eq!(read(claude, &busy), Status::Working);
    let e = explain(claude, &busy);
    assert_eq!((e.status, e.signal.as_deref()), (Status::Working, Some("title-busy")));
    assert_eq!(explain_label(Some("claude"), &busy), e);
    let vim = explain_label(Some("vim"), &busy);
    assert_eq!((vim.agent.as_deref(), vim.status), (Some("vim"), Status::Unknown));
    let none = explain_label(None, &busy);
    assert_eq!((none.agent, none.status), (None, Status::Unknown));
}

#[test]
fn identify_on_the_global_catalogue() {
    let job = Job {
        pgid: 7,
        processes: vec![Process {
            pid: 7,
            name: "codex".into(),
            argv: vec!["codex".into()],
        }],
    };
    assert_eq!(identify(&job).map(|a| a.id()), Some("codex"));
    assert_eq!(
        identify(&Job {
            pgid: 7,
            processes: vec![]
        }),
        None
    );
}

#[test]
fn catalogue_rows() {
    let rows = catalog_rows();
    assert_eq!(rows.len(), 24);
    assert_eq!(rows[0]["agent"], "claude");
    assert_eq!(rows[0]["source"], "built-in");
    assert!(rows[0]["signals"].as_u64().unwrap() > 10);
    assert_eq!(rows[22]["signals"], 0, "omp has no screen signals");
}
