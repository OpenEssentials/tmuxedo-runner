import AppKit
import SwiftUI
import UserNotifications

/// Settings → Isolation: where terminals run, the folders the separate user may reach,
/// confirm-on-this-Mac, and removing the separate user (docs/design/mac-isolation.md).
struct IsolationTab: View {
    @EnvironmentObject var model: AppModel
    @EnvironmentObject var settings: Settings

    var body: some View {
        if let iso = model.iso {
            IsolationForm(iso: iso)
        } else {
            Form {
                Section("Isolation") {
                    Text("This copy of tmuxedo is a test profile: its terminals run as you (full access), and the separate user is not offered.")
                        .font(.caption).fixedSize(horizontal: false, vertical: true)
                }
                ConfirmSection()
            }
            .formStyle(.grouped)
        }
    }
}

private struct IsolationForm: View {
    @EnvironmentObject var model: AppModel
    @EnvironmentObject var settings: Settings
    @ObservedObject var iso: IsolationManager
    @State private var confirmFull = false
    @State private var confirmMove = false
    @State private var confirmRemove = false
    @State private var working = false

    var body: some View {
        Form {
            Section("Where terminals run") {
                LabeledContent("Now") {
                    Text(settings.isolation == .separateUser ? "As the separate user “\(Iso.user)”" : "As you (full access)")
                        .foregroundStyle(settings.isolation == .separateUser ? Color.primary : Color.orange)
                }
                Text(settings.isolation.summary).font(.caption).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
                if settings.isolation == .fullAccess {
                    Button("Move to the separate user…") { confirmMove = true }.disabled(working || iso.busy != nil)
                } else {
                    Button("Switch to full access…") { confirmFull = true }.disabled(working || iso.busy != nil)
                    Text("Full access is for work that needs your own tools and credentials (Xcode builds, signing).")
                        .font(.caption).foregroundStyle(.secondary)
                }
            }
            Section("Separate user") {
                LabeledContent("State") { Text(phaseText).font(.caption) }
                if let busy = iso.busy { HStack { ProgressView().controlSize(.small); Text(busy).font(.caption) } }
                if let e = iso.lastError { Text(e).font(.caption).foregroundStyle(.orange).fixedSize(horizontal: false, vertical: true) }
                if iso.phase == .needsApproval {
                    Button("Open Login Items") {
                        NSWorkspace.shared.open(URL(string: "x-apple.systempreferences:com.apple.LoginItems-Settings.extension")!)
                    }
                    Text("macOS asks an administrator to allow tmuxedo's helper once. It can only create the separate user, "
                         + "start and stop its terminals, and remove it again.")
                        .font(.caption).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
                }
                if iso.phase != .ready {
                    Button("Set up the separate user") { Task { working = true; await iso.setUp(); working = false } }
                        .disabled(working || iso.busy != nil)
                } else {
                    LabeledContent("Terminal holder") { Text(iso.status.holder.running ? "running" : "stopped").font(.caption) }
                    LabeledContent("Runner") { Text(iso.status.runner.running ? "running" : "stopped").font(.caption) }
                    Text("Claude Code signs in separately here: open a terminal from tmuxedo and run `claude` once. "
                         + "Your own Claude login is never shared with the separate user.")
                        .font(.caption).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
                }
            }
            Section("Folders the terminals may use") {
                if settings.grantState.grants.isEmpty {
                    Text("None yet. The terminals start in their own home folder. Grant project folders, never your whole home.")
                        .font(.caption).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
                }
                ForEach(settings.grantState.grants, id: \.self) { g in
                    HStack {
                        Image(systemName: "folder")
                        Text(g).font(.caption.monospaced()).lineLimit(1).truncationMode(.middle)
                        Spacer()
                        Button("Revoke") { Task { await iso.revoke(g) } }.disabled(iso.busy != nil)
                    }
                }
                Button("Grant a folder…") { pickFolder() }.disabled(iso.phase != .ready || iso.busy != nil)
                Text("A granted folder is readable and writable from every terminal, including files like .env in it. "
                     + "Revoking closes it again, including anything the terminals created there.")
                    .font(.caption).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
            }
            ConfirmSection()
            Section("Remove") {
                Button("Remove the separate user…", role: .destructive) { confirmRemove = true }
                    .disabled(iso.phase == .notSetUp || iso.busy != nil)
                Text("Ends its terminals and deletes the user, its home folder (and its Claude login), its background "
                     + "services and every folder grant. tmuxedo then runs terminals as you.")
                    .font(.caption).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
            }
        }
        .formStyle(.grouped)
        .task { await iso.refresh() }
        .alert("Switch to full access?", isPresented: $confirmFull) {
            Button("Switch to full access", role: .destructive) { Task { working = true; await model.switchToFullAccess(); working = false } }
            Button("Cancel", role: .cancel) {}
        } message: {
            Text("Terminals will run as you. Anything typed in them, by you, an agent, or anyone who takes control from "
                 + "tmuxedo, can read your files, ~/.ssh and your Keychain items, and change anything you can. "
                 + "The separate user's terminals end now.")
        }
        .alert("Move to the separate user?", isPresented: $confirmMove) {
            Button("Move") { Task { working = true; _ = await model.moveToSeparateUser(); working = false } }
            Button("Cancel", role: .cancel) {}
        } message: {
            Text("tmuxedo creates a hidden standard macOS user “\(Iso.user)” (an administrator approves this once) and "
                 + "runs every terminal as it. Your current terminals END, and your sessions resume as that user: their "
                 + "layout and the Claude conversations in folders you granted are copied over. Grant your project "
                 + "folders first. Claude Code needs signing in once more, as that user.")
        }
        .alert("Remove the separate user?", isPresented: $confirmRemove) {
            Button("Remove", role: .destructive) { Task { working = true; _ = await model.removeSeparateUser(); working = false } }
            Button("Cancel", role: .cancel) {}
        } message: {
            Text("Its terminals end, and its home folder, its Claude login, its services and every folder grant are deleted.")
        }
    }

