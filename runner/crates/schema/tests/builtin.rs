//! The settings schemas of the built-in plugins
//! (`crates/runner/plugins/<id>/settings.schema.json`) are in the dialect, and
//! their defaults fit them. Web checks the same files through the WebAssembly
//! build (web/tests/plugins/schema-wasm.test.ts).

use serde_json::json;
use serde_json::Value;
use tmuxedo_schema::validate;

const PLUGINS: [&str; 3] = ["claude", "codex", "cursor"];

fn schema(id: &str) -> Value {
    let path = format!(
        "{}/../runner/plugins/{id}/settings.schema.json",
        env!("CARGO_MANIFEST_DIR")
    );
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn every_built_in_schema_is_in_the_dialect() {
    for id in PLUGINS {
        let got = validate(&schema(id), &json!({}));
        assert!(got.schema.is_empty(), "{id}: {:?}", got.schema);
        assert!(got.value.is_empty(), "{id}: {:?}", got.value);
    }
}

#[test]
fn the_shared_settings_fit_and_an_unknown_one_is_refused() {
    for id in PLUGINS {
        let s = schema(id);
        let good = json!({"args": ["--model", "x"], "model": "some-model.1", "extras": false});
        let got = validate(&s, &good);
        assert!(got.ok(), "{id}: {:?} {:?}", got.schema, got.value);
        let bad = json!({"args": "--model x", "model": "a b", "nope": 1});
        let got = validate(&s, &bad);
        assert!(got.schema.is_empty(), "{id}: {:?}", got.schema);
        assert_eq!(got.value.len(), 3, "{id}: {:?}", got.value);
    }
}

#[test]
fn every_dangerous_setting_is_a_switch_that_is_off_by_default() {
    for id in PLUGINS {
        let s = schema(id);
        let mut seen = 0;
        for (name, node) in s["properties"].as_object().unwrap() {
            if node.get("x-dangerous").is_none() {
                continue;
            }
            seen += 1;
            assert_eq!(node["type"], "boolean", "{id}.{name}");
            assert_eq!(node["default"], false, "{id}.{name}");
        }
        assert_eq!(seen, 1, "{id}");
    }
}
