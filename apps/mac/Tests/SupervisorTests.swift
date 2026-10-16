import Darwin
import XCTest

/// The supervisor's state machine, edge by edge (Supervisor.swift header), with a fake
/// launcher and a hand-driven clock.
@MainActor
final class SupervisorTests: XCTestCase {
    var launcher: FakeLauncher!
    var clock: ManualScheduler!
    var specResult: Result<(LaunchSpec, SignerEndpoint?), SupervisorError>!
    var sup: Supervisor!

    override func setUp() async throws {
        launcher = FakeLauncher()
        clock = ManualScheduler()
        specResult = .success((LaunchSpec(executable: URL(fileURLWithPath: "/bin/true"), arguments: ["run"], environment: [:], signerFD: nil), nil))
        sup = Supervisor(launcher: launcher, now: { [unowned self] in self.clock.now },
                         schedule: { [unowned self] d, f in self.clock.schedule(d, f) },
                         makeSpec: { [unowned self] in self.specResult })
    }

    func testStartConnectStop() {
        sup.setIdle(paired: true)
        XCTAssertEqual(sup.display, .off)
        sup.start()
        XCTAssertEqual(sup.display, .connecting(attempt: 1))
        XCTAssertEqual(launcher.launched.count, 1)
        launcher.last.say("2026-10-05T12:00:00Z connected to wss://tmuxedo.test/gw/runner")
        drainMain(self)
        XCTAssertEqual(sup.display, .connecting(attempt: 1), "an open socket is not admission")
        launcher.last.say("2026-10-05T12:00:01Z welcomed as machine m1 (user u1, web has events up to 0)")
        drainMain(self)
        XCTAssertEqual(sup.display, .connected(since: clock.now))
        sup.start() // already running: no second process
        XCTAssertEqual(launcher.launched.count, 1)
        sup.stop()
        drainMain(self)
        XCTAssertEqual(launcher.processes[0].terminated, [5])
        XCTAssertEqual(sup.display, .off)
        XCTAssertFalse(sup.isRunning)
    }

    func testDisconnectGoesBackToConnecting() {
        sup.start()
        launcher.last.say("welcomed as machine m1 (user u1, web has events up to 0)")
        launcher.last.say("gateway disconnected (code None: eof); reconnecting in 2.0s")
        drainMain(self)
        XCTAssertEqual(sup.display, .connecting(attempt: 1))
        launcher.last.say("welcomed as machine m1 (user u1, web has events up to 0)")
        drainMain(self)
        XCTAssertTrue(sup.display.isConnected)
    }

    func testCrashRestartsWithDoublingBackoffThatResetsAfterAHealthyRun() {
        sup.start()
        var waits: [TimeInterval] = []
        for _ in 0..<8 {
            launcher.last.finish(1)
            drainMain(self)
            guard case let .unreachable(_, _, retry) = sup.display else { return XCTFail("\(sup.display)") }
            waits.append(retry)
            clock.advance(retry)
        }
        XCTAssertEqual(waits, [1, 2, 4, 8, 16, 32, 60, 60])
        XCTAssertEqual(launcher.launched.count, 9)
        // A run that lasted a minute resets the backoff.
        clock.advance(61)
        launcher.last.finish(1)
        drainMain(self)
        guard case let .unreachable(_, _, retry) = sup.display else { return XCTFail() }
        XCTAssertEqual(retry, 1)
        XCTAssertGreaterThanOrEqual(sup.restarts, 9)
    }

    func testExit78IsARefusalAndIsNeverRetried() {
        sup.start()
        launcher.last.say("the gateway refused this machine's key proof; pair this machine again")
        launcher.last.say("exiting without restart (close code 4003)")
        launcher.last.finish(78)
        drainMain(self)
        guard case let .refused(why) = sup.display else { return XCTFail("\(sup.display)") }
        XCTAssertTrue(why.contains("key proof"))
        XCTAssertTrue(clock.pending.isEmpty, "a refused credential must not be retried")
        XCTAssertEqual(launcher.launched.count, 1)
    }

