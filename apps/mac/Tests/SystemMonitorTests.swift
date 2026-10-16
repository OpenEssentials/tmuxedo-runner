import XCTest

/// A stand-in holder: a shell whose CHILD spins (so the spinner is a descendant, as a terminal's
/// processes are of the holder's). Killed with its process group.
private final class SpinningTree {
    let process = Process()
    init() throws {
        process.executableURL = URL(fileURLWithPath: "/bin/sh")
        process.arguments = ["-c", "sh -c 'while :; do :; done' & wait"]
        try process.run()
    }
    var pid: pid_t { process.processIdentifier }
    func stop() {
        let kids = Measure.members(of: Measure.readProcessTable(), scope: AgentScope(roots: [pid]))
        for k in kids { kill(k, SIGKILL) }
        process.terminate()
        process.waitUntilExit()
    }
}

/// CPU and memory as the activity charts show them.
final class MeasureTests: XCTestCase {
    typealias T = Measure.CPUTicks

    func testCPUUsageFromTickDeltas() {
        let a = T(user: 100, system: 50, idle: 850, nice: 0)
        let b = T(user: 400, system: 100, idle: 1_500, nice: 0)   // +350 busy, +650 idle
        XCTAssertEqual(Measure.cpuUsage(from: a, to: b)!, 0.35, accuracy: 1e-9)
    }

    /// A counter that went backwards is unknown, never a spike or a dip.
    func testImpossibleReadingsAreUnknown() {
        let a = T(user: 100, system: 0, idle: 100, nice: 0)
        XCTAssertNil(Measure.cpuUsage(from: a, to: T(user: 50, system: 0, idle: 100, nice: 0)))
        XCTAssertNil(Measure.cpuUsage(from: a, to: a), "no time passed")
    }

    /// One saturated core on a 4-core machine is a quarter of the machine —
    /// the scale the machine line is on.
    func testGroupShareIsOfTheWholeMachine() {
        XCTAssertEqual(Measure.groupShare(cpuNanosFrom: 0, to: 1_000_000_000, wallNanos: 1_000_000_000, cores: 4)!, 0.25, accuracy: 1e-9)
        XCTAssertEqual(Measure.groupShare(cpuNanosFrom: 0, to: 9_000_000_000, wallNanos: 1_000_000_000, cores: 4)!, 1, "clamped")
        XCTAssertNil(Measure.groupShare(cpuNanosFrom: 5, to: 4, wallNanos: 1, cores: 4))
    }

    /// REGRESSION: a fraction of a second between readings is too few ticks
    /// to mean anything — it read 100 % and 0 % for the same machine.
    func testTooShortAnIntervalIsUnknown() {
        let a = T(user: 100, system: 0, idle: 100, nice: 0)
        let b = T(user: 101, system: 0, idle: 100, nice: 0)   // one tick: "100 %"
        XCTAssertNil(Measure.cpuUsage(from: a, to: b, minTicks: 400))
        XCTAssertEqual(Measure.cpuUsage(from: a, to: b), 1, "without the floor it is exactly the bug")
    }

    /// Readings too close together never produce a spike or a dip on the
    /// chart: the last good value is carried until an interval is long
    /// enough to measure.
    @MainActor
    func testShortIntervalsCarryTheLastValue() {
        let m = SystemMonitor(agentGroup: { nil })
        for _ in 0..<5 { m.tick() }   // back to back: far too short
        XCTAssertTrue(m.samples.allSatisfy { $0.cpu == 0 }, "\(m.samples.map(\.cpu))")
    }

    func testRealCountersAreSane() {
        let a = Measure.readCPUTicks(), b = Measure.readCPUTicks()
        XCTAssertNotNil(a)
        XCTAssertGreaterThanOrEqual(b!.total, a!.total)
        let used = Measure.readMemoryUsed() ?? 0
        XCTAssertGreaterThan(used, 64 << 20, "a running Mac uses more than 64 MB")
        XCTAssertLessThan(used, ProcessInfo.processInfo.physicalMemory)
    }

