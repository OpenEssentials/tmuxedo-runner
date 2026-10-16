import Darwin
import Foundation

/// What the `tmuxedo` user may reach of the OWNER's files: the home lock and the folder
/// grants (docs/design/mac-isolation.md §2.1–2.2). Runs AS THE OWNER, never as root: the
/// owner owns these files and may set their ACLs, so no privilege is involved and a grant
/// can never reach anything the owner could not already change.
///
/// WHY A HOME LOCK. A standard macOS user is a member of `staff` (through `localaccounts`),
/// and home folders are `drwxr-x--- owner:staff`: without an ACL, `tmuxedo` could walk into
/// the owner's home and read every file that is readable by group or others, which is most of
/// them (`~/git` is 0755 by default). So:
///
///   no grant inside the home   the home denies `tmuxedo` everything, including passing through.
///   grants inside the home     each folder on the way to a grant lets `tmuxedo` pass through
///                              (search) but not list or write; every OTHER entry in those
///                              folders denies `tmuxedo` everything, and new entries inherit
///                              that deny. So only the granted folders are reachable.
///   a granted folder           `tmuxedo` may read and write it, recursively, and the owner gets
///                              an inherited allow too (files an agent creates stay the owner's
///                              to edit). Applied through file descriptors opened without
///                              following symlinks; hard-linked files are skipped (a link
///                              planted by a terminal must not carry the grant elsewhere).
///   a revoked folder           `tmuxedo` is denied the folder itself, so nothing beneath it
///                              (even files it created there) stays reachable.
///
/// Every non-recursive entry this writes is recorded in a manifest so it can all be undone.
enum ACLRights {
    /// Everything on a directory.
    static let dirAll: [acl_perm_t] = [
        ACL_LIST_DIRECTORY, ACL_SEARCH, ACL_ADD_FILE, ACL_ADD_SUBDIRECTORY, ACL_DELETE_CHILD, ACL_DELETE,
        ACL_READ_ATTRIBUTES, ACL_WRITE_ATTRIBUTES, ACL_READ_EXTATTRIBUTES, ACL_WRITE_EXTATTRIBUTES,
        ACL_READ_SECURITY, ACL_WRITE_SECURITY, ACL_CHANGE_OWNER,
    ]
    /// Everything on a file.
    static let fileAll: [acl_perm_t] = [
        ACL_READ_DATA, ACL_WRITE_DATA, ACL_APPEND_DATA, ACL_EXECUTE, ACL_DELETE,
        ACL_READ_ATTRIBUTES, ACL_WRITE_ATTRIBUTES, ACL_READ_EXTATTRIBUTES, ACL_WRITE_EXTATTRIBUTES,
        ACL_READ_SECURITY, ACL_WRITE_SECURITY, ACL_CHANGE_OWNER,
    ]
    /// A granted folder (and every folder in it).
    static let dirGrant: [acl_perm_t] = [
        ACL_LIST_DIRECTORY, ACL_SEARCH, ACL_ADD_FILE, ACL_ADD_SUBDIRECTORY, ACL_DELETE_CHILD,
        ACL_READ_ATTRIBUTES, ACL_WRITE_ATTRIBUTES, ACL_READ_EXTATTRIBUTES, ACL_WRITE_EXTATTRIBUTES, ACL_READ_SECURITY,
    ]
    /// A file in a granted folder.
    static let fileGrant: [acl_perm_t] = [
        ACL_READ_DATA, ACL_WRITE_DATA, ACL_APPEND_DATA, ACL_EXECUTE, ACL_DELETE,
        ACL_READ_ATTRIBUTES, ACL_WRITE_ATTRIBUTES, ACL_READ_EXTATTRIBUTES, ACL_WRITE_EXTATTRIBUTES, ACL_READ_SECURITY,
    ]
    /// Pass through a folder on the way to a grant, without listing or changing it.
    static let passDeny: [acl_perm_t] = [
        ACL_LIST_DIRECTORY, ACL_ADD_FILE, ACL_ADD_SUBDIRECTORY, ACL_DELETE_CHILD, ACL_DELETE,
        ACL_WRITE_ATTRIBUTES, ACL_WRITE_EXTATTRIBUTES, ACL_WRITE_SECURITY, ACL_CHANGE_OWNER,
    ]
    static let inherit: [acl_flag_t] = [ACL_ENTRY_FILE_INHERIT, ACL_ENTRY_DIRECTORY_INHERIT]
    /// New entries directly in this folder inherit it; the folder itself does not get it.
    static let newChildren: [acl_flag_t] = [ACL_ENTRY_FILE_INHERIT, ACL_ENTRY_DIRECTORY_INHERIT,
                                            ACL_ENTRY_LIMIT_INHERIT, ACL_ENTRY_ONLY_INHERIT]
}

