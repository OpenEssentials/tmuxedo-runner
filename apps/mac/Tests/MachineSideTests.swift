import Darwin
import ServiceManagement
import XCTest

/// The holder decision and its LaunchAgent, the install.sh migration, the runner's local API,
/// the bundled binary's checks, and the QR code.
@MainActor
final class HolderTests: XCTestCase {
    private func info(_ running: Bool, _ build: String?, pid: Int? = 42) -> RunnerBinary.HolderInfo {
        RunnerBinary.HolderInfo(running: running, version: "v", pid: pid, build: build)
    }

    func testRestartOnlyWhenTheHoldersCodeChanged() {
        XCTAssertEqual(HolderPolicy.decide(running: nil, bundledBuild: "h1"), .start)
        XCTAssertEqual(HolderPolicy.decide(running: info(false, nil), bundledBuild: "h1"), .start)
        XCTAssertEqual(HolderPolicy.decide(running: info(true, "h1"), bundledBuild: "h1"), .keep,
                       "an ordinary update (same holder code) keeps every terminal")
        XCTAssertEqual(HolderPolicy.decide(running: info(true, "h0"), bundledBuild: "h1"), .restart(from: "h0", to: "h1"))
        XCTAssertEqual(HolderPolicy.decide(running: info(true, nil), bundledBuild: "h1"), .keepLegacy,
                       "an install.sh holder keeps its sessions until it exits")
        XCTAssertEqual(HolderPolicy.decide(running: info(true, ""), bundledBuild: "h1"), .keepLegacy)
    }

    /// The service with a fake registry, launchctl and a stand-in runner binary (a shell script
    /// answering `holder-info`).
    private func service(holderJSON: String, registry: FakeRegistry) -> (HolderService, [[String]]) {
        let dir = tempDir()
        let bin = dir.appendingPathComponent("tmuxedo-runner")
        try! "#!/bin/sh\n[ \"$1\" = holder-info ] && echo '\(holderJSON)'\n".write(to: bin, atomically: true, encoding: .utf8)
        chmod(bin.path, 0o755)
        var calls: [[String]] = []
        let s = HolderService(registry: registry, binary: RunnerBinary(url: bin), socket: nil, log: { _ in })
        s.launchctl = { calls.append($0); return 0 }
        return (s, calls)
    }

