import Foundation

/// Tiny synchronous runner for one-shot probes (`launchctl`, the runner's `version --json`,
/// and the helper's `dscl` / `dseditgroup` calls).
enum Shell {
    /// (status, stdout) or nil when it could not start.
    static func run(_ exe: String, _ args: [String], environment: [String: String]? = nil, timeout: TimeInterval = 15) -> (Int32, String)? {
        guard let (rc, out, _) = runFull(exe, args, environment: environment, timeout: timeout) else { return nil }
        return (rc, out)
    }

    /// (status, stdout, stderr) or nil when it could not start or ran out of time.
    static func runFull(_ exe: String, _ args: [String], environment: [String: String]? = nil,
                        timeout: TimeInterval = 15) -> (Int32, String, String)? {
        let p = Process()
        p.executableURL = URL(fileURLWithPath: exe)
        p.arguments = args
        if let environment { p.environment = environment }
        let out = Pipe()
        let err = Pipe()
        p.standardOutput = out
        p.standardError = err
        p.standardInput = FileHandle.nullDevice
        do { try p.run() } catch { return nil }
        let deadline = DispatchTime.now() + timeout
        let done = DispatchSemaphore(value: 0)
        DispatchQueue.global().async { p.waitUntilExit(); done.signal() }
        let errBox = DataBox()
        let errRead = DispatchSemaphore(value: 0)
        DispatchQueue.global().async { errBox.set(err.fileHandleForReading.readDataToEndOfFile()); errRead.signal() }
        let data = out.fileHandleForReading.readDataToEndOfFile()
        _ = errRead.wait(timeout: deadline)
        if done.wait(timeout: deadline) == .timedOut { p.terminate(); return nil }
        let s = { (d: Data) in String(data: d, encoding: .utf8)?.trimmingCharacters(in: .whitespacesAndNewlines) ?? "" }
        return (p.terminationStatus, s(data), s(errBox.get()))
    }

    private final class DataBox: @unchecked Sendable {
        private let lock = NSLock()
        private var d = Data()
        func set(_ v: Data) { lock.lock(); d = v; lock.unlock() }
        func get() -> Data { lock.lock(); defer { lock.unlock() }; return d }
    }
}
