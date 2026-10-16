//! The roots a browser may open (docs/design/file-browser.md §3.2): each is
//! a folder from the runner's own records, held as an open handle from the
//! moment it is registered. A client names a root by its id only.
//!
//! - A folder that is too broad ([`crate::jail::too_broad`]) is refused
//!   and reported as refused, not hidden.
//! - A root whose folder was moved or replaced is `gone` and stays gone:
//!   its handle is never reopened by path. It comes back only when its
//!   source (the workspace, the chat) goes away and returns.
//! - `workspace` and `project` roots are writable (design §3.2); the
//!   runner still refuses a change without web's flags.

use crate::api::{err, ApiErr};

/// The kinds of root the browser may change (design §3.2): a workspace's
/// folder and a project's. Anything else is read-only.
pub fn writable_kind(kind: &str) -> bool {
    matches!(kind, "workspace" | "project")
}
use crate::jail::{self, Guard};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::Arc;

/// Roots registered at most (more sources than this are not offered).
pub const ROOTS_MAX: usize = 64;

/// A folder the runner's records name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    /// `workspace` or `project`.
    pub kind: &'static str,
    pub label: String,
    pub path: PathBuf,
}

struct Entry {
    id: String,
    source: Source,
    root: Arc<jail::Root>,
}

struct Refused {
    source: Source,
    why: &'static str,
}

#[derive(Default)]
pub struct Roots {
    entries: Vec<Entry>,
    refused: Vec<Refused>,
}

fn label_of(s: &Source) -> String {
    let l: String = s.label.chars().filter(|c| !c.is_control()).take(80).collect();
    if l.trim().is_empty() {
        "Folder".into()
    } else {
        l
    }
}

fn id_of(real: &std::path::Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    format!("r{}", &hex::encode(Sha256::digest(real.as_os_str().as_bytes()))[..16])
}

impl Roots {
    /// Bring the registry in line with the runner's records. A source seen
    /// before keeps its handle (it is never opened twice); a new one is
    /// opened once, here; one that is gone from the records is dropped.
    pub fn sync(&mut self, sources: &[Source], guard: &Guard) {
        self.entries.retain(|e| sources.iter().any(|s| s.path == e.source.path));
        // An unreachable folder is tried again each time (a volume may appear); a too-broad one stays refused.
        self.refused
            .retain(|r| r.why == "too_broad" && sources.iter().any(|s| s.path == r.source.path));
        for s in sources {
            let known = self.entries.iter().any(|e| e.source.path == s.path)
                || self.refused.iter().any(|r| r.source.path == s.path);
            if known {
                continue;
            }
            let refuse = |why| Refused { source: s.clone(), why };
            if self.entries.len() >= ROOTS_MAX || !s.path.is_absolute() {
                continue;
            }
            match jail::Root::open_trusted(&s.path) {
                Err(_) => self.refused.push(refuse("unreachable")),
                Ok(root) if jail::too_broad(root.real(), guard) => self.refused.push(refuse("too_broad")),
                Ok(root) => {
                    let id = id_of(root.real());
                    // Two records naming one folder are one root.
                    if self.entries.iter().any(|e| e.id == id) {
                        continue;
                    }
                    self.entries.push(Entry {
                        id,
                        source: s.clone(),
                        root: Arc::new(root),
                    });
                }
            }
        }
        self.refused.truncate(ROOTS_MAX);
    }

    /// The handle of root `id` and whether it is writable, or `root_unknown` / `root_gone`.
    pub fn get(&self, id: &str) -> Result<(Arc<jail::Root>, bool), ApiErr> {
        let e = self
            .entries
            .iter()
            .find(|e| e.id == id)
            .ok_or_else(|| err("root_unknown", "No such folder on this machine."))?;
        if !e.root.is_current() {
            return Err(err("root_gone", "This folder was moved or replaced."));
        }
        Ok((e.root.clone(), writable_kind(e.source.kind)))
    }

    /// The id of the root registered for `path` (tests and logs).
    pub fn id_for(&self, path: &std::path::Path) -> Option<&str> {
        self.entries
            .iter()
            .find(|e| e.source.path == path)
            .map(|e| e.id.as_str())
    }

    /// The `fs_roots` answer. A root inside another names it (`place`).
    pub fn describe(&self) -> Value {
        let mut roots = Vec::new();
        let mut refused: Vec<Value> = self
            .refused
            .iter()
            .map(|r| json!({ "label": label_of(&r.source), "kind": r.source.kind, "why": r.why }))
            .collect();
        for e in &self.entries {
            if !e.root.is_current() {
                refused.push(json!({ "label": label_of(&e.source), "kind": e.source.kind, "why": "gone" }));
                continue;
            }
            let mut v = json!({ "id": e.id, "kind": e.source.kind, "label": label_of(&e.source), "writable": writable_kind(e.source.kind) });
            let outer = self
                .entries
                .iter()
                .filter(|o| o.id != e.id && o.root.is_current() && e.root.real().starts_with(o.root.real()))
                .min_by_key(|o| o.root.real().as_os_str().len());
            if let Some(o) = outer {
                if let Some(rel) = e.root.real().strip_prefix(o.root.real()).ok().and_then(|p| p.to_str()) {
                    v["place"] = json!({ "root": o.id, "path": rel });
                }
            }
            roots.push(v);
        }
        json!({
            "type": "fs_roots", "roots": roots, "refused": refused,
            "canWrite": true, "limits": super::limits(),
        })
    }
}
