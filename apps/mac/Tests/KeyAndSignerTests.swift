import CryptoKit
import Darwin
import XCTest

/// The machine key, the signed messages, and the signer the runner talks to
/// (docs/design/mac-pairing.md §3). CI runners have no Secure Enclave, so these
/// use the software key; the enclave path is checked when one is available.
final class MachineKeyTests: XCTestCase {
    func testJWKAndFingerprintMatchTheServersDefinition() throws {
        let k = SoftwareMachineKey()
        let jwk = k.publicJWK
        XCTAssertEqual(jwk["kty"], "EC")
        XCTAssertEqual(jwk["crv"], "P-256")
        XCTAssertEqual(jwk["x"]?.count, 43)
        XCTAssertEqual(jwk["y"]?.count, 43)
        // RFC 7638: members in lexical order, no spaces; base64url SHA-256.
        let canonical = "{\"crv\":\"P-256\",\"kty\":\"EC\",\"x\":\"\(jwk["x"]!)\",\"y\":\"\(jwk["y"]!)\"}"
        XCTAssertEqual(k.fingerprint, Data(SHA256.hash(data: Data(canonical.utf8))).base64URL)
        XCTAssertEqual(k.shortFingerprint.count, 19)
        XCTAssertEqual(k.shortFingerprint.split(separator: " ").count, 4)
        XCTAssertNotEqual(SoftwareMachineKey().fingerprint, k.fingerprint)
    }

    func testSignaturesAreDERAndVerifyAgainstTheJWK() throws {
        let k = SoftwareMachineKey()
        let msg = SigningInput.runnerProof(nonce: "n0nce-n0nce-n0nce", lookup: "lk")
        let sig = try k.sign(Data(msg.utf8))
        XCTAssertEqual(sig.first, 0x30, "DER sequence")
        XCTAssertTrue(FakeTmuxedo.verify(k.publicJWK, msg, sig.base64URL))
        XCTAssertFalse(FakeTmuxedo.verify(k.publicJWK, msg + "x", sig.base64URL))
        XCTAssertFalse(FakeTmuxedo.verify(SoftwareMachineKey().publicJWK, msg, sig.base64URL))
    }

    func testSigningInputsAreDomainSeparated() {
        XCTAssertEqual(SigningInput.start(fingerprint: "fp", deviceName: "Mac"), "tmuxedo-pair-start-v1\nfp\nMac")
        XCTAssertEqual(SigningInput.poll(pairingId: "p", ts: 5), "tmuxedo-pair-poll-v1\np\n5")
        XCTAssertEqual(SigningInput.push(lookup: "l", tokenSHA256Hex: "ab", ts: 5), "tmuxedo-push-v1\nl\nab\n5")
        XCTAssertEqual(SigningInput.unpair(lookup: "l", ts: 5), "tmuxedo-unpair-v1\nl\n5")
        XCTAssertEqual(SigningInput.runnerProof(nonce: "n", lookup: "l"), "tmuxedo-runner-proof-v1\nn\nl")
    }

    func testStoreCreatesLoadsReplacesAndDeletes() throws {
        let mem = MemoryStore()
        let store = MachineKeyStore(store: mem, useEnclave: false)
        XCTAssertNil(try store.load())
        XCTAssertFalse(store.exists)
        let a = try store.loadOrCreate()
        XCTAssertFalse(a.isHardwareBacked)
        XCTAssertEqual(try store.load()?.fingerprint, a.fingerprint)
        XCTAssertEqual(try store.loadOrCreate().fingerprint, a.fingerprint, "loadOrCreate keeps the key")
        let b = try store.create()
        XCTAssertNotEqual(b.fingerprint, a.fingerprint, "a new pairing gets a new key")
        XCTAssertEqual(try store.load()?.fingerprint, b.fingerprint)
        store.delete()
        XCTAssertNil(try store.load())
    }

    func testJunkAndForeignBlobsAreRefusedNotTrusted() {
        let mem = MemoryStore([SecretItem.machineKey: Data("Xjunk".utf8)])
        XCTAssertThrowsError(try MachineKeyStore(store: mem, useEnclave: false).load())
        // An enclave handle that this Mac's Secure Enclave did not make (or no enclave at all).
        mem.write(SecretItem.machineKey, Data("S".utf8) + Data(repeating: 7, count: 64))
        XCTAssertThrowsError(try MachineKeyStore(store: mem, useEnclave: false).load())
    }

    func testSecureEnclaveKeyWhenAvailable() throws {
        try XCTSkipUnless(SecureEnclave.isAvailable, "no Secure Enclave on this machine (CI runners are VMs)")
        let mem = MemoryStore()
        let store = MachineKeyStore(store: mem, useEnclave: true)
        let k = try store.create()
        XCTAssertTrue(k.isHardwareBacked)
        let msg = "tmuxedo-runner-proof-v1\nnnnnnnnnnnnnnnnn\nl"
        XCTAssertTrue(FakeTmuxedo.verify(k.publicJWK, msg, try k.sign(Data(msg.utf8)).base64URL))
        XCTAssertEqual(try store.load()?.fingerprint, k.fingerprint)
        XCTAssertTrue(mem.read(SecretItem.machineKey)!.first == UInt8(ascii: "S"))
    }

