import Foundation
import ServiceManagement

/// What this Mac is paired with, and how it behaves.
///
/// The runner token and the machine key are in the Keychain (Keychain.swift,
/// MachineKey.swift), never here.
@MainActor
final class Settings: ObservableObject {
    /// The tmuxedo server. Changing it means pairing again.
    @Published var serverURL: String {
        didSet { defaults.set(serverURL, forKey: Keys.serverURL) }
    }
    /// The folder new terminals start in (the runner's `--work-dir`; a value set on
    /// tmuxedo.dev for this machine wins). `~` is allowed. Empty = the runner's default, `~/work`.
    @Published var workDir: String {
        didSet { defaults.set(workDir, forKey: Keys.workDir) }
    }
    /// Start the runner when the app launches (on by default once paired:
    /// a paired Mac that is not reachable after a reboot surprises people).
    @Published var connectAtLaunch: Bool {
        didSet { defaults.set(connectAtLaunch, forKey: Keys.connectAtLaunch) }
    }
    /// Hold off idle sleep while connected. See SleepGuard.
    @Published var keepAwake: Bool {
        didSet { defaults.set(keepAwake, forKey: Keys.keepAwake) }
    }
    /// Native notifications: needs-you, pairing requests, new machines.
    @Published var notifications: Bool {
        didSet { defaults.set(notifications, forKey: Keys.notifications) }
    }
    /// Observe-only: remote viewers may watch terminals but not type, resize or decide (enforced by the runner).
    @Published var observeOnly: Bool {
        didSet { defaults.set(observeOnly, forKey: Keys.observeOnly) }
    }
    /// The onboarding sheet (background items) was shown and dismissed.
    @Published var onboarded: Bool {
        didSet { defaults.set(onboarded, forKey: Keys.onboarded) }
    }
    /// Where terminals run (docs/design/mac-isolation.md §2). Decided once at first launch:
    /// separate user for a new install, full access for one that existed before (moved only on purpose).
    @Published var isolation: IsolationMode {
        didSet { defaults.set(isolation.rawValue, forKey: Keys.isolation) }
    }
    /// Ask on this Mac (a notification with Allow / Deny) before remote control; a timeout denies.
    @Published var confirmOnMac: Bool {
        didSet { defaults.set(confirmOnMac, forKey: Keys.confirmOnMac) }
    }
    /// Seconds the question stays open (15, 30, 60 or 120).
    @Published var confirmTimeout: Int {
        didSet { defaults.set(confirmTimeout, forKey: Keys.confirmTimeout) }
    }
    /// The folders granted to the separate user, and what the home lock wrote (FolderGrants).
    @Published var grantState: GrantState {
        didSet { if let d = try? JSONEncoder().encode(grantState) { defaults.set(d, forKey: Keys.grantState) } }
    }
    @Published private(set) var machine: PairedMachine? {
        didSet {
            if let machine, let data = try? JSONEncoder().encode(machine) { defaults.set(data, forKey: Keys.machine) }
            else { defaults.removeObject(forKey: Keys.machine) }
        }
    }

    let defaults: UserDefaults
    let secrets: SecretStore

    private enum Keys {
        static let serverURL = "serverURL"
        static let workDir = "workDir"
        static let connectAtLaunch = "connectAtLaunch"
        static let keepAwake = "keepAwake"
        static let notifications = "notifications"
        static let onboarded = "onboarded"
        static let observeOnly = "observeOnly"
        static let machine = "pairedMachine"
        static let isolation = "isolationMode"
        static let confirmOnMac = "confirmOnMac"
        static let confirmTimeout = "confirmTimeout"
        static let grantState = "isolationGrants"
    }

    static let confirmTimeouts = [15, 30, 60, 120]

    static let defaultServer = "https://tmuxedo.dev"

    init(defaults: UserDefaults = Profile.defaults(for: Profile.name), secrets: SecretStore = KeychainStore()) {
        self.defaults = defaults
        self.secrets = secrets
        serverURL = defaults.string(forKey: Keys.serverURL) ?? Self.defaultServer
        workDir = defaults.string(forKey: Keys.workDir) ?? ""
        connectAtLaunch = defaults.object(forKey: Keys.connectAtLaunch) as? Bool ?? true
        // OFF BY DEFAULT: keeping a laptop awake is the owner's decision.
        keepAwake = defaults.object(forKey: Keys.keepAwake) as? Bool ?? false
        notifications = defaults.object(forKey: Keys.notifications) as? Bool ?? true
        onboarded = defaults.bool(forKey: Keys.onboarded)
        observeOnly = defaults.bool(forKey: Keys.observeOnly)
        machine = defaults.data(forKey: Keys.machine).flatMap { try? JSONDecoder().decode(PairedMachine.self, from: $0) }
        let existing = defaults.object(forKey: Keys.onboarded) != nil || defaults.data(forKey: Keys.machine) != nil
        let mode = defaults.string(forKey: Keys.isolation).flatMap(IsolationMode.init(rawValue:))
            ?? IsolationMode.initial(existingInstall: existing)
        isolation = mode
        // Decided once: a new install that later onboards must not turn into an "existing" one.
        defaults.set(mode.rawValue, forKey: Keys.isolation)
        confirmOnMac = defaults.bool(forKey: Keys.confirmOnMac)
        let t = defaults.integer(forKey: Keys.confirmTimeout)
        confirmTimeout = Self.confirmTimeouts.contains(t) ? t : 30
        grantState = defaults.data(forKey: Keys.grantState).flatMap { try? JSONDecoder().decode(GrantState.self, from: $0) }
            ?? GrantState()
    }

