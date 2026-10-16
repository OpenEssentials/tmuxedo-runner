//! Secret delivery end to end on the runner side (docs/protocol.md
//! "Secrets"; secrets-and-env.md §3.3, §4): key registration, the policy and
//! mask list, `secret exec` with its allowlist, the git credential helper,
//! agent env, and what the gateway sees (ciphertext only).

use crate::common::{options, start_holder, Conn, FakeGateway, H};
use serde_json::{json, Value};
use sha2::Digest;
use std::time::Duration;
use tmuxedo_runner::daemon;
use tmuxedo_runner::secrets::seal;

const S: Duration = Duration::from_secs(20);
const BIN: &str = env!("CARGO_BIN_EXE_tmuxedo-runner");

fn fake(prefix: &str) -> String {
    let mut b = [0u8; 12];
    getrandom::getrandom(&mut b).unwrap();
    format!("{prefix}-{}", hex::encode(b))
}

fn sha(s: &str) -> String {
    hex::encode(sha2::Sha256::digest(s.as_bytes()))
}

/// Answer the next `secret_req` with `values` (sealed as web does).
async fn answer(c: &mut Conn, key: &str, values: Value) -> Value {
    let req = c.text(S, |v| v["t"] == "secret_req").await;
    let id = req["id"].as_str().unwrap().to_string();
    let versions: serde_json::Map<String, Value> = values
        .as_object()
        .unwrap()
        .keys()
        .map(|k| (k.clone(), json!(1)))
        .collect();
    let plain = json!({"values": values, "versions": versions}).to_string();
    let sealed = seal::seal(key, plain.as_bytes(), &seal::answer_context("m1", &id)).unwrap();
    c.send(json!({"t":"secret_res","id": id,"result":{"sealed": sealed,"missing":[],"denied":[]}}))
        .await;
    req
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn secrets_are_delivered_sealed_and_used_without_being_seen() {
    let token = fake("fake-token");
    let old = fake("old-value");
    let dir = crate::common::tempdir();
    let holder = start_holder(dir.path());
    let gw = FakeGateway::start().await;
    let o = options(dir.path(), &gw.url, holder);
    let sock = o.runner_socket.clone();
    let task = tokio::spawn(daemon::run(o));
    let mut c = gw.accept(S).await.expect("runner dials out");
    c.hello(S).await;
    // The key is registered after welcome; it stays the same across restarts (a 0600 file).
    let reg = c.text(S, |v| v["t"] == "secret_key").await;
    let key = reg["publicKey"].as_str().unwrap().to_string();
    assert_eq!(reg["alg"], seal::ALG);
    let key_file = dir.path().join("state/secret-key");
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&key_file).unwrap().permissions().mode() & 0o777,
        0o600
    );

    // Policy and the mask list (a value delivered before, under an older version).
    let mask = seal::seal(
        &key,
        json!({"values": [old]}).to_string().as_bytes(),
        &seal::mask_context("m1"),
    )
    .unwrap();
    c.send(json!({"t":"secrets","mask": mask,"policy":{"names":[
        {"name":"FAKE_TOKEN","bindings":[{"kind":"exec","programs":["printenv"]},{"kind":"git","hosts":["example.test"]},{"kind":"agent","plugin":"claude"}]}
    ],"offlineCache":false}}))
        .await;
    // Applied (the guard's hook lands in the generated settings while the frame is handled).
    crate::wait_file_has(&dir.path().join("state/claude-settings.json"), "PreToolUse").await;
    let h = H::attach_to(&sock).await;
    let run = |cmd: String| json!({"pane_id":"w1:p1","text": cmd + "\n"});

    // A value from the mask list is masked although it was never delivered here.
    h.ok(
        "pane.send_text",
        run(format!("echo 'old {old}'; echo OLD-END-$((40+2))")),
    )
    .await;
    let t = h.wait_text("w1:p1", "OLD-END-42").await;
    assert!(!t.contains(&old), "the mask list masks");

    // secret exec: an allowed program gets the value in its env; the pane shows it masked.
    h.ok(
        "pane.send_text",
        run(format!(
            "'{BIN}' cli secret exec FAKE_TOKEN -- printenv FAKE_TOKEN; echo EXIT-A=$?"
        )),
    )
    .await;
    let req = answer(&mut c, &key, json!({"FAKE_TOKEN": token})).await;
    assert_eq!(req["names"], json!(["FAKE_TOKEN"]));
    assert_eq!(req["purpose"]["kind"], "exec");
    assert!(req["purpose"]["program"].as_str().unwrap().ends_with("/printenv"));
    assert_eq!(req["purpose"]["paneId"], "w1:p1");
    let t = h.wait_text("w1:p1", "EXIT-A=0").await;
    assert!(!t.contains(&token), "the printed value is masked");
    assert!(
        t.contains(&"*".repeat(token.len())),
        "printenv printed it, same length, masked"
    );

    // A program that is not allowed is refused before web is asked.
    h.ok(
        "pane.send_text",
        run(format!("'{BIN}' cli secret exec FAKE_TOKEN -- env; echo EXIT-B=$?")),
    )
    .await;
    let t = h.wait_text("w1:p1", "EXIT-B=1").await;
    assert!(t.contains("not_allowed"), "{t}");
    assert!(t.contains("EXIT-B=126"));

    // The git helper and agent env read the cached value (no second request);
    // their output goes to a pipe, so the test compares a hash.
    h.ok(
        "pane.send_text",
        run(format!(
            "printf 'protocol=https\\nhost=example.test\\n\\n' | '{BIN}' cli secret git-credential get | sed -n 's/^password=//p' | tr -d '\\n' | sha256sum; echo GIT-END-$((40+2))"
        )),
    )
    .await;
    let t = h.wait_text("w1:p1", "GIT-END-42").await;
    assert!(t.contains(&sha(&token)), "git gets the token for its host");
    h.ok(
        "pane.send_text",
        run(format!(
            "printf 'protocol=https\\nhost=other.test\\n\\n' | '{BIN}' cli secret git-credential get | wc -c; echo GIT2-END-$((40+2))"
        )),
    )
    .await;
    let t = h.wait_text("w1:p1", "GIT2-END-42").await;
    assert!(t.lines().any(|l| l.trim() == "0"), "another host gets nothing: {t}");
    h.ok(
        "pane.send_text",
        run(format!(
            "eval \"$('{BIN}' cli secret agent-env claude)\"; printf %s \"$FAKE_TOKEN\" | sha256sum; echo ENV-END-$((40+2))"
        )),
    )
    .await;
    let t = h.wait_text("w1:p1", "ENV-END-42").await;
    assert!(t.contains(&sha(&token)));
    // The pane has the git helper configured (a secret is bound to git hosts)... in new panes.
    let split = h
        .ok("pane.split", json!({"target_pane_id":"w1:p1","direction":"right"}))
        .await;
    let p2 = split["pane"]["pane_id"].as_str().unwrap().to_string();
    h.wait_alive(&p2).await;
    h.ok(
        "pane.send_text",
        json!({"pane_id": p2, "text": "git --no-pager config --get-all credential.helper; echo HELPER-END-$((40+2))\n"}),
    )
    .await;
    let t = h.wait_text(&p2, "HELPER-END-42").await;
    assert!(t.contains("cli secret git-credential"), "{t}");

    // Outside a pane: refused.
    let out = std::process::Command::new(BIN)
        .args(["cli", "secret", "exec", "FAKE_TOKEN", "--", "printenv", "FAKE_TOKEN"])
        .env("TMUXEDO_SOCKET_PATH", &sock)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("forbidden"));
    assert!(!String::from_utf8_lossy(&out.stdout).contains(&token));
    let local = crate::common::call_once(
        &sock,
        &json!({"id":"x","method":"secret.agent_env","params":{"plugin":"claude"}}),
    )
    .await;
    assert_eq!(local["error"]["code"], "forbidden", "the test process is not in a pane");

    // Revocation drops the cached value: the next use asks web again.
    c.send(json!({"t":"secrets","revoked":["FAKE_TOKEN"]})).await;
    h.ok(
        "pane.send_text",
        run(format!(
            "'{BIN}' cli secret exec FAKE_TOKEN -- printenv FAKE_TOKEN; echo EXIT-C=$?"
        )),
    )
    .await;
    c.send_later_err().await;
    let t = h.wait_text("w1:p1", "EXIT-C=1").await;
    assert!(t.contains("EXIT-C=126") && t.contains("revoked"), "{t}");

    // What the gateway saw: never a value, in any frame.
    c.drain(Duration::from_millis(300)).await;
    for v in c.texts.iter() {
        let s = v.to_string();
        assert!(
            !s.contains(&token) && !s.contains(&old),
            "a text frame held a value: {}",
            v["t"]
        );
    }
    for f in c.bins.iter() {
        let s = String::from_utf8_lossy(&f.payload);
        assert!(!s.contains(&token) && !s.contains(&old), "a data frame held a value");
    }
    c.close(4003).await;
    let _ = tokio::time::timeout(S, task).await;
}

trait Refuse {
    async fn send_later_err(&mut self);
}

impl Refuse for Conn {
    /// Refuse the next request as web does for a revoked secret.
    async fn send_later_err(&mut self) {
        let req = self.text(S, |v| v["t"] == "secret_req").await;
        self.send(json!({"t":"secret_res","id": req["id"],"error":{"code":"secret_unavailable","message":"FAKE_TOKEN was revoked"}}))
            .await;
    }
}
