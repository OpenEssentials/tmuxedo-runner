import Darwin
import Foundation
import Security

/// Who is on the other end of a unix socket, from the kernel (never from what the peer says).
struct PeerInfo: Equatable {
    let uid: uid_t
    let gid: gid_t
    let pid: pid_t
    /// The peer's audit token: identifies the process for a code-signature check.
    let auditToken: Data?

    static func of(fd: Int32) -> PeerInfo? {
        var uid: uid_t = 0, gid: gid_t = 0
        guard getpeereid(fd, &uid, &gid) == 0 else { return nil }
        var pid: pid_t = 0
        var len = socklen_t(MemoryLayout<pid_t>.size)
        guard getsockopt(fd, SOL_LOCAL, LOCAL_PEERPID, &pid, &len) == 0 else { return nil }
        var token = audit_token_t()
        var tlen = socklen_t(MemoryLayout<audit_token_t>.size)
        let data: Data? = getsockopt(fd, SOL_LOCAL, LOCAL_PEERTOKEN, &token, &tlen) == 0
            ? withUnsafeBytes(of: &token) { Data($0) } : nil
        return PeerInfo(uid: uid, gid: gid, pid: pid, auditToken: data)
    }

    /// The peer's code satisfies `requirement` (checked on the RUNNING process, by audit token).
    func satisfies(_ requirement: String) -> Bool {
        guard let auditToken else { return false }
        var code: SecCode?
        let attrs = [kSecGuestAttributeAudit: auditToken] as CFDictionary
        guard SecCodeCopyGuestWithAttributes(nil, attrs, [], &code) == errSecSuccess, let code else { return false }
        var req: SecRequirement?
        guard SecRequirementCreateWithString(requirement as CFString, [], &req) == errSecSuccess, let req else { return false }
        return SecCodeCheckValidity(code, [], req) == errSecSuccess
    }
}

/// Which peers may use the signer socket (docs/design/mac-isolation.md §2.1). A terminal runs
/// as the same uid as the runner, so the uid alone proves nothing: the peer must also be the
/// pid launchd started for the runner daemon and, in a signed app, code signed by our team.
struct SignerAdmission {
    let uid: uid_t
    /// The runner daemon's pid now (asks the helper when unknown); nil = none running.
    let runnerPid: (_ refresh: Bool) -> pid_t?
    /// The code requirement the peer must meet; nil only in an unsigned (development, CI) build.
    let requirement: String?

    /// nil = admit; else why not.
    func check(_ p: PeerInfo) -> String? {
        guard p.uid == uid else { return "uid \(p.uid) is not the runner's user" }
        var pid = runnerPid(false)
        if pid != p.pid { pid = runnerPid(true) }
        guard let pid, pid == p.pid else { return "pid \(p.pid) is not the runner launchd started" }
        if let requirement, !p.satisfies(requirement) { return "pid \(p.pid) is not signed by tmuxedo" }
        return nil
    }
}

/// The signer as a listening unix socket, for a runner that is NOT the app's child (the
/// separate-user mode's LaunchDaemon). The directory is the owner's, group `tmuxedo`, 0750;
/// the socket 0660. Every connection is checked (SignerAdmission) before a byte is read.
final class SignerSocketServer: SignerEndpoint {
    let path: String
    private let key: MachineKey
    private let lookup: String
    private let broker: ConfirmBroker?
    private let group: gid_t?
    private let admit: (PeerInfo) -> String?
    private let onSign: (Bool) -> Void
    private let onRefuse: (String) -> Void
    private var listenFD: Int32 = -1
    private let lock = NSLock()

    init(path: String, key: MachineKey, lookup: String, broker: ConfirmBroker?, group: gid_t?,
         admit: @escaping (PeerInfo) -> String?, onSign: @escaping (Bool) -> Void = { _ in },
         onRefuse: @escaping (String) -> Void = { _ in }) {
        self.path = path
        self.key = key
        self.lookup = lookup
        self.broker = broker
        self.group = group
        self.admit = admit
        self.onSign = onSign
        self.onRefuse = onRefuse
    }

    enum SocketError: Error, LocalizedError {
        case posix(String, Int32)
        var errorDescription: String? {
            switch self { case let .posix(what, e): return "signer socket: \(what): \(String(cString: strerror(e)))" }
        }
    }

    /// Bind and listen (throws), then serve on a background thread.
    func listen() throws {
        unlink(path)
        let fd = socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else { throw SocketError.posix("socket", errno) }
        _ = fcntl(fd, F_SETFD, FD_CLOEXEC)
        var addr = sockaddr_un()
        addr.sun_family = sa_family_t(AF_UNIX)
        let bytes = Array(path.utf8)
        guard bytes.count < MemoryLayout.size(ofValue: addr.sun_path) else {
            close(fd)
            throw SocketError.posix("path too long", ENAMETOOLONG)
        }
        withUnsafeMutableBytes(of: &addr.sun_path) { raw in
            for (i, b) in bytes.enumerated() { raw[i] = b }
            raw[bytes.count] = 0
        }
        addr.sun_len = UInt8(MemoryLayout<sockaddr_un>.size)
        let rc = withUnsafePointer(to: &addr) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { bind(fd, $0, socklen_t(MemoryLayout<sockaddr_un>.size)) }
        }
        guard rc == 0 else { let e = errno; close(fd); throw SocketError.posix("bind \(path)", e) }
        if let group { _ = chown(path, uid_t.max, group) } // uid -1: unchanged
        guard chmod(path, 0o660) == 0 else { let e = errno; close(fd); throw SocketError.posix("chmod", e) }
        guard Darwin.listen(fd, 8) == 0 else { let e = errno; close(fd); throw SocketError.posix("listen", e) }
        lock.lock(); listenFD = fd; lock.unlock()
        let t = Thread { [weak self] in self?.acceptLoop(fd) }
        t.name = "tmuxedo-signer-socket"
        t.start()
    }

    private func acceptLoop(_ fd: Int32) {
        while true {
            let c = accept(fd, nil, nil)
            if c < 0 {
                if errno == EINTR { continue }
                return // closed by stop()
            }
            _ = fcntl(c, F_SETFD, FD_CLOEXEC)
            var on: Int32 = 1
            _ = setsockopt(c, SOL_SOCKET, SO_NOSIGPIPE, &on, socklen_t(MemoryLayout<Int32>.size))
            var tv = timeval(tv_sec: 10, tv_usec: 0)
            _ = setsockopt(c, SOL_SOCKET, SO_RCVTIMEO, &tv, socklen_t(MemoryLayout<timeval>.size))
            DispatchQueue.global().async { [weak self] in
                defer { close(c) }
                guard let self else { return }
                guard let peer = PeerInfo.of(fd: c) else { self.onRefuse("a peer whose identity could not be read"); return }
                if let why = self.admit(peer) {
                    self.onRefuse(why)
                    let msg = Array(#"{"error":"not the runner"}\#n"#.utf8)
                    _ = msg.withUnsafeBytes { write(c, $0.baseAddress, $0.count) }
                    return
                }
                SignerPolicy.serve(fd: c, key: self.key, lookup: self.lookup, broker: self.broker, onSign: self.onSign)
            }
        }
    }

    // MARK: SignerEndpoint

    func start() {
        do { try listen() } catch { onRefuse("could not listen: \(error.localizedDescription)") }
    }

    func closeChildEnd() {}

    func stop() {
        lock.lock()
        let fd = listenFD
        listenFD = -1
        lock.unlock()
        if fd >= 0 {
            shutdown(fd, SHUT_RDWR)
            close(fd)
        }
        unlink(path)
    }
}
