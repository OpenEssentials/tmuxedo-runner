//! The dialect against packages/protocol/fixtures/settings-schema.json (web
//! runs the same cases through the WebAssembly build:
//! web/tests/plugins/schema-wasm.test.ts), and the hard limits, whose inputs
//! are too large to keep in a file.

use serde_json::json;
use serde_json::Value;
use tmuxedo_schema::check_value_limits;
use tmuxedo_schema::is_secret_name;
use tmuxedo_schema::validate;
use tmuxedo_schema::validate_text;
use tmuxedo_schema::Problem;

fn fx() -> Value {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../packages/protocol/fixtures/settings-schema.json"
    );
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn found(list: &[Problem]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for p in list {
        let line = format!("{} {}", p.code, p.path);
        out.push(line.trim().to_string());
    }
    out.sort();
    out
}

fn wanted(v: &Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for x in v.as_array().unwrap() {
        out.push(x.as_str().unwrap().to_string());
    }
    out.sort();
    out
}

fn codes(list: &[Problem]) -> Vec<&'static str> {
    let mut out: Vec<&'static str> = Vec::new();
    for p in list {
        out.push(p.code);
    }
    out.sort();
    out.dedup();
    out
}

fn schema_codes(schema: &Value) -> Vec<&'static str> {
    codes(&validate(schema, &json!({})).schema)
}

fn value_codes(schema: &Value, value: &Value) -> Vec<&'static str> {
    let got = validate(schema, value);
    assert!(got.schema.is_empty(), "{:?}", got.schema);
    codes(&got.value)
}

fn props(n: usize, each: Value) -> Value {
    let mut m = serde_json::Map::new();
    for i in 0..n {
        m.insert(format!("p{i}"), each.clone());
    }
    Value::Object(m)
}

fn object(properties: Value) -> Value {
    json!({ "type": "object", "properties": properties })
}

fn one(schema: Value) -> Value {
    object(json!({ "a": schema }))
}

#[test]
fn every_fixture_case_agrees() {
    let f = fx();
    let cases = f["cases"].as_array().unwrap();
    assert!(cases.len() > 100, "{} cases", cases.len());
    for c in cases {
        let name = c["name"].as_str().unwrap();
        let got = validate(&c["schema"], &c["value"]);
        let schema = found(&got.schema);
        let value = found(&got.value);
        assert_eq!(schema, wanted(&c["schema_problems"]), "{name}: schema");
        assert_eq!(value, wanted(&c["value_problems"]), "{name}: value");
        assert_eq!(got.ok(), schema.is_empty() && value.is_empty(), "{name}");
    }
}

#[test]
fn every_problem_has_a_message_and_serialises() {
    let schema = one(json!({ "type": "string", "oneOf": [] }));
    let got = validate(&schema, &json!({}));
    assert_eq!(codes(&got.schema), ["keyword_not_allowed"]);
    assert!(got.schema[0].message.contains("oneOf"));
    let j = got.to_json();
    assert_eq!(j["schema"][0]["path"], "/properties/a");
    assert_eq!(j["schema"][0]["code"], "keyword_not_allowed");
    assert_eq!(j["value"], json!([]));
}

#[test]
fn schema_size_and_depth_are_bounded() {
    let text = json!({ "type": "string", "description": "d".repeat(500) });
    let big = object(props(60, object(props(3, text))));
    assert_eq!(schema_codes(&big), ["too_large"]);

    let mut deep = json!("x");
    for _ in 0..9 {
        deep = json!([deep]);
    }
    let nested = one(json!({ "type": "string", "default": deep }));
    assert_eq!(schema_codes(&nested), ["too_deep"]);
}

