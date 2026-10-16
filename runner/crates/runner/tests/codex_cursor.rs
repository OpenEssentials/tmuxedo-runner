//! PLAN Phase 16 end to end, through a real daemon, the real holder, the
//! generated shims and the real `tmuxedo-runner hook`, with scripted fake
//! `codex` and Cursor `agent` binaries and a fake gateway:
//!
//! - Codex: hooks arrive as `-c 'hooks.<Event>=[…]'`; working → blocked
//!   (held `PermissionRequest`) → allowed from the gateway → a second prompt
//!   given back to the terminal when someone types → done; `hello.plugins`
//!   carries Codex's version and login; a login file appearing raises
//!   `auth_ok {plugin: codex}`; a refresh token reused elsewhere on screen
//!   raises `auth_failed {reason: refresh_reused}`; the device code is
//!   lifted to the user's device with the code.
//! - Cursor: hooks from the `--plugin-dir` plugin, `NO_OPEN_BROWSER` set, the
//!   sign-in link lifted; a dialog on screen is not decidable until the
//!   user opts in, then a stale decision is refused and a fresh one sends
//!   exactly `y`.

mod common;

use common::{options, start_holder, Conn, FakeGateway, H};
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;
use tmuxedo_runner::daemon;

const S: Duration = Duration::from_secs(20);

const FAKE_CODEX: &str = r#"#!/bin/sh
if [ "$1" = "--version" ]; then echo "codex-cli 0.160.0"; exit 0; fi
if [ "$1" = "login" ] && [ "$2" = "status" ]; then
  if [ -f "$HOME/.codex/auth.json" ]; then echo "Logged in using ChatGPT"; exit 0; fi
  echo "Not logged in"; exit 1
fi
cmd=""; n=0
while [ $# -gt 0 ]; do
  case "$1" in
    -c) case "$2" in hooks.*=*) n=$((n+1)); [ -n "$cmd" ] || cmd=$(printf '%s' "$2" | sed -n 's/.*command="\(.*\)"}]}]$/\1/p') ;; esac; shift 2 ;;
    *) shift ;;
  esac
done
[ -n "$cmd" ] || { echo "FAKE: no hooks"; exit 3; }
echo "FAKE CODEX up with $n hooks"
hook() { printf '%s' "$1" | eval "$cmd"; }
sid=019a-fake-codex
printf 'prompt> '
read line
hook '{"hook_event_name":"SessionStart","session_id":"'$sid'","cwd":"'"$PWD"'","source":"startup"}'
hook '{"hook_event_name":"UserPromptSubmit","session_id":"'$sid'","turn_id":"t1","prompt":"x"}'
out=$(hook '{"hook_event_name":"PermissionRequest","session_id":"'$sid'","turn_id":"t1","tool_name":"Bash","tool_input":{"command":"touch x","description":"d"}}')
echo "DECISION1:$out"
hook '{"hook_event_name":"PostToolUse","session_id":"'$sid'","tool_name":"Bash"}'
hook '{"hook_event_name":"SubagentStart","session_id":"'$sid'","turn_id":"t1"}'
hook '{"hook_event_name":"SubagentStop","session_id":"'$sid'","turn_id":"t1"}'
out2=$(hook '{"hook_event_name":"PermissionRequest","session_id":"'$sid'","turn_id":"t1","tool_name":"Bash","tool_input":{"command":"rm y"}}')
echo "DECISION2:[$out2]"
printf 'type here> '
read line2
hook '{"hook_event_name":"Stop","session_id":"'$sid'","turn_id":"t1"}'
echo "FAKE DONE"
echo "Your access token could not be refreshed because your refresh token was already used. Please log out and sign in again."
echo "1. Open this link in your browser and sign in to your account"
echo "   https://auth.openai.com/codex/device"
echo "2. Enter this one-time code (expires in 15 minutes)"
echo "   EEYJ-1J3G7"
read line3
"#;