/// One ACL entry to write.
struct ACE: Equatable {
    var allow: Bool
    var qualifier: uuid_t
    var perms: [acl_perm_t]
    var flags: [acl_flag_t] = []

    static func == (a: ACE, b: ACE) -> Bool {
        a.allow == b.allow && FolderAccess.same(a.qualifier, b.qualifier)
            && a.perms.map(\.rawValue).sorted() == b.perms.map(\.rawValue).sorted()
            && a.flags.map(\.rawValue).sorted() == b.flags.map(\.rawValue).sorted()
    }
}

enum FolderAccess {
    enum Failure: Error, LocalizedError, Equatable {
        case refused(String)
        case posix(String, Int32)
        var errorDescription: String? {
            switch self {
            case let .refused(why): return why
            case let .posix(what, e): return "\(what): \(String(cString: strerror(e)))"
            }
        }
    }

    static func same(_ a: uuid_t, _ b: uuid_t) -> Bool {
        withUnsafeBytes(of: a) { x in withUnsafeBytes(of: b) { y in x.elementsEqual(y) } }
    }

    /// A user's (or group's) directory UUID, the ACL qualifier.
    static func uuid(ofUser name: String) -> uuid_t? {
        guard let (rc, out) = Shell.run("/usr/bin/dsmemberutil", ["getuuid", "-U", name]), rc == 0 else { return nil }
        return UUID(uuidString: out.trimmingCharacters(in: .whitespacesAndNewlines))?.uuid
    }

    // MARK: - Low level: one descriptor

    /// Add `entries` at the front of the ACL of the open file `fd` (deny entries must come first).
    static func add(_ entries: [ACE], fd: Int32) throws {
        var acl: acl_t? = acl_get_fd_np(fd, ACL_TYPE_EXTENDED)
        if acl == nil { acl = acl_init(Int32(entries.count)) }
        guard let first = acl else { throw Failure.posix("acl_init", errno) }
        defer { if let a = acl { acl_free(UnsafeMutableRawPointer(a)) } }
        let existing = read(first)
        let missing = entries.filter { !existing.contains($0) }
        if missing.isEmpty { return }
        for e in missing.reversed() {
            var entry: acl_entry_t?
            guard acl_create_entry_np(&acl, &entry, 0) == 0, let entry else { throw Failure.posix("acl_create_entry", errno) }
            acl_set_tag_type(entry, e.allow ? ACL_EXTENDED_ALLOW : ACL_EXTENDED_DENY)
            var q = e.qualifier
            _ = withUnsafePointer(to: &q) { acl_set_qualifier(entry, $0) }
            var perms: acl_permset_t?
            acl_get_permset(entry, &perms)
            if let perms {
                acl_clear_perms(perms)
                for p in e.perms { acl_add_perm(perms, p) }
                acl_set_permset(entry, perms)
            }
            var flags: acl_flagset_t?
            acl_get_flagset_np(UnsafeMutableRawPointer(entry), &flags)
            if let flags {
                acl_clear_flags_np(flags)
                for f in e.flags { acl_add_flag_np(flags, f) }
                acl_set_flagset_np(UnsafeMutableRawPointer(entry), flags)
            }
        }
        guard acl_set_fd_np(fd, acl, ACL_TYPE_EXTENDED) == 0 else { throw Failure.posix("acl_set_fd", errno) }
    }

