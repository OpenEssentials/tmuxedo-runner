import Darwin
import XCTest

/// The home lock and the folder grants, for real: this process (the Mac's owner) writes the
/// ACLs with FolderGrants, and the REAL `tmuxedo` user probes them through `sudo -u`.
///
/// Runs where the user exists and sudo needs no password: CI creates it first
/// (`sudo tmuxedo-helper setup`, .github/workflows/mac.yml) and sets TMUXEDO_TEST_ISO=1.
/// Skipped everywhere else. Every test leaves the owner's files as it found them.
final class IsolationLiveTests: XCTestCase {
    var user: uuid_t!
    var owner: uuid_t!
    var base: String!

    override func setUpWithError() throws {
        guard ProcessInfo.processInfo.environment["TMUXEDO_TEST_ISO"] == "1" else {
            throw XCTSkip("set TMUXEDO_TEST_ISO=1 where the tmuxedo user exists and sudo is passwordless")
        }
        guard let u = FolderAccess.uuid(ofUser: Iso.user), let o = FolderAccess.uuid(ofUser: NSUserName()) else {
            throw XCTSkip("the tmuxedo user does not exist")
        }
        user = u
        owner = o
        base = NSHomeDirectory() + "/tmx-live-\(UUID().uuidString.prefix(6))"
        for d in ["proj/src", "other", "later"] {
            try FileManager.default.createDirectory(atPath: base + "/" + d, withIntermediateDirectories: true,
                                                    attributes: [.posixPermissions: 0o755])
        }
        try FileManager.default.setAttributes([.posixPermissions: 0o755], ofItemAtPath: base)
        try "code".write(toFile: base + "/proj/src/a.txt", atomically: false, encoding: .utf8)
        try "other".write(toFile: base + "/other/b.txt", atomically: false, encoding: .utf8)
    }

    override func tearDownWithError() throws {
        if let base { _ = Shell.run("/bin/chmod", ["-R", "-N", base]); try? FileManager.default.removeItem(atPath: base) }
    }

    /// As the user: did `args` succeed?
    func asUser(_ args: String...) -> Bool {
        Shell.run("/usr/bin/sudo", ["-n", "-u", Iso.user, "--"] + args)?.0 == 0
    }

    func acl(_ path: String) -> String {
        Shell.run("/bin/ls", ["-led", path])?.1 ?? ""
    }

    func testLockGrantRevokeAndCleanUp() throws {
        var g = FolderGrants(home: NSHomeDirectory(), user: user, owner: owner, state: GrantState())
        defer {
            g.removeAll()
            XCTAssertFalse(acl(NSHomeDirectory()).contains(Iso.user), "the home lock is gone")
        }

        try g.lock()
        XCTAssertTrue(acl(NSHomeDirectory()).contains("user:\(Iso.user) deny"), acl(NSHomeDirectory()))
        XCTAssertFalse(asUser("/bin/ls", NSHomeDirectory()), "the owner's home is closed")
        XCTAssertFalse(asUser("/bin/cat", base + "/proj/src/a.txt"))

        try g.grant(base + "/proj")
        XCTAssertTrue(asUser("/bin/cat", base + "/proj/src/a.txt"), "granted: readable")
        XCTAssertTrue(asUser("/usr/bin/touch", base + "/proj/src/agent.txt"), "granted: writable")
        XCTAssertNoThrow(try "owner".write(toFile: base + "/proj/src/agent.txt", atomically: false, encoding: .utf8),
                         "the owner can still edit what the terminals wrote")
        XCTAssertFalse(asUser("/bin/cat", base + "/other/b.txt"), "a sibling stays closed")
        XCTAssertFalse(asUser("/bin/ls", base), "the folder on the way cannot be listed")
        XCTAssertFalse(asUser("/bin/ls", NSHomeDirectory()))
        // Something created next to the grant later is closed too (inherited deny).
        try "later".write(toFile: base + "/later-file.txt", atomically: false, encoding: .utf8)
        _ = Shell.run("/bin/chmod", ["644", base + "/later-file.txt"])
        XCTAssertFalse(asUser("/bin/cat", base + "/later-file.txt"))
        XCTAssertEqual(g.state.grants, [FolderGrants.standardise(base + "/proj")])

        try g.revoke(base + "/proj")
        XCTAssertFalse(asUser("/bin/cat", base + "/proj/src/a.txt"), "revoked: closed")
        XCTAssertFalse(asUser("/bin/cat", base + "/proj/src/agent.txt"), "including what the terminals created")
        XCTAssertTrue(g.state.revoked.contains(FolderGrants.standardise(base + "/proj")))

        try g.grant(base + "/proj")
        XCTAssertTrue(asUser("/bin/cat", base + "/proj/src/a.txt"), "granted again")
        XCTAssertTrue(g.state.revoked.isEmpty)
    }

