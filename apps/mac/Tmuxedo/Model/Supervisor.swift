import Darwin
import Foundation

/// How the runner is started: everything the supervisor hands the child.
struct LaunchSpec: Equatable {
    var executable: URL
    var arguments: [String]
    var environment: [String: String]
    /// Dup'd to fd 3 in the child (`TMUXEDO_SIGNER_FD=3`); nil = no signer (an adopted bearer token).
    var signerFD: Int32?

    /// The runner, managed by this app (docs/design/mac-pairing.md §1): it never starts or
    /// updates itself or the holder, takes its credential from the environment, and signs
    /// the gateway's challenge through fd 3.
    /// `startHolder`: false when the holder is the LaunchAgent (the owner's app); a profile
    /// (test) copy lets the runner start its own holder in the profile's runtime directory,
    /// so it never touches the owner's terminals or background item.
    static func runner(binary: URL, server: String, token: String, signerFD: Int32?, workDir: String?,
                       runtimeDir: URL?, startHolder: Bool = false, allowUser: String? = nil, observeOnly: Bool = false,
                       confirm: Bool = false, confirmTimeout: Int = 30,
                       home: String = NSHomeDirectory()) -> LaunchSpec {
        var args = ["run", "--managed"]
        if !startHolder { args.append("--no-start-holder") }
        if let workDir { args += ["--work-dir", workDir] }
        // Machine-side controls (docs/design/mac-isolation.md §3), enforced by the runner.
        if let allowUser { args += ["--allow-user", allowUser] }
        if observeOnly { args.append("--observe-only") }
        if confirm { args += ["--confirm-on-mac", "--confirm-timeout", String(confirmTimeout)] }
        var env: [String: String] = [
            "PATH": Settings.childPath,
            "HOME": home,
            "LANG": ProcessInfo.processInfo.environment["LANG"] ?? "en_US.UTF-8",
            "TMUXEDO_SERVER": server,
            "TMUXEDO_RUNNER_TOKEN": token,
            "TMUXEDO_LAUNCHED_BY": "tmuxedo.app",
        ]
        if let user = ProcessInfo.processInfo.environment["USER"] { env["USER"] = user }
        if let shell = ProcessInfo.processInfo.environment["SHELL"] { env["SHELL"] = shell }
        if signerFD != nil { env["TMUXEDO_SIGNER_FD"] = "3" }
        if let runtimeDir {
            env["TMUXEDO_RUNTIME_DIR"] = runtimeDir.appendingPathComponent("run").path
            env["TMUXEDO_STATE_DIR"] = runtimeDir.appendingPathComponent("state").path
            env["TMUXEDO_CONFIG"] = runtimeDir.appendingPathComponent("runner.toml").path
        }
        return LaunchSpec(executable: binary, arguments: args, environment: env, signerFD: signerFD)
    }
}

/// A started child.
protocol RunningProcess: AnyObject {
    var pid: pid_t { get }
    /// SIGTERM to its group, SIGKILL after `grace`.
    func terminate(grace: TimeInterval)
}

/// Starting a child; the supervisor never calls posix_spawn itself, so tests can stand in.
protocol ProcessLauncher {
    func launch(_ spec: LaunchSpec, line: @escaping (String) -> Void, exit: @escaping (Int32) -> Void) throws -> RunningProcess
}

