//! Machine stats sampler (docs/protocol.md "Machine stats"): CPU, memory and
//! disk of this machine, read only while the gateway asks (someone is
//! looking). With no rate set, [`Sampler::tick`] returns at once and reads
//! nothing; the daemon calls it from the tick it already runs, so an idle
//! runner has no timer and no wakeup for stats.
//!
//! Inside a container the numbers are the CONTAINER's: memory from the nearest
//! cgroup v2 directory with a numeric `memory.max` (the one the memory
//! watchdog enforces), CPU against the nearest numeric `cpu.max` quota. On a
//! machine without limits they come from `/proc`. On macOS they come from the
//! host's own counters (`host_statistics64`, `host_statistics`, `sysctl`; no
//! process is run): [`mac_mem`] and [`mac_cpu_ticks`] turn those into the
//! same numbers. Where none of these exists, CPU and memory are left out of
//! the sample: "not available", never a guess. Disk is `statvfs` of the
//! runner's home everywhere.
//!
//! Values only. No process is listed, no name, command line, user or path is
//! read or sent. Every file may be missing, short, huge or junk: reads are
//! capped at [`READ_CAP`] bytes, parsing never panics, and nothing is run.

use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};
use tmuxedo_protocol::stats::{StatsCpu, StatsDisk, StatsMem, StatsSample};

/// Most bytes read of any one file. `/proc/stat` on a machine with hundreds
/// of cores has its `cpu` lines well inside this; the rest is not needed.
pub const READ_CAP: u64 = 64 * 1024;
/// How far up the cgroup tree a limit is looked for.
const MAX_DEPTH: usize = 32;

/// Where the sampler reads from; tests point these at fake trees.
#[derive(Debug, Clone)]
pub struct Roots {
    /// `/proc`, where there is one.
    pub proc: Option<PathBuf>,
    /// The cgroup v2 mount, where there is one.
    pub cgroup: Option<PathBuf>,
    /// The runner's home: the disk that is reported.
    pub home: PathBuf,
    /// The process whose cgroup is "this machine" (the pane holder, as the watchdog uses).
    pub pid: u32,
    /// Ask the host's own counters where there is no `/proc` (macOS).
    pub host_api: bool,
}

impl Roots {
    /// The real machine. Linux: `/proc` and cgroups. macOS: the host's
    /// counters. Anything else: CPU and memory are not available.
    pub fn system(home: &Path, pid: u32) -> Self {
        let linux = cfg!(target_os = "linux");
        Roots {
            proc: linux.then(|| PathBuf::from("/proc")),
            cgroup: linux.then(|| PathBuf::from("/sys/fs/cgroup")),
            home: home.to_path_buf(),
            pid,
            host_api: cfg!(target_os = "macos"),
        }
    }
}

/// At most [`READ_CAP`] bytes of a file as text; `None` when it cannot be opened or read.
fn read_capped(p: &Path) -> Option<String> {
    let f = std::fs::File::open(p).ok()?;
    let mut buf = Vec::with_capacity(1024);
    f.take(READ_CAP).read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// A file holding one unsigned number (`memory.current`; `memory.max` when limited).
fn read_u64(p: &Path) -> Option<u64> {
    read_capped(p)?.trim().parse().ok()
}

/// `key value` lines (`cpu.stat`): the value of `key`.
fn kv(text: &str, key: &str) -> Option<u64> {
    text.lines().find_map(|l| {
        let mut it = l.split_ascii_whitespace();
        (it.next() == Some(key)).then(|| it.next()?.parse().ok()).flatten()
    })
}

/// `/proc/stat`: (busy jiffies, all jiffies, cores).
pub fn parse_proc_stat(text: &str) -> Option<(u64, u64, u32)> {
    let mut totals = None;
    let mut cores = 0u32;
    for l in text.lines() {
        let Some(rest) = l.strip_prefix("cpu") else {
            continue;
        };
        if rest.starts_with(|c: char| c.is_ascii_digit()) {
            cores = cores.saturating_add(1);
            continue;
        }
        if !rest.starts_with(' ') || totals.is_some() {
            continue;
        }
        // user nice system idle iowait irq softirq steal (guest time is already in user).
        let mut v = [0u64; 8];
        let mut n = 0;
        for (slot, word) in v.iter_mut().zip(rest.split_ascii_whitespace()) {
            *slot = word.parse().ok()?;
            n += 1;
        }
        if n < 4 {
            return None;
        }
        let all = v.iter().try_fold(0u64, |a, x| a.checked_add(*x))?;
        let idle = v[3].checked_add(v[4])?;
        totals = Some((all.checked_sub(idle)?, all));
    }
    let (busy, all) = totals?;
    Some((busy, all, cores))
}

/// `/proc/meminfo` in bytes: (total, available, swap total, swap free).
pub fn parse_meminfo(text: &str) -> Option<(u64, u64, Option<u64>, Option<u64>)> {
    let get = |key: &str| -> Option<u64> {
        text.lines().find_map(|l| {
            let rest = l.strip_prefix(key)?.strip_prefix(':')?;
            let kb: u64 = rest.split_ascii_whitespace().next()?.parse().ok()?;
            kb.checked_mul(1024)
        })
    };
    Some((
        get("MemTotal")?,
        get("MemAvailable")?,
        get("SwapTotal"),
        get("SwapFree"),
    ))
}

/// `cpu.max` (`"200000 100000"`): the quota in cores; `"max …"` or junk = no quota.
pub fn parse_cpu_max(text: &str) -> Option<f64> {
    let mut it = text.split_ascii_whitespace();
    let quota: u64 = it.next()?.parse().ok()?;
    let period: u64 = it.next()?.parse().ok()?;
    (quota > 0 && period > 0).then(|| quota as f64 / period as f64)
}

/// A PSI file (`some avg10=1.23 avg60=…`): the `some avg10` percentage.
pub fn parse_pressure(text: &str) -> Option<f64> {
    let line = text.lines().find(|l| l.starts_with("some "))?;
    let v: f64 = line
        .split_ascii_whitespace()
        .find_map(|w| w.strip_prefix("avg10="))?
        .parse()
        .ok()?;
    (v.is_finite() && (0.0..=100.0).contains(&v)).then_some(v)
}

/// `/proc/<pid>/cgroup`: the cgroup v2 path relative to the mount (`0::/a/b` → `a/b`).
/// A path with anything but plain names (`..`, a root) is refused.
pub fn parse_cgroup_rel(text: &str) -> Option<PathBuf> {
    let line = text.lines().find(|l| l.starts_with("0::"))?;
    let rel = PathBuf::from(line.trim_start_matches("0::").trim().trim_start_matches('/'));
    rel.components()
        .all(|c| matches!(c, Component::Normal(_)))
        .then_some(rel)
}

/// `/proc/loadavg`: the one-minute load.
pub fn parse_loadavg(text: &str) -> Option<f64> {
    let v: f64 = text.split_ascii_whitespace().next()?.parse().ok()?;
    (v.is_finite() && v >= 0.0).then_some(v)
}

/// macOS page counts, as `host_statistics64(HOST_VM_INFO64)` gives them (and `vm_stat` prints them).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MacVm {
    pub free: u64,
    pub active: u64,
    pub inactive: u64,
    pub speculative: u64,
    pub wired: u64,
    pub purgeable: u64,
    /// Pages the compressor occupies ("Pages occupied by compressor").
    pub compressor: u64,
    /// Anonymous pages ("Anonymous pages"; `internal_page_count`).
    pub internal: u64,
    /// File-backed pages ("File-backed pages"; `external_page_count`).
    pub external: u64,
}

