//! What an agent made, for the chat's Files panel (docs/design/agent-chat.md
//! §7, docs/protocol.md "Agent chat"): `chat.file` reads one file of a
//! chat's work folder (Preview · Raw · Changes), and the turn-end scan
//! reports files a shell command wrote (no tool event names them).
//!
//! CONFINEMENT. A path is relative to the chat's work folder (or absolute
//! inside it). [`relative`] makes it relative by names alone and refuses
//! `..`; the file is then opened by the jail ([`crate::jail`]), which walks
//! from the folder's open handle and never follows a link, so there is no
//! gap between a check and the open. Browsing outside the work folder is
//! not offered (design D-C11).
//!
//! SECRETS. Environment and credential files are refused even when the
//! agent wrote them (`.env*`, keys, `.netrc`, cloud and SSH credential
//! folders, `.git/` internals): the panel lists them as hidden. The rules
//! are [`crate::jail::secret_reason`] and `secretFileReason` in web
//! (`web/src/lib/chat-files.ts`), both held to
//! `packages/protocol/fixtures/secret-files.json`.
//!
//! LIMITS. Text up to 2 MiB (the rest is cut, `truncated`), masked like
//! every chat string; images up to 2 MiB as base64 (rendered by the client,
//! never parsed here); anything else is described (name, size) only.

use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

/// Text shown at most (bytes).
pub const MAX_TEXT: usize = 2 * 1024 * 1024;
/// Images previewed at most (bytes).
pub const MAX_IMAGE: u64 = 2 * 1024 * 1024;
/// A `git diff` shown at most (bytes).
const MAX_PATCH: usize = 512 * 1024;
/// Entries the turn-end scan looks at, at most.
const MAX_SCAN: usize = 2000;

/// A best-effort MIME type from the file name (the client renders by it).
pub fn mime_of(name: &str) -> &'static str {
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "md" | "markdown" | "mdx" => "text/markdown",
        "txt" | "log" | "text" => "text/plain",
        "json" => "application/json",
        "html" | "htm" => "text/html",
        "csv" => "text/csv",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "pdf" => "application/pdf",
        _ => "text/plain",
    }
}

pub(crate) fn is_raster_image(mime: &str) -> bool {
    matches!(mime, "image/png" | "image/jpeg" | "image/gif" | "image/webp")
}

#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    Invalid,
    Outside,
    Secret(&'static str),
    NotFound,
    NotAFile,
    /// A link (never followed) on the way to the file, or the file itself.
    Link,
    /// A file with more than one name (a hard link).
    HardLink,
    Unreadable,
}

impl From<crate::jail::Refusal> for Refusal {
    fn from(r: crate::jail::Refusal) -> Self {
        use crate::jail::Refusal as J;
        match r {
            J::Invalid => Refusal::Invalid,
            J::Outside => Refusal::Outside,
            J::Symlink => Refusal::Link,
            J::Secret(why) => Refusal::Secret(why),
            J::Linked => Refusal::HardLink,
            J::NotFound | J::RootGone => Refusal::NotFound,
            J::NotAFile => Refusal::NotAFile,
            J::TooLarge { .. } | J::Unreadable(_) => Refusal::Unreadable,
        }
    }
}

impl Refusal {
    pub fn code(&self) -> (&'static str, String) {
        match self {
            Refusal::Invalid => ("invalid_path", "that is not a path in this chat's folder".into()),
            Refusal::Outside => (
                "file_refused",
                "Only files in this chat's work folder can be opened here.".into(),
            ),
            Refusal::Secret(why) => (
                "file_refused",
                format!("Hidden: this file may hold secrets ({why}). Open it in the terminal if you need it."),
            ),
            Refusal::NotFound => ("file_not_found", "That file is not there any more.".into()),
            Refusal::NotAFile => ("file_refused", "That is not a regular file.".into()),
            Refusal::Link => (
                "file_refused",
                "Links are not followed here. Open the file the link points to.".into(),
            ),
            Refusal::HardLink => (
                "file_refused",
                "This file has more than one name (a hard link), so it is not shown here. Open it in the terminal if you need it.".into(),
            ),
            Refusal::Unreadable => ("file_unreadable", "That file could not be opened.".into()),
        }
    }
}

