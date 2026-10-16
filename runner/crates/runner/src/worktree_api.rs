//! `worktree.{list,create,open,remove}` on the runner (Phase 7), with the
//! error codes (`not_git_worktree`, `linked_worktree_source`,
//! `worktree_not_found`, `ambiguous_worktree_branch`, `not_linked_worktree`,
//! `dirty_worktree_requires_force`, `worktree_create_failed`,
//! `worktree_remove_failed`, `invalid_request`) and lifecycle events
//! (`worktree_created`, `worktree_opened`, `worktree_removed`). Git runs
//! synchronously on the daemon task; these are short local commands.

use crate::api::{err, ApiErr, ApiResult};
use crate::daemon::Daemon;
use crate::events::Event;
use crate::worktree::{self, canonical, Entry, Membership};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// The repository a request works on and the workspace it came from.
struct Source {
    workspace: Option<String>,
    key: String,
    repo_name: String,
    repo_root: PathBuf,
}

fn opt_str<'a>(p: &'a Value, k: &str) -> Option<&'a str> {
    p.get(k).and_then(Value::as_str).filter(|v| !v.is_empty())
}

fn absolute(p: &str, home: &str) -> Result<PathBuf, ApiErr> {
    let path = worktree::expand_tilde(p, home);
    if !path.is_absolute() {
        return Err(err("invalid_request", format!("{p} must be an absolute path")));
    }
    Ok(path)
}

impl Daemon {
    pub(crate) async fn worktree_method(&mut self, method: &str, p: &Value) -> ApiResult {
        let trust = p["trust_repository"].as_bool() == Some(true);
        match method {
            "worktree.list" => self.worktree_list(p, trust),
            "worktree.create" => self.worktree_create(p, trust).await,
            "worktree.open" => self.worktree_open(p, trust).await,
            "worktree.remove" => self.worktree_remove(p, trust).await,
            m => Err(err("unknown_method", m)),
        }
    }

    fn membership_of(&self, ws: &str, trust: bool) -> Option<Membership> {
        let w = self.state.workspace(ws)?;
        if let Some(m) = &w.worktree {
            return Some(m.clone());
        }
        let sp = worktree::space(Path::new(&w.cwd), trust)?;
        Some(Membership {
            key: sp.key,
            repo_name: sp.repo_name,
            repo_root: sp.repo_root.display().to_string(),
            checkout_path: sp.repo_root.display().to_string(),
            is_linked_worktree: sp.is_linked_worktree,
        })
    }

    fn worktree_source(&self, p: &Value, trust: bool) -> Result<Source, ApiErr> {
        let ws = opt_str(p, "workspace_id");
        let cwd = opt_str(p, "cwd");
        if ws.is_some() && cwd.is_some() {
            return Err(err(
                "invalid_request",
                "only one of workspace_id or cwd may be supplied",
            ));
        }
        let linked = || {
            err(
                "linked_worktree_source",
                "New and open worktree actions start from the repo parent workspace.",
            )
        };
        if let Some(cwd) = cwd {
            let path = absolute(cwd, &self.opts.home)?;
            let sp = worktree::space(&path, trust).ok_or_else(|| {
                err(
                    "not_git_worktree",
                    "worktree actions require a path inside a Git work tree",
                )
            })?;
            if sp.is_linked_worktree {
                return Err(linked());
            }
            let parent = self
                .state
                .workspaces_sorted()
                .into_iter()
                .map(|w| w.id.clone())
                .find(|w| {
                    self.membership_of(w, trust)
                        .is_some_and(|m| m.key == sp.key && !m.is_linked_worktree)
                });
            return Ok(Source {
                workspace: parent,
                key: sp.key,
                repo_name: sp.repo_name,
                repo_root: sp.repo_root,
            });
        }
        let ws = match ws {
            Some(w) => {
                self.state
                    .workspace(w)
                    .ok_or_else(|| err("workspace_not_found", format!("workspace {w} not found")))?;
                w.to_string()
            }
            None => self.state.focused_workspace().map(|w| w.id.clone()).ok_or_else(|| {
                err(
                    "invalid_request",
                    "workspace_id or cwd is required when no workspace is active",
                )
            })?,
        };
        let m = self.membership_of(&ws, trust).ok_or_else(|| {
            err(
                "not_git_worktree",
                "worktree actions require a workspace inside a Git work tree",
            )
        })?;
        if m.is_linked_worktree {
            return Err(linked());
        }
        Ok(Source {
            workspace: Some(ws),
            key: m.key,
            repo_name: m.repo_name,
            repo_root: PathBuf::from(m.repo_root),
        })
    }

    /// The workspace showing `checkout` (explicit membership first, then cwd).
    fn workspace_for_checkout(&self, checkout: &Path) -> Option<String> {
        let want = canonical(checkout);
        let all = self.state.workspaces_sorted();
        all.iter()
            .find(|w| {
                w.worktree
                    .as_ref()
                    .is_some_and(|m| canonical(Path::new(&m.checkout_path)) == want)
            })
            .or_else(|| {
                all.iter()
                    .find(|w| w.worktree.is_none() && canonical(Path::new(&w.cwd)) == want)
            })
            .map(|w| w.id.clone())
    }

