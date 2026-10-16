import Darwin
import Foundation

/// What the app will sign for the runner (docs/design/mac-pairing.md §3.2).
///
/// The runner never holds the key. When the gateway challenges it, it asks
/// the app over the inherited signer fd. The app signs ONLY a runner proof
/// (`tmuxedo-runner-proof-v1\n<nonce>\n<lookup>`) naming THIS Mac's own token,
/// with a nonce shaped like the gateway's. Anything else is refused, so the fd
/// is not a general signing oracle even for a compromised runner.
enum SignerPolicy {
    /// SSH keys this Mac holds for the runner's ssh-agent (tests swap in a MemoryStore).
    static var sshKeys = SshKeys(store: KeychainStore())

    static func allows(_ input: Data, ownLookup: String) -> Bool {
        guard let text = String(data: input, encoding: .utf8) else { return false }
        let lines = text.split(separator: "\n", omittingEmptySubsequences: false)
        guard lines.count == 3, lines[0] == Substring(SigningInput.runnerProofDomain), lines[2] == Substring(ownLookup) else { return false }
        let nonce = lines[1]
        return (16...128).contains(nonce.count) && nonce.unicodeScalars.allSatisfy {
            CharacterSet.alphanumerics.contains($0) && $0.isASCII || $0 == "-" || $0 == "_"
        }
    }

    /// One request line → one answer line (without the newline).
    ///
    /// Besides `sign`, the runner may ask the person at this Mac (`confirm`, `confirm_poll`;
    /// ConfirmBroker) when confirm-on-Mac is on. Without a broker those are bad requests, and
    /// the runner then denies (it fails closed).
    static func answer(_ line: String, key: MachineKey, ownLookup: String, broker: ConfirmBroker? = nil) -> String {
        guard let data = line.data(using: .utf8),
              let req = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let op = req["op"] as? String
        else { return #"{"error":"bad request"}"# }
        switch op {
        case "confirm":
            guard let broker, let pane = req["pane"] as? String, let what = req["what"] as? String,
                  let timeout = (req["timeout"] as? NSNumber)?.doubleValue
            else { return #"{"error":"bad request"}"# }
            switch broker.ask(pane: pane, what: what, timeout: timeout) {
            case let .success(ticket): return json(["ticket": ticket])
            case let .failure(e): return json(["error": e.message])
            }
        case "confirm_poll":
            guard let broker, let ticket = req["ticket"] as? String else { return #"{"error":"bad request"}"# }
            guard let state = broker.poll(ticket) else { return #"{"error":"unknown ticket"}"# }
            return json(["state": state.rawValue])
        case "sign":
            break
        case "ssh_create", "ssh_sign":
            // The runner's ssh-agent (SshKeys): Secure Enclave keys that never leave this Mac.
            return sshKeys.answer(req)
        default:
            return #"{"error":"bad request"}"#
        }
        guard let b64 = req["input"] as? String, let input = Data(base64Encoded: b64)
        else { return #"{"error":"bad request"}"# }
        guard allows(input, ownLookup: ownLookup) else { return #"{"error":"not a runner proof for this machine"}"# }
        guard let sig = try? key.sign(input) else { return #"{"error":"the key could not sign"}"# }
        return #"{"sig":""# + sig.base64URL + #""}"#
    }

    static func json(_ o: [String: String]) -> String {
        (try? JSONSerialization.data(withJSONObject: o, options: [.sortedKeys]))
            .flatMap { String(data: $0, encoding: .utf8) } ?? #"{"error":"internal"}"#
    }

    /// Serve one connection or inherited fd: read lines, answer each, until EOF or a write fails.
    static func serve(fd: Int32, key: MachineKey, lookup: String, broker: ConfirmBroker?, onSign: (Bool) -> Void) {
        var buffer = Data()
        var chunk = [UInt8](repeating: 0, count: 4096)
        while true {
            let n = read(fd, &chunk, chunk.count)
            if n <= 0 { return }
            buffer.append(contentsOf: chunk[0..<n])
            while let nl = buffer.firstIndex(of: 0x0A) {
                let line = String(decoding: buffer[buffer.startIndex..<nl], as: UTF8.self)
                buffer.removeSubrange(buffer.startIndex...nl)
                let answer = SignerPolicy.answer(line, key: key, ownLookup: lookup, broker: broker)
                if answer.hasPrefix(#"{"sig""#) { onSign(true) } else if line.contains(#""sign""#) { onSign(false) }
                let out = Array((answer + "\n").utf8)
                var off = 0
                while off < out.count {
                    let w = out[off...].withUnsafeBytes { write(fd, $0.baseAddress, $0.count) }
                    if w <= 0 { return }
                    off += w
                }
            }
            if buffer.count > 64 * 1024 { buffer.removeAll() }
        }
    }
}

/// The app's end of the runner's signer: an inherited socketpair (full access, the runner is
/// the app's child) or a listening socket (separate user, the runner is a LaunchDaemon).
protocol SignerEndpoint: AnyObject {
    func start()
    /// The child has its copy now (socketpair only).
    func closeChildEnd()
    func stop()
}

/// The app's end of the socketpair the runner inherits as `TMUXEDO_SIGNER_FD`.
///
/// No file on disk: only the runner child holds the other end. One request at
/// a time, answered on a background thread; every signature is counted so the
/// dashboard can show "signed N proofs".
final class SignerChannel: SignerEndpoint {
    /// The fd to hand the child (dup2'd to 3 at spawn; close it in the parent afterwards).
    let childFD: Int32
    private let parentFD: Int32
    private let key: MachineKey
    private let lookup: String
    private let onSign: (Bool) -> Void
    private let broker: ConfirmBroker?
    private var thread: Thread?

    init(key: MachineKey, lookup: String, broker: ConfirmBroker? = nil, onSign: @escaping (Bool) -> Void = { _ in }) throws {
        var fds: [Int32] = [0, 0]
        guard socketpair(AF_UNIX, SOCK_STREAM, 0, &fds) == 0 else {
            throw NSError(domain: NSPOSIXErrorDomain, code: Int(errno), userInfo: [NSLocalizedDescriptionKey: "socketpair: \(String(cString: strerror(errno)))"])
        }
        parentFD = fds[0]
        childFD = fds[1]
        // The parent's end must never leak into any child (the runner gets only fd 3).
        _ = fcntl(parentFD, F_SETFD, FD_CLOEXEC)
        _ = fcntl(childFD, F_SETFD, FD_CLOEXEC)
        var on: Int32 = 1
        _ = setsockopt(parentFD, SOL_SOCKET, SO_NOSIGPIPE, &on, socklen_t(MemoryLayout<Int32>.size))
        self.key = key
        self.lookup = lookup
        self.onSign = onSign
        self.broker = broker
    }

    func start() {
        let t = Thread { [parentFD, key, lookup, onSign, broker] in
            SignerPolicy.serve(fd: parentFD, key: key, lookup: lookup, broker: broker, onSign: onSign)
        }
        t.name = "tmuxedo-signer"
        thread = t
        t.start()
    }

    /// The child has its copy now.
    func closeChildEnd() { close(childFD) }

    /// Ends the serving thread (its read returns 0 once both ends are closed).
    func stop() {
        shutdown(parentFD, SHUT_RDWR)
        close(parentFD)
    }
}
