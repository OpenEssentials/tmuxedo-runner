import Charts
import SwiftUI

/// CPU and memory, live, with history.
///
/// FORM: emphasis, not categorical. The question is "how much of this Mac are
/// tmuxedo's agents using?", so the agent is the one coloured series (the
/// accent blue) and the whole machine is recessive gray context. Two charts,
/// never one with two y-axes: CPU is a share (0–100 %) and memory is a size
/// (0 → installed RAM), different scales that one chart could only fake.
///
/// COLOURS were validated with the data-viz validator in both modes: blue
/// `#2a78d6` / `#3987e5` against gray `#898781` passes CVD separation (ΔE
/// 15.9), the normal-vision floor and 3:1 contrast. The gray's low chroma is
/// the point of a de-emphasis series. Identity is never colour alone: the
/// legend names both, and the tiles above state every current value in text.
struct ActivityView: View {
    @EnvironmentObject var monitor: SystemMonitor

    enum Window: Int, CaseIterable, Identifiable {
        case five = 300, fifteen = 900, hour = 3600
        var id: Int { rawValue }
        var seconds: TimeInterval { TimeInterval(rawValue) }
        var label: String {
            switch self { case .five: return "5 min"; case .fifteen: return "15 min"; case .hour: return "1 hour" }
        }
    }
    @AppStorage("activityWindow", store: Profile.defaults(for: Profile.name))
    private var window: Window = .fifteen
    /// One crosshair for both charts, so a CPU spike and the memory at that
    /// moment are read together.
    @State private var hovered: Date?

