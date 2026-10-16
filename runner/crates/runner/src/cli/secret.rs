//! `tmuxedo secret …` (docs/design/secrets-and-env.md §4): use a secret
//! without seeing it.
//!
//! - `tmuxedo secret list` — the names this machine may use, and how.
//! - `tmuxedo secret exec NAME… -- cmd …` — run one allowed program with the
//!   values in its environment (the runner starts it; secrets/exec.rs).
//! - `tmuxedo secret exec --mcp <plugin>/<server> NAME… -- cmd …` — the same
//!   for an MCP server a plugin's generated config starts.
//! - `tmuxedo secret git-credential get` — git's credential helper protocol
//!   (the runner sets `credential.helper` in panes when a secret is bound to
//!   git hosts).
//! - `tmuxedo secret agent-env <plugin>` — `export` lines for an agent's own
//!   environment, read by that plugin's launch shim.

use super::call_local;
use serde_json::{json, Value};
use std::io::{BufRead, Write};

pub fn run(args: &[String]) -> i32 {
    let res = match args.first().map(String::as_str) {
        Some("list") => list(),
        Some("exec") => return exec(&args[1..]),
        Some("git-credential") => git_credential(args.get(1).map(String::as_str).unwrap_or("")),
        Some("agent-env") => agent_env(args.get(1).map(String::as_str).unwrap_or("")),
        _ => Err("secret list | exec NAME… -- cmd | git-credential get | agent-env <plugin>".into()),
    };
    match res {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("tmuxedo: {e}");
            1
        }
    }
}

fn result(v: Value) -> Result<Value, String> {
    match v.get("error") {
        Some(e) => Err(format!(
            "{}: {}",
            e["code"].as_str().unwrap_or("error"),
            e["message"].as_str().unwrap_or("")
        )),
        None => Ok(v["result"].clone()),
    }
}

fn list() -> Result<(), String> {
    let r = result(call_local("secret.list", json!({}))?)?;
    let rows = r["secrets"].as_array().cloned().unwrap_or_default();
    if rows.is_empty() {
        println!("No secrets are bound to this machine (Settings → Secrets).");
    }
    for s in rows {
        let uses: Vec<&str> = s["uses"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        println!("{:<24} {}", s["name"].as_str().unwrap_or("?"), uses.join("; "));
    }
    Ok(())
}

/// `[--mcp p/s] NAME… -- cmd …` → (names, argv, mcp).
/// Secret names, the command, and the MCP server it is for.
pub type ExecArgs = (Vec<String>, Vec<String>, Option<(String, String)>);

pub fn parse_exec(args: &[String]) -> Result<ExecArgs, String> {
    let Some(dd) = args.iter().position(|a| a == "--") else {
        return Err("usage: tmuxedo secret exec NAME… -- command [args…]".into());
    };
    let mut names = vec![];
    let mut mcp = None;
    let mut it = args[..dd].iter();
    while let Some(a) = it.next() {
        if a == "--mcp" {
            let v = it.next().ok_or("--mcp needs <plugin>/<server>")?;
            let (p, s) = v.split_once('/').ok_or("--mcp needs <plugin>/<server>")?;
            mcp = Some((p.to_string(), s.to_string()));
        } else if a.starts_with('-') {
            return Err(format!("unknown option {a}"));
        } else {
            names.push(a.clone());
        }
    }
    let argv = args[dd + 1..].to_vec();
    if names.is_empty() || argv.is_empty() {
        return Err("usage: tmuxedo secret exec NAME… -- command [args…]".into());
    }
    Ok((names, argv, mcp))
}

fn exec(args: &[String]) -> i32 {
    let (names, argv, mcp) = match parse_exec(args) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("tmuxedo: {e}");
            return 2;
        }
    };
    let sock = crate::secrets::exec::socket_path(&super::socket_path());
    match crate::secrets::exec::client(
        &sock,
        &names,
        &argv,
        mcp.as_ref().map(|(p, s)| (p.as_str(), s.as_str())),
    ) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("tmuxedo secret exec: {e}");
            126
        }
    }
}

