//! The plugin host (docs/design/plugins.md; D-1, D-2, D-4, D-13).
//!
//! Plugin logic runs **only on the runner**. Internally the host is the
//! [`AgentPlugin`] trait; its default methods interpret a declarative
//! manifest ([`manifest::Manifest`]), and a first-party plugin may override a
//! method in Rust where a table is not enough. First-party plugins are
//! embedded in the binary ([`builtin`]); curated packages are verified
//! against the compiled-in signing keys at every load ([`sign`]).
//!
//! What the host does for a plugin:
//! - renders its PATH shim and generated files (`launch`, `generate`);
//! - registers its hooks with the agent and maps native hook payloads to the
//!   normalised [`events::AgentEvent`]s the daemon acts on (`hooks`);
//! - holds permission prompts and prints the plugin's reply template
//!   (`permissions`);
//! - builds the resume argv (`resume`) and the login-health rules (`auth`);
//! - lists and runs its actions and keeps a log (runner API `plugin.*`, §13).

pub mod conformance;
pub mod events;
pub mod manifest;
mod manifest_usage;
pub mod sign;

pub use events::{AgentEvent, HookOutcome};
pub use manifest::Manifest;

use serde_json::{json, Value};
use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

/// `tmuxedo-runner hook` with no `--plugin` means this plugin, so settings
/// files generated before the host existed keep working (§12 step 2).
pub const DEFAULT_PLUGIN: &str = "claude";

/// The program names that open a URL; each gets a PATH stand-in (core: every
/// plugin's agent benefits, plugins.md §4 row 15).
pub const OPENERS: &[&str] = &["xdg-open", "open", "sensible-browser", "x-www-browser", "www-browser"];

/// Log entries kept per machine (`plugin.log.list`).
pub const LOG_CAP: usize = 200;

/// POSIX single-quoted literal.
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

// ── built-in packages ────────────────────────────────────────────────────

/// First-party packages embedded in the binary.
pub mod builtin {
    use super::sign::Files;

    macro_rules! pkg {
        ($id:literal, [$($f:literal),* $(,)?]) => {{
            let mut files = Files::new();
            $( files.insert($f.to_string(), include_bytes!(concat!("../../plugins/", $id, "/", $f)).to_vec()); )*
            ($id, files)
        }};
    }

    /// (id, files) for every built-in plugin.
    pub fn packages() -> Vec<(&'static str, Files)> {
        vec![
            pkg!(
                "claude",
                [
                    "plugin.toml",
                    "tests/cli.toml",
                    "tests/fake-cli.sh",
                    "tests/hooks/lifecycle.jsonl",
                    "tests/hooks/replies.jsonl",
                    "tests/screens/permission.blocked.txt",
                    "tests/screens/prompt-box.idle.txt",
                    "tests/screens/thinking.working.txt",
                ]
            ),
            pkg!(
                "codex",
                [
                    "plugin.toml",
                    "tests/cli.toml",
                    "tests/fake-cli.sh",
                    "tests/auth.jsonl",
                    "tests/processes.jsonl",
                    "tests/hooks/lifecycle.jsonl",
                    "tests/hooks/replies.jsonl",
                    "tests/screens/approval.blocked.txt",
                    "tests/screens/composer.idle.txt",
                    "tests/screens/hooks-review.blocked.txt",
                    "tests/screens/turn.working.txt",
                ]
            ),
            pkg!(
                "cursor",
                [
                    "plugin.toml",
                    "tests/cli.toml",
                    "tests/fake-cli.sh",
                    "tests/auth.jsonl",
                    "tests/processes.jsonl",
                    "tests/prompts.jsonl",
                    "tests/hooks/lifecycle.jsonl",
                    "tests/screens/run-command.blocked.txt",
                    "tests/screens/write-file.blocked.txt",
                    "tests/screens/generating.working.txt",
                ]
            ),
        ]
    }

    pub fn package(id: &str) -> Option<Files> {
        packages().into_iter().find(|(i, _)| *i == id).map(|(_, f)| f)
    }
}

// ── the trait and its manifest interpreter ───────────────────────────────

pub trait AgentPlugin: Send + Sync {
    fn manifest(&self) -> &Manifest;

    fn id(&self) -> &str {
        &self.manifest().id
    }

    /// The agent label used as the agent `kind` (plugins.md §4 row 19).
    fn agent_label(&self) -> &str {
        self.manifest().agent_label()
    }

    /// Translate one native hook payload (§5.3, §5.4 `command` transport).
    fn map_hook(&self, native: &Value) -> HookOutcome {
        let m = self.manifest();
        let mut out = HookOutcome::default();
        let Some(h) = &m.hooks else { return out };
        let field = |name: &str| -> Option<String> {
            (!name.is_empty())
                .then(|| native.get(name).and_then(Value::as_str).map(str::to_owned))
                .flatten()
        };
        out.session_id = field(&h.session_field);
        out.cwd = field(&h.cwd_field);
        let name = field(&h.event_field).unwrap_or_default();
        for row in h.map.iter().filter(|r| r.native == name) {
            let when_ok = row.when.iter().all(|(f, allowed)| {
                native
                    .get(f)
                    .and_then(Value::as_str)
                    .is_some_and(|v| allowed.iter().any(|a| a == v))
            });
            if !when_ok {
                continue;
            }
            out.release |= row.release;
            let ev = match row.emit.as_str() {
                "session.started" => AgentEvent::SessionStarted {
                    session_id: out.session_id.clone(),
                    cwd: out.cwd.clone(),
                    source: field(&h.source_field),
                },
                "turn.started" => AgentEvent::TurnStarted,
                "turn.completed" => AgentEvent::TurnCompleted,
                "turn.failed" => AgentEvent::TurnFailed {
                    error: field(&row.error_field).unwrap_or_default(),
                },
                "attention.blocked" => AgentEvent::AttentionBlocked {
                    reason: if row.reason.is_empty() {
                        row.when.keys().next().and_then(|k| field(k)).unwrap_or_default()
                    } else {
                        row.reason.clone()
                    },
                },
                "attention.idle" => AgentEvent::AttentionIdle,
                "prompt.permission" => {
                    let f = m.permissions.as_ref().map(|p| p.prompt.clone()).unwrap_or_default();
                    AgentEvent::PromptPermission {
                        tool: field(&f.tool).unwrap_or_default(),
                        input: if f.input.is_empty() {
                            Value::Null
                        } else {
                            native.get(&f.input).cloned().unwrap_or(Value::Null)
                        },
                        suggestions: if f.suggestions.is_empty() {
                            None
                        } else {
                            native.get(&f.suggestions).cloned().filter(|v| !v.is_null())
                        },
                        decidable: row.hold,
                    }
                }
                "prompt.question" => AgentEvent::PromptQuestion {
                    question: field("message").unwrap_or_default(),
                    options: native.get("options").cloned(),
                },
                "prompt.resolved" => AgentEvent::PromptResolved,
                "session.ended" => AgentEvent::SessionEnded,
                "open_url" => AgentEvent::OpenUrl {
                    url: field("url").unwrap_or_default(),
                },
                "usage" => AgentEvent::Usage(native.clone()),
                "log" => AgentEvent::Log {
                    level: "info".into(),
                    message: field("message").unwrap_or_default(),
                },
                "subagent.started" => AgentEvent::SubagentStarted(crate::subagents::Started {
                    id: field(&h.subagent.id_field),
                    name: field(&h.subagent.name_field).unwrap_or_default(),
                    parent_id: field(&h.subagent.parent_field),
                    model: field(&h.subagent.model_field),
                    ..Default::default()
                }),
                "subagent.completed" => AgentEvent::SubagentCompleted {
                    id: field(&h.subagent.id_field),
                    name: field(&h.subagent.name_field),
                },
                _ => continue,
            };
            out.hold |= row.hold;
            out.events.push(ev);
        }
        out
    }

    /// What the agent reads on the hook's stdout for a decision result
    /// (`{decision: {behavior, message?}}`): the reply template, or nothing.
    fn reply(&self, result: &Value) -> String {
        let Some(p) = &self.manifest().permissions else {
            return String::new();
        };
        let d = &result["decision"];
        let template = match d["behavior"].as_str() {
            Some("allow") => &p.reply.allow,
            Some("deny") => &p.reply.deny,
            _ => return String::new(),
        };
        let Ok(mut v) = serde_json::from_str::<Value>(template) else {
            return String::new();
        };
        fill_message(&mut v, d["message"].as_str());
        v.to_string()
    }

    /// Claude-style hook settings: `{hooks: {<event>: [{matcher?, hooks:
    /// [{type: command, command, timeout?}]}]}}` (the `hook-settings` builder).
    fn hook_settings(&self, hook_exe: &Path) -> Option<Value> {
        let m = self.manifest();
        let h = m.hooks.as_ref()?;
        if !m.generate.values().any(|g| g.builder == "hook-settings") {
            return None;
        }
        let mut command = format!("{} hook", sh_quote(&hook_exe.to_string_lossy()));
        if m.id != DEFAULT_PLUGIN {
            command.push_str(&format!(" --plugin {}", m.id));
        }
        let mut hooks = serde_json::Map::new();
        for r in &h.register {
            let mut hk = json!({ "type": "command", "command": command });
            if let Some(t) = r.timeout {
                hk["timeout"] = json!(t);
            }
            let mut entry = json!({ "hooks": [hk] });
            if let Some(mt) = &r.matcher {
                entry["matcher"] = json!(mt);
            }
            hooks.insert(r.event.clone(), json!([entry]));
        }
        Some(json!({ "hooks": hooks }))
    }

