//! Which agent a process is (docs/design/agent-detection.md §8).

use crate::catalog::{strip_script_ext, AgentDef, Catalog};
use crate::platform::{Job, Process};
use std::path::Path;

/// Endings dropped from a name before matching (launchers and shims keep them).
const NAME_ENDINGS: &[&str] = &[".js", ".exe", ".cmd", ".bat", ".ps1"];

/// Lower-cased base name without one launcher ending (§8.1 steps 1–2).
pub fn normalise(name: &str) -> String {
    let lower = name.trim().to_lowercase();
    let base = lower.rsplit('/').find(|c| !c.is_empty()).unwrap_or("");
    NAME_ENDINGS
        .iter()
        .find_map(|e| base.strip_suffix(e))
        .unwrap_or(base)
        .to_string()
}

/// The agent a name, path or label names (§8.1).
pub fn lookup(cat: &Catalog, name: &str) -> Option<usize> {
    let n = normalise(name);
    cat.agents.iter().position(|a| a.names.contains(&n)).or_else(|| {
        cat.agents.iter().position(|a| {
            a.process.versioned.as_deref().is_some_and(|prefix| {
                n.strip_prefix(prefix)
                    .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()))
            })
        })
    })
}

/// The runtime family of a program base name, if it is one (§8.3 step 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Runtime {
    Js,
    Python,
    Shell,
}

fn runtime(base: &str) -> Option<Runtime> {
    match base {
        "node" | "bun" => Some(Runtime::Js),
        "sh" | "bash" | "zsh" | "fish" => Some(Runtime::Shell),
        "python" => Some(Runtime::Python),
        _ => {
            let version = base.strip_prefix("python")?;
            let numeric = version
                .split('.')
                .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
            numeric.then_some(Runtime::Python)
        }
    }
}

/// Flags that run inline code (no script operand follows).
fn inline_flags(rt: Runtime) -> &'static [&'static str] {
    match rt {
        Runtime::Js => &["-e", "--eval", "-p", "--print"],
        Runtime::Python => &["-c", "-m"],
        Runtime::Shell => &["-c"],
    }
}

/// Options that take the next argument as their value.
const VALUE_OPTIONS: &[&str] = &[
    "-r",
    "--require",
    "--import",
    "--loader",
    "--experimental-loader",
    "--inspect-port",
    "-W",
    "-X",
    "-S",
    "-L",
    "-o",
];

/// `-e`, `-eCODE`, `--eval`, `--eval=CODE` all match `-e` / `--eval`.
fn is_flag(arg: &str, flag: &str) -> bool {
    match arg.strip_prefix(flag) {
        Some("") => true,
        Some(rest) => {
            if flag.starts_with("--") {
                rest.starts_with('=')
            } else {
                true
            }
        }
        None => false,
    }
}

/// Index (into `argv`) of the script operand a runtime runs, if any.
fn script_operand(rt: Runtime, argv: &[String]) -> Option<usize> {
    let mut i = 1;
    while let Some(arg) = argv.get(i) {
        if arg == "--" {
            return (i + 1 < argv.len()).then_some(i + 1);
        }
        if inline_flags(rt).iter().any(|f| is_flag(arg, f)) {
            return None;
        }
        if !arg.starts_with('-') {
            return Some(i);
        }
        i += if VALUE_OPTIONS.contains(&arg.as_str()) { 2 } else { 1 };
    }
    None
}

/// Whether `path` ends with `suffix` (lower-cased components, script ending ignored, `*` = any one).
fn ends_with_script(path: &str, suffix: &[String]) -> bool {
    let parts: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();
    if parts.len() < suffix.len() {
        return false;
    }
    let tail = &parts[parts.len() - suffix.len()..];
    tail.iter().zip(suffix).enumerate().all(|(i, (have, want))| {
        let have = have.to_lowercase();
        let have = if i + 1 == suffix.len() {
            strip_script_ext(&have).to_string()
        } else {
            have
        };
        want == "*" || *want == have
    })
}

