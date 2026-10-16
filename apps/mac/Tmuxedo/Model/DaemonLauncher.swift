import Darwin
import Foundation

/// Follows a log file: new complete lines since the last read. Survives truncation and
/// replacement (a reinstalled daemon, a rotated log).
final class LogTail {
    let path: String
    private var offset: UInt64
    private var inode: UInt64 = 0
    private var pending = Data()

    /// `fromEnd`: skip what is already there (the previous run's lines).
    init(path: String, fromEnd: Bool = true) {
        self.path = path
        var st = stat()
        if stat(path, &st) == 0 {
            offset = fromEnd ? UInt64(st.st_size) : 0
            inode = UInt64(st.st_ino)
        } else {
            offset = 0
        }
    }

    func read() -> [String] {
        var st = stat()
        guard stat(path, &st) == 0 else { return [] }
        if UInt64(st.st_ino) != inode || UInt64(st.st_size) < offset {
            inode = UInt64(st.st_ino)
            offset = 0
            pending.removeAll()
        }
        guard UInt64(st.st_size) > offset, let h = FileHandle(forReadingAtPath: path) else { return [] }
        defer { try? h.close() }
        do { try h.seek(toOffset: offset) } catch { return [] }
        let data = h.readData(ofLength: min(Int(UInt64(st.st_size) - offset), 4 << 20))
        offset += UInt64(data.count)
        pending.append(data)
        var lines: [String] = []
        while let nl = pending.firstIndex(of: 0x0A) {
            lines.append(String(decoding: pending[pending.startIndex..<nl], as: UTF8.self))
            pending.removeSubrange(pending.startIndex...nl)
        }
        if pending.count > 1 << 20 { pending.removeAll() }
        return lines
    }
}

/// Starts the runner as the separate user's LaunchDaemon, through the helper, and makes it look
/// like a child to the Supervisor: its log (written by launchd, root-owned, so a terminal cannot
/// forge a line) is followed for the lines, and launchd's state for the exit. So the
/// supervisor's whole state machine (backoff, refusals, restart) is the same in both modes.
final class DaemonLauncher: ProcessLauncher {
    let helper: HelperClient
    /// The runner's settings for the next start (token, account, guardrails).
    let config: () -> IsoRunnerConfig?
    let logPath: String
    let pollInterval: TimeInterval
    private let lock = NSLock()
    private var knownPid: pid_t?

    init(helper: HelperClient, logPath: String = Iso.runnerLog, pollInterval: TimeInterval = 1,
         config: @escaping () -> IsoRunnerConfig?) {
        self.helper = helper
        self.config = config
        self.logPath = logPath
        self.pollInterval = pollInterval
    }

    /// The runner daemon's pid as far as launchd knows (SignerAdmission). `refresh` asks the helper.
    func runnerPid(refresh: Bool) -> pid_t? {
        if refresh {
            let r = helper.call(HelperRequest(.runnerState))
            let p = r.daemon?.running == true ? r.daemon?.pid.map { pid_t($0) } : nil
            lock.lock(); knownPid = p; lock.unlock()
            return p
        }
        lock.lock(); defer { lock.unlock() }
        return knownPid
    }

    func launch(_ spec: LaunchSpec, line: @escaping (String) -> Void, exit: @escaping (Int32) -> Void) throws -> RunningProcess {
        guard let cfg = config() else {
            throw NSError(domain: "tmuxedo", code: 1, userInfo: [NSLocalizedDescriptionKey: "not paired"])
        }
        let tail = LogTail(path: logPath, fromEnd: true)
        let r = helper.call(HelperRequest(.startRunner, runner: cfg))
        guard r.ok, let d = r.daemon else {
            throw NSError(domain: "tmuxedo", code: 2, userInfo: [
                NSLocalizedDescriptionKey: "the helper could not start the runner: \(r.error ?? "unknown error")",
            ])
        }
        let pid = pid_t(d.pid ?? 0)
        lock.lock(); knownPid = d.running ? pid : nil; lock.unlock()
        let proc = DaemonProcess(pid: pid, helper: helper)
        proc.watch(tail: tail, every: pollInterval, line: line, exit: { [weak self] code in
            self?.lock.lock(); self?.knownPid = nil; self?.lock.unlock()
            exit(code)
        })
        return proc
    }
}

/// The app's child (full access) or the separate user's daemon, as the mode says at each start.
final class SwitchingLauncher: ProcessLauncher {
    let child: ProcessLauncher
    let daemon: ProcessLauncher?
    let mode: () -> IsolationMode

    init(child: ProcessLauncher, daemon: ProcessLauncher?, mode: @escaping () -> IsolationMode) {
        self.child = child
        self.daemon = daemon
        self.mode = mode
    }

    func launch(_ spec: LaunchSpec, line: @escaping (String) -> Void, exit: @escaping (Int32) -> Void) throws -> RunningProcess {
        guard mode() == .separateUser else { return try child.launch(spec, line: line, exit: exit) }
        guard let daemon else {
            throw NSError(domain: "tmuxedo", code: 3, userInfo: [NSLocalizedDescriptionKey: "separate-user mode is not available here"])
        }
        return try daemon.launch(spec, line: line, exit: exit)
    }
}

/// The runner daemon, seen as a process.
final class DaemonProcess: RunningProcess {
    let pid: pid_t
    private let helper: HelperClient
    private let lock = NSLock()
    private var stopRequested = false
    private var finished = false
    private var timer: DispatchSourceTimer?

    init(pid: pid_t, helper: HelperClient) {
        self.pid = pid
        self.helper = helper
    }

    func watch(tail: LogTail, every: TimeInterval, line: @escaping (String) -> Void, exit: @escaping (Int32) -> Void) {
        let q = DispatchQueue(label: "tmuxedo.daemon-watch")
        let t = DispatchSource.makeTimerSource(queue: q)
        t.schedule(deadline: .now() + every, repeating: every)
        t.setEventHandler { [weak self] in
            guard let self else { return }
            for l in tail.read() { line(l) }
            let s = self.helper.call(HelperRequest(.runnerState)).daemon ?? .absent
            if s.running, s.pid == Int(self.pid) { return }
            // Gone: its last lines first, then the exit.
            for l in tail.read() { line(l) }
            self.lock.lock()
            let stopped = self.stopRequested
            let already = self.finished
            self.finished = true
            self.lock.unlock()
            guard !already else { return }
            self.timer?.cancel()
            exit(stopped ? 0 : Int32(s.lastExit ?? 1))
        }
        timer = t
        t.resume()
    }

    /// Stop the daemon (launchd sends SIGTERM, then SIGKILL after its exit timeout).
    func terminate(grace: TimeInterval) {
        lock.lock()
        stopRequested = true
        lock.unlock()
        let helper = self.helper
        if grace <= 2 {
            // Quitting (terminateNow): stop it before the app is gone; the runner never outlives the app.
            _ = helper.call(HelperRequest(.stopRunner))
        } else {
            DispatchQueue.global().async { _ = helper.call(HelperRequest(.stopRunner)) }
        }
    }
}
