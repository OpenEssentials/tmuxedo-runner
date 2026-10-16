import SwiftUI

/// The window you get by opening the app.
///
/// THE MENU-BAR ICON IS NOT A RELIABLE WAY IN (macOS hides status items on a
/// full menu bar), so opening tmuxedo from Finder, Launchpad, Spotlight or the
/// Dock shows this: the state sentence, the machine, workspaces and agents,
/// what needs you, links into tmuxedo, the log, and About.
struct DashboardView: View {
    @EnvironmentObject var model: AppModel
    @EnvironmentObject var settings: Settings
    @EnvironmentObject var supervisor: Supervisor
    @EnvironmentObject var updates: UpdateController
    @Environment(\.openSettings) private var openSettings

    enum Tab: String, CaseIterable, Identifiable {
        case overview, log, activity, mac, about
        var id: String { rawValue }
    }
    @AppStorage("dashboardTab", store: Profile.defaults(for: Profile.name)) private var tab: Tab = .overview

    var body: some View {
        VStack(spacing: 0) {
            header
            Divider()
            banners
            Picker("", selection: $tab) {
                Text("Overview").tag(Tab.overview)
                Text("Log").tag(Tab.log)
                Text("Activity").tag(Tab.activity)
                Text("This Mac").tag(Tab.mac)
                Text("About").tag(Tab.about)
            }
            .pickerStyle(.segmented)
            .labelsHidden()
            .padding(.horizontal, 20)
            .padding(.vertical, 12)
            switch tab {
            case .overview: ScrollView { overview.padding(.horizontal, 20).padding(.bottom, 20) }
            case .log: TerminalLogView().padding(.horizontal, 20).padding(.bottom, 20)
            case .activity:
                ActivityView()
            case .mac: ScrollView { MacFacts().padding(.horizontal, 20).padding(.bottom, 20) }
            case .about: AboutView()
            }
        }
        .frame(minWidth: 640, minHeight: 560)
    }

    // MARK: Header

    private var header: some View {
        HStack(alignment: .center, spacing: 14) {
            Image(systemName: supervisor.display.symbol)
                .font(.system(size: 30, weight: .medium))
                .foregroundStyle(tint)
                .frame(width: 44)
                .symbolEffect(.variableColor.iterative, options: .repeating, isActive: supervisor.display.animating)
            VStack(alignment: .leading, spacing: 3) {
                Text(supervisor.display.sentence)
                    .font(.system(size: 14, weight: .semibold))
                    .foregroundStyle(supervisor.display.isFault ? Color.orange : Color.primary)
                    .fixedSize(horizontal: false, vertical: true)
                Text(settings.machine.map { "\($0.name) · \(settings.serverURL)" } ?? settings.serverURL)
                    .font(.system(size: 12)).foregroundStyle(.secondary).lineLimit(1).truncationMode(.middle)
                HStack(spacing: 8) {
                    BuildBadge()
                    if let profile = Profile.name {
                        Text("PROFILE: \(profile)")
                            .font(.system(size: 10, weight: .bold, design: .monospaced))
                            .padding(.horizontal, 6).padding(.vertical, 2)
                            .background(Color.orange.opacity(0.2), in: Capsule())
                            .foregroundStyle(.orange)
                            .help("An isolated copy: its own settings, key, token and terminals. Your real tmuxedo is unaffected.")
                    }
                }
            }
            Spacer(minLength: 12)
            controls
        }
        .padding(.horizontal, 20)
        .padding(.vertical, 16)
    }

    private var tint: Color {
        switch supervisor.display {
        case .connected: return .green
        case .refused: return .red
        case let d where d.isFault: return .orange
        default: return .secondary
        }
    }

    @ViewBuilder
    private var controls: some View {
        HStack(spacing: 8) {
            if !settings.isPaired {
                Button("Pair this Mac…") { NotificationCenter.default.post(name: LaunchKind.showPairing, object: nil) }
                    .keyboardShortcut(.defaultAction)
            } else if supervisor.display.isRunning {
                Button("Stop") { model.stop() }
                Button("Restart") { supervisor.restart() }
            } else if supervisor.display != .stopping {
                Button("Start") { model.start() }.keyboardShortcut(.defaultAction)
            }
            Button { openSettings(); NSApp.activate(ignoringOtherApps: true) } label: { Label("Settings", systemImage: "gearshape") }
        }
        .controlSize(.large)
    }