/// posix_spawn with the runner in its own process group, every signal back to its
/// default (the app ignores SIGTERM to quit properly; an ignored disposition survives
/// exec), and ONLY stdin(/dev/null), stdout/stderr (a pipe) and the signer (fd 3)
/// crossing into it.
struct PosixLauncher: ProcessLauncher {
    func launch(_ spec: LaunchSpec, line: @escaping (String) -> Void, exit: @escaping (Int32) -> Void) throws -> RunningProcess {
        let out = Pipe()
        let devnull = open("/dev/null", O_RDONLY)
        defer { if devnull >= 0 { close(devnull) } }

        var actions: posix_spawn_file_actions_t?
        posix_spawn_file_actions_init(&actions)
        defer { posix_spawn_file_actions_destroy(&actions) }
        posix_spawn_file_actions_adddup2(&actions, devnull, 0)
        posix_spawn_file_actions_adddup2(&actions, out.fileHandleForWriting.fileDescriptor, 1)
        posix_spawn_file_actions_adddup2(&actions, out.fileHandleForWriting.fileDescriptor, 2)
        if let fd = spec.signerFD { posix_spawn_file_actions_adddup2(&actions, fd, 3) }

        var attr: posix_spawnattr_t?
        posix_spawnattr_init(&attr)
        defer { posix_spawnattr_destroy(&attr) }
        posix_spawnattr_setflags(&attr, Int16(POSIX_SPAWN_SETPGROUP | POSIX_SPAWN_CLOEXEC_DEFAULT
                                                | POSIX_SPAWN_SETSIGDEF | POSIX_SPAWN_SETSIGMASK))
        posix_spawnattr_setpgroup(&attr, 0)
        var all = sigset_t(), none = sigset_t()
        sigfillset(&all)
        sigemptyset(&none)
        posix_spawnattr_setsigdefault(&attr, &all)
        posix_spawnattr_setsigmask(&attr, &none)

        let argv = [spec.executable.path] + spec.arguments
        let envp = spec.environment.map { "\($0.key)=\($0.value)" }
        var cArgv = argv.map { strdup($0) } + [nil]
        var cEnv = envp.map { strdup($0) } + [nil]
        defer {
            cArgv.forEach { free($0) }
            cEnv.forEach { free($0) }
        }
        var pid: pid_t = 0
        let rc = posix_spawn(&pid, spec.executable.path, &actions, &attr, &cArgv, &cEnv)
        try? out.fileHandleForWriting.close()
        guard rc == 0 else {
            throw NSError(domain: NSPOSIXErrorDomain, code: Int(rc), userInfo: [
                NSLocalizedDescriptionKey: "could not start \(spec.executable.lastPathComponent): \(String(cString: strerror(rc)))",
            ])
        }
        let proc = PosixProcess(pid: pid, pipe: out)
        var pending = Data()
        let reader = out.fileHandleForReading
        let lines = DispatchGroup()
        lines.enter()
        reader.readabilityHandler = { h in
            let chunk = h.availableData
            if chunk.isEmpty {
                h.readabilityHandler = nil
                if !pending.isEmpty { line(String(decoding: pending, as: UTF8.self)) }
                lines.leave()
                return
            }
            pending.append(chunk)
            while let nl = pending.firstIndex(of: 0x0A) {
                line(String(decoding: pending[pending.startIndex..<nl], as: UTF8.self))
                pending.removeSubrange(pending.startIndex...nl)
            }
        }
        DispatchQueue.global().async {
            var status: Int32 = 0
            while waitpid(pid, &status, 0) == -1 && errno == EINTR {}
            // Every line first, then the exit.
            _ = lines.wait(timeout: .now() + 2)
            let code: Int32 = (status & 0x7f) == 0 ? (status >> 8) & 0xff : 128 + (status & 0x7f)
            proc.exited = true
            exit(code)
        }
        return proc
    }
}

final class PosixProcess: RunningProcess {
    let pid: pid_t
    private let pipe: Pipe
    var exited = false
    init(pid: pid_t, pipe: Pipe) { self.pid = pid; self.pipe = pipe }

    func terminate(grace: TimeInterval) {
        guard !exited else { return }
        killpg(pid, SIGTERM)
        let pid = self.pid
        DispatchQueue.global().asyncAfter(deadline: .now() + grace) { [weak self] in
            if self?.exited == false { killpg(pid, SIGKILL) }
        }
    }
}

/// The runner's supervisor: start, stop, restart with backoff, and what to say about it.
///
/// THE STATE MACHINE (SupervisorTests walks every edge):
///
///     off/notPaired --start--> connecting --"connected to"--> connected
///     connected --"gateway disconnected"--> connecting (attempt n+1)
///     any running --exit 0 after stop--> off
///     any running --exit 78 (4003 revoked / proof_failed, 4001 replaced)--> refused   (never retried)
///     any running --other exit--> unreachable, restart after backoff (1 s, 2 s, 4 s … 60 s);
///                                 the backoff resets once a run lasted 60 s
///     broken: the binary is missing, unsigned, too old, or the key cannot be read
@MainActor
final class Supervisor: ObservableObject {
    struct LogLine: Identifiable, Equatable {
        let id: Int
        let at: Date
        let text: String
        let kind: LogKind
    }

    @Published private(set) var display: Display = .notPaired
    @Published private(set) var log: [LogLine] = []
    @Published private(set) var signedProofs = 0
    @Published private(set) var restarts = 0
    /// The local attach log: every remote attach and decision the runner reported (newest last).
    @Published private(set) var remoteLog: [LogLine] = []
    /// The account and machine tmuxedo connected this runner as (`welcomed as machine M (user U, …)`).
    var onWelcome: ((_ machineId: String, _ userId: String) -> Void)?

