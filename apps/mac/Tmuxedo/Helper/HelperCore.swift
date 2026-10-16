import Darwin
import Foundation

/// Running a command. The real one is `Shell`; the tests record and answer.
protocol Commands {
    /// (status, stdout, stderr)
    func run(_ exe: String, _ args: [String]) -> (Int32, String, String)
}

struct RealCommands: Commands {
    func run(_ exe: String, _ args: [String]) -> (Int32, String, String) {
        Shell.runFull(exe, args, timeout: 120) ?? (-1, "", "\(exe) did not run")
    }
}

/// The privileged helper's work (docs/design/mac-isolation.md §2.1), as root. Exactly the
/// operations of `HelperRequest.Op`; every path is a constant from `Iso` (under `root`, which
/// only the tests change), the owner comes from the caller's credentials, and nothing a
/// terminal (uid `tmuxedo`) can write is ever executed or followed by root.
final class HelperCore {
    struct Owner: Equatable {
        let name: String
        let uid: uid_t
        let gid: gid_t
        let home: String

        static func lookup(uid: uid_t) -> Owner? {
            guard let pw = getpwuid(uid) else { return nil }
            return Owner(name: String(cString: pw.pointee.pw_name), uid: pw.pointee.pw_uid, gid: pw.pointee.pw_gid,
                         home: String(cString: pw.pointee.pw_dir))
        }

        static func lookup(name: String) -> Owner? {
            guard let pw = getpwnam(name) else { return nil }
            return lookup(uid: pw.pointee.pw_uid)
        }
    }

    let cmd: Commands
    let owner: Owner
    /// Prefix for every file this writes (the tests use a temporary folder; real runs "").
    let root: String
    /// The runner binary to install: the one bundled next to this helper (or `--runner`).
    let runnerSource: String?
    /// The team this helper is signed by (nil: unsigned, a development or CI build).
    let team: String?
    private(set) var steps: [String] = []

    init(cmd: Commands = RealCommands(), owner: Owner, root: String = "", runnerSource: String?, team: String?) {
        self.cmd = cmd
        self.owner = owner
        self.root = root
        self.runnerSource = runnerSource
        self.team = team
    }

    private func p(_ path: String) -> String { root + path }

    private func note(_ s: String) { steps.append(s) }

    @discardableResult
    private func sh(_ exe: String, _ args: [String], must: Bool = false) throws -> (Int32, String) {
        let (rc, out, err) = cmd.run(exe, args)
        if must && rc != 0 {
            throw HelperError.failed("\((exe as NSString).lastPathComponent) \(args.prefix(3).joined(separator: " ")) failed (\(rc)): \(err.isEmpty ? out : err)")
        }
        return (rc, out)
    }

    enum HelperError: Error, Equatable {
        case failed(String)
        var message: String { switch self { case let .failed(m): return m } }
    }

    // MARK: - Entry point

    func handle(_ r: HelperRequest) -> HelperResponse {
        steps = []
        do {
            switch r.op {
            case .status:
                return ok(status: status())
            case .setup:
                try setup()
                return ok(status: status())
            case .installRuntime:
                try installRuntime()
                return ok(status: status())
            case .startRunner:
                guard let cfg = r.runner else { return .fail("no runner settings") }
                if let why = cfg.validate() { return .fail(why) }
                return ok(daemon: try startRunner(cfg))
            case .stopRunner:
                bootout(Iso.runnerLabel)
                return ok(daemon: daemonState(Iso.runnerLabel))
            case .runnerState:
                return ok(daemon: daemonState(Iso.runnerLabel))
            case .stopAll:
                try stopAll()
                return ok(status: status())
            case .importState:
                try importState()
                return ok(status: status())
            case .uninstall:
                try uninstall()
                return ok(status: status())
            }
        } catch let e as HelperError {
            return .fail(e.message, steps: steps)
        } catch {
            return .fail(error.localizedDescription, steps: steps)
        }
    }

    private func ok(status: IsoStatus? = nil, daemon: DaemonState? = nil) -> HelperResponse {
        HelperResponse(ok: true, error: nil, status: status, daemon: daemon, steps: steps)
    }

    // MARK: - Status

    /// (uid, gid) of the user, if it exists.
    func userIDs() -> (Int, Int)? {
        let (rc, out, _) = cmd.run("/usr/bin/dscl", [".", "-read", "/Users/\(Iso.user)", "UniqueID", "PrimaryGroupID"])
        guard rc == 0 else { return nil }
        var uid: Int?, gid: Int?
        for line in out.split(separator: "\n") {
            let parts = line.split(separator: ":", maxSplits: 1).map { $0.trimmingCharacters(in: .whitespaces) }
            guard parts.count == 2 else { continue }
            if parts[0] == "UniqueID" { uid = Int(parts[1]) }
            if parts[0] == "PrimaryGroupID" { gid = Int(parts[1]) }
        }
        guard let uid, let gid else { return nil }
        return (uid, gid)
    }