    func testKeepRegistersButNeverKicks() {
        let reg = FakeRegistry()
        var kicks: [[String]] = []
        let (s, _) = service(holderJSON: #"{"running":true,"version":"v","pid":7,"build":"h1"}"#, registry: reg)
        s.launchctl = { kicks.append($0); return 0 }
        XCTAssertEqual(s.ensure(bundledBuild: "h1"), .keep)
        XCTAssertEqual(reg.registered, 1)
        XCTAssertTrue(kicks.isEmpty, "the holder was touched although its code did not change")
        XCTAssertEqual(s.state, .running(build: "h1", pid: 7))
    }

    func testChangedHolderIsRestartedOnce() {
        let reg = FakeRegistry()
        reg.status = .enabled
        var kicks: [[String]] = []
        let (s, _) = service(holderJSON: #"{"running":true,"version":"v","pid":7,"build":"h0"}"#, registry: reg)
        s.launchctl = { kicks.append($0); return 0 }
        XCTAssertEqual(s.ensure(bundledBuild: "h1"), .restart(from: "h0", to: "h1"))
        XCTAssertEqual(kicks, [["kickstart", "-k", "\(HolderService.guiDomain)/dev.tmuxedo.mac.holder"]])
        XCTAssertEqual(reg.registered, 0, "already enabled")
    }

    func testNotRunningIsStarted() {
        let reg = FakeRegistry()
        var kicks: [[String]] = []
        let (s, _) = service(holderJSON: #"{"running":false}"#, registry: reg)
        s.launchctl = { kicks.append($0); return 0 }
        XCTAssertEqual(s.ensure(bundledBuild: "h1"), .start)
        XCTAssertEqual(kicks, [["kickstart", "\(HolderService.guiDomain)/dev.tmuxedo.mac.holder"]])
    }

    func testApprovalNeededIsReportedNotHidden() {
        let reg = FakeRegistry()
        reg.becomes = .requiresApproval
        let (s, _) = service(holderJSON: #"{"running":false}"#, registry: reg)
        s.ensure(bundledBuild: "h1")
        XCTAssertEqual(s.state, .needsApproval)
        XCTAssertTrue(s.state.sentence.contains("Login Items"))
        reg.failRegister = NSError(domain: "x", code: 1, userInfo: [NSLocalizedDescriptionKey: "nope"])
        reg.status = .notRegistered
        reg.becomes = .notRegistered
        s.ensure(bundledBuild: "h1")
        XCTAssertEqual(s.state, .notRunning, "refresh after a failed registration reports what is true")
    }

    func testKillSwitchBootsOutAndUnregisters() {
        let reg = FakeRegistry()
        reg.status = .enabled
        var calls: [[String]] = []
        let (s, _) = service(holderJSON: #"{"running":true,"build":"h1"}"#, registry: reg)
        s.launchctl = { calls.append($0); return 0 }
        s.stopAndUnregister()
        XCTAssertEqual(calls, [["bootout", "\(HolderService.guiDomain)/dev.tmuxedo.mac.holder"]])
        XCTAssertEqual(reg.unregistered, 1)
        XCTAssertEqual(s.state, .notRunning)
    }
}

final class MigrationTests: XCTestCase {
    func testFindsInstallShServicesAndTheEnrolment() throws {
        let home = tempDir()
        let agents = home.appendingPathComponent("Library/LaunchAgents")
        try FileManager.default.createDirectory(at: agents, withIntermediateDirectories: true)
        for l in ["dev.tmuxedo.runner", "dev.tmuxedo.holder", "ai.aimux.holder", "dev.tmuxedo.mac.holder", "com.other.thing"] {
            try "<plist/>".write(to: agents.appendingPathComponent("\(l).plist"), atomically: true, encoding: .utf8)
        }
        let cfg = home.appendingPathComponent(".config/tmuxedo/runner.toml")
        try FileManager.default.createDirectory(at: cfg.deletingLastPathComponent(), withIntermediateDirectories: true)
        try "server = \"https://tmuxedo.dev\"\ntoken = \"tmr.abc.def\"\nname = \"box\"\nauto_update = true\n".write(to: cfg, atomically: true, encoding: .utf8)
        let f = Migration.scan(home: home)
        XCTAssertEqual(f.runnerPlists.map(\.lastPathComponent), ["dev.tmuxedo.runner.plist"])
        XCTAssertEqual(f.holderPlists.map(\.lastPathComponent), ["dev.tmuxedo.holder.plist", "ai.aimux.holder.plist"])
        XCTAssertEqual(f.enrolment, Migration.Enrolment(server: "https://tmuxedo.dev", token: "tmr.abc.def", name: "box", configPath: cfg))
        XCTAssertFalse(f.isEmpty)
        XCTAssertTrue(Migration.scan(home: tempDir()).isEmpty)
    }

    /// Nothing stops, restarts or kills an old holder; the runner is retired first.
    func testThePlanNeverStopsTheOldHolder() throws {
        let home = tempDir()
        let r = home.appendingPathComponent("dev.tmuxedo.runner.plist")
        let h = home.appendingPathComponent("dev.tmuxedo.holder.plist")
        for p in [r, h] { try "x".write(to: p, atomically: true, encoding: .utf8) }
        let plan = Migration.plan(Migration.Found(runnerPlists: [r], holderPlists: [h], enrolment: nil))
        XCTAssertEqual(Array(plan.prefix(3)), [.bootout(r), .remove(r), .remove(h)])
        XCTAssertFalse(plan.contains(.bootout(h)), "the old holder must keep its sessions")
        var launchctl: [[String]] = []
        var said: [String] = []
        Migration.execute(plan, launchctl: { launchctl.append($0); return 0 }, say: { said.append($0) })
        XCTAssertEqual(launchctl.count, 1)
        XCTAssertEqual(launchctl[0].last, r.path)
        XCTAssertFalse(FileManager.default.fileExists(atPath: r.path))
        XCTAssertFalse(FileManager.default.fileExists(atPath: h.path), "its plist goes so it never starts again")
        XCTAssertTrue(said.first?.contains("stay in the old holder") ?? false)
    }

    func testTomlParsingIsStrictAboutTheToken() {
        let p = URL(fileURLWithPath: "/x")
        XCTAssertNil(Migration.parseRunnerToml("server = \"s\"\n", path: p))
        XCTAssertNil(Migration.parseRunnerToml("server = \"s\"\ntoken = \"nope\"\n", path: p))
        XCTAssertNil(Migration.parseRunnerToml("server = s\ntoken = tmr.a.b\n", path: p), "unquoted values are not read")
        XCTAssertEqual(Migration.parseRunnerToml("# c\nserver='s'\ntoken='amr.a.b'\n", path: p)?.token, "amr.a.b")
    }

    func testAdoptingScrubsTheTokenFromTheFile() throws {
        let cfg = tempDir().appendingPathComponent("runner.toml")
        try "server = \"s\"\ntoken = \"tmr.a.b\"\nname = \"box\"\n".write(to: cfg, atomically: true, encoding: .utf8)
        try Migration.scrubToken(from: cfg)
        let text = try String(contentsOf: cfg, encoding: .utf8)
        XCTAssertFalse(text.contains("tmr.a.b"))
        XCTAssertTrue(text.contains("server"))
    }
}

/// The local API against a fake runner socket speaking newline-delimited JSON.
final class LocalAPITests: XCTestCase {
    private func fakeRunner(_ answer: @escaping ([String: Any]) -> [String: Any]) -> (URL, () -> Void) {
        let dir = URL(fileURLWithPath: "/tmp/tmx-\(UUID().uuidString.prefix(6))")
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        let path = dir.appendingPathComponent("runner.sock")
        let fd = socket(AF_UNIX, SOCK_STREAM, 0)
        var addr = sockaddr_un()
        addr.sun_family = sa_family_t(AF_UNIX)
        let bytes = Array(path.path.utf8CString)
        withUnsafeMutableBytes(of: &addr.sun_path) { d in bytes.withUnsafeBytes { d.copyMemory(from: $0) } }
        _ = withUnsafePointer(to: &addr) { $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { Darwin.bind(fd, $0, socklen_t(MemoryLayout<sockaddr_un>.size)) } }
        Darwin.listen(fd, 8)
        let t = Thread {
            while true {
                let c = accept(fd, nil, nil)
                if c < 0 { return }
                var buf = [UInt8](repeating: 0, count: 65536)
                let n = read(c, &buf, buf.count)
                if n > 0, let req = try? JSONSerialization.jsonObject(with: Data(buf[0..<n].prefix { $0 != 0x0A })) as? [String: Any] {
                    var out = answer(req)
                    out["id"] = req["id"]
                    let d = try! JSONSerialization.data(withJSONObject: out) + Data([0x0A])
                    _ = d.withUnsafeBytes { write(c, $0.baseAddress, $0.count) }
                }
                close(c)
            }
        }
        t.start()
        return (path, { close(fd); try? FileManager.default.removeItem(at: dir) })
    }

    func testSnapshotOfWorkspacesAgentsAndNeedsYou() throws {
        let (sock, stop) = fakeRunner { req in
            switch req["method"] as? String {
            case "workspace.list":
                return ["result": ["type": "workspace_list", "workspaces": [
                    ["workspace_id": "w1", "number": 1, "label": "api", "focused": true, "pane_count": 3, "tab_count": 2, "agent_status": "blocked"],
                    ["workspace_id": "w2", "number": 2, "label": "web", "focused": false, "pane_count": 1, "tab_count": 1, "agent_status": "idle"],
                ]]]
            case "agent.list":
                return ["result": ["type": "agent_list", "agents": [
                    ["pane_id": "w1:p1", "workspace_id": "w1", "agent": "claude", "agent_status": "blocked", "cwd": "/Users/me/api", "name": "fixer"],
                    ["pane_id": "w1:p2", "workspace_id": "w1", "agent": "claude", "agent_status": "working"],
                    ["pane_id": "w2:p1", "workspace_id": "w2", "agent": "codex", "agent_status": "something new"],
                ]]]
            default:
                return ["error": ["code": "unknown_method", "message": "no"]]
            }
        }
        defer { stop() }
        let s = try LocalAPI(socket: sock).snapshot()
        XCTAssertEqual(s.workspaces.map(\.label), ["api", "web"])
        XCTAssertEqual(s.workspaces[0].status, .blocked)
        XCTAssertEqual(s.workspaces[0].panes, 3)
        XCTAssertEqual(s.agents.count, 3)
        XCTAssertEqual(s.agents[2].status, .unknown, "an unknown status is not guessed")
        XCTAssertEqual(s.needsYou.map(\.id), ["w1:p1"])
        XCTAssertEqual(s.needsYou[0].name, "fixer")
        XCTAssertThrowsError(try LocalAPI(socket: sock).call("nope")) { e in
            XCTAssertEqual(e as? LocalAPI.CallError, .refused(code: "unknown_method", message: "no"))
        }
    }

    func testNoRunnerIsUnavailable() {
        XCTAssertThrowsError(try LocalAPI(socket: URL(fileURLWithPath: "/tmp/nothing-\(UUID().uuidString.prefix(6)).sock")).call("workspace.list")) { e in
            guard case .unavailable = e as? LocalAPI.CallError else { return XCTFail("\(e)") }
        }
    }

    func testDefaultSockets() {
        XCTAssertTrue(LocalAPI.defaultSocket(runtimeRoot: URL(fileURLWithPath: "/P")).path == "/P/run/runner.sock")
        XCTAssertEqual(LocalAPI.holderSocket(runtimeRoot: URL(fileURLWithPath: "/P"))?.path, "/P/run/holder.sock")
        XCTAssertNil(LocalAPI.holderSocket(runtimeRoot: nil))
        XCTAssertEqual(LocalAPI.AgentStatus.blocked.label, "needs you")
    }
}

final class RunnerBinaryTests: XCTestCase {
    func testVersionParsingAndRequiredFeatures() {
        let v = RunnerBinary.Version.parse(#"{"version":"main-r1-abc","holderBuild":"h0123456789abcdef","protocol":1,"holderProto":1,"platform":"darwin-aarch64","features":["managed","signer-fd","signer-socket","holder-build","pair","holder-info"]}"#)
        XCTAssertEqual(v?.holderBuild, "h0123456789abcdef")
        XCTAssertEqual(v?.missingFeatures, [])
        let old = RunnerBinary.Version.parse(#"{"version":"old","holderBuild":"h","platform":"p","features":["managed"]}"#)
        XCTAssertEqual(old?.missingFeatures, ["signer-fd", "holder-build", "holder-info"])
        XCTAssertNil(RunnerBinary.Version.parse("tmuxedo-runner 0.1.0 (darwin-aarch64)"))
        XCTAssertEqual(RunnerBinary.HolderInfo.parse(#"{"running":false}"#), RunnerBinary.HolderInfo(running: false, version: nil, pid: nil, build: nil))
    }

    /// A signed app runs only a runner signed by its own team; an unsigned build may run an unsigned one.
    func testLaunchDecision() {
        XCTAssertNil(RunnerBinary.mayLaunch(check: .valid(team: "ABCDE12345"), appTeam: "ABCDE12345"))
        XCTAssertNil(RunnerBinary.mayLaunch(check: .unsigned, appTeam: nil))
        XCTAssertNotNil(RunnerBinary.mayLaunch(check: .unsigned, appTeam: "ABCDE12345"))
        XCTAssertNotNil(RunnerBinary.mayLaunch(check: .invalid("x"), appTeam: nil))
    }

    /// Real code objects: an ad-hoc signed binary has no team; a tampered one does not verify;
    /// a system binary is Apple's, not ours.
    func testSignatureChecksOnRealBinaries() throws {
        let dir = tempDir()
        let copy = dir.appendingPathComponent("true-copy")
        try FileManager.default.copyItem(at: URL(fileURLWithPath: "/usr/bin/true"), to: copy)
        _ = Shell.run("/usr/bin/codesign", ["-f", "-s", "-", copy.path])
        XCTAssertEqual(RunnerBinary.checkSignature(of: copy, team: nil), .unsigned)
        guard case .invalid = RunnerBinary.checkSignature(of: URL(fileURLWithPath: "/usr/bin/true"), team: "ABCDE12345") else {
            return XCTFail("Apple's binary passed as ours")
        }
        let notCode = dir.appendingPathComponent("text")
        try "hello".write(to: notCode, atomically: true, encoding: .utf8)
        guard case .invalid = RunnerBinary.checkSignature(of: notCode, team: nil) else { return XCTFail() }
        XCTAssertNil(RunnerBinary.appTeam, "the test bundle is not team-signed")
    }

    func testBundledLookupAndOverride() throws {
        XCTAssertNil(RunnerBinary.bundled(in: Bundle(for: RunnerBinaryTests.self), environment: [:]))
        let bin = tempDir().appendingPathComponent("tmuxedo-runner")
        try "#!/bin/sh\n".write(to: bin, atomically: true, encoding: .utf8)
        chmod(bin.path, 0o755)
        XCTAssertEqual(RunnerBinary.bundled(environment: ["TMUXEDO_MAC_RUNNER": bin.path])?.url, bin)
    }
}

final class QRCodeTests: XCTestCase {
    func testTheCodeIsSquareAndScaled() {
        let img = QRCode.image(for: "https://tmuxedo.dev/m/pair?code=" + String(repeating: "A", count: 43), scale: 4)
        XCTAssertNotNil(img)
        XCTAssertEqual(img?.width, img?.height)
        XCTAssertEqual((img?.width ?? 0) % 4, 0)
        XCTAssertNil(QRCode.image(for: ""))
    }
}