    static let exitNoRestart: Int32 = 78
    static let maxLog = 2000
    static let healthyRun: TimeInterval = 60

    private let launcher: ProcessLauncher
    /// Builds the spec at each start (so a rotated token or a new key is picked up), or explains why it cannot.
    private let makeSpec: () -> Result<(LaunchSpec, SignerEndpoint?), SupervisorError>
    private let now: () -> Date
    private let schedule: (TimeInterval, @escaping @MainActor () -> Void) -> Void
    private var process: RunningProcess?
    private var signer: SignerEndpoint?
    private var stopping = false
    private var attempt = 0
    private var backoff = Backoff()
    private var startedAt: Date?
    private var nextLine = 0
    private var generation = 0
    var onLine: ((String) -> Void)?
    /// Whether the Mac is still paired when the runner stops (Unpair and the kill switch stop it).
    var isPaired: () -> Bool = { true }
    var onStateChange: ((Display) -> Void)?

    struct Backoff: Equatable {
        static let min: TimeInterval = 1
        static let max: TimeInterval = 60
        private(set) var current: TimeInterval = 0
        mutating func next() -> TimeInterval {
            current = current == 0 ? Self.min : Swift.min(current * 2, Self.max)
            return current
        }
        mutating func reset() { current = 0 }
    }

    init(launcher: ProcessLauncher = PosixLauncher(),
         now: @escaping () -> Date = Date.init,
         schedule: @escaping (TimeInterval, @escaping @MainActor () -> Void) -> Void = { delay, f in
             DispatchQueue.main.asyncAfter(deadline: .now() + delay) { MainActor.assumeIsolated { f() } }
         },
         makeSpec: @escaping () -> Result<(LaunchSpec, SignerEndpoint?), SupervisorError>) {
        self.launcher = launcher
        self.makeSpec = makeSpec
        self.now = now
        self.schedule = schedule
    }

    var isRunning: Bool { process != nil }
    var pid: pid_t? { process?.pid }

    func setIdle(paired: Bool) {
        guard process == nil else { return }
        set(paired ? .off : .notPaired)
    }

    func start() {
        guard process == nil else { return }
        stopping = false
        backoff.reset()
        attempt = 0
        launch()
    }

    func stop() {
        generation += 1 // cancels any scheduled restart
        guard let p = process else { if display.isRunning { set(.off) }; return }
        stopping = true
        set(.stopping)
        note("stopping the runner")
        p.terminate(grace: 5)
    }

    func restart() {
        if process == nil { start(); return }
        stopping = true
        note("restarting the runner")
        let p = process
        pendingRestart = true
        p?.terminate(grace: 5)
    }
    private var pendingRestart = false

    /// Called at quit: no restart, no waiting.
    func terminateNow() {
        stopping = true
        generation += 1
        process?.terminate(grace: 2)
    }

    // MARK: - Internals

    private func launch() {
        attempt += 1
        switch makeSpec() {
        case let .failure(e):
            set(e.display)
            note(e.message, error: true)
        case let .success((spec, signer)):
            self.signer = signer
            signer?.start()
            set(.connecting(attempt: attempt))
            note("starting tmuxedo-runner (attempt \(attempt))")
            let gen = generation
            do {
                // The main queue is FIFO: every line lands before the exit that follows it.
                process = try launcher.launch(spec, line: { [weak self] l in
                    DispatchQueue.main.async { MainActor.assumeIsolated { self?.runnerLine(l) } }
                }, exit: { [weak self] code in
                    DispatchQueue.main.async { MainActor.assumeIsolated { self?.exited(code, generation: gen) } }
                })
                startedAt = now()
                signer?.closeChildEnd()
            } catch {
                signer?.stop()
                self.signer = nil
                note("could not start the runner: \(error.localizedDescription)", error: true)
                scheduleRestart(after: error.localizedDescription)
            }
        }
    }

