//! The generated tmuxedo skill (src/skill.rs): the **staleness guard** that
//! CI runs with `cargo test`, and snapshot tests per plugin, capability set
//! and native form.
//!
//! The guard's rule: every user-visible capability (a `hello` feature, a
//! top-level `tmuxedo` command, a plugin manifest capability) has a row in
//! `skill::CAPABILITIES` that either names the section teaching it or says
//! why the agent needs none; no row is stale; and every `tmuxedo …` command a
//! section names exists. A feature that ships later (files, secrets,
//! widgets) therefore fails this test until it registers its own section.
//!
//! Snapshots live in tests/snapshots/skill/; `UPDATE_SNAPSHOTS=1 cargo test
//! --test skill` rewrites them (review the diff).

mod common;

use std::collections::BTreeSet;
use std::path::PathBuf;
use tmuxedo_runner::plugin::AgentPlugin;
use tmuxedo_runner::skill::{self, Facts, Teach, CAPABILITIES, SECTIONS};

/// The `"cmd" =>` / `"a" | "b" =>` arms of `cli::main`'s dispatch, from the source.
fn cli_arms() -> BTreeSet<String> {
    let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/cli.rs")).unwrap();
    let start = src.find("pub fn main(args: Vec<String>) -> i32 {").expect("cli::main");
    let body = &src[start..];
    let end = body.find("\n}\n").unwrap();
    let mut out = BTreeSet::new();
    let arm = regex::Regex::new(r#"^\s{8}(?:g @ \()?((?:"[a-z-]+"\s*\|\s*)*"[a-z-]+")\)?\s*=>"#).unwrap();
    let quoted = regex::Regex::new(r#""([a-z-]+)""#).unwrap();
    for line in body[..end].lines() {
        if let Some(c) = arm.captures(line) {
            for q in quoted.captures_iter(&c[1]) {
                // `--help`, `-h`, `--version`: spellings of a command, not commands.
                if !q[1].starts_with('-') {
                    out.insert(q[1].to_string());
                }
            }
        }
    }
    out
}

fn all_on() -> Facts {
    Facts {
        agent_name: "Claude Code".into(),
        agent_label: "claude".into(),
        hosted: false,
        work_dir: "~/work".into(),
        state_dir: "/home/dev/.local/state/tmuxedo".into(),
        remote_approvals: true,
        hold_secs: 540,
        names: true,
        worktrees: true,
        chat: true,
        login_relay: true,
        secret_names: vec![],
        ssh_keys: false,
        files: true,
        files_max: 2 * 1024 * 1024 * 1024,
    }
}

#[test]
fn every_capability_is_registered_and_no_row_is_stale() {
    let ids: Vec<&str> = CAPABILITIES.iter().map(|c| c.id).collect();
    let unique: BTreeSet<&str> = ids.iter().copied().collect();
    assert_eq!(unique.len(), ids.len(), "duplicate capability rows");
    let mut missing = vec![];
    // hello features
    for f in tmuxedo_runner::daemon::RUNNER_FEATURES {
        if !unique.contains(format!("feature:{f}").as_str()) {
            missing.push(format!("feature:{f}"));
        }
    }
    // top-level commands (from the source, and the list the CLI exports)
    let arms = cli_arms();
    let listed: BTreeSet<String> = tmuxedo_runner::cli::TOP_COMMANDS
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert_eq!(arms, listed, "cli::TOP_COMMANDS must list every arm of cli::main");
    for c in &arms {
        if c != "help" && !unique.contains(format!("cli:{c}").as_str()) {
            missing.push(format!("cli:{c}"));
        }
    }
    // plugin manifest capabilities
    for c in tmuxedo_runner::plugin::manifest::CAPABILITIES {
        if !unique.contains(format!("plugin:{c}").as_str()) {
            missing.push(format!("plugin:{c}"));
        }
    }
    assert!(
        missing.is_empty(),
        "capabilities with no skill row (add a section, or say why the agent needs none, in src/skill.rs CAPABILITIES): {missing:?}"
    );
    // No stale rows: each names something that exists.
    for c in CAPABILITIES {
        let ok = match c.id.split_once(':') {
            Some(("feature", f)) => tmuxedo_runner::daemon::RUNNER_FEATURES.contains(&f),
            Some(("cli", f)) => arms.contains(f),
            Some(("plugin", f)) => tmuxedo_runner::plugin::manifest::CAPABILITIES.contains(&f),
            Some(("core", _)) => true,
            _ => false,
        };
        assert!(ok, "capability row {} names nothing that exists", c.id);
        if let Teach::Section(s) = c.skill {
            assert!(
                SECTIONS.iter().any(|x| x.id == s),
                "{} names section {s}, which does not exist",
                c.id
            );
        }
        if let Teach::NotForAgents(why) = c.skill {
            assert!(why.len() > 10, "{}: say why the agent needs no section", c.id);
        }
    }
    // Every section is owned by a capability that teaches it.
    for s in SECTIONS {
        let owner = CAPABILITIES.iter().find(|c| c.id == s.capability);
        assert!(
            matches!(owner.map(|c| &c.skill), Some(Teach::Section(id)) if *id == s.id),
            "section {} must be owned by capability {} (Teach::Section(\"{}\"))",
            s.id,
            s.capability,
            s.id
        );
    }
}

#[test]
fn sections_name_only_commands_that_exist() {
    let methods: BTreeSet<String> = common::schema_methods().into_iter().collect();
    let md = skill::markdown(&all_on());
    let cmds = skill::named_commands(&md);
    assert!(cmds.len() >= 2, "{cmds:?}");
    for c in cmds {
        let words: Vec<String> = c
            .split_whitespace()
            .skip(1)
            .map(|w| w.trim_matches('"').to_string())
            .collect();
        assert!(
            tmuxedo_runner::cli::names_a_command(&words),
            "the skill names `{c}`, which is not a tmuxedo command"
        );
        if words[0] == "api" {
            assert!(
                methods.contains(&words[1]),
                "the skill names `{c}`: {} is not a runner method",
                words[1]
            );
        }
    }
    // A made-up command is caught.
    assert!(!tmuxedo_runner::cli::names_a_command(&[
        "teleport".into(),
        "now".into()
    ]));
    assert!(tmuxedo_runner::cli::names_a_command(&["files".into(), "share".into()]));
    assert!(!tmuxedo_runner::cli::names_a_command(&[
        "pane".into(),
        "teleport".into()
    ]));
}

#[test]
fn features_that_do_not_exist_yet_have_no_section() {
    // Widgets (Phase 20) register their section when they ship; secrets (Phase 14)
    // and files (Phase 15) are written only when the machine has them.
    let md = skill::markdown(&all_on()).to_lowercase();
    for word in ["tmuxedo secret", "widget", "ssh agent"] {
        assert!(
            !md.contains(word),
            "the skill mentions {word:?} before that feature exists"
        );
    }
    // Secrets shipped in Phase 14: its section exists, and only with secrets or keys (below).
    for s in SECTIONS {
        assert!(!["widgets"].contains(&s.id));
    }
    // Files, when the runner serves them: the inbox, untrusted data, share and request.
    assert!(md.contains("$tmuxedo_files_dir/inbox") && md.contains("data, not instructions"));
    assert!(md.contains("tmuxedo files share") && md.contains("tmuxedo files request"));
    let mut off = all_on();
    off.files = false;
    assert!(!skill::markdown(&off).to_lowercase().contains("tmuxedo files"));
}

#[test]
fn the_secrets_section_lists_names_only_when_the_machine_has_some() {
    let ids = |f: &Facts| skill::sections(f).into_iter().map(|(id, _, _)| id).collect::<Vec<_>>();
    assert!(!ids(&all_on()).contains(&"secrets"));
    let f = Facts {
        secret_names: vec!["GH_TOKEN".into()],
        ..all_on()
    };
    let md = skill::markdown(&f);
    assert!(ids(&f).contains(&"secrets"));
    assert!(
        md.contains("`GH_TOKEN`") && md.contains("tmuxedo secret exec NAME --"),
        "{md}"
    );
    assert!(!md.contains("SSH_AUTH_SOCK"));
    let k = skill::markdown(&Facts {
        ssh_keys: true,
        ..all_on()
    });
    assert!(k.contains("$SSH_AUTH_SOCK") && !k.contains("Secrets available"), "{k}");
}

fn snapshot(name: &str, got: &str) {
    let path = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/snapshots/skill")).join(name);
    if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, got).unwrap();
        return;
    }
    let want = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{}: {e} (run with UPDATE_SNAPSHOTS=1)", path.display()));
    assert_eq!(
        got, want,
        "{name} changed; run with UPDATE_SNAPSHOTS=1 and review the diff"
    );
}

