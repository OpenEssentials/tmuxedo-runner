import Foundation

/// Confirm on this Mac (docs/design/mac-isolation.md §3): the app's half.
///
/// The RUNNER decides (it holds remote control until it is allowed, and drops it on Deny or a
/// timeout); this broker only asks the person and keeps the answer until the runner polls:
///
///   runner → `confirm`        broker opens a question (a ticket) and shows it (a notification
///                             with Allow / Deny; Allow needs the Mac unlocked)
///   runner → `confirm_poll`   pending | allow | deny | timeout
///
/// A question nobody answers within its timeout is a Deny, decided here as well as in the
/// runner. Thread-safe: the signer threads call in; `present` and `withdraw` are called on the
/// caller's thread and must hop to the main thread themselves.
final class ConfirmBroker {
    enum State: String, Equatable {
        case pending, allow, deny, timeout
    }

    struct Question: Equatable {
        let ticket: String
        let pane: String
        let what: String
        let asked: Date
        let timeout: TimeInterval
    }

    /// Open questions at once; a runner asks one at a time, so more means something is wrong.
    static let maxOpen = 4
    /// Decided questions are remembered this long for the runner's last poll.
    static let keepDecided: TimeInterval = 120

    private let lock = NSLock()
    private var questions: [String: (q: Question, state: State, decided: Date?)] = [:]
    private let now: () -> Date
    private let makeTicket: () -> String

    /// Show a question to the person.
    var present: (Question) -> Void = { _ in }
    /// Take a question down (it was decided or timed out).
    var withdraw: (String) -> Void = { _ in }
    /// Every outcome, for the local attach log.
    var decided: (Question, State) -> Void = { _, _ in }

    init(now: @escaping () -> Date = Date.init,
         makeTicket: @escaping () -> String = { UUID().uuidString.lowercased() }) {
        self.now = now
        self.makeTicket = makeTicket
    }

    /// Open a question. A pane id and a reason come from the runner: both are shown, so both are bounded.
    func ask(pane: String, what: String, timeout: TimeInterval) -> Result<String, ConfirmError> {
        let pane = Self.clean(pane, max: 64)
        let what = Self.clean(what, max: 48)
        let timeout = min(max(timeout, 5), 300)
        lock.lock()
        let expired = expireLocked()
        if questions.values.filter({ $0.state == .pending }).count >= Self.maxOpen {
            lock.unlock()
            report(expired)
            return .failure(.tooMany)
        }
        let q = Question(ticket: makeTicket(), pane: pane, what: what, asked: now(), timeout: timeout)
        questions[q.ticket] = (q, .pending, nil)
        lock.unlock()
        report(expired)
        present(q)
        return .success(q.ticket)
    }

    /// The runner's poll. nil = no such ticket.
    func poll(_ ticket: String) -> State? {
        lock.lock()
        let expired = expireLocked()
        let s = questions[ticket]?.state
        lock.unlock()
        report(expired)
        return s
    }

    /// The person answered (from the notification or the dashboard). A late answer changes nothing.
    @discardableResult
    func answer(_ ticket: String, allow: Bool) -> Bool {
        lock.lock()
        let expired = expireLocked()
        guard var entry = questions[ticket], entry.state == .pending else {
            lock.unlock()
            report(expired)
            return false
        }
        entry.state = allow ? .allow : .deny
        entry.decided = now()
        questions[ticket] = entry
        lock.unlock()
        report(expired)
        withdraw(ticket)
        decided(entry.q, entry.state)
        return true
    }

    /// Open questions, oldest first (the dashboard shows them too).
    var open: [Question] {
        lock.lock()
        let expired = expireLocked()
        let qs = questions.values.filter { $0.state == .pending }.map(\.q).sorted { $0.asked < $1.asked }
        lock.unlock()
        report(expired)
        return qs
    }

    /// Time out what is due and forget what was decided long ago. Returns the newly timed-out.
    @discardableResult
    private func expireLocked() -> [Question] {
        let t = now()
        var timedOut: [Question] = []
        for (k, v) in questions {
            if v.state == .pending, t.timeIntervalSince(v.q.asked) >= v.q.timeout {
                questions[k] = (v.q, .timeout, t)
                timedOut.append(v.q)
            } else if let d = v.decided, t.timeIntervalSince(d) > Self.keepDecided {
                questions[k] = nil
            }
        }
        return timedOut
    }

    private func report(_ expired: [Question]) {
        for q in expired {
            withdraw(q.ticket)
            decided(q, .timeout)
        }
    }

    /// Run the timeouts now (a timer calls this so a question disappears even if nobody polls).
    func tick() {
        lock.lock()
        let expired = expireLocked()
        lock.unlock()
        report(expired)
    }

    static func clean(_ s: String, max: Int) -> String {
        var out = String.UnicodeScalarView()
        var n = 0
        for sc in s.unicodeScalars where sc.value >= 0x20 && sc.value != 0x7F {
            if n >= max { break }
            out.append(sc)
            n += 1
        }
        return String(out)
    }
}

enum ConfirmError: Error, Equatable {
    case tooMany
    var message: String { "too many open questions" }
}
