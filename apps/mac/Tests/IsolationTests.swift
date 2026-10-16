import Darwin
import ServiceManagement
import XCTest

// MARK: - Fakes

/// Records every command the helper core runs and answers from a script.
final class FakeCommands: Commands {
    var calls: [[String]] = []
    var answer: ([String]) -> (Int32, String, String) = { _ in (0, "", "") }
    func run(_ exe: String, _ args: [String]) -> (Int32, String, String) {
        calls.append([exe] + args)
        return answer([exe] + args)
    }
    func ran(_ needle: [String]) -> Bool { calls.contains { $0.count >= needle.count && Array($0.prefix(needle.count)) == needle } }
}

/// A helper that answers like the real one, from state the test controls.
final class FakeHelper: HelperClient {
    let lock = NSLock()
    var requests: [HelperRequest] = []
    var status = IsoStatus(userExists: true, uid: 601, gid: 601, isAdmin: false, ownerInGroup: true, runtimeInstalled: true,
                           holder: DaemonState(loaded: true, running: true, pid: 50, lastExit: nil), runner: .absent)
    var runner = DaemonState.absent
    var nextPid = 700
    var failing: Set<HelperRequest.Op> = []
    /// Called when the app asks for an import (the staging folder is complete then).
    var onImport: () -> Void = {}

    func call(_ r: HelperRequest) -> HelperResponse {
        lock.lock(); defer { lock.unlock() }
        requests.append(r)
        if failing.contains(r.op) { return .fail("\(r.op.rawValue) failed on purpose") }
        switch r.op {
        case .startRunner:
            nextPid += 1
            runner = DaemonState(loaded: true, running: true, pid: nextPid, lastExit: nil)
            return HelperResponse(ok: true, error: nil, status: nil, daemon: runner)
        case .stopRunner:
            runner = .absent
            return HelperResponse(ok: true, error: nil, status: nil, daemon: runner)
        case .runnerState:
            return HelperResponse(ok: true, error: nil, status: nil, daemon: runner)
        case .stopAll:
            runner = .absent
            status.holder = .absent
            return HelperResponse(ok: true, error: nil, status: status, daemon: nil)
        case .uninstall:
            status = .none
            return HelperResponse(ok: true, error: nil, status: status, daemon: nil)
        case .importState:
            onImport()
            return HelperResponse(ok: true, error: nil, status: status, daemon: nil)
        default:
            return HelperResponse(ok: true, error: nil, status: status, daemon: nil)
        }
    }
    var ops: [HelperRequest.Op] { lock.lock(); defer { lock.unlock() }; return requests.map(\.op) }
    /// The runner exits by itself with `code` (launchd keeps the job loaded).
    func exitRunner(_ code: Int) { lock.lock(); runner = DaemonState(loaded: true, running: false, pid: nil, lastExit: code); lock.unlock() }
}

let uuidA: uuid_t = UUID(uuidString: "11111111-2222-3333-4444-555555555555")!.uuid
let uuidB: uuid_t = UUID(uuidString: "66666666-7777-8888-9999-AAAAAAAAAAAA")!.uuid

// MARK: - Modes and settings

@MainActor
final class IsolationModeTests: XCTestCase {
    func suite() -> (UserDefaults, String) {
        let s = "dev.tmuxedo.mac.tests.iso.\(UUID().uuidString.prefix(8))"
        return (UserDefaults(suiteName: s)!, s)
    }

    func testANewInstallStartsAsTheSeparateUserAndAnExistingOneKeepsFullAccess() {
        let (d, s) = suite()
        defer { d.removePersistentDomain(forName: s) }
        XCTAssertEqual(Settings(defaults: d, secrets: MemoryStore()).isolation, .separateUser)
        // Decided once: onboarding later does not turn a new install into an "existing" one.
        d.set(true, forKey: "onboarded")
        XCTAssertEqual(Settings(defaults: d, secrets: MemoryStore()).isolation, .separateUser)

        let (d2, s2) = suite()
        defer { d2.removePersistentDomain(forName: s2) }
        d2.set(true, forKey: "onboarded") // an install from before separate-user mode
        XCTAssertEqual(Settings(defaults: d2, secrets: MemoryStore()).isolation, .fullAccess)
        XCTAssertEqual(IsolationMode.initial(existingInstall: false), .separateUser)
        XCTAssertEqual(IsolationMode.initial(existingInstall: true), .fullAccess)
    }

    func testConfirmSettingsPersistAndAreBounded() {
        let (d, s) = suite()
        defer { d.removePersistentDomain(forName: s) }
        let a = Settings(defaults: d, secrets: MemoryStore())
        XCTAssertFalse(a.confirmOnMac, "optional: off until the owner turns it on")
        XCTAssertEqual(a.confirmTimeout, 30)
        a.confirmOnMac = true
        a.confirmTimeout = 60
        a.grantState = GrantState(grants: ["/x"], revoked: [], manifest: ["/h"], locked: true)
        let b = Settings(defaults: d, secrets: MemoryStore())
        XCTAssertTrue(b.confirmOnMac)
        XCTAssertEqual(b.confirmTimeout, 60)
        XCTAssertEqual(b.grantState.grants, ["/x"])
        d.set(7, forKey: "confirmTimeout")
        XCTAssertEqual(Settings(defaults: d, secrets: MemoryStore()).confirmTimeout, 30, "only the offered values")
    }

    func testEveryModeExplainsItselfAndFullAccessWarns() {
        XCTAssertTrue(IsolationMode.separateUser.summary.contains("cannot read your home"))
        XCTAssertTrue(IsolationMode.fullAccess.summary.contains("can read your files"))
        XCTAssertEqual(IsolationMode.allCases.count, 2, "the VM mode is designed, not offered")
    }
}

// MARK: - What the helper may be asked to run

final class IsoRunnerConfigTests: XCTestCase {
    func cfg() -> IsoRunnerConfig {
        IsoRunnerConfig(server: "https://tmuxedo.dev", token: "tmr.lookup.secret", allowUser: "u1", observeOnly: true,
                        confirm: true, confirmTimeout: 30, workDir: "/Users/o/proj")
    }

    func testArguments() {
        XCTAssertEqual(cfg().arguments, ["run", "--managed", "--no-start-holder", "--work-dir", "/Users/o/proj",
                                         "--allow-user", "u1", "--observe-only", "--confirm-on-mac", "--confirm-timeout", "30"])
    }