    /// The entries of an ACL (allow/deny with a qualifier; others are ignored).
    static func read(_ acl: acl_t) -> [ACE] {
        var out: [ACE] = []
        var entry: acl_entry_t?
        var id: Int32 = 0 // ACL_FIRST_ENTRY, then ACL_NEXT_ENTRY (-1)
        while acl_get_entry(acl, id, &entry) == 0, let e = entry {
            id = -1
            var tag = acl_tag_t(rawValue: 0)
            acl_get_tag_type(e, &tag)
            guard tag == ACL_EXTENDED_ALLOW || tag == ACL_EXTENDED_DENY, let qp = acl_get_qualifier(e) else { continue }
            let q = qp.assumingMemoryBound(to: uuid_t.self).pointee
            acl_free(qp)
            let ps = permsOf(e, ACLRights.dirAll + ACLRights.fileAll)
            let fs = flagsOf(e, [ACL_ENTRY_FILE_INHERIT, ACL_ENTRY_DIRECTORY_INHERIT, ACL_ENTRY_LIMIT_INHERIT,
                                 ACL_ENTRY_ONLY_INHERIT, ACL_ENTRY_INHERITED])
            var uniq: [acl_perm_t] = []
            for p in ps where !uniq.contains(where: { $0.rawValue == p.rawValue }) { uniq.append(p) }
            out.append(ACE(allow: tag == ACL_EXTENDED_ALLOW, qualifier: q, perms: uniq, flags: fs))
        }
        return out
    }

    /// Which of `candidates` an entry's permission set holds.
    static func permsOf(_ e: acl_entry_t, _ candidates: [acl_perm_t]) -> [acl_perm_t] {
        var ps: acl_permset_t?
        guard acl_get_permset(e, &ps) == 0, let p = ps else { return [] }
        return candidates.filter { acl_get_perm_np(p, $0) == 1 }
    }

    /// Which of `candidates` an entry's flag set holds.
    static func flagsOf(_ e: acl_entry_t, _ candidates: [acl_flag_t]) -> [acl_flag_t] {
        var fl: acl_flagset_t?
        guard acl_get_flagset_np(UnsafeMutableRawPointer(e), &fl) == 0, let f = fl else { return [] }
        return candidates.filter { acl_get_flag_np(f, $0) == 1 }
    }

    static func entries(fd: Int32) -> [ACE] {
        guard let a = acl_get_fd_np(fd, ACL_TYPE_EXTENDED) else { return [] }
        defer { acl_free(UnsafeMutableRawPointer(a)) }
        return read(a)
    }

    /// Remove every entry naming `qualifier` (and, if given, the owner entries `alsoOwner` matches).
    static func remove(qualifier: uuid_t, fd: Int32, alsoOwner: ((ACE) -> Bool)? = nil) throws {
        guard let a = acl_get_fd_np(fd, ACL_TYPE_EXTENDED) else { return }
        defer { acl_free(UnsafeMutableRawPointer(a)) }
        var removed = false
        var entry: acl_entry_t?
        var id: Int32 = 0
        var doomed: [acl_entry_t] = []
        while acl_get_entry(a, id, &entry) == 0, let e = entry {
            id = -1
            guard let qp = acl_get_qualifier(e) else { continue }
            let q = qp.assumingMemoryBound(to: uuid_t.self).pointee
            acl_free(qp)
            if same(q, qualifier) {
                doomed.append(e)
            } else if let alsoOwner {
                var tag = acl_tag_t(rawValue: 0)
                acl_get_tag_type(e, &tag)
                let ps = permsOf(e, ACLRights.dirGrant)
                let fs = flagsOf(e, ACLRights.inherit)
                if alsoOwner(ACE(allow: tag == ACL_EXTENDED_ALLOW, qualifier: q, perms: ps, flags: fs)) { doomed.append(e) }
            }
        }
        // Last first: deleting an entry shifts the ones after it, which would leave the
        // collected handles of later entries pointing at the wrong ones.
        for e in doomed.reversed() {
            if acl_delete_entry(a, e) == 0 { removed = true }
        }
        if removed, acl_set_fd_np(fd, a, ACL_TYPE_EXTENDED) != 0 { throw Failure.posix("acl_set_fd", errno) }
    }

    // MARK: - Opening without following links

    /// Open `path` component by component, refusing any symlink on the way.
    static func openNoFollow(_ path: String, directory: Bool) throws -> Int32 {
        guard path.hasPrefix("/") else { throw Failure.refused("\(path) is not an absolute path") }
        var fd = open("/", O_RDONLY | O_DIRECTORY | O_CLOEXEC)
        guard fd >= 0 else { throw Failure.posix("open /", errno) }
        let parts = path.split(separator: "/").map(String.init)
        for (i, p) in parts.enumerated() {
            if p == "." || p == ".." { close(fd); throw Failure.refused("\(path) is not a plain path") }
            let last = i == parts.count - 1
            let flags = O_RDONLY | O_NOFOLLOW | O_CLOEXEC | O_NONBLOCK | ((last && !directory) ? 0 : O_DIRECTORY)
            let next = openat(fd, p, flags)
            let e = errno
            close(fd)
            guard next >= 0 else { throw Failure.posix("open \(path)", e) }
            fd = next
        }
        return fd
    }

