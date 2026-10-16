//! The plugin host on the wire (PLAN Phase 12, protocol dual-write, UI
//! primitives, the generated skill), through a real daemon and a fake
//! gateway:
//!
//! - `hello.runner.plugins[]` beside the unchanged legacy Claude fields;
//! - `ui` frames only to a gateway whose `welcome` lists `ui`, following
//!   plugin enable / disable;
//! - `config.plugins` (skill on/off, launch args winning over `claude.args`)
//!   and an older web's `claude`-only config;
//! - the `plugin.*` methods answering over the gateway;
//! - a scripted agent, started through the real shim, that reads the
//!   generated skill and follows it (names its pane with the `tmuxedo` CLI).

mod common;

use common::{options, start_holder, Conn, FakeGateway, H};
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;
use tmuxedo_runner::daemon;

const S: Duration = Duration::from_secs(10);
const BIN: &str = env!("CARGO_BIN_EXE_tmuxedo-runner");

/// A fake `claude` that reports 2.1.289 and, when given `--plugin-dir`, reads
/// the generated SKILL.md and runs the naming command it teaches (with its
/// own name), as an agent following the skill would.
const FAKE_CLAUDE: &str = r#"#!/bin/sh
if [ "$1" = "--version" ]; then echo "2.1.289 (Claude Code)"; exit 0; fi
printf 'ARGV:'
for a; do printf '<%s>' "$a"; done
echo
dir=""; prev=""
for a; do [ "$prev" = "--plugin-dir" ] && dir="$a"; prev="$a"; done
if [ -n "$dir" ]; then
  skill="$dir/skills/tmuxedo/SKILL.md"
  head -n 3 "$skill" | grep -q '^name: tmuxedo$' || { echo NO-SKILL; exit 3; }
  cmd=$(grep -m1 '^tmuxedo pane rename ' "$skill" | sed 's/"fix login redirect"/"followed the skill"/')
  [ -n "$cmd" ] || { echo NO-COMMAND; exit 4; }
  eval "$cmd" && echo SKILL-FOLLOWED
fi
"#;