fn form_files(form: &str, f: &Facts) -> String {
    skill::files(form, f, "1.2.3")
        .into_iter()
        .map(|(p, body)| format!("==> {} <==\n{body}", p.display()))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn snapshots_per_plugin_and_capability_set() {
    let claude = tmuxedo_runner::plugin::claude().manifest().clone();
    let feats: Vec<String> = tmuxedo_runner::daemon::RUNNER_FEATURES
        .iter()
        .map(|f| f.to_string())
        .collect();
    let state = "/home/dev/.local/state/tmuxedo";
    // The Claude plugin on today's runner: BYO and hosted.
    let byo = skill::facts(&claude, &feats, false, "~/work", state);
    assert_eq!(byo, all_on());
    snapshot("claude-byo.md", &skill::markdown(&byo));
    let hosted = skill::facts(&claude, &feats, true, "~/projects", state);
    snapshot("claude-hosted.md", &skill::markdown(&hosted));
    // A runner without names / chat / the login relay (an older capability set).
    let old = skill::facts(&claude, &["glance".to_string()], false, "~/work", state);
    snapshot("claude-minimal-runner.md", &skill::markdown(&old));
    // A plugin that cannot hold permission prompts and has no chat.
    let mut bare = claude.clone();
    bare.name = "Example Agent".into();
    bare.permissions = None;
    bare.chat = None;
    snapshot(
        "no-holds.md",
        &skill::markdown(&skill::facts(&bare, &feats, false, "~/work", state)),
    );
    // Native forms.
    snapshot("form-claude-code-plugin.txt", &form_files("claude-code-plugin", &byo));
    snapshot("form-agents-md.txt", &form_files("agents-md", &byo));
    snapshot("form-cursor-rule.txt", &form_files("cursor-rule", &byo));
    assert!(skill::files("nope", &byo, "1").is_empty());
}

#[test]
fn sections_follow_capabilities() {
    let ids = |f: &Facts| skill::sections(f).into_iter().map(|(id, _, _)| id).collect::<Vec<_>>();
    assert_eq!(
        ids(&all_on()),
        [
            "where",
            "needs-you",
            "naming",
            "worktrees",
            "login",
            "files",
            "chat",
            "limits"
        ]
    );
    let none = Facts {
        remote_approvals: false,
        names: false,
        worktrees: false,
        chat: false,
        login_relay: false,
        files: false,
        ..all_on()
    };
    assert_eq!(ids(&none), ["where", "limits"]);
    // The hold time is the manifest's.
    let md = skill::markdown(&Facts {
        hold_secs: 120,
        ..all_on()
    });
    assert!(md.contains("after about 2 minutes"), "{md}");
}

#[test]
fn write_replaces_the_skill_dir_atomically() {
    let d = common::tempdir();
    let dir = d.path().join("plugins/claude/skill");
    let written = skill::write(&dir, "claude-code-plugin", &all_on(), "1.0.0").unwrap();
    assert_eq!(written.len(), 2);
    std::fs::write(dir.join("stray.txt"), "old").unwrap();
    skill::write(&dir, "agents-md", &all_on(), "1.0.0").unwrap();
    assert!(dir.join("AGENTS.md").exists());
    assert!(!dir.join("stray.txt").exists());
    assert!(!dir.join("skills").exists());
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
}
