//! `addon.test`: only the origins of an add-on in the kept plugin set, and
//! per origin whether this machine can open a TCP connection to it.

use std::net::TcpListener;
use std::time::Duration;

use serde_json::json;
use tmuxedo_runner::addon_test::{host_port, origins, reach, reach_all};

fn state_with(net: serde_json::Value) -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    let set = json!({
        "v": 1,
        "items": [{"id": "addon-example", "kind": "addon", "name": "example", "version": 1,
                   "definition": {"net": net, "agents": {"claude": {"env": {"EXAMPLE_LEVEL": "1"}}}}}],
        "machine": {"claude": {"items": ["addon-example"]}},
        "projects": {}
    });
    tmuxedo_runner::plugin_set::apply(d.path(), Some(&set)).expect("kept");
    d
}

#[test]
fn only_the_origins_of_an_addon_in_the_kept_set() {
    let d = state_with(json!(["https://telemetry.example.com", "http://192.0.2.10:8080"]));
    assert_eq!(
        origins(d.path(), "example"),
        Some(vec![
            "https://telemetry.example.com".to_string(),
            "http://192.0.2.10:8080".to_string()
        ])
    );
    assert_eq!(origins(d.path(), "other"), None);
    assert_eq!(origins(tempfile::tempdir().unwrap().path(), "example"), None);
}

#[test]
fn origins_become_host_and_port() {
    assert_eq!(host_port("https://a.example.com"), Some(("a.example.com".into(), 443)));
    assert_eq!(host_port("http://a.example.com"), Some(("a.example.com".into(), 80)));
    assert_eq!(host_port("http://192.0.2.10:8080"), Some(("192.0.2.10".into(), 8080)));
    assert_eq!(host_port("ftp://a.example.com"), None);
    assert_eq!(host_port("https://a.example.com:0"), None);
}

#[test]
fn a_listening_origin_connects_a_closed_one_is_refused_an_unknown_name_fails_lookup() {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let open = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
    let closed = {
        let c = TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://127.0.0.1:{}", c.local_addr().unwrap().port())
    };
    let r = reach_all(&[open.clone(), closed.clone()], Duration::from_secs(3));
    assert_eq!(r[0]["origin"], open);
    assert_eq!(r[0]["ok"], true);
    assert!(r[0]["ms"].is_u64());
    assert_eq!(r[1]["origin"], closed);
    assert_eq!(r[1]["ok"], false);
    assert_eq!(r[1]["error"], "refused");
    let dns = reach("https://no-such-host.invalid", Duration::from_secs(3));
    assert_eq!(dns["ok"], false);
    assert_eq!(dns["error"], "dns");
    assert_eq!(reach("ftp://x", Duration::from_secs(1))["error"], "invalid");
}
