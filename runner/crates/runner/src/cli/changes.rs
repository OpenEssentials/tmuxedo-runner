//! `tmuxedo changes <machine>[:<pane>] [--diff PATH] [--json]`: what a
//! session changed in its folder, from anywhere: the files with their counts
//! and a totals line, the same counts the web's and the app's Changes chip
//! show; `--diff PATH` prints one file's diff. Read only: committing,
//! pushing and discarding are on the web and in the app.
//!
//! Through the REST twin of the runner's `git.status` / `git.diff`
//! (`GET /api/v1/machines/{id}/review/{status,diff}`, the CLI token). With
//! no pane named, the machine's focused pane (else its first) is read, as
//! `tmuxedo attach` picks one.
//!
//! Everything the machine answers is hostile until decoded here: a wrong
//! type drops the row, the list is capped, and every name and line reaches
//! the terminal through [`safe`]. A refusal is web's sentence, made inert.

use super::tui::browse::safe;
use super::{api, api_error, load_cli_config, machines, parse_args, resolve_machine};
use serde_json::{json, Value};

const HELP: &str = "tmuxedo changes <machine>[:<pane>] [--diff PATH] [--json]
  what a session changed in its folder: files and counts (read only)
  --diff PATH   one file's diff
  --json        the answer as JSON";

/// The most files listed (the runner caps its own list too).
pub const FILES_MAX: usize = 1_000;

fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// `box` → (`box`, None); `box:w1:p1` → (`box`, Some(`w1:p1`)).
pub fn split_target(t: &str) -> (String, Option<String>) {
    match t.split_once(':') {
        Some((m, p)) if !p.is_empty() => (m.to_string(), Some(p.to_string())),
        Some((m, _)) => (m.to_string(), None),
        None => (t.to_string(), None),
    }
}

/// The focused pane of a `pane.list` answer, else the first.
pub fn pick_pane(list: &Value) -> Option<String> {
    let panes = list["result"]["panes"].as_array().or(list["panes"].as_array())?;
    let id = |p: &Value| p["pane_id"].as_str().filter(|s| !s.is_empty()).map(str::to_owned);
    panes
        .iter()
        .find(|p| p["focused"] == json!(true))
        .and_then(id)
        .or_else(|| panes.iter().find_map(id))
}

/// `GET /api/v1/machines/{id}/review/{op}?pane=…[&path=…]`.
pub fn path(machine_id: &str, op: &str, pane: &str, file: Option<&str>) -> String {
    let mut p = format!("/api/v1/machines/{}/review/{op}?pane={}", enc(machine_id), enc(pane));
    if let Some(f) = file {
        p.push_str(&format!("&path={}", enc(f)));
    }
    p
}

fn nat(v: &Value) -> Option<u64> {
    v.as_u64().filter(|n| *n <= 1_000_000_000)
}

