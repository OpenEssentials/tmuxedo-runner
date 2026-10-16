import Foundation

/// Everything the app does, composed: pairing state, the runner's supervisor, the
/// holder, migration from an install.sh setup, Unpair, the kill switch and the
/// dashboard's snapshot. The app delegate and the views only call into this, so
/// the behaviour is testable without a window (AppModelTests).
@MainActor
final class AppModel: ObservableObject {
    let settings: Settings
    let keys: MachineKeyStore
    let supervisor: Supervisor
    let holder: HolderService?
    let binary: RunnerBinary?
    let profile: String?

    @Published private(set) var snapshot = LocalAPI.Snapshot.empty
    @Published private(set) var snapshotError: String?
    @Published private(set) var migration = Migration.Found()
    @Published private(set) var pushStatus = "Not registered"
    @Published private(set) var lastMessage: String?

    private var apnsToken: String?
    private var snapshotTimer: Timer?
    private var lastBlocked: Set<String> = []
    /// Called with an agent that just started needing the person (local notifications).
    var onNeedsYou: ((LocalAPI.Agent) -> Void)?
    /// Injectable for tests: the pairing API for the configured server.
    var api: () -> PairingAPI?
    var launchctl: ([String]) -> Int32 = { args in Shell.run("/bin/launchctl", args)?.0 ?? -1 }
    var scanMigration: () -> Migration.Found = { Migration.scan() }
    /// The runner's local API, run off the main thread.
    var readSnapshot: (URL) async -> Result<LocalAPI.Snapshot, Error> = { socket in
        await Task.detached { Result { try LocalAPI(socket: socket).snapshot() } }.value
    }

    /// The separate-user mode (nil: full access only, as in a profile copy or a test of that mode).
    let iso: IsolationManager?
    /// Confirm on this Mac: the questions the runner asks through the signer.
    let confirm = ConfirmBroker()
    /// Starts the runner as the separate user's LaunchDaemon (through the helper).
    let daemonLauncher: DaemonLauncher?
    /// The uid of the separate user, for the signer socket's admission (looked up when needed).
    var isoUID: () -> uid_t? = { getpwnam(Iso.user).map { $0.pointee.pw_uid } }
    var isoGID: () -> gid_t? = { getgrnam(Iso.group).map { $0.pointee.gr_gid } }
    /// Where the separate user's runner signs (the tests use a temporary folder).
    var signerSocketPath = Iso.signerSocket

    init(settings: Settings, keys: MachineKeyStore, binary: RunnerBinary?, holder: HolderService?,
         launcher: ProcessLauncher = PosixLauncher(), profile: String? = Profile.name,
         iso: IsolationManager? = nil, daemonLauncher: DaemonLauncher? = nil,
         schedule: ((TimeInterval, @escaping @MainActor () -> Void) -> Void)? = nil) {
        self.settings = settings
        self.keys = keys
        self.binary = binary
        self.holder = holder
        self.profile = profile
        self.iso = profile == nil ? iso : nil
        self.daemonLauncher = profile == nil ? daemonLauncher : nil
        weak var weakSelf: AppModel?
        let make: () -> Result<(LaunchSpec, SignerEndpoint?), SupervisorError> = {
            MainActor.assumeIsolated { weakSelf?.prepareSpec() ?? .failure(.notPaired) }
        }
        let modeOf: () -> IsolationMode = { MainActor.assumeIsolated { weakSelf?.mode ?? .fullAccess } }
        let switching = SwitchingLauncher(child: launcher, daemon: profile == nil ? daemonLauncher : nil, mode: modeOf)
        if let schedule {
            supervisor = Supervisor(launcher: switching, schedule: schedule, makeSpec: make)
        } else {
            supervisor = Supervisor(launcher: switching, makeSpec: make)
        }
        api = { settings.server.map { PairingAPI(server: $0) } }
        weakSelf = self
        supervisor.setIdle(paired: settings.isPaired)
        supervisor.onStateChange = { [weak self] _ in self?.applyKeepAwake() }
        supervisor.isPaired = { [weak settings] in settings?.isPaired ?? false }
        supervisor.onWelcome = { [weak settings] machineId, userId in settings?.learnAccount(machineId: machineId, userId: userId) }
    }

