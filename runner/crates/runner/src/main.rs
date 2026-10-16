//! `tmuxedo-runner`: one binary, several roles. See `tmuxedo-runner --help`.
//! Also installed as `tmuxedo` (the CLI) and, deprecated, `aimux-runner` and `aimux`.

use clap::{Parser, Subcommand};
use std::path::PathBuf;
use tmuxedo_runner::config::{gateway_url, http_base, RunnerConfig};
use tmuxedo_runner::daemon::{ExitReason, RunOptions};
use tmuxedo_runner::{log, paths, EXIT_NO_RESTART, VERSION};

#[derive(Parser)]
#[command(name = "tmuxedo-runner", version = VERSION, about = "Connects this machine's terminals to tmuxedo")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the PTY holder (normally its own service: tmuxedo-holder).
    Holder {
        /// Socket path (default $XDG_RUNTIME_DIR/tmuxedo/holder.sock or ~/.local/state/tmuxedo/holder.sock).
        #[arg(long)]
        socket: Option<PathBuf>,
        /// While another holder (e.g. one started before the aimux → tmuxedo
        /// rename) still serves sessions, wait for it to exit instead of
        /// failing. The service units use this.
        #[arg(long)]
        wait: bool,
        /// Ring size per pane in bytes.
        #[arg(long, default_value_t = tmuxedo_holder::DEFAULT_RING)]
        ring: usize,
    },
    /// Run the runner daemon: connect to the holder and dial out to the gateway.
    Run {
        /// Server (https://…) or gateway URL (wss://…/gw/runner); default from runner.toml.
        #[arg(long)]
        server: Option<String>,
        /// Never start a holder; wait for one (the systemd/launchd setup).
        #[arg(long)]
        no_start_holder: bool,
        /// Command for the default pane instead of the login shell.
        #[arg(long, num_args = 1.., allow_hyphen_values = true)]
        shell: Option<Vec<String>>,
        /// Started and supervised by tmuxedo for Mac: never update itself (the
        /// app's own updates replace it), take the server and token from
        /// TMUXEDO_SERVER / TMUXEDO_RUNNER_TOKEN, and sign the gateway's key
        /// challenge through TMUXEDO_SIGNER_FD (docs/design/mac-pairing.md §3).
        #[arg(long)]
        managed: bool,
        /// The folder new panes start in when tmuxedo has not set one (`~`, `~/…` or absolute;
        /// default `~/work`). A `workDir` from tmuxedo for this machine still wins.
        #[arg(long)]
        work_dir: Option<String>,
        /// Serve only this account (user id); a connection naming another stops the runner.
        #[arg(long)]
        allow_user: Option<String>,
        /// Remote viewers may watch but not type, resize, decide or change anything.
        #[arg(long)]
        observe_only: bool,
        /// Ask the person at this Mac (through the app, over the signer) before remote control;
        /// held input is sent only once allowed. A timeout counts as Deny.
        #[arg(long)]
        confirm_on_mac: bool,
        /// Seconds the Mac's question stays open (5–300).
        #[arg(long, default_value_t = 30)]
        confirm_timeout: u64,
        /// An approval lapses after this many seconds without remote control (60–86400).
        #[arg(long, default_value_t = 900)]
        confirm_idle: u64,
        /// Let headless automation start tasks here, for accounts whose policy names this
        /// label (repeatable; adds to `automation_labels` in runner.toml). Off when none.
        #[arg(long = "automation-label")]
        automation_label: Vec<String>,
    },
    /// Pair this machine: a QR code and a link to approve on your phone or in a
    /// browser, with a match code to compare (docs/design/mac-pairing.md §2.5).
    Pair {
        #[arg(long, default_value = "https://tmuxedo.dev")]
        server: String,
        /// Machine name shown in tmuxedo (default: hostname).
        #[arg(long)]
        name: Option<String>,
    },
    /// Report the running holder: version, pid and code fingerprint (JSON).
    HolderInfo {
        #[arg(long)]
        socket: Option<PathBuf>,
    },
    /// Enrol this machine with a device code.
    Enrol {
        #[arg(long)]
        server: String,
        /// Machine name shown in tmuxedo (default: hostname).
        #[arg(long)]
        name: Option<String>,
    },
    /// An agent's command hook: native event JSON on stdin (docs/design/plugins.md §5.4).
    Hook {
        /// The plugin the hook belongs to (default: claude).
        #[arg(long)]
        plugin: Option<String>,
        /// A chat's Auto-approve guard (PreToolUse on every tool): asks the
        /// runner and fails closed (denies) when it cannot.
        #[arg(long)]
        guard: bool,
        /// The agent's status-line command: records the usage numbers on stdin,
        /// then runs the person's own status line and prints it unchanged.
        #[arg(long)]
        statusline: bool,
    },
    /// The PATH stand-in for URL openers (xdg-open, open, …) in panes.
    #[command(hide = true)]
    OpenUrl {
        #[arg(long)]
        program: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Print the version (`--json`: version, holder build, platform, features).
    Version {
        #[arg(long)]
        json: bool,
    },
    /// Install and start the holder and runner user services.
    InstallService,
    /// Stop and remove the user services.
    UninstallService,
    /// Update the runner binary from <server>/dl/manifest.json.
    Update {
        #[arg(long)]
        server: Option<String>,
        /// Only report what would happen.
        #[arg(long)]
        check: bool,
        /// Also restart the holder onto the new binary. ENDS EVERY SESSION.
        #[arg(long)]
        holder: bool,
    },
    /// Debug: stream one holder pane to a file (used by tests).
    #[command(hide = true)]
    HolderAttach {
        #[arg(long)]
        socket: PathBuf,
        #[arg(long)]
        id: String,
        #[arg(long, default_value_t = 0)]
        from: u64,
        #[arg(long)]
        out: PathBuf,
    },
}

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    // The aimux → tmuxedo rename on disk (dirs, then this binary's own name),
    // before the binary path is recorded. Not for the hook (it runs on every
    // Claude event) or `version`.
    if !matches!(argv.get(1).map(String::as_str), Some("hook" | "version")) {
        for line in tmuxedo_runner::migrate::run() {
            log!("{line}");
        }
    }
    // Before anything can replace the binary (self-update re-execs this path).
    tmuxedo_runner::update::remember_exe();
    // `tmuxedo` (a symlink to this binary; `aimux` deprecated) and
    // `tmuxedo-runner cli …` are the CLI.
    let invoked = argv
        .first()
        .map(|a| a.rsplit('/').next().unwrap_or(a).to_string())
        .unwrap_or_default();
    if invoked == tmuxedo_runner::migrate::CLI || invoked == tmuxedo_runner::migrate::LEGACY_CLI {
        std::process::exit(tmuxedo_runner::cli::main(argv[1..].to_vec()));
    }
    if argv.get(1).map(String::as_str) == Some("cli") {
        std::process::exit(tmuxedo_runner::cli::main(argv[2..].to_vec()));
    }
    let cli = Cli::parse();
    let code = match cli.cmd {
        Cmd::Version { json } => {
            if json {
                println!("{}", tmuxedo_runner::pair::version_json());
            } else {
                println!("tmuxedo-runner {VERSION} ({})", tmuxedo_runner::util::platform_key());
            }
            0
        }
        Cmd::Pair { server, name } => match tmuxedo_runner::pair::pair(&server, name, &paths::config_path()) {
            Ok(_) => 0,
            Err(e) => {
                eprintln!("pairing failed: {e}");
                1
            }
        },
        Cmd::HolderInfo { socket } => {
            let socket = socket.unwrap_or_else(paths::holder_socket);
            println!("{}", tmuxedo_runner::pair::holder_info(&socket));
            0
        }
        Cmd::Holder { socket, ring, wait } => holder(socket, ring, wait),
        Cmd::Run {
            server,
            no_start_holder,
            shell,
            managed,
            work_dir,
            allow_user,
            observe_only,
            confirm_on_mac,
            confirm_timeout,
            confirm_idle,
            automation_label,
        } => run(
            server,
            !no_start_holder,
            shell,
            managed,
            work_dir,
            tmuxedo_runner::guardrails::Guardrails {
                allow_user,
                observe_only,
            },
            tmuxedo_runner::confirm::Policy {
                enabled: confirm_on_mac,
                timeout: std::time::Duration::from_secs(confirm_timeout.clamp(5, 300)),
                idle: std::time::Duration::from_secs(confirm_idle.clamp(60, 86_400)),
                ..Default::default()
            },
            automation_label,
        ),
        Cmd::Enrol { server, name } => match tmuxedo_runner::enrol::enrol(&server, name, &paths::config_path()) {
            Ok(_) => 0,
            Err(e) => {
                eprintln!("enrol failed: {e}");
                1
            }
        },
        Cmd::OpenUrl { program, args } => {
            let args: Vec<String> = args.into_iter().skip_while(|a| a == "--").collect();
            tmuxedo_runner::open_url::run(&program, &args)
        }
        Cmd::Hook {
            statusline: true,
            plugin,
            ..
        } => {
            use std::io::Write;
            let env = tmuxedo_runner::usage::StatusEnv {
                plugin: plugin.unwrap_or_else(|| tmuxedo_runner::plugin::DEFAULT_PLUGIN.to_string()),
                socket: tmuxedo_holder::env_os("SOCKET_PATH")
                    .map(PathBuf::from)
                    .unwrap_or_else(paths::runner_socket),
                pane_id: tmuxedo_holder::env_var("PANE_ID"),
                config_dir: std::env::var_os("CLAUDE_CONFIG_DIR")
                    .map(PathBuf::from)
                    .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".claude"))),
                cwd: std::env::current_dir().ok(),
                timeout: tmuxedo_runner::usage::USER_TIMEOUT,
                now_s: tmuxedo_runner::util::now_ms() / 1000,
            };
            let (out, code) = tmuxedo_runner::usage::run_statusline(&mut std::io::stdin(), &env);
            let mut so = std::io::stdout();
            let _ = so.write_all(&out);
            let _ = so.flush();
            code
        }
        Cmd::Hook { guard: true, .. } => {
            let out = tmuxedo_runner::hook::run_chat_guard(
                &mut std::io::stdin(),
                tmuxedo_holder::env_os("SOCKET_PATH").map(PathBuf::from),
                tmuxedo_holder::env_var("CHAT_ID").filter(|c| !c.is_empty()),
            );
            if !out.is_empty() {
                println!("{out}");
            }
            0
        }
        Cmd::Hook { plugin, .. } => {
            let out = tmuxedo_runner::hook::run_hook_in(
                plugin.as_deref().unwrap_or(tmuxedo_runner::plugin::DEFAULT_PLUGIN),
                &mut std::io::stdin(),
                tmuxedo_holder::env_os("SOCKET_PATH").map(PathBuf::from),
                tmuxedo_holder::env_var("PANE_ID"),
                tmuxedo_holder::env_var("CHAT_ID").filter(|c| !c.is_empty()),
            );
            if !out.is_empty() {
                println!("{out}");
            }
            0
        }
        Cmd::InstallService => report(tmuxedo_runner::service::install()),
        Cmd::UninstallService => report(tmuxedo_runner::service::uninstall()),
        Cmd::Update { server, check, holder } => update(server, check, holder),
        Cmd::HolderAttach { socket, id, from, out } => holder_attach(&socket, &id, from, &out),
    };
    std::process::exit(code);
}

