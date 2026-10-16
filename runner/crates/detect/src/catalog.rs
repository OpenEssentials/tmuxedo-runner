//! Agent files: parsing, validation, local overrides, reading a screen and
//! explaining a reading (docs/design/agent-detection.md §4, §7, §9, §10).

use crate::cond::{Cond, Subject};
use crate::scope::Scope;
use crate::{Input, Status};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Most signals one agent file may have.
pub const MAX_SIGNALS: usize = 100;
/// Longest signal name.
pub const MAX_NAME: usize = 64;
/// How much of a scope `explain` shows.
pub const PREVIEW_CHARS: usize = 240;

/// The agent files compiled into the binary, in catalogue order.
pub const BUNDLED: &[(&str, &str)] = &[
    ("claude", include_str!("../agents/claude.toml")),
    ("codex", include_str!("../agents/codex.toml")),
    ("gemini", include_str!("../agents/gemini.toml")),
    ("cursor", include_str!("../agents/cursor.toml")),
    ("copilot", include_str!("../agents/copilot.toml")),
    ("opencode", include_str!("../agents/opencode.toml")),
    ("amp", include_str!("../agents/amp.toml")),
    ("devin", include_str!("../agents/devin.toml")),
    ("agy", include_str!("../agents/agy.toml")),
    ("cline", include_str!("../agents/cline.toml")),
    ("kilo", include_str!("../agents/kilo.toml")),
    ("kimi", include_str!("../agents/kimi.toml")),
    ("kiro", include_str!("../agents/kiro.toml")),
    ("droid", include_str!("../agents/droid.toml")),
    ("grok", include_str!("../agents/grok.toml")),
    ("hermes", include_str!("../agents/hermes.toml")),
    ("letta", include_str!("../agents/letta.toml")),
    ("maki", include_str!("../agents/maki.toml")),
    ("muse", include_str!("../agents/muse.toml")),
    ("qodercli", include_str!("../agents/qodercli.toml")),
    ("qwen", include_str!("../agents/qwen.toml")),
    ("pi", include_str!("../agents/pi.toml")),
    ("omp", include_str!("../agents/omp.toml")),
    ("mastracode", include_str!("../agents/mastracode.toml")),
];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileDoc {
    agent: String,
    title: String,
    revision: String,
    launch: String,
    #[serde(default)]
    names: Vec<String>,
    #[serde(default)]
    process: ProcessDoc,
    #[serde(default)]
    signal: Vec<SignalDoc>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct ProcessDoc {
    #[serde(default)]
    scripts: Vec<String>,
    #[serde(default)]
    versioned: Option<String>,
    #[serde(default)]
    batch_flags: Vec<String>,
    #[serde(default)]
    value_flags: Vec<String>,
    #[serde(default)]
    subcommand_is_batch: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SignalDoc {
    name: String,
    status: String,
    rank: i32,
    scope: String,
    when: String,
}

/// Where an agent's definition came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    BuiltIn,
    Override(PathBuf),
}

impl Source {
    pub fn label(&self) -> String {
        match self {
            Source::BuiltIn => "built-in".to_string(),
            Source::Override(p) => p.display().to_string(),
        }
    }
}

/// How the process side recognises the agent (§8).
#[derive(Debug, Clone, Default)]
pub struct ProcessRules {
    /// Script path suffixes, as lower-cased components (last one without `.js`/`.mjs`/`.cjs`).
    pub scripts: Vec<Vec<String>>,
    pub versioned: Option<String>,
    pub batch_flags: Vec<String>,
    pub value_flags: Vec<String>,
    pub subcommand_is_batch: bool,
}

#[derive(Debug, Clone)]
pub struct Signal {
    pub name: String,
    pub status: Status,
    pub rank: i32,
    pub scope: Scope,
    pub when: String,
    pub cond: Cond,
}

#[derive(Debug, Clone)]
pub struct AgentDef {
    pub id: String,
    pub title: String,
    pub revision: String,
    pub launch: String,
    /// The id and every name, normalised for [`crate::identify::normalise`].
    pub names: Vec<String>,
    pub process: ProcessRules,
    /// In file order.
    pub signals: Vec<Signal>,
    /// Indexes into `signals`, highest rank first (file order among equals).
    by_rank: Vec<usize>,
    pub source: Source,
    pub warning: Option<String>,
}

/// Strip one script extension (`.js`, `.mjs`, `.cjs`) for comparisons.
pub(crate) fn strip_script_ext(s: &str) -> &str {
    [".js", ".mjs", ".cjs"]
        .iter()
        .find_map(|e| s.strip_suffix(e))
        .unwrap_or(s)
}

impl AgentDef {
    /// Parse and validate one agent file.
    pub fn parse(text: &str, source: Source) -> Result<AgentDef, String> {
        let doc: FileDoc = toml::from_str(text).map_err(|e| e.to_string().trim().to_string())?;
        for (key, value) in [
            ("agent", &doc.agent),
            ("title", &doc.title),
            ("revision", &doc.revision),
            ("launch", &doc.launch),
        ] {
            if value.trim().is_empty() {
                return Err(format!("{key} must not be empty"));
            }
        }
        if doc.signal.len() > MAX_SIGNALS {
            return Err(format!("{} signals, the limit is {MAX_SIGNALS}", doc.signal.len()));
        }
        let mut signals: Vec<Signal> = Vec::with_capacity(doc.signal.len());
        for s in doc.signal {
            let name = s.name;
            if name.trim().is_empty() || name.chars().count() > MAX_NAME {
                return Err(format!("signal name {name:?} must be 1 to {MAX_NAME} characters"));
            }
            if signals.iter().any(|o| o.name == name) {
                return Err(format!("two signals are named {name:?}"));
            }
            let status =
                Status::parse(&s.status).ok_or_else(|| format!("signal {name}: unknown status {:?}", s.status))?;
            let scope = Scope::parse(&s.scope).map_err(|e| format!("signal {name}: {e}"))?;
            if status == Status::Hold && !scope.on_screen() {
                return Err(format!("signal {name}: hold needs a screen scope, not {scope}"));
            }
            let cond = Cond::parse(&s.when).map_err(|e| format!("signal {name}: when {e}"))?;
            signals.push(Signal {
                name,
                status,
                rank: s.rank,
                scope,
                when: s.when.trim().to_string(),
                cond,
            });
        }
        let mut by_rank: Vec<usize> = (0..signals.len()).collect();
        by_rank.sort_by_key(|&i| std::cmp::Reverse(signals[i].rank));
        let mut names = vec![crate::identify::normalise(&doc.agent)];
        names.extend(doc.names.iter().map(|n| crate::identify::normalise(n)));
        let p = doc.process;
        let process = ProcessRules {
            scripts: p
                .scripts
                .iter()
                .map(|s| {
                    let mut parts: Vec<String> =
                        s.split('/').filter(|c| !c.is_empty()).map(str::to_lowercase).collect();
                    if let Some(last) = parts.last_mut() {
                        *last = strip_script_ext(last).to_string();
                    }
                    parts
                })
                .collect(),
            versioned: p.versioned.map(|v| v.to_lowercase()),
            batch_flags: p.batch_flags,
            value_flags: p.value_flags,
            subcommand_is_batch: p.subcommand_is_batch,
        };
        Ok(AgentDef {
            id: doc.agent,
            title: doc.title,
            revision: doc.revision,
            launch: doc.launch,
            names,
            process,
            signals,
            by_rank,
            source,
            warning: None,
        })
    }

    /// The reading for this input (§7).
    pub fn read(&self, input: &Input<'_>) -> Status {
        let mut cache = SubjectCache::default();
        self.by_rank
            .iter()
            .map(|&i| &self.signals[i])
            .find(|s| s.cond.eval(cache.get(s.scope, input)))
            .map_or(Status::Idle, |s| s.status)
    }

    /// Every signal's result and the decision (§10).
    pub fn explain(&self, input: &Input<'_>) -> Explanation {
        let mut cache = SubjectCache::default();
        let matched: Vec<bool> = self
            .signals
            .iter()
            .map(|s| s.cond.eval(cache.get(s.scope, input)))
            .collect();
        let decided = self.by_rank.iter().copied().find(|&i| matched[i]);
        Explanation {
            agent: Some(self.id.clone()),
            status: decided.map_or(Status::Idle, |i| self.signals[i].status),
            signal: decided.map(|i| self.signals[i].name.clone()),
            default: decided.is_none(),
            source: Some(self.source.label()),
            revision: Some(self.revision.clone()),
            warning: self.warning.clone(),
            signals: self
                .signals
                .iter()
                .zip(matched)
                .map(|(s, matched)| {
                    let text = s.scope.slice(input);
                    SignalResult {
                        name: s.name.clone(),
                        status: s.status,
                        rank: s.rank,
                        scope: s.scope.to_string(),
                        when: s.when.clone(),
                        matched,
                        scope_bytes: text.len(),
                        scope_preview: preview(text),
                    }
                })
                .collect(),
        }
    }

    /// The `server.agent_manifests` row.
    pub fn row(&self) -> Value {
        json!({
            "agent": self.id,
            "title": self.title,
            "source": self.source.label(),
            "revision": self.revision,
            "signals": self.signals.len(),
            "warning": self.warning,
        })
    }
}

fn preview(text: &str) -> String {
    let mut out: String = text.chars().take(PREVIEW_CHARS).collect();
    if out.len() < text.len() {
        out.push('…');
    }
    out
}

/// Scope texts already lower-cased in this reading.
#[derive(Default)]
struct SubjectCache<'a> {
    seen: Vec<(Scope, Subject<'a>)>,
}

impl<'a> SubjectCache<'a> {
    fn get(&mut self, scope: Scope, input: &Input<'a>) -> &Subject<'a> {
        let i = match self.seen.iter().position(|(s, _)| *s == scope) {
            Some(i) => i,
            None => {
                self.seen.push((scope, Subject::new(scope.slice(input))));
                self.seen.len() - 1
            }
        };
        &self.seen[i].1
    }
}

/// `explain` output (§10).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Explanation {
    pub agent: Option<String>,
    pub status: Status,
    pub signal: Option<String>,
    pub default: bool,
    pub source: Option<String>,
    pub revision: Option<String>,
    pub warning: Option<String>,
    pub signals: Vec<SignalResult>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignalResult {
    pub name: String,
    pub status: Status,
    pub rank: i32,
    pub scope: String,
    pub when: String,
    pub matched: bool,
    pub scope_bytes: usize,
    pub scope_preview: String,
}

impl Explanation {
    /// For a label that names no agent (or no label at all).
    pub fn unknown(label: Option<&str>) -> Explanation {
        Explanation {
            agent: label.map(str::to_owned),
            status: Status::Unknown,
            signal: None,
            default: false,
            source: None,
            revision: None,
            warning: None,
            signals: vec![],
        }
    }

    pub fn to_json(&self) -> Value {
        json!({
            "agent": self.agent,
            "status": self.status.as_str(),
            "signal": self.signal,
            "default": self.default,
            "source": self.source,
            "revision": self.revision,
            "warning": self.warning,
            "signals": self.signals.iter().map(|s| json!({
                "name": s.name,
                "status": s.status.as_str(),
                "rank": s.rank,
                "scope": s.scope,
                "when": s.when,
                "matched": s.matched,
                "scope_bytes": s.scope_bytes,
                "scope_preview": s.scope_preview,
            })).collect::<Vec<_>>(),
        })
    }
}

/// Every supported agent (bundled order), with local overrides applied.
#[derive(Debug, Clone)]
pub struct Catalog {
    pub agents: Vec<AgentDef>,
}

impl Catalog {
    /// The bundled files, each replaced by `<dir>/<agent>.toml` when that is valid.
    pub fn load(override_dir: Option<&Path>) -> Catalog {
        let agents = BUNDLED
            .iter()
            .map(|(id, text)| {
                let bundled = AgentDef::parse(text, Source::BuiltIn).expect("bundled agent files are valid (tested)");
                match override_dir {
                    Some(dir) => apply_override(bundled, id, &dir.join(format!("{id}.toml"))),
                    None => bundled,
                }
            })
            .collect();
        Catalog { agents }
    }

    pub fn rows(&self) -> Vec<Value> {
        self.agents.iter().map(AgentDef::row).collect()
    }
}

fn apply_override(bundled: AgentDef, id: &str, path: &Path) -> AgentDef {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return bundled,
        Err(e) => return warn(bundled, path, &e.to_string()),
    };
    match AgentDef::parse(&text, Source::Override(path.to_path_buf())) {
        Ok(def) if def.id == id => def,
        Ok(def) => warn(bundled, path, &format!("it is for agent {:?}", def.id)),
        Err(e) => warn(bundled, path, &e),
    }
}

fn warn(mut def: AgentDef, path: &Path, why: &str) -> AgentDef {
    def.warning = Some(format!("ignored {}: {why}", path.display()));
    def
}

/// `$XDG_CONFIG_HOME/tmuxedo/agents`, else `$HOME/.config/tmuxedo/agents`.
pub fn override_dir(xdg_config_home: Option<PathBuf>, home: Option<PathBuf>) -> Option<PathBuf> {
    let base = xdg_config_home
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| home.map(|h| h.join(".config")))?;
    Some(base.join("tmuxedo").join("agents"))
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))] // the tests themselves are not product code
mod tests {
    use super::*;

