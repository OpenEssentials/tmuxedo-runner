//! Memory watchdog. On the hosted Kubernetes nodes an OOM kills the whole
//! container, holder and all, so the runner stops the biggest consumer first:
//! when the cgroup holding the panes is above 90% of its `memory.max`, it
//! finds the pane whose SESSION uses the most memory and, inside it, the
//! process group using the most, sends that group SIGTERM, then SIGKILL, and
//! records an `event` of type `memory_stopped`.
//!
//! Sessions, not the shell's process group: every pane's shell is a session
//! leader (the holder setsid()s it), but an interactive shell runs each
//! foreground command (`claude`, `cargo build`, a runaway script) in a
//! process group of its own. Counting or signalling only the shell's group
//! (`kill(-shell_pid)`) misses the process actually using the memory: the
//! live Phase 5 gate (2026-10-03) OOM-killed the container exactly that way.
//! Stopping the biggest group leaves the shell, and every other pane, alive.
//!
//! Everything here is pure over a cgroup directory and a proc directory, so
//! tests run against fake trees.

use std::path::{Path, PathBuf};

pub const THRESHOLD: f64 = 0.90;
/// Where the warning starts (machine-hud.md §6): the next victim is named
/// before the watchdog acts (crate::memory_warn).
pub const WARN: f64 = 0.85;

/// `memory.current` and a numeric `memory.max` of one cgroup directory.
pub fn read_usage(dir: &Path) -> Option<(u64, u64)> {
    let max = std::fs::read_to_string(dir.join("memory.max")).ok()?;
    let max: u64 = max.trim().parse().ok()?; // "max" → None (no limit here)
    let cur: u64 = std::fs::read_to_string(dir.join("memory.current"))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    Some((cur, max))
}

/// The cgroup v2 path of `pid` relative to the hierarchy root (`0::/a/b` → `a/b`).
pub fn cgroup_of(proc_root: &Path, pid: u32) -> Option<PathBuf> {
    let text = std::fs::read_to_string(proc_root.join(pid.to_string()).join("cgroup")).ok()?;
    let line = text.lines().find(|l| l.starts_with("0::"))?;
    Some(PathBuf::from(line.trim_start_matches("0::").trim_start_matches('/')))
}

/// The nearest cgroup at or above `rel` that has a numeric memory limit.
pub fn limited_cgroup(cgroup_root: &Path, rel: &Path) -> Option<PathBuf> {
    let mut cur = Some(rel.to_path_buf());
    while let Some(r) = cur {
        let dir = cgroup_root.join(&r);
        if read_usage(&dir).is_some() {
            return Some(dir);
        }
        cur = if r.as_os_str().is_empty() {
            None
        } else {
            Some(r.parent().map(Path::to_path_buf).unwrap_or_default())
        };
    }
    None
}

pub fn over_threshold(current: u64, max: u64) -> bool {
    max > 0 && current as f64 > THRESHOLD * max as f64
}

/// `current` is at least `frac` of `max`.
pub fn at_least(current: u64, max: u64, frac: f64) -> bool {
    max > 0 && current as f64 >= frac * max as f64
}

/// The command name of process `pid` from its `stat` (`pid (name) …`): printable,
/// at most 32 characters; None when it is gone or unreadable.
pub fn command_of(proc_root: &Path, pid: u32) -> Option<String> {
    let stat = std::fs::read_to_string(proc_root.join(pid.to_string()).join("stat")).ok()?;
    let (_, rest) = stat.split_once('(')?;
    let (name, _) = rest.rsplit_once(')')?;
    let name: String = name.chars().filter(|c| !c.is_control()).take(32).collect();
    let name = name.trim().to_string();
    (!name.is_empty()).then_some(name)
}

/// (pid, pgrp, session, rss pages) of one process.
pub type ProcRow = (u32, u32, u32, u64);
/// A pane's (id, session id, process groups with their rss).
type PaneGroups = (String, u32, Vec<(u32, u64)>);