    func testValidationRefusesWhatRootMustNotWrite() {
        XCTAssertNil(cfg().validate())
        var c = cfg(); c.server = "http://evil.example"; XCTAssertNotNil(c.validate())
        c = cfg(); c.server = "https://x\n<key>"; XCTAssertNotNil(c.validate())
        c = cfg(); c.server = "http://127.0.0.1:9"; XCTAssertNil(c.validate(), "loopback http for tests")
        c = cfg(); c.token = "tmr.x</string><key>Program</key>"; XCTAssertNotNil(c.validate())
        c = cfg(); c.token = "short"; XCTAssertNotNil(c.validate())
        c = cfg(); c.allowUser = "u 1"; XCTAssertNotNil(c.validate())
        c = cfg(); c.confirmTimeout = 1000; XCTAssertNotNil(c.validate())
        c = cfg(); c.workDir = "relative/path"; XCTAssertNotNil(c.validate())
    }

    func testDaemonStateFromLaunchctl() {
        XCTAssertEqual(DaemonState.parse(launchctlPrint: "\tstate = not running\n\tlast exit code = 1", loaded: true).lastExit, 1)
        let running = "system/dev.tmuxedo.mac.iso-runner = {\n\tactive count = 1\n\tstate = running\n\tpid = 4242\n\tlast exit code = (never exited)\n}"
        XCTAssertEqual(DaemonState.parse(launchctlPrint: running, loaded: true), DaemonState(loaded: true, running: true, pid: 4242, lastExit: nil))
        let exited = "\tstate = not running\n\tlast exit code = 78: EX_CONFIG"
        XCTAssertEqual(DaemonState.parse(launchctlPrint: exited, loaded: true), DaemonState(loaded: true, running: false, pid: nil, lastExit: 78))
        XCTAssertEqual(DaemonState.parse(launchctlPrint: "Could not find service", loaded: false), .absent)
    }
}

// MARK: - The helper core, with every command recorded

final class HelperCoreTests: XCTestCase {
    var root: String!
    var cmd: FakeCommands!
    let owner = HelperCore.Owner(name: "alex", uid: 501, gid: 20, home: "/Users/alex")

    override func setUp() {
        root = tempDir().path
        cmd = FakeCommands()
    }

    override func tearDown() {
        try? FileManager.default.removeItem(atPath: root)
    }

    func core(runner: String? = nil) -> HelperCore {
        HelperCore(cmd: cmd, owner: owner, root: root, runnerSource: runner, team: nil)
    }

    /// No user yet: the group, then a hidden, passwordless, standard user, its home, the owner in its group.
    func testSetupCreatesAHiddenStandardPasswordlessUser() {
        var userExists = false
        cmd.answer = { c in
            switch Array(c.prefix(4)) {
            case ["/usr/bin/dscl", ".", "-read", "/Users/tmuxedo"]:
                return userExists ? (0, "UniqueID: 601\nPrimaryGroupID: 602", "") : (56, "", "no such user")
            case ["/usr/bin/dscl", ".", "-read", "/Groups/tmuxedo"]: return (56, "", "")
            case ["/usr/bin/dscl", ".", "-list", "/Groups"]: return (0, "staff 20\nadmin 80\nsomething 601", "")
            case ["/usr/bin/dscl", ".", "-list", "/Users"]: return (0, "alex 501\nroot 0", "")
            case ["/usr/sbin/dseditgroup", "-o", "checkmember", "-m"]: return (67, "no", "")
            default:
                if c.contains("IsHidden") { userExists = true }
                return (0, "", "")
            }
        }
        let r = core().handle(HelperRequest(.setup))
        XCTAssertTrue(r.ok, r.error ?? "")
        XCTAssertTrue(cmd.ran(["/usr/bin/dscl", ".", "-create", "/Groups/tmuxedo", "PrimaryGroupID", "602"]), "the first free gid")
        XCTAssertTrue(cmd.ran(["/usr/bin/dscl", ".", "-create", "/Users/tmuxedo", "UniqueID", "601"]))
        XCTAssertTrue(cmd.ran(["/usr/bin/dscl", ".", "-create", "/Users/tmuxedo", "PrimaryGroupID", "602"]))
        XCTAssertTrue(cmd.ran(["/usr/bin/dscl", ".", "-create", "/Users/tmuxedo", "Password", "*"]), "no password")
        XCTAssertTrue(cmd.ran(["/usr/bin/dscl", ".", "-create", "/Users/tmuxedo", "IsHidden", "1"]))
        XCTAssertTrue(cmd.ran(["/usr/sbin/dseditgroup", "-o", "edit", "-a", "alex", "-t", "user", "tmuxedo"]))
        XCTAssertFalse(cmd.calls.contains { $0.contains("admin") && $0.contains("-a") }, "never added to admin")
        XCTAssertFalse(cmd.calls.contains { $0.contains("sysadminctl") }, "no password-bearing command line")
        XCTAssertTrue(cmd.ran(["/usr/sbin/chown", "tmuxedo:tmuxedo", root + Iso.home]))
        var st = stat()
        XCTAssertEqual(stat(root + Iso.home, &st), 0)
        XCTAssertEqual(st.st_mode & 0o777, 0o750)
        XCTAssertEqual(stat(root + Iso.signerDir, &st), 0)
        XCTAssertEqual(st.st_mode & 0o777, 0o750)
        XCTAssertTrue(cmd.ran(["/usr/sbin/chown", "501:602", root + Iso.signerDir]), "the owner's signer folder, group tmuxedo")
        XCTAssertTrue(cmd.ran(["/usr/bin/sudo", "-n", "-u", "tmuxedo", "--", "/bin/chmod", "+a",
                               "user:alex allow list,add_file,search,readattr,readextattr,readsecurity,file_inherit,directory_inherit",
                               root + Iso.apiDir]), "the dashboard socket's folder lets the owner in, set as the user")
    }

    func testSetupRemovesAnAdminMembershipAndIsIdempotent() {
        cmd.answer = { c in
            if c.starts(with: ["/usr/bin/dscl", ".", "-read", "/Users/tmuxedo"]) { return (0, "UniqueID: 601\nPrimaryGroupID: 601", "") }
            if c.starts(with: ["/usr/bin/dscl", ".", "-read", "/Groups/tmuxedo"]) { return (0, "PrimaryGroupID: 601", "") }
            if c.starts(with: ["/usr/sbin/dseditgroup", "-o", "checkmember"]) { return (0, "yes it is", "") }
            return (0, "", "")
        }
        XCTAssertTrue(core().handle(HelperRequest(.setup)).ok)
        XCTAssertFalse(cmd.calls.contains { $0.contains("-create") }, "nothing created twice")
        XCTAssertTrue(cmd.ran(["/usr/sbin/dseditgroup", "-o", "edit", "-d", "tmuxedo", "-t", "user", "admin"]))
    }

