//! `install-service` / `uninstall-service`: two user services, the holder and
//! the runner, so restarting or upgrading the runner never touches the
//! holder (decision 0001). systemd --user on Linux, LaunchAgents on macOS.
//!
//! **Moving from the aimux-era services** (`aimux-holder.service` /
//! `aimux-runner.service`, LaunchAgents `ai.aimux.*`) without ending a session:
//! the old runner is stopped and removed (the runner holds no sessions); the
//! old holder is disabled and its unit file removed but it is NOT stopped, so
//! it keeps every PTY until it exits on its own (logout without lingering,
//! reboot, or `systemctl --user stop aimux-holder.service`). Meanwhile the new
//! holder runs `holder --wait` and stands by, and the new runner attaches to
//! the old holder (daemon::connect_holder). When the old holder is gone the
//! new one binds and the runner (restarted by its service) moves over.

use std::path::{Path, PathBuf};
use std::process::Command;

pub const HOLDER_UNIT: &str = "tmuxedo-holder.service";
pub const RUNNER_UNIT: &str = "tmuxedo-runner.service";
pub const HOLDER_LABEL: &str = "dev.tmuxedo.holder";
pub const RUNNER_LABEL: &str = "dev.tmuxedo.runner";
pub const LEGACY_HOLDER_UNIT: &str = "aimux-holder.service";
pub const LEGACY_RUNNER_UNIT: &str = "aimux-runner.service";
pub const LEGACY_HOLDER_LABEL: &str = "ai.aimux.holder";
pub const LEGACY_RUNNER_LABEL: &str = "ai.aimux.runner";

pub fn holder_unit(exe: &Path) -> String {
    format!(
        "[Unit]
Description=tmuxedo PTY holder (keeps terminal sessions alive across runner restarts)
Documentation=https://tmuxedo.dev

[Service]
Type=simple
# --wait: while an older holder still serves sessions, stand by until it exits.
ExecStart={exe} holder --wait
Restart=always
RestartSec=2
# Stopping the holder ends every session in it; that is what stopping it means.
KillMode=control-group

[Install]
WantedBy=default.target
",
        exe = exe.display()
    )
}

pub fn runner_unit(exe: &Path) -> String {
    format!(
        "[Unit]
Description=tmuxedo runner (connects this machine to tmuxedo)
Documentation=https://tmuxedo.dev
Requires={HOLDER_UNIT}
After={HOLDER_UNIT} network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart={exe} run --no-start-holder
Restart=always
RestartSec=2
# 78: token revoked or replaced by another runner; do not loop.
RestartPreventExitStatus={code}

[Install]
WantedBy=default.target
",
        exe = exe.display(),
        code = crate::EXIT_NO_RESTART
    )
}

pub fn launch_agent(label: &str, exe: &Path, args: &[&str], log: &Path) -> String {
    let args_xml: String = std::iter::once(exe.display().to_string())
        .chain(args.iter().map(|s| s.to_string()))
        .map(|a| format!("    <string>{}</string>\n", xml_escape(&a)))
        .collect();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{label}</string>
  <key>ProgramArguments</key>
  <array>
{args_xml}  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>ProcessType</key>
  <string>Interactive</string>
  <key>StandardOutPath</key>
  <string>{log}</string>
  <key>StandardErrorPath</key>
  <string>{log}</string>
</dict>
</plist>
"#,
        log = xml_escape(&log.display().to_string())
    )
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn systemd_dir() -> PathBuf {
    crate::paths::config_home().join("systemd/user")
}

fn launch_agents_dir() -> PathBuf {
    tmuxedo_holder::home_dir().join("Library/LaunchAgents")
}

/// One thing [`install`] / [`uninstall`] does, in order. A plan is built
/// first (pure, unit-tested) and then executed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Write(PathBuf, String),
    Remove(PathBuf),
    /// A command whose failure aborts the install.
    Run(Vec<String>),
    /// A command whose failure is ignored (e.g. stopping what is not running).
    Try(Vec<String>),
    Say(String),
}

fn cmd(args: &[&str]) -> Vec<String> {
    args.iter().map(|s| s.to_string()).collect()
}

