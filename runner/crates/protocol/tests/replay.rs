//! packages/protocol/fixtures/replay.json: replay's method names, limits and
//! parameter shapes, so the runner and the TypeScript side cannot drift.

use serde_json::Value;
use tmuxedo_protocol::replay::*;

fn fx() -> Value {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../packages/protocol/fixtures/replay.json"
    );
    serde_json::from_str(&std::fs::read_to_string(path).expect("fixture")).expect("json")
}

#[test]
fn methods_and_limits_match_the_fixture() {
    let f = fx();
    assert_eq!(f["feature"], FEATURE_REPLAY);
    let m: Vec<&str> = f["methods"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_str().unwrap())
        .collect();
    assert_eq!(m, METHODS);
    assert!(METHODS.iter().all(|m| is_replay_method(m)));
    assert!(!is_replay_method("pane.read"));
    let l = &f["limits"];
    assert_eq!(l["sessionId"], tmuxedo_protocol::review::MAX_SESSION_ID);
    assert_eq!(l["readBytes"], READ_BYTES_MAX);
    assert_eq!(l["readBytesDefault"], READ_BYTES_DEFAULT);
    assert_eq!(l["retentionDays"], RETENTION_DAYS);
    assert_eq!(l["maxBytes"], MAX_BYTES);
    assert_eq!(l["segmentBytes"], SEGMENT_BYTES);
    assert_eq!(l["segmentMs"], SEGMENT_MS);
}

#[test]
fn params_match_the_fixture() {
    let f = fx();
    for row in f["params"]["valid"].as_array().unwrap() {
        let m = row["method"].as_str().unwrap();
        assert!(parse_params(m, &row["params"]).is_ok(), "{row}");
    }
    for row in f["params"]["invalid"].as_array().unwrap() {
        let m = row["method"].as_str().unwrap();
        let e = parse_params(m, &row["params"]).expect_err(&row.to_string());
        assert_eq!(e.0, row["code"].as_str().unwrap(), "{row}");
    }
}

#[test]
fn cursors_round_trip_and_junk_is_refused() {
    let c = Cursor {
        part: 1_760_000_000_000,
        offset: 4096,
        at: 1_760_000_120_000,
    };
    assert_eq!(Cursor::parse(&c.to_text()), Some(c));
    for junk in ["", "1.2", "1.2.3.4", "a.b.c", "1..3", "-1.2.3", "12345678901234567.1.1"] {
        assert_eq!(Cursor::parse(junk), None, "{junk}");
    }
    let r = parse_params("replay.read", &serde_json::json!({"pane": "w1:p1", "from_ms": 7})).unwrap();
    assert!(matches!(
        r,
        Params::Read {
            from: From::At(7),
            limit: READ_BYTES_DEFAULT,
            markers_only: false,
            ..
        }
    ));
}
