import Foundation
import ServiceManagement

/// The PTY holder: the process that owns every terminal on this Mac.
///
/// It runs as ITS OWN LaunchAgent (`dev.tmuxedo.mac.holder`, a plist inside
/// the app bundle registered with `SMAppService.agent`), not as a child of the
/// app. So quitting the app, an app update and every runner restart leave the
/// terminals alone; only logout, a reboot, or a holder restart end them
/// (docs/design/mac-pairing.md §1).
///
/// The holder is restarted ONLY when its code changed: the app compares the
/// running holder's build fingerprint with the bundled binary's
/// (`HolderPolicy`). An ordinary update changes the runner and the app, not
/// the holder, and keeps every terminal. When the holder itself changed, the
/// app restarts it once; Claude sessions come back through the runner's resume.
enum HolderPolicy {
    enum Decision: Equatable {
        /// Nothing running: register / start it.
        case start
        /// The running holder is this build's: leave it alone.
        case keep
        /// A holder older than build fingerprints (an install.sh or pre-fingerprint holder):
        /// leave it alone; its sessions last until it exits, then ours takes over (`holder --wait`).
        case keepLegacy
        /// The holder's code changed: restart it (ends terminals; agents resume).
        case restart(from: String, to: String)
    }

    static func decide(running: RunnerBinary.HolderInfo?, bundledBuild: String) -> Decision {
        guard let running, running.running else { return .start }
        guard let build = running.build, !build.isEmpty else { return .keepLegacy }
        return build == bundledBuild ? .keep : .restart(from: build, to: bundledBuild)
    }
}

/// Registering the holder LaunchAgent. The real one is SMAppService; tests use a fake.
protocol AgentRegistry: AnyObject {
    var status: SMAppService.Status { get }
    func register() throws
    func unregister() throws
}

final class SMAgentRegistry: AgentRegistry {
    static let plistName = "dev.tmuxedo.mac.holder.plist"
    static let label = "dev.tmuxedo.mac.holder"
    private let service = SMAppService.agent(plistName: SMAgentRegistry.plistName)
    var status: SMAppService.Status { service.status }
    func register() throws { try service.register() }
    func unregister() throws { try service.unregister() }
}

/// What the holder looks like to the person, in one sentence.
enum HolderState: Equatable {
    case running(build: String?, pid: Int?)
    case notRunning
    /// macOS needs the person to allow it (System Settings, General, Login Items).
    case needsApproval
    case failed(String)

    var sentence: String {
        switch self {
        case let .running(build, pid):
            return "Terminal holder running" + (pid.map { " (pid \($0))" } ?? "") + (build.map { ", build \($0)" } ?? "")
        case .notRunning: return "Terminal holder not running"
        case .needsApproval:
            return "macOS is waiting for you to allow tmuxedo's background item: System Settings, General, Login Items."
        case let .failed(why): return "Terminal holder could not start: \(why)"
        }
    }
}

@MainActor
final class HolderService: ObservableObject {
    @Published private(set) var state: HolderState = .notRunning
    @Published private(set) var lastDecision: HolderPolicy.Decision?

    private let registry: AgentRegistry
    private let binary: RunnerBinary?
    /// nil = the default socket (the owner's); a profile's own otherwise.
    let socket: URL?
    private let log: (String) -> Void
    /// launchctl, injectable for tests: (args) -> exit status.
    var launchctl: ([String]) -> Int32 = { args in Shell.run("/bin/launchctl", args)?.0 ?? -1 }

    init(registry: AgentRegistry, binary: RunnerBinary?, socket: URL?, log: @escaping (String) -> Void) {
        self.registry = registry
        self.binary = binary
        self.socket = socket
        self.log = log
    }

    static var guiDomain: String { "gui/\(getuid())" }

    /// Make sure the right holder serves this Mac's terminals. Returns the decision taken.
    @discardableResult
    func ensure(bundledBuild: String) -> HolderPolicy.Decision {
        let info = binary?.holderInfo(socket: socket)
        let d = HolderPolicy.decide(running: info, bundledBuild: bundledBuild)
        lastDecision = d
        switch d {
        case .keep:
            state = .running(build: info?.build, pid: info?.pid)
            ensureRegistered()
        case .keepLegacy:
            state = .running(build: nil, pid: info?.pid)
            log("holder: an older holder (pid \(info?.pid.map(String.init) ?? "?")) keeps your current terminals; "
                + "tmuxedo's own holder takes over when it exits")
            ensureRegistered()
        case .start:
            ensureRegistered()
            _ = launchctl(["kickstart", "\(Self.guiDomain)/\(SMAgentRegistry.label)"])
            refresh()
        case let .restart(from, to):
            log("holder restarts: its code changed (\(from) to \(to)); terminals end and Claude sessions resume")
            ensureRegistered()
            _ = launchctl(["kickstart", "-k", "\(Self.guiDomain)/\(SMAgentRegistry.label)"])
            refresh()
        }
        return d
    }

    private func ensureRegistered() {
        switch registry.status {
        case .enabled: return
        case .requiresApproval:
            state = .needsApproval
            return
        default:
            do {
                try registry.register()
                if registry.status == .requiresApproval { state = .needsApproval }
            } catch {
                state = registry.status == .requiresApproval ? .needsApproval : .failed(error.localizedDescription)
                log("holder: could not register its background item: \(error.localizedDescription)")
            }
        }
    }

    func refresh() {
        if registry.status == .requiresApproval { state = .needsApproval; return }
        let info = binary?.holderInfo(socket: socket)
        state = info?.running == true ? .running(build: info?.build, pid: info?.pid) : .notRunning
    }

    /// The kill switch and Unpair: stop the holder (ENDS every terminal) and keep it from starting again.
    func stopAndUnregister() {
        _ = launchctl(["bootout", "\(Self.guiDomain)/\(SMAgentRegistry.label)"])
        try? registry.unregister()
        state = .notRunning
    }
}
