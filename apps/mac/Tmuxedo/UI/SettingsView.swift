import SwiftUI

/// Settings: the connection, terminals, startup, notifications, updates, and Unpair.
struct SettingsView: View {
    var body: some View {
        TabView {
            GeneralTab().tabItem { Label("General", systemImage: "gearshape") }
            TerminalsTab().tabItem { Label("Terminals", systemImage: "terminal") }
            IsolationTab().tabItem { Label("Isolation", systemImage: "person.badge.shield.checkmark") }
            SecurityTab().tabItem { Label("Pairing", systemImage: "lock.shield") }
            TerminalLogView().padding(12).tabItem { Label("Log", systemImage: "list.bullet.rectangle") }
        }
        .frame(width: 640, height: 560)
    }
}

private struct GeneralTab: View {
    @EnvironmentObject var model: AppModel
    @EnvironmentObject var settings: Settings
    @EnvironmentObject var supervisor: Supervisor
    @State private var loginItemOn = LoginItem.isEnabled
    @State private var loginItemError: String?

    var body: some View {
        Form {
            Section("Startup") {
                Toggle("Open tmuxedo at login", isOn: $loginItemOn)
                    .disabled(Profile.name != nil)
                    .onChange(of: loginItemOn) { _, wanted in
                        loginItemError = LoginItem.set(wanted)
                        loginItemOn = LoginItem.isEnabled
                    }
                if let loginItemError { Text(loginItemError).font(.caption).foregroundStyle(.orange) }
                Toggle("Connect when tmuxedo opens", isOn: $settings.connectAtLaunch)
                Text("Your terminals survive quitting this app and its updates either way: they live in the terminal holder.")
                    .font(.caption).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
                Toggle("Keep this Mac awake while connected", isOn: $settings.keepAwake)
                    .onChange(of: settings.keepAwake) { _, _ in model.applyKeepAwake() }
                Text("The display still sleeps; closing the lid still sleeps the Mac.")
                    .font(.caption).foregroundStyle(.secondary)
            }
            Section("Notifications") {
                Toggle("Tell me when an agent needs me, and about new pairings", isOn: $settings.notifications)
                    .onChange(of: settings.notifications) { _, on in if on { model.registerPushIfPossible() } }
                LabeledContent("Push") { Text(model.pushStatus).font(.caption) }
                Text("Notifications never contain terminal content. Answering opens tmuxedo, which asks you to confirm it's you.")
                    .font(.caption).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
            }
            UpdatesSection()
        }
        .formStyle(.grouped)
    }
}

private struct TerminalsTab: View {
    @EnvironmentObject var settings: Settings
    @EnvironmentObject var supervisor: Supervisor

    var body: some View {
        Form {
            Section("Work folder") {
                TextField("Folder new terminals start in", text: $settings.workDir, prompt: Text("~/work"))
                    .textFieldStyle(.roundedBorder)
                Text(settings.resolvedWorkDir.map { "New terminals start in \($0)." }
                     ?? (settings.workDir.isEmpty ? "Default: ~/work (created if missing)." : "Use ~/… or an absolute path."))
                    .font(.caption).foregroundStyle(settings.workDir.isEmpty || settings.resolvedWorkDir != nil ? Color.secondary : Color.orange)
                Text("A work folder set for this machine on tmuxedo.dev takes precedence.")
                    .font(.caption).foregroundStyle(.secondary)
            }
            Section("Launch options") {
                Text("Claude launch options and your shell setup (aliases, environment) are kept in your tmuxedo account, "
                     + "for every machine or for this one.")
                    .font(.caption).fixedSize(horizontal: false, vertical: true)
                Button("Edit in tmuxedo…") {
                    if let u = settings.webURL(settings.machine.map { "/machines/\($0.machineId)/settings" } ?? "/settings/terminal") {
                        NSWorkspace.shared.open(u)
                    }
                }
            }
            if supervisor.display.isRunning {
                Section { Text("A new work folder applies after Restart.").font(.caption).foregroundStyle(.orange) }
            }
        }
        .formStyle(.grouped)
    }
}