    func testWelcomeTeachesTheAccountAndAttachesAreLogged() {
        var seen: [(String, String)] = []
        sup.onWelcome = { seen.append(($0, $1)) }
        sup.start()
        launcher.last.say("2026-10-05T12:00:00Z welcomed as machine m-1 (user u-9, web has events up to 0)")
        launcher.last.say("remote attach: pane w1:p1")
        launcher.last.say("remote decision for pane w1:p1: allow")
        launcher.last.say("refused pane.send_text from tmuxedo: this machine is observe-only")
        launcher.last.say("something else")
        drainMain(self)
        XCTAssertEqual(seen.map(\.0), ["m-1"])
        XCTAssertEqual(seen.map(\.1), ["u-9"])
        XCTAssertEqual(sup.remoteLog.map(\.text), ["remote attach: pane w1:p1", "remote decision for pane w1:p1: allow",
                                                   "refused pane.send_text from tmuxedo: this machine is observe-only"])
        XCTAssertNil(Supervisor.parseWelcome("welcomed as machine  (user , x)"))
        XCTAssertNil(Supervisor.parseWelcome("hello"))
        XCTAssertTrue(sup.refusalReason(from: ["tmuxedo connected this machine to account u2, but it serves only u1; refusing"]).contains("different account"))
    }

    func testRefusalReasons() {
        XCTAssertTrue(sup.refusalReason(from: ["exiting without restart (close code 4003)"]).contains("revoked"))
        XCTAssertTrue(sup.refusalReason(from: ["exiting without restart (close code 4001)"]).contains("replaced"))
        XCTAssertTrue(sup.refusalReason(from: ["the gateway refused this machine's key proof"]).contains("key proof"))
    }

    func testStopDuringBackoffCancelsTheRestart() {
        sup.start()
        launcher.last.finish(1)
        drainMain(self)
        XCTAssertFalse(clock.pending.isEmpty)
        sup.stop()
        XCTAssertEqual(sup.display, .off)
        clock.advance(120)
        XCTAssertEqual(launcher.launched.count, 1, "the cancelled restart still ran")
    }

    func testRestartStartsAgainAtOnce() {
        sup.start()
        sup.restart()
        drainMain(self)
        XCTAssertEqual(launcher.launched.count, 2)
        XCTAssertEqual(sup.display, .connecting(attempt: 1))
        XCTAssertEqual(sup.restarts, 1)
    }

    func testBrokenOrUnpairedNeverLaunches() {
        specResult = .failure(.broken("the bundled tmuxedo-runner is missing"))
        sup.start()
        XCTAssertEqual(sup.display, .broken("the bundled tmuxedo-runner is missing"))
        XCTAssertTrue(launcher.launched.isEmpty)
        specResult = .failure(.notPaired)
        sup.start()
        XCTAssertEqual(sup.display, .notPaired)
        XCTAssertTrue(sup.log.contains { $0.kind == .error })
    }

    func testALaunchFailureBacksOff() {
        launcher.fail = NSError(domain: NSPOSIXErrorDomain, code: Int(ENOENT))
        sup.start()
        guard case .unreachable = sup.display else { return XCTFail("\(sup.display)") }
        XCTAssertEqual(clock.delays, [1])
    }

    func testCountsSignedProofsAndCapsTheLog() {
        sup.start()
        launcher.last.say("answered the gateway's key challenge")
        launcher.last.say("answered the gateway's key challenge")
        for i in 0..<(Supervisor.maxLog + 10) { launcher.last.say("line \(i)") }
        drainMain(self)
        XCTAssertEqual(sup.signedProofs, 2)
        XCTAssertEqual(sup.log.count, Supervisor.maxLog)
        XCTAssertTrue(sup.logText.contains("line \(Supervisor.maxLog + 9)"))
        sup.clearLog()
        XCTAssertTrue(sup.log.isEmpty)
    }

    func testTerminateNowAtQuitNeverRestarts() {
        sup.start()
        sup.terminateNow()
        drainMain(self)
        XCTAssertEqual(launcher.processes[0].terminated, [2])
        XCTAssertEqual(sup.display, .off)
        clock.advance(120)
        XCTAssertEqual(launcher.launched.count, 1)
    }

