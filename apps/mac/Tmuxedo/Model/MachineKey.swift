import CryptoKit
import Foundation
import LocalAuthentication
import Security

/// This Mac's identity key (docs/design/mac-pairing.md §3.1).
///
/// An EC P-256 key created INSIDE the Secure Enclave. Its private half never
/// leaves the chip: what the Keychain stores (`machine-key`, ThisDeviceOnly) is
/// the enclave's wrapped handle, usable only by this Mac's Secure Enclave. A
/// copy of it on another Mac, or restored from a backup, signs nothing.
///
/// The server binds the runner token to the public half; every gateway
/// connection proves possession by signing a fresh challenge, so a copied
/// token alone is useless. No biometry flag: the runner must reconnect
/// unattended after sleep and at login.
///
/// A Mac without a Secure Enclave (an Intel Mac without a T2 chip) gets a
/// software key in the Keychain instead, and says so (`isHardwareBacked`).
protocol MachineKey: AnyObject {
    /// Uncompressed public point, 65 bytes (`04 ‖ x ‖ y`).
    var publicX963: Data { get }
    var isHardwareBacked: Bool { get }
    /// ECDSA P-256 over SHA-256 of `message`, DER encoded.
    func sign(_ message: Data) throws -> Data
}

extension MachineKey {
    /// The public JWK the server stores: `{kty, crv, x, y}`.
    var publicJWK: [String: String] {
        let p = publicX963
        return ["kty": "EC", "crv": "P-256", "x": p[1..<33].base64URL, "y": p[33..<65].base64URL]
    }

    /// base64url SHA-256 of the RFC 7638 thumbprint input; equal to the server's `keyFingerprint`.
    var fingerprint: String {
        let j = publicJWK
        let canonical = "{\"crv\":\"P-256\",\"kty\":\"EC\",\"x\":\"\(j["x"]!)\",\"y\":\"\(j["y"]!)\"}"
        return Data(SHA256.hash(data: Data(canonical.utf8))).base64URL
    }

    /// "AbCd EfGh IjKl MnOp", what the approve screen shows.
    var shortFingerprint: String {
        let s = String(fingerprint.prefix(16))
        return stride(from: 0, to: s.count, by: 4).map { i -> String in
            let a = s.index(s.startIndex, offsetBy: i)
            return String(s[a..<s.index(a, offsetBy: 4)])
        }.joined(separator: " ")
    }

    func signBase64URL(_ text: String) throws -> String {
        try sign(Data(text.utf8)).base64URL
    }
}

final class SecureEnclaveMachineKey: MachineKey {
    private let key: SecureEnclave.P256.Signing.PrivateKey
    init(_ key: SecureEnclave.P256.Signing.PrivateKey) { self.key = key }
    var publicX963: Data { key.publicKey.x963Representation }
    var isHardwareBacked: Bool { true }
    /// The wrapped handle the Keychain keeps.
    var handle: Data { key.dataRepresentation }
    func sign(_ message: Data) throws -> Data { try key.signature(for: message).derRepresentation }
}

final class SoftwareMachineKey: MachineKey {
    private let key: P256.Signing.PrivateKey
    init(_ key: P256.Signing.PrivateKey = P256.Signing.PrivateKey()) { self.key = key }
    var publicX963: Data { key.publicKey.x963Representation }
    var isHardwareBacked: Bool { false }
    var raw: Data { key.rawRepresentation }
    func sign(_ message: Data) throws -> Data { try key.signature(for: message).derRepresentation }
}

enum MachineKeyError: LocalizedError {
    case unavailable(String)
    var errorDescription: String? {
        switch self {
        case let .unavailable(why): return "This Mac's key is not available: \(why)"
        }
    }
}

/// Loads, creates and deletes the machine key, in a `SecretStore`.
///
/// The stored value is one byte of kind (`S` = Secure Enclave handle,
/// `K` = software key) followed by the bytes.
final class MachineKeyStore {
    private let store: SecretStore
    /// False forces a software key (tests; CI runners have no Secure Enclave).
    private let useEnclave: Bool

    init(store: SecretStore, useEnclave: Bool = SecureEnclave.isAvailable) {
        self.store = store
        self.useEnclave = useEnclave
    }

