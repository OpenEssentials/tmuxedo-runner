//! `tmuxedo-runner run`: the runner daemon.
//!
//! One task owns all state (topology, an emulator, mirror and agent tracker
//! per pane, the gateway sender, API waiters and subscribers) and reacts to
//! its inputs: holder events (pane bytes, exits), gateway frames, local
//! socket requests, and timers. Nothing here ever blocks a PTY: the holder
//! drains PTYs on its own, and a slow gateway only slows this task, which at
//! worst lets the holder ring wrap (reported as a gap, repaired by snapshot).

use crate::api::{ReplyTo, Subscriber, Waiter};
use crate::events::Event;
use crate::gateway::{self, GatewayOptions, GwEvent};
use crate::local::LocalReq;
use crate::pane_rt::{Out, PaneRt, COALESCE, COALESCE_BYTES};
use crate::topology::{Closed, State};
use crate::util::iso_now;
use crate::{agent, inject, log, watchdog};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tmuxedo_holder::{HolderClient, HolderEvent, SpawnSpec};
use tmuxedo_protocol::frames::{AgentPrompt, AgentStatus, Behavior, GatewayFrame, Parsed, RunnerFrame, RunnerInfo};
use tmuxedo_protocol::{DataFrame, DataKind};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

/// How long a held PermissionRequest waits for a remote decision.
/// How long a PermissionRequest waits for a remote decision (web and the
/// phone assume 540 s). The runner owns this deadline: it answers the hook
/// `{expired: true}` at 540 s, and the hook itself waits a little longer
/// ([`crate::hook::PERMISSION_WAIT`]), so a decision can never be taken
/// here after the hook gave up (audit P2-16a).
pub const PROMPT_HOLD: Duration = Duration::from_secs(540);
/// Outbound bytes allowed to wait beyond the queue before the connection
/// is treated as stalled and reset.
pub const GW_OVERFLOW_MAX: usize = 8 * 1024 * 1024;
/// No outbound progress for this long resets the connection.
pub const GW_STALL: Duration = Duration::from_secs(10);
/// Agent detection cadence.
pub const DETECT_EVERY: Duration = Duration::from_millis(400);
/// Opt-in saved pane history: a pane's file is rewritten at most this often
/// while it prints (and once more when the runner stops).
pub const HISTORY_SAVE_EVERY: Duration = Duration::from_secs(30);
/// The newest part of a pane's history that is saved (decision 0009).
pub const HISTORY_FILE_MAX: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct RunOptions {
    pub gateway_url: String,
    pub token: String,
    pub http_base: Option<String>,
    pub state_dir: PathBuf,
    pub holder_socket: PathBuf,
    /// An aimux-era holder's socket, used while that holder lives.
    pub legacy_holder_socket: Option<PathBuf>,
    pub runner_socket: PathBuf,
    /// Linked to `runner_socket` so panes from before the rename still reach us.
    pub legacy_runner_socket: Option<PathBuf>,
    /// Start `tmuxedo-runner holder` (detached) if none answers.
    pub start_holder: bool,
    /// argv for new panes; None = the login shell with `-l`.
    pub shell: Option<Vec<String>>,
    pub home: String,
    /// The folder new panes start in when web has not set `config.workDir`
    /// ([`crate::launch_config::DEFAULT_WORK_DIR`]).
    pub default_work_dir: String,
    pub cols: u16,
    pub rows: u16,
    pub backoff_min: Duration,
    pub backoff_max: Duration,
    pub auto_update: bool,
    pub watchdog: bool,
    /// Prepend the `claude` hook shim to every pane's PATH.
    pub inject_hooks: bool,
    /// The binary registered as the Claude hook (`<exe> hook`).
    pub hook_exe: PathBuf,
    /// Opt-in on-disk pane history.
    pub pane_history: bool,
    /// Lines and memory cap of each pane's history (decision 0009).
    pub history: crate::config::HistorySize,
    /// Saved history is rewritten at most this often per pane ([`HISTORY_SAVE_EVERY`]).
    pub history_save_every: Duration,
    /// Where `worktree.create` puts checkouts: `<dir>/<repo>/<branch-slug>`.
    pub worktree_dir: PathBuf,
    /// Track Claude login health: None = when Claude Code is installed or
    /// its credentials file exists.
    pub claude_auth: Option<bool>,
    /// Version probe limit, backoff, and (tests) Claude's command.
    pub probe: crate::reprobe::Timing,
    /// First self-update check after start (then every 6 h).
    pub update_after: Duration,
    /// Outbound bytes allowed to wait beyond the queue before the gateway
    /// connection is treated as stalled and reset.
    pub gw_overflow_max: usize,
    /// No outbound progress for this long resets the gateway connection.
    pub gw_stall: Duration,
    /// How long a held PermissionRequest waits ([`PROMPT_HOLD`]).
    pub prompt_hold: Duration,
    /// Hooks count only from the pane's agent (crate::ancestry). On in
    /// every real runner; the in-process test harness turns it off where a
    /// test plays the agent from its own process.
    pub verify_hook_callers: bool,
    /// An idle chat nobody watches stops its agent after this (crate::chat).
    pub chat_park_after: Duration,
    /// A chat nobody watches this long is unattended (D-C4 guarded (b); crate::chat).
    pub chat_unwatched_grace: Duration,
    /// Signs the gateway's key challenge (key-bound tokens; the Mac app's Secure Enclave key).
    pub signer: Option<crate::signer::SignerHandle>,
    /// Machine-side controls (allow-list, observe-only), enforced here whatever the server says.
    pub guardrails: crate::guardrails::Guardrails,
    /// Confirm on this Mac before remote control (crate::confirm); asked through the signer.
    pub confirm: crate::confirm::Policy,
    /// How often an open question is polled.
    pub confirm_poll: Duration,
    /// The files root (crate::files); None = `$TMUXEDO_FILES_ROOT` or the XDG default.
    pub files_root: Option<PathBuf>,
    /// Automation labels this machine's owner turned on (crate::task); empty = no tasks.
    pub automation_labels: Vec<String>,
}