    /// The `statusLine` entry for the generated settings (crate::usage, D-U2):
    /// the agent's numbers, then the person's own line. Only for a plugin with
    /// a `statusline` usage source, and written only while the person has AI
    /// usage on ([`ShimEnv::usage`]): with it off the settings carry no such key.
    fn status_line(&self, hook_exe: &Path) -> Option<Value> {
        let m = self.manifest();
        m.usage.as_ref()?.source("statusline")?;
        let mut line = format!("{} hook --statusline", sh_quote(&hook_exe.to_string_lossy()));
        if m.id != DEFAULT_PLUGIN {
            line.push_str(&format!(" --plugin {}", m.id));
        }
        Some(json!({ "type": "command", "command": line }))
    }

    /// The whole generated settings document: [`AgentPlugin::hook_settings`],
    /// plus the status line when `usage` is on.
    fn settings_doc(&self, hook_exe: &Path, usage: bool) -> Option<Value> {
        let mut out = self.hook_settings(hook_exe)?;
        if usage {
            if let Some(line) = self.status_line(hook_exe) {
                out["statusLine"] = line;
            }
        }
        Some(out)
    }

    /// The hook command line the agent runs (`'<exe>' hook [--plugin <id>]`).
    fn hook_command(&self, hook_exe: &Path) -> String {
        let mut command = format!("{} hook", sh_quote(&hook_exe.to_string_lossy()));
        if self.manifest().id != DEFAULT_PLUGIN {
            command.push_str(&format!(" --plugin {}", self.manifest().id));
        }
        command
    }

    /// Whether `hooks.managed_file` already registers this plugin's hook
    /// (a hosted image's `/etc/codex/config.toml`): then no per-launch flags.
    fn hooks_managed(&self) -> bool {
        let Some(h) = &self.manifest().hooks else { return false };
        !h.managed_file.is_empty()
            && std::fs::read_to_string(&h.managed_file)
                .is_ok_and(|t| t.contains(&format!("hook --plugin {}", self.manifest().id)))
    }

    /// `{hooks.argv...}`: one `<config_flag> 'hooks.<Event>=[{…}]'` pair per
    /// registered event, in manifest order. The definition is byte-identical
    /// for the same hook path (Codex trusts a hook by its hash: any change
    /// asks the user again, decision 0003 spike 1), so no `timeout` unless
    /// the manifest sets one. Empty when the hooks are managed.
    fn hooks_argv(&self, hook_exe: &Path) -> Vec<String> {
        let Some(h) = &self.manifest().hooks else { return vec![] };
        if h.config_flag.is_empty() || self.hooks_managed() {
            return vec![];
        }
        let command = self.hook_command(hook_exe);
        let mut out = vec![];
        for r in &h.register {
            let mut handler = format!("type=\"command\",command={}", toml_str(&command));
            if let Some(t) = r.timeout {
                handler.push_str(&format!(",timeout={t}"));
            }
            let group = match &r.matcher {
                Some(m) => format!("{{matcher={},hooks=[{{{handler}}}]}}", toml_str(m)),
                None => format!("{{hooks=[{{{handler}}}]}}"),
            };
            out.push(h.config_flag.clone());
            out.push(format!("hooks.{}=[{group}]", r.event));
        }
        out
    }

    /// The `cursor-plugin` builder: a Cursor `--plugin-dir` directory with
    /// `.cursor-plugin/plugin.json`, `hooks/hooks.json` (Cursor's schema:
    /// `{version: 1, hooks: {<event>: [{command, timeout?}]}}`) and, while
    /// the generated skill is on, its rule under `rules/`.
    fn cursor_plugin_files(&self, hook_exe: &Path, skill: Option<&Path>) -> Vec<(PathBuf, String)> {
        let m = self.manifest();
        let Some(h) = &m.hooks else { return vec![] };
        let command = self.hook_command(hook_exe);
        let mut hooks = serde_json::Map::new();
        for r in &h.register {
            let mut hk = json!({ "command": command });
            if let Some(t) = r.timeout {
                hk["timeout"] = json!(t);
            }
            hooks.insert(r.event.clone(), json!([hk]));
        }
        let rule = skill
            .map(|d| d.join("tmuxedo.mdc"))
            .and_then(|p| std::fs::read_to_string(p).ok());
        let mut manifest = json!({
            "name": "tmuxedo",
            "version": m.version,
            "description": "Generated by tmuxedo-runner: status hooks (and the tmuxedo rule) for this session.",
            "hooks": "hooks/hooks.json",
        });
        let mut files = vec![];
        if let Some(r) = rule {
            manifest["rules"] = json!("rules/");
            files.push((PathBuf::from("rules/tmuxedo.mdc"), r));
        }
        files.push((
            PathBuf::from(".cursor-plugin/plugin.json"),
            serde_json::to_string_pretty(&manifest).unwrap_or_default() + "\n",
        ));
        files.push((
            PathBuf::from("hooks/hooks.json"),
            serde_json::to_string_pretty(&json!({ "version": 1, "hooks": hooks })).unwrap_or_default() + "\n",
        ));
        files
    }

    /// Generated files: name → file name under the plugin's state dir.
    fn generated_files(&self) -> Vec<(String, String)> {
        self.manifest()
            .generate
            .iter()
            .map(|(n, g)| {
                (
                    n.clone(),
                    if g.file.is_empty() {
                        format!("{n}.json")
                    } else {
                        g.file.clone()
                    },
                )
            })
            .collect()
    }

    /// The PATH shim: finds the real binary further down PATH and execs it
    /// with `launch.prepend` (resolved; launch args and paths single-quoted,
    /// plain flags bare) before the user's own arguments.
    /// `skill`: the generated skill's directory while the skill is on for
    /// this plugin (`{skill.argv...}` splats `skill.argv` then; nothing when off).
    fn shim_script(&self, gen: &BTreeMap<String, PathBuf>, args: &[String], skill: Option<&Path>) -> Option<String> {
        self.shim_script_for(
            &ShimEnv {
                gen,
                args,
                skill,
                hook_exe: None,
                hosted: false,
                set: None,
                usage: false,
            },
            None,
        )
    }

    /// The shim for `name` (the launch shim, or one of `launch.aliases`).
    /// The Claude shim is byte-identical to the one before the plugin host
    /// (tests/claude_baseline.rs): every addition below is only rendered
    /// when its manifest field is set.
    fn shim_script_for(&self, e: &ShimEnv, name: Option<&str>) -> Option<String> {
        let (gen, args, skill) = (e.gen, e.args, e.skill);
        let m = self.manifest();
        let l = m.launch.as_ref()?;
        let mut pre = String::new();
        let hosted_extra: &[String] = if e.hosted { &l.hosted_prepend } else { &[] };
        for tok in l.prepend.iter().chain(hosted_extra) {
            if tok == "{hooks.argv...}" {
                if let Some(exe) = e.hook_exe {
                    for a in self.hooks_argv(exe) {
                        pre.push_str(&shell_token(&a, &a));
                        pre.push(' ');
                    }
                }
                continue;
            }
            if tok == "{settings.launch.args...}" {
                for a in args {
                    pre.push_str(&sh_quote(a));
                    pre.push(' ');
                }
                continue;
            }
            if tok == "{skill.argv...}" {
                if let (Some(dir), Some(sk)) = (skill, &m.skill) {
                    let mut g = gen.clone();
                    g.insert("skill".into(), dir.to_path_buf());
                    for t in &sk.argv {
                        if let Some(r) = resolve_skill_toml(t, dir) {
                            pre.push_str(&sh_quote(&r));
                            pre.push(' ');
                            continue;
                        }
                        let r = resolve(
                            t,
                            &Ctx {
                                gen: Some(&g),
                                ..Ctx::default()
                            },
                        );
                        pre.push_str(&shell_token(t, &r));
                        pre.push(' ');
                    }
                }
                continue;
            }
            let resolved = resolve(
                tok,
                &Ctx {
                    gen: Some(gen),
                    ..Ctx::default()
                },
            );
            pre.push_str(&shell_token(tok, &resolved));
            pre.push(' ');
        }
        // Files first: their flags end before the next option (`--add-dir` takes several values).
        if let Some(f) = &l.files {
            pre.insert_str(0, &f.shell_fragment());
        }
        let bin = name.unwrap_or(&l.shim);
        // Extras, each rendered only when its field is set (Claude has none).
        let mut env = String::new();
        for (k, v) in &l.env {
            env.push_str(&format!("{k}={}; export {k}\n", sh_quote(v)));
        }
        let mut guard = String::new();
        if !l.passthrough.is_empty() {
            guard.push_str(&format!(
                "    case \"${{1-}}\" in {}) exec \"$d/{bin}\" \"$@\" ;; esac\n",
                l.passthrough.join("|")
            ));
        }
        if !l.target_contains.is_empty() {
            guard.push_str(&format!(
                "    case \"$(readlink -f \"$d/{bin}\" 2>/dev/null || printf '%s' \"$d/{bin}\")\" in *{}*) ;; *) exec \"$d/{bin}\" \"$@\" ;; esac\n",
                l.target_contains
            ));
        }
        let set = e.set.unwrap_or_default();
        Some(format!(
            r#"#!/bin/sh
# tmuxedo: run the real {bin} with tmuxedo's hook settings layered on top of the
# user's own (which are left untouched). Generated by tmuxedo-runner.
shim_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
{env}old_ifs=$IFS
IFS=:
for d in $PATH; do
  [ -n "$d" ] || continue
  abs=$(CDPATH= cd -- "$d" 2>/dev/null && pwd) || continue
  [ "$abs" = "$shim_dir" ] && continue
  if [ -x "$d/{bin}" ] && [ ! -d "$d/{bin}" ]; then
    IFS=$old_ifs
{guard}{set}    exec "$d/{bin}" {pre}"$@"
  fi
done
IFS=$old_ifs
echo "tmuxedo: {bin} is not installed (not found on PATH)" >&2
exit 127
"#
        ))
    }

