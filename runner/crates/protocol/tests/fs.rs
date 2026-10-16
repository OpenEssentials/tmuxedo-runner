//! packages/protocol/fixtures/fs.json: the file browser's method names,
//! limits, the lexical path rule, cursors, parameter shapes and versions,
//! so the runner and the TypeScript side cannot drift.

use serde_json::Value;
use tmuxedo_protocol::fs::*;

fn fx() -> Value {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../packages/protocol/fixtures/fs.json"
    );
    serde_json::from_str(&std::fs::read_to_string(path).expect("fixture")).expect("json")
}

fn strs(v: &Value) -> Vec<&str> {
    v.as_array().unwrap().iter().map(|x| x.as_str().unwrap()).collect()
}

#[test]
fn methods_and_limits_match_the_fixture() {
    let f = fx();
    assert_eq!(strs(&f["methods"]["read"]), READ_METHODS);
    assert_eq!(strs(&f["methods"]["mutating"]), MUTATING_METHODS);
    assert!(READ_METHODS.iter().chain(MUTATING_METHODS).all(|m| is_fs_method(m)));
    assert!(!is_fs_method("files.list") && !is_fs_method("fs"));
    let l = &f["limits"];
    assert_eq!(l["pathBytes"], MAX_PATH_BYTES);
    assert_eq!(l["components"], MAX_COMPONENTS);
    assert_eq!(l["componentBytes"], MAX_COMPONENT_BYTES);
    assert_eq!(l["rootId"], MAX_ROOT_ID);
    assert_eq!(l["listPage"], LIST_MAX);
    assert_eq!(l["searchQuery"], SEARCH_QUERY_MAX);
    assert_eq!(l["searchResults"], SEARCH_MAX_RESULTS);
    assert_eq!(l["textBytes"], TEXT_MAX);
    assert_eq!(l["editableBytes"], EDITABLE_MAX);
    for c in ERROR_CODES {
        assert!(
            c.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
            "{c}"
        );
    }
}

#[test]
fn the_path_rule_refuses_and_never_cleans() {
    let f = fx();
    for p in strs(&f["paths"]["valid"]) {
        let parts = path_components(p).unwrap_or_else(|| panic!("{p:?} should be accepted"));
        assert_eq!(parts.join("/"), p, "nothing is changed");
    }
    for p in strs(&f["paths"]["invalid"]) {
        assert!(path_components(p).is_none(), "{p:?} should be refused");
    }
    let lc = &f["paths"]["longComponent"];
    let ch = lc["char"].as_str().unwrap();
    assert!(path_components(&ch.repeat(lc["valid"].as_u64().unwrap() as usize)).is_some());
    assert!(path_components(&ch.repeat(lc["invalid"].as_u64().unwrap() as usize)).is_none());
    let mc = &f["paths"]["manyComponents"];
    let deep = |n: u64| vec![mc["component"].as_str().unwrap(); n as usize].join("/");
    assert!(path_components(&deep(mc["valid"].as_u64().unwrap())).is_some());
    assert!(path_components(&deep(mc["invalid"].as_u64().unwrap())).is_none());
    // A path of exactly so many bytes: 200-byte components and a tail.
    let lp = &f["paths"]["longPath"];
    let of = |bytes: u64| {
        let mut s = String::new();
        let comp = "x".repeat(200);
        while (s.len() as u64) + 201 <= bytes {
            s.push_str(&comp);
            s.push('/');
        }
        let rest = bytes as usize - s.len();
        s.push_str(&"y".repeat(rest));
        s
    };
    let ok = of(lp["validBytes"].as_u64().unwrap());
    assert_eq!(ok.len(), 4096);
    assert!(path_components(&ok).is_some());
    assert!(path_components(&of(lp["invalidBytes"].as_u64().unwrap())).is_none());
}

#[test]
fn root_ids_and_cursors() {
    let f = fx();
    for id in strs(&f["rootIds"]["valid"]) {
        assert!(valid_root_id(id), "{id}");
    }
    for id in f["rootIds"]["invalid"].as_array().unwrap() {
        assert!(!id.as_str().is_some_and(valid_root_id), "{id}");
    }
    for c in f["cursors"]["valid"].as_array().unwrap() {
        let s = c[0].as_str().unwrap();
        let got = Cursor::parse(s).unwrap_or_else(|| panic!("{s:?}"));
        assert_eq!(
            (got.dir, got.name.as_str()),
            (c[1].as_bool().unwrap(), c[2].as_str().unwrap())
        );
        assert_eq!(got.encode(), s);
    }
    for c in f["cursors"]["invalid"].as_array().unwrap() {
        assert!(c.as_str().and_then(Cursor::parse).is_none(), "{c}");
    }
    assert!(Cursor::parse(&format!("f/{}", "n".repeat(255))).is_some());
    assert!(Cursor::parse(&format!("f/{}", "n".repeat(256))).is_none());
}

