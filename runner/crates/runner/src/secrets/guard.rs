//! The hook guard and the transcript scan (docs/design/secrets-and-env.md
//! §4.3, §5.3).
//!
//! - **Guard.** While a machine has secrets, an agent's `PreToolUse` hook
//!   (Claude, Codex; Bash commands) is checked here before the command
//!   runs: commands that dump the environment (`env`, `printenv`, `set`,
//!   `export -p`, `declare -p`, `/proc/*/environ`) or name a bound secret
//!   (`$GH_TOKEN`) outside `tmuxedo secret exec` are denied with a reason
//!   that says what to do instead. A heuristic for accidents and lazy
//!   injections, not a sandbox. On by default; off per plugin in policy.
//! - **Transcript scan.** On every completed turn whose hook names the
//!   agent's `transcript_path`, the bytes appended since the last scan are
//!   checked for every masked value (and its encodings). A hit raises a
//!   durable `secret_exposed {name, plugin, kind, transcript}` event (names
//!   only): the rotate banner, a needs-you item and a push. The transcript
//!   is never rewritten (it is the CLI's own format).

use regex::Regex;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;
use std::sync::LazyLock;

/// Commands that print the whole environment, as a command word.
static DUMP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?x)
        (?:^|[;&|({`]|\$\(|\bxargs\s+|\bsudo\s+|\bexec\s+|\bcommand\s+|\bbusybox\s+)\s*
        (?:/usr/bin/|/bin/)?(?:env|printenv|set|export\s+-p|declare\s+-[a-zA-Z]*p[a-zA-Z]*|typeset\s+-[a-zA-Z]*p|compgen\s+-v)
        \s*(?:$|[;&|)`>]|\s+[-|>]|\s*$)")
    .unwrap()
});
static ENVIRON: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"/proc/[^\s/]+/environ").unwrap());
/// `printenv` prints variables whatever its arguments.
static PRINTENV: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:^|[;&|({`\s]|\$\()(?:/usr/bin/|/bin/)?printenv(?:\s|$|[;&|)`])").unwrap());

/// Why `command` is refused, or None.
pub fn check(command: &str, names: &[String]) -> Option<String> {
    let c = command.trim();
    if c.is_empty() {
        return None;
    }
    if ENVIRON.is_match(c) {
        return Some("reads a process environment".into());
    }
    if DUMP.is_match(c) || PRINTENV.is_match(c) {
        return Some("prints the environment".into());
    }
    // `$NAME` / `${NAME…}` of a bound secret, except as the program's own
    // environment through `tmuxedo secret exec NAME -- …` (no `$` there).
    for n in names {
        let r = Regex::new(&format!(r"\$\{{?{}\b", regex::escape(n))).unwrap();
        if r.is_match(c) {
            return Some(format!("reads ${n}"));
        }
    }
    None
}

/// The hook reply that denies a tool call (Claude Code / Codex `PreToolUse`).
pub fn deny_reply(why: &str, names: &[String]) -> Value {
    let example = names.first().map(String::as_str).unwrap_or("NAME");
    json!({"hookSpecificOutput": {
        "hookEventName": "PreToolUse",
        "permissionDecision": "deny",
        "permissionDecisionReason": format!(
            "tmuxedo blocked this command: it {why}, which could show a secret. Secrets are never printed. \
             To use one, run the program through tmuxedo: `tmuxedo secret exec {example} -- <program> <args>` \
             (`tmuxedo secret list` shows which programs may have which secret)."
        ),
    }})
}

/// The shell command of a pre-tool hook event, if it is one.
pub fn command_of(ev: &Value) -> Option<&str> {
    let pre = matches!(
        ev["hook_event_name"].as_str(),
        Some("PreToolUse") | Some("beforeShellExecution")
    );
    if !pre {
        return None;
    }
    ev["tool_input"]["command"].as_str().or(ev["command"].as_str())
}

/// Add or remove the guard's `PreToolUse` registration in a generated
/// hook-settings file (`{"hooks": {Event: [{matcher?, hooks: [...]}]}}`):
/// the same command as the plugin's other hooks, Bash only.
pub fn patch_settings(path: &Path, on: bool) -> std::io::Result<bool> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Ok(false);
    };
    let Ok(mut v) = serde_json::from_str::<Value>(&text) else {
        return Ok(false);
    };
    let Some(hooks) = v["hooks"].as_object_mut() else {
        return Ok(false);
    };
    let has = hooks.contains_key("PreToolUse");
    if on == has {
        return Ok(false);
    }
    if on {
        // Only for agents with Claude's hook protocol (they register PermissionRequest).
        let Some(cmd) = hooks
            .get("PermissionRequest")
            .and_then(|e| e[0]["hooks"][0]["command"].as_str())
            .map(String::from)
        else {
            return Ok(false);
        };
        hooks.insert(
            "PreToolUse".into(),
            json!([{"matcher": "Bash", "hooks": [{"type": "command", "command": cmd, "timeout": 30}]}]),
        );
    } else {
        hooks.remove("PreToolUse");
    }
    // Atomically: the agent (and anyone reading it) never sees a half-written file.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&v)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(true)
}