    /// The spec the app hands the runner: managed, no holder of its own, the credential in the
    /// environment (never argv), the signer on fd 3.
    func testRunnerLaunchSpec() {
        let s = LaunchSpec.runner(binary: URL(fileURLWithPath: "/A/tmuxedo-runner"), server: "https://tmuxedo.dev",
                                  token: "tmr.l.s", signerFD: 9, workDir: "/Users/me/work", runtimeDir: nil, home: "/Users/me")
        XCTAssertEqual(s.arguments, ["run", "--managed", "--no-start-holder", "--work-dir", "/Users/me/work"])
        XCTAssertFalse(s.arguments.joined().contains("tmr."), "the token must not be visible in ps")
        XCTAssertEqual(s.environment["TMUXEDO_RUNNER_TOKEN"], "tmr.l.s")
        XCTAssertEqual(s.environment["TMUXEDO_SERVER"], "https://tmuxedo.dev")
        XCTAssertEqual(s.environment["TMUXEDO_SIGNER_FD"], "3")
        XCTAssertEqual(s.environment["HOME"], "/Users/me")
        XCTAssertTrue(s.environment["PATH"]!.contains("/opt/homebrew/bin"))
        XCTAssertEqual(s.signerFD, 9)
        let guarded = LaunchSpec.runner(binary: URL(fileURLWithPath: "/A/r"), server: "s", token: "t", signerFD: nil, workDir: nil,
                                        runtimeDir: nil, allowUser: "u1", observeOnly: true)
        XCTAssertEqual(guarded.arguments, ["run", "--managed", "--no-start-holder", "--allow-user", "u1", "--observe-only"])
        let profile = LaunchSpec.runner(binary: URL(fileURLWithPath: "/A/r"), server: "s", token: "t", signerFD: nil, workDir: nil,
                                        runtimeDir: URL(fileURLWithPath: "/P"), startHolder: true)
        XCTAssertEqual(profile.arguments, ["run", "--managed"])
        XCTAssertNil(profile.environment["TMUXEDO_SIGNER_FD"])
        XCTAssertEqual(profile.environment["TMUXEDO_RUNTIME_DIR"], "/P/run")
        XCTAssertEqual(profile.environment["TMUXEDO_STATE_DIR"], "/P/state")
    }

    /// The real launcher: own process group, lines then exit, the exit status, SIGTERM to the group,
    /// and only fds 0-3 crossing (the signer arrives as fd 3).
    func testPosixLauncherRunsARealProcess() throws {
        let done = expectation(description: "exit")
        var lines: [String] = []
        var code: Int32 = -1
        var fds: [Int32] = [0, 0]
        XCTAssertEqual(socketpair(AF_UNIX, SOCK_STREAM, 0, &fds), 0)
        // `[ -e /dev/fd/N ]` stats without opening anything, so it lists exactly what the child inherited above fd 3.
        let script = "echo one; echo two >&2; [ -e /dev/fd/3 ] && echo fd3; for n in 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20; do [ -e /dev/fd/$n ] && echo leaked$n; done; echo end; exit 7"
        let spec = LaunchSpec(executable: URL(fileURLWithPath: "/bin/sh"), arguments: ["-c", script],
                              environment: ["PATH": "/usr/bin:/bin"], signerFD: fds[1])
        let p = try PosixLauncher().launch(spec, line: { l in DispatchQueue.main.async { lines.append(l) } },
                                           exit: { c in DispatchQueue.main.async { code = c; done.fulfill() } })
        XCTAssertEqual(getpgid(p.pid), p.pid, "the runner leads its own process group")
        wait(for: [done], timeout: 10)
        close(fds[0]); close(fds[1])
        XCTAssertEqual(code, 7)
        XCTAssertEqual(lines, ["one", "two", "fd3", "end"], "only stdin, stdout, stderr and the signer (fd 3) cross into the child")
    }

    func testPosixTerminateKillsTheGroup() throws {
        let done = expectation(description: "exit")
        let spec = LaunchSpec(executable: URL(fileURLWithPath: "/bin/sh"), arguments: ["-c", "sleep 30 & wait"],
                              environment: ["PATH": "/usr/bin:/bin"], signerFD: nil)
        var code: Int32 = -1
        let p = try PosixLauncher().launch(spec, line: { _ in }, exit: { c in code = c; done.fulfill() })
        usleep(200_000)
        p.terminate(grace: 2)
        wait(for: [done], timeout: 10)
        XCTAssertEqual(code, 128 + SIGTERM)
        usleep(500_000)
        XCTAssertNotEqual(killpg(p.pid, 0), 0, "the group outlived terminate")
    }
}
