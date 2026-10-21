use super::*;
use crate::plugin::manifest::Manifest;
use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixListener;

const NOW: u64 = 1_791_890_000;

// FROM MEMORY, NOT CAPTURED FROM A REAL CLI: the status-line JSON of Claude
// Code as docs/design/agent-usage.md §1.1 describes it (`rate_limits.*`,
// `context_window.*`) plus `model.display_name` and `workspace.project_dir`
// as remembered from its documentation. The other values stand for what must
// never leave the machine.
fn claude_status(marker: &str) -> String {
    json!({
        "hook_event_name": "Status",
        "session_id": "s-1",
        "transcript_path": format!("/home/someone/{marker}.jsonl"),
        "cwd": "/w",
        "model": { "id": "model-x", "display_name": "Opus" },
        "workspace": { "current_dir": "/w", "project_dir": "/nonexistent-project" },
        "cost": { "total_cost_usd": 1.25 },
        "context_window": {
            "used_percentage": 34, "context_window_size": 200000,
            "total_input_tokens": 15234, "total_output_tokens": 4521,
        },
        "rate_limits": {
            "five_hour": { "used_percentage": 62.44, "resets_at": NOW + 3600 },
            "seven_day": { "used_percentage": 41, "resets_at": NOW + 400_000 },
        },
    })
    .to_string()
}

// FROM MEMORY, NOT CAPTURED FROM A REAL CLI: one line of a Codex session log
// as docs/design/agent-usage.md §1.2 describes it (`event_msg` /
// `token_count`, `rate_limits.primary|secondary` with `used_percent`,
// `window_minutes`, `resets_at`; `plan_type`).
fn codex_line(primary: f64, resets: u64) -> String {
    json!({
        "timestamp": "2026-10-13T10:00:00Z",
        "type": "event_msg",
        "payload": {
            "type": "token_count",
            "info": {
                "total_token_usage": { "input_tokens": 1000, "cached_input_tokens": 400, "output_tokens": 200, "total_tokens": 1200 },
                "model_context_window": 272000,
            },
            "rate_limits": {
                "limit_id": "codex", "plan_type": "Plus",
                "primary": { "used_percent": primary, "window_minutes": 300, "resets_at": resets },
                "secondary": { "used_percent": 12.0, "window_minutes": 10080, "resets_at": resets + 500_000 },
            },
        },
    })
    .to_string()
}

fn section(id: &str) -> UsageSection {
    let p = crate::plugin::Host::builtin().get(id).expect("built in");
    p.manifest().usage.clone().expect("declares usage")
}

fn statusline() -> UsageSource {
    section("claude").source("statusline").unwrap().clone()
}

#[test]
fn the_built_in_manifests_declare_what_the_design_says() {
    assert_eq!(statusline().windows.len(), 2);
    let codex = section("codex");
    assert_eq!(codex.sources.len(), 1);
    assert_eq!(codex.sources[0].wire_name(), "jsonl");
    let cursor = section("cursor");
    assert!(cursor.sources.is_empty(), "Cursor has no source to read");
    assert!(cursor.link.as_deref().is_some_and(|l| l.starts_with("https://")));
}

const DEMO: &str = r#"
schema = 1
id = "demo"
name = "Demo"
version = "0.1.0"
contract = "1"
capabilities = ["usage.report", "generate.files", "hooks.receive"]
[agent]
labels = ["codex"]
[generate.s]
builder = "hook-settings"
file = "s.json"
[hooks]
transport = "command"
register = [{ event = "Stop" }]
"#;

