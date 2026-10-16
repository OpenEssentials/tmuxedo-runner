import XCTest

/// Pairing against the fake tmuxedo: every approval path, the key binding, and the abuse
/// cases the Mac's half can see (docs/design/mac-pairing.md §7.1).
@MainActor
final class PairingTests: XCTestCase, PairingAuthenticator {
    var api: PairingAPI!
    var keys: MachineKeyStore!
    var mem: MemoryStore!
    var paired: [(PairedMachine, String)] = []
    var passkeyID = "good-credential"
    let facts = PairingAPI.MachineFacts(deviceName: "Alex's MacBook Pro", model: "Mac15,3", arch: "aarch64", osVersion: "15.1", appVersion: "0.1.0")

    override func setUp() async throws {
        FakeTmuxedo.reset()
        api = PairingAPI(server: FakeTmuxedo.base, session: FakeTmuxedo.session())
        mem = MemoryStore()
        keys = MachineKeyStore(store: mem, useEnclave: false)
        paired = []
    }

    // PairingAuthenticator stand-in for the system passkey sheet.
    var passkeyCancels = false
    func passkeyAssertion(options: [String: Any]) async throws -> [String: Any] {
        XCTAssertNotNil(options["challenge"])
        if passkeyCancels { throw PairingAPI.APIError(status: 0, code: "cancelled", message: "Cancelled.") }
        return ["id": passkeyID, "rawId": passkeyID, "type": "public-key", "response": [:] as [String: Any]]
    }

    private func flow() -> PairingFlow {
        PairingFlow(api: api, keys: keys, facts: facts, auth: self, sleep: { _ in await Task.yield() }) { [weak self] m, t in
            self?.paired.append((m, t))
        }
    }

    private func waitFor(_ f: PairingFlow, _ pred: @escaping (PairingFlow.Phase) -> Bool, timeout: TimeInterval = 5) async {
        let end = Date().addingTimeInterval(timeout)
        while !pred(f.phase) && Date() < end { try? await Task.sleep(nanoseconds: 10_000_000) }
    }

    func testStartSendsAProvedKeyAndShowsTheCodes() async throws {
        let f = flow()
        await f.begin()
        guard case .waiting = f.phase else { return XCTFail("\(f.phase)") }
        XCTAssertEqual(f.matchCode?.count, 6)
        XCTAssertEqual(f.matchCodeSpaced?.count, 7)
        XCTAssertTrue(f.qrPayload!.contains("/m/pair?code="))
        XCTAssertTrue(f.browserURL!.absoluteString.contains("/pair?code="))
        let start = FakeTmuxedo.state.with { $0.requests.first { $0.path == "/api/pair/start" } }!
        XCTAssertEqual(start.body["client"] as? String, "mac")
        XCTAssertEqual(start.body["os"] as? String, "macos")
        XCTAssertEqual(start.body["model"] as? String, "Mac15,3")
        XCTAssertEqual((start.body["publicKey"] as? [String: String])?["crv"], "P-256")
        XCTAssertNil(start.body["d"])
        XCTAssertTrue(keys.exists)
        f.cancel()
    }

    /// Paths 3 and 4: approved from the phone or a browser; the Mac collects a token bound to its key.
    func testApprovedElsewhereDeliversOnceToThisKey() async throws {
        let f = flow()
        await f.begin()
        let id = FakeTmuxedo.state.with { $0.attempts.last!.id }
        FakeTmuxedo.open(id)
        await waitFor(f) { $0 == .waiting(opened: true) }
        XCTAssertEqual(f.phase, .waiting(opened: true), "the Mac says someone has the approve screen open")
        FakeTmuxedo.approve(id, method: "phone")
        await waitFor(f) { if case .paired = $0 { return true }; return false }
        guard case let .paired(m) = f.phase else { return XCTFail("\(f.phase)") }
        XCTAssertEqual(m.method, "phone")
        XCTAssertTrue(m.keyBound)
        XCTAssertEqual(m.keyFingerprint, try keys.load()?.fingerprint)
        XCTAssertEqual(paired.count, 1)
        XCTAssertTrue(paired[0].1.hasPrefix("tmr."))
        // The fake server checked every poll's signature; the token is bound to this key.
        XCTAssertEqual(FakeTmuxedo.state.with { $0.tokens[paired[0].1] }, try keys.load()?.publicJWK)
    }

