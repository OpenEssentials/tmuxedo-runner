//! Helpers on the wire: the runner writes exactly what the TypeScript side
//! reads (packages/protocol/fixtures/subagents.json, also checked by
//! packages/protocol/test/subagents.test.ts).

use serde_json::Value;
use tmuxedo_protocol::frames::RunnerFrame;
use tmuxedo_protocol::subagents::{AgentChildStatus, MAX_AGENT_CHILDREN, SUBAGENT_COMPLETED, SUBAGENT_STARTED};

fn fixture() -> Value {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../packages/protocol/fixtures/subagents.json"
    );
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn constants_match_the_fixture() {
    let f = fixture();
    assert_eq!(f["maxChildren"], MAX_AGENT_CHILDREN);
    let want = serde_json::json!([SUBAGENT_STARTED, SUBAGENT_COMPLETED]);
    assert_eq!(f["eventTypes"], want);
}

#[test]
fn status_frames_with_children_round_trip() {
    let f = fixture();
    let mut working = 0;
    let mut done = 0;
    let mut nested = 0;
    let mut without = 0;
    for c in f["frames"].as_array().expect("frames[]") {
        let name = c["name"].as_str().unwrap();
        let frame: RunnerFrame =
            serde_json::from_str(c["text"].as_str().unwrap()).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(serde_json::to_value(&frame).unwrap(), c["frame"], "{name}");
        let RunnerFrame::Status { agent: Some(a), .. } = frame else {
            panic!("{name}: not a status frame with an agent")
        };
        if a.children.is_empty() {
            without += 1;
            assert!(c["frame"]["agent"].get("children").is_none(), "{name}");
        }
        for child in &a.children {
            match child.status {
                AgentChildStatus::Working => working += 1,
                AgentChildStatus::Done => done += 1,
            }
            if child.parent_id.is_some() {
                nested += 1;
            }
        }
    }
    assert!(working > 0 && done > 0 && nested > 0 && without > 0);
}

#[test]
fn durable_events_round_trip() {
    let f = fixture();
    let mut seen = vec![];
    for c in f["events"].as_array().expect("events[]") {
        let name = c["name"].as_str().unwrap();
        let frame: RunnerFrame =
            serde_json::from_str(c["text"].as_str().unwrap()).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(serde_json::to_value(&frame).unwrap(), c["frame"], "{name}");
        seen.push(c["frame"]["type"].as_str().unwrap().to_string());
    }
    assert_eq!(seen, [SUBAGENT_STARTED, SUBAGENT_COMPLETED]);
}