#[test]
fn a_manifest_usage_section_is_parsed_strictly() {
    let lint = |usage: &str| Manifest::parse(&format!("{DEMO}{usage}")).map(|m| m.lint());
    let ok = "[usage]\nmodes = [\"plan\"]\n[[usage.sources]]\nkind = \"statusline\"\nwindows = [{ at = \"a.b\", pct = \"p\", id = \"5h\" }]\n";
    assert_eq!(lint(ok).unwrap(), Vec::<String>::new());
    // A kind this host does not read, by name.
    let e = lint("[usage]\nmodes = [\"plan\"]\n[[usage.sources]]\nkind = \"screen\"\n").unwrap();
    assert_eq!(e.len(), 1, "{e:?}");
    assert!(
        e[0].contains("usage.sources[0].kind \"screen\" is not one of [\"statusline\", \"jsonl_tail\"]"),
        "{e:?}"
    );
    // A key the section does not have.
    assert!(lint("[usage]\nmodes = [\"plan\"]\nscrape = true\n")
        .unwrap_err()
        .contains("scrape"));
    assert!(lint(&ok.replace("id = \"5h\"", "id = \"5h\", raw = true"))
        .unwrap_err()
        .contains("raw"));
    for (bad, why) in [
        (ok.replace("modes = [\"plan\"]", "modes = [\"free\"]"), "usage.modes"),
        (ok.replace("a.b", "a..b"), "not a field path"),
        (ok.replace("a.b", "a/../b"), "not a field path"),
        (ok.replace(", id = \"5h\"", ""), "needs id, mins or mins_field"),
        (ok.replace("id = \"5h\"", "id = \"5 H\""), "not a window id"),
        (format!("{ok}path_field = \"x\"\n"), "belong to jsonl_tail"),
        (format!("{ok}[[usage.sources]]\nkind = \"statusline\"\n"), "a second"),
        ("[usage]\nmodes = [\"plan\"]\n".to_string(), "no sources and no link"),
        ("[usage]\nmodes = [\"plan\"]\nlink = \"http://x\"\n".to_string(), "https URL"),
        (
            "[usage]\nmodes = [\"plan\"]\n[[usage.sources]]\nkind = \"jsonl_tail\"\npath_field = \"p\"\nroot = \"/etc\"\nselect = { a = \"b\" }\n".to_string(),
            "under {home}/",
        ),
        (
            "[usage]\nmodes = [\"plan\"]\n[[usage.sources]]\nkind = \"jsonl_tail\"\nroot = \"{home}/x/../y\"\n".to_string(),
            "needs path_field",
        ),
    ] {
        let e = lint(&bad).unwrap();
        assert!(e.iter().any(|l| l.contains(why)), "{why}: {e:?}");
    }
    let no_cap = format!("{}{ok}", DEMO.replace("\"usage.report\", ", ""));
    let e = Manifest::parse(&no_cap).unwrap().lint();
    assert!(e.iter().any(|l| l.contains("needs capability usage.report")), "{e:?}");
}

#[test]
fn the_generated_settings_name_the_status_line_command_only_while_usage_is_on() {
    let d = tempfile::tempdir().unwrap();
    let mut host = crate::plugin::Host::builtin();
    host.hook_exe = Some("/opt/t/runner".into());
    let read = |host: &crate::plugin::Host| -> Value {
        let p = host.get("claude").unwrap();
        let gen = crate::plugin::gen_paths(p.as_ref(), d.path());
        serde_json::from_slice(&std::fs::read(&gen["hook-settings"]).unwrap()).unwrap()
    };
    // Nobody asked: no status line (the settings are the hooks alone).
    host.install(d.path(), Path::new("/opt/t/runner"), &[]).unwrap();
    let off = read(&host);
    assert!(off.get("statusLine").is_none(), "{off}");
    assert!(off["hooks"]["Stop"].is_array());
    // On: one more key, the hooks untouched.
    host.set_usage("claude", true);
    host.write_settings(d.path()).unwrap();
    let on = read(&host);
    assert_eq!(
        on["statusLine"],
        json!({ "type": "command", "command": "'/opt/t/runner' hook --statusline" })
    );
    assert_eq!(on["hooks"], off["hooks"], "the hooks are still there");
    // Off again: the file is the first one, byte for byte in meaning.
    host.set_usage("claude", false);
    host.write_settings(d.path()).unwrap();
    assert_eq!(read(&host), off);
    // A plugin without a status-line source never gets one, on or not.
    host.set_usage("codex", true);
    let codex = host.get("codex").unwrap();
    assert!(codex.status_line(Path::new("/opt/t/runner")).is_none());
}

#[test]
fn the_usage_wishes_are_only_what_web_turned_on_and_survive_a_restart() {
    let on =
        json!([{ "id": "claude", "settings": { "usage": { "enabled": true } } }, { "id": "codex", "settings": {} }]);
    let cp = crate::plugin_wire::read_config_plugins(Some(&on)).unwrap();
    assert_eq!(cp.usage.keys().collect::<Vec<_>>(), ["claude"]);
    let off = json!([{ "id": "claude", "settings": { "usage": { "enabled": false } } }]);
    assert!(crate::plugin_wire::read_config_plugins(Some(&off))
        .unwrap()
        .usage
        .is_empty());
    // Not a boolean is not "on".
    let junk = json!([{ "id": "claude", "settings": { "usage": { "enabled": "yes" } } }, { "id": "codex", "settings": { "usage": true } }]);
    assert!(crate::plugin_wire::read_config_plugins(Some(&junk))
        .unwrap()
        .usage
        .is_empty());
    let d = tempfile::tempdir().unwrap();
    assert!(
        crate::plugin_wire::load_usage_wishes(d.path()).is_empty(),
        "no file = none"
    );
    crate::plugin_wire::save_usage_wishes(d.path(), &cp.usage);
    assert_eq!(crate::plugin_wire::load_usage_wishes(d.path()), cp.usage);
}