    /// The command that resumes a session, as a shell-quoted string
    /// (`resume.argv` with `{shim}` = the shim in `shim_dir`).
    fn resume_command(&self, shim_dir: &Path, session_id: &str) -> Option<String> {
        let m = self.manifest();
        let r = m.resume.as_ref()?;
        let shim = m.launch.as_ref().map(|l| shim_dir.join(&l.shim));
        let ctx = Ctx {
            shim: shim.as_deref(),
            session_id: Some(session_id),
            ..Ctx::default()
        };
        Some(
            r.argv
                .iter()
                .map(|t| shell_token(t, &resolve(t, &ctx)))
                .collect::<Vec<_>>()
                .join(" "),
        )
    }

    /// `auth.login.argv`, resolved.
    fn login_argv(&self, shim_dir: &Path) -> Option<Vec<String>> {
        let m = self.manifest();
        let l = m.auth.as_ref()?.login.as_ref()?;
        let shim = m.launch.as_ref().map(|s| shim_dir.join(&s.shim));
        let ctx = Ctx {
            shim: shim.as_deref(),
            ..Ctx::default()
        };
        Some(l.argv.iter().map(|t| resolve(t, &ctx)).collect())
    }

    /// `turn.failed` errors that mean the login is unusable.
    fn is_auth_error(&self, error: &str) -> bool {
        self.manifest()
            .auth
            .as_ref()
            .is_some_and(|a| a.failure_errors.iter().any(|e| e == error))
    }

    /// The id of the first `auth.screen` rule matching `text` (case-insensitive).
    fn auth_screen_rule(&self, text: &str) -> Option<&str> {
        self.auth_screen_match(text).map(|r| r.id.as_str())
    }

    /// The first `auth.screen` rule whose text is in `text` (case-insensitive;
    /// white space runs, line breaks included, count as one space: a long
    /// message may wrap on a narrow terminal).
    fn auth_screen_match(&self, text: &str) -> Option<&manifest::ScreenRule> {
        let a = self.manifest().auth.as_ref()?;
        let lower = text.to_lowercase();
        let flat = lower.split_whitespace().collect::<Vec<_>>().join(" ");
        a.screen
            .iter()
            .find(|r| lower.contains(&r.contains) || flat.contains(&r.contains))
    }

    /// The first `auth.screen` rule matching `text`: (id, state), state
    /// `expired` or `failed`.
    fn auth_screen_hit(&self, text: &str) -> Option<(&str, &str)> {
        let a = self.manifest().auth.as_ref()?;
        let lower = text.to_lowercase();
        a.screen.iter().find(|r| lower.contains(&r.contains)).map(|r| {
            (
                r.id.as_str(),
                if r.state.is_empty() {
                    "expired"
                } else {
                    r.state.as_str()
                },
            )
        })
    }

    /// A login link (and one-time code) on screen (`auth.lift`).
    fn lift(&self, text: &str) -> Option<Lifted> {
        let a = self.manifest().auth.as_ref()?;
        if !text.contains("http") {
            return None;
        }
        for rule in &a.lift {
            let Some(re) = cached_regex(&rule.url) else { continue };
            let Some(m) = re.find(text) else { continue };
            let url = m.as_str().to_string();
            if !crate::open_url::valid_url(&url) {
                continue;
            }
            let code = (!rule.code.is_empty())
                .then(|| cached_regex(&rule.code))
                .flatten()
                .and_then(|re| re.captures(&text[m.end()..]))
                .and_then(|c| c.get(1).or_else(|| c.get(0)))
                .map(|c| c.as_str().to_string());
            return Some(Lifted {
                rule: rule.id.clone(),
                url,
                code,
            });
        }
        None
    }

    /// `permissions.mode = "keystroke"`: the dialog on screen that can be
    /// answered, as (tool, fingerprint). The fingerprint is the needle's
    /// line and the non-empty lines after it (up to `fingerprint_lines` in
    /// all), each trimmed with inner runs of spaces collapsed, joined by ` | `.
    fn prompt_fingerprint(&self, text: &str) -> Option<(String, String)> {
        let p = self.manifest().permissions.as_ref().filter(|p| p.mode == "keystroke")?;
        let lines: Vec<&str> = text.lines().collect();
        // The live dialog is the newest one: the last line any rule matches
        // (an answered dialog may still be in the scrollback above it).
        let (i, rule) = p
            .screen
            .iter()
            .filter_map(|r| {
                lines
                    .iter()
                    .rposition(|l| l.to_lowercase().contains(&r.contains))
                    .map(|i| (i, r))
            })
            .max_by_key(|(i, _)| *i)?;
        let n = p.fingerprint_lines.max(1) as usize;
        let fp: Vec<String> = lines[i..]
            .iter()
            .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
            .filter(|l| !l.is_empty())
            .take(n)
            .collect();
        Some((rule.tool.clone(), fp.join(" | ")))
    }

    /// `env.scrub`: names and `PREFIX_*` globs.
    fn scrubs(&self, key: &str) -> bool {
        self.manifest().env.scrub.iter().any(|s| match s.strip_suffix('*') {
            Some(prefix) => key.starts_with(prefix),
            None => key == s,
        })
    }

    /// `hello.runner.*Version`: the first word of `version_cmd`'s stdout.
    fn version_cmd(&self) -> Option<&[String]> {
        let v = &self.manifest().agent.version_cmd;
        (!v.is_empty()).then_some(v.as_slice())
    }
}

/// What a shim is rendered with.
pub struct ShimEnv<'a> {
    pub gen: &'a BTreeMap<String, PathBuf>,
    /// The user's launch args (`{settings.launch.args...}`).
    pub args: &'a [String],
    /// The generated skill's directory while the skill is on.
    pub skill: Option<&'a Path>,
    /// The hook binary (`{hooks.argv...}` needs it; None renders none).
    pub hook_exe: Option<&'a Path>,
    /// A hosted machine (`launch.hosted_prepend`).
    pub hosted: bool,
    /// The line that puts the plugin set's arguments before the user's own
    /// (crate::plugin_gen); None while the set has nothing for the plugin.
    pub set: Option<&'a str>,
    /// AI usage is on for this plugin: the generated settings name the
    /// runner's status-line command. Off = no `statusLine` key at all.
    pub usage: bool,
}

