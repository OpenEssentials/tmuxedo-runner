//! Files through the runner socket, with real processes (a holder and the
//! runner binary): web's `files.upload_init`, chunks as `file_put` + FILE
//! frames, the runner SIGKILLed between chunks and the upload resumed by a
//! new runner from what is on disk, completion with the whole-file SHA-256
//! and the durable `file_uploaded` event, a download as FILE frames, the
//! pane's `TMUXEDO_FILES_DIR`, the disk frame, and which caller may use
//! which method.

mod common;

use common::{FakeGateway, TOKEN};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tmuxedo_protocol::files::{FileFrame, CHUNK_MIN, FRAME_PAYLOAD};

const BIN: &str = env!("CARGO_BIN_EXE_tmuxedo-runner");
const S: Duration = Duration::from_secs(20);
const CS: usize = CHUNK_MIN as usize;

struct Killer(Child);
impl Drop for Killer {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn sha(b: &[u8]) -> String {
    hex::encode(Sha256::digest(b))
}

async fn req(c: &mut common::Conn, id: &str, method: &str, params: Value) -> Value {
    c.send(json!({"t": "req", "id": id, "method": method, "params": params}))
        .await;
    let id = id.to_string();
    c.text(S, move |v| v["t"] == "res" && v["id"] == id.as_str()).await
}

async fn send_chunk(c: &mut common::Conn, id: &str, body: &[u8], n: usize) -> Value {
    let chunk = &body[n * CS..((n + 1) * CS).min(body.len())];
    c.send(json!({"t": "file_put", "uploadId": id, "n": n, "size": chunk.len(), "sha256": sha(chunk)}))
        .await;
    for (i, part) in chunk.chunks(FRAME_PAYLOAD / 4).enumerate() {
        let f = FileFrame {
            id: id.into(),
            req: n as u32,
            offset: (n * CS + i * FRAME_PAYLOAD / 4) as u64,
            payload: part.to_vec(),
        };
        c.send_bin(f.encode().unwrap()).await;
    }
    let id = id.to_string();
    c.text(S, move |v| {
        v["t"] == "file_ack" && v["uploadId"] == id.as_str() && v["n"] == n
    })
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_upload_survives_a_runner_restart_and_a_download_comes_back_in_frames() {
    let dir = common::tempdir();
    let d = dir.path();
    let hsock = d.join("holder.sock");
    let rsock = d.join("runner.sock");
    let files_root = d.join("data/files");
    let _holder = {
        let child = Command::new(BIN)
            .args(["holder", "--socket"])
            .arg(&hsock)
            .env("HOME", d)
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(d.join("holder.log")).unwrap())
            .spawn()
            .unwrap();
        let until = Instant::now() + S;
        while std::os::unix::net::UnixStream::connect(&hsock).is_err() {
            assert!(Instant::now() < until, "holder did not start");
            std::thread::sleep(Duration::from_millis(50));
        }
        Killer(child)
    };
    let gw = FakeGateway::start().await;
    let runner = |n: u32| {
        Killer(
            Command::new(BIN)
                .args(["run", "--no-start-holder", "--server", &gw.url, "--shell", "sh"])
                .env("TMUXEDO_RUNNER_TOKEN", TOKEN)
                .env("TMUXEDO_STATE_DIR", d.join("state"))
                .env("TMUXEDO_HOLDER_SOCKET", &hsock)
                .env("TMUXEDO_RUNNER_SOCKET", &rsock)
                .env("TMUXEDO_CONFIG", d.join("runner.toml"))
                .env("TMUXEDO_FILES_ROOT", &files_root)
                // A disk with room, whatever the test machine has.
                .env("TMUXEDO_DISK_FAKE", "100000000000,200000000000")
                .env("HOME", d)
                .stdout(Stdio::null())
                .stderr(std::fs::File::create(d.join(format!("runner{n}.log"))).unwrap())
                .spawn()
                .unwrap(),
        )
    };
    let body: Vec<u8> = (0..CS * 2 + 4321).map(|i| (i % 251) as u8).collect();
    let up = "up_restart_test1";

    let mut r1 = runner(1);
    let (mut c, hello) = gw.accept_hello(S).await.expect("runner 1 dials out");
    c.send(json!({"t": "welcome", "machineId": "m1", "userId": "u1"})).await;
    assert!(
        hello["runner"]["features"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f == "files"),
        "{hello}"
    );
    // The disk, at once after the connection.
    let disk = c.text_t(S, "disk").await;
    assert_eq!(disk["level"], "ok");
    // web: files.upload_init (machine.call), then chunk 0.
    let init = req(
        &mut c,
        "i1",
        "files.upload_init",
        json!({"upload_id": up, "project": "github.com_org_repo", "name": "big data.bin", "size": body.len(), "chunk_size": CS, "by": "u1", "source": "web"}),
    )
    .await;
    assert_eq!(init["result"]["chunkCount"], 3, "{init}");
    let a = send_chunk(&mut c, up, &body, 0).await;
    assert_eq!(a["ok"], true, "{a}");
    assert_eq!(a["sha256"], sha(&body[..CS]));
    // Half of chunk 1, then the runner dies.
    let c1 = &body[CS..2 * CS];
    c.send(json!({"t": "file_put", "uploadId": up, "n": 1, "size": CS, "sha256": sha(c1)}))
        .await;
    c.send_bin(
        FileFrame {
            id: up.into(),
            req: 1,
            offset: CS as u64,
            payload: c1[..1000].to_vec(),
        }
        .encode()
        .unwrap(),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    // SAFETY: kill(2) on our own child.
    unsafe { libc::kill(r1.0.id() as i32, libc::SIGKILL) };
    let _ = r1.0.wait();

    let _r2 = runner(2);
    let (mut c, _) = gw.accept_hello(S).await.expect("runner 2 dials out");
    c.send(json!({"t": "welcome", "machineId": "m1", "userId": "u1"})).await;
    let st = req(&mut c, "s1", "files.upload_status", json!({"upload_id": up})).await;
    assert_eq!(st["result"]["received"], json!([0]), "{st}");
    for n in [2, 1] {
        let a = send_chunk(&mut c, up, &body, n).await;
        assert_eq!(a["ok"], true, "{a}");
    }
    // A damaged chunk is refused (nothing recorded), then resent fine.
    let done = req(
        &mut c,
        "c1",
        "files.upload_complete",
        json!({"upload_id": up, "sha256": sha(&body)}),
    )
    .await;
    let path = done["result"]["path"].as_str().expect("complete").to_string();
    assert!(
        Path::new(&path).starts_with(files_root.canonicalize().unwrap().join("github.com_org_repo/inbox")),
        "{path}"
    );
    assert!(path.ends_with("_big_data.bin"), "{path}");
    assert_eq!(std::fs::read(&path).unwrap(), body);
    let ev = c.text(S, |v| v["t"] == "event" && v["type"] == "file_uploaded").await;
    assert_eq!(ev["data"]["sha256"], sha(&body));
    assert_eq!(ev["paneId"], "");

    // A download: two windows of FILE frames, each followed by its res.
    let name = done["result"]["name"].as_str().unwrap().to_string();
    let mut got = vec![];
    let mut offset = 0usize;
    let mut n = 0;
    while offset < body.len() {
        let r = req(
            &mut c,
            &format!("r{n}"),
            "files.read",
            json!({"xfer": "dl_testtest", "n": n, "project": "github.com_org_repo", "dir": "inbox", "name": name, "offset": offset, "length": CS}),
        )
        .await;
        let bytes = r["result"]["bytes"].as_u64().expect("read") as usize;
        let frames: Vec<FileFrame> = c.files.drain(..).filter(|f| f.req == n).collect();
        let mut at = offset as u64;
        for f in frames {
            assert_eq!((f.id.as_str(), f.offset), ("dl_testtest", at));
            at += f.payload.len() as u64;
            got.extend(f.payload);
        }
        assert_eq!(at as usize, offset + bytes, "every frame arrived before the res");
        offset += bytes;
        n += 1;
    }
    assert_eq!(got, body);
    // Over the gateway, the agent's methods are refused; locally, web's are.
    let r = req(
        &mut c,
        "x1",
        "files.share",
        json!({"path": "/etc/hostname", "project": "p"}),
    )
    .await;
    assert_eq!(r["error"]["code"], "not_supported", "{r}");
    let local = common::call_once(
        &rsock,
        &json!({"id": "l1", "method": "files.read", "params": {"xfer": "dl_testtest"}}),
    )
    .await;
    assert_eq!(local["error"]["code"], "not_supported", "{local}");
    let local = common::call_once(
        &rsock,
        &json!({"id": "l2", "method": "files.list", "params": {"project": "github.com_org_repo", "dir": "inbox"}}),
    )
    .await;
    assert_eq!(local["result"]["files"][0]["name"], name.as_str(), "{local}");
    // The pane knows its files folder.
    let _ = common::call_once(
        &rsock,
        &json!({"id": "l3", "method": "pane.send_input", "params": {"pane_id": "w1:p1", "text": "printf 'FD=%s\\nRT=%s\\nEND\\n' \"$TMUXEDO_FILES_DIR\" \"$TMUXEDO_FILES_ROOT\"", "keys": ["Enter"]}}),
    )
    .await;
    // The paths can be longer than the pane is wide (macOS temp folders): the
    // rows are joined, so a wrapped path reads whole (the paths hold no spaces).
    let root = files_root.canonicalize().unwrap().display().to_string();
    let until = Instant::now() + S;
    loop {
        let r = common::call_once(
            &rsock,
            &json!({"id": "l4", "method": "pane.read", "params": {"pane_id": "w1:p1", "source": "recent", "format": "text"}}),
        )
        .await;
        let text = r["result"]["read"]["text"].as_str().unwrap_or("").to_string();
        let joined: String = text.lines().map(str::trim_end).collect();
        if joined.contains(&format!("RT={root}END")) {
            assert!(joined.contains(&format!("FD={root}/path_")), "{root}: {text}");
            break;
        }
        assert!(Instant::now() < until, "no FD=/RT= lines for {root}: {text}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_files_root_inside_a_repository_turns_files_off() {
    let h = common::H::start_with(|o| {
        let repo = o.state_dir.parent().unwrap().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        o.files_root = Some(repo.join("uploads"));
    })
    .await;
    let r = h.raw("files.quota", json!({})).await;
    assert_eq!(r["error"]["code"], "not_supported", "{r}");
    assert!(
        !h.dir.path().join("repo/uploads").exists(),
        "nothing made inside the repository"
    );
}

/// A pane that has closed (a stale `?pane=` link), or no project at all, gets a
/// code the app can act on and a sentence a person can read, never the
/// parameter names.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_closed_pane_or_no_project_says_so_in_words() {
    let h = common::H::start_with(|_| {}).await;
    let r = h.raw("files.list", json!({"pane_id": "w9:p9"})).await;
    assert_eq!(r["error"]["code"], "pane_gone", "{r}");
    let r = h.raw("files.list", json!({"session_id": "no-such-chat"})).await;
    assert_eq!(r["error"]["code"], "pane_gone", "{r}");
    let r = h.raw("files.list", json!({})).await;
    assert_eq!(r["error"]["code"], "no_project", "{r}");
    let msg = r["error"]["message"].as_str().unwrap_or_default();
    assert!(!msg.contains("pane_id") && !msg.contains("cwd"), "{r}");
}

fn ev(name: &str, extra: Value) -> Value {
    let mut e = json!({"hook_event_name": name, "session_id": "s-files"});
    for (k, v) in extra.as_object().unwrap() {
        e[k] = v.clone();
    }
    e
}

/// Design §2 measure 5: an agent that read an uploaded file in this turn and
/// then asks for a permission carries the file's name on the prompt
/// (`afterUpload`), so whoever approves from a phone sees it; the next turn
/// starts clean. And the Claude shim hands Claude the files folder and the
/// instructions only in a pane that has one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_permission_after_reading_an_upload_says_so_and_the_shim_adds_the_folder() {
    let dir = common::tempdir();
    let holder = common::start_holder(dir.path());
    let gw = FakeGateway::start().await;
    let o = common::options(dir.path(), &gw.url, holder);
    let sock = o.runner_socket.clone();
    let _task = tokio::spawn(tmuxedo_runner::daemon::run(o));
    let mut c = gw.accept(S).await.expect("dials out");
    c.hello(S).await;
    let _h = common::H::attach_to(&sock).await;
    let root = dir.path().join("files").canonicalize().unwrap();
    let hook = |e: Value| {
        let sock = sock.clone();
        tokio::spawn(async move {
            common::call_once(
                &sock,
                &json!({"id": "hook", "method": "hook.event", "params": {"paneId": "w1:p1", "event": e}}),
            )
            .await
        })
    };
    let _ = hook(ev(
        "SessionStart",
        json!({"cwd": dir.path().display().to_string(), "source": "startup"}),
    ))
    .await;
    let _ = hook(ev("UserPromptSubmit", json!({"prompt": "look at the screenshot"}))).await;
    let upload = root.join("path_x_0000/inbox/2026-10-08T10-00-00Z_shot.png");
    let _ = hook(ev(
        "PostToolUse",
        json!({"tool_name": "Read", "tool_input": {"file_path": upload.display().to_string()}}),
    ))
    .await;
    // A Read of any other file does not count.
    let _ = hook(ev(
        "PostToolUse",
        json!({"tool_name": "Read", "tool_input": {"file_path": "/etc/hostname"}}),
    ))
    .await;
    let _held = hook(ev(
        "PermissionRequest",
        json!({"tool_name": "Bash", "tool_input": {"command": "git push"}}),
    ));
    let st = c
        .text(S, |v| v["t"] == "status" && v["agent"]["status"] == "blocked")
        .await;
    assert_eq!(
        st["agent"]["prompt"]["afterUpload"], "2026-10-08T10-00-00Z_shot.png",
        "{st}"
    );
    // A new turn: nothing read yet, so nothing said.
    let _ = hook(ev("UserPromptSubmit", json!({"prompt": "now something else"}))).await;
    let _held2 = hook(ev(
        "PermissionRequest",
        json!({"tool_name": "Bash", "tool_input": {"command": "ls"}}),
    ));
    let st = c
        .text(S, |v| {
            v["t"] == "status" && v["agent"]["status"] == "blocked" && v["agent"]["prompt"]["input"]["command"] == "ls"
        })
        .await;
    assert!(st["agent"]["prompt"].get("afterUpload").is_none(), "{st}");

    // The shim: in a pane with a files folder, Claude gets it and the instructions; elsewhere nothing.
    let d = common::tempdir();
    let fake = d.path().join("bin");
    std::fs::create_dir_all(&fake).unwrap();
    std::fs::write(
        fake.join("claude"),
        "#!/bin/sh\nprintf 'ARGV:'; for a; do printf '<%s>' \"$a\"; done; echo\n",
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(fake.join("claude"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let shim = tmuxedo_runner::inject::install_with(d.path(), Path::new(BIN), &[]).unwrap();
    let path = format!("{}:{}:/usr/bin:/bin", shim.display(), fake.display());
    let run = |files: Option<(&str, &str)>| {
        let mut cmd = Command::new(shim.join("claude"));
        cmd.arg("hi").env_clear().env("PATH", &path);
        if let Some((dir, root)) = files {
            cmd.env("TMUXEDO_FILES_DIR", dir).env("TMUXEDO_FILES_ROOT", root);
        }
        String::from_utf8_lossy(&cmd.output().unwrap().stdout).into_owned()
    };
    let settings = d.path().join("claude-settings.json").display().to_string();
    assert_eq!(
        run(Some(("/f/my project", "/f"))),
        format!(
            "ARGV:<--add-dir></f/my project><--append-system-prompt-file></f/.instructions.md><--settings><{settings}><hi>\n"
        )
    );
    assert_eq!(run(None), format!("ARGV:<--settings><{settings}><hi>\n"));
}

/// `tmuxedo files request` is a needs-you item (Phase 13's one model): the
/// pane's set gets a `file` item answered by an upload; an upload naming the
/// request closes it (`by: remote, responseId: upload`), `dismiss` closes
/// another, `upload` is not an answer, and an open request is kept on disk
/// for a restart. The durable `file_requested` event names the item, so web
/// pushes the item instead of the event.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_file_request_is_a_needs_you_item_and_an_upload_answers_it() {
    let dir = common::tempdir();
    let holder = common::start_holder(dir.path());
    let gw = FakeGateway::start().await;
    let o = common::options(dir.path(), &gw.url, holder);
    let sock = o.runner_socket.clone();
    let state = o.state_dir.clone();
    let _task = tokio::spawn(tmuxedo_runner::daemon::run(o));
    let mut c = gw.accept(S).await.expect("dials out");
    c.text_t(S, "hello").await;
    c.send(json!({"t": "welcome", "machineId": "m1", "userId": "u1", "lastEventSeq": 0, "features": ["needs"]}))
        .await;
    let _h = common::H::attach_to(&sock).await;
    let cwd = dir.path().display().to_string();
    let ask = |what: &str| {
        let sock = sock.clone();
        let p =
            json!({"id": "rq", "method": "files.request", "params": {"what": what, "cwd": cwd, "pane_id": "w1:p1"}});
        async move { common::call_once(&sock, &p).await }
    };

    // ── asked: an event naming the item, the item in the pane's set ──
    let r = ask("the logo as SVG").await;
    let rid = r["result"]["requestId"].as_str().expect("requested").to_string();
    let project = r["result"]["project"].as_str().unwrap().to_string();
    let item_id = format!("file-{rid}");
    let ev = c.text(S, |v| v["t"] == "event" && v["type"] == "file_requested").await;
    assert_eq!(
        (ev["paneId"].clone(), ev["data"]["itemId"].clone()),
        (json!("w1:p1"), json!(item_id)),
        "{ev}"
    );
    let opened = c.text(S, |v| v["t"] == "event" && v["type"] == "needs_opened").await;
    let item = &opened["data"]["item"];
    assert_eq!(item["kind"], "file", "{opened}");
    assert_eq!(item["answerVia"], "upload");
    assert_eq!(item["detail"]["upload"], json!({"project": project, "requestId": rid}));
    assert_eq!(
        item["title"], "An agent wants a file: the logo as SVG",
        "a shell asked (no agent in the pane)"
    );
    let f = c
        .text(S, |v| {
            v["t"] == "needs" && v["paneId"] == "w1:p1" && v["items"].as_array().is_some_and(|i| !i.is_empty())
        })
        .await;
    assert_eq!(f["items"][0]["id"], item_id.as_str());
    // Kept for a restart.
    let kept = std::fs::read_to_string(state.join("file-requests.json")).unwrap();
    assert!(kept.contains(&rid), "{kept}");

    // ── `upload` is not an answer; an upload naming another request does not close it ──
    let r = req(
        &mut c,
        "n1",
        "needs.respond",
        json!({"itemId": item_id, "responseId": "upload"}),
    )
    .await;
    assert_eq!(r["error"]["code"], "invalid_params", "{r}");
    let body: Vec<u8> = (0..1000).map(|i| (i % 251) as u8).collect();
    let upload = |id: &'static str, request: String| json!({"upload_id": id, "project": project, "name": "logo.svg", "size": body.len(), "chunk_size": CS, "by": "u1", "source": "web", "request_id": request});
    let wrong = upload("up_wrong_request1", "someOtherReq1".into());
    assert_eq!(
        req(&mut c, "i0", "files.upload_init", wrong).await["result"]["chunkCount"],
        1
    );
    send_chunk(&mut c, "up_wrong_request1", &body, 0).await;
    req(
        &mut c,
        "c0",
        "files.upload_complete",
        json!({"upload_id": "up_wrong_request1", "sha256": sha(&body)}),
    )
    .await;
    c.text(S, |v| v["t"] == "event" && v["type"] == "file_uploaded").await;
    let l = common::call_once(&sock, &json!({"id": "l", "method": "needs.list", "params": {}})).await;
    assert!(l.to_string().contains(&item_id), "still open: {l}");

    // ── the upload that names it closes it ──
    req(
        &mut c,
        "i1",
        "files.upload_init",
        upload("up_answers_req01", rid.clone()),
    )
    .await;
    send_chunk(&mut c, "up_answers_req01", &body, 0).await;
    let done = req(
        &mut c,
        "c1",
        "files.upload_complete",
        json!({"upload_id": "up_answers_req01", "sha256": sha(&body)}),
    )
    .await;
    assert!(done["result"]["path"].as_str().is_some(), "{done}");
    let up = c
        .text(S, |v| {
            v["t"] == "event" && v["type"] == "file_uploaded" && v["data"]["requestId"] == rid.as_str()
        })
        .await;
    assert_eq!(up["data"]["project"], project.as_str());
    let res = c.text(S, |v| v["t"] == "event" && v["type"] == "needs_resolved").await;
    assert_eq!(
        (
            res["data"]["id"].clone(),
            res["data"]["by"].clone(),
            res["data"]["responseId"].clone(),
            res["data"]["kind"].clone()
        ),
        (json!(item_id), json!("remote"), json!("upload"), json!("file")),
        "{res}"
    );
    c.text(S, |v| {
        v["t"] == "needs" && v["paneId"] == "w1:p1" && v["items"] == json!([])
    })
    .await;
    assert!(!state.join("file-requests.json").exists(), "nothing left to keep");

    // ── another: Not now (dismiss) closes it; again is stale ──
    let r = ask("the CSV export").await;
    let rid2 = r["result"]["requestId"].as_str().unwrap().to_string();
    let id2 = format!("file-{rid2}");
    c.text(S, |v| v["t"] == "event" && v["type"] == "needs_opened").await;
    let r = req(
        &mut c,
        "n2",
        "needs.respond",
        json!({"itemId": id2, "paneId": "w1:p1", "responseId": "dismiss"}),
    )
    .await;
    assert_eq!(r["result"]["outcome"], "accepted", "{r}");
    let res = c.text(S, |v| v["t"] == "event" && v["type"] == "needs_resolved").await;
    assert_eq!(
        (res["data"]["id"].clone(), res["data"]["responseId"].clone()),
        (json!(id2), json!("dismiss"))
    );
    let r = req(
        &mut c,
        "n3",
        "needs.respond",
        json!({"itemId": id2, "responseId": "dismiss"}),
    )
    .await;
    assert_eq!(r["result"]["outcome"], "stale", "{r}");
    // The same words twice in a pane are one request: the newest (the id the agent was told).
    ask("a photo of the whiteboard").await;
    let b = ask("a photo of the whiteboard").await;
    let newest = format!("file-{}", b["result"]["requestId"].as_str().unwrap());
    let until = Instant::now() + S;
    loop {
        let l = common::call_once(&sock, &json!({"id": "l2", "method": "needs.list", "params": {}})).await;
        let ids: Vec<String> = l["result"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|i| i["kind"] == "file")
            .map(|i| i["id"].as_str().unwrap().to_string())
            .collect();
        if ids == vec![newest.clone()] {
            break;
        }
        assert!(Instant::now() < until, "{l}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