#[test]
fn turning_usage_off_forgets_what_was_read() {
    let d = tempfile::tempdir().unwrap();
    let mut st = Store::new(d.path());
    assert!(st.note(&section("claude"), "claude", Some("w1:p1"), reading(10.0), NOW * 1000));
    assert!(st.has_dirty());
    st.forget("claude");
    assert!(!st.has_dirty());
    assert!(st.due(Instant::now()).is_empty());
}

#[test]
fn a_status_line_gives_its_numbers_and_nothing_else() {
    let v: Value = serde_json::from_str(&claude_status("m")).unwrap();
    let r = extract(&statusline(), &v, NOW);
    assert_eq!(r.source, "statusline");
    assert_eq!(
        r.windows,
        vec![
            UsageWindow {
                id: "5h".into(),
                used_pct: 62.4,
                resets_at: Some(NOW + 3600),
                window_mins: Some(300)
            },
            UsageWindow {
                id: "7d".into(),
                used_pct: 41.0,
                resets_at: Some(NOW + 400_000),
                window_mins: Some(10080)
            },
        ]
    );
    assert_eq!(r.context, Some((34.0, Some(200_000))));
    assert_eq!(r.plan, None);
    assert_eq!(
        minimal_line(&r, Some("Opus")).split(" (").next(),
        Some("Opus · ctx 34% · 5h 62%")
    );
}

#[test]
fn missing_renamed_or_odd_fields_record_nothing_rather_than_a_guess() {
    let s = statusline();
    let get = |v: Value| extract(&s, &v, NOW);
    // An API key, or before the first reply: no rate_limits at all.
    let r = get(json!({ "context_window": { "used_percentage": 5 } }));
    assert!(r.windows.is_empty());
    assert_eq!(r.context, Some((5.0, None)));
    // Renamed, or a different kind of value.
    for v in [
        json!({}),
        json!([1, 2]),
        json!({ "rate_limits": { "fiveHour": { "used_percentage": 5 } } }),
        json!({ "rate_limits": { "five_hour": { "usedPercentage": 5 } } }),
        json!({ "rate_limits": { "five_hour": { "used_percentage": "62" } } }),
        json!({ "rate_limits": { "five_hour": { "used_percentage": -1 } } }),
        json!({ "rate_limits": { "five_hour": 62 }, "context_window": "full" }),
        // Its reset has passed: it says nothing about now.
        json!({ "rate_limits": { "five_hour": { "used_percentage": 62, "resets_at": NOW - 1 } } }),
    ] {
        let r = get(v.clone());
        assert!(r.windows.is_empty() && r.context.is_none(), "{v}");
    }
    // Over 100 is "used up"; a reset that is not Unix seconds is left out, the window stays.
    let r = get(
        json!({ "rate_limits": { "five_hour": { "used_percentage": 104, "resets_at": NOW * 1000 },
        "seven_day": { "used_percentage": 1, "resets_at": "tomorrow" } } }),
    );
    assert_eq!(r.windows[0].used_pct, 100.0);
    assert_eq!((r.windows[0].resets_at, r.windows[1].resets_at), (None, None));
    assert_eq!(minimal_line(&Reading::default(), None), "");
    assert_eq!(minimal_line(&Reading::default(), Some("a\u{1b}[31mb\n")), "a[31mb");
}

struct Rig {
    dir: tempfile::TempDir,
    listener: UnixListener,
}

impl Rig {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let listener = UnixListener::bind(dir.path().join("r.sock")).unwrap();
        listener.set_nonblocking(true).unwrap();
        std::fs::create_dir_all(dir.path().join("cfg")).unwrap();
        Self { dir, listener }
    }
    fn user_line(&self, command: &str) {
        let s = json!({ "theme": "dark", "statusLine": { "type": "command", "command": command } });
        std::fs::write(self.dir.path().join("cfg/settings.json"), s.to_string()).unwrap();
    }
    fn env(&self, timeout_ms: u64) -> StatusEnv {
        StatusEnv {
            plugin: "claude".into(),
            socket: self.dir.path().join("r.sock"),
            pane_id: Some("w1:p1".into()),
            config_dir: Some(self.dir.path().join("cfg")),
            cwd: None,
            timeout: Duration::from_millis(timeout_ms),
            now_s: NOW,
        }
    }
    fn run(&self, stdin: &[u8], timeout_ms: u64) -> (Vec<u8>, i32) {
        run_statusline(&mut &stdin[..], &self.env(timeout_ms))
    }
    /// What reached the runner's socket, if anything.
    fn sent(&self) -> Option<String> {
        let (s, _) = self.listener.accept().ok()?;
        s.set_nonblocking(false).unwrap();
        // macOS refuses socket options (EINVAL) once the peer has closed; the
        // read then returns what was sent at once, so the timeout is not needed.
        let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
        let mut line = String::new();
        BufReader::new(s).read_line(&mut line).ok()?;
        Some(line)
    }
}

