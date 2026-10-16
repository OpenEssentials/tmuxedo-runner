import Darwin
import Foundation

/// CPU and memory, for this Mac and for everything tmuxedo runs on it.
///
/// TWO QUESTIONS, NOT ONE. "Is the Mac busy?" and "is it OUR work making it
/// busy?" have different answers on a laptop somebody is also using, so
/// every sample carries both: the machine's totals, and the share taken by
/// the agents' processes — every terminal's process tree under the holder
/// (shells, coding agents and everything they started), or in separate-user
/// mode every process of the `tmuxedo` user (`AgentScope`).
///
/// Sampled every 2 s for as long as the app runs, not only while the
/// dashboard is open: the history is the point, and it has to exist before
/// somebody opens the window to ask what happened. One sample is a handful
/// of syscalls; an hour of them is ~1,800 small structs.
struct SystemSample: Equatable, Identifiable {
    let at: Date
    /// Whole machine, 0…1.
    let cpu: Double
    let memUsed: UInt64
    let memTotal: UInt64
    /// The agents' processes, as a share of the WHOLE machine (0…1), so
    /// it sits on the same scale as `cpu`. nil when no terminal is running.
    let agentCPU: Double?
    let agentMem: UInt64?
    let thermal: ProcessInfo.ThermalState

    var id: Date { at }
    var memFraction: Double { memTotal == 0 ? 0 : Double(memUsed) / Double(memTotal) }
}

// MARK: - Measurement (pure, testable)

enum Measure {
    /// Cumulative CPU ticks across all cores.
    struct CPUTicks: Equatable {
        var user: UInt64, system: UInt64, idle: UInt64, nice: UInt64
        var busy: UInt64 { user + system + nice }
        var total: UInt64 { busy + idle }
    }

    /// Share of time the machine was busy between two readings. Counters only
    /// grow; a reading that went backwards (wrap, or a bad read) is reported
    /// as unknown rather than as a nonsense spike.
    ///
    /// TOO FEW TICKS IS ALSO UNKNOWN. The counters are coarse (100 Hz per
    /// core, and on Apple silicon an idle core's are brought up to date
    /// lazily), so over a fraction of a second one busy tick out of one
    /// total reads as a 100 % machine — measured: two samples 0.1 s apart
    /// said 1.0, and another pair said 0.0 with a core pinned. `minTicks`
    /// refuses intervals too short to mean anything.
    static func cpuUsage(from a: CPUTicks, to b: CPUTicks, minTicks: UInt64 = 1) -> Double? {
        guard b.total >= a.total, b.busy >= a.busy else { return nil }
        let total = b.total - a.total
        guard total >= max(minTicks, 1) else { return nil }
        return min(1, max(0, Double(b.busy - a.busy) / Double(total)))
    }

    /// CPU share of the whole machine taken by a process tree, from its
    /// cumulative CPU time. `cores` because one saturated core on a 12-core
    /// Mac is 1/12 of the machine — the scale the machine line is on.
    static func groupShare(cpuNanosFrom a: UInt64, to b: UInt64, wallNanos: UInt64, cores: Int) -> Double? {
        guard b >= a, wallNanos > 0, cores > 0 else { return nil }
        return min(1, Double(b - a) / (Double(wallNanos) * Double(cores)))
    }

    static func readCPUTicks() -> CPUTicks? {
        var info = host_cpu_load_info()
        var count = mach_msg_type_number_t(MemoryLayout<host_cpu_load_info_data_t>.size / MemoryLayout<integer_t>.size)
        let rc = withUnsafeMutablePointer(to: &info) {
            $0.withMemoryRebound(to: integer_t.self, capacity: Int(count)) {
                host_statistics(mach_host_self(), HOST_CPU_LOAD_INFO, $0, &count)
            }
        }
        guard rc == KERN_SUCCESS else { return nil }
        return CPUTicks(user: UInt64(info.cpu_ticks.0), system: UInt64(info.cpu_ticks.1),
                        idle: UInt64(info.cpu_ticks.2), nice: UInt64(info.cpu_ticks.3))
    }

    /// Memory in use, the way Activity Monitor's "Memory Used" counts it:
    /// app memory (anonymous pages, less purgeable) + wired + compressed.
    /// Cached files are NOT "used" — macOS fills free RAM with them on purpose,
    /// and counting them would show every healthy Mac as nearly full.
    static func readMemoryUsed() -> UInt64? {
        var stats = vm_statistics64()
        var count = mach_msg_type_number_t(MemoryLayout<vm_statistics64_data_t>.size / MemoryLayout<integer_t>.size)
        let rc = withUnsafeMutablePointer(to: &stats) {
            $0.withMemoryRebound(to: integer_t.self, capacity: Int(count)) {
                host_statistics64(mach_host_self(), HOST_VM_INFO64, $0, &count)
            }
        }
        guard rc == KERN_SUCCESS else { return nil }
        let page = UInt64(vm_kernel_page_size)
        let app = UInt64(stats.internal_page_count) &- UInt64(stats.purgeable_count)
        return (app + UInt64(stats.wire_count) + UInt64(stats.compressor_page_count)) * page
    }

