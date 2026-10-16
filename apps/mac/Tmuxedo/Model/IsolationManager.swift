import Foundation
import ServiceManagement

/// The privileged helper's registration (SMAppService.daemon: one administrator approval in
/// System Settings, Login Items). Tests use FakeRegistry.
final class SMDaemonRegistry: AgentRegistry {
    private let service = SMAppService.daemon(plistName: Iso.helperPlist)
    var status: SMAppService.Status { service.status }
    func register() throws { try service.register() }
    func unregister() throws { try service.unregister() }
}

/// The separate-user mode, as the app drives it: the helper (root, a few fixed operations),
/// the owner's folder grants and home lock (as the owner, FolderGrants), and the explicit
/// move of an existing full-access install (migration).
@MainActor
final class IsolationManager: ObservableObject {
    enum Phase: Equatable {
        case unknown
        /// The helper is not registered yet (nothing is set up).
        case notSetUp
        /// macOS waits for an administrator to allow the helper (System Settings, Login Items).
        case needsApproval
        /// The user and the runtime exist.
        case ready
        case failed(String)
    }

    @Published private(set) var phase: Phase = .unknown
    @Published private(set) var status: IsoStatus = .none
    @Published private(set) var busy: String?
    @Published private(set) var lastError: String?

    let settings: Settings
    let registry: AgentRegistry
    let helper: HelperClient
    var note: (String) -> Void = { _ in }
    /// The owner's identity and home (tests point these elsewhere).
    var ownerHome: String = NSHomeDirectory()
    var ownerName: String = NSUserName()
    /// Directory UUIDs (the ACL qualifiers); injectable for tests.
    var uuidOf: (String) -> uuid_t? = { FolderAccess.uuid(ofUser: $0) }
    /// Where the owner's full-access runner keeps its state (migration source).
    var ownerStateDir: String { ownerHome + "/.local/state/tmuxedo" }
    var claudeProjects: String { ownerHome + "/.claude/projects" }
    var stagingDir: String = Iso.stagingDir

    init(settings: Settings, registry: AgentRegistry, helper: HelperClient) {
        self.settings = settings
        self.registry = registry
        self.helper = helper
    }

    /// The helper, off the main thread.
    func call(_ r: HelperRequest) async -> HelperResponse {
        let h = helper
        let resp = await Task.detached { h.call(r) }.value
        for s in resp.steps { note("separate user: \(s)") }
        return resp
    }

    func refresh() async {
        switch registry.status {
        case .requiresApproval: phase = .needsApproval; return
        case .notRegistered, .notFound: phase = .notSetUp; return
        default: break
        }
        let r = await call(HelperRequest(.status))
        if let s = r.status {
            status = s
            phase = s.ready ? .ready : .notSetUp
        } else {
            phase = .failed(r.error ?? "the helper did not answer")
        }
    }

    // MARK: - Setting up

    /// Register the helper (macOS may ask for approval), then create the user, install the
    /// runtime and lock the home. Returns true when the separate user is ready.
    @discardableResult
    func setUp() async -> Bool {
        busy = "Setting up the separate user…"
        defer { busy = nil }
        lastError = nil
        if registry.status != .enabled {
            do { try registry.register() } catch {
                if registry.status != .requiresApproval {
                    return failed("macOS refused tmuxedo's helper: \(error.localizedDescription)")
                }
            }
            if registry.status == .requiresApproval {
                phase = .needsApproval
                lastError = "Allow “tmuxedo” in System Settings, General, Login Items (an administrator's approval), then try again."
                return false
            }
        }
        for op in [HelperRequest.Op.setup, .installRuntime] {
            let r = await call(HelperRequest(op))
            guard r.ok else { return failed(r.error ?? "\(op.rawValue) failed") }
            if let s = r.status { status = s }
        }
        do {
            var g = try grants()
            try g.lock()
            settings.grantState = g.state
        } catch {
            return failed("could not lock your home folder against the terminals: \(error.localizedDescription)")
        }
        phase = status.ready ? .ready : .notSetUp
        note("separate user ready: terminals run as \(Iso.user); your home is closed to them")
        return status.ready
    }

    private func failed(_ why: String) -> Bool {
        lastError = why
        phase = .failed(why)
        note("separate user: \(why)")
        return false
    }

    // MARK: - Folders