    /// THE UNIT TRAP. On Apple silicon a process's CPU time is reported in
    /// Mach ticks, not nanoseconds; read raw, a process busy for a second
    /// shows ~24 ms. A real busy loop for ~1 s must measure as ~1 s.
    func testProcessGroupCPUTimeIsInRealNanoseconds() async throws {
        let tree = try SpinningTree()
        defer { tree.stop() }
        try await Task.sleep(nanoseconds: 300_000_000)
        let scope = AgentScope(roots: [tree.pid])
        let a = try XCTUnwrap(Measure.readGroup(scope))
        let t0 = DispatchTime.now().uptimeNanoseconds
        try await Task.sleep(nanoseconds: 1_000_000_000)
        let b = try XCTUnwrap(Measure.readGroup(scope))
        let wall = Double(DispatchTime.now().uptimeNanoseconds - t0)
        let busy = Double(b.cpuNanos - a.cpuNanos)
        // THE BOUND GUARDS THE UNITS, NOT THE HARDWARE. Raw Mach ticks report
        // ~24 ms for a busy second, a ratio near 0.024, so 0.25 still catches
        // that with an order of magnitude to spare. 0.6 additionally asserted
        // that this machine gave the loop most of a core, which is a property of
        // the runner and not of the code: MEASURED 2026-10-02 on a GitHub
        // macos-latest runner, a correct implementation scored 0.535 under CPU
        // contention and failed. It passes on the Mac and passed on an earlier
        // hosted run, which is the signature of contention rather than a defect.
        XCTAssertGreaterThan(busy / wall, 0.25, "a busy loop measured as \(busy / wall) of a core — units are wrong")
        XCTAssertLessThan(busy / wall, 1.2)
        XCTAssertGreaterThan(b.resident, 0)
        XCTAssertGreaterThanOrEqual(b.processes, 1)
    }

    func testNoSuchGroup() {
        XCTAssertNil(Measure.readGroup(AgentScope()))
        XCTAssertNil(Measure.readGroup(AgentScope(roots: [999_999])))
    }

    // MARK: Whose processes are "the agents" (tmuxedo)

    private let table: [Measure.ProcRow] = [
        .init(pid: 1, ppid: 0, uid: 0),        // launchd
        .init(pid: 10, ppid: 1, uid: 501),     // the holder
        .init(pid: 11, ppid: 10, uid: 501),    // a terminal's shell
        .init(pid: 12, ppid: 11, uid: 501),    // claude in it
        .init(pid: 13, ppid: 12, uid: 501),    // a build it started
        .init(pid: 14, ppid: 10, uid: 501),    // another terminal
        .init(pid: 20, ppid: 1, uid: 501),     // the owner's own app: not ours
        .init(pid: 30, ppid: 1, uid: 601),     // the tmuxedo user's holder (separate-user mode)
        .init(pid: 31, ppid: 30, uid: 601),
        .init(pid: 32, ppid: 1, uid: 601),     // daemonised: left the tree, still the tmuxedo user's
    ]

    func testTheHolderTreeIsEveryDescendantButNotTheHolder() {
        XCTAssertEqual(Measure.members(of: table, scope: AgentScope(roots: [10])), [11, 12, 13, 14])
        XCTAssertEqual(Measure.members(of: table, scope: AgentScope(roots: [11])), [12, 13])
    }

    func testSeparateUserModeIsEveryProcessOfThatUser() {
        XCTAssertEqual(Measure.members(of: table, scope: AgentScope(uid: 601)), [30, 31, 32])
        XCTAssertEqual(Measure.members(of: table, scope: AgentScope(roots: [10], uid: 601)), [11, 12, 13, 14, 30, 31, 32])
    }

    func testGoneOrUnreadablePidsAreSkippedAndSummed() {
        let info: (pid_t) -> (cpuNanos: UInt64, resident: UInt64)? = { pid in pid == 13 ? nil : (UInt64(pid) * 1_000, 100) }
        let g = Measure.readGroup(AgentScope(roots: [10]), table: table, taskInfo: info)
        XCTAssertEqual(g?.processes, 3, "13 exited between listing and reading")
        XCTAssertEqual(g?.cpuNanos, 37_000)
        XCTAssertEqual(g?.resident, 300)
        XCTAssertNil(Measure.readGroup(AgentScope(roots: [999]), table: table, taskInfo: info), "a holder that is gone: idle")
        XCTAssertNil(Measure.readGroup(AgentScope(roots: [10]), table: table, taskInfo: { _ in nil }))
        XCTAssertTrue(Measure.members(of: [.init(pid: 5, ppid: 5, uid: 0)], scope: AgentScope(roots: [5])).isEmpty, "a cycle ends")
    }
}

@MainActor
final class SystemMonitorTests: XCTestCase {
    private func sample(_ t: TimeInterval, agent: Bool) -> SystemSample {
        SystemSample(at: Date(timeIntervalSince1970: t), cpu: 0.5, memUsed: 1, memTotal: 2,
                     agentCPU: agent ? 0.1 : nil, agentMem: agent ? 1 : nil, thermal: .nominal)
    }

