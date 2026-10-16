import XCTest

/// The app's behaviour without a window: what the runner is started with, pairing, Unpair,
/// the kill switch, migration, settings and the dashboard snapshot.
@MainActor
final class AppModelTests: XCTestCase {
    var defaults: UserDefaults!
    var suite: String!
    var mem: MemoryStore!
    var settings: Settings!
    var keys: MachineKeyStore!
    var launcher: FakeLauncher!
    var clock: ManualScheduler!
    var runnerBin: URL!

    override func setUp() async throws {
        suite = "dev.tmuxedo.mac.tests.\(UUID().uuidString.prefix(8))"
        defaults = UserDefaults(suiteName: suite)!
        mem = MemoryStore()
        settings = Settings(defaults: defaults, secrets: mem)
        keys = MachineKeyStore(store: mem, useEnclave: false)
        launcher = FakeLauncher()
        clock = ManualScheduler()
        FakeTmuxedo.reset()
        // An ad-hoc signed stand-in for the bundled runner (the test bundle is unsigned too).
        runnerBin = tempDir().appendingPathComponent("tmuxedo-runner")
        try FileManager.default.copyItem(at: URL(fileURLWithPath: "/usr/bin/true"), to: runnerBin)
        _ = Shell.run("/usr/bin/codesign", ["-f", "-s", "-", runnerBin.path])
    }

    override func tearDown() async throws {
        defaults.removePersistentDomain(forName: suite)
    }

    private func model(binary: Bool = true, profile: String? = nil) -> AppModel {
        let m = AppModel(settings: settings, keys: keys, binary: binary ? RunnerBinary(url: runnerBin) : nil, holder: nil,
                         launcher: launcher, profile: profile, schedule: { [unowned self] d, f in self.clock.schedule(d, f) })
        m.api = { PairingAPI(server: FakeTmuxedo.base, session: FakeTmuxedo.session()) }
        m.scanMigration = { Migration.Found() }
        m.launchctl = { _ in 0 }
        return m
    }

    private func pairKeyBound() throws -> MachineKey {
        let k = try keys.create()
        let token = "tmr.LOOKUP.secret"
        FakeTmuxedo.state.with { $0.tokens[token] = k.publicJWK }
        settings.storePairing(PairedMachine(machineId: "m1", name: "Mac", server: "https://tmuxedo.test", method: "phone",
                                            pairedAt: Date(), keyBound: true, keyFingerprint: k.fingerprint), token: token)
        return k
    }

    func testNotPairedNeverStarts() {
        let m = model()
        XCTAssertEqual(m.supervisor.display, .notPaired)
        m.start()
        XCTAssertTrue(launcher.launched.isEmpty)
        guard case .failure(.notPaired) = m.prepareSpec() else { return XCTFail() }
    }

    func testAKeyBoundPairingStartsTheManagedRunnerWithASigner() throws {
        _ = try pairKeyBound()
        settings.workDir = "~/code"
        let m = model()
        XCTAssertEqual(m.supervisor.display, .off)
        m.start()
        let spec = try XCTUnwrap(launcher.launched.first)
        XCTAssertEqual(spec.executable, runnerBin)
        XCTAssertEqual(Array(spec.arguments.prefix(3)), ["run", "--managed", "--no-start-holder"])
        XCTAssertEqual(spec.arguments.last, (NSHomeDirectory() as NSString).appendingPathComponent("code"))
        XCTAssertEqual(spec.environment["TMUXEDO_RUNNER_TOKEN"], "tmr.LOOKUP.secret")
        XCTAssertEqual(spec.environment["TMUXEDO_SERVER"], "https://tmuxedo.test")
        XCTAssertNotNil(spec.signerFD)
        m.supervisor.terminateNow()
    }

