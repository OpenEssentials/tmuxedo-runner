import Foundation

/// What a log line is, so the terminal can colour it.
///
/// Classified from the line itself, because the lines come from two places:
/// the app's own supervisor notes and the runner's stderr. The rules are
/// ordered and tested against the exact lines both write (LogKindTests), so a
/// reworded message that loses its colour fails a test rather than quietly
/// turning grey, and no failure can come out green.
enum LogKind: Equatable {
    /// Something went wrong that a person should read.
    case error
    /// Transient or notable: a dropped connection, a retry, a holder waiting.
    case warning
    /// Connected, paired, signed.
    case success
    /// The app's and runner's own life: starting, stopping, updates.
    case lifecycle
    /// Pairing and the machine key.
    case security
    case info

    static func classify(_ text: String, isError: Bool) -> LogKind {
        // Runner lines start with an ISO timestamp; classify on what follows it.
        let t = Self.stripTimestamp(text).lowercased()

        let errors = ["refused", "failed", "cannot ", "could not", "error", "not paired", "revoked",
                      "proof_failed", "signature", "exited with", "invalid"]
        let warnings = ["disconnected", "reconnecting", "retrying", "waiting for it to exit", "slow_down",
                        "gateway silent", "will restart", "holder restarts", "backing off"]
        // A refusal or failure is never green, whatever else the line says.
        if isError || errors.contains(where: t.contains) { return .error }
        if warnings.contains(where: t.contains) { return .warning }
        if t.hasPrefix("connected to ") || t.hasPrefix("paired") || t.contains("welcome") { return .success }
        if t.contains("key challenge") || t.hasPrefix("pairing") || t.contains("machine key") { return .security }
        if t.hasPrefix("starting") || t.hasPrefix("tmuxedo-runner ") || t.hasPrefix("stopped")
            || t.hasPrefix("stopping") || t.hasPrefix("holder ") || t.hasPrefix("managed by")
            || t.hasPrefix("update") || t.hasPrefix("build:") {
            return .lifecycle
        }
        return .info
    }

    /// `2026-10-05T12:00:00.000Z connected to …` → `connected to …`.
    static func stripTimestamp(_ s: String) -> String {
        guard s.count > 20, s.first?.isNumber == true, let space = s.firstIndex(of: " "),
              s[s.startIndex..<space].contains("T")
        else { return s }
        return String(s[s.index(after: space)...])
    }
}