    func testStartRunnerRefusesWhenNotReadyOrBadSettings() {
        cmd.answer = { _ in (1, "", "") } // no user
        let bad = IsoRunnerConfig(server: "http://evil", token: "tmr.a.b.c.d", allowUser: nil, observeOnly: false,
                                  confirm: false, confirmTimeout: 30, workDir: nil)
        XCTAssertFalse(core().handle(HelperRequest(.startRunner, runner: bad)).ok)
        XCTAssertFalse(core().handle(HelperRequest(.startRunner)).ok, "no settings")
        let good = IsoRunnerConfig(server: "https://tmuxedo.dev", token: "tmr.lookup.secret", allowUser: nil, observeOnly: false,
                                   confirm: false, confirmTimeout: 30, workDir: nil)
        let r = core().handle(HelperRequest(.startRunner, runner: good))
        XCTAssertFalse(r.ok)
        XCTAssertTrue(r.error?.contains("not set up") == true, r.error ?? "")
        XCTAssertFalse(cmd.calls.contains { $0.contains("bootstrap") })
    }

    func testTheDaemonsRunAsTheUserFromRootsCopy() throws {
        let cfg = IsoRunnerConfig(server: "https://tmuxedo.dev", token: "tmr.lookup.secret", allowUser: "u1", observeOnly: false,
                                  confirm: true, confirmTimeout: 30, workDir: nil)
        let r = HelperCore.runnerPlist(cfg, runner: Iso.runnerPath, gid: 601)
        XCTAssertEqual(r["UserName"] as? String, "tmuxedo")
        XCTAssertEqual(r["KeepAlive"] as? Bool, false, "the app supervises the runner")
        XCTAssertEqual((r["ProgramArguments"] as? [String])?.first, Iso.runnerPath)
        let env = r["EnvironmentVariables"] as? [String: String] ?? [:]
        XCTAssertEqual(env["HOME"], Iso.home)
        XCTAssertEqual(env["TMUXEDO_SIGNER_SOCKET"], Iso.signerSocket)
        XCTAssertEqual(env["TMUXEDO_RUNNER_SOCKET_GROUP"], "601")
        XCTAssertEqual(r["StandardOutPath"] as? String, Iso.runnerLog)
        let h = HelperCore.holderPlist(runner: Iso.runnerPath)
        XCTAssertEqual(h["UserName"] as? String, "tmuxedo")
        XCTAssertEqual(h["KeepAlive"] as? Bool, true)
        XCTAssertEqual(h["ProgramArguments"] as? [String], [Iso.runnerPath, "holder", "--wait", "--socket", Iso.holderSocket])
        XCTAssertNil((h["EnvironmentVariables"] as? [String: String])?["TMUXEDO_RUNNER_TOKEN"], "the holder never sees the token")
        XCTAssertTrue(Iso.runnerPath.hasPrefix("/Library/"), "root-owned, outside anything the user can write")
    }

    func testInstallRuntimeRefusesAMissingOrForeignRunner() throws {
        cmd.answer = { c in c.starts(with: ["/usr/bin/dscl", ".", "-read", "/Users/tmuxedo"]) ? (0, "UniqueID: 601\nPrimaryGroupID: 601", "") : (0, "", "") }
        XCTAssertTrue(core(runner: nil).handle(HelperRequest(.installRuntime)).error?.contains("missing") == true)
        // A signed release helper only installs a runner of its own team: an ad-hoc one is refused.
        let bin = tempDir().appendingPathComponent("tmuxedo-runner").path
        try FileManager.default.copyItem(atPath: "/usr/bin/true", toPath: bin)
        _ = Shell.run("/usr/bin/codesign", ["-f", "-s", "-", bin])
        let signed = HelperCore(cmd: cmd, owner: owner, root: root, runnerSource: bin, team: "ABCDE12345")
        let r = signed.handle(HelperRequest(.installRuntime))
        XCTAssertFalse(r.ok)
        XCTAssertTrue(r.error?.contains("not signed") == true, r.error ?? "")
    }

    func testInstallRuntimeCopiesAndStartsTheHolderOnce() throws {
        let bin = tempDir().appendingPathComponent("tmuxedo-runner").path
        try FileManager.default.copyItem(atPath: "/usr/bin/true", toPath: bin)
        _ = Shell.run("/usr/bin/codesign", ["-f", "-s", "-", bin])
        try FileManager.default.createDirectory(atPath: root + Iso.binDir, withIntermediateDirectories: true)
        try FileManager.default.createDirectory(atPath: root + Iso.daemonDir, withIntermediateDirectories: true)
        var loaded = false
        cmd.answer = { c in
            if c.starts(with: ["/usr/bin/dscl", ".", "-read", "/Users/tmuxedo"]) { return (0, "UniqueID: 601\nPrimaryGroupID: 601", "") }
            if c.starts(with: ["/bin/launchctl", "print"]) { return loaded ? (0, "state = running\npid = 9", "") : (113, "", "") }
            if c.starts(with: ["/bin/launchctl", "bootstrap"]) { loaded = true }
            return (0, "", "")
        }
        XCTAssertTrue(core(runner: bin).handle(HelperRequest(.installRuntime)).ok)
        XCTAssertTrue(FileManager.default.isExecutableFile(atPath: root + Iso.runnerPath))
        XCTAssertEqual(cmd.calls.filter { $0.contains("bootstrap") }.count, 1)
        // Again, same plist, same holder build: kept (an update does not end the terminals).
        cmd.calls = []
        XCTAssertTrue(core(runner: bin).handle(HelperRequest(.installRuntime)).ok)
        XCTAssertFalse(cmd.calls.contains { $0.contains("bootstrap") || $0.contains("kickstart") })
    }