/// systemd --user: install (or refresh) the tmuxedo units and retire the
/// aimux-era ones without stopping the old holder.
pub fn systemd_install_plan(dir: &Path, exe: &Path, legacy_runner: bool, legacy_holder: bool) -> Vec<Step> {
    let mut s = vec![
        Step::Write(dir.join(HOLDER_UNIT), holder_unit(exe)),
        Step::Write(dir.join(RUNNER_UNIT), runner_unit(exe)),
        Step::Say(format!("wrote {HOLDER_UNIT} and {RUNNER_UNIT} in {}", dir.display())),
    ];
    if legacy_runner {
        // The runner holds no sessions: stopping it is free.
        s.push(Step::Try(cmd(&[
            "systemctl",
            "--user",
            "disable",
            "--now",
            LEGACY_RUNNER_UNIT,
        ])));
        s.push(Step::Remove(dir.join(LEGACY_RUNNER_UNIT)));
    }
    if legacy_holder {
        // Disabled and its file removed, but left running: it keeps every
        // session until it exits; tmuxedo-holder (--wait) takes over then.
        s.push(Step::Try(cmd(&["systemctl", "--user", "disable", LEGACY_HOLDER_UNIT])));
        s.push(Step::Remove(dir.join(LEGACY_HOLDER_UNIT)));
    }
    s.push(Step::Run(cmd(&["systemctl", "--user", "daemon-reload"])));
    s.push(Step::Run(cmd(&[
        "systemctl",
        "--user",
        "enable",
        "--now",
        HOLDER_UNIT,
        RUNNER_UNIT,
    ])));
    if legacy_holder {
        s.push(Step::Say(format!(
            "moved from the aimux services. The old holder ({LEGACY_HOLDER_UNIT}) keeps your current terminal \
             sessions until it exits; the runner uses it meanwhile and {HOLDER_UNIT} takes over afterwards. \
             To switch now (ENDS those sessions; agents come back with `claude --resume`): \
             systemctl --user stop {LEGACY_HOLDER_UNIT}"
        )));
    }
    s
}

/// LaunchAgents: same idea; the old holder job stays loaded until it exits
/// or the user logs out, its plist is removed so it never loads again.
pub fn launchd_install_plan(
    agents: &Path,
    logs: &Path,
    uid: u32,
    exe: &Path,
    legacy_runner: bool,
    legacy_holder: bool,
) -> Vec<Step> {
    let target = format!("gui/{uid}");
    let mut s = vec![];
    if legacy_runner {
        let p = agents.join(format!("{LEGACY_RUNNER_LABEL}.plist"));
        s.push(Step::Try(cmd(&["launchctl", "bootout", &target, &p.to_string_lossy()])));
        s.push(Step::Remove(p));
    }
    if legacy_holder {
        s.push(Step::Remove(agents.join(format!("{LEGACY_HOLDER_LABEL}.plist"))));
    }
    for (label, args, log) in [
        (HOLDER_LABEL, vec!["holder", "--wait"], logs.join("tmuxedo-holder.log")),
        (RUNNER_LABEL, vec!["run"], logs.join("tmuxedo-runner.log")),
    ] {
        let path = agents.join(format!("{label}.plist"));
        let p = path.to_string_lossy().into_owned();
        s.push(Step::Write(path.clone(), launch_agent(label, exe, &args, &log)));
        s.push(Step::Try(cmd(&["launchctl", "bootout", &target, &p])));
        s.push(Step::Run(cmd(&["launchctl", "bootstrap", &target, &p])));
        s.push(Step::Say(format!("installed {p}")));
    }
    if legacy_holder {
        s.push(Step::Say(format!(
            "moved from the aimux LaunchAgents. The old holder ({LEGACY_HOLDER_LABEL}) keeps your current terminal \
             sessions until it exits (at the latest when you log out); {HOLDER_LABEL} takes over afterwards. \
             To switch now (ENDS those sessions): launchctl bootout {target}/{LEGACY_HOLDER_LABEL}"
        )));
    }
    s
}

fn execute(steps: Vec<Step>) -> Result<(), String> {
    for step in steps {
        match step {
            Step::Write(p, body) => std::fs::write(&p, body).map_err(|e| format!("{}: {e}", p.display()))?,
            Step::Remove(p) => {
                let _ = std::fs::remove_file(&p);
            }
            Step::Run(a) => {
                let r = run(&a[0], &a[1..].iter().map(String::as_str).collect::<Vec<_>>());
                // launchctl bootstrap fails on older macOS: fall back to load -w.
                if r.is_err() && a[0] == "launchctl" && a.get(1).is_some_and(|x| x == "bootstrap") {
                    run("launchctl", &["load", "-w", &a[3]])?;
                } else {
                    r?;
                }
            }
            Step::Try(a) => {
                let _ = run(&a[0], &a[1..].iter().map(String::as_str).collect::<Vec<_>>());
            }
            Step::Say(m) => eprintln!("{m}"),
        }
    }
    Ok(())
}

fn run(cmd: &str, args: &[&str]) -> Result<(), String> {
    let st = Command::new(cmd)
        .args(args)
        .status()
        .map_err(|e| format!("{cmd}: {e}"))?;
    if st.success() {
        Ok(())
    } else {
        Err(format!("{cmd} {} failed ({st})", args.join(" ")))
    }
}