#[test]
fn hostile_parameters_are_refused_with_the_right_code() {
    let f = fx();
    for row in f["params"].as_array().unwrap() {
        let method = row["method"].as_str().unwrap();
        let got = parse_read_params(method, &row["params"]);
        match row["code"].as_str() {
            Some(code) => assert_eq!(got.as_ref().err().map(|e| e.0), Some(code), "{row}"),
            None => {
                let p = got.unwrap_or_else(|e| panic!("{row}: {e:?}"));
                let limit = match &p {
                    ReadParams::List { limit, .. } | ReadParams::Search { limit, .. } => Some(*limit as u64),
                    _ => None,
                };
                assert_eq!(limit, row["limit"].as_u64(), "{row}");
            }
        }
    }
    // Never a panic, whatever the shape.
    for junk in [
        "null",
        "[]",
        "7",
        "\"x\"",
        "{\"root\":{},\"path\":[]}",
        "{\"root\":\"r\",\"path\":\"a\",\"limit\":1e999}",
    ] {
        if let Ok(v) = serde_json::from_str::<Value>(junk) {
            for m in READ_METHODS.iter().chain(MUTATING_METHODS) {
                let _ = parse_read_params(m, &v);
            }
        }
    }
}

#[test]
fn versions_are_strict() {
    let f = fx();
    for v in f["versions"]["valid"].as_array().unwrap() {
        let s = v[0].as_str().unwrap();
        let got = Version::parse(s).unwrap_or_else(|| panic!("{s}"));
        assert_eq!(got.size.to_string(), v[1].as_str().unwrap());
        assert_eq!(got.mtime_ns.to_string(), v[2].as_str().unwrap());
        assert_eq!(got.encode(), s);
        assert!(got.same_content(&Version {
            mtime_ns: 1,
            ..got.clone()
        }));
    }
    for s in strs(&f["versions"]["invalid"]) {
        assert!(Version::parse(s).is_none(), "{s:?}");
    }
}

#[test]
fn write_methods_flags_and_limits_match_the_fixture() {
    let f = fx();
    let w = &f["write"];
    assert_eq!(strs(&w["methods"]), WRITE_METHODS);
    assert!(WRITE_METHODS.iter().all(|m| MUTATING_METHODS.contains(m)));
    assert_eq!(strs(&w["webFlags"]), WEB_FLAGS);
    assert_eq!(w["stepupWindowMs"].as_u64(), Some(STEPUP_WINDOW_MS));
    assert_eq!(w["stepupSkewMs"].as_u64(), Some(STEPUP_SKEW_MS));
    assert_eq!(w["opId"].as_u64(), Some(MAX_OP_ID as u64));
}

#[test]
fn write_parameters_are_refused_with_the_right_code() {
    let f = fx();
    for row in f["writeParams"].as_array().unwrap() {
        let method = row["method"].as_str().unwrap();
        let got = parse_write_params(method, &row["params"]);
        match row["code"].as_str() {
            Some(code) => assert_eq!(got.as_ref().err().map(|e| e.0), Some(code), "{row}"),
            None => {
                let r = got.unwrap_or_else(|e| panic!("{row}: {e:?}"));
                if let Some(create) = row.get("create").and_then(Value::as_bool) {
                    let WriteOp::Write { base, .. } = &r.op else {
                        panic!("{row}")
                    };
                    assert_eq!(base.is_none(), create, "{row}");
                }
            }
        }
    }
    // Text past the editable limit is too large (built here: the fixture stays small).
    let big = "a".repeat(EDITABLE_MAX as usize + 1);
    let p = serde_json::json!({"root": "r1", "path": "a", "text": big, "create": true, "op_id": "o"});
    assert_eq!(parse_write_params("fs.write", &p).err().map(|e| e.0), Some("too_large"));
    // Never a panic, whatever the shape.
    for junk in [
        "null",
        "[]",
        "7",
        "{\"root\":{},\"path\":[]}",
        "{\"undoId\":\"u\",\"op_id\":{}}",
    ] {
        let v: Value = serde_json::from_str(junk).unwrap();
        for m in MUTATING_METHODS {
            let _ = parse_write_params(m, &v);
        }
    }
}

#[test]
fn web_flags_decide_control_and_step_up() {
    let f = fx();
    for row in f["flagChecks"].as_array().unwrap() {
        let mut p = row["flags"].clone();
        p["undoId"] = "u0123456789abcdef".into();
        p["op_id"] = "o".into();
        let r = parse_write_params("fs.undo", &p).unwrap();
        let got = r.flags.check(row["now"].as_u64().unwrap());
        assert_eq!(got.err(), row["code"].as_str(), "{row}");
    }
}