    var body: some View {
        let data = monitor.recent(window.seconds)
        let latest = monitor.samples.last
        ScrollView {
            VStack(alignment: .leading, spacing: 16) {
                tiles(latest)
                HStack {
                    legend
                    Spacer()
                    Picker("", selection: $window) {
                        ForEach(Window.allCases) { Text($0.label).tag($0) }
                    }
                    .pickerStyle(.segmented)
                    .labelsHidden()
                    .frame(width: 240)
                }
                chart(title: "CPU", data: data, unit: .percent)
                chart(title: "Memory", data: data, unit: .bytes)
                Text("Sampled every 2 s since the app launched; an hour is kept. \"Agent\" is "
                     + "everything tmuxedo runs on this Mac (its terminals, the agents in them and everything "
                     + "they started), as a share of the whole Mac. Shaded periods are when an agent was working.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            .padding(.horizontal, 20)
            .padding(.bottom, 20)
        }
    }

    // MARK: - Tiles

    private func tiles(_ s: SystemSample?) -> some View {
        HStack(spacing: 10) {
            Tile(label: "CPU · this Mac", value: s.map { Self.pct($0.cpu) } ?? "—")
            Tile(label: "Memory · this Mac",
                 value: s.map { Self.gb($0.memUsed) } ?? "—",
                 detail: s.map { "of \(Self.gb($0.memTotal))" })
            Tile(label: "CPU · agent", value: s?.agentCPU.map(Self.pct) ?? "idle",
                 accent: s?.agentCPU != nil)
            Tile(label: "Memory · agent", value: s?.agentMem.map(Self.gb) ?? "idle",
                 accent: s?.agentMem != nil)
            ThermalTile(state: s?.thermal ?? ProcessInfo.processInfo.thermalState)
        }
        // Every tile as tall as the tallest, so the row reads as one row.
        .fixedSize(horizontal: false, vertical: true)
    }

    // MARK: - Legend

    private var legend: some View {
        HStack(spacing: 14) {
            LegendKey(color: Palette.agent, label: "Agent")
            LegendKey(color: Palette.machine, label: "This Mac")
            HStack(spacing: 5) {
                RoundedRectangle(cornerRadius: 2).fill(Palette.work).frame(width: 14, height: 10)
                Text("Agent working").font(.caption).foregroundStyle(.secondary)
            }
        }
    }

    // MARK: - Charts

    enum Unit { case percent, bytes }

    private func chart(title: String, data: [SystemSample], unit: Unit) -> some View {
        let now = data.last?.at ?? Date()
        let from = now.addingTimeInterval(-window.seconds)
        let top = unit == .percent ? 100.0 : Double(monitor.memTotal) / 1_073_741_824
        let machine: (SystemSample) -> Double = unit == .percent
            ? { $0.cpu * 100 } : { Double($0.memUsed) / 1_073_741_824 }
        let agent: (SystemSample) -> Double? = unit == .percent
            ? { $0.agentCPU.map { $0 * 100 } } : { $0.agentMem.map { Double($0) / 1_073_741_824 } }
        let hoveredSample = hovered.flatMap { h in data.min { abs($0.at.timeIntervalSince(h)) < abs($1.at.timeIntervalSince(h)) } }

        return VStack(alignment: .leading, spacing: 6) {
            Text(title.uppercased())
                .font(.system(size: 10, weight: .semibold))
                .foregroundStyle(.secondary)
            Chart {
                // Working periods first, so the data draws over them.
                ForEach(monitor.workSpans.filter { ($0.end ?? now) >= from }) { span in
                    RectangleMark(xStart: .value("Start", max(span.start, from)),
                                  xEnd: .value("End", span.end ?? now),
                                  yStart: .value("Low", 0), yEnd: .value("High", top))
                        .foregroundStyle(Palette.work)
                }
                ForEach(data) { s in
                    AreaMark(x: .value("Time", s.at), y: .value(title, machine(s)))
                        .foregroundStyle(Palette.machine.opacity(0.14))
                        .interpolationMethod(.monotone)
                    LineMark(x: .value("Time", s.at), y: .value(title, machine(s)), series: .value("Series", "machine"))
                        .foregroundStyle(Palette.machine)
                        .lineStyle(StrokeStyle(lineWidth: 1.5))
                        .interpolationMethod(.monotone)
                }
                // One line per stretch with terminals, so time without any is a gap,
                // not a line drawn straight through it.
                ForEach(SystemMonitor.agentRuns(data), id: \.first!.at) { run in
                    ForEach(run) { s in
                        if let v = agent(s) {
                            LineMark(x: .value("Time", s.at), y: .value(title, v),
                                     series: .value("Series", "agent-\(run.first!.at.timeIntervalSince1970)"))
                                .foregroundStyle(Palette.agent)
                                .lineStyle(StrokeStyle(lineWidth: 2))
                                .interpolationMethod(.monotone)
                        }
                    }
                }
                if let h = hoveredSample {
                    RuleMark(x: .value("Time", h.at))
                        .foregroundStyle(Color.secondary.opacity(0.6))
                        .lineStyle(StrokeStyle(lineWidth: 1))
                        .annotation(position: h.at < now.addingTimeInterval(-window.seconds / 2) ? .trailing : .leading,
                                    alignment: .top, spacing: 6) {
                            Tooltip(time: h.at,
                                    rows: [("This Mac", Self.fmt(machine(h), unit, total: monitor.memTotal), Palette.machine),
                                           ("Agent", agent(h).map { Self.fmt($0, unit, total: monitor.memTotal) } ?? "idle", Palette.agent)])
                        }
                }
            }
            .chartXScale(domain: from...now)
            .chartYScale(domain: 0...top)
            .chartYAxis {
                AxisMarks(position: .leading, values: .automatic(desiredCount: 4)) { v in
                    AxisGridLine().foregroundStyle(Palette.grid)
                    AxisValueLabel {
                        if let d = v.as(Double.self) {
                            Text(unit == .percent ? "\(Int(d))%" : String(format: "%.0f GB", d))
                                .font(.system(size: 10)).foregroundStyle(.secondary)
                        }
                    }
                }
            }
            .chartXAxis {
                AxisMarks(values: .automatic(desiredCount: 5)) { _ in
                    AxisGridLine().foregroundStyle(Palette.grid)
                    // Centred on its tick, and the plot keeps a margin on the
                        // right, so the newest label is not cut to "2:…".
                    AxisValueLabel(format: .dateTime.hour().minute(), anchor: .top)
                        .font(.system(size: 10))
                }
            }
            .chartOverlay { proxy in
                GeometryReader { geo in
                    Rectangle().fill(.clear).contentShape(Rectangle())
                        .onContinuousHover { phase in
                            switch phase {
                            case let .active(p):
                                guard let plot = proxy.plotFrame else { return }
                                let x = p.x - geo[plot].origin.x
                                hovered = proxy.value(atX: x, as: Date.self)
                            case .ended:
                                hovered = nil
                            }
                        }
                }
            }
            .padding(.trailing, 18)
            .frame(height: 170)
            .accessibilityLabel("\(title) over the last \(window.label)")
        }
    }

    // MARK: - Formatting

    static func pct(_ f: Double) -> String { "\(Int((f * 100).rounded()))%" }
    static func gb(_ b: UInt64) -> String { String(format: "%.1f GB", Double(b) / 1_073_741_824) }
    static func fmt(_ v: Double, _ unit: Unit, total: UInt64) -> String {
        unit == .percent ? "\(Int(v.rounded()))%" : String(format: "%.1f GB", v)
    }
}

// MARK: - Pieces

/// The validated palette, light and dark.
enum Palette {
    static let agent = Color(nsColor: NSColor(name: nil) { a in
        a.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua
            ? NSColor(srgbRed: 0x39 / 255, green: 0x87 / 255, blue: 0xe5 / 255, alpha: 1)
            : NSColor(srgbRed: 0x2a / 255, green: 0x78 / 255, blue: 0xd6 / 255, alpha: 1)
    })
    static let machine = Color(red: 0x89 / 255, green: 0x87 / 255, blue: 0x81 / 255)
    static let work = agent.opacity(0.08)
    static let grid = Color(nsColor: NSColor(name: nil) { a in
        a.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua
            ? NSColor(srgbRed: 0x2c / 255, green: 0x2c / 255, blue: 0x2a / 255, alpha: 1)
            : NSColor(srgbRed: 0xe1 / 255, green: 0xe0 / 255, blue: 0xd9 / 255, alpha: 1)
    })
}

private struct Tile: View {
    let label: String
    let value: String
    var detail: String? = nil
    var accent = false

    var body: some View {
        VStack(alignment: .leading, spacing: 3) {
            HStack(spacing: 5) {
                if accent { Circle().fill(Palette.agent).frame(width: 7, height: 7) }
                Text(label).font(.system(size: 10, weight: .medium)).foregroundStyle(.secondary)
            }
            Text(value)
                .font(.system(size: 20, weight: .semibold, design: .rounded))
                .monospacedDigit()
            if let detail {
                Text(detail).font(.system(size: 10)).foregroundStyle(.secondary)
            }
        }
        .padding(10)
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
        .background(Color.primary.opacity(0.04), in: RoundedRectangle(cornerRadius: 8))
    }
}

/// Thermal state in the fixed STATUS colours, always with an icon and a
/// word — a status colour never carries meaning alone.
private struct ThermalTile: View {
    let state: ProcessInfo.ThermalState

    private var info: (String, String, Color) {
        switch state {
        case .nominal: return ("Normal", "thermometer.low", Color(red: 0x0c / 255, green: 0xa3 / 255, blue: 0x0c / 255))
        case .fair: return ("Warm", "thermometer.medium", Color(red: 0xfa / 255, green: 0xb2 / 255, blue: 0x19 / 255))
        case .serious: return ("Hot — throttling", "thermometer.high", Color(red: 0xec / 255, green: 0x83 / 255, blue: 0x5a / 255))
        case .critical: return ("Critical", "flame.fill", Color(red: 0xd0 / 255, green: 0x3b / 255, blue: 0x3b / 255))
        @unknown default: return ("Unknown", "thermometer.variable.and.figure", .secondary)
        }
    }

    var body: some View {
        let (word, symbol, color) = info
        VStack(alignment: .leading, spacing: 3) {
            Text("Thermal").font(.system(size: 10, weight: .medium)).foregroundStyle(.secondary)
            HStack(spacing: 5) {
                Image(systemName: symbol).foregroundStyle(color)
                Text(word).font(.system(size: 14, weight: .semibold))
            }
        }
        .padding(10)
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
        .background(Color.primary.opacity(0.04), in: RoundedRectangle(cornerRadius: 8))
    }
}

private struct LegendKey: View {
    let color: Color
    let label: String
    var body: some View {
        HStack(spacing: 5) {
            RoundedRectangle(cornerRadius: 1).fill(color).frame(width: 14, height: 3)
            Text(label).font(.caption).foregroundStyle(.secondary)
        }
    }
}

private struct Tooltip: View {
    let time: Date
    let rows: [(String, String, Color)]
    var body: some View {
        VStack(alignment: .leading, spacing: 3) {
            Text(time, format: .dateTime.hour().minute().second())
                .font(.system(size: 10)).foregroundStyle(.secondary)
            ForEach(rows, id: \.0) { r in
                HStack(spacing: 5) {
                    RoundedRectangle(cornerRadius: 1).fill(r.2).frame(width: 10, height: 3)
                    Text(r.0).font(.system(size: 11)).foregroundStyle(.secondary)
                    Text(r.1).font(.system(size: 11, weight: .semibold)).monospacedDigit()
                }
            }
        }
        .padding(8)
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 6))
        .overlay(RoundedRectangle(cornerRadius: 6).strokeBorder(Color.primary.opacity(0.1)))
    }
}