    func testTokenLookup() {
        XCTAssertEqual(runnerTokenLookup("tmr.abc.def"), "abc")
        XCTAssertEqual(runnerTokenLookup("amr.abc.def"), "abc")
        XCTAssertNil(runnerTokenLookup("tmr..def"))
        XCTAssertNil(runnerTokenLookup("tmr.a.b.c"))
        XCTAssertNil(runnerTokenLookup("xyz.a.b"))
        XCTAssertNil(runnerTokenLookup(""))
    }

    func testBase64URLRoundTrip() {
        for n in 0..<40 {
            let d = Data((0..<n).map { UInt8(truncatingIfNeeded: $0 * 37 + 5) })
            XCTAssertEqual(Data(base64URL: d.base64URL), d)
            XCTAssertFalse(d.base64URL.contains("="))
        }
        XCTAssertEqual(Data([0xab, 0x01]).hex, "ab01")
        XCTAssertEqual(Data("abc".utf8).sha256Hex, "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
    }
}

/// The signer is not a signing oracle: only runner proofs naming this Mac's own token.
final class SignerTests: XCTestCase {
    let key = SoftwareMachineKey()
    let nonce = "Zm9vYmFyZm9vYmFyZm9vYmFyZm9vYmFyZm9vYmFyZm8"

    func testPolicyAllowsOnlyThisMachinesRunnerProof() {
        let ok = SigningInput.runnerProof(nonce: nonce, lookup: "MINE")
        XCTAssertTrue(SignerPolicy.allows(Data(ok.utf8), ownLookup: "MINE"))
        for bad in [
            SigningInput.runnerProof(nonce: nonce, lookup: "OTHER"),          // another token
            SigningInput.start(fingerprint: "fp", deviceName: "Mac"),         // a pairing start
            SigningInput.poll(pairingId: "p", ts: 1),                         // a poll
            SigningInput.unpair(lookup: "MINE", ts: 1),                       // an unpair
            "tmuxedo-runner-proof-v1\n\(nonce)\nMINE\nextra",                 // more lines
            "tmuxedo-runner-proof-v1\nshort\nMINE",                           // nonce too short
            "tmuxedo-runner-proof-v1\n\(nonce) spaced\nMINE",                 // nonce not base64url
            "tmuxedo-runner-proof-v2\n\(nonce)\nMINE",                        // another domain
            "",
        ] {
            XCTAssertFalse(SignerPolicy.allows(Data(bad.utf8), ownLookup: "MINE"), bad)
        }
        XCTAssertFalse(SignerPolicy.allows(Data([0xff, 0xfe]), ownLookup: "MINE"))
    }

    func testAnswersSignOrRefuse() throws {
        let input = Data(SigningInput.runnerProof(nonce: nonce, lookup: "MINE").utf8)
        let req = "{\"op\":\"sign\",\"input\":\"\(input.base64EncodedString())\"}"
        let ans = SignerPolicy.answer(req, key: key, ownLookup: "MINE")
        let obj = try JSONSerialization.jsonObject(with: Data(ans.utf8)) as! [String: String]
        XCTAssertTrue(FakeTmuxedo.verify(key.publicJWK, String(data: input, encoding: .utf8)!, obj["sig"]))
        XCTAssertTrue(SignerPolicy.answer(req, key: key, ownLookup: "OTHER").contains("error"))
        XCTAssertTrue(SignerPolicy.answer("{\"op\":\"export\"}", key: key, ownLookup: "MINE").contains("bad request"))
        XCTAssertTrue(SignerPolicy.answer("not json", key: key, ownLookup: "MINE").contains("bad request"))
    }

