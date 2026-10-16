//! Needs you on the wire against packages/protocol/fixtures/needs.json (the
//! TypeScript side checks the same file in test/needs.test.ts).

use serde_json::Value;
use tmuxedo_protocol::frames::RunnerFrame;
use tmuxedo_protocol::needs::{host_allowed, parse_item, parse_respond, NeedsYou};

fn fx() -> Value {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../packages/protocol/fixtures/needs.json"
    );
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn valid_items_round_trip_exactly() {
    let f = fx();
    for (name, want) in f["items"]["valid"].as_object().unwrap() {
        let it = parse_item(want).unwrap_or_else(|| panic!("{name} refused"));
        assert_eq!(&serde_json::to_value(&it).unwrap(), want, "{name}");
    }
}

#[test]
fn junk_items_are_refused() {
    let f = fx();
    for j in f["items"]["junk"].as_array().unwrap() {
        assert!(parse_item(&j["item"]).is_none(), "{}", j["why"]);
    }
}

#[test]
fn a_needs_frame_serialises_as_the_fixture_says() {
    let f = fx();
    let item: NeedsYou = serde_json::from_value(f["items"]["valid"]["permission_hook"].clone()).unwrap();
    let frame = RunnerFrame::Needs {
        pane_id: "w1:p1".into(),
        items: vec![item],
    };
    let v: Value = serde_json::from_str(&frame.to_json()).unwrap();
    assert_eq!(v["t"], "needs");
    assert_eq!(v["paneId"], "w1:p1");
    assert_eq!(v["items"][0], f["items"]["valid"]["permission_hook"]);
}

#[test]
fn respond_params() {
    let f = fx();
    for p in f["respond"]["valid"].as_array().unwrap() {
        let r = parse_respond(p).unwrap_or_else(|e| panic!("{p}: {e}"));
        assert_eq!(&serde_json::to_value(&r).unwrap(), p);
    }
    for c in f["respond"]["invalid"].as_array().unwrap() {
        assert!(parse_respond(&c["params"]).is_err(), "{}", c["why"]);
    }
}

#[test]
fn host_allowlists() {
    let f = fx();
    let allow: Vec<String> = serde_json::from_value(f["hosts"]["allow"].clone()).unwrap();
    for c in f["hosts"]["cases"].as_array().unwrap() {
        assert_eq!(
            host_allowed(c["host"].as_str().unwrap(), &allow),
            c["ok"].as_bool().unwrap(),
            "{}",
            c["host"]
        );
    }
}
