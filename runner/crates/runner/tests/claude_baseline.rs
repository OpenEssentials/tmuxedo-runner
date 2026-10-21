//! Phase 12 baseline: today's exact Claude behaviour, captured BEFORE the
//! plugin-host refactor (docs/design/plugins.md §12 step 1). Every row of
//! plugins.md §4 that the runner owns is pinned here: the PATH shim and the
//! argv it execs, the generated `--settings` JSON, the URL-opener stand-ins,
//! every `hook` reply (incl. the PermissionRequest hold and its decision,
//! release, expiry and staleness), the status / durable event / `hello`
//! fields, the resume argv, the env scrub, and how a `config` frame's
//! `claude.args` and `shell` are applied.
//!
//! The refactor must keep this file green with **no assertion edits**.

mod common;

use common::{options, start_holder, Conn, FakeGateway, H};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;
use tmuxedo_runner::{agent, claude_auth, daemon, hook, inject};

const S: Duration = Duration::from_secs(20);
const BIN: &str = env!("CARGO_BIN_EXE_tmuxedo-runner");

/// A fake `claude` on the test process's PATH (panes and the runner's
/// `claude --version` both see it): `--version` prints a version, anything
/// else prints its argv as `ARGV:<a><b>…`.
const FAKE_CLAUDE: &str = r#"#!/bin/sh
if [ "$1" = "--version" ]; then echo "2.1.289 (Claude Code)"; exit 0; fi
printf 'ARGV:'
for a; do printf '<%s>' "$a"; done
echo
"#;

fn fake_bin() -> &'static Path {
    static D: OnceLock<PathBuf> = OnceLock::new();
    D.get_or_init(|| {
        let d = std::env::temp_dir().join(format!("tmuxedo-baseline-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("claude"), FAKE_CLAUDE).unwrap();
        std::fs::set_permissions(d.join("claude"), std::fs::Permissions::from_mode(0o755)).unwrap();
        std::env::set_var(
            "PATH",
            format!("{}:{}", d.display(), std::env::var("PATH").unwrap_or_default()),
        );
        // Markers a parent Claude would leave; the pane env must not carry them.
        std::env::set_var("CLAUDECODE", "1");
        std::env::set_var("CLAUDE_PID", "4242");
        std::env::set_var("CLAUDE_CODE_ENTRYPOINT", "cli");
        std::env::set_var("CLAUDE_EFFORT", "high");
        std::env::set_var("AI_AGENT", "claude-code");
        d
    })
}

// ── row 5: generated hook settings ───────────────────────────────────────

fn expected_settings(command: &str) -> Value {
    let plain = || json!([{ "hooks": [{ "type": "command", "command": command }] }]);
    let tool = || json!([{ "matcher": "*", "hooks": [{ "type": "command", "command": command }] }]);
    json!({ "hooks": {
        "SessionStart": plain(),
        "UserPromptSubmit": plain(),
        "PermissionRequest": [{ "matcher": "*", "hooks": [{ "type": "command", "command": command, "timeout": 600 }] }],
        "PostToolUse": tool(),
        "PostToolUseFailure": tool(),
        "PermissionDenied": tool(),
        "Notification": plain(),
        "Stop": plain(),
        "StopFailure": plain(),
        "SubagentStart": plain(),
        "SubagentStop": plain(),
        "SessionEnd": plain(),
    }})
}

// THE OFF CASE, which is the original Phase 12 pin, unchanged. AI usage is
// off until web's config turns it on for the plugin (an older web, a runner
// that never heard, or a person who switched it off), and with it off the
// generated settings must be exactly what they were before AI usage existed:
// the hooks and nothing else. In particular there is NO `statusLine` key, so
// Claude keeps showing the person's own status line by its own rules.
#[test]
fn generated_settings_are_exact() {
    let exe = Path::new("/opt/it's here/tmuxedo-runner");
    let want = expected_settings(r"'/opt/it'\''s here/tmuxedo-runner' hook");
    assert_eq!(inject::settings_json(exe), want);
    assert!(want.get("statusLine").is_none());
    // The file on disk is that JSON, pretty-printed, keys sorted.
    let d = common::tempdir();
    inject::install(d.path(), exe).unwrap();
    let on_disk = std::fs::read(d.path().join("claude-settings.json")).unwrap();
    assert_eq!(on_disk, serde_json::to_vec_pretty(&want).unwrap());
    let text = String::from_utf8(on_disk).unwrap();
    assert!(text.starts_with("{\n  \"hooks\": {\n    \"Notification\": [\n"));
    assert!(!text.contains("statusLine") && !text.contains("--statusline"));
}

// THE ON CASE (Phase 20, AI usage, D-U2): only while the person has AI usage
// on, the same settings gain one key, `statusLine`, naming tmuxedo's command
// (which records the plan numbers and then runs the person's own status-line
// command, printing its output unchanged). Nothing else in the document
// differs from the off case. This is a deliberate, opt-out addition to the
// baseline, not a change to it: the off case above stays the pin.
#[test]
fn generated_settings_with_ai_usage_on_add_only_the_status_line() {
    let exe = Path::new("/opt/it's here/tmuxedo-runner");
    let mut want = expected_settings(r"'/opt/it'\''s here/tmuxedo-runner' hook");
    want["statusLine"] =
        json!({ "type": "command", "command": r"'/opt/it'\''s here/tmuxedo-runner' hook --statusline" });
    assert_eq!(inject::settings_json_with(exe, true), want);
    assert_eq!(inject::settings_json_with(exe, false), inject::settings_json(exe));
    let d = common::tempdir();
    inject::install_with_usage(d.path(), exe).unwrap();
    let on_disk = std::fs::read(d.path().join("claude-settings.json")).unwrap();
    assert_eq!(on_disk, serde_json::to_vec_pretty(&want).unwrap());
}

// ── row 4: the PATH shim ─────────────────────────────────────────────────

// Phase 15 (2026-10-08) added the files fragment (`${TMUXEDO_FILES_DIR:+...}`): in a
// pane with a files folder Claude may read it and is handed the files
// instructions; elsewhere it expands to nothing and the argv is as before.
const SHIM_GOLDEN: &str = r#"#!/bin/sh
# tmuxedo: run the real claude with tmuxedo's hook settings layered on top of the
# user's own (which are left untouched). Generated by tmuxedo-runner.
shim_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
old_ifs=$IFS
IFS=:
for d in $PATH; do
  [ -n "$d" ] || continue
  abs=$(CDPATH= cd -- "$d" 2>/dev/null && pwd) || continue
  [ "$abs" = "$shim_dir" ] && continue
  if [ -x "$d/claude" ] && [ ! -d "$d/claude" ]; then
    IFS=$old_ifs
    set -- ${TMUXEDO_FILES_DIR:+--add-dir "$TMUXEDO_FILES_DIR" --append-system-prompt-file "$TMUXEDO_FILES_ROOT/.instructions.md"} '--model' 'opus' '--append-system-prompt' 'it'\''s $HOME' --settings '/st ate/claude-settings.json' "$@"
    tmuxedo_p=; tmuxedo_w=; tmuxedo_e=; tmuxedo_n=1; tmuxedo_nl='
'
    for tmuxedo_a in "$@"; do
      [ "$tmuxedo_n" = 1 ] && set -- && tmuxedo_n=
      if [ -z "$tmuxedo_w$tmuxedo_e" ]; then
        case $tmuxedo_a in
          --) tmuxedo_e=1 ;;
          --append-system-prompt-file) tmuxedo_w=f; continue ;;
          --append-system-prompt) tmuxedo_w=t; continue ;;
          --append-system-prompt-file=*) tmuxedo_w=f; tmuxedo_a=${tmuxedo_a#*=} ;;
          --append-system-prompt=*) tmuxedo_w=t; tmuxedo_a=${tmuxedo_a#*=} ;;
        esac
      fi
      if [ "$tmuxedo_w" = f ]; then
        if tmuxedo_v=$(cat -- "$tmuxedo_a" 2>/dev/null); then tmuxedo_w=t; tmuxedo_a=$tmuxedo_v
        else set -- "$@" --append-system-prompt-file "$tmuxedo_a"; tmuxedo_w=; continue; fi
      fi
      if [ "$tmuxedo_w" = t ]; then
        tmuxedo_p=${tmuxedo_p:+$tmuxedo_p$tmuxedo_nl$tmuxedo_nl}$tmuxedo_a; tmuxedo_w=; continue
      fi
      set -- "$@" "$tmuxedo_a"
    done
    [ "$tmuxedo_n" = 1 ] && set --
    case $tmuxedo_w in f) set -- "$@" --append-system-prompt-file ;; t) set -- "$@" --append-system-prompt ;; esac
    [ -n "$tmuxedo_p" ] && set -- --append-system-prompt "$tmuxedo_p" "$@"
    exec "$d/claude" "$@"
  fi