    /// Mach time units to nanoseconds. On Apple silicon `pti_total_user` and
    /// friends are in Mach ticks (24 MHz), not nanoseconds — read raw, the
    /// agent would appear to use ~1/40th of the CPU it does.
    static let timebase: (numer: UInt64, denom: UInt64) = {
        var tb = mach_timebase_info_data_t()
        mach_timebase_info(&tb)
        return (UInt64(tb.numer), UInt64(max(tb.denom, 1)))
    }()

    /// One row of the process table: who started it, and as whom.
    struct ProcRow: Equatable { let pid: pid_t; let ppid: pid_t; let uid: uid_t }

    /// The agents' processes: every descendant of `scope.roots` (the holder's terminals and
    /// everything they started; the holder itself is not one), plus every process of
    /// `scope.uid` (separate-user mode: whatever the `tmuxedo` user runs, even when it left
    /// the tree). Pure, for tests.
    static func members(of table: [ProcRow], scope: AgentScope) -> [pid_t] {
        var children: [pid_t: [pid_t]] = [:]
        for r in table { children[r.ppid, default: []].append(r.pid) }
        var out = Set<pid_t>()
        var stack = Array(scope.roots)
        while let p = stack.popLast() {
            for c in children[p] ?? [] where c != p && out.insert(c).inserted { stack.append(c) }
        }
        if let uid = scope.uid { for r in table where r.uid == uid { out.insert(r.pid) } }
        return out.sorted()
    }

    static func readProcessTable() -> [ProcRow] {
        let bytes = proc_listallpids(nil, 0)
        guard bytes > 0 else { return [] }
        var pids = [pid_t](repeating: 0, count: Int(bytes) + 64)
        let got = pids.withUnsafeMutableBytes { proc_listallpids($0.baseAddress, Int32($0.count)) }
        guard got > 0 else { return [] }
        return pids.prefix(Int(got)).compactMap { pid in
            guard pid > 0 else { return nil }
            var bi = proc_bsdinfo()
            let size = Int32(MemoryLayout<proc_bsdinfo>.size)
            guard proc_pidinfo(pid, PROC_PIDTBSDINFO, 0, &bi, size) == size else { return nil }
            return ProcRow(pid: pid, ppid: pid_t(bi.pbi_ppid), uid: bi.pbi_uid)
        }
    }

    static func readTaskInfo(_ pid: pid_t) -> (cpuNanos: UInt64, resident: UInt64)? {
        var ti = proc_taskinfo()
        let size = Int32(MemoryLayout<proc_taskinfo>.size)
        guard proc_pidinfo(pid, PROC_PIDTASKINFO, 0, &ti, size) == size else { return nil }
        return ((ti.pti_total_user + ti.pti_total_system) * timebase.numer / timebase.denom, ti.pti_resident_size)
    }

    /// Total CPU time (ns) and resident memory of the agents' processes. Processes that exit
    /// between listing and reading (or that this user may not read) are simply skipped.
    static func readGroup(_ scope: AgentScope, table: [ProcRow]? = nil,
                          taskInfo: (pid_t) -> (cpuNanos: UInt64, resident: UInt64)? = Measure.readTaskInfo)
        -> (cpuNanos: UInt64, resident: UInt64, processes: Int)? {
        guard !scope.roots.isEmpty || scope.uid != nil else { return nil }
        var cpu: UInt64 = 0, rss: UInt64 = 0, n = 0
        for pid in members(of: table ?? readProcessTable(), scope: scope) {
            guard let t = taskInfo(pid) else { continue }
            cpu += t.cpuNanos
            rss += t.resident
            n += 1
        }
        return n > 0 ? (cpu, rss, n) : nil
    }
}

/// Whose processes are "the agents": the holder's (full access: its pid), or the separate
/// user's (its uid). Equal scopes are one interval for the CPU share.
struct AgentScope: Equatable {
    var roots: [pid_t] = []
    var uid: uid_t?
}

// MARK: - The sampler

/// Periods when an agent on this Mac was working, so the charts can show which work
/// a spike belongs to.
struct WorkSpan: Equatable, Identifiable {
    let id: String
    let start: Date
    let end: Date?
}

@MainActor
final class SystemMonitor: ObservableObject {
    @Published private(set) var samples: [SystemSample] = []

    /// An hour at one sample per two seconds.
    static let capacity = 1_800
    static let interval: TimeInterval = 2

    let memTotal = ProcessInfo.processInfo.physicalMemory
    let cores = ProcessInfo.processInfo.activeProcessorCount