/// The line a plugin's launch shim runs first when the plugin's agent has
/// secrets bound to its own environment (secrets-and-env.md §4 #4).
pub const AGENT_ENV_MARK: &str = "# tmuxedo: agent env (secrets bound to this agent)";

pub fn patch_shim(path: &Path, exe: &Path, plugin: &str, on: bool) -> std::io::Result<bool> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Ok(false);
    };
    let has = text.contains(AGENT_ENV_MARK);
    if has == on {
        return Ok(false);
    }
    let next = if on {
        let line = format!(
            "{AGENT_ENV_MARK}\neval \"$({} cli secret agent-env {} 2>/dev/null)\"\n",
            crate::plugin::sh_quote(&exe.display().to_string()),
            crate::plugin::sh_quote(plugin)
        );
        match text.split_once('\n') {
            Some((first, rest)) if first.starts_with("#!") => format!("{first}\n{line}{rest}"),
            _ => format!("{line}{text}"),
        }
    } else {
        let mut out = String::new();
        let mut skip = 0;
        for l in text.split_inclusive('\n') {
            if l.trim_end() == AGENT_ENV_MARK {
                skip = 2;
            }
            if skip > 0 {
                skip -= 1;
                continue;
            }
            out.push_str(l);
        }
        out
    };
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, next)?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    std::fs::rename(&tmp, path)?;
    Ok(true)
}

/// Where each transcript was read up to (bytes).
#[derive(Default)]
pub struct Transcripts {
    seen: HashMap<String, u64>,
}

/// Longest tail read per scan (a turn rarely appends more).
pub const SCAN_MAX: u64 = 16 * 1024 * 1024;