    func testKillSwitchEndsEveryProcessOfTheUserAndRemovesTheDaemons() throws {
        try FileManager.default.createDirectory(atPath: root + Iso.daemonDir, withIntermediateDirectories: true)
        FileManager.default.createFile(atPath: root + Iso.runnerPlistPath, contents: Data("token".utf8))
        FileManager.default.createFile(atPath: root + Iso.holderPlistPath, contents: Data())
        cmd.answer = { c in c.starts(with: ["/usr/bin/dscl", ".", "-read", "/Users/tmuxedo"]) ? (0, "UniqueID: 601\nPrimaryGroupID: 601", "") : (0, "", "") }
        XCTAssertTrue(core().handle(HelperRequest(.stopAll)).ok)
        XCTAssertTrue(cmd.ran(["/bin/launchctl", "bootout", "system/\(Iso.runnerLabel)"]))
        XCTAssertTrue(cmd.ran(["/bin/launchctl", "bootout", "system/\(Iso.holderLabel)"]))
        XCTAssertTrue(cmd.ran(["/usr/bin/pkill", "-9", "-U", "601"]))
        XCTAssertFalse(FileManager.default.fileExists(atPath: root + Iso.runnerPlistPath), "the token goes with it")
        XCTAssertFalse(FileManager.default.fileExists(atPath: root + Iso.holderPlistPath))
    }

    func testUninstallRemovesEverythingButNeverAHomeThatIsNotTheUsers() throws {
        try FileManager.default.createDirectory(atPath: root + Iso.home + "/.claude", withIntermediateDirectories: true)
        try FileManager.default.createDirectory(atPath: root + Iso.binDir, withIntermediateDirectories: true)
        try FileManager.default.createDirectory(atPath: root + Iso.logDir, withIntermediateDirectories: true)
        var deleted = false
        let myUID = getuid()
        cmd.answer = { c in
            if c.starts(with: ["/usr/bin/dscl", ".", "-read", "/Users/tmuxedo"]) {
                return deleted ? (56, "", "") : (0, "UniqueID: \(myUID)\nPrimaryGroupID: 601", "")
            }
            if c.starts(with: ["/usr/bin/dscl", ".", "-delete", "/Users/tmuxedo"]) { deleted = true }
            return (0, "", "")
        }
        let r = core().handle(HelperRequest(.uninstall))
        XCTAssertTrue(r.ok, r.error ?? "")
        XCTAssertFalse(FileManager.default.fileExists(atPath: root + Iso.home))
        XCTAssertFalse(FileManager.default.fileExists(atPath: root + Iso.supportDir))
        XCTAssertFalse(FileManager.default.fileExists(atPath: root + Iso.logDir))
        XCTAssertTrue(cmd.ran(["/usr/bin/dscl", ".", "-delete", "/Users/tmuxedo"]))
        XCTAssertTrue(cmd.ran(["/usr/bin/dscl", ".", "-delete", "/Groups/tmuxedo"]))

        // A "home" that is someone else's (or a link): left alone, and said so.
        try FileManager.default.createDirectory(atPath: root + "/Users", withIntermediateDirectories: true)
        try FileManager.default.createSymbolicLink(atPath: root + Iso.home, withDestinationPath: "/Users/alex")
        deleted = false
        let r2 = core().handle(HelperRequest(.uninstall))
        XCTAssertFalse(r2.ok)
        XCTAssertTrue(r2.error?.contains("left alone") == true, r2.error ?? "")
    }

    func testImportRunsAsTheUserNeverAsRoot() throws {
        try FileManager.default.createDirectory(atPath: root + Iso.stagingDir + "/state", withIntermediateDirectories: true)
        cmd.answer = { c in c.starts(with: ["/usr/bin/dscl", ".", "-read", "/Users/tmuxedo"]) ? (0, "UniqueID: 601\nPrimaryGroupID: 601", "") : (0, "", "") }
        XCTAssertTrue(core().handle(HelperRequest(.importState)).ok)
        let copies = cmd.calls.filter { $0.contains("/usr/bin/ditto") }
        XCTAssertEqual(copies.count, 1)
        XCTAssertEqual(Array(copies[0].prefix(5)), ["/usr/bin/sudo", "-n", "-u", "tmuxedo", "--"], "with the user's own rights")
    }

    func testFreeIDs() {
        XCTAssertEqual(HelperCore.freeID(used: HelperCore.parseIDs("a 601\nb 602\nc 20")), 603)
        XCTAssertEqual(HelperCore.parseIDs("_www                 70\nroot   0"), [70, 0])
    }
}

// MARK: - The home lock and grants, planned

final class HomeLockPlanTests: XCTestCase {
    let home = "/Users/o"

    func plan(grants: [String], revoked: [String] = []) -> HomeLockPlan {
        let tree: [String: [String]] = [
            "/Users/o": [".ssh", "Documents", "git", ".zshrc", "notes.txt"],
            "/Users/o/git": ["proj", "other", "README"],
        ]
        return HomeLockPlan.make(home: home, grants: grants, revoked: revoked, user: uuidA,
                                 isDirectory: { !$0.hasSuffix(".zshrc") && !$0.hasSuffix(".txt") && !$0.hasSuffix("README") },
                                 list: { tree[$0] ?? [] })
    }

    func testNoGrantInTheHomeClosesTheWholeHome() {
        let p = plan(grants: ["/Volumes/Work/proj"])
        XCTAssertEqual(p.items.count, 1)
        XCTAssertEqual(p.items[0].path, home)
        XCTAssertEqual(p.items[0].entries.count, 1)
        XCTAssertFalse(p.items[0].entries[0].allow)
        XCTAssertTrue(p.items[0].entries[0].perms.contains(ACL_SEARCH), "not even passing through")
    }

    func testAGrantInTheHomeOpensOnlyThePathToIt() {
        let p = plan(grants: ["/Users/o/git/proj"])
        let paths = p.items.map(\.path)
        // Pass through the home and ~/git, without listing them.
        for a in ["/Users/o", "/Users/o/git"] {
            let e = p.items.first { $0.path == a }!.entries
            XCTAssertTrue(e.contains { $0.allow && $0.perms.map(\.rawValue) == [ACL_SEARCH.rawValue] }, a)
            XCTAssertTrue(e.contains { !$0.allow && $0.perms.contains(ACL_LIST_DIRECTORY) && $0.flags.isEmpty }, a)
            XCTAssertTrue(e.contains { !$0.allow && $0.flags.contains(ACL_ENTRY_ONLY_INHERIT) }, "new entries in \(a) are closed too")
        }
        // Everything else at those levels is closed; the grant itself carries no deny.
        for closed in ["/Users/o/.ssh", "/Users/o/Documents", "/Users/o/.zshrc", "/Users/o/notes.txt", "/Users/o/git/other", "/Users/o/git/README"] {
            XCTAssertTrue(paths.contains(closed), closed)
        }
        XCTAssertFalse(paths.contains("/Users/o/git/proj"))
        let zshrc = p.items.first { $0.path == "/Users/o/.zshrc" }!.entries[0]
        XCTAssertTrue(zshrc.perms.contains(ACL_READ_DATA), "a file is closed with file rights")
    }

