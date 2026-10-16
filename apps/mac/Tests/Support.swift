import CryptoKit
import Foundation
import ServiceManagement
import XCTest

// MARK: - Fake tmuxedo (the pairing API), in process

/// An in-process stand-in for tmuxedo's `/api/pair/*` and the app-session calls the
/// pairing sheet makes, reached through `URLProtocol` (no socket).
///
/// It enforces the server's rules (web/src/server/pair/service.ts): the start proof,
/// signed polls with a fresh timestamp, the token delivered exactly once and only to
/// the key that started the attempt, a passkey session (both factors) before /self,
/// no Sign in with Apple for the Mac at all, and the machine proof on push and unpair. A client that
/// passes here obeys the contract, not this file's opinion of it.
final class FakeTmuxedo: URLProtocol {
    static let host = "tmuxedo.test"
    static var base: URL { URL(string: "https://\(host)")! }

    struct Attempt {
        var id: String
        var pollToken: String
        var matchCode: String
        var publicKey: [String: String]?
        var approved = false
        var method = ""
        var delivered = false
        var denied = false
        var opened = false
        var expired = false
    }

    final class State {
        let lock = NSLock()
        var attempts: [Attempt] = []
        var requests: [(path: String, method: String, auth: String?, body: [String: Any])] = []
        var sessions: [String: Bool] = [:]                         // access token → MFA verified
        var tokens: [String: [String: String]] = [:]               // runner token → key
        var revoked: Set<String> = []
        var pushDevices: [String: String] = [:]                    // apns token → runner token
        var forced: (Int, [String: Any])?
        var unreachable = false
        var clockSkew: TimeInterval = 0
        func with<T>(_ f: (State) -> T) -> T { lock.lock(); defer { lock.unlock() }; return f(self) }
    }

    static var state = State()
    static func reset() { state = State() }

    static func session() -> URLSession {
        let c = URLSessionConfiguration.ephemeral
        c.protocolClasses = [FakeTmuxedo.self]
        return URLSession(configuration: c)
    }

    /// Approve the attempt from "the phone" (the server-side approval path).
    static func approve(_ pairingId: String, method: String = "phone") {
        state.with { s in
            if let i = s.attempts.firstIndex(where: { $0.id == pairingId }) { s.attempts[i].approved = true; s.attempts[i].method = method }
        }
    }

    static func deny(_ pairingId: String) {
        state.with { s in if let i = s.attempts.firstIndex(where: { $0.id == pairingId }) { s.attempts[i].denied = true } }
    }

    static func open(_ pairingId: String) {
        state.with { s in if let i = s.attempts.firstIndex(where: { $0.id == pairingId }) { s.attempts[i].opened = true } }
    }