    func grants() throws -> FolderGrants {
        guard let user = uuidOf(Iso.user) else { throw FolderAccess.Failure.refused("the user \(Iso.user) does not exist yet") }
        guard let owner = uuidOf(ownerName) else { throw FolderAccess.Failure.refused("cannot find your account's directory id") }
        return FolderGrants(home: ownerHome, user: user, owner: owner, state: settings.grantState,
                            say: { [weak self] s in Task { @MainActor in self?.note(s) } })
    }

    func grant(_ path: String) async -> Bool {
        busy = "Granting \(path)…"
        defer { busy = nil }
        do {
            let g = try grants()
            let result: Result<GrantState, Error> = await Task.detached {
                var w = g
                do { try w.grant(path); return .success(w.state) } catch { return .failure(error) }
            }.value
            switch result {
            case let .success(s): settings.grantState = s; lastError = nil; return true
            case let .failure(e): lastError = e.localizedDescription; return false
            }
        } catch {
            lastError = error.localizedDescription
            return false
        }
    }

    func revoke(_ path: String) async {
        busy = "Revoking \(path)…"
        defer { busy = nil }
        do {
            let g = try grants()
            let result: Result<GrantState, Error> = await Task.detached {
                var w = g
                do { try w.revoke(path); return .success(w.state) } catch { return .failure(error) }
            }.value
            switch result {
            case let .success(s): settings.grantState = s; lastError = nil
            case let .failure(e): lastError = e.localizedDescription
            }
        } catch {
            lastError = error.localizedDescription
        }
    }

    // MARK: - Stopping, removing

    /// The kill switch's part: every terminal of the separate user ends, the daemons are removed.
    func stopAll() async {
        guard registry.status == .enabled else { return }
        _ = await call(HelperRequest(.stopAll))
    }

    /// Remove the separate user entirely: its daemons, processes, home and group, the home lock,
    /// every grant, and the helper's registration. Leaves this Mac in full access.
    func uninstall() async -> Bool {
        busy = "Removing the separate user…"
        defer { busy = nil }
        if registry.status == .enabled {
            let r = await call(HelperRequest(.uninstall))
            guard r.ok else { return failed(r.error ?? "uninstall failed") }
        }
        if var g = try? grants() {
            g.removeAll()
        }
        settings.grantState = GrantState()
        try? registry.unregister()
        settings.isolation = .fullAccess
        status = .none
        phase = .notSetUp
        note("separate user removed: user, home, daemons, grants and the home lock are gone")
        return true
    }

    // MARK: - Migration from full access

    /// Claude's project folder name for a path (every character that is not a letter or digit becomes `-`).
    nonisolated static func claudeSlug(_ path: String) -> String {
        String(path.map { $0.isASCII && ($0.isLetter || $0.isNumber) ? $0 : "-" })
    }

    /// Stage the owner's runner state and the Claude transcripts of granted folders for the
    /// helper (owner-owned, group `tmuxedo` readable). Returns how many files were staged.
    func stage() throws -> Int {
        let fm = FileManager.default
        try? fm.removeItem(atPath: stagingDir)
        try fm.createDirectory(atPath: stagingDir + "/state", withIntermediateDirectories: true)
        var n = 0
        for name in (try? fm.contentsOfDirectory(atPath: ownerStateDir)) ?? [] {
            let src = ownerStateDir + "/" + name
            var st = stat()
            guard lstat(src, &st) == 0, st.st_mode & S_IFMT == S_IFREG, !name.hasSuffix(".sock"), !name.hasSuffix(".log")
            else { continue }
            try fm.copyItem(atPath: src, toPath: stagingDir + "/state/" + name)
            n += 1
        }
        let slugs = settings.grantState.grants.map(Self.claudeSlug)
        if !slugs.isEmpty {
            for name in (try? fm.contentsOfDirectory(atPath: claudeProjects)) ?? [] where slugs.contains(where: { name.hasPrefix($0) }) {
                try fm.createDirectory(atPath: stagingDir + "/claude-projects", withIntermediateDirectories: true)
                try fm.copyItem(atPath: claudeProjects + "/" + name, toPath: stagingDir + "/claude-projects/" + name)
                n += 1
            }
        }
        _ = Shell.run("/bin/chmod", ["-R", "g+rX", stagingDir])
        return n
    }
}