    /// The key that signs must be the one the pairing recorded.
    func testAMismatchedOrMissingKeyIsBroken() throws {
        _ = try pairKeyBound()
        _ = try keys.create() // replaced behind the pairing's back
        guard case let .failure(.broken(why)) = model().prepareSpec() else { return XCTFail() }
        XCTAssertTrue(why.contains("does not match"))
        keys.delete()
        guard case let .failure(.broken(why2)) = model().prepareSpec() else { return XCTFail() }
        XCTAssertTrue(why2.contains("key is gone"))
    }

    func testAMissingBinaryIsBroken() throws {
        _ = try pairKeyBound()
        guard case let .failure(.broken(why)) = model(binary: false).prepareSpec() else { return XCTFail() }
        XCTAssertTrue(why.contains("missing"))
    }

    /// An adopted install.sh enrolment: bearer token, no signer, the CLI's file scrubbed.
    func testAdoptingAnEnrolment() throws {
        let cfg = tempDir().appendingPathComponent("runner.toml")
        try "server = \"https://tmuxedo.test\"\ntoken = \"tmr.old.bearer\"\n".write(to: cfg, atomically: true, encoding: .utf8)
        let m = model()
        m.adopt(Migration.Enrolment(server: "https://tmuxedo.test", token: "tmr.old.bearer", name: "box", configPath: cfg))
        XCTAssertTrue(settings.isPaired)
        XCTAssertEqual(settings.machine?.keyBound, false)
        let spec = try XCTUnwrap(launcher.launched.first)
        XCTAssertNil(spec.signerFD)
        XCTAssertNil(spec.environment["TMUXEDO_SIGNER_FD"])
        XCTAssertFalse(try String(contentsOf: cfg, encoding: .utf8).contains("tmr.old.bearer"))
        m.supervisor.terminateNow()
    }

    /// An install.sh runner still installed: retired before ours starts (two would replace each other).
    func testStartRetiresAnInstallShRunnerFirst() throws {
        _ = try pairKeyBound()
        let dir = tempDir()
        let r = dir.appendingPathComponent("dev.tmuxedo.runner.plist")
        try "x".write(to: r, atomically: true, encoding: .utf8)
        let m = model()
        var boots: [[String]] = []
        m.launchctl = { boots.append($0); return 0 }
        m.scanMigration = { FileManager.default.fileExists(atPath: r.path) ? Migration.Found(runnerPlists: [r]) : Migration.Found() }
        m.boot()
        XCTAssertTrue(launcher.launched.isEmpty, "boot does not race an install.sh runner")
        m.start()
        XCTAssertEqual(boots.first?.first, "bootout")
        XCTAssertFalse(FileManager.default.fileExists(atPath: r.path))
        XCTAssertEqual(launcher.launched.count, 1)
        m.supervisor.terminateNow()
    }

    func testBootConnectsWhenPairedAndAsked() throws {
        _ = try pairKeyBound()
        settings.connectAtLaunch = false
        model().boot()
        XCTAssertTrue(launcher.launched.isEmpty)
        settings.connectAtLaunch = true
        let m = model()
        m.boot()
        XCTAssertEqual(launcher.launched.count, 1)
        m.supervisor.terminateNow()
    }

    /// The account is pinned at the first connection and passed to the runner from then on.
    func testTheAccountIsLearnedOnceAndEnforcedByTheRunner() throws {
        _ = try pairKeyBound()
        settings.observeOnly = true
        let m = model()
        m.start()
        XCTAssertFalse(launcher.launched[0].arguments.contains("--allow-user"))
        XCTAssertTrue(launcher.launched[0].arguments.contains("--observe-only"))
        launcher.last.say("welcomed as machine m1 (user u-owner, web has events up to 0)")
        drainMain(self)
        XCTAssertEqual(settings.machine?.userId, "u-owner")
        settings.learnAccount(machineId: "m1", userId: "u-other")
        XCTAssertEqual(settings.machine?.userId, "u-owner", "never re-learned")
        m.supervisor.restart()
        drainMain(self)
        let args = launcher.launched.last!.arguments
        XCTAssertEqual(Array(args.drop(while: { $0 != "--allow-user" }).prefix(2)), ["--allow-user", "u-owner"])
        m.supervisor.terminateNow()
    }