    /// Walk a folder's tree through descriptors: `visit(fd, isDirectory)` for the folder and
    /// everything in it that is a real directory or a singly-linked regular file. Symlinks,
    /// devices, sockets and hard-linked files are never visited. Returns how many were visited.
    @discardableResult
    static func walk(_ path: String, maxEntries: Int = 1_000_000, visit: @escaping (Int32, Bool) -> Void) throws -> Int {
        let root = try openNoFollow(path, directory: true)
        var count = 0
        func descend(_ dfd: Int32, depth: Int) {
            visit(dfd, true)
            count += 1
            guard depth < 128, count < maxEntries else { return }
            let dupfd = dup(dfd)
            guard dupfd >= 0, let dir = fdopendir(dupfd) else { if dupfd >= 0 { close(dupfd) }; return }
            defer { closedir(dir) }
            while let ent = readdir(dir) {
                let name = withUnsafePointer(to: &ent.pointee.d_name) {
                    String(cString: UnsafeRawPointer($0).assumingMemoryBound(to: CChar.self))
                }
                if name == "." || name == ".." { continue }
                let fd = openat(dfd, name, O_RDONLY | O_NOFOLLOW | O_CLOEXEC | O_NONBLOCK)
                guard fd >= 0 else { continue } // a symlink (ELOOP) or unreadable: skipped
                var st = stat()
                guard fstat(fd, &st) == 0 else { close(fd); continue }
                switch st.st_mode & S_IFMT {
                case S_IFDIR:
                    descend(fd, depth: depth + 1)
                case S_IFREG where st.st_nlink == 1:
                    visit(fd, false)
                    count += 1
                default:
                    break
                }
                close(fd)
                if count >= maxEntries { return }
            }
        }
        descend(root, depth: 0)
        close(root)
        return count
    }

    // MARK: - Grants

    /// Give `user` read and write on `folder`, recursively (and the owner an inherited allow).
    static func grant(_ folder: String, user: uuid_t, owner: uuid_t) throws -> Int {
        try walk(folder) { fd, isDir in
            let mine = isDir
                ? [ACE(allow: true, qualifier: user, perms: ACLRights.dirGrant, flags: ACLRights.inherit),
                   ACE(allow: true, qualifier: owner, perms: ACLRights.dirGrant, flags: ACLRights.inherit)]
                : [ACE(allow: true, qualifier: user, perms: ACLRights.fileGrant)]
            try? add(mine, fd: fd) // an entry the owner does not own (created by a terminal) cannot be changed: skipped
        }
    }

    /// Take every entry for `user` off `folder`'s tree (and the owner's inherited allow we added).
    static func ungrant(_ folder: String, user: uuid_t, owner: uuid_t) throws -> Int {
        try walk(folder) { fd, _ in
            try? remove(qualifier: user, fd: fd, alsoOwner: { e in
                e.allow && same(e.qualifier, owner) && e.perms.count == ACLRights.dirGrant.count
                    && e.flags.count == ACLRights.inherit.count
            })
        }
    }

    /// Put `entries` on one path (no recursion), opened without following links.
    static func set(_ entries: [ACE], on path: String) throws {
        var st = stat()
        guard lstat(path, &st) == 0 else { throw Failure.posix("stat \(path)", errno) }
        let isDir = st.st_mode & S_IFMT == S_IFDIR
        guard isDir || st.st_mode & S_IFMT == S_IFREG else { return } // symlinks etc.: nothing to do
        let fd = try openNoFollow(path, directory: isDir)
        defer { close(fd) }
        try add(entries, fd: fd)
    }

    static func clear(_ user: uuid_t, on path: String) {
        var st = stat()
        guard lstat(path, &st) == 0 else { return }
        let isDir = st.st_mode & S_IFMT == S_IFDIR
        guard isDir || st.st_mode & S_IFMT == S_IFREG, let fd = try? openNoFollow(path, directory: isDir) else { return }
        defer { close(fd) }
        try? remove(qualifier: user, fd: fd)
    }
}

