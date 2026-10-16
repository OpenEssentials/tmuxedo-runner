import Foundation
import Security

/// This Mac's secrets, in the login Keychain: the runner token and the
/// machine key's Secure Enclave blob. Never in UserDefaults, never in a file,
/// never on screen.
///
/// `ThisDeviceOnly`: the items are never synchronised to iCloud Keychain and
/// never restored onto another Mac from a backup (docs/design/mac-pairing.md §3.1).
/// After first unlock, so a login-item launch can read them without the screen
/// being unlocked a second time.
///
/// A Keychain item's ACL is bound to the identity that created it. The app is
/// Developer ID signed, so that identity survives updates and the user is not
/// asked for their password on every launch.
enum Keychain {
    static let service = "dev.tmuxedo.mac"

    /// Is there an item? Reads ATTRIBUTES ONLY, which never prompts.
    ///
    /// Reading the secret of an item this app did not create makes macOS ask
    /// for the login password, and `SecItemCopyMatching` blocks the calling
    /// thread until someone answers. Presence is all the UI needs.
    static func has(account: String) -> Bool {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
            kSecReturnAttributes as String: true,
            kSecMatchLimit as String: kSecMatchLimitOne,
        ]
        var item: CFTypeRef?
        return SecItemCopyMatching(query as CFDictionary, &item) == errSecSuccess
    }

    static func read(account: String) -> Data? {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
            kSecReturnData as String: true,
            kSecMatchLimit as String: kSecMatchLimitOne,
        ]
        var item: CFTypeRef?
        guard SecItemCopyMatching(query as CFDictionary, &item) == errSecSuccess, let data = item as? Data, !data.isEmpty
        else { return nil }
        return data
    }

    /// Replaces any existing item. Empty data deletes it.
    @discardableResult
    static func write(_ data: Data, account: String) -> Bool {
        let base: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
        ]
        SecItemDelete(base as CFDictionary)
        guard !data.isEmpty else { return true }
        var add = base
        add[kSecValueData as String] = data
        add[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly
        add[kSecAttrSynchronizable as String] = false
        add[kSecAttrLabel as String] = "tmuxedo (\(account))"
        return SecItemAdd(add as CFDictionary, nil) == errSecSuccess
    }

    static func delete(account: String) {
        write(Data(), account: account)
    }
}

/// Where a secret lives. The Keychain in the app; memory in tests, which must
/// never read or overwrite the real items on this Mac.
protocol SecretStore: AnyObject {
    func has(_ item: String) -> Bool
    func read(_ item: String) -> Data?
    @discardableResult func write(_ item: String, _ data: Data) -> Bool
}

extension SecretStore {
    func readString(_ item: String) -> String? {
        read(item).flatMap { String(data: $0, encoding: .utf8) }.flatMap { $0.isEmpty ? nil : $0 }
    }
    @discardableResult func writeString(_ item: String, _ value: String) -> Bool {
        write(item, Data(value.trimmingCharacters(in: .whitespacesAndNewlines).utf8))
    }
    func delete(_ item: String) { write(item, Data()) }
}

enum SecretItem {
    static let runnerToken = "runner-token"
    static let machineKey = "machine-key"
}

/// The real Keychain, with a profile's own accounts when one is active.
final class KeychainStore: SecretStore {
    let profile: String?
    init(profile: String? = Profile.name) { self.profile = profile }
    func has(_ item: String) -> Bool { Keychain.has(account: Profile.keychainAccount(item, for: profile)) }
    func read(_ item: String) -> Data? { Keychain.read(account: Profile.keychainAccount(item, for: profile)) }
    @discardableResult func write(_ item: String, _ data: Data) -> Bool {
        Keychain.write(data, account: Profile.keychainAccount(item, for: profile))
    }
}

final class MemoryStore: SecretStore {
    private var items: [String: Data] = [:]
    private let lock = NSLock()
    init(_ initial: [String: Data] = [:]) { items = initial }
    func has(_ item: String) -> Bool { lock.lock(); defer { lock.unlock() }; return !(items[item]?.isEmpty ?? true) }
    func read(_ item: String) -> Data? { lock.lock(); defer { lock.unlock() }; return items[item].flatMap { $0.isEmpty ? nil : $0 } }
    @discardableResult func write(_ item: String, _ data: Data) -> Bool {
        lock.lock(); defer { lock.unlock() }
        items[item] = data.isEmpty ? nil : data
        return true
    }
}