/// macOS memory "used" as Activity Monitor counts it: app memory (anonymous
/// minus purgeable) + wired + compressed, against physical memory. Cache
/// (file-backed, purgeable, free) is not used, the same rule as
/// `MemAvailable` on Linux. None without a page size or physical memory.
pub fn mac_mem(vm: &MacVm, page: u64, physical: u64) -> Option<StatsMem> {
    if page == 0 || physical == 0 {
        return None;
    }
    let pages = vm
        .internal
        .saturating_sub(vm.purgeable)
        .saturating_add(vm.wired)
        .saturating_add(vm.compressor);
    Some(StatsMem {
        used: pages.saturating_mul(page).min(physical),
        limit: physical,
        limited: false,
        pressure10: None,
        swap_used: None,
    })
}

/// macOS `host_cpu_load_info.cpu_ticks` (user, system, idle, nice) as
/// (busy, all), the same pair [`parse_proc_stat`] gives on Linux.
pub fn mac_cpu_ticks(t: [u32; 4]) -> (u64, u64) {
    let [user, system, idle, nice] = t.map(u64::from);
    let busy = user + system + nice;
    (busy, busy + idle)
}

/// The host calls behind [`Roots::host_api`]: macOS only; elsewhere they answer nothing.
#[cfg(target_os = "macos")]
mod host {
    use super::{mac_cpu_ticks, mac_mem, MacVm};
    use std::ffi::{c_void, CStr};
    use std::sync::OnceLock;
    use tmuxedo_protocol::stats::StatsMem;

    /// One send right for the life of the process (each `mach_host_self` adds a reference).
    #[allow(deprecated)] // libc points at the mach2 crate; the call is the same
    fn port() -> libc::mach_port_t {
        static PORT: OnceLock<libc::mach_port_t> = OnceLock::new();
        // SAFETY: no arguments; returns the host port name.
        *PORT.get_or_init(|| unsafe { libc::mach_host_self() })
    }