    @ViewBuilder
    private var banners: some View {
        if let v = updates.available {
            banner("arrow.down.circle", "tmuxedo \(v) is available. Installing keeps your terminals running.") {
                Button("Install…") { updates.checkNow() }
            }
        }
        if !model.migration.isEmpty, settings.isPaired || model.migration.enrolment != nil {
            MigrationBanner()
        }
        if let iso = model.iso {
            IsolationBanner(iso: iso)
        }
        if model.holder?.state == .needsApproval {
            banner("exclamationmark.shield", "Allow tmuxedo's background item so your terminals survive quitting the app.") {
                Button("Open Login Items") {
                    NSWorkspace.shared.open(URL(string: "x-apple.systempreferences:com.apple.LoginItems-Settings.extension")!)
                }
            }
        }
    }

    private func banner<A: View>(_ icon: String, _ text: String, @ViewBuilder action: () -> A) -> some View {
        HStack(spacing: 10) {
            Image(systemName: icon).foregroundStyle(.blue)
            Text(text).font(.system(size: 12)).fixedSize(horizontal: false, vertical: true)
            Spacer()
            action()
        }
        .padding(.horizontal, 20)
        .padding(.vertical, 8)
        .background(Color.blue.opacity(0.08))
    }

    // MARK: Overview

    private var overview: some View {
        VStack(alignment: .leading, spacing: 18) {
            if !settings.isPaired {
                section("Get started") {
                    VStack(alignment: .leading, spacing: 8) {
                        Text("Pair this Mac with your tmuxedo account to reach its terminals from your phone and any browser.")
                            .fixedSize(horizontal: false, vertical: true)
                        if let e = model.migration.enrolment {
                            Text("This Mac was already set up with the command-line installer (\(e.server)).")
                                .font(.caption).foregroundStyle(.secondary)
                            HStack {
                                Button("Pair with this Mac's own key (recommended)") {
                                    NotificationCenter.default.post(name: LaunchKind.showPairing, object: nil)
                                }
                                Button("Keep the existing enrolment") { model.adopt(e) }
                            }
                        } else {
                            Button("Pair this Mac…") { NotificationCenter.default.post(name: LaunchKind.showPairing, object: nil) }
                        }
                    }
                }
            }
            if !model.snapshot.needsYou.isEmpty { NeedsYouList(agents: model.snapshot.needsYou) }
            section("Workspaces and agents") {
                if model.snapshot.workspaces.isEmpty {
                    Text(supervisor.isRunning ? (model.snapshotError.map { "Not available yet: \($0)" } ?? "No workspaces yet. Open a terminal from tmuxedo.")
                         : "The runner is not running, so this Mac's workspaces cannot be listed.")
                        .font(.system(size: 12)).foregroundStyle(.secondary)
                } else {
                    VStack(alignment: .leading, spacing: 10) {
                        WorkspaceList(workspaces: model.snapshot.workspaces)
                        if !model.snapshot.agents.isEmpty {
                            Divider()
                            ForEach(model.snapshot.agents) { a in
                                HStack(spacing: 8) {
                                    StatusChip(status: a.status)
                                    Text(a.name ?? a.title ?? a.kind.capitalized).font(.system(size: 12)).lineLimit(1)
                                    Spacer()
                                    Text(a.cwd ?? "").font(.caption).foregroundStyle(.secondary).lineLimit(1).truncationMode(.middle)
                                }
                            }
                        }
                    }
                }
            }
            section("Open in the browser") {
                HStack(spacing: 10) {
                    link("This machine", settings.machine.map { "/machines/\($0.machineId)" } ?? "/machines")
                    link("Needs you", "/agents")
                    link("Dashboard", "/")
                    link("Machines", "/settings/machines")
                }
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    private func link(_ title: String, _ path: String) -> some View {
        Button(title) { if let u = settings.webURL(path) { NSWorkspace.shared.open(u) } }
            .disabled(settings.server == nil)
    }

    private func section<C: View>(_ title: String, @ViewBuilder _ content: () -> C) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(title.uppercased()).font(.system(size: 10, weight: .semibold)).foregroundStyle(.secondary)
            content()
        }
    }
}

/// An install.sh setup was found: say what will happen to it, in words.
private struct MigrationBanner: View {
    @EnvironmentObject var model: AppModel

