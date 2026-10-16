import CoreServices
import IOKit.pwr_mgt
import XCTest

/// The models carried over from the app this one was copied from, with their tests adapted:
/// build identity, update feed, launch kind, profiles, sleep, update outcomes.

final class BuildInfoTests: XCTestCase {
    private func info(_ channel: String, _ path: String) -> BuildInfo {
        BuildInfo(channel: channel, version: "0.1.0", build: "1", commit: "abc1234", bundlePath: path)
    }

    func testOnlyAnInstalledReleaseIsRelease() {
        XCTAssertTrue(info("release", "/Applications/tmuxedo.app").isRelease)
        XCTAssertTrue(info("motorcade", "/Applications/tmuxedo.app").isRelease)
        XCTAssertNil(info("motorcade", "/Applications/tmuxedo.app").devReason)
    }

    func testADevBuildIsDevWhereverItRuns() {
        let i = info("dev", "/Applications/tmuxedo.app")
        XCTAssertFalse(i.isRelease)
        XCTAssertTrue(i.devReason?.contains("dev build") ?? false)
    }

    func testAReleaseOutsideApplicationsIsDev() {
        let i = info("motorcade", "/Users/me/tmuxedo/mac/build/tmuxedo.app")
        XCTAssertFalse(i.isRelease)
        XCTAssertTrue(i.devReason?.contains("/Users/me/tmuxedo/mac/build") ?? false)
        XCTAssertTrue(i.summary.contains("abc1234"))
    }

    func testThisProcessIsNotARelease() {
        XCTAssertFalse(BuildInfo.current.isRelease)
    }
}

/// tmuxedo for Mac updates only from Motorcade's feed, and never sends a credential to it.
final class FeedSourceTests: XCTestCase {
    func testMotorcadesFeedIsBundledAndGetsNothing() {
        let s = FeedSource.from(infoFeed: "https://motorcade.io/apps/tmuxedo-mac/appcast.xml")
        XCTAssertEqual(s, .bundled("https://motorcade.io/apps/tmuxedo-mac/appcast.xml"))
        XCTAssertFalse(s.sendsCredentials)
    }

    func testNoFeedMeansNoUpdates() {
        for raw in [nil, "", "   ", "$(SU_FEED_URL)", "http://motorcade.io/appcast.xml", 5] as [Any?] {
            XCTAssertEqual(FeedSource.from(infoFeed: raw), .none, "\(String(describing: raw))")
        }
        XCTAssertFalse(FeedSource.none.sendsCredentials)
    }

    func testAMotorcadeBuildIsAPublishedRelease() {
        let feed = "https://motorcade.io/apps/tmuxedo-mac/appcast.xml"
        XCTAssertEqual(BuildInfo.channel(stamped: "dev", feed: feed), "motorcade")
        XCTAssertEqual(BuildInfo.channel(stamped: "dev", feed: nil), "dev")
        XCTAssertEqual(BuildInfo.channel(stamped: "release", feed: feed), "release")
    }

    /// This test bundle has no feed: a CI or local build never updates itself.
    func testThisBuildHasNoFeed() {
        XCTAssertEqual(FeedSource.current, .none)
    }
}

final class LaunchKindTests: XCTestCase {
    private func openApplication(loginItem: Bool) -> NSAppleEventDescriptor {
        let e = NSAppleEventDescriptor(eventClass: AEEventClass(kCoreEventClass), eventID: AEEventID(kAEOpenApplication),
                                       targetDescriptor: nil, returnID: AEReturnID(kAutoGenerateReturnID),
                                       transactionID: AETransactionID(kAnyTransactionID))
        if loginItem {
            e.setParam(NSAppleEventDescriptor(enumCode: OSType(keyAELaunchedAsLogInItem)), forKeyword: AEKeyword(keyAEPropData))
        }
        return e
    }

    func testLoginItemLaunchIsRecognised() { XCTAssertTrue(LaunchKind.isLoginItem(openApplication(loginItem: true))) }
    func testOpeningFromFinderIsNotALoginLaunch() { XCTAssertFalse(LaunchKind.isLoginItem(openApplication(loginItem: false))) }
    func testNoEventIsNotALoginLaunch() { XCTAssertFalse(LaunchKind.isLoginItem(nil)) }

    func testOtherEventsAreNotLoginLaunches() {
        let reopen = NSAppleEventDescriptor(eventClass: AEEventClass(kCoreEventClass), eventID: AEEventID(kAEReopenApplication),
                                            targetDescriptor: nil, returnID: AEReturnID(kAutoGenerateReturnID),
                                            transactionID: AETransactionID(kAnyTransactionID))
        reopen.setParam(NSAppleEventDescriptor(enumCode: OSType(keyAELaunchedAsLogInItem)), forKeyword: AEKeyword(keyAEPropData))
        XCTAssertFalse(LaunchKind.isLoginItem(reopen))
    }
}