    func isMember(_ user: String, of group: String) -> Bool {
        let (rc, out, _) = cmd.run("/usr/sbin/dseditgroup", ["-o", "checkmember", "-m", user, group])
        return rc == 0 && out.hasPrefix("yes")
    }

    func daemonState(_ label: String) -> DaemonState {
        let (rc, out, _) = cmd.run("/bin/launchctl", ["print", "system/\(label)"])
        return DaemonState.parse(launchctlPrint: out, loaded: rc == 0)
    }

    func status() -> IsoStatus {
        let ids = userIDs()
        return IsoStatus(
            userExists: ids != nil, uid: ids?.0, gid: ids?.1,
            isAdmin: ids != nil && isMember(Iso.user, of: "admin"),
            ownerInGroup: ids != nil && isMember(owner.name, of: Iso.group),
            runtimeInstalled: FileManager.default.isExecutableFile(atPath: p(Iso.runnerPath))
                && FileManager.default.fileExists(atPath: p(Iso.holderPlistPath)),
            holder: daemonState(Iso.holderLabel), runner: daemonState(Iso.runnerLabel))
    }

    // MARK: - Setup: the user, its group, the folders

    /// The first id in `Iso.idRange` not in `used`.
    static func freeID(used: Set<Int>) -> Int? { Iso.idRange.first { !used.contains($0) } }

    static func parseIDs(_ dsclList: String) -> Set<Int> {
        Set(dsclList.split(separator: "\n").compactMap { line in
            line.split(whereSeparator: { $0 == " " || $0 == "\t" }).last.flatMap { Int($0) }
        })
    }

    func setup() throws {
        let dscl = "/usr/bin/dscl"
        var gid: Int
        let (grc, gout, _) = cmd.run(dscl, [".", "-read", "/Groups/\(Iso.group)", "PrimaryGroupID"])
        if grc == 0, let g = gout.split(separator: ":").last.flatMap({ Int($0.trimmingCharacters(in: .whitespaces)) }) {
            gid = g
        } else {
            let (_, list, _) = cmd.run(dscl, [".", "-list", "/Groups", "PrimaryGroupID"])
            guard let g = Self.freeID(used: Self.parseIDs(list)) else { throw HelperError.failed("no free group id") }
            gid = g
            try sh(dscl, [".", "-create", "/Groups/\(Iso.group)"], must: true)
            try sh(dscl, [".", "-create", "/Groups/\(Iso.group)", "PrimaryGroupID", String(g)], must: true)
            try sh(dscl, [".", "-create", "/Groups/\(Iso.group)", "RealName", Iso.realName], must: true)
            try sh(dscl, [".", "-create", "/Groups/\(Iso.group)", "Password", "*"], must: true)
            note("created group \(Iso.group) (gid \(g))")
        }
        if userIDs() == nil {
            let (_, list, _) = cmd.run(dscl, [".", "-list", "/Users", "UniqueID"])
            guard let uid = Self.freeID(used: Self.parseIDs(list)) else { throw HelperError.failed("no free user id") }
            let u = "/Users/\(Iso.user)"
            try sh(dscl, [".", "-create", u], must: true)
            for (k, v) in [("UniqueID", String(uid)), ("PrimaryGroupID", String(gid)), ("UserShell", "/bin/zsh"),
                           ("NFSHomeDirectory", Iso.home), ("RealName", Iso.realName),
                           // No password at all: nobody can log in or `su` as it; launchd runs it.
                           ("Password", "*"), ("IsHidden", "1")] {
                try sh(dscl, [".", "-create", u, k, v], must: true)
            }
            note("created the hidden standard user \(Iso.user) (uid \(uid))")
        } else if let (_, ugid) = userIDs() {
            gid = ugid
        }
        // Never an administrator, whatever happened before.
        if isMember(Iso.user, of: "admin") {
            try sh("/usr/sbin/dseditgroup", ["-o", "edit", "-d", Iso.user, "-t", "user", "admin"], must: true)
            note("removed \(Iso.user) from admin")
        }
        // Its home: its own, 0750, group tmuxedo (which only it and the owner are in).
        let home = p(Iso.home)
        if !FileManager.default.fileExists(atPath: home) {
            if root.isEmpty { try? sh("/usr/sbin/createhomedir", ["-c", "-u", Iso.user]) }
            if !FileManager.default.fileExists(atPath: home) {
                try FileManager.default.createDirectory(atPath: home, withIntermediateDirectories: true)
            }
            try sh("/usr/sbin/chown", ["-R", "\(Iso.user):\(Iso.group)", home], must: true)
            note("created \(Iso.home)")
        }
        try sh("/usr/sbin/chown", ["\(Iso.user):\(Iso.group)", home], must: true)
        chmod(home, 0o750)
        // The owner joins the group: the app reaches the runner's dashboard socket and the
        // terminals' runner reaches the app's signer socket.
        if !isMember(owner.name, of: Iso.group) {
            try sh("/usr/sbin/dseditgroup", ["-o", "edit", "-a", owner.name, "-t", "user", Iso.group], must: true)
            note("added \(owner.name) to group \(Iso.group)")
        }
        try folders(gid: gid)
        _ = cmd.run("/usr/bin/dscacheutil", ["-flushcache"]) // the new user and group, visible to sudo
        try dashboardFolder()
    }