    /// The real channel over a real socketpair, as the runner uses it (one JSON line each way).
    func testChannelServesOverTheSocketpair() throws {
        var signed: [Bool] = []
        let lock = NSLock()
        let ch = try SignerChannel(key: key, lookup: "MINE") { ok in lock.lock(); signed.append(ok); lock.unlock() }
        ch.start()
        defer { ch.stop() }
        let fd = ch.childFD
        // Both ends are close-on-exec until spawn dups the child's to fd 3.
        XCTAssertEqual(fcntl(fd, F_GETFD) & FD_CLOEXEC, FD_CLOEXEC)
        func roundTrip(_ line: String) -> String {
            let out = Array((line + "\n").utf8)
            _ = out.withUnsafeBytes { write(fd, $0.baseAddress, $0.count) }
            var buf = [UInt8](repeating: 0, count: 4096)
            var got = Data()
            while !got.contains(0x0A) {
                let n = read(fd, &buf, buf.count)
                if n <= 0 { break }
                got.append(contentsOf: buf[0..<n])
            }
            return String(decoding: got, as: UTF8.self).trimmingCharacters(in: .newlines)
        }
        let input = SigningInput.runnerProof(nonce: nonce, lookup: "MINE")
        let ok = roundTrip("{\"op\":\"sign\",\"input\":\"\(Data(input.utf8).base64EncodedString())\"}")
        let sig = (try JSONSerialization.jsonObject(with: Data(ok.utf8)) as? [String: String])?["sig"]
        XCTAssertTrue(FakeTmuxedo.verify(key.publicJWK, input, sig))
        let refused = roundTrip("{\"op\":\"sign\",\"input\":\"\(Data("anything".utf8).base64EncodedString())\"}")
        XCTAssertTrue(refused.contains("error"))
        lock.lock(); XCTAssertEqual(signed, [true, false]); lock.unlock()
    }
}

final class DisplayTests: XCTestCase {
    func testEveryStateHasItsOwnSentenceAndOnlyConnectedSaysReachable() {
        let t = Date(timeIntervalSince1970: 0)
        let all: [Display] = [.notPaired, .off, .connecting(attempt: 1), .connecting(attempt: 3), .connected(since: t),
                              .unreachable(since: t, error: "eof", retryIn: 4), .refused("revoked."), .broken("no binary"), .stopping]
        XCTAssertEqual(Set(all.map(\.sentence)).count, all.count)
        for d in all where d != .connected(since: t) {
            XCTAssertFalse(d.sentence.contains("are reachable"), d.sentence)
        }
        XCTAssertTrue(Display.unreachable(since: t, error: "eof", retryIn: 4).sentence.contains("not the same as connected"))
    }

    func testFaultsRunningAndSymbols() {
        let t = Date()
        XCTAssertTrue(Display.unreachable(since: t, error: "x", retryIn: 1).isFault)
        XCTAssertTrue(Display.refused("x").isFault)
        XCTAssertTrue(Display.broken("x").isFault)
        XCTAssertFalse(Display.connected(since: t).isFault)
        XCTAssertFalse(Display.off.isFault)
        XCTAssertTrue(Display.connecting(attempt: 1).isRunning)
        XCTAssertTrue(Display.connected(since: t).isRunning)
        XCTAssertTrue(Display.unreachable(since: t, error: "x", retryIn: 1).isRunning)
        XCTAssertFalse(Display.refused("x").isRunning)
        XCTAssertFalse(Display.off.isRunning)
        XCTAssertTrue(Display.connecting(attempt: 1).animating)
        XCTAssertFalse(Display.connected(since: t).animating)
        XCTAssertTrue(Display.connected(since: t).isConnected)
        XCTAssertFalse(Display.off.isConnected)
        for d in [Display.notPaired, .off, .connecting(attempt: 1), .connected(since: t), .refused("x"), .stopping] {
            XCTAssertFalse(d.symbol.isEmpty)
        }
    }
}

final class SecretStoreTests: XCTestCase {
    func testMemoryStore() {
        let m = MemoryStore()
        XCTAssertFalse(m.has("a"))
        m.writeString("a", "  value \n")
        XCTAssertEqual(m.readString("a"), "value", "trimmed")
        XCTAssertTrue(m.has("a"))
        m.delete("a")
        XCTAssertNil(m.read("a"))
        m.write("b", Data())
        XCTAssertFalse(m.has("b"))
    }

    /// The real Keychain, under a throwaway account: never synchronised. (macOS's file-based login keychain
    /// ignores kSecAttrAccessible, so "this device only" is the Secure Enclave key's property, not the item's.)
    func testKeychainItemIsThisDeviceOnly() {
        let account = "test-\(UUID().uuidString.prefix(8))"
        defer { Keychain.delete(account: account) }
        XCTAssertTrue(Keychain.write(Data("secret".utf8), account: account))
        XCTAssertTrue(Keychain.has(account: account))
        XCTAssertEqual(Keychain.read(account: account), Data("secret".utf8))
        let q: [String: Any] = [kSecClass as String: kSecClassGenericPassword, kSecAttrService as String: Keychain.service,
                                kSecAttrAccount as String: account, kSecReturnAttributes as String: true]
        var item: CFTypeRef?
        XCTAssertEqual(SecItemCopyMatching(q as CFDictionary, &item), errSecSuccess)
        let attrs = item as? [String: Any] ?? [:]
        XCTAssertNotEqual(attrs[kSecAttrSynchronizable as String] as? Bool, true)
        Keychain.delete(account: account)
        XCTAssertFalse(Keychain.has(account: account))
        XCTAssertNil(Keychain.read(account: account))
    }
}
