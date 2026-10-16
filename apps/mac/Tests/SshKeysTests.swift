import CryptoKit
import XCTest

/// SSH keys for the runner's ssh-agent (secrets-and-env.md §6a): created
/// once per id, the public line in OpenSSH form, signatures that verify, no
/// way to read the private half, refusals for bad ids and unknown keys.
/// (CI Macs have no Secure Enclave: software keys here; the enclave path is
/// the same code with `useEnclave: true`, checked on a real Mac.)
final class SshKeysTests: XCTestCase {
    func testCreateSignAndVerify() throws {
        let keys = SshKeys(store: MemoryStore(), useEnclave: false)
        let (line, hw) = try keys.create("k1")
        XCTAssertFalse(hw)
        XCTAssertTrue(line.hasPrefix("ecdsa-sha2-nistp256 AAAAE2VjZHNhLXNoYTItbmlzdHAyNTY"))
        XCTAssertEqual(try keys.create("k1").line, line, "the same key for the same id")
        // Parse the blob back to the public point and verify a signature with CryptoKit.
        let blob = Data(base64Encoded: String(line.split(separator: " ")[1]))!
        var r = Reader(blob)
        XCTAssertEqual(String(data: r.string()!, encoding: .utf8), "ecdsa-sha2-nistp256")
        XCTAssertEqual(String(data: r.string()!, encoding: .utf8), "nistp256")
        let pub = try P256.Signing.PublicKey(x963Representation: r.string()!)
        let data = Data("session-id-and-userauth-request".utf8)
        var s = Reader(try keys.sign("k1", data: data))
        XCTAssertEqual(String(data: s.string()!, encoding: .utf8), "ecdsa-sha2-nistp256")
        var rs = Reader(s.string()!)
        let rr = rs.string()!.drop(while: { $0 == 0 }), ss = rs.string()!.drop(while: { $0 == 0 })
        let raw = Data(repeating: 0, count: 32 - rr.count) + rr + Data(repeating: 0, count: 32 - ss.count) + ss
        XCTAssertTrue(pub.isValidSignature(try P256.Signing.ECDSASignature(rawRepresentation: raw), for: data))
        XCTAssertThrowsError(try keys.sign("nope", data: data))
        XCTAssertThrowsError(try keys.create("bad id!"))
    }

    func testSignerChannel() {
        SignerPolicy.sshKeys = SshKeys(store: MemoryStore(), useEnclave: false)
        let key = SoftwareMachineKey()
        let created = SignerPolicy.answer(#"{"op":"ssh_create","keyId":"k9"}"#, key: key, ownLookup: "L")
        XCTAssertTrue(created.contains(#""publicKey":"ecdsa-sha2-nistp256 "#), created)
        XCTAssertTrue(created.contains(#""hardware":"software""#))
        let signed = SignerPolicy.answer(#"{"op":"ssh_sign","keyId":"k9","data":"aGVsbG8="}"#, key: key, ownLookup: "L")
        XCTAssertTrue(signed.hasPrefix(#"{"sig":""#), signed)
        XCTAssertTrue(SignerPolicy.answer(#"{"op":"ssh_sign","keyId":"zz","data":"aGVsbG8="}"#, key: key, ownLookup: "L").contains("error"))
        XCTAssertTrue(SignerPolicy.answer(#"{"op":"ssh_sign","keyId":"k9"}"#, key: key, ownLookup: "L").contains("bad request"))
    }

    private struct Reader {
        var d: Data
        init(_ d: Data) { self.d = d }
        mutating func string() -> Data? {
            guard d.count >= 4 else { return nil }
            let n = d.prefix(4).reduce(0) { $0 << 8 | Int($1) }
            d = d.dropFirst(4)
            guard d.count >= n else { return nil }
            let s = d.prefix(n)
            d = d.dropFirst(n)
            return Data(s)
        }
    }
}