/// The plan for the owner's home and the folders on the way to grants: which single entries
/// to put where. Pure, so the tests check it without touching a disk.
struct HomeLockPlan: Equatable {
    struct Item: Equatable {
        let path: String
        let entries: [ACE]
    }
    var items: [Item]

    /// `home`: the owner's home. `grants`: absolute, standardised folders. `revoked`: folders
    /// revoked and not granted again. `list(dir)`: the names in a directory (lstat-level).
    static func make(home: String, grants: [String], revoked: [String], user: uuid_t,
                     isDirectory: (String) -> Bool, list: (String) -> [String]) -> HomeLockPlan {
        var items: [Item] = []
        let inHome = grants.filter { $0.hasPrefix(home + "/") }
        if inHome.isEmpty {
            items.append(Item(path: home, entries: [ACE(allow: false, qualifier: user, perms: ACLRights.dirAll)]))
        } else {
            // Every folder strictly between the home (included) and a grant (excluded).
            var ancestors: [String] = []
            for g in inHome {
                var p = (g as NSString).deletingLastPathComponent
                var chain: [String] = []
                while p.count >= home.count {
                    chain.append(p)
                    if p == home { break }
                    p = (p as NSString).deletingLastPathComponent
                }
                for a in chain.reversed() where !ancestors.contains(a) { ancestors.append(a) }
            }
            let onPath = Set(ancestors + inHome)
            for a in ancestors {
                items.append(Item(path: a, entries: [
                    ACE(allow: false, qualifier: user, perms: ACLRights.passDeny),
                    ACE(allow: false, qualifier: user, perms: ACLRights.dirAll + ACLRights.fileAll.filter { p in
                        !ACLRights.dirAll.contains(where: { $0.rawValue == p.rawValue })
                    }, flags: ACLRights.newChildren),
                    ACE(allow: true, qualifier: user, perms: [ACL_SEARCH]),
                ]))
                for name in list(a).sorted() {
                    let child = a + "/" + name
                    if onPath.contains(child) { continue }
                    let perms = isDirectory(child) ? ACLRights.dirAll : ACLRights.fileAll
                    items.append(Item(path: child, entries: [ACE(allow: false, qualifier: user, perms: perms)]))
                }
            }
        }
        for r in revoked where !grants.contains(r) {
            items.append(Item(path: r, entries: [ACE(allow: false, qualifier: user, perms: ACLRights.dirAll)]))
        }
        return HomeLockPlan(items: items)
    }
}

/// The owner's grants and everything written for them; persisted by the caller (the app in its
/// settings, the helper's command line in a file).
struct GrantState: Codable, Equatable {
    var grants: [String] = []
    var revoked: [String] = []
    /// Single paths that carry entries from the home lock (cleared before every reconcile).
    var manifest: [String] = []
    /// The home lock is on (separate-user mode is set up).
    var locked = false
}

/// Grants, revokes and the home lock, as the owner. Every method leaves the disk matching `state`.
struct FolderGrants {
    let home: String
    let user: uuid_t
    let owner: uuid_t
    var state: GrantState
    /// Notes for the log.
    var say: (String) -> Void = { _ in }

    static func standardise(_ p: String) -> String {
        (URL(fileURLWithPath: p).resolvingSymlinksInPath().standardizedFileURL.path as NSString).standardizingPath
    }

    static func ownerOf(_ path: String) -> uid_t? {
        var st = stat()
        return lstat(path, &st) == 0 ? st.st_uid : nil
    }

    static func isDir(_ path: String) -> Bool {
        var st = stat()
        return lstat(path, &st) == 0 && st.st_mode & S_IFMT == S_IFDIR
    }

    static func list(_ dir: String) -> [String] {
        (try? FileManager.default.contentsOfDirectory(atPath: dir)) ?? []
    }

    /// Why `path` cannot be granted by the user `uid`, or nil.
    func refusal(_ path: String, uid: uid_t) -> String? {
        GrantPolicy.refusal(path, home: home, owns: { Self.ownerOf($0) == uid }, isDirectory: Self.isDir,
                            resolve: Self.standardise)
    }