pub fn install() -> Result<(), String> {
    let exe = crate::update::current_exe()?;
    if let Some(dir) = exe.parent() {
        // `tmuxedo` (CLI) and the deprecated aliases beside the binary.
        if exe.file_name().and_then(|n| n.to_str()) == Some(crate::migrate::BIN) {
            if let Err(e) = crate::migrate::ensure_aliases(dir) {
                eprintln!("note: {e}");
            }
        }
    }
    if cfg!(target_os = "macos") {
        let agents = launch_agents_dir();
        std::fs::create_dir_all(&agents).map_err(|e| e.to_string())?;
        let logs = tmuxedo_holder::home_dir().join("Library/Logs");
        let _ = std::fs::create_dir_all(&logs);
        let uid = unsafe { libc::getuid() };
        let lr = agents.join(format!("{LEGACY_RUNNER_LABEL}.plist")).exists();
        let lh = agents.join(format!("{LEGACY_HOLDER_LABEL}.plist")).exists();
        return execute(launchd_install_plan(&agents, &logs, uid, &exe, lr, lh));
    }
    let dir = systemd_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let lr = dir.join(LEGACY_RUNNER_UNIT).exists();
    let lh = dir.join(LEGACY_HOLDER_UNIT).exists();
    execute(systemd_install_plan(&dir, &exe, lr, lh))?;
    linger_hint();
    Ok(())
}

fn linger_hint() {
    let user = std::env::var("USER").unwrap_or_default();
    let out = Command::new("loginctl")
        .args(["show-user", &user, "-p", "Linger"])
        .output();
    let lingering = matches!(&out, Ok(o) if String::from_utf8_lossy(&o.stdout).contains("Linger=yes"));
    if !lingering {
        eprintln!();
        eprintln!("  Note: without lingering, your user services stop when you log out.");
        eprintln!("  To keep sessions running: sudo loginctl enable-linger {user}");
    }
}

/// Stop and remove the services, the tmuxedo ones and any aimux-era ones.
pub fn uninstall() -> Result<(), String> {
    if cfg!(target_os = "macos") {
        let uid = unsafe { libc::getuid() };
        for label in [RUNNER_LABEL, HOLDER_LABEL, LEGACY_RUNNER_LABEL, LEGACY_HOLDER_LABEL] {
            let path = launch_agents_dir().join(format!("{label}.plist"));
            let _ = run(
                "launchctl",
                &["bootout", &format!("gui/{uid}"), &path.to_string_lossy()],
            );
            let _ = std::fs::remove_file(&path);
        }
        return Ok(());
    }
    let units = [RUNNER_UNIT, HOLDER_UNIT, LEGACY_RUNNER_UNIT, LEGACY_HOLDER_UNIT];
    let mut args = vec!["--user", "disable", "--now"];
    args.extend(units);
    let _ = run("systemctl", &args);
    for u in units {
        let _ = std::fs::remove_file(systemd_dir().join(u));
    }
    let _ = run("systemctl", &["--user", "daemon-reload"]);
    eprintln!("removed {RUNNER_UNIT} and {HOLDER_UNIT}");
    Ok(())
}