#[test]
fn the_shim_sends_numbers_only_and_prints_a_short_line_when_there_is_none() {
    let rig = Rig::new();
    // Built at run time: stands for anything in the agent's JSON that is not a number we name.
    let marker = format!("private{}marker", 7 * 6);
    let (out, code) = rig.run(claude_status(&marker).as_bytes(), 2000);
    assert_eq!(code, 0);
    let line = String::from_utf8(out).unwrap();
    assert!(line.starts_with("Opus · ctx 34% · 5h 62% ("), "{line:?}");
    assert!(line.ends_with(") · 7d 41%"), "{line:?}");
    let sent = rig.sent().expect("a report");
    let v: Value = serde_json::from_str(&sent).unwrap();
    assert_eq!(v["method"], "usage.report");
    assert_eq!(v["params"]["plugin"], "claude");
    assert_eq!(v["params"]["paneId"], "w1:p1");
    assert_eq!(v["params"]["reading"]["windows"][0]["usedPct"], 62.4);
    for never in [
        marker.as_str(),
        "transcript",
        "session_id",
        "s-1",
        "/w",
        "cost",
        "model",
        "Opus",
        "project",
    ] {
        assert!(!sent.contains(never), "{never} was sent: {sent}");
    }
    // The runner takes exactly what was sent.
    let r: Reading = serde_json::from_value(v["params"]["reading"].clone()).unwrap();
    assert_eq!(r.windows.len(), 2);
    assert!(serde_json::from_value::<Reading>(json!({ "source": "statusline", "raw": {} })).is_err());
}

#[test]
fn the_persons_own_status_line_is_passed_through_byte_for_byte() {
    let rig = Rig::new();
    let input = claude_status("x");
    // Same stdin in, the same bytes out: no newline added, none removed.
    rig.user_line("cat");
    assert_eq!(rig.run(input.as_bytes(), 2000), (input.clone().into_bytes(), 0));
    assert!(rig.sent().is_some(), "and the numbers were still recorded");
    // Escapes, a NUL, bytes that are not UTF-8, a trailing blank line, its exit status.
    rig.user_line(r"printf '\033[1mA\033[0m \377\000 B\n\n'; exit 3");
    assert_eq!(
        rig.run(input.as_bytes(), 2000),
        (b"\x1b[1mA\x1b[0m \xff\x00 B\n\n".to_vec(), 3)
    );
    // Nothing printed is nothing printed.
    rig.user_line("true");
    assert_eq!(rig.run(input.as_bytes(), 2000), (vec![], 0));
    // A command that fails, or does not exist: its status, no line of ours in its place.
    rig.user_line("echo partial; exit 7");
    assert_eq!(rig.run(input.as_bytes(), 2000), (b"partial\n".to_vec(), 7));
    rig.user_line("/nonexistent/statusline-command");
    assert_eq!(rig.run(input.as_bytes(), 2000), (vec![], 127));
    while rig.sent().is_some() {}
}

#[test]
fn bad_input_never_stops_the_persons_status_line() {
    let rig = Rig::new();
    rig.user_line("wc -c | tr -d ' \\n'");
    // Not JSON: nothing recorded, their command still gets the bytes.
    assert_eq!(rig.run(b"not json at all", 2000), (b"15".to_vec(), 0));
    assert!(rig.sent().is_none());
    assert_eq!(rig.run(b"", 2000), (b"0".to_vec(), 0));
    // Too large: nothing recorded, their command gets the first STDIN_MAX bytes.
    let mut big = claude_status("x").into_bytes();
    big.resize(STDIN_MAX as usize + 4096, b' ');
    assert_eq!(rig.run(&big, 5000), (STDIN_MAX.to_string().into_bytes(), 0));
    assert!(rig.sent().is_none());
    // No runner at all: the line is still printed.
    let mut env = rig.env(2000);
    env.socket = rig.dir.path().join("none.sock");
    assert_eq!(run_statusline(&mut &b"{}"[..], &env), (b"2".to_vec(), 0));
    // An unknown plugin: the same.
    env.plugin = "nope".into();
    assert_eq!(run_statusline(&mut &b"{}"[..], &env), (b"2".to_vec(), 0));
    // No status line of their own and nothing to say: nothing printed.
    std::fs::remove_file(rig.dir.path().join("cfg/settings.json")).unwrap();
    assert_eq!(rig.run(b"not json", 2000), (vec![], 0));
}