    override class func canInit(with request: URLRequest) -> Bool { request.url?.host == host }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }
    override func stopLoading() {}

    override func startLoading() {
        let s = Self.state
        if s.with({ $0.unreachable }) {
            client?.urlProtocol(self, didFailWithError: URLError(.cannotConnectToHost))
            return
        }
        let body = Self.body(of: request)
        let path = request.url!.path
        let auth = request.value(forHTTPHeaderField: "Authorization")
        s.with { $0.requests.append((path, request.httpMethod ?? "GET", auth, body)) }
        let (status, json) = s.with { $0.forced } ?? Self.route(path: path, method: request.httpMethod ?? "GET", auth: auth, body: body)
        let data = try! JSONSerialization.data(withJSONObject: json)
        let resp = HTTPURLResponse(url: request.url!, statusCode: status, httpVersion: "HTTP/1.1", headerFields: ["Content-Type": "application/json"])!
        client?.urlProtocol(self, didReceive: resp, cacheStoragePolicy: .notAllowed)
        client?.urlProtocol(self, didLoad: data)
        client?.urlProtocolDidFinishLoading(self)
    }

    static func body(of r: URLRequest) -> [String: Any] {
        var data = r.httpBody
        if data == nil, let stream = r.httpBodyStream {
            stream.open()
            var d = Data()
            var buf = [UInt8](repeating: 0, count: 4096)
            while stream.hasBytesAvailable {
                let n = stream.read(&buf, maxLength: buf.count)
                if n <= 0 { break }
                d.append(buf, count: n)
            }
            stream.close()
            data = d
        }
        return data.flatMap { try? JSONSerialization.jsonObject(with: $0) as? [String: Any] } ?? [:]
    }

    static func err(_ status: Int, _ code: String, _ message: String = "refused") -> (Int, [String: Any]) {
        (status, ["error": ["code": code, "message": message]])
    }

    static func verify(_ jwk: [String: String], _ message: String, _ sig: Any?) -> Bool {
        guard let x = jwk["x"].flatMap({ Data(base64URL: $0) }), let y = jwk["y"].flatMap({ Data(base64URL: $0) }),
              let sigS = sig as? String, let sigD = Data(base64URL: sigS),
              let key = try? P256.Signing.PublicKey(x963Representation: Data([4]) + x + y),
              let signature = try? P256.Signing.ECDSASignature(derRepresentation: sigD)
        else { return false }
        return key.isValidSignature(signature, for: Data(message.utf8))
    }

    static func fingerprint(_ jwk: [String: String]) -> String {
        let c = "{\"crv\":\"P-256\",\"kty\":\"EC\",\"x\":\"\(jwk["x"]!)\",\"y\":\"\(jwk["y"]!)\"}"
        return Data(SHA256.hash(data: Data(c.utf8))).base64URL
    }

    static func fresh(_ ts: Any?) -> Bool {
        guard let t = (ts as? NSNumber)?.doubleValue else { return false }
        return abs(Date().addingTimeInterval(state.with { $0.clockSkew }).timeIntervalSince1970 * 1000 - t) <= 60_000
    }

    static func route(path: String, method: String, auth: String?, body: [String: Any]) -> (Int, [String: Any]) {
        let bearer = auth.flatMap { $0.hasPrefix("Bearer ") ? String($0.dropFirst(7)) : nil }
        switch path {
        case "/api/pair/start":
            guard let jwk = body["publicKey"] as? [String: String], let name = body["deviceName"] as? String else { return err(400, "invalid_key") }
            guard verify(jwk, SigningInput.start(fingerprint: fingerprint(jwk), deviceName: name), body["proof"]) else { return err(400, "invalid_proof") }
            let a = Attempt(id: "p\(UUID().uuidString.prefix(8))", pollToken: UUID().uuidString,
                            matchCode: String(format: "%06d", Int.random(in: 0..<1_000_000)), publicKey: jwk)
            state.with { $0.attempts.append(a) }
            return (200, ["pairingId": a.id, "pollToken": a.pollToken, "url": "https://\(host)/pair?code=L\(a.id)",
                          "appUrl": "https://\(host)/m/pair?code=L\(a.id)", "matchCode": a.matchCode,
                          "interval": 2, "expiresIn": 300])
        case "/api/pair/poll":
            guard let id = body["pairingId"] as? String, let tok = body["pollToken"] as? String,
                  let a = state.with({ $0.attempts.first { $0.id == id && $0.pollToken == tok } })
            else { return (410, ["error": "expired_token"]) }
            guard let key = a.publicKey, fresh(body["ts"]),
                  verify(key, SigningInput.poll(pairingId: id, ts: (body["ts"] as! NSNumber).int64Value), body["signature"])
            else { return (401, ["error": "proof_invalid"]) }
            if a.denied { return (403, ["error": "access_denied"]) }
            if a.expired || a.delivered { return (410, ["error": "expired_token"]) }
            if !a.approved { return (428, ["error": "authorization_pending", "opened": a.opened]) }
            let token = "tmr.\(id)lookup.secret\(UUID().uuidString.prefix(8))"
            state.with { s in
                if let i = s.attempts.firstIndex(where: { $0.id == id }) { s.attempts[i].delivered = true }
                s.tokens[token] = key
            }
            return (200, ["kind": "runner", "runnerToken": token, "machineId": "m-\(id)", "server": "https://\(host)", "method": a.method])
        case "/api/mobile/v1/auth/passkey/options":
            return (200, ["nonce": "pk-nonce", "options": ["challenge": Data("chal".utf8).base64URL, "rpId": "tmuxedo.test"]])
        case "/api/mobile/v1/auth/passkey/verify":
            guard (body["response"] as? [String: Any])?["id"] as? String == "good-credential" else { return err(400, "passkey_failed") }
            let access = "tma_\(UUID().uuidString.prefix(8))"
            state.with { $0.sessions[access] = true }
            return (200, ["tokenType": "Bearer", "accessToken": access, "refreshToken": "tmf_x.y", "sessionId": "s2",
                          "mfa": ["verified": true, "hasFactor": true, "next": NSNull()]])
        case "/api/pair/self":
            guard let b = bearer, let sess = state.with({ $0.sessions[b] }) else { return err(401, "unauthenticated") }
            guard sess else { return err(403, "mfa_required", "Finish your second factor first.") }
            guard let id = body["pairingId"] as? String, let tok = body["pollToken"] as? String,
                  state.with({ $0.attempts.contains { $0.id == id && $0.pollToken == tok } })
            else { return err(404, "not_found") }
            approve(id, method: "passkey")
            return (200, ["ok": true, "machine": ["id": "m-\(id)", "name": "Mac", "method": "passkey"]])
        case "/api/mobile/v1/auth/signout":
            if let b = bearer { _ = state.with { $0.sessions.removeValue(forKey: b) } }
            return (200, ["ok": true])
        case "/api/pair/push-device", "/api/pair/machine":
            guard let tok = bearer, let key = state.with({ $0.tokens[tok] }), !state.with({ $0.revoked.contains(tok) }) else {
                return err(401, "unauthenticated")
            }
            let lookup = runnerTokenLookup(tok)!
            let ts = (body["ts"] as? NSNumber)?.int64Value ?? 0
            if path == "/api/pair/push-device" && method == "POST" {
                let apns = body["token"] as? String ?? ""
                guard fresh(body["ts"]), verify(key, SigningInput.push(lookup: lookup, tokenSHA256Hex: Data(apns.utf8).sha256Hex, ts: ts), body["signature"])
                else { return err(401, "proof_invalid") }
                state.with { $0.pushDevices[apns] = tok }
                return (201, ["id": "d1", "platform": "macos", "provider": "apns"])
            }
            guard fresh(body["ts"]), verify(key, SigningInput.unpair(lookup: lookup, ts: ts), body["signature"]) else { return err(401, "proof_invalid") }
            state.with { s in s.pushDevices = s.pushDevices.filter { $0.value != tok } }
            if path == "/api/pair/machine" { state.with { _ = $0.revoked.insert(tok) } }
            return (200, path == "/api/pair/machine" ? ["revoked": true] : ["removed": true])
        default:
            return err(404, "not_found")
        }
    }
}

