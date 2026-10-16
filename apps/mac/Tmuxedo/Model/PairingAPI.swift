import Foundation

/// tmuxedo's pairing API (docs/design/mac-pairing.md §2, web/src/app/api/pair/*) and the
/// few app-session calls the in-app sign-in needs (docs/mobile-api.md §2.2, §2.5, §3.2).
///
/// Every call goes to the configured server only. Nothing here is ever sent
/// to an update feed or any other host.
struct PairingAPI {
    let server: URL
    var session: URLSession = .shared

    struct APIError: LocalizedError, Equatable {
        let status: Int
        let code: String
        let message: String
        var errorDescription: String? { message }
    }

    // MARK: Start

    struct Started: Decodable, Equatable {
        let pairingId: String
        let pollToken: String
        let url: String
        let appUrl: String
        let matchCode: String
        let interval: Int
        let expiresIn: Int
    }

    struct MachineFacts: Equatable {
        var deviceName: String
        var model: String?
        var arch: String
        var osVersion: String?
        var appVersion: String?

        static var current: MachineFacts {
            MachineFacts(deviceName: Host.current().localizedName ?? ProcessInfo.processInfo.hostName,
                         model: Self.sysctl("hw.model"),
                         arch: Self.arch,
                         osVersion: ProcessInfo.processInfo.operatingSystemVersionString,
                         appVersion: BuildInfo.current.version)
        }

        static var arch: String {
            #if arch(arm64)
            return "aarch64"
            #else
            return "x86_64"
            #endif
        }

        static func sysctl(_ name: String) -> String? {
            var size = 0
            guard sysctlbyname(name, nil, &size, nil, 0) == 0, size > 0 else { return nil }
            var buf = [CChar](repeating: 0, count: size)
            guard sysctlbyname(name, &buf, &size, nil, 0) == 0 else { return nil }
            return String(cString: buf)
        }
    }

    func start(key: MachineKey, facts: MachineFacts) async throws -> Started {
        let proof = try key.signBase64URL(SigningInput.start(fingerprint: key.fingerprint, deviceName: facts.deviceName))
        var body: [String: Any] = [
            "client": "mac", "publicKey": key.publicJWK, "proof": proof, "deviceName": facts.deviceName,
            "os": "macos", "arch": facts.arch,
        ]
        if let m = facts.model { body["model"] = m }
        if let v = facts.osVersion { body["osVersion"] = v }
        if let v = facts.appVersion { body["appVersion"] = v }
        return try await decode(post("api/pair/start", body))
    }

    // MARK: Poll

    enum PollResult: Equatable {
        case pending(opened: Bool)
        case slowDown
        case denied
        case expired
        case proofRefused
        case delivered(runnerToken: String, machineId: String, server: String, method: String)
    }

    func poll(_ s: Started, key: MachineKey, now: Date = Date()) async throws -> PollResult {
        let ts = Int64(now.timeIntervalSince1970 * 1000)
        let sig = try key.signBase64URL(SigningInput.poll(pairingId: s.pairingId, ts: ts))
        let (status, json) = try await post("api/pair/poll", ["pairingId": s.pairingId, "pollToken": s.pollToken, "ts": ts, "signature": sig])
        return Self.pollResult(status: status, json: json)
    }

    static func pollResult(status: Int, json: [String: Any]) -> PollResult {
        if status == 200, let t = json["runnerToken"] as? String, runnerTokenLookup(t) != nil,
           let m = json["machineId"] as? String, let server = json["server"] as? String {
            return .delivered(runnerToken: t, machineId: m, server: server, method: json["method"] as? String ?? "approved")
        }
        switch (status, json["error"] as? String) {
        case (428, _), (_, "authorization_pending"): return .pending(opened: json["opened"] as? Bool ?? false)
        case (429, _), (_, "slow_down"): return .slowDown
        case (403, _), (_, "access_denied"): return .denied
        case (401, _), (_, "proof_invalid"): return .proofRefused
        default: return .expired
        }
    }

    // MARK: In-app sign-in (an app session that exists only until the pairing is approved)

    struct Grant: Decodable, Equatable {
        let accessToken: String
        let refreshToken: String
        let sessionId: String
        struct MFA: Decodable, Equatable { let verified: Bool; let hasFactor: Bool; let next: String? }
        let mfa: MFA
    }

    /// WebAuthn request options for an anonymous passkey assertion (both factors at once).
    func passkeyOptions() async throws -> (nonce: String, options: [String: Any]) {
        let (status, json) = try await post("api/mobile/v1/auth/passkey/options", [:])
        try Self.check(status, json)
        guard let nonce = json["nonce"] as? String, let options = json["options"] as? [String: Any] else {
            throw APIError(status: status, code: "bad_answer", message: "tmuxedo answered without passkey options.")
        }
        return (nonce, options)
    }

