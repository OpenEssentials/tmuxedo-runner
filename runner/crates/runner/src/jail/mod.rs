//! The jail: the one path resolver for everything that opens a file a
//! client or an agent named (docs/design/file-browser.md §3.4). `chat.file`
//! and `files.share` use it today; the file browser's `fs.*` methods will.
//!
//! A request is a ROOT (a directory the runner holds open) and a relative
//! path. Resolution never follows a link and never hands the kernel a path
//! it could resolve differently from what was checked:
//!
//! 1. LEXICAL. The path is refused, never normalised: `..`, `.`, empty
//!    components, a leading `/`, `\`, `:`, control characters, a trailing
//!    dot or space, a reserved Windows device name, more than 4096 bytes,
//!    64 components or 255 bytes a component ([`validate`]).
//! 2. SECRETS, on the request: the folded path against the deny list
//!    ([`secret_reason`]).
//! 3. WALK from the root's handle. Linux 5.6+: one `openat2` with
//!    `RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS |
//!    RESOLVE_NO_XDEV` (the kernel resolves and checks atomically).
//!    Everywhere else (older kernels, macOS): one `openat` a component with
//!    `O_NOFOLLOW | O_DIRECTORY`, so each step enters a real directory
//!    inside the one already held, or fails.
//! 4. OPEN the last component from its parent's handle with `O_NOFOLLOW |
//!    O_NONBLOCK` (a FIFO or a device cannot hang the runner).
//! 5. CHECK WHAT WAS OPENED, by `fstat` on the handle: a regular file (or a
//!    directory, for [`Root::open_dir`]) on the root's device, with exactly
//!    one name (a hard link is refused: the name says nothing about what
//!    the content is), within the caller's size limit.
//! 6. SECRETS again, on the name the system reports for the handle (a
//!    case-insensitive or normalising volume stores `.ENV` for `.env`), and
//!    that name must be under the root's real path.
//! 7. The caller uses only the handle. Nothing is reopened by path.
//!
//! There is no check-then-open: every step is relative to a handle already
//! held and no step follows a link. Swapping a directory for a link after
//! it was entered changes nothing (the handle is still the directory);
//! swapping it before fails the step.

mod broad;
mod secret;
mod write;
#[cfg(test)]
pub(crate) use write::FAIL_SWAP;
pub use write::{Kind, Parent};
#[cfg(test)]
mod tests;

pub use broad::{too_broad, Guard};
pub use secret::{fold, secret_reason};

use std::ffi::{CStr, CString};
use std::fs::{File, Metadata};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

/// A request path is at most this long (bytes).
pub const MAX_PATH_BYTES: usize = tmuxedo_protocol::fs::MAX_PATH_BYTES;
/// A request path has at most this many components.
pub const MAX_COMPONENTS: usize = tmuxedo_protocol::fs::MAX_COMPONENTS;
/// One component is at most this long (bytes).
pub const MAX_COMPONENT_BYTES: usize = tmuxedo_protocol::fs::MAX_COMPONENT_BYTES;

/// Why the jail did not open something. A refusal never says whether the
/// target exists outside the root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The request is not a path the jail accepts (step 1).
    Invalid,
    /// The target is not under the root (another device, a moved folder, an
    /// absolute path under no root).
    Outside,
    /// A component is a link. Links are never followed.
    Symlink,
    /// The deny list: the file may hold secrets (the reason is for people).
    Secret(&'static str),
    /// A regular file with more than one name (a hard link).
    Linked,
    NotFound,
    /// Not a regular file (or, for a folder request, not a folder): a
    /// folder, a FIFO, a socket, a device.
    NotAFile,
    /// Larger than the caller's limit.
    TooLarge {
        size: u64,
        max: u64,
    },
    /// The root itself is missing or is not a folder.
    RootGone,
    /// The system refused the open (permissions and the like): the errno.
    Unreadable(i32),
}

impl Refusal {
    /// The error code of the file browser's contract (design §3.4, §4).
    pub fn code(&self) -> &'static str {
        match self {
            Refusal::Invalid => "invalid_path",
            Refusal::Outside => "outside_root",
            Refusal::Symlink => "symlink_refused",
            Refusal::Secret(_) => "file_refused",
            Refusal::Linked => "linked_file",
            Refusal::NotFound => "not_found",
            Refusal::NotAFile => "not_a_file",
            Refusal::TooLarge { .. } => "file_too_large",
            Refusal::RootGone => "root_gone",
            Refusal::Unreadable(_) => "file_unreadable",
        }
    }
}

