//! `tmuxedo keys` (secrets-and-env.md §6a, §8): your SSH keys and secrets
//! from the terminal, through the web API with the CLI's token.
//!
//! - `tmuxedo keys` — SSH keys (where each lives, its limits, fingerprint)
//!   and secrets (names, last 4 characters, uses). Never a value.
//! - `tmuxedo keys revoke <name>` — revoke an SSH key on every machine
//!   (the CLI token needs the `terminal` scope).
//! - `tmuxedo keys github <name>` — print the public key and the GitHub page
//!   to add it to (the repository's deploy keys for a project key).
//!
//! Adding secrets and keys stays in the web app and the phone app, where a
//! value is typed once and a passkey confirms it.

use super::{api, load_cli_config, CliConfig};
use serde_json::Value;

pub fn run(args: &[String]) -> Result<(), String> {
    let cfg = load_cli_config()?;
    match args.first().map(String::as_str) {
        None | Some("list") => list(&cfg),
        Some("revoke") => revoke(&cfg, args.get(1).ok_or("keys revoke <name>")?),
        Some("github") => github(&cfg, args.get(1).ok_or("keys github <name>")?),
        Some(o) => Err(format!(
            "unknown keys command {o}: list | revoke <name> | github <name>"
        )),
    }
}

fn get(cfg: &CliConfig, path: &str) -> Result<Value, String> {
    let (status, v) = api(cfg, "GET", path, None)?;
    if status != 200 {
        return Err(v["error"]["message"].as_str().unwrap_or("request failed").to_string());
    }
    Ok(v)
}

pub fn where_label(k: &Value) -> String {
    if k["kind"] == "vault" {
        return "vault".into();
    }
    let hw = if k["hardware"] == "secure_enclave" {
        "Secure Enclave"
    } else {
        "on disk"
    };
    format!("{} ({hw})", k["machine"]["name"].as_str().unwrap_or("machine"))
}

pub fn limits_label(k: &Value) -> String {
    let dest: Vec<&str> = k["destinations"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let mut parts = vec![if dest.is_empty() {
        "any host".to_string()
    } else {
        dest.join(",")
    }];
    match k["confirm"].as_str() {
        Some("each") => parts.push("asks each time".into()),
        Some("each_stepup") => parts.push("asks with a passkey".into()),
        _ => {}
    }
    if k["signing"] == true {
        parts.push("signs commits".into());
    }
    parts.join(" · ")
}

fn list(cfg: &CliConfig) -> Result<(), String> {
    let keys = get(cfg, "/api/me/ssh-keys")?;
    let secrets = get(cfg, "/api/me/secrets")?;
    println!("SSH KEYS");
    let ks = keys["keys"].as_array().cloned().unwrap_or_default();
    if ks.is_empty() {
        println!("  none (make one on a machine in Settings → Secrets & keys)");
    }
    for k in &ks {
        println!(
            "  {:<20} {:<26} {}\n  {:<20} {}",
            k["name"].as_str().unwrap_or("?"),
            where_label(k),
            limits_label(k),
            "",
            k["fingerprint"].as_str().unwrap_or("")
        );
    }
    println!("\nSECRETS");
    let ss = secrets["secrets"].as_array().cloned().unwrap_or_default();
    if ss.is_empty() {
        println!("  none");
    }
    for s in &ss {
        let uses: Vec<&str> = s["bindings"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|b| b["kind"].as_str())
            .collect();
        let warn = if s["exposed"].is_object() {
            "  ⚠ seen in an agent's conversation: rotate it"
        } else {
            ""
        };
        println!(
            "  {:<20} …{} v{:<3} {:<16} {}{warn}",
            s["name"].as_str().unwrap_or("?"),
            s["hint"].as_str().unwrap_or(""),
            s["version"].as_u64().unwrap_or(0),
            s["scopeLabel"].as_str().unwrap_or(""),
            if uses.is_empty() {
                "not used yet".to_string()
            } else {
                uses.join(",")
            }
        );
    }
    Ok(())
}

fn find(cfg: &CliConfig, name: &str) -> Result<Value, String> {
    let keys = get(cfg, "/api/me/ssh-keys")?;
    keys["keys"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|k| k["name"] == name || k["id"] == name)
        .cloned()
        .ok_or_else(|| format!("no SSH key named {name}"))
}

fn revoke(cfg: &CliConfig, name: &str) -> Result<(), String> {
    let k = find(cfg, name)?;
    let id = k["id"].as_str().unwrap_or("");
    let (status, v) = api(cfg, "DELETE", &format!("/api/me/ssh-keys/{id}"), None)?;
    if status != 200 {
        return Err(v["error"]["message"].as_str().unwrap_or("refused").to_string());
    }
    println!("Revoked {name}: every machine drops it now. Remove it from GitHub too.");
    Ok(())
}

/// The GitHub page for a key: a project key's repository deploy keys, else the account's.
pub fn github_url(k: &Value) -> String {
    let r = k["scopeRef"].as_str().unwrap_or("");
    if k["scope"] == "project" && k["signing"] != true {
        if let Some(rest) = r.strip_prefix("github.com/") {
            if rest.split('/').count() == 2 {
                return format!("https://github.com/{rest}/settings/keys/new");
            }
        }
    }
    "https://github.com/settings/ssh/new".into()
}

fn github(cfg: &CliConfig, name: &str) -> Result<(), String> {
    let k = find(cfg, name)?;
    println!("{}", k["publicKey"].as_str().unwrap_or(""));
    eprintln!("Add it at {}", github_url(&k));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn labels_and_github_pages() {
        let k = json!({"kind":"machine","machine":{"name":"laptop"},"hardware":"secure_enclave","destinations":["github.com"],"confirm":"each","signing":true,"scope":"project","scopeRef":"github.com/o/r"});
        assert_eq!(where_label(&k), "laptop (Secure Enclave)");
        assert_eq!(limits_label(&k), "github.com · asks each time · signs commits");
        assert_eq!(
            github_url(&k),
            "https://github.com/settings/ssh/new",
            "a signing key belongs to the account"
        );
        let d =
            json!({"kind":"vault","destinations":[],"confirm":"never","scope":"project","scopeRef":"github.com/o/r"});
        assert_eq!(where_label(&d), "vault");
        assert_eq!(limits_label(&d), "any host");
        assert_eq!(github_url(&d), "https://github.com/o/r/settings/keys/new");
    }
}
