//! macOS: libproc and sysctl (docs/design/agent-detection.md §12).

use super::{parse_procargs2, Job, Process};
use std::ffi::{c_char, c_int, CStr};
use std::mem::{size_of, MaybeUninit};

extern "C" {
    /// libSystem `devname(3)`: the `/dev` name of a device number, or NULL.
    fn devname(dev: libc::dev_t, kind: libc::mode_t) -> *const c_char;
}

/// Most processes read from one foreground group.
const MAX_GROUP: usize = 1024;

fn c_pid(pid: u32) -> Option<c_int> {
    c_int::try_from(pid).ok().filter(|&p| p > 0)
}

/// `proc_pidinfo` of a flavour whose result is the plain struct `T`.
fn pidinfo<T>(pid: u32, flavor: c_int) -> Option<T> {
    let pid = c_pid(pid)?;
    let mut out = MaybeUninit::<T>::zeroed();
    let size = c_int::try_from(size_of::<T>()).ok()?;
    // SAFETY: the buffer is `size` bytes of a plain C struct; the kernel writes at most that.
    let n = unsafe { libc::proc_pidinfo(pid, flavor, 0, out.as_mut_ptr().cast(), size) };
    // SAFETY: a full-size answer initialised every byte (and zeroed() covered padding).
    (n == size).then(|| unsafe { out.assume_init() })
}

fn bsdinfo(pid: u32) -> Option<libc::proc_bsdinfo> {
    pidinfo(pid, libc::PROC_PIDTBSDINFO)
}

/// A NUL-terminated C string from a fixed array.
fn c_text(chars: &[c_char]) -> String {
    let bytes: Vec<u8> = chars.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// `pbi_name` (up to 32 bytes), else `pbi_comm` (16).
fn short_name(info: &libc::proc_bsdinfo) -> String {
    name_or_comm(&info.pbi_name, &info.pbi_comm)
}

fn name_or_comm(name: &[c_char], comm: &[c_char]) -> String {
    let name = c_text(name);
    if name.is_empty() {
        c_text(comm)
    } else {
        name
    }
}

/// `sysctl(mib)` into `buf`; the length written.
fn sysctl(mib: &mut [c_int], buf: &mut [u8]) -> Option<usize> {
    let mut len = buf.len();
    let depth = u32::try_from(mib.len()).ok()?;
    // SAFETY: `buf` is `len` writable bytes; the kernel updates `len` to what it wrote.
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            depth,
            buf.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    (rc == 0).then_some(len)
}

/// argv via `KERN_PROCARGS2` (empty for processes we may not read).
pub fn argv(pid: u32) -> Vec<String> {
    let Some(pid) = c_pid(pid) else { return vec![] };
    let mut argmax = [0u8; size_of::<c_int>()];
    let max = sysctl(&mut [libc::CTL_KERN, libc::KERN_ARGMAX], &mut argmax)
        .map_or(0, |_| usize::try_from(c_int::from_ne_bytes(argmax)).unwrap_or(0));
    let mut buf = vec![0u8; max];
    let n = sysctl(&mut [libc::CTL_KERN, libc::KERN_PROCARGS2, pid], &mut buf).unwrap_or(0);
    parse_procargs2(&buf[..n.min(buf.len())])
}

pub fn foreground_job(shell_pid: u32) -> Option<Job> {
    let pgid = bsdinfo(shell_pid)?.e_tpgid;
    let group = c_pid(pgid)?;
    let mut pids: Vec<c_int> = vec![0; MAX_GROUP];
    let bytes = c_int::try_from(pids.len() * size_of::<c_int>()).ok()?;
    // SAFETY: `pids` has room for `bytes` bytes; libproc returns how many pids it wrote.
    let n = unsafe { libc::proc_listpgrppids(group, pids.as_mut_ptr().cast(), bytes) };
    let n = usize::try_from(n).unwrap_or(0).min(MAX_GROUP);
    let pids = &mut pids[..n];
    pids.sort_unstable();
    let processes: Vec<Process> = pids
        .iter()
        .filter_map(|&pid| {
            let pid = u32::try_from(pid).ok()?;
            // Re-check the group: the list can be stale by the time we look.
            let info = bsdinfo(pid).filter(|i| i.pbi_pgid == pgid)?;
            Some(Process {
                pid,
                name: short_name(&info),
                argv: argv(pid),
            })
        })
        .collect();
    (!processes.is_empty()).then_some(Job { pgid, processes })
}

pub fn cwd(pid: u32) -> Option<String> {
    let info: libc::proc_vnodepathinfo = pidinfo(pid, libc::PROC_PIDVNODEPATHINFO)?;
    let flat: Vec<c_char> = info.pvi_cdir.vip_path.iter().flatten().copied().collect();
    Some(c_text(&flat)).filter(|p| !p.is_empty())
}

/// `e_tdev` as a device number: 0 and NODEV (all ones) mean no controlling terminal.
fn tty_dev(raw: u32) -> Option<libc::dev_t> {
    if raw == 0 || raw == u32::MAX {
        return None;
    }
    libc::dev_t::try_from(raw).ok()
}

/// A process's parent pid; None for a gone process or `launchd`'s 0.
pub fn parent(pid: u32) -> Option<u32> {
    Some(bsdinfo(pid)?.pbi_ppid).filter(|&p| p > 0)
}

/// One process: its short name and argv.
pub fn process(pid: u32) -> Option<Process> {
    let info = bsdinfo(pid)?;
    Some(Process {
        pid,
        name: short_name(&info),
        argv: argv(pid),
    })
}

pub fn tty(pid: u32) -> Option<String> {
    let dev = tty_dev(bsdinfo(pid)?.e_tdev)?;
    // SAFETY: devname returns NULL or a pointer to a static NUL-terminated name.
    let name = unsafe { devname(dev, libc::S_IFCHR) };
    // SAFETY: checked non-null; the string lives in libc's static buffer until the next call.
    let name = (!name.is_null()).then(|| unsafe { CStr::from_ptr(name) }.to_string_lossy().into_owned())?;
    Some(format!("/dev/{name}")).filter(|_| name != "??")
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))] // the tests themselves are not product code
mod tests {
    use super::*;

