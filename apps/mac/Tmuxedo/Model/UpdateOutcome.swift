import Foundation

/// What an "aborted" update check actually means.
///
/// SPARKLE REPORTS SOME ORDINARY OUTCOMES AS ERRORS. When there is nothing
/// newer it calls `updater(_:didAbortWithError:)` with SUNoUpdateError —
/// whose own text is "You're up to date!" — in addition to
/// `updaterDidNotFindUpdate`. Rendering every abort as a failure put
/// "Check failed: You're up to date!" in Settings, reported by the owner the
/// day auto-update shipped. Codes from Sparkle's SUErrors.h (2.10.0).
enum UpdateOutcome {
    static let sparkleDomain = "SUSparkleErrorDomain"
    static let noUpdate = 1001
    static let installationCanceled = 4007
    static let installationAuthorizeLater = 4008

    /// The sentence to show, and whether it is actually a failure.
    static func fromAbort(domain: String, code: Int, message: String) -> (text: String, isFailure: Bool) {
        guard domain == sparkleDomain else { return ("Check failed: \(message)", true) }
        switch code {
        case noUpdate: return ("Up to date", false)
        case installationCanceled: return ("Update cancelled", false)
        case installationAuthorizeLater: return ("Update postponed", false)
        default: return ("Check failed: \(message)", true)
        }
    }
}