    /// The assertion → a verified grant (a passkey is both factors).
    func passkeyVerify(nonce: String, response: [String: Any], deviceName: String) async throws -> Grant {
        let (status, json) = try await post("api/mobile/v1/auth/passkey/verify",
                                            ["nonce": nonce, "response": response, "deviceName": deviceName])
        try Self.check(status, json)
        return try Self.decodeJSON(json, status: status)
    }

    /// The Mac's own session approves its own attempt.
    func selfApprove(_ s: Started, bearer: String) async throws -> String {
        let (status, json) = try await post("api/pair/self", ["pairingId": s.pairingId, "pollToken": s.pollToken], bearer: bearer)
        try Self.check(status, json)
        return ((json["machine"] as? [String: Any])?["method"] as? String) ?? "in-app"
    }

    /// The in-app session is not kept: the Mac holds only its machine credential.
    func signOut(bearer: String, refreshToken: String?) async {
        var body: [String: Any] = [:]
        if let refreshToken { body["refreshToken"] = refreshToken }
        _ = try? await post("api/mobile/v1/auth/signout", body, bearer: bearer)
    }

    // MARK: Machine-authenticated (runner token + a signature by the key)

    func registerPush(token apns: String, sandbox: Bool, runnerToken: String, key: MachineKey, now: Date = Date()) async throws {
        guard let lookup = runnerTokenLookup(runnerToken) else { throw APIError(status: 0, code: "no_token", message: "This Mac is not paired.") }
        let ts = Int64(now.timeIntervalSince1970 * 1000)
        let tok = apns.lowercased()
        let sig = try key.signBase64URL(SigningInput.push(lookup: lookup, tokenSHA256Hex: Data(tok.utf8).sha256Hex, ts: ts))
        let (status, json) = try await post("api/pair/push-device",
                                            ["token": tok, "apnsEnvironment": sandbox ? "sandbox" : "production",
                                             "appVersion": BuildInfo.current.version, "ts": ts, "signature": sig],
                                            bearer: runnerToken)
        try Self.check(status, json)
    }

    func unregisterPush(runnerToken: String, key: MachineKey, now: Date = Date()) async throws {
        try await machineDelete("api/pair/push-device", runnerToken: runnerToken, key: key, now: now)
    }

    /// Unpair from this Mac: the server revokes the machine, its token and its push devices at once.
    func unpair(runnerToken: String, key: MachineKey, now: Date = Date()) async throws {
        try await machineDelete("api/pair/machine", runnerToken: runnerToken, key: key, now: now)
    }

    private func machineDelete(_ path: String, runnerToken: String, key: MachineKey, now: Date) async throws {
        guard let lookup = runnerTokenLookup(runnerToken) else { throw APIError(status: 0, code: "no_token", message: "This Mac is not paired.") }
        let ts = Int64(now.timeIntervalSince1970 * 1000)
        let sig = try key.signBase64URL(SigningInput.unpair(lookup: lookup, ts: ts))
        let (status, json) = try await send("DELETE", path, ["ts": ts, "signature": sig], bearer: runnerToken)
        try Self.check(status, json)
    }

    // MARK: Plumbing

    func post(_ path: String, _ body: [String: Any], bearer: String? = nil) async throws -> (Int, [String: Any]) {
        try await send("POST", path, body, bearer: bearer)
    }

    func send(_ method: String, _ path: String, _ body: [String: Any], bearer: String? = nil) async throws -> (Int, [String: Any]) {
        guard let url = URL(string: path, relativeTo: server)?.absoluteURL else {
            throw APIError(status: 0, code: "bad_url", message: "The tmuxedo server address is not valid.")
        }
        var req = URLRequest(url: url, timeoutInterval: 30)
        req.httpMethod = method
        req.setValue("application/json", forHTTPHeaderField: "Content-Type")
        req.setValue("tmuxedo-mac/\(BuildInfo.current.version)", forHTTPHeaderField: "User-Agent")
        if let bearer { req.setValue("Bearer \(bearer)", forHTTPHeaderField: "Authorization") }
        req.httpBody = try JSONSerialization.data(withJSONObject: body)
        let (data, resp) = try await session.data(for: req)
        let status = (resp as? HTTPURLResponse)?.statusCode ?? 0
        let json = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any] ?? [:]
        return (status, json)
    }

    static func check(_ status: Int, _ json: [String: Any]) throws {
        guard !(200..<300).contains(status) else { return }
        let err = json["error"] as? [String: Any]
        throw APIError(status: status,
                       code: err?["code"] as? String ?? (json["error"] as? String) ?? "http_\(status)",
                       message: err?["message"] as? String ?? "tmuxedo answered \(status).")
    }

    private func decode<T: Decodable>(_ r: (Int, [String: Any])) throws -> T {
        try Self.check(r.0, r.1)
        return try Self.decodeJSON(r.1, status: r.0)
    }

    static func decodeJSON<T: Decodable>(_ json: [String: Any], status: Int) throws -> T {
        do {
            return try JSONDecoder().decode(T.self, from: JSONSerialization.data(withJSONObject: json))
        } catch {
            throw APIError(status: status, code: "bad_answer", message: "tmuxedo's answer could not be read.")
        }
    }
}