    /// Whose processes are the agents' right now (the holder's pid, or the separate user).
    private let agentGroup: @MainActor () -> AgentScope?
    /// Whether any agent is working right now (the runner's agent status), for the shading.
    private let agentsWorking: @MainActor () -> Bool
    @Published private(set) var workSpans: [WorkSpan] = []
    private var timer: Timer?
    private var lastTicks: Measure.CPUTicks?
    private var lastCPU: Double = 0
    /// Half a second of every core at 100 Hz: below this, an interval says
    /// nothing reliable (see Measure.cpuUsage).
    private var minTicks: UInt64 { UInt64(max(cores, 1) * 50) }
    private var lastGroup: (pgid: AgentScope, cpuNanos: UInt64, at: UInt64)?

    init(agentGroup: @escaping @MainActor () -> AgentScope?, agentsWorking: @escaping @MainActor () -> Bool = { false }) {
        self.agentGroup = agentGroup
        self.agentsWorking = agentsWorking
    }

    func start() {
        guard timer == nil else { return }
        lastTicks = Measure.readCPUTicks()
        let t = Timer(timeInterval: Self.interval, repeats: true) { [weak self] _ in
            Task { @MainActor in self?.tick() }
        }
        // Tolerance lets macOS batch the wakeups; a 2 s sampler has no need
        // to be the thing that stops a laptop's CPU idling.
        t.tolerance = 0.5
        RunLoop.main.add(t, forMode: .common)
        timer = t
    }

    func stop() {
        timer?.invalidate()
        timer = nil
    }

    /// Take one sample now. Public for tests.
    func tick(now: Date = Date()) {
        guard let ticks = Measure.readCPUTicks(), let used = Measure.readMemoryUsed() else { return }
        // An interval too short to measure keeps ACCUMULATING (lastTicks is
        // not advanced) and the chart carries the last good value, rather
        // than drawing a spike or a dip that never happened.
        let cpu: Double
        if let last = lastTicks, let v = Measure.cpuUsage(from: last, to: ticks, minTicks: minTicks) {
            cpu = v
            lastCPU = v
            lastTicks = ticks
        } else {
            cpu = lastCPU
            if lastTicks == nil { lastTicks = ticks }
        }

        var agentCPU: Double?
        var agentMem: UInt64?
        let wall = DispatchTime.now().uptimeNanoseconds
        if let pgid = agentGroup(), let g = Measure.readGroup(pgid) {
            agentMem = g.resident
            if let last = lastGroup, last.pgid == pgid {
                agentCPU = Measure.groupShare(cpuNanosFrom: last.cpuNanos, to: g.cpuNanos,
                                              wallNanos: wall &- last.at, cores: cores)
            } else {
                agentCPU = 0 // first sight of this agent: no interval to measure yet
            }
            lastGroup = (pgid, g.cpuNanos, wall)
        } else {
            lastGroup = nil
        }

        append(SystemSample(at: now, cpu: cpu, memUsed: used, memTotal: memTotal,
                            agentCPU: agentCPU, agentMem: agentMem,
                            thermal: ProcessInfo.processInfo.thermalState))
        noteWorking(agentsWorking(), at: now)
    }

    /// Opens a span when an agent starts working, closes it when none is; spans older than
    /// the history are dropped with it. Public for tests.
    func noteWorking(_ working: Bool, at now: Date) {
        if working, workSpans.last.map({ $0.end != nil }) ?? true {
            workSpans.append(WorkSpan(id: "\(now.timeIntervalSince1970)", start: now, end: nil))
        } else if !working, let last = workSpans.last, last.end == nil {
            workSpans[workSpans.count - 1] = WorkSpan(id: last.id, start: last.start, end: now)
        }
        if let oldest = samples.first?.at { workSpans.removeAll { ($0.end ?? now) < oldest } }
    }

    func append(_ s: SystemSample) {
        samples.append(s)
        if samples.count > Self.capacity { samples.removeFirst(samples.count - Self.capacity) }
    }

    /// Consecutive samples that had an agent, split wherever there was none.
    nonisolated static func agentRuns(_ data: [SystemSample]) -> [[SystemSample]] {
        var runs: [[SystemSample]] = []
        var cur: [SystemSample] = []
        for s in data {
            if s.agentCPU != nil || s.agentMem != nil {
                cur.append(s)
            } else if !cur.isEmpty {
                runs.append(cur); cur = []
            }
        }
        if !cur.isEmpty { runs.append(cur) }
        return runs
    }

    /// The samples within `window` of the newest one.
    func recent(_ window: TimeInterval) -> [SystemSample] {
        guard let last = samples.last else { return [] }
        let from = last.at.addingTimeInterval(-window)
        return samples.filter { $0.at >= from }
    }
}
