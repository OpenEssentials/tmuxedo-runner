//! packages/protocol/fixtures/secrets.json from the runner's side: the
//! sealed vectors open (and only under their own context), and the frames
//! parse as the TypeScript side writes them.

use serde_json::Value;
use tmuxedo_protocol::frames::{parse_gateway, GatewayFrame, Parsed};
use tmuxedo_runner::secrets::seal::{self, RunnerKey};

fn fixture() -> Value {
    let p = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../packages/protocol/fixtures/secrets.json"
    );
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

#[test]
fn vectors_open_under_their_context_only() {
    let f = fixture();
    let k = RunnerKey::from_b64(f["runnerPrivate"].as_str().unwrap()).unwrap();
    assert_eq!(k.public_b64(), f["runnerPublic"]);
    for v in f["vectors"].as_array().unwrap() {
        let sealed = v["sealed"].as_str().unwrap();
        let ctx = v["context"].as_str().unwrap();
        let plain = k.open(sealed, ctx).unwrap_or_else(|e| panic!("{}: {e}", v["name"]));
        assert_eq!(
            plain.as_slice(),
            v["plain"].as_str().unwrap().as_bytes(),
            "{}",
            v["name"]
        );
        assert!(k.open(sealed, f["wrongContext"].as_str().unwrap()).is_err());
    }
    // Rust-sealed values open the same way (the TS side checks it opens them too).
    let s = seal::seal(f["runnerPublic"].as_str().unwrap(), b"x", "m|req:1").unwrap();
    assert_eq!(k.open(&s, "m|req:1").unwrap().as_slice(), b"x");
}

#[test]
fn frames_parse() {
    let f = fixture();
    for fr in f["frames"]["gatewayToRunner"].as_array().unwrap() {
        match parse_gateway(&fr.to_string()) {
            Parsed::Frame(GatewayFrame::SecretRes { id, result, error }) => {
                assert!(!id.is_empty());
                assert!(result.is_some() != error.is_some());
            }
            Parsed::Frame(GatewayFrame::Secrets { .. }) => {}
            other => panic!("{fr}: {other:?}"),
        }
    }
    for bad in f["frames"]["invalid"].as_array().unwrap() {
        assert!(
            matches!(parse_gateway(bad.as_str().unwrap()), Parsed::Invalid(_)),
            "{bad}"
        );
    }
    for fr in f["frames"]["runnerToGateway"].as_array().unwrap() {
        assert!(fr["t"].as_str().unwrap().starts_with("secret_"));
    }
}