    // MARK: - Keep awake

    let sleepGuard = SleepGuard()

    /// Hold off idle sleep while connected, when the owner asked for it.
    func applyKeepAwake() {
        if settings.keepAwake && supervisor.display.isRunning { sleepGuard.hold() } else { sleepGuard.release() }
    }

    var runtimeRoot: URL? { Profile.runtimeRoot(for: profile) }
    var runnerSocket: URL {
        mode == .separateUser ? URL(fileURLWithPath: Iso.apiSocket) : LocalAPI.defaultSocket(runtimeRoot: runtimeRoot)
    }

    /// The mode in effect: a profile copy, or an app without the separate-user machinery, is full access.
    var mode: IsolationMode { iso == nil ? .fullAccess : settings.isolation }

    // MARK: - Launching the runner

    /// Everything the runner needs, or why it cannot start. Checked at EVERY start.
    func prepareSpec() -> Result<(LaunchSpec, SignerEndpoint?), SupervisorError> {
        guard let machine = settings.machine, let token = settings.runnerToken() else { return .failure(.notPaired) }
        guard let binary else { return .failure(.broken("the bundled tmuxedo-runner is missing; reinstall tmuxedo")) }
        if let why = RunnerBinary.mayLaunch(check: RunnerBinary.checkSignature(of: binary.url, team: RunnerBinary.appTeam),
                                            appTeam: RunnerBinary.appTeam) {
            return .failure(.broken(why))
        }
        let isolated = mode == .separateUser
        if isolated, iso?.phase != .ready {
            return .failure(.broken("terminals run as a separate user, which is not set up yet: Settings, Isolation"))
        }
        let broker = settings.confirmOnMac ? confirm : nil
        var signer: SignerEndpoint?
        var childFD: Int32?
        if machine.keyBound {
            do {
                guard let key = try keys.load() else {
                    return .failure(.broken("this Mac's key is gone (the Keychain item was removed); pair it again"))
                }
                if let fp = machine.keyFingerprint, fp != key.fingerprint {
                    return .failure(.broken("this Mac's key does not match its pairing; pair it again"))
                }
                guard let lookup = runnerTokenLookup(token) else { return .failure(.broken("the stored token is malformed; pair again")) }
                if isolated {
                    guard let uid = isoUID() else { return .failure(.broken("the user \(Iso.user) is missing; set it up again")) }
                    let launcher = daemonLauncher
                    let admission = SignerAdmission(uid: uid, runnerPid: { launcher?.runnerPid(refresh: $0) },
                                                    requirement: RunnerBinary.appTeam.map {
                                                        "anchor apple generic and certificate leaf[subject.OU] = \"\($0)\""
                                                    })
                    signer = SignerSocketServer(path: signerSocketPath, key: key, lookup: lookup, broker: broker, group: isoGID(),
                                                admit: admission.check,
                                                onRefuse: { [weak self] why in
                                                    DispatchQueue.main.async {
                                                        MainActor.assumeIsolated { self?.supervisor.note("signer refused a caller: \(why)", error: true) }
                                                    }
                                                })
                } else {
                    let channel = try SignerChannel(key: key, lookup: lookup, broker: broker)
                    childFD = channel.childFD
                    signer = channel
                }
            } catch {
                return .failure(.broken(error.localizedDescription))
            }
        } else if isolated {
            // A bearer token (an adopted CLI enrolment) has no key to protect, but confirm-on-Mac
            // needs the app reachable: the separate user is only offered with a key-bound pairing.
            return .failure(.broken("separate-user mode needs this Mac paired with its own key: pair again"))
        }
        let spec = isolated
            ? LaunchSpec(executable: URL(fileURLWithPath: Iso.runnerPath), arguments: isoConfig()?.arguments ?? [],
                         environment: [:], signerFD: nil)
            : LaunchSpec.runner(binary: binary.url, server: machine.server, token: token, signerFD: childFD,
                                workDir: settings.resolvedWorkDir, runtimeDir: runtimeRoot, startHolder: profile != nil,
                                allowUser: machine.userId, observeOnly: settings.observeOnly,
                                confirm: settings.confirmOnMac, confirmTimeout: settings.confirmTimeout)
        return .success((spec, signer))
    }

