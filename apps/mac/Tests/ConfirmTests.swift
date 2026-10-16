import Darwin
import XCTest

/// Confirm on this Mac (docs/design/mac-isolation.md §3), the app's half: the broker's Allow,
/// Deny and timeout; the signer routing the runner's `confirm` / `confirm_poll`; the flags the
/// runner gets; and the local attach log. (The runner's half, holding input until allowed, is
/// in runner/crates/runner/tests/gateway.rs `confirm_on_mac_holds_control_until_allowed`.)
final class ConfirmTests: XCTestCase {
    final class Clock: @unchecked Sendable {
        var t = Date(timeIntervalSince1970: 1_800_000_000)
    }

    func broker(_ clock: Clock) -> (ConfirmBroker, () -> [String]) {
        var n = 0
        let b = ConfirmBroker(now: { clock.t }, makeTicket: { n += 1; return "t\(n)" })
        let lock = NSLock()
        var events: [String] = []
        b.present = { q in lock.lock(); events.append("present \(q.ticket) \(q.pane) \(q.what) \(Int(q.timeout))"); lock.unlock() }
        b.withdraw = { t in lock.lock(); events.append("withdraw \(t)"); lock.unlock() }
        b.decided = { q, s in lock.lock(); events.append("decided \(q.ticket) \(s.rawValue)"); lock.unlock() }
        return (b, { lock.lock(); defer { lock.unlock() }; return events })
    }

    func testAllow() throws {
        let c = Clock()
        let (b, events) = broker(c)
        let ticket = try b.ask(pane: "w1:p1", what: "typing", timeout: 30).get()
        XCTAssertEqual(b.poll(ticket), .pending)
        XCTAssertEqual(b.open.map(\.ticket), [ticket])
        XCTAssertTrue(b.answer(ticket, allow: true))
        XCTAssertEqual(b.poll(ticket), .allow)
        XCTAssertFalse(b.answer(ticket, allow: false), "an answer is final")
        XCTAssertEqual(b.poll(ticket), .allow)
        XCTAssertEqual(events(), ["present t1 w1:p1 typing 30", "withdraw t1", "decided t1 allow"])
        XCTAssertTrue(b.open.isEmpty)
    }

    func testDeny() throws {
        let c = Clock()
        let (b, events) = broker(c)
        let ticket = try b.ask(pane: "w1:p2", what: "answering a prompt", timeout: 30).get()
        XCTAssertTrue(b.answer(ticket, allow: false))
        XCTAssertEqual(b.poll(ticket), .deny)
        XCTAssertEqual(events().last, "decided t1 deny")
    }

    func testTimeoutIsADenyAndALateAllowChangesNothing() throws {
        let c = Clock()
        let (b, events) = broker(c)
        let ticket = try b.ask(pane: "w1:p1", what: "typing", timeout: 30).get()
        c.t += 29
        XCTAssertEqual(b.poll(ticket), .pending)
        c.t += 1
        XCTAssertEqual(b.poll(ticket), .timeout)
        XCTAssertEqual(events().suffix(2), ["withdraw t1", "decided t1 timeout"])
        XCTAssertFalse(b.answer(ticket, allow: true), "too late")
        XCTAssertEqual(b.poll(ticket), .timeout)
    }

    func testTickTimesOutWithoutAPoll() throws {
        let c = Clock()
        let (b, events) = broker(c)
        _ = try b.ask(pane: "p", what: "typing", timeout: 5).get()
        c.t += 6
        b.tick()
        XCTAssertEqual(events().last, "decided t1 timeout")
    }

    func testDecidedQuestionsAreForgottenAndUnknownTicketsAreNil() throws {
        let c = Clock()
        let (b, _) = broker(c)
        let ticket = try b.ask(pane: "p", what: "typing", timeout: 30).get()
        b.answer(ticket, allow: true)
        c.t += ConfirmBroker.keepDecided + 1
        XCTAssertNil(b.poll(ticket))
        XCTAssertNil(b.poll("nope"))
    }

    func testBoundsTimeoutsQuestionsAndText() throws {
        let c = Clock()
        let (b, events) = broker(c)
        for _ in 0..<ConfirmBroker.maxOpen { _ = try b.ask(pane: "p", what: "typing", timeout: 1).get() }
        XCTAssertEqual(b.ask(pane: "p", what: "typing", timeout: 30), .failure(.tooMany))
        XCTAssertTrue(events()[0].hasSuffix(" 5"), "a timeout below 5 s is raised to 5: \(events()[0])")
        let (b2, events2) = broker(c)
        _ = try b2.ask(pane: String(repeating: "x", count: 500) + "\u{1b}[31m", what: "typing\n\u{7}", timeout: 9999).get()
        let shown = events2()[0]
        XCTAssertFalse(shown.contains("\u{1b}") || shown.contains("\n") || shown.contains("\u{7}"), "control characters are never shown")
        XCTAssertTrue(shown.hasSuffix(" 300"))
        XCTAssertLessThan(shown.count, 200)
    }

    // MARK: - The signer routes the runner's questions

