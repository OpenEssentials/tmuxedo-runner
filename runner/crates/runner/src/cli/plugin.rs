//! `tmuxedo plugin …`
//!
//! - `plugin test [<dir>]` — the conformance suite (docs/design/plugins.md
//!   §11 steps 1–4) on a plugin directory, or on every built-in plugin when
//!   no directory is given. Exit 1 if any check fails. A directory's
//!   `SIGNATURE` is verified against the compiled-in keys and reported, but
//!   an unsigned directory is still tested (that is what authors do locally).
//! - `plugin list | enable <id> | disable <id> | actions [<id>] |
//!   invoke <action> [--plugin <id>] | log [<id>]` — the runner API's `plugin.*`
//!   against the local runner socket.

use crate::plugin::{builtin, conformance, sign};
use serde_json::json;
use std::path::Path;

pub fn run(rest: &[String]) -> Result<(), String> {
    let a = super::parse_args(rest.get(1..).unwrap_or_default());
    let local = |m: &str, p: serde_json::Value| super::print_local(m, p);
    match rest.first().map(String::as_str) {
        Some("test") => test(a.pos.first().map(String::as_str)),
        Some("smoke") => smoke(&a),
        Some("list") => local("plugin.list", json!({ "plugin_id": a.pos.first() })),
        Some(v @ ("enable" | "disable")) => {
            let id = a.pos.first().ok_or(format!("plugin {v} <id>"))?;
            local(&format!("plugin.{v}"), json!({ "plugin_id": id }))
        }
        Some("actions") => local("plugin.action.list", json!({ "plugin_id": a.pos.first() })),
        Some("invoke") => {
            let action = a.pos.first().ok_or("plugin invoke <action> [--plugin <id>]")?;
            let pane = tmuxedo_holder::env_var("PANE_ID");
            local(
                "plugin.action.invoke",
                json!({ "plugin_id": a.f("--plugin"), "action_id": action,
                        "context": { "focused_pane_id": pane, "invocation_source": "cli" } }),
            )
        }
        Some("log") => local("plugin.log.list", json!({ "plugin_id": a.pos.first() })),
        _ => Err(
            "plugin test [<dir>] | smoke <id> [--expect A,B] [--timeout S] -- <argv> | list | enable <id> | disable <id> | actions | invoke <action> | log"
                .into(),
        ),
    }
}

/// `plugin smoke <id> [--expect Ev1,Ev2] [--timeout 300] -- <argv…>`: run the
/// real CLI through the built-in plugin's shim against a fake runner; fail
/// unless every `--expect`ed native hook arrived (the weekly smoke job,
/// .github/workflows/agent-cli-smoke.yml).
fn smoke(a: &super::Args) -> Result<(), String> {
    let id = a.pos.first().ok_or("plugin smoke <id> -- <argv>")?;
    let files = builtin::package(id).ok_or_else(|| format!("no built-in plugin {id}"))?;
    let text = String::from_utf8_lossy(&files["plugin.toml"]).into_owned();
    let plugin = crate::plugin::ManifestPlugin::new(crate::plugin::Manifest::parse(&text)?);
    let exe = crate::update::current_exe().map_err(|e| format!("this binary: {e}"))?;
    if a.has("--print-argv") {
        // The hook flags this plugin's shim passes, shell-quoted (Codex's
        // `-c 'hooks.…'`), for a `--strict-config` check of the real CLI.
        let argv = crate::plugin::AgentPlugin::hooks_argv(&plugin, &exe);
        let q: Vec<String> = argv.iter().map(|t| crate::plugin::sh_quote(t)).collect();
        println!("{}", q.join(" "));
        return Ok(());
    }
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let timeout = std::time::Duration::from_secs(a.num("--timeout").unwrap_or(300));
    let argv = a.all("--");
    let r = conformance::smoke(&plugin, &exe, dir.path(), &argv, timeout)?;
    println!("smoke {id}: exit {:?}; hooks {}", r.status, r.hooks.join(" "));
    let want: Vec<String> = a
        .f("--expect")
        .map(|e| e.split(',').filter(|x| !x.is_empty()).map(str::to_owned).collect())
        .unwrap_or_default();
    let missing: Vec<&String> = want.iter().filter(|w| !r.hooks.contains(w)).collect();
    if !missing.is_empty() {
        return Err(format!("smoke {id}: expected hooks did not arrive: {missing:?}"));
    }
    if r.status.is_none() {
        return Err(format!(
            "smoke {id}: the CLI did not finish within {} s",
            timeout.as_secs()
        ));
    }
    Ok(())
}

fn test(dir: Option<&str>) -> Result<(), String> {
    let exe = crate::update::current_exe().map_err(|e| format!("this binary: {e}"))?;
    let packages: Vec<(String, sign::Files, String)> = match dir {
        Some(d) => {
            let files = sign::read_dir(Path::new(d))?;
            let sig = match sign::verify(&files) {
                Ok(kid) => format!("signed (key {kid})"),
                Err(e) => format!("unsigned for this runner: {e}"),
            };
            vec![(d.to_string(), files, sig)]
        }
        None => builtin::packages()
            .into_iter()
            .map(|(id, f)| (format!("builtin:{id}"), f, "built in (part of this binary)".into()))
            .collect(),
    };
    let mut failed = 0;
    for (src, files, sig) in packages {
        let r = conformance::run(&files, Some(&exe));
        print!("{}", r.render());
        println!("  source {src}; {sig}");
        if !r.ok() {
            failed += 1;
        }
    }
    if failed > 0 {
        Err(format!("{failed} plugin(s) failed conformance"))
    } else {
        Ok(())
    }
}
