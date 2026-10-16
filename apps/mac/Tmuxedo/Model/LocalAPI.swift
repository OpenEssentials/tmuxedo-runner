import Darwin
import Foundation

/// The runner's local API on this Mac (`<runtime>/runner.sock`, newline-delimited JSON,
/// the same methods as over the gateway: runner/api/runner-api.schema.json).
///
/// The dashboard reads workspaces and agents from HERE, not from tmuxedo's
/// servers: it works offline, needs no user session, and shows exactly what
/// this Mac's runner believes.
struct LocalAPI {
    let socket: URL

    /// `~/.local/state/tmuxedo/runner.sock` (macOS has no XDG_RUNTIME_DIR), or a profile's own.
    static func defaultSocket(runtimeRoot: URL?) -> URL {
        if let r = runtimeRoot { return r.appendingPathComponent("run/runner.sock") }
        if let x = ProcessInfo.processInfo.environment["XDG_RUNTIME_DIR"], !x.isEmpty {
            return URL(fileURLWithPath: x).appendingPathComponent("tmuxedo/runner.sock")
        }
        return FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent(".local/state/tmuxedo/runner.sock")
    }

    /// The holder's socket beside it.
    static func holderSocket(runtimeRoot: URL?) -> URL? {
        runtimeRoot.map { $0.appendingPathComponent("run/holder.sock") }
    }

    enum CallError: Error, Equatable {
        case unavailable(String)
        case refused(code: String, message: String)
    }

    /// One request, one answer, on its own connection. Blocking: call off the main thread.
    func call(_ method: String, _ params: [String: Any] = [:], timeout: TimeInterval = 3) throws -> [String: Any] {
        let fd = Darwin.socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else { throw CallError.unavailable("socket") }
        defer { close(fd) }
        var on: Int32 = 1
        setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &on, socklen_t(MemoryLayout<Int32>.size))
        var tv = timeval(tv_sec: Int(timeout), tv_usec: Int32((timeout - floor(timeout)) * 1e6))
        setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, socklen_t(MemoryLayout<timeval>.size))
        setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &tv, socklen_t(MemoryLayout<timeval>.size))
        var addr = sockaddr_un()
        addr.sun_family = sa_family_t(AF_UNIX)
        let path = Array(socket.path.utf8CString)
        guard path.count <= MemoryLayout.size(ofValue: addr.sun_path) else { throw CallError.unavailable("socket path too long") }
        withUnsafeMutableBytes(of: &addr.sun_path) { dst in
            path.withUnsafeBytes { src in dst.copyMemory(from: UnsafeRawBufferPointer(rebasing: src[0..<path.count])) }
        }
        let rc = withUnsafePointer(to: &addr) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { connect(fd, $0, socklen_t(MemoryLayout<sockaddr_un>.size)) }
        }
        guard rc == 0 else { throw CallError.unavailable("the runner is not running") }
        let req = try JSONSerialization.data(withJSONObject: ["id": "1", "method": method, "params": params]) + Data([0x0A])
        let sent = req.withUnsafeBytes { write(fd, $0.baseAddress, $0.count) }
        guard sent == req.count else { throw CallError.unavailable("write failed") }
        var buf = Data()
        var chunk = [UInt8](repeating: 0, count: 65536)
        while !buf.contains(0x0A) {
            let n = read(fd, &chunk, chunk.count)
            if n <= 0 { break }
            buf.append(contentsOf: chunk[0..<n])
            if buf.count > 8 * 1024 * 1024 { break }
        }
        let line = buf.split(separator: 0x0A, maxSplits: 1, omittingEmptySubsequences: true).first ?? Data()
        guard let obj = try JSONSerialization.jsonObject(with: Data(line)) as? [String: Any] else {
            throw CallError.unavailable("the runner answered junk")
        }
        if let e = obj["error"] as? [String: Any] {
            throw CallError.refused(code: e["code"] as? String ?? "error", message: e["message"] as? String ?? "")
        }
        return obj["result"] as? [String: Any] ?? [:]
    }

    // MARK: What the dashboard shows

    struct Workspace: Identifiable, Equatable {
        let id: String
        let label: String
        let number: Int
        let panes: Int
        let tabs: Int
        let status: AgentStatus
        let focused: Bool
    }

    struct Agent: Identifiable, Equatable {
        let id: String
        let workspaceId: String
        let kind: String
        let name: String?
        let title: String?
        let cwd: String?
        let status: AgentStatus
    }

    enum AgentStatus: String, Equatable, CaseIterable {
        case working, blocked, done, idle, unknown
        init(_ raw: Any?) { self = (raw as? String).flatMap(AgentStatus.init(rawValue:)) ?? .unknown }
        var label: String {
            switch self {
            case .working: return "working"
            case .blocked: return "needs you"
            case .done: return "done"
            case .idle: return "idle"
            case .unknown: return "—"
            }
        }
    }

    struct Snapshot: Equatable {
        var workspaces: [Workspace] = []
        var agents: [Agent] = []
        var needsYou: [Agent] { agents.filter { $0.status == .blocked } }
        static let empty = Snapshot()
    }

    static func parseWorkspaces(_ r: [String: Any]) -> [Workspace] {
        (r["workspaces"] as? [[String: Any]] ?? []).compactMap { w in
            guard let id = w["workspace_id"] as? String else { return nil }
            return Workspace(id: id, label: w["label"] as? String ?? id, number: w["number"] as? Int ?? 0,
                             panes: w["pane_count"] as? Int ?? 0, tabs: w["tab_count"] as? Int ?? 0,
                             status: AgentStatus(w["agent_status"]), focused: w["focused"] as? Bool ?? false)
        }
    }

    static func parseAgents(_ r: [String: Any]) -> [Agent] {
        (r["agents"] as? [[String: Any]] ?? []).compactMap { a in
            guard let id = a["pane_id"] as? String else { return nil }
            return Agent(id: id, workspaceId: a["workspace_id"] as? String ?? "", kind: a["agent"] as? String ?? "agent",
                         name: a["name"] as? String, title: a["title"] as? String, cwd: a["cwd"] as? String,
                         status: AgentStatus(a["agent_status"]))
        }
    }

    func snapshot() throws -> Snapshot {
        Snapshot(workspaces: Self.parseWorkspaces(try call("workspace.list")),
                 agents: Self.parseAgents(try call("agent.list")))
    }
}