/// The agent a path names: base name, script suffix, or symlink target (§8.3).
fn names_agent(cat: &Catalog, token: &str) -> Option<usize> {
    if token.is_empty() || token.starts_with('-') {
        return None;
    }
    lookup(cat, token)
        .or_else(|| {
            cat.agents
                .iter()
                .position(|a| a.process.scripts.iter().any(|s| ends_with_script(token, s)))
        })
        .or_else(|| {
            // Only paths with a directory part are resolved (a bare name is not a file here).
            let target = token
                .contains('/')
                .then(|| std::fs::canonicalize(Path::new(token)).ok())??;
            named_by_file(cat, &target)
        })
}

/// The agent a resolved file is named after (its last component, which must be UTF-8).
fn named_by_file(cat: &Catalog, path: &Path) -> Option<usize> {
    lookup(cat, path.file_name()?.to_str()?)
}

/// A one-shot run of an agent that declares batch use (§8.3).
fn is_batch(def: &AgentDef, args: &[String]) -> bool {
    let p = &def.process;
    let flagged = args.iter().any(|a| {
        let name = a.split_once('=').map_or(a.as_str(), |(n, _)| n);
        p.batch_flags.iter().any(|f| f == name)
    });
    if flagged {
        return true;
    }
    if !p.subcommand_is_batch {
        return false;
    }
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if p.value_flags.contains(a) {
            it.next();
            continue;
        }
        if p.value_flags
            .iter()
            .any(|f| a.strip_prefix(f.as_str()).is_some_and(|r| r.starts_with('=')))
        {
            continue;
        }
        return !a.starts_with('-');
    }
    false
}

/// The agent a runtime's script operand names, with the arguments after the operand.
fn wrapped<'p>(cat: &Catalog, rt: Runtime, argv: &'p [String]) -> Option<(usize, &'p [String])> {
    let i = script_operand(rt, argv)?;
    Some((names_agent(cat, &argv[i])?, &argv[i + 1..]))
}

/// The agent one process runs, one-shot (batch) runs included: who a hook
/// caller descends from (the runner's hook ancestry check).
pub fn process_any(cat: &Catalog, p: &Process) -> Option<usize> {
    let program = p.argv.first().map_or(p.name.as_str(), String::as_str);
    match runtime(&normalise(program)) {
        Some(rt) => wrapped(cat, rt, &p.argv).map(|(a, _)| a),
        None => {
            names_agent(cat, program).or_else(|| wrapped(cat, runtime(&normalise(&p.name))?, &p.argv).map(|(a, _)| a))
        }
    }
}

/// The agent one process is (§8.3).
pub fn process(cat: &Catalog, p: &Process) -> Option<usize> {
    let program = p.argv.first().map_or(p.name.as_str(), String::as_str);
    let args = p.argv.get(1..).unwrap_or_default();
    let (agent, args) = match runtime(&normalise(program)) {
        Some(rt) => wrapped(cat, rt, &p.argv)?,
        None => match names_agent(cat, program) {
            Some(agent) => (agent, args),
            // `exec -a <name> node script.js`: argv[0] is the wrapper's name, comm is the runtime.
            None => wrapped(cat, runtime(&normalise(&p.name))?, &p.argv)?,
        },
    };
    (!is_batch(&cat.agents[agent], args)).then_some(agent)
}

/// The agent of a foreground job: the group leader's, else the lowest pid's (§8.4).
pub fn job(cat: &Catalog, job: &Job) -> Option<usize> {
    let leader = job
        .processes
        .iter()
        .find(|p| p.pid == job.pgid)
        .and_then(|p| process(cat, p));
    leader.or_else(|| {
        let mut found: Vec<(u32, usize)> = job
            .processes
            .iter()
            .filter_map(|p| Some((p.pid, process(cat, p)?)))
            .collect();
        found.sort_unstable();
        found.first().map(|&(_, a)| a)
    })
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))] // the tests themselves are not product code
mod tests {
    use super::*;

    fn cat() -> Catalog {
        Catalog::load(None)
    }

    fn id(c: &Catalog, i: Option<usize>) -> Option<&str> {
        i.map(|i| c.agents[i].id.as_str())
    }