    private var phaseText: String {
        switch iso.phase {
        case .unknown: return "checking…"
        case .notSetUp: return "not set up"
        case .needsApproval: return "waiting for an administrator's approval"
        case .ready: return "ready (uid \(iso.status.uid.map(String.init) ?? "?"), not an administrator)"
        case let .failed(why): return "failed: \(why)"
        }
    }

    private func pickFolder() {
        let p = NSOpenPanel()
        p.canChooseDirectories = true
        p.canChooseFiles = false
        p.allowsMultipleSelection = false
        p.prompt = "Grant"
        p.message = "Choose a project folder the terminals may read and write."
        guard p.runModal() == .OK, let url = p.url else { return }
        Task { _ = await iso.grant(url.path) }
    }
}

/// Confirm on this Mac: a question before remote control (both modes).
struct ConfirmSection: View {
    @EnvironmentObject var model: AppModel
    @EnvironmentObject var settings: Settings
    @State private var notificationsOff = false

    var body: some View {
        Section("Confirm on this Mac") {
            Toggle("Ask me here before anyone controls a terminal remotely", isOn: $settings.confirmOnMac)
                .onChange(of: settings.confirmOnMac) { _, on in
                    if on { checkNotifications() }
                    if model.supervisor.isRunning { model.supervisor.restart() }
                }
            Picker("Deny automatically after", selection: $settings.confirmTimeout) {
                ForEach(Settings.confirmTimeouts, id: \.self) { Text("\($0) seconds").tag($0) }
            }
            .disabled(!settings.confirmOnMac)
            .onChange(of: settings.confirmTimeout) { _, _ in if model.supervisor.isRunning { model.supervisor.restart() } }
            if notificationsOff {
                Text("Notifications for tmuxedo are off, so it cannot ask you: every remote control request will be denied. "
                     + "Turn them on in System Settings, Notifications.")
                    .font(.caption).foregroundStyle(.orange).fixedSize(horizontal: false, vertical: true)
            }
            Text("Enforced by this Mac's runner: typing, answering prompts and changes wait for your Allow; Deny or no "
                 + "answer drops them. Allowing needs this Mac unlocked. Watching is never blocked, and every request is "
                 + "in the log.")
                .font(.caption).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
        }
        .task { if settings.confirmOnMac { checkNotifications() } }
    }

    private func checkNotifications() {
        UNUserNotificationCenter.current().requestAuthorization(options: [.alert, .sound]) { granted, _ in
            DispatchQueue.main.async { notificationsOff = !granted }
        }
    }
}

/// The dashboard's line about isolation: unfinished setup, or full access in use.
struct IsolationBanner: View {
    @EnvironmentObject var settings: Settings
    @ObservedObject var iso: IsolationManager
    @Environment(\.openSettings) private var openSettings

    var body: some View {
        if settings.isolation == .separateUser && iso.phase != .ready && iso.phase != .unknown {
            row("person.badge.shield.checkmark", "Terminals run as a separate user, which is not set up yet.", .orange) {
                Button("Set up…") { Task { await iso.setUp() } }.disabled(iso.busy != nil)
            }
        } else if settings.isolation == .fullAccess {
            row("exclamationmark.triangle", "Full access: terminals run as you and can reach your files and credentials.", .orange) {
                Button("Isolate…") { openSettings(); NSApp.activate(ignoringOtherApps: true) }
            }
        }
    }

    private func row<A: View>(_ icon: String, _ text: String, _ tint: Color, @ViewBuilder action: () -> A) -> some View {
        HStack(spacing: 10) {
            Image(systemName: icon).foregroundStyle(tint)
            Text(text).font(.system(size: 12)).fixedSize(horizontal: false, vertical: true)
            Spacer()
            action()
        }
        .padding(.horizontal, 20)
        .padding(.vertical, 8)
        .background(tint.opacity(0.08))
    }
}

/// The onboarding sheet's isolation step (a new install starts in separate-user mode).
struct IsolationOnboarding: View {
    @EnvironmentObject var settings: Settings
    @ObservedObject var iso: IsolationManager
    @State private var confirmFull = false

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Label("Your terminals run as a separate, standard macOS user, “\(Iso.user)”. They cannot read your home "
                  + "folder, ~/.ssh or your Keychain, cannot use sudo, and reach only the project folders you grant.",
                  systemImage: "person.badge.shield.checkmark")
                .fixedSize(horizontal: false, vertical: true)
            HStack {
                Button(iso.phase == .ready ? "Separate user ready" : "Set up the separate user") { Task { await iso.setUp() } }
                    .disabled(iso.phase == .ready || iso.busy != nil)
                Button("Use full access instead…") { confirmFull = true }
            }
            if iso.phase == .needsApproval {
                Text("Allow “tmuxedo” in System Settings, General, Login Items (an administrator's approval), then set up again.")
                    .font(.caption).foregroundStyle(.orange).fixedSize(horizontal: false, vertical: true)
            }
            if let e = iso.lastError, iso.phase != .needsApproval {
                Text(e).font(.caption).foregroundStyle(.orange).fixedSize(horizontal: false, vertical: true)
            }
        }
        .task { await iso.refresh() }
        .alert("Use full access?", isPresented: $confirmFull) {
            Button("Use full access", role: .destructive) { settings.isolation = .fullAccess }
            Button("Cancel", role: .cancel) {}
        } message: {
            Text(IsolationMode.fullAccess.summary + " You can isolate them later in Settings.")
        }
    }
}