const FAKE_CURSOR: &str = r#"#!/bin/sh
if [ "$1" = "--version" ]; then echo "2026.10.01-e373342"; exit 0; fi
if [ "$1" = "status" ]; then echo '{"status":"unauthenticated","isAuthenticated":false}'; exit 0; fi
dir=""
while [ $# -gt 0 ]; do
  case "$1" in
    --plugin-dir) dir="$2"; shift 2 ;;
    *) shift ;;
  esac
done
cmd=$(sed -n 's/.*"command": "\([^"]*\)".*/\1/p' "$dir/hooks/hooks.json" | head -n 1)
[ -n "$cmd" ] || { echo "FAKE: no hooks"; exit 3; }
hook() { printf '%s' "$1" | eval "$cmd"; }
cid=0f7c-fake-cursor
echo "NO_OPEN_BROWSER=$NO_OPEN_BROWSER"
echo "If your browser didn't open, click this link to log in:"
echo "https://cursor.com/loginDeepControl?challenge=Zm9v&uuid=8c1b9a4e&mode=login&redirectTarget=cli"
# The real CLI keeps the link up while it polls; give the runner a look at it.
sleep 2
hook '{"hook_event_name":"sessionStart","conversation_id":"'$cid'"}'
hook '{"hook_event_name":"beforeShellExecution","conversation_id":"'$cid'","command":"npm install","cwd":"'"$PWD"'"}'
printf '\033[2J\033[H  Waiting for approval...\n\n  Run this command?\n  $ npm install\n\n   → Run (once) (y)\n     Skip (esc or n)\n'
read go
printf '\033[2J\033[H  Waiting for approval...\n\n  Run this command?\n  $ rm -rf build\n\n   → Run (once) (y)\n     Skip (esc or n)\n'
stty -icanon -echo
k=$(dd bs=1 count=1 2>/dev/null)
stty icanon echo
printf '\033[2J\033[HANSWER:[%s]\n' "$k"
hook '{"hook_event_name":"afterShellExecution","conversation_id":"'$cid'","command":"rm -rf build"}'
hook '{"hook_event_name":"stop","conversation_id":"'$cid'","status":"completed"}'
echo "FAKE CURSOR DONE"
read rest
"#;