#[test]
fn a_slow_status_line_is_stopped_with_everything_it_started() {
    let rig = Rig::new();
    let flag = rig.dir.path().join("late");
    rig.user_line(&format!("echo early; (sleep 2; touch '{}') & sleep 30", flag.display()));
    let t = Instant::now();
    assert_eq!(rig.run(claude_status("x").as_bytes(), 300), (vec![], 0));
    assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
    assert!(
        rig.sent().is_some(),
        "the numbers were recorded before their command ran"
    );
    std::thread::sleep(Duration::from_millis(2300));
    assert!(!flag.exists(), "what it started was stopped too");
}

#[test]
fn the_persons_command_is_found_without_writing_their_files() {
    let d = tempfile::tempdir().unwrap();
    let (cfg, proj) = (d.path().join("cfg"), d.path().join("proj"));
    std::fs::create_dir_all(proj.join(".claude")).unwrap();
    std::fs::create_dir_all(&cfg).unwrap();
    let set = |p: PathBuf, v: Value| std::fs::write(p, v.to_string()).unwrap();
    let find = || user_command(Some(&cfg), Some(&proj));
    assert_eq!(find(), None);
    set(
        cfg.join("settings.json"),
        json!({ "statusLine": { "type": "command", "command": " user " } }),
    );
    assert_eq!(find().as_deref(), Some("user"));
    set(
        proj.join(".claude/settings.json"),
        json!({ "statusLine": { "command": "project" } }),
    );
    assert_eq!(find().as_deref(), Some("project"));
    set(
        proj.join(".claude/settings.local.json"),
        json!({ "statusLine": { "type": "command", "command": "local" } }),
    );
    assert_eq!(find().as_deref(), Some("local"));
    // Not a command, empty, not JSON, or tmuxedo's own: the next file decides.
    for v in [
        json!({ "statusLine": { "type": "static", "command": "x" } }),
        json!({ "statusLine": { "type": "command", "command": "" } }),
        json!({ "statusLine": "x" }),
        json!({ "statusLine": { "type": "command", "command": "'/opt/t/runner' hook --statusline" } }),
    ] {
        set(proj.join(".claude/settings.local.json"), v);
        assert_eq!(find().as_deref(), Some("project"));
    }
    std::fs::write(proj.join(".claude/settings.local.json"), "{ nope").unwrap();
    assert_eq!(find().as_deref(), Some("project"));
    assert_eq!(
        std::fs::read_to_string(proj.join(".claude/settings.local.json")).unwrap(),
        "{ nope"
    );
}

fn codex_source() -> UsageSource {
    section("codex").source("jsonl_tail").unwrap().clone()
}

#[test]
fn a_session_log_gives_its_newest_snapshot() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().join("sessions");
    std::fs::create_dir_all(root.join("2026/10/13")).unwrap();
    let log = root.join("2026/10/13/rollout-a.jsonl");
    let other = json!({ "type": "response_item", "payload": { "type": "message", "content": "words" } }).to_string();
    let no_limits = json!({ "type": "event_msg", "payload": { "type": "token_count", "info": {} } }).to_string();
    let text = [
        codex_line(10.0, NOW + 100),
        other.clone(),
        codex_line(55.5, NOW + 9000),
        "{ half a line".to_string(),
        format!("{{\"type\":\"event_msg\",\"pad\":\"{}\"}}", "x".repeat(LINE_MAX)),
        no_limits,
        other,
    ]
    .join("\n");
    std::fs::write(&log, format!("{text}\n")).unwrap();
    let s = codex_source();
    let r = read_tail(&s, &log, &root, NOW).expect("a reading");
    assert_eq!(r.source, "jsonl");
    assert_eq!(r.plan.as_deref(), Some("plus"));
    assert_eq!(
        r.windows,
        vec![
            UsageWindow {
                id: "5h".into(),
                used_pct: 55.5,
                resets_at: Some(NOW + 9000),
                window_mins: Some(300)
            },
            UsageWindow {
                id: "7d".into(),
                used_pct: 12.0,
                resets_at: Some(NOW + 509_000),
                window_mins: Some(10080)
            },
        ]
    );
    assert_eq!(r.context, None);
    // Later: the short window has reset, the long one still counts.
    let later = read_tail(&s, &log, &root, NOW + 9001).unwrap();
    assert_eq!(later.windows.len(), 1);
    assert_eq!(later.windows[0].id, "7d");
    // A window of another length is named by its length.
    let month = codex_line(1.0, NOW + 10).replace("\"window_minutes\":300", "\"window_minutes\":43200");
    std::fs::write(&log, month).unwrap();
    assert_eq!(read_tail(&s, &log, &root, NOW).unwrap().windows[0].id, "43200m");
}

