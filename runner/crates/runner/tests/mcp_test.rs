//! `mcp.test`: only a server in the kept set that the plugin has on is
//! started; the handshake and `tools/list` come back, secrets masked; a
//! server that fails says why with its stderr tail.

use std::time::Duration;

use serde_json::json;
use tmuxedo_runner::mcp_test::{find, mask, origin_of, test_http, test_stdio};

fn state() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    let set = json!({
        "v": 1,
        "items": [
            {"id": "srv1", "kind": "mcp", "name": "github", "version": 1,
             "definition": {"transport": "stdio", "command": ["npx", "-y", "example-server@1.2.3"]}},
            {"id": "srv2", "kind": "mcp", "name": "proj-only", "version": 1,
             "definition": {"transport": "stdio", "command": ["example"]}},
            {"id": "ins1", "kind": "instructions", "name": "notes", "version": 1, "definition": {"text": "hi"}}
        ],
        "machine": {"claude": {"items": ["srv1", "ins1"]}},
        "projects": {"github.com/example/app": {"codex": {"items": ["srv2"]}}}
    });
    tmuxedo_runner::plugin_set::apply(d.path(), Some(&set)).expect("kept");
    d
}

#[test]
fn only_a_server_the_plugin_has_on_somewhere_on_the_machine() {
    let d = state();
    assert_eq!(find(d.path(), "srv1", "claude").map(|i| i.name), Some("github".into()));
    assert!(find(d.path(), "srv1", "codex").is_none(), "not on for codex");
    assert_eq!(
        find(d.path(), "srv2", "codex").map(|i| i.name),
        Some("proj-only".into())
    );
    assert!(find(d.path(), "ins1", "claude").is_none(), "not an MCP server");
    assert!(find(d.path(), "nope", "claude").is_none());
    assert!(find(tempfile::tempdir().unwrap().path(), "srv1", "claude").is_none());
}

#[test]
fn origins_and_masking() {
    assert_eq!(
        origin_of("https://mcp.example.com/v1/sse?x=1").as_deref(),
        Some("https://mcp.example.com")
    );
    assert_eq!(
        origin_of("http://user:pw@192.0.2.5:8080/mcp").as_deref(),
        Some("http://192.0.2.5:8080")
    );
    assert_eq!(origin_of("ftp://example.com"), None);
    assert_eq!(
        mask("token abcd1234 and abc", &["abcd1234".into(), "abc".into()]),
        "token •••• and abc"
    );
    assert_eq!(test_http("not a url")["error"], "invalid");
}

const FAKE: &str = r#"
echo "starting with $EXAMPLE_TOKEN" >&2
while IFS= read -r l; do
  case "$l" in
    *'"initialize"'*) echo 'not json, a log line'; echo '{"jsonrpc":"2.0","id":1,"result":{"serverInfo":{"name":"fake","version":"1.0.0"}}}';;
    *'"tools/list"'*) echo '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"search","description":"Search with s3cr3t-value\nsecond line"},{"name":"create"}]}}';;
  esac
done
"#;

#[test]
fn a_local_server_answers_with_its_tools_and_secrets_never_come_back() {
    let argv: Vec<String> = ["/bin/sh", "-c", FAKE].map(String::from).to_vec();
    let env = vec![("EXAMPLE_TOKEN".to_string(), "s3cr3t-value".to_string())];
    let home = tempfile::tempdir().unwrap();
    let r = test_stdio(
        &argv,
        &env,
        &["s3cr3t-value".into()],
        home.path(),
        Duration::from_secs(10),
    );
    assert_eq!(r["ok"], true, "{r}");
    assert_eq!(r["server"], json!({"name": "fake", "version": "1.0.0"}));
    assert_eq!(
        r["tools"],
        json!([{"name": "search", "description": "Search with ••••"}, {"name": "create"}])
    );
    assert!(!r.to_string().contains("s3cr3t-value"));
}

#[test]
fn a_server_that_fails_says_why_with_its_stderr_masked() {
    let home = tempfile::tempdir().unwrap();
    let script = r#"echo "bad token $EXAMPLE_TOKEN" >&2; exit 3"#;
    let argv: Vec<String> = ["/bin/sh", "-c", script].map(String::from).to_vec();
    let env = vec![("EXAMPLE_TOKEN".to_string(), "s3cr3t-value".to_string())];
    let r = test_stdio(
        &argv,
        &env,
        &["s3cr3t-value".into()],
        home.path(),
        Duration::from_secs(10),
    );
    assert_eq!(r["ok"], false);
    assert_eq!(r["error"], "exited");
    assert_eq!(r["stderr"], json!(["bad token ••••"]));

    let silent: Vec<String> = ["/bin/sh", "-c", "sleep 30"].map(String::from).to_vec();
    let t = std::time::Instant::now();
    let r = test_stdio(&silent, &[], &[], home.path(), Duration::from_millis(500));
    assert_eq!(r["error"], "timeout");
    assert!(
        t.elapsed() < Duration::from_secs(5),
        "the server is killed at the deadline"
    );

    let r = test_stdio(
        &["/nonexistent/server".into()],
        &[],
        &[],
        home.path(),
        Duration::from_secs(1),
    );
    assert_eq!(r["error"], "start");
}
