//! Manifest `[[ui]]` and `[skill]` (PLAN Phase 12 UI primitives and skill):
//! the linter's rules, the shim's `{skill.argv...}`, and local / dev plugins.

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use tmuxedo_runner::plugin::{self, AgentPlugin, Host, Manifest, ManifestPlugin};

const BASE: &str = r#"
schema = 1
id = "demo"
name = "Demo"
version = "1.0.0"
contract = ">=1.0, <2"
capabilities = ["ui.surfaces", "generate.files", "launch.wrap"]
[agent]
labels = ["codex"]
binaries = ["codex"]
[[actions]]
id = "hi"
title = "Say hi"
kind = "pane.send"
text = "echo hi\r"
"#;

fn lint(extra: &str) -> Vec<String> {
    Manifest::parse(&format!("{BASE}{extra}")).unwrap().lint()
}

#[test]
fn ui_items_lint_clean_when_they_follow_the_rules() {
    let ok = lint(
        r#"
[[ui]]
key = "b"
kind = "banner"
tone = "warning"
text = "Hello {machine.name}"
when = "agent.status == 'blocked'"
actions = [{ label = "Say hi", action = "hi" }, { label = "Hide", action = "dismiss" }]
dismissible = true
[[ui]]
key = "t"
kind = "toast"
text = "done"
action = { label = "Settings", action = "settings.open" }
[[ui]]
key = "g"
kind = "badge"
text = "demo"
target = "agent"
[[ui]]
key = "q"
kind = "quick_action"
label = "Say hi"
action = "hi"
[[ui]]
key = "s"
kind = "status_line"
text = "{agent.status}"
target = "agent"
"#,
    );
    assert!(ok.is_empty(), "{ok:?}");
}

#[test]
fn ui_lint_catches_each_rule() {
    let cases = [
        ("[[ui]]\nkey = \"Bad\"\nkind = \"badge\"\ntext = \"x\"\n", "bad key"),
        ("[[ui]]\nkey = \"a\"\nkind = \"sparkle\"\ntext = \"x\"\n", "unknown kind"),
        ("[[ui]]\nkey = \"a\"\nkind = \"badge\"\ntext = \"x\"\nwhen = \"a ==\"\n", "when"),
        ("[[ui]]\nkey = \"a\"\nkind = \"quick_action\"\nlabel = \"x\"\naction = \"nope\"\n", "not declared in [[actions]]"),
        (
            "[[ui]]\nkey = \"a\"\nkind = \"badge\"\ntext = \"x\"\n[[ui]]\nkey = \"a\"\nkind = \"badge\"\ntext = \"y\"\n",
            "duplicate ui key",
        ),
        (
            "[[ui]]\nkey = \"a\"\nkind = \"banner\"\ntext = \"x\"\nactions = [{label=\"1\",action=\"hi\"},{label=\"2\",action=\"hi\"},{label=\"3\",action=\"hi\"},{label=\"4\",action=\"hi\"}]\n",
            "banner",
        ),
        ("[[ui]]\nkey = \"a\"\nkind = \"badge\"\ntext = \"x\"\ntarget = \"room\"\n", "target"),
    ];
    for (extra, want) in cases {
        let e = lint(extra);
        assert!(e.iter().any(|p| p.contains(want)), "{want}: {e:?}");
    }
    // Surfaces without the capability.
    let no_cap = Manifest::parse(&format!(
        "{}[[ui]]\nkey = \"a\"\nkind = \"badge\"\ntext = \"x\"\n",
        BASE.replace("\"ui.surfaces\", ", "")
    ))
    .unwrap()
    .lint();
    assert!(no_cap.iter().any(|p| p.contains("ui.surfaces")), "{no_cap:?}");
    // More than 20 items.
    let many: String = (0..21)
        .map(|i| format!("[[ui]]\nkey = \"k{i}\"\nkind = \"badge\"\ntext = \"x\"\n"))
        .collect();
    assert!(lint(&many).iter().any(|p| p.contains("at most 20")));
}

#[test]
fn skill_lint_rules() {
    let launch = "[launch]\nshim = \"codex\"\nprepend = [\"{skill.argv...}\"]\n";
    let ok = lint(&format!(
        "{launch}[skill]\nform = \"agents-md\"\nargv = [\"--x\", \"{{gen:skill}}\"]\nmin_agent = \"1.0.0\"\n"
    ));
    assert!(ok.is_empty(), "{ok:?}");
    let bad_form = lint(&format!("{launch}[skill]\nform = \"word-doc\"\n"));
    assert!(bad_form.iter().any(|p| p.contains("skill.form")), "{bad_form:?}");
    let no_section = lint(launch);
    assert!(
        no_section.iter().any(|p| p.contains("[skill] is not declared")),
        "{no_section:?}"
    );
    let no_min = lint(&format!("{launch}[skill]\nform = \"agents-md\"\nargv = [\"--x\"]\n"));
    assert!(no_min.iter().any(|p| p.contains("min_agent")), "{no_min:?}");
    let unused = lint("[skill]\nform = \"agents-md\"\nargv = [\"--x\"]\nmin_agent = \"1.0.0\"\n");
    assert!(unused.iter().any(|p| p.contains("no {skill.argv...}")), "{unused:?}");
}