fn real_bin() -> &'static Path {
    static D: OnceLock<PathBuf> = OnceLock::new();
    D.get_or_init(|| {
        // Under a directory named like Cursor's install: the `agent` shim
        // wraps only a binary whose path says cursor-agent.
        let d = std::env::temp_dir().join(format!("tmuxedo-p16-{}/cursor-agent-bin", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        for (name, body) in [
            ("codex", FAKE_CODEX),
            ("agent", FAKE_CURSOR),
            ("cursor-agent", FAKE_CURSOR),
        ] {
            std::fs::write(d.join(name), body).unwrap();
            std::fs::set_permissions(d.join(name), std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        // A thread that forks while a script is still open for writing holds
        // that descriptor until it execs, and running the script meanwhile
        // fails with "text file busy". The runner's version probe takes that
        // as no version (and the hello then has no auth either), so wait
        // here until each one runs.
        for name in ["codex", "agent", "cursor-agent"] {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            loop {
                match std::process::Command::new(d.join(name)).arg("--version").output() {
                    Err(e) if e.raw_os_error() == Some(26) && std::time::Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(20))
                    }
                    r => {
                        r.unwrap_or_else(|e| panic!("{name}: {e}"));
                        break;
                    }
                }
            }
        }
        std::env::set_var(
            "PATH",
            format!("{}:{}", d.display(), std::env::var("PATH").unwrap_or_default()),
        );
        d
    })
}

struct Rig {
    _dir: tempfile::TempDir,
    home: PathBuf,
    c: Conn,
    h: H,
    hello: Value,
    task: tokio::task::JoinHandle<Result<daemon::ExitReason, String>>,
}

async fn rig() -> Rig {
    real_bin();
    let dir = common::tempdir();
    let holder = start_holder(dir.path());
    let gw = FakeGateway::start().await;
    let mut o = options(dir.path(), &gw.url, holder);
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    o.home = home.to_string_lossy().into_owned();
    let sock = o.runner_socket.clone();
    let task = tokio::spawn(daemon::run(o));
    let mut c = gw.accept(S).await.unwrap();
    let hello = c.text_t(S, "hello").await;
    c.send(json!({"t": "welcome", "machineId": "m1", "userId": "u1"})).await;
    c.text_t(S, "topology").await;
    let h = H::attach_to(&sock).await;
    Rig {
        _dir: dir,
        home,
        c,
        h,
        hello,
        task,
    }
}

fn plugin<'a>(hello: &'a Value, id: &str) -> &'a Value {
    hello["runner"]["plugins"]
        .as_array()
        .and_then(|a| a.iter().find(|p| p["id"] == id))
        .unwrap_or_else(|| panic!("no plugin {id} in {hello}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn codex_through_hooks_release_on_typing_login_health_and_device_code() {
    let mut r = rig().await;
    // hello: Codex installed (its version), signed out (no ~/.codex/auth.json).
    let codex = plugin(&r.hello, "codex");
    assert_eq!(codex["state"], "enabled");
    assert_eq!(codex["agentVersion"], "0.160.0");
    assert_eq!(codex["auth"]["state"], "missing", "{codex}");
    assert_eq!(plugin(&r.hello, "cursor")["agentVersion"], "2026.10.01-e373342");

    r.h.ok("pane.send_text", json!({"pane_id": "w1:p1", "text": "codex\n"}))
        .await;
    let up = r.h.wait_text("w1:p1", "prompt>").await;
    assert!(up.contains("FAKE CODEX up with 9 hooks"), "{up}");
    r.h.ok("pane.send_text", json!({"pane_id": "w1:p1", "text": "go\n"}))
        .await;
    let st =
        r.c.text(S, |v| v["t"] == "status" && v["agent"]["status"] == "blocked")
            .await;
    assert_eq!(st["agent"]["kind"], "codex");
    assert_eq!(st["agent"]["sessionId"], "019a-fake-codex");
    assert_eq!(st["agent"]["prompt"]["input"]["command"], "touch x");
    let p1 = st["agent"]["promptId"].as_str().unwrap().to_string();
    r.c.send(json!({"t": "decision", "paneId": "w1:p1", "promptId": p1, "behavior": "allow"}))
        .await;
    let t = r.h.wait_text("w1:p1", "DECISION1:").await.replace('\n', "");
    assert!(t.contains(r#""behavior":"allow""#), "{t}");

    // A helper Codex ran in between (Phase 18): numbered by the core, since
    // Codex's hooks name none, and finished by its stop.
    let is = |v: &Value, t: &str| v["t"] == "event" && v["type"] == t;
    let ev = r.c.text(S, |v| is(v, "subagent_started")).await;
    assert_eq!(ev["data"]["kind"], "codex");
    assert_eq!(ev["data"]["childId"], "helper-1");
    let ev = r.c.text(S, |v| is(v, "subagent_completed")).await;
    assert_eq!(ev["data"]["childId"], "helper-1");

    // The second prompt is held (Codex shows no dialog meanwhile); typing in
    // the pane gives it back with no decision.
    let st2 =
        r.c.text(S, |v| {
            v["t"] == "status" && v["agent"]["status"] == "blocked" && v["agent"]["promptId"] != p1.as_str()
        })
        .await;
    assert_eq!(st2["agent"]["prompt"]["input"]["command"], "rm y");
    r.c.input("w1:p1", "z").await;
    let t = r.h.wait_text("w1:p1", "DECISION2:").await;
    assert!(t.contains("DECISION2:[]"), "{t}");
    r.h.wait_text("w1:p1", "type here>").await;
    r.h.ok("pane.send_text", json!({"pane_id": "w1:p1", "text": "\n"}))
        .await;
    r.c.text(S, |v| v["t"] == "status" && v["agent"]["status"] == "done")
        .await;

    // A reused refresh token on screen: failed, with its own reason.
    let ev = r.c.text(S, |v| v["t"] == "event" && v["type"] == "auth_failed").await;
    assert_eq!(ev["data"]["plugin"], "codex");
    assert_eq!(ev["data"]["reason"], "refresh_reused");
    // The device code goes to the user's device with its link.
    let ou = r.c.text(S, |v| v["t"] == "event" && v["type"] == "open_url").await;
    assert_eq!(ou["data"]["url"], "https://auth.openai.com/codex/device");
    assert_eq!(ou["data"]["code"], "EEYJ-1J3G7");
    assert_eq!(ou["data"]["source"], "screen");
    assert_eq!(ou["data"]["program"], "codex");

    // Signing in again (a new login file) clears it.
    std::fs::create_dir_all(r.home.join(".codex")).unwrap();
    std::fs::write(
        r.home.join(".codex/auth.json"),
        format!(r#"{{"last_refresh":"{}"}}"#, tmuxedo_runner::util::iso_now()),
    )
    .unwrap();
    let ok =
        r.c.text(Duration::from_secs(30), |v| {
            v["t"] == "event" && v["type"] == "auth_ok" && v["data"]["source"] != "hook"
        })
        .await;
    assert_eq!(ok["data"]["plugin"], "codex");
    assert!(
        ["credentials present", "credentials renewed", "status: logged in"]
            .contains(&ok["data"]["reason"].as_str().unwrap()),
        "{ok}"
    );
    r.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cursor_plugin_dir_link_lift_and_opt_in_keystroke_answers() {
    let mut r = rig().await;
    let cursor = plugin(&r.hello, "cursor");
    assert_eq!(cursor["auth"]["state"], "missing", "{cursor}");

    r.h.ok("pane.send_text", json!({"pane_id": "w1:p1", "text": "agent\n"}))
        .await;
    let t = r.h.wait_text("w1:p1", "NO_OPEN_BROWSER=").await;
    assert!(t.contains("NO_OPEN_BROWSER=1"), "{t}");
    let ou = r.c.text(S, |v| v["t"] == "event" && v["type"] == "open_url").await;
    assert!(
        ou["data"]["url"]
            .as_str()
            .unwrap()
            .starts_with("https://cursor.com/loginDeepControl?challenge=Zm9v&uuid=8c1b9a4e"),
        "{ou}"
    );
    assert!(ou["data"].get("code").is_none());

    // Blocked on screen, but not decidable: the user has not opted in.
    let st =
        r.c.text(S, |v| v["t"] == "status" && v["agent"]["status"] == "blocked")
            .await;
    assert_eq!(st["agent"]["kind"], "cursor");
    assert!(st["agent"]["promptId"].is_null(), "{st}");
    // Opt in (web: settings.answers.keystroke): the dialog already on screen becomes decidable.
    r.c.send(json!({"t": "config", "plugins": [
        {"id": "cursor", "settings": {"keystroke": {"enabled": true}}}
    ]}))
    .await;
    let first =
        r.c.text(S, |v| {
            v["t"] == "status"
                && v["agent"]["promptId"].is_string()
                && v["agent"]["prompt"]["input"]["command"]
                    .as_str()
                    .is_some_and(|c| c.contains("npm install"))
        })
        .await;
    let p1 = first["agent"]["promptId"].as_str().unwrap().to_string();
    // The next dialog replaces it: a new prompt; a decision for the old one is stale.
    r.h.ok("pane.send_text", json!({"pane_id": "w1:p1", "text": "\n"}))
        .await;
    let st =
        r.c.text(S, |v| {
            v["t"] == "status"
                && v["agent"]["promptId"].is_string()
                && v["agent"]["prompt"]["input"]["command"]
                    .as_str()
                    .is_some_and(|c| c.contains("rm -rf build"))
        })
        .await;
    assert_eq!(st["agent"]["prompt"]["tool"], "Shell");
    assert_eq!(st["agent"]["prompt"]["input"]["answer"], "keystroke");
    let p = st["agent"]["promptId"].as_str().unwrap().to_string();
    assert_ne!(p, p1);
    r.c.send(json!({"t": "decision", "paneId": "w1:p1", "promptId": p1, "behavior": "allow"}))
        .await;
    r.c.text(S, |v| {
        v["t"] == "event" && v["type"] == "decision_stale" && v["data"]["promptId"] == p1.as_str()
    })
    .await;
    // The live one sends exactly `y`.
    r.c.send(json!({"t": "decision", "paneId": "w1:p1", "promptId": p, "behavior": "allow"}))
        .await;
    let t = r.h.wait_text("w1:p1", "ANSWER:").await;
    assert!(t.contains("ANSWER:[y]"), "{t}");
    r.c.text(S, |v| v["t"] == "status" && v["agent"]["status"] == "done")
        .await;
    assert!(std::fs::read_to_string(r._dir.path().join("state/keystroke.json"))
        .unwrap()
        .contains("\"cursor\": true"));
    // The generated plugin dir: Cursor's hooks schema, our hook command.
    let hooks: Value = serde_json::from_str(
        &std::fs::read_to_string(r._dir.path().join("state/cursor-plugin/hooks/hooks.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(hooks["version"], 1);
    assert!(hooks["hooks"]["sessionStart"][0]["command"]
        .as_str()
        .unwrap()
        .ends_with("hook --plugin cursor"));
    r.task.abort();
}

// ── resume after a holder kill (real processes) ──────────────────────────

use common::TOKEN;
use std::process::{Child, Command, Stdio};
use std::time::Instant;
const BIN_R: &str = env!("CARGO_BIN_EXE_tmuxedo-runner");

struct Killer(Child);
impl Drop for Killer {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn holder(sock: &Path, log: &Path) -> Killer {
    let child = Command::new(BIN_R)
        .args(["holder", "--socket"])
        .arg(sock)
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(log).unwrap())
        .spawn()
        .unwrap();
    let until = Instant::now() + S;
    while std::os::unix::net::UnixStream::connect(sock).is_err() {
        assert!(Instant::now() < until, "holder did not start");
        std::thread::sleep(Duration::from_millis(50));
    }
    Killer(child)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn codex_and_cursor_resume_after_a_holder_kill() {
    let dir = common::tempdir();
    let d = dir.path();
    let home = d.join("home");
    // Named like Cursor's install: the `agent` shim wraps only Cursor's binary.
    let real = d.join("cursor-agent-bin");
    let a = d.join("project-a");
    let b = d.join("project-b");
    for p in [&home, &real, &a, &b] {
        std::fs::create_dir_all(p).unwrap();
    }
    for (name, tag) in [("codex", "CODEX"), ("agent", "CURSOR")] {
        std::fs::write(
            real.join(name),
            format!("#!/bin/sh\n[ \"$1\" = --version ] && {{ echo 1.0.0; exit 0; }}\necho \"RESUMED-{tag}: $* in $PWD\"\nexec sleep 600\n"),
        )
        .unwrap();
        std::fs::set_permissions(real.join(name), std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let cfg = d.join("runner.toml");
    std::fs::write(
        &cfg,
        format!("server = 'http://127.0.0.1:9'\ntoken = '{TOKEN}'\nauto_update = false\n"),
    )
    .unwrap();
    let hsock = d.join("h.sock");
    let rsock = d.join("runner.sock");
    let gw = FakeGateway::start().await;
    // Not the process PATH: the other tests here put their own fakes on it.
    let path = format!("{}:/usr/local/bin:/usr/bin:/bin", real.display());
    let runner = |n: u32| {
        Killer(
            Command::new(BIN_R)
                .args(["run", "--no-start-holder", "--server", &gw.url, "--shell", "sh", "-i"])
                .env("TMUXEDO_RUNNER_TOKEN", TOKEN)
                .env("TMUXEDO_STATE_DIR", d.join("state"))
                .env("TMUXEDO_HOLDER_SOCKET", &hsock)
                .env("TMUXEDO_RUNNER_SOCKET", &rsock)
                .env("TMUXEDO_CONFIG", &cfg)
                .env("HOME", &home)
                .env("PATH", &path)
                .stdout(Stdio::null())
                .stderr(std::fs::File::create(d.join(format!("runner{n}.log"))).unwrap())
                .spawn()
                .unwrap(),
        )
    };

    let mut h1 = holder(&hsock, &d.join("holder1.log"));
    let _r1 = runner(1);
    let mut c = gw.accept(S * 2).await.expect("runner connects");
    c.hello(S).await;
    let h = H::attach_to(&rsock).await;
    h.ok("workspace.create", json!({"cwd": a.to_string_lossy()})).await;
    h.ok("workspace.create", json!({"cwd": b.to_string_lossy()})).await;
    h.wait_alive("w2:p1").await;
    h.wait_alive("w3:p1").await;
    // Alive is not yet "has run": a pane the runner has read no output from counts as never
    // started (a fresh shell is all it restores). Wait until each shell has answered.
    for p in ["w2:p1", "w3:p1"] {
        h.ok("pane.send_text", json!({"pane_id": p, "text": "echo up-$((20+22))\n"}))
            .await;
        h.wait_text(p, "up-42").await;
    }
    for (p, agent, s) in [
        ("w2:p1", "codex", "019a-codex-S1"),
        ("w3:p1", "cursor", "0f7c-cursor-C1"),
    ] {
        h.ok(
            "pane.report_agent_session",
            json!({"pane_id": p, "source": "test", "agent": agent, "agent_session_id": s}),
        )
        .await;
    }
    h.ok("pane.rename", json!({"pane_id": "w1:p1", "label": "keep"})).await; // persists the state

    // Pod loss: the holder dies with every PTY; a new holder and runner come up.
    unsafe { libc::kill(h1.0.id() as i32, libc::SIGKILL) };
    let _ = h1.0.wait();
    drop(c);
    drop(_r1);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let _h2 = holder(&hsock, &d.join("holder2.log"));
    let _r2 = runner(2);
    let mut c = gw.accept(S * 2).await.expect("runner 2 connects");
    c.hello(S).await;
    let mut resumed = vec![];
    for _ in 0..2 {
        let e = c
            .text(S * 2, |v| {
                v["t"] == "event"
                    && v["type"] == "resumed"
                    && !resumed.contains(&v["paneId"].as_str().unwrap_or("").to_string())
            })
            .await;
        resumed.push(e["paneId"].as_str().unwrap().to_string());
    }
    resumed.sort();
    assert_eq!(resumed, ["w2:p1", "w3:p1"]);
    let h = H::attach_to(&rsock).await;
    let codex = h.wait_text("w2:p1", "RESUMED-CODEX").await.replace('\n', "");
    assert!(codex.contains("resume 019a-codex-S1"), "{codex}");
    assert!(
        codex.contains("-c hooks.SessionStart=[{hooks=[{type=\"command\""),
        "resumed through the shim: {codex}"
    );
    assert!(codex.contains(&format!("in {}", a.display())), "{codex}");
    let cursor = h.wait_text("w3:p1", "RESUMED-CURSOR").await.replace('\n', "");
    assert!(cursor.contains("--resume 0f7c-cursor-C1"), "{cursor}");
    assert!(cursor.contains("--plugin-dir"), "resumed through the shim: {cursor}");
    assert!(
        cursor.contains(&format!("in {}", b.display())),
        "Cursor resumes in its chat's cwd: {cursor}"
    );
}