private struct SecurityTab: View {
    @EnvironmentObject var model: AppModel
    @EnvironmentObject var settings: Settings
    @State private var confirmUnpair = false
    @State private var busy = false

    var body: some View {
        Form {
            Section("Server") {
                TextField("tmuxedo server", text: $settings.serverURL).textFieldStyle(.roundedBorder)
                    .disabled(settings.isPaired)
                if settings.server == nil { Text("Must be an https:// address.").font(.caption).foregroundStyle(.orange) }
                if settings.isPaired { Text("Unpair to change the server.").font(.caption).foregroundStyle(.secondary) }
            }
            Section("Controls on this Mac") {
                Toggle("Observe-only: remote viewers can watch, not type", isOn: $settings.observeOnly)
                    .onChange(of: settings.observeOnly) { _, _ in if model.supervisor.isRunning { model.supervisor.restart() } }
                Text("Enforced by this Mac, not by tmuxedo's servers. The account this Mac serves is pinned at its first "
                     + "connection: tmuxedo cannot hand it to anyone else.")
                    .font(.caption).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
                if let u = settings.machine?.userId { LabeledContent("Serves account") { Text(u).font(.caption.monospaced()) } }
            }
            Section("This Mac") {
                if let m = settings.machine {
                    LabeledContent("Paired as") { Text(m.name) }
                    LabeledContent("Approved with") { Text(m.method) }
                    LabeledContent("Credential") { Text(m.keyBound ? "bound to this Mac's key" : "bearer token") }
                    if let fp = m.keyFingerprint { LabeledContent("Key") { Text(String(fp.prefix(16))).font(.caption.monospaced()) } }
                    if !m.keyBound {
                        Button("Pair again with this Mac's key") { NotificationCenter.default.post(name: LaunchKind.showPairing, object: nil) }
                    }
                    Button("Unpair this Mac…", role: .destructive) { confirmUnpair = true }.disabled(busy)
                    Text("tmuxedo revokes this Mac at once; its key and credential are deleted here. Your terminals keep running.")
                        .font(.caption).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
                } else {
                    Button("Pair this Mac…") { NotificationCenter.default.post(name: LaunchKind.showPairing, object: nil) }
                }
            }
        }
        .formStyle(.grouped)
        .confirmationDialog("Unpair this Mac?", isPresented: $confirmUnpair) {
            Button("Unpair", role: .destructive) {
                busy = true
                Task { await model.unpair(); busy = false }
            }
        }
    }
}

private struct UpdatesSection: View {
    @EnvironmentObject var updates: UpdateController

    var body: some View {
        Section("Updates") {
            LabeledContent("This version") {
                Text("\(BuildInfo.current.version) (\(BuildInfo.current.build)) · \(BuildInfo.current.channel)").font(.caption.monospaced())
            }
            Toggle("Check for updates automatically", isOn: Binding(get: { updates.automaticallyChecks }, set: { updates.automaticallyChecks = $0 }))
                .disabled(!updates.enabled)
            Toggle("Download and install updates automatically", isOn: Binding(get: { updates.automaticallyInstalls }, set: { updates.automaticallyInstalls = $0 }))
                .disabled(!updates.enabled)
            Text("Updates come from Motorcade and are verified against this app's signing key before they install. "
                 + "Installing keeps your terminals running.")
                .font(.caption).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
            HStack {
                Text(updates.lastResult + (updates.lastCheck.map { " · \($0.formatted(date: .omitted, time: .shortened))" } ?? ""))
                    .font(.caption).foregroundStyle(.secondary)
                Spacer()
                Button("Check Now") { updates.checkNow() }.disabled(!UpdateController.allowed)
            }
        }
    }
}