/// (pid, pgrp, session, rss pages) of every process.
fn scan(proc_root: &Path) -> Vec<ProcRow> {
    let Ok(rd) = std::fs::read_dir(proc_root) else {
        return vec![];
    };
    let mut out = vec![];
    for e in rd.flatten() {
        let name = e.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(stat) = std::fs::read_to_string(e.path().join("stat")) else {
            continue;
        };
        // Fields after the command's closing paren: state ppid pgrp ... rss is field 24.
        let Some(rest) = stat.rsplit_once(')').map(|(_, r)| r) else {
            continue;
        };
        let f: Vec<&str> = rest.split_whitespace().collect();
        let (Some(pg), Some(sid), Some(rss)) = (f.get(2), f.get(3), f.get(21)) else {
            continue;
        };
        let (Ok(pid), Ok(pg), Ok(sid)) = (name.parse::<u32>(), pg.parse::<u32>(), sid.parse::<u32>()) else {
            continue;
        };
        out.push((pid, pg, sid, rss.parse::<u64>().unwrap_or(0)));
    }
    out
}

/// Resident bytes of every process whose process group is `pgid`.
pub fn group_rss(proc_root: &Path, pgid: u32, page_size: u64) -> u64 {
    scan(proc_root)
        .iter()
        .filter(|p| p.1 == pgid)
        .map(|p| p.3 * page_size)
        .sum()
}

/// Resident bytes per process group inside session `sid` (a pane), largest first.
pub fn session_groups(procs: &[ProcRow], sid: u32, page_size: u64) -> Vec<(u32, u64)> {
    let mut by: std::collections::BTreeMap<u32, u64> = std::collections::BTreeMap::new();
    for (_, pg, s, rss) in procs {
        if *s == sid {
            *by.entry(*pg).or_default() += rss * page_size;
        }
    }
    let mut v: Vec<(u32, u64)> = by.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    v
}

/// The pane with the largest footprint, if any uses memory at all.
pub fn pick_victim(usages: &[(String, u64)]) -> Option<(String, u64)> {
    usages
        .iter()
        .filter(|(_, rss)| *rss > 0)
        .max_by_key(|(_, rss)| *rss)
        .cloned()
}

pub fn page_size() -> u64 {
    // SAFETY: sysconf is always safe to call.
    let p = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if p > 0 {
        p as u64
    } else {
        4096
    }
}

/// What one check decided to stop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Victim {
    pub pane: String,
    /// The process group to signal (the shell's own group only if it is the biggest).
    pub pgid: u32,
    /// Resident bytes of that group, and of the whole pane (session).
    pub group_rss: u64,
    pub pane_rss: u64,
    pub current: u64,
    pub max: u64,
    /// The command name of the group's leader (or of its largest process), for the warning.
    pub command: Option<String>,
}

/// One look at the limit: (current, max), and the pane and group the
/// watchdog would stop, found only at or above [`WARN`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assessment {
    pub current: u64,
    pub max: u64,
    pub victim: Option<Victim>,
}

/// The cgroup limiting the holder: (current, max), when it has a numeric limit.
pub fn usage_for(cgroup_root: &Path, proc_root: &Path, holder_pid: u32) -> Option<(u64, u64)> {
    let rel = cgroup_of(proc_root, holder_pid)?;
    read_usage(&limited_cgroup(cgroup_root, &rel)?)
}

/// One check: `panes` are (pane id, shell pid = session id). None below the
/// threshold (two small file reads; /proc is scanned only above it).
pub fn check(cgroup_root: &Path, proc_root: &Path, holder_pid: u32, panes: &[(String, u32)]) -> Option<Victim> {
    assess(cgroup_root, proc_root, holder_pid, panes)
        .filter(|a| over_threshold(a.current, a.max))
        .and_then(|a| a.victim)
}