    /// The runner API socket's folder, in the user's home, with an inherited entry that lets the
    /// OWNER (by name, not by group: a running app's group list is fixed when the owner logged in)
    /// connect to the socket the runner creates there. Done AS the user, never as root: the path
    /// is inside its home, where a terminal could have planted links.
    func dashboardFolder() throws {
        let asUser = ["-n", "-u", Iso.user, "--"]
        let ace = "user:\(owner.name) allow list,add_file,search,readattr,readextattr,readsecurity,file_inherit,directory_inherit"
        try sh("/usr/bin/sudo", asUser + ["/bin/mkdir", "-p", p(Iso.apiDir)], must: true)
        _ = cmd.run("/usr/bin/sudo", asUser + ["/bin/chmod", "-a", ace, p(Iso.apiDir)]) // no duplicate on a second setup
        try sh("/usr/bin/sudo", asUser + ["/bin/chmod", "+a", ace, p(Iso.apiDir)], must: true)
    }

    private func mkdir(_ path: String, owner o: String, mode: mode_t) throws {
        let full = p(path)
        var st = stat()
        if lstat(full, &st) == 0, st.st_mode & S_IFMT != S_IFDIR {
            throw HelperError.failed("\(path) exists and is not a folder")
        }
        try FileManager.default.createDirectory(atPath: full, withIntermediateDirectories: true)
        try sh("/usr/sbin/chown", [o, full], must: true)
        chmod(full, mode)
    }

    private func folders(gid: Int) throws {
        try mkdir(Iso.supportDir, owner: "root:wheel", mode: 0o755)
        try mkdir(Iso.binDir, owner: "root:wheel", mode: 0o755)
        try mkdir(Iso.logDir, owner: "root:wheel", mode: 0o755)
        try mkdir(Iso.signerDir, owner: "\(owner.uid):\(gid)", mode: 0o750)
        // launchd opens a daemon's log AS THE DAEMON'S USER: the files are the user's (0644, so the
        // owner's app can read them), in a root-owned folder, so a terminal can append but never
        // remove or replace them (see mac-isolation.md §2.2).
        for log in [Iso.runnerLog, Iso.holderLog] {
            let f = p(log)
            var st = stat()
            if lstat(f, &st) == 0, st.st_mode & S_IFMT != S_IFREG { throw HelperError.failed("\(log) is not a file") }
            if !FileManager.default.fileExists(atPath: f) { FileManager.default.createFile(atPath: f, contents: nil) }
            try sh("/usr/sbin/chown", ["\(Iso.user):\(Iso.group)", f], must: true)
            chmod(f, 0o644)
        }
    }

    // MARK: - The runtime: a root-owned runner, the holder daemon