    var body: some View {
        HStack(alignment: .top, spacing: 10) {
            Image(systemName: "arrow.triangle.2.circlepath").foregroundStyle(.blue)
            VStack(alignment: .leading, spacing: 4) {
                Text("This Mac still has the command-line service installed.").font(.system(size: 12, weight: .semibold))
                Text("tmuxedo will retire its runner (it holds no terminals). Your current terminals stay in the old holder "
                     + "until it exits, at the latest when you log out; nothing is closed now.")
                    .font(.caption).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
            }
            Spacer()
            Button("Move to the app") { model.finishMigration(); model.start() }
        }
        .padding(.horizontal, 20)
        .padding(.vertical, 8)
        .background(Color.blue.opacity(0.08))
    }
}

/// The machine, its key, its holder and the switches that matter.
struct MacFacts: View {
    @EnvironmentObject var model: AppModel
    @EnvironmentObject var settings: Settings
    @EnvironmentObject var supervisor: Supervisor
    @State private var confirmKill = false

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            Grid(alignment: .leading, horizontalSpacing: 16, verticalSpacing: 6) {
                row("Machine", settings.machine.map { "\($0.name) (\($0.machineId.isEmpty ? "id unknown" : $0.machineId))" } ?? "not paired")
                row("Paired", settings.machine.map { "\($0.pairedAt.formatted(date: .abbreviated, time: .shortened)) · \($0.method)" } ?? "—")
                row("Credential", settings.machine.map { $0.keyBound ? "bound to this Mac's key" : "bearer token (pair again for a key-bound one)" } ?? "—")
                row("Machine key", settings.machine?.keyFingerprint.map { String($0.prefix(16)) } ?? "—")
                row("Key proofs signed", "\(supervisor.signedProofs) since launch")
                row("Holder", model.holder?.state.sentence ?? "the runner's own (profile copy)")
                row("Runner", supervisor.pid.map { "pid \($0) · \(supervisor.restarts) restart\(supervisor.restarts == 1 ? "" : "s")" } ?? "not running")
                row("Notifications", model.pushStatus)
            }
            .font(.system(size: 12))
            Divider()
            VStack(alignment: .leading, spacing: 6) {
                Button(role: .destructive) { confirmKill = true } label: { Label("Kill switch", systemImage: "bolt.slash.fill") }
                Text("Stops the runner AND the terminal holder (every terminal on this Mac ends), and deletes this Mac's "
                     + "credential and key here. Works without tmuxedo's servers. Revoke the Mac on tmuxedo.dev as well.")
                    .font(.caption).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
            }
            if let m = model.lastMessage { Text(m).font(.caption).foregroundStyle(.secondary) }
            Divider()
            VStack(alignment: .leading, spacing: 4) {
                Text("REMOTE ACCESS LOG").font(.system(size: 10, weight: .semibold)).foregroundStyle(.secondary)
                if supervisor.remoteLog.isEmpty {
                    Text("No remote attach or decision since the app started.").font(.caption).foregroundStyle(.secondary)
                } else {
                    ForEach(supervisor.remoteLog.suffix(50).reversed()) { l in
                        HStack(spacing: 8) {
                            Text(Terminal.time.string(from: l.at)).font(.caption.monospaced()).foregroundStyle(.secondary)
                            Text(l.text).font(.caption.monospaced()).textSelection(.enabled)
                        }
                    }
                }
            }
        }
        .confirmationDialog("Stop everything and forget this Mac's credential?", isPresented: $confirmKill) {
            Button("Kill switch: end all terminals", role: .destructive) { model.killSwitch() }
        } message: {
            Text("Every terminal on this Mac ends now. You will have to pair this Mac again.")
        }
    }

    private func row(_ k: String, _ v: String) -> some View {
        GridRow {
            Text(k).foregroundStyle(.secondary)
            Text(v).textSelection(.enabled)
        }
    }
}

/// DEV BUILD, loudly, or the release version, quietly.
struct BuildBadge: View {
    let info = BuildInfo.current

    var body: some View {
        HStack(spacing: 6) {
            if let why = info.devReason {
                Text("DEV BUILD")
                    .font(.system(size: 10, weight: .heavy, design: .monospaced))
                    .lineLimit(1).fixedSize()
                    .padding(.horizontal, 7).padding(.vertical, 2)
                    .background(Color.purple.opacity(0.22), in: Capsule())
                    .overlay(Capsule().strokeBorder(Color.purple.opacity(0.6), lineWidth: 1))
                    .foregroundStyle(.purple)
                    .help("This is \(why).\n\(info.summary)")
                    .accessibilityLabel("Development build: \(why)")
            }
            Text(info.isRelease ? "v\(info.version) (\(info.build))" : "v\(info.version) (\(info.build)) · \(info.commit)")
                .font(.system(size: 10, weight: .medium, design: .monospaced))
                .lineLimit(1).fixedSize()
                .padding(.horizontal, 7).padding(.vertical, 2)
                .background(Color.primary.opacity(0.07), in: Capsule())
                .foregroundStyle(.secondary)
                .textSelection(.enabled)
                .help(info.summary)
        }
    }
}