/// git credential helper: answer `get` for the host git names; ignore
/// `store`/`erase` (the vault is the store).
fn git_credential(op: &str) -> Result<(), String> {
    if op != "get" {
        return Ok(());
    }
    let mut host = String::new();
    let mut protocol = String::new();
    for line in std::io::stdin().lock().lines().map_while(Result::ok) {
        if line.is_empty() {
            break;
        }
        if let Some(v) = line.strip_prefix("host=") {
            host = v.to_string();
        } else if let Some(v) = line.strip_prefix("protocol=") {
            protocol = v.to_string();
        }
    }
    if protocol != "https" || host.is_empty() {
        return Ok(());
    }
    let host_only = host.split(':').next().unwrap_or("").to_string();
    // Not bound, offline or refused: say nothing, so git tries its next helper.
    let Ok(r) = result(call_local("secret.git_credential", json!({"host": host_only}))?) else {
        return Ok(());
    };
    let Some((_, v)) = r["values"].as_object().and_then(|m| m.iter().next()) else {
        return Ok(());
    };
    let user = if host_only.contains("gitlab") {
        "oauth2"
    } else {
        "x-access-token"
    };
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "username={user}\npassword={}\n", v.as_str().unwrap_or(""));
    Ok(())
}

/// `tmuxedo project-env [--shell]`: the project env for the current folder.
/// With `--shell`: lines for the cd hook to eval (export, unset, and the
/// names it set, kept in `__TMUXEDO_PROJECT_VARS` for the next change).
pub fn project_env(args: &[String]) -> Result<(), String> {
    let shell = args.iter().any(|a| a == "--shell");
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let previous: Vec<String> = std::env::var("__TMUXEDO_PROJECT_VARS")
        .unwrap_or_default()
        .split_whitespace()
        .map(String::from)
        .collect();
    let r = result(call_local("project.env", json!({"cwd": cwd, "previous": previous}))?)?;
    let mut out = std::io::stdout().lock();
    if !shell {
        let _ = writeln!(out, "{}", serde_json::to_string_pretty(&r).unwrap_or_default());
        return Ok(());
    }
    let ok = |k: &str| !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    for (k, v) in r["set"].as_object().into_iter().flatten() {
        if ok(k) {
            let _ = writeln!(out, "export {k}={}", crate::plugin::sh_quote(v.as_str().unwrap_or("")));
        }
    }
    for k in r["unset"].as_array().into_iter().flatten().filter_map(Value::as_str) {
        if ok(k) {
            let _ = writeln!(out, "unset {k}");
        }
    }
    let names: Vec<&str> = r["names"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|k| ok(k))
        .collect();
    let _ = writeln!(
        out,
        "__TMUXEDO_PROJECT_VARS={}",
        crate::plugin::sh_quote(&names.join(" "))
    );
    Ok(())
}

fn agent_env(plugin: &str) -> Result<(), String> {
    if plugin.is_empty() {
        return Err("agent-env <plugin>".into());
    }
    let r = match result(call_local("secret.agent_env", json!({"plugin": plugin}))?) {
        Ok(r) => r,
        Err(e) => {
            // The agent starts anyway, without them (the plugin says so).
            eprintln!("tmuxedo: {e}");
            return Ok(());
        }
    };
    let mut out = std::io::stdout().lock();
    for (k, v) in r["values"].as_object().into_iter().flatten() {
        if !k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            continue;
        }
        let _ = writeln!(out, "export {k}={}", crate::plugin::sh_quote(v.as_str().unwrap_or("")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn exec_arguments() {
        let (n, argv, mcp) = parse_exec(&a("GH_TOKEN NPM -- gh api user")).unwrap();
        assert_eq!((n, argv, mcp), (a("GH_TOKEN NPM"), a("gh api user"), None));
        let (_, _, mcp) = parse_exec(&a("--mcp claude/github GH_TOKEN -- npx server")).unwrap();
        assert_eq!(mcp, Some(("claude".into(), "github".into())));
        assert!(parse_exec(&a("GH_TOKEN gh")).is_err());
        assert!(parse_exec(&a("-- gh")).is_err());
        assert!(parse_exec(&a("X --")).is_err());
        assert!(parse_exec(&a("--mcp nope X -- y")).is_err());
        assert!(parse_exec(&a("--weird X -- y")).is_err());
    }
}
