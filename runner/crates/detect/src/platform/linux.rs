//! Linux: everything from `/proc` (docs/design/agent-detection.md §12).

use super::{Job, Process};
use std::path::PathBuf;

/// A `/proc` tree; tests point it at a fake one.
#[derive(Debug, Clone)]
pub struct Procfs {
    pub root: PathBuf,
}

impl Default for Procfs {
    fn default() -> Self {
        Procfs {
            root: PathBuf::from("/proc"),
        }
    }
}

/// `comm` and the fields after it in `/proc/<pid>/stat`.
pub(crate) fn parse_stat(stat: &str) -> Option<(String, Vec<&str>)> {
    let open = stat.find('(')?;
    let close = stat.rfind(')')?;
    let comm = stat.get(open + 1..close)?.to_string();
    Some((comm, stat[close + 1..].split_whitespace().collect()))
}

/// Field `i` (0 = state, 2 = pgrp, 5 = tpgid) as a positive pid.
fn pid_field(fields: &[&str], i: usize) -> Option<u32> {
    let v: i64 = fields.get(i)?.parse().ok()?;
    u32::try_from(v).ok().filter(|&v| v > 0)
}

impl Procfs {
    fn stat(&self, pid: u32) -> Option<String> {
        std::fs::read_to_string(self.root.join(pid.to_string()).join("stat")).ok()
    }

    fn argv(&self, pid: u32) -> Vec<String> {
        let raw = std::fs::read(self.root.join(pid.to_string()).join("cmdline")).unwrap_or_default();
        raw.split(|&b| b == 0)
            .filter(|a| !a.is_empty())
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .collect()
    }

    fn link(&self, pid: u32, rel: &str) -> Option<String> {
        let target = std::fs::read_link(self.root.join(pid.to_string()).join(rel)).ok()?;
        Some(target.display().to_string())
    }

    /// The `tpgid` of the shell's terminal.
    pub fn foreground_pgid(&self, shell_pid: u32) -> Option<u32> {
        let stat = self.stat(shell_pid)?;
        let (_, fields) = parse_stat(&stat)?;
        pid_field(&fields, 5)
    }

    pub fn foreground_job(&self, shell_pid: u32) -> Option<Job> {
        let pgid = self.foreground_pgid(shell_pid)?;
        let mut processes = vec![];
        for entry in std::fs::read_dir(&self.root).ok()?.flatten() {
            let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else {
                continue;
            };
            // The process may be gone by now, or not a well-formed stat: skip it.
            let Some(stat) = self.stat(pid) else { continue };
            let Some((name, fields)) = parse_stat(&stat) else {
                continue;
            };
            if pid_field(&fields, 2) == Some(pgid) {
                processes.push(Process {
                    pid,
                    name,
                    argv: self.argv(pid),
                });
            }
        }
        processes.sort_by_key(|p| p.pid);
        (!processes.is_empty()).then_some(Job { pgid, processes })
    }

    pub fn cwd(&self, pid: u32) -> Option<String> {
        self.link(pid, "cwd")
    }

    /// A process's parent pid (`ppid`, field 1 after `comm`); None for pid 1's 0 or a gone process.
    pub fn parent(&self, pid: u32) -> Option<u32> {
        let stat = self.stat(pid)?;
        let (_, fields) = parse_stat(&stat)?;
        pid_field(&fields, 1)
    }

    /// One process: its `comm` and argv.
    pub fn process(&self, pid: u32) -> Option<Process> {
        let stat = self.stat(pid)?;
        let (name, _) = parse_stat(&stat)?;
        Some(Process {
            pid,
            name,
            argv: self.argv(pid),
        })
    }

    pub fn tty(&self, pid: u32) -> Option<String> {
        self.link(pid, "fd/0")
    }
}

pub fn foreground_job(shell_pid: u32) -> Option<Job> {
    Procfs::default().foreground_job(shell_pid)
}

pub fn cwd(pid: u32) -> Option<String> {
    Procfs::default().cwd(pid)
}

pub fn tty(pid: u32) -> Option<String> {
    Procfs::default().tty(pid)
}

pub fn parent(pid: u32) -> Option<u32> {
    Procfs::default().parent(pid)
}

pub fn process(pid: u32) -> Option<Process> {
    Procfs::default().process(pid)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))] // the tests themselves are not product code
mod tests {
    use super::*;
    use std::fs;

    /// A fake /proc: `(pid, stat, cmdline)`.
    fn fake(entries: &[(&str, &str, &[u8])]) -> (tempfile::TempDir, Procfs) {
        let dir = tempfile::tempdir().unwrap();
        for (pid, stat, cmdline) in entries {
            let p = dir.path().join(pid);
            fs::create_dir_all(&p).unwrap();
            if !stat.is_empty() {
                fs::write(p.join("stat"), stat).unwrap();
            }
            fs::write(p.join("cmdline"), cmdline).unwrap();
        }
        let fs = Procfs {
            root: dir.path().to_path_buf(),
        };
        (dir, fs)
    }

