//! Keep-awake: hold the OS sleep inhibitor while an agent in a pane works
//! (docs/protocol.md "Keep-awake and Wake-on-LAN").
//!
//! The inhibitor is a child process, so nothing new links in and the hold
//! ends with the process: `systemd-inhibit` on Linux (systemd hosts only),
//! `caffeinate -i -w <runner pid>` on macOS. Both watch the runner's pid,
//! so a runner that dies without releasing still lets the machine sleep.
//! The hold lasts until [`GRACE`] after the last agent stops working, and
//! is skipped on a battery below [`LOW_BATTERY`] percent.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tmuxedo_protocol::wake::KeepAwake as Wire;

/// How long the inhibitor stays held after the last agent stops working.
pub const GRACE: Duration = Duration::from_secs(5 * 60);
/// Below this charge, on battery, the machine is let sleep.
pub const LOW_BATTERY: u8 = 20;
/// The shortest gap between attempts to take an inhibitor that failed.
const RETRY: Duration = Duration::from_secs(60);

/// Something that keeps the OS from sleeping while held.
pub trait Inhibitor: Send {
    /// This OS has the inhibitor.
    fn available(&self) -> bool;
    /// Take it; Err when it could not be taken.
    fn hold(&mut self) -> Result<(), String>;
    fn release(&mut self);
    /// Held now (a hold whose process died is not).
    fn held(&mut self) -> bool;
}

/// The OS's inhibitor, as a child process in its own process group.
pub struct ChildInhibitor {
    argv: Option<Vec<String>>,
    child: Option<Child>,
}

impl ChildInhibitor {
    /// The inhibitor for this OS (unavailable where there is none).
    pub fn for_this_os() -> Self {
        Self {
            argv: os_argv(std::process::id()),
            child: None,
        }
    }
}

#[cfg(target_os = "linux")]
fn os_argv(pid: u32) -> Option<Vec<String>> {
    // logind only on a systemd host (not in containers).
    if !Path::new("/run/systemd/system").is_dir() {
        return None;
    }
    let exe = on_path("systemd-inhibit")?;
    Some(vec![
        exe.display().to_string(),
        "--what=idle:sleep".into(),
        "--who=tmuxedo".into(),
        "--why=An agent is working".into(),
        "--mode=block".into(),
        "sh".into(),
        "-c".into(),
        format!("while kill -0 {pid} 2>/dev/null; do sleep 30; done"),
    ])
}

#[cfg(target_os = "linux")]
fn on_path(name: &str) -> Option<std::path::PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