done
IFS=$old_ifs
echo "tmuxedo: claude is not installed (not found on PATH)" >&2
exit 127
"#;

#[test]
fn shim_script_is_exact() {
    let args: Vec<String> = ["--model", "opus", "--append-system-prompt", "it's $HOME"]
        .map(String::from)
        .to_vec();
    assert_eq!(
        inject::shim_script(Path::new("/st ate/claude-settings.json"), &args),
        SHIM_GOLDEN
    );
    // No launch args: `--settings` comes first.
    let bare = inject::shim_script(Path::new("/s/claude-settings.json"), &[]);
    assert!(
        bare.contains("    set -- ${TMUXEDO_FILES_DIR:+--add-dir \"$TMUXEDO_FILES_DIR\" --append-system-prompt-file \"$TMUXEDO_FILES_ROOT/.instructions.md\"} --settings '/s/claude-settings.json' \"$@\"\n"),
        "{bare}"
    );
}

fn run_shim(shim: &Path, path: &str, args: &[&str]) -> std::process::Output {
    std::process::Command::new(shim.join("claude"))
        .args(args)
        .env("PATH", path)
        .output()
        .unwrap()
}

#[test]
fn shim_execs_the_real_claude_with_exact_argv() {
    let real = fake_bin();
    let d = common::tempdir();
    let args: Vec<String> = ["--model", "opus"].map(String::from).to_vec();
    let shim = inject::install_with(d.path(), Path::new(BIN), &args).unwrap();
    assert_eq!(shim, d.path().join("shim"));
    let path = format!("{}:{}:/usr/bin:/bin", shim.display(), real.display());
    let out = run_shim(&shim, &path, &["hello world", "--resume", "x"]);
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!(
            "ARGV:<--model><opus><--settings><{}/claude-settings.json><hello world><--resume><x>\n",
            d.path().display()
        )
    );
    // The shim's own directory twice on PATH (and an empty entry) is skipped.
    let path = format!("{s}::{s}:{}:/usr/bin:/bin", real.display(), s = shim.display());
    assert!(String::from_utf8_lossy(&run_shim(&shim, &path, &[]).stdout).starts_with("ARGV:<--model>"));
    // No real claude: a clean 127, never a recursion.
    let out = run_shim(&shim, &format!("{}:/usr/bin:/bin", shim.display()), &[]);
    assert_eq!(out.status.code(), Some(127));
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "tmuxedo: claude is not installed (not found on PATH)\n"
    );
    // A new config rewrites only the claude shim.
    let opener_before = std::fs::read(shim.join("xdg-open")).unwrap();
    inject::write_claude_shim(d.path(), &[]).unwrap();
    let out = run_shim(
        &shim,
        &format!("{}:{}:/usr/bin:/bin", shim.display(), real.display()),
        &["a"],
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!("ARGV:<--settings><{}/claude-settings.json><a>\n", d.path().display())
    );
    assert_eq!(std::fs::read(shim.join("xdg-open")).unwrap(), opener_before);
}

// ── row 15: URL-opener stand-ins ─────────────────────────────────────────

