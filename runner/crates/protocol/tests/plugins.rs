//! Plugins on the wire against packages/protocol/fixtures/plugins.json (the
//! TypeScript side checks the same file in test/plugins.test.ts): hello
//! dual-write, welcome features, config dual-write, ui items.

use serde_json::Value;
use tmuxedo_protocol::frames::{parse_gateway, GatewayFrame, Parsed, RunnerFrame};
use tmuxedo_protocol::plugins::{parse_config_plugins, parse_ui_items};

fn fx() -> Value {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../packages/protocol/fixtures/plugins.json"
    );
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn hellos_round_trip_exactly() {
    let f = fx();
    for name in ["legacy", "dual", "plugins_only", "disabled"] {
        let want = &f["hello"][name];
        let frame: RunnerFrame = serde_json::from_value(want.clone()).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(&serde_json::to_value(&frame).unwrap(), want, "{name}");
    }
    // The dual hello's legacy fields are exactly the legacy hello's.
    for k in ["claudeVersion", "claudeAuth", "features", "os", "arch"] {
        assert_eq!(
            f["hello"]["dual"]["runner"][k], f["hello"]["legacy"]["runner"][k],
            "{k}"
        );
    }
    // An older runner's hello (no plugins) still parses, with none.
    let RunnerFrame::Hello { runner, .. } = serde_json::from_value(f["hello"]["legacy"].clone()).unwrap() else {
        panic!("hello")
    };
    assert!(runner.plugins.is_none());
}

#[test]
fn welcome_features() {
    let f = fx();
    for (name, want) in [("new", Some(vec!["ui".to_string()])), ("old", None)] {
        match parse_gateway(&f["welcome"][name].to_string()) {
            Parsed::Frame(GatewayFrame::Welcome {
                features,
                last_event_seq,
                ..
            }) => {
                assert_eq!(features, want, "{name}");
                assert_eq!(last_event_seq, 4);
            }
            other => panic!("{name}: {other:?}"),
        }
    }
}

#[test]
fn config_plugins_win_over_claude_args() {
    let f = fx();
    for name in [
        "dual",
        "legacy",
        "plugins_only",
        "skill_off",
        "plugins_win",
        "keystroke_on",
        "usage_on",
        "usage_off",
    ] {
        let Parsed::Frame(GatewayFrame::Config { claude, plugins, .. }) = parse_gateway(&f["config"][name].to_string())
        else {
            panic!("{name}")
        };
        let from_plugins = plugins
            .as_ref()
            .and_then(parse_config_plugins)
            .and_then(|l| l.into_iter().find(|(id, _)| id == "claude"))
            .and_then(|(_, c)| c.launch_args);
        let args = from_plugins.or_else(|| claude.map(|c| c["args"].clone())).unwrap();
        assert_eq!(args, f["config"]["effective_args"][name], "{name}");
        let skill = plugins
            .as_ref()
            .and_then(parse_config_plugins)
            .and_then(|l| l.into_iter().find(|(id, _)| id == "claude"))
            .and_then(|(_, c)| c.skill)
            .unwrap_or(false);
        assert_eq!(Value::Bool(skill), f["config"]["effective_skill"][name], "{name}");
        // AI usage: on only when the config says so; absent (an older web) is off.
        for id in ["claude", "codex"] {
            let usage = plugins
                .as_ref()
                .and_then(parse_config_plugins)
                .and_then(|l| l.into_iter().find(|(i, _)| i == id))
                .and_then(|(_, c)| c.usage)
                .unwrap_or(false);
            assert_eq!(Value::Bool(usage), f["config"]["effective_usage"][name], "{name} {id}");
        }
        let keystroke = plugins
            .as_ref()
            .and_then(parse_config_plugins)
            .and_then(|l| l.into_iter().find(|(id, _)| id == "cursor"))
            .and_then(|(_, c)| c.keystroke);
        assert_eq!(
            keystroke.map(Value::Bool).unwrap_or(Value::Null),
            f["config"]["effective_keystroke"][name],
            "{name}"
        );
    }
}

#[test]
fn ui_items_parse_like_the_typescript_side() {
    let f = fx();
    let mut frames = vec![f["ui"]["claude"].clone()];
    frames.extend(f["ui"]["valid"].as_array().unwrap().iter().cloned());
    for fr in frames {
        let items = parse_ui_items(&fr["items"]).unwrap();
        assert_eq!(serde_json::to_value(&items).unwrap(), fr["items"], "{fr}");
        let frame: RunnerFrame = serde_json::from_value(fr.clone()).unwrap();
        assert_eq!(serde_json::to_value(&frame).unwrap(), fr);
    }
    let kept = parse_ui_items(&f["ui"]["dropped_items"]["frame"]["items"]).unwrap();
    assert_eq!(serde_json::to_value(&kept).unwrap(), f["ui"]["dropped_items"]["keep"]);
    let too_many: Vec<Value> = (0..21)
        .map(|i| serde_json::json!({"key": format!("k{i}"), "kind": "status_line", "text": "x"}))
        .collect();
    assert!(parse_ui_items(&Value::Array(too_many)).is_none());
}

#[test]
fn usage_alert_levels_parse_like_the_typescript_side() {
    let one = |alerts: Value| {
        let v = serde_json::json!([{ "id": "claude", "settings": { "usage": { "enabled": true, "alerts": alerts } } }]);
        parse_config_plugins(&v).unwrap()[0].1.usage_alerts.clone()
    };
    for case in fx()["usage_alerts"].as_array().unwrap() {
        let want = case["want"]
            .as_array()
            .map(|w| w.iter().map(|x| x.as_u64().unwrap() as u8).collect::<Vec<_>>());
        assert_eq!(one(case["alerts"].clone()), want, "{case}");
    }
}