    fn worktree_info(&self, src: &Source, e: &Entry) -> Value {
        json!({
            "path": e.path.display().to_string(),
            "branch": e.branch,
            "is_bare": e.is_bare,
            "is_detached": e.is_detached,
            "is_prunable": e.is_prunable,
            "is_linked_worktree": canonical(&e.path) != canonical(&src.repo_root),
            "open_workspace_id": self.workspace_for_checkout(&e.path),
            "label": src.repo_name,
        })
    }

    fn worktree_list(&mut self, p: &Value, trust: bool) -> ApiResult {
        let src = self.worktree_source(p, trust)?;
        let entries = worktree::list(&src.repo_root, trust).map_err(|e| err("worktree_list_failed", e))?;
        let worktrees: Vec<Value> = entries.iter().map(|e| self.worktree_info(&src, e)).collect();
        Ok(json!({
            "type": "worktree_list",
            "source": {
                "repo_key": src.key,
                "repo_name": src.repo_name,
                "repo_root": src.repo_root.display().to_string(),
                "source_checkout_path": src.repo_root.display().to_string(),
                "source_workspace_id": src.workspace,
            },
            "worktrees": worktrees,
        }))
    }

    /// Open a workspace on `checkout` and mark its membership.
    async fn open_checkout_workspace(
        &mut self,
        src: &Source,
        checkout: &Path,
        linked: bool,
        label: Option<&str>,
        focus: bool,
    ) -> (String, String, String) {
        let cwd = checkout.display().to_string();
        let label = label
            .map(str::to_owned)
            .or_else(|| checkout.file_name().and_then(|n| n.to_str()).map(str::to_owned));
        let argv = self.default_argv();
        let (ws, tab, pane) = self.state.create_workspace(&cwd, label, argv, Default::default());
        if let Some(w) = self.state.workspace_mut(&ws) {
            w.worktree = Some(Membership {
                key: src.key.clone(),
                repo_name: src.repo_name.clone(),
                repo_root: src.repo_root.display().to_string(),
                checkout_path: canonical(checkout).display().to_string(),
                is_linked_worktree: linked,
            });
        }
        // The parent workspace learns its membership too.
        if let Some(parent) = &src.workspace {
            if let Some(w) = self.state.workspace_mut(parent) {
                if w.worktree.is_none() {
                    w.worktree = Some(Membership {
                        key: src.key.clone(),
                        repo_name: src.repo_name.clone(),
                        repo_root: src.repo_root.display().to_string(),
                        checkout_path: canonical(&src.repo_root).display().to_string(),
                        is_linked_worktree: false,
                    });
                }
            }
        }
        if focus {
            let _ = self.state.focus_pane(&pane);
        }
        self.spawn_pane(&pane, None);
        self.emit(Event::new(
            "workspace_created",
            json!({ "workspace": self.workspace_info(&ws) }),
        ));
        self.emit(Event::new("tab_created", json!({ "tab": self.tab_info(&tab) })));
        self.emit(Event::new("pane_created", json!({ "pane": self.pane_info(&pane) })));
        (ws, tab, pane)
    }

    fn entry_for(&self, checkout: &Path, trust: bool, repo_root: &Path) -> Entry {
        let want = canonical(checkout);
        worktree::list(repo_root, trust)
            .ok()
            .and_then(|v| v.into_iter().find(|e| canonical(&e.path) == want))
            .unwrap_or(Entry {
                path: checkout.to_path_buf(),
                branch: None,
                is_bare: false,
                is_detached: true,
                is_prunable: false,
            })
    }