    /// The separate user's runner settings, from the pairing and the guardrails.
    func isoConfig() -> IsoRunnerConfig? {
        guard let m = settings.machine, let token = settings.runnerToken() else { return nil }
        let work = settings.resolvedWorkDir.flatMap { w in
            settings.grantState.grants.contains(where: { w == $0 || w.hasPrefix($0 + "/") }) ? w : nil
        }
        return IsoRunnerConfig(server: m.server, token: token, allowUser: m.userId, observeOnly: settings.observeOnly,
                               confirm: settings.confirmOnMac, confirmTimeout: settings.confirmTimeout, workDir: work)
    }

    // MARK: - Lifecycle

    /// At launch: look for an install.sh setup, make sure the holder is the right one, connect.
    func boot() {
        migration = scanMigration()
        supervisor.setIdle(paired: settings.isPaired)
        let connect = settings.isPaired && settings.connectAtLaunch && migration.runnerPlists.isEmpty
        if mode == .separateUser, let iso {
            // The helper says whether the separate user is ready (and updates its runtime) first.
            Task {
                await iso.refresh()
                if iso.phase == .ready {
                    _ = await iso.call(HelperRequest(.installRuntime))
                    if connect { supervisor.start() }
                }
            }
            return
        }
        ensureHolder()
        if connect { supervisor.start() }
    }

    /// The holder decision (keep / start / restart once after a holder change). In separate-user
    /// mode the helper makes it (installRuntime), for the holder that runs as `tmuxedo`.
    func ensureHolder() {
        guard profile == nil else { return }
        if mode == .separateUser {
            guard let iso, iso.phase == .ready || iso.phase == .unknown else { return }
            Task {
                await iso.refresh()
                if iso.phase == .ready { _ = await iso.call(HelperRequest(.installRuntime)) }
            }
            return
        }
        guard let holder, let v = binary?.version() else { return }
        holder.ensure(bundledBuild: v.holderBuild)
    }

    // MARK: - Modes (docs/design/mac-isolation.md §2)

    /// Move to the separate user: explicit, explained in the UI first. The runner stops; the
    /// owner's runner state and the Claude transcripts of granted folders are copied to the
    /// separate user (so sessions resume there); the full-access holder is stopped, which ENDS
    /// the terminals that ran as the owner; then the runner starts as `tmuxedo`.
    func moveToSeparateUser() async -> Bool {
        guard let iso else { return false }
        let wasRunning = supervisor.isRunning
        if iso.phase != .ready {
            guard await iso.setUp() else { return false }
        }
        supervisor.stop()
        for _ in 0..<100 where supervisor.isRunning { try? await Task.sleep(nanoseconds: 50_000_000) }
        do {
            let n = try iso.stage()
            let r = await iso.call(HelperRequest(.importState))
            try? FileManager.default.removeItem(atPath: iso.stagingDir)
            guard r.ok else {
                lastMessage = "Could not copy your sessions to the separate user: \(r.error ?? "unknown error"). Nothing was changed."
                if wasRunning { start() }
                return false
            }
            supervisor.note("moved \(n) items of runner state and Claude sessions to the separate user")
        } catch {
            try? FileManager.default.removeItem(atPath: iso.stagingDir)
            lastMessage = "Could not stage your sessions: \(error.localizedDescription). Nothing was changed."
            if wasRunning { start() }
            return false
        }
        // The owner's holder: its terminals ran as the owner; separate-user mode means they end.
        holder?.stopAndUnregister()
        settings.isolation = .separateUser
        lastMessage = "Terminals now run as the separate user “\(Iso.user)”. Your sessions resume there."
        if settings.isPaired { start() }
        return true
    }