    func installRuntime() throws {
        guard userIDs() != nil else { throw HelperError.failed("set up the separate user first") }
        guard let src = runnerSource, FileManager.default.isExecutableFile(atPath: src) else {
            throw HelperError.failed("the bundled runner is missing")
        }
        if let why = RunnerBinary.mayLaunch(check: RunnerBinary.checkSignature(of: URL(fileURLWithPath: src), team: team),
                                            appTeam: team) {
            throw HelperError.failed(why)
        }
        let dest = p(Iso.runnerPath)
        let tmp = dest + ".new"
        try? FileManager.default.removeItem(atPath: tmp)
        try FileManager.default.copyItem(atPath: src, toPath: tmp)
        try sh("/usr/sbin/chown", ["root:wheel", tmp], must: true)
        chmod(tmp, 0o755)
        guard rename(tmp, dest) == 0 else { throw HelperError.failed("could not install the runner (\(errno))") }
        note("installed the runner at \(Iso.runnerPath)")

        let plist = Self.holderPlist(runner: Iso.runnerPath)
        let changed = try write(plist, to: p(Iso.holderPlistPath), mode: 0o644)
        let state = daemonState(Iso.holderLabel)
        if !state.loaded || changed {
            if state.loaded { bootout(Iso.holderLabel) }
            try sh("/bin/launchctl", ["bootstrap", "system", p(Iso.holderPlistPath)], must: true)
            note("started the terminal holder as \(Iso.user)")
            return
        }
        // Running: restart only if the holder's own code changed (it ends every terminal).
        let new = Self.field(cmd.run(dest, ["version", "--json"]).1, "holderBuild")
        let running = Self.field(cmd.run(dest, ["holder-info", "--socket", Iso.holderSocket]).1, "build")
        if let new, let running, new != running {
            try sh("/bin/launchctl", ["kickstart", "-k", "system/\(Iso.holderLabel)"], must: true)
            note("restarted the terminal holder: its code changed (\(running) to \(new))")
        } else {
            note("kept the running terminal holder")
        }
    }

    static func field(_ json: String, _ key: String) -> String? {
        guard let d = json.data(using: .utf8), let o = try? JSONSerialization.jsonObject(with: d) as? [String: Any] else { return nil }
        return o[key] as? String
    }

    static func baseEnvironment() -> [String: String] {
        ["HOME": Iso.home, "USER": Iso.user, "LOGNAME": Iso.user, "SHELL": "/bin/zsh", "PATH": Iso.path,
         "LANG": "en_US.UTF-8", "TMUXEDO_LAUNCHED_BY": "tmuxedo.app (separate user)",
         "TMUXEDO_HOLDER_SOCKET": Iso.holderSocket]
    }

    static func holderPlist(runner: String) -> [String: Any] {
        [
            "Label": Iso.holderLabel,
            "ProgramArguments": [runner, "holder", "--wait", "--socket", Iso.holderSocket],
            "UserName": Iso.user, "GroupName": Iso.group, "InitGroups": true,
            "EnvironmentVariables": baseEnvironment(),
            "WorkingDirectory": Iso.home,
            "RunAtLoad": true, "KeepAlive": true, "ProcessType": "Interactive", "Umask": 0o022,
            "StandardOutPath": Iso.holderLog, "StandardErrorPath": Iso.holderLog,
        ]
    }

    /// The runner daemon: NOT kept alive by launchd (the app supervises it, with its backoff
    /// and its stop-for-good on a refusal). Root-only plist: it holds the token.
    static func runnerPlist(_ cfg: IsoRunnerConfig, runner: String, gid: Int) -> [String: Any] {
        var env = baseEnvironment()
        env["TMUXEDO_SERVER"] = cfg.server
        env["TMUXEDO_RUNNER_TOKEN"] = cfg.token
        env["TMUXEDO_SIGNER_SOCKET"] = Iso.signerSocket
        env["TMUXEDO_RUNNER_SOCKET"] = Iso.apiSocket
        env["TMUXEDO_RUNNER_SOCKET_GROUP"] = String(gid)
        return [
            "Label": Iso.runnerLabel,
            "ProgramArguments": [runner] + cfg.arguments,
            "UserName": Iso.user, "GroupName": Iso.group, "InitGroups": true,
            "EnvironmentVariables": env,
            "WorkingDirectory": Iso.home,
            "RunAtLoad": true, "KeepAlive": false, "ProcessType": "Interactive", "Umask": 0o022,
            "ExitTimeOut": 10,
            "StandardOutPath": Iso.runnerLog, "StandardErrorPath": Iso.runnerLog,
        ]
    }

    /// Write a plist owned by root. Returns whether the content changed.
    @discardableResult
    private func write(_ plist: [String: Any], to path: String, mode: mode_t) throws -> Bool {
        let data = try PropertyListSerialization.data(fromPropertyList: plist, format: .xml, options: 0)
        if let old = FileManager.default.contents(atPath: path), old == data { return false }
        let tmp = path + ".new"
        try? FileManager.default.removeItem(atPath: tmp)
        guard FileManager.default.createFile(atPath: tmp, contents: nil, attributes: [.posixPermissions: NSNumber(value: 0o600)])
        else { throw HelperError.failed("could not write \(path)") }
        let h = try FileHandle(forWritingTo: URL(fileURLWithPath: tmp))
        try h.write(contentsOf: data)
        try h.close()
        try sh("/usr/sbin/chown", ["root:wheel", tmp], must: true)
        chmod(tmp, mode)
        guard rename(tmp, path) == 0 else { throw HelperError.failed("could not write \(path) (\(errno))") }
        return true
    }