    fn sysctl<T: Copy + Default>(name: &CStr) -> Option<T> {
        let mut v = T::default();
        let mut len = std::mem::size_of::<T>();
        // SAFETY: `v` is a plain value of `len` bytes; no new value is set.
        let r = unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                (&mut v as *mut T).cast::<c_void>(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        (r == 0 && len == std::mem::size_of::<T>()).then_some(v)
    }

    pub fn mem() -> Option<StatsMem> {
        // SAFETY: a zeroed plain C struct, filled by the kernel up to `count` integers.
        let mut vm: libc::vm_statistics64 = unsafe { std::mem::zeroed() };
        let mut count = libc::HOST_VM_INFO64_COUNT;
        let r = unsafe {
            libc::host_statistics64(
                port(),
                libc::HOST_VM_INFO64,
                (&mut vm as *mut libc::vm_statistics64).cast(),
                &mut count,
            )
        };
        if r != libc::KERN_SUCCESS {
            return None;
        }
        let v = MacVm {
            free: vm.free_count.into(),
            active: vm.active_count.into(),
            inactive: vm.inactive_count.into(),
            speculative: vm.speculative_count.into(),
            wired: vm.wire_count.into(),
            purgeable: vm.purgeable_count.into(),
            compressor: vm.compressor_page_count.into(),
            internal: vm.internal_page_count.into(),
            external: vm.external_page_count.into(),
        };
        mac_mem(&v, crate::watchdog::page_size(), sysctl::<u64>(c"hw.memsize")?)
    }

    /// (busy, all, cores).
    pub fn cpu() -> Option<(u64, u64, u32)> {
        // SAFETY: as in `mem`, for the four tick counters.
        let mut info: libc::host_cpu_load_info = unsafe { std::mem::zeroed() };
        let mut count = libc::HOST_CPU_LOAD_INFO_COUNT;
        let r = unsafe {
            libc::host_statistics(
                port(),
                libc::HOST_CPU_LOAD_INFO,
                (&mut info as *mut libc::host_cpu_load_info).cast(),
                &mut count,
            )
        };
        if r != libc::KERN_SUCCESS {
            return None;
        }
        let cores = sysctl::<i32>(c"hw.logicalcpu")
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(0);
        let (busy, all) = mac_cpu_ticks(info.cpu_ticks);
        Some((busy, all, cores))
    }

    pub fn load1() -> Option<f64> {
        let mut l = [0f64; 3];
        // SAFETY: room for the three values asked for.
        let n = unsafe { libc::getloadavg(l.as_mut_ptr(), 3) };
        (n >= 1 && l[0].is_finite() && l[0] >= 0.0).then_some((l[0] * 100.0).round() / 100.0)
    }
}

#[cfg(not(target_os = "macos"))]
mod host {
    use tmuxedo_protocol::stats::StatsMem;
    pub fn mem() -> Option<StatsMem> {
        None
    }
    pub fn cpu() -> Option<(u64, u64, u32)> {
        None
    }
    pub fn load1() -> Option<f64> {
        None
    }
}

/// The nearest cgroup directory at or above `rel` for which `found` answers.
fn nearest<T>(root: &Path, rel: &Path, found: impl Fn(&Path) -> Option<T>) -> Option<(PathBuf, T)> {
    let mut cur = rel.to_path_buf();
    for _ in 0..MAX_DEPTH {
        let dir = root.join(&cur);
        if let Some(v) = found(&dir) {
            return Some((dir, v));
        }
        if !cur.pop() {
            break;
        }
    }
    None
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

/// `part` of `whole` as a percentage with one decimal, kept inside 0..=100.
fn pct(part: f64, whole: f64) -> Option<f64> {
    (whole > 0.0 && part.is_finite() && part >= 0.0).then(|| round1((part / whole * 100.0).min(100.0)))
}

/// The counters the next sample's CPU share is measured against.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct CpuMark {
    /// `/proc/stat` (busy, all) jiffies.
    host: Option<(u64, u64)>,
    /// cgroup `cpu.stat` (usage_usec, nr_periods, nr_throttled).
    group: Option<(u64, u64, u64)>,
}

/// Samples the machine while a rate is set; does nothing otherwise.
#[derive(Debug)]
pub struct Sampler {
    roots: Roots,
    period: Option<Duration>,
    next: Instant,
    mark: Option<(Instant, CpuMark)>,
    taken: u64,
}

impl Sampler {
    pub fn new(roots: Roots) -> Self {
        Sampler {
            roots,
            period: None,
            next: Instant::now(),
            mark: None,
            taken: 0,
        }
    }

    /// `Some(hz)`: sample that often, the first one on the next tick.
    /// `None`: stop, and forget the CPU counters (a later start measures afresh).
    pub fn set_rate(&mut self, hz: Option<f64>, now: Instant) {
        match hz.and_then(tmuxedo_protocol::stats::stats_rate) {
            Some(hz) => {
                if self.period.is_none() {
                    self.next = now;
                }
                self.period = Some(Duration::from_secs_f64(1.0 / hz));
            }
            None => {
                self.period = None;
                self.mark = None;
            }
        }
    }

    pub fn active(&self) -> bool {
        self.period.is_some()
    }

    /// Samples taken since this sampler was made (tests: none without a rate).
    pub fn taken(&self) -> u64 {
        self.taken
    }

    /// A sample when one is due; `None` (and no file touched) when no rate is set or it is not time yet.
    pub fn tick(&mut self, now: Instant, at_ms: u64) -> Option<StatsSample> {
        let period = self.period?;
        if now < self.next {
            return None;
        }
        self.next = now + period;
        self.taken += 1;
        Some(self.sample(now, at_ms))
    }

    fn sample(&mut self, now: Instant, at: u64) -> StatsSample {
        let rel = self.roots.proc.as_deref().and_then(|p| {
            read_capped(&p.join(self.roots.pid.to_string()).join("cgroup")).and_then(|t| parse_cgroup_rel(&t))
        });
        let cg = self.roots.cgroup.as_deref().zip(rel.as_deref());
        let prev = self.mark.take();
        let (cpu, mark) = self.cpu(cg, prev, now);
        self.mark = Some((now, mark));
        let disks = crate::files::statvfs(&self.roots.home)
            .filter(|(free, total)| *total > 0 && free <= total)
            .map(|(free, total)| StatsDisk {
                id: "home".into(),
                free,
                total,
            })
            .into_iter()
            .collect();
        StatsSample {
            at,
            cpu,
            mem: self.mem(cg),
            disks,
        }
    }

