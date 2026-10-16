//! `config.pluginSet` against packages/protocol/fixtures/plugin-set.json (the
//! TypeScript side checks the same file in test/plugin-set.test.ts).

use serde_json::Value;
use tmuxedo_protocol::frames::parse_gateway;
use tmuxedo_protocol::frames::GatewayFrame;
use tmuxedo_protocol::frames::Parsed;
use tmuxedo_protocol::plugin_set::parse_plugin_set;
use tmuxedo_protocol::plugin_set::valid_item_id;
use tmuxedo_protocol::plugin_set::valid_item_name;

const NAMES: [&str; 7] = ["full", "absent", "empty", "junk", "future", "not_object", "null"];

fn fx() -> Value {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../packages/protocol/fixtures/plugin-set.json"
    );
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// The frame's `pluginSet` as the runner receives it.
fn received(frame: &Value) -> Option<Value> {
    match parse_gateway(&frame.to_string()) {
        Parsed::Frame(GatewayFrame::Config { plugin_set, .. }) => plugin_set.map(|b| *b),
        _ => panic!("not a config frame: {frame}"),
    }
}

#[test]
fn every_frame_parses_as_the_fixture_says() {
    let f = fx();
    assert_eq!(f["parsed"].as_object().unwrap().len(), NAMES.len());
    for name in NAMES {
        let got = received(&f["config"][name]);
        let set = got.as_ref().and_then(parse_plugin_set);
        let json = serde_json::to_value(&set).unwrap();
        assert_eq!(json, f["parsed"][name], "{name}");
    }
}

#[test]
fn a_frame_without_the_set_is_the_frame_it_always_was() {
    let f = fx();
    let frame = &f["config"]["absent"];
    assert!(received(frame).is_none());
    let Parsed::Frame(parsed) = parse_gateway(&frame.to_string()) else {
        panic!("not a frame")
    };
    assert_eq!(&serde_json::to_value(&parsed).unwrap(), frame);
}

#[test]
fn a_project_that_is_not_listed_gets_the_machine_set() {
    let f = fx();
    let set = parse_plugin_set(&f["config"]["full"]["pluginSet"]).unwrap();
    let api = Some("github.com/acme/api");
    let machine = set.entry("claude", None).unwrap();
    assert_eq!(machine.items, ["i-gh", "i-pg", "i-notes"]);
    assert_eq!(machine.values["extras"], true);
    let in_api = set.entry("claude", api).unwrap();
    assert_eq!(in_api.items, ["i-gh-api", "i-pg"]);
    assert_eq!(in_api.values["extras"], false);
    let other = set.entry("claude", Some("github.com/acme/untouched"));
    assert_eq!(other, Some(machine));
    assert!(set.entry("unknown", api).is_none());
    assert!(set.entry("cursor", api).unwrap().items.is_empty());
    assert_eq!(set.item("i-gh-api").unwrap().name, "github");
    assert!(set.item("i-other").is_none());
}

#[test]
fn a_parsed_set_serialises_to_itself() {
    let f = fx();
    for name in ["full", "junk", "empty"] {
        let set = parse_plugin_set(&f["config"][name]["pluginSet"]).unwrap();
        let json = serde_json::to_value(&set).unwrap();
        assert_eq!(parse_plugin_set(&json), Some(set), "{name}");
    }
}

#[test]
fn ids_and_names_are_bounded() {
    assert!(valid_item_id("cmf0abc_DEF-1"));
    assert!(!valid_item_id(""));
    assert!(!valid_item_id("a b"));
    assert!(!valid_item_id(&"a".repeat(65)));
    assert!(valid_item_name("release-notes"));
    assert!(valid_item_name("7zip"));
    assert!(!valid_item_name("-x"));
    assert!(!valid_item_name("Release"));
    assert!(!valid_item_name("a_b"));
    assert!(!valid_item_name(&"a".repeat(41)));
}
