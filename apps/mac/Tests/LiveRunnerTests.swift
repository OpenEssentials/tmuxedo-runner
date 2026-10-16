import CryptoKit
import Darwin
import XCTest

/// The REAL tmuxedo-runner (TMUXEDO_TEST_RUNNER: CI builds it from this commit), supervised
/// exactly as the app supervises it, against Tools/mock-host.py playing tmuxedo:
///
///   pair (the mock approves "from the phone") → the key-bound token → the runner started
///   `--managed` with the signer on fd 3 → the gateway's challenge answered with a signature
///   that VERIFIES against this Mac's key (checked here, with CryptoKit) → connected; then a
///   revocation (4003) stops it for good, with no retry.
///
/// Skipped when no runner binary is given.
@MainActor
final class LiveRunnerTests: XCTestCase {
    var mock: Process?
    var port = 0
    var runtime: URL!

    override func setUp() async throws {
        let env = ProcessInfo.processInfo.environment
        guard let bin = env["TMUXEDO_TEST_RUNNER"], FileManager.default.isExecutableFile(atPath: bin) else {
            throw XCTSkip("set TMUXEDO_TEST_RUNNER to a tmuxedo-runner binary")
        }
        port = 20000 + Int.random(in: 0..<20000)
        let p = Process()
        p.executableURL = URL(fileURLWithPath: "/usr/bin/python3")
        p.arguments = [Self.toolsDir.appendingPathComponent("mock-host.py").path, String(port)]
        p.standardOutput = Pipe()
        p.standardError = Pipe()
        try p.run()
        mock = p
        for _ in 0..<100 {
            if (try? await get("/_admin/state")) != nil { break }
            try await Task.sleep(nanoseconds: 50_000_000)
        }
        runtime = URL(fileURLWithPath: "/tmp/tmx-live-\(UUID().uuidString.prefix(6))")
        try FileManager.default.createDirectory(at: runtime, withIntermediateDirectories: true)
    }

    override func tearDown() async throws {
        mock?.terminate()
        if let runtime, let bin = ProcessInfo.processInfo.environment["TMUXEDO_TEST_RUNNER"] {
            // The runner started its own holder in the test's runtime directory: stop it.
            if let info = RunnerBinary(url: URL(fileURLWithPath: bin)).holderInfo(socket: runtime.appendingPathComponent("run/holder.sock")),
               let pid = info.pid { kill(pid_t(pid), SIGTERM) }
            try? FileManager.default.removeItem(at: runtime)
        }
    }