/// The relative path of `path` (relative to `root`, or absolute) after
/// lexical checks: no `..`, no NUL, not empty, at most 4 KiB.
pub fn relative(root: &Path, path: &str) -> Result<PathBuf, Refusal> {
    if path.is_empty() || path.len() > 4096 || path.contains('\0') {
        return Err(Refusal::Invalid);
    }
    let p = Path::new(path);
    let rel = if p.is_absolute() {
        p.strip_prefix(root).map_err(|_| Refusal::Outside)?.to_path_buf()
    } else {
        p.to_path_buf()
    };
    let mut out = PathBuf::new();
    for c in rel.components() {
        match c {
            Component::Normal(x) => out.push(x),
            Component::CurDir => {}
            _ => return Err(Refusal::Outside),
        }
    }
    if out.as_os_str().is_empty() {
        return Err(Refusal::Invalid);
    }
    Ok(out)
}

/// The file's content for the panel: text (masked by the caller), a small
/// image as base64, or neither (binary / too big to preview).
pub struct Content {
    pub size: u64,
    pub mtime_ms: u64,
    pub mime: &'static str,
    pub text: Option<String>,
    pub data: Option<String>,
    pub truncated: bool,
    pub binary: bool,
}

pub fn read_content(mut f: std::fs::File, name: &str) -> std::io::Result<Content> {
    use base64::Engine;
    let meta = f.metadata()?;
    let size = meta.len();
    let mtime_ms = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_millis() as u64);
    let mime = mime_of(name);
    let mut c = Content {
        size,
        mtime_ms,
        mime,
        text: None,
        data: None,
        truncated: false,
        binary: false,
    };
    if is_raster_image(mime) {
        c.binary = true;
        if size <= MAX_IMAGE {
            let mut b = Vec::with_capacity(size as usize);
            f.take(MAX_IMAGE).read_to_end(&mut b)?;
            c.data = Some(base64::engine::general_purpose::STANDARD.encode(&b));
        }
        return Ok(c);
    }
    let mut b = Vec::with_capacity(size.min(MAX_TEXT as u64) as usize);
    (&mut f).take(MAX_TEXT as u64).read_to_end(&mut b)?;
    if b.iter().take(8192).any(|x| *x == 0) {
        c.binary = true;
        return Ok(c);
    }
    c.truncated = size > MAX_TEXT as u64;
    // A cut may split a character: drop the partial tail; anything else invalid is binary.
    match String::from_utf8(b) {
        Ok(s) => c.text = Some(s),
        Err(e) => {
            let valid = e.utf8_error().valid_up_to();
            let b = e.into_bytes();
            if c.truncated && b.len() - valid < 4 {
                c.text = Some(String::from_utf8_lossy(&b[..valid]).into_owned());
            } else {
                c.binary = true;
            }
        }
    }
    Ok(c)
}

/// `git diff HEAD -- <rel>` in `root` (a tracked file's changes), or None
/// (not a repo, not tracked, no change, git missing or slow).
pub async fn git_patch(root: &Path, rel: &Path) -> Option<String> {
    let out = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["diff", "--no-color", "--no-ext-diff", "HEAD", "--"])
            .arg(rel)
            .env("GIT_TERMINAL_PROMPT", "0")
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    if !out.status.success() || out.stdout.is_empty() {
        return None;
    }
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    if s.len() > MAX_PATCH {
        s = super::claude::cut(&s, MAX_PATCH);
        s.push_str("\n… (cut: open it in the terminal for the rest)\n");
    }
    Some(s)
}

/// One entry of the turn-end scan: path (relative), and what we saw.
pub type Snapshot = BTreeMap<String, (bool, u64, u64)>;