fn report(r: Result<(), String>) -> i32 {
    match r {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    }
}

fn holder(socket: Option<PathBuf>, ring: usize, wait: bool) -> i32 {
    let socket = socket.unwrap_or_else(paths::holder_socket);
    if wait {
        wait_for_other_holders(&socket);
    }
    let bound = match tmuxedo_holder::server::bind(&tmuxedo_holder::Config {
        socket: socket.clone(),
        ring_capacity: ring,
    }) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("holder: {}: {e}", socket.display());
            return 1;
        }
    };
    log!(
        "holder {VERSION} (pid {}) listening on {}",
        std::process::id(),
        socket.display()
    );
    match tmuxedo_holder::server::run(bound) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("holder: {e}");
            1
        }
    }
}

/// Block while another holder serves sessions at `socket` or at the aimux-era
/// socket: a holder started before the rename keeps its PTYs until it exits,
/// and the runner uses it meanwhile (daemon::connect_holder). Taking over only
/// then means installing the tmuxedo services never ends a session.
fn wait_for_other_holders(socket: &std::path::Path) {
    let legacy = tmuxedo_holder::legacy_socket();
    let mut said = false;
    loop {
        let busy: Vec<String> = [Some(socket), legacy.as_deref()]
            .into_iter()
            .flatten()
            .filter(|p| tmuxedo_holder::holder_alive(p))
            .map(|p| p.display().to_string())
            .collect();
        if busy.is_empty() {
            if said {
                log!("the other holder exited; starting");
            }
            return;
        }
        if !said {
            log!(
                "another holder still serves sessions at {}; waiting for it to exit (its sessions are left alone)",
                busy.join(", ")
            );
            said = true;
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
}

#[allow(clippy::too_many_arguments)]
fn run(
    server: Option<String>,
    start_holder: bool,
    shell: Option<Vec<String>>,
    managed: bool,
    work_dir: Option<String>,
    guardrails: tmuxedo_runner::guardrails::Guardrails,
    confirm: tmuxedo_runner::confirm::Policy,
    automation_label: Vec<String>,
) -> i32 {
    let cfg = RunnerConfig::load(&paths::config_path());
    let token = tmuxedo_holder::env_var("RUNNER_TOKEN").or_else(|| cfg.as_ref().ok().map(|c| c.token.clone()));
    let server = server
        .or_else(|| managed.then(|| tmuxedo_holder::env_var("SERVER")).flatten())
        .or_else(|| cfg.as_ref().ok().map(|c| c.server.clone()));
    // The credential reached us through the environment: keep it out of every child's.
    std::env::remove_var("TMUXEDO_RUNNER_TOKEN");
    std::env::remove_var("AIMUX_RUNNER_TOKEN");
    let signer = match tmuxedo_runner::signer::from_env() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("signer: {e}");
            return 1;
        }
    };
    let (Some(token), Some(server)) = (token, server) else {
        eprintln!(
            "not enrolled: run `tmuxedo-runner enrol --server https://…` first ({})",
            cfg.err().unwrap_or_default()
        );
        return 1;
    };
    let mut opts = RunOptions::new(gateway_url(&server), token);
    opts.http_base = Some(http_base(&server));
    opts.start_holder = start_holder;
    opts.shell = shell;
    // Managed by tmuxedo for Mac: Sparkle replaces the whole app, the runner never replaces itself.
    opts.auto_update = !managed && cfg.as_ref().map(|c| c.auto_update).unwrap_or(false);
    opts.signer = signer;
    if guardrails != Default::default() {
        log!(
            "guardrails: {}{}",
            guardrails
                .allow_user
                .as_deref()
                .map(|u| format!("serves only account {u}"))
                .unwrap_or_default(),
            if guardrails.observe_only { "; observe-only" } else { "" }
        );
    }
    opts.guardrails = guardrails;
    if confirm.enabled {
        log!(
            "guardrails: confirm on this Mac before remote control ({} s to answer, approval lapses after {} s idle){}",
            confirm.timeout.as_secs(),
            confirm.idle.as_secs(),
            if opts.signer.is_none() {
                "; NO APP TO ASK: every request will be denied"
            } else {
                ""
            }
        );
    }
    opts.confirm = confirm;
    if let Some(w) = work_dir.filter(|w| !w.trim().is_empty()) {
        opts.default_work_dir = w;
    }
    if managed {
        log!(
            "managed by tmuxedo for Mac: self-update off{}",
            if opts.signer.is_some() {
                ", key signer attached"
            } else {
                ""
            }
        );
    }
    if let Some(s) = tmuxedo_holder::env_var("UPDATE_AFTER_SECS").and_then(|s| s.parse().ok()) {
        opts.update_after = std::time::Duration::from_secs(s);
    }
    opts.pane_history = cfg.as_ref().map(|c| c.pane_history).unwrap_or(false);
    let mut automation = cfg.as_ref().map(|c| c.automation_labels.clone()).unwrap_or_default();
    automation.extend(automation_label);
    if let Some(a) = tmuxedo_runner::task::consent(&automation) {
        log!("automation: on, labels {}", a.labels.join(", "));
    }
    opts.automation_labels = automation;
    opts.history = tmuxedo_runner::config::HistorySize::resolve(
        tmuxedo_holder::env_var("HISTORY_LINES").as_deref(),
        tmuxedo_holder::env_var("HISTORY_MIB").as_deref(),
        cfg.as_ref().ok(),
        tmuxedo_runner::plugin_wire::Wire::new(Default::default()).hosted,
    );
    log!(
        "pane history: {} lines, {} MiB cap per pane{}",
        opts.history.lines,
        opts.history.bytes >> 20,
        if opts.pane_history { ", saved to disk" } else { "" }
    );
    if let Some(dir) = cfg.as_ref().ok().and_then(|c| c.worktree_dir.as_deref()) {
        opts.worktree_dir = tmuxedo_runner::worktree::expand_tilde(dir, &opts.home);
    }
    opts.inject_hooks = cfg.as_ref().map(|c| c.hooks).unwrap_or(true);
    let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("runtime: {e}");
            return 1;
        }
    };
    log!("tmuxedo-runner {VERSION} starting; gateway {}", opts.gateway_url);
    let reason = rt.block_on(tmuxedo_runner::daemon::run(opts));
    rt.shutdown_timeout(std::time::Duration::from_secs(1));
    match reason {
        Ok(ExitReason::Fatal(code)) => {
            log!("exiting without restart (close code {code})");
            EXIT_NO_RESTART
        }
        Ok(ExitReason::HolderLost) => {
            log!("holder connection lost; exiting so the service manager restarts us");
            1
        }
        Ok(ExitReason::Updated) => {
            let e = tmuxedo_runner::update::reexec();
            log!("re-exec failed: {e}");
            1
        }
        Ok(ExitReason::Signal) => 0,
        Err(e) => {
            log!("runner: {e}");
            1
        }
    }
}