    /// Path 2: a passkey alone is both factors; the in-app session is signed out once the token is collected.
    func testPasskeyAlone() async throws {
        let f = flow()
        await f.begin()
        await f.usePasskey()
        await waitFor(f) { if case .paired = $0 { return true }; return false }
        guard case let .paired(m) = f.phase else { return XCTFail("\(f.phase)") }
        XCTAssertEqual(m.method, "passkey")
        let verify = FakeTmuxedo.state.with { $0.requests.first { $0.path == "/api/mobile/v1/auth/passkey/verify" } }!
        XCTAssertNil(verify.auth, "anonymous: the passkey is both factors, nothing to step up")
        XCTAssertTrue(FakeTmuxedo.state.with { $0.sessions.isEmpty })
        XCTAssertTrue(FakeTmuxedo.state.with { $0.requests.contains { $0.path == "/api/mobile/v1/auth/signout" } })
    }

    /// Apple does not offer Sign in with Apple to Developer ID apps, so the app must not ask for it:
    /// a Developer ID profile can never carry the entitlement and the release would not sign.
    func testEntitlementsHaveNoSignInWithApple() throws {
        let url = URL(fileURLWithPath: #filePath).deletingLastPathComponent().deletingLastPathComponent()
            .appendingPathComponent("Tmuxedo/App/Tmuxedo.entitlements")
        let plist = try PropertyListSerialization.propertyList(from: Data(contentsOf: url), format: nil) as? [String: Any]
        let ent = try XCTUnwrap(plist)
        XCTAssertNil(ent["com.apple.developer.applesignin"])
        // What the app does need stays (push, the site's passkeys).
        XCTAssertEqual(ent["com.apple.developer.aps-environment"] as? String, "production")
        XCTAssertEqual(ent["com.apple.developer.associated-domains"] as? [String], ["webcredentials:tmuxedo.dev"])
    }

    /// The pairing never calls Sign in with Apple or an authenticator-code route.
    func testNoAppleOrTOTPCalls() async throws {
        let f = flow()
        await f.begin()
        await f.usePasskey()
        await waitFor(f) { if case .paired = $0 { return true }; return false }
        XCTAssertFalse(FakeTmuxedo.state.with { $0.requests.contains { $0.path == "/api/mobile/v1/auth/apple" || $0.path == "/api/mfa/verify" } })
        let start = FakeTmuxedo.state.with { $0.requests.first { $0.path == "/api/pair/start" } }!
        XCTAssertNil(start.body["appleNonce"])
    }

    func testAWrongPasskeyFails() async throws {
        passkeyID = "someone-elses"
        let f = flow()
        await f.begin()
        await f.usePasskey()
        guard case .failed = f.phase else { return XCTFail("\(f.phase)") }
        XCTAssertTrue(paired.isEmpty)
        XCTAssertFalse(FakeTmuxedo.state.with { $0.requests.contains { $0.path == "/api/pair/self" } })
    }

    /// Queue 3k: cancelling the passkey sheet is not a failure: the waiting screen (QR, code, passkey)
    /// comes back with no error and nothing is sent to the server. A real failure still says so.
    func testCancelledPasskeyGoesBackQuietly() async throws {
        passkeyCancels = true
        let f = flow()
        await f.begin()
        guard case .waiting = f.phase else { return XCTFail("\(f.phase)") }
        await f.usePasskey()
        guard case .waiting = f.phase else { return XCTFail("cancel: \(f.phase)") }
        XCTAssertNil(f.lastError, "a cancel is not an error")
        XCTAssertFalse(FakeTmuxedo.state.with { $0.requests.contains { $0.path == "/api/mobile/v1/auth/passkey/verify" } })
        // The attempt is still alive: the iPhone can still approve it.
        FakeTmuxedo.approve(FakeTmuxedo.state.with { $0.attempts.last!.id }, method: "phone")
        await waitFor(f) { if case .paired = $0 { return true }; return false }
        guard case let .paired(m) = f.phase else { return XCTFail("\(f.phase)") }
        XCTAssertEqual(m.method, "phone")
    }

    func testDeniedAndExpired() async throws {
        let f = flow()
        await f.begin()
        FakeTmuxedo.deny(FakeTmuxedo.state.with { $0.attempts.last!.id })
        await waitFor(f) { if case .failed = $0 { return true }; return false }
        guard case let .failed(why) = f.phase else { return XCTFail("\(f.phase)") }
        XCTAssertTrue(why.contains("denied"))
        XCTAssertTrue(paired.isEmpty)

        let g = flow()
        await g.begin()
        FakeTmuxedo.state.with { $0.attempts[$0.attempts.count - 1].expired = true }
        await waitFor(g) { if case .failed = $0 { return true }; return false }
        guard case let .failed(why2) = g.phase else { return XCTFail("\(g.phase)") }
        XCTAssertTrue(why2.contains("expired"))
    }

    /// A poll the server cannot verify (a clock far off) ends the attempt with a reason, never a token.
    func testRefusedProofEndsTheAttempt() async throws {
        FakeTmuxedo.state.with { $0.clockSkew = 3600 }
        let f = flow()
        await f.begin()
        await waitFor(f) { if case .failed = $0 { return true }; return false }
        guard case let .failed(why) = f.phase else { return XCTFail("\(f.phase)") }
        XCTAssertTrue(why.contains("key"))
    }

    func testOfflineStartSaysSo() async throws {
        FakeTmuxedo.state.with { $0.unreachable = true }
        let f = flow()
        await f.begin()
        guard case let .failed(why) = f.phase else { return XCTFail("\(f.phase)") }
        XCTAssertTrue(why.contains("could not be reached"), why)
    }

    func testPollResultMapping() {
        XCTAssertEqual(PairingAPI.pollResult(status: 428, json: ["error": "authorization_pending", "opened": true]), .pending(opened: true))
        XCTAssertEqual(PairingAPI.pollResult(status: 429, json: [:]), .slowDown)
        XCTAssertEqual(PairingAPI.pollResult(status: 403, json: [:]), .denied)
        XCTAssertEqual(PairingAPI.pollResult(status: 401, json: ["error": "proof_invalid"]), .proofRefused)
        XCTAssertEqual(PairingAPI.pollResult(status: 410, json: [:]), .expired)
        XCTAssertEqual(PairingAPI.pollResult(status: 200, json: ["runnerToken": "nope", "machineId": "m", "server": "s"]), .expired)
        XCTAssertEqual(PairingAPI.pollResult(status: 200, json: ["runnerToken": "tmr.a.b", "machineId": "m", "server": "s"]),
                       .delivered(runnerToken: "tmr.a.b", machineId: "m", server: "s", method: "approved"))
    }

    /// Push registration and Unpair are signed by the key; the token alone does nothing.
    func testMachineCallsNeedTheKey() async throws {
        let f = flow()
        await f.begin()
        FakeTmuxedo.approve(FakeTmuxedo.state.with { $0.attempts.last!.id })
        await waitFor(f) { if case .paired = $0 { return true }; return false }
        let token = paired[0].1
        let key = try keys.load()!
        try await api.registerPush(token: "ABCD" + String(repeating: "0", count: 60), sandbox: false, runnerToken: token, key: key)
        XCTAssertEqual(FakeTmuxedo.state.with { $0.pushDevices.count }, 1)
        // Another key (a copied token on another Mac) is refused.
        do {
            try await api.registerPush(token: String(repeating: "1", count: 64), sandbox: false, runnerToken: token, key: SoftwareMachineKey())
            XCTFail("a foreign key registered a push device")
        } catch let e as PairingAPI.APIError { XCTAssertEqual(e.code, "proof_invalid") }
        do {
            try await api.unpair(runnerToken: token, key: SoftwareMachineKey())
            XCTFail("a foreign key unpaired the machine")
        } catch let e as PairingAPI.APIError { XCTAssertEqual(e.code, "proof_invalid") }
        try await api.unpair(runnerToken: token, key: key)
        XCTAssertTrue(FakeTmuxedo.state.with { $0.revoked.contains(token) })
        XCTAssertEqual(FakeTmuxedo.state.with { $0.pushDevices.count }, 0)
        do {
            try await api.unpair(runnerToken: token, key: key)
            XCTFail("a revoked token still works")
        } catch let e as PairingAPI.APIError { XCTAssertEqual(e.status, 401) }
    }

    func testErrorsCarryTheServersMessage() {
        XCTAssertThrowsError(try PairingAPI.check(429, ["error": ["code": "rate_limited", "message": "Too many."]])) { e in
            XCTAssertEqual((e as? PairingAPI.APIError)?.message, "Too many.")
            XCTAssertEqual(PairingFlow.describe(e), "Too many.")
        }
        XCTAssertNoThrow(try PairingAPI.check(201, [:]))
        XCTAssertTrue(PairingFlow.describe(URLError(.notConnectedToInternet)).contains("offline"))
    }
}