/// Restart the holder (ends every session). Used by `update --holder`. An
/// aimux-era holder that is still serving is stopped too, so the restarted
/// tmuxedo holder is the only one.
pub fn restart_holder(holder_pid: Option<u32>) -> Result<(), String> {
    if cfg!(target_os = "macos") {
        let uid = unsafe { libc::getuid() };
        let _ = run("launchctl", &["bootout", &format!("gui/{uid}/{LEGACY_HOLDER_LABEL}")]);
        return run("launchctl", &["kickstart", "-k", &format!("gui/{uid}/{HOLDER_LABEL}")]);
    }
    if systemd_dir().join(HOLDER_UNIT).exists() {
        let _ = run("systemctl", &["--user", "stop", LEGACY_HOLDER_UNIT]);
        return run("systemctl", &["--user", "restart", HOLDER_UNIT, RUNNER_UNIT]);
    }
    if systemd_dir().join(LEGACY_HOLDER_UNIT).exists() {
        return run(
            "systemctl",
            &["--user", "restart", LEGACY_HOLDER_UNIT, LEGACY_RUNNER_UNIT],
        );
    }
    match holder_pid {
        // SAFETY: plain kill(2).
        Some(pid) if unsafe { libc::kill(pid as i32, libc::SIGTERM) } == 0 => Ok(()),
        _ => Err("no holder service and no running holder found".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn units_reference_the_binary_and_each_other() {
        let exe = Path::new("/home/u/.local/bin/tmuxedo-runner");
        let h = holder_unit(exe);
        assert!(h.contains("ExecStart=/home/u/.local/bin/tmuxedo-runner holder --wait"));
        assert!(h.contains("Restart=always"));
        let r = runner_unit(exe);
        assert!(r.contains("ExecStart=/home/u/.local/bin/tmuxedo-runner run --no-start-holder"));
        assert!(r.contains("Requires=tmuxedo-holder.service"));
        assert!(r.contains("After=tmuxedo-holder.service"));
        assert!(r.contains("Restart=always"));
        assert!(r.contains("RestartPreventExitStatus=78"));
        assert!(!h.contains("aimux") && !r.contains("aimux"));
        let p = launch_agent(RUNNER_LABEL, exe, &["run"], Path::new("/tmp/l.log"));
        assert!(p.contains("<string>dev.tmuxedo.runner</string>"));
        assert!(p.contains("<string>/home/u/.local/bin/tmuxedo-runner</string>\n    <string>run</string>"));
        assert!(p.contains("<key>KeepAlive</key>"));
    }

    fn runs(plan: &[Step]) -> Vec<String> {
        plan.iter()
            .filter_map(|s| match s {
                Step::Run(a) | Step::Try(a) => Some(a.join(" ")),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn fresh_systemd_install() {
        let d = Path::new("/h/.config/systemd/user");
        let plan = systemd_install_plan(d, Path::new("/b/tmuxedo-runner"), false, false);
        assert!(plan.contains(&Step::Write(
            d.join(HOLDER_UNIT),
            holder_unit(Path::new("/b/tmuxedo-runner"))
        )));
        assert_eq!(
            runs(&plan),
            vec![
                "systemctl --user daemon-reload",
                "systemctl --user enable --now tmuxedo-holder.service tmuxedo-runner.service"
            ]
        );
        assert!(!plan.iter().any(|s| matches!(s, Step::Remove(_))));
    }

    #[test]
    fn systemd_migration_never_stops_the_old_holder() {
        let d = Path::new("/h/.config/systemd/user");
        let plan = systemd_install_plan(d, Path::new("/b/tmuxedo-runner"), true, true);
        let r = runs(&plan);
        assert_eq!(
            r,
            vec![
                "systemctl --user disable --now aimux-runner.service",
                "systemctl --user disable aimux-holder.service",
                "systemctl --user daemon-reload",
                "systemctl --user enable --now tmuxedo-holder.service tmuxedo-runner.service"
            ]
        );
        // Nothing stops, restarts or kills the old holder.
        for c in &r {
            assert!(
                !(c.contains("aimux-holder") && (c.contains("--now") || c.contains(" stop") || c.contains("restart"))),
                "{c}"
            );
        }
        assert!(plan.contains(&Step::Remove(d.join(LEGACY_RUNNER_UNIT))));
        assert!(plan.contains(&Step::Remove(d.join(LEGACY_HOLDER_UNIT))));
        // The new units are written before anything old is touched.
        let first_old = plan.iter().position(|s| matches!(s, Step::Try(_))).unwrap();
        let last_write = plan.iter().rposition(|s| matches!(s, Step::Write(..))).unwrap();
        assert!(last_write < first_old);
        assert!(plan
            .iter()
            .any(|s| matches!(s, Step::Say(m) if m.contains("systemctl --user stop aimux-holder.service"))));
    }

    #[test]
    fn launchd_migration_keeps_the_old_holder_loaded() {
        let a = Path::new("/Users/u/Library/LaunchAgents");
        let plan = launchd_install_plan(
            a,
            Path::new("/Users/u/Library/Logs"),
            501,
            Path::new("/b/tmuxedo-runner"),
            true,
            true,
        );
        let r = runs(&plan);
        assert!(
            r.contains(&"launchctl bootout gui/501 /Users/u/Library/LaunchAgents/ai.aimux.runner.plist".to_string())
        );
        assert!(!r.iter().any(|c| c.contains("ai.aimux.holder")), "{r:?}");
        assert!(plan.contains(&Step::Remove(a.join("ai.aimux.holder.plist"))));
        assert!(r.contains(
            &"launchctl bootstrap gui/501 /Users/u/Library/LaunchAgents/dev.tmuxedo.holder.plist".to_string()
        ));
        assert!(r.contains(
            &"launchctl bootstrap gui/501 /Users/u/Library/LaunchAgents/dev.tmuxedo.runner.plist".to_string()
        ));
        let holder_plist = plan
            .iter()
            .find_map(|s| match s {
                Step::Write(p, b) if p.ends_with("dev.tmuxedo.holder.plist") => Some(b.clone()),
                _ => None,
            })
            .unwrap();
        assert!(holder_plist.contains("<string>holder</string>\n    <string>--wait</string>"));
        assert!(holder_plist.contains("tmuxedo-holder.log"));
    }
}