impl RunOptions {
    pub fn new(gateway_url: String, token: String) -> Self {
        let home = tmuxedo_holder::home_dir().to_string_lossy().into_owned();
        let worktree_dir = crate::paths::default_worktree_dir(std::path::Path::new(&home));
        Self {
            gateway_url,
            token,
            http_base: None,
            state_dir: crate::paths::state_dir(),
            holder_socket: crate::paths::holder_socket(),
            legacy_holder_socket: tmuxedo_holder::legacy_socket(),
            runner_socket: crate::paths::runner_socket(),
            legacy_runner_socket: crate::paths::legacy_runner_socket(),
            start_holder: true,
            shell: None,
            home,
            default_work_dir: crate::launch_config::DEFAULT_WORK_DIR.into(),
            cols: 120,
            rows: 40,
            backoff_min: Duration::from_secs(2),
            backoff_max: Duration::from_secs(60),
            auto_update: false,
            watchdog: true,
            inject_hooks: true,
            hook_exe: crate::update::current_exe().unwrap_or_else(|_| PathBuf::from("tmuxedo-runner")),
            pane_history: false,
            history: crate::config::HistorySize::DEFAULT,
            history_save_every: HISTORY_SAVE_EVERY,
            worktree_dir,
            claude_auth: None,
            probe: crate::reprobe::Timing::default(),
            update_after: Duration::from_secs(60),
            gw_overflow_max: GW_OVERFLOW_MAX,
            gw_stall: GW_STALL,
            prompt_hold: PROMPT_HOLD,
            verify_hook_callers: true,
            chat_park_after: crate::chat::PARK_AFTER,
            chat_unwatched_grace: crate::chat::UNWATCHED_GRACE,
            signer: None,
            guardrails: Default::default(),
            confirm: Default::default(),
            confirm_poll: Duration::from_millis(500),
            files_root: None,
            automation_labels: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExitReason {
    /// 4001 / 4003: do not restart.
    Fatal(u16),
    /// The holder went away (the service manager restarts us; we reattach).
    HolderLost,
    /// A new binary is in place; the caller re-execs.
    Updated,
    /// SIGTERM / SIGINT.
    Signal,
}

pub(crate) enum Timer {
    Nudge(String),
    KillHard(String, String),
    /// SIGKILL a process group the memory watchdog sent SIGTERM (if still there).
    KillGroupHard(u32),
}

pub(crate) enum Internal {
    Spawned {
        pane_id: String,
        hid: String,
        result: Result<u32, String>,
    },
    UpdateReady,
    /// A task's command exited; finish it once its last output has arrived.
    TaskExit {
        pane_id: String,
        hid: String,
        code: Option<i32>,
        signal: Option<String>,
        tries: u32,
    },
    /// A file-browser request finished on its blocking thread (crate::fs).
    Fs {
        reply: ReplyTo,
        result: crate::api::ApiResult,
        /// FILE frames sent ahead of the answer (`fs.download`, gateway only).
        frames: Vec<Vec<u8>>,
    },
    /// A `login_callback` GET finished (or failed): answer the gateway.
    LoginDelivered {
        id: String,
        pane_id: String,
        port: u16,
        result: Result<crate::login_relay::Delivered, String>,
    },
    /// The files worker's results (crate::files::worker::Out).
    Files(crate::files::worker::Out),
    /// The Mac answered confirm-on-this-Mac question number `question`.
    Confirmed {
        question: u64,
        result: crate::signer::Confirmed,
    },
}

/// Remote control work held while this Mac is asked (crate::confirm).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ControlWork {
    Input {
        pane_id: String,
        bytes: Vec<u8>,
    },
    Resize {
        pane_id: String,
        cols: u16,
        rows: u16,
    },
    Decision {
        pane_id: String,
        prompt_id: String,
        behavior: Behavior,
        message: Option<String>,
    },
}

pub(crate) struct HeldPrompt {
    pub pane_id: String,
    /// The agent kind of the plugin that holds it (status after a decision).
    pub kind: String,
    pub reply: ReplyTo,
    pub deadline: Instant,
}

pub struct Daemon {
    pub(crate) opts: RunOptions,
    pub(crate) state: State,
    state_path: PathBuf,
    pub(crate) holder: HolderClient,
    pub(crate) panes: BTreeMap<String, PaneRt>,
    by_hid: HashMap<String, String>,
    gw: Option<gateway::GwLink>,
    /// Frames the outbound queue had no room for, in order (audit P2-5:
    /// the daemon never awaits the connection).
    gw_overflow: std::collections::VecDeque<Message>,
    gw_overflow_bytes: usize,
    /// Since when the overflow has not shrunk.
    gw_stuck_since: Option<Instant>,
    rr: usize,
    timers: Vec<(Instant, Timer)>,
    /// The memory watchdog does not act again before this (its victim is dying).
    /// Process groups being stopped (SIGTERM sent, SIGKILL due at the
    /// instant): a round whose victim is one of them waits; any other group
    /// is still handled (audit P2-16b: no blind window).
    watchdog_stopping: Vec<(u32, Instant)>,
    pub(crate) prompts: HashMap<String, HeldPrompt>,
    pub(crate) info: RunnerInfo,
    /// Plugins on the wire: gateway features, agent versions, skill wishes (crate::plugin_wire).
    pub(crate) wire: crate::plugin_wire::Wire,
    pub(crate) internal_tx: mpsc::UnboundedSender<Internal>,
    /// The file browser: roots and work in flight (crate::fs).
    pub(crate) fs: crate::fs::Rt,
    /// Review and ship: git reads in flight (crate::review).
    pub(crate) review: crate::review::Rt,
    dirty: bool,
    exit: Option<ExitReason>,
    pub(crate) waiters: Vec<Waiter>,
    pub(crate) subscribers: Vec<Subscriber>,
    pub(crate) shim_dir: Option<PathBuf>,
    pub(crate) shell: String,
    /// Durable events not yet handed to a gateway socket.
    /// Durable events awaiting `events_ack` (docs/protocol.md).
    outbox: crate::outbox::Outbox,
    /// The gateway sent `welcome` on the current connection.
    welcomed: bool,
    /// Launch/shell configuration from web (persisted in config.json).
    pub(crate) launch: crate::launch_config::LaunchConfig,
    /// The plugin host (built-in plugins + their enable state).
    pub(crate) plugins: crate::plugin::Host,
    /// Claude login health (None: no Claude Code on this machine).
    pub(crate) auth: Option<crate::claude_auth::AuthHealth>,
    /// Every other plugin at run time: login health, lifted links,
    /// keystroke answers (crate::plugin_rt).
    pub(crate) prt: crate::plugin_rt::PluginRt,
    /// CLIs whose version probe did not answer: asked again in the background.
    pub(crate) reprobe: crate::reprobe::Reprobe,
    last_detect: Instant,
    /// Panes the gateway wants glances of (docs/protocol.md "Glances"); empty = no glance work.
    glances: BTreeMap<String, GlanceState>,
    glance_hz: f64,
    /// Glances sent per pane in this process (`rev`; survives set changes).
    glance_rev: HashMap<String, u64>,
    /// Machine stats (docs/protocol.md "Machine stats"): samples only while the gateway asks.
    stats: crate::stats::Sampler,
    /// AI usage (crate::usage): the newest record per plugin, sent on change.
    usage: crate::usage::Store,
    /// Loopback OAuth callbacks seen in `open_url`s, for `login_callback`.
    login_relay: crate::login_relay::Relay,
    /// Confirm on this Mac: remote control waits for the person at the machine.
    confirm: crate::confirm::Gate<ControlWork>,
    /// Agent chats (crate::chat).
    pub(crate) chats: crate::chat::Chats,
    /// Values the masker hides (crate::secrets).
    pub(crate) secrets: crate::secrets::Secrets,
    /// Headless tasks started through the task API (crate::task).
    pub(crate) tasks: crate::task::Tasks,
    /// The runner key, policy and delivered values (crate::secrets::delivery).
    pub(crate) delivery: crate::secrets::delivery::Rt,
    secrets_relaunch_pending: bool,
    /// Folder → project key lookups (crate::project).
    pub(crate) projects: crate::project::Resolver,
    /// The ssh-agent's keys and pending confirmations (crate::secrets::ssh).
    pub(crate) ssh: crate::secrets::ssh::Rt,
    /// An ssh confirmation was queued: flush events after this turn of the loop.
    pub(crate) ssh_flush: bool,
    /// Files and uploads (crate::files); None when files are off on this machine.
    pub(crate) files: Option<crate::files::glue::Files>,
    /// The files worker's inbox, handed to the gateway connection (upload bytes go there directly).
    files_in: Option<mpsc::Sender<crate::files::worker::Msg>>,
    /// Open needs-you items (crate::needs).
    pub(crate) needs: crate::needs::Needs,
    /// Queued prompts already delivered (crate::queued).
    pub(crate) queued: crate::queued::Delivered,
}

/// Per glanced pane: when to look next, what was seen, what was sent.
pub(crate) struct GlanceState {
    next_check: Instant,
    /// (mirror end, rows, cols) at the last build: unchanged = nothing to do.
    seen: Option<(u64, u16, u16)>,
    /// Hash of the last glance sent.
    hash: Option<u64>,
}

/// Connect to the holder, starting one if allowed and absent. A holder from
/// before the aimux → tmuxedo rename that is still alive (it owns the
/// sessions started before the update) is used first; a new holder waits
/// for it to exit (`holder --wait`), so the two never split the sessions.
pub async fn connect_holder(opts: &RunOptions) -> Result<(HolderClient, mpsc::Receiver<HolderEvent>), String> {
    if let Some(legacy) = opts.legacy_holder_socket.as_deref() {
        if tmuxedo_holder::holder_alive(legacy) {
            match HolderClient::connect(legacy, "runner").await {
                Ok(c) => {
                    log!(
                        "using the aimux-era holder at {} (its sessions stay; tmuxedo-holder takes over when it exits)",
                        legacy.display()
                    );
                    return Ok(c);
                }
                Err(e) => log!("aimux-era holder at {}: {e}", legacy.display()),
            }
        }
    }
    if let Ok(c) = HolderClient::connect(&opts.holder_socket, "runner").await {
        return Ok(c);
    }
    if opts.start_holder {
        spawn_holder(opts).map_err(|e| format!("starting holder: {e}"))?;
    }
    let deadline = Instant::now() + Duration::from_secs(if opts.start_holder { 10 } else { 30 });
    loop {
        match HolderClient::connect(&opts.holder_socket, "runner").await {
            Ok(c) => return Ok(c),
            Err(e) if e.kind() == std::io::ErrorKind::Unsupported => {
                return Err(format!(
                    "{e} (run `tmuxedo-runner update --holder` or restart tmuxedo-holder)"
                ))
            }
            Err(e) if Instant::now() > deadline => {
                return Err(format!("holder at {}: {e}", opts.holder_socket.display()))
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
}

/// Start `<this exe> holder` in its own session, detached from us, so it
/// survives this process (including `kill -9`).
fn spawn_holder(opts: &RunOptions) -> std::io::Result<()> {
    use std::os::unix::process::CommandExt;
    // The path recorded at startup (current_exe is "… (deleted)" after a
    // self-update; audit P2-16d).
    let exe = crate::update::current_exe().map_err(std::io::Error::other)?;
    std::fs::create_dir_all(&opts.state_dir)?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(opts.state_dir.join("holder.log"))?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("holder")
        .arg("--socket")
        .arg(&opts.holder_socket)
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    // SAFETY: setsid is async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = cmd.spawn()?;
    log!("started holder (pid {})", child.id());
    std::thread::spawn(move || {
        let mut child = child;
        let _ = child.wait();
    });
    Ok(())
}

/// See [`Daemon::work_dir`].
fn work_dir_for(opts: &RunOptions, launch: &crate::launch_config::LaunchConfig) -> String {
    let want = launch.work_dir.as_deref().unwrap_or(&opts.default_work_dir);
    match crate::launch_config::resolve_work_dir(&opts.home, Some(want)) {
        Ok(d) => d,
        Err(e) => {
            log!("work dir {want}: {e}; using {}", opts.home);
            opts.home.clone()
        }
    }
}

/// The protocol additions this runner serves (`hello.runner.features`).
/// Each is a user-visible capability: the skill's staleness guard
/// (tests/skill.rs) requires a row in `skill::CAPABILITIES` for every one.
pub const RUNNER_FEATURES: &[&str] = &[
    tmuxedo_protocol::frames::FEATURE_GLANCE,
    tmuxedo_protocol::frames::FEATURE_LOGIN_CALLBACK,
    tmuxedo_protocol::frames::FEATURE_NAMES,
    tmuxedo_protocol::chat::FEATURE_CHAT,
    tmuxedo_protocol::frames::FEATURE_CHAT_OFFER,
    // Files (Phase 15): dropped from the hello when files are off on this machine.
    tmuxedo_protocol::files::FEATURE_FILES,
    tmuxedo_protocol::needs::FEATURE_NEEDS,
    tmuxedo_protocol::fs::FEATURE_FS,
    tmuxedo_protocol::stats::FEATURE_STATS,
    tmuxedo_protocol::usage::FEATURE_USAGE,
    // Review and ship (Phase 23): git.status and git.diff on the gateway.
    tmuxedo_protocol::review::FEATURE_REVIEW,
    tmuxedo_protocol::queued::FEATURE_QUEUED,
];

pub async fn run(opts: RunOptions) -> Result<ExitReason, String> {
    let (holder, holder_rx) = connect_holder(&opts).await?;
    // (For the log only: the socket connect_holder chose.)
    let at = opts
        .legacy_holder_socket
        .as_deref()
        .filter(|p| tmuxedo_holder::holder_alive(p))
        .unwrap_or(&opts.holder_socket);
    log!("holder {} (pid {}) at {}", holder.version, holder.pid, at.display());
    let claude_probe = crate::plugin::probe_agent(
        crate::plugin::claude(),
        opts.probe.claude_argv.as_deref(),
        opts.probe.limit,
    )
    .await;
    let claude_version = claude_probe.clone().version();
    let track_auth = opts
        .claude_auth
        .unwrap_or_else(|| claude_version.is_some() || crate::claude_auth::credentials_path(&opts.home).exists());
    let auth = track_auth.then(|| {
        crate::claude_auth::AuthHealth::new(
            &opts.home,
            cfg!(target_os = "linux"),
            std::env::var("ANTHROPIC_API_KEY").is_ok_and(|k| !k.is_empty()),
        )
    });
    let info = RunnerInfo {
        version: crate::VERSION.into(),
        holder_version: holder.version.clone(),
        os: crate::util::os_name().into(),
        arch: crate::util::arch_name().into(),
        hostname: crate::util::hostname(),
        claude_version,
        claude_auth: auth.as_ref().map(|a| a.json()),
        features: Some(RUNNER_FEATURES.iter().map(|f| f.to_string()).collect()),
        plugins: None,
        automation: crate::task::consent(&opts.automation_labels),
    };
    let mut info = info;
    // Files: a root outside every repository, or off (logged) when there is none.
    let files_root = opts
        .files_root
        .clone()
        .or_else(|| crate::files::configured_root(std::path::Path::new(&opts.home)))
        .and_then(|r| match crate::files::prepare_root(&r) {
            Ok(real) => Some(real),
            Err(e) => {
                log!("files: {e}");
                None
            }
        });
    let shell_argv = opts
        .shell
        .clone()
        .unwrap_or_else(|| vec![crate::util::login_shell(), "-l".into()]);
    let launch: crate::launch_config::LaunchConfig = std::fs::read(opts.state_dir.join("config.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    // The wrappers always exist: the shell left after an agent run reads them.
    if let Err(e) = crate::launch_config::write_shell_wrappers(&opts.home, &opts.state_dir) {
        log!("shell wrappers: {e}");
    }
    if !launch.shell.is_empty() || !launch.projects.is_empty() {
        let hook = (!launch.projects.is_empty()).then(|| opts.hook_exe.display().to_string());
        if let Err(e) =
            crate::launch_config::write_shell_files_with(&opts.home, &opts.state_dir, &launch.shell, hook.as_deref())
        {
            log!("shell settings: {e}");
        }
    }
    let mut plugins = crate::plugin::Host::load(&opts.state_dir);
    // Local / dev plugins (plugin authors, the e2e stack): never on a hosted machine.
    if let Some(dev) = std::env::var_os("TMUXEDO_PLUGIN_DEV_DIR").filter(|d| !d.is_empty()) {
        if crate::plugin_wire::Wire::new(Default::default()).hosted {
            log!("dev plugins are not loaded on a hosted machine");
        } else {
            for note in plugins.load_dev(std::path::Path::new(&dev)) {
                log!("{note}");
            }
        }
    }
    // Each plugin's agent version (Claude's is the hello's claudeVersion),
    // probed side by side: a CLI that hangs costs its 5 s once, not per plugin.
    let mut agent_versions = std::collections::BTreeMap::new();
    let probes: Vec<(String, std::sync::Arc<dyn crate::plugin::AgentPlugin>)> = plugins
        .ids()
        .into_iter()
        .filter(|id| *id != crate::plugin::DEFAULT_PLUGIN)
        .filter_map(|id| plugins.get(id).map(|p| (id.to_string(), p)))
        .collect();
    let found = futures_util::future::join_all(
        probes
            .iter()
            .map(|(_, p)| async move { crate::plugin::probe_agent(p.as_ref(), None, opts.probe.limit).await }),
    )
    .await;
    // A probe that found no version is asked again later (src/reprobe.rs).
    let mut reprobe = crate::reprobe::Reprobe::new(opts.probe.clone());
    let started = Instant::now();
    reprobe.note(crate::plugin::DEFAULT_PLUGIN, &claude_probe, started);
    for ((id, _), v) in probes.into_iter().zip(found) {
        reprobe.note(&id, &v, started);
        agent_versions.insert(id, v.version());
    }
    for id in plugins.ids() {
        if id == crate::plugin::DEFAULT_PLUGIN {
            agent_versions.insert(id.to_string(), info.claude_version.clone());
        }
    }
    let mut wire = crate::plugin_wire::Wire::new(agent_versions);
    // The skill wishes web sent last time, so panes restored before the next
    // `config` launch the same way.
    wire.skill_wanted = crate::plugin_wire::load_skill_wishes(&opts.state_dir);
    let mut needs = crate::needs::Needs::default();
    needs.keystroke = crate::needs::load_keystroke_wishes(&opts.state_dir);
    // File requests still open from before a restart: their items are open (no second needs_opened).
    needs.files = crate::needs_files::load(&opts.state_dir);
    for a in &needs.files {
        needs.open.entry(a.pane_id.clone()).or_default().push(a.item());
    }
    for (id, on) in &wire.skill_wanted {
        let ok = plugins
            .get(id)
            .and_then(|p| p.manifest().skill.clone())
            .is_some_and(|sk| {
                sk.argv.is_empty()
                    || crate::skill::version_at_least(
                        wire.agent_versions.get(id).cloned().flatten().as_deref(),
                        &sk.min_agent,
                    )
            });
        plugins.set_skill(id, *on && ok && plugins.enabled(id));
    }
    // AI usage as web last asked (none until it does): the settings files
    // written below carry a status line only for these.
    wire.usage_wanted = crate::plugin_wire::load_usage_wishes(&opts.state_dir);
    for (id, on) in &wire.usage_wanted {
        plugins.set_usage(id, *on);
    }
    plugins.hosted = wire.hosted;
    plugins.hook_exe = Some(opts.hook_exe.clone());
    // What the last plugin set built is still there; the next config rebuilds it.
    plugins.set_plugin_set(crate::plugin_gen::existing_lines(&opts.state_dir, &opts.hook_exe));
    let shim_dir = if opts.inject_hooks {
        for id in plugins.ids() {
            if !plugins.enabled(id) {
                let _ = plugins.remove_shim(id, &opts.state_dir);
            }
        }
        match plugins.install(&opts.state_dir, &opts.hook_exe, &launch.claude_args) {
            Ok(d) => Some(d),
            Err(e) => {
                log!("hook injection unavailable: {e}");
                None
            }
        }
    } else {
        None
    };
    let (internal_tx, internal_rx) = mpsc::unbounded_channel();
    let (files, files_in) = match files_root {
        Some(root) => {
            let (ftx, frx) = mpsc::channel(crate::files::worker::CHANNEL);
            let store = crate::files::store::Store::new(root.clone(), crate::files::Limits::from_env());
            let itx = internal_tx.clone();
            crate::files::worker::spawn(store, frx, move |o| {
                let _ = itx.send(Internal::Files(o));
            });
            log!("files at {}", root.display());
            if let Err(e) = crate::files::write_instructions(&root, crate::files::Limits::from_env().max_file) {
                log!("files: the instructions file: {e}");
            }
            (Some(crate::files::glue::Files::new(root, ftx.clone())), Some(ftx))
        }
        None => {
            if let Some(f) = info.features.as_mut() {
                f.retain(|x| x != tmuxedo_protocol::files::FEATURE_FILES);
            }
            (None, None)
        }
    };
    let state_path = opts.state_dir.join("state.json");
    // Counters carried over even when the topology itself is not reused.
    let (mut rev_floor, mut seq_floor) = (0, 0);
    let first_cwd = work_dir_for(&opts, &launch);
    let fresh = || State::default_topology(&first_cwd, shell_argv.clone(), opts.cols, opts.rows);
    let mut state = match State::load(&state_path) {
        crate::topology::Loaded::Ok(s) if !s.workspaces.is_empty() => s,
        crate::topology::Loaded::Ok(s) => {
            (rev_floor, seq_floor) = (s.rev, s.event_seq);
            fresh()
        }
        crate::topology::Loaded::Unreadable {
            rev,
            event_seq,
            backup,
            why,
        } => {
            log!(
                "state.json unusable ({why}); starting a new layout, keeping rev {rev} and event seq {event_seq}{}",
                backup
                    .map(|b| format!(" (old file kept as {})", b.display()))
                    .unwrap_or_default()
            );
            (rev_floor, seq_floor) = (rev, event_seq);
            fresh()
        }
        crate::topology::Loaded::Missing => fresh(),
    };
    state.rev = state.rev.max(rev_floor + 1);
    state.event_seq = state.event_seq.max(seq_floor);
    let outbox = crate::outbox::Outbox::open(&opts.state_dir.join("events.jsonl"));
    // Never reuse a seq that is still queued on disk.
    state.event_seq = state.event_seq.max(outbox.max_seq());
    let confirm = crate::confirm::Gate::new(opts.confirm.clone());
    let mut chats = crate::chat::Chats::load(&opts.state_dir.join("chat"), opts.chat_park_after);
    chats.unwatched_grace = opts.chat_unwatched_grace;
    // The mask set is in force before the first holder byte is read.
    let mut secrets = crate::secrets::Secrets::default();
    secrets.replace_source(
        crate::secrets::Source::ShellEnv,
        crate::secrets::secret_named(&launch.shell.env),
    );
    let delivery = crate::secrets::delivery::Rt::new(&opts.state_dir);
    let queued = crate::queued::Delivered::load(&opts.state_dir);
    let state_dir_for_ssh = opts.state_dir.join("ssh");
    let stats = crate::stats::Sampler::new(crate::stats::Roots::system(
        std::path::Path::new(&opts.home),
        holder.pid,
    ));
    let usage = crate::usage::Store::new(&opts.state_dir);
    let mut d = Daemon {
        shell: shell_argv[0].clone(),
        opts,
        state,
        state_path,
        holder,
        panes: BTreeMap::new(),
        by_hid: HashMap::new(),
        gw: None,
        gw_overflow: Default::default(),
        gw_overflow_bytes: 0,
        gw_stuck_since: None,
        rr: 0,
        timers: vec![],
        watchdog_stopping: vec![],
        prompts: HashMap::new(),
        info,
        internal_tx,
        fs: Default::default(),
        review: Default::default(),
        dirty: true,
        exit: None,
        waiters: vec![],
        subscribers: vec![],
        shim_dir,
        outbox,
        welcomed: false,
        launch,
        plugins,
        wire,
        auth,
        prt: crate::plugin_rt::PluginRt::default(),
        reprobe,
        last_detect: Instant::now(),
        glances: BTreeMap::new(),
        glance_hz: tmuxedo_protocol::frames::GLANCE_HZ_DEFAULT,
        glance_rev: HashMap::new(),
        stats,
        usage,
        login_relay: crate::login_relay::Relay::default(),
        confirm,
        chats,
        secrets,
        tasks: crate::task::Tasks::default(),
        delivery,
        secrets_relaunch_pending: false,
        projects: Default::default(),
        ssh: crate::secrets::ssh::Rt::new(state_dir_for_ssh),
        ssh_flush: false,
        files,
        files_in,
        needs,
        queued,
    };
    d.init_plugin_auth();
    d.persist();
    d.adopt_panes().await?;
    {
        let list = d.holder.list().await?;
        let held: HashMap<String, Value> = list["panes"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|p| Some((p["id"].as_str()?.to_string(), p)))
            .collect();
        d.chat_adopt(&held).await;
    }
    d.main_loop(holder_rx, internal_rx).await
}

impl Daemon {
    pub(crate) fn persist(&mut self) {
        for p in self.state.panes.iter_mut() {
            if let Some(rt) = self.panes.get(&p.pane.id) {
                p.high_water = p.high_water.max(rt.end());
                // `chat` is the daemon's (agent_update), not the tracker's: keep what was sent.
                let chat = p.pane.agent.as_ref().and_then(|a| a.chat.clone());
                p.pane.agent = rt.track.agent(p.meta.session_id.as_deref()).map(|mut a| {
                    a.chat = chat;
                    a
                });
            }
        }
        if let Err(e) = self.state.save(&self.state_path) {
            log!("saving state: {e}");
        }
        if self.opts.pane_history {
            self.save_history(self.exit.is_some());
        }
        self.dirty = false;
    }

    pub(crate) fn default_argv(&self) -> Vec<String> {
        self.opts
            .shell
            .clone()
            .unwrap_or_else(|| vec![self.shell.clone(), "-l".into()])
    }

    fn pane_env(&self, pane_id: &str) -> BTreeMap<String, String> {
        let sock = self.opts.runner_socket.to_string_lossy().into_owned();
        let (ws, tab) = self.state.workspace_of(pane_id).unwrap_or_default();
        // TMUXEDO_* and, deprecated (kept while scripts move over), the same
        // values as AIMUX_*.
        let mut env = BTreeMap::new();
        for p in ["TMUXEDO", "AIMUX"] {
            env.insert(format!("{p}_PANE_ID"), pane_id.to_string());
            env.insert(format!("{p}_WORKSPACE_ID"), ws.clone());
            env.insert(format!("{p}_TAB_ID"), tab.clone());
            env.insert(format!("{p}_SOCKET_PATH"), sock.clone());
        }
        // Where the shell wrappers are (launch_config::exec_wrapped_shell).
        env.insert(
            "TMUXEDO_SHELL_DIR".into(),
            crate::launch_config::shell_dir(&self.opts.state_dir)
                .display()
                .to_string(),
        );
        if let Some(shim) = &self.shim_dir {
            let path = std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".into());
            let pane_path = format!("{}:{path}", shim.display());
            env.insert("PATH".into(), pane_path.clone());
            for p in ["TMUXEDO", "AIMUX"] {
                env.insert(format!("{p}_PANE_PATH"), pane_path.clone());
                env.insert(format!("{p}_SHIM_DIR"), shim.display().to_string());
            }
            if std::env::var_os("PROMPT_COMMAND").is_none() {
                env.insert("PROMPT_COMMAND".into(), inject::prompt_command().into());
            }
            // Web links open on the user's device (open_url events).
            env.insert("BROWSER".into(), shim.join("x-www-browser").display().to_string());
        }
        // The user's launch settings (validated: no runner-managed names).
        for (k, v) in &self.launch.shell.env {
            env.entry(k.clone()).or_insert_with(|| v.clone());
        }
        // The pane's project (its folder's git repository) over the machine's (D-7).
        if let Some(cwd) = self.state.pane(pane_id).map(|p| p.pane.cwd.clone()) {
            let machine = self.delivery.machine.clone();
            if let Some(k) = self
                .projects
                .project_for(std::path::Path::new(&cwd), &machine, &self.launch.projects)
            {
                // The cd hook restores these when the shell leaves the project.
                let names: Vec<&str> = self.launch.projects[&k].keys().map(String::as_str).collect();
                env.insert("__TMUXEDO_PROJECT_VARS".into(), names.join(" "));
                for (n, v) in &self.launch.projects[&k] {
                    match v {
                        Some(v) => {
                            env.insert(n.clone(), v.clone());
                        }
                        None => {
                            env.remove(n);
                        }
                    }
                }
            }
        }
        // Secrets bound to "pane env" (secrets-and-env.md §4 #5; the UI warns).
        for (k, v) in self.pane_secret_env() {
            env.entry(k).or_insert(v);
        }
        // SSH keys bound to this pane's scope: the runner's agent (§6a).
        self.ssh_pane_env(pane_id, &mut env);
        // git asks the runner for credentials bound to a host (§4 #2).
        if !self
            .delivery
            .policy
            .bound(|b| matches!(b, crate::secrets::delivery::Binding::Git { .. }))
            .is_empty()
        {
            let helper = format!(
                "!{} cli secret git-credential",
                crate::plugin::sh_quote(&self.opts.hook_exe.display().to_string())
            );
            let n: usize = std::env::var("GIT_CONFIG_COUNT")
                .ok()
                .and_then(|n| n.parse().ok())
                .unwrap_or(0);
            env.insert(format!("GIT_CONFIG_KEY_{n}"), "credential.helper".into());
            env.insert(format!("GIT_CONFIG_VALUE_{n}"), helper);
            env.insert("GIT_CONFIG_COUNT".into(), (n + 1).to_string());
        }
        // Files (docs/protocol.md "Files"): the root, and this pane's project folder.
        if let Some(files) = &self.files {
            let root = files.root.display().to_string();
            let dir = self
                .state
                .pane(pane_id)
                .map(|p| files.dir_for(std::path::Path::new(&p.pane.cwd)).display().to_string());
            for p in ["TMUXEDO", "AIMUX"] {
                env.insert(format!("{p}_FILES_ROOT"), root.clone());
                if let Some(d) = &dir {
                    env.insert(format!("{p}_FILES_DIR"), d.clone());
                }
            }
        }
        if let Some(p) = self.state.pane(pane_id) {
            env.extend(p.env.clone());
        }
        // Plugins' env.scrub (the holder enforces the same list on its own
        // environment; this covers what the runner itself passes).
        env.retain(|k, _| !self.plugins.active().any(|p| p.scrubs(k)));
        env
    }

    fn spawn_spec(&self, pane_id: &str, hid: &str, argv: Vec<String>) -> Option<SpawnSpec> {
        let p = self.state.pane(pane_id)?;
        let mut env = self.pane_env(pane_id);
        let argv = self.with_shell_settings(argv, &mut env);
        Some(SpawnSpec {
            id: hid.to_string(),
            argv,
            cwd: Some(p.pane.cwd.clone()).filter(|c| std::path::Path::new(c).is_dir()),
            env,
            cols: p.pane.cols,
            rows: p.pane.rows,
        })
    }

    /// A plain interactive bash/zsh starts with the user's rc and then
    /// `~/.config/tmuxedo/shell-init.sh` (docs/protocol.md `config.shell`):
    /// bash `--rcfile <wrapper> -i`, zsh through a ZDOTDIR wrapper. Other
    /// programs and shells are left alone (their env already has the
    /// settings' variables).
    fn with_shell_settings(&self, argv: Vec<String>, env: &mut BTreeMap<String, String>) -> Vec<String> {
        if self.launch.shell.is_empty() && self.launch.projects.is_empty() {
            return argv;
        }
        let dir = crate::launch_config::shell_dir(&self.opts.state_dir);
        crate::launch_config::wrapped_shell(argv, &dir, &self.opts.home, env)
    }

    /// `config` from the gateway: validate each part (an invalid part keeps
    /// the previous one), persist, and regenerate the claude shim and the
    /// shell files. Applies to new launches and new panes.
    /// Build what the kept plugin set gives each agent (crate::plugin_gen);
    /// true when a shim gains or loses its plugin-set line.
    fn sync_plugin_set(&mut self) -> bool {
        let dir = self.opts.state_dir.clone();
        let roles = crate::role_set::load(&dir);
        let set = match crate::plugin_set::load(&dir) {
            Some(set) => set,
            // Roles alone are built too (as helpers), against an empty set.
            None if roles.is_some() => tmuxedo_protocol::plugin_set::PluginSet {
                v: 1,
                items: vec![],
                machine: Default::default(),
                projects: Default::default(),
            },
            None => return false,
        };
        // A skill that is an instructions file shares the argument the builder writes.
        let skill_text = |id: &str| {
            let form = self.plugins.get(id)?.manifest().skill.as_ref()?.form.clone();
            let at = self.plugins.skill_dir(id, &dir)?;
            (form == "agents-md").then(|| std::fs::read_to_string(at.join("AGENTS.md")).ok())?
        };
        let multi_agent = |id: &str| self.plugins.get(id)?.manifest().multi_agent.clone();
        let lines = crate::plugin_gen::rebuild_all_with(
            &dir,
            &set,
            roles.as_ref(),
            &self.delivery.machine,
            &self.opts.hook_exe,
            skill_text,
            multi_agent,
        );
        self.plugins.set_plugin_set(lines)
    }

    fn apply_config(
        &mut self,
        claude: Option<Value>,
        shell: Option<Value>,
        work_dir: Option<Value>,
        projects: Option<Value>,
    ) {
        use crate::launch_config::{
            validate_args, validate_projects, validate_shell, validate_work_dir, write_shell_files_with,
        };
        let mut next = self.launch.clone();
        if let Some(p) = projects.filter(|p| !p.is_null()) {
            match validate_projects(&p) {
                Ok((projects, refused)) => {
                    if !refused.is_empty() {
                        log!("config: project env refused for {}", refused.join(", "));
                    }
                    next.projects = projects;
                }
                Err(e) => log!("config: project env refused ({e}); keeping the previous one"),
            }
        }
        if let Some(w) = work_dir.filter(|w| !w.is_null()) {
            match validate_work_dir(&w) {
                Ok(w) => next.work_dir = w,
                Err(e) => log!("config: workDir refused ({e}); keeping the previous one"),
            }
        }
        if let Some(c) = claude {
            match validate_args(&c["args"]) {
                Ok(a) => next.claude_args = a,
                Err(e) if c["args"].is_null() => {
                    let _ = e;
                    next.claude_args = vec![];
                }
                Err(e) => log!("config: claude args refused ({e}); keeping the previous ones"),
            }
        }
        if let Some(s) = shell {
            match validate_shell(&s) {
                Ok((cfg, refused)) => {
                    if !refused.is_empty() {
                        log!("config: shell settings refused for {}", refused.join(", "));
                    }
                    next.shell = cfg;
                }
                Err(e) => log!("config: shell settings refused ({e}); keeping the previous ones"),
            }
        }
        if next == self.launch {
            return;
        }
        if next.claude_args != self.launch.claude_args && self.shim_dir.is_some() {
            if let Err(e) = self.plugins.write_shims(&self.opts.state_dir, &next.claude_args) {
                log!("config: claude shim: {e}");
            }
            // Rewritten shims lose the agent-env line: put it back once applied.
            self.secrets_relaunch_pending = true;
        }
        if next.shell != self.launch.shell || next.projects.is_empty() != self.launch.projects.is_empty() {
            let hook = (!next.projects.is_empty()).then(|| self.opts.hook_exe.display().to_string());
            if let Err(e) = write_shell_files_with(&self.opts.home, &self.opts.state_dir, &next.shell, hook.as_deref())
            {
                log!("config: shell files: {e}");
            }
        }
        if next.projects != self.launch.projects {
            self.projects.clear();
        }
        if let Ok(b) = serde_json::to_vec_pretty(&next) {
            let _ = crate::config::write_private(&self.opts.state_dir.join("config.json"), &b);
        }
        log!(
            "config: claude args {:?}; shell {} aliases, {} env, rc {} bytes; work dir {}",
            next.claude_args,
            next.shell.aliases.len(),
            next.shell.env.len(),
            next.shell.rc.len(),
            next.work_dir
                .as_deref()
                .unwrap_or(crate::launch_config::DEFAULT_WORK_DIR)
        );
        self.launch = next;
        if std::mem::take(&mut self.secrets_relaunch_pending) {
            self.secrets_patch_launch();
        }
        let grew = self.secrets.replace_source(
            crate::secrets::Source::ShellEnv,
            crate::secrets::secret_named(&self.launch.shell.env),
        );
        self.mask_changed(grew);
    }

    /// The mask set changed: when it gained a value, re-read
    /// every pane from its holder ring so nothing derived (mirror, screen,
    /// history, glances) keeps that value from before.
    pub(crate) fn mask_changed(&mut self, grew: bool) {
        if !grew {
            return;
        }
        let ids: Vec<String> = self.panes.keys().cloned().collect();
        for id in ids {
            let Some(rt) = self.panes.get_mut(&id) else { continue };
            if !rt.alive {
                continue;
            }
            rt.restart_from_ring();
            let hid = rt.hid.clone();
            self.holder.attach_nowait(&hid, 0);
        }
        // Saved pane history was masked with the old set, and a value split
        // across two saved lines (a soft wrap) would not match a mask applied
        // to the file: drop the files that held it; the next save rewrites
        // them from the re-read (newly masked) history.
        if self.opts.pane_history {
            let set = self.secrets.set();
            let ids: Vec<String> = self.panes.keys().cloned().collect();
            for id in ids {
                let path = self.history_path(&id);
                if let Ok(mut blob) = std::fs::read(&path) {
                    let flat = tmuxedo_emu::history::plain_text(&blob).replace("\r\n", "");
                    if set.mask_in_place(&mut blob) > 0 || set.mask(flat.as_bytes()).as_slice() != flat.as_bytes() {
                        self.remove_history(&id);
                        if let Some(rt) = self.panes.get_mut(&id) {
                            rt.history_saved = None;
                        }
                    }
                }
            }
        }
    }

    /// `runner.config` (read-only, for `tmuxedo config`).
    pub(crate) fn config_view(&self) -> Value {
        let mut v = json!({
            "claude": { "args": self.launch.claude_args },
            "shell": self.launch.shell,
            "files": {
                "config": self.opts.state_dir.join("config.json"),
                "shell_init": crate::launch_config::shell_init_path(&self.opts.home),
            },
        });
        // Only when web set one: the default view is pinned by the baseline.
        if let Some(w) = &self.launch.work_dir {
            v["workDir"] = json!(w);
        }
        v
    }

    /// The folder new panes start in: `config.workDir`, else the runner's
    /// default (`~/work`), created if missing; `$HOME` if it cannot be.
    pub(crate) fn work_dir(&self) -> String {
        work_dir_for(&self.opts, &self.launch)
    }

    /// The cwd for a new pane that would otherwise inherit `inherited` (a
    /// workspace's cwd): the default working folder instead of `$HOME`.
    pub(crate) fn new_pane_cwd(&self, inherited: Option<String>) -> String {
        match inherited {
            Some(c) if !crate::launch_config::is_home(&c, &self.opts.home, crate::launch_config::LEGACY_HOME) => c,
            _ => self.work_dir(),
        }
    }

    /// Whether an `open_url` came from the pane's agent and goes to a host its
    /// plugin lists (`[links]`): Some((verified, plugin, reason)), or None
    /// when callers are not checked (the old behaviour: no verdict sent).
    fn open_url_verdict(
        &self,
        pane: &str,
        url: &str,
        peer: Option<u32>,
    ) -> Option<(bool, Option<String>, &'static str)> {
        if !self.opts.verify_hook_callers {
            return None;
        }
        let rt = self.panes.get(pane);
        let plugin = rt.and_then(|rt| rt.track.kind()).and_then(|k| self.plugins.by_label(k));
        let (Some(rt), Some(plugin)) = (rt, plugin) else {
            return Some((false, None, "not_from_agent"));
        };
        let m = plugin.manifest();
        if let Err(r) = crate::ancestry::check(&crate::ancestry::System, peer, rt.pid, &m.agent.labels) {
            log!("open_url from {pane}: not from the agent ({})", r.code());
            return Some((false, None, "not_from_agent"));
        }
        let allow = m.links.as_ref().map(|l| l.open_url.as_slice()).unwrap_or(&[]);
        let host = tmuxedo_protocol::needs::url_host(url).unwrap_or_default();
        if tmuxedo_protocol::needs::host_allowed(&host, allow) {
            Some((true, Some(m.id.clone()), ""))
        } else {
            Some((false, Some(m.id.clone()), "host_not_allowed"))
        }
    }

    /// Whether a login callback may be offered for `url` (its host on the plugin's `links.login`).
    fn login_host_ok(&self, plugin: Option<&str>, url: &str) -> bool {
        let Some(p) = plugin.and_then(|id| self.plugins.get(id)) else {
            return false;
        };
        let host = tmuxedo_protocol::needs::url_host(url).unwrap_or_default();
        let allow = p.manifest().links.as_ref().map(|l| l.login.as_slice()).unwrap_or(&[]);
        tmuxedo_protocol::needs::host_allowed(&host, allow)
    }

    /// `pane.open_url` from a pane's opener stand-in: validate and emit a
    /// non-durable `open_url` event (seq 0, never queued for acks). With the
    /// caller check on (every real runner), the event says whether the link
    /// is `verified`: from the pane's agent, to a host its plugin lists.
    /// Clients show any other as an unverified link (Phase 13).
    fn open_url(&mut self, p: &Value, peer: Option<u32>) -> crate::api::ApiResult {
        let url = p["url"].as_str().unwrap_or("");
        if !crate::open_url::valid_url(url) {
            return Err(crate::api::err(
                "invalid_url",
                "only http(s) URLs up to 8 KiB can be opened",
            ));
        }
        let pane = p["pane_id"].as_str().unwrap_or("").to_string();
        let program = p["program"].as_str().unwrap_or("").chars().take(64).collect::<String>();
        log!("open_url from {pane} ({program}): {url}");
        let mut data = json!({ "url": url, "source": "browser-shim", "program": program });
        let verdict = self.open_url_verdict(&pane, url, peer);
        if let Some((verified, plugin, reason)) = &verdict {
            data["verified"] = json!(verified);
            if let Some(id) = plugin {
                data["plugin"] = json!(id);
            }
            if !verified {
                data["reason"] = json!(reason);
                log!("open_url from {pane}: unverified link ({reason})");
            }
        }
        // Only the agent's own login, to its plugin's login hosts, gets the relay.
        let relay_ok = match &verdict {
            None => true,
            Some((verified, plugin, _)) => *verified && self.login_host_ok(plugin.as_deref(), url),
        };
        // A login whose OAuth redirect goes to this machine's loopback: mark
        // it, so the user can paste the address their browser ended on.
        if let Some(cb) = crate::login_relay::detect(url).filter(|_| relay_ok) {
            log!(
                "open_url from {pane}: login callback on {}:{}{}",
                cb.host,
                cb.port,
                cb.path
            );
            data["callback"] = cb.to_json();
            self.login_relay.record(&pane, cb, Instant::now());
        }
        self.queue_msg(Message::Text(
            RunnerFrame::Event {
                seq: 0,
                pane_id: pane,
                kind: "open_url".into(),
                at: iso_now(),
                data,
            }
            .to_json()
            .into(),
        ));
        Ok(json!({ "type": "ok" }))
    }

    /// `login_callback` from the gateway: check the pasted URL against the
    /// pane's recorded callbacks (login_relay.rs) and, if it passes, GET it
    /// on the loopback port in the background; the `res` follows from
    /// [`Internal::LoginDelivered`]. The URL (its `code`) is never logged.
    fn login_callback(&mut self, id: String, pane_id: String, url: String) {
        use crate::login_relay::{listening_addr, Refusal};
        let mut addr = None;
        let checked = self.login_relay.check(&pane_id, &url, Instant::now(), |port| {
            addr = listening_addr(port);
            addr.is_some()
        });
        let accepted = match checked {
            Ok(a) => a,
            Err(r) => {
                log!(
                    "login_callback for {pane_id} ({}): refused, {}",
                    crate::login_relay::host_of(&url),
                    r.code()
                );
                self.queue_msg(Message::Text(
                    RunnerFrame::err(id, r.code(), r.message()).to_json().into(),
                ));
                return;
            }
        };
        let Some(addr) = addr else {
            let r = Refusal::NotListening;
            self.queue_msg(Message::Text(
                RunnerFrame::err(id, r.code(), r.message()).to_json().into(),
            ));
            return;
        };
        let port = accepted.callback.port;
        log!(
            "login_callback for {pane_id}: delivering to {addr}{}",
            accepted.callback.path
        );
        let tx = self.internal_tx.clone();
        tokio::spawn(async move {
            let result = crate::login_relay::deliver(addr, &accepted).await;
            let _ = tx.send(Internal::LoginDelivered {
                id,
                pane_id,
                port,
                result,
            });
        });
    }

    fn register(&mut self, pane_id: &str) {
        let Some(p) = self.state.pane(pane_id) else { return };
        let mut rt = PaneRt::new(
            pane_id,
            p.holder_id(),
            p.base,
            p.pane.cols,
            p.pane.rows,
            self.secrets.shared(),
        )
        .with_history(self.opts.history);
        rt.track.reported_kind = None;
        self.by_hid.insert(rt.hid.clone(), pane_id.to_string());
        if let Some(old) = self.panes.insert(pane_id.to_string(), rt) {
            if old.hid != self.panes[pane_id].hid {
                self.by_hid.remove(&old.hid);
            }
        }
    }

    /// Spawn a pane's process in the holder (its record exists in `state`).
    /// `argv` None = the record's own argv.
    pub(crate) fn spawn_pane(&mut self, pane_id: &str, argv: Option<Vec<String>>) {
        let Some(rec) = self.state.pane(pane_id) else { return };
        let argv = argv.unwrap_or_else(|| rec.argv.clone());
        let hid = rec.holder_id();
        let Some(spec) = self.spawn_spec(pane_id, &hid, argv) else {
            return;
        };
        self.register(pane_id);
        if self.opts.pane_history {
            self.load_history(pane_id);
        }
        let holder = self.holder.clone();
        let tx = self.internal_tx.clone();
        let pane_id = pane_id.to_string();
        tokio::spawn(async move {
            let result = holder.spawn(spec).await.map(|v| v["pid"].as_u64().unwrap_or(0) as u32);
            let _ = tx.send(Internal::Spawned { pane_id, hid, result });
        });
    }

    /// Find our panes in the holder; spawn (or resume) the missing ones; attach
    /// to all from 0 (the ring rebuilds the emulator and the mirror).
    async fn adopt_panes(&mut self) -> Result<(), String> {
        let list = self.holder.list().await?;
        let held: HashMap<String, Value> = list["panes"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|p| Some((p["id"].as_str()?.to_string(), p)))
            .collect();
        let ids: Vec<String> = self.state.panes.iter().map(|p| p.pane.id.clone()).collect();
        let mut closed_exited = vec![];
        for id in ids {
            let hid = self.state.pane(&id).map(|p| p.holder_id()).unwrap_or_default();
            match held.get(&hid) {
                Some(h) if h["exited"].is_null() => {
                    log!("reattaching {id} ({hid})");
                    self.register(&id);
                    // The same process as before the restart: its helpers still run.
                    let saved = self.state.pane(&id).map(|p| p.meta.helpers.clone());
                    if let Some(rt) = self.panes.get_mut(&id) {
                        let now = Instant::now();
                        rt.track.children = crate::subagents::Children::restore(&saved.unwrap_or_default(), now);
                        rt.pid = h["pid"].as_u64().unwrap_or(0) as u32;
                        let (c, r) = (
                            h["cols"].as_u64().unwrap_or(80) as u16,
                            h["rows"].as_u64().unwrap_or(24) as u16,
                        );
                        rt.cols = c;
                        rt.rows = r;
                        rt.emu.resize(r, c);
                    }
                    self.holder.attach(&hid, 0).await?;
                }
                Some(_) => {
                    // Exited while we were away: the pane closes.
                    let _ = self.holder.remove(&hid).await;
                    closed_exited.push(id);
                }
                None => {
                    let fresh = self
                        .state
                        .pane(&id)
                        .map(|p| p.high_water == 0 && p.base == 0)
                        .unwrap_or(false);
                    if fresh {
                        self.spawn_pane(&id, None);
                    } else {
                        self.restore_pane(&id);
                    }
                }
            }
        }
        for id in closed_exited {
            if let Ok(c) = self.state.close_pane(&id) {
                log!("{id}: its process exited while the runner was away; closed");
                let _ = c;
            }
        }
        if self.state.workspaces.is_empty() {
            self.ensure_a_pane();
        }
        self.state.rev += 1;
        self.persist();
        Ok(())
    }

    /// Holder restart or pod loss: a new incarnation in the saved cwd,
    /// resuming the agent session if one was reported and the cwd guard allows.
    ///
    /// The saved cwd is first moved off the pre-rename hosted home
    /// (`/home/aimux/…` → `$HOME/…`). An agent whose plugin sets
    /// `resume.leave_home` (Claude: it never remembers trust for `$HOME`) and
    /// whose session was recorded in `$HOME` itself resumes in the default
    /// working folder instead; sessions in any other folder resume where they
    /// were.
    fn restore_pane(&mut self, id: &str) {
        use crate::launch_config::{is_home, rewrite_legacy_home, LEGACY_HOME};
        self.state.next_incarnation(id);
        let Some(rec) = self.state.pane(id).cloned() else {
            return;
        };
        let home = self.opts.home.clone();
        let mut cwd = rewrite_legacy_home(&rec.pane.cwd, &home, LEGACY_HOME);
        let mut moved_from_home = false;
        if let (Some(kind), Some(_)) = (&rec.meta.kind, &rec.meta.session_id) {
            let leave_home = self
                .plugins
                .by_label(kind)
                .is_some_and(|p| p.manifest().resume.as_ref().is_some_and(|r| r.leave_home));
            if leave_home && is_home(&cwd, &home, LEGACY_HOME) {
                let work = self.work_dir();
                if !is_home(&work, &home, LEGACY_HOME) {
                    cwd = work;
                    moved_from_home = true;
                }
            }
        }
        if cwd != rec.pane.cwd {
            if let Some(p) = self.state.pane_mut(id) {
                log!("{id}: cwd {} -> {cwd}", p.pane.cwd);
                p.pane.cwd = cwd.clone();
                if moved_from_home {
                    p.meta.cwd_was_repo = inject::is_repo(&cwd);
                }
            }
        }
        let Some(rec) = self.state.pane(id).cloned() else {
            return;
        };
        let mut argv = None;
        if let (Some(kind), Some(session)) = (rec.meta.kind.clone(), rec.meta.session_id.clone()) {
            match inject::resume_cwd_ok(&rec.pane.cwd, rec.meta.cwd_was_repo) {
                Ok(()) => {
                    let shim = self.shim_dir.clone().unwrap_or_default();
                    argv = Some(inject::resume_argv_with(
                        &self.plugins,
                        &self.shell,
                        &shim,
                        &kind,
                        &session,
                        rec.meta.resume_argv.as_deref(),
                    ));
                    log!("{id}: resuming {kind} session {session} in {}", rec.pane.cwd);
                    let mut data = json!({ "kind": kind, "sessionId": session, "cwd": rec.pane.cwd });
                    if moved_from_home {
                        data["movedFrom"] = json!(home);
                    }
                    self.queue_event(id, "resumed", data);
                }
                Err(reason) => {
                    log!("{id}: not resuming {kind} session {session}: {reason}");
                    let work = (!std::path::Path::new(&rec.pane.cwd).is_dir()).then(|| self.work_dir());
                    if let Some(p) = self.state.pane_mut(id) {
                        p.meta.session_id = None;
                        if let Some(w) = work {
                            p.pane.cwd = w;
                        }
                    }
                    self.queue_event(
                        id,
                        "resume_refused",
                        json!({ "kind": kind, "sessionId": session, "reason": reason }),
                    );
                }
            }
        } else {
            log!(
                "{id}: holder lost its process; starting a fresh shell in {}",
                rec.pane.cwd
            );
        }
        if argv.is_none() {
            argv = Some(rec.argv.clone());
        }
        self.spawn_pane(id, argv);
    }

    /// Never leave a machine without a terminal.
    pub(crate) fn ensure_a_pane(&mut self) {
        let argv = self.default_argv();
        let cwd = self.work_dir();
        let (_, _, pane) = self.state.create_workspace(&cwd, None, argv, BTreeMap::new());
        self.spawn_pane(&pane, None);
    }

    /// Replay the holder rings into the emulators before talking to the
    /// gateway, so an `attach {from}` right after reconnecting finds its
    /// offset in the mirror (replay) instead of falling back to a snapshot.
    async fn catch_up(
        &mut self,
        rx: &mut mpsc::Receiver<HolderEvent>,
        internal: &mut mpsc::UnboundedReceiver<Internal>,
    ) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let done = self
                .panes
                .values()
                .all(|p| p.catchup.is_some_and(|e| p.holder_pos.unwrap_or(0) >= e));
            if done || self.exit.is_some() {
                return;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            tokio::select! {
                ev = rx.recv() => match ev {
                    Some(ev) => self.on_holder(ev).await,
                    None => {
                        self.exit = Some(ExitReason::HolderLost);
                        return;
                    }
                },
                Some(m) = internal.recv() => self.on_internal(m).await,
                _ = tokio::time::sleep(left) => {
                    log!("holder catch-up incomplete after 5s; continuing");
                    return;
                }
            }
        }
    }

    async fn main_loop(
        mut self,
        mut holder_rx: mpsc::Receiver<HolderEvent>,
        mut internal_rx: mpsc::UnboundedReceiver<Internal>,
    ) -> Result<ExitReason, String> {
        self.catch_up(&mut holder_rx, &mut internal_rx).await;
        let (gw_tx, mut gw_rx) = mpsc::channel::<GwEvent>(256);
        let gw_task = tokio::spawn(gateway::connection_loop(
            GatewayOptions {
                url: self.opts.gateway_url.clone(),
                token: self.opts.token.clone(),
                backoff_min: self.opts.backoff_min,
                backoff_max: self.opts.backoff_max,
                unsupported_wait: Duration::from_secs(15 * 60),
                signer: self.opts.signer.clone(),
                files: self.files_in.clone(),
            },
            gw_tx,
        ));
        let (local_tx, mut local_rx) = mpsc::channel::<LocalReq>(64);
        let local_task = match crate::local::serve(&self.opts.runner_socket, local_tx).await {
            Ok(t) => {
                if let Some(legacy) = &self.opts.legacy_runner_socket {
                    crate::migrate::link_legacy_runner_socket(&self.opts.runner_socket, legacy);
                }
                Some(t)
            }
            Err(e) => {
                log!(
                    "local socket {}: {e} (runner API and hooks unavailable)",
                    self.opts.runner_socket.display()
                );
                None
            }
        };
        let (exec_tx, mut exec_rx) = mpsc::unbounded_channel::<crate::secrets::exec::ExecAsk>();
        let exec_sock = crate::secrets::exec::socket_path(&self.opts.runner_socket);
        if let Err(e) = crate::secrets::exec::serve(&exec_sock, exec_tx) {
            log!(
                "secret exec socket {}: {e} (`tmuxedo secret exec` unavailable)",
                exec_sock.display()
            );
        }
        let (ssh_tx, mut ssh_rx) = mpsc::unbounded_channel::<crate::secrets::ssh::Ask>();
        let ssh_sock = crate::secrets::ssh::socket_path(&self.opts.runner_socket);
        if let Err(e) = crate::secrets::ssh::serve(&ssh_sock, ssh_tx) {
            log!("ssh agent socket {}: {e} (SSH keys unavailable)", ssh_sock.display());
        }
        let mut tick = tokio::time::interval(COALESCE);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut slow = tokio::time::interval(Duration::from_secs(5));
        // The memory watchdog: every second. Below the threshold it reads two
        // small cgroup files; at 5 s a fast allocation outran it to the OOM.
        let mut memory = tokio::time::interval(Duration::from_secs(1));
        memory.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut update = tokio::time::interval_at(
            tokio::time::Instant::now() + self.opts.update_after,
            Duration::from_secs(6 * 3600),
        );
        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).map_err(|e| e.to_string())?;
        let mut sigint =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()).map_err(|e| e.to_string())?;

        while self.exit.is_none() {
            tokio::select! {
                ev = holder_rx.recv() => {
                    match ev {
                        Some(ev) => self.on_holder(ev).await,
                        None => self.exit = Some(ExitReason::HolderLost),
                    }
                }
                ev = gw_rx.recv() => {
                    if let Some(ev) = ev { self.on_gateway(ev).await }
                }
                Some(req) = local_rx.recv() => self.on_local(req).await,
                Some(m) = internal_rx.recv() => self.on_internal(m).await,
                Some(a) = exec_rx.recv() => self.on_exec_ask(a),
                Some(a) = ssh_rx.recv() => {
                    self.on_ssh_ask(a);
                    if std::mem::take(&mut self.ssh_flush) {
                        self.flush_events().await;
                    }
                }
                _ = tick.tick() => self.on_tick().await,
                _ = slow.tick() => self.on_slow().await,
                _ = memory.tick(), if self.opts.watchdog => self.watchdog().await,
                _ = update.tick(), if self.opts.auto_update => self.check_update(),
                _ = sigterm.recv() => self.exit = Some(ExitReason::Signal),
                _ = sigint.recv() => self.exit = Some(ExitReason::Signal),
            }
        }
        let reason = self.exit.clone().unwrap_or(ExitReason::Signal);
        self.persist();
        if let Some(gw) = self.gw.take() {
            if !matches!(reason, ExitReason::Fatal(_)) {
                let close = gateway::close_message(tmuxedo_protocol::close::GOING_AWAY, "runner restarting");
                let _ = tokio::time::timeout(Duration::from_millis(500), gw.out.send(close)).await;
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
        gw_task.abort();
        if let Some(t) = local_task {
            t.abort();
        }
        let _ = std::fs::remove_file(&self.opts.runner_socket);
        let _ = std::fs::remove_file(crate::secrets::exec::socket_path(&self.opts.runner_socket));
        let _ = std::fs::remove_file(crate::secrets::ssh::socket_path(&self.opts.runner_socket));
        Ok(reason)
    }

    // ── sending ───────────────────────────────────────────────────────────

    pub(crate) async fn send_out(&mut self, out: Vec<Out>) {
        for o in out {
            let Out::Bin(b) = o;
            if !b.is_empty() {
                self.send_msg(Message::Binary(b.into())).await;
            }
        }
    }

    async fn send_msg(&mut self, m: Message) {
        self.queue_msg(m);
    }

    /// Queue a frame for the gateway without ever waiting (audit P2-5): the
    /// bounded outbound queue first, else an ordered overflow drained on
    /// every tick. A stalled socket (overflow over [`GW_OVERFLOW_MAX`], or no
    /// progress for [`GW_STALL`]; both in [`RunOptions`]) resets the connection; the reconnect
    /// resyncs topology, status, events (outbox) and panes (offsets).
    pub(crate) fn queue_msg(&mut self, m: Message) {
        let Some(gw) = &self.gw else { return };
        // Every text frame passes the masker too (status prompts, titles,
        // replies, events): a known secret value never leaves as text.
        let m = match m {
            Message::Text(t) => match crate::secrets::mask_text_frame(&self.secrets.set(), t.as_str()) {
                Some(masked) => Message::Text(masked.into()),
                None => Message::Text(t),
            },
            other => other,
        };
        if self.gw_overflow.is_empty() {
            match gw.out.try_send(m) {
                Ok(()) => return,
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    self.gw = None;
                    return;
                }
                Err(mpsc::error::TrySendError::Full(m)) => self.overflow_push(m),
            }
        } else {
            self.overflow_push(m);
        }
        if self.gw_overflow_bytes > self.opts.gw_overflow_max {
            self.gw_reset("outbound overflow over its limit");
        }
    }

    fn overflow_push(&mut self, m: Message) {
        self.gw_overflow_bytes += m.len();
        self.gw_overflow.push_back(m);
        self.gw_stuck_since.get_or_insert_with(Instant::now);
    }

    /// Move overflowed frames into the outbound queue as it frees up.
    fn flush_overflow(&mut self) {
        let Some(gw) = self.gw.clone() else {
            self.gw_overflow.clear();
            self.gw_overflow_bytes = 0;
            self.gw_stuck_since = None;
            return;
        };
        let mut moved = false;
        while let Some(m) = self.gw_overflow.pop_front() {
            let len = m.len();
            match gw.out.try_send(m) {
                Ok(()) => {
                    self.gw_overflow_bytes -= len;
                    moved = true;
                }
                Err(mpsc::error::TrySendError::Full(m)) => {
                    self.gw_overflow.push_front(m);
                    break;
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    self.gw = None;
                    break;
                }
            }
        }
        if self.gw_overflow.is_empty() {
            self.gw_stuck_since = None;
        } else if moved {
            self.gw_stuck_since = Some(Instant::now());
        } else if self.gw_stuck_since.is_some_and(|t| t.elapsed() > self.opts.gw_stall) {
            self.gw_reset("outbound queue made no progress");
        }
    }

    /// Drop a stalled connection: the connection task closes the socket and
    /// reconnects; until then this side behaves as disconnected.
    fn gw_reset(&mut self, why: &str) {
        log!(
            "gateway connection stalled ({why}; {} frames, {} bytes waiting); reconnecting",
            self.gw_overflow.len(),
            self.gw_overflow_bytes
        );
        if let Some(gw) = self.gw.take() {
            gw.reset.notify_one();
        }
        self.gw_overflow.clear();
        self.gw_overflow_bytes = 0;
        self.gw_stuck_since = None;
        self.welcomed = false;
        for rt in self.panes.values_mut() {
            rt.detach();
        }
        self.chat_gateway_reset();
    }

    /// `project.env {cwd, previous}` (the shell's cd hook): what to export and
    /// unset in a shell now in `cwd`, given the names its last project set.
    fn project_env(&mut self, p: &Value) -> crate::api::ApiResult {
        let cwd = p["cwd"].as_str().unwrap_or("/");
        let previous: Vec<String> = p["previous"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|x| x.as_str().map(String::from))
            .filter(|n| crate::launch_config::valid_env_name(n))
            .collect();
        let machine = self.delivery.machine.clone();
        let key = self
            .projects
            .project_for(std::path::Path::new(cwd), &machine, &self.launch.projects);
        let mut base = self.launch.shell.env.clone();
        base.extend(self.pane_secret_env());
        let (set, unset, touched) =
            crate::project::shell_delta(key.as_ref().map(|k| &self.launch.projects[k]), &previous, &base);
        Ok(json!({ "project": key, "set": set, "unset": unset, "names": touched }))
    }

    /// Connected and welcomed: a request can reach web.
    pub(crate) fn gateway_ready(&self) -> bool {
        self.gw.is_some() && self.welcomed
    }

    pub(crate) async fn send_frame(&mut self, f: RunnerFrame) {
        self.send_msg(Message::Text(f.to_json().into())).await;
    }

    async fn send_topology(&mut self) {
        let mut panes = vec![];
        for p in &self.state.panes {
            let mut pane = p.pane.clone();
            if let Some(rt) = self.panes.get(&p.pane.id) {
                pane.agent = rt.track.agent(p.meta.session_id.as_deref());
                pane.cols = rt.cols;
                pane.rows = rt.rows;
            }
            panes.push(pane);
        }
        let f = RunnerFrame::Topology {
            rev: self.state.rev,
            workspaces: self.state.protocol_workspaces(),
            tabs: self.state.protocol_tabs(),
            panes,
            focused_workspace: self.state.focused_workspace.clone(),
        };
        self.send_frame(f).await;
    }

    /// A topology change: bump rev, persist, tell the gateway.
    pub(crate) async fn commit_topology(&mut self) {
        self.state.rev += 1;
        self.persist();
        self.send_topology().await;
    }

    /// A durable `event` frame: seq persisted first, then the event is
    /// appended (fsynced) to the on-disk outbox until `events_ack`.
    pub(crate) fn queue_event(&mut self, pane_id: &str, kind: &str, mut data: Value) {
        // Masked before it is written to the on-disk outbox.
        crate::secrets::mask_json(&self.secrets.set(), &mut data);
        self.state.event_seq += 1;
        let seq = self.state.event_seq;
        self.persist();
        self.outbox.push(RunnerFrame::Event {
            seq,
            pane_id: pane_id.into(),
            kind: kind.into(),
            at: iso_now(),
            data,
        });
    }

    pub(crate) async fn send_event(&mut self, pane_id: &str, kind: &str, data: Value) {
        self.queue_event(pane_id, kind, data);
        self.flush_events().await;
    }

    /// Send queued events not yet sent on this connection (only after
    /// `welcome`; a reconnect resends everything unacknowledged).
    async fn flush_events(&mut self) {
        if self.gw.is_none() || !self.welcomed {
            return;
        }
        for f in self.outbox.take_unsent() {
            self.send_frame(f).await;
        }
    }

    // ── runner API events ──────────────────────────────────────────────────────

    pub(crate) fn emit(&mut self, ev: Event) {
        let line = ev.envelope().to_string();
        self.subscribers.retain(|s| !s.tx.is_closed());
        for s in &self.subscribers {
            if s.wants_lifecycle(&ev.kind) {
                let _ = s.tx.send(line.clone());
            }
        }
        self.check_event_waiters(&ev);
    }

    // ── holder ────────────────────────────────────────────────────────────

    async fn on_holder(&mut self, ev: HolderEvent) {
        match ev {
            HolderEvent::Attached { id, from, gap, .. } if self.chat_owns(&id) => {
                self.chat_attached_holder(&id, from, gap).await
            }
            HolderEvent::Data { id, offset, bytes } if self.chat_owns(&id) => self.chat_data(&id, offset, &bytes).await,
            HolderEvent::Exit { id, code, signal } if self.chat_owns(&id) => self.chat_exit(&id, code, signal).await,
            HolderEvent::Attached { id, from, gap, end } => {
                let Some(pid) = self.by_hid.get(&id).cloned() else {
                    return;
                };
                let Some(rt) = self.panes.get_mut(&pid) else { return };
                rt.holder_pos = Some(from);
                rt.catchup = Some(end);
                rt.mask.reset();
                if std::mem::take(&mut rt.restarting) {
                    rt.emu.reset();
                    rt.mirror.reset_at(rt.base + from);
                } else if rt.mirror.end() != rt.base + from {
                    rt.mirror.reset_at(rt.base + from);
                }
                rt.pending.clear();
                rt.pending_start = rt.base + from;
                if gap {
                    log!("{pid}: holder ring wrapped before attach; repainting");
                    rt.emu.reset_screen();
                    self.nudge(&pid);
                }
            }
            HolderEvent::Data { id, offset, bytes } => {
                let Some(pid) = self.by_hid.get(&id).cloned() else {
                    return;
                };
                let holder = self.holder.clone();
                let mut out = vec![];
                let gap = match self.panes.get_mut(&pid) {
                    Some(rt) => rt.ingest(offset, &bytes, &holder, &mut out),
                    None => return,
                };
                if gap {
                    log!("{pid}: gap in holder stream; repainting");
                    self.nudge(&pid);
                }
                self.send_out(out).await;
                if self.panes.get(&pid).is_some_and(|p| p.pending.len() >= COALESCE_BYTES) {
                    self.flush_round(false).await;
                }
            }
            HolderEvent::Exit { id, code, signal } => {
                let Some(pid) = self.by_hid.get(&id).cloned() else {
                    let _ = self.holder.remove(&id).await;
                    return;
                };
                if self.task_runs_pane(&pid) {
                    // A fast command can exit before its output is attached: wait for it.
                    self.task_exit_later(pid, id, code, signal, 0);
                    return;
                }
                self.on_exit(&pid, &id, code, signal).await;
            }
            HolderEvent::Closed => self.exit = Some(ExitReason::HolderLost),
        }
    }

    async fn on_exit(&mut self, pane_id: &str, hid: &str, code: Option<i32>, signal: Option<String>) {
        // A headless task's command: keep its result before the pane goes.
        self.task_pane_exited(pane_id, code, signal.clone());
        // Let the last bytes reach viewers.
        let mut out = vec![];
        if let Some(rt) = self.panes.get_mut(pane_id) {
            rt.flush(&mut out);
            rt.alive = false;
            rt.track.clear_agent();
        }
        self.send_out(out).await;
        let closing = self.panes.get(pane_id).is_some_and(|p| p.closing);
        self.by_hid.remove(hid);
        self.panes.remove(pane_id);
        let holder = self.holder.clone();
        let hid = hid.to_string();
        tokio::spawn(async move {
            let _ = holder.remove(&hid).await;
        });
        self.remove_history(pane_id);
        if closing {
            return;
        }
        log!("{pane_id}: process exited (code {code:?}, signal {signal:?}); closing the pane");
        let ws = self.state.workspace_of(pane_id).map(|w| w.0).unwrap_or_default();
        self.emit(Event::new(
            "pane_exited",
            json!({ "pane_id": pane_id, "workspace_id": ws }),
        ));
        self.fail_pane_waiters(pane_id);
        if let Ok(closed) = self.state.close_pane(pane_id) {
            self.emit_closed(&closed);
        }
        if self.state.workspaces.is_empty() {
            self.ensure_a_pane();
            self.emit_created_for_new_workspace();
        }
        self.commit_topology().await;
    }

    pub(crate) fn emit_closed(&mut self, c: &Closed) {
        for p in &c.panes {
            let ws = c
                .tabs
                .first()
                .map(|t| t.1.clone())
                .or_else(|| self.state.workspace_of(p).map(|w| w.0))
                .unwrap_or_default();
            self.emit(Event::new("pane_closed", json!({ "pane_id": p, "workspace_id": ws })));
        }
        for (t, w) in &c.tabs {
            self.emit(Event::new("tab_closed", json!({ "tab_id": t, "workspace_id": w })));
        }
        for w in &c.workspaces {
            self.emit(Event::new(
                "workspace_closed",
                json!({ "workspace_id": w, "workspace": null }),
            ));
        }
    }

    fn emit_created_for_new_workspace(&mut self) {
        if let Some(w) = self.state.workspaces.last().map(|w| w.id.clone()) {
            let info = self.workspace_info(&w);
            self.emit(Event::new("workspace_created", json!({ "workspace": info })));
        }
    }

    /// Close panes' processes; their exits drop the runtime state.
    pub(crate) fn kill_panes(&mut self, ids: &[String]) {
        for id in ids {
            if let Some(rt) = self.panes.get_mut(id) {
                rt.closing = true;
                let hid = rt.hid.clone();
                self.holder.kill_nowait(&hid, libc::SIGHUP);
                self.timers.push((
                    Instant::now() + Duration::from_secs(3),
                    Timer::KillHard(id.clone(), hid),
                ));
            }
            self.fail_pane_waiters(id);
            let held: Vec<String> = self
                .prompts
                .iter()
                .filter(|(_, p)| &p.pane_id == id)
                .map(|(k, _)| k.clone())
                .collect();
            for k in held {
                if let Some(p) = self.prompts.remove(&k) {
                    crate::api::respond(p.reply, Ok(json!({})), self);
                }
            }
        }
    }

    /// Force a full repaint: rows-1, then rows a moment later.
    fn nudge(&mut self, pane_id: &str) {
        if let Some(rt) = self.panes.get(pane_id) {
            if rt.rows > 1 {
                self.holder.resize(&rt.hid, rt.cols, rt.rows - 1);
                self.timers
                    .push((Instant::now() + Duration::from_millis(60), Timer::Nudge(pane_id.into())));
            }
        }
    }

    async fn on_internal(&mut self, m: Internal) {
        match m {
            Internal::Spawned { pane_id, hid, result } => match result {
                Ok(pid) => {
                    if let Some(rt) = self.panes.get_mut(&pane_id) {
                        if rt.hid == hid {
                            rt.pid = pid;
                            rt.alive = true;
                        }
                    }
                    self.holder.attach_nowait(&hid, 0);
                }
                Err(e) => {
                    log!("{pane_id}: could not start its process: {e}");
                    self.panes.remove(&pane_id);
                    self.by_hid.remove(&hid);
                    if let Ok(closed) = self.state.close_pane(&pane_id) {
                        self.emit_closed(&closed);
                        if self.state.workspaces.is_empty() {
                            self.ensure_a_pane();
                        }
                        self.commit_topology().await;
                    }
                }
            },
            Internal::UpdateReady => self.exit = Some(ExitReason::Updated),
            Internal::TaskExit {
                pane_id,
                hid,
                code,
                signal,
                tries,
            } => {
                let attached = self.panes.get(&pane_id).is_some_and(|p| p.holder_pos.is_some());
                if !attached && tries < 20 && self.panes.contains_key(&pane_id) {
                    self.task_exit_later(pane_id, hid, code, signal, tries + 1);
                } else if self.by_hid.get(&hid) == Some(&pane_id) {
                    self.on_exit(&pane_id, &hid, code, signal).await;
                }
            }
            Internal::Fs { reply, result, frames } => {
                if matches!(reply, ReplyTo::Gateway(_)) {
                    for f in frames {
                        self.queue_msg(Message::Binary(f.into()));
                    }
                }
                crate::api::respond(reply, result, self)
            }
            Internal::Files(o) => self.on_files(o).await,
            Internal::Confirmed { question, result } => self.on_confirmed(question, result).await,
            Internal::LoginDelivered {
                id,
                pane_id,
                port,
                result,
            } => {
                let f = match result {
                    Ok(d) => {
                        log!("login_callback for {pane_id}: port {port} answered {}", d.status);
                        RunnerFrame::ok(id, d.to_json())
                    }
                    Err(e) => {
                        log!("login_callback for {pane_id}: port {port}: {e}");
                        let (code, msg) = match e.as_str() {
                            "timeout" => ("timeout", "The login did not answer within 10 seconds.".to_string()),
                            "not_listening" => (
                                "not_listening",
                                crate::login_relay::Refusal::NotListening.message().to_string(),
                            ),
                            _ => ("unreachable", format!("The login could not be reached ({e}).")),
                        };
                        RunnerFrame::err(id, code, msg)
                    }
                };
                self.send_frame(f).await;
            }
        }
    }

    // ── gateway ───────────────────────────────────────────────────────────

    async fn on_gateway(&mut self, ev: GwEvent) {
        match ev {
            GwEvent::Connected(tx) => {
                self.gw = Some(tx);
                self.gw_overflow.clear();
                self.gw_overflow_bytes = 0;
                self.gw_stuck_since = None;
                for rt in self.panes.values_mut() {
                    rt.detach();
                }
                // The gateway sends the glance set again after welcome.
                self.glances.clear();
                self.stats.set_rate(None, Instant::now());
                self.usage.resend();
                self.chat_gateway_reset();
                let hello = self.hello_info();
                self.send_frame(RunnerFrame::hello(hello)).await;
                self.send_topology().await;
                let statuses: Vec<RunnerFrame> = self
                    .panes
                    .values()
                    .filter_map(|rt| {
                        let s = self.state.pane(&rt.id)?.meta.session_id.as_deref();
                        rt.track.agent(s).map(|a| RunnerFrame::Status {
                            pane_id: rt.id.clone(),
                            agent: Some(a),
                        })
                    })
                    .collect();
                for f in statuses {
                    self.send_frame(f).await;
                }
                self.welcomed = false;
                self.outbox.reset_sent();
                if let Some(f) = &self.files {
                    f.send(crate::files::worker::Msg::DiskNow);
                }
            }
            GwEvent::Disconnected { .. } => {
                self.gw = None;
                self.welcomed = false;
                self.secrets_disconnected();
                self.glances.clear();
                self.stats.set_rate(None, Instant::now());
                self.usage.resend();
                self.chat_gateway_reset();
                for rt in self.panes.values_mut() {
                    rt.detach();
                }
            }
            GwEvent::Fatal(code) => {
                log!(
                    "gateway closed with {code} ({}); not reconnecting",
                    if code == tmuxedo_protocol::close::REVOKED {
                        "token revoked"
                    } else {
                        "replaced by another runner"
                    }
                );
                self.exit = Some(ExitReason::Fatal(code));
            }
            // Frames still queued from a connection this side already dropped
            // (a stall reset): its requests' replies would go to the next
            // connection, and they delay its welcome. Drop them.
            GwEvent::Text(_) | GwEvent::Binary(_) if self.gw.is_none() => {}
            GwEvent::Text(t) => match tmuxedo_protocol::frames::parse_gateway(&t) {
                Parsed::Frame(f) => self.on_frame(f).await,
                Parsed::Unknown(_) => {}
                Parsed::Invalid(why) => log!("ignored gateway frame ({why})"),
            },
            GwEvent::Binary(b) => {
                let Some(f) = DataFrame::decode(&b) else {
                    log!("ignored binary frame (invalid)");
                    return;
                };
                if f.kind != DataKind::Input {
                    return;
                }
                if !self.opts.guardrails.allows_control() {
                    return; // observe-only: viewers watch, nobody types
                }
                let size = f.payload.len();
                let w = ControlWork::Input {
                    pane_id: f.pane_id,
                    bytes: f.payload,
                };
                self.control(w, size, true, "typing").await;
            }
        }
    }

    async fn on_frame(&mut self, f: GatewayFrame) {
        match f {
            GatewayFrame::Welcome {
                machine_id,
                user_id,
                last_event_seq,
                features,
            } => {
                self.wire.gateway_features = features.unwrap_or_default();
                if let Some(why) = self.opts.guardrails.check_welcome(&user_id) {
                    log!("{why}");
                    self.exit = Some(ExitReason::Fatal(tmuxedo_protocol::close::REVOKED));
                    return;
                }
                log!("welcomed as machine {machine_id} (user {user_id}, web has events up to {last_event_seq})");
                // Never number an event at or below what web already stored.
                if last_event_seq > self.state.event_seq {
                    log!(
                        "event seq {} is behind web's {last_event_seq}; continuing above it",
                        self.state.event_seq
                    );
                    self.state.event_seq = last_event_seq;
                }
                let mut seq = self.state.event_seq;
                let n = self.outbox.renumber_at_or_below(last_event_seq, || {
                    seq += 1;
                    seq
                });
                if n > 0 {
                    log!("renumbered {n} unacknowledged events above {last_event_seq}");
                }
                self.state.event_seq = seq;
                self.persist();
                self.welcomed = true;
                self.secrets_welcomed(&machine_id);
                self.outbox.reset_sent();
                self.flush_events().await;
                self.send_ui_frames().await;
                self.needs_resend().await;
            }
            GatewayFrame::EventsAck { up_to } => self.outbox.ack(up_to),
            GatewayFrame::SecretRes { id, result, error } => self.on_secret_res(id, result, error),
            GatewayFrame::Secrets { mask, policy, revoked } => self.on_secrets_frame(mask, policy, revoked),
            GatewayFrame::Config {
                claude,
                plugins,
                shell,
                work_dir,
                chat,
                projects,
                plugin_set,
                roles,
            } => {
                // Kept for the plugins' builders (crate::plugin_gen, below).
                crate::plugin_set::apply(&self.opts.state_dir, plugin_set.as_deref());
                crate::role_set::apply(&self.opts.state_dir, roles.as_deref());
                if let Some(c) = chat {
                    self.chat_apply_policy(&c).await;
                }
                // Dual-write: `plugins[claude].settings.launch.args` wins over
                // `claude.args` when a newer web sends both.
                let mut claude = claude;
                if let Some(cp) = crate::plugin_wire::read_config_plugins(plugins.as_ref()) {
                    if let Some(a) = cp.claude_args {
                        claude = Some(json!({ "args": a }));
                    }
                    if cp.keystroke != self.needs.keystroke {
                        self.needs.keystroke = cp.keystroke;
                        crate::needs::save_keystroke_wishes(&self.opts.state_dir, &self.needs.keystroke);
                        log!("keystroke answers: {:?}", self.needs.keystroke);
                        // A dialog already on screen may be answerable now (crate::plugin_rt).
                        self.prt.rescan();
                    }
                    if cp.usage != self.wire.usage_wanted {
                        self.apply_usage_wishes(cp.usage.clone());
                    }
                    if cp.skill != self.wire.skill_wanted {
                        self.wire.skill_wanted = cp.skill;
                        crate::plugin_wire::save_skill_wishes(&self.opts.state_dir, &self.wire.skill_wanted);
                    }
                } else if !self.wire.usage_wanted.is_empty() {
                    // A config with no `plugins` (an older web, which has no AI usage switch): off.
                    self.apply_usage_wishes(Default::default());
                }
                let skill_changed = self.sync_skills();
                // After the skills: Codex's instructions carry its skill's text.
                let set_changed = self.sync_plugin_set();
                self.apply_config(claude, shell, work_dir, projects);
                if (skill_changed || set_changed) && self.shim_dir.is_some() {
                    if let Err(e) = self.plugins.write_shims(&self.opts.state_dir, &self.launch.claude_args) {
                        log!("config: skill shim: {e}");
                    }
                    // A rewritten shim loses its agent-env line (crate::secrets).
                    self.secrets_patch_launch();
                }
            }
            GatewayFrame::ChatAttach { req, session_id, from } => self.chat_attach(req, &session_id, from),
            GatewayFrame::ChatDetach { session_id } => self.chat_detach(&session_id),
            GatewayFrame::EventsRejected { seqs, last_event_seq } => {
                // Before any later events_ack (same socket, in order): new
                // numbers above everything web has, then resend.
                let mut seq = self.state.event_seq.max(last_event_seq);
                let n = self.outbox.renumber_rejected(&seqs, || {
                    seq += 1;
                    seq
                });
                self.state.event_seq = seq;
                self.persist();
                log!("web rejected event seqs {seqs:?}; {n} renumbered above {last_event_seq} and resent");
                self.flush_events().await;
            }
            GatewayFrame::Attach { req, pane_id, from } => {
                // The local attach log (docs/design/mac-isolation.md §3).
                log!(
                    "remote attach: pane {pane_id}{}",
                    if self.opts.guardrails.observe_only {
                        " (observe-only)"
                    } else {
                        ""
                    }
                );
                let mut out = vec![];
                match self.panes.get_mut(&pane_id) {
                    Some(rt) => rt.attach(req, from, &mut out),
                    None => {
                        log!("attach for unknown pane {pane_id}");
                        return;
                    }
                }
                self.send_out(out).await;
            }
            GatewayFrame::Detach { pane_id } => {
                if let Some(rt) = self.panes.get_mut(&pane_id) {
                    rt.detach();
                }
            }
            GatewayFrame::Resize { pane_id, cols, rows } => {
                if self.opts.guardrails.allows_control() {
                    // Held (the newest per pane) while the Mac is asked; a resize alone never asks.
                    self.control(ControlWork::Resize { pane_id, cols, rows }, 0, false, "resizing")
                        .await;
                }
            }
            GatewayFrame::Req { id, method, params } => {
                if !self.opts.guardrails.allows_method(&method) {
                    log!("refused {method} from tmuxedo: this machine is observe-only");
                    crate::api::respond(
                        ReplyTo::Gateway(id),
                        Err(crate::api::err(
                            "observe_only",
                            "this machine is observe-only: changes are refused on the machine",
                        )),
                        self,
                    );
                    return;
                }
                if !crate::guardrails::READ_ONLY_METHODS.contains(&method.as_str()) {
                    let pane = params
                        .get("pane_id")
                        .or_else(|| params.get("target_pane_id"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    if let Some(e) = self.control_now(&pane, &method) {
                        crate::api::respond(ReplyTo::Gateway(id), Err(e), self);
                        return;
                    }
                }
                if method == "ssh.generate" {
                    let r = self.ssh_generate(&params);
                    crate::api::respond(ReplyTo::Gateway(id), r, self);
                    return;
                }
                self.request(&method, params, ReplyTo::Gateway(id)).await;
            }
            GatewayFrame::Decision {
                pane_id,
                prompt_id,
                behavior,
                message,
            } => {
                if !self.opts.guardrails.allows_control() {
                    log!("refused a decision for pane {pane_id}: this machine is observe-only");
                    return;
                }
                let w = ControlWork::Decision {
                    pane_id,
                    prompt_id,
                    behavior,
                    message,
                };
                self.control(w, 0, true, "answering a prompt").await
            }
            GatewayFrame::Glance { pane_ids, hz } => self.set_glances(pane_ids, hz),
            GatewayFrame::Stats { hz } => self.stats.set_rate(Some(hz), Instant::now()),
            GatewayFrame::LoginCallback { id, pane_id, url } => {
                if !self.opts.guardrails.allows_control() {
                    crate::api::respond(
                        ReplyTo::Gateway(id),
                        Err(crate::api::err("observe_only", "this machine is observe-only")),
                        self,
                    );
                    return;
                }
                if let Some(e) = self.control_now(&pane_id, "login_callback") {
                    crate::api::respond(ReplyTo::Gateway(id), Err(e), self);
                    return;
                }
                self.login_callback(id, pane_id, url)
            }
            GatewayFrame::Ping { .. } | GatewayFrame::Pong { .. } => {}
        }
    }

    // ── confirm on this Mac (crate::confirm) ─────────────────────────────

    /// Remote control work that can wait for the Mac's answer: done now, held, or dropped.
    async fn control(&mut self, w: ControlWork, size: usize, asks: bool, what: &'static str) {
        let pane = match &w {
            ControlWork::Input { pane_id, .. }
            | ControlWork::Resize { pane_id, .. }
            | ControlWork::Decision { pane_id, .. } => pane_id.clone(),
        };
        let is_resize = matches!(w, ControlWork::Resize { .. });
        match self.confirm.admit(Instant::now(), w, size, asks) {
            crate::confirm::Admit::Pass(w) => self.do_control(w).await,
            crate::confirm::Admit::Held => {
                if is_resize {
                    let p = pane.clone();
                    self.confirm
                        .replace_held(|x| matches!(x, ControlWork::Resize { pane_id, .. } if *pane_id == p));
                }
            }
            crate::confirm::Admit::Ask => self.ask_confirm(&pane, what),
            crate::confirm::Admit::Dropped(Some(why)) => log!("remote control: {why} (pane {pane})"),
            crate::confirm::Admit::Dropped(None) => {}
        }
    }

    /// A control call that needs an answer now (a mutating method, a login callback):
    /// None = go ahead; Some(error) = refused (and the Mac asked, the first time).
    fn control_now(&mut self, pane: &str, what: &str) -> Option<crate::api::ApiErr> {
        match self.confirm.check(Instant::now()) {
            crate::confirm::Check::Pass => None,
            crate::confirm::Check::Ask => {
                self.ask_confirm(pane, "changing this machine");
                log!("remote control: {what} refused until this Mac allows it");
                Some(crate::api::err(
                    "confirm_pending",
                    "this Mac was asked to allow remote control; try again once it is allowed",
                ))
            }
            crate::confirm::Check::Pending => Some(crate::api::err(
                "confirm_pending",
                "waiting for this Mac to allow remote control",
            )),
            crate::confirm::Check::Denied => Some(crate::api::err("confirm_denied", "this Mac denied remote control")),
        }
    }

    async fn do_control(&mut self, w: ControlWork) {
        match w {
            ControlWork::Input { pane_id, bytes } => {
                self.release_on_input(&pane_id);
                if let Some(rt) = self.panes.get(&pane_id) {
                    if rt.alive {
                        self.holder.input(&rt.hid, &bytes);
                    }
                }
            }
            ControlWork::Resize { pane_id, cols, rows } => {
                self.resize(&pane_id, cols, rows).await;
            }
            ControlWork::Decision {
                pane_id,
                prompt_id,
                behavior,
                message,
            } => {
                log!(
                    "remote decision for pane {pane_id}: {}",
                    match behavior {
                        Behavior::Allow => "allow",
                        Behavior::Deny => "deny",
                    }
                );
                self.on_decision(pane_id, prompt_id, behavior, message).await
            }
        }
    }

    /// Ask the person at the Mac, off the event loop; the answer comes back as `Internal::Confirmed`.
    fn ask_confirm(&mut self, pane: &str, what: &str) {
        let question = self.confirm.question();
        let Some(signer) = self.opts.signer.clone() else {
            // Fail closed: confirm-on-Mac is on but there is nobody to ask.
            let _ = self.internal_tx.send(Internal::Confirmed {
                question,
                result: crate::signer::Confirmed::Failed("no app to ask (no signer)".into()),
            });
            return;
        };
        log!("remote control: asking on this Mac (pane {pane}, {what})");
        let (pane, what) = (pane.to_string(), what.to_string());
        let (timeout, poll) = (self.confirm.policy.timeout, self.opts.confirm_poll);
        let tx = self.internal_tx.clone();
        tokio::task::spawn_blocking(move || {
            let result = crate::signer::confirm(&signer, &pane, &what, timeout, poll);
            let _ = tx.send(Internal::Confirmed { question, result });
        });
    }

    async fn on_confirmed(&mut self, question: u64, r: crate::signer::Confirmed) {
        use crate::signer::Confirmed;
        if !self.confirm.is_pending() || question != self.confirm.question() {
            return; // a late answer to a question that already expired
        }
        let allowed = r == Confirmed::Allowed;
        match &r {
            Confirmed::Allowed => log!("remote control: allowed on this Mac"),
            Confirmed::Denied => log!("remote control: denied on this Mac; held input dropped"),
            Confirmed::TimedOut => log!("remote control: not answered on this Mac in time; denied"),
            Confirmed::Failed(e) => log!("remote control: this Mac could not be asked ({e}); denied"),
        }
        for w in self.confirm.resolve(Instant::now(), allowed) {
            self.do_control(w).await;
        }
    }

    /// The gateway's glance set replaces ours; a newly added pane gets a
    /// first glance on the next tick, kept panes keep their state.
    fn set_glances(&mut self, pane_ids: Vec<String>, hz: f64) {
        self.glance_hz = tmuxedo_protocol::frames::clamp_hz(hz);
        let now = Instant::now();
        let mut next = BTreeMap::new();
        for id in pane_ids {
            let st = self.glances.remove(&id).unwrap_or(GlanceState {
                next_check: now,
                seen: None,
                hash: None,
            });
            next.insert(id, st);
        }
        self.glances = next;
    }

    /// Send a glance for each glanced pane whose screen changed, at most
    /// `glance_hz` checks a second per pane, never mid synchronized frame.
    async fn glance_tick(&mut self, now: Instant) {
        let panes = &self.panes;
        self.glances.retain(|id, _| panes.contains_key(id));
        let period = Duration::from_secs_f64(1.0 / self.glance_hz);
        let mut frames = vec![];
        for (id, st) in self.glances.iter_mut() {
            if now < st.next_check {
                continue;
            }
            let Some(rt) = self.panes.get_mut(id) else {
                continue;
            };
            if rt.emu.in_sync() {
                continue; // look again next tick, once the frame is complete
            }
            st.next_check = now + period;
            let (rows, cols) = rt.emu.size();
            let seen = (rt.end(), rows, cols);
            if st.seen == Some(seen) {
                continue;
            }
            st.seen = Some(seen);
            let g = crate::glance::encode(&mut rt.emu);
            if st.hash == Some(g.hash) {
                continue;
            }
            st.hash = Some(g.hash);
            let rev = self.glance_rev.entry(id.clone()).or_insert(0);
            *rev += 1;
            frames.push(RunnerFrame::Glance {
                pane_id: id.clone(),
                rev: *rev,
                cols: g.cols,
                rows: g.rows,
                cursor: g.cursor,
                lines: g.lines,
            });
        }
        for f in frames {
            self.send_frame(f).await;
        }
    }

    pub(crate) async fn on_decision(
        &mut self,
        pane_id: String,
        prompt_id: String,
        behavior: Behavior,
        message: Option<String>,
    ) {
        // An ssh key's confirm-each-use (crate::secrets::ssh).
        if self.ssh_decision(&pane_id, &prompt_id, matches!(behavior, Behavior::Allow)) {
            return;
        }
        // A dialog answered by keystroke (crate::plugin_rt), not a held hook.
        if !self.prompts.contains_key(&prompt_id)
            && self
                .key_decision(&pane_id, &prompt_id, matches!(behavior, Behavior::Allow))
                .await
        {
            return;
        }
        match self.prompts.remove(&prompt_id) {
            Some(p) if p.pane_id == pane_id => {
                let b = match behavior {
                    Behavior::Allow => "allow",
                    Behavior::Deny => "deny",
                };
                let mut d = json!({ "behavior": b });
                if let Some(m) = message {
                    d["message"] = json!(m);
                }
                crate::api::respond(p.reply, Ok(json!({ "decision": d })), self);
                self.needs.hint(
                    &prompt_id,
                    "remote",
                    Some(if b == "allow" { "approve" } else { "deny" }.into()),
                );
                if let Some(rt) = self.panes.get_mut(&pane_id) {
                    rt.track.prompt = None;
                    rt.track.report(&p.kind, AgentStatus::Working, "hook");
                }
                self.agent_update(&pane_id, true).await;
            }
            other => {
                if let Some(p) = other {
                    self.prompts.insert(prompt_id.clone(), p);
                }
                self.send_event(&pane_id, "decision_stale", json!({ "promptId": prompt_id }))
                    .await;
            }
        }
    }

    pub(crate) async fn resize(&mut self, pane_id: &str, cols: u16, rows: u16) -> bool {
        let (cols, rows) = (cols.clamp(1, 1000), rows.clamp(1, 1000));
        let Some(rt) = self.panes.get_mut(pane_id) else {
            return false;
        };
        rt.cols = cols;
        rt.rows = rows;
        rt.emu.resize(rows, cols);
        self.holder.resize(&rt.hid, cols, rows);
        if let Some(p) = self.state.pane_mut(pane_id) {
            p.pane.cols = cols;
            p.pane.rows = rows;
        }
        self.dirty = true;
        self.send_frame(RunnerFrame::Size {
            pane_id: pane_id.into(),
            cols,
            rows,
        })
        .await;
        true
    }

    // ── agent status ──────────────────────────────────────────────────────

    /// Recompute a pane's agent status and propagate a change (or `force`).
    /// A change of the published view alone (source, kind, session id — e.g.
    /// SessionStart's reported idle over a heuristic idle) is sent too.
    pub(crate) async fn agent_update(&mut self, pane_id: &str, force: bool) {
        let Some(rt) = self.panes.get_mut(pane_id) else { return };
        // Helpers past their retention leave the list (crate::subagents).
        rt.track.children.prune(Instant::now());
        let change = rt.track.recompute();
        let session = self.state.pane(pane_id).and_then(|p| p.meta.session_id.clone());
        let mut agent = rt.track.agent(session.as_deref());
        let prompt_id = rt.track.prompt.as_ref().map(|p| p.0.clone());
        let kind = rt.track.kind().map(str::to_owned);
        let helpers = rt.track.children.list();
        // Agent chat: can this session open as a chat, and which chat holds it (chat::chat_offer).
        if let Some(a) = agent.as_mut() {
            a.chat = self.chat_offer(&a.kind, a.session_id.as_deref());
        }
        let rec = self.state.pane(pane_id);
        let view_changed = rec.is_some_and(|p| p.pane.agent != agent || p.meta.helpers != helpers);
        if change.is_none() && !force && !view_changed {
            // The status stands; a question's item may still be due (crate::needs).
            return self.needs_sync(pane_id).await;
        }
        if let Some(p) = self.state.pane_mut(pane_id) {
            p.pane.agent = agent.clone();
            // Saved with the pane: a runner restart keeps the list.
            if p.meta.helpers != helpers {
                p.meta.helpers = helpers;
                self.dirty = true;
            }
        }
        self.send_frame(RunnerFrame::Status {
            pane_id: pane_id.into(),
            agent,
        })
        .await;
        if let Some((_, new)) = change {
            let ws = self.state.workspace_of(pane_id).map(|w| w.0).unwrap_or_default();
            let data = json!({
                "pane_id": pane_id, "workspace_id": ws, "agent_status": agent::status_str(new),
                "agent": kind,
            });
            let sub_line = json!({ "event": "pane.agent_status_changed", "data": data }).to_string();
            for s in &self.subscribers {
                if s.wants_agent(pane_id, agent::status_str(new)) {
                    let _ = s.tx.send(sub_line.clone());
                }
            }
            self.emit(Event::new("pane_agent_status_changed", data));
            if kind.is_some() {
                if let Some(t) = agent::durable_event(new) {
                    let mut d = json!({ "kind": kind });
                    if let Some(p) = prompt_id {
                        d["promptId"] = json!(p);
                    }
                    self.send_event(pane_id, t, d).await;
                }
            }
            self.check_agent_waiters(pane_id);
        }
        self.needs_sync(pane_id).await;
    }

    /// Tier 0 + tier 2 for every pane.
    async fn detect(&mut self) {
        let now = Instant::now();
        let mut auth_hits: Vec<(String, &'static str)> = vec![];
        let claude_on = self.plugins.enabled(crate::plugin::DEFAULT_PLUGIN);
        let ids: Vec<String> = self.panes.keys().cloned().collect();
        for id in ids {
            let mut detected_change = None;
            {
                let Some(rt) = self.panes.get_mut(&id) else { continue };
                if !rt.alive || rt.pid == 0 {
                    continue;
                }
                // Claude login messages, on any pane (a new match only).
                if self.auth.is_some() && claude_on && rt.auth_checked_at != rt.end() {
                    rt.auth_checked_at = rt.end();
                    let rule = crate::claude_auth::screen_rule(&rt.emu.detection_text());
                    let fresh = rule.is_some() && !rt.auth_match;
                    rt.auth_match = rule.is_some();
                    if let (true, Some(rule)) = (fresh, rule) {
                        auth_hits.push((id.clone(), rule));
                    }
                }
                let job = tmuxedo_detect::platform::foreground_job(rt.pid);
                let fg = job
                    .as_ref()
                    .and_then(tmuxedo_detect::identify)
                    .map(|a| a.id().to_string());
                rt.track.fg_pgid = job.as_ref().map(|j| j.pgid);
                if fg != rt.track.process_kind {
                    let left = rt.track.set_process_kind(fg.clone());
                    detected_change = Some((fg.clone(), left));
                }
                let sig = rt.emu.signals().clone();
                rt.track.heuristic_tick(now, sig.bells + sig.notifications);
                if let Some(agent) = rt.track.kind().and_then(tmuxedo_detect::Agent::lookup) {
                    // New output since the last reading, and no synchronized frame half drawn
                    // (docs/design/agent-detection.md §11.1): read the screen.
                    if rt.track.detected_at != rt.end() && !rt.emu.in_sync() {
                        rt.track.detected_at = rt.end();
                        let text = rt.emu.detection_text();
                        let input = tmuxedo_detect::Input {
                            screen: &text,
                            title: &sig.title,
                            progress: &sig.progress,
                        };
                        rt.track.screen = match tmuxedo_detect::read(agent, &input) {
                            tmuxedo_detect::Status::Idle => Some(AgentStatus::Idle),
                            tmuxedo_detect::Status::Working => Some(AgentStatus::Working),
                            tmuxedo_detect::Status::Blocked => Some(AgentStatus::Blocked),
                            tmuxedo_detect::Status::Unknown => None,
                            tmuxedo_detect::Status::Hold => rt.track.screen,
                        };
                    }
                } else {
                    rt.track.screen = None;
                }
            }
            if let Some((agent, left)) = detected_change {
                let ws = self.state.workspace_of(&id).map(|w| w.0).unwrap_or_default();
                self.emit(Event::new(
                    "pane_agent_detected",
                    json!({ "pane_id": id, "workspace_id": ws, "agent": agent, "released": left }),
                ));
                if let (Some(kind), Some(p)) = (agent, self.state.pane_mut(&id)) {
                    p.meta.kind = Some(kind);
                }
            }
            self.agent_update(&id, false).await;
        }
        for (pane, rule) in auth_hits {
            let state = crate::claude_auth::screen_state(rule);
            let c = self
                .auth
                .as_mut()
                .and_then(|a| a.text_hit(&pane, rule, state, "screen"));
            self.auth_changed(c).await;
        }
        self.plugin_detect().await;
    }

    // ── timers ────────────────────────────────────────────────────────────

    /// One coalescing round: panes in rotating order, each sends at most one
    /// 32 KiB chunk per pass; passes repeat while any pane has a full chunk;
    /// then anything older than 10 ms goes out.
    async fn flush_round(&mut self, include_aged: bool) {
        let ids: Vec<String> = self.panes.keys().cloned().collect();
        if ids.is_empty() {
            return;
        }
        let start = self.rr % ids.len();
        self.rr = self.rr.wrapping_add(1);
        loop {
            let mut out = vec![];
            let mut more = false;
            for i in 0..ids.len() {
                let id = &ids[(start + i) % ids.len()];
                if let Some(rt) = self.panes.get_mut(id) {
                    if rt.pending.len() >= COALESCE_BYTES {
                        rt.take_chunk(COALESCE_BYTES, &mut out);
                        more |= rt.pending.len() >= COALESCE_BYTES;
                    }
                }
            }
            self.send_out(out).await;
            if !more {
                break;
            }
        }
        if include_aged {
            let now = Instant::now();
            let mut out = vec![];
            for i in 0..ids.len() {
                let id = &ids[(start + i) % ids.len()];
                if let Some(rt) = self.panes.get_mut(id) {
                    if rt.pending_since.is_some_and(|t| now.duration_since(t) >= COALESCE) {
                        rt.flush(&mut out);
                    }
                }
            }
            self.send_out(out).await;
        }
    }

    async fn on_tick(&mut self) {
        self.flush_overflow();
        self.flush_round(true).await;
        let now = Instant::now();
        let mut out = vec![];
        for rt in self.panes.values_mut() {
            for req in rt.gate.expired(now) {
                rt.snapshot(req, &mut out);
            }
            rt.release_held(now, &self.holder, &mut out);
        }
        self.send_out(out).await;
        let due: Vec<Timer> = {
            let (due, keep): (Vec<_>, Vec<_>) = std::mem::take(&mut self.timers)
                .into_iter()
                .partition(|(t, _)| *t <= now);
            self.timers = keep;
            due.into_iter().map(|(_, t)| t).collect()
        };
        for t in due {
            match t {
                Timer::Nudge(id) => {
                    if let Some(rt) = self.panes.get(&id) {
                        self.holder.resize(&rt.hid, rt.cols, rt.rows);
                    }
                }
                Timer::KillHard(id, hid) => {
                    if self.panes.get(&id).is_some_and(|p| p.hid == hid) {
                        self.holder.kill_nowait(&hid, libc::SIGKILL);
                    }
                }
                Timer::KillGroupHard(pgid) => {
                    // SAFETY: plain syscall; ESRCH (already gone) is the usual outcome.
                    unsafe { libc::kill(-(pgid as libc::pid_t), libc::SIGKILL) };
                }
            }
        }
        let expired: Vec<String> = self
            .prompts
            .iter()
            .filter(|(_, p)| p.deadline <= now)
            .map(|(k, _)| k.clone())
            .collect();
        for k in expired {
            if let Some(p) = self.prompts.remove(&k) {
                // The hook prints nothing on `expired`: Claude's own dialog
                // stays; a later decision for this id is `decision_stale`.
                crate::api::respond(p.reply, Ok(json!({ "expired": true })), self);
                self.needs.hint(&k, "expired", None);
                if let Some(rt) = self.panes.get_mut(&p.pane_id) {
                    if rt.track.prompt.as_ref().is_some_and(|(id, _)| *id == k) {
                        rt.track.prompt = None;
                    }
                }
                self.agent_update(&p.pane_id, true).await;
                self.send_event(&p.pane_id, "prompt_expired", json!({ "promptId": k }))
                    .await;
            }
        }
        if !self.glances.is_empty() {
            self.glance_tick(now).await;
        }
        // Nobody looking: `active` is false and nothing is read.
        if self.stats.active() {
            if let Some(s) = self.stats.tick(now, crate::util::now_ms()) {
                self.send_frame(RunnerFrame::Stats(s)).await;
            }
        }
        if self.welcomed && self.usage.has_dirty() {
            for r in self.usage.due(now) {
                self.send_frame(RunnerFrame::Usage(r)).await;
            }
        }
        self.needs_tick(now).await;
        if now.duration_since(self.last_detect) >= DETECT_EVERY {
            self.last_detect = now;
            self.detect().await;
            self.needs_gc().await;
            self.check_output_waiters();
        }
        self.expire_waiters(now);
    }

    /// Report a Claude login-health transition: `hello` carries the new
    /// state from now on, and a durable `auth_*` event goes out.
    pub(crate) async fn auth_changed(&mut self, c: Option<crate::claude_auth::Change>) {
        let (Some(c), Some(a)) = (c, self.auth.as_ref()) else {
            return;
        };
        self.info.claude_auth = Some(a.json());
        log!("claude auth: {} ({}, via {})", c.state.as_str(), c.reason, c.source);
        let data = json!({ "state": c.state, "reason": c.reason, "source": c.source });
        self.send_event(c.pane.as_deref().unwrap_or(""), c.state.event(), data)
            .await;
        self.needs_sync(crate::needs::MACHINE).await;
    }

    async fn on_slow(&mut self) {
        self.chat_tick().await;
        self.task_tick().await;
        self.secrets_tick();
        self.ssh_tick();
        self.files_tick();
        self.file_asks_tick().await;
        if self.confirm.expire(Instant::now()) {
            log!("remote control: this Mac's answer never came; denied");
        }
        let c = if self.plugins.enabled(crate::plugin::DEFAULT_PLUGIN) {
            self.auth.as_mut().and_then(|a| a.check_credentials())
        } else {
            None
        };
        self.auth_changed(c).await;
        self.plugin_slow_tick().await;
        self.reprobe_tick().await;
        let moved = self
            .state
            .panes
            .iter()
            .any(|p| self.panes.get(&p.pane.id).is_some_and(|rt| rt.end() > p.high_water));
        if moved || self.dirty {
            self.persist();
        }
    }

    async fn watchdog(&mut self) {
        let now = Instant::now();
        self.watchdog_stopping.retain(|(_, until)| *until > now);
        let panes: Vec<(String, u32)> = self
            .panes
            .values()
            .filter(|p| p.alive && p.pid > 0)
            .map(|p| (p.id.clone(), p.pid))
            .collect();
        let Some(v) = watchdog::check(
            std::path::Path::new("/sys/fs/cgroup"),
            std::path::Path::new("/proc"),
            self.holder.pid,
            &panes,
        ) else {
            return;
        };
        if self.watchdog_stopping.iter().any(|(g, _)| *g == v.pgid) {
            return; // already being stopped; still the biggest
        }
        let Some((hid, shell_pid)) = self.panes.get(&v.pane).map(|p| (p.hid.clone(), p.pid)) else {
            return;
        };
        log!(
            "memory {}/{} above {}%: stopping process group {} in {} (group rss {}, pane rss {})",
            v.current,
            v.max,
            (watchdog::THRESHOLD * 100.0) as u32,
            v.pgid,
            v.pane,
            v.group_rss,
            v.pane_rss
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        if v.pgid == shell_pid {
            // The shell's own group is the biggest: stop the pane as before.
            self.holder.kill_nowait(&hid, libc::SIGTERM);
            self.timers.push((deadline, Timer::KillHard(v.pane.clone(), hid)));
        } else {
            // A job inside the pane: stop only it; the shell and the pane live on.
            // SAFETY: plain syscall with integer arguments.
            unsafe { libc::kill(-(v.pgid as libc::pid_t), libc::SIGTERM) };
            self.timers.push((deadline, Timer::KillGroupHard(v.pgid)));
        }
        self.watchdog_stopping.push((v.pgid, deadline + Duration::from_secs(1)));
        self.send_event(
            &v.pane,
            "memory_stopped",
            json!({
                "rss": v.group_rss, "paneRss": v.pane_rss, "pgid": v.pgid,
                "current": v.current, "max": v.max, "reason": "memory limit",
            }),
        )
        .await;
    }

    fn check_update(&mut self) {
        let Some(base) = self.opts.http_base.clone() else {
            return;
        };
        let tx = self.internal_tx.clone();
        tokio::task::spawn_blocking(move || match crate::update::update_runner(&base) {
            Ok(crate::update::Outcome::Updated(v)) => {
                log!("updated runner to {v}; restarting the runner (the holder keeps running)");
                let _ = tx.send(Internal::UpdateReady);
            }
            Ok(crate::update::Outcome::Current) => {}
            Err(e) => log!("update check: {e}"),
        });
    }

    // ── local socket (runner API + hook shim) ──────────────────────────────

    async fn on_local(&mut self, req: LocalReq) {
        if req.method == "project.env" {
            let r = self.project_env(&req.params);
            crate::api::respond(ReplyTo::Local(req.reply), r, self);
            return;
        }
        // Secret values are served on the local socket only (never over the gateway).
        if req.method.starts_with("secret.") {
            self.secret_local(&req.method, &req.params, req.peer, ReplyTo::Local(req.reply));
            return;
        }
        if req.method == "hook.event" {
            if let Some(why) = self.hook_caller_refused(&req) {
                // Logged, never acted on: no status, no prompt, no item (Phase 13).
                let pane = req.params["paneId"].as_str().unwrap_or("").to_string();
                let chat = req.params["chatId"].as_str().unwrap_or("").to_string();
                let event = req.params["event"]["hook_event_name"]
                    .as_str()
                    .unwrap_or("?")
                    .chars()
                    .take(40)
                    .collect::<String>();
                log!(
                    "hook ignored: {event} for {} from pid {} ({why}): not the agent's own hook",
                    if chat.is_empty() {
                        format!("pane {pane}")
                    } else {
                        format!("chat {chat}")
                    },
                    req.peer_pid().map_or("?".into(), |p| p.to_string())
                );
                crate::api::respond(ReplyTo::Local(req.reply), Ok(json!({})), self);
                return;
            }
            if req.params["chatId"].as_str().is_some_and(|c| !c.is_empty()) {
                self.chat_hook(req).await;
            } else {
                self.on_hook(req).await;
            }
            return;
        }
        if req.method == "usage.report" {
            // From the status-line command; the same caller check as a hook, and never an answer worth waiting for.
            let dropped = match self.hook_caller_refused(&req) {
                Some(why) => Some(why),
                None => self.usage_report(&req.params).err(),
            };
            if let Some(why) = dropped {
                // The reason and the pane only, never the reading.
                let pane = req.params["paneId"]
                    .as_str()
                    .unwrap_or("")
                    .chars()
                    .take(40)
                    .collect::<String>();
                log!("usage report dropped for pane {pane}: {why}");
            }
            crate::api::respond(ReplyTo::Local(req.reply), Ok(json!({})), self);
            return;
        }
        if req.method == "pane.open_url" {
            let r = self.open_url(&req.params, req.peer_pid());
            crate::api::respond(ReplyTo::Local(req.reply), r, self);
            return;
        }
        if crate::plugin_api::METHODS.contains(&req.method.as_str()) {
            let r = self.plugin_request(&req.method, &req.params).await;
            crate::api::respond(ReplyTo::Local(req.reply), r, self);
            return;
        }
        if req.method == "runner.skill" {
            let r = self.skill_view(&req.params);
            crate::api::respond(ReplyTo::Local(req.reply), r, self);
            return;
        }
        if req.method == "runner.config" {
            let v = self.config_view();
            crate::api::respond(ReplyTo::Local(req.reply), Ok(v), self);
            return;
        }
        self.request(&req.method, req.params, ReplyTo::Local(req.reply)).await;
    }

    /// AI usage went on or off for some plugin (web's `config.plugins`): keep
    /// the wish, rewrite the generated settings (with or without the status
    /// line) and forget what was read for a plugin that is now off. An agent
    /// already running keeps its settings; its reports are dropped below.
    fn apply_usage_wishes(&mut self, wanted: std::collections::BTreeMap<String, bool>) {
        for id in self.plugins.ids().into_iter().map(str::to_string).collect::<Vec<_>>() {
            let on = wanted.get(&id).copied().unwrap_or(false);
            self.plugins.set_usage(&id, on);
            if !on {
                self.usage.forget(&id);
            }
        }
        self.wire.usage_wanted = wanted;
        crate::plugin_wire::save_usage_wishes(&self.opts.state_dir, &self.wire.usage_wanted);
        log!("ai usage: {:?}", self.wire.usage_wanted);
        if self.shim_dir.is_some() {
            if let Err(e) = self.plugins.write_settings(&self.opts.state_dir) {
                log!("config: usage settings: {e}");
            }
        }
    }

    /// A reading from an agent's status line (`hook --statusline`): only for a
    /// live pane of an enabled plugin that declares that source, while AI
    /// usage is on for it.
    fn usage_report(&mut self, p: &Value) -> Result<(), &'static str> {
        let pane = p["paneId"].as_str().unwrap_or("").to_string();
        let plugin = self.plugins.for_hook(p["plugin"].as_str()).ok_or("unknown plugin")?;
        if !self.plugins.usage_enabled(plugin.id()) {
            return Err("usage off");
        }
        if self.panes.get(&pane).is_none_or(|rt| rt.closing) {
            return Err("no such pane");
        }
        if self.hook_for_other_agent(&pane, plugin.as_ref()) {
            return Err("another agent's pane");
        }
        let section = plugin.manifest().usage.clone().ok_or("no usage section")?;
        let r = serde_json::from_value::<crate::usage::Reading>(p["reading"].clone()).map_err(|_| "bad reading")?;
        if r.source != "statusline" {
            return Err("not a status line reading");
        }
        if !self
            .usage
            .note(&section, plugin.id(), Some(&pane), r, crate::util::now_ms())
        {
            return Err("reading refused or unchanged");
        }
        Ok(())
    }

    /// The agent's session log (a `jsonl_tail` usage source), named by its own
    /// hook: the end of it is read, at most once per interval per plugin.
    fn usage_tail(&mut self, pane: &str, plugin: &dyn crate::plugin::AgentPlugin, ev: &Value) {
        if !self.plugins.usage_enabled(plugin.id()) {
            return;
        }
        let Some(section) = plugin.manifest().usage.as_ref() else {
            return;
        };
        let Some(src) = section.source("jsonl_tail") else {
            return;
        };
        let (Some(field), Some(root)) = (&src.path_field, &src.root) else {
            return;
        };
        let Some(path) = crate::usage::at(ev, field)
            .and_then(Value::as_str)
            .filter(|p| p.starts_with('/'))
        else {
            return;
        };
        if !self.usage.tail_due(plugin.id(), Instant::now()) {
            return;
        }
        let root = root.replace("{home}", &self.opts.home);
        let now_ms = crate::util::now_ms();
        let read = crate::usage::read_tail(
            src,
            std::path::Path::new(path),
            std::path::Path::new(&root),
            now_ms / 1000,
        );
        if let Some(r) = read {
            self.usage.note(section, plugin.id(), Some(pane), r, now_ms);
        }
    }

    /// Why a hook caller is not the agent (crate::ancestry), or None to go on.
    fn hook_caller_refused(&self, req: &LocalReq) -> Option<&'static str> {
        if !self.opts.verify_hook_callers {
            return None;
        }
        let procs = crate::ancestry::System;
        if let Some(chat) = req.params["chatId"].as_str().filter(|c| !c.is_empty()) {
            let pid = self.chats.sessions.get(chat).map_or(0, |s| s.pid);
            if pid == 0 {
                // A chat the runner does not know: chat_hook answers `{}` anyway.
                return None;
            }
            return crate::ancestry::below(&procs, req.peer_pid(), pid)
                .err()
                .map(|r| r.code());
        }
        let pane = req.params["paneId"].as_str().unwrap_or("");
        let rt = self.panes.get(pane)?;
        let plugin = self.plugins.for_hook(req.params["plugin"].as_str())?;
        let labels = plugin.manifest().agent.labels.clone();
        crate::ancestry::check(&procs, req.peer_pid(), rt.pid, &labels)
            .err()
            .map(|r| r.code())
    }

    /// Agent hook events (`tmuxedo-runner hook [--plugin <id>]`) → the
    /// plugin's normalised events → reported status, held permission
    /// prompts, session capture, auth signals (docs/design/plugins.md §5.3).
    async fn on_hook(&mut self, req: LocalReq) {
        use crate::plugin::AgentEvent;
        let pane_id = req.params["paneId"].as_str().unwrap_or("").to_string();
        let ev = req.params["event"].clone();
        let reply = ReplyTo::Local(req.reply);
        // `hooks.receive`: only an enabled plugin's hooks count.
        let plugin = self.plugins.for_hook(req.params["plugin"].as_str());
        // A pane that is closing (its agent is being killed) is no longer anyone's: a hook that
        // was already on its way must not bind a chat to it or move any state.
        let Some(plugin) = plugin.filter(|_| self.panes.get(&pane_id).is_some_and(|rt| !rt.closing)) else {
            crate::api::respond(reply, Ok(json!({})), self);
            return;
        };
        // A hook of another plugin's agent (Cursor also runs a project's
        // Claude hooks): not this pane's agent, so it says nothing.
        if self.hook_for_other_agent(&pane_id, plugin.as_ref()) {
            crate::api::respond(reply, Ok(json!({})), self);
            return;
        }
        // The CLI is running in a pane: if its version is still unknown, ask once now.
        self.reprobe.seen_in_pane(plugin.id(), Instant::now());
        let kind = plugin.agent_label().to_string();
        let is_claude = plugin.id() == crate::plugin::DEFAULT_PLUGIN;
        let plugin_id = plugin.id().to_string();
        // The secret guard (secrets-and-env.md §4.3) answers before the command runs.
        if let Some(cmd) = crate::secrets::guard::command_of(&ev) {
            if let Some(deny) = self.secret_guard(&plugin_id, &pane_id, cmd) {
                crate::api::respond(reply, Ok(json!({ "output": deny })), self);
                return;
            }
        }
        // Tool output with a secret in it is masked before the model sees it (PostToolUse).
        let rewrite = self.post_tool_rewrite(&ev);
        let out = plugin.map_hook(&ev);
        self.usage_tail(&pane_id, plugin.as_ref(), &ev);
        self.needs_note_hook(&pane_id, plugin.as_ref(), &ev, &out);
        if out.events.contains(&AgentEvent::TurnCompleted) && self.scan_transcript(&pane_id, &plugin_id, &kind, &ev) {
            self.flush_events().await;
        }
        if let Some(sid) = &out.session_id {
            if let Some(p) = self.state.pane_mut(&pane_id) {
                if p.meta.session_id.as_deref() != Some(sid) {
                    p.meta.session_id = Some(sid.clone());
                    p.meta.kind = Some(kind.clone());
                    if let Some(c) = out.cwd.clone() {
                        p.meta.cwd_was_repo = inject::is_repo(&c);
                        p.pane.cwd = c;
                    }
                    self.dirty = true;
                }
            }
        }
        // The agent moved on: a held prompt was answered at the terminal.
        if out.releases_holds() {
            let held: Vec<String> = self
                .prompts
                .iter()
                .filter(|(_, p)| p.pane_id == pane_id)
                .map(|(k, _)| k.clone())
                .collect();
            for k in held {
                if let Some(p) = self.prompts.remove(&k) {
                    crate::api::respond(p.reply, Ok(json!({})), self);
                }
            }
        }
        if out.events.contains(&AgentEvent::SessionEnded) {
            if let Some(rt) = self.panes.get_mut(&pane_id) {
                rt.track.reported = None;
                rt.track.children.clear();
            }
            let ended = self.state.pane_mut(&pane_id).and_then(|p| p.meta.session_id.take());
            self.agent_update(&pane_id, true).await;
            crate::api::respond(reply, Ok(json!({})), self);
            // Agent chat: a chat that continued in this pane can drive its session again.
            self.chat_pane_ended(&pane_id, out.session_id.as_deref().or(ended.as_deref()))
                .await;
            return;
        }
        // Helpers the agent started or finished in its own process.
        self.on_subagents(&pane_id, &kind, &out.events).await;
        // Login health (the Claude plugin's `auth`; hello.runner.claudeAuth).
        if is_claude {
            for e in &out.events {
                let c = match e {
                    AgentEvent::TurnFailed { error } if plugin.is_auth_error(error) => {
                        self.auth.as_mut().and_then(|a| a.hook_failure(&pane_id, error))
                    }
                    AgentEvent::TurnCompleted => self.auth.as_mut().and_then(|a| a.turn_completed(&pane_id)),
                    _ => None,
                };
                self.auth_changed(c).await;
            }
        } else if self.prt.auth.contains_key(plugin.id()) {
            let pid = plugin.id().to_string();
            for e in &out.events {
                let c = match e {
                    AgentEvent::TurnFailed { error } if plugin.is_auth_error(error) => self
                        .prt
                        .auth
                        .get_mut(&pid)
                        .and_then(|a| a.hook_failure(&pane_id, error)),
                    AgentEvent::TurnCompleted => self.prt.auth.get_mut(&pid).and_then(|a| a.turn_completed(&pane_id)),
                    _ => None,
                };
                self.plugin_auth_changed(&pid, c).await;
            }
        }
        // Files (Phase 15, design §2 measure 5): an uploaded file read in this turn is shown with
        // any permission the agent asks for next ("after reading screenshot.png").
        let after_upload = self.note_upload_read(&pane_id, &ev);
        let permission = out.events.iter().find_map(|e| match e {
            AgentEvent::PromptPermission {
                tool,
                input,
                suggestions,
                ..
            } => Some(AgentPrompt {
                tool: tool.clone(),
                input: input.clone(),
                suggestions: suggestions.clone(),
                after_upload: after_upload.clone(),
            }),
            _ => None,
        });
        if let (true, Some(prompt)) = (out.hold, permission) {
            let prompt_id = crate::util::random_id();
            let hold_secs = plugin.manifest().permissions.as_ref().map_or(0, |p| p.hold_secs);
            let hold = if hold_secs > 0 {
                self.opts.prompt_hold.min(Duration::from_secs(hold_secs))
            } else {
                self.opts.prompt_hold
            };
            self.prompts.insert(
                prompt_id.clone(),
                HeldPrompt {
                    pane_id: pane_id.clone(),
                    kind: kind.clone(),
                    reply,
                    deadline: Instant::now() + hold,
                },
            );
            if let Some(rt) = self.panes.get_mut(&pane_id) {
                rt.track.report(&kind, AgentStatus::Blocked, "hook");
                rt.track.prompt = Some((prompt_id, prompt));
            }
            self.agent_update(&pane_id, true).await;
            return; // replied when the decision arrives (or at the deadline)
        }
        if let Some(s) = out.status() {
            // A notification about the dialog keeps a prompt already held.
            let keep_prompt = out.events.iter().any(AgentEvent::keeps_prompt);
            if let Some(rt) = self.panes.get_mut(&pane_id) {
                rt.track.report(&kind, s, "hook");
                if !keep_prompt {
                    rt.track.prompt = None;
                }
            }
            self.agent_update(&pane_id, false).await;
        }
        let res = rewrite.map_or(json!({}), |o| json!({ "output": o }));
        crate::api::respond(reply, Ok(res), self);
        // Agent chat (design §5.1): an agent session (re)started in this pane — a chat that holds it
        // follows it here; a chat bound to this pane on another session is released.
        if let Some(AgentEvent::SessionStarted {
            session_id: Some(v),
            source,
            ..
        }) = out
            .events
            .iter()
            .find(|e| matches!(e, AgentEvent::SessionStarted { .. }))
        {
            self.chat_pane_started(&pane_id, &plugin_id, v, source.as_deref()).await;
        }
    }

    /// The `subagent.*` events of one hook → the pane's helpers
    /// (`Agent.children`, crate::subagents) and their durable events.
    async fn on_subagents(&mut self, pane_id: &str, kind: &str, events: &[crate::plugin::AgentEvent]) {
        use crate::plugin::AgentEvent;
        use tmuxedo_protocol::subagents::{SUBAGENT_COMPLETED, SUBAGENT_STARTED};
        // A helper whose name a builder gave a role carries that role's id.
        let mut events = events.to_vec();
        if events.iter().any(|e| matches!(e, AgentEvent::SubagentStarted(_))) {
            if let Some(set) = crate::role_set::load(&self.opts.state_dir) {
                let cwd = self.state.pane(pane_id).map(|p| p.pane.cwd.clone()).unwrap_or_default();
                let keys = crate::role_set::keys(&set);
                let project = self
                    .projects
                    .project_for(std::path::Path::new(&cwd), &self.delivery.machine, &keys);
                let ma = self.plugins.get(kind).and_then(|p| p.manifest().multi_agent.clone());
                for e in &mut events {
                    if let AgentEvent::SubagentStarted(s) = e {
                        s.role_id =
                            crate::role_launch::role_of_helper(&set, project.as_deref(), kind, ma.as_ref(), &s.name);
                    }
                }
            }
        }
        let Some(rt) = self.panes.get_mut(pane_id) else { return };
        let now = Instant::now();
        let iso = crate::util::iso_now();
        let kids = &mut rt.track.children;
        let mut changed = vec![];
        for e in &events {
            match e {
                AgentEvent::SubagentStarted(s) => {
                    let c = kids.start(s.clone(), now, &iso);
                    changed.extend(c.map(|c| (SUBAGENT_STARTED, c)));
                }
                AgentEvent::SubagentCompleted { id, name } => {
                    let c = kids.complete(id.as_deref(), name.as_deref(), now, &iso);
                    changed.extend(c.map(|c| (SUBAGENT_COMPLETED, c)));
                }
                _ => {}
            }
        }
        if changed.is_empty() {
            return;
        }
        // A helper hook may be the first thing heard from the agent.
        if rt.track.kind().is_none() {
            rt.track.reported_kind = Some(kind.to_string());
        }
        for (t, c) in changed {
            let mut d = json!({ "kind": kind, "childId": c.id, "name": c.name });
            if let Some(p) = c.parent_id {
                d["parentId"] = json!(p);
            }
            if let Some(m) = c.model {
                d["model"] = json!(m);
            }
            if let Some(r) = c.role_id {
                d["roleId"] = json!(r);
            }
            self.queue_event(pane_id, t, d);
        }
        self.flush_events().await;
        self.agent_update(pane_id, false).await;
    }

    // ── pane history (opt-in) ─────────────────────────────────────────────

    fn history_path(&self, pane_id: &str) -> PathBuf {
        self.opts
            .state_dir
            .join("history")
            .join(format!("{}.bin", pane_id.replace(':', "_")))
    }

    /// Write each pane's history (its lines as ANSI, then the screen; decision
    /// 0009) when it changed, at most every [`HISTORY_SAVE_EVERY`] unless `force`.
    fn save_history(&mut self, force: bool) {
        if !self.disk_allows_history() {
            return;
        }
        let now = Instant::now();
        let ids: Vec<String> = self.panes.keys().cloned().collect();
        for id in ids {
            let Some(rec) = self.state.pane(&id) else { continue };
            let fp = self.state.layout_fingerprint(&rec.pane.tab_id);
            let path = self.history_path(&id);
            let Some(rt) = self.panes.get_mut(&id) else { continue };
            if rt.restarting {
                continue;
            }
            let end = rt.end();
            if let Some((at, saved_end)) = rt.history_saved {
                if saved_end == end || (!force && now.duration_since(at) < self.opts.history_save_every) {
                    continue;
                }
            }
            let lines = rt.emu.history_bytes(HISTORY_FILE_MAX);
            let mut blob = fp.into_bytes();
            blob.push(0);
            blob.extend_from_slice(&lines);
            if crate::config::write_private(&path, &blob).is_ok() {
                rt.history_saved = Some((now, end));
            }
        }
    }

    /// Replay saved history only when the tab's layout is the one it was saved under.
    fn load_history(&mut self, pane_id: &str) {
        let Ok(blob) = std::fs::read(self.history_path(pane_id)) else {
            return;
        };
        let Some(nul) = blob.iter().position(|&b| b == 0) else {
            return;
        };
        let tab = self
            .state
            .pane(pane_id)
            .map(|p| p.pane.tab_id.clone())
            .unwrap_or_default();
        if blob[..nul] != *self.state.layout_fingerprint(&tab).as_bytes() {
            log!("{pane_id}: saved history skipped (layout changed)");
            return;
        }
        let mut bytes = blob[nul + 1..].to_vec();
        bytes.extend_from_slice(b"\x1b[0m\r\n");
        if let Some(rt) = self.panes.get_mut(pane_id) {
            rt.preload(&bytes);
            log!("{pane_id}: replayed {} bytes of saved history", bytes.len());
        }
        if let Some(p) = self.state.pane_mut(pane_id) {
            p.base += bytes.len() as u64;
            p.high_water = p.base;
        }
    }

    fn remove_history(&self, pane_id: &str) {
        let _ = std::fs::remove_file(self.history_path(pane_id));
    }
}