    async fn worktree_create(&mut self, p: &Value, trust: bool) -> ApiResult {
        let branch = match p.get("branch").and_then(Value::as_str) {
            Some(b) => b.trim().to_string(),
            None => {
                let seed = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_micros() as u64)
                    .unwrap_or(0);
                worktree::generated_branch(seed)
            }
        };
        if branch.is_empty() {
            return Err(err("invalid_request", "branch is required"));
        }
        let base = opt_str(p, "base").unwrap_or("HEAD").to_string();
        let src = self.worktree_source(p, trust)?;
        let checkout = match opt_str(p, "path") {
            Some(path) => absolute(path, &self.opts.home)?,
            None => worktree::default_checkout_path(&self.opts.worktree_dir, &src.repo_name, &branch),
        };
        worktree::add(&src.repo_root, &checkout, &branch, &base, trust)
            .map_err(|e| err("worktree_create_failed", e))?;
        let focus = p["focus"].as_bool() == Some(true);
        let (ws, tab, pane) = self
            .open_checkout_workspace(&src, &checkout, true, opt_str(p, "label"), focus)
            .await;
        let info = self.worktree_info(&src, &self.entry_for(&checkout, trust, &src.repo_root));
        self.emit(Event::new(
            "worktree_created",
            json!({ "workspace": self.workspace_info(&ws), "worktree": info }),
        ));
        self.commit_topology().await;
        Ok(json!({
            "type": "worktree_created",
            "workspace": self.workspace_info(&ws),
            "tab": self.tab_info(&tab),
            "root_pane": self.pane_info(&pane),
            "worktree": info,
        }))
    }

    async fn worktree_open(&mut self, p: &Value, trust: bool) -> ApiResult {
        let src = self.worktree_source(p, trust)?;
        let entries = worktree::list(&src.repo_root, trust).map_err(|e| err("worktree_open_failed", e))?;
        let entry = if let Some(path) = opt_str(p, "path") {
            let want = canonical(&absolute(path, &self.opts.home)?);
            entries
                .into_iter()
                .find(|e| canonical(&e.path) == want)
                .ok_or_else(|| err("worktree_not_found", "worktree path not found"))?
        } else if let Some(branch) = opt_str(p, "branch") {
            let mut m: Vec<Entry> = entries
                .into_iter()
                .filter(|e| !e.is_bare && !e.is_prunable && !e.is_detached && e.branch.as_deref() == Some(branch))
                .collect();
            match m.len() {
                0 => return Err(err("worktree_not_found", "worktree branch not found")),
                1 => m.remove(0),
                _ => return Err(err("ambiguous_worktree_branch", "multiple worktrees matched branch")),
            }
        } else {
            return Err(err("invalid_request", "exactly one of path or branch is required"));
        };
        if entry.is_bare || entry.is_prunable || !entry.path.is_dir() {
            return Err(err("worktree_not_found", "worktree cannot be opened"));
        }
        let focus = p["focus"].as_bool() == Some(true);
        let label = opt_str(p, "label");
        let linked = canonical(&entry.path) != canonical(&src.repo_root);
        let (ws, already_open) = match self.workspace_for_checkout(&entry.path) {
            Some(ws) => {
                if let Some(l) = label {
                    if let Some(w) = self.state.workspace_mut(&ws) {
                        w.name = l.to_string();
                    }
                    self.emit(Event::new(
                        "workspace_renamed",
                        json!({ "workspace_id": ws, "label": l }),
                    ));
                }
                if focus {
                    let _ = self.state.focus_workspace(&ws);
                }
                (ws, true)
            }
            None => {
                let (ws, _, _) = self
                    .open_checkout_workspace(&src, &entry.path, linked, label, focus)
                    .await;
                (ws, false)
            }
        };
        let tab = self
            .state
            .workspace(&ws)
            .map(|w| w.active_tab.clone())
            .unwrap_or_default();
        let pane = self.state.tab(&tab).map(|t| t.focused_pane.clone()).unwrap_or_default();
        let info = self.worktree_info(&src, &entry);
        self.emit(Event::new(
            "worktree_opened",
            json!({ "workspace": self.workspace_info(&ws), "worktree": info, "already_open": already_open }),
        ));
        self.commit_topology().await;
        Ok(json!({
            "type": "worktree_opened",
            "workspace": self.workspace_info(&ws),
            "tab": self.tab_info(&tab),
            "root_pane": self.pane_info(&pane),
            "worktree": info,
            "already_open": already_open,
        }))
    }

    async fn worktree_remove(&mut self, p: &Value, trust: bool) -> ApiResult {
        let ws = opt_str(p, "workspace_id").ok_or_else(|| err("invalid_params", "workspace_id is required"))?;
        let ws = ws.to_string();
        if self.state.workspace(&ws).is_none() {
            return Err(err("workspace_not_found", format!("workspace {ws} not found")));
        }
        let m = self
            .membership_of(&ws, trust)
            .filter(|m| m.is_linked_worktree)
            .ok_or_else(|| err("not_linked_worktree", "only a linked worktree workspace can be removed"))?;
        let force = p["force"].as_bool() == Some(true);
        let path = PathBuf::from(&m.checkout_path);
        if !force && worktree::is_dirty(&path, trust).unwrap_or(false) {
            return Err(err(
                "dirty_worktree_requires_force",
                format!(
                    "{} has modified or untracked files; use force to delete it",
                    path.display()
                ),
            ));
        }
        let src = Source {
            workspace: None,
            key: m.key.clone(),
            repo_name: m.repo_name.clone(),
            repo_root: PathBuf::from(&m.repo_root),
        };
        let info = self.worktree_info(&src, &self.entry_for(&path, trust, &src.repo_root));
        let workspace = self.workspace_info(&ws);
        worktree::remove(&src.repo_root, &path, force, trust).map_err(|e| err("worktree_remove_failed", e))?;
        let closed = self.state.close_workspace(&ws)?;
        self.kill_panes(&closed.panes);
        self.emit_closed(&closed);
        if self.state.workspaces.is_empty() {
            self.ensure_a_pane();
        }
        self.emit(Event::new(
            "worktree_removed",
            json!({ "workspace_id": ws, "workspace": workspace, "worktree": info, "forced": force }),
        ));
        self.commit_topology().await;
        Ok(json!({
            "type": "worktree_removed",
            "workspace_id": ws,
            "path": m.checkout_path,
            "forced": force,
        }))
    }
}
