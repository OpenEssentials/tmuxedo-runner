import AppKit
import Combine
import SwiftUI
import UserNotifications

/// Menu-bar lifecycle: the status item, its popover, the dashboard, the pairing
/// sheet, notifications, and the runner's life tied to the app's (the holder's is not).
@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate, UNUserNotificationCenterDelegate {
    /// The owner's settings and secrets, or, with TMUXEDO_MAC_PROFILE, a profile's own.
    let settings = Settings()
    lazy var keys = MachineKeyStore(store: settings.secrets)
    lazy var binary = RunnerBinary.bundled()
    lazy var holder: HolderService? = Profile.name == nil
        ? HolderService(registry: SMAgentRegistry(), binary: binary, socket: nil, log: { [weak self] in self?.model.supervisor.note($0) })
        : nil
    /// The privileged helper, reached only if it is signed by our team (an unsigned build has no team to check).
    lazy var helperClient: XPCHelperClient = XPCHelperClient(requirement: RunnerBinary.appTeam.map { (team: String) -> String in
        Iso.requirement(identifier: "dev.tmuxedo.mac.helper", team: team)
    })
    lazy var iso: IsolationManager? = Profile.name == nil
        ? IsolationManager(settings: settings, registry: SMDaemonRegistry(), helper: helperClient)
        : nil
    lazy var daemonLauncher: DaemonLauncher = DaemonLauncher(helper: helperClient, config: { [weak self] () -> IsoRunnerConfig? in
        MainActor.assumeIsolated { self?.model.isoConfig() }
    })
    lazy var model: AppModel = AppModel(settings: settings, keys: keys, binary: binary, holder: holder,
                                        iso: iso, daemonLauncher: daemonLauncher)
    lazy var updates = UpdateController(note: { [weak self] in self?.model.supervisor.note($0) })
    /// CPU and memory, sampled for as long as the app runs.
    lazy var monitor = SystemMonitor(agentGroup: { [weak self] in self?.model.agentScope },
                                     agentsWorking: { [weak self] in self?.model.pollAgentsWorking() ?? false })

    private var statusItem: NSStatusItem!
    private var hosting: NSHostingView<StatusIcon>?
    private let popover = NSPopover()
    private var displayObserver: AnyCancellable?
    private var dashboard: NSWindow?
    private var pairingWindow: NSWindow?
    private var onboardingWindow: NSWindow?
    private var pairing: PairingFlow?
    private var sigterm: DispatchSourceSignal?
    private var launchedAtLogin = false
    private lazy var auth = PairingAuth(rpID: settings.server?.host ?? "tmuxedo.dev")

    func applicationWillFinishLaunching(_ notification: Notification) {
        launchedAtLogin = LaunchKind.isLoginItem(NSAppleEventManager.shared().currentAppleEvent)
        // SIGTERM QUITS PROPERLY (applicationWillTerminate stops the runner); by
        // default it would kill the app on the spot and orphan the runner.
        signal(SIGTERM, SIG_IGN)
        let src = DispatchSource.makeSignalSource(signal: SIGTERM, queue: .main)
        src.setEventHandler { NSApp.terminate(nil) }
        src.resume()
        sigterm = src
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        closeWindowsOpenedAtLaunch()
        model.supervisor.note("build: \(BuildInfo.current.summary)")

        statusItem = NSStatusBar.system.statusItem(withLength: NSStatusItem.variableLength)
        installIcon()
        popover.behavior = .transient
        popover.contentSize = NSSize(width: 360, height: 480)
        popover.contentViewController = NSHostingController(rootView: MenuView()
            .environmentObject(model).environmentObject(settings).environmentObject(model.supervisor))

        // Repaint the icon whenever the state changes (the hosting view observes nothing itself).
        displayObserver = model.supervisor.$display.sink { [weak self] _ in
            DispatchQueue.main.async { self?.refreshIcon() }
        }
        model.onNeedsYou = { [weak self] agent in self?.notifyNeedsYou(agent) }
        iso?.note = { [weak self] in self?.model.supervisor.note($0) }
        wireConfirm()

        UNUserNotificationCenter.current().delegate = self
        UNUserNotificationCenter.current().setNotificationCategories([Self.confirmCategory])
        if settings.notifications || settings.confirmOnMac { requestNotifications() }

        model.boot()
        monitor.start()
        updates.start()

        NotificationCenter.default.addObserver(forName: LaunchKind.showDashboard, object: nil, queue: .main) { [weak self] _ in
            MainActor.assumeIsolated { self?.showDashboard() }
        }
        NotificationCenter.default.addObserver(forName: LaunchKind.showPairing, object: nil, queue: .main) { [weak self] _ in
            MainActor.assumeIsolated { self?.showPairing() }
        }
        NotificationCenter.default.addObserver(forName: NSWindow.willCloseNotification, object: nil, queue: .main) { [weak self] note in
            let w = note.object as? NSWindow
            MainActor.assumeIsolated { self?.windowClosed(w) }
        }
        if !launchedAtLogin {
            showDashboard()
            if !settings.onboarded || holder?.state == .needsApproval { showOnboarding() }
            else if !settings.isPaired && model.migration.enrolment == nil { showPairing() }
        }
    }

    func applicationShouldHandleReopen(_ sender: NSApplication, hasVisibleWindows flag: Bool) -> Bool {
        showDashboard()
        return true
    }

    /// The runner does not outlive the app; the holder (and every terminal) does.
    func applicationWillTerminate(_ notification: Notification) {
        model.supervisor.terminateNow()
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool { false }

    // MARK: - Windows

    func showDashboard() {
        if dashboard == nil {
            let host = NSHostingController(rootView: DashboardView()
                .environmentObject(model).environmentObject(settings).environmentObject(model.supervisor)
                .environmentObject(updates).environmentObject(monitor))
            let w = NSWindow(contentViewController: host)
            let b = BuildInfo.current
            w.title = "tmuxedo \(b.version) (\(b.build))" + (b.isRelease ? "" : " — DEV BUILD")
            w.styleMask = [.titled, .closable, .miniaturizable, .resizable]
            w.isReleasedWhenClosed = false
            w.setContentSize(NSSize(width: 720, height: 680))
            w.setFrameAutosaveName("TmuxedoDashboard")
            if !w.setFrameUsingName("TmuxedoDashboard") { w.center() }
            dashboard = w
        }
        NSApp.setActivationPolicy(.regular)
        NSApp.dockTile.badgeLabel = BuildInfo.current.isRelease ? nil : "DEV"
        dashboard?.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)
        model.startSnapshots()
    }

    func showPairing() {
        guard let server = settings.server else { showDashboard(); return }
        let flow = PairingFlow(api: PairingAPI(server: server), keys: keys, facts: .current, auth: auth) { [weak self] m, token in
            self?.model.paired(m, token: token)
        }
        pairing = flow
        let host = NSHostingController(rootView: PairingView(flow: flow, close: { [weak self] in self?.pairingWindow?.close() })
            .environmentObject(settings))
        let w = pairingWindow ?? NSWindow(contentViewController: host)
        w.contentViewController = host
        w.title = "Pair this Mac"
        w.styleMask = [.titled, .closable]
        w.isReleasedWhenClosed = false
        w.center()
        pairingWindow = w
        NSApp.setActivationPolicy(.regular)
        w.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)
        Task { await flow.begin() }
    }

    func showOnboarding() {
        let host = NSHostingController(rootView: OnboardingView(done: { [weak self] in
            guard let self else { return }
            self.settings.onboarded = true
            self.onboardingWindow?.close()
            self.model.ensureHolder()
            if !self.settings.isPaired && self.model.migration.enrolment == nil { self.showPairing() }
        }).environmentObject(model).environmentObject(settings))
        let w = onboardingWindow ?? NSWindow(contentViewController: host)
        w.contentViewController = host
        w.title = "Welcome to tmuxedo"
        w.styleMask = [.titled, .closable]
        w.isReleasedWhenClosed = false
        w.center()
        onboardingWindow = w
        NSApp.setActivationPolicy(.regular)
        w.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)
    }

    private func windowClosed(_ closing: NSWindow?) {
        if closing === pairingWindow { pairing?.cancel(); pairing = nil }
        if closing === dashboard { model.stopSnapshots() }
        DispatchQueue.main.async {
            let open = NSApp.windows.filter { $0 !== closing && $0.isVisible && $0.styleMask.contains(.titled) }
            if open.isEmpty { NSApp.setActivationPolicy(.accessory) }
        }
    }

    // MARK: - Icon

    private func installIcon() {
        guard let button = statusItem.button else { return }
        let view = NSHostingView(rootView: StatusIcon(display: model.supervisor.display))
        view.translatesAutoresizingMaskIntoConstraints = false
        button.addSubview(view)
        NSLayoutConstraint.activate([
            view.leadingAnchor.constraint(equalTo: button.leadingAnchor),
            view.trailingAnchor.constraint(equalTo: button.trailingAnchor),
            view.topAnchor.constraint(equalTo: button.topAnchor),
            view.bottomAnchor.constraint(equalTo: button.bottomAnchor),
        ])
        hosting = view
        button.action = #selector(togglePopover(_:))
        button.target = self
        button.toolTip = model.supervisor.display.sentence
    }

    private func refreshIcon() {
        hosting?.rootView = StatusIcon(display: model.supervisor.display)
        statusItem.button?.toolTip = model.supervisor.display.sentence
    }

    @objc private func togglePopover(_ sender: Any?) {
        guard let button = statusItem.button else { return }
        if popover.isShown {
            popover.performClose(sender)
        } else {
            popover.show(relativeTo: button.bounds, of: button, preferredEdge: .minY)
            popover.contentViewController?.view.window?.makeKey()
            Task { await model.refreshSnapshot() }
        }
    }

    private func closeWindowsOpenedAtLaunch() {
        DispatchQueue.main.async { [unowned self] in
            for window in NSApp.windows where window.styleMask.contains(.titled) && window !== self.dashboard {
                window.close()
            }
        }
    }

    // MARK: - Notifications (docs/design/mac-pairing.md §5)

    func requestNotifications() {
        UNUserNotificationCenter.current().requestAuthorization(options: [.alert, .sound, .badge]) { granted, _ in
            guard granted else { return }
            DispatchQueue.main.async { NSApp.registerForRemoteNotifications() }
        }
    }

    func application(_ application: NSApplication, didRegisterForRemoteNotificationsWithDeviceToken deviceToken: Data) {
        // A Motorcade (Developer ID) build talks to production APNs; anything else to the sandbox.
        model.apnsTokenArrived(deviceToken, sandbox: !BuildInfo.current.isRelease)
    }

    func application(_ application: NSApplication, didFailToRegisterForRemoteNotificationsWithError error: Error) {
        model.supervisor.note("push: macOS did not register for notifications (\(error.localizedDescription)); local notifications only")
    }

    /// A local notification for one of THIS Mac's agents, when tmuxedo's push is not set up
    /// (with push, the server sends it, so it is never shown twice). No terminal content.
    private func notifyNeedsYou(_ agent: LocalAPI.Agent) {
        guard settings.notifications, !model.pushStatus.hasPrefix("Registered") else { return }
        let c = UNMutableNotificationContent()
        c.title = "\(agent.kind.capitalized) needs you · \(settings.machine?.name ?? "this Mac")"
        c.body = "Open tmuxedo to answer."
        c.sound = .default
        c.userInfo = ["url": "/agents"]
        UNUserNotificationCenter.current().add(UNNotificationRequest(identifier: "needs-you-\(agent.id)", content: c, trigger: nil))
    }

    nonisolated func userNotificationCenter(_ center: UNUserNotificationCenter, willPresent notification: UNNotification,
                                            withCompletionHandler completionHandler: @escaping (UNNotificationPresentationOptions) -> Void) {
        completionHandler([.banner, .sound])
    }

    // MARK: - Confirm on this Mac (docs/design/mac-isolation.md §3)

    static let confirmCategory = UNNotificationCategory(
        identifier: "tmuxedo.confirm",
        actions: [
            // Allow needs the Mac unlocked: a question on a locked Mac can only be denied (or time out).
            UNNotificationAction(identifier: "allow", title: "Allow", options: [.authenticationRequired]),
            UNNotificationAction(identifier: "deny", title: "Deny", options: [.destructive]),
        ],
        intentIdentifiers: [], options: [.customDismissAction])

    private var confirmTimer: Timer?

    private func wireConfirm() {
        let broker: ConfirmBroker = model.confirm
        let category: String = Self.confirmCategory.identifier
        broker.present = { (q: ConfirmBroker.Question) in
            DispatchQueue.main.async {
                let c = UNMutableNotificationContent()
                c.title = "Allow remote control of this Mac?"
                c.body = "Someone using tmuxedo wants to control a terminal (\(q.what), pane \(q.pane)). "
                    + "Denied automatically in \(Int(q.timeout)) seconds."
                c.sound = .default
                c.categoryIdentifier = category
                c.userInfo = ["confirmTicket": q.ticket]
                UNUserNotificationCenter.current().add(UNNotificationRequest(identifier: "confirm-\(q.ticket)", content: c, trigger: nil))
            }
        }
        broker.withdraw = { (ticket: String) in
            DispatchQueue.main.async {
                UNUserNotificationCenter.current().removeDeliveredNotifications(withIdentifiers: ["confirm-\(ticket)"])
                UNUserNotificationCenter.current().removePendingNotificationRequests(withIdentifiers: ["confirm-\(ticket)"])
            }
        }
        broker.decided = { [weak self] (q: ConfirmBroker.Question, state: ConfirmBroker.State) in
            DispatchQueue.main.async {
                MainActor.assumeIsolated {
                    self?.model.supervisor.note("confirm on this Mac: \(state.rawValue) (pane \(q.pane), \(q.what))",
                                                error: state != .allow)
                }
            }
        }
        // Timeouts happen even if the runner stops polling.
        confirmTimer = Timer.scheduledTimer(withTimeInterval: 1, repeats: true) { _ in broker.tick() }
    }

    /// The notification itself was clicked (not a button): ask in a dialog.
    private func askInDialog(_ ticket: String) {
        guard let q = model.confirm.open.first(where: { $0.ticket == ticket }) else { return }
        let a = NSAlert()
        a.messageText = "Allow remote control of this Mac?"
        a.informativeText = "Someone using tmuxedo wants to control a terminal (\(q.what), pane \(q.pane)). "
            + "If you did not start this, deny it and use the kill switch."
        a.addButton(withTitle: "Deny")
        a.addButton(withTitle: "Allow")
        a.alertStyle = .warning
        NSApp.activate(ignoringOtherApps: true)
        let r = a.runModal()
        model.confirm.answer(ticket, allow: r == .alertSecondButtonReturn)
    }

    /// Acting on a notification opens tmuxedo in the browser, where approving needs the usual step-up.
    nonisolated func userNotificationCenter(_ center: UNUserNotificationCenter, didReceive response: UNNotificationResponse,
                                            withCompletionHandler completionHandler: @escaping () -> Void) {
        let info = response.notification.request.content.userInfo
        if let ticket = info["confirmTicket"] as? String {
            let action = response.actionIdentifier
            DispatchQueue.main.async {
                MainActor.assumeIsolated {
                    switch action {
                    case "allow": self.model.confirm.answer(ticket, allow: true)
                    case "deny", UNNotificationDismissActionIdentifier: self.model.confirm.answer(ticket, allow: false)
                    default: self.askInDialog(ticket)
                    }
                }
            }
            completionHandler()
            return
        }
        let tm = (info["tmuxedo"] as? [String: Any]) ?? (info["body"] as? [String: Any])
        let path = (tm?["url"] as? String) ?? (info["url"] as? String) ?? "/"
        DispatchQueue.main.async {
            MainActor.assumeIsolated {
                let safe = path.hasPrefix("/") && !path.hasPrefix("//") ? path : "/"
                if let url = self.settings.webURL(safe) { NSWorkspace.shared.open(url) }
            }
        }
        completionHandler()
    }
}