    var exists: Bool { store.has(SecretItem.machineKey) }

    func load() throws -> MachineKey? {
        guard let blob = store.read(SecretItem.machineKey), blob.count > 1 else { return nil }
        let body = blob.dropFirst()
        switch blob.first {
        case UInt8(ascii: "S"):
            do {
                return SecureEnclaveMachineKey(try SecureEnclave.P256.Signing.PrivateKey(dataRepresentation: body))
            } catch {
                // A handle from another Mac (or a wiped enclave) is not a key here.
                throw MachineKeyError.unavailable("the Secure Enclave refused the stored key (\(error.localizedDescription))")
            }
        case UInt8(ascii: "K"):
            return SoftwareMachineKey(try P256.Signing.PrivateKey(rawRepresentation: body))
        default:
            throw MachineKeyError.unavailable("the stored key is not one this app wrote")
        }
    }

    /// The existing key, or a new one.
    func loadOrCreate() throws -> MachineKey {
        if let k = try load() { return k }
        return try create()
    }

    /// A NEW key, replacing any old one (a new pairing never reuses a key).
    func create() throws -> MachineKey {
        if useEnclave {
            var error: Unmanaged<CFError>?
            guard let access = SecAccessControlCreateWithFlags(
                nil, kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly, .privateKeyUsage, &error)
            else { throw MachineKeyError.unavailable("access control: \(String(describing: error))") }
            let k = SecureEnclaveMachineKey(try SecureEnclave.P256.Signing.PrivateKey(accessControl: access))
            guard store.write(SecretItem.machineKey, Data("S".utf8) + k.handle) else {
                throw MachineKeyError.unavailable("the Keychain refused to store it")
            }
            return k
        }
        let k = SoftwareMachineKey()
        guard store.write(SecretItem.machineKey, Data("K".utf8) + k.raw) else {
            throw MachineKeyError.unavailable("the Keychain refused to store it")
        }
        return k
    }

    func delete() { store.delete(SecretItem.machineKey) }
}

/// The signed messages (docs/design/mac-pairing.md, lib/pair.ts on the server). Each starts with
/// its own domain string so a signature made for one purpose is never valid for another.
enum SigningInput {
    static let runnerProofDomain = "tmuxedo-runner-proof-v1"

    static func start(fingerprint: String, deviceName: String) -> String { "tmuxedo-pair-start-v1\n\(fingerprint)\n\(deviceName)" }
    static func poll(pairingId: String, ts: Int64) -> String { "tmuxedo-pair-poll-v1\n\(pairingId)\n\(ts)" }
    static func push(lookup: String, tokenSHA256Hex: String, ts: Int64) -> String { "tmuxedo-push-v1\n\(lookup)\n\(tokenSHA256Hex)\n\(ts)" }
    static func unpair(lookup: String, ts: Int64) -> String { "tmuxedo-unpair-v1\n\(lookup)\n\(ts)" }
    static func runnerProof(nonce: String, lookup: String) -> String { "\(runnerProofDomain)\n\(nonce)\n\(lookup)" }
}

extension Data {
    var base64URL: String {
        base64EncodedString().replacingOccurrences(of: "+", with: "-").replacingOccurrences(of: "/", with: "_")
            .replacingOccurrences(of: "=", with: "")
    }

    init?(base64URL s: String) {
        var b = s.replacingOccurrences(of: "-", with: "+").replacingOccurrences(of: "_", with: "/")
        while b.count % 4 != 0 { b += "=" }
        self.init(base64Encoded: b)
    }

    var hex: String { map { String(format: "%02x", $0) }.joined() }
}

extension DataProtocol {
    var sha256Hex: String { Data(SHA256.hash(data: Data(self))).hex }
}

/// The token's public lookup half (`tmr.<lookup>.<secret>`).
func runnerTokenLookup(_ token: String) -> String? {
    let parts = token.split(separator: ".", omittingEmptySubsequences: false)
    guard parts.count == 3, !parts[1].isEmpty, !parts[2].isEmpty, parts[0] == "tmr" || parts[0] == "amr" else { return nil }
    return String(parts[1])
}
