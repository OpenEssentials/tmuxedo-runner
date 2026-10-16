import AppKit
import Combine
import Sparkle

/// Updates, through Sparkle, from Motorcade's appcast.
///
/// ONE FEED, AND THE BUILD CARRIES IT. Motorcade's Mac lane writes its feed and
/// EdDSA public key into the Info.plist (`SUFeedURL`, `SUPublicEDKey`). Sparkle
/// uses those with NO credential: Motorcade's appcast is public and its
/// download links are short-lived install tokens. The app never sends its
/// runner token, key or anything else to the feed. A copy without a feed (a
/// local or CI build) does not update.
///
/// EVERY UPDATE IS SIGNED. Sparkle refuses an archive whose EdDSA signature does
/// not verify against `SUPublicEDKey`, whatever the server says.
///
/// AN UPDATE KEEPS THE TERMINALS. The holder is its own LaunchAgent: quitting the
/// app for the install stops only the runner, and the relaunched app starts the
/// new runner against the same holder. The holder itself is restarted only when
/// its code changed (HolderPolicy), once, after the relaunch.
@MainActor
final class UpdateController: NSObject, ObservableObject, SPUUpdaterDelegate {
    @Published private(set) var available: String?
    @Published private(set) var lastCheck: Date?
    @Published private(set) var lastResult: String = "Not checked yet"
    @Published private(set) var enabled = false

    private var controller: SPUStandardUpdaterController?
    private let note: (String) -> Void

    /// Test copies (profiles) and builds without Motorcade's feed never update themselves.
    /// TMUXEDO_MAC_ALLOW_UPDATES=1 overrides the profile rule for the updater's own end-to-end check.
    static var allowed: Bool {
        guard case .bundled = FeedSource.current else { return false }
        return Profile.name == nil || ProcessInfo.processInfo.environment["TMUXEDO_MAC_ALLOW_UPDATES"] == "1"
    }

    init(note: @escaping (String) -> Void) {
        self.note = note
        super.init()
    }

    var updater: SPUUpdater? { controller?.updater }

    func start() {
        guard Self.allowed else {
            lastResult = FeedSource.current == .none ? "This build has no update feed (a development build)"
                                                     : "Updates are off in a test (profile) copy"
            return
        }
        if controller == nil {
            controller = SPUStandardUpdaterController(startingUpdater: false, updaterDelegate: self, userDriverDelegate: nil)
        }
        guard let updater, !enabled else { return }
        updater.httpHeaders = [:]
        do {
            try updater.start()
            enabled = true
            updater.checkForUpdatesInBackground()
        } catch {
            lastResult = "The updater could not start: \(error.localizedDescription)"
        }
    }

    /// Sparkle's own window, with the release notes.
    func checkNow() {
        start()
        controller?.checkForUpdates(nil)
    }

    var automaticallyChecks: Bool {
        get { updater?.automaticallyChecksForUpdates ?? true }
        set { updater?.automaticallyChecksForUpdates = newValue; objectWillChange.send() }
    }

    var automaticallyInstalls: Bool {
        get { updater?.automaticallyDownloadsUpdates ?? false }
        set { updater?.automaticallyDownloadsUpdates = newValue; objectWillChange.send() }
    }

    // MARK: - SPUUpdaterDelegate

    /// nil: always the Info.plist feed (Motorcade's).
    nonisolated func feedURLString(for updater: SPUUpdater) -> String? { nil }

    nonisolated func updater(_ updater: SPUUpdater, didFindValidUpdate item: SUAppcastItem) {
        let v = "\(item.displayVersionString) (\(item.versionString))"
        MainActor.assumeIsolated {
            available = v
            lastCheck = Date()
            lastResult = "Version \(v) is available"
            note("update available: \(v)")
        }
    }

    nonisolated func updaterDidNotFindUpdate(_ updater: SPUUpdater) {
        MainActor.assumeIsolated {
            available = nil
            lastCheck = Date()
            lastResult = "Up to date"
        }
    }

    nonisolated func updater(_ updater: SPUUpdater, didAbortWithError error: Error) {
        let e = error as NSError
        let outcome = UpdateOutcome.fromAbort(domain: e.domain, code: e.code, message: e.localizedDescription)
        MainActor.assumeIsolated {
            lastCheck = Date()
            lastResult = outcome.text
            if outcome.isFailure { note("update check failed: \(e.localizedDescription)") }
        }
    }

    /// AUTOMATIC: downloaded and verified. Install now rather than "on quit": this app is
    /// rarely quit, and an install never ends a terminal (the holder is not this process).
    nonisolated func updater(_ updater: SPUUpdater, willInstallUpdateOnQuit item: SUAppcastItem,
                             immediateInstallationBlock: @escaping () -> Void) -> Bool {
        let v = item.displayVersionString
        MainActor.assumeIsolated { note("update \(v) downloaded and verified; installing and relaunching (terminals stay up)") }
        immediateInstallationBlock()
        return true
    }
}
