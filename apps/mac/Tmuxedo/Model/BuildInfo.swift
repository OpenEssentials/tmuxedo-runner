import Foundation

/// Which build this is, so a test copy can never pass for the owner's.
///
/// Two signals, because either alone can be wrong:
///
///   · the CHANNEL stamped at build time (`TMXBuildChannel` in Info.plist,
///     from the `TMX_BUILD_CHANNEL` build setting), `dev` unless a release
///     lane says otherwise. A build Motorcade distributes carries Motorcade's
///     feed in its Info.plist and counts as `motorcade`.
///   · WHERE IT RUNS FROM. The owner's copy lives in /Applications. A copy
///     running from anywhere else is a test copy, whatever its stamp says.
struct BuildInfo: Equatable {
    let channel: String
    let version: String
    let build: String
    let commit: String
    let bundlePath: String

    static let current: BuildInfo = {
        let info = Bundle.main.infoDictionary ?? [:]
        func s(_ k: String, _ fallback: String) -> String {
            let v = (info[k] as? String)?.trimmingCharacters(in: .whitespaces) ?? ""
            // An unexpanded "$(…)" means the setting never reached the plist.
            return v.isEmpty || v.hasPrefix("$(") ? fallback : v
        }
        return BuildInfo(channel: channel(stamped: s("TMXBuildChannel", "dev"), feed: info["SUFeedURL"]),
                         version: s("CFBundleShortVersionString", "?"),
                         build: s("CFBundleVersion", "?"),
                         commit: s("TMXBuildCommit", "unknown"),
                         bundlePath: Bundle.main.bundlePath)
    }()

    /// A build Motorcade distributes is a published release too: its lane
    /// writes Motorcade's feed into the Info.plist, which no dev build carries.
    static func channel(stamped: String, feed: Any?) -> String {
        if stamped == "release" { return stamped }
        if case .bundled = FeedSource.from(infoFeed: feed) { return "motorcade" }
        return stamped
    }

    /// The owner's installed release, and nothing else.
    var isRelease: Bool {
        (channel == "release" || channel == "motorcade") && bundlePath.hasPrefix("/Applications/")
    }

    /// One line for the badge's tooltip and the log: what, from where.
    var summary: String {
        "\(version) (\(build)) · \(channel) · \(commit) · \(bundlePath)"
    }

    /// Why it counts as dev, in words.
    var devReason: String? {
        if isRelease { return nil }
        if channel != "release" && channel != "motorcade" { return "a \(channel) build (\(commit)), not a published release" }
        return "a release build running from \(bundlePath), not the copy installed in /Applications"
    }
}
