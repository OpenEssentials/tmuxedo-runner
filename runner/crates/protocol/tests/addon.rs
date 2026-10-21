//! Add-on definitions against packages/protocol/fixtures/addons.json (the
//! TypeScript side checks the same cases in test/addon.test.ts).

use serde_json::{json, Value};
use tmuxedo_protocol::addon::{parse_addon, reserved_env_name, valid_file_path, EnvValue, Probe};
use tmuxedo_protocol::plugin_set::parse_plugin_set;

fn fx() -> Value {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../packages/protocol/fixtures/addons.json"
    );
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn the_full_example_parses_into_its_parts() {
    let a = parse_addon(&fx()["full"]).expect("valid");
    assert_eq!(a.net, ["https://telemetry.example.com"]);
    assert_eq!(a.probe[0], Probe::File(".example-telemetry/installed".into()));
    assert_eq!(a.probe[1], Probe::Mentions("example-telemetry".into()));
    assert!(a.files["bin/hook"].exec && !a.files["README.txt"].exec);
    let claude = &a.agents["claude"];
    assert_eq!(
        claude.env["EXAMPLE_TELEMETRY_TOKEN"],
        EnvValue::Secret("EXAMPLE_TELEMETRY_TOKEN".into())
    );
    assert_eq!(claude.hooks["SessionStart"], ["{file:bin/hook} start"]);
    assert_eq!(claude.statusline.as_deref(), Some("{file:bin/statusline}"));
    assert_eq!(a.agents["codex"].config.len(), 3);
    assert_eq!(a.agents["cursor"].hooks.len(), 1);
}

#[test]
fn good_cases_are_accepted() {
    for c in fx()["good"].as_array().unwrap() {
        assert!(parse_addon(&c["definition"]).is_ok(), "{}", c["name"]);
    }
}

#[test]
fn bad_cases_name_the_same_path_as_the_other_side() {
    for c in fx()["bad"].as_array().unwrap() {
        let issues = parse_addon(&c["definition"]).expect_err(c["name"].as_str().unwrap());
        let want = c["path"].as_str().unwrap();
        assert!(issues.iter().any(|i| i.path == want), "{}: {:?}", c["name"], issues);
    }
}

#[test]
fn too_large_is_refused() {
    let files: serde_json::Map<String, Value> = (0..5)
        .map(|i| (format!("f{i}"), json!({ "body": "x".repeat(60 * 1024) })))
        .collect();
    let issues = parse_addon(&json!({ "files": files, "agents": { "claude": { "env": { "A": "1" } } } })).unwrap_err();
    assert_eq!(issues[0].code, "too_large");
}

#[test]
fn paths_and_names() {
    assert!(["bin/hook", "a.b/c-d_e", "x"].iter().all(|p| valid_file_path(p)));
    let bad = ["", "/x", "../x", "a/../b", "a/./b", ".hidden", "a//b", "a/b/c/d/e"];
    assert!(!bad.iter().any(|p| valid_file_path(p)));
    let reserved = [
        "PATH",
        "path",
        "TMUXEDO_X",
        "AIMUX_X",
        "LD_PRELOAD",
        "DYLD_X",
        "BASH_ENV",
    ];
    assert!(reserved.iter().all(|n| reserved_env_name(n)));
    assert!(!reserved_env_name("SERVICE_TOKEN"));
}

#[test]
fn the_wire_carries_an_addon() {
    let set = parse_plugin_set(&json!({
        "v": 1,
        "items": [{ "id": "a1", "kind": "addon", "name": "telemetry", "version": 1, "definition": fx()["full"] }],
        "machine": { "claude": { "items": ["a1"] } },
        "projects": {}
    }))
    .unwrap();
    assert_eq!(set.items[0].kind, "addon");
    assert_eq!(set.machine["claude"].items, ["a1"]);
}