#[test]
fn the_claude_shim_carries_the_skill_only_when_it_is_on() {
    let c = plugin::claude();
    let gen: BTreeMap<String, PathBuf> =
        [("hook-settings".to_string(), PathBuf::from("/s/claude-settings.json"))].into();
    let off = c.shim_script(&gen, &["--model".into(), "opus".into()], None).unwrap();
    assert!(
        // (The files fragment, Phase 15, comes first; it is empty outside a pane with a files folder.)
        off.contains("exec \"$d/claude\" ${TMUXEDO_FILES_DIR:+--add-dir \"$TMUXEDO_FILES_DIR\" --append-system-prompt-file \"$TMUXEDO_FILES_ROOT/.instructions.md\"} '--model' 'opus' --settings '/s/claude-settings.json' \"$@\"\n"),
        "{off}"
    );
    let on = c
        .shim_script(
            &gen,
            &["--model".into(), "opus".into()],
            Some(Path::new("/s/plugins/claude/skill")),
        )
        .unwrap();
    assert!(
        on.contains("exec \"$d/claude\" ${TMUXEDO_FILES_DIR:+--add-dir \"$TMUXEDO_FILES_DIR\" --append-system-prompt-file \"$TMUXEDO_FILES_ROOT/.instructions.md\"} '--model' 'opus' --settings '/s/claude-settings.json' --plugin-dir '/s/plugins/claude/skill' \"$@\"\n"),
        "{on}"
    );
    // The Claude manifest declares exactly one surface: Sign in, while a login is needed.
    let ui = &c.manifest().ui;
    assert_eq!(ui.len(), 1);
    assert_eq!(ui[0].kind, "quick_action");
    assert!(tmuxedo_protocol::when::eval(
        ui[0].when.as_deref(),
        &serde_json::json!({"auth": {"state": "expired"}})
    ));
    assert!(!tmuxedo_protocol::when::eval(
        ui[0].when.as_deref(),
        &serde_json::json!({"auth": {"state": "ok"}})
    ));
}

#[test]
fn the_e2e_stacks_demo_plugin_lints_clean_and_loads() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../e2e/runner-box/plugins");
    // The runner's Docker build context holds runner/ and packages/ only: the
    // check runs where the repo is checked out whole (dev, verify's e2e stack).
    let Ok(text) = std::fs::read_to_string(format!("{dir}/e2e-demo/plugin.toml")) else {
        eprintln!("skipped: {dir} is not in this checkout");
        return;
    };
    let m = Manifest::parse(&text).unwrap();
    assert!(m.lint().is_empty(), "{:?}", m.lint());
    assert_eq!(
        m.ui.iter().map(|i| i.kind.as_str()).collect::<Vec<_>>(),
        ["banner", "toast", "badge", "quick_action", "status_line"]
    );
    let mut h = Host::builtin();
    let notes = h.load_dev(Path::new(dir));
    assert!(h.enabled("e2e-demo"), "{notes:?}");
}

#[test]
fn dev_plugins_load_unverified_and_bad_ones_are_skipped() {
    let d = common::tempdir();
    let good = d.path().join("demo");
    std::fs::create_dir_all(&good).unwrap();
    std::fs::write(good.join("plugin.toml"), BASE).unwrap();
    let bad = d.path().join("bad");
    std::fs::create_dir_all(&bad).unwrap();
    std::fs::write(
        bad.join("plugin.toml"),
        BASE.replace("id = \"demo\"", "id = \"Bad Id\""),
    )
    .unwrap();
    let taken = d.path().join("claude2");
    std::fs::create_dir_all(&taken).unwrap();
    std::fs::write(
        taken.join("plugin.toml"),
        BASE.replace("id = \"demo\"", "id = \"claude\""),
    )
    .unwrap();
    let mut h = Host::builtin();
    let notes = h.load_dev(d.path());
    assert!(h.enabled("demo"));
    let l = h.loaded("demo").unwrap();
    assert_eq!(l.tier, "local");
    assert!(l.warnings.iter().any(|w| w.contains("unverified")));
    assert!(h.loaded("Bad Id").is_none());
    assert!(notes.iter().any(|n| n.contains("id already taken")), "{notes:?}");
    assert!(notes.iter().any(|n| n.contains("must match")), "{notes:?}");
    assert_eq!(h.ids(), ["claude", "codex", "cursor", "demo"]);
    // A plugin that is only a manifest still lists its actions.
    let _ = ManifestPlugin::new(Manifest::parse(BASE).unwrap());
    assert_eq!(h.action_infos(Some("demo"))[0]["action_id"], "hi");
}