    /// Full access: opt-in, after the explicit warning in the UI. The separate user's terminals
    /// end (its daemons stop); the user, its home and the grants stay until removed.
    func switchToFullAccess() async {
        supervisor.stop()
        for _ in 0..<100 where supervisor.isRunning { try? await Task.sleep(nanoseconds: 50_000_000) }
        await iso?.stopAll()
        settings.isolation = .fullAccess
        supervisor.note("full access: terminals run as you from now on", error: true)
        ensureHolder()
        if settings.isPaired { start() }
    }

    /// Remove the separate user (Settings): everything it had goes, and this Mac is in full access.
    func removeSeparateUser() async -> Bool {
        guard let iso else { return false }
        supervisor.stop()
        for _ in 0..<100 where supervisor.isRunning { try? await Task.sleep(nanoseconds: 50_000_000) }
        let ok = await iso.uninstall()
        if ok { lastMessage = "The separate user and everything it had are removed. Terminals run as you (full access)." }
        return ok
    }

    func start() {
        guard settings.isPaired else { supervisor.setIdle(paired: false); return }
        if !migration.runnerPlists.isEmpty {
            // Two runners for one machine replace each other for ever: retire install.sh's first.
            finishMigration()
        }
        supervisor.start()
    }

    func stop() { supervisor.stop() }

    /// After a pairing: store it, retire an install.sh runner, connect.
    func paired(_ m: PairedMachine, token: String) {
        settings.storePairing(m, token: token)
        lastMessage = "Paired as \(m.name) (\(m.method))."
        supervisor.note("paired as \(m.name) via \(m.method); machine key \(m.keyFingerprint.map { String($0.prefix(16)) } ?? "none")")
        start()
        registerPushIfPossible()
    }

    /// Retire install.sh's services without ending a session (Migration.plan).
    func finishMigration() {
        let steps = Migration.plan(migration)
        Migration.execute(steps, launchctl: launchctl, say: { [weak self] in self?.supervisor.note($0) })
        migration = scanMigration()
    }

    /// Use an existing `tmuxedo-runner enrol` token (bearer-only) instead of pairing anew.
    func adopt(_ e: Migration.Enrolment) {
        let m = PairedMachine(machineId: "", name: e.name ?? (Host.current().localizedName ?? "this Mac"), server: e.server,
                              method: "cli", pairedAt: Date(), keyBound: false, keyFingerprint: nil)
        settings.storePairing(m, token: e.token)
        try? Migration.scrubToken(from: e.configPath)
        supervisor.note("adopted the existing enrolment (bearer token; pair again for a key-bound one)")
        finishMigration()
        start()
    }

    /// Unpair: the server revokes the machine (best effort when offline), this Mac forgets
    /// its token and key. Terminals keep running in the holder; nobody can reach them.
    func unpair() async {
        supervisor.stop()
        if let api = api(), let token = settings.runnerToken(), let key = try? keys.load() {
            do {
                try await api.unpair(runnerToken: token, key: key)
                supervisor.note("unpaired: tmuxedo revoked this machine")
            } catch {
                supervisor.note("unpair: tmuxedo could not be told (\(PairingFlow.describe(error))); revoke this Mac on tmuxedo.dev too", error: true)
            }
        }
        settings.forgetPairing()
        keys.delete()
        pushStatus = "Not registered"
        supervisor.setIdle(paired: false)
        lastMessage = "This Mac is no longer paired."
    }

