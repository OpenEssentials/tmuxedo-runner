//! Opening web links on the user's device (docs/protocol.md "Open-URL
//! requests"). Panes get `BROWSER` and PATH stand-ins for `xdg-open`,
//! `open`, `sensible-browser`, `x-www-browser` and `www-browser`
//! (`inject::OPENERS`), which run `tmuxedo-runner open-url --program <name>`:
//! an http(s) URL goes to the runner over the local socket (a short
//! timeout; it always exits 0 at once, so the caller — e.g. Claude's
//! /login — carries on with its own fallback); anything else (a file, a
//! directory, `open -a …`) goes to the real program further down PATH.
//! The runner validates the URL and emits a non-durable `open_url` event.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const MAX_URL: usize = 8 * 1024;

/// http(s) only, at most 8 KiB, no whitespace or control characters.
pub fn valid_url(u: &str) -> bool {
    let l = u.to_ascii_lowercase();
    (l.starts_with("http://") || l.starts_with("https://"))
        && u.len() > l.find("://").unwrap_or(0) + 3
        && u.len() <= MAX_URL
        && !u.chars().any(|c| c.is_whitespace() || c.is_control())
}

fn send(socket: &Path, pane: Option<String>, url: &str, program: &str) -> Option<Value> {
    let mut s = UnixStream::connect(socket).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    s.set_write_timeout(Some(Duration::from_secs(2))).ok()?;
    let req = json!({"id": "open", "method": "pane.open_url",
        "params": {"pane_id": pane, "url": url, "program": program}});
    let mut line = serde_json::to_vec(&req).ok()?;
    line.push(b'\n');
    s.write_all(&line).ok()?;
    let mut resp = String::new();
    BufReader::new(s).read_line(&mut resp).ok()?;
    serde_json::from_str(&resp).ok()
}

/// The real `name` on PATH, skipping the shim directory.
fn real_program(name: &str, shim_dir: Option<&Path>) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let shim = shim_dir.and_then(|d| std::fs::canonicalize(d).ok());
    std::env::split_paths(&path).find_map(|d| {
        if d.as_os_str().is_empty() || std::fs::canonicalize(&d).ok() == shim && shim.is_some() {
            return None;
        }
        let p = d.join(name);
        let meta = std::fs::metadata(&p).ok()?;
        use std::os::unix::fs::PermissionsExt;
        (meta.is_file() && meta.permissions().mode() & 0o111 != 0).then_some(p)
    })
}

/// `tmuxedo-runner open-url --program <name> -- <args…>`.
pub fn run(program: &str, args: &[String]) -> i32 {
    let url = args.iter().find(|a| !a.starts_with('-'));
    if let Some(u) = url.filter(|u| valid_url(u)) {
        let socket = tmuxedo_holder::env_os("SOCKET_PATH").map(PathBuf::from);
        let pane = tmuxedo_holder::env_var("PANE_ID");
        match socket.and_then(|s| send(&s, pane, u, program)) {
            Some(v) if v.get("error").is_none() => eprintln!("tmuxedo: opening {u} on your device"),
            Some(v) => eprintln!(
                "tmuxedo: not opened: {}",
                v["error"]["message"].as_str().unwrap_or("refused")
            ),
            None => eprintln!("tmuxedo: the runner is not reachable; open {u} yourself"),
        }
        return 0;
    }
    let shim = tmuxedo_holder::env_os("SHIM_DIR").map(PathBuf::from);
    match real_program(program, shim.as_deref()) {
        Some(p) => {
            use std::os::unix::process::CommandExt;
            let e = std::process::Command::new(p).args(args).exec();
            eprintln!("tmuxedo: {program}: {e}");
            126
        }
        None => {
            eprintln!("tmuxedo: {program}: only web links can be opened from a remote pane");
            1
        }
    }
}

/// Open `url` with this machine's opener (the user's side: the TUI and
/// `attach --open-urls`), detached; only validated http(s) URLs.
pub fn open_locally(url: &str) -> Result<(), String> {
    if !valid_url(url) {
        return Err("not an http(s) URL".into());
    }
    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    std::process::Command::new(opener)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("{opener}: {e}"))
}

/// An OSC 8 hyperlink to `url` (most terminals make it clickable).
pub fn osc8(url: &str) -> String {
    format!("\x1b]8;;{url}\x1b\\{url}\x1b]8;;\x1b\\")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_http_urls() {
        assert!(valid_url("https://claude.ai/oauth/authorize?code=x&state=y"));
        assert!(valid_url("HTTP://example.com"));
        assert!(!valid_url("file:///etc/passwd"));
        assert!(!valid_url("javascript:alert(1)"));
        assert!(!valid_url("https://"));
        assert!(!valid_url("https://a b"));
        assert!(!valid_url(&format!("https://x/{}", "a".repeat(MAX_URL))));
    }
}