/// A TOML basic string.
pub fn toml_str(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\t' => o.push_str("\\t"),
            c if c.is_control() => o.push_str(&format!("\\u{:04X}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// `key={skill.toml:FILE}` in `skill.argv`: the skill file's text as a TOML
/// string (`-c developer_instructions="…"`). None when `tok` is not one.
fn resolve_skill_toml(tok: &str, dir: &Path) -> Option<String> {
    let start = tok.find("{skill.toml:")?;
    let end = tok[start..].find('}')? + start;
    let file = &tok[start + "{skill.toml:".len()..end];
    if file.contains('/') || file.starts_with('.') {
        return None;
    }
    let text = std::fs::read_to_string(dir.join(file)).unwrap_or_default();
    Some(format!("{}{}{}", &tok[..start], toml_str(&text), &tok[end + 1..]))
}

/// A login link lifted from the screen.
#[derive(Debug, Clone, PartialEq)]
pub struct Lifted {
    pub rule: String,
    pub url: String,
    pub code: Option<String>,
}

/// Manifest regexes, compiled once.
fn cached_regex(src: &str) -> Option<regex::Regex> {
    static CACHE: LazyLock<std::sync::Mutex<BTreeMap<String, Option<regex::Regex>>>> = LazyLock::new(Default::default);
    let mut c = CACHE.lock().ok()?;
    c.entry(src.to_string())
        .or_insert_with(|| regex::Regex::new(src).ok())
        .clone()
}

/// Placeholder resolution context.
#[derive(Default)]
pub struct Ctx<'a> {
    pub gen: Option<&'a BTreeMap<String, PathBuf>>,
    pub shim: Option<&'a Path>,
    pub session_id: Option<&'a str>,
    pub home: Option<&'a str>,
}

/// Resolve the placeholders in one manifest token. Unknown ones are kept
/// verbatim (the linter has refused them already).
pub fn resolve(tok: &str, ctx: &Ctx) -> String {
    let mut out = tok.to_string();
    if let Some(g) = ctx.gen {
        for (name, path) in g {
            out = out.replace(&format!("{{gen:{name}}}"), &path.to_string_lossy());
        }
    }
    if let Some(s) = ctx.shim {
        out = out.replace("{shim}", &s.to_string_lossy());
    }
    if let Some(s) = ctx.session_id {
        out = out.replace("{session_id}", s);
    }
    if let Some(h) = ctx.home {
        out = out.replace("{home}", h);
    }
    out
}

/// A manifest token in a generated command line: a plain literal flag from
/// the manifest (`--settings`, `--resume`) stays bare; anything resolved
/// from a placeholder (paths, ids) is single-quoted. Never evaluated.
pub fn shell_token(tok: &str, resolved: &str) -> String {
    let literal = resolved == tok && !tok.is_empty();
    if literal && tok.chars().all(|c| c.is_ascii_alphanumeric() || "-_.=/".contains(c)) {
        tok.to_string()
    } else {
        sh_quote(resolved)
    }
}

/// Replace string leaves equal to `{message}` with the message, or drop
/// their key when there is none.
fn fill_message(v: &mut Value, message: Option<&str>) {
    match v {
        Value::Object(m) => {
            let drop: Vec<String> = m
                .iter()
                .filter(|(_, v)| v.as_str() == Some("{message}") && message.is_none())
                .map(|(k, _)| k.clone())
                .collect();
            for k in drop {
                m.remove(&k);
            }
            for x in m.values_mut() {
                fill_message(x, message);
            }
        }
        Value::Array(a) => a.iter_mut().for_each(|x| fill_message(x, message)),
        Value::String(s) if s == "{message}" => {
            if let Some(msg) = message {
                *v = Value::String(msg.to_string());
            }
        }
        _ => {}
    }
}

/// A plugin that is nothing but its manifest.
pub struct ManifestPlugin {
    manifest: Manifest,
}

impl ManifestPlugin {
    pub fn new(manifest: Manifest) -> Self {
        Self { manifest }
    }
}

impl AgentPlugin for ManifestPlugin {
    fn manifest(&self) -> &Manifest {
        &self.manifest
    }
}

/// The built-in Claude plugin (parsed once; `builtin_manifests_lint_clean`
/// keeps the embedded manifest valid).
pub fn claude() -> &'static ManifestPlugin {
    static P: LazyLock<ManifestPlugin> = LazyLock::new(|| {
        let text = String::from_utf8_lossy(&builtin::package("claude").expect("claude is built in")["plugin.toml"])
            .into_owned();
        ManifestPlugin::new(Manifest::parse(&text).expect("the built-in claude manifest parses"))
    });
    &P
}

// ── the registry ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LogEntry {
    pub log_id: String,
    pub plugin_id: String,
    pub command: Vec<String>,
    /// `running` | `succeeded` | `failed`
    pub status: String,
    pub started_unix_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_unix_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub struct Loaded {
    pub plugin: Arc<dyn AgentPlugin>,
    pub enabled: bool,
    /// `builtin` | `curated` | `local`
    pub tier: &'static str,
    pub root: String,
    pub warnings: Vec<String>,
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct Persisted {
    #[serde(default)]
    disabled: Vec<String>,
}

/// A plugin's generated-skill directory under the runner's state dir.
pub fn skill_path(state_dir: &Path, id: &str) -> PathBuf {
    state_dir.join("plugins").join(id).join("skill")
}

pub struct Host {
    /// A hosted machine (`launch.hosted_prepend`); set by the daemon.
    pub hosted: bool,
    /// The hook binary generated files and shims name; set at install.
    pub hook_exe: Option<PathBuf>,
    plugins: Vec<Loaded>,
    /// Plugins whose generated skill is on (none until web's config says so).
    skill_on: std::collections::BTreeSet<String>,
    /// Plugin id → its shim's plugin-set line (crate::plugin_gen).
    set_lines: BTreeMap<String, String>,
    /// Plugins whose AI usage is on (none until web's config says so).
    usage_on: std::collections::BTreeSet<String>,
    state_path: Option<PathBuf>,
    log: VecDeque<LogEntry>,
    next_log: u64,
}

impl Host {
    /// Every built-in plugin, enabled.
    pub fn builtin() -> Host {
        let mut plugins = vec![];
        for (id, files) in builtin::packages() {
            let text = String::from_utf8_lossy(&files["plugin.toml"]).into_owned();
            let manifest = Manifest::parse(&text).unwrap_or_else(|e| panic!("built-in {id}: {e}"));
            let warnings = manifest.lint();
            plugins.push(Loaded {
                plugin: Arc::new(ManifestPlugin::new(manifest)),
                enabled: true,
                tier: "builtin",
                root: format!("builtin:{id}"),
                warnings,
            });
        }
        Host {
            hosted: false,
            hook_exe: None,
            plugins,
            skill_on: Default::default(),
            set_lines: Default::default(),
            usage_on: Default::default(),
            state_path: None,
            log: VecDeque::new(),
            next_log: 1,
        }
    }

    /// Only the named built-ins (the legacy `inject::*` helpers are Claude's
    /// alone, exactly as before the other plugins existed).
    pub fn builtin_only(ids: &[&str]) -> Host {
        let mut h = Host::builtin();
        h.plugins.retain(|l| ids.contains(&l.plugin.id()));
        h
    }

    /// Built-ins plus the persisted enable state in `<state_dir>/plugins.json`.
    pub fn load(state_dir: &Path) -> Host {
        let mut h = Host::builtin();
        let path = state_dir.join("plugins.json");
        if let Some(p) = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Persisted>(&b).ok())
        {
            for l in &mut h.plugins {
                l.enabled = !p.disabled.iter().any(|d| *d == l.plugin.id());
            }
        }
        h.state_path = Some(path);
        h
    }

    /// Local / dev plugins (plugins.md §9.1): manifest-only packages in
    /// `dir/<id>/plugin.toml`, for plugin authors on their own machine and for
    /// the e2e stack. Unsigned, so marked "local" and unverified everywhere;
    /// never on a hosted machine (the caller checks); a package whose lint
    /// fails, or whose id is taken, is skipped with the reason.
    pub fn load_dev(&mut self, dir: &Path) -> Vec<String> {
        let mut notes = vec![];
        let Ok(rd) = std::fs::read_dir(dir) else {
            return vec![format!("dev plugins: cannot read {}", dir.display())];
        };
        let mut entries: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
        entries.sort();
        for root in entries {
            let file = root.join("plugin.toml");
            let Ok(text) = std::fs::read_to_string(&file) else {
                continue;
            };
            let m = match Manifest::parse(&text) {
                Ok(m) => m,
                Err(e) => {
                    notes.push(format!("dev plugin {}: {e}", root.display()));
                    continue;
                }
            };
            let problems = m.lint();
            if !problems.is_empty() {
                notes.push(format!("dev plugin {}: {}", m.id, problems.join("; ")));
                continue;
            }
            if self.loaded(&m.id).is_some() {
                notes.push(format!("dev plugin {}: id already taken", m.id));
                continue;
            }
            notes.push(format!(
                "dev plugin {} {} loaded from {} (unverified)",
                m.id,
                m.version,
                root.display()
            ));
            self.plugins.push(Loaded {
                plugin: Arc::new(ManifestPlugin::new(m)),
                enabled: true,
                tier: "local",
                root: root.display().to_string(),
                warnings: vec!["unverified: a local plugin, not signed".into()],
            });
        }
        notes
    }

    fn persist(&self) {
        let Some(p) = &self.state_path else { return };
        let v = Persisted {
            disabled: self
                .plugins
                .iter()
                .filter(|l| !l.enabled)
                .map(|l| l.plugin.id().to_string())
                .collect(),
        };
        if let Ok(b) = serde_json::to_vec_pretty(&v) {
            let _ = crate::config::write_private(p, &b);
        }
    }

    pub fn ids(&self) -> Vec<&str> {
        self.plugins.iter().map(|l| l.plugin.id()).collect()
    }

    pub fn loaded(&self, id: &str) -> Option<&Loaded> {
        self.plugins.iter().find(|l| l.plugin.id() == id)
    }

    pub fn get(&self, id: &str) -> Option<Arc<dyn AgentPlugin>> {
        self.loaded(id).map(|l| l.plugin.clone())
    }

    pub fn enabled(&self, id: &str) -> bool {
        self.loaded(id).is_some_and(|l| l.enabled)
    }

    /// Enabled plugins, in registry order.
    pub fn active(&self) -> impl Iterator<Item = &Arc<dyn AgentPlugin>> {
        self.plugins.iter().filter(|l| l.enabled).map(|l| &l.plugin)
    }

    /// The enabled plugin owning agent label `kind`.
    pub fn by_label(&self, kind: &str) -> Option<Arc<dyn AgentPlugin>> {
        self.active()
            .find(|p| p.manifest().agent.labels.iter().any(|l| l == kind))
            .cloned()
    }

    /// The plugin a `hook --plugin <id>` (None = [`DEFAULT_PLUGIN`]) belongs
    /// to, if it is enabled (capability `hooks.receive`, §9.3).
    pub fn for_hook(&self, id: Option<&str>) -> Option<Arc<dyn AgentPlugin>> {
        let id = id.unwrap_or(DEFAULT_PLUGIN);
        self.loaded(id)
            .filter(|l| l.enabled && l.plugin.manifest().has("hooks.receive"))
            .map(|l| l.plugin.clone())
    }

    pub fn set_enabled(&mut self, id: &str, on: bool) -> Result<(), String> {
        let l = self
            .plugins
            .iter_mut()
            .find(|l| l.plugin.id() == id)
            .ok_or_else(|| format!("no plugin {id}"))?;
        l.enabled = on;
        self.persist();
        self.push_log(LogEntry {
            log_id: String::new(),
            plugin_id: id.to_string(),
            command: vec![],
            status: "succeeded".into(),
            started_unix_ms: crate::util::now_ms(),
            finished_unix_ms: Some(crate::util::now_ms()),
            action_id: None,
            event: Some(if on {
                "plugin.enabled".into()
            } else {
                "plugin.disabled".into()
            }),
            error: None,
        });
        Ok(())
    }

    /// The first enabled plugin whose `auth.screen` matches: (plugin id, rule id).
    pub fn auth_screen_rule(&self, text: &str) -> Option<(String, String)> {
        self.active()
            .find_map(|p| p.auth_screen_rule(text).map(|r| (p.id().to_string(), r.to_string())))
    }

    pub fn push_log(&mut self, mut e: LogEntry) -> LogEntry {
        if e.log_id.is_empty() {
            e.log_id = format!("l{}", self.next_log);
            self.next_log += 1;
        }
        if self.log.len() >= LOG_CAP {
            self.log.pop_front();
        }
        self.log.push_back(e.clone());
        e
    }

    pub fn logs(&self, plugin: Option<&str>, limit: Option<usize>) -> Vec<&LogEntry> {
        let it = self
            .log
            .iter()
            .rev()
            .filter(|e| plugin.is_none_or(|p| e.plugin_id == p));
        match limit {
            Some(n) => it.take(n).collect(),
            None => it.collect(),
        }
    }

    // ── files on disk ────────────────────────────────────────────────────

    /// Write every enabled plugin's generated files and shim, plus the URL
    /// opener stand-ins, under `dir`; returns the shim dir.
    pub fn install(&self, dir: &Path, hook_exe: &Path, args: &[String]) -> std::io::Result<PathBuf> {
        let shim = dir.join("shim");
        std::fs::create_dir_all(&shim)?;
        for p in self.active() {
            let skill = self.skill_dir(p.id(), dir);
            install_plugin_env(
                p.as_ref(),
                dir,
                &ShimEnv {
                    gen: &gen_paths(p.as_ref(), dir),
                    args: self.args_for(p.id(), args),
                    skill: skill.as_deref(),
                    hook_exe: Some(hook_exe),
                    hosted: self.hosted,
                    set: self.set_lines.get(p.id()).map(String::as_str),
                    usage: self.usage_enabled(p.id()),
                },
            )?;
        }
        for name in OPENERS {
            write_exec(&shim.join(name), &opener_script(hook_exe, name))?;
        }
        Ok(shim)
    }

    /// Rewrite only the shims (new launch args); running agents keep theirs.
    pub fn write_shims(&self, dir: &Path, args: &[String]) -> std::io::Result<()> {
        for p in self.active() {
            let skill = self.skill_dir(p.id(), dir);
            let env = ShimEnv {
                gen: &gen_paths(p.as_ref(), dir),
                args: self.args_for(p.id(), args),
                skill: skill.as_deref(),
                hook_exe: self.hook_exe.as_deref(),
                hosted: self.hosted,
                set: self.set_lines.get(p.id()).map(String::as_str),
                usage: self.usage_enabled(p.id()),
            };
            // Directory builders carry the skill (Cursor's rule): refresh them.
            if self.hook_exe.is_some() {
                write_dir_builders(p.as_ref(), &env)?;
            }
            write_shim_env(p.as_ref(), dir, &env)?;
        }
        Ok(())
    }

    /// The user's launch args are Claude's (`config.claude.args`, Settings →
    /// Terminal); other plugins get none until Phase 17 settings.
    fn args_for<'a>(&self, id: &str, args: &'a [String]) -> &'a [String] {
        if id == DEFAULT_PLUGIN {
            args
        } else {
            &[]
        }
    }

    /// Turn the generated skill on or off for a plugin (web's `config.plugins`
    /// and the agent's version decide; crate::skill). Shims follow on the
    /// next [`Host::write_shims`].
    pub fn set_skill(&mut self, id: &str, on: bool) {
        if on {
            self.skill_on.insert(id.to_string());
        } else {
            self.skill_on.remove(id);
        }
    }

    /// The plugin-set lines of the shims (crate::plugin_gen::rebuild_all);
    /// true when they changed. Shims follow on the next [`Host::write_shims`].
    pub fn set_plugin_set(&mut self, lines: BTreeMap<String, String>) -> bool {
        let changed = self.set_lines != lines;
        self.set_lines = lines;
        changed
    }

    pub fn skill_enabled(&self, id: &str) -> bool {
        self.skill_on.contains(id)
    }

    /// Turn AI usage on or off for a plugin (web's `config.plugins`). The
    /// settings files follow on the next [`Host::write_settings`].
    pub fn set_usage(&mut self, id: &str, on: bool) {
        if on {
            self.usage_on.insert(id.to_string());
        } else {
            self.usage_on.remove(id);
        }
    }

    pub fn usage_enabled(&self, id: &str) -> bool {
        self.usage_on.contains(id)
    }

    /// Rewrite the generated settings files (AI usage went on or off). An
    /// agent already running keeps the settings it started with.
    pub fn write_settings(&self, dir: &Path) -> std::io::Result<()> {
        for p in self.active() {
            write_settings(
                p.as_ref(),
                &ShimEnv {
                    gen: &gen_paths(p.as_ref(), dir),
                    args: &[],
                    skill: None,
                    hook_exe: self.hook_exe.as_deref(),
                    hosted: self.hosted,
                    set: self.set_lines.get(p.id()).map(String::as_str),
                    usage: self.usage_enabled(p.id()),
                },
            )?;
        }
        Ok(())
    }

    /// Where a plugin's skill lives while it is on (`<state>/plugins/<id>/skill`).
    pub fn skill_dir(&self, id: &str, dir: &Path) -> Option<PathBuf> {
        let has = self.get(id).is_some_and(|p| p.manifest().skill.is_some());
        (has && self.skill_on.contains(id)).then(|| skill_path(dir, id))
    }

    /// Remove a (disabled) plugin's shim so its agent runs unwrapped.
    pub fn remove_shim(&self, id: &str, dir: &Path) -> std::io::Result<()> {
        if let Some(l) = self.loaded(id).and_then(|l| l.plugin.manifest().launch.clone()) {
            for name in std::iter::once(&l.shim).chain(&l.aliases) {
                match std::fs::remove_file(dir.join("shim").join(name)) {
                    Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
                    _ => {}
                }
            }
        }
        Ok(())
    }

    // ── runner API shapes (§13) ───────────────────────────────────────────────

    /// A `plugin.list` row.
    pub fn info(&self, l: &Loaded) -> Value {
        let m = l.plugin.manifest();
        let events: Vec<Value> = m
            .hooks
            .iter()
            .flat_map(|h| h.map.iter())
            .map(|r| json!({ "on": r.native, "command": ["emit", r.emit] }))
            .collect();
        json!({
            "plugin_id": m.id,
            "name": m.name,
            "version": m.version,
            "description": m.description,
            "manifest_path": format!("{}/plugin.toml", l.root),
            "plugin_root": l.root,
            "enabled": l.enabled,
            "source": { "kind": "local", "managed_path": l.root },
            "platforms": ["linux", "macos"],
            "actions": m.actions.iter().map(manifest_action).collect::<Vec<_>>(),
            "events": events,
            "panes": [],
            "link_handlers": [],
            "build": [],
            "startup": [],
            "warnings": l.warnings,
            // tmuxedo plugin details
            "tier": l.tier,
            "contract": m.contract,
            "capabilities": m.capabilities,
            "agent": m.agent.labels,
        })
    }

    pub fn infos(&self, id: Option<&str>) -> Vec<Value> {
        self.plugins
            .iter()
            .filter(|l| id.is_none_or(|i| l.plugin.id() == i))
            .map(|l| self.info(l))
            .collect()
    }

    /// `plugin.action.list` rows.
    pub fn action_infos(&self, id: Option<&str>) -> Vec<Value> {
        self.plugins
            .iter()
            .filter(|l| l.enabled && id.is_none_or(|i| l.plugin.id() == i))
            .flat_map(|l| {
                let pid = l.plugin.id().to_string();
                l.plugin.manifest().actions.iter().map(move |a| {
                    let mut v = manifest_action(a);
                    v["plugin_id"] = json!(pid);
                    v["action_id"] = v["id"].take();
                    v.as_object_mut().unwrap().remove("id");
                    v
                })
            })
            .collect()
    }
}