fn update(server: Option<String>, check: bool, holder: bool) -> i32 {
    let server = server.or_else(|| RunnerConfig::load(&paths::config_path()).ok().map(|c| c.server));
    let Some(server) = server else {
        eprintln!("no server: pass --server or enrol first");
        return 1;
    };
    let base = http_base(&server);
    if check {
        return match tmuxedo_runner::update::fetch_manifest(&base) {
            Ok(m) => {
                println!("installed {VERSION}, available {}", m.version);
                0
            }
            Err(e) => {
                eprintln!("{e}");
                1
            }
        };
    }
    std::env::set_var("TMUXEDO_UPDATE_DEV", "1"); // an explicit update also replaces dev builds
    match tmuxedo_runner::update::update_runner(&base) {
        Ok(tmuxedo_runner::update::Outcome::Current) => println!("already at {VERSION}"),
        Ok(tmuxedo_runner::update::Outcome::Updated(v)) => {
            println!("updated to {v}; the runner service picks it up on its next restart")
        }
        Err(e) => {
            eprintln!("update failed: {e}");
            return 1;
        }
    }
    if holder {
        eprintln!("restarting the holder: every terminal session on this machine ends now");
        return report(tmuxedo_runner::service::restart_holder(None));
    }
    0
}

/// Debug client: write the attach start offset to `<out>.start`, then append
/// every data byte to `<out>` as it arrives, until killed.
fn holder_attach(socket: &std::path::Path, id: &str, from: u64, out: &std::path::Path) -> i32 {
    use std::io::Write;
    use tmuxedo_holder::wire::{self, Event, Msg, Request, HOLDER_PROTO};
    let Ok(mut s) = std::os::unix::net::UnixStream::connect(socket) else {
        return 1;
    };
    let _ = wire::write_all(
        &mut s,
        &wire::encode_json(&Request::Hello {
            proto: HOLDER_PROTO,
            client: "holder-attach".into(),
        }),
    );
    let _ = wire::write_all(
        &mut s,
        &wire::encode_json(&Request::Attach {
            rid: 1,
            id: id.into(),
            from,
        }),
    );
    let Ok(mut f) = std::fs::File::create(out) else {
        return 1;
    };
    loop {
        match wire::read_msg(&mut s) {
            Ok(Msg::Json(j)) => {
                if let Ok(Event::Attached { from, .. }) = serde_json::from_slice::<Event>(&j) {
                    let _ = std::fs::write(out.with_extension("start"), from.to_string());
                }
            }
            Ok(Msg::Data { bytes, .. }) => {
                if f.write_all(&bytes).is_err() {
                    return 1;
                }
            }
            Err(_) => return 0,
        }
    }
}