impl Transcripts {
    /// The bytes appended to `path` since the last call (the first call
    /// reads from the start, at most [`SCAN_MAX`] from the end). A file that
    /// shrank (a new session) is read from the start again.
    pub fn tail(&mut self, path: &str) -> Option<Vec<u8>> {
        use std::io::{Read, Seek, SeekFrom};
        let mut f = std::fs::File::open(path).ok()?;
        let len = f.metadata().ok()?.len();
        let from = match self.seen.get(path) {
            Some(&o) if o <= len => o,
            _ => 0,
        }
        .max(len.saturating_sub(SCAN_MAX));
        // Keep a little overlap so a value written across two turns is seen.
        let start = from.saturating_sub(256);
        f.seek(SeekFrom::Start(start)).ok()?;
        let mut buf = vec![];
        f.take(len - start).read_to_end(&mut buf).ok()?;
        self.seen.insert(path.to_string(), len);
        (len > from).then_some(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dumps_and_named_reads_are_refused() {
        let names = vec!["GH_TOKEN".to_string(), "API_KEY".to_string()];
        for c in [
            "env",
            "env | grep TOKEN",
            "printenv",
            "printenv GH_TOKEN",
            "set",
            "export -p",
            "declare -xp",
            "cd /x && env",
            "ls; /usr/bin/env",
            "cat /proc/self/environ",
            "cat /proc/1234/environ | tr '\\0' '\\n'",
            "echo $GH_TOKEN",
            "curl -H \"Authorization: token ${GH_TOKEN}\" https://api.github.com",
            "echo \"$(printenv)\"",
            "sudo env",
        ] {
            assert!(check(c, &names).is_some(), "{c}");
        }
        for c in [
            "ls -la",
            "git status",
            "tmuxedo secret exec GH_TOKEN -- gh api user",
            "env FOO=1 make",
            "npm run env-check",
            "echo $HOME",
            "cat environment.md",
            "echo $GH_TOKENS_URL",
            "settings",
            "make setup",
        ] {
            assert_eq!(check(c, &names), None, "{c}");
        }
        let r = deny_reply("prints the environment", &names);
        assert_eq!(r["hookSpecificOutput"]["permissionDecision"], "deny");
        assert!(r.to_string().contains("tmuxedo secret exec GH_TOKEN"));
    }

    #[test]
    fn only_pre_tool_events_carry_a_command() {
        assert_eq!(
            command_of(&json!({"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"env"}})),
            Some("env")
        );
        assert_eq!(
            command_of(&json!({"hook_event_name":"beforeShellExecution","command":"env"})),
            Some("env")
        );
        assert_eq!(
            command_of(&json!({"hook_event_name":"PostToolUse","tool_input":{"command":"env"}})),
            None
        );
    }

    #[test]
    fn settings_and_shims_are_patched_both_ways() {
        let d = tempfile::tempdir().unwrap();
        let s = d.path().join("settings.json");
        std::fs::write(
            &s,
            r#"{"hooks":{"PermissionRequest":[{"matcher":"*","hooks":[{"type":"command","command":"'/x/runner' hook","timeout":600}]}],"Stop":[{"hooks":[{"type":"command","command":"'/x/runner' hook"}]}]}}"#,
        )
        .unwrap();
        assert!(patch_settings(&s, true).unwrap());
        assert!(!patch_settings(&s, true).unwrap(), "idempotent");
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&s).unwrap()).unwrap();
        assert_eq!(v["hooks"]["PreToolUse"][0]["matcher"], "Bash");
        assert_eq!(v["hooks"]["PreToolUse"][0]["hooks"][0]["command"], "'/x/runner' hook");
        assert!(patch_settings(&s, false).unwrap());
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&s).unwrap()).unwrap();
        assert!(v["hooks"].get("PreToolUse").is_none());
        // No PermissionRequest: not an agent with this hook protocol.
        let o = d.path().join("other.json");
        std::fs::write(&o, r#"{"hooks":{"Stop":[]}}"#).unwrap();
        assert!(!patch_settings(&o, true).unwrap());

        let sh = d.path().join("claude");
        let body = "#!/bin/sh\necho real\n";
        std::fs::write(&sh, body).unwrap();
        assert!(patch_shim(&sh, Path::new("/x/runner"), "claude", true).unwrap());
        let t = std::fs::read_to_string(&sh).unwrap();
        assert!(t.starts_with("#!/bin/sh\n# tmuxedo: agent env"));
        assert!(t.contains("cli secret agent-env 'claude'") || t.contains("cli secret agent-env claude"));
        assert!(!patch_shim(&sh, Path::new("/x/runner"), "claude", true).unwrap());
        assert!(patch_shim(&sh, Path::new("/x/runner"), "claude", false).unwrap());
        assert_eq!(std::fs::read_to_string(&sh).unwrap(), body);
    }

    #[test]
    fn transcript_tails_are_read_once() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("t.jsonl");
        std::fs::write(&p, "one\n").unwrap();
        let mut t = Transcripts::default();
        let ps = p.display().to_string();
        assert_eq!(t.tail(&ps).unwrap(), b"one\n");
        assert!(t.tail(&ps).is_none(), "nothing new");
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(&p)
            .unwrap()
            .write_all(b"two\n")
            .unwrap();
        let tail = t.tail(&ps).unwrap();
        assert!(tail.ends_with(b"two\n"));
        std::fs::write(&p, "new\n").unwrap();
        assert_eq!(t.tail(&ps).unwrap(), b"new\n", "a shorter file starts over");
        assert!(t.tail("/nonexistent/x").is_none());
    }
}