    private func bootout(_ label: String) {
        _ = cmd.run("/bin/launchctl", ["bootout", "system/\(label)"])
    }

    func startRunner(_ cfg: IsoRunnerConfig) throws -> DaemonState {
        let s = status()
        guard s.ready, let gid = s.gid else { throw HelperError.failed("the separate user is not set up (run setup and install first)") }
        guard s.holder.loaded else { throw HelperError.failed("the terminal holder is not installed") }
        bootout(Iso.runnerLabel)
        try write(Self.runnerPlist(cfg, runner: Iso.runnerPath, gid: gid), to: p(Iso.runnerPlistPath), mode: 0o600)
        try sh("/bin/launchctl", ["bootstrap", "system", p(Iso.runnerPlistPath)], must: true)
        note("started the runner as \(Iso.user)")
        var d = daemonState(Iso.runnerLabel)
        for _ in 0..<20 where !d.running && d.lastExit == nil {
            usleep(100_000)
            d = daemonState(Iso.runnerLabel)
        }
        return d
    }

    // MARK: - Kill switch and uninstall

    func stopAll() throws {
        bootout(Iso.runnerLabel)
        bootout(Iso.holderLabel)
        for f in [Iso.runnerPlistPath, Iso.holderPlistPath] { try? FileManager.default.removeItem(atPath: p(f)) }
        if let (uid, _) = userIDs() {
            // Every process of the terminals' user ends, including ones that ignored the hangup,
            // and the per-user launchd domain macOS started for it.
            _ = cmd.run("/bin/launchctl", ["bootout", "user/\(uid)"])
            _ = cmd.run("/usr/bin/pkill", ["-9", "-U", String(uid)])
        }
        note("stopped the runner, the holder and every process of \(Iso.user)")
    }

    func uninstall() throws {
        try stopAll()
        let ids = userIDs()
        if let (uid, _) = ids {
            _ = cmd.run("/bin/launchctl", ["bootout", "user/\(uid)"])
            _ = cmd.run("/usr/bin/pkill", ["-9", "-U", String(uid)])
        }
        // The home: only that exact folder, only if it is a real folder owned by that user.
        let home = p(Iso.home)
        var st = stat()
        if lstat(home, &st) == 0 {
            guard st.st_mode & S_IFMT == S_IFDIR, ids.map({ Int(st.st_uid) == $0.0 }) ?? true else {
                throw HelperError.failed("\(Iso.home) is not the terminals' home; left alone")
            }
            try FileManager.default.removeItem(atPath: home)
            note("removed \(Iso.home)")
        }
        if ids != nil {
            try sh("/usr/bin/dscl", [".", "-delete", "/Users/\(Iso.user)"], must: true)
            note("removed the user \(Iso.user)")
        }
        if cmd.run("/usr/bin/dscl", [".", "-read", "/Groups/\(Iso.group)"]).0 == 0 {
            try sh("/usr/bin/dscl", [".", "-delete", "/Groups/\(Iso.group)"], must: true)
            note("removed the group \(Iso.group)")
        }
        for d in [Iso.supportDir, Iso.logDir] { try? FileManager.default.removeItem(atPath: p(d)) }
        _ = cmd.run("/usr/bin/dscacheutil", ["-flushcache"])
    }

    // MARK: - Migration

    /// Copy what the owner staged (their runner state, Claude transcripts for granted folders)
    /// into the terminals' home. The copy runs AS `tmuxedo`, not as root: the staging folder is
    /// readable by group `tmuxedo`, and the destination is that user's own home, where a
    /// terminal could have planted links for root to follow. So nothing here reads or writes
    /// with more than the terminals' own rights.
    func importState() throws {
        guard userIDs() != nil else { throw HelperError.failed("set up the separate user first") }
        guard !daemonState(Iso.runnerLabel).running else { throw HelperError.failed("stop the runner first") }
        let staging = p(Iso.stagingDir)
        for (name, dest) in [("state", Iso.stateDir), ("claude-projects", Iso.home + "/.claude/projects")] {
            let src = staging + "/" + name
            guard FileManager.default.fileExists(atPath: src) else { continue }
            try sh("/usr/bin/sudo", ["-n", "-u", Iso.user, "--", "/bin/mkdir", "-p", p(dest)], must: true)
            try sh("/usr/bin/sudo", ["-n", "-u", Iso.user, "--", "/usr/bin/ditto", "--norsrc", "--noextattr", "--noacl",
                                     src, p(dest)], must: true)
            note("imported \(name) into \(dest)")
        }
    }
}