/// What a caller allows beyond the jail's own rules.
#[derive(Debug, Clone, Copy, Default)]
pub struct Limits {
    /// Refuse a file larger than this (bytes). None: any size.
    pub max_size: Option<u64>,
}

/// An open file or folder inside a root. Read, hash and stat on `file`;
/// never reopen `real`.
#[derive(Debug)]
pub struct Opened {
    pub file: File,
    /// The handle's own `fstat`.
    pub meta: Metadata,
    /// The path the system reports for the handle (for display and logs).
    pub real: PathBuf,
    /// `real` relative to the root, `/`-separated: the stored name.
    pub rel: String,
}

/// Which resolution to use. `Auto` is the only one callers want; tests run
/// the walk on its own so both are held to the same suite.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) enum Engine {
    Auto,
    Walk,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Want {
    File,
    Dir,
}

#[cfg(any(target_os = "linux", target_os = "android"))]
const DIR_FLAGS: libc::c_int = libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW;
#[cfg(not(any(target_os = "linux", target_os = "android")))]
const DIR_FLAGS: libc::c_int = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW;

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO)
}

fn cstr(s: &[u8]) -> Result<CString, Refusal> {
    CString::new(s).map_err(|_| Refusal::Invalid)
}

/// `openat(dir, name, flags | O_CLOEXEC)`: `name` is one component (or, for
/// a root, the root's own path with `AT_FDCWD`).
fn openat(dir: RawFd, name: &CString, flags: libc::c_int) -> Result<File, i32> {
    loop {
        // SAFETY: `name` is a NUL-terminated string that outlives the call; the result is checked.
        let fd = unsafe { libc::openat(dir, name.as_ptr(), flags | libc::O_CLOEXEC) };
        if fd >= 0 {
            // SAFETY: `fd` was just returned by the kernel and nothing else owns it.
            return Ok(unsafe { File::from_raw_fd(fd) });
        }
        let e = errno();
        if e != libc::EINTR {
            return Err(e);
        }
    }
}

/// The file type of `name` in `dir`, not following a link (`S_IF*`). Only
/// ever used to say WHY something is refused; what is opened is checked on
/// its handle.
fn kind_at(dir: RawFd, name: &CString) -> Result<libc::mode_t, i32> {
    let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: `name` is NUL-terminated; `st` is a valid buffer the kernel fills on success.
    let r = unsafe { libc::fstatat(dir, name.as_ptr(), st.as_mut_ptr(), libc::AT_SYMLINK_NOFOLLOW) };
    if r != 0 {
        return Err(errno());
    }
    // SAFETY: fstatat returned 0, so the buffer is initialised.
    Ok(unsafe { st.assume_init() }.st_mode & libc::S_IFMT)
}

/// The path the system reports for an open handle.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn handle_path(f: &File) -> std::io::Result<PathBuf> {
    std::fs::read_link(format!("/proc/self/fd/{}", f.as_raw_fd()))
}

