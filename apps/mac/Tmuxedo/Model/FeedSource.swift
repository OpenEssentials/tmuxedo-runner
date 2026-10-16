import Foundation

/// Where this copy's updates come from, decided from the bundle alone (no
/// Sparkle here, so the model tests can reach it).
///
/// tmuxedo for Mac updates ONLY from the feed Motorcade's Mac lane writes into
/// its Info.plist (`SUFeedURL`, with `SUPublicEDKey`). That feed is public and
/// gets NO credential: the app never sends its runner token, its key or a
/// session anywhere but tmuxedo itself. A copy without a feed (a local or CI
/// build) does not update.
enum FeedSource: Equatable {
    /// The Info.plist feed (Motorcade's).
    case bundled(String)
    /// No feed: updates are off for this copy.
    case none

    static func from(infoFeed: Any?) -> FeedSource {
        if let s = (infoFeed as? String)?.trimmingCharacters(in: .whitespaces),
           s.hasPrefix("https://"), !s.hasPrefix("$(") {
            return .bundled(s)
        }
        return .none
    }

    static var current: FeedSource { from(infoFeed: Bundle.main.object(forInfoDictionaryKey: "SUFeedURL")) }

    /// Never: no credential is ever sent to an update feed.
    var sendsCredentials: Bool { false }
}