    func testTickRecordsTheMachineAndTheAgent() async throws {
        let tree = try SpinningTree()
        defer { tree.stop() }
        var group: AgentScope? = nil
        let m = SystemMonitor(agentGroup: { group })
        m.tick()
        XCTAssertNil(m.samples.last?.agentCPU, "no agent yet")
        group = AgentScope(roots: [tree.pid])
        try await Task.sleep(nanoseconds: 1_000_000_000)
        m.tick()
        try await Task.sleep(nanoseconds: 1_000_000_000)
        m.tick()
        let s = try XCTUnwrap(m.samples.last)
        XCTAssertGreaterThan(s.agentCPU ?? 0, 0, "a spinning agent used no CPU")
        XCTAssertGreaterThan(s.agentMem ?? 0, 0)
        XCTAssertGreaterThan(s.cpu, 0)
        XCTAssertEqual(s.memTotal, ProcessInfo.processInfo.physicalMemory)
        group = nil
        m.tick()
        XCTAssertNil(m.samples.last?.agentCPU, "the agent outlived its task on the chart")
    }

    /// The shading: a span opens when an agent starts working and closes when none is.
    func testWorkingSpansFollowTheAgentStatus() {
        var working = false
        let m = SystemMonitor(agentGroup: { nil }, agentsWorking: { working })
        let t = { (s: TimeInterval) in Date(timeIntervalSince1970: 1_000 + s) }
        m.tick(now: t(0))
        XCTAssertTrue(m.workSpans.isEmpty)
        working = true
        m.tick(now: t(2)); m.tick(now: t(4))
        XCTAssertEqual(m.workSpans.count, 1)
        XCTAssertEqual(m.workSpans[0].start, t(2))
        XCTAssertNil(m.workSpans[0].end, "still working: open")
        working = false
        m.tick(now: t(6))
        XCTAssertEqual(m.workSpans[0].end, t(6))
        working = true
        m.tick(now: t(8))
        XCTAssertEqual(m.workSpans.map(\.start), [t(2), t(8)])
    }

    func testOldSpansLeaveWithTheHistory() {
        let m = SystemMonitor(agentGroup: { nil })
        m.noteWorking(true, at: Date(timeIntervalSince1970: 0))
        m.noteWorking(false, at: Date(timeIntervalSince1970: 5))
        m.append(sample(10, agent: false))
        m.noteWorking(false, at: Date(timeIntervalSince1970: 10))
        XCTAssertTrue(m.workSpans.isEmpty, "a span that ended before the oldest sample is gone")
    }

    /// The runner's status → shading: only "working" counts (needs-you and done do not).
    func testOnlyWorkingAgentsShade() {
        let a = { (s: LocalAPI.AgentStatus) in LocalAPI.Agent(id: "p", workspaceId: "w", kind: "claude", name: nil, title: nil, cwd: nil, status: s) }
        XCTAssertFalse(AppModel.anyWorking([]))
        XCTAssertFalse(AppModel.anyWorking([a(.blocked), a(.done), a(.idle)]))
        XCTAssertTrue(AppModel.anyWorking([a(.idle), a(.working)]))
    }

    func testHistoryIsCapped() {
        let m = SystemMonitor(agentGroup: { nil })
        for i in 0..<(SystemMonitor.capacity + 25) { m.append(sample(TimeInterval(i), agent: false)) }
        XCTAssertEqual(m.samples.count, SystemMonitor.capacity)
        XCTAssertEqual(m.samples.first?.at, Date(timeIntervalSince1970: 25))
    }

    func testRecentWindow() {
        let m = SystemMonitor(agentGroup: { nil })
        for i in 0..<100 { m.append(sample(TimeInterval(i * 10), agent: false)) }   // 0…990 s
        XCTAssertEqual(m.recent(300).count, 31, "990−300 … 990 at 10 s steps")
        XCTAssertEqual(SystemMonitor(agentGroup: { nil }).recent(300).count, 0)
    }

    /// Idle time between tasks is a gap in the agent line, not a line drawn
    /// straight across it.
    func testAgentLineBreaksBetweenTasks() {
        let data = [false, true, true, false, false, true, false, true, true, true].enumerated()
            .map { sample(TimeInterval($0.offset), agent: $0.element) }
        XCTAssertEqual(SystemMonitor.agentRuns(data).map(\.count), [2, 1, 3])
        XCTAssertEqual(SystemMonitor.agentRuns([]).count, 0)
    }
}
