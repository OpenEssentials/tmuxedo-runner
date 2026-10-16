import SwiftUI

/// The popover: the state sentence first (verbatim from `Display.sentence`), what
/// needs the person, the workspaces, then the buttons.
struct MenuView: View {
    @EnvironmentObject var model: AppModel
    @EnvironmentObject var settings: Settings
    @EnvironmentObject var supervisor: Supervisor
    @Environment(\.openSettings) private var openSettings

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            header
            Divider()
            ScrollView {
                VStack(alignment: .leading, spacing: 12) {
                    Text(supervisor.display.sentence)
                        .font(.system(size: 12))
                        .fixedSize(horizontal: false, vertical: true)
                        .foregroundStyle(supervisor.display.isFault ? Color.orange : Color.primary)
                    if !model.snapshot.needsYou.isEmpty { NeedsYouList(agents: model.snapshot.needsYou) }
                    if !model.snapshot.workspaces.isEmpty { WorkspaceList(workspaces: model.snapshot.workspaces, compact: true) }
                    recent
                }
                .padding(12)
            }
            Divider()
            footer
        }
        .frame(width: 360)
        .frame(maxHeight: 480)
    }

    private var header: some View {
        HStack(spacing: 8) {
            StateDot(display: supervisor.display)
            VStack(alignment: .leading, spacing: 1) {
                HStack(spacing: 6) {
                    Text("tmuxedo").font(.system(size: 13, weight: .semibold))
                    BuildBadge()
                }
                Text(settings.machine.map { "\($0.name) · \(settings.server?.host ?? "")" } ?? "not paired")
                    .font(.system(size: 11)).foregroundStyle(.secondary)
            }
            Spacer()
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 10)
    }

    private var recent: some View {
        VStack(alignment: .leading, spacing: 2) {
            ForEach(supervisor.log.suffix(6)) { line in
                Text(line.text).font(.system(size: 10, design: .monospaced)).foregroundStyle(line.kind.color).lineLimit(2)
            }
            if supervisor.log.isEmpty {
                Text("$ nothing yet").font(.system(size: 10, design: .monospaced)).foregroundStyle(Terminal.dim)
            }
        }
        .padding(8)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(Terminal.background, in: RoundedRectangle(cornerRadius: 6))
    }

    private var footer: some View {
        HStack(spacing: 8) {
            if !settings.isPaired {
                Button("Pair this Mac…") { NotificationCenter.default.post(name: LaunchKind.showPairing, object: nil) }
            } else if supervisor.display.isRunning {
                Button("Stop") { model.stop() }
                Button("Restart") { supervisor.restart() }
            } else if supervisor.display != .stopping {
                Button("Start") { model.start() }
            }
            Spacer()
            if let url = settings.webURL(settings.machine.map { "/machines/\($0.machineId)" } ?? "/") {
                Button { NSWorkspace.shared.open(url) } label: { Image(systemName: "safari") }
                    .help("Open in the browser")
            }
            Button { NotificationCenter.default.post(name: LaunchKind.showDashboard, object: nil) } label: {
                Image(systemName: "macwindow")
            }.help("Open the dashboard")
            Button { openSettings(); NSApp.activate(ignoringOtherApps: true) } label: { Image(systemName: "gearshape") }
                .help("Settings")
            Button { NSApp.terminate(nil) } label: { Image(systemName: "power") }
                .help("Quit tmuxedo. Your terminals keep running; nobody can reach them until it is open again.")
        }
        .buttonStyle(.bordered)
        .controlSize(.small)
        .padding(.horizontal, 12)
        .padding(.vertical, 10)
    }
}

struct NeedsYouList: View {
    let agents: [LocalAPI.Agent]
    @EnvironmentObject var settings: Settings

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text("NEEDS YOU").font(.system(size: 10, weight: .semibold)).foregroundStyle(.orange)
            ForEach(agents) { a in
                HStack(spacing: 8) {
                    Image(systemName: "hand.raised.fill").foregroundStyle(.orange)
                    VStack(alignment: .leading, spacing: 1) {
                        Text(a.name ?? a.title ?? a.kind.capitalized).font(.system(size: 12, weight: .medium)).lineLimit(1)
                        Text(a.cwd ?? a.id).font(.system(size: 10)).foregroundStyle(.secondary).lineLimit(1).truncationMode(.middle)
                    }
                    Spacer()
                    if let url = settings.webURL(settings.machine.map { "/machines/\($0.machineId)?pane=\(a.id)" } ?? "/agents") {
                        Button("Answer…") { NSWorkspace.shared.open(url) }.controlSize(.small)
                            .help("Opens tmuxedo in the browser; approving asks you to confirm it's you.")
                    }
                }
            }
        }
        .padding(8)
        .background(Color.orange.opacity(0.08), in: RoundedRectangle(cornerRadius: 6))
    }
}

struct WorkspaceList: View {
    let workspaces: [LocalAPI.Workspace]
    var compact = false

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text("WORKSPACES").font(.system(size: 10, weight: .semibold)).foregroundStyle(.secondary)
            ForEach(workspaces) { w in
                HStack(spacing: 8) {
                    StatusChip(status: w.status)
                    Text(w.label).font(.system(size: 12)).lineLimit(1)
                    Spacer()
                    if !compact {
                        Text("\(w.tabs) tab\(w.tabs == 1 ? "" : "s") · \(w.panes) pane\(w.panes == 1 ? "" : "s")")
                            .font(.caption).foregroundStyle(.secondary)
                    }
                }
            }
        }
    }
}

/// Status with a word, never colour alone.
struct StatusChip: View {
    let status: LocalAPI.AgentStatus
    var body: some View {
        Text(status.label)
            .font(.system(size: 10, weight: .semibold))
            .padding(.horizontal, 6).padding(.vertical, 2)
            .background(color.opacity(0.18), in: Capsule())
            .foregroundStyle(color)
            .accessibilityLabel("Agent status: \(status.label)")
    }
    private var color: Color {
        switch status {
        case .working: return .blue
        case .blocked: return .orange
        case .done: return .green
        case .idle, .unknown: return .secondary
        }
    }
}