#[test]
fn a_session_log_is_read_from_its_end_only_and_only_under_its_folder() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().join("sessions");
    std::fs::create_dir_all(&root).unwrap();
    let s = codex_source();
    // The only snapshot is further back than the tail reaches: not found, and no error.
    let log = root.join("rollout-big.jsonl");
    let filler = format!("{}\n", json!({ "type": "response_item", "pad": "y".repeat(1000) }));
    let mut text = format!("{}\n", codex_line(10.0, NOW + 100));
    while (text.len() as u64) < TAIL_MAX + 5000 {
        text.push_str(&filler);
    }
    std::fs::write(&log, &text).unwrap();
    assert_eq!(read_tail(&s, &log, &root, NOW), None);
    text.push_str(&codex_line(20.0, NOW + 100));
    std::fs::write(&log, &text).unwrap();
    assert_eq!(
        read_tail(&s, &log, &root, NOW).unwrap().windows[0].used_pct,
        20.0,
        "no final newline"
    );
    // Outside the folder, through a link, not a log, a folder, missing, empty.
    let outside = d.path().join("rollout-x.jsonl");
    std::fs::write(&outside, codex_line(10.0, NOW + 100)).unwrap();
    assert_eq!(read_tail(&s, &outside, &root, NOW), None);
    std::os::unix::fs::symlink(&outside, root.join("link.jsonl")).unwrap();
    assert_eq!(read_tail(&s, &root.join("link.jsonl"), &root, NOW), None);
    assert_eq!(read_tail(&s, &root.join("../rollout-x.jsonl"), &root, NOW), None);
    std::fs::write(root.join("notes.txt"), codex_line(10.0, NOW + 100)).unwrap();
    assert_eq!(read_tail(&s, &root.join("notes.txt"), &root, NOW), None);
    std::fs::create_dir(root.join("dir.jsonl")).unwrap();
    assert_eq!(read_tail(&s, &root.join("dir.jsonl"), &root, NOW), None);
    assert_eq!(read_tail(&s, &root.join("gone.jsonl"), &root, NOW), None);
    std::fs::write(root.join("empty.jsonl"), "").unwrap();
    assert_eq!(read_tail(&s, &root.join("empty.jsonl"), &root, NOW), None);
    assert_eq!(std::fs::read_to_string(&log).unwrap(), text, "never written");
}

fn reading(pct: f64) -> Reading {
    Reading {
        source: "statusline".into(),
        windows: vec![UsageWindow {
            id: "5h".into(),
            used_pct: pct,
            resets_at: Some(NOW + 60),
            window_mins: Some(300),
        }],
        context: Some((10.0, None)),
        plan: None,
        tokens: None,
    }
}

#[test]
fn the_store_sends_on_change_and_at_most_once_per_interval() {
    let d = tempfile::tempdir().unwrap();
    let mut st = Store::new(d.path());
    let sec = section("claude");
    let t0 = Instant::now();
    let ms = NOW * 1000;
    assert!(st.due(t0).is_empty());
    assert!(st.note(&sec, "claude", Some("w1:p1"), reading(10.0), ms));
    let first = st.due(t0);
    assert_eq!(first.len(), 1);
    assert!(first[0].ok());
    assert_eq!(first[0].context.as_ref().map(|c| c.pane_id.as_str()), Some("w1:p1"));
    assert_eq!((first[0].mode.as_str(), first[0].reached.clone()), ("plan", None));
    // The same numbers again: nothing to send.
    assert!(!st.note(&sec, "claude", Some("w1:p1"), reading(10.0), ms + 500));
    assert!(st.due(t0 + Duration::from_secs(10)).is_empty());
    // A change inside the interval waits; only the newest goes.
    assert!(st.note(&sec, "claude", Some("w1:p1"), reading(11.0), ms + 600));
    assert!(st.note(&sec, "claude", Some("w1:p1"), reading(100.0), ms + 700));
    assert!(st.due(t0 + Duration::from_millis(1900)).is_empty());
    let next = st.due(t0 + Duration::from_millis(USAGE_MIN_INTERVAL_MS));
    assert_eq!(next.len(), 1);
    assert_eq!(next[0].windows[0].used_pct, 100.0);
    assert_eq!(next[0].reached.as_deref(), Some("5h"));
    assert_eq!(next[0].observed_at, ms + 700);
    // A reading without windows keeps the last known ones until their reset passes.
    let mut none = reading(0.0);
    none.windows.clear();
    st.note(&sec, "claude", Some("p-2"), none.clone(), ms + 800);
    let kept = st.due(t0 + Duration::from_secs(60));
    assert_eq!(kept[0].windows.len(), 1);
    assert_eq!(kept[0].context.as_ref().unwrap().pane_id, "p-2");
    st.note(&sec, "claude", Some("p-2"), none, (NOW + 61) * 1000);
    assert!(st.due(t0 + Duration::from_secs(120))[0].windows.is_empty());
    // A new gateway connection gets every record again.
    assert!(!st.has_dirty());
    st.resend();
    assert_eq!(st.due(t0 + Duration::from_secs(120)).len(), 1);
}

