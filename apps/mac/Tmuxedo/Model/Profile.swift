import Foundation

/// An isolated identity for a copy of the app that is NOT the owner's.
///
/// WHY THIS EXISTS (carried over from the app this one was copied from, where
/// it was learned the hard way): a freshly built copy and the installed one
/// share the same settings domain and Keychain entries, so every manual check
/// of a build would replace or delete the owner's pairing.
///
/// Launched with `TMUXEDO_MAC_PROFILE=<name>` (`fastlane mac try`), the app uses its own:
///
///   · settings   UserDefaults suite `dev.tmuxedo.mac.profile.<name>`
///   · secrets    Keychain accounts `<item>.profile.<name>` (token, machine key)
///   · runtime    `~/Library/Application Support/tmuxedo/profiles/<name>`: its own
///                holder socket and state, so it never touches the owner's terminals
///
/// and says so on the dashboard. Without the variable nothing changes.
enum Profile {
    static let environmentKey = "TMUXEDO_MAC_PROFILE"

    /// The active profile, or nil for the owner's real app.
    static let name: String? = parse(ProcessInfo.processInfo.environment[environmentKey])

    /// Lower-case letters, digits and dashes, at most 32. Anything else is
    /// refused (nil) rather than mangled: a profile name that silently became
    /// a different one would share state it was meant not to.
    static func parse(_ raw: String?) -> String? {
        guard let raw else { return nil }
        let s = raw.trimmingCharacters(in: .whitespacesAndNewlines).lowercased()
        let ok = CharacterSet(charactersIn: "abcdefghijklmnopqrstuvwxyz0123456789-")
        guard !s.isEmpty, s.count <= 32, s.unicodeScalars.allSatisfy(ok.contains) else { return nil }
        return s
    }

    static func defaults(for profile: String?) -> UserDefaults {
        guard let profile else { return .standard }
        return UserDefaults(suiteName: "dev.tmuxedo.mac.profile.\(profile)") ?? .standard
    }

    /// The Keychain account for `item` (`runner-token`, `machine-key`).
    static func keychainAccount(_ item: String, for profile: String?) -> String {
        profile.map { "\(item).profile.\($0)" } ?? item
    }

    /// A profile's own runtime directory (holder socket, runner state); nil = the owner's defaults.
    static func runtimeRoot(for profile: String?) -> URL? {
        guard let profile else { return nil }
        return FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent("Library/Application Support/tmuxedo/profiles/\(profile)", isDirectory: true)
    }
}
