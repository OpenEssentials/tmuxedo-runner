//! packages/protocol/fixtures/review.json: review's method names, limits and
//! parameter shapes, so the runner and the TypeScript side cannot drift.

use serde_json::Value;
use tmuxedo_protocol::review::*;

fn fx() -> Value {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../packages/protocol/fixtures/review.json"
    );
    serde_json::from_str(&std::fs::read_to_string(path).expect("fixture")).expect("json")
}

fn strs(v: &Value) -> Vec<&str> {
    v.as_array().unwrap().iter().map(|x| x.as_str().unwrap()).collect()
}

#[test]
fn methods_and_limits_match_the_fixture() {
    let f = fx();
    assert_eq!(f["feature"], FEATURE_REVIEW);
    assert_eq!(strs(&f["methods"]["read"]), READ_METHODS);
    assert_eq!(strs(&f["methods"]["mutating"]), MUTATING_METHODS);
    assert!(READ_METHODS.iter().chain(MUTATING_METHODS).all(|m| is_review_method(m)));
    assert!(!is_review_method("fs.read") && !is_review_method("git"));
    let l = &f["limits"];
    assert_eq!(l["sessionId"], MAX_SESSION_ID);
    assert_eq!(l["statusFiles"], STATUS_FILES_MAX);
    assert_eq!(l["patchBytes"], PATCH_MAX);
    assert_eq!(l["largeLines"], LARGE_LINES);
    assert_eq!(l["messageBytes"], MESSAGE_MAX);
    assert_eq!(l["branchBytes"], BRANCH_MAX);
    assert_eq!(strs(&f["methods"]["web"]), WEB_METHODS);
    assert!(WEB_METHODS.iter().all(|m| MUTATING_METHODS.contains(m)));
}

#[test]
fn params_match_the_fixture() {
    let f = fx();
    for row in f["params"]["valid"].as_array().unwrap() {
        let m = row["method"].as_str().unwrap();
        assert!(parse_read_params(m, &row["params"]).is_ok(), "{row}");
    }
    for row in f["params"]["invalid"].as_array().unwrap() {
        let m = row["method"].as_str().unwrap();
        let e = parse_read_params(m, &row["params"]).expect_err(&row.to_string());
        assert_eq!(e.0, row["code"].as_str().unwrap(), "{row}");
    }
}

#[test]
fn ship_params_match_the_fixture() {
    let f = fx();
    for row in f["ship"]["valid"].as_array().unwrap() {
        let m = row["method"].as_str().unwrap();
        assert!(parse_ship_params(m, &row["params"]).is_ok(), "{row}");
    }
    for row in f["ship"]["invalid"].as_array().unwrap() {
        let m = row["method"].as_str().unwrap();
        let e = parse_ship_params(m, &row["params"]).expect_err(&row.to_string());
        assert_eq!(e.0, row["code"].as_str().unwrap(), "{row}");
    }
    let long = "x".repeat(MESSAGE_MAX + 1);
    let e = parse_ship_params("git.commit", &serde_json::json!({ "pane": "p", "message": long })).unwrap_err();
    assert_eq!(e.0, "invalid_params");
    let p = parse_ship_params("git.discard", &serde_json::json!({ "chat": "c" })).unwrap();
    assert_eq!(
        p,
        ShipParams::Discard {
            target: Target::Chat("c".into())
        }
    );
    let p = parse_ship_params("git.push", &serde_json::json!({ "pane": "p" })).unwrap();
    assert_eq!(
        p,
        ShipParams::Push {
            target: Target::Pane("p".into()),
            pr: false,
            base: None
        }
    );
}