#[test]
fn opener_stand_ins_are_exact() {
    assert_eq!(
        inject::OPENERS,
        &["xdg-open", "open", "sensible-browser", "x-www-browser", "www-browser"]
    );
    assert_eq!(
        inject::opener_script(Path::new("/opt/it's/tmuxedo-runner"), "xdg-open"),
        "#!/bin/sh\n# tmuxedo: open web links on your device; anything else goes to the real 'xdg-open'.\n\
         exec '/opt/it'\\''s/tmuxedo-runner' open-url --program 'xdg-open' -- \"$@\"\n"
    );
    let d = common::tempdir();
    let shim = inject::install(d.path(), Path::new("/x/tmuxedo-runner")).unwrap();
    let mut names: Vec<String> = std::fs::read_dir(&shim)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            "claude",
            "open",
            "sensible-browser",
            "www-browser",
            "x-www-browser",
            "xdg-open"
        ]
    );
    for n in &names {
        let mode = std::fs::metadata(shim.join(n)).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755, "{n}");
    }
    for n in inject::OPENERS {
        assert_eq!(
            std::fs::read_to_string(shim.join(n)).unwrap(),
            inject::opener_script(Path::new("/x/tmuxedo-runner"), n)
        );
    }
    assert_eq!(
        inject::prompt_command(),
        r#"case ":$PATH:" in *":$TMUXEDO_SHIM_DIR:"*) ;; *) [ -n "$TMUXEDO_SHIM_DIR" ] && PATH="$TMUXEDO_SHIM_DIR:$PATH" ;; esac"#
    );
}

// ── row 6 + 10: the hook entry point and its replies ─────────────────────

#[test]
fn decision_output_is_exact() {
    assert_eq!(
        hook::decision_output(&json!({"decision": {"behavior": "allow"}})),
        r#"{"hookSpecificOutput":{"decision":{"behavior":"allow"},"hookEventName":"PermissionRequest"}}"#
    );
    assert_eq!(
        hook::decision_output(&json!({"decision": {"behavior": "deny", "message": "not \"now\""}})),
        r#"{"hookSpecificOutput":{"decision":{"behavior":"deny","message":"not \"now\""},"hookEventName":"PermissionRequest"}}"#
    );
    assert_eq!(
        hook::decision_output(&json!({"decision": {"behavior": "deny"}})),
        r#"{"hookSpecificOutput":{"decision":{"behavior":"deny"},"hookEventName":"PermissionRequest"}}"#
    );
    // A message on allow is passed through as well.
    assert_eq!(
        hook::decision_output(&json!({"decision": {"behavior": "allow", "message": "ok"}})),
        r#"{"hookSpecificOutput":{"decision":{"behavior":"allow","message":"ok"},"hookEventName":"PermissionRequest"}}"#
    );
    for nothing in [
        json!({}),
        json!({"expired": true}),
        json!({"decision": {"behavior": "ask"}}),
        json!({"decision": {}}),
    ] {
        assert_eq!(hook::decision_output(&nothing), "", "{nothing}");
    }
    assert_eq!(hook::PERMISSION_WAIT, Duration::from_secs(550));
    assert_eq!(hook::OTHER_WAIT, Duration::from_secs(10));
}

