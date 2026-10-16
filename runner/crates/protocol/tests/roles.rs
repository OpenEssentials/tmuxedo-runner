//! Roles: the runner reads a role document exactly as the TypeScript side
//! does, and takes its hash over the same text
//! (packages/protocol/fixtures/roles.json, also checked by
//! packages/protocol/test/roles.test.ts).

use serde_json::Value;
use sha2::{Digest, Sha256};
use tmuxedo_protocol::roles::{
    parse_role, ROLE_ACCESS, ROLE_COLORS, ROLE_DELEGATES_MAX, ROLE_GLYPH_MAX, ROLE_INSTRUCTIONS_MAX_BYTES,
    ROLE_LIST_MAX, ROLE_MAX_DEPTH, ROLE_MAX_SUBAGENTS, ROLE_MODELS_MAX, ROLE_MODEL_MAX, ROLE_NAME_MAX,
    ROLE_PURPOSE_MAX, ROLE_SCHEMA, ROLE_TOOLS_MAX, ROLE_TOOL_MAX,
};

fn fixture() -> Value {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../packages/protocol/fixtures/roles.json"
    );
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn sha(text: &str) -> String {
    hex::encode(Sha256::digest(text.as_bytes()))
}

#[test]
fn constants_match_the_fixture() {
    let f = fixture();
    assert_eq!(f["schema"], ROLE_SCHEMA);
    assert_eq!(f["access"], serde_json::json!(ROLE_ACCESS));
    assert_eq!(f["colors"], serde_json::json!(ROLE_COLORS));
    let want = serde_json::json!({
        "name": ROLE_NAME_MAX, "purpose": ROLE_PURPOSE_MAX, "instructionsBytes": ROLE_INSTRUCTIONS_MAX_BYTES,
        "glyph": ROLE_GLYPH_MAX, "model": ROLE_MODEL_MAX, "models": ROLE_MODELS_MAX, "tool": ROLE_TOOL_MAX,
        "tools": ROLE_TOOLS_MAX, "list": ROLE_LIST_MAX, "delegates": ROLE_DELEGATES_MAX,
        "maxSubagents": ROLE_MAX_SUBAGENTS, "maxDepth": ROLE_MAX_DEPTH,
    });
    assert_eq!(f["limits"], want);
}

#[test]
fn valid_roles_give_the_listed_canonical_text_and_hash() {
    let f = fixture();
    let cases = f["valid"].as_array().expect("valid[]");
    assert!(cases.len() > 5);
    for c in cases {
        let name = c["name"].as_str().unwrap();
        let role = parse_role(&c["input"]).unwrap_or_else(|p| panic!("{name}: {p:?}"));
        let text = role.canonical();
        assert_eq!(text, c["canonical"].as_str().unwrap(), "{name}");
        assert_eq!(sha(&text), c["hash"].as_str().unwrap(), "{name}");
        // The canonical text is itself a role document, and a fixed point.
        let back = parse_role(&serde_json::from_str(&text).unwrap()).unwrap_or_else(|p| panic!("{name}: {p:?}"));
        assert_eq!(back, role, "{name}");
        assert_eq!(back.canonical(), text, "{name}");
        // The normal form the TypeScript side lists holds the same values.
        let listed = &c["role"];
        assert_eq!(listed["id"], role.id.as_str(), "{name}");
        assert_eq!(listed["cli"], role.cli.as_str(), "{name}");
        assert_eq!(listed["access"], role.access.as_str(), "{name}");
        assert_eq!(listed["delegates"], serde_json::json!(role.delegates), "{name}");
        assert_eq!(listed["tools"]["allow"], serde_json::json!(role.tools_allow), "{name}");
    }
}

#[test]
fn invalid_roles_name_the_same_fields_in_the_same_order() {
    let f = fixture();
    let cases = f["invalid"].as_array().expect("invalid[]");
    assert!(cases.len() > 10);
    for c in cases {
        let name = c["name"].as_str().unwrap();
        let problems = parse_role(&c["input"]).expect_err(name);
        let paths: Vec<&str> = problems.iter().map(|p| p.path.as_str()).collect();
        assert_eq!(serde_json::json!(paths), c["paths"], "{name}");
        assert!(problems.iter().all(|p| p.message.len() > 5), "{name}");
    }
}
