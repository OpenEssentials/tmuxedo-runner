//! The terminal's foreground job, and a process's cwd and tty
//! (docs/design/agent-detection.md §8.2, §12).

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "linux")]
use linux as imp;
#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "macos")]
use macos as imp;
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod imp {
    pub fn foreground_job(_: u32) -> Option<super::Job> {
        None
    }
    pub fn cwd(_: u32) -> Option<String> {
        None
    }
    pub fn tty(_: u32) -> Option<String> {
        None
    }
    pub fn parent(_: u32) -> Option<u32> {
        None
    }
    pub fn process(_: u32) -> Option<super::Process> {
        None
    }
}

/// A process's parent pid (Linux `/proc/<pid>/stat`, macOS `proc_pidinfo`).
pub fn parent(pid: u32) -> Option<u32> {
    imp::parent(pid)
}

/// One process's name and argv.
pub fn process(pid: u32) -> Option<Process> {
    imp::process(pid)
}

/// One process of a foreground job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Process {
    pub pid: u32,
    /// The kernel's short name (`comm`).
    pub name: String,
    /// Empty when unreadable.
    pub argv: Vec<String>,
}

/// The terminal's foreground process group: every process in it, by pid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    pub pgid: u32,
    pub processes: Vec<Process>,
}

/// The foreground job of the terminal whose session leader (the pane's shell) is `shell_pid`.
pub fn foreground_job(shell_pid: u32) -> Option<Job> {
    imp::foreground_job(shell_pid)
}

/// A process's working directory.
pub fn cwd(pid: u32) -> Option<String> {
    imp::cwd(pid)
}

/// The terminal device on a process's standard input.
pub fn tty(pid: u32) -> Option<String> {
    imp::tty(pid)
}

/// argv from a macOS `KERN_PROCARGS2` buffer: a native-endian `int argc`, the exec
/// path, NUL padding, then `argc` NUL-terminated strings (then the environment).
/// Compiled everywhere so its tests run on every platform.
pub fn parse_procargs2(buf: &[u8]) -> Vec<String> {
    let Some((head, rest)) = buf.split_first_chunk::<4>() else {
        return vec![];
    };
    let argc = usize::try_from(i32::from_ne_bytes(*head)).unwrap_or(0);
    let Some(path_end) = rest.iter().position(|&b| b == 0) else {
        return vec![];
    };
    let rest = &rest[path_end..];
    let Some(start) = rest.iter().position(|&b| b != 0) else {
        return vec![];
    };
    rest[start..]
        .split(|&b| b == 0)
        .take(argc)
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .collect()
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))] // the tests themselves are not product code
mod tests {
    use super::*;

    fn buf(argc: i32, body: &[u8]) -> Vec<u8> {
        let mut b = argc.to_ne_bytes().to_vec();
        b.extend_from_slice(body);
        b
    }

    #[test]
    fn procargs2_reads_argc_strings_after_the_exec_path() {
        let b = buf(2, b"/usr/bin/claude\0\0\0\0claude\0--resume\0HOME=/x\0");
        assert_eq!(parse_procargs2(&b), ["claude", "--resume"]);
        let one = buf(1, b"/bin/sh\0sh\0");
        assert_eq!(parse_procargs2(&one), ["sh"]);
    }

    #[test]
    fn procargs2_survives_junk() {
        assert!(parse_procargs2(&[]).is_empty());
        assert!(parse_procargs2(&[1, 0]).is_empty());
        assert!(parse_procargs2(&buf(1, b"no-nul")).is_empty());
        assert!(parse_procargs2(&buf(1, b"/path\0\0\0")).is_empty(), "padding only");
        assert!(parse_procargs2(&buf(-3, b"/p\0a\0")).is_empty());
        assert_eq!(
            parse_procargs2(&buf(5, b"/p\0a\0b")),
            ["a", "b"],
            "fewer strings than argc"
        );
    }

    #[test]
    fn this_process_is_visible() {
        let me = std::process::id();
        let dir = cwd(me).expect("own cwd");
        assert_eq!(
            std::path::Path::new(&dir),
            std::env::current_dir().unwrap().canonicalize().unwrap()
        );
        assert!(cwd(u32::MAX).is_none());
        assert!(tty(u32::MAX).is_none());
        assert!(foreground_job(u32::MAX).is_none());
        // Parents and processes (the runner's hook ancestry check).
        assert!(parent(me).is_some());
        assert_eq!(process(me).map(|p| p.pid), Some(me));
        assert!(parent(u32::MAX).is_none());
        assert!(process(u32::MAX).is_none());
    }
}