/// One `tmuxedo-runner hook` run against a scripted socket: what it sent,
/// what it printed, its exit status.
fn run_hook_binary(stdin: &str, reply: Option<Value>) -> (Option<Value>, String, i32) {
    let d = common::tempdir();
    let sock = d.path().join("r.sock");
    let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    let server = std::thread::spawn(move || {
        listener.set_nonblocking(false).unwrap();
        let (s, _) = listener.accept().ok()?;
        let mut line = String::new();
        BufReader::new(s.try_clone().unwrap()).read_line(&mut line).ok()?;
        let req: Value = serde_json::from_str(&line).ok()?;
        if let Some(r) = reply {
            let mut w = s;
            let mut out = serde_json::to_vec(&r).unwrap();
            out.push(b'\n');
            let _ = w.write_all(&out);
        }
        Some(req)
    });
    let mut child = std::process::Command::new(BIN)
        .arg("hook")
        .env("TMUXEDO_SOCKET_PATH", &sock)
        .env("TMUXEDO_PANE_ID", "w9:p9")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    // A junk payload never connects: unblock the accept.
    let _ = std::os::unix::net::UnixStream::connect(&sock);
    let req = server.join().unwrap();
    (
        req,
        String::from_utf8_lossy(&out.stdout).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

#[test]
fn the_hook_binary_forwards_and_prints_exactly() {
    let pr = json!({"hook_event_name": "PermissionRequest", "session_id": "s", "tool_name": "Bash",
        "tool_input": {"command": "ls"}});
    let cases: Vec<(Value, Option<Value>, &str)> = vec![
        (
            pr.clone(),
            Some(json!({"id": "hook", "result": {"decision": {"behavior": "allow"}}})),
            "{\"hookSpecificOutput\":{\"decision\":{\"behavior\":\"allow\"},\"hookEventName\":\"PermissionRequest\"}}\n",
        ),
        (
            pr.clone(),
            Some(json!({"id": "hook", "result": {"decision": {"behavior": "deny", "message": "no"}}})),
            "{\"hookSpecificOutput\":{\"decision\":{\"behavior\":\"deny\",\"message\":\"no\"},\"hookEventName\":\"PermissionRequest\"}}\n",
        ),
        (pr.clone(), Some(json!({"id": "hook", "result": {"expired": true}})), ""),
        (pr.clone(), Some(json!({"id": "hook", "result": {}})), ""),
        (
            pr.clone(),
            Some(json!({"id": "hook", "error": {"code": "x", "message": "y"}})),
            "",
        ),
        (pr.clone(), None, ""),
        (
            json!({"hook_event_name": "Stop", "session_id": "s"}),
            Some(json!({"id": "hook", "result": {"decision": {"behavior": "allow"}}})),
            "",
        ),
        (
            json!({"hook_event_name": "SessionStart", "session_id": "s", "cwd": "/tmp", "source": "startup"}),
            Some(json!({"id": "hook", "result": {}})),
            "",
        ),
    ];
    for (event, reply, want) in cases {
        let (req, out, code) = run_hook_binary(&event.to_string(), reply.clone());
        assert_eq!(code, 0, "{event}");
        assert_eq!(out, want, "{event} / {reply:?}");
        let req = req.expect("the hook connected");
        assert_eq!(req["id"], "hook");
        assert_eq!(req["method"], "hook.event");
        assert_eq!(req["params"]["paneId"], "w9:p9");
        assert_eq!(req["params"]["event"], event);
    }
    // Junk on stdin: nothing sent, nothing printed, exit 0.
    let (req, out, code) = run_hook_binary("not json", None);
    assert_eq!((req.is_none(), out.as_str(), code), (true, "", 0));
    // No runner listening: nothing printed, exit 0.
    let out = std::process::Command::new(BIN)
        .arg("hook")
        .env("TMUXEDO_SOCKET_PATH", "/nonexistent/tmuxedo.sock")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert_eq!((out.status.code(), out.stdout.is_empty()), (Some(0), true));
}

// ── row 7: hook → status ─────────────────────────────────────────────────

#[test]
fn hook_status_table_is_exact() {
    use tmuxedo_protocol::frames::AgentStatus::*;
    let table = [
        ("SessionStart", "", Some(Idle)),
        ("UserPromptSubmit", "", Some(Working)),
        ("PermissionRequest", "", Some(Blocked)),
        ("PostToolUse", "", Some(Working)),
        ("PostToolUseFailure", "", Some(Working)),
        ("PermissionDenied", "", Some(Working)),
        ("Notification", "permission_prompt", Some(Blocked)),
        ("Notification", "elicitation_dialog", Some(Blocked)),
        ("Notification", "idle_prompt", Some(Idle)),
        ("Notification", "auth_success", None),
        ("Notification", "", None),
        ("Stop", "", Some(Done)),
        ("StopFailure", "", Some(Idle)),
        ("SessionEnd", "", None),
        ("PreToolUse", "", None),
        ("SubagentStop", "", None),
        ("", "", None),
    ];
    for (event, nt, want) in table {
        assert_eq!(agent::hook_status(event, nt), want, "{event}/{nt}");
    }
    assert_eq!(agent::durable_event(Blocked), Some("blocked"));
    assert_eq!(agent::durable_event(Done), Some("done"));
    assert_eq!(agent::durable_event(Idle), Some("idle"));
    assert_eq!(agent::durable_event(Working), None);
    assert_eq!(agent::durable_event(Unknown), None);
}

// ── rows 7–10 end to end: status frames, durable events, holds ───────────

struct Rig {
    dir: tempfile::TempDir,
    c: Conn,
    h: H,
    sock: PathBuf,
    task: tokio::task::JoinHandle<Result<daemon::ExitReason, String>>,
}

async fn rig(f: impl FnOnce(&mut daemon::RunOptions, &Path)) -> (Rig, Value) {
    fake_bin();
    let dir = common::tempdir();
    let holder = start_holder(dir.path());
    let gw = FakeGateway::start().await;
    let mut o = options(dir.path(), &gw.url, holder);
    f(&mut o, dir.path());
    let sock = o.runner_socket.clone();
    let task = tokio::spawn(daemon::run(o));
    let mut c = gw.accept(S).await.unwrap();
    let hello = c.hello(S).await;
    c.text_t(S, "topology").await;
    let h = H::attach_to(&sock).await;
    (Rig { dir, c, h, sock, task }, hello)
}

impl Rig {
    async fn hook(&self, ev: Value) -> Value {
        self.h.raw("hook.event", json!({"paneId": "w1:p1", "event": ev})).await
    }
    /// A hook call that may block (a held PermissionRequest).
    fn hook_bg(&self, ev: Value) -> tokio::task::JoinHandle<Value> {
        let sock = self.sock.clone();
        tokio::spawn(async move {
            common::call_once(
                &sock,
                &json!({"id": "hook", "method": "hook.event", "params": {"paneId": "w1:p1", "event": ev}}),
            )
            .await
        })
    }
    async fn status(&mut self, s: &str) -> Value {
        let s = s.to_string();
        self.c
            .text(S, move |v| v["t"] == "status" && v["agent"]["status"] == s.as_str())
            .await
    }
    async fn event(&mut self, t: &str) -> Value {
        let t = t.to_string();
        self.c
            .text(S, move |v| v["t"] == "event" && v["type"] == t.as_str())
            .await
    }
    /// Let queued frames arrive, then forget them (so later waits only see
    /// what follows).
    async fn settle(&mut self) {
        self.c.drain(Duration::from_millis(300)).await;
        self.c.texts.clear();
    }
    async fn done(mut self) {
        self.c.close(4003).await;
        let _ = tokio::time::timeout(S, self.task).await;
    }
}

fn ev(name: &str, extra: Value) -> Value {
    let mut e = json!({"hook_event_name": name, "session_id": "s-1"});
    for (k, v) in extra.as_object().unwrap() {
        e[k] = v.clone();
    }
    e
}

/// The keys of a status frame's `agent`, sorted.
fn keys(v: &Value) -> Vec<String> {
    let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
    k.sort();
    k
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hook_lifecycle_status_events_and_replies() {
    let (mut r, _) = rig(|_, _| {}).await;
    let proj = r.dir.path().join("proj");
    std::fs::create_dir_all(proj.join(".git")).unwrap();

    // A hook for a pane the runner does not have: `{}`, nothing else.
    let x =
        r.h.raw(
            "hook.event",
            json!({"paneId": "w9:p9", "event": ev("PermissionRequest", json!({}))}),
        )
        .await;
    assert_eq!(x["result"], json!({}));

    // SessionStart → idle (reported), session id and cwd captured.
    let res = r
        .hook(ev(
            "SessionStart",
            json!({"cwd": proj.to_string_lossy(), "source": "startup"}),
        ))
        .await;
    assert_eq!(res["result"], json!({}));
    let st = r.status("idle").await;
    assert_eq!(st["paneId"], "w1:p1");
    assert_eq!(keys(&st["agent"]), ["kind", "sessionId", "since", "source", "status"]);
    assert_eq!(
        (st["agent"]["kind"].as_str(), st["agent"]["source"].as_str()),
        (Some("claude"), Some("reported"))
    );
    assert_eq!(st["agent"]["sessionId"], "s-1");
    let e = r.event("idle").await;
    assert_eq!(e["paneId"], "w1:p1");
    assert_eq!(e["data"], json!({"kind": "claude"}));
    let p = r.h.ok("pane.get", json!({"pane_id": "w1:p1"})).await;
    assert_eq!(p["pane"]["cwd"], proj.to_string_lossy().as_ref());
    assert_eq!(
        p["pane"]["agent_session"],
        json!({"source": "tmuxedo", "agent": "claude", "kind": "id", "value": "s-1"})
    );

    // UserPromptSubmit → working; no durable event for working.
    r.hook(ev("UserPromptSubmit", json!({"prompt": "x"}))).await;
    r.status("working").await;

    // PermissionRequest → blocked, held; prompt fields copied.
    let held = r.hook_bg(ev(
        "PermissionRequest",
        json!({"tool_name": "Bash", "tool_input": {"command": "ls"},
               "permission_suggestions": [{"type": "addRules"}]}),
    ));
    let st = r.status("blocked").await;
    assert_eq!(
        keys(&st["agent"]),
        ["kind", "prompt", "promptId", "sessionId", "since", "source", "status"]
    );
    let pid = st["agent"]["promptId"].as_str().unwrap().to_string();
    assert_eq!(pid.len(), 24, "12 random bytes, hex");
    assert_eq!(
        st["agent"]["prompt"],
        json!({"tool": "Bash", "input": {"command": "ls"}, "suggestions": [{"type": "addRules"}]})
    );
    let e = r.event("blocked").await;
    assert_eq!(e["data"], json!({"kind": "claude", "promptId": pid}));
    let a = r.h.ok("agent.get", json!({"target": "w1:p1"})).await;
    assert_eq!(a["agent"]["agent_status"], "blocked");
    // A decision for another pane is stale; the prompt stays held.
    r.c.send(json!({"t": "decision", "paneId": "w1:p2", "promptId": pid, "behavior": "allow"}))
        .await;
    let stale = r.event("decision_stale").await;
    assert_eq!(stale["data"], json!({"promptId": pid}));
    assert_eq!(stale["paneId"], "w1:p2");
    // The right decision reaches the hook and the pane goes back to working.
    r.c.send(json!({"t": "decision", "paneId": "w1:p1", "promptId": pid, "behavior": "allow"}))
        .await;
    let res = tokio::time::timeout(S, held).await.unwrap().unwrap();
    assert_eq!(res["result"], json!({"decision": {"behavior": "allow"}}));
    let st = r.status("working").await;
    assert!(st["agent"]["promptId"].is_null() && st["agent"]["prompt"].is_null());

    // Deny with a message; no suggestions in the payload → none in the prompt.
    let held = r.hook_bg(ev(
        "PermissionRequest",
        json!({"tool_name": "Write", "tool_input": {"file_path": "/x"}}),
    ));
    let st = r.status("blocked").await;
    assert_eq!(
        st["agent"]["prompt"],
        json!({"tool": "Write", "input": {"file_path": "/x"}})
    );
    let pid = st["agent"]["promptId"].as_str().unwrap().to_string();
    r.c.send(json!({"t": "decision", "paneId": "w1:p1", "promptId": pid, "behavior": "deny", "message": "nope"}))
        .await;
    let res = tokio::time::timeout(S, held).await.unwrap().unwrap();
    assert_eq!(
        res["result"],
        json!({"decision": {"behavior": "deny", "message": "nope"}})
    );
    r.status("working").await;

    // A permission Notification while a prompt is held keeps the prompt.
    let held = r.hook_bg(ev(
        "PermissionRequest",
        json!({"tool_name": "Bash", "tool_input": {"command": "rm y"}}),
    ));
    let st = r.status("blocked").await;
    let pid = st["agent"]["promptId"].as_str().unwrap().to_string();
    r.hook(ev("Notification", json!({"notification_type": "permission_prompt"})))
        .await;
    let a = r.h.ok("agent.get", json!({"target": "w1:p1"})).await;
    assert_eq!(a["agent"]["agent_status"], "blocked");
    let snap = r.h.ok("pane.get", json!({"pane_id": "w1:p1"})).await;
    assert_eq!(snap["pane"]["agent_status"], "blocked");
    // Answered at the terminal: PostToolUse releases the hold with `{}`.
    r.hook(ev("PostToolUse", json!({"tool_name": "Bash"}))).await;
    let res = tokio::time::timeout(S, held).await.unwrap().unwrap();
    assert_eq!(res["result"], json!({}));
    let st = r.status("working").await;
    assert!(st["agent"]["promptId"].is_null());
    // The late decision for the released prompt is stale.
    r.c.send(json!({"t": "decision", "paneId": "w1:p1", "promptId": pid, "behavior": "allow"}))
        .await;
    r.event("decision_stale").await;

    // Every other release trigger, one hold each.
    for trigger in [
        "PostToolUseFailure",
        "PermissionDenied",
        "Stop",
        "UserPromptSubmit",
        "SessionEnd",
    ] {
        let held = r.hook_bg(ev("PermissionRequest", json!({"tool_name": "Bash", "tool_input": {}})));
        r.status("blocked").await;
        r.hook(ev(trigger, json!({}))).await;
        let res = tokio::time::timeout(S, held).await.unwrap().unwrap();
        assert_eq!(res["result"], json!({}), "{trigger} releases a held prompt");
        if trigger == "SessionEnd" {
            // SessionEnd clears the reported status and the session id; the
            // kind stays (screen / heuristic tiers take over).
            let st =
                r.c.text(S, |v| {
                    v["t"] == "status" && v["agent"]["sessionId"].is_null() && v["agent"]["source"] != "reported"
                })
                .await;
            assert_eq!(
                (st["paneId"].as_str(), st["agent"]["kind"].as_str()),
                (Some("w1:p1"), Some("claude"))
            );
            let p = r.h.ok("pane.get", json!({"pane_id": "w1:p1"})).await;
            assert!(p["pane"]["agent_session"].is_null(), "{p}");
            r.hook(ev("SessionStart", json!({}))).await;
            // (Earlier `working` frames of this loop are still queued: match idle.)
            let st =
                r.c.text(S, |v| {
                    v["t"] == "status" && v["agent"]["source"] == "reported" && v["agent"]["status"] == "idle"
                })
                .await;
            assert_eq!(st["agent"]["sessionId"], "s-1");
        }
    }
    r.settle().await;

    // Notification idle_prompt → idle; an unmapped notification changes nothing.
    r.hook(ev("UserPromptSubmit", json!({}))).await;
    r.status("working").await;
    r.hook(ev("Notification", json!({"notification_type": "idle_prompt"})))
        .await;
    r.status("idle").await;
    r.hook(ev("Notification", json!({"notification_type": "auth_success"})))
        .await;
    r.hook(ev("PreToolUse", json!({"tool_name": "Bash"}))).await;
    let a = r.h.ok("agent.get", json!({"target": "w1:p1"})).await;
    assert_eq!(a["agent"]["agent_status"], "idle");
    // elicitation_dialog → blocked without a prompt id.
    r.hook(ev("Notification", json!({"notification_type": "elicitation_dialog"})))
        .await;
    let st = r.status("blocked").await;
    assert!(st["agent"]["promptId"].is_null());
    let e = r.event("blocked").await;
    assert_eq!(e["data"], json!({"kind": "claude"}));

    // Stop → done (durable); StopFailure → idle.
    r.hook(ev("UserPromptSubmit", json!({}))).await;
    r.status("working").await;
    r.hook(ev("Stop", json!({}))).await;
    r.status("done").await;
    let e = r.event("done").await;
    assert_eq!(e["data"], json!({"kind": "claude"}));
    r.hook(ev("StopFailure", json!({"error_type": "rate_limit"}))).await;
    r.status("idle").await;

    // A new session id (e.g. /clear) replaces the stored one.
    r.hook(ev("SessionStart", json!({"session_id": "s-2", "source": "clear"})))
        .await;
    let p = r.h.ok("pane.get", json!({"pane_id": "w1:p1"})).await;
    assert_eq!(p["pane"]["agent_session"]["value"], "s-2");
    r.done().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_held_prompt_expires_into_the_hook_and_events() {
    let (mut r, _) = rig(|o, _| o.prompt_hold = Duration::from_millis(600)).await;
    let held = r.hook_bg(ev("PermissionRequest", json!({"tool_name": "Bash", "tool_input": {}})));
    let st = r.status("blocked").await;
    let pid = st["agent"]["promptId"].as_str().unwrap().to_string();
    let res = tokio::time::timeout(S, held).await.unwrap().unwrap();
    assert_eq!(res["result"], json!({"expired": true}));
    let e = r.event("prompt_expired").await;
    assert_eq!(
        (e["paneId"].as_str(), &e["data"]),
        (Some("w1:p1"), &json!({"promptId": pid}))
    );
    r.c.send(json!({"t": "decision", "paneId": "w1:p1", "promptId": pid, "behavior": "allow"}))
        .await;
    let e = r.event("decision_stale").await;
    assert_eq!(e["data"], json!({"promptId": pid}));
    r.done().await;
}

// ── rows 11–12: hello fields and auth ────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hello_carries_claude_version_and_auth() {
    // Auto: Claude Code is on PATH, so its login is tracked.
    let (r, hello) = rig(|o, d| {
        let home = d.join("home");
        std::fs::create_dir_all(&home).unwrap();
        o.home = home.to_string_lossy().into_owned();
    })
    .await;
    let runner = &hello["runner"];
    assert_eq!(runner["claudeVersion"], "2.1.289");
    let auth = &runner["claudeAuth"];
    assert_eq!(keys(auth), ["reason", "since", "state"]);
    if cfg!(target_os = "linux") {
        let creds = r.dir.path().join("home/.claude/.credentials.json");
        assert_eq!(auth["state"], "missing");
        assert_eq!(auth["reason"], format!("no {}", creds.display()));
    } else {
        assert_eq!(
            (auth["state"].as_str(), auth["reason"].as_str()),
            (Some("ok"), Some("keychain (not inspected)"))
        );
    }
    r.done().await;

    // Credentials present.
    let (r, hello) = rig(|o, d| {
        let home = d.join("home");
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        std::fs::write(home.join(".claude/.credentials.json"), "{}").unwrap();
        o.home = home.to_string_lossy().into_owned();
    })
    .await;
    if cfg!(target_os = "linux") {
        assert_eq!(hello["runner"]["claudeAuth"]["reason"], "credentials present");
    }
    assert_eq!(hello["runner"]["claudeAuth"]["state"], "ok");
    r.done().await;

    // Tracking off: no claudeAuth at all.
    let (r, hello) = rig(|o, _| o.claude_auth = Some(false)).await;
    assert!(hello["runner"].get("claudeAuth").is_none(), "{hello}");
    assert_eq!(hello["runner"]["claudeVersion"], "2.1.289");
    r.done().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auth_events_from_hooks_and_screen() {
    let (mut r, _) = rig(|o, d| {
        let home = d.join("home");
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        std::fs::write(home.join(".claude/.credentials.json"), "{}").unwrap();
        o.home = home.to_string_lossy().into_owned();
        o.claude_auth = Some(true);
    })
    .await;
    for et in ["authentication_failed", "oauth_org_not_allowed", "account_on_hold"] {
        r.hook(ev("StopFailure", json!({"error_type": et}))).await;
        let e = r.event("auth_failed").await;
        assert_eq!(e["paneId"], "w1:p1");
        assert_eq!(e["data"], json!({"state": "failed", "reason": et, "source": "hook"}));
        r.hook(ev("Stop", json!({}))).await;
        let e = r.event("auth_ok").await;
        assert_eq!(
            e["data"],
            json!({"state": "ok", "reason": "turn completed", "source": "hook"})
        );
    }
    // A screen match on ANY pane (no agent needed), case-insensitive.
    r.h.ok(
        "pane.send_text",
        json!({"pane_id": "w1:p1", "text": "printf 'LOGIN %s\\n' 'EXPIRED'\n"}),
    )
    .await;
    let e = r.event("auth_expired").await;
    assert_eq!(
        (e["paneId"].as_str(), &e["data"]),
        (
            Some("w1:p1"),
            &json!({"state": "expired", "reason": "login_expired", "source": "screen"})
        )
    );
    r.done().await;
}

#[test]
fn auth_rules_and_reasons_are_exact() {
    assert_eq!(
        claude_auth::SCREEN_RULES,
        &[("login_expired", "login expired"), ("run_login", "please run /login")]
    );
    assert_eq!(claude_auth::screen_rule("Login Expired"), Some("login_expired"));
    assert_eq!(claude_auth::screen_rule("PLEASE RUN /LOGIN"), Some("run_login"));
    assert_eq!(
        claude_auth::screen_rule("login expired · please run /login"),
        Some("login_expired"),
        "the first rule wins"
    );
    for e in ["authentication_failed", "oauth_org_not_allowed", "account_on_hold"] {
        assert!(claude_auth::is_auth_error(e), "{e}");
    }
    for e in ["rate_limit", "billing_error", "", "unknown"] {
        assert!(!claude_auth::is_auth_error(e), "{e}");
    }
    assert_eq!(
        claude_auth::credentials_path("/home/u"),
        Path::new("/home/u/.claude/.credentials.json")
    );
    let d = common::tempdir();
    let home = d.path().to_str().unwrap();
    let h = claude_auth::AuthHealth::new(home, true, true);
    assert_eq!(
        (h.state, h.reason.as_str()),
        (claude_auth::AuthState::Ok, "ANTHROPIC_API_KEY")
    );
    let mut h = claude_auth::AuthHealth::new(home, true, false);
    assert_eq!(h.reason, format!("no {home}/.claude/.credentials.json"));
    assert_eq!(keys(&h.json()), ["reason", "since", "state"]);
    assert_eq!(h.json()["state"], "missing");
    let c = h.screen_expired("w1:p1", "run_login").unwrap();
    assert_eq!(
        c,
        claude_auth::Change {
            state: claude_auth::AuthState::Expired,
            reason: "run_login".into(),
            source: "screen",
            pane: Some("w1:p1".into()),
        }
    );
    assert_eq!(h.screen_expired("w1:p2", "login_expired"), None, "already expired");
    let c = h.turn_completed("w1:p1").unwrap();
    assert_eq!((c.reason.as_str(), c.source), ("turn completed", "hook"));
    assert_eq!(h.turn_completed("w1:p1"), None);
    for (s, e) in [
        (claude_auth::AuthState::Ok, "auth_ok"),
        (claude_auth::AuthState::Missing, "auth_missing"),
        (claude_auth::AuthState::Expired, "auth_expired"),
        (claude_auth::AuthState::Failed, "auth_failed"),
    ] {
        assert_eq!(s.event(), e);
        assert_eq!(format!("auth_{}", s.as_str()), e);
    }
}

// ── row 14: resume argv ──────────────────────────────────────────────────

#[test]
fn resume_argv_is_exact() {
    assert_eq!(
        inject::resume_argv("/bin/bash", Path::new("/st/shim"), "claude", "abc-1", None),
        [
            "/bin/bash",
            "-lc",
            // Changed 2026-10-12 with the owner's approval: the login shell left
            // behind lost the pane PATH (a login profile resets it), so an agent
            // started again there ran without the shim. bash is now left through
            // the rc wrapper new panes use. The pinned tail was:
            //   …'/st/shim/claude' --resume 'abc-1'; exec '/bin/bash' -l
            "[ -n \"$TMUXEDO_PANE_PATH\" ] && export PATH=\"$TMUXEDO_PANE_PATH:$PATH\"; '/st/shim/claude' --resume 'abc-1'; [ -r \"$TMUXEDO_SHELL_DIR/bashrc\" ] && exec '/bin/bash' --rcfile \"$TMUXEDO_SHELL_DIR/bashrc\" -i; exec '/bin/bash' -l",
        ]
    );
    // Changed 2026-10-12 with the owner's approval, for the same reason (the
    // login shell lost the pane PATH): zsh is now left through the ZDOTDIR
    // wrapper new panes use. The pinned tail was:
    //   …'codex' --resume 'it'\''s'; exec '/bin/zsh' -l
    assert_eq!(
        inject::resume_argv("/bin/zsh", Path::new("/s"), "codex", "it's", None)[2],
        "[ -n \"$TMUXEDO_PANE_PATH\" ] && export PATH=\"$TMUXEDO_PANE_PATH:$PATH\"; 'codex' --resume 'it'\\''s'; [ -r \"$TMUXEDO_SHELL_DIR/zdotdir/.zshrc\" ] && export TMUXEDO_REAL_ZDOTDIR=\"${TMUXEDO_REAL_ZDOTDIR:-${ZDOTDIR:-$HOME}}\" ZDOTDIR=\"$TMUXEDO_SHELL_DIR/zdotdir\"; exec '/bin/zsh' -l"
    );
    let own: Vec<String> = ["opencode", "-s", "x y"].map(String::from).to_vec();
    assert_eq!(
        inject::resume_argv("/bin/sh", Path::new("/s"), "claude", "ignored", Some(&own))[2],
        "[ -n \"$TMUXEDO_PANE_PATH\" ] && export PATH=\"$TMUXEDO_PANE_PATH:$PATH\"; 'opencode' '-s' 'x y'; exec '/bin/sh' -l"
    );
    // An empty reported argv falls back to the kind's default.
    assert!(
        inject::resume_argv("/bin/sh", Path::new("/s"), "claude", "q", Some(&[]))[2]
            .contains("'/s/claude' --resume 'q';")
    );
    let d = common::tempdir();
    let p = d.path().join("gone").to_string_lossy().into_owned();
    assert_eq!(
        inject::resume_cwd_ok(&p, false),
        Err(format!("recorded cwd {p} no longer exists; not recreating it empty"))
    );
    let p = d.path().to_string_lossy().into_owned();
    assert_eq!(
        inject::resume_cwd_ok(&p, true),
        Err(format!("recorded cwd {p} was a git checkout and no longer is"))
    );
    assert!(!inject::is_repo(&p));
}

// ── row 17 + 4 + 15: the pane environment ────────────────────────────────

#[test]
fn holder_scrub_table_is_exact() {
    for k in [
        "CLAUDECODE",
        "CLAUDE_PID",
        // Pinned later: current Claude Code also sets these outside the
        // CLAUDE_CODE_ prefix, and a runner started from inside such a
        // session leaked them into panes (owner approved this baseline change).
        "CLAUDE_EFFORT",
        "AI_AGENT",
        "CLAUDE_CODE_ENTRYPOINT",
        "CLAUDE_CODE_SSE_PORT",
        "CLAUDE_CODE_",
    ] {
        assert!(tmuxedo_holder::scrubbed_env_key(k), "{k}");
    }
    for k in [
        "CLAUDE",
        "CLAUDE_CONFIG_DIR",
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_BASE_URL",
        "CLAUDE_EFFORTS",
        "AI_AGENTS",
        "CLAUDECODE_X",
        "claudecode",
        "PATH",
    ] {
        assert!(!tmuxedo_holder::scrubbed_env_key(k), "{k}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pane_env_is_scrubbed_and_carries_the_shims() {
    let (r, _) = rig(|_, _| {}).await;
    // Into the test's own dir: the pane starts in the harness home (`/`), which is
    // writable only by root.
    let out = r.dir.path().join("env.out");
    r.h.ok(
        "pane.send_text",
        json!({"pane_id": "w1:p1", "text": format!("env | sort > '{}'; echo ENV-DONE\n", out.display())}),
    )
    .await;
    r.h.wait_text("w1:p1", "\nENV-DONE\n").await;
    let text = std::fs::read_to_string(&out).unwrap_or_else(|e| panic!("{}: {e}", out.display()));
    let env: std::collections::BTreeMap<&str, &str> = text.lines().filter_map(|l| l.split_once('=')).collect();
    assert!(
        !env.keys().any(|k| k.starts_with("CLAUDE") || *k == "AI_AGENT"),
        "Claude markers are scrubbed: {text}"
    );
    let shim = r.dir.path().join("state/shim");
    let shim_s = shim.display().to_string();
    assert!(env["PATH"].starts_with(&format!("{shim_s}:")), "{}", env["PATH"]);
    assert_eq!(env["BROWSER"], format!("{shim_s}/x-www-browser"));
    for p in ["TMUXEDO", "AIMUX"] {
        assert_eq!(env[format!("{p}_SHIM_DIR").as_str()], shim_s);
        assert!(env[format!("{p}_PANE_PATH").as_str()].starts_with(&format!("{shim_s}:")));
    }
    if std::env::var_os("PROMPT_COMMAND").is_none() {
        assert_eq!(env["PROMPT_COMMAND"], inject::prompt_command());
    }
    r.done().await;
}

// ── row 16: `config` → claude args and shell settings ────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn config_frame_rewrites_the_shim_and_persists() {
    let (mut r, _) = rig(|o, d| {
        let home = d.join("home");
        std::fs::create_dir_all(&home).unwrap();
        o.home = home.to_string_lossy().into_owned();
    })
    .await;
    let state = r.dir.path().join("state");
    let home = r.dir.path().join("home");
    let settings = state.join("claude-settings.json");
    assert_eq!(
        std::fs::read_to_string(state.join("shim/claude")).unwrap(),
        inject::shim_script(&settings, &[])
    );
    let args = ["--model", "opus", "--append-system-prompt", "a 'b' $(c)"];
    r.c.send(json!({"t": "config", "claude": {"args": args},
        "shell": {"aliases": {"c": "claude"}, "env": {"EDITOR": "vim", "PATH": "/evil"}, "rc": "set -o vi"}}))
        .await;
    let want_args: Vec<String> = args.map(String::from).to_vec();
    let shim = state.join("shim/claude");
    let mut ok = false;
    for _ in 0..50 {
        if std::fs::read_to_string(&shim).unwrap() == inject::shim_script(&settings, &want_args) {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(ok, "shim rewritten with the args");
    // The shim is rewritten first, config.json saved right after: poll it.
    let want_cfg = json!({"claude_args": args, "shell": {"aliases": {"c": "claude"}, "env": {"EDITOR": "vim"}, "rc": "set -o vi"}});
    let mut cfg = Value::Null;
    for _ in 0..50 {
        cfg = std::fs::read(state.join("config.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or(Value::Null);
        if cfg == want_cfg {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(cfg, want_cfg);
    assert_eq!(
        std::fs::metadata(state.join("config.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::read_to_string(home.join(".config/tmuxedo/shell-init.sh")).unwrap(),
        "# Generated by tmuxedo from your launch settings; edits are overwritten.\n\
         export EDITOR='vim'\nalias c='claude'\n# rc\nset -o vi\n"
    );
    let view = r.h.raw("runner.config", json!({})).await["result"].clone();
    assert_eq!(
        view,
        json!({
            "claude": {"args": args},
            "shell": {"aliases": {"c": "claude"}, "env": {"EDITOR": "vim"}, "rc": "set -o vi"},
            "files": {
                "config": state.join("config.json"),
                "shell_init": home.join(".config/tmuxedo/shell-init.sh"),
            },
        })
    );
    // Not a flag and not a flag's value: refused, the previous args stay.
    r.c.send(json!({"t": "config", "claude": {"args": ["--model", "a", "b"]}}))
        .await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        std::fs::read_to_string(&shim).unwrap(),
        inject::shim_script(&settings, &want_args)
    );
    // `args: null` (or absent) clears them.
    r.c.send(json!({"t": "config", "claude": {}})).await;
    let mut ok = false;
    for _ in 0..50 {
        if std::fs::read_to_string(&shim).unwrap() == inject::shim_script(&settings, &[]) {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(ok, "args cleared");
    let view = r.h.raw("runner.config", json!({})).await["result"].clone();
    assert_eq!(view["claude"], json!({"args": []}));
    // A new pane's claude goes through the rewritten shim.
    r.h.ok(
        "pane.send_text",
        json!({"pane_id": "w1:p1", "text": "claude --print hi\n"}),
    )
    .await;
    let text = r.h.wait_text("w1:p1", "<hi>").await.replace(['\r', '\n'], "");
    // In a pane the files fragment (Phase 15) comes first: the files folder and
    // the instructions, whose text the shim hands over as the one joined
    // --append-system-prompt (Claude reads only the last of each prompt flag).
    assert!(
        text.contains(&format!("<--settings><{}><--print><hi>", settings.display()))
            && text.contains("ARGV:<--append-system-prompt><# Files from the person")
            && text.contains("<--add-dir><")
            && !text.contains("--append-system-prompt-file"),
        "{text}"
    );
    r.done().await;
}
