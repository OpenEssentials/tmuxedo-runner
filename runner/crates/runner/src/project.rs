//! Project scope for environment variables (docs/design/secrets-and-env.md
//! §2.1, D-7): a pane's project is its folder's git repository, keyed by
//! the normalised `origin` remote (`github.com/org/repo`), so it follows the
//! repository across machines and worktrees. A folder outside any repository
//! can be named by path on one machine (`path:<machineId>:<abs>`).
//!
//! The key is read from the repository's own config file (no `git` process:
//! this runs on every `cd` a shell reports), following `.git` files of
//! worktrees and submodules to the common config.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

/// `git@github.com:Org/Repo.git`, `https://user@github.com/org/repo/`,
/// `ssh://git@github.com:22/org/repo.git` → `github.com/org/repo` (web's
/// server/secrets/vault.ts normaliseProject does the same).
pub fn normalise(remote: &str) -> String {
    let mut r = remote.trim().to_string();
    if r.starts_with("path:") {
        return r;
    }
    if let Some(i) = r.find("://") {
        if r[..i].chars().all(|c| c.is_ascii_alphabetic() || c == '+') {
            r = r[i + 3..].to_string();
        }
    }
    if let Some(at) = r.find('@') {
        if !r[..at].contains('/') {
            r = r[at + 1..].to_string();
        }
    }
    // host:port/path or host:path (scp-like)
    if let Some(colon) = r.find(':') {
        let (host, rest) = (&r[..colon], &r[colon + 1..]);
        if !host.contains('/') {
            let digits = rest.chars().take_while(|c| c.is_ascii_digit()).count();
            let rest = if digits > 0 && rest[digits..].starts_with('/') {
                &rest[digits + 1..]
            } else {
                rest
            };
            r = format!("{host}/{rest}");
        }
    }
    let r = r.trim_end_matches('/');
    let r = r.strip_suffix(".git").unwrap_or(r);
    r.trim_end_matches('/').to_ascii_lowercase()
}

/// The repository root and `origin` URL for `cwd`, if it is inside one.
fn origin_of(cwd: &Path) -> Option<(PathBuf, String)> {
    let mut dir = Some(cwd);
    while let Some(d) = dir {
        let dotgit = d.join(".git");
        if let Ok(meta) = std::fs::metadata(&dotgit) {
            let gitdir = if meta.is_dir() {
                dotgit
            } else {
                // A worktree or submodule: "gitdir: <path>", and its commondir.
                let text = std::fs::read_to_string(&dotgit).ok()?;
                let p = PathBuf::from(text.trim().strip_prefix("gitdir:")?.trim());
                let p = if p.is_absolute() { p } else { d.join(p) };
                match std::fs::read_to_string(p.join("commondir")) {
                    Ok(c) => {
                        let c = PathBuf::from(c.trim());
                        if c.is_absolute() {
                            c
                        } else {
                            p.join(c)
                        }
                    }
                    Err(_) => p,
                }
            };
            let config = std::fs::read_to_string(gitdir.join("config")).ok()?;
            return origin_in(&config).map(|u| (d.to_path_buf(), u));
        }
        dir = d.parent();
    }
    None
}

/// `[remote "origin"] url = …` from a git config file.
fn origin_in(config: &str) -> Option<String> {
    let mut inside = false;
    for line in config.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            let h = t.trim_start_matches('[').trim_end_matches(']').replace(' ', "");
            inside = h == "remote\"origin\"";
            continue;
        }
        if inside {
            if let Some((k, v)) = t.split_once('=') {
                if k.trim().eq_ignore_ascii_case("url") {
                    return Some(v.trim().trim_matches('"').to_string());
                }
            }
        }
    }
    None
}

/// Project env from `config.projects`: key → (NAME → value, or None = unset).
pub type Projects = BTreeMap<String, BTreeMap<String, Option<String>>>;

#[derive(Default)]
pub struct Resolver {
    cache: std::sync::Mutex<HashMap<PathBuf, Option<String>>>,
}

impl Resolver {
    /// The configured project `cwd` is in: its git remote, else a path
    /// project of this machine that contains it (the longest).
    pub fn project_for(&self, cwd: &Path, machine: &str, projects: &Projects) -> Option<String> {
        if projects.is_empty() {
            return None;
        }
        let key = {
            let mut c = self.cache.lock().ok()?;
            if c.len() > 4096 {
                c.clear();
            }
            c.entry(cwd.to_path_buf())
                .or_insert_with(|| origin_of(cwd).map(|(_, u)| normalise(&u)))
                .clone()
        };
        if let Some(k) = key.filter(|k| projects.contains_key(k)) {
            return Some(k);
        }
        let prefix = format!("path:{machine}:");
        projects
            .keys()
            .filter_map(|k| Some((k, k.strip_prefix(&prefix)?)))
            .filter(|(_, p)| cwd == Path::new(p) || cwd.starts_with(p))
            .max_by_key(|(_, p)| p.len())
            .map(|(k, _)| k.clone())
    }

    /// Forget cached lookups (a repository's remote may have changed).
    pub fn clear(&mut self) {
        if let Ok(mut c) = self.cache.lock() {
            c.clear();
        }
    }
}

