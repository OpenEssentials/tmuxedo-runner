import Foundation

// tmuxedo-helper: the privileged helper of tmuxedo for Mac (docs/design/mac-isolation.md §2.1).
//
//   tmuxedo-helper --xpc
//       As the LaunchDaemon `dev.tmuxedo.mac.helper` (SMAppService.daemon, approved once by an
//       administrator). Serves exactly HelperRequest's operations over XPC, only to the
//       tmuxedo app signed by the same team; the owner is the caller's uid.
//
//   sudo tmuxedo-helper <op> --owner <user> [--runner <path>] [--config <file.json>]
//       The same operations from a shell, for the CI abuse tests. ROOT ONLY: it grants
//       nothing a root shell does not already have.
//
//   tmuxedo-helper acl <lock|grant|revoke|unlock> [<folder>] --state <file.json>
//       The owner's side (home lock, grants), with the CALLER's own rights, no root: the
//       same code the app runs (FolderGrants).

func fail(_ s: String) -> Never {
    FileHandle.standardError.write(Data("tmuxedo-helper: \(s)\n".utf8))
    exit(2)
}

func value(_ args: [String], _ flag: String) -> String? {
    guard let i = args.firstIndex(of: flag), i + 1 < args.count else { return nil }
    return args[i + 1]
}

/// The runner bundled next to this helper: Contents/MacOS/tmuxedo-helper → Contents/Helpers/tmuxedo-runner.
func bundledRunner() -> String? {
    let me = URL(fileURLWithPath: CommandLine.arguments[0]).resolvingSymlinksInPath()
    let r = me.deletingLastPathComponent().deletingLastPathComponent().appendingPathComponent("Helpers/tmuxedo-runner")
    return FileManager.default.isExecutableFile(atPath: r.path) ? r.path : nil
}

func printJSON<T: Encodable>(_ v: T) {
    let e = JSONEncoder()
    e.outputFormatting = [.prettyPrinted, .sortedKeys]
    if let d = try? e.encode(v) { print(String(decoding: d, as: UTF8.self)) }
}

final class Listener: NSObject, NSXPCListenerDelegate {
    let team: String?
    init(team: String?) { self.team = team }

    func listener(_ listener: NSXPCListener, shouldAcceptNewConnection c: NSXPCConnection) -> Bool {
        // An unsigned helper (a development build) serves nobody: it cannot check who calls.
        guard team != nil else { return false }
        c.exportedInterface = NSXPCInterface(with: TmuxedoHelperXPC.self)
        c.exportedObject = ConnectionHandler(uid: c.effectiveUserIdentifier, team: team)
        c.resume()
        return true
    }
}

final class ConnectionHandler: NSObject, TmuxedoHelperXPC {
    let uid: uid_t
    let team: String?
    init(uid: uid_t, team: String?) {
        self.uid = uid
        self.team = team
    }

    /// One operation at a time, whoever asks (launchctl and dscl steps must not interleave).
    static let serial = NSLock()

    func handle(_ request: Data, withReply reply: @escaping (Data) -> Void) {
        Self.serial.lock()
        defer { Self.serial.unlock() }
        let resp: HelperResponse
        if let r = try? JSONDecoder().decode(HelperRequest.self, from: request) {
            if let owner = HelperCore.Owner.lookup(uid: uid), uid != 0 {
                resp = HelperCore(owner: owner, runnerSource: bundledRunner(), team: team).handle(r)
            } else {
                resp = .fail("unknown caller")
            }
        } else {
            resp = .fail("bad request")
        }
        reply((try? JSONEncoder().encode(resp)) ?? Data())
    }
}

let args = Array(CommandLine.arguments.dropFirst())
let team = RunnerBinary.appTeam

if args.first == "--xpc" {
    let delegate = Listener(team: team)
    let listener = NSXPCListener(machServiceName: Iso.helperLabel)
    if let team {
        // Only the tmuxedo app of the same team may connect (checked by the kernel's view of its code).
        listener.setConnectionCodeSigningRequirement(Iso.requirement(identifier: "dev.tmuxedo.mac", team: team))
    }
    listener.delegate = delegate
    listener.resume()
    RunLoop.main.run()
    exit(0)
}

if args.first == "acl" {
    // The owner's side, with the caller's own rights.
    guard args.count >= 2, let statePath = value(args, "--state") else {
        fail("usage: tmuxedo-helper acl <lock|grant|revoke|unlock> [<folder>] --state <file.json>")
    }
    guard let user = FolderAccess.uuid(ofUser: Iso.user) else { fail("the user \(Iso.user) does not exist") }
    let me = String(cString: getpwuid(getuid()).pointee.pw_name)
    guard let owner = FolderAccess.uuid(ofUser: me) else { fail("cannot find your own directory uuid") }
    let home = (value(args, "--home") ?? NSHomeDirectory())
    let state = (FileManager.default.contents(atPath: statePath)).flatMap { try? JSONDecoder().decode(GrantState.self, from: $0) }
        ?? GrantState()
    var g = FolderGrants(home: home, user: user, owner: owner, state: state, say: { print($0) })
    do {
        switch args[1] {
        case "lock": try g.lock()
        case "grant":
            guard args.count >= 3 else { fail("grant needs a folder") }
            try g.grant(args[2])
        case "revoke":
            guard args.count >= 3 else { fail("revoke needs a folder") }
            try g.revoke(args[2])
        case "unlock": g.removeAll()
        default: fail("unknown acl command \(args[1])")
        }
    } catch {
        fail(error.localizedDescription)
    }
    if let d = try? JSONEncoder().encode(g.state) { FileManager.default.createFile(atPath: statePath, contents: d) }
    printJSON(g.state)
    exit(0)
}

guard let opName = args.first, let op = HelperRequest.Op(rawValue: opName) else {
    fail("usage: tmuxedo-helper <\(HelperRequest.Op.allCases.map(\.rawValue).joined(separator: "|"))> --owner <user> "
        + "[--runner <path>] [--config <file.json>] | --xpc | acl …")
}
guard getuid() == 0 else { fail("\(opName) needs root (sudo)") }
guard let ownerName = value(args, "--owner"), let owner = HelperCore.Owner.lookup(name: ownerName), owner.uid != 0 else {
    fail("--owner <user> names the Mac's owner (not root)")
}
var cfg: IsoRunnerConfig?
if let c = value(args, "--config") {
    guard let d = FileManager.default.contents(atPath: c), let v = try? JSONDecoder().decode(IsoRunnerConfig.self, from: d) else {
        fail("cannot read the runner settings in \(c)")
    }
    cfg = v
}
let core = HelperCore(owner: owner, runnerSource: value(args, "--runner") ?? bundledRunner(), team: team)
let resp = core.handle(HelperRequest(op, runner: cfg))
printJSON(resp)
exit(resp.ok ? 0 : 1)