    private func runnerLine(_ raw: String) {
        let text = LogKind.stripTimestamp(raw)
        append(text, kind: LogKind.classify(text, isError: false))
        onLine?(text)
        let t = text.lowercased()
        // Connected means ADMITTED: a key-bound runner's socket opens ("connected to") before its proof,
        // and only the gateway's `welcome` says the proof (and the token) were accepted.
        if t.hasPrefix("welcomed as machine ") {
            set(.connected(since: now()))
        } else if t.hasPrefix("gateway disconnected"), display.isConnected || display.animating {
            set(.connecting(attempt: attempt))
        } else if t.contains("answered the gateway's key challenge") {
            signedProofs += 1
        } else if t.hasPrefix("remote attach:") || t.hasPrefix("remote decision") || t.hasPrefix("remote control")
                    || t.hasPrefix("refused ") {
            nextLine += 1
            remoteLog.append(LogLine(id: nextLine, at: now(), text: text, kind: LogKind.classify(text, isError: false)))
            if remoteLog.count > 500 { remoteLog.removeFirst(remoteLog.count - 500) }
        }
        if let (m, u) = Self.parseWelcome(text) { onWelcome?(m, u) }
    }

    private var lastFatal: String?

    private func exited(_ code: Int32, generation gen: Int) {
        process = nil
        signer?.stop()
        signer = nil
        let ran = startedAt.map { now().timeIntervalSince($0) } ?? 0
        startedAt = nil
        if pendingRestart {
            pendingRestart = false
            stopping = false
            restarts += 1
            note("runner exited (\(code)); starting it again")
            backoff.reset()
            attempt = 0
            launch()
            return
        }
        if stopping || gen != generation {
            stopping = false
            note("stopped")
            set(isPaired() ? .off : .notPaired)
            return
        }
        if code == Self.exitNoRestart {
            let why = refusalReason(from: log.suffix(20).map(\.text))
            note("the runner stopped for good: \(why)", error: true)
            set(.refused(why))
            return
        }
        if ran >= Self.healthyRun { backoff.reset() }
        note("the runner exited with \(code); it will restart", error: true)
        scheduleRestart(after: "the runner exited with status \(code)")
    }

    /// `welcomed as machine M (user U, web has events up to N)` → (M, U).
    nonisolated static func parseWelcome(_ line: String) -> (String, String)? {
        guard line.hasPrefix("welcomed as machine "), let open = line.range(of: " (user "), let comma = line.range(of: ",", range: open.upperBound..<line.endIndex)
        else { return nil }
        let machine = String(line[line.index(line.startIndex, offsetBy: "welcomed as machine ".count)..<open.lowerBound])
        let user = String(line[open.upperBound..<comma.lowerBound])
        return machine.isEmpty || user.isEmpty ? nil : (machine, user)
    }

    /// The runner's last words before a 78 say which refusal it was.
    func refusalReason(from lines: [String]) -> String {
        let text = lines.joined(separator: "\n").lowercased()
        if text.contains("key proof") || text.contains("proof_failed") {
            return "this Mac's key proof was not accepted."
        }
        if text.contains("serves only") {
            return "tmuxedo connected this Mac to a different account than the one it was paired with."
        }
        if text.contains("close code 4001") {
            return "another runner for this machine connected and replaced this one (is `tmuxedo-runner` still installed as a service?)."
        }
        return "its credential was revoked (unpaired on tmuxedo.dev or from another device)."
    }

    private func scheduleRestart(after error: String) {
        let wait = backoff.next()
        restarts += 1
        set(.unreachable(since: now(), error: error, retryIn: wait))
        let gen = generation
        schedule(wait) { [weak self] in
            guard let self, gen == self.generation, self.process == nil, !self.stopping else { return }
            self.launch()
        }
    }

    private func set(_ d: Display) {
        guard d != display else { return }
        display = d
        onStateChange?(d)
    }

    func note(_ text: String, error: Bool = false) {
        append(text, kind: LogKind.classify(text, isError: error))
    }

    private func append(_ text: String, kind: LogKind) {
        nextLine += 1
        log.append(LogLine(id: nextLine, at: now(), text: text, kind: kind))
        if log.count > Self.maxLog { log.removeFirst(log.count - Self.maxLog) }
    }

    func clearLog() { log.removeAll() }

    static let clock: DateFormatter = {
        let f = DateFormatter()
        f.dateFormat = "HH:mm:ss"
        return f
    }()
    var logText: String {
        log.map { "\(Self.clock.string(from: $0.at)) \($0.text)" }.joined(separator: "\n")
    }
}

enum SupervisorError: Error, Equatable {
    case notPaired
    case broken(String)

    var display: Display {
        switch self {
        case .notPaired: return .notPaired
        case let .broken(why): return .broken(why)
        }
    }

    var message: String {
        switch self {
        case .notPaired: return "cannot start: this Mac is not paired"
        case let .broken(why): return "cannot start: \(why)"
        }
    }
}