    // MARK: - Pairing state

    var hasToken: Bool { secrets.has(SecretItem.runnerToken) }
    func runnerToken() -> String? { secrets.readString(SecretItem.runnerToken) }

    /// Paired = a token and the record of what it is for.
    var isPaired: Bool { machine != nil && hasToken }

    /// The account this Mac serves, learned from the first connection after pairing; from then on the
    /// runner refuses any other (`--allow-user`), whatever tmuxedo's servers say.
    func learnAccount(machineId: String, userId: String) {
        guard var m = machine, m.userId == nil else { return }
        m.userId = userId
        if m.machineId.isEmpty { m.machineId = machineId }
        machine = m
    }

    func storePairing(_ m: PairedMachine, token: String) {
        secrets.writeString(SecretItem.runnerToken, token)
        machine = m
    }

    /// Forget the pairing on this Mac (the key is deleted by the caller, which owns it).
    func forgetPairing() {
        secrets.delete(SecretItem.runnerToken)
        machine = nil
    }

    // MARK: - Derived

    var server: URL? {
        let trimmed = serverURL.trimmingCharacters(in: .whitespacesAndNewlines)
        guard let url = URL(string: trimmed), let scheme = url.scheme?.lowercased(), url.host != nil,
              scheme == "https" || (scheme == "http" && Self.isLoopback(url.host!))
        else { return nil }
        return url
    }

    /// Plain http only to this Mac itself (tests and the mock host); anything else must be https.
    nonisolated static func isLoopback(_ host: String) -> Bool {
        host == "127.0.0.1" || host == "localhost" || host == "::1"
    }

    /// The work folder as an absolute path, or nil for the runner's default.
    var resolvedWorkDir: String? {
        let t = workDir.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !t.isEmpty else { return nil }
        let expanded = (t as NSString).expandingTildeInPath
        return expanded.hasPrefix("/") ? expanded : nil
    }

    /// A link into tmuxedo for this machine (`/machines/<id>`), or the site.
    func webURL(_ path: String = "") -> URL? {
        guard let server else { return nil }
        return URL(string: path, relativeTo: server)?.absoluteURL
    }

    /// PATH for the runner. A Finder-launched app inherits almost no PATH, and
    /// the runner's panes and the `claude` it starts need the user's usual one.
    nonisolated static var childPath: String {
        let home = FileManager.default.homeDirectoryForCurrentUser.path
        let existing = ProcessInfo.processInfo.environment["PATH"] ?? ""
        var seen = Set<String>()
        return ([
            "\(home)/.local/bin",
            "/opt/homebrew/bin",
            "/usr/local/bin",
            "/usr/bin", "/bin", "/usr/sbin", "/sbin",
        ] + existing.split(separator: ":").map(String.init))
            .filter { !$0.isEmpty && seen.insert($0).inserted }
            .joined(separator: ":")
    }
}

/// The machine this Mac is paired as.
struct PairedMachine: Codable, Equatable {
    var machineId: String
    var name: String
    var server: String
    /// How the pairing was approved (`apple+passkey`, `phone`, `browser`, … or `cli` for an adopted CLI enrolment).
    var method: String
    var pairedAt: Date
    /// The token is bound to this Mac's key (false only for an adopted bearer token from `tmuxedo-runner enrol`).
    var keyBound: Bool
    var keyFingerprint: String?
    /// The account (user id) this Mac serves; nil until the first connection.
    var userId: String?
}

/// Registering the app to open at login. `SMAppService.mainApp`: visible in
/// System Settings, leaves nothing behind.
@MainActor
enum LoginItem {
    static var isEnabled: Bool { SMAppService.mainApp.status == .enabled }

    /// nil on success, or a sentence explaining the refusal. Reported, not assumed:
    /// a toggle that silently snapped back would be the interface lying.
    static func set(_ enabled: Bool) -> String? {
        do {
            if enabled { try SMAppService.mainApp.register() }
            else { try SMAppService.mainApp.unregister() }
            return nil
        } catch {
            if SMAppService.mainApp.status == .requiresApproval {
                return "macOS needs this approved in System Settings, General, Login Items. "
                    + "The app cannot grant it for you."
            }
            return error.localizedDescription
        }
    }
}

