import Foundation

/// Moving a Mac that was set up with `install.sh` (or `tmuxedo-runner enrol`) onto the app,
/// WITHOUT ending a terminal session.
///
/// What install.sh left behind (runner/crates/runner/src/service.rs):
///   · `~/Library/LaunchAgents/dev.tmuxedo.runner.plist` and `dev.tmuxedo.holder.plist`
///     (and, older, `ai.aimux.runner.plist` / `ai.aimux.holder.plist`);
///   · `~/.config/tmuxedo/runner.toml` with the server and a BEARER token.
///
/// The plan, the same one `install-service` uses for the aimux → tmuxedo move:
///   · the old RUNNER is stopped and its plist removed: it holds no sessions, and two
///     runners for one machine would replace each other (4001) for ever;
///   · the old HOLDER is NOT stopped: it keeps every PTY. Its plist is removed so it never
///     starts again; the app's holder (`holder --wait`) stands by and takes over when the
///     old one exits (at the latest at logout). Meanwhile the app's runner uses the old
///     holder, which listens on the same socket;
///   · an existing enrolment can be ADOPTED (its bearer token moves into the Keychain and
///     runner.toml loses it), or replaced by a key-bound pairing (recommended).
struct Migration: Equatable {
    struct Found: Equatable {
        var runnerPlists: [URL] = []
        var holderPlists: [URL] = []
        var enrolment: Enrolment?
        var isEmpty: Bool { runnerPlists.isEmpty && holderPlists.isEmpty && enrolment == nil }
    }

    struct Enrolment: Equatable {
        var server: String
        var token: String
        var name: String?
        var configPath: URL
    }

    enum Step: Equatable {
        /// `launchctl bootout gui/<uid> <plist>` (failure ignored: it may not be loaded).
        case bootout(URL)
        case remove(URL)
        case say(String)
    }

    static let runnerLabels = ["dev.tmuxedo.runner", "ai.aimux.runner"]
    static let holderLabels = ["dev.tmuxedo.holder", "ai.aimux.holder"]

    static func scan(home: URL = FileManager.default.homeDirectoryForCurrentUser) -> Found {
        let fm = FileManager.default
        let agents = home.appendingPathComponent("Library/LaunchAgents")
        var f = Found()
        for l in runnerLabels {
            let p = agents.appendingPathComponent("\(l).plist")
            if fm.fileExists(atPath: p.path) { f.runnerPlists.append(p) }
        }
        for l in holderLabels {
            let p = agents.appendingPathComponent("\(l).plist")
            if fm.fileExists(atPath: p.path) { f.holderPlists.append(p) }
        }
        for dir in ["tmuxedo", "aimux"] {
            let cfg = home.appendingPathComponent(".config/\(dir)/runner.toml")
            if let text = try? String(contentsOf: cfg, encoding: .utf8), let e = parseRunnerToml(text, path: cfg) {
                f.enrolment = e
                break
            }
        }
        return f
    }

    /// The two keys the app needs from runner.toml (a strict subset of TOML: `key = "value"` lines).
    static func parseRunnerToml(_ text: String, path: URL) -> Enrolment? {
        var server: String?, token: String?, name: String?
        for raw in text.split(whereSeparator: \.isNewline) {
            let line = raw.trimmingCharacters(in: .whitespaces)
            guard !line.hasPrefix("#"), let eq = line.firstIndex(of: "=") else { continue }
            let k = line[..<eq].trimmingCharacters(in: .whitespaces)
            var v = line[line.index(after: eq)...].trimmingCharacters(in: .whitespaces)
            if (v.hasPrefix("\"") && v.hasSuffix("\"")) || (v.hasPrefix("'") && v.hasSuffix("'")), v.count >= 2 {
                v = String(v.dropFirst().dropLast())
            } else { continue }
            switch k {
            case "server": server = v
            case "token": token = v
            case "name": name = v
            default: break
            }
        }
        guard let server, let token, runnerTokenLookup(token) != nil else { return nil }
        return Enrolment(server: server, token: token, name: name, configPath: path)
    }

    /// The steps, in order. Nothing stops, restarts or kills an old holder.
    static func plan(_ f: Found) -> [Step] {
        var s: [Step] = []
        for p in f.runnerPlists {
            s.append(.bootout(p))
            s.append(.remove(p))
        }
        for p in f.holderPlists {
            s.append(.remove(p))
        }
        if !f.holderPlists.isEmpty {
            s.append(.say("Your current terminals stay in the old holder until it exits (at the latest when you log out); "
                          + "tmuxedo's own holder takes over then."))
        }
        return s
    }

    /// Runs the plan. `launchctl` is injectable for tests.
    static func execute(_ steps: [Step], launchctl: ([String]) -> Int32, say: (String) -> Void) {
        for step in steps {
            switch step {
            case let .bootout(p): _ = launchctl(["bootout", "gui/\(getuid())", p.path])
            case let .remove(p): try? FileManager.default.removeItem(at: p)
            case let .say(m): say(m)
            }
        }
    }

    /// Adopting an enrolment: the token goes to the Keychain and is removed from the file
    /// (the server, name and the rest stay, for the CLI).
    static func scrubToken(from config: URL) throws {
        let text = try String(contentsOf: config, encoding: .utf8)
        let kept = text.split(separator: "\n", omittingEmptySubsequences: false)
            .filter { !$0.trimmingCharacters(in: .whitespaces).hasPrefix("token") }
            .joined(separator: "\n")
        try kept.write(to: config, atomically: true, encoding: .utf8)
    }
}
