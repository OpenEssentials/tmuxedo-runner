import Foundation

/// The platform passkey sheet. The app's AuthenticationServices implementation lives in
/// App/PairingAuth.swift; tests stand in for it.
@MainActor
protocol PairingAuthenticator: AnyObject {
    /// A passkey assertion for RP `tmuxedo.dev` with the server's request options → the JSON the server verifies.
    func passkeyAssertion(options: [String: Any]) async throws -> [String: Any]
}

/// "Pair this Mac" (docs/design/mac-pairing.md §2): one attempt, every way to approve it.
///
///   1. the iPhone ("Sign in with your iPhone"): scan the QR code with the tmuxedo app, type the
///      match code there, approve (step-up on the phone);
///   2. a passkey here (both factors at once);
///   3. a browser: open the link, same as 1.
///
/// No Sign in with Apple on the Mac: Apple does not offer it to Developer ID apps.
/// Path 2 approves with a short-lived app session that is signed out as soon as the
/// pairing is approved. The machine then collects its key-bound
/// token by polling, whatever approved it.
@MainActor
final class PairingFlow: ObservableObject {
    enum Phase: Equatable {
        case idle
        case starting
        /// Waiting for an approval; `opened`: someone has the approve screen open.
        case waiting(opened: Bool)
        case signingIn(String)
        case approving
        case paired(PairedMachine)
        case failed(String)
    }

    @Published private(set) var phase: Phase = .idle
    @Published private(set) var started: PairingAPI.Started?
    @Published private(set) var expiresAt: Date?

    private let api: PairingAPI
    private let keys: MachineKeyStore
    private let facts: PairingAPI.MachineFacts
    private weak var auth: PairingAuthenticator?
    private let onPaired: (PairedMachine, String) -> Void
    private let now: () -> Date
    private let sleep: (TimeInterval) async -> Void
    private var key: MachineKey?
    private var grant: PairingAPI.Grant?
    private var pollTask: Task<Void, Never>?

    init(api: PairingAPI, keys: MachineKeyStore, facts: PairingAPI.MachineFacts, auth: PairingAuthenticator?,
         now: @escaping () -> Date = Date.init,
         sleep: @escaping (TimeInterval) async -> Void = { try? await Task.sleep(nanoseconds: UInt64($0 * 1e9)) },
         onPaired: @escaping (PairedMachine, String) -> Void) {
        self.api = api
        self.keys = keys
        self.facts = facts
        self.auth = auth
        self.now = now
        self.sleep = sleep
        self.onPaired = onPaired
    }

    var matchCode: String? { started?.matchCode }
    /// "123 456".
    var matchCodeSpaced: String? { started.map { String($0.matchCode.prefix(3)) + " " + String($0.matchCode.dropFirst(3)) } }
    var browserURL: URL? { started.flatMap { URL(string: $0.url) } }
    /// What the QR code carries: the universal link, which opens the phone app (or the browser without it).
    var qrPayload: String? { started?.appUrl }
    var keyIsHardwareBacked: Bool { key?.isHardwareBacked ?? true }

    // MARK: Start

    /// A new attempt with a NEW key (a pairing never reuses an old key).
    func begin() async {
        pollTask?.cancel()
        phase = .starting
        do {
            let k = try keys.create()
            key = k
            let s = try await api.start(key: k, facts: facts)
            started = s
            expiresAt = now().addingTimeInterval(TimeInterval(s.expiresIn))
            phase = .waiting(opened: false)
            pollTask = Task { [weak self] in await self?.pollLoop() }
        } catch {
            phase = .failed(Self.describe(error))
        }
    }

    func cancel() {
        pollTask?.cancel()
        pollTask = nil
        if let g = grant { let api = self.api; Task { await api.signOut(bearer: g.accessToken, refreshToken: g.refreshToken) } }
        grant = nil
        if case .paired = phase { return }
        phase = .idle
    }

    // MARK: Poll

    func pollLoop() async {
        guard let s = started, let key else { return }
        var interval = TimeInterval(max(1, s.interval))
        while !Task.isCancelled {
            await sleep(interval)
            if Task.isCancelled { return }
            if let exp = expiresAt, now() > exp.addingTimeInterval(10) { phase = .failed("The pairing expired. Start again."); return }
            let r: PairingAPI.PollResult
            do { r = try await api.poll(s, key: key, now: now()) } catch { continue }
            switch r {
            case let .pending(opened):
                if case .waiting = phase { phase = .waiting(opened: opened) }
            case .slowDown:
                interval += 2
            case .denied:
                phase = .failed("The pairing was denied, or the code did not match three times. Start again.")
                return
            case .expired:
                phase = .failed("The pairing expired. Start again.")
                return
            case .proofRefused:
                phase = .failed("tmuxedo did not accept this Mac's key. Start again.")
                return
            case let .delivered(token, machineId, server, method):
                let m = PairedMachine(machineId: machineId, name: facts.deviceName, server: server, method: method,
                                      pairedAt: now(), keyBound: true, keyFingerprint: key.fingerprint)
                if let g = grant { await api.signOut(bearer: g.accessToken, refreshToken: g.refreshToken) }
                grant = nil
                phase = .paired(m)
                onPaired(m, token)
                return
            }
        }
    }

    // MARK: In-app approval

    /// A passkey: both factors at once. Cancelling the passkey sheet goes back to where the
    /// user was, with no error (queue 3k: cancel means cancel).
    func usePasskey() async {
        guard let auth else { return }
        let before = phase
        phase = .signingIn("Passkey")
        do {
            let (nonce, options) = try await api.passkeyOptions()
            let assertion = try await auth.passkeyAssertion(options: options)
            grant = try await api.passkeyVerify(nonce: nonce, response: assertion, deviceName: facts.deviceName)
            await approveSelf()
        } catch let e as PairingAPI.APIError where e.code == "cancelled" {
            // Unless the pairing moved on meanwhile (approved from the phone, expired).
            if phase == .signingIn("Passkey") { phase = before }
            lastError = nil
        } catch {
            phase = .failed(Self.describe(error))
            lastError = Self.describe(error)
        }
    }

    @Published private(set) var lastError: String?

    private func approveSelf() async {
        guard let s = started, let g = grant else { return }
        phase = .approving
        do {
            _ = try await api.selfApprove(s, bearer: g.accessToken)
            // The poll loop collects the token now.
            phase = .waiting(opened: true)
        } catch {
            phase = .failed(Self.describe(error))
            lastError = Self.describe(error)
        }
    }

    static func describe(_ error: Error) -> String {
        if let e = error as? PairingAPI.APIError { return e.message }
        if let e = error as? URLError {
            switch e.code {
            case .notConnectedToInternet, .networkConnectionLost: return "This Mac is offline. Connect to the internet and try again."
            case .cannotFindHost, .cannotConnectToHost, .timedOut: return "tmuxedo could not be reached. Try again in a moment."
            default: return e.localizedDescription
            }
        }
        return error.localizedDescription
    }
}