    static var toolsDir: URL {
        URL(fileURLWithPath: #filePath).deletingLastPathComponent().deletingLastPathComponent().appendingPathComponent("Tools")
    }

    var base: URL { URL(string: "http://127.0.0.1:\(port)")! }

    func get(_ path: String) async throws -> [String: Any] {
        let (d, _) = try await URLSession.shared.data(from: base.appendingPathComponent(path))
        return try JSONSerialization.jsonObject(with: d) as? [String: Any] ?? [:]
    }

    func post(_ path: String, _ body: [String: Any]) async throws {
        var r = URLRequest(url: base.appendingPathComponent(path))
        r.httpMethod = "POST"
        r.httpBody = try JSONSerialization.data(withJSONObject: body)
        _ = try await URLSession.shared.data(for: r)
    }

    func testPairConnectWithAKeyProofThenRevocationStopsForGood() async throws {
        let bin = RunnerBinary(url: URL(fileURLWithPath: ProcessInfo.processInfo.environment["TMUXEDO_TEST_RUNNER"]!))
        let v = try XCTUnwrap(bin.version(), "the runner does not answer version --json")
        XCTAssertEqual(v.missingFeatures, [], "the runner lacks features the app needs")

        // 1. Pair: the Mac's half for real, approved "from the phone" on the mock.
        let mem = MemoryStore()
        let keys = MachineKeyStore(store: mem, useEnclave: false)
        var delivered: (PairedMachine, String)?
        let flow = PairingFlow(api: PairingAPI(server: base), keys: keys, facts: .current, auth: nil,
                               sleep: { try? await Task.sleep(nanoseconds: UInt64($0 * 1e8)) }) { delivered = ($0, $1) }
        await flow.begin()
        let pid = try XCTUnwrap(flow.started?.pairingId)
        try await post("/_admin/approve", ["pairingId": pid, "method": "phone"])
        for _ in 0..<100 where delivered == nil { try await Task.sleep(nanoseconds: 50_000_000) }
        let (machine, token) = try XCTUnwrap(delivered)
        XCTAssertTrue(machine.keyBound)
        let key = try XCTUnwrap(try keys.load())

        // The polls were signed by this key (the mock records, we verify).
        let attempts = try await get("/_admin/state")["attempts"] as? [String: [String: Any]]
        let polls = attempts?[pid]?["polls"] as? [[String: Any]] ?? []
        XCTAssertFalse(polls.isEmpty)
        for p in polls {
            let ts = (p["ts"] as! NSNumber).int64Value
            XCTAssertTrue(FakeTmuxedo.verify(key.publicJWK, SigningInput.poll(pairingId: pid, ts: ts), p["signature"]))
        }

        // 2. The runner, supervised as the app does it.
        let lookup = try XCTUnwrap(runnerTokenLookup(token))
        let signer = try SignerChannel(key: key, lookup: lookup)
        let spec = LaunchSpec.runner(binary: bin.url, server: base.absoluteString, token: token, signerFD: signer.childFD,
                                     workDir: runtime.appendingPathComponent("work").path, runtimeDir: runtime, startHolder: true)
        let sup = Supervisor(makeSpec: { .success((spec, signer)) })
        sup.start()
        for _ in 0..<200 where !sup.display.isConnected { try await Task.sleep(nanoseconds: 50_000_000) }
        XCTAssertTrue(sup.display.isConnected, "never connected:\n\(sup.logText)")
        XCTAssertEqual(sup.signedProofs, 1)

        // The proof the mock gateway received verifies against THIS Mac's key, for its nonce and lookup.
        let st = try await get("/_admin/state")
        let proofs = st["proofs"] as? [[String: Any]] ?? []
        XCTAssertEqual(proofs.count, 1)
        let proof = try XCTUnwrap(proofs.first)
        XCTAssertEqual(proof["lookup"] as? String, lookup)
        XCTAssertTrue(FakeTmuxedo.verify(key.publicJWK, SigningInput.runnerProof(nonce: proof["nonce"] as! String, lookup: lookup), proof["sig"]))
        XCTAssertFalse(FakeTmuxedo.verify(SoftwareMachineKey().publicJWK, SigningInput.runnerProof(nonce: proof["nonce"] as! String, lookup: lookup), proof["sig"]))
        XCTAssertEqual((st["hellos"] as? [[String: Any]])?.count, 1)

        // 3. Revoked on the server: the runner stops for good (78) and the supervisor never retries.
        try await post("/_admin/close", ["code": 4003, "reason": "revoked"])
        for _ in 0..<200 { if case .refused = sup.display { break }; try await Task.sleep(nanoseconds: 50_000_000) }
        guard case let .refused(why) = sup.display else { return XCTFail("\(sup.display)\n\(sup.logText)") }
        XCTAssertTrue(why.contains("revoked"))
        try await Task.sleep(nanoseconds: 3_000_000_000)
        let after = try await get("/_admin/state")
        XCTAssertEqual((after["hellos"] as? [[String: Any]])?.count, 1, "the runner reconnected after a revocation")
    }

    /// A copied token without the key: the runner (no signer) cannot answer the challenge, and the
    /// gateway refuses it (4003 proof_failed); it never connects.
    func testACopiedTokenWithoutTheKeyNeverConnects() async throws {
        let bin = URL(fileURLWithPath: ProcessInfo.processInfo.environment["TMUXEDO_TEST_RUNNER"]!)
        let key = SoftwareMachineKey()
        let token = "tmr.copied\(UUID().uuidString.prefix(6).lowercased()).secret"
        try await post("/_admin/token", ["token": token, "publicKey": key.publicJWK])
        let spec = LaunchSpec.runner(binary: bin, server: base.absoluteString, token: token, signerFD: nil, workDir: nil,
                                     runtimeDir: runtime, startHolder: true)
        let sup = Supervisor(makeSpec: { .success((spec, nil)) })
        sup.start()
        for _ in 0..<300 { if case .refused = sup.display { break }; try await Task.sleep(nanoseconds: 50_000_000) }
        guard case .refused = sup.display else { return XCTFail("\(sup.display)\n\(sup.logText)") }
        XCTAssertTrue(sup.logText.contains("no signer"), sup.logText)
        let st = try await get("/_admin/state")
        XCTAssertEqual((st["proofs"] as? [Any])?.count, 0)
    }
}