    func testARevokedFolderIsClosedUnlessGrantedAgain() {
        XCTAssertTrue(plan(grants: [], revoked: ["/Volumes/W/p"]).items.contains { $0.path == "/Volumes/W/p" && !$0.entries[0].allow })
        XCTAssertFalse(plan(grants: ["/Volumes/W/p"], revoked: ["/Volumes/W/p"]).items.contains { $0.path == "/Volumes/W/p" })
    }

    func testTwoGrantsShareAncestors() {
        let p = plan(grants: ["/Users/o/git/proj", "/Users/o/git/other"])
        XCTAssertEqual(p.items.filter { $0.path == "/Users/o/git" }.count, 1)
        XCTAssertFalse(p.items.contains { $0.path == "/Users/o/git/other" })
        XCTAssertTrue(p.items.contains { $0.path == "/Users/o/git/README" })
    }
}

final class GrantPolicyTests: XCTestCase {
    let home = "/Users/o"

    func refusal(_ p: String, owned: Bool = true, dir: Bool = true) -> String? {
        GrantPolicy.refusal(p, home: home, owns: { _ in owned }, isDirectory: { _ in dir }, resolve: { $0 })
    }

    func testProjectFoldersAreAllowed() {
        XCTAssertNil(refusal("/Users/o/git/proj"))
        XCTAssertNil(refusal("/Volumes/Work/proj"))
        XCTAssertNil(refusal("/Users/Shared/team"))
    }

    func testNeverTheHomeSystemOrPrivatePlaces() {
        for p in ["/", "/Users", "/Users/o", "/Users/Shared", "/System/Library", "/Library/Keychains", "/usr/local",
                  "/private/etc", "/Applications/Xcode.app", "/etc", "/Users/o/.ssh", "/Users/o/.ssh/keys",
                  "/Users/o/Library/Keychains", "/Users/o/Documents/x", "/Users/o/.aws", "/Users/o/.claude",
                  "/Users/o/.config/gh", "/Users/tmuxedo", "/Users/tmuxedo/work"] {
            XCTAssertNotNil(refusal(p), p)
        }
    }

    func testNotSomeoneElsesAndNotAFile() {
        XCTAssertNotNil(refusal("/Users/other/proj", owned: false))
        XCTAssertNotNil(refusal("/Users/o/file.txt", dir: false))
        XCTAssertNotNil(refusal("relative"))
    }

    /// A link to a private place is judged by where it leads.
    func testLinksAreResolvedFirst() {
        XCTAssertNotNil(GrantPolicy.refusal("/Users/o/proj-link", home: home, owns: { _ in true }, isDirectory: { _ in true },
                                            resolve: { $0 == "/Users/o/proj-link" ? "/Users/o/.ssh" : $0 }))
    }

    func testClaudeSlugs() {
        XCTAssertEqual(IsolationManager.claudeSlug("/Users/o/git/my.proj_x"), "-Users-o-git-my-proj-x")
    }
}

// MARK: - The signer socket's admission

final class SignerAdmissionTests: XCTestCase {
    func peer(uid: uid_t, pid: pid_t) -> PeerInfo { PeerInfo(uid: uid, gid: 20, pid: pid, auditToken: nil) }

    func testOnlyTheRunnerPidOfTheUser() {
        var refreshed = 0
        let a = SignerAdmission(uid: 601, runnerPid: { refresh in if refresh { refreshed += 1 }; return refresh ? 4242 : 4000 },
                                requirement: nil)
        XCTAssertNotNil(a.check(peer(uid: 501, pid: 4242)), "another uid (the owner, anyone)")
        XCTAssertNotNil(a.check(peer(uid: 601, pid: 9999)), "a terminal: same uid, not the runner")
        XCTAssertNil(a.check(peer(uid: 601, pid: 4242)), "the runner, after asking launchd again")
        XCTAssertEqual(refreshed, 2)
        let none = SignerAdmission(uid: 601, runnerPid: { _ in nil }, requirement: nil)
        XCTAssertNotNil(none.check(peer(uid: 601, pid: 4242)), "no runner running: nobody")
    }

    func testASignedAppAlsoChecksTheCode() {
        let a = SignerAdmission(uid: 601, runnerPid: { _ in 4242 }, requirement: "anchor apple generic")
        XCTAssertNotNil(a.check(peer(uid: 601, pid: 4242)), "no audit token, no code check, no admission")
    }

