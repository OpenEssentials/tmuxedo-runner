import Foundation

/// What the app may ask the privileged helper (`dev.tmuxedo.mac.helper`, root). Exactly these
/// operations and nothing else: no paths to write, no commands to run. The owner is never
/// taken from a request: over XPC it is the caller's uid; on the command line (root only,
/// the CI abuse tests) it is `--owner`.
struct HelperRequest: Codable, Equatable {
    enum Op: String, Codable, CaseIterable {
        /// What exists: the user, the runtime, the two daemons.
        case status
        /// Create the hidden standard user `tmuxedo` and its group (the owner joins it), the
        /// root-owned support folders and the owner's signer folder. Idempotent.
        case setup
        /// Copy the bundled runner to the root-owned bin folder (signature checked) and install /
        /// start the holder daemon; restart it only if the holder's own code changed.
        case installRuntime
        /// (Re)start the runner daemon as `tmuxedo` with `runner`.
        case startRunner
        case stopRunner
        case runnerState
        /// THE KILL SWITCH in this mode: stop runner and holder, remove both daemons (the runner's
        /// plist holds the token), end every process of `tmuxedo`.
        case stopAll
        /// Move the owner's staged state (Iso.stagingDir) into `tmuxedo`'s home (migration).
        case importState
        /// Remove everything: daemons, processes, the user, its home, its group, the support folders.
        case uninstall
    }

    var op: Op
    var runner: IsoRunnerConfig?

    init(_ op: Op, runner: IsoRunnerConfig? = nil) {
        self.op = op
        self.runner = runner
    }
}

struct DaemonState: Codable, Equatable {
    var loaded: Bool
    var running: Bool
    var pid: Int?
    var lastExit: Int?

    static let absent = DaemonState(loaded: false, running: false, pid: nil, lastExit: nil)

    /// `launchctl print system/<label>` → state. Unknown lines are ignored.
    static func parse(launchctlPrint out: String, loaded: Bool) -> DaemonState {
        guard loaded else { return .absent }
        var s = DaemonState(loaded: true, running: false, pid: nil, lastExit: nil)
        for raw in out.split(separator: "\n") {
            let line = raw.trimmingCharacters(in: .whitespaces)
            func value(_ key: String) -> String? {
                line.hasPrefix(key + " = ") ? String(line.dropFirst(key.count + 3)) : nil
            }
            if let v = value("state") { s.running = v == "running" }
            if let v = value("pid"), let p = Int(v) { s.pid = p }
            // `78: EX_CONFIG`, `1`, or `(never exited)`.
            if let v = value("last exit code"), let c = Int(String(v.prefix { $0 == "-" || $0.isNumber })) { s.lastExit = c }
        }
        if !s.running { s.pid = nil }
        return s
    }
}

struct IsoStatus: Codable, Equatable {
    var userExists: Bool
    var uid: Int?
    var gid: Int?
    var isAdmin: Bool
    var ownerInGroup: Bool
    var runtimeInstalled: Bool
    var holder: DaemonState
    var runner: DaemonState

    static let none = IsoStatus(userExists: false, uid: nil, gid: nil, isAdmin: false, ownerInGroup: false,
                                runtimeInstalled: false, holder: .absent, runner: .absent)

    /// Ready to start a runner.
    var ready: Bool { userExists && !isAdmin && ownerInGroup && runtimeInstalled }
}

struct HelperResponse: Codable, Equatable {
    var ok: Bool
    var error: String?
    var status: IsoStatus?
    var daemon: DaemonState?
    /// What was done, step by step (shown in the app's log).
    var steps: [String] = []

    static func fail(_ why: String, steps: [String] = []) -> HelperResponse {
        HelperResponse(ok: false, error: why, status: nil, daemon: nil, steps: steps)
    }
}

/// The XPC surface: one call, JSON both ways (Codable above), so the interface never changes shape.
@objc(TmuxedoHelperXPC) protocol TmuxedoHelperXPC {
    func handle(_ request: Data, withReply reply: @escaping (Data) -> Void)
}

/// Talking to the helper. Synchronous; never call it on the main thread.
protocol HelperClient: AnyObject {
    func call(_ request: HelperRequest) -> HelperResponse
}

/// The real one: NSXPCConnection to the helper's Mach service, the helper's code signature
/// checked (a signed app only talks to a helper signed by the same team).
final class XPCHelperClient: HelperClient {
    let requirement: String?
    let timeout: TimeInterval

    init(requirement: String?, timeout: TimeInterval = 120) {
        self.requirement = requirement
        self.timeout = timeout
    }

    func call(_ request: HelperRequest) -> HelperResponse {
        guard let body = try? JSONEncoder().encode(request) else { return .fail("could not encode the request") }
        let c = NSXPCConnection(machServiceName: Iso.helperLabel, options: .privileged)
        c.remoteObjectInterface = NSXPCInterface(with: TmuxedoHelperXPC.self)
        if let requirement { c.setCodeSigningRequirement(requirement) }
        c.resume()
        defer { c.invalidate() }
        let box = ResultBox()
        let done = DispatchSemaphore(value: 0)
        let proxy = c.remoteObjectProxyWithErrorHandler { error in
            box.set(.fail("the helper is not reachable: \(error.localizedDescription)"))
            done.signal()
        } as? TmuxedoHelperXPC
        guard let proxy else { return .fail("the helper's interface is not the expected one") }
        proxy.handle(body) { data in
            box.set((try? JSONDecoder().decode(HelperResponse.self, from: data)) ?? .fail("the helper answered junk"))
            done.signal()
        }
        if done.wait(timeout: .now() + timeout) == .timedOut { return .fail("the helper did not answer in time") }
        return box.get()
    }

    private final class ResultBox: @unchecked Sendable {
        private let lock = NSLock()
        private var value: HelperResponse = .fail("the helper did not answer")
        func set(_ v: HelperResponse) { lock.lock(); value = v; lock.unlock() }
        func get() -> HelperResponse { lock.lock(); defer { lock.unlock() }; return value }
    }
}