#[test]
fn the_store_refuses_what_is_not_sound_and_keys_tell_nothing() {
    let d = tempfile::tempdir().unwrap();
    let mut st = Store::new(d.path());
    let sec = section("claude");
    let ms = NOW * 1000;
    // A source this plugin does not declare; a pane id that is not one; junk windows.
    let mut r = reading(1.0);
    r.source = "jsonl".into();
    assert!(!st.note(&sec, "claude", Some("w1:p1"), r, ms));
    assert!(!st.note(&sec, "claude", Some("../p"), reading(1.0), ms));
    let mut r = reading(1.0);
    r.windows[0].id = "five hours".into();
    r.windows.push(UsageWindow {
        id: "7d".into(),
        used_pct: 250.0,
        resets_at: None,
        window_mins: None,
    });
    r.plan = Some("Not A Plan".into());
    assert!(st.note(&sec, "claude", Some("w1:p1"), r, ms));
    let sent = st.due(Instant::now());
    assert!(sent[0].windows.is_empty() && sent[0].plan.is_none());
    // The tail's own clock.
    let t0 = Instant::now();
    assert!(st.tail_due("codex", t0));
    assert!(!st.tail_due("codex", t0 + Duration::from_millis(1999)));
    assert!(st.tail_due("codex", t0 + Duration::from_millis(2000)));
    // Keys: opaque, stable on this machine, different per plugin and per machine.
    let k = st.account_key("claude");
    assert!(
        tmuxedo_protocol::usage::account_key_ok(&k) && k.starts_with("m:") && k.len() == 34,
        "{k}"
    );
    assert_ne!(k, st.account_key("codex"));
    assert_eq!(k, Store::new(d.path()).account_key("claude"));
    let other = tempfile::tempdir().unwrap();
    assert_ne!(k, Store::new(other.path()).account_key("claude"));
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(d.path().join("usage-salt"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[test]
fn the_report_waits_for_the_runner_so_its_caller_check_finds_it_alive() {
    // The runner checks a report's caller by its process ancestry, so the status line must not
    // exit before the runner has read the request. Here the runner answers 100 ms after reading.
    let d = tempfile::tempdir().unwrap();
    let sock = d.path().join("r.sock");
    let listener = UnixListener::bind(&sock).unwrap();
    let runner = std::thread::spawn(move || {
        let (s, _) = listener.accept().unwrap();
        let mut line = String::new();
        BufReader::new(s.try_clone().unwrap()).read_line(&mut line).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        (&s).write_all(b"{\"id\":\"usage\",\"result\":{}}\n").unwrap();
        line
    });
    let started = std::time::Instant::now();
    report(
        &sock,
        "claude",
        Some("w1:p1"),
        &extract(&statusline(), &serde_json::from_str(&claude_status("x")).unwrap(), NOW),
    );
    let took = started.elapsed();
    assert!(runner.join().unwrap().contains("usage.report"));
    assert!(
        took >= Duration::from_millis(100),
        "returned before the runner answered: {took:?}"
    );
    assert!(took < Duration::from_secs(2), "{took:?}");
}

// ── what one session used (session.summary's `usage`) ────────────────────

#[test]
fn the_sources_name_a_sessions_running_token_total() {
    let v: Value = serde_json::from_str(&claude_status("x")).unwrap();
    assert_eq!(extract(&statusline(), &v, NOW).tokens, Some(15234 + 4521));
    let codex = section("codex").sources[0].clone();
    let v: Value = serde_json::from_str(&codex_line(10.0, NOW + 100)).unwrap();
    assert_eq!(extract(&codex, &v, NOW).tokens, Some(1200));
    // Missing or not a whole number: left out, never guessed.
    let v = json!({"context_window": {"total_input_tokens": "lots"}});
    assert_eq!(extract(&statusline(), &v, NOW).tokens, None);
    let v = json!({"context_window": {"total_input_tokens": -4, "total_output_tokens": 3}});
    assert_eq!(extract(&statusline(), &v, NOW).tokens, Some(3));
}

#[test]
fn a_manifest_names_at_most_four_token_fields() {
    let base = "[usage]\nmodes = [\"plan\"]\n[[usage.sources]]\nkind = \"statusline\"\nwindows = [{ at = \"a.b\", pct = \"p\", id = \"5h\" }]\n";
    let lint = |extra: &str| Manifest::parse(&format!("{DEMO}{base}{extra}")).unwrap().lint();
    assert_eq!(lint("tokens = [\"t.in\", \"t.out\"]\n"), Vec::<String>::new());
    assert!(lint("tokens = [\"t..in\"]\n")
        .iter()
        .any(|e| e.contains("not a field path")));
    assert!(lint("tokens = [\"a\", \"b\", \"c\", \"d\", \"e\"]\n")
        .iter()
        .any(|e| e.contains("more than 4 token fields")));
}

fn spent_reading(pct: f64, resets: u64, tokens: Option<u64>) -> Reading {
    let mut r = reading(pct);
    r.windows[0].resets_at = Some(resets);
    r.tokens = tokens;
    r
}

#[test]
fn a_session_is_credited_with_the_rise_its_own_readings_show() {
    let d = tempfile::tempdir().unwrap();
    let mut st = Store::new(d.path());
    let sec = section("claude");
    let ms = NOW * 1000;
    let reset = NOW + 3600;
    // The first reading on this machine: where the account stood, nothing credited.
    st.note(
        &sec,
        "claude",
        Some("w1:p1"),
        spent_reading(10.0, reset, Some(1000)),
        ms,
    );
    // p1 again: +4 points, and its running total says 3000 tokens.
    st.note(
        &sec,
        "claude",
        Some("w1:p1"),
        spent_reading(14.0, reset, Some(3000)),
        ms + 1000,
    );
    // p2's first reading: +1.5 since p1's, and its 500 tokens.
    st.note(
        &sec,
        "claude",
        Some("w2:p1"),
        spent_reading(15.5, reset, Some(500)),
        ms + 2000,
    );
    // An older number than the newest (another device lags): nothing is taken back.
    st.note(
        &sec,
        "claude",
        Some("w1:p1"),
        spent_reading(15.0, reset, Some(3000)),
        ms + 3000,
    );
    let p1 = st.spent("w1:p1", 0).expect("p1 used some");
    assert_eq!(p1.plugin, "claude");
    assert_eq!(p1.windows.len(), 1);
    assert_eq!(
        (
            p1.windows[0].id.as_str(),
            p1.windows[0].spent_pct,
            p1.windows[0].window_mins
        ),
        ("5h", 4.0, Some(300))
    );
    assert_eq!(p1.tokens, Some(3000));
    assert!(p1.ok());
    let p2 = st.spent("w2:p1", 0).unwrap();
    assert_eq!((p2.windows[0].spent_pct, p2.tokens), (1.5, Some(500)));
    assert_eq!(st.spent("w9:p9", 0), None);

    // The window resets: a new one starts at 0, and p1's first reading in it counts whole.
    let later = (reset + 10) * 1000;
    st.note(
        &sec,
        "claude",
        Some("w1:p1"),
        spent_reading(3.0, reset + 18_000, Some(3400)),
        later,
    );
    let p1 = st.spent("w1:p1", 0).unwrap();
    assert_eq!((p1.windows[0].spent_pct, p1.tokens), (7.0, Some(3400)));
    // Only what came after `since`.
    let since = st.spent("w1:p1", later).unwrap();
    assert_eq!((since.windows[0].spent_pct, since.tokens), (3.0, Some(400)));
    assert_eq!(st.spent("w1:p1", later + 1), None);
    // The agent restarted in the pane: its total starts again, and counts.
    st.note(
        &sec,
        "claude",
        Some("w1:p1"),
        spent_reading(3.0, reset + 18_000, Some(250)),
        later + 1000,
    );
    assert_eq!(st.spent("w1:p1", later + 1).unwrap().tokens, Some(250));

    // Turning AI usage off forgets what each session used.
    st.forget("claude");
    assert_eq!(st.spent("w1:p1", 0), None);
}

#[test]
fn what_sessions_used_is_bounded() {
    let d = tempfile::tempdir().unwrap();
    let mut st = Store::new(d.path());
    let sec = section("claude");
    let ms = NOW * 1000;
    for i in 0..(SPENT_PANES_MAX as u64 + 10) {
        st.note(
            &sec,
            "claude",
            Some(&format!("w{i}:p1")),
            spent_reading(1.0, NOW + 3600, Some(i + 1)),
            ms + i,
        );
    }
    assert_eq!(st.spent("w0:p1", 0), None, "the oldest session is let go");
    assert!(st.spent(&format!("w{}:p1", SPENT_PANES_MAX + 9), 0).is_some());
    for i in 0..(SPENT_EVENTS_MAX as u64 * 2) {
        st.note(
            &sec,
            "claude",
            Some("w1:p9"),
            spent_reading(1.0, NOW + 3600, Some(i + 1)),
            ms + 1000 + i,
        );
    }
    // Each step was one token; only the newest SPENT_EVENTS_MAX steps are kept.
    assert_eq!(st.spent("w1:p9", 0).unwrap().tokens, Some(SPENT_EVENTS_MAX as u64));
}