#[cfg(target_vendor = "apple")]
fn handle_path(f: &File) -> std::io::Result<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    let mut buf = vec![0u8; libc::PATH_MAX as usize];
    // SAFETY: F_GETPATH writes at most PATH_MAX bytes (NUL-terminated) into the buffer.
    if unsafe { libc::fcntl(f.as_raw_fd(), libc::F_GETPATH, buf.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let n = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    buf.truncate(n);
    Ok(PathBuf::from(std::ffi::OsString::from_vec(buf)))
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
fn handle_path(_: &File) -> std::io::Result<PathBuf> {
    // No way to ask: the stored name cannot be checked, so nothing is opened.
    Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
}

#[cfg(target_os = "linux")]
mod openat2 {
    //! `openat2(2)` (Linux 5.6). The structure and flags are the kernel's
    //! stable ABI (`include/uapi/linux/openat2.h`).
    use std::ffi::CString;
    use std::fs::File;
    use std::os::fd::{FromRawFd, RawFd};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    #[repr(C)]
    struct OpenHow {
        flags: u64,
        mode: u64,
        resolve: u64,
    }

    const RESOLVE_NO_XDEV: u64 = 0x01;
    const RESOLVE_NO_MAGICLINKS: u64 = 0x02;
    const RESOLVE_NO_SYMLINKS: u64 = 0x04;
    const RESOLVE_BENEATH: u64 = 0x08;
    /// Everything the jail asks of the kernel.
    pub const BENEATH: u64 = RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS | RESOLVE_NO_XDEV;

    /// Set once the kernel says it has no `openat2` (`ENOSYS`).
    static MISSING: AtomicBool = AtomicBool::new(false);
    /// Successful calls (tests check the fast path really ran).
    pub static HITS: AtomicU64 = AtomicU64::new(0);

    pub fn missing() -> bool {
        MISSING.load(Ordering::Relaxed)
    }

    pub fn open(dir: RawFd, path: &CString, flags: libc::c_int, resolve: u64) -> Result<File, i32> {
        let how = OpenHow {
            flags: (flags | libc::O_CLOEXEC) as u64,
            mode: 0,
            resolve,
        };
        loop {
            // SAFETY: `path` is NUL-terminated and `how` is a valid open_how of the size passed; both outlive the call.
            let fd = unsafe {
                libc::syscall(
                    libc::SYS_openat2,
                    dir,
                    path.as_ptr(),
                    &how as *const OpenHow,
                    std::mem::size_of::<OpenHow>(),
                )
            };
            if fd >= 0 {
                HITS.fetch_add(1, Ordering::Relaxed);
                // SAFETY: `fd` was just returned by the kernel and nothing else owns it.
                return Ok(unsafe { File::from_raw_fd(fd as RawFd) });
            }
            match super::errno() {
                libc::EINTR => continue,
                libc::ENOSYS => {
                    MISSING.store(true, Ordering::Relaxed);
                    return Err(libc::ENOSYS);
                }
                e => return Err(e),
            }
        }
    }

    /// Does this kernel (and any syscall filter around us) have `openat2`?
    pub fn supported() -> bool {
        open(libc::AT_FDCWD, &c"/".to_owned(), libc::O_PATH | libc::O_DIRECTORY, 0).is_ok()
    }
}

/// Whether the atomic `openat2` resolution is in use here (Linux 5.6+ with
/// no syscall filter in the way); otherwise every request takes the walk.
pub fn openat2_supported() -> bool {
    #[cfg(target_os = "linux")]
    {
        openat2::supported()
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

/// Step 1: the components of a request path, or `Invalid`. Nothing is
/// normalised away: a path that needs cleaning is refused. The rule itself
/// is the protocol's ([`tmuxedo_protocol::fs::path_components`]), so the
/// codecs and the jail cannot disagree about what a path is.
pub fn validate(rel: &str) -> Result<Vec<&str>, Refusal> {
    tmuxedo_protocol::fs::path_components(rel).ok_or(Refusal::Invalid)
}

/// What a listed entry is, by `fstatat(AT_SYMLINK_NOFOLLOW)`: a link is a
/// link, whatever it points to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Dir,
    Link,
    Other,
}

/// One entry of a folder, as the folder's own handle reports it.
#[derive(Debug, Clone)]
pub struct Listed {
    pub name: std::ffi::OsString,
    pub kind: EntryKind,
    pub size: u64,
    pub mtime_ms: u64,
    pub dev: u64,
}

/// The entries of an open folder (a handle from [`Root::open_dir`] or
/// [`Root::open_self`]), at most `max`; the flag says more were there.
/// Nothing is opened and no link is followed: names come from the handle
/// (`fdopendir`) and each entry is described relative to it.
// `st_dev` and the time fields are not the same width on every system.
#[allow(clippy::unnecessary_cast)]
pub fn read_entries(dir: &File, max: usize) -> Result<(Vec<Listed>, bool), Refusal> {
    use std::os::unix::ffi::OsStringExt;
    // SAFETY: duplicates a descriptor we hold; the result is checked.
    let fd = unsafe { libc::fcntl(dir.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
    if fd < 0 {
        return Err(Refusal::Unreadable(errno()));
    }
    // SAFETY: `fd` is ours; on success the stream owns it (closedir closes it).
    let stream = unsafe { libc::fdopendir(fd) };
    if stream.is_null() {
        let e = errno();
        // SAFETY: fdopendir failed, so `fd` is still ours to close.
        unsafe { libc::close(fd) };
        return Err(Refusal::Unreadable(e));
    }
    // SAFETY: `stream` is a valid directory stream until closedir below.
    unsafe { libc::rewinddir(stream) };
    let mut out = Vec::new();
    let mut more = false;
    loop {
        // SAFETY: `stream` is valid; the entry is read before the next readdir call.
        let ent = unsafe { libc::readdir(stream) };
        if ent.is_null() {
            break;
        }
        // SAFETY: `d_name` is a NUL-terminated string inside the entry readdir returned.
        let name = unsafe { std::ffi::CStr::from_ptr((*ent).d_name.as_ptr()) }.to_owned();
        if matches!(name.to_bytes(), b"." | b"..") {
            continue;
        }
        if out.len() >= max {
            more = true;
            break;
        }
        let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: `name` is NUL-terminated; `st` is a valid buffer the kernel fills on success.
        if unsafe { libc::fstatat(fd, name.as_ptr(), st.as_mut_ptr(), libc::AT_SYMLINK_NOFOLLOW) } != 0 {
            continue; // gone since it was named
        }
        // SAFETY: fstatat returned 0, so the buffer is initialised.
        let st = unsafe { st.assume_init() };
        let kind = match st.st_mode & libc::S_IFMT {
            libc::S_IFREG => EntryKind::File,
            libc::S_IFDIR => EntryKind::Dir,
            libc::S_IFLNK => EntryKind::Link,
            _ => EntryKind::Other,
        };
        let ms = (st.st_mtime as i128) * 1000 + (st.st_mtime_nsec as i128) / 1_000_000;
        out.push(Listed {
            name: std::ffi::OsString::from_vec(name.into_bytes()),
            kind,
            size: if kind == EntryKind::File {
                st.st_size.max(0) as u64
            } else {
                0
            },
            mtime_ms: ms.clamp(0, u64::MAX as i128) as u64,
            dev: st.st_dev as u64,
        });
    }
    // SAFETY: `stream` came from fdopendir and is closed exactly once.
    unsafe { libc::closedir(stream) };
    Ok((out, more))
}

impl Root {
    /// The root folder itself, as a readable handle (for a listing of `""`).
    /// Opened from the held handle, never by the root's path.
    pub fn open_self(&self) -> Result<Opened, Refusal> {
        let file = openat(
            self.dir.as_raw_fd(),
            &c".".to_owned(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        )
        .map_err(|e| match e {
            libc::ENOENT | libc::ENOTDIR => Refusal::RootGone,
            e => Refusal::Unreadable(e),
        })?;
        let meta = file
            .metadata()
            .map_err(|e| Refusal::Unreadable(e.raw_os_error().unwrap_or(libc::EIO)))?;
        if !meta.is_dir() || meta.dev() != self.dev || meta.ino() != self.ino {
            return Err(Refusal::RootGone);
        }
        Ok(Opened {
            file,
            meta,
            real: self.real.clone(),
            rel: String::new(),
        })
    }

    /// The device the root is on (an entry on another one is a mount).
    pub fn dev(&self) -> u64 {
        self.dev
    }
}

/// A folder the jail opens files under, held as an open directory handle.
#[derive(Debug)]
pub struct Root {
    dir: File,
    /// The path the system reports for the handle.
    real: PathBuf,
    /// The path the root was opened by (may differ from `real` by links
    /// above it, or, for [`Root::open_trusted`], at it).
    given: PathBuf,
    dev: u64,
    ino: u64,
}

impl Root {
    /// Open a root by its absolute path. The path's last component is not
    /// followed: a root that is itself a link is refused (`Symlink`).
    pub fn open(path: &Path) -> Result<Root, Refusal> {
        if !path.is_absolute() {
            return Err(Refusal::Invalid);
        }
        let c = cstr(path.as_os_str().as_bytes())?;
        let dir = openat(libc::AT_FDCWD, &c, DIR_FLAGS).map_err(|e| match e {
            libc::ELOOP | libc::EMLINK => Refusal::Symlink,
            libc::ENOTDIR if kind_at(libc::AT_FDCWD, &c) == Ok(libc::S_IFLNK) => Refusal::Symlink,
            _ => Refusal::RootGone,
        })?;
        let meta = dir.metadata().map_err(|_| Refusal::RootGone)?;
        let real = handle_path(&dir).map_err(|_| Refusal::RootGone)?;
        Ok(Root {
            dir,
            real,
            given: path.to_path_buf(),
            dev: meta.dev(),
            ino: meta.ino(),
        })
    }

    /// Open a root whose path comes from the runner's own records (a chat's
    /// work folder, a pane's repository), where a link AT the root is the
    /// person's own layout (`~/code -> /mnt/data/code`): the path is made
    /// real once, then opened as [`Root::open`] does. Never for a path a
    /// client supplied.
    pub fn open_trusted(path: &Path) -> Result<Root, Refusal> {
        let real = path.canonicalize().map_err(|_| Refusal::RootGone)?;
        let mut root = Root::open(&real)?;
        root.given = path.to_path_buf();
        Ok(root)
    }

    /// The root's real path (what the system reports for its handle).
    pub fn real(&self) -> &Path {
        &self.real
    }

    /// Does the root's path still lead to the folder that was opened? False
    /// once it was moved, removed or replaced (`root_gone`).
    pub fn is_current(&self) -> bool {
        std::fs::symlink_metadata(&self.real).is_ok_and(|m| m.is_dir() && m.dev() == self.dev && m.ino() == self.ino)
    }

    /// Open the regular file at `rel` (relative, `/`-separated).
    pub fn open_file(&self, rel: &str, limits: Limits) -> Result<Opened, Refusal> {
        self.open_with(rel, Want::File, limits, Engine::Auto)
    }

    /// Open the folder at `rel` (relative, `/`-separated).
    pub fn open_dir(&self, rel: &str) -> Result<Opened, Refusal> {
        self.open_with(rel, Want::Dir, Limits::default(), Engine::Auto)
    }

    #[cfg(test)]
    pub(crate) fn open_file_with(&self, rel: &str, limits: Limits, engine: Engine) -> Result<Opened, Refusal> {
        self.open_with(rel, Want::File, limits, engine)
    }

    fn open_with(&self, rel: &str, want: Want, limits: Limits, engine: Engine) -> Result<Opened, Refusal> {
        let parts = validate(rel)?;
        if let Some(why) = secret_reason(rel) {
            return Err(Refusal::Secret(why));
        }
        let (name, parents) = parts.split_last().ok_or(Refusal::Invalid)?;
        let held;
        let parent: &File = if parents.is_empty() {
            &self.dir
        } else {
            held = self.enter_all(parents, engine)?;
            &held
        };
        let (file, meta) = self.open_last(parent, name, want, limits)?;
        // What the filesystem says was opened: under the root, and not a secret by its stored name.
        let real = handle_path(&file).map_err(|e| Refusal::Unreadable(e.raw_os_error().unwrap_or(libc::ENOTSUP)))?;
        let stored = real.strip_prefix(&self.real).map_err(|_| Refusal::Outside)?;
        let rel = stored
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        if let Some(why) = secret_reason(&rel) {
            return Err(Refusal::Secret(why));
        }
        Ok(Opened { file, meta, real, rel })
    }

    /// The handle of the folder `parents` leads to, entering no link.
    fn enter_all(&self, parents: &[&str], engine: Engine) -> Result<File, Refusal> {
        #[cfg(target_os = "linux")]
        {
            if engine == Engine::Auto && !openat2::missing() {
                let path = cstr(parents.join("/").as_bytes())?;
                match openat2::open(self.dir.as_raw_fd(), &path, DIR_FLAGS, openat2::BENEATH) {
                    Ok(f) => return Ok(f),
                    Err(libc::ELOOP) => return Err(Refusal::Symlink),
                    Err(libc::EXDEV) => return Err(Refusal::Outside),
                    Err(libc::ENOENT) => return Err(Refusal::NotFound),
                    // Anything else (no openat2, a filter, a link as the last folder, a retry): the walk decides.
                    Err(_) => {}
                }
            }
        }
        let _ = engine;
        let mut held: Option<File> = None;
        for p in parents {
            let dir = held.as_ref().unwrap_or(&self.dir);
            let c = cstr(p.as_bytes())?;
            let next = openat(dir.as_raw_fd(), &c, DIR_FLAGS).map_err(|e| match e {
                libc::ELOOP | libc::EMLINK => Refusal::Symlink,
                libc::ENOTDIR if kind_at(dir.as_raw_fd(), &c) == Ok(libc::S_IFLNK) => Refusal::Symlink,
                libc::ENOENT | libc::ENOTDIR => Refusal::NotFound,
                libc::ENAMETOOLONG => Refusal::Invalid,
                e => Refusal::Unreadable(e),
            })?;
            // Another filesystem mounted inside the root is not the root.
            let meta = next
                .metadata()
                .map_err(|e| Refusal::Unreadable(e.raw_os_error().unwrap_or(libc::EIO)))?;
            if meta.dev() != self.dev {
                return Err(Refusal::Outside);
            }
            held = Some(next);
        }
        held.ok_or(Refusal::Invalid)
    }

    /// Steps 4 and 5: open `name` in `parent` and check the handle.
    fn open_last(&self, parent: &File, name: &str, want: Want, limits: Limits) -> Result<(File, Metadata), Refusal> {
        let c = cstr(name.as_bytes())?;
        // A look first, so a FIFO or a device is refused without being opened at all. It decides
        // nothing: what is opened is checked again on its handle below.
        match kind_at(parent.as_raw_fd(), &c) {
            Ok(libc::S_IFLNK) => return Err(Refusal::Symlink),
            Ok(libc::S_IFREG) if want == Want::File => {}
            Ok(libc::S_IFDIR) if want == Want::Dir => {}
            Ok(_) => return Err(Refusal::NotAFile),
            Err(libc::ENOENT | libc::ENOTDIR) => return Err(Refusal::NotFound),
            Err(libc::ENAMETOOLONG) => return Err(Refusal::Invalid),
            Err(e) => return Err(Refusal::Unreadable(e)),
        }
        let mut flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK;
        if want == Want::Dir {
            flags |= libc::O_DIRECTORY;
        }
        let file = openat(parent.as_raw_fd(), &c, flags).map_err(|e| match e {
            libc::ELOOP | libc::EMLINK => Refusal::Symlink,
            libc::ENOENT => Refusal::NotFound,
            libc::ENOTDIR | libc::ENXIO | libc::ENODEV => Refusal::NotAFile,
            e => Refusal::Unreadable(e),
        })?;
        let meta = file
            .metadata()
            .map_err(|e| Refusal::Unreadable(e.raw_os_error().unwrap_or(libc::EIO)))?;
        let right_kind = match want {
            Want::File => meta.is_file(),
            Want::Dir => meta.is_dir(),
        };
        if !right_kind {
            return Err(Refusal::NotAFile);
        }
        if meta.dev() != self.dev {
            return Err(Refusal::Outside);
        }
        if want == Want::File {
            match meta.nlink() {
                0 => return Err(Refusal::NotFound),
                1 => {}
                _ => return Err(Refusal::Linked),
            }
            if let Some(max) = limits.max_size.filter(|max| meta.len() > *max) {
                return Err(Refusal::TooLarge { size: meta.len(), max });
            }
        }
        Ok((file, meta))
    }
}

/// For callers that are handed an ABSOLUTE path (`files.share`, a chat's
/// tool events): which root it is under and the path relative to it, by
/// comparing names only. A root matches by the path it was opened by or by
/// its real path. `..` is refused, not resolved; the jail then walks the
/// relative path, so a link anywhere under the root is still refused.
pub fn locate(roots: &[Root], path: &str) -> Result<(usize, String), Refusal> {
    if path.is_empty() || path.len() > MAX_PATH_BYTES || path.chars().any(char::is_control) {
        return Err(Refusal::Invalid);
    }
    let p = Path::new(path);
    if !p.is_absolute() {
        return Err(Refusal::Invalid);
    }
    for (i, root) in roots.iter().enumerate() {
        for base in [&root.given, &root.real] {
            let Ok(rest) = p.strip_prefix(base) else {
                continue;
            };
            let mut parts = Vec::new();
            for c in rest.components() {
                match c {
                    Component::Normal(x) => parts.push(x.to_str().ok_or(Refusal::Invalid)?),
                    _ => return Err(Refusal::Invalid),
                }
            }
            if parts.is_empty() {
                return Err(Refusal::NotAFile);
            }
            return Ok((i, parts.join("/")));
        }
    }
    Err(Refusal::Outside)
}

/// Open the regular file at an absolute `path` under one of `roots`
/// ([`locate`], then [`Root::open_file`]). Returns the root's index too.
pub fn open_absolute(roots: &[Root], path: &str, limits: Limits) -> Result<(usize, Opened), Refusal> {
    let (i, rel) = locate(roots, path)?;
    Ok((i, roots[i].open_file(&rel, limits)?))
}
