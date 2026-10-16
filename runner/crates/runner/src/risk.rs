//! Risk classes of a permission request (D-22, not-yet-thought-of.md #2):
//! pushes, package installs, administrator commands, deletes and edits
//! outside the project, network use and credential reads are **high risk**.
//! A high-risk item is never approved from a notification action, only
//! from the full sheet, with a step-up in the last 5 minutes. Shared by the
//! terminal's needs-you items and agent chat (crate::chat).
//!
//! The tool names are the common agent vocabulary (`Bash`/`shell` run a
//! command, `Write`/`Edit` change a file, `Read`/`Grep` read, `WebFetch`
//! fetches); an unknown tool is normal risk.

use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Risk {
    /// One of tmuxedo_protocol::needs::RISK_CLASSES.
    pub class: &'static str,
    /// In plain words, for the sheet.
    pub why: &'static str,
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}

fn command_risk(cmd: &str) -> Option<Risk> {
    use regex::Regex;
    use std::sync::LazyLock;
    static RULES: LazyLock<Vec<(Regex, Risk)>> = LazyLock::new(|| {
        [
            (r"\bgit\s+(?:\S+\s+)*push\b", "git_push", "Pushes to a remote"),
            (
                r"\b(?:(?:npm|pnpm|yarn|bun)\s+(?:i|install|add)|pip3?\s+install|uv\s+(?:pip\s+install|add)|cargo\s+(?:install|add)|gem\s+install|brew\s+install|apt(?:-get)?\s+install|go\s+(?:get|install))\b",
                "install",
                "Installs packages",
            ),
            (r"\bsudo\b", "admin", "Runs as an administrator"),
            (
                r"\brm\s+(?:-\S+\s+)*(?:/|~|\$HOME|\.\./)",
                "delete_outside",
                "Deletes files outside the project",
            ),
            (
                r"\b(?:curl|wget|nc|ncat|ssh|scp|sftp|rsync|ftp|telnet)\b",
                "network",
                "Uses the network",
            ),
            (CREDENTIALS, "credentials", "Reads credentials"),
        ]
        .into_iter()
        .map(|(r, class, why)| (Regex::new(r).expect("risk rule"), Risk { class, why }))
        .collect()
    });
    RULES.iter().find(|(re, _)| re.is_match(cmd)).map(|(_, r)| *r)
}

const CREDENTIALS: &str =
    r"(?:\.env\b|id_rsa|id_ed25519|\.pem\b|\.aws/credentials|\.netrc|\.npmrc|\.pypirc|\.ssh/|credentials\.json)";

fn reads_credentials(text: &str) -> bool {
    use regex::Regex;
    use std::sync::LazyLock;
    static RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(CREDENTIALS).expect("credentials rule"));
    RE.is_match(text)
}

/// The risk of `tool` with `input`, run in `cwd`; None = normal risk.
pub fn classify(tool: &str, input: &Value, cwd: &str) -> Option<Risk> {
    let path = s(input, "file_path");
    let outside =
        |p: &str| p.starts_with('/') && !cwd.is_empty() && !p.starts_with(&format!("{}/", cwd.trim_end_matches('/')));
    match tool {
        "Bash" | "shell" | "Shell" | "exec" | "run_shell_command" => command_risk(s(input, "command")),
        "Read" | "Grep" | "Glob" => {
            let p = format!("{path} {} {}", s(input, "path"), s(input, "pattern"));
            reads_credentials(&p).then_some(Risk {
                class: "credentials",
                why: "Reads credentials",
            })
        }
        "Edit" | "MultiEdit" | "Write" | "NotebookEdit" if outside(path) => Some(Risk {
            class: "edit_outside",
            why: "Changes files outside the project",
        }),
        "WebFetch" => Some(Risk {
            class: "network",
            why: "Uses the network",
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn classes() {
        let b = |c: &str| classify("Bash", &json!({ "command": c }), "/w").map(|r| r.class);
        assert_eq!(b("git push origin main"), Some("git_push"));
        assert_eq!(b("git -C x push"), Some("git_push"));
        assert_eq!(b("npm install --save-dev vitest"), Some("install"));
        assert_eq!(b("sudo ls"), Some("admin"));
        assert_eq!(b("rm -rf ~/old"), Some("delete_outside"));
        assert_eq!(b("curl https://x"), Some("network"));
        assert_eq!(b("cat .env"), Some("credentials"));
        assert_eq!(b("npm test"), None);
        assert_eq!(b("rm -rf build"), None);
        let w = |p: &str| classify("Write", &json!({ "file_path": p }), "/w").map(|r| r.class);
        assert_eq!(w("/etc/hosts"), Some("edit_outside"));
        assert_eq!(w("/w/a.md"), None);
        assert_eq!(w("/wx/a.md"), Some("edit_outside"), "a sibling folder is outside");
        assert_eq!(
            classify("Read", &json!({"file_path": "/w/.env"}), "/w").map(|r| r.class),
            Some("credentials")
        );
        assert_eq!(classify("WebFetch", &json!({}), "/w").map(|r| r.class), Some("network"));
        // A shell tool by another name is judged by its command; an unknown tool is normal.
        assert_eq!(
            classify("shell", &json!({"command": "git push"}), "/w").map(|r| r.class),
            Some("git_push")
        );
        assert_eq!(classify("mcp__x__run", &json!({"command": "git push"}), "/w"), None);
        // Every class the classifier names is a protocol class.
        for (tool, input) in [
            ("Bash", json!({"command": "git push"})),
            ("Bash", json!({"command": "pip install x"})),
            ("Bash", json!({"command": "sudo x"})),
            ("Bash", json!({"command": "rm /x"})),
            ("Bash", json!({"command": "ssh h"})),
            ("Bash", json!({"command": "cat id_rsa"})),
            ("Write", json!({"file_path": "/etc/x"})),
        ] {
            let r = classify(tool, &input, "/w").unwrap();
            assert!(tmuxedo_protocol::needs::RISK_CLASSES.contains(&r.class), "{}", r.class);
        }
    }
}