    func testPairedStoresAndStarts() throws {
        let k = try keys.create()
        let m = model()
        m.paired(PairedMachine(machineId: "m9", name: "Mac", server: "https://tmuxedo.test", method: "browser",
                               pairedAt: Date(), keyBound: true, keyFingerprint: k.fingerprint), token: "tmr.L9.s")
        XCTAssertTrue(settings.isPaired)
        XCTAssertEqual(settings.runnerToken(), "tmr.L9.s")
        XCTAssertEqual(launcher.launched.count, 1)
        XCTAssertEqual(m.lastMessage, "Paired as Mac (browser).")
        m.supervisor.terminateNow()
    }

    /// Unpair: the server revokes (signed by the key), this Mac forgets token and key.
    func testUnpairRevokesOnTheServerAndForgetsHere() async throws {
        _ = try pairKeyBound()
        let m = model()
        m.start()
        await m.unpair()
        XCTAssertTrue(FakeTmuxedo.state.with { $0.revoked.contains("tmr.LOOKUP.secret") })
        XCTAssertFalse(settings.isPaired)
        XCTAssertNil(settings.runnerToken())
        XCTAssertFalse(keys.exists)
        XCTAssertEqual(m.supervisor.display, .notPaired)
    }

    func testUnpairOfflineStillForgetsAndSaysSo() async throws {
        _ = try pairKeyBound()
        FakeTmuxedo.state.with { $0.unreachable = true }
        let m = model()
        await m.unpair()
        XCTAssertFalse(settings.isPaired)
        XCTAssertTrue(m.supervisor.log.contains { $0.text.contains("revoke this Mac on tmuxedo.dev") })
    }

    /// The kill switch works with no server at all.
    func testKillSwitch() throws {
        _ = try pairKeyBound()
        FakeTmuxedo.state.with { $0.unreachable = true }
        let m = model()
        m.start()
        m.killSwitch()
        drainMain(self)
        XCTAssertEqual(launcher.processes[0].terminated.count, 1)
        XCTAssertFalse(settings.isPaired)
        XCTAssertFalse(keys.exists)
        XCTAssertEqual(m.supervisor.display, .notPaired)
        XCTAssertTrue(m.lastMessage?.contains("Kill switch") ?? false)
    }

    func testPushRegistersOnlyForAKeyBoundPairingWithNotificationsOn() async throws {
        _ = try pairKeyBound()
        let m = model()
        settings.notifications = false
        m.apnsTokenArrived(Data(repeating: 0xab, count: 32), sandbox: false)
        try await Task.sleep(nanoseconds: 200_000_000)
        XCTAssertEqual(FakeTmuxedo.state.with { $0.pushDevices.count }, 0)
        settings.notifications = true
        m.registerPushIfPossible()
        for _ in 0..<50 where m.pushStatus != "Registered" { try await Task.sleep(nanoseconds: 20_000_000) }
        XCTAssertEqual(m.pushStatus, "Registered")
        XCTAssertEqual(FakeTmuxedo.state.with { Array($0.pushDevices.keys) }, [String(repeating: "ab", count: 32)])
    }

    func testSnapshotNotifiesEachNewlyBlockedAgentOnce() async throws {
        _ = try pairKeyBound()
        let m = model()
        m.start()
        var notified: [String] = []
        m.onNeedsYou = { notified.append($0.id) }
        var blocked: [LocalAPI.Agent] = []
        m.readSnapshot = { _ in .success(LocalAPI.Snapshot(workspaces: [], agents: blocked)) }
        let a = LocalAPI.Agent(id: "w1:p1", workspaceId: "w1", kind: "claude", name: nil, title: nil, cwd: nil, status: .blocked)
        await m.refreshSnapshot()
        blocked = [a]
        await m.refreshSnapshot()
        await m.refreshSnapshot()
        XCTAssertEqual(notified, ["w1:p1"])
        blocked = []
        await m.refreshSnapshot()
        blocked = [a]
        await m.refreshSnapshot()
        XCTAssertEqual(notified, ["w1:p1", "w1:p1"], "blocked again later: notified again")
        m.readSnapshot = { _ in .failure(LocalAPI.CallError.unavailable("the runner is not running")) }
        await m.refreshSnapshot()
        XCTAssertEqual(m.snapshotError, "the runner is not running")
        m.supervisor.terminateNow()
        await drainMainAsync()
        await m.refreshSnapshot()
        XCTAssertEqual(m.snapshot, .empty)
    }