    mutating func grant(_ raw: String, uid: uid_t = getuid()) throws {
        if let why = refusal(raw, uid: uid) { throw FolderAccess.Failure.refused(why) }
        let path = Self.standardise(raw)
        // A folder inside (or around) an existing grant: the outer grant decides.
        if state.grants.contains(where: { path == $0 || path.hasPrefix($0 + "/") }) { return }
        state.revoked.removeAll { $0 == path }
        // A deny the lock or a revoke put on this very folder (or that it inherited) goes first.
        FolderAccess.clear(user, on: path)
        state.grants.append(path)
        try reconcile()
        let n = try FolderAccess.grant(path, user: user, owner: owner)
        say("granted \(path) to the terminals (\(n) items)")
    }

    mutating func revoke(_ raw: String) throws {
        let path = Self.standardise(raw)
        guard state.grants.contains(path) else { return }
        let n = (try? FolderAccess.ungrant(path, user: user, owner: owner)) ?? 0
        state.grants.removeAll { $0 == path }
        if !state.revoked.contains(path) { state.revoked.append(path) }
        try reconcile()
        say("revoked \(path) (\(n) items)")
    }

    /// The home lock and the revoked-folder denies, rewritten from `state`.
    mutating func reconcile() throws {
        clearManifest()
        guard state.locked else { return }
        let plan = HomeLockPlan.make(home: home, grants: state.grants, revoked: state.revoked, user: user,
                                     isDirectory: Self.isDir, list: Self.list)
        var written: [String] = []
        var firstError: Error?
        for item in plan.items {
            do {
                try FolderAccess.set(item.entries, on: item.path)
                written.append(item.path)
            } catch {
                // An entry the owner does not own cannot carry the lock; not fatal for the others.
                if item.path == home { firstError = firstError ?? error }
                say("could not protect \(item.path): \(error.localizedDescription)")
            }
        }
        state.manifest = written
        if let firstError { throw firstError }
    }

    /// Every entry the lock wrote, and those its inheritable entries put on newer children.
    mutating func clearManifest() {
        let granted = Set(state.grants)
        for p in state.manifest where !granted.contains(p) {
            FolderAccess.clear(user, on: p)
            if Self.isDir(p) {
                for c in Self.list(p) where !granted.contains(p + "/" + c) { FolderAccess.clear(user, on: p + "/" + c) }
            }
        }
        state.manifest = []
    }

    /// Turn the lock on (separate-user mode set up).
    mutating func lock() throws {
        state.locked = true
        try reconcile()
    }

    /// Everything off: every grant, the lock, the revoked denies (uninstall).
    mutating func removeAll() {
        for g in state.grants { _ = try? FolderAccess.ungrant(g, user: user, owner: owner) }
        clearManifest()
        for r in state.revoked { FolderAccess.clear(user, on: r) }
        state = GrantState()
    }
}

/// Why a folder cannot be granted, or nil. `owns(path)`: the owner owns it.
enum GrantPolicy {
    static func refusal(_ raw: String, home: String, owns: (String) -> Bool, isDirectory: (String) -> Bool,
                        resolve: (String) -> String) -> String? {
        let path = resolve(raw)
        guard path.hasPrefix("/"), !path.contains("\0") else { return "Choose a folder by its full path." }
        guard isDirectory(path) else { return "\(path) is not a folder." }
        if Iso.neverGrant.contains(path) || path == home {
            return "\(path) cannot be granted: it would open up far more than a project. Grant a project folder."
        }
        for system in ["/System/", "/Library/", "/usr/", "/bin/", "/sbin/", "/private/", "/Applications/", "/etc/", "/var/", "/dev/"]
            where path.hasPrefix(system) {
            return "\(path) is a system location and cannot be granted."
        }
        if path.hasPrefix(Iso.home + "/") || path == Iso.home { return "That folder is already the terminals' own." }
        for p in Iso.privateInHome {
            let base = home + "/" + p
            if path == base || path.hasPrefix(base + "/") {
                return "\(path) is inside \(p), which holds private data (credentials, the Keychain, mail, documents). "
                    + "Grant a project folder instead."
            }
        }
        if home.hasPrefix(path + "/") { return "\(path) contains your home folder and cannot be granted." }
        guard owns(path) else { return "You do not own \(path), so you cannot grant it." }
        return nil
    }
}
