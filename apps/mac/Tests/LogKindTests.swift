import XCTest

/// Every line the supervisor and the runner write gets the right colour, and no failure is ever green.
final class LogKindTests: XCTestCase {
    private func k(_ s: String, error: Bool = false) -> LogKind { LogKind.classify(s, isError: error) }

    func testRunnerLinesWithTimestamps() {
        XCTAssertEqual(LogKind.stripTimestamp("2026-10-05T12:00:00.123Z connected to wss://tmuxedo.dev/gw/runner"),
                       "connected to wss://tmuxedo.dev/gw/runner")
        XCTAssertEqual(LogKind.stripTimestamp("connected to x"), "connected to x")
        XCTAssertEqual(k("2026-10-05T12:00:00Z connected to wss://tmuxedo.dev/gw/runner"), .success)
        XCTAssertEqual(k("tmuxedo-runner 0.1.0 starting; gateway wss://tmuxedo.dev/gw/runner"), .lifecycle)
        XCTAssertEqual(k("managed by tmuxedo for Mac: self-update off, key signer attached"), .lifecycle)
        XCTAssertEqual(k("answered the gateway's key challenge"), .security)
        XCTAssertEqual(k("gateway disconnected (code Some(1006): eof); reconnecting in 2.1s"), .warning)
        XCTAssertEqual(k("another holder still serves sessions at /x; waiting for it to exit (its sessions are left alone)"), .warning)
    }

    func testFailuresAreRedAndNeverGreen() {
        for line in [
            "cannot answer the gateway's key challenge: this machine's token is key-bound but no signer was given",
            "the gateway refused this machine's key proof; pair this machine again",
            "the runner exited with 1; it will restart",
            "could not start the runner: No such file",
            "cannot start: the bundled runner failed its signature check",
            "connected to x but then refused",
        ] {
            XCTAssertEqual(k(line), .error, line)
        }
        XCTAssertEqual(k("connected to wss://x", error: true), .error)
    }

    func testAppNotes() {
        XCTAssertEqual(k("starting tmuxedo-runner (attempt 1)"), .lifecycle)
        XCTAssertEqual(k("stopped"), .lifecycle)
        XCTAssertEqual(k("build: 0.1.0 (1) · dev · local · /x"), .lifecycle)
        XCTAssertEqual(k("paired as Mac via phone; machine key abc"), .success)
        XCTAssertEqual(k("holder restarts: its code changed (h1 to h2); terminals end and Claude sessions resume"), .warning)
        XCTAssertEqual(k("update available: 0.2.0 (2)"), .lifecycle)
        XCTAssertEqual(k("something else"), .info)
    }
}
