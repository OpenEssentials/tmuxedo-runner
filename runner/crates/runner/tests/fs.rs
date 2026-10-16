//! The file browser's reading methods through the gateway, in process
//! (docs/design/file-browser.md §3, §4): a real holder, the runner daemon
//! and a fake gateway. Roots come from the runner's records; the default
//! workspace's folder here is `/`, which is too broad and is reported as
//! refused; a workspace opened in a project folder becomes a root.

mod common;

use common::{options, start_holder, FakeGateway};
use serde_json::{json, Value};
use std::time::Duration;
use tmuxedo_runner::daemon;

const S: Duration = Duration::from_secs(10);

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fs_reads_through_the_gateway_from_registered_roots_only() {
    let dir = common::tempdir();
    let proj = dir.path().join("proj");
    std::fs::create_dir_all(proj.join("docs")).unwrap();
    std::fs::write(proj.join("docs/guide.md"), "# guide\n").unwrap();
    std::fs::write(proj.join(".env"), "TOKEN=1\n").unwrap();
    std::fs::write(dir.path().join("outside.txt"), "outside\n").unwrap();
    std::os::unix::fs::symlink(dir.path().join("outside.txt"), proj.join("link.txt")).unwrap();

    let holder = start_holder(dir.path());
    let gw = FakeGateway::start().await;
    let opts = options(dir.path(), &gw.url, holder.clone());
    let task = tokio::spawn(daemon::run(opts));
    let mut c = gw.accept(S).await.expect("runner dials out");
    let hello = c.hello(S).await;
    assert!(
        hello.to_string().contains("\"fs\""),
        "the hello advertises the fs feature: {hello}"
    );
    c.text_t(S, "topology").await;
    c.send(json!({"t":"welcome","machineId":"m1","userId":"u1"})).await;

    // Only the default workspace exists and its folder is `/`: nothing to browse, and it says why.
    c.send(json!({"t":"req","id":"a","method":"fs.roots","params":{}}))
        .await;
    let r = c.res("a").await;
    assert_eq!(r["result"]["roots"], json!([]), "{r}");
    assert_eq!(r["result"]["refused"][0]["why"], "too_broad", "{r}");
    assert_eq!(r["result"]["canWrite"], true);

    c.send(json!({"t":"req","id":"b","method":"workspace.create","params":{"cwd": proj.display().to_string(), "label": "proj"}}))
        .await;
    assert!(c.res("b").await["error"].is_null());
    c.send(json!({"t":"req","id":"c","method":"fs.roots","params":{}}))
        .await;
    let r = c.res("c").await;
    let roots = r["result"]["roots"].as_array().unwrap();
    assert_eq!(roots.len(), 1, "{r}");
    assert_eq!(
        (roots[0]["kind"].as_str(), roots[0]["writable"].as_bool()),
        (Some("workspace"), Some(true))
    );
    assert!(
        !r.to_string().contains(&*dir.path().to_string_lossy()),
        "no machine path in the answer: {r}"
    );
    let root = roots[0]["id"].as_str().unwrap().to_string();

    let mut call = async |id: &str, method: &str, params: Value| -> Value {
        c.send(json!({"t":"req","id":id,"method":method,"params":params})).await;
        c.res(id).await
    };
    let l = call("d", "fs.list", json!({"root": root, "path": ""})).await;
    let names: Vec<&str> = l["result"]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["docs", "link.txt"], "{l}");
    assert_eq!(l["result"]["hidden"], 1);
    assert_eq!(l["result"]["entries"][1]["kind"], "link");
    let f = call("e", "fs.read", json!({"root": root, "path": "docs/guide.md"})).await;
    assert_eq!(f["result"]["text"], "# guide\n", "{f}");
    let s = call("f", "fs.stat", json!({"root": root, "path": "docs/guide.md"})).await;
    assert_eq!(s["result"]["version"], f["result"]["version"]);
    let q = call("g", "fs.search", json!({"root": root, "query": "GUIDE"})).await;
    assert_eq!(
        q["result"]["matches"],
        json!([{ "path": "docs/guide.md", "kind": "file" }])
    );

    // Refusals, each with the contract's code.
    for (id, method, params, code) in [
        ("h1", "fs.read", json!({"root": root, "path": ".env"}), "file_refused"),
        (
            "h2",
            "fs.read",
            json!({"root": root, "path": "link.txt"}),
            "symlink_refused",
        ),
        (
            "h3",
            "fs.read",
            json!({"root": root, "path": "../outside.txt"}),
            "invalid_path",
        ),
        (
            "h4",
            "fs.read",
            json!({"root": root, "path": "/etc/hostname"}),
            "invalid_path",
        ),
        (
            "h5",
            "fs.read",
            json!({"root": "r0000000000000000", "path": "a"}),
            "root_unknown",
        ),
        (
            "h6",
            "fs.read",
            json!({"root": "/", "path": "etc/hostname"}),
            "invalid_params",
        ),
        (
            "h7",
            "fs.list",
            json!({"root": root, "path": "", "limit": 100000}),
            "invalid_params",
        ),
        (
            "h8",
            "fs.list",
            json!({"root": root, "path": "", "cursor": {"x": 1}}),
            "invalid_params",
        ),
        ("h9", "fs.search", json!({"root": root, "query": ""}), "invalid_params"),
        ("h10", "fs.read", json!({"root": root}), "invalid_params"),
        // A change without web's flags is refused by the runner itself (slice A2), and so is one
        // without its retry key.
        (
            "h11",
            "fs.write",
            json!({"root": root, "path": "docs/guide.md", "text": "x", "create": true, "op_id": "h11"}),
            "control_required",
        ),
        (
            "h12",
            "fs.delete",
            json!({"root": root, "path": "docs/guide.md"}),
            "invalid_params",
        ),
        // Not built: import and the root changes.
        ("h14", "fs.import", json!({"root": root}), "not_supported"),
        ("h13", "fs.root_add", json!({"path": "/"}), "not_supported"),
    ] {
        let r = call(id, method, params).await;
        assert_eq!(r["error"]["code"], code, "{method} {id}: {r}");
        assert!(!r.to_string().contains("outside.txt") || id == "h3", "{r}");
    }
    assert_eq!(
        std::fs::read_to_string(proj.join("docs/guide.md")).unwrap(),
        "# guide\n"
    );

    // Slice A2: a change with web's flags lands; without them, or on a secret name, it does not.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let w = call(
        "w1",
        "fs.write",
        json!({"root": root, "path": "docs/new.md", "text": "new\n", "create": true, "op_id": "w1", "control": true, "stepup_at": now}),
    )
    .await;
    assert_eq!(w["result"]["type"], "fs_written", "{w}");
    assert_eq!(std::fs::read_to_string(proj.join("docs/new.md")).unwrap(), "new\n");
    let r = call("w2", "fs.mkdir", json!({"root": root, "path": "made", "op_id": "w2"})).await;
    assert_eq!(r["error"]["code"], "control_required", "{r}");
    let r = call(
        "w3",
        "fs.mkdir",
        json!({"root": root, "path": "made", "op_id": "w3", "control": true}),
    )
    .await;
    assert_eq!(r["error"]["code"], "stepup_required", "{r}");
    assert!(!proj.join("made").exists());
    let r = call(
        "w4",
        "fs.write",
        json!({"root": root, "path": ".env", "text": "X=1", "create": true, "op_id": "w4", "control": true, "stepup_at": now}),
    )
    .await;
    assert_eq!(r["error"]["code"], "file_refused", "{r}");
    let u = call(
        "w5",
        "fs.undo",
        json!({"undoId": w["result"]["undoId"], "op_id": "w5", "control": true, "stepup_at": now}),
    )
    .await;
    assert_eq!(u["result"]["type"], "fs_undone", "{u}");
    assert!(!proj.join("docs/new.md").exists());
    // The kept versions and the journal are in the runner's state folder, not the project.
    assert!(!std::fs::read_dir(&proj)
        .unwrap()
        .flatten()
        .any(|e| e.file_name().to_string_lossy().contains("tmuxedo")));
    task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_observe_only_machine_reads_and_refuses_every_fs_mutation() {
    let dir = common::tempdir();
    let holder = start_holder(dir.path());
    let gw = FakeGateway::start().await;
    let mut opts = options(dir.path(), &gw.url, holder.clone());
    opts.guardrails = tmuxedo_runner::guardrails::Guardrails {
        allow_user: None,
        observe_only: true,
    };
    let task = tokio::spawn(daemon::run(opts));
    let mut c = gw.accept(S).await.expect("runner dials out");
    c.hello(S).await;
    c.text_t(S, "topology").await;
    c.send(json!({"t":"welcome","machineId":"m1","userId":"u1"})).await;
    c.send(json!({"t":"req","id":"a","method":"fs.roots","params":{}}))
        .await;
    let roots = c.res("a").await;
    assert_eq!(roots["result"]["type"], "fs_roots", "reading works while observing");
    assert_eq!(
        (roots["result"]["canWrite"].as_bool(), roots["result"]["why"].as_str()),
        (Some(false), Some("observe_only"))
    );
    for (i, m) in tmuxedo_protocol::fs::MUTATING_METHODS.iter().enumerate() {
        let id = format!("m{i}");
        c.send(json!({"t":"req","id":id,"method":m,"params":{"root":"r0000000000000000","path":"x","text":"y"}}))
            .await;
        assert_eq!(c.res(&id).await["error"]["code"], "observe_only", "{m}");
        // Web's flags change nothing here: the machine itself refuses.
        let id = format!("f{i}");
        c.send(json!({"t":"req","id":id,"method":m,"params":{"root":"r0000000000000000","path":"x","text":"y","create":true,"op_id":"o","control":true,"stepup_at":1}}))
            .await;
        assert_eq!(c.res(&id).await["error"]["code"], "observe_only", "{m}");
    }
    task.abort();
}