    fn mem(&self, cg: Option<(&Path, &Path)>) -> Option<StatsMem> {
        // A container: the nearest cgroup with a numeric limit, as the watchdog reads it.
        let limited = cg.and_then(|(root, rel)| {
            nearest(root, rel, |d| {
                let limit = read_u64(&d.join("memory.max")).filter(|m| *m > 0)?;
                Some((read_u64(&d.join("memory.current"))?, limit))
            })
        });
        if let Some((dir, (used, limit))) = limited {
            return Some(StatsMem {
                used,
                limit,
                limited: true,
                pressure10: read_capped(&dir.join("memory.pressure")).and_then(|t| parse_pressure(&t)),
                swap_used: read_u64(&dir.join("memory.swap.current")).filter(|s| *s > 0),
            });
        }
        let Some(proc) = self.roots.proc.as_deref() else {
            return if self.roots.host_api { host::mem() } else { None };
        };
        let (total, avail, swap_total, swap_free) = parse_meminfo(&read_capped(&proc.join("meminfo"))?)?;
        if total == 0 {
            return None;
        }
        Some(StatsMem {
            used: total.saturating_sub(avail),
            limit: total,
            limited: false,
            pressure10: read_capped(&proc.join("pressure/memory")).and_then(|t| parse_pressure(&t)),
            swap_used: swap_total
                .zip(swap_free)
                .map(|(t, f)| t.saturating_sub(f))
                .filter(|s| *s > 0),
        })
    }