fn manifest_action(a: &manifest::Action) -> Value {
    let mut cmd = vec![a.kind.clone()];
    cmd.extend(a.argv.iter().cloned());
    if !a.text.is_empty() {
        cmd.push(a.text.clone());
    }
    if !a.url.is_empty() {
        cmd.push(a.url.clone());
    }
    json!({
        "id": a.id,
        "title": a.title,
        "description": if a.description.is_empty() { Value::Null } else { json!(a.description) },
        "command": cmd,
        "contexts": if a.contexts.is_empty() { json!(["global"]) } else { json!(a.contexts) },
        "platforms": ["linux", "macos"],
    })
}

/// A PATH stand-in for a URL opener: http(s) URLs go to the runner (which
/// tells the user's device to open them); anything else goes to the real
/// program further down PATH (`tmuxedo-runner open-url` handles both).
pub fn opener_script(exe: &Path, name: &str) -> String {
    format!(
        "#!/bin/sh\n# tmuxedo: open web links on your device; anything else goes to the real {name}.\n\
         exec {exe} open-url --program {name} -- \"$@\"\n",
        exe = sh_quote(&exe.to_string_lossy()),
        name = sh_quote(name)
    )
}

pub fn write_exec(path: &Path, body: &str) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, body)?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    std::fs::rename(&tmp, path)
}

