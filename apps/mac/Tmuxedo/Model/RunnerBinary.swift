import Foundation
import Security

/// The Rust `tmuxedo-runner` binary bundled in the app (`Contents/Helpers/tmuxedo-runner`).
///
/// One binary plays both parts: `holder` (its own LaunchAgent) and `run`
/// (supervised by the app). It is never rewritten here: this app is the
/// runner's supervisor and update channel, not a second implementation.
struct RunnerBinary: Equatable {
    let url: URL

    /// The bundled copy; `TMUXEDO_MAC_RUNNER` overrides it for development and tests.
    static func bundled(in bundle: Bundle = .main, environment: [String: String] = ProcessInfo.processInfo.environment) -> RunnerBinary? {
        if let p = environment["TMUXEDO_MAC_RUNNER"], !p.isEmpty, FileManager.default.isExecutableFile(atPath: p) {
            return RunnerBinary(url: URL(fileURLWithPath: p))
        }
        let u = bundle.bundleURL.appendingPathComponent("Contents/Helpers/tmuxedo-runner")
        return FileManager.default.isExecutableFile(atPath: u.path) ? RunnerBinary(url: u) : nil
    }

    // MARK: - Integrity (docs/design/mac-pairing.md §6)

    enum SignatureCheck: Equatable {
        case valid(team: String)
        /// Unsigned or ad-hoc (a local or CI build): allowed only when the app itself is not a signed release.
        case unsigned
        case invalid(String)
    }

    /// Checks the binary's code signature: valid, hardened runtime, and the SAME team as
    /// `team` (this app's). A failure refuses to start the runner.
    static func checkSignature(of url: URL, team: String?) -> SignatureCheck {
        var code: SecStaticCode?
        guard SecStaticCodeCreateWithPath(url as CFURL, [], &code) == errSecSuccess, let code else {
            return .invalid("not a code object")
        }
        var info: CFDictionary?
        guard SecCodeCopySigningInformation(code, SecCSFlags(rawValue: UInt32(kSecCSSigningInformation)), &info) == errSecSuccess,
              let dict = info as? [String: Any]
        else { return .invalid("its signature cannot be read") }
        let codeFlags = (dict[kSecCodeInfoFlags as String] as? NSNumber)?.uint32Value ?? 0
        guard let signedTeam = dict[kSecCodeInfoTeamIdentifier as String] as? String else {
            // No team: ad-hoc (kSecCodeSignatureAdhoc, 0x2) is a local build; anything else
            // (an Apple platform binary, say) is simply not ours.
            return codeFlags & 0x2 != 0 ? .unsigned : .invalid("it is not signed by tmuxedo")
        }
        if let team, signedTeam != team { return .invalid("it is signed by team \(signedTeam), not \(team)") }
        let reqText = "anchor apple generic and certificate leaf[subject.OU] = \"\(signedTeam)\""
        var req: SecRequirement?
        guard SecRequirementCreateWithString(reqText as CFString, [], &req) == errSecSuccess, let req else {
            return .invalid("requirement")
        }
        let flags = SecCSFlags(rawValue: UInt32(kSecCSCheckAllArchitectures) | UInt32(kSecCSStrictValidate))
        let rc = SecStaticCodeCheckValidity(code, flags, req)
        guard rc == errSecSuccess else { return .invalid("its signature does not verify (\(rc))") }
        // kSecCodeSignatureRuntime (0x10000): the hardened runtime.
        if let f = (dict[kSecCodeInfoFlags as String] as? NSNumber)?.uint32Value, f & 0x10000 == 0 {
            return .invalid("it was not built with the hardened runtime")
        }
        return .valid(team: signedTeam)
    }

    /// This app's own team identifier, nil when it is unsigned (a local or CI build).
    static var appTeam: String? {
        var code: SecCode?
        guard SecCodeCopySelf([], &code) == errSecSuccess, let code else { return nil }
        var s: SecStaticCode?
        guard SecCodeCopyStaticCode(code, [], &s) == errSecSuccess, let s else { return nil }
        var info: CFDictionary?
        guard SecCodeCopySigningInformation(s, SecCSFlags(rawValue: UInt32(kSecCSSigningInformation)), &info) == errSecSuccess else { return nil }
        return (info as? [String: Any])?[kSecCodeInfoTeamIdentifier as String] as? String
    }

    /// The decision: a signed app runs only a runner signed by its own team; an
    /// unsigned build may run an unsigned runner (development, CI).
    static func mayLaunch(check: SignatureCheck, appTeam: String?) -> String? {
        switch (check, appTeam) {
        case (.valid, _): return nil
        case (.unsigned, nil): return nil
        case (.unsigned, .some): return "the bundled runner is not signed; reinstall tmuxedo"
        case let (.invalid(why), _): return "the bundled runner failed its signature check: \(why); reinstall tmuxedo"
        }
    }

    // MARK: - What it is

    struct Version: Equatable, Decodable {
        let version: String
        let holderBuild: String
        let platform: String
        let features: [String]

        static func parse(_ json: String) -> Version? {
            try? JSONDecoder().decode(Version.self, from: Data(json.utf8))
        }
        /// What this app needs from a runner it starts.
        static let required = ["managed", "signer-fd", "holder-build", "holder-info"]
        var missingFeatures: [String] { Self.required.filter { !features.contains($0) } }
    }

    func version() -> Version? {
        guard let (rc, out) = Shell.run(url.path, ["version", "--json"]), rc == 0 else { return nil }
        return Version.parse(out)
    }

    struct HolderInfo: Equatable, Decodable {
        let running: Bool
        let version: String?
        let pid: Int?
        let build: String?
        static func parse(_ json: String) -> HolderInfo? {
            try? JSONDecoder().decode(HolderInfo.self, from: Data(json.utf8))
        }
    }

    /// The holder currently serving `socket`.
    func holderInfo(socket: URL?) -> HolderInfo? {
        var args = ["holder-info"]
        if let socket { args += ["--socket", socket.path] }
        guard let (rc, out) = Shell.run(url.path, args), rc == 0 else { return nil }
        return HolderInfo.parse(out)
    }
}