// MARK: - A fake runner process

final class FakeProcess: RunningProcess {
    let pid: pid_t
    var terminated: [TimeInterval] = []
    private let exit: (Int32) -> Void
    private let line: (String) -> Void
    var exited = false
    init(pid: pid_t, line: @escaping (String) -> Void, exit: @escaping (Int32) -> Void) {
        self.pid = pid
        self.line = line
        self.exit = exit
    }
    func say(_ l: String) { line(l) }
    func finish(_ code: Int32) { guard !exited else { return }; exited = true; exit(code) }
    func terminate(grace: TimeInterval) { terminated.append(grace); finish(0) }
}

final class FakeLauncher: ProcessLauncher {
    var launched: [LaunchSpec] = []
    var processes: [FakeProcess] = []
    var fail: Error?
    private var nextPid: pid_t = 1000
    func launch(_ spec: LaunchSpec, line: @escaping (String) -> Void, exit: @escaping (Int32) -> Void) throws -> RunningProcess {
        if let fail { throw fail }
        launched.append(spec)
        nextPid += 1
        let p = FakeProcess(pid: nextPid, line: line, exit: exit)
        processes.append(p)
        return p
    }
    var last: FakeProcess { processes.last! }
}

final class FakeRegistry: AgentRegistry {
    var status: SMAppService.Status = .notRegistered
    var registered = 0
    var unregistered = 0
    var failRegister: Error?
    var becomes: SMAppService.Status = .enabled
    func register() throws {
        registered += 1
        if let failRegister { throw failRegister }
        status = becomes
    }
    func unregister() throws { unregistered += 1; status = .notRegistered }
}

/// A clock and scheduler the tests drive by hand.
@MainActor
final class ManualScheduler {
    var now = Date(timeIntervalSince1970: 1_800_000_000)
    var pending: [(at: Date, f: @MainActor () -> Void)] = []
    func schedule(_ delay: TimeInterval, _ f: @escaping @MainActor () -> Void) { pending.append((now.addingTimeInterval(delay), f)) }
    /// Advance and run what is due, in order.
    func advance(_ s: TimeInterval) {
        now = now.addingTimeInterval(s)
        let due = pending.filter { $0.at <= now }
        pending.removeAll { $0.at <= now }
        for d in due { d.f() }
    }
    var delays: [TimeInterval] { pending.map { $0.at.timeIntervalSince(now) } }
}

/// Main-queue work queued by the supervisor (lines and exits) runs before this returns.
@MainActor
func drainMain(_ test: XCTestCase) {
    let e = test.expectation(description: "main queue drained")
    DispatchQueue.main.async { e.fulfill() }
    test.wait(for: [e], timeout: 5)
}

/// The same from an async test (a blocking wait there would hold the main actor the queue needs).
@MainActor
func drainMainAsync() async {
    await withCheckedContinuation { (c: CheckedContinuation<Void, Never>) in DispatchQueue.main.async { c.resume() } }
}

func tempDir() -> URL {
    let u = FileManager.default.temporaryDirectory.appendingPathComponent("tmuxedo-tests-\(UUID().uuidString.prefix(8))", isDirectory: true)
    try? FileManager.default.createDirectory(at: u, withIntermediateDirectories: true)
    return u
}