/// The generated files' paths for `plugin` under `dir`.
pub fn gen_paths(plugin: &dyn AgentPlugin, dir: &Path) -> BTreeMap<String, PathBuf> {
    plugin
        .generated_files()
        .into_iter()
        .map(|(n, f)| (n, dir.join(f)))
        .collect()
}

/// Write one plugin's generated files and shim.
pub fn install_plugin(
    plugin: &dyn AgentPlugin,
    dir: &Path,
    hook_exe: &Path,
    args: &[String],
    skill: Option<&Path>,
) -> std::io::Result<()> {
    install_plugin_env(
        plugin,
        dir,
        &ShimEnv {
            gen: &gen_paths(plugin, dir),
            args,
            skill,
            hook_exe: Some(hook_exe),
            hosted: false,
            set: None,
            usage: false,
        },
    )
}

/// Write one plugin's generated files and shims.
pub fn install_plugin_env(plugin: &dyn AgentPlugin, dir: &Path, e: &ShimEnv) -> std::io::Result<()> {
    std::fs::create_dir_all(dir.join("shim"))?;
    if e.hook_exe.is_some() {
        write_settings(plugin, e)?;
        write_dir_builders(plugin, e)?;
    }
    write_shim_env(plugin, dir, e)
}

/// The generated settings files (`hook-settings`), with or without the status
/// line as [`ShimEnv::usage`] says.
pub fn write_settings(plugin: &dyn AgentPlugin, e: &ShimEnv) -> std::io::Result<()> {
    let Some(hook_exe) = e.hook_exe else { return Ok(()) };
    let Some(settings) = plugin.settings_doc(hook_exe, e.usage) else {
        return Ok(());
    };
    for (name, g) in &plugin.manifest().generate {
        if g.builder == "hook-settings" {
            // Atomically: a running agent (or a reader) never sees a half-written file.
            let path = &e.gen[name];
            let tmp = path.with_extension("json.tmp");
            std::fs::write(&tmp, serde_json::to_vec_pretty(&settings)?)?;
            std::fs::rename(&tmp, path)?;
        }
    }
    Ok(())
}

/// The directory builders (`cursor-plugin`), written whole and swapped in.
pub fn write_dir_builders(plugin: &dyn AgentPlugin, e: &ShimEnv) -> std::io::Result<()> {
    let Some(hook_exe) = e.hook_exe else { return Ok(()) };
    for (name, g) in &plugin.manifest().generate {
        if g.builder != "cursor-plugin" {
            continue;
        }
        let out = &e.gen[name];
        let tmp = out.with_extension("tmp");
        let _ = std::fs::remove_dir_all(&tmp);
        for (rel, body) in plugin.cursor_plugin_files(hook_exe, e.skill) {
            let p = tmp.join(rel);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(p, body)?;
        }
        let _ = std::fs::remove_dir_all(out);
        std::fs::rename(&tmp, out)?;
    }
    Ok(())
}

pub fn write_shim(plugin: &dyn AgentPlugin, dir: &Path, args: &[String], skill: Option<&Path>) -> std::io::Result<()> {
    write_shim_env(
        plugin,
        dir,
        &ShimEnv {
            gen: &gen_paths(plugin, dir),
            args,
            skill,
            hook_exe: None,
            hosted: false,
            set: None,
            usage: false,
        },
    )
}

/// Write the shim and every alias of it.
pub fn write_shim_env(plugin: &dyn AgentPlugin, dir: &Path, e: &ShimEnv) -> std::io::Result<()> {
    if let Some(l) = &plugin.manifest().launch {
        for name in std::iter::once(&l.shim).chain(&l.aliases) {
            if let Some(body) = plugin.shim_script_for(e, Some(name)) {
                write_exec(&dir.join("shim").join(name), &body)?;
            }
        }
    }
    Ok(())
}

/// Run `cmd` (a shell-quoted command line) and then leave an interactive
/// shell behind, started as a new pane's shell is when there are shell
/// settings (`launch_config::exec_wrapped_shell`), so the pane's PATH (shim
/// first) survives. `cmd` itself runs with that PATH restored too, in case
/// the login profile reset it (Debian's /etc/profile does). A shell that is
/// neither bash nor zsh is left as a login shell.
pub fn run_then_shell(shell: &str, cmd: &str) -> Vec<String> {
    vec![
        shell.to_string(),
        "-lc".into(),
        format!(
            "[ -n \"$TMUXEDO_PANE_PATH\" ] && export PATH=\"$TMUXEDO_PANE_PATH:$PATH\"; {cmd}; {}",
            crate::launch_config::exec_wrapped_shell(shell)
        ),
    ]
}

/// `<version_cmd>` → first word of stdout, with the plugin's scrubbed env
/// (a parent agent's markers would make the child think it is nested).
pub async fn agent_version(plugin: &dyn AgentPlugin) -> Option<String> {
    probe_agent(plugin, None, VERSION_PROBE_LIMIT).await.version()
}

/// How long one `<version_cmd>` probe may take.
pub const VERSION_PROBE_LIMIT: std::time::Duration = std::time::Duration::from_secs(5);

/// What one version probe found. A timeout is `Unknown`, never `Absent`: the
/// CLI may be there and slow (cold disk, loaded machine), so it is asked again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
    Version(String),
    /// No such program (or the plugin has no version command).
    Absent,
    /// It started but did not answer in time, failed, or printed nothing.
    Unknown,
}

impl Probe {
    pub fn version(self) -> Option<String> {
        match self {
            Probe::Version(v) => Some(v),
            _ => None,
        }
    }
}

/// The probe behind [`agent_version`], telling a missing program from a slow one.
/// `argv` replaces the manifest's command (tests); no shell either way.
pub async fn probe_agent(plugin: &dyn AgentPlugin, argv: Option<&[String]>, limit: std::time::Duration) -> Probe {
    let Some(argv) = argv.or_else(|| plugin.version_cmd()) else {
        return Probe::Absent;
    };
    let mut cmd = tokio::process::Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    for (k, _) in std::env::vars_os() {
        if k.to_str().is_some_and(|k| plugin.scrubs(k)) {
            cmd.env_remove(k);
        }
    }
    match tokio::time::timeout(limit, cmd.output()).await {
        Err(_) => Probe::Unknown,
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => Probe::Absent,
        Ok(Err(_)) => Probe::Unknown,
        Ok(Ok(out)) if !out.status.success() => Probe::Unknown,
        Ok(Ok(out)) => version_word(&String::from_utf8_lossy(&out.stdout)).map_or(Probe::Unknown, Probe::Version),
    }
}