    const MINI: &str = r#"
agent = "demo"
title = "Demo"
revision = "r1"
launch = "demo"
names = ["Demo-CLI"]

[process]
scripts = ["pkg/dist/CLI.mjs", "/lead/slash/"]
versioned = "Demo-Bin-"
batch_flags = ["-p"]
value_flags = ["--x"]
subcommand_is_batch = true

[[signal]]
name = "busy"
status = "working"
rank = 10
scope = "last 2"
when = '''"busy"'''

[[signal]]
name = "ask"
status = "blocked"
rank = 20
scope = "screen"
when = ''' "?" '''

[[signal]]
name = "ask-too"
status = "idle"
rank = 20
scope = "screen"
when = '''"?"'''

[[signal]]
name = "viewer"
status = "hold"
rank = 30
scope = "screen"
when = '''"viewer"'''

[[signal]]
name = "quiet"
status = "unknown"
rank = 5
scope = "title"
when = '''"quiet"'''
"#;

    fn mini() -> AgentDef {
        AgentDef::parse(MINI, Source::BuiltIn).unwrap()
    }

    fn with(extra: &str) -> Result<AgentDef, String> {
        AgentDef::parse(&format!("{MINI}\n{extra}"), Source::BuiltIn)
    }

    fn input(screen: &str) -> Input<'_> {
        Input {
            screen,
            title: "",
            progress: "",
        }
    }

    #[test]
    fn every_bundled_file_is_valid_and_named_after_its_agent() {
        for (id, text) in BUNDLED {
            let def = AgentDef::parse(text, Source::BuiltIn).unwrap_or_else(|e| panic!("{id}: {e}"));
            assert_eq!(def.id, *id);
            assert!(def.names.contains(&id.to_string()));
            assert!(def.warning.is_none());
        }
    }

    #[test]
    fn no_name_means_two_agents() {
        let cat = Catalog::load(None);
        let mut seen = std::collections::HashMap::new();
        for a in &cat.agents {
            for n in &a.names {
                assert!(seen.insert(n.clone(), a.id.clone()).is_none(), "{n} is claimed twice");
            }
        }
    }

    #[test]
    fn parse_reads_every_field() {
        let d = mini();
        assert_eq!(
            (d.id.as_str(), d.title.as_str(), d.revision.as_str(), d.launch.as_str()),
            ("demo", "Demo", "r1", "demo")
        );
        assert_eq!(d.names, ["demo", "demo-cli"]);
        assert_eq!(d.process.scripts, [vec!["pkg", "dist", "cli"], vec!["lead", "slash"]]);
        assert_eq!(d.process.versioned.as_deref(), Some("demo-bin-"));
        assert_eq!(d.process.batch_flags, ["-p"]);
        assert_eq!(d.process.value_flags, ["--x"]);
        assert!(d.process.subcommand_is_batch);
        assert_eq!(d.signals.len(), 5);
        assert_eq!(d.signals[1].when, r#""?""#, "trimmed");
        assert_eq!(d.signals[0].scope, Scope::Last(2));
        assert_eq!(d.by_rank, [3, 1, 2, 0, 4]);
        let bare = AgentDef::parse("agent='a'\ntitle='A'\nrevision='1'\nlaunch='a'", Source::BuiltIn).unwrap();
        assert!(bare.signals.is_empty() && bare.process.scripts.is_empty() && !bare.process.subcommand_is_batch);
        let slash = AgentDef::parse(
            "agent='a'\ntitle='A'\nrevision='1'\nlaunch='a'\n[process]\nscripts=['/']",
            Source::BuiltIn,
        )
        .unwrap();
        assert_eq!(slash.process.scripts, [Vec::<String>::new()]);
    }

    #[test]
    fn parse_errors() {
        let e = |t: &str| AgentDef::parse(t, Source::BuiltIn).unwrap_err();
        assert!(!e("agent = ").is_empty(), "toml syntax");
        assert!(e("agent='a'\ntitle='A'\nrevision='1'\nlaunch='a'\nextra=1").contains("unknown field"));
        assert!(e("title='A'\nrevision='1'\nlaunch='a'").contains("missing field"));
        assert_eq!(
            e("agent='a'\ntitle=' '\nrevision='1'\nlaunch='a'"),
            "title must not be empty"
        );
        assert_eq!(
            e("agent=''\ntitle='A'\nrevision='1'\nlaunch='a'"),
            "agent must not be empty"
        );
        assert_eq!(
            e("agent='a'\ntitle='A'\nrevision=''\nlaunch='a'"),
            "revision must not be empty"
        );
        assert_eq!(
            e("agent='a'\ntitle='A'\nrevision='1'\nlaunch=''"),
            "launch must not be empty"
        );
        assert!(
            with("[[signal]]\nname='x'\nstatus='idle'\nrank=1\nscope='screen'\nwhen='\"a\"'\nmore=1")
                .unwrap_err()
                .contains("unknown field")
        );
        assert!(e("agent='a'\ntitle='A'\nrevision='1'\nlaunch='a'\n[process]\nnope=1").contains("unknown field"));
        let sig = |name: &str, status: &str, scope: &str, when: &str| {
            with(&format!(
                "[[signal]]\nname='{name}'\nstatus='{status}'\nrank=1\nscope='{scope}'\nwhen='''{when}'''"
            ))
            .unwrap_err()
        };
        assert_eq!(sig("busy", "idle", "screen", "\"a\""), "two signals are named \"busy\"");
        assert_eq!(
            sig(" ", "idle", "screen", "\"a\""),
            "signal name \" \" must be 1 to 64 characters"
        );
        assert!(sig(&"n".repeat(65), "idle", "screen", "\"a\"").starts_with("signal name"));
        assert_eq!(sig("x", "done", "screen", "\"a\""), "signal x: unknown status \"done\"");
        assert_eq!(sig("x", "idle", "whole", "\"a\""), "signal x: unknown scope \"whole\"");
        assert_eq!(
            sig("x", "hold", "title", "\"a\""),
            "signal x: hold needs a screen scope, not title"
        );
        assert_eq!(
            sig("x", "hold", "progress", "\"a\""),
            "signal x: hold needs a screen scope, not progress"
        );
        assert_eq!(
            sig("x", "idle", "screen", "\"a"),
            "signal x: when at byte 0: unterminated text"
        );
        let many: String = (0..=MAX_SIGNALS)
            .map(|i| format!("[[signal]]\nname='s{i}'\nstatus='idle'\nrank=1\nscope='screen'\nwhen='\"a\"'\n"))
            .collect();
        assert_eq!(
            AgentDef::parse(
                &format!("agent='a'\ntitle='A'\nrevision='1'\nlaunch='a'\n{many}"),
                Source::BuiltIn
            )
            .unwrap_err(),
            format!("{} signals, the limit is {MAX_SIGNALS}", MAX_SIGNALS + 1)
        );
        assert!(with("[[signal]]\nname='h'\nstatus='hold'\nrank=1\nscope='box'\nwhen='\"a\"'").is_ok());
    }

    #[test]
    fn read_picks_the_highest_rank_and_the_first_among_equals() {
        let d = mini();
        assert_eq!(d.read(&input("nothing")), Status::Idle, "default");
        assert_eq!(d.read(&input("x\nbusy")), Status::Working);
        assert_eq!(
            d.read(&input("busy\na\nb")),
            Status::Idle,
            "busy is outside last 2: default idle"
        );
        assert_eq!(
            d.read(&input("busy?")),
            Status::Blocked,
            "rank 20 beats 10; ask is declared before ask-too"
        );
        assert_eq!(d.read(&input("viewer? busy")), Status::Hold);
        assert_eq!(
            d.read(&Input {
                screen: "",
                title: "Quiet",
                progress: ""
            }),
            Status::Unknown
        );
    }

    #[test]
    fn explain_shows_every_signal_and_the_decision() {
        let d = mini();
        let e = d.explain(&input("busy?"));
        assert_eq!(e.agent.as_deref(), Some("demo"));
        assert_eq!(
            (e.status, e.signal.as_deref(), e.default),
            (Status::Blocked, Some("ask"), false)
        );
        assert_eq!(e.source.as_deref(), Some("built-in"));
        assert_eq!(e.revision.as_deref(), Some("r1"));
        let matched: Vec<_> = e.signals.iter().map(|s| (s.name.as_str(), s.matched)).collect();
        assert_eq!(
            matched,
            [
                ("busy", true),
                ("ask", true),
                ("ask-too", true),
                ("viewer", false),
                ("quiet", false)
            ]
        );
        assert_eq!(e.signals[0].scope, "last 2");
        assert_eq!(e.signals[0].scope_bytes, 5);
        assert_eq!(e.signals[4].scope_preview, "");
        let none = d.explain(&input("calm"));
        assert_eq!((none.status, none.signal, none.default), (Status::Idle, None, true));

        let long = "x".repeat(PREVIEW_CHARS + 1);
        let e = d.explain(&input(&long));
        assert_eq!(e.signals[1].scope_preview.chars().count(), PREVIEW_CHARS + 1);
        assert!(e.signals[1].scope_preview.ends_with('…'));
        let exact = "y".repeat(PREVIEW_CHARS);
        assert_eq!(d.explain(&input(&exact)).signals[1].scope_preview, exact);

        let j = d.explain(&input("busy?")).to_json();
        assert_eq!(
            j,
            json!({
                "agent": "demo", "status": "blocked", "signal": "ask", "default": false, "source": "built-in",
                "revision": "r1", "warning": null,
                "signals": [
                    {"name": "busy", "status": "working", "rank": 10, "scope": "last 2", "when": "\"busy\"", "matched": true, "scope_bytes": 5, "scope_preview": "busy?"},
                    {"name": "ask", "status": "blocked", "rank": 20, "scope": "screen", "when": "\"?\"", "matched": true, "scope_bytes": 5, "scope_preview": "busy?"},
                    {"name": "ask-too", "status": "idle", "rank": 20, "scope": "screen", "when": "\"?\"", "matched": true, "scope_bytes": 5, "scope_preview": "busy?"},
                    {"name": "viewer", "status": "hold", "rank": 30, "scope": "screen", "when": "\"viewer\"", "matched": false, "scope_bytes": 5, "scope_preview": "busy?"},
                    {"name": "quiet", "status": "unknown", "rank": 5, "scope": "title", "when": "\"quiet\"", "matched": false, "scope_bytes": 0, "scope_preview": ""},
                ],
            })
        );
    }

    #[test]
    fn unknown_explanations() {
        let e = Explanation::unknown(Some("vim"));
        assert_eq!(e.agent.as_deref(), Some("vim"));
        assert_eq!(e.status, Status::Unknown);
        assert!(e.signals.is_empty() && !e.default && e.signal.is_none() && e.source.is_none());
        let j = Explanation::unknown(None).to_json();
        assert_eq!(j["agent"], Value::Null);
        assert_eq!(j["status"], "unknown");
        assert_eq!(j["signals"], json!([]));
    }

    #[test]
    fn rows() {
        assert_eq!(
            mini().row(),
            json!({"agent": "demo", "title": "Demo", "source": "built-in", "revision": "r1", "signals": 5, "warning": null})
        );
        assert_eq!(Catalog::load(None).rows().len(), BUNDLED.len());
    }

    #[test]
    fn overrides_replace_valid_files_and_warn_about_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        std::fs::write(
            p.join("claude.toml"),
            "agent='claude'\ntitle='Mine'\nrevision='local-1'\nlaunch='claude'\n[[signal]]\nname='x'\nstatus='working'\nrank=1\nscope='screen'\nwhen='\"zzz\"'",
        )
        .unwrap();
        std::fs::write(
            p.join("codex.toml"),
            "agent='gemini'\ntitle='G'\nrevision='1'\nlaunch='gemini'",
        )
        .unwrap();
        std::fs::write(p.join("gemini.toml"), "agent = ").unwrap();
        std::fs::create_dir(p.join("cursor.toml")).unwrap();
        let cat = Catalog::load(Some(p));
        let by = |id: &str| cat.agents.iter().find(|a| a.id == id).unwrap();
        assert_eq!(by("claude").title, "Mine");
        assert_eq!(by("claude").source, Source::Override(p.join("claude.toml")));
        assert_eq!(
            by("claude").row()["source"],
            p.join("claude.toml").display().to_string()
        );
        assert_eq!(by("claude").read(&input("zzz")), Status::Working);
        let w = by("codex").warning.clone().unwrap();
        assert!(w.contains("it is for agent \"gemini\""), "{w}");
        assert_eq!(by("codex").source, Source::BuiltIn);
        assert!(by("gemini").warning.clone().unwrap().starts_with("ignored "));
        assert!(by("cursor").warning.is_some(), "unreadable");
        assert!(by("amp").warning.is_none(), "no file: bundled, no warning");
        let none = Catalog::load(Some(&p.join("missing")));
        assert!(none
            .agents
            .iter()
            .all(|a| a.warning.is_none() && a.source == Source::BuiltIn));
    }

    #[test]
    fn override_dir_follows_xdg_then_home() {
        let x = |a: Option<&str>, b: Option<&str>| override_dir(a.map(PathBuf::from), b.map(PathBuf::from));
        assert_eq!(x(Some("/c"), Some("/h")), Some(PathBuf::from("/c/tmuxedo/agents")));
        assert_eq!(
            x(Some(""), Some("/h")),
            Some(PathBuf::from("/h/.config/tmuxedo/agents"))
        );
        assert_eq!(x(None, Some("/h")), Some(PathBuf::from("/h/.config/tmuxedo/agents")));
        assert_eq!(x(None, None), None);
    }

    #[test]
    fn script_endings() {
        assert_eq!(strip_script_ext("a.js"), "a");
        assert_eq!(strip_script_ext("a.mjs"), "a");
        assert_eq!(strip_script_ext("a.cjs"), "a");
        assert_eq!(strip_script_ext("a.ts"), "a.ts");
    }
}