    func testSignerRoutesConfirmAndPoll() throws {
        let key = SoftwareMachineKey()
        let c = Clock()
        let (b, _) = broker(c)
        let ask = SignerPolicy.answer(#"{"op":"confirm","pane":"w1:p1","what":"typing","timeout":30}"#, key: key, ownLookup: "L", broker: b)
        XCTAssertEqual(ask, #"{"ticket":"t1"}"#)
        XCTAssertEqual(SignerPolicy.answer(#"{"op":"confirm_poll","ticket":"t1"}"#, key: key, ownLookup: "L", broker: b), #"{"state":"pending"}"#)
        b.answer("t1", allow: true)
        XCTAssertEqual(SignerPolicy.answer(#"{"op":"confirm_poll","ticket":"t1"}"#, key: key, ownLookup: "L", broker: b), #"{"state":"allow"}"#)
        XCTAssertTrue(SignerPolicy.answer(#"{"op":"confirm_poll","ticket":"zz"}"#, key: key, ownLookup: "L", broker: b).contains("unknown ticket"))
        XCTAssertTrue(SignerPolicy.answer(#"{"op":"confirm","pane":1}"#, key: key, ownLookup: "L", broker: b).contains("bad request"))
    }

    /// Confirm-on-Mac off: the runner's questions are bad requests, and the runner then DENIES (fails closed).
    func testWithoutABrokerConfirmIsRefused() {
        let key = SoftwareMachineKey()
        XCTAssertTrue(SignerPolicy.answer(#"{"op":"confirm","pane":"p","what":"typing","timeout":30}"#, key: key, ownLookup: "L")
            .contains("bad request"))
        XCTAssertTrue(SignerPolicy.answer(#"{"op":"confirm_poll","ticket":"t1"}"#, key: key, ownLookup: "L").contains("bad request"))
    }

    /// Over the real socketpair the runner holds: a sign and a question interleave on one channel.
    func testQuestionsOverTheChannel() throws {
        let key = SoftwareMachineKey()
        let c = Clock()
        let (b, _) = broker(c)
        let ch = try SignerChannel(key: key, lookup: "MINE", broker: b)
        ch.start()
        defer { ch.stop() }
        let fd = ch.childFD
        func roundTrip(_ line: String) -> String {
            let out = Array((line + "\n").utf8)
            _ = out.withUnsafeBytes { write(fd, $0.baseAddress, $0.count) }
            var buf = [UInt8](repeating: 0, count: 4096)
            var got = Data()
            while !got.contains(0x0A) {
                let n = read(fd, &buf, buf.count)
                if n <= 0 { break }
                got.append(contentsOf: buf[0..<n])
            }
            return String(decoding: got, as: UTF8.self).trimmingCharacters(in: .newlines)
        }
        XCTAssertEqual(roundTrip(#"{"op":"confirm","pane":"w1:p1","what":"typing","timeout":30}"#), #"{"ticket":"t1"}"#)
        let nonce = "Zm9vYmFyZm9vYmFyZm9vYmFyZm9vYmFy"
        let input = SigningInput.runnerProof(nonce: nonce, lookup: "MINE")
        let sig = roundTrip("{\"op\":\"sign\",\"input\":\"\(Data(input.utf8).base64EncodedString())\"}")
        XCTAssertTrue(sig.hasPrefix(#"{"sig""#), "a signature is not held up by an open question")
        b.answer("t1", allow: false)
        XCTAssertEqual(roundTrip(#"{"op":"confirm_poll","ticket":"t1"}"#), #"{"state":"deny"}"#)
    }

    // MARK: - The runner is told, and its answers are logged

    func testTheRunnerGetsTheFlags() {
        let on = LaunchSpec.runner(binary: URL(fileURLWithPath: "/bin/true"), server: "https://x", token: "t", signerFD: 3,
                                   workDir: nil, runtimeDir: nil, confirm: true, confirmTimeout: 60)
        XCTAssertEqual(Array(on.arguments.suffix(3)), ["--confirm-on-mac", "--confirm-timeout", "60"])
        let off = LaunchSpec.runner(binary: URL(fileURLWithPath: "/bin/true"), server: "https://x", token: "t", signerFD: 3,
                                    workDir: nil, runtimeDir: nil)
        XCTAssertFalse(off.arguments.contains("--confirm-on-mac"))
        let iso = IsoRunnerConfig(server: "https://x", token: "tmr.abc.def", allowUser: "u1", observeOnly: false, confirm: true,
                                  confirmTimeout: 15, workDir: nil)
        XCTAssertEqual(Array(iso.arguments.suffix(3)), ["--confirm-on-mac", "--confirm-timeout", "15"])
    }

    @MainActor
    func testTheAttachLogKeepsEveryRemoteControlOutcome() {
        let launcher = FakeLauncher()
        let sup = Supervisor(launcher: launcher, makeSpec: {
            .success((LaunchSpec(executable: URL(fileURLWithPath: "/bin/true"), arguments: [], environment: [:], signerFD: nil), nil))
        })
        sup.start()
        for l in ["remote control: asking on this Mac (pane w1:p1, typing)", "remote control: allowed on this Mac",
                  "remote control: denied on this Mac; held input dropped", "remote control: not answered on this Mac in time; denied",
                  "something else"] {
            launcher.last.say(l)
        }
        drainMain(self)
        XCTAssertEqual(sup.remoteLog.count, 4)
        XCTAssertTrue(sup.remoteLog.allSatisfy { $0.text.hasPrefix("remote control") })
        sup.terminateNow()
    }
}