    fn cpu(
        &self,
        cg: Option<(&Path, &Path)>,
        prev: Option<(Instant, CpuMark)>,
        now: Instant,
    ) -> (Option<StatsCpu>, CpuMark) {
        let mut mark = CpuMark::default();
        let mut out = StatsCpu::default();
        let host = self
            .roots
            .proc
            .as_deref()
            .and_then(|p| read_capped(&p.join("stat")))
            .and_then(|t| parse_proc_stat(&t))
            .or_else(|| self.roots.host_api.then(host::cpu).flatten());
        if let Some((_, _, cores)) = host {
            out.cores = (cores > 0).then_some(cores);
        }
        let quota = cg.and_then(|(root, rel)| nearest(root, rel, |d| parse_cpu_max(&read_capped(&d.join("cpu.max"))?)));
        let group = quota.as_ref().and_then(|(dir, q)| {
            let t = read_capped(&dir.join("cpu.stat"))?;
            Some((
                *q,
                kv(&t, "usage_usec")?,
                kv(&t, "nr_periods").unwrap_or(0),
                kv(&t, "nr_throttled").unwrap_or(0),
            ))
        });
        if let Some((q, usage, periods, throttled)) = group {
            // A container with a quota: its own time against what the quota allows.
            mark.group = Some((usage, periods, throttled));
            out.quota = Some((q * 100.0).round() / 100.0);
            if let Some((
                then,
                CpuMark {
                    group: Some((u0, p0, t0)),
                    ..
                },
            )) = prev
            {
                let wall_usec = now.saturating_duration_since(then).as_micros() as f64;
                if let Some(used) = usage.checked_sub(u0) {
                    out.pct = pct(used as f64, wall_usec * q);
                }
                if let (Some(dp), Some(dt)) = (periods.checked_sub(p0), throttled.checked_sub(t0)) {
                    out.throttled_pct = pct(dt as f64, dp as f64);
                }
            }
        } else if let Some((busy, all, _)) = host {
            mark.host = Some((busy, all));
            if let Some((
                _,
                CpuMark {
                    host: Some((b0, a0)), ..
                },
            )) = prev
            {
                if let (Some(db), Some(da)) = (busy.checked_sub(b0), all.checked_sub(a0)) {
                    out.pct = pct(db as f64, da as f64);
                }
            }
            out.load1 = self
                .roots
                .proc
                .as_deref()
                .and_then(|p| read_capped(&p.join("loadavg")))
                .and_then(|t| parse_loadavg(&t))
                .or_else(|| self.roots.host_api.then(host::load1).flatten());
        }
        let known = out != StatsCpu::default();
        (known.then_some(out), mark)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const GIB: u64 = 1024 * 1024 * 1024;

    fn stat(busy: u64, idle: u64, cores: usize) -> String {
        let mut s = format!("cpu  {busy} 0 0 {idle} 0 0 0 0 0 0\n");
        for i in 0..cores {
            s.push_str(&format!("cpu{i} 1 0 0 1 0 0 0 0 0 0\n"));
        }
        s.push_str("intr 12345 0 0\nctxt 99\n");
        s
    }

    const MEMINFO: &str = "MemTotal:       16000000 kB\nMemFree:         1000000 kB\nMemAvailable:   12000000 kB\nBuffers: 5 kB\nSwapTotal:       2000000 kB\nSwapFree:        1500000 kB\n";

    /// A host: `/proc` files, a cgroup tree without limits.
    fn host(dir: &Path) -> Roots {
        let proc = dir.join("proc");
        let cg = dir.join("cg");
        fs::create_dir_all(proc.join("7")).unwrap();
        fs::create_dir_all(proc.join("pressure")).unwrap();
        fs::create_dir_all(cg.join("user/session")).unwrap();
        fs::write(proc.join("7/cgroup"), "0::/user/session\n").unwrap();
        fs::write(proc.join("stat"), stat(100, 900, 4)).unwrap();
        fs::write(proc.join("meminfo"), MEMINFO).unwrap();
        fs::write(proc.join("loadavg"), "1.25 0.80 0.50 2/300 4242\n").unwrap();
        fs::write(
            proc.join("pressure/memory"),
            "some avg10=2.50 avg60=1.00 avg300=0.10 total=5\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=0\n",
        )
        .unwrap();
        fs::write(cg.join("memory.max"), "max\n").unwrap();
        fs::write(cg.join("cpu.max"), "max 100000\n").unwrap();
        fs::write(cg.join("user/session/memory.max"), "max\n").unwrap();
        fs::write(cg.join("user/session/memory.current"), "123\n").unwrap();
        Roots {
            proc: Some(proc),
            cgroup: Some(cg),
            home: dir.to_path_buf(),
            pid: 7,
            host_api: false,
        }
    }

    /// A container: the same host files (the node's), limits one level above the leaf.
    fn container(dir: &Path) -> Roots {
        let r = host(dir);
        let cg = r.cgroup.clone().unwrap();
        fs::create_dir_all(cg.join("pod/app")).unwrap();
        fs::write(r.proc.as_ref().unwrap().join("7/cgroup"), "0::/pod/app\n").unwrap();
        fs::write(cg.join("pod/app/memory.max"), "max\n").unwrap();
        fs::write(cg.join("pod/memory.max"), format!("{}\n", 4 * GIB)).unwrap();
        fs::write(cg.join("pod/memory.current"), format!("{}\n", 3 * GIB)).unwrap();
        fs::write(cg.join("pod/memory.swap.current"), "0\n").unwrap();
        fs::write(
            cg.join("pod/memory.pressure"),
            "some avg10=12.00 avg60=0 avg300=0 total=1\n",
        )
        .unwrap();
        fs::write(cg.join("pod/cpu.max"), "200000 100000\n").unwrap();
        fs::write(
            cg.join("pod/cpu.stat"),
            "usage_usec 1000000\nuser_usec 1\nnr_periods 100\nnr_throttled 10\nthrottled_usec 5\n",
        )
        .unwrap();
        r
    }

    fn started(roots: Roots, t0: Instant) -> Sampler {
        let mut s = Sampler::new(roots);
        s.set_rate(Some(0.5), t0);
        s
    }

    #[test]
    fn no_rate_means_no_sample_and_no_read() {
        let d = tempfile::tempdir().unwrap();
        let mut s = Sampler::new(host(d.path()));
        let t0 = Instant::now();
        for i in 0..1000 {
            assert_eq!(s.tick(t0 + Duration::from_secs(i), i), None);
        }
        assert_eq!(s.taken(), 0);
        assert!(!s.active());
        // Stopped again after a start: nothing more, and the CPU counters are forgotten.
        s.set_rate(Some(0.5), t0);
        assert!(s.tick(t0, 1).is_some());
        s.set_rate(None, t0);
        assert_eq!(s.tick(t0 + Duration::from_secs(60), 2), None);
        s.set_rate(Some(0.0), t0);
        assert_eq!(s.tick(t0 + Duration::from_secs(120), 3), None);
        assert_eq!(s.taken(), 1);
        assert!(s.mark.is_none());
    }

    #[test]
    fn samples_at_the_rate_asked_never_faster() {
        let d = tempfile::tempdir().unwrap();
        let t0 = Instant::now();
        let mut s = started(host(d.path()), t0);
        let mut n = 0;
        for ms in (0..10_000).step_by(20) {
            if s.tick(t0 + Duration::from_millis(ms), ms).is_some() {
                n += 1;
            }
        }
        assert_eq!(n, 5, "10 s at 0.5 Hz");
        // A rate above the most allowed is the most allowed.
        s.set_rate(Some(100.0), t0);
        assert_eq!(s.period, Some(Duration::from_secs(2)));
    }

    #[test]
    fn host_numbers_come_from_proc() {
        let d = tempfile::tempdir().unwrap();
        let t0 = Instant::now();
        let mut s = started(host(d.path()), t0);
        let a = s.tick(t0, 1000).unwrap();
        let cpu = a.cpu.unwrap();
        assert_eq!(cpu.pct, None, "the first sample has nothing to compare with");
        assert_eq!((cpu.cores, cpu.quota, cpu.load1), (Some(4), None, Some(1.25)));
        let mem = a.mem.unwrap();
        assert_eq!(
            (mem.used, mem.limit, mem.limited),
            (4_000_000 * 1024, 16_000_000 * 1024, false)
        );
        assert_eq!(mem.pressure10, Some(2.5));
        assert_eq!(mem.swap_used, Some(500_000 * 1024));
        assert_eq!(a.disks.len(), 1);
        assert_eq!(a.disks[0].id, "home");
        assert!(a.disks[0].free <= a.disks[0].total);

        // 300 more busy jiffies of 1000 more: 30 %.
        fs::write(d.path().join("proc/stat"), stat(400, 1600, 4)).unwrap();
        let b = s.tick(t0 + Duration::from_secs(2), 3000).unwrap();
        assert_eq!(b.cpu.unwrap().pct, Some(30.0));
    }

    #[test]
    fn a_container_reports_its_own_limits_not_the_nodes() {
        let d = tempfile::tempdir().unwrap();
        let t0 = Instant::now();
        let roots = container(d.path());
        let cg = roots.cgroup.clone().unwrap();
        let mut s = started(roots, t0);
        let a = s.tick(t0, 1).unwrap();
        let mem = a.mem.unwrap();
        assert_eq!((mem.used, mem.limit, mem.limited), (3 * GIB, 4 * GIB, true));
        assert_eq!(mem.pressure10, Some(12.0), "the cgroup's pressure, not the node's");
        assert_eq!(mem.swap_used, None);
        let cpu = a.cpu.unwrap();
        assert_eq!((cpu.pct, cpu.quota, cpu.cores), (None, Some(2.0), Some(4)));
        assert_eq!(cpu.load1, None, "the node's load says nothing about the container");

        // 2 s later: 1 s of CPU used of the 4 s a 2-core quota allows; 20 of 20 new periods throttled.
        fs::write(
            cg.join("pod/cpu.stat"),
            "usage_usec 2000000\nnr_periods 120\nnr_throttled 30\n",
        )
        .unwrap();
        let cpu = s.tick(t0 + Duration::from_secs(2), 2).unwrap().cpu.unwrap();
        assert_eq!(cpu.pct, Some(25.0));
        assert_eq!(cpu.throttled_pct, Some(100.0));
    }

    #[test]
    fn missing_files_leave_fields_out() {
        let d = tempfile::tempdir().unwrap();
        let t0 = Instant::now();
        // Nothing at all: not Linux.
        let mut s = started(
            Roots {
                proc: None,
                cgroup: None,
                home: d.path().to_path_buf(),
                pid: 7,
                host_api: false,
            },
            t0,
        );
        let a = s.tick(t0, 1).unwrap();
        assert_eq!((a.cpu, a.mem), (None, None));
        assert_eq!(a.disks.len(), 1);
        // Roots that do not exist, and a home that does not exist.
        let mut s = started(
            Roots {
                proc: Some(d.path().join("no-proc")),
                cgroup: Some(d.path().join("no-cg")),
                home: d.path().join("no-home"),
                pid: 7,
                host_api: false,
            },
            t0,
        );
        let a = s.tick(t0, 1).unwrap();
        assert_eq!((a.cpu, a.mem, a.disks.len()), (None, None, 0));
        // A host whose files go away between samples.
        let roots = host(d.path());
        let proc = roots.proc.clone().unwrap();
        let mut s = started(roots, t0);
        assert!(s.tick(t0, 1).unwrap().cpu.is_some());
        fs::remove_file(proc.join("stat")).unwrap();
        fs::remove_file(proc.join("meminfo")).unwrap();
        fs::remove_file(proc.join("loadavg")).unwrap();
        let b = s.tick(t0 + Duration::from_secs(2), 2).unwrap();
        assert_eq!((b.cpu, b.mem), (None, None));
    }

    #[test]
    fn malformed_and_truncated_files_never_panic_or_invent() {
        let d = tempfile::tempdir().unwrap();
        let t0 = Instant::now();
        let roots = container(d.path());
        let (proc, cg) = (roots.proc.clone().unwrap(), roots.cgroup.clone().unwrap());
        let junk: &[&[u8]] = &[
            b"",
            b"\n\n\n",
            b"cpu",
            b"cpu  \n",
            b"cpu  1 2\n",
            b"cpu  a b c d e\n",
            b"cpu  18446744073709551615 18446744073709551615 1 1\n",
            b"cpu  -1 -2 -3 -4\n",
            b"MemTotal:\nMemAvailable: kB\n",
            b"MemTotal: 99999999999999999999 kB\nMemAvailable: 1 kB\n",
            b"\xff\xfe\x00\x01 not text \x80",
            b"0::/../../etc\n",
            b"0::\n",
            b"max",
            b"-5\n",
            b"0 0\n",
            b"100000 0\n",
            b"some avg10=nan\n",
            b"some avg10=1e99\n",
            b"usage_usec\nnr_periods x\n",
        ];
        let files = [
            proc.join("stat"),
            proc.join("meminfo"),
            proc.join("loadavg"),
            proc.join("7/cgroup"),
            proc.join("pressure/memory"),
            cg.join("pod/memory.max"),
            cg.join("pod/memory.current"),
            cg.join("pod/memory.pressure"),
            cg.join("pod/cpu.max"),
            cg.join("pod/cpu.stat"),
        ];
        let mut s = started(roots, t0);
        let mut i = 0u64;
        for f in &files {
            let good = fs::read(f).unwrap();
            for j in junk {
                fs::write(f, j).unwrap();
                i += 1;
                let a = s.tick(t0 + Duration::from_secs(2 * i), i).unwrap();
                if let Some(c) = a.cpu {
                    assert!(c.pct.is_none_or(|p| (0.0..=100.0).contains(&p)), "{f:?} {j:?}");
                    assert!(c.throttled_pct.is_none_or(|p| (0.0..=100.0).contains(&p)));
                }
                if let Some(m) = a.mem {
                    assert!(m.limit > 0, "{f:?} {j:?}");
                }
            }
            fs::write(f, good).unwrap();
        }
        // Every file junk at once.
        for f in &files {
            fs::write(f, b"\x00\x00garbage").unwrap();
        }
        let a = s.tick(t0 + Duration::from_secs(9999), 0).unwrap();
        assert_eq!((a.cpu, a.mem), (None, None));
        // A cgroup path that tries to leave the mount is refused, not followed.
        assert_eq!(parse_cgroup_rel("0::/../../etc\n"), None);
        assert_eq!(parse_cgroup_rel("0::/a/b\n"), Some(PathBuf::from("a/b")));
        assert_eq!(parse_cgroup_rel("0::/\n"), Some(PathBuf::new()));
    }

    #[test]
    fn huge_files_are_read_only_to_the_cap() {
        let d = tempfile::tempdir().unwrap();
        let t0 = Instant::now();
        let roots = host(d.path());
        let proc = roots.proc.clone().unwrap();
        // A real first part, then 8 MiB of interrupt counters.
        let mut big = stat(100, 900, 4).into_bytes();
        big.extend(std::iter::repeat_n(b'7', 8 * 1024 * 1024));
        fs::write(proc.join("stat"), &big).unwrap();
        assert_eq!(read_capped(&proc.join("stat")).unwrap().len() as u64, READ_CAP);
        // A number that never ends is not a number.
        fs::write(proc.join("loadavg"), vec![b'9'; 4 * 1024 * 1024]).unwrap();
        // A meminfo whose keys come after the cap is as good as missing.
        let mut late = vec![b'\n'; READ_CAP as usize + 10];
        late.extend_from_slice(MEMINFO.as_bytes());
        fs::write(proc.join("meminfo"), late).unwrap();
        let mut s = started(roots, t0);
        let a = s.tick(t0, 1).unwrap();
        let cpu = a.cpu.unwrap();
        assert_eq!(cpu.cores, Some(4));
        assert_eq!(cpu.load1, None);
        assert_eq!(a.mem, None);
    }

    #[test]
    fn counters_that_go_backwards_give_no_percentage() {
        let d = tempfile::tempdir().unwrap();
        let t0 = Instant::now();
        let mut s = started(host(d.path()), t0);
        s.tick(t0, 1).unwrap();
        fs::write(d.path().join("proc/stat"), stat(1, 2, 4)).unwrap();
        let b = s.tick(t0 + Duration::from_secs(2), 2).unwrap();
        assert_eq!(b.cpu.unwrap().pct, None);
    }

    #[test]
    fn a_sample_is_small_and_holds_only_numbers_and_the_word_home() {
        let d = tempfile::tempdir().unwrap();
        let t0 = Instant::now();
        let mut s = started(container(d.path()), t0);
        s.tick(t0, 1).unwrap();
        let a = s.tick(t0 + Duration::from_secs(2), 1_800_000_000_000).unwrap();
        let v = serde_json::to_value(tmuxedo_protocol::frames::RunnerFrame::Stats(a)).unwrap();
        let text = v.to_string();
        assert!(text.len() < 400, "{} bytes: {text}", text.len());
        fn strings(v: &serde_json::Value, out: &mut Vec<String>) {
            match v {
                serde_json::Value::String(s) => out.push(s.clone()),
                serde_json::Value::Array(a) => a.iter().for_each(|x| strings(x, out)),
                serde_json::Value::Object(o) => o.values().for_each(|x| strings(x, out)),
                _ => {}
            }
        }
        let mut found = vec![];
        strings(&v, &mut found);
        found.sort();
        assert_eq!(found, ["home", "stats"]);
    }

    #[test]
    fn parsers_take_the_usual_shapes() {
        assert_eq!(
            parse_proc_stat("cpu  10 0 5 80 5 0 0 0 0 0\ncpu0 1 1 1 1\n"),
            Some((15, 100, 1))
        );
        assert_eq!(parse_cpu_max("max 100000"), None);
        assert_eq!(parse_cpu_max("150000 100000\n"), Some(1.5));
        assert_eq!(
            parse_pressure("some avg10=0.00 avg60=0.00\nfull avg10=9.00\n"),
            Some(0.0)
        );
        assert_eq!(parse_pressure("full avg10=9.00\n"), None);
        assert_eq!(parse_loadavg("0.10 0.20 0.30 1/2 3\n"), Some(0.1));
        assert_eq!(kv("a 1\nb 2\n", "b"), Some(2));
        assert_eq!(kv("ab 1\n", "a"), None);
    }

    /// `vm_stat` output (fixture strings from an Apple-silicon and an Intel Mac) → page counts and page size.
    fn vm_stat(text: &str) -> (MacVm, u64) {
        let page = text
            .split("page size of ")
            .nth(1)
            .and_then(|r| r.split_whitespace().next())
            .and_then(|n| n.parse().ok())
            .unwrap_or(0);
        let n = |key: &str| {
            text.lines()
                .find_map(|l| l.strip_prefix(key)?.strip_prefix(':'))
                .and_then(|v| v.trim().trim_end_matches('.').parse::<u64>().ok())
                .unwrap_or(0)
        };
        let vm = MacVm {
            free: n("Pages free"),
            active: n("Pages active"),
            inactive: n("Pages inactive"),
            speculative: n("Pages speculative"),
            wired: n("Pages wired down"),
            purgeable: n("Pages purgeable"),
            compressor: n("Pages occupied by compressor"),
            internal: n("Anonymous pages"),
            external: n("File-backed pages"),
        };
        (vm, page)
    }

    const VM_STAT_ARM: &str = "Mach Virtual Memory Statistics: (page size of 16384 bytes)
Pages free:                               12345.
Pages active:                            250000.
Pages inactive:                          240000.
Pages speculative:                         5000.
Pages throttled:                              0.
Pages wired down:                        100000.
Pages purgeable:                          10000.
\"Translation faults\":                 123456789.
Pages copy-on-write:                    1234567.
Pages zero filled:                     98765432.
Pages reactivated:                        12345.
Pages purged:                              6789.
File-backed pages:                       200000.
Anonymous pages:                         295000.
Pages stored in compressor:              150000.
Pages occupied by compressor:             50000.
Decompressions:                          100000.
Compressions:                            200000.
Pageins:                                 300000.
Pageouts:                                  1000.
Swapins:                                      0.
Swapouts:                                     0.
";

    const VM_STAT_INTEL: &str = "Mach Virtual Memory Statistics: (page size of 4096 bytes)
Pages free:                                1000.
Pages active:                            900000.
Pages inactive:                          800000.
Pages speculative:                          100.
Pages throttled:                              0.
Pages wired down:                        500000.
Pages purgeable:                              0.
File-backed pages:                       300000.
Anonymous pages:                        1400000.
Pages occupied by compressor:            400000.
";

    #[test]
    fn mac_memory_counts_app_wired_and_compressed_against_physical() {
        let (vm, page) = vm_stat(VM_STAT_ARM);
        assert_eq!(page, 16384);
        assert_eq!(
            (vm.internal, vm.purgeable, vm.wired, vm.compressor),
            (295_000, 10_000, 100_000, 50_000)
        );
        // A 16 GiB Mac: app 285000 + wired 100000 + compressed 50000 pages = 435000 × 16 KiB.
        let m = mac_mem(&vm, page, 16 * GIB).unwrap();
        assert_eq!((m.used, m.limit, m.limited), (435_000 * 16384, 16 * GIB, false));
        assert!((m.used as f64 / m.limit as f64 - 0.4148).abs() < 0.001);
        assert_eq!((m.pressure10, m.swap_used), (None, None));
        // Intel, 4 KiB pages, an 8 GiB machine reporting more than it has: capped at physical.
        let (vm, page) = vm_stat(VM_STAT_INTEL);
        assert_eq!(page, 4096);
        let m = mac_mem(&vm, page, 8 * GIB).unwrap();
        assert_eq!(m.used, 8 * GIB, "2.3 M pages × 4 KiB > 8 GiB: never above the limit");
        // Junk: no page size, no memory, empty output.
        assert_eq!(mac_mem(&vm, 0, 8 * GIB), None);
        assert_eq!(mac_mem(&vm, page, 0), None);
        let (empty, p) = vm_stat("");
        assert_eq!((empty, p), (MacVm::default(), 0));
        assert_eq!(mac_mem(&empty, 4096, GIB).unwrap().used, 0);
        // Purgeable larger than anonymous (a stale snapshot) never goes negative.
        let odd = MacVm {
            internal: 5,
            purgeable: 9,
            ..MacVm::default()
        };
        assert_eq!(mac_mem(&odd, 4096, GIB).unwrap().used, 0);
    }

    #[test]
    fn mac_cpu_ticks_give_the_proc_stat_pair() {
        // user, system, idle, nice
        assert_eq!(mac_cpu_ticks([300, 100, 550, 50]), (450, 1000));
        assert_eq!(
            mac_cpu_ticks([u32::MAX, u32::MAX, u32::MAX, u32::MAX]).1,
            4 * u64::from(u32::MAX)
        );
        // Two readings → a share, as on Linux; a counter that wrapped gives none.
        let (b0, a0) = mac_cpu_ticks([300, 100, 550, 50]);
        let (b1, a1) = mac_cpu_ticks([400, 150, 600, 50]);
        assert_eq!(pct((b1 - b0) as f64, (a1 - a0) as f64), Some(75.0));
        let (b2, _) = mac_cpu_ticks([1, 0, 0, 0]);
        assert_eq!(b2.checked_sub(b1), None);
    }

    #[test]
    fn host_api_off_or_not_macos_reads_nothing() {
        let d = tempfile::tempdir().unwrap();
        let t0 = Instant::now();
        let mut s = started(
            Roots {
                proc: None,
                cgroup: None,
                home: d.path().to_path_buf(),
                pid: 7,
                host_api: cfg!(target_os = "macos"),
            },
            t0,
        );
        let a = s.tick(t0, 1).unwrap();
        if cfg!(target_os = "macos") {
            let m = a.mem.expect("macOS reports memory");
            assert!(m.used > 0 && m.used <= m.limit && !m.limited);
            assert!(a.cpu.and_then(|c| c.cores).is_some_and(|n| n > 0));
        } else {
            assert_eq!((a.cpu, a.mem), (None, None));
        }
    }
}