/// The limit and, at or above [`WARN`], the would-be victim. None without a
/// numeric limit. Below [`WARN`] it reads only the two cgroup files.
pub fn assess(cgroup_root: &Path, proc_root: &Path, holder_pid: u32, panes: &[(String, u32)]) -> Option<Assessment> {
    let (cur, max) = usage_for(cgroup_root, proc_root, holder_pid)?;
    let mut out = Assessment {
        current: cur,
        max,
        victim: None,
    };
    if !at_least(cur, max, WARN) {
        return Some(out);
    }
    let ps = page_size();
    let procs = scan(proc_root);
    let per_pane: Vec<PaneGroups> = panes
        .iter()
        .map(|(id, sid)| (id.clone(), *sid, session_groups(&procs, *sid, ps)))
        .collect();
    let totals: Vec<(String, u64)> = per_pane
        .iter()
        .map(|(id, _, g)| (id.clone(), g.iter().map(|x| x.1).sum()))
        .collect();
    out.victim = (|| {
        let (pane, pane_rss) = pick_victim(&totals)?;
        let (_, sid, groups) = per_pane.into_iter().find(|(id, _, _)| *id == pane)?;
        let (pgid, group_rss) = groups.first().copied().unwrap_or((sid, pane_rss));
        // The leader names the job; when it has gone, the group's largest process.
        let command = command_of(proc_root, pgid).or_else(|| {
            procs
                .iter()
                .filter(|p| p.1 == pgid)
                .max_by_key(|p| p.3)
                .and_then(|p| command_of(proc_root, p.0))
        });
        Some(Victim {
            pane,
            pgid,
            group_rss,
            pane_rss,
            current: cur,
            max,
            command,
        })
    })();
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn stat(pid: u32, comm: &str, pgrp: u32, sid: u32, rss_pages: u64) -> String {
        // pid (comm) state ppid pgrp session tty tpgid flags minflt cminflt majflt cmajflt
        // utime stime cutime cstime priority nice threads itrealvalue starttime vsize rss ...
        format!("{pid} ({comm}) S 1 {pgrp} {sid} 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 100 1000 {rss_pages} 0 0")
    }

    /// procs: (pid, comm, pgrp, rss pages); the session is the pgrp unless given via fake_proc_sid.
    fn fake_proc(root: &Path, procs: &[(u32, &str, u32, u64)], holder: (u32, &str)) {
        let with_sid: Vec<(u32, &str, u32, u32, u64)> = procs.iter().map(|(p, c, g, r)| (*p, *c, *g, *g, *r)).collect();
        fake_proc_sid(root, &with_sid, holder);
    }

    fn fake_proc_sid(root: &Path, procs: &[(u32, &str, u32, u32, u64)], holder: (u32, &str)) {
        for (pid, comm, pg, sid, rss) in procs {
            let d = root.join(pid.to_string());
            fs::create_dir_all(&d).unwrap();
            fs::write(d.join("stat"), stat(*pid, comm, *pg, *sid, *rss)).unwrap();
        }
        let d = root.join(holder.0.to_string());
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("cgroup"), format!("0::/{}\n", holder.1)).unwrap();
        fs::write(d.join("stat"), stat(holder.0, "tmuxedo-runner", holder.0, holder.0, 10)).unwrap();
        fs::create_dir_all(root.join("self")).unwrap(); // non-numeric entries are skipped
    }

    #[test]
    fn finds_limit_above_leaf_and_picks_largest_group() {
        let t = tempfile::tempdir().unwrap();
        let (cg, pr) = (t.path().join("cg"), t.path().join("proc"));
        let leaf = cg.join("user.slice/tmuxedo-holder.service");
        fs::create_dir_all(&leaf).unwrap();
        fs::write(leaf.join("memory.max"), "max\n").unwrap();
        fs::write(leaf.join("memory.current"), "950\n").unwrap();
        fs::write(cg.join("user.slice/memory.max"), "1000\n").unwrap();
        fs::write(cg.join("user.slice/memory.current"), "950\n").unwrap();
        // Pane A group 100: two processes; pane B group 200: one big one; a stranger in group 300.
        fake_proc(
            &pr,
            &[
                (100, "zsh", 100, 10),
                (101, "claude (x) y", 100, 50),
                (200, "claude", 200, 70),
                (300, "other", 300, 999),
            ],
            (42, "user.slice/tmuxedo-holder.service"),
        );
        let ps = page_size();
        assert_eq!(group_rss(&pr, 100, ps), 60 * ps);
        let v = check(&cg, &pr, 42, &[("w1:p1".into(), 100), ("w1:p2".into(), 200)]).unwrap();
        assert_eq!(
            (v.pane.as_str(), v.pgid, v.group_rss, v.current, v.max),
            ("w1:p2", 200, 70 * ps, 950, 1000)
        );
    }

    /// The live failure: an interactive shell runs the hog in its OWN process
    /// group inside the pane's session. The pane is chosen by session total and
    /// the hog's group is the one stopped, not the shell's.
    #[test]
    fn foreground_job_in_its_own_group_is_the_victim() {
        let t = tempfile::tempdir().unwrap();
        let (cg, pr) = (t.path().join("cg"), t.path().join("proc"));
        fs::create_dir_all(&cg).unwrap();
        fs::write(cg.join("memory.max"), "1000\n").unwrap();
        fs::write(cg.join("memory.current"), "990\n").unwrap();
        fake_proc_sid(
            &pr,
            &[
                // pane A: bash 77 (session leader), hog 100 in group 100, same session
                (77, "bash", 77, 77, 5),
                (100, "python3", 100, 77, 900),
                // pane B: bash 80, survivor 110 in its own group
                (80, "bash", 80, 80, 5),
                (110, "python3", 110, 80, 20),
            ],
            (42, ""),
        );
        let ps = page_size();
        let v = check(&cg, &pr, 42, &[("w5:p1".into(), 77), ("w4:p1".into(), 80)]).unwrap();
        assert_eq!(v.pane, "w5:p1");
        assert_eq!(v.pgid, 100, "the hog's group, not the shell's (77)");
        assert_eq!((v.group_rss, v.pane_rss), (900 * ps, 905 * ps));
        // Under the old shell-group accounting the hog was invisible.
        assert_eq!(group_rss(&pr, 77, ps), 5 * ps);
    }

    #[test]
    fn quiet_below_threshold_or_without_limit() {
        let t = tempfile::tempdir().unwrap();
        let (cg, pr) = (t.path().join("cg"), t.path().join("proc"));
        fs::create_dir_all(cg.join("a")).unwrap();
        fs::write(cg.join("a/memory.max"), "1000").unwrap();
        fs::write(cg.join("a/memory.current"), "900").unwrap(); // exactly 90%: not above
        fake_proc(&pr, &[(100, "sh", 100, 5)], (42, "a"));
        assert_eq!(usage_for(&cg, &pr, 42), Some((900, 1000)));
        assert!(check(&cg, &pr, 42, &[("p".into(), 100)]).is_none());
        fs::write(cg.join("a/memory.current"), "901").unwrap();
        assert!(check(&cg, &pr, 42, &[("p".into(), 100)]).is_some());
        fs::write(cg.join("a/memory.max"), "max").unwrap();
        assert!(check(&cg, &pr, 42, &[("p".into(), 100)]).is_none());
        assert!(pick_victim(&[("x".into(), 0)]).is_none());
    }

    #[test]
    fn assess_names_the_next_victim_from_the_warning_level() {
        let t = tempfile::tempdir().unwrap();
        let (cg, pr) = (t.path().join("cg"), t.path().join("proc"));
        fs::create_dir_all(cg.join("a")).unwrap();
        fs::write(cg.join("a/memory.max"), "1000").unwrap();
        fs::write(cg.join("a/memory.current"), "849").unwrap();
        fake_proc_sid(
            &pr,
            &[(77, "bash", 77, 77, 5), (100, "cargo (build)", 100, 77, 900)],
            (42, "a"),
        );
        let panes = [("w1:p1".to_string(), 77)];
        let a = assess(&cg, &pr, 42, &panes).unwrap();
        assert_eq!((a.current, a.max, a.victim), (849, 1000, None), "below 85 %: no scan");
        fs::write(cg.join("a/memory.current"), "850").unwrap();
        let v = assess(&cg, &pr, 42, &panes).unwrap().victim.unwrap();
        assert_eq!((v.pane.as_str(), v.pgid), ("w1:p1", 100));
        assert_eq!(v.command.as_deref(), Some("cargo (build)"));
        assert!(
            check(&cg, &pr, 42, &panes).is_none(),
            "85 % warns; only above 90 % acts"
        );
        // The leader gone: the group's largest process names it.
        fs::remove_dir_all(pr.join("100")).unwrap();
        let d = pr.join("101");
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("stat"), stat(101, "node\u{7}", 100, 77, 400)).unwrap();
        let v = assess(&cg, &pr, 42, &panes).unwrap().victim.unwrap();
        assert_eq!(v.command.as_deref(), Some("node"), "control characters dropped");
        // No limit: nothing to assess.
        fs::write(cg.join("a/memory.max"), "max").unwrap();
        assert!(assess(&cg, &pr, 42, &panes).is_none());
    }
}
