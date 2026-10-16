import CryptoKit
import Foundation
import Security

/// SSH keys held by this Mac's Secure Enclave (docs/design/secrets-and-env.md §6a.1).
///
/// The runner's ssh-agent asks the app over the signer channel:
///   → `{"op":"ssh_create","keyId":"…"}`  ← `{"publicKey":"ecdsa-sha2-nistp256 AAAA…"}`
///   → `{"op":"ssh_sign","keyId":"…","data":"<base64>"}`  ← `{"sig":"<base64 SSH signature blob>"}`
///
/// Each key is an ECDSA P-256 key created inside the Secure Enclave; the
/// Keychain keeps only the enclave's wrapped handle (`ssh-key.<id>`,
/// ThisDeviceOnly), so the private half can neither leave this Mac nor be
/// read by the runner or any agent. A Mac without an enclave gets a software
/// key in the Keychain and the runner is told so (it reports `software`).
final class SshKeys {
    private let store: SecretStore
    private let useEnclave: Bool

    init(store: SecretStore, useEnclave: Bool = SecureEnclave.isAvailable) {
        self.store = store
        self.useEnclave = useEnclave
    }

    static func item(_ id: String) -> String { "ssh-key.\(id)" }

    static func validId(_ id: String) -> Bool {
        (1...64).contains(id.count) && id.unicodeScalars.allSatisfy { $0.isASCII && (CharacterSet.alphanumerics.contains($0) || $0 == "-" || $0 == "_") }
    }

    /// A new key for `id` (an existing one is kept: the server asks once per key).
    func create(_ id: String) throws -> (line: String, hardware: Bool) {
        guard Self.validId(id) else { throw MachineKeyError.unavailable("bad key id") }
        if let k = try load(id) { return (Self.publicLine(k.publicX963), k.isHardwareBacked) }
        if useEnclave {
            var error: Unmanaged<CFError>?
            guard let access = SecAccessControlCreateWithFlags(nil, kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly, .privateKeyUsage, &error)
            else { throw MachineKeyError.unavailable("access control: \(String(describing: error))") }
            let k = try SecureEnclave.P256.Signing.PrivateKey(accessControl: access)
            guard store.write(Self.item(id), Data("S".utf8) + k.dataRepresentation) else { throw MachineKeyError.unavailable("the Keychain refused to store it") }
            return (Self.publicLine(k.publicKey.x963Representation), true)
        }
        let k = P256.Signing.PrivateKey()
        guard store.write(Self.item(id), Data("K".utf8) + k.rawRepresentation) else { throw MachineKeyError.unavailable("the Keychain refused to store it") }
        return (Self.publicLine(k.publicKey.x963Representation), false)
    }

    private func load(_ id: String) throws -> MachineKey? {
        guard let blob = store.read(Self.item(id)), blob.count > 1 else { return nil }
        let body = blob.dropFirst()
        switch blob.first {
        case UInt8(ascii: "S"): return SecureEnclaveMachineKey(try SecureEnclave.P256.Signing.PrivateKey(dataRepresentation: body))
        case UInt8(ascii: "K"): return SoftwareMachineKey(try P256.Signing.PrivateKey(rawRepresentation: body))
        default: return nil
        }
    }

    /// An SSH signature blob (`ecdsa-sha2-nistp256`, mpint r, mpint s) over `data`.
    func sign(_ id: String, data: Data) throws -> Data {
        guard Self.validId(id), let k = try load(id) else { throw MachineKeyError.unavailable("no such key on this Mac") }
        let der = try k.sign(data)
        let raw = try P256.Signing.ECDSASignature(derRepresentation: der).rawRepresentation
        var inner = Data()
        inner += Self.mpint(raw.prefix(32))
        inner += Self.mpint(raw.suffix(32))
        return Self.string(Data("ecdsa-sha2-nistp256".utf8)) + Self.string(inner)
    }

    static func string(_ d: Data) -> Data {
        var n = UInt32(d.count).bigEndian
        return Data(bytes: &n, count: 4) + d
    }

    static func mpint(_ d: Data) -> Data {
        var b = Data(d.drop(while: { $0 == 0 }))
        if let f = b.first, f & 0x80 != 0 { b.insert(0, at: 0) }
        return string(b)
    }

    /// `ecdsa-sha2-nistp256 <base64 blob>` for an uncompressed public point.
    static func publicLine(_ x963: Data) -> String {
        let blob = string(Data("ecdsa-sha2-nistp256".utf8)) + string(Data("nistp256".utf8)) + string(x963)
        return "ecdsa-sha2-nistp256 \(blob.base64EncodedString())"
    }

    /// The signer channel's `ssh_create` / `ssh_sign` answers.
    func answer(_ req: [String: Any]) -> String {
        guard let op = req["op"] as? String, let id = req["keyId"] as? String else { return #"{"error":"bad request"}"# }
        do {
            switch op {
            case "ssh_create":
                let (line, hw) = try create(id)
                return SignerPolicy.json(["publicKey": line, "hardware": hw ? "secure_enclave" : "software"])
            case "ssh_sign":
                guard let b64 = req["data"] as? String, let data = Data(base64Encoded: b64), data.count <= 64 * 1024 else { return #"{"error":"bad request"}"# }
                return SignerPolicy.json(["sig": try sign(id, data: data).base64EncodedString()])
            default:
                return #"{"error":"bad request"}"#
            }
        } catch {
            return SignerPolicy.json(["error": error.localizedDescription])
        }
    }
}