    /// The real socket: a peer that is not admitted gets a refusal and nothing is signed.
    func testTheSocketRefusesBeforeReadingAByte() throws {
        let dir = URL(fileURLWithPath: "/tmp/tmx-sock-\(UUID().uuidString.prefix(6))")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        let path = dir.appendingPathComponent("s.sock").path
        var refusals: [String] = []
        let lock = NSLock()
        let myPid = getpid()
        let server = SignerSocketServer(path: path, key: SoftwareMachineKey(), lookup: "MINE", broker: ConfirmBroker(), group: nil,
                                        admit: { p in p.pid == myPid && p.uid == getuid() ? nil : "no" },
                                        onRefuse: { lock.lock(); refusals.append($0); lock.unlock() })
        try server.listen()
        defer { server.stop() }
        var st = stat()
        XCTAssertEqual(stat(path, &st), 0)
        XCTAssertEqual(st.st_mode & 0o777, 0o660)
        // This process is admitted (the predicate says so): a question is answered.
        XCTAssertEqual(try Self.ask(path, #"{"op":"confirm","pane":"p","what":"typing","timeout":30}"#).hasPrefix(#"{"ticket""#), true)
        // A child process (another pid): refused before anything is read.
        let p = Process()
        p.executableURL = URL(fileURLWithPath: "/usr/bin/python3")
        p.arguments = ["-c", """
        import socket,sys
        s=socket.socket(socket.AF_UNIX); s.connect(sys.argv[1]); s.sendall(b'{"op":"sign","input":"eA=="}\\n'); print(s.recv(200).decode())
        """, path]
        let out = Pipe()
        p.standardOutput = out
        try p.run()
        p.waitUntilExit()
        let answer = String(decoding: out.fileHandleForReading.readDataToEndOfFile(), as: UTF8.self)
        XCTAssertTrue(answer.contains("not the runner"), answer)
        Thread.sleep(forTimeInterval: 0.2)
        lock.lock(); XCTAssertEqual(refusals, ["no"]); lock.unlock()
    }

    static func ask(_ path: String, _ line: String) throws -> String {
        let fd = socket(AF_UNIX, SOCK_STREAM, 0)
        defer { close(fd) }
        var addr = sockaddr_un()
        addr.sun_family = sa_family_t(AF_UNIX)
        let bytes = Array(path.utf8)
        withUnsafeMutableBytes(of: &addr.sun_path) { raw in for (i, b) in bytes.enumerated() { raw[i] = b }; raw[bytes.count] = 0 }
        addr.sun_len = UInt8(MemoryLayout<sockaddr_un>.size)
        let rc = withUnsafePointer(to: &addr) { $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { connect(fd, $0, socklen_t(MemoryLayout<sockaddr_un>.size)) } }
        guard rc == 0 else { throw NSError(domain: NSPOSIXErrorDomain, code: Int(errno)) }
        let out = Array((line + "\n").utf8)
        _ = out.withUnsafeBytes { write(fd, $0.baseAddress, $0.count) }
        var buf = [UInt8](repeating: 0, count: 4096)
        let n = read(fd, &buf, buf.count)
        return n > 0 ? String(decoding: buf[0..<n], as: UTF8.self) : ""
    }
}

// MARK: - The runner as a daemon, seen by the supervisor

final class DaemonLauncherTests: XCTestCase {
    func testLogTailFollowsAppendsTruncationAndReplacement() throws {
        let f = tempDir().appendingPathComponent("runner.log").path
        try "old line\n".write(toFile: f, atomically: false, encoding: .utf8)
        let t = LogTail(path: f, fromEnd: true)
        XCTAssertEqual(t.read(), [], "the previous run's lines are skipped")
        let h = FileHandle(forWritingAtPath: f)!
        h.seekToEndOfFile()
        h.write(Data("one\ntw".utf8))
        XCTAssertEqual(t.read(), ["one"])
        h.write(Data("o\n".utf8))
        XCTAssertEqual(t.read(), ["two"], "a partial line waits for its end")
        try h.close()
        try "x\n".write(toFile: f, atomically: false, encoding: .utf8) // truncated
        XCTAssertEqual(t.read(), ["x"])
        let h2 = FileHandle(forWritingAtPath: f)!
        h2.seekToEndOfFile()
        h2.write(Data("a line longer than the file that replaces it\n".utf8))
        try h2.close()
        XCTAssertEqual(t.read(), ["a line longer than the file that replaces it"])
        try FileManager.default.removeItem(atPath: f)
        try "new file\n".write(toFile: f, atomically: true, encoding: .utf8) // replaced
        XCTAssertEqual(t.read(), ["new file"])
    }

    @MainActor
    func testTheSupervisorRunsTheDaemonLikeAChild() async throws {
        let helper = FakeHelper()
        let log = tempDir().appendingPathComponent("runner.log").path
        FileManager.default.createFile(atPath: log, contents: Data())
        let cfg = IsoRunnerConfig(server: "https://tmuxedo.test", token: "tmr.lookup.secret", allowUser: nil, observeOnly: false,
                                  confirm: false, confirmTimeout: 30, workDir: nil)
        let launcher = DaemonLauncher(helper: helper, logPath: log, pollInterval: 0.05, config: { cfg })
        let sup = Supervisor(launcher: launcher, schedule: { _, _ in }, makeSpec: {
            .success((LaunchSpec(executable: URL(fileURLWithPath: Iso.runnerPath), arguments: [], environment: [:], signerFD: nil), nil))
        })
        sup.start()
        XCTAssertEqual(helper.requests.first?.runner, cfg)
        XCTAssertEqual(launcher.runnerPid(refresh: false), 701)
        let h = FileHandle(forWritingAtPath: log)!
        h.seekToEndOfFile()
        h.write(Data("welcomed as machine m1 (user u1, web has events up to 0)\nremote control: allowed on this Mac\n".utf8))
        for _ in 0..<100 where !sup.display.isConnected { try await Task.sleep(nanoseconds: 20_000_000) }
        XCTAssertTrue(sup.display.isConnected, sup.logText)
        XCTAssertEqual(sup.remoteLog.last?.text, "remote control: allowed on this Mac")
        // The runner refuses for good (78): never restarted.
        h.write(Data("the gateway refused this machine's key proof\n".utf8))
        try h.close()
        helper.exitRunner(78)
        for _ in 0..<100 { if case .refused = sup.display { break }; try await Task.sleep(nanoseconds: 20_000_000) }
        guard case let .refused(why) = sup.display else { return XCTFail("\(sup.display)") }
        XCTAssertTrue(why.contains("key proof"))
        XCTAssertNil(launcher.runnerPid(refresh: false))
        XCTAssertEqual(helper.ops.filter { $0 == .startRunner }.count, 1)
    }

    @MainActor
    func testStopAsksTheHelperAndEndsCleanly() async throws {
        let helper = FakeHelper()
        let log = tempDir().appendingPathComponent("runner.log").path
        let cfg = IsoRunnerConfig(server: "https://tmuxedo.test", token: "tmr.lookup.secret", allowUser: nil, observeOnly: false,
                                  confirm: false, confirmTimeout: 30, workDir: nil)
        let launcher = DaemonLauncher(helper: helper, logPath: log, pollInterval: 0.05, config: { cfg })
        let sup = Supervisor(launcher: launcher, schedule: { _, _ in }, makeSpec: {
            .success((LaunchSpec(executable: URL(fileURLWithPath: Iso.runnerPath), arguments: [], environment: [:], signerFD: nil), nil))
        })
        sup.setIdle(paired: true)
        sup.start()
        sup.stop()
        for _ in 0..<100 where sup.isRunning { try await Task.sleep(nanoseconds: 20_000_000) }
        XCTAssertFalse(sup.isRunning)
        XCTAssertEqual(sup.display, .off)
        XCTAssertTrue(helper.ops.contains(.stopRunner))
    }

    func testTheHelperRefusingToStartIsAnError() {
        let helper = FakeHelper()
        helper.failing = [.startRunner]
        let launcher = DaemonLauncher(helper: helper, config: { nil })
        XCTAssertThrowsError(try launcher.launch(LaunchSpec(executable: URL(fileURLWithPath: "/x"), arguments: [], environment: [:], signerFD: nil),
                                                 line: { _ in }, exit: { _ in }), "not paired")
    }

    func testTheModeChoosesTheLauncher() throws {
        let child = FakeLauncher()
        let daemon = FakeLauncher()
        var mode = IsolationMode.fullAccess
        let s = SwitchingLauncher(child: child, daemon: daemon, mode: { mode })
        let spec = LaunchSpec(executable: URL(fileURLWithPath: "/bin/true"), arguments: [], environment: [:], signerFD: nil)
        _ = try s.launch(spec, line: { _ in }, exit: { _ in })
        mode = .separateUser
        _ = try s.launch(spec, line: { _ in }, exit: { _ in })
        XCTAssertEqual([child.launched.count, daemon.launched.count], [1, 1])
        XCTAssertThrowsError(try SwitchingLauncher(child: child, daemon: nil, mode: { .separateUser }).launch(spec, line: { _ in }, exit: { _ in }))
    }
}

// MARK: - The app in separate-user mode

@MainActor
final class IsolationModelTests: XCTestCase {
    var defaults: UserDefaults!
    var suite: String!
    var settings: Settings!
    var keys: MachineKeyStore!
    var helper: FakeHelper!
    var registry: FakeRegistry!
    var child: FakeLauncher!
    var iso: IsolationManager!
    var runnerBin: URL!

    override func setUp() async throws {
        suite = "dev.tmuxedo.mac.tests.isomodel.\(UUID().uuidString.prefix(8))"
        defaults = UserDefaults(suiteName: suite)!
        let mem = MemoryStore()
        settings = Settings(defaults: defaults, secrets: mem)
        keys = MachineKeyStore(store: mem, useEnclave: false)
        helper = FakeHelper()
        registry = FakeRegistry()
        child = FakeLauncher()
        iso = IsolationManager(settings: settings, registry: registry, helper: helper)
        iso.uuidOf = { $0 == Iso.user ? uuidA : uuidB }
        let home = tempDir().path
        iso.ownerHome = home
        iso.stagingDir = tempDir().appendingPathComponent("staging").path
        runnerBin = tempDir().appendingPathComponent("tmuxedo-runner")
        try FileManager.default.copyItem(at: URL(fileURLWithPath: "/usr/bin/true"), to: runnerBin)
        _ = Shell.run("/usr/bin/codesign", ["-f", "-s", "-", runnerBin.path])
    }

    override func tearDown() async throws {
        defaults.removePersistentDomain(forName: suite)
    }

    func model(daemon: DaemonLauncher? = nil) -> AppModel {
        let m = AppModel(settings: settings, keys: keys, binary: RunnerBinary(url: runnerBin), holder: nil, launcher: child,
                         profile: nil, iso: iso, daemonLauncher: daemon, schedule: { _, _ in })
        m.scanMigration = { Migration.Found() }
        m.isoUID = { 601 }
        m.isoGID = { getegid() }
        m.signerSocketPath = "/tmp/tmx-signer-\(UUID().uuidString.prefix(6)).sock"
        return m
    }

    func pair() throws {
        let k = try keys.create()
        settings.storePairing(PairedMachine(machineId: "m1", name: "Mac", server: "https://tmuxedo.test", method: "phone",
                                            pairedAt: Date(), keyBound: true, keyFingerprint: k.fingerprint, userId: "u1"),
                              token: "tmr.LOOKUP.secret")
    }

    func testNotReadyIsBrokenWithAWayOut() throws {
        try pair()
        settings.isolation = .separateUser
        let m = model()
        guard case let .failure(.broken(why)) = m.prepareSpec() else { return XCTFail() }
        XCTAssertTrue(why.contains("Settings, Isolation"), why)
        XCTAssertTrue(child.launched.isEmpty)
    }

    func testReadyStartsTheDaemonWithASignerSocketAndNoChild() async throws {
        try pair()
        settings.isolation = .separateUser
        settings.confirmOnMac = true
        registry.status = .enabled
        await iso.refresh()
        XCTAssertEqual(iso.phase, .ready)
        let launcher = DaemonLauncher(helper: helper, logPath: tempDir().appendingPathComponent("l").path, pollInterval: 0.05,
                                      config: { nil })
        let m = model(daemon: launcher)
        guard case let .success((spec, signer)) = m.prepareSpec() else { return XCTFail() }
        XCTAssertTrue(signer is SignerSocketServer)
        XCTAssertEqual(spec.executable.path, Iso.runnerPath)
        XCTAssertTrue(spec.arguments.contains("--confirm-on-mac"))
        XCTAssertTrue(spec.environment.isEmpty, "nothing secret on a child command line: the helper writes a root-only plist")
        signer?.stop()
        let cfg = try XCTUnwrap(m.isoConfig())
        XCTAssertEqual(cfg.allowUser, "u1")
        XCTAssertTrue(cfg.confirm)
        XCTAssertEqual(m.runnerSocket.path, Iso.apiSocket)
    }

    func testAWorkFolderOutsideTheGrantsIsNotUsed() throws {
        try pair()
        settings.workDir = "/Users/o/git/proj/sub"
        let m = model()
        XCTAssertNil(m.isoConfig()?.workDir, "the user could not reach it")
        settings.grantState = GrantState(grants: ["/Users/o/git/proj"])
        XCTAssertEqual(m.isoConfig()?.workDir, "/Users/o/git/proj/sub")
    }

    func testABearerTokenIsNotOfferedTheSeparateUser() async throws {
        settings.storePairing(PairedMachine(machineId: "m1", name: "Mac", server: "https://tmuxedo.test", method: "cli",
                                            pairedAt: Date(), keyBound: false, keyFingerprint: nil), token: "tmr.old.bearer")
        settings.isolation = .separateUser
        registry.status = .enabled
        await iso.refresh()
        guard case let .failure(.broken(why)) = model().prepareSpec() else { return XCTFail() }
        XCTAssertTrue(why.contains("pair again"))
    }

    func testFullAccessStillRunsTheChild() throws {
        try pair()
        settings.isolation = .fullAccess
        let m = model()
        m.start()
        XCTAssertEqual(child.launched.count, 1)
        XCTAssertEqual(child.launched[0].environment["TMUXEDO_RUNNER_TOKEN"], "tmr.LOOKUP.secret")
        m.supervisor.terminateNow()
    }

    func testSetUpNeedsTheAdministratorsApprovalFirst() async {
        registry.becomes = .requiresApproval
        let ok = await iso.setUp()
        XCTAssertFalse(ok)
        XCTAssertEqual(iso.phase, .needsApproval)
        XCTAssertTrue(iso.lastError?.contains("Login Items") == true)
        XCTAssertTrue(helper.ops.isEmpty, "nothing runs as root before macOS allows the helper")
    }

    func testAHelperFailureIsReportedNotHidden() async {
        registry.status = .enabled
        helper.failing = [.setup]
        let ok = await iso.setUp()
        XCTAssertFalse(ok)
        XCTAssertTrue(iso.lastError?.contains("setup failed on purpose") == true, iso.lastError ?? "")
    }

    /// Migration: stops the runner, stages the owner's state and the Claude sessions of granted
    /// folders, imports them as the user, ends the owner's holder, switches, starts as the user.
    func testMovingAnExistingInstallKeepsSessionsResumable() async throws {
        try pair()
        settings.isolation = .fullAccess
        registry.status = .enabled
        await iso.refresh()
        let state = iso.ownerStateDir
        try FileManager.default.createDirectory(atPath: state, withIntermediateDirectories: true)
        try "{}".write(toFile: state + "/state.json", atomically: true, encoding: .utf8)
        try "x".write(toFile: state + "/runner.log", atomically: true, encoding: .utf8)
        let proj = "/Users/o/git/proj"
        settings.grantState = GrantState(grants: [proj])
        let sessions = iso.claudeProjects + "/" + IsolationManager.claudeSlug(proj)
        try FileManager.default.createDirectory(atPath: sessions, withIntermediateDirectories: true)
        try "{}".write(toFile: sessions + "/abc.jsonl", atomically: true, encoding: .utf8)
        try FileManager.default.createDirectory(atPath: iso.claudeProjects + "/-Users-o-secret", withIntermediateDirectories: true)
        var staged: [String] = []
        helper.lock.lock(); helper.requests = []; helper.lock.unlock()
        weak var ref: AppModel?
        let launcher = DaemonLauncher(helper: helper, logPath: tempDir().appendingPathComponent("l").path, pollInterval: 0.05,
                                      config: { MainActor.assumeIsolated { ref?.isoConfig() } })
        let m = model(daemon: launcher)
        ref = m
        // Look at the staging folder at the moment the helper imports it.
        let watcher = StagingWatcher(path: iso.stagingDir)
        helper.onImport = { staged = watcher.list() }
        m.start()
        XCTAssertEqual(child.launched.count, 1, "running in full access before the move")
        let ok = await m.moveToSeparateUser()
        XCTAssertTrue(ok, m.lastMessage ?? "")
        XCTAssertEqual(settings.isolation, .separateUser)
        XCTAssertTrue(staged.contains("state/state.json"), "\(staged)")
        XCTAssertFalse(staged.contains("state/runner.log"))
        XCTAssertTrue(staged.contains("claude-projects/\(IsolationManager.claudeSlug(proj))/abc.jsonl"), "\(staged)")
        XCTAssertFalse(staged.contains { $0.contains("secret") }, "only granted folders' sessions")
        XCTAssertFalse(FileManager.default.fileExists(atPath: iso.stagingDir), "staging removed")
        XCTAssertTrue(helper.ops.contains(.importState))
        XCTAssertTrue(helper.ops.contains(.startRunner), "started as the user")
        XCTAssertEqual(child.launched.count, 1, "no child runner any more")
        m.supervisor.terminateNow()
    }

    func testAFailedImportChangesNothing() async throws {
        try pair()
        settings.isolation = .fullAccess
        registry.status = .enabled
        await iso.refresh()
        helper.failing = [.importState]
        let m = model()
        let ok = await m.moveToSeparateUser()
        XCTAssertFalse(ok)
        XCTAssertEqual(settings.isolation, .fullAccess)
        XCTAssertTrue(m.lastMessage?.contains("Nothing was changed") == true)
    }

    func testSwitchingToFullAccessStopsTheUsersTerminals() async throws {
        try pair()
        settings.isolation = .separateUser
        registry.status = .enabled
        await iso.refresh()
        let m = model()
        await m.switchToFullAccess()
        XCTAssertEqual(settings.isolation, .fullAccess)
        XCTAssertTrue(helper.ops.contains(.stopAll))
        XCTAssertEqual(child.launched.count, 1, "now the app's child")
        m.supervisor.terminateNow()
    }

    func testRemovingTheUserUnregistersTheHelperAndForgetsTheGrants() async throws {
        settings.isolation = .separateUser
        registry.status = .enabled
        settings.grantState = GrantState(grants: [], revoked: [], manifest: [], locked: false)
        let m = model()
        let ok = await m.removeSeparateUser()
        XCTAssertTrue(ok)
        XCTAssertTrue(helper.ops.contains(.uninstall))
        XCTAssertEqual(registry.unregistered, 1)
        XCTAssertEqual(settings.isolation, .fullAccess)
        XCTAssertEqual(settings.grantState, GrantState())
        XCTAssertEqual(iso.phase, .notSetUp)
    }

    func testTheKillSwitchStopsTheSeparateUserToo() async throws {
        try pair()
        registry.status = .enabled
        let m = model()
        m.killSwitch()
        for _ in 0..<50 where !helper.ops.contains(.stopAll) { try await Task.sleep(nanoseconds: 20_000_000) }
        XCTAssertTrue(helper.ops.contains(.stopAll))
        XCTAssertFalse(settings.isPaired)
    }

    func testAProfileCopyNeverTouchesTheSeparateUser() {
        let m = AppModel(settings: settings, keys: keys, binary: RunnerBinary(url: runnerBin), holder: nil, launcher: child,
                         profile: "verify", iso: iso, daemonLauncher: nil, schedule: { _, _ in })
        XCTAssertNil(m.iso)
        XCTAssertEqual(m.mode, .fullAccess)
    }
}

/// Lists a folder's files (relative paths) when asked.
final class StagingWatcher {
    let path: String
    init(path: String) { self.path = path }
    func list() -> [String] {
        (FileManager.default.enumerator(atPath: path)?.allObjects as? [String] ?? []).filter {
            var d: ObjCBool = false
            return FileManager.default.fileExists(atPath: path + "/" + $0, isDirectory: &d) && !d.boolValue
        }
    }
}
