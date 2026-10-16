import SwiftUI

/// "Pair this Mac": every way to approve, most convenient first, in one sheet
/// (docs/design/mac-pairing.md §2).
struct PairingView: View {
    @ObservedObject var flow: PairingFlow
    @EnvironmentObject var settings: Settings
    let close: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            Text("Pair this Mac with tmuxedo").font(.title2.weight(.semibold))
            switch flow.phase {
            case .idle, .starting:
                HStack(spacing: 8) { ProgressView().controlSize(.small); Text("Preparing this Mac's key…") }
            case let .failed(why):
                Label(why, systemImage: "exclamationmark.triangle.fill").foregroundStyle(.orange)
                    .fixedSize(horizontal: false, vertical: true)
                Button("Start again") { Task { await flow.begin() } }
            case let .paired(m):
                Label("Paired as \(m.name). This Mac is connecting now.", systemImage: "checkmark.seal.fill")
                    .foregroundStyle(.green)
                Text("Approved with \(m.method). A “new machine paired” email is on its way to your account.")
                    .font(.caption).foregroundStyle(.secondary)
                Button("Done") { close() }.keyboardShortcut(.defaultAction)
            case let .signingIn(what):
                HStack(spacing: 8) { ProgressView().controlSize(.small); Text("\(what)…") }
            case .approving:
                HStack(spacing: 8) { ProgressView().controlSize(.small); Text("Approving…") }
            case let .waiting(opened):
                waiting(opened: opened)
            }
        }
        .padding(24)
        .frame(width: 560)
    }

    // MARK: Waiting: every path

    private func waiting(opened: Bool) -> some View {
        VStack(alignment: .leading, spacing: 18) {
            // 1: the iPhone (most people have the tmuxedo app signed in there already).
            HStack(alignment: .top, spacing: 18) {
                if let payload = flow.qrPayload, let img = QRCode.image(for: payload) {
                    Image(decorative: img, scale: 2)
                        .interpolation(.none)
                        .resizable()
                        .frame(width: 168, height: 168)
                        .padding(6)
                        .background(Color.white, in: RoundedRectangle(cornerRadius: 8))
                        .accessibilityLabel("QR code to sign in with your iPhone")
                }
                VStack(alignment: .leading, spacing: 8) {
                    Text("Sign in with your iPhone").font(.headline)
                    Text("Scan the code with your iPhone's camera: it opens the tmuxedo app. "
                         + "Check where the request comes from, approve, and type this code there:")
                        .font(.callout).fixedSize(horizontal: false, vertical: true)
                    Text(flow.matchCodeSpaced ?? "––– –––")
                        .font(.system(size: 34, weight: .bold, design: .monospaced))
                        .textSelection(.enabled)
                        .accessibilityLabel("Match code \(flow.matchCode ?? "")")
                    Text("Only approve a Mac you are sitting at. tmuxedo never asks you to read this code to anyone.")
                        .font(.caption).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
                }
            }
            Divider()
            // 2 and 3: a passkey here, or the same approval in a browser.
            VStack(alignment: .leading, spacing: 8) {
                Text("Or sign in on this Mac").font(.headline)
                HStack(spacing: 10) {
                    Button { Task { await flow.usePasskey() } } label: { Label("Use a passkey", systemImage: "person.badge.key.fill") }
                        .controlSize(.large)
                    if let u = flow.browserURL {
                        Button("Open in my browser") { NSWorkspace.shared.open(u) }
                            .controlSize(.large)
                    }
                }
                if let e = flow.lastError { Text(e).font(.caption).foregroundStyle(.orange) }
            }
            HStack(spacing: 8) {
                ProgressView().controlSize(.small)
                Text(opened ? "The approval page is open. Finish there…" : "Waiting for your approval…")
                    .font(.caption).foregroundStyle(.secondary)
                Spacer()
                if let exp = flow.expiresAt {
                    TimelineView(.periodic(from: .now, by: 1)) { ctx in
                        let left = max(0, Int(exp.timeIntervalSince(ctx.date)))
                        Text("Expires in \(left / 60):\(String(format: "%02d", left % 60))").font(.caption.monospacedDigit())
                            .foregroundStyle(.secondary)
                    }
                }
            }
            if !flow.keyIsHardwareBacked {
                Text("This Mac has no Secure Enclave: its key is kept in your Keychain instead.")
                    .font(.caption).foregroundStyle(.orange)
            }
        }
    }
}

/// First launch: what the app is, and the macOS background-items prompt explained.
struct OnboardingView: View {
    @EnvironmentObject var model: AppModel
    let done: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            Text("tmuxedo keeps this Mac's terminals reachable").font(.title2.weight(.semibold))
            Label("Your terminals live in a small background helper, the terminal holder. It keeps running when you "
                  + "quit this app and while tmuxedo updates, so nothing you started is lost.",
                  systemImage: "rectangle.stack.badge.play")
                .fixedSize(horizontal: false, vertical: true)
            Label("macOS will say “Background Items Added: tmuxedo”. That is the holder. If macOS asks, allow it in "
                  + "System Settings, General, Login Items.", systemImage: "bell.badge")
                .fixedSize(horizontal: false, vertical: true)
            Label("Pairing binds this Mac to your account with a key kept in this Mac's Secure Enclave. A copied "
                  + "credential is useless anywhere else, and you can unpair from tmuxedo.dev, your phone or here.",
                  systemImage: "lock.shield")
                .fixedSize(horizontal: false, vertical: true)
            if model.holder?.state == .needsApproval {
                Button("Open Login Items") {
                    NSWorkspace.shared.open(URL(string: "x-apple.systempreferences:com.apple.LoginItems-Settings.extension")!)
                }
            }
            if let iso = model.iso, model.settings.isolation == .separateUser {
                IsolationOnboarding(iso: iso)
            }
            HStack {
                Spacer()
                Button("Continue") { done() }.keyboardShortcut(.defaultAction).controlSize(.large)
            }
        }
        .padding(24)
        .frame(width: 520)
    }
}

/// About: the version and the bundled runner's Apache-2.0 NOTICE and licence.
struct AboutView: View {
    private static func resource(_ name: String) -> String {
        Bundle.main.url(forResource: name, withExtension: nil)
            .flatMap { try? String(contentsOf: $0, encoding: .utf8) } ?? "(\(name) is missing from this build)"
    }

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 12) {
                HStack(spacing: 10) {
                    Image(nsImage: NSApp.applicationIconImage).resizable().frame(width: 48, height: 48)
                    VStack(alignment: .leading) {
                        Text("tmuxedo for Mac").font(.title3.weight(.semibold))
                        Text(BuildInfo.current.summary).font(.caption.monospaced()).foregroundStyle(.secondary).textSelection(.enabled)
                    }
                }
                Text("This app bundles the tmuxedo runner, licensed under the Apache License, Version 2.0.")
                    .font(.callout)
                GroupBox("NOTICE") {
                    Text(Self.resource("NOTICE")).font(.caption.monospaced()).textSelection(.enabled)
                        .frame(maxWidth: .infinity, alignment: .leading)
                }
                GroupBox("LICENSE (runner)") {
                    Text(Self.resource("LICENSE-runner.txt")).font(.caption2.monospaced()).textSelection(.enabled)
                        .frame(maxWidth: .infinity, alignment: .leading)
                }
            }
            .padding(20)
        }
    }
}