#[cfg(target_os = "macos")]
fn os_argv(pid: u32) -> Option<Vec<String>> {
    let exe = Path::new("/usr/bin/caffeinate");
    exe.exists()
        .then(|| vec![exe.display().to_string(), "-i".into(), "-w".into(), pid.to_string()])
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn os_argv(_pid: u32) -> Option<Vec<String>> {
    None
}

impl Inhibitor for ChildInhibitor {
    fn available(&self) -> bool {
        self.argv.is_some()
    }

    fn hold(&mut self) -> Result<(), String> {
        use std::os::unix::process::CommandExt;
        if self.held() {
            return Ok(());
        }
        let argv = self.argv.as_ref().ok_or("no sleep inhibitor on this OS")?;
        let child = Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .map_err(|e| format!("{}: {e}", argv[0]))?;
        self.child = Some(child);
        Ok(())
    }

    fn release(&mut self) {
        if let Some(mut c) = self.child.take() {
            // The whole group: systemd-inhibit and its watcher shell.
            if let Ok(pid) = i32::try_from(c.id()) {
                unsafe {
                    libc::kill(-pid, libc::SIGKILL);
                }
            }
            let _ = c.wait();
        }
    }

    fn held(&mut self) -> bool {
        match self.child.as_mut().map(|c| c.try_wait()) {
            Some(Ok(None)) => true,
            Some(_) => {
                self.child = None;
                false
            }
            None => false,
        }
    }
}

impl Drop for ChildInhibitor {
    fn drop(&mut self) {
        self.release();
    }
}

/// When to hold the inhibitor.
pub struct KeepAwake {
    inh: Box<dyn Inhibitor>,
    enabled: bool,
    /// When the last agent stopped working while held.
    idle_since: Option<Instant>,
    battery_low: bool,
    working: usize,
    failed_at: Option<Instant>,
}

impl KeepAwake {
    pub fn new(inh: Box<dyn Inhibitor>, enabled: bool) -> Self {
        Self {
            inh,
            enabled,
            idle_since: None,
            battery_low: false,
            working: 0,
            failed_at: None,
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// The owner's setting; off releases at once.
    pub fn set_enabled(&mut self, on: bool, now: Instant) {
        self.enabled = on;
        self.update(self.working, self.battery_low, now);
    }

    /// Take or release for `working` agents now. True when the hold changed.
    pub fn update(&mut self, working: usize, battery_low: bool, now: Instant) -> bool {
        self.working = working;
        self.battery_low = battery_low;
        let held = self.inh.held();
        let mut want = if working > 0 {
            self.idle_since = None;
            true
        } else if held {
            let since = *self.idle_since.get_or_insert(now);
            now.duration_since(since) < GRACE
        } else {
            false
        };
        want &= self.enabled && self.inh.available() && !battery_low;
        if want && !held {
            if self.failed_at.is_some_and(|t| now.duration_since(t) < RETRY) {
                return false;
            }
            return match self.inh.hold() {
                Ok(()) => {
                    self.failed_at = None;
                    crate::log!("keep-awake: held ({working} agent(s) working)");
                    true
                }
                Err(e) => {
                    self.failed_at = Some(now);
                    crate::log!("keep-awake: could not hold: {e}");
                    false
                }
            };
        }
        if !want && held {
            self.inh.release();
            self.idle_since = None;
            crate::log!("keep-awake: released");
            return true;
        }
        false
    }

    /// Let go now (shutdown).
    pub fn release(&mut self) {
        if self.inh.held() {
            self.inh.release();
            crate::log!("keep-awake: released");
        }
        self.idle_since = None;
    }

    /// `hello.runner.keepAwake`.
    pub fn wire(&mut self) -> Wire {
        let available = self.inh.available();
        let held = self.inh.held();
        let reason =
            (self.working > 0 && self.enabled && available && self.battery_low && !held).then(|| "battery".to_string());
        Wire {
            enabled: self.enabled,
            available,
            held,
            reason,
        }
    }
}

/// On battery below [`LOW_BATTERY`] percent (Linux sysfs, macOS `pmset -g
/// batt`; false elsewhere, on mains power, or when it cannot be read).
pub fn battery_low() -> bool {
    #[cfg(target_os = "linux")]
    {
        battery_low_in(Path::new("/sys/class/power_supply"))
    }
    #[cfg(target_os = "macos")]
    {
        mac_battery_low()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        false
    }
}

/// How long a macOS battery reading is reused (`pmset` is a process spawn;
/// the keep-awake tick runs every 5 s).
#[cfg(target_os = "macos")]
const PMSET_EVERY: Duration = Duration::from_secs(60);

#[cfg(target_os = "macos")]
fn mac_battery_low() -> bool {
    use std::sync::Mutex;
    static LAST: Mutex<Option<(Instant, bool)>> = Mutex::new(None);
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((at, low)) = *last {
        if at.elapsed() < PMSET_EVERY {
            return low;
        }
    }
    let low = Command::new("/usr/bin/pmset")
        .args(["-g", "batt"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .is_some_and(|o| pmset_battery_low(&String::from_utf8_lossy(&o.stdout)));
    *last = Some((Instant::now(), low));
    low
}

/// [`battery_low`] over `pmset -g batt` output: drawing from battery power,
/// an internal battery discharging, below [`LOW_BATTERY`] percent. Pure, so
/// it is tested on every OS. A UPS line or a Mac with no battery is never low.
/// For example (the gap before the percentage is a tab in the real output):
/// ```text
/// Now drawing from 'Battery Power'
///  -InternalBattery-0 (id=1234567)    15%; discharging; 0:42 remaining present: true
/// ```
pub fn pmset_battery_low(out: &str) -> bool {
    let mut lines = out.lines();
    let on_battery = lines.next().is_some_and(|l| l.contains("'Battery Power'"));
    if !on_battery {
        return false;
    }
    lines.any(|l| {
        let l = l.trim();
        if !l.starts_with("-InternalBattery") {
            return false;
        }
        let Some(rest) = l
            .split_once('\t')
            .map(|(_, r)| r)
            .or_else(|| l.split_once(')').map(|(_, r)| r))
        else {
            return false;
        };
        let mut fields = rest.split(';').map(str::trim);
        let pct = fields
            .next()
            .and_then(|p| p.strip_suffix('%'))
            .and_then(|p| p.trim().parse::<u8>().ok());
        let state = fields.next().unwrap_or("");
        state == "discharging" && pct.is_some_and(|p| p < LOW_BATTERY)
    })
}

/// [`battery_low`] over a power_supply folder.
pub fn battery_low_in(dir: &Path) -> bool {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return false;
    };
    for e in rd.flatten() {
        let p = e.path();
        let kind = std::fs::read_to_string(p.join("type")).unwrap_or_default();
        if kind.trim() != "Battery" {
            continue;
        }
        let status = std::fs::read_to_string(p.join("status")).unwrap_or_default();
        let cap = std::fs::read_to_string(p.join("capacity"))
            .ok()
            .and_then(|c| c.trim().parse::<u8>().ok());
        if status.trim() == "Discharging" && cap.is_some_and(|c| c < LOW_BATTERY) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct Log {
        held: bool,
        holds: u32,
        releases: u32,
        fail: bool,
    }

    struct Fake(Arc<Mutex<Log>>, bool);

    impl Inhibitor for Fake {
        fn available(&self) -> bool {
            self.1
        }
        fn hold(&mut self) -> Result<(), String> {
            let mut l = self.0.lock().unwrap();
            if l.fail {
                return Err("no".into());
            }
            l.held = true;
            l.holds += 1;
            Ok(())
        }
        fn release(&mut self) {
            let mut l = self.0.lock().unwrap();
            l.held = false;
            l.releases += 1;
        }
        fn held(&mut self) -> bool {
            self.0.lock().unwrap().held
        }
    }

    fn ka(enabled: bool, available: bool) -> (KeepAwake, Arc<Mutex<Log>>) {
        let log = Arc::new(Mutex::new(Log::default()));
        (KeepAwake::new(Box::new(Fake(log.clone(), available)), enabled), log)
    }

    #[test]
    fn holds_while_working_and_releases_after_grace() {
        let (mut k, log) = ka(true, true);
        let t = Instant::now();
        assert!(!k.update(0, false, t));
        assert_eq!(log.lock().unwrap().holds, 0);
        assert!(k.update(2, false, t));
        assert!(k.wire().held);
        // Idle: still held inside the grace.
        assert!(!k.update(0, false, t + Duration::from_secs(10)));
        assert!(!k.update(0, false, t + Duration::from_secs(10) + GRACE - Duration::from_secs(1)));
        assert!(log.lock().unwrap().held);
        // Work again resets the grace.
        k.update(1, false, t + Duration::from_secs(200));
        // The grace counts from the first idle look after the work stopped.
        assert!(!k.update(0, false, t + Duration::from_secs(201)));
        assert!(!k.update(0, false, t + Duration::from_secs(201) + GRACE - Duration::from_secs(1)));
        assert!(log.lock().unwrap().held);
        assert!(k.update(0, false, t + Duration::from_secs(201) + GRACE));
        let l = log.lock().unwrap();
        assert!(!l.held);
        assert_eq!((l.holds, l.releases), (1, 1));
    }

    #[test]
    fn off_unavailable_and_battery() {
        let (mut k, log) = ka(false, true);
        let t = Instant::now();
        k.update(1, false, t);
        assert_eq!(log.lock().unwrap().holds, 0);
        k.set_enabled(true, t);
        assert!(log.lock().unwrap().held);
        k.set_enabled(false, t);
        assert!(!log.lock().unwrap().held);
        assert!(!k.wire().enabled);

        let (mut k, log) = ka(true, false);
        k.update(1, false, t);
        assert_eq!(log.lock().unwrap().holds, 0);
        assert!(!k.wire().available);

        let (mut k, log) = ka(true, true);
        k.update(1, false, t);
        assert!(log.lock().unwrap().held);
        // Battery low: released at once, and said why.
        k.update(1, true, t);
        assert!(!log.lock().unwrap().held);
        assert_eq!(k.wire().reason.as_deref(), Some("battery"));
    }

    #[test]
    fn failure_backs_off_and_shutdown_releases() {
        let (mut k, log) = ka(true, true);
        let t = Instant::now();
        log.lock().unwrap().fail = true;
        assert!(!k.update(1, false, t));
        log.lock().unwrap().fail = false;
        assert!(!k.update(1, false, t + Duration::from_secs(5)));
        assert!(k.update(1, false, t + RETRY));
        k.release();
        let l = log.lock().unwrap();
        assert!(!l.held);
        assert_eq!(l.releases, 1);
    }

    #[test]
    fn battery_from_sysfs() {
        let d = tempfile::tempdir().unwrap();
        let bat = d.path().join("BAT0");
        std::fs::create_dir(&bat).unwrap();
        std::fs::write(bat.join("type"), "Battery\n").unwrap();
        std::fs::write(bat.join("status"), "Discharging\n").unwrap();
        std::fs::write(bat.join("capacity"), "15\n").unwrap();
        assert!(battery_low_in(d.path()));
        std::fs::write(bat.join("status"), "Charging\n").unwrap();
        assert!(!battery_low_in(d.path()));
        std::fs::write(bat.join("status"), "Discharging\n").unwrap();
        std::fs::write(bat.join("capacity"), "40\n").unwrap();
        assert!(!battery_low_in(d.path()));
        assert!(!battery_low_in(&d.path().join("missing")));
    }

    #[test]
    fn battery_from_pmset() {
        let low = "Now drawing from 'Battery Power'\n -InternalBattery-0 (id=1234567)\t15%; discharging; 0:42 remaining present: true\n";
        assert!(pmset_battery_low(low));
        // Above the line, on battery.
        assert!(!pmset_battery_low(
            "Now drawing from 'Battery Power'\n -InternalBattery-0 (id=1234567)\t64%; discharging; 3:10 remaining present: true\n"
        ));
        // Plugged in, still low: charging, or attached but not charging.
        assert!(!pmset_battery_low(
            "Now drawing from 'AC Power'\n -InternalBattery-0 (id=1234567)\t12%; charging; 1:30 remaining present: true\n"
        ));
        assert!(!pmset_battery_low(
            "Now drawing from 'AC Power'\n -InternalBattery-0 (id=1234567)\t12%; AC attached; not charging present: true\n"
        ));
        // Fully charged on power; a desktop Mac with no battery at all.
        assert!(!pmset_battery_low(
            "Now drawing from 'AC Power'\n -InternalBattery-0 (id=1234567)\t100%; charged; 0:00 remaining present: true\n"
        ));
        assert!(!pmset_battery_low("Now drawing from 'AC Power'\n"));
        // A UPS is not the Mac's battery; junk and empty output are never low.
        assert!(!pmset_battery_low(
            "Now drawing from 'Battery Power'\n -Back-UPS 700 (id=1234)\t10%; discharging; (no estimate) present: true\n"
        ));
        assert!(!pmset_battery_low(
            "Now drawing from 'Battery Power'\n -InternalBattery-0 garbage\n"
        ));
        assert!(!pmset_battery_low(""));
        // Spaces instead of a tab after the id still parse.
        assert!(pmset_battery_low(
            "Now drawing from 'Battery Power'\n -InternalBattery-0 (id=1234567)  9%; discharging; (no estimate) present: true\n"
        ));
    }
}