/// What a shell entering `cwd` should do: names to export (with values) and
/// names to unset, given the names the previous project set (`previous`)
/// and the machine's own env (`base`, restored when a project's value goes).
pub fn shell_delta(
    project: Option<&BTreeMap<String, Option<String>>>,
    previous: &[String],
    base: &BTreeMap<String, String>,
) -> (BTreeMap<String, String>, Vec<String>, Vec<String>) {
    let mut set = BTreeMap::new();
    let mut unset = vec![];
    let mut touched = vec![];
    if let Some(p) = project {
        for (k, v) in p {
            touched.push(k.clone());
            match v {
                Some(v) => {
                    set.insert(k.clone(), v.clone());
                }
                None => unset.push(k.clone()),
            }
        }
    }
    for k in previous {
        if project.is_some_and(|p| p.contains_key(k)) {
            continue;
        }
        match base.get(k) {
            Some(v) => {
                set.insert(k.clone(), v.clone());
            }
            None => unset.push(k.clone()),
        }
    }
    (set, unset, touched)
}

/// The shell hook appended to shell-init.sh when the user has project env:
/// on every directory change, ask the runner what to export and unset.
pub fn shell_hook(exe: &str) -> String {
    format!(
        r#"# tmuxedo: project env (variables for the git repository this shell is in)
__tmuxedo_project_env() {{
  [ "$PWD" = "${{__TMUXEDO_PROJECT_PWD:-}}" ] && return 0
  __TMUXEDO_PROJECT_PWD=$PWD
  eval "$({exe} cli project-env --shell 2>/dev/null)"
}}
if [ -n "${{ZSH_VERSION:-}}" ]; then
  autoload -Uz add-zsh-hook 2>/dev/null && add-zsh-hook chpwd __tmuxedo_project_env
  __tmuxedo_project_env
elif [ -n "${{BASH_VERSION:-}}" ]; then
  case ";${{PROMPT_COMMAND:-}};" in
    *";__tmuxedo_project_env;"*) ;;
    *) PROMPT_COMMAND="__tmuxedo_project_env${{PROMPT_COMMAND:+;$PROMPT_COMMAND}}" ;;
  esac
fi
"#,
        exe = crate::launch_config::sq(exe)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remotes_normalise_like_web() {
        for r in [
            "git@github.com:OpenEssentials/tmuxedo.git",
            "https://github.com/openessentials/tmuxedo",
            "ssh://git@github.com/OpenEssentials/tmuxedo.git",
            "https://user@github.com/openessentials/tmuxedo/",
            "ssh://git@github.com:22/openessentials/tmuxedo.git",
        ] {
            assert_eq!(normalise(r), "github.com/openessentials/tmuxedo", "{r}");
        }
        assert_eq!(normalise("path:m1:/home/x/proj"), "path:m1:/home/x/proj");
    }

    #[test]
    fn repositories_worktrees_and_path_projects() {
        let d = tempfile::tempdir().unwrap();
        let repo = d.path().join("shop");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("src/deep")).unwrap();
        std::fs::write(
            repo.join(".git/config"),
            "[core]\n\tbare = false\n[remote \"upstream\"]\n\turl = git@x.com:a/b\n[remote \"origin\"]\n\turl = git@github.com:Org/Shop.git\n",
        )
        .unwrap();
        // A worktree: .git is a file pointing into the main repository.
        let wt = d.path().join("shop-wt");
        std::fs::create_dir_all(repo.join(".git/worktrees/wt")).unwrap();
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(
            wt.join(".git"),
            format!("gitdir: {}\n", repo.join(".git/worktrees/wt").display()),
        )
        .unwrap();
        std::fs::write(repo.join(".git/worktrees/wt/commondir"), "../..\n").unwrap();
        let plain = d.path().join("notes/sub");
        std::fs::create_dir_all(&plain).unwrap();
        let mut projects = Projects::new();
        projects.insert(
            "github.com/org/shop".into(),
            BTreeMap::from([("NODE_ENV".into(), Some("dev".into()))]),
        );
        projects.insert(format!("path:m1:{}", d.path().join("notes").display()), BTreeMap::new());
        let r = Resolver::default();
        assert_eq!(
            r.project_for(&repo.join("src/deep"), "m1", &projects).as_deref(),
            Some("github.com/org/shop")
        );
        assert_eq!(
            r.project_for(&wt, "m1", &projects).as_deref(),
            Some("github.com/org/shop")
        );
        assert!(r.project_for(&plain, "m1", &projects).unwrap().starts_with("path:m1:"));
        assert_eq!(
            r.project_for(&plain, "m2", &projects),
            None,
            "path projects are per machine"
        );
        assert_eq!(r.project_for(d.path(), "m1", &projects), None);
        assert_eq!(r.project_for(&repo, "m1", &Projects::new()), None);
    }

    #[test]
    fn entering_and_leaving_projects() {
        let base = BTreeMap::from([
            ("EDITOR".to_string(), "vim".to_string()),
            ("LANG".to_string(), "C".to_string()),
        ]);
        let p = BTreeMap::from([
            ("EDITOR".to_string(), Some("nano".to_string())),
            ("LANG".to_string(), None),
            ("NODE_ENV".to_string(), Some("dev".to_string())),
        ]);
        let (set, unset, touched) = shell_delta(Some(&p), &[], &base);
        assert_eq!(
            set,
            BTreeMap::from([("EDITOR".into(), "nano".into()), ("NODE_ENV".into(), "dev".into())])
        );
        assert_eq!(unset, ["LANG"]);
        assert_eq!(touched, ["EDITOR", "LANG", "NODE_ENV"]);
        // Leaving: the machine's values come back, project-only names go.
        let (set, unset, touched) = shell_delta(None, &touched, &base);
        assert_eq!(
            set,
            BTreeMap::from([("EDITOR".into(), "vim".into()), ("LANG".into(), "C".into())])
        );
        assert_eq!(unset, ["NODE_ENV"]);
        assert!(touched.is_empty());
        assert!(shell_hook("/x/y z").contains("'/x/y z' cli project-env --shell"));
    }
}