    func testKeepAwakeFollowsTheConnection() throws {
        _ = try pairKeyBound()
        let m = model()
        settings.keepAwake = true
        m.start()
        XCTAssertTrue(m.sleepGuard.isHeld)
        m.supervisor.terminateNow()
        drainMain(self)
        XCTAssertFalse(m.sleepGuard.isHeld)
        settings.keepAwake = false
        m.start()
        XCTAssertFalse(m.sleepGuard.isHeld)
        m.supervisor.terminateNow()
    }
}

@MainActor
final class SettingsTests: XCTestCase {
    func testDefaultsAndPersistence() {
        let suite = "dev.tmuxedo.mac.tests.s\(UUID().uuidString.prefix(6))"
        let d = UserDefaults(suiteName: suite)!
        defer { d.removePersistentDomain(forName: suite) }
        let mem = MemoryStore()
        let s = Settings(defaults: d, secrets: mem)
        XCTAssertEqual(s.serverURL, "https://tmuxedo.dev")
        XCTAssertTrue(s.connectAtLaunch)
        XCTAssertFalse(s.keepAwake)
        XCTAssertTrue(s.notifications)
        XCTAssertFalse(s.isPaired)
        s.workDir = "~/work/x"
        s.keepAwake = true
        let m = PairedMachine(machineId: "m", name: "Mac", server: "https://tmuxedo.dev", method: "phone", pairedAt: Date(timeIntervalSince1970: 5),
                              keyBound: true, keyFingerprint: "fp")
        s.storePairing(m, token: "tmr.a.b")
        let again = Settings(defaults: d, secrets: mem)
        XCTAssertEqual(again.workDir, "~/work/x")
        XCTAssertTrue(again.keepAwake)
        XCTAssertEqual(again.machine, m)
        XCTAssertTrue(again.isPaired)
        again.forgetPairing()
        XCTAssertFalse(Settings(defaults: d, secrets: mem).isPaired)
        XCTAssertNil(mem.read(SecretItem.runnerToken))
    }

    func testServerMustBeHTTPSExceptLoopback() {
        let s = Settings(defaults: UserDefaults(suiteName: "t\(UUID().uuidString.prefix(6))")!, secrets: MemoryStore())
        for (u, ok) in [("https://tmuxedo.dev", true), ("http://tmuxedo.dev", false), ("http://127.0.0.1:3000", true),
                        ("http://localhost:3000", true), ("ftp://x", false), ("not a url", false), ("", false)] {
            s.serverURL = u
            XCTAssertEqual(s.server != nil, ok, u)
        }
        s.serverURL = "https://tmuxedo.dev"
        XCTAssertEqual(s.webURL("/machines/m1")?.absoluteString, "https://tmuxedo.dev/machines/m1")
    }

    func testWorkDirResolution() {
        let s = Settings(defaults: UserDefaults(suiteName: "t\(UUID().uuidString.prefix(6))")!, secrets: MemoryStore())
        XCTAssertNil(s.resolvedWorkDir)
        s.workDir = "~/code"
        XCTAssertEqual(s.resolvedWorkDir, (NSHomeDirectory() as NSString).appendingPathComponent("code"))
        s.workDir = "/opt/w"
        XCTAssertEqual(s.resolvedWorkDir, "/opt/w")
        s.workDir = "relative/path"
        XCTAssertNil(s.resolvedWorkDir, "a relative path is not guessed at")
        XCTAssertTrue(Settings.childPath.hasPrefix(NSHomeDirectory() + "/.local/bin"))
    }
}