fn plural(n: u64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// The status answer as lines: the files, then the totals.
pub fn render_status(v: &Value, machine: &str, pane: &str) -> Result<String, String> {
    let r = &v["result"];
    if r["type"] != json!("git_status") {
        return Err("Couldn't read the changes from this machine.".into());
    }
    let Some(files) = r["files"].as_array() else {
        return Err("Couldn't read the changes from this machine.".into());
    };
    let mut rows = vec![];
    for f in files.iter().take(FILES_MAX) {
        let (Some(p), Some(st)) = (f["path"].as_str(), f["status"].as_str()) else {
            continue;
        };
        let letter = match st {
            "?" => "N",
            "M" | "A" | "D" | "U" | "T" => st,
            _ => continue,
        };
        let counts = if f["binary"] == json!(true) {
            "binary".to_string()
        } else {
            match (nat(&f["add"]), nat(&f["del"])) {
                (None, None) => String::new(),
                (a, d) => format!("+{} −{}", a.unwrap_or(0), d.unwrap_or(0)),
            }
        };
        rows.push((letter, safe(p), counts));
    }
    let mut out = String::new();
    let repo = r["repo"].as_str().map(safe).unwrap_or_default();
    let branch = r["branch"]
        .as_str()
        .map(|b| format!(" on {}", safe(b)))
        .unwrap_or_default();
    out.push_str(&format!(
        "Changes on {} › {}{}{}\n",
        safe(machine),
        safe(pane),
        if repo.is_empty() {
            String::new()
        } else {
            format!(" · {repo}")
        },
        branch
    ));
    if rows.is_empty() {
        out.push_str("No changes yet.\n");
    } else {
        let w = rows
            .iter()
            .map(|(_, p, _)| p.chars().count())
            .max()
            .unwrap_or(0)
            .min(60);
        for (l, p, c) in &rows {
            let line = format!("  {l}  {p:<w$}  {c}");
            out.push_str(line.trim_end());
            out.push('\n');
        }
        let (add, del) = (nat(&r["add"]).unwrap_or(0), nat(&r["del"]).unwrap_or(0));
        out.push_str(&format!(
            "{} changed, {} added, {del} removed\n",
            plural(rows.len() as u64, "file", "files"),
            plural(add, "line", "lines")
        ));
    }
    if let Some(h) = nat(&r["hidden"]).filter(|h| *h > 0) {
        out.push_str(&format!("{h} more that may hold secrets, not named here.\n"));
    }
    if r["truncated"] == json!(true) {
        out.push_str("More files changed than shown here.\n");
    }
    Ok(out)
}

/// The diff answer as lines (the patch, or why there is none).
pub fn render_diff(v: &Value, file: &str) -> Result<String, String> {
    let r = &v["result"];
    if r["type"] != json!("git_diff") || r["path"].as_str() != Some(file) {
        return Err("Couldn't read the changes from this machine.".into());
    }
    if r["binary"] == json!(true) {
        return Ok("A binary file: no diff to show.\n".into());
    }
    let Some(patch) = r["patch"].as_str() else {
        return Ok("Too large to show here.\n".into());
    };
    let mut out: String = patch.lines().map(|l| format!("{}\n", safe(l))).collect();
    if r["truncated"] == json!(true) {
        out.push_str("(cut short: the rest is too large to show here)\n");
    }
    Ok(out)
}

/// A failed answer as one inert sentence.
pub fn refusal(code: u16, v: &Value) -> String {
    match code {
        401 => "Sign in first: tmuxedo login".into(),
        _ => safe(&api_error(v)),
    }
}

pub fn run(rest: &[String]) -> Result<(), String> {
    let a = parse_args(rest);
    let Some(target) = a.pos.first() else {
        return Err(HELP.into());
    };
    let cfg = load_cli_config()?;
    let (want, pane) = split_target(target);
    let m = resolve_machine(&machines(&cfg)?, &want)?;
    let mid = m["id"].as_str().ok_or("machine without id")?.to_string();
    let name = m["name"].as_str().unwrap_or(&mid).to_string();
    let pane = match pane {
        Some(p) => p,
        None => {
            let (code, v) = api(&cfg, "GET", &format!("/api/v1/machines/{}/panes", enc(&mid)), None)?;
            if code != 200 {
                return Err(refusal(code, &v));
            }
            pick_pane(&v).ok_or("This machine has no sessions open.")?
        }
    };
    let file = a.f("--diff").filter(|f| !f.is_empty());
    let op = if file.is_some() { "diff" } else { "status" };
    let (code, v) = api(&cfg, "GET", &path(&mid, op, &pane, file.as_deref()), None)?;
    if code != 200 {
        return Err(refusal(code, &v));
    }
    if a.has("--json") {
        println!("{}", serde_json::to_string_pretty(&v["result"]).unwrap_or_default());
        return Ok(());
    }
    let text = match &file {
        Some(f) => render_diff(&v, f)?,
        None => render_status(&v, &name, &pane)?,
    };
    print!("{text}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(files: Value) -> Value {
        json!({"result": {"type": "git_status", "repo": "site", "branch": "main", "files": files, "add": 16, "del": 3, "hidden": 0, "truncated": false}})
    }

    #[test]
    fn target_names_a_machine_and_maybe_a_pane() {
        assert_eq!(split_target("box"), ("box".into(), None));
        assert_eq!(split_target("box:w1:p2"), ("box".into(), Some("w1:p2".into())));
        assert_eq!(split_target("box:"), ("box".into(), None));
    }

    #[test]
    fn the_focused_pane_else_the_first() {
        let l = json!({"result": {"panes": [{"pane_id": "w1:p1", "focused": false}, {"pane_id": "w1:p2", "focused": true}]}});
        assert_eq!(pick_pane(&l).as_deref(), Some("w1:p2"));
        let l = json!({"result": {"panes": [{"pane_id": "w1:p1"}, {"pane_id": "w1:p2"}]}});
        assert_eq!(pick_pane(&l).as_deref(), Some("w1:p1"));
        assert_eq!(pick_pane(&json!({"result": {"panes": []}})), None);
        assert_eq!(pick_pane(&json!({"result": {"panes": [{"pane_id": 7}]}})), None);
    }

    #[test]
    fn paths_are_encoded() {
        assert_eq!(
            path("m1", "status", "w1:p1", None),
            "/api/v1/machines/m1/review/status?pane=w1%3Ap1"
        );
        assert_eq!(
            path("m 1", "diff", "w1:p1", Some("src/a b.rs")),
            "/api/v1/machines/m%201/review/diff?pane=w1%3Ap1&path=src%2Fa%20b.rs"
        );
    }

    #[test]
    fn files_counts_and_totals() {
        let v = status(json!([
            {"path": "src/app.rs", "status": "M", "add": 12, "del": 3},
            {"path": "notes.md", "status": "?", "add": 4, "del": 0},
            {"path": "logo.png", "status": "A", "binary": true, "add": null, "del": null},
        ]));
        let t = render_status(&v, "box", "w1:p1").unwrap();
        assert!(t.starts_with("Changes on box › w1:p1 · site on main\n"), "{t}");
        assert!(t.contains("  M  src/app.rs  +12 −3\n"), "{t}");
        assert!(t.contains("  N  notes.md    +4 −0\n"), "{t}");
        assert!(t.contains("  A  logo.png    binary\n"), "{t}");
        assert!(t.ends_with("3 files changed, 16 lines added, 3 removed\n"), "{t}");
    }

    #[test]
    fn nothing_changed_says_so() {
        let t = render_status(&status(json!([])), "box", "w1:p1").unwrap();
        assert!(t.ends_with("No changes yet.\n"), "{t}");
        assert!(!t.contains("changed,"));
    }

    #[test]
    fn hidden_and_cut_short_are_said_plainly() {
        let mut v = status(json!([{"path": "a", "status": "M", "add": 1, "del": 0}]));
        v["result"]["hidden"] = json!(2);
        v["result"]["add"] = json!(1);
        v["result"]["del"] = json!(0);
        v["result"]["truncated"] = json!(true);
        let t = render_status(&v, "box", "w1:p1").unwrap();
        assert!(t.contains("1 file changed, 1 line added, 0 removed\n"), "{t}");
        assert!(t.contains("2 more that may hold secrets, not named here.\n"));
        assert!(t.contains("More files changed than shown here.\n"));
    }

    #[test]
    fn hostile_answers_are_inert_or_refused() {
        let v = status(json!([
            {"path": "evil\u{1b}[2Jname", "status": "M", "add": 1, "del": 1},
            {"path": "x", "status": "Z"},
            {"status": "M"},
            "nope",
        ]));
        let t = render_status(&v, "box", "w1:p1").unwrap();
        assert!(!t.contains('\u{1b}'));
        assert!(t.contains("1 file changed"), "{t}");
        assert!(render_status(&json!({"result": {"type": "other"}}), "box", "p").is_err());
        assert!(render_status(&json!({"result": {"type": "git_status"}}), "box", "p").is_err());
    }

    #[test]
    fn a_diff_is_its_lines_made_inert() {
        let v = json!({"result": {"type": "git_diff", "path": "a.rs", "patch": "@@ -1 +1 @@\n-old\n+new\u{1b}]8;;x\u{7}", "truncated": true}});
        let t = render_diff(&v, "a.rs").unwrap();
        assert!(t.starts_with("@@ -1 +1 @@\n-old\n+new"), "{t}");
        assert!(!t.contains('\u{1b}') && !t.contains('\u{7}'));
        assert!(t.ends_with("(cut short: the rest is too large to show here)\n"));
        let bin = json!({"result": {"type": "git_diff", "path": "a.png", "binary": true}});
        assert_eq!(render_diff(&bin, "a.png").unwrap(), "A binary file: no diff to show.\n");
        assert!(render_diff(&v, "other.rs").is_err());
    }

    #[test]
    fn refusals_are_one_inert_sentence() {
        let v = json!({"error": {"code": "not_a_repo", "message": "This folder isn't a git repository: changes need git.\u{1b}[31m"}});
        assert_eq!(
            refusal(409, &v),
            "This folder isn't a git repository: changes need git.�[31m"
        );
        assert_eq!(refusal(401, &json!({})), "Sign in first: tmuxedo login");
    }
}