    fn p(pid: u32, name: &str, argv: &[&str]) -> Process {
        Process {
            pid,
            name: name.into(),
            argv: argv.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn who(argv: &[&str]) -> Option<String> {
        let c = cat();
        let name = argv.first().map_or("x", |a| a.rsplit('/').next().unwrap());
        id(&c, process(&c, &p(1, name, argv))).map(str::to_owned)
    }

    #[test]
    fn normalise_names() {
        assert_eq!(normalise("  /usr/bin/Claude.js "), "claude");
        assert_eq!(normalise("dir/sub/"), "sub");
        assert_eq!(normalise(""), "");
        assert_eq!(normalise("///"), "");
        assert_eq!(normalise("a.mjs"), "a.mjs", "script endings other than .js stay");
        assert_eq!(normalise("C:/bin/Codex.EXE"), "codex");
        assert_eq!(normalise("cline.cmd"), "cline");
        assert_eq!(normalise("x.bat"), "x");
        assert_eq!(normalise("x.ps1"), "x");
    }

    #[test]
    fn lookup_ids_names_paths_and_versions() {
        let c = cat();
        for (name, want) in [
            ("claude", Some("claude")),
            ("Claude-Code", Some("claude")),
            ("/opt/bin/codex", Some("codex")),
            ("cursor-agent", Some("cursor")),
            ("kiro-cli", Some("kiro")),
            ("devin cli", Some("devin")),
            ("muse-bin-0.1.0-R708.1", Some("muse")),
            ("/x/muse-bin-2", Some("muse")),
            ("muse-bin-", None),
            ("muse-bin-x", None),
            ("muse-binary", None),
            ("vim", None),
            ("", None),
        ] {
            assert_eq!(id(&c, lookup(&c, name)), want, "{name}");
        }
    }

    #[test]
    fn runtimes() {
        for (b, rt) in [
            ("node", Some(Runtime::Js)),
            ("bun", Some(Runtime::Js)),
            ("sh", Some(Runtime::Shell)),
            ("bash", Some(Runtime::Shell)),
            ("zsh", Some(Runtime::Shell)),
            ("fish", Some(Runtime::Shell)),
            ("python", Some(Runtime::Python)),
            ("python3", Some(Runtime::Python)),
            ("python3.12", Some(Runtime::Python)),
            ("python3.", None),
            ("python3a", None),
            ("pythonx", None),
            ("deno", None),
            ("tmux", None),
        ] {
            assert_eq!(runtime(b), rt, "{b}");
        }
    }

    #[test]
    fn flags() {
        assert!(is_flag("-e", "-e"));
        assert!(is_flag("-ecode", "-e"));
        assert!(is_flag("--eval", "--eval"));
        assert!(is_flag("--eval=x", "--eval"));
        assert!(!is_flag("--evaluate", "--eval"));
        assert!(!is_flag("-x", "-e"));
    }

    fn operand(rt: Runtime, argv: &[&str]) -> Option<usize> {
        let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
        script_operand(rt, &argv)
    }

    #[test]
    fn script_operands() {
        assert_eq!(operand(Runtime::Js, &["node", "cli.js"]), Some(1));
        assert_eq!(operand(Runtime::Js, &["node", "--use-system-ca", "cli.js"]), Some(2));
        assert_eq!(operand(Runtime::Js, &["node", "-r", "dotenv", "cli.js"]), Some(3));
        assert_eq!(operand(Runtime::Js, &["node", "--", "cli.js"]), Some(2));
        assert_eq!(operand(Runtime::Js, &["node", "--"]), None);
        assert_eq!(operand(Runtime::Js, &["node", "-e", "codex"]), None);
        assert_eq!(operand(Runtime::Js, &["node", "--print=1", "codex"]), None);
        assert_eq!(operand(Runtime::Js, &["node", "--inspect"]), None);
        assert_eq!(operand(Runtime::Js, &["node"]), None);
        assert_eq!(operand(Runtime::Python, &["python3", "-m", "codex"]), None);
        assert_eq!(operand(Runtime::Python, &["python3", "-cimport x", "codex"]), None);
        assert_eq!(operand(Runtime::Python, &["python3", "-X", "dev", "hermes"]), Some(3));
        assert_eq!(operand(Runtime::Shell, &["bash", "-c", "codex"]), None);
        assert_eq!(operand(Runtime::Shell, &["bash", "-o", "posix", "./codex"]), Some(3));
        assert_eq!(
            operand(Runtime::Shell, &["bash", "-lc", "codex"]),
            Some(2),
            "-lc is not -c"
        );
    }

    #[test]
    fn script_suffixes() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert!(ends_with_script(
            "/a/node_modules/x/dist/cli.js",
            &s(&["node_modules", "x", "dist", "cli"])
        ));
        assert!(ends_with_script(
            "/a/NODE_MODULES/x/dist/CLI.MJS",
            &s(&["node_modules", "x", "dist", "cli"])
        ));
        assert!(!ends_with_script(
            "/a/node_modules/x/dist/cli.js/more",
            &s(&["node_modules", "x", "dist", "cli"])
        ));
        assert!(!ends_with_script("cli.js", &s(&["dist", "cli"])), "too short");
        assert!(ends_with_script(
            "/h/cursor-agent/versions/2026.1/index.js",
            &s(&["cursor-agent", "versions", "*", "index"])
        ));
        assert!(!ends_with_script(
            "/h/cursor-agent/other/2026.1/index.js",
            &s(&["cursor-agent", "versions", "*", "index"])
        ));
        assert!(!ends_with_script("/h/dist/index.js", &s(&["dist", "cli"])));
    }

    #[test]
    fn paths_name_agents_by_base_name_script_or_symlink() {
        let c = cat();
        assert_eq!(names_agent(&c, ""), None);
        assert_eq!(names_agent(&c, "-p"), None);
        assert_eq!(id(&c, names_agent(&c, "/usr/local/bin/codex")), Some("codex"));
        assert_eq!(
            id(
                &c,
                names_agent(&c, "/l/node_modules/@qwen-code/qwen-code/dist/index.js")
            ),
            Some("qwen")
        );
        assert_eq!(
            names_agent(&c, "unrelated"),
            None,
            "bare names are not resolved as files"
        );
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("versions").join("claude");
        std::fs::create_dir_all(real.parent().unwrap()).unwrap();
        std::fs::write(&real, "").unwrap();
        let link = dir.path().join("agent");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert_eq!(id(&c, names_agent(&c, link.to_str().unwrap())), Some("claude"));
        let other = dir.path().join("tool");
        std::os::unix::fs::symlink(dir.path().join("versions"), &other).unwrap();
        assert_eq!(names_agent(&c, other.to_str().unwrap()), None, "target is not an agent");
        assert_eq!(
            names_agent(&c, dir.path().join("gone").to_str().unwrap()),
            None,
            "missing file"
        );
        assert_eq!(names_agent(&c, "/"), None, "no file name");
        // A file name that is not UTF-8 names no agent. Built in memory: APFS (macOS)
        // refuses to create such a file (EILSEQ), so it cannot come from the disk there.
        use std::os::unix::ffi::OsStrExt;
        let nonutf8 = Path::new(std::ffi::OsStr::from_bytes(b"/opt/x\xff"));
        assert_eq!(named_by_file(&c, nonutf8), None, "file name is not UTF-8");
        assert_eq!(id(&c, named_by_file(&c, Path::new("/opt/v/claude"))), Some("claude"));
    }

    #[test]
    fn processes() {
        assert_eq!(who(&["/usr/bin/claude", "--resume"]).as_deref(), Some("claude"));
        assert_eq!(
            who(&["node", "/usr/lib/node_modules/@qwen-code/qwen-code/dist/index.js"]).as_deref(),
            Some("qwen")
        );
        assert_eq!(who(&["node", "/usr/lib/other/index.js"]), None);
        assert_eq!(who(&["node", "-e", "require('codex')"]), None);
        assert_eq!(
            who(&["python3.12", "/usr/bin/hermes", "chat"]).as_deref(),
            Some("hermes")
        );
        assert_eq!(who(&["bash", "./codex"]).as_deref(), Some("codex"));
        assert_eq!(who(&["bash", "-c", "codex"]), None);
        assert_eq!(who(&["vim", "codex"]), None);
        assert_eq!(
            who(&["bun", "/x/node_modules/@oh-my-pi/pi-coding-agent/dist/cli.js"]).as_deref(),
            Some("omp")
        );
        let c = cat();
        // exec -a: argv[0] is the wrapper name, comm the runtime.
        let cursor = p(
            5,
            "node",
            &[
                "/home/u/.local/bin/agent",
                "--use-system-ca",
                "/home/u/.local/share/cursor-agent/versions/2026.10.01-e373342/index.js",
            ],
        );
        assert_eq!(id(&c, process(&c, &cursor)), Some("cursor"));
        let cursor_other = p(5, "node", &["/home/u/.local/bin/agent", "/x/index.js"]);
        assert_eq!(process(&c, &cursor_other), None);
        // With argv, argv[0] is the name; comm only adds the runtime case above.
        assert_eq!(process(&c, &p(6, "codex", &[".codex-wrapped", "--yolo"])), None);
        assert_eq!(process(&c, &p(6, "zz", &["yy"])), None);
        // No argv: the short name.
        assert_eq!(id(&c, process(&c, &p(7, "gemini", &[]))), Some("gemini"));
        assert_eq!(
            process(&c, &p(7, "bash", &[])),
            None,
            "a runtime with no argv has no operand"
        );
    }

    #[test]
    fn batch_runs_are_not_the_interactive_agent() {
        let c = cat();
        let letta = |argv: &[&str]| id(&c, process(&c, &p(1, "letta", argv))).map(str::to_owned);
        assert_eq!(letta(&["letta"]).as_deref(), Some("letta"));
        assert_eq!(
            letta(&["letta", "--model", "x"]).as_deref(),
            Some("letta"),
            "first argument is an option"
        );
        assert_eq!(letta(&["letta", "--backend", "local"]).as_deref(), Some("letta"));
        assert_eq!(letta(&["letta", "--backend=local", "--new"]).as_deref(), Some("letta"));
        assert_eq!(letta(&["letta", "-p", "hi"]), None);
        assert_eq!(letta(&["letta", "--output-format=json"]), None);
        assert_eq!(letta(&["letta", "run"]), None, "subcommand");
        assert_eq!(letta(&["letta", "--backend", "local", "run"]), None);
        assert_eq!(letta(&["letta", "--backend=local", "run"]), None);
        assert_eq!(
            id(
                &c,
                process(
                    &c,
                    &p(
                        1,
                        "node",
                        &["node", "/x/node_modules/@letta-ai/letta-code/letta.js", "--json"]
                    )
                )
            ),
            None
        );
        assert_eq!(
            id(
                &c,
                process(
                    &c,
                    &p(1, "node", &["node", "/x/node_modules/@letta-ai/letta-code/letta.js"])
                )
            ),
            Some("letta")
        );
        // Agents without batch rules ignore their arguments.
        assert_eq!(who(&["claude", "-p", "hi"]).as_deref(), Some("claude"));
    }

    #[test]
    fn jobs_prefer_the_leader_then_the_lowest_pid() {
        let c = cat();
        let job = |pgid: u32, ps: Vec<Process>| super::job(&c, &Job { pgid, processes: ps });
        let codex = p(30, "codex", &["codex"]);
        let claude = p(20, "claude", &["claude"]);
        let shell = p(10, "bash", &["bash"]);
        assert_eq!(
            id(&c, job(30, vec![shell.clone(), claude.clone(), codex.clone()])),
            Some("codex"),
            "leader"
        );
        assert_eq!(
            id(&c, job(10, vec![shell.clone(), codex.clone(), claude.clone()])),
            Some("claude"),
            "lowest pid"
        );
        assert_eq!(
            id(&c, job(99, vec![codex.clone()])),
            Some("codex"),
            "leader not in the list"
        );
        assert_eq!(job(10, vec![shell]), None);
        assert_eq!(job(10, vec![]), None);
    }
}
