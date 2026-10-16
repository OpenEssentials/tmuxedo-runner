import Foundation

/// What the menu bar says.
///
/// THIS ENUM IS STILL THE POINT OF THE APP. "Connected", "stopped",
/// "cannot reach tmuxedo" and "tmuxedo refused this Mac" must never render
/// alike: a display that does is one that reports a reachable Mac when nobody
/// can reach it. Each state has one sentence, shown verbatim everywhere.
enum Display: Equatable {
    /// Not paired with an account yet.
    case notPaired
    /// Paired, but the person stopped the runner (terminals stay alive in the holder).
    case off
    /// The runner is starting or reconnecting.
    case connecting(attempt: Int)
    /// The runner is connected and serving terminals.
    case connected(since: Date)
    /// The runner keeps exiting or cannot reach tmuxedo; retrying with backoff. NOT connected.
    case unreachable(since: Date, error: String, retryIn: TimeInterval)
    /// tmuxedo refused this Mac's credential (revoked, unpaired elsewhere, or a failed key proof). Stopped for good.
    case refused(String)
    /// Something on this Mac stops the runner from starting (missing binary, bad signature, no key).
    case broken(String)
    /// Stopping (Stop, Quit or an update).
    case stopping

    /// One sentence, shown verbatim everywhere the state is described.
    var sentence: String {
        switch self {
        case .notPaired:
            return "This Mac is not paired with tmuxedo yet."
        case .off:
            return "Stopped. Your terminals are still running; nobody can reach them until you press Start."
        case let .connecting(n):
            return n <= 1 ? "Connecting to tmuxedo…" : "Reconnecting to tmuxedo (attempt \(n))…"
        case let .connected(since):
            return "Connected since \(Self.time.string(from: since)). Your terminals are reachable."
        case let .unreachable(since, error, retry):
            return "CANNOT REACH TMUXEDO since \(Self.time.string(from: since)): \(error). "
                + "Trying again in \(Int(retry.rounded())) s. This is not the same as connected."
        case let .refused(why):
            return "tmuxedo REFUSED this Mac: \(why) Stopped. Pair it again to reconnect."
        case let .broken(why):
            return "Cannot start: \(why)"
        case .stopping:
            return "Stopping."
        }
    }

    var animating: Bool {
        if case .connecting = self { return true }
        return false
    }

    /// True when something needs a person to look. Drives the warning tint.
    var isFault: Bool {
        switch self {
        case .unreachable, .refused, .broken: return true
        default: return false
        }
    }

    /// True while the supervisor is (re)starting or running the runner.
    var isRunning: Bool {
        switch self {
        case .connecting, .connected, .unreachable: return true
        default: return false
        }
    }

    var isConnected: Bool {
        if case .connected = self { return true }
        return false
    }

    var symbol: String {
        switch self {
        case .notPaired: return "link.badge.plus"
        case .off: return "pause.circle"
        case .connecting: return "antenna.radiowaves.left.and.right"
        case .connected: return "terminal"
        case .unreachable: return "wifi.exclamationmark"
        case .refused, .broken: return "exclamationmark.triangle.fill"
        case .stopping: return "stop.circle"
        }
    }

    static let time: DateFormatter = {
        let f = DateFormatter()
        f.dateFormat = "HH:mm"
        return f
    }()
}
