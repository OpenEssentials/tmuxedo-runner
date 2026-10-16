import Foundation

/// Where this Mac's terminals run (docs/design/mac-isolation.md §2).
///
///   separateUser  DEFAULT for new installs: the holder and every terminal run as the hidden,
///                 standard (non-admin) macOS user `tmuxedo`, which cannot read your home,
///                 your Keychain or `~/.ssh`, cannot sudo, and reaches only the folders you grant.
///   fullAccess    opt-in, behind a warning: terminals run as YOU, with everything you can do
///                 (Xcode builds, your credentials). This is how every install before
///                 separate-user mode worked, and existing installs stay on it until moved.
///
/// The Linux VM mode is designed (mac-isolation.md §7) and not built: it is not offered here.
enum IsolationMode: String, Codable, CaseIterable, Identifiable {
    case separateUser
    case fullAccess

    var id: String { rawValue }

    var title: String {
        switch self {
        case .separateUser: return "Separate user (recommended)"
        case .fullAccess: return "Full access"
        }
    }

    var summary: String {
        switch self {
        case .separateUser:
            return "Terminals run as a separate, standard macOS user (“tmuxedo”). They cannot read your home folder, "
                + "your Keychain or ~/.ssh, cannot use sudo, and reach only the folders you grant."
        case .fullAccess:
            return "Terminals run as you: anything typed there, by you, an agent or anyone who takes control remotely, "
                + "can read your files and credentials and change anything you can."
        }
    }

    /// What a new install starts in. An install that existed before separate-user mode (it has
    /// settings already) keeps full access until the owner moves it, explicitly.
    static func initial(existingInstall: Bool) -> IsolationMode {
        existingInstall ? .fullAccess : .separateUser
    }
}

/// Names and places of the separate-user mode. Shared by the app, the privileged helper and
/// the tests, so all three agree; nothing here is configurable at runtime.
enum Iso {
    /// The hidden standard user the terminals run as, and its own primary group (which the
    /// owner joins, so the owner's app can reach the runner's dashboard socket).
    static let user = "tmuxedo"
    static let group = "tmuxedo"
    static let realName = "tmuxedo terminals"
    static let home = "/Users/tmuxedo"
    /// UIDs / GIDs are picked from here (hidden by IsHidden, not by the range).
    static let idRange = 601...699

    /// Root-owned: the runner copy the daemons execute (nothing `tmuxedo` can write is ever run by root or the owner).
    static let supportDir = "/Library/Application Support/tmuxedo"
    static let binDir = supportDir + "/bin"
    static let runnerPath = binDir + "/tmuxedo-runner"
    /// Owned by the owner, group `tmuxedo`, 0750: the app's signer socket for the isolated runner.
    static let signerDir = supportDir + "/signer"
    static let signerSocket = signerDir + "/signer.sock"
    /// Owner-owned staging for a migration (state the app copies; the helper moves it into tmuxedo's home).
    static let stagingDir = signerDir + "/staging"
    /// Root-owned logs written by launchd: terminals (uid tmuxedo) cannot forge a line the app reads.
    static let logDir = "/Library/Logs/tmuxedo"
    static let runnerLog = logDir + "/runner.log"
    static let holderLog = logDir + "/holder.log"

    /// The runner's own state, as `tmuxedo`.
    static let stateDir = home + "/.local/state/tmuxedo"
    static let holderSocket = stateDir + "/holder.sock"
    /// The runner API socket, shared with group `tmuxedo` (0660) for the dashboard.
    static let apiDir = home + "/.local/state/tmuxedo-api"
    static let apiSocket = apiDir + "/runner.sock"

    /// The privileged helper (SMAppService.daemon), and the two daemons it installs.
    static let helperLabel = "dev.tmuxedo.mac.helper"
    static let helperPlist = "dev.tmuxedo.mac.helper.plist"
    static let holderLabel = "dev.tmuxedo.mac.iso-holder"
    static let runnerLabel = "dev.tmuxedo.mac.iso-runner"
    static let daemonDir = "/Library/LaunchDaemons"
    static var holderPlistPath: String { "\(daemonDir)/\(holderLabel).plist" }
    static var runnerPlistPath: String { "\(daemonDir)/\(runnerLabel).plist" }

    /// The XPC peer requirements (docs/design/mac-isolation.md §2.1). `team` is the build's own
    /// (RunnerBinary.appTeam), never a constant, so any team's signed build pins itself.
    static func requirement(identifier: String, team: String) -> String {
        "identifier \"\(identifier)\" and anchor apple generic and certificate leaf[subject.OU] = \"\(team)\""
    }

    /// PATH for terminals as `tmuxedo`: its own bin first, then the shared tool locations.
    static let path = [
        home + "/.local/bin", "/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin", "/usr/sbin", "/sbin",
    ].joined(separator: ":")

    /// Folders that are never granted, whoever asks (a grant on them would hand out the Mac).
    static let neverGrant: [String] = [
        "/", "/System", "/Library", "/Applications", "/usr", "/bin", "/sbin", "/private", "/etc", "/var", "/tmp",
        "/opt", "/cores", "/dev", "/Volumes", "/Users", "/Users/Shared",
    ]

    /// Inside the owner's home: the folders a grant may never be in (credentials, the Keychain,
    /// browser profiles, TCC-protected places). Relative to the home.
    static let privateInHome: [String] = [
        "Library", ".ssh", ".gnupg", ".aws", ".config", ".kube", ".docker", ".claude", ".netrc", ".npmrc",
        ".git-credentials", ".pypirc", ".cargo", ".local", ".zsh_history", ".bash_history", "Documents",
        "Desktop", "Downloads", "Pictures", "Movies", "Music",
    ]
}

/// What the runner needs to run as `tmuxedo` (the helper writes it into a root-only plist).
struct IsoRunnerConfig: Codable, Equatable {
    var server: String
    var token: String
    var allowUser: String?
    var observeOnly: Bool
    var confirm: Bool
    var confirmTimeout: Int
    var workDir: String?

    /// The `run` arguments, built here so app, helper and tests agree.
    var arguments: [String] {
        var a = ["run", "--managed", "--no-start-holder"]
        if let workDir { a += ["--work-dir", workDir] }
        if let allowUser { a += ["--allow-user", allowUser] }
        if observeOnly { a.append("--observe-only") }
        if confirm { a += ["--confirm-on-mac", "--confirm-timeout", String(confirmTimeout)] }
        return a
    }

    /// Refusals the helper applies before writing anything (it runs as root).
    func validate() -> String? {
        let loopback = ["127.0.0.1", "localhost", "::1"]
        guard let u = URL(string: server), let host = u.host,
              u.scheme == "https" || (u.scheme == "http" && loopback.contains(host)),
              !server.contains("\n") else { return "the server must be an https:// address" }
        let tokenOK = CharacterSet(charactersIn: "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789._-")
        guard (8...512).contains(token.count), token.unicodeScalars.allSatisfy(tokenOK.contains) else {
            return "the runner token is malformed"
        }
        if let a = allowUser, !(1...128).contains(a.count) || !a.unicodeScalars.allSatisfy(tokenOK.contains) {
            return "the account id is malformed"
        }
        guard (5...300).contains(confirmTimeout) else { return "the confirm timeout must be 5 to 300 seconds" }
        if let w = workDir, !(w.hasPrefix("/") || w.hasPrefix("~")) || w.contains("\0") || w.count > 1024 {
            return "the work folder must be an absolute path"
        }
        return nil
    }
}