    /// THE KILL SWITCH (works with tmuxedo's servers down or compromised): stop the runner,
    /// stop the holder (ENDS every terminal), and drop the credential and the key here.
    func killSwitch() {
        supervisor.terminateNow()
        holder?.stopAndUnregister()
        if let iso { Task { await iso.stopAll() } }
        settings.forgetPairing()
        keys.delete()
        supervisor.setIdle(paired: false)
        supervisor.note("kill switch: runner and holder stopped, credential and key deleted on this Mac", error: true)
        lastMessage = "Kill switch: everything stopped and this Mac's credential deleted. Revoke it on tmuxedo.dev as well."
    }

    // MARK: - Push

    func apnsTokenArrived(_ token: Data, sandbox: Bool) {
        apnsToken = token.hex
        apnsSandbox = sandbox
        registerPushIfPossible()
    }
    private var apnsSandbox = false

    func registerPushIfPossible() {
        guard settings.notifications, let apns = apnsToken, let m = settings.machine, m.keyBound,
              let token = settings.runnerToken(), let key = try? keys.load(), let api = api() else { return }
        Task {
            do {
                try await api.registerPush(token: apns, sandbox: apnsSandbox, runnerToken: token, key: key)
                pushStatus = "Registered"
            } catch {
                pushStatus = "Not registered: \(PairingFlow.describe(error))"
            }
        }
    }

    // MARK: - Activity (SystemMonitor)

    /// Whose processes the Activity charts call "Agent": in separate-user mode every process of
    /// the `tmuxedo` user; otherwise the holder's terminals (descendants of the holder's pid).
    /// nil when there is neither.
    var agentScope: AgentScope? {
        if mode == .separateUser {
            guard let pw = getpwnam(Iso.user) else { return nil }
            return AgentScope(roots: [], uid: pw.pointee.pw_uid)
        }
        if case let .running(_, pid?)? = holder?.state { return AgentScope(roots: [pid_t(pid)], uid: nil) }
        return nil
    }

    private var working = false
    private var workingInFlight = false

    /// Whether any agent on this Mac is working (the runner's `agent.list`), as last read. Each call
    /// starts one read in the background (never two at once) and answers the previous one, so the
    /// 2 s sampler never waits on the socket.
    func pollAgentsWorking() -> Bool {
        guard supervisor.isRunning else { working = false; return false }
        if !workingInFlight {
            workingInFlight = true
            let socket = runnerSocket
            Task { [weak self] in
                let r = await Task.detached { try? LocalAPI.parseAgents(LocalAPI(socket: socket).call("agent.list", timeout: 1)) }.value
                await MainActor.run {
                    self?.working = Self.anyWorking(r ?? [])
                    self?.workingInFlight = false
                }
            }
        }
        return working
    }

    nonisolated static func anyWorking(_ agents: [LocalAPI.Agent]) -> Bool { agents.contains { $0.status == .working } }

    // MARK: - Dashboard snapshot

    func refreshSnapshot() async {
        guard supervisor.isRunning else { snapshot = .empty; snapshotError = nil; return }
        switch await readSnapshot(runnerSocket) {
        case let .success(s):
            let blocked = Set(s.needsYou.map(\.id))
            for a in s.needsYou where !lastBlocked.contains(a.id) { onNeedsYou?(a) }
            lastBlocked = blocked
            snapshot = s
            snapshotError = nil
        case let .failure(e):
            snapshotError = Self.describe(e)
        }
    }

    nonisolated static func describe(_ e: Error) -> String {
        switch e as? LocalAPI.CallError {
        case let .unavailable(why)?: return why
        case let .refused(code, message)?: return "\(code): \(message)"
        case nil: return e.localizedDescription
        }
    }

    func startSnapshots(every interval: TimeInterval = 3) {
        snapshotTimer?.invalidate()
        snapshotTimer = Timer.scheduledTimer(withTimeInterval: interval, repeats: true) { [weak self] _ in
            Task { @MainActor in await self?.refreshSnapshot() }
        }
        Task { await refreshSnapshot() }
    }

    func stopSnapshots() {
        snapshotTimer?.invalidate()
        snapshotTimer = nil
    }
}