    #[test]
    fn parent_and_process_from_a_fake_proc() {
        let (_d, fs) = fake(&[
            ("10", "10 (bash) S 1 10 10 34816 20 0", b"bash\0"),
            (
                "20",
                "20 (claude) S 10 20 10 34816 20 0",
                b"/usr/bin/claude\0--resume\0x\0",
            ),
            ("1", "1 (init) S 0 1 1 0 -1 0", b"/sbin/init\0"),
            ("30", "", b""),
            ("31", "garbage without parens", b"x\0"),
        ]);
        assert_eq!(fs.parent(20), Some(10));
        assert_eq!(fs.parent(10), Some(1));
        assert_eq!(fs.parent(1), None, "ppid 0");
        assert_eq!(fs.parent(30), None, "no stat");
        assert_eq!(fs.parent(99), None, "gone");
        let p = fs.process(20).unwrap();
        assert_eq!((p.name.as_str(), p.argv.len()), ("claude", 3));
        assert!(fs.process(30).is_none());
        assert!(fs.process(31).is_none(), "a stat it cannot read");
        assert!(fs.parent(31).is_none());
        let me = std::process::id();
        assert!(parent(me).is_some());
        assert_eq!(process(me).map(|p| p.pid), Some(me));
    }

    #[test]
    fn stat_parsing_handles_parens_and_spaces_in_comm() {
        let (comm, f) = parse_stat("42 (my (odd) name) S 1 42 42 34816 77 4194560").unwrap();
        assert_eq!(comm, "my (odd) name");
        assert_eq!(f[0], "S");
        assert_eq!(pid_field(&f, 2), Some(42));
        assert_eq!(pid_field(&f, 5), Some(77));
        assert_eq!(pid_field(&f, 40), None);
        assert!(parse_stat("no parens").is_none());
        assert!(parse_stat("1 (x").is_none());
        assert!(parse_stat("1 )x(").is_none(), "close before open");
        let (_, f) = parse_stat("1 (x) S 1 1 1 0 -1").unwrap();
        assert_eq!(pid_field(&f, 5), None, "no controlling terminal");
        let (_, f) = parse_stat("1 (x) S 1 1 1 0 0").unwrap();
        assert_eq!(pid_field(&f, 5), None);
        let (_, f) = parse_stat("1 (x) S 1 1 1 0 zz").unwrap();
        assert_eq!(pid_field(&f, 5), None);
        let (_, f) = parse_stat("1 (x) S 1 1 1 0 99999999999").unwrap();
        assert_eq!(pid_field(&f, 5), None, "out of range");
    }

    #[test]
    fn foreground_job_from_a_fake_proc() {
        let (_d, fs) = fake(&[
            ("100", "100 (bash) S 1 100 100 34816 200 0", b"bash\0"),
            (
                "200",
                "200 (claude) S 100 200 100 34816 200 0",
                b"/usr/bin/claude\0--resume\0\0",
            ),
            ("201", "201 (node) S 200 200 100 34816 200 0", b""),
            ("300", "300 (other) S 1 300 300 0 -1 0", b"other\0"),
            ("400", "", b""),
            ("401", "garbage", b""),
            ("self", "", b""),
        ]);
        fs::write(fs.root.join("uptime"), "1 2").unwrap();
        let job = fs.foreground_job(100).unwrap();
        assert_eq!(job.pgid, 200);
        assert_eq!(
            job.processes,
            vec![
                Process {
                    pid: 200,
                    name: "claude".into(),
                    argv: vec!["/usr/bin/claude".into(), "--resume".into()]
                },
                Process {
                    pid: 201,
                    name: "node".into(),
                    argv: vec![]
                },
            ]
        );
        assert!(fs.foreground_job(300).is_none(), "no terminal");
        assert!(fs.foreground_job(400).is_none(), "no stat");
        assert!(fs.foreground_job(401).is_none(), "bad stat");
        assert!(fs.foreground_job(999).is_none(), "no such pid");
    }

    #[test]
    fn empty_group_is_no_job() {
        let (_d, fs) = fake(&[("100", "100 (bash) S 1 100 100 34816 555 0", b"bash\0")]);
        assert!(fs.foreground_job(100).is_none());
    }

    #[test]
    fn unlistable_root_is_no_job() {
        use std::os::unix::fs::PermissionsExt;
        let (d, fs) = fake(&[("100", "100 (bash) S 1 100 100 34816 100 0", b"bash\0")]);
        assert_eq!(fs.foreground_job(100).unwrap().processes.len(), 1);
        // Execute-only: the shell's stat is readable by path, the directory cannot be listed.
        fs::set_permissions(d.path(), fs::Permissions::from_mode(0o100)).unwrap();
        let listable = fs::read_dir(d.path()).is_ok(); // true when the tests run as root
        let job = fs.foreground_job(100);
        fs::set_permissions(d.path(), fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(fs.foreground_pgid(100), Some(100));
        if !listable {
            assert!(job.is_none());
        }
    }

    #[test]
    fn cwd_and_tty_are_links() {
        let (d, fs) = fake(&[("7", "7 (x) S 1 7 7 0 -1 0", b"x\0")]);
        std::os::unix::fs::symlink("/work/dir", d.path().join("7/cwd")).unwrap();
        fs::create_dir(d.path().join("7/fd")).unwrap();
        std::os::unix::fs::symlink("/dev/pts/3", d.path().join("7/fd/0")).unwrap();
        assert_eq!(fs.cwd(7).as_deref(), Some("/work/dir"));
        assert_eq!(fs.tty(7).as_deref(), Some("/dev/pts/3"));
        assert_eq!(fs.cwd(8), None);
        assert_eq!(fs.tty(8), None);
    }

    #[test]
    fn real_proc_defaults() {
        assert_eq!(Procfs::default().root, PathBuf::from("/proc"));
        let me = std::process::id();
        assert!(cwd(me).is_some());
        // stdin of the test harness may or may not be a terminal; the call must not panic.
        let _ = tty(me);
        assert!(foreground_job(u32::MAX).is_none());
    }
}