/// The version in a `--version` line: the first word that starts with a
/// digit (`codex-cli 0.160.0` → `0.160.0`), else the first word.
pub fn version_word(s: &str) -> Option<String> {
    let mut words = s.split_whitespace();
    let first = words.clone().next()?;
    let v = words
        .find(|w| w.starts_with(|c: char| c.is_ascii_digit()))
        .unwrap_or(first);
    Some(v.chars().take(64).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_manifests_lint_clean() {
        let h = Host::builtin();
        assert_eq!(h.ids(), ["claude", "codex", "cursor"]);
        for l in &h.plugins {
            assert_eq!(l.warnings, Vec::<String>::new(), "{}", l.plugin.id());
        }
        assert_eq!(claude().agent_label(), "claude");
    }

    #[test]
    fn builtin_manifests_say_which_roles_they_host_as_helpers() {
        use manifest::RoleFieldSupport::{No, SomeVersions, Yes};
        let h = Host::builtin();
        let of = |id: &str| {
            let l = h.plugins.iter().find(|l| l.plugin.id() == id).unwrap();
            l.plugin.manifest().multi_agent.clone().unwrap()
        };
        let (claude, codex, cursor) = (of("claude"), of("codex"), of("cursor"));
        // A role fixed to another CLI is never a helper: it has to be a step.
        assert!(claude.hosts_role("claude") && claude.hosts_role("any") && !claude.hosts_role("codex"));
        assert!(codex.hosts_role("codex") && codex.hosts_role("any") && !codex.hosts_role("claude"));
        assert!(!cursor.hosts_role("cursor") && !cursor.hosts_role("any"));
        assert_eq!(
            ["instructions", "mcp", "skills"].map(|f| claude.role_field(f)),
            [Yes, SomeVersions, SomeVersions]
        );
        // Unverified Codex keys are not claimed: they apply to the whole pane.
        assert_eq!(
            ["purpose", "model", "instructions", "access"].map(|f| codex.role_field(f)),
            [Yes, Yes, No, No]
        );
        assert_eq!(codex.limits.max_depth, "agents.max_depth");
        assert_eq!(claude.limits, manifest::MultiAgentLimits::default());
    }

    #[test]
    fn claude_manifest_mirrors_the_legacy_tables() {
        // The Rust consts stay as compatibility mirrors of the manifest rows
        // (inject::HOOK_EVENTS, claude_auth::SCREEN_RULES); they must agree.
        let m = claude().manifest();
        let h = m.hooks.as_ref().unwrap();
        let reg: Vec<(&str, Option<u32>, bool)> = h
            .register
            .iter()
            .map(|r| (r.event.as_str(), r.timeout, r.matcher.is_some()))
            .collect();
        assert_eq!(reg, crate::inject::HOOK_EVENTS.to_vec());
        let rules: Vec<(&str, &str)> = m
            .auth
            .as_ref()
            .unwrap()
            .screen
            .iter()
            .filter(|r| r.state.is_empty())
            .map(|r| (r.id.as_str(), r.contains.as_str()))
            .collect();
        // The legacy table holds the sign-in rules; rules with a `state` came later.
        assert_eq!(rules, crate::claude_auth::SCREEN_RULES.to_vec());
        assert_eq!(OPENERS, crate::inject::OPENERS);
        // env.scrub is what the holder enforces.
        for k in [
            "CLAUDECODE",
            "CLAUDE_PID",
            "CLAUDE_EFFORT",
            "AI_AGENT",
            "CLAUDE_CODE_ENTRYPOINT",
        ] {
            assert!(claude().scrubs(k) && tmuxedo_holder::scrubbed_env_key(k), "{k}");
        }
        for k in [
            "CLAUDE",
            "CLAUDE_CONFIG_DIR",
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_BASE_URL",
            "PATH",
        ] {
            assert!(!claude().scrubs(k) && !tmuxedo_holder::scrubbed_env_key(k), "{k}");
        }
    }

    #[test]
    fn hook_mapping_and_holds() {
        let p = claude();
        let o = p.map_hook(&json!({"hook_event_name": "PermissionRequest", "session_id": "s",
            "tool_name": "Bash", "tool_input": {"command": "ls"}}));
        assert!(o.hold);
        assert_eq!(o.names(), ["prompt.permission"]);
        assert_eq!(o.status(), Some(tmuxedo_protocol::frames::AgentStatus::Blocked));
        assert_eq!(o.session_id.as_deref(), Some("s"));
        let o = p.map_hook(&json!({"hook_event_name": "Notification", "notification_type": "idle_prompt"}));
        assert_eq!(o.names(), ["attention.idle"]);
        assert!(!o.hold && !o.releases_holds());
        let o = p.map_hook(&json!({"hook_event_name": "Notification"}));
        assert!(o.events.is_empty() && o.status().is_none());
        let o = p.map_hook(&json!({"hook_event_name": "StopFailure", "error_type": "account_on_hold"}));
        assert_eq!(
            o.events,
            [AgentEvent::TurnFailed {
                error: "account_on_hold".into()
            }]
        );
        assert!(p.is_auth_error("account_on_hold") && !p.is_auth_error("rate_limit"));
        assert!(p.map_hook(&json!({"hook_event_name": "Stop"})).releases_holds());
        assert!(p.map_hook(&json!({})).events.is_empty());
    }

    #[test]
    fn subagent_hooks_map_through_the_manifest_fields() {
        use crate::subagents::Started;
        // Claude names the helper in both hooks.
        let p = claude();
        let start = json!({"hook_event_name": "SubagentStart", "agent_id": "a1", "agent_type": "Explore"});
        let o = p.map_hook(&start);
        let want = Started {
            id: Some("a1".into()),
            name: "Explore".into(),
            ..Started::default()
        };
        assert_eq!(o.events, [AgentEvent::SubagentStarted(want)]);
        assert_eq!((o.status(), o.hold, o.releases_holds()), (None, false, false));
        let stop = json!({"hook_event_name": "SubagentStop", "agent_id": "a1", "agent_type": "Explore"});
        let o = p.map_hook(&stop);
        let want = AgentEvent::SubagentCompleted {
            id: Some("a1".into()),
            name: Some("Explore".into()),
        };
        assert_eq!(o.events, [want]);
        assert_eq!((o.status(), o.releases_holds()), (None, false));
        // Codex names no fields: the core numbers and pairs its helpers.
        let codex = host().get("codex").unwrap();
        let o = codex.map_hook(&json!({"hook_event_name": "SubagentStart", "agent_id": "x"}));
        assert_eq!(o.events, [AgentEvent::SubagentStarted(Started::default())]);
        let o = codex.map_hook(&json!({"hook_event_name": "SubagentStop"}));
        assert_eq!(o.names(), ["subagent.completed"]);
        // A manifest that names the parent's field gets nested helpers.
        let mut m = claude().manifest().clone();
        m.hooks.as_mut().unwrap().subagent.parent_field = "parent_agent_id".into();
        let nested = ManifestPlugin::new(m);
        let inner = json!({"hook_event_name": "SubagentStart", "agent_id": "a2", "parent_agent_id": "a1"});
        let o = nested.map_hook(&inner);
        let want = Started {
            id: Some("a2".into()),
            parent_id: Some("a1".into()),
            ..Started::default()
        };
        assert_eq!(o.events, [AgentEvent::SubagentStarted(want)]);
        // Cursor's are not wired until its CLI is seen firing them.
        let cursor = host().get("cursor").unwrap();
        let o = cursor.map_hook(&json!({"hook_event_name": "subagentStart", "subagent_id": "x"}));
        assert!(o.events.is_empty());
    }

    #[test]
    fn reply_templates_fill_or_drop_the_message() {
        let p = claude();
        assert_eq!(
            p.reply(&json!({"decision": {"behavior": "deny", "message": "no"}})),
            r#"{"hookSpecificOutput":{"decision":{"behavior":"deny","message":"no"},"hookEventName":"PermissionRequest"}}"#
        );
        assert_eq!(
            p.reply(&json!({"decision": {"behavior": "allow"}})),
            r#"{"hookSpecificOutput":{"decision":{"behavior":"allow"},"hookEventName":"PermissionRequest"}}"#
        );
        assert_eq!(p.reply(&json!({"expired": true})), "");
    }

    #[test]
    fn registry_enable_disable_and_logs() {
        let d = tempfile::tempdir().unwrap();
        let mut h = Host::load(d.path());
        assert!(h.enabled("claude") && h.for_hook(None).is_some());
        h.set_enabled("claude", false).unwrap();
        assert!(h.for_hook(None).is_none() && h.for_hook(Some("claude")).is_none());
        assert!(h.by_label("claude").is_none());
        assert!(h.set_enabled("nope", true).is_err());
        let again = Host::load(d.path());
        assert!(!again.enabled("claude"), "persisted");
        assert_eq!(
            h.logs(Some("claude"), Some(1))[0].event.as_deref(),
            Some("plugin.disabled")
        );
        h.set_enabled("claude", true).unwrap();
        assert_eq!(h.logs(None, None).len(), 2);
        let info = &h.infos(None)[0];
        assert_eq!(info["plugin_id"], "claude");
        assert_eq!(info["actions"][0]["id"], "login");
        assert_eq!(h.action_infos(Some("claude"))[0]["action_id"], "login");
        assert!(h.infos(Some("other")).is_empty());
    }

    #[test]
    fn install_writes_shim_settings_and_openers() {
        let d = tempfile::tempdir().unwrap();
        let h = Host::builtin();
        let shim = h.install(d.path(), Path::new("/x/tmuxedo-runner"), &[]).unwrap();
        let mut names: Vec<String> = std::fs::read_dir(&shim)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            [
                "agent",
                "claude",
                "codex",
                "cursor-agent",
                "open",
                "sensible-browser",
                "www-browser",
                "x-www-browser",
                "xdg-open"
            ]
        );
        assert!(d.path().join("claude-settings.json").is_file());
        assert!(d.path().join("cursor-plugin/hooks/hooks.json").is_file());
        h.remove_shim("cursor", d.path()).unwrap();
        assert!(!shim.join("agent").exists() && !shim.join("cursor-agent").exists());
        h.remove_shim("claude", d.path()).unwrap();
        assert!(!shim.join("claude").exists());
        h.remove_shim("claude", d.path()).unwrap();
        h.write_shims(d.path(), &["--verbose".into()]).unwrap();
        assert!(std::fs::read_to_string(shim.join("claude"))
            .unwrap()
            .contains("'--verbose' --settings"));
        // The user's launch args are Claude's only.
        assert!(!std::fs::read_to_string(shim.join("codex"))
            .unwrap()
            .contains("--verbose"));
    }

    fn host() -> Host {
        Host::builtin()
    }

    /// Run a shim against a fake real binary that prints its argv and env.
    fn run_shim(shim: &Path, real_dir: &Path, name: &str, args: &[&str]) -> String {
        std::fs::create_dir_all(real_dir).unwrap();
        write_exec(
            &real_dir.join(name),
            "#!/bin/sh\nprintf 'ARGV:'\nfor a; do printf '<%s>' \"$a\"; done\necho\necho \"NOB=${NO_OPEN_BROWSER-unset}\"\n",
        )
        .unwrap();
        let out = std::process::Command::new(shim.join(name))
            .args(args)
            .env_clear()
            .env(
                "PATH",
                format!("{}:{}:/usr/bin:/bin", shim.display(), real_dir.display()),
            )
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    #[test]
    fn codex_shim_injects_hooks_with_dash_c_byte_identically() {
        let d = tempfile::tempdir().unwrap();
        let h = host();
        let shim = h.install(d.path(), Path::new("/st/bin/tmuxedo-runner"), &[]).unwrap();
        let codex = h.get("codex").unwrap();
        let argv = codex.hooks_argv(Path::new("/st/bin/tmuxedo-runner"));
        assert_eq!(argv.len(), 18, "9 events, a flag and a value each");
        assert_eq!(argv[0], "-c");
        assert_eq!(
            argv[1],
            r#"hooks.SessionStart=[{hooks=[{type="command",command="'/st/bin/tmuxedo-runner' hook --plugin codex"}]}]"#
        );
        let events: Vec<&str> = argv
            .iter()
            .skip(1)
            .step_by(2)
            .map(|v| v.split('=').next().unwrap())
            .collect();
        assert_eq!(
            events,
            [
                "hooks.SessionStart",
                "hooks.UserPromptSubmit",
                "hooks.PermissionRequest",
                "hooks.PostToolUse",
                "hooks.Stop",
                "hooks.Interrupt",
                "hooks.SubagentStart",
                "hooks.SubagentStop",
                "hooks.SessionEnd"
            ]
        );
        assert!(
            argv.iter().all(|a| !a.contains("timeout")),
            "no timeout: Codex's 600 s default, trust by hash"
        );
        // Same path, same definition: nothing for the user to trust again.
        assert_eq!(argv, codex.hooks_argv(Path::new("/st/bin/tmuxedo-runner")));
        let out = run_shim(&shim, &d.path().join("real"), "codex", &["fix it"]);
        assert!(out.contains(&format!("<-c><{}>", argv[1])), "{out}");
        assert!(out.trim_end().contains("<fix it>\nNOB=unset"), "{out}");
        // `--version` and friends run untouched.
        let out = run_shim(&shim, &d.path().join("real"), "codex", &["--version"]);
        assert!(out.starts_with("ARGV:<--version>\n"), "{out}");
        // Odd characters in the hook path are TOML-escaped inside the shell-quoted value.
        let odd = codex.hooks_argv(Path::new("/a \"b\"/it's/tmuxedo-runner"));
        assert!(
            odd[1].contains(r#"command="'/a \"b\"/it'\\''s/tmuxedo-runner' hook --plugin codex""#),
            "{}",
            odd[1]
        );
    }

    #[test]
    fn codex_managed_hooks_need_no_flags() {
        let d = tempfile::tempdir().unwrap();
        let managed = d.path().join("config.toml");
        let text = String::from_utf8(builtin::package("codex").unwrap()["plugin.toml"].clone())
            .unwrap()
            .replace("/etc/codex/config.toml", &managed.to_string_lossy());
        let p = ManifestPlugin::new(Manifest::parse(&text).unwrap());
        assert_eq!(p.hooks_argv(Path::new("/x/r")).len(), 18);
        std::fs::write(&managed, "[hooks]\n# something else\n").unwrap();
        assert_eq!(p.hooks_argv(Path::new("/x/r")).len(), 18, "another file is not ours");
        std::fs::write(
            &managed,
            "[hooks]\nSessionStart = [{ hooks = [{ type = \"command\", command = \"/h/r hook --plugin codex\" }] }]\n",
        )
        .unwrap();
        assert!(
            p.hooks_argv(Path::new("/x/r")).is_empty(),
            "managed: no per-launch hooks, no trust step"
        );
    }

    #[test]
    fn codex_skill_rides_in_developer_instructions() {
        let d = tempfile::tempdir().unwrap();
        let skill = d.path().join("skill");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(skill.join("AGENTS.md"), "# tmuxedo\nSay \"hi\".\n").unwrap();
        let p = host().get("codex").unwrap();
        let gen = BTreeMap::new();
        let s = p
            .shim_script_for(
                &ShimEnv {
                    gen: &gen,
                    args: &[],
                    skill: Some(&skill),
                    hook_exe: None,
                    hosted: false,
                    set: None,
                    usage: false,
                },
                None,
            )
            .unwrap();
        assert!(
            s.contains(r##"-c 'developer_instructions="# tmuxedo\nSay \"hi\".\n"' "$@""##),
            "{s}"
        );
    }

    #[test]
    fn cursor_shim_wraps_only_cursor_and_sets_no_open_browser() {
        let d = tempfile::tempdir().unwrap();
        let h = host();
        let shim = h.install(d.path(), Path::new("/x/tmuxedo-runner"), &[]).unwrap();
        let plugin_dir = d.path().join("cursor-plugin");
        // A real Cursor install: flags added, env set.
        let cursor_bin = d.path().join("cursor-agent/versions/1");
        for name in ["agent", "cursor-agent"] {
            let out = run_shim(&shim, &cursor_bin, name, &["hi"]);
            assert!(
                out.starts_with(&format!("ARGV:<--plugin-dir><{}><hi>\nNOB=1", plugin_dir.display())),
                "{name}: {out}"
            );
        }
        // Subcommands that take no session flags run untouched (env still set).
        let out = run_shim(&shim, &cursor_bin, "agent", &["status", "--format", "json"]);
        assert!(out.starts_with("ARGV:<status><--format><json>\nNOB=1"), "{out}");
        // Another program called `agent`: untouched.
        let out = run_shim(&shim, &d.path().join("other"), "agent", &["hi"]);
        assert!(out.starts_with("ARGV:<hi>\n"), "{out}");
        // Hosted machines: no self-update.
        let mut hosted = host();
        hosted.hosted = true;
        hosted.install(d.path(), Path::new("/x/tmuxedo-runner"), &[]).unwrap();
        let out = run_shim(&shim, &cursor_bin, "agent", &["hi"]);
        assert!(out.contains("<--disable-auto-update><hi>"), "{out}");
    }

    #[test]
    fn cursor_plugin_dir_carries_hooks_and_the_rule_when_the_skill_is_on() {
        let d = tempfile::tempdir().unwrap();
        let p = host().get("cursor").unwrap();
        let files = p.cursor_plugin_files(Path::new("/x/r"), None);
        let names: Vec<String> = files.iter().map(|(f, _)| f.display().to_string()).collect();
        assert_eq!(names, [".cursor-plugin/plugin.json", "hooks/hooks.json"]);
        let hooks: Value = serde_json::from_str(&files[1].1).unwrap();
        assert_eq!(hooks["version"], 1);
        assert_eq!(hooks["hooks"]["stop"][0]["command"], "'/x/r' hook --plugin cursor");
        assert!(hooks["hooks"]["sessionStart"][0].get("timeout").is_none());
        let skill = d.path().join("skill");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(skill.join("tmuxedo.mdc"), "---\nalwaysApply: true\n---\nhi\n").unwrap();
        let files = p.cursor_plugin_files(Path::new("/x/r"), Some(&skill));
        let m: Value = serde_json::from_str(&files[1].1).unwrap();
        assert_eq!(m["rules"], "rules/");
        assert_eq!(files[0].0, Path::new("rules/tmuxedo.mdc"));
        assert!(files[0].1.ends_with("hi\n"));
    }

    #[test]
    fn lift_and_fingerprint_rules() {
        let codex = host().get("codex").unwrap();
        let l = codex
            .lift("Open this link\n  https://auth.openai.com/codex/device\nEnter this one-time code\n  ABCD-12345\n")
            .unwrap();
        assert_eq!(
            (l.rule.as_str(), l.code.as_deref()),
            ("device_code", Some("ABCD-12345"))
        );
        // A code before the link is not taken.
        let l = codex.lift("ABCD-12345\nhttps://auth.openai.com/codex/device").unwrap();
        assert_eq!(l.code, None);
        let cursor = host().get("cursor").unwrap();
        assert!(cursor.lift("no link here").is_none());
        assert!(cursor.prompt_fingerprint("Run this command?\n$ ls").is_some());
        assert!(
            codex.prompt_fingerprint("Run this command?\n$ ls").is_none(),
            "Codex answers by hook only"
        );
        assert_eq!(version_word("codex-cli 0.160.0\n").as_deref(), Some("0.160.0"));
        assert_eq!(version_word("2.1.289 (Claude Code)").as_deref(), Some("2.1.289"));
        assert_eq!(
            version_word("2026.10.01-e373342").as_deref(),
            Some("2026.10.01-e373342")
        );
        assert_eq!(version_word("dev").as_deref(), Some("dev"));
        assert_eq!(version_word("  "), None);
    }

    #[test]
    fn codex_interrupt_releases_and_cursor_reads_conversation_ids() {
        let codex = host().get("codex").unwrap();
        let o = codex.map_hook(&json!({"hook_event_name": "Interrupt", "session_id": "s"}));
        assert!(o.releases_holds() && o.status() == Some(tmuxedo_protocol::frames::AgentStatus::Idle));
        let o = codex.map_hook(&json!({"hook_event_name": "PermissionRequest", "tool_name": "Bash", "tool_input": {}}));
        assert!(o.hold);
        let p = codex.manifest().permissions.as_ref().unwrap();
        assert!(p.release_on_input && p.hold_secs < 600);
        let cursor = host().get("cursor").unwrap();
        let o =
            cursor.map_hook(&json!({"hook_event_name": "sessionStart", "conversation_id": "c1", "session_id": "c1"}));
        assert_eq!(o.session_id.as_deref(), Some("c1"));
        assert!(
            !cursor
                .map_hook(&json!({"hook_event_name": "beforeShellExecution"}))
                .hold,
            "Cursor's allow does not approve: no hold"
        );
    }
}