fn fake_bin() -> &'static Path {
    static D: OnceLock<PathBuf> = OnceLock::new();
    D.get_or_init(|| {
        let d = std::env::temp_dir().join(format!("tmuxedo-plugin-wire-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("claude"), FAKE_CLAUDE).unwrap();
        std::fs::set_permissions(d.join("claude"), std::fs::Permissions::from_mode(0o755)).unwrap();
        let _ = std::os::unix::fs::symlink(BIN, d.join("tmuxedo"));
        std::env::set_var(
            "PATH",
            format!("{}:{}", d.display(), std::env::var("PATH").unwrap_or_default()),
        );
        d
    })
}

fn fixtures() -> Value {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../packages/protocol/fixtures/plugins.json"
    );
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

struct Rig {
    dir: tempfile::TempDir,
    c: Conn,
    h: H,
    task: tokio::task::JoinHandle<Result<daemon::ExitReason, String>>,
}

/// A daemon whose gateway answers `welcome` with `features`.
async fn rig(features: Option<Value>) -> (Rig, Value) {
    fake_bin();
    let dir = common::tempdir();
    let holder = start_holder(dir.path());
    let gw = FakeGateway::start().await;
    let mut o = options(dir.path(), &gw.url, holder);
    let home = dir.path().join("home");
    std::fs::create_dir_all(home.join(".claude")).unwrap();
    std::fs::write(home.join(".claude/.credentials.json"), "{}").unwrap();
    o.home = home.to_string_lossy().into_owned();
    let sock = o.runner_socket.clone();
    let task = tokio::spawn(daemon::run(o));
    let mut c = gw.accept(S).await.unwrap();
    let hello = c.text_t(S, "hello").await;
    let mut welcome = json!({"t": "welcome", "machineId": "m1", "userId": "u1"});
    if let Some(f) = features {
        welcome["features"] = f;
    }
    c.send(welcome).await;
    c.text_t(S, "topology").await;
    let h = H::attach_to(&sock).await;
    (Rig { dir, c, h, task }, hello)
}

impl Rig {
    fn state(&self) -> PathBuf {
        self.dir.path().join("state")
    }
    fn shim(&self) -> String {
        std::fs::read_to_string(self.state().join("shim/claude")).unwrap()
    }
    async fn wait_shim(&self, pred: impl Fn(&str) -> bool) -> String {
        for _ in 0..100 {
            let s = self.shim();
            if pred(&s) {
                return s;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("shim never matched:\n{}", self.shim());
    }
    async fn done(self) {
        self.task.abort();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hello_lists_plugins_beside_the_legacy_fields() {
    let (r, hello) = rig(None).await;
    let runner = &hello["runner"];
    // Legacy fields as before (the baseline pins them in detail).
    assert_eq!(runner["claudeVersion"], "2.1.289");
    assert_eq!(runner["claudeAuth"]["state"], "ok");
    let plugins = runner["plugins"].as_array().expect("plugins[]");
    // Claude first; Codex and Cursor beside it (PLAN Phase 16). Neither CLI is
    // installed here, so neither tracks a login (no "sign in" for an absent CLI).
    let ids: Vec<&str> = plugins.iter().map(|p| p["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["claude", "codex", "cursor"], "{plugins:?}");
    for p in &plugins[1..] {
        assert_eq!(p["state"], "enabled");
        assert!(p.get("auth").is_none() && p.get("agentVersion").is_none(), "{p}");
    }
    let p = &plugins[0];
    assert_eq!(p["id"], "claude");
    assert_eq!(p["version"], "1.0.0");
    assert_eq!(p["contract"], ">=1.0, <2");
    assert_eq!(p["state"], "enabled");
    assert_eq!(p["agent"], "claude");
    assert_eq!(p["agentVersion"], "2.1.289");
    // One login state, two places: plugins[claude].auth is claudeAuth.
    assert_eq!(p["auth"], runner["claudeAuth"]);
    // The shape the TypeScript side parses (fixtures/plugins.json `dual`).
    let fx = fixtures();
    let mut keys: Vec<&String> = p.as_object().unwrap().keys().collect();
    keys.sort();
    let want = fx["hello"]["dual"]["runner"]["plugins"][0].as_object().unwrap();
    let mut want_keys: Vec<&String> = want.keys().collect();
    want_keys.sort();
    assert_eq!(keys, want_keys);
    r.done().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ui_frames_need_the_gateway_feature_and_follow_enable_state() {
    let fx = fixtures();
    // A gateway that relays ui: the Claude plugin's declared surfaces, exactly.
    let (mut r, _) = rig(Some(json!(["ui"]))).await;
    let ui = r.c.text_t(S, "ui").await;
    assert_eq!(ui, fx["ui"]["claude"]);
    // Codex and Cursor (PLAN Phase 16): their sign-in banner and actions, exactly.
    for id in ["codex", "cursor"] {
        let f = r.c.text(S, |v| v["t"] == "ui" && v["pluginId"] == id).await;
        assert_eq!(f, fx["ui"][id], "{id}");
    }
    // Disable → an empty set for the plugin; enable → the surfaces again.
    r.c.send(json!({"t": "req", "id": "d1", "method": "plugin.disable", "params": {"plugin_id": "claude"}}))
        .await;
    assert_eq!(r.c.res("d1").await["result"]["type"], "plugin_disabled");
    let off = r.c.text_t(S, "ui").await;
    assert_eq!(off["items"], json!([]));
    r.c.send(json!({"t": "req", "id": "e1", "method": "plugin.enable", "params": {"plugin_id": "claude"}}))
        .await;
    assert_eq!(r.c.res("e1").await["result"]["type"], "plugin_enabled");
    assert_eq!(r.c.text_t(S, "ui").await, fx["ui"]["claude"]);
    r.done().await;

    // An older gateway (no features): never a ui frame, even on toggles.
    let (mut r, _) = rig(None).await;
    r.c.send(json!({"t": "req", "id": "d2", "method": "plugin.disable", "params": {"plugin_id": "claude"}}))
        .await;
    r.c.res("d2").await;
    r.c.send(json!({"t": "req", "id": "e2", "method": "plugin.enable", "params": {"plugin_id": "claude"}}))
        .await;
    r.c.res("e2").await;
    r.c.drain(Duration::from_millis(500)).await;
    assert!(!r.c.texts.iter().any(|t| t["t"] == "ui"), "{:?}", r.c.texts);
    r.done().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn plugin_methods_answer_over_the_gateway() {
    let (mut r, _) = rig(None).await;
    r.c.send(json!({"t": "req", "id": "l1", "method": "plugin.list", "params": {}}))
        .await;
    let list = r.c.res("l1").await;
    assert_eq!(list["result"]["plugins"][0]["plugin_id"], "claude", "{list}");
    r.c.send(json!({"t": "req", "id": "a1", "method": "plugin.action.list", "params": {}}))
        .await;
    assert_eq!(r.c.res("a1").await["result"]["actions"][0]["action_id"], "login");
    // What a client's plugin_action becomes: the manifest's login, in a new tab.
    r.c.send(json!({"t": "req", "id": "i1", "method": "plugin.action.invoke",
        "params": {"plugin_id": "claude", "action_id": "login", "context": {}}}))
        .await;
    let inv = r.c.res("i1").await;
    assert_eq!(inv["result"]["type"], "plugin_action_invoked", "{inv}");
    assert_eq!(inv["result"]["log"]["status"], "succeeded", "{inv}");
    // An action the manifest does not declare is refused; argv cannot be sent.
    r.c.send(json!({"t": "req", "id": "i2", "method": "plugin.action.invoke",
        "params": {"plugin_id": "claude", "action_id": "rm-everything", "argv": ["rm", "-rf", "/"]}}))
        .await;
    assert_eq!(r.c.res("i2").await["error"]["code"], "action_not_found");
    r.c.send(json!({"t": "req", "id": "g1", "method": "plugin.log.list", "params": {"limit": 5}}))
        .await;
    assert_eq!(r.c.res("g1").await["result"]["logs"][0]["action_id"], "login");
    r.done().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn config_plugins_turn_the_skill_on_and_off() {
    let (mut r, _) = rig(None).await;
    let skill_dir = r.state().join("plugins/claude/skill");
    let flag = format!("--plugin-dir '{}'", skill_dir.display());
    // No plugins in config yet (an older web): no skill.
    assert!(!r.shim().contains("--plugin-dir"), "{}", r.shim());
    r.c.send(json!({"t": "config", "claude": {"args": ["--model", "haiku"]}}))
        .await;
    r.wait_shim(|s| s.contains("'--model' 'haiku' --settings")).await;
    assert!(!r.shim().contains("--plugin-dir"));
    assert!(!skill_dir.exists());

    // A newer web: both fields; plugins wins; the skill is on.
    r.c.send(json!({"t": "config", "claude": {"args": ["--model", "haiku"]},
        "plugins": [{"id": "claude", "settings": {"launch": {"args": ["--model", "opus"]}, "skill": {"enabled": true}}}]}))
        .await;
    let shim = r.wait_shim(|s| s.contains("--plugin-dir")).await;
    assert!(
        shim.contains(&format!(
            "'--model' 'opus' --settings '{}' {flag} \"$@\"",
            r.state().join("claude-settings.json").display()
        )),
        "{shim}"
    );
    let md = std::fs::read_to_string(skill_dir.join("skills/tmuxedo/SKILL.md")).unwrap();
    assert!(md.starts_with("---\nname: tmuxedo\n"), "{md}");
    assert!(md.contains("## You are running inside tmuxedo"));
    let manifest: Value =
        serde_json::from_slice(&std::fs::read(skill_dir.join(".claude-plugin/plugin.json")).unwrap()).unwrap();
    assert_eq!(manifest["name"], "tmuxedo");
    let wishes: Value = serde_json::from_slice(&std::fs::read(r.state().join("skill.json")).unwrap()).unwrap();
    assert_eq!(wishes, json!({"claude": true}));
    // `runner.skill` (`tmuxedo skill`): what the agent is told.
    let view = r.h.raw("runner.skill", json!({})).await["result"].clone();
    assert_eq!(view["enabled"], true, "{view}");
    assert!(view["markdown"].as_str().unwrap().contains("tmuxedo pane rename"));

    // Turned off: the flag and the files go.
    r.c.send(json!({"t": "config", "plugins": [{"id": "claude", "settings": {"skill": {"enabled": false}}}]}))
        .await;
    r.wait_shim(|s| !s.contains("--plugin-dir")).await;
    assert!(!skill_dir.exists());
    // Its launch args were not in that config: the previous ones stay.
    assert!(r.shim().contains("'--model' 'opus' --settings"), "{}", r.shim());
    r.done().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_scripted_agent_follows_the_skill() {
    let (mut r, _) = rig(None).await;
    r.c.send(json!({"t": "config", "plugins": [{"id": "claude", "settings": {"skill": {"enabled": true}}}]}))
        .await;
    r.wait_shim(|s| s.contains("--plugin-dir")).await;
    r.h.ok("pane.send_text", json!({"pane_id": "w1:p1", "text": "claude\n"}))
        .await;
    let text = r.h.wait_text("w1:p1", "SKILL-FOLLOWED").await;
    assert!(text.contains("<--plugin-dir>"), "{text}");
    // The name it gave its pane reaches the gateway (and so the person's devices).
    let topo =
        r.c.text(S, |v| {
            v["t"] == "topology"
                && v["panes"]
                    .as_array()
                    .is_some_and(|p| p.iter().any(|p| p["label"] == "followed the skill"))
        })
        .await;
    assert!(topo["rev"].as_u64().is_some());
    r.done().await;
}
