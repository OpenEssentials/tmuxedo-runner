import Foundation
import IOKit.pwr_mgt

/// Keeps the Mac from IDLE-sleeping while it is connected, when the owner wants that.
///
/// A sleeping Mac drops off the network: its terminals stay alive in the
/// holder, but nobody can reach them until it wakes. With "Keep this Mac
/// awake" on, this holds a `PreventUserIdleSystemSleep` assertion (the same
/// mechanism as `caffeinate -i`) while the runner is connected.
///
/// AND IT TURNS OFF APP NAP for the app, so the runner's supervision timers
/// are not throttled while the app has no window open.
///
/// WHAT IT DOES NOT DO:
///   · the DISPLAY still sleeps on its normal schedule;
///   · CLOSING THE LID still sleeps the Mac (preventing that needs root).
///
/// SAFE BY CONSTRUCTION: macOS releases an assertion when the process that
/// holds it exits, crash included, so this can never be left stuck on.
final class SleepGuard {
    private var id: IOPMAssertionID = 0
    private var activity: NSObjectProtocol?
    private(set) var isHeld = false
    /// App Nap is off for this app while this is true.
    var appNapDisabled: Bool { activity != nil }

    /// ASCII on purpose: `pmset -g assertions` mangles non-ASCII names.
    static let reason = "tmuxedo keeps this Mac reachable - sleeping would drop its terminals off the network"

    /// Take the assertion. Idempotent. Returns whether it is held afterwards.
    @discardableResult
    func hold() -> Bool {
        guard !isHeld else { return true }
        let rc = IOPMAssertionCreateWithName(
            kIOPMAssertionTypePreventUserIdleSystemSleep as CFString,
            IOPMAssertionLevel(kIOPMAssertionLevelOn),
            Self.reason as CFString,
            &id)
        isHeld = rc == kIOReturnSuccess
        if isHeld {
            activity = ProcessInfo.processInfo.beginActivity(
                options: .userInitiatedAllowingIdleSystemSleep, reason: Self.reason)
        }
        return isHeld
    }

    /// Let the Mac sleep normally again. Idempotent.
    func release() {
        guard isHeld else { return }
        if let activity { ProcessInfo.processInfo.endActivity(activity) }
        activity = nil
        IOPMAssertionRelease(id)
        id = 0
        isHeld = false
    }

    deinit { release() }

    /// The assertions macOS reports for THIS process, by name: what the power
    /// manager believes, rather than what this object believes.
    static func assertionsHeldByThisProcess() -> [String] {
        var raw: Unmanaged<CFDictionary>?
        guard IOPMCopyAssertionsByProcess(&raw) == kIOReturnSuccess,
              let byPid = raw?.takeRetainedValue() as? [NSNumber: [[String: Any]]]
        else { return [] }
        let mine = byPid[NSNumber(value: getpid())] ?? []
        return mine.compactMap { $0[kIOPMAssertionNameKey] as? String }
    }
}