#[test]
fn schema_counts_are_bounded() {
    let flag = json!({ "type": "boolean" });
    let nodes = object(props(5, object(props(60, flag.clone()))));
    assert_eq!(schema_codes(&nodes), ["too_many_nodes"]);

    let fits = object(props(64, flag.clone()));
    assert!(schema_codes(&fits).is_empty());
    let wide = object(props(65, flag));
    assert_eq!(schema_codes(&wide), ["too_many_properties"]);

    let pattern = json!({ "type": "string", "pattern": "^a$" });
    let fits = object(props(32, pattern.clone()));
    assert!(schema_codes(&fits).is_empty());
    let many = object(props(33, pattern));
    assert_eq!(schema_codes(&many), ["too_many_patterns"]);
}

#[test]
fn pattern_length_and_cost_are_bounded() {
    let fits = one(json!({ "type": "string", "pattern": "a".repeat(256) }));
    assert!(schema_codes(&fits).is_empty());
    let long = one(json!({ "type": "string", "pattern": "a".repeat(257) }));
    assert_eq!(schema_codes(&long), ["pattern_too_long"]);
    let costly = one(json!({ "type": "string", "pattern": "^\\w{1000}$" }));
    assert_eq!(schema_codes(&costly), ["pattern_too_costly"]);
}

#[test]
fn value_size_and_lengths_are_bounded() {
    let schema = object(props(5, json!({ "type": "string", "format": "markdown" })));
    let big = props(5, json!("m".repeat(60_000)));
    assert_eq!(value_codes(&schema, &big), ["too_large"]);
    let over = json!({ "p0": "m".repeat(65_537) });
    assert_eq!(value_codes(&schema, &over), ["max_length"]);
    let fits = json!({ "p0": "m".repeat(65_536) });
    assert!(value_codes(&schema, &fits).is_empty());

    let line = one(json!({ "type": "string" }));
    let fits = json!({ "a": "x".repeat(4096) });
    assert!(value_codes(&line, &fits).is_empty());
    let long = json!({ "a": "x".repeat(4097) });
    assert_eq!(value_codes(&line, &long), ["max_length"]);

    let uri = one(json!({ "type": "string", "format": "uri" }));
    let long = json!({ "a": format!("https://{}", "x".repeat(2048)) });
    assert_eq!(value_codes(&uri, &long), ["max_length"]);
}

#[test]
fn list_length_is_bounded() {
    let schema = one(json!({ "type": "array", "items": { "type": "string" } }));
    let fits = json!({ "a": vec!["x"; 256] });
    assert!(value_codes(&schema, &fits).is_empty());
    let long = json!({ "a": vec!["x"; 257] });
    assert_eq!(value_codes(&schema, &long), ["max_items"]);
}

#[test]
fn value_limits_hold_without_a_schema() {
    let deep = json!({ "a": { "b": { "c": {} } } });
    assert_eq!(codes(&check_value_limits(&deep)), ["too_deep"]);
    let fits = json!({ "a": { "b": ["c"] } });
    assert!(check_value_limits(&fits).is_empty());
}

#[test]
fn texts_are_measured_before_they_are_parsed() {
    let schema = r#"{"type":"object","properties":{}}"#;
    assert!(validate_text(schema, "{}").ok());
    assert_eq!(codes(&validate_text("{", "{}").schema), ["json"]);
    assert_eq!(codes(&validate_text(schema, "{").value), ["json"]);
    let pad = " ".repeat(tmuxedo_schema::MAX_VALUE_BYTES);
    let padded = format!("{schema}{pad}");
    assert_eq!(codes(&validate_text(&padded, "{}").schema), ["too_large"]);
    let padded = format!("{{}}{pad}");
    assert_eq!(codes(&validate_text(schema, &padded).value), ["too_large"]);
}

#[test]
fn secret_names_are_shell_names() {
    assert!(is_secret_name("GH_TOKEN"));
    assert!(is_secret_name("_x1"));
    assert!(!is_secret_name("1X"));
    assert!(!is_secret_name("A-B"));
    assert!(!is_secret_name(""));
    assert!(!is_secret_name(&"A".repeat(65)));
}