/// Files git reports as untracked or changed in `root`, with their size and
/// mtime (`git status --porcelain -z`), or None when `root` is not in a git
/// work tree (the scan is then skipped: tool events still report files).
pub async fn git_snapshot(root: &Path) -> Option<Snapshot> {
    let out = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args([
                "status",
                "--porcelain=v1",
                "-z",
                "--untracked-files=all",
                "--no-renames",
                ".",
            ])
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    if !out.status.success() {
        return None;
    }
    // Paths are relative to the repository's top; make them relative to root.
    let top = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["rev-parse", "--show-toplevel"])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    let top = PathBuf::from(String::from_utf8_lossy(&top.stdout).trim());
    let root_real = root.canonicalize().ok()?;
    let mut snap = Snapshot::new();
    for rec in out.stdout.split(|b| *b == 0).take(MAX_SCAN) {
        if rec.len() < 4 {
            continue;
        }
        let code = &rec[..2];
        let path = String::from_utf8_lossy(&rec[3..]).into_owned();
        let abs = top.join(&path);
        let Ok(rel) = abs.strip_prefix(&root_real) else {
            continue;
        };
        let created = code == b"??" || code[0] == b'A';
        let deleted = code.contains(&b'D');
        let (size, mtime) = if deleted {
            (0, 0)
        } else {
            std::fs::symlink_metadata(&abs)
                .ok()
                .map(|m| {
                    let t = m
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map_or(0, |d| d.as_millis() as u64);
                    (m.len(), t)
                })
                .unwrap_or((0, 0))
        };
        snap.insert(rel.to_string_lossy().into_owned(), (created, size, mtime));
    }
    Some(snap)
}

/// What changed between two snapshots (`before` taken at the turn's start):
/// `[(rel, created|modified|deleted)]`, minus what tool events already said.
pub fn snapshot_changes(before: &Snapshot, after: &Snapshot, said: &BTreeSet<String>) -> Vec<(String, &'static str)> {
    let mut out = vec![];
    for (p, (created, size, mtime)) in after {
        if said.contains(p) {
            continue;
        }
        let kind = match before.get(p) {
            None if *created => "created",
            None => "modified",
            Some((_, s0, m0)) if (s0, m0) != (size, mtime) => {
                if *size == 0 && *mtime == 0 {
                    "deleted"
                } else {
                    "modified"
                }
            }
            Some(_) => continue,
        };
        out.push((p.clone(), kind));
    }
    out
}