/// A profile copy must never reach the owner's pairing, key or terminals.
final class ProfileTests: XCTestCase {
    func testNoProfileMeansTheOwnersOwnState() {
        XCTAssertNil(Profile.parse(nil))
        XCTAssertEqual(Profile.keychainAccount("runner-token", for: nil), "runner-token")
        XCTAssertTrue(Profile.defaults(for: nil) === UserDefaults.standard)
        XCTAssertNil(Profile.runtimeRoot(for: nil))
    }

    func testAProfileGetsItsOwnEverything() {
        let p = Profile.parse("verify")
        XCTAssertEqual(p, "verify")
        XCTAssertEqual(Profile.keychainAccount("runner-token", for: p), "runner-token.profile.verify")
        XCTAssertEqual(Profile.keychainAccount("machine-key", for: p), "machine-key.profile.verify")
        XCTAssertFalse(Profile.defaults(for: p) === UserDefaults.standard)
        XCTAssertEqual(Profile.runtimeRoot(for: p)?.path.hasSuffix("tmuxedo/profiles/verify"), true)
    }

    func testBadNamesAreRefused() {
        for bad in ["", "  ", "Has Space", "../etc", "a/b", "é", String(repeating: "a", count: 33)] {
            XCTAssertNil(Profile.parse(bad), bad)
        }
        XCTAssertEqual(Profile.parse(" Verify-2 "), "verify-2")
    }

    /// Against the real Keychain: a profile's items never touch the owner's (presence only is read for the owner).
    func testProfileSecretsNeverTouchTheOwnersEntries() {
        let ownerHadToken = Keychain.has(account: "runner-token")
        let store = KeychainStore(profile: "test-\(UUID().uuidString.prefix(8).lowercased())")
        defer { store.delete(SecretItem.runnerToken) }
        XCTAssertTrue(store.writeString(SecretItem.runnerToken, "tmr.profile.secret"))
        XCTAssertEqual(store.readString(SecretItem.runnerToken), "tmr.profile.secret")
        XCTAssertEqual(Keychain.has(account: "runner-token"), ownerHadToken)
        store.delete(SecretItem.runnerToken)
        XCTAssertNil(store.readString(SecretItem.runnerToken))
        XCTAssertEqual(Keychain.has(account: "runner-token"), ownerHadToken)
    }
}

final class SleepGuardTests: XCTestCase {
    private func held() -> Bool { SleepGuard.assertionsHeldByThisProcess().contains(SleepGuard.reason) }

    func testHoldAndReleaseReachThePowerManager() {
        let g = SleepGuard()
        XCTAssertFalse(held())
        XCTAssertTrue(g.hold())
        XCTAssertTrue(held(), "macOS does not know about the assertion")
        XCTAssertTrue(g.appNapDisabled)
        g.release()
        XCTAssertFalse(g.appNapDisabled)
        XCTAssertFalse(held())
    }

    func testIdempotent() {
        let g = SleepGuard()
        g.hold(); g.hold()
        XCTAssertEqual(SleepGuard.assertionsHeldByThisProcess().filter { $0 == SleepGuard.reason }.count, 1)
        g.release(); g.release()
        XCTAssertFalse(held())
    }

    func testDisplayIsAllowedToSleep() {
        let g = SleepGuard()
        g.hold()
        defer { g.release() }
        var raw: Unmanaged<CFDictionary>?
        XCTAssertEqual(IOPMCopyAssertionsByProcess(&raw), kIOReturnSuccess)
        let mine = (raw?.takeRetainedValue() as? [NSNumber: [[String: Any]]])?[NSNumber(value: getpid())] ?? []
        let types = mine.filter { $0[kIOPMAssertionNameKey] as? String == SleepGuard.reason }.compactMap { $0[kIOPMAssertionTypeKey] as? String }
        XCTAssertEqual(types, [kIOPMAssertionTypePreventUserIdleSystemSleep as String])
    }

    func testDroppingTheGuardReleases() {
        do { let g = SleepGuard(); g.hold(); XCTAssertTrue(held()) }
        XCTAssertFalse(held())
    }
}

final class UpdateOutcomeTests: XCTestCase {
    private func o(_ code: Int, domain: String = UpdateOutcome.sparkleDomain) -> (text: String, isFailure: Bool) {
        UpdateOutcome.fromAbort(domain: domain, code: code, message: "the message")
    }

    func testNothingNewerIsNotAFailure() {
        XCTAssertEqual(o(1001).text, "Up to date")
        XCTAssertFalse(o(1001).isFailure)
    }

    func testCancellingOrPostponingIsNotAFailure() {
        XCTAssertFalse(o(4007).isFailure)
        XCTAssertFalse(o(4008).isFailure)
    }

    func testRealFailuresStillSaySo() {
        XCTAssertTrue(o(2001).isFailure)
        XCTAssertEqual(o(2001).text, "Check failed: the message")
        XCTAssertTrue(o(1001, domain: NSURLErrorDomain).isFailure)
    }
}