    #[test]
    fn own_process_is_readable() {
        let me = std::process::id();
        let info = bsdinfo(me).unwrap();
        assert!(!short_name(&info).is_empty());
        let args = argv(me);
        assert!(!args.is_empty(), "argv of the test binary");
        assert!(cwd(me).is_some());
        assert!(bsdinfo(0).is_none());
        assert!(argv(0).is_empty());
        assert!(argv(u32::MAX).is_empty());
        assert!(argv(999_999).is_empty(), "no such pid");
        assert_eq!(parent(me), Some(std::os::unix::process::parent_id()));
        assert_eq!(process(me).map(|p| p.pid), Some(me));
        assert!(parent(0).is_none() && process(0).is_none());
    }

    #[test]
    fn tty_device_numbers() {
        assert_eq!(tty_dev(0), None, "no terminal");
        assert_eq!(tty_dev(u32::MAX), None, "NODEV");
        assert_eq!(tty_dev(0x1000_0003), Some(0x1000_0003));
        assert_eq!(tty_dev(0x8000_0000), None, "not a dev_t");
        assert!(foreground_job(0).is_none());
        assert!(tty(0).is_none());
    }

    /// `sleep` as the session leader of a fresh pseudo-terminal: it is that
    /// terminal's foreground job, and the terminal is its tty.
    #[test]
    fn foreground_job_and_tty_of_a_terminal() {
        use std::os::fd::{FromRawFd, OwnedFd};
        use std::os::unix::process::CommandExt;
        use std::process::{Command, Stdio};
        let (mut leader, mut follower): (c_int, c_int) = (-1, -1);
        // SAFETY: out-pointers to two ints; no name buffer, default termios and size.
        let rc = unsafe {
            libc::openpty(
                &mut leader,
                &mut follower,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(rc, 0, "openpty: {}", std::io::Error::last_os_error());
        // SAFETY: each dup is a fresh descriptor this test owns.
        let stdio = || Stdio::from(unsafe { OwnedFd::from_raw_fd(libc::dup(follower)) });
        let mut cmd = Command::new("/bin/sleep");
        cmd.arg("30").stdin(stdio()).stdout(stdio()).stderr(stdio());
        // SAFETY: only async-signal-safe calls between fork and exec.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY.into(), 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                libc::tcsetpgrp(0, libc::getpid());
                Ok(())
            });
        }
        let mut child = cmd.spawn().expect("spawn sleep on a pty");
        let pid = child.id();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let job = loop {
            // Until exec, the child still carries the test binary's name.
            if let Some(j) = foreground_job(pid).filter(|j| j.processes.iter().any(|p| p.name == "sleep")) {
                break j;
            }
            assert!(std::time::Instant::now() < deadline, "no foreground job for {pid}");
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        let tty_name = tty(pid);
        let _ = child.kill();
        let _ = child.wait();
        // SAFETY: both descriptors came from openpty above and are closed once.
        unsafe {
            libc::close(follower);
            libc::close(leader);
        }
        assert_eq!(job.pgid, pid);
        assert_eq!(job.processes.len(), 1, "{job:?}");
        assert_eq!(job.processes[0].pid, pid);
        assert_eq!(job.processes[0].argv, ["/bin/sleep", "30"]);
        let tty_name = tty_name.expect("the pty is its controlling terminal");
        assert!(tty_name.starts_with("/dev/ttys"), "{tty_name}");
    }

    #[test]
    fn c_text_stops_at_nul() {
        let raw: Vec<c_char> = b"abc\0def".iter().map(|&b| b as c_char).collect();
        assert_eq!(c_text(&raw), "abc");
        assert_eq!(c_text(&[]), "");
        let comm: Vec<c_char> = b"sleep\0".iter().map(|&b| b as c_char).collect();
        assert_eq!(name_or_comm(&raw, &comm), "abc", "pbi_name first");
        assert_eq!(name_or_comm(&[0], &comm), "sleep", "pbi_comm when pbi_name is empty");
    }
}