/// The `chat.file` answer.
pub fn answer(rel: &str, abs: &Path, c: Content, changes: Value) -> Value {
    json!({
        "type": "chat_file", "path": rel, "absPath": abs.to_string_lossy(), "mime": c.mime, "size": c.size,
        "mtime": c.mtime_ms, "text": c.text, "data": c.data, "truncated": c.truncated, "binary": c.binary,
        "changes": changes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_paths_stay_inside() {
        let root = Path::new("/w/proj");
        assert_eq!(relative(root, "docs/a.md").unwrap(), PathBuf::from("docs/a.md"));
        assert_eq!(relative(root, "/w/proj/docs/a.md").unwrap(), PathBuf::from("docs/a.md"));
        assert_eq!(relative(root, "./a.md").unwrap(), PathBuf::from("a.md"));
        assert_eq!(relative(root, "../x").unwrap_err(), Refusal::Outside);
        assert_eq!(relative(root, "docs/../../x").unwrap_err(), Refusal::Outside);
        assert_eq!(relative(root, "/etc/passwd").unwrap_err(), Refusal::Outside);
        assert_eq!(relative(root, "/w/projX/a").unwrap_err(), Refusal::Outside);
        assert_eq!(relative(root, "").unwrap_err(), Refusal::Invalid);
        assert_eq!(relative(root, "a\0b").unwrap_err(), Refusal::Invalid);
        assert_eq!(relative(root, ".").unwrap_err(), Refusal::Invalid);
    }

    /// `chat.file`'s way in: [`relative`], then the jail. The codes the panel knows are unchanged.
    #[test]
    fn the_jail_opens_chat_files_and_refuses_links_secrets_and_escapes() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("w");
        std::fs::create_dir_all(root.join("docs")).unwrap();
        std::fs::write(root.join("docs/a.md"), "# A\n").unwrap();
        std::fs::write(root.join(".env"), "KEY=1\n").unwrap();
        std::fs::write(d.path().join("outside.txt"), "secret\n").unwrap();
        std::os::unix::fs::symlink(d.path().join("outside.txt"), root.join("link.txt")).unwrap();
        std::os::unix::fs::symlink(d.path(), root.join("up")).unwrap();
        std::os::unix::fs::symlink(root.join(".env"), root.join("innocent.txt")).unwrap();
        std::os::unix::fs::symlink(root.join("docs"), root.join("alias")).unwrap();
        std::fs::hard_link(d.path().join("outside.txt"), root.join("hard.txt")).unwrap();
        let jail = crate::jail::Root::open_trusted(&root).unwrap();
        let open = |p: &str| -> Result<String, Refusal> {
            let rel = relative(&root, p)?;
            let o = jail.open_file(&rel.to_string_lossy(), Default::default())?;
            Ok(std::io::read_to_string(o.file).unwrap())
        };
        assert_eq!(open("docs/a.md").unwrap(), "# A\n");
        assert_eq!(open("./docs/a.md").unwrap(), "# A\n");
        assert_eq!(open(&root.join("docs/a.md").to_string_lossy()).unwrap(), "# A\n");
        assert_eq!(open(".env").unwrap_err(), Refusal::Secret("environment file"));
        assert_eq!(open("link.txt").unwrap_err(), Refusal::Link);
        assert_eq!(open("innocent.txt").unwrap_err(), Refusal::Link);
        assert_eq!(open("up/outside.txt").unwrap_err(), Refusal::Link);
        // A link to a folder INSIDE the work folder was followed before; links are never followed now.
        assert_eq!(open("alias/a.md").unwrap_err(), Refusal::Link);
        assert_eq!(open("hard.txt").unwrap_err(), Refusal::HardLink);
        assert_eq!(open("../outside.txt").unwrap_err(), Refusal::Outside);
        assert_eq!(open("docs").unwrap_err(), Refusal::NotAFile);
        assert_eq!(open("nope.md").unwrap_err(), Refusal::NotFound);
        for r in [Refusal::Link, Refusal::HardLink, Refusal::Outside, Refusal::NotAFile] {
            assert_eq!(r.code().0, "file_refused");
        }
    }

    #[test]
    fn content_text_binary_images_and_cuts() {
        let d = tempfile::tempdir().unwrap();
        let w = |n: &str, b: &[u8]| {
            std::fs::write(d.path().join(n), b).unwrap();
            read_content(std::fs::File::open(d.path().join(n)).unwrap(), n).unwrap()
        };
        let t = w("a.md", b"# Hi\n");
        assert_eq!(
            (t.mime, t.text.as_deref(), t.binary),
            ("text/markdown", Some("# Hi\n"), false)
        );
        let b = w("x.bin", b"ab\0cd");
        assert!(b.binary && b.text.is_none());
        let i = w("p.png", b"\x89PNG\r\n");
        assert!(i.binary && i.data.is_some());
        let mut big = "é".repeat(MAX_TEXT / 2 + 10).into_bytes();
        big.insert(0, b'x');
        let c = w("big.txt", &big);
        assert!(c.truncated && c.text.as_ref().unwrap().len() <= MAX_TEXT);
    }

    #[test]
    fn snapshot_diffs() {
        let mut before = Snapshot::new();
        before.insert("old.md".into(), (false, 10, 1));
        before.insert("same.md".into(), (true, 5, 2));
        let mut after = before.clone();
        after.insert("old.md".into(), (false, 12, 3));
        after.insert("new.md".into(), (true, 4, 4));
        after.insert("tool.md".into(), (true, 4, 4));
        let said: BTreeSet<String> = ["tool.md".to_string()].into();
        let ch = snapshot_changes(&before, &after, &said);
        assert_eq!(
            ch,
            vec![("new.md".to_string(), "created"), ("old.md".to_string(), "modified")]
        );
    }
}