    /// A link planted in a granted folder never carries the grant to what it points at.
    func testAPlantedLinkIsNotFollowed() throws {
        var g = FolderGrants(home: NSHomeDirectory(), user: user, owner: owner, state: GrantState())
        defer { g.removeAll() }
        try g.lock()
        try g.grant(base + "/proj")
        try FileManager.default.createSymbolicLink(atPath: base + "/proj/link", withDestinationPath: base + "/other/b.txt")
        try FileManager.default.createSymbolicLink(atPath: base + "/proj/dirlink", withDestinationPath: base + "/other")
        let n = try FolderAccess.grant(base + "/proj", user: user, owner: owner)
        XCTAssertGreaterThan(n, 0)
        XCTAssertFalse(acl(base + "/other/b.txt").contains(Iso.user), "the link's target got nothing")
        XCTAssertFalse(acl(base + "/other").contains(Iso.user + " allow"))
        XCTAssertFalse(asUser("/bin/cat", base + "/proj/link"))
        // Refusals: the home, a private folder, a file.
        XCTAssertThrowsError(try g.grant(NSHomeDirectory()))
        XCTAssertThrowsError(try g.grant(NSHomeDirectory() + "/.ssh"))
        XCTAssertThrowsError(try g.grant(base + "/proj/src/a.txt"))
    }

    /// The signer socket in a group-shared folder: a process of the separate user that is not
    /// the runner is refused (the admission sees its real uid and pid).
    func testTheSeparateUserCannotUseTheSignerUnlessItIsTheRunner() throws {
        let dir = "/tmp/tmx-live-signer-\(UUID().uuidString.prefix(6))"
        try FileManager.default.createDirectory(atPath: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(atPath: dir) }
        guard let gid = getgrnam(Iso.group)?.pointee.gr_gid, let uid = getpwnam(Iso.user)?.pointee.pw_uid else {
            throw XCTSkip("no group")
        }
        // As the helper does for the real signer folder (root sets the group): a process cannot
        // chown to a group it joined after it started, and the socket inherits the folder's group.
        XCTAssertEqual(Shell.run("/usr/bin/sudo", ["-n", "/usr/sbin/chown", ":\(Iso.group)", dir])?.0, 0)
        chmod(dir, 0o750)
        let path = dir + "/signer.sock"
        let seen = NSLock()
        var peers: [PeerInfo] = []
        let admission = SignerAdmission(uid: uid, runnerPid: { _ in 1 }, requirement: nil) // pid 1 is never a terminal
        let server = SignerSocketServer(path: path, key: SoftwareMachineKey(), lookup: "MINE", broker: ConfirmBroker(), group: gid,
                                        admit: { p in seen.lock(); peers.append(p); seen.unlock(); return admission.check(p) })
        try server.listen()
        defer { server.stop() }
        // nc, not python3: /usr/bin/python3 is a developer-tools shim a fresh user cannot run.
        let r = Shell.runFull("/usr/bin/sudo", ["-n", "-u", Iso.user, "--", "/bin/sh", "-c",
                                                #"printf '{"op":"confirm","pane":"p","what":"x","timeout":30}\n' | /usr/bin/nc -U -w 3 "$0""#,
                                                path])
        XCTAssertEqual(r?.0, 0, "the user reaches the socket (group \(Iso.group)): \(r?.2 ?? "")")
        XCTAssertTrue(r?.1.contains("not the runner") == true, "\(r?.1 ?? "") \(r?.2 ?? "")")
        Thread.sleep(forTimeInterval: 0.2)
        seen.lock(); defer { seen.unlock() }
        XCTAssertEqual(peers.first?.uid, uid, "the kernel's view of the peer")
    }
}
