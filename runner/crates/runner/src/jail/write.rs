//! The jail's writing primitives (design §3.4, §4.1). Every change happens
//! relative to a held parent handle with one validated name: nothing is
//! opened, created, renamed or removed by a path string, and no step
//! follows a link. A rename never replaces (`RENAME_NOREPLACE`), and a
//! replace is an exchange between a same-folder temporary file and the
//! target, so the swapped-out file can be checked after the fact.

use super::*;

/// The folder a request's last component lives in, held open, and that
/// name. `rel` is the stored (filesystem-reported) path of the target.
#[derive(Debug)]
pub struct Parent {
    pub dir: File,
    pub name: String,
    pub rel: String,
    cname: CString,
}

/// What [`Parent::kind`] saw at the name, without following it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Missing,
    File,
    Dir,
    Link,
    Other,
}

/// `renameat2` through the raw syscall: the libc wrapper is missing from the
/// musl the published (zig) builds link against. Same return and errno as the wrapper.
#[cfg(target_os = "linux")]
unsafe fn renameat2(
    olddirfd: libc::c_int,
    old: *const libc::c_char,
    newdirfd: libc::c_int,
    new: *const libc::c_char,
    flags: libc::c_uint,
) -> libc::c_int {
    libc::syscall(libc::SYS_renameat2, olddirfd, old, newdirfd, new, flags) as libc::c_int
}

#[cfg(test)]
thread_local! {
    /// Tests: make the next swap or rename fail as a crash would (EIO).
    pub(crate) static FAIL_SWAP: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn injected_failure() -> bool {
    #[cfg(test)]
    {
        FAIL_SWAP.with(|f| f.replace(false))
    }
    #[cfg(not(test))]
    {
        false
    }
}

fn check(r: libc::c_int) -> Result<(), i32> {
    if r == 0 {
        Ok(())
    } else {
        Err(errno())
    }
}

impl Root {
    /// The parent folder of `rel` and its last name: the lexical rule and
    /// the deny list on the request, the parents entered without following
    /// a link, then the parent's stored path checked again (under the root,
    /// not a secret folder, the root's device).
    pub fn open_parent(&self, rel: &str) -> Result<Parent, Refusal> {
        let parts = validate(rel)?;
        if let Some(why) = secret_reason(rel) {
            return Err(Refusal::Secret(why));
        }
        let (name, parents) = parts.split_last().ok_or(Refusal::Invalid)?;
        let dir = if parents.is_empty() {
            let d = openat(self.dir.as_raw_fd(), &c".".to_owned(), DIR_FLAGS).map_err(|_| Refusal::RootGone)?;
            let m = d.metadata().map_err(|_| Refusal::RootGone)?;
            if m.dev() != self.dev || m.ino() != self.ino {
                return Err(Refusal::RootGone);
            }
            d
        } else {
            self.enter_all(parents, Engine::Auto)?
        };
        let meta = dir
            .metadata()
            .map_err(|e| Refusal::Unreadable(e.raw_os_error().unwrap_or(libc::EIO)))?;
        if meta.dev() != self.dev {
            return Err(Refusal::Outside);
        }
        let real = handle_path(&dir).map_err(|e| Refusal::Unreadable(e.raw_os_error().unwrap_or(libc::ENOTSUP)))?;
        let stored = real.strip_prefix(&self.real).map_err(|_| Refusal::Outside)?;
        let mut srel: Vec<String> = stored
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        srel.push((*name).to_string());
        let srel = srel.join("/");
        if let Some(why) = secret_reason(&srel) {
            return Err(Refusal::Secret(why));
        }
        Ok(Parent {
            dir,
            name: (*name).to_string(),
            rel: srel,
            cname: cstr(name.as_bytes())?,
        })
    }

    /// Open the regular file (or folder) at the parent's name, with every
    /// check [`Root::open_file`] makes on the handle (kind, device, one link).
    pub fn open_in(&self, p: &Parent, dir: bool) -> Result<(File, Metadata), Refusal> {
        let want = if dir { Want::Dir } else { Want::File };
        self.open_last(&p.dir, &p.name, want, Limits::default())
    }
}

impl Parent {
    /// What is at the name now (never followed).
    pub fn kind(&self) -> Result<Kind, Refusal> {
        self.kind_of(&self.cname)
    }

    /// What is at `name` in this folder now (never followed).
    pub fn kind_of(&self, name: &CStr) -> Result<Kind, Refusal> {
        Ok(match kind_at(self.dir.as_raw_fd(), &name.to_owned()) {
            Ok(libc::S_IFREG) => Kind::File,
            Ok(libc::S_IFDIR) => Kind::Dir,
            Ok(libc::S_IFLNK) => Kind::Link,
            Ok(_) => Kind::Other,
            Err(libc::ENOENT) => Kind::Missing,
            Err(e) => return Err(Refusal::Unreadable(e)),
        })
    }

    /// `(dev, ino)` of whatever is at `name` in this folder, not followed.
    pub fn identity_of(&self, name: &CStr) -> Result<(u64, u64), i32> {
        let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: `name` is NUL-terminated; `st` is a buffer the kernel fills on success.
        check(unsafe {
            libc::fstatat(
                self.dir.as_raw_fd(),
                name.as_ptr(),
                st.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        })?;
        // SAFETY: fstatat returned 0.
        let st = unsafe { st.assume_init() };
        // The widths differ by platform (macOS: a 32-bit device number).
        #[allow(clippy::unnecessary_cast)]
        Ok((st.st_dev as u64, st.st_ino as u64))
    }

    pub fn cname(&self) -> &CStr {
        &self.cname
    }

    /// A new temporary file in this folder (`.tmuxedo-<random>.tmp`,
    /// `O_CREAT | O_EXCL | O_NOFOLLOW`), its mode set to `mode`.
    pub fn temp(&self, mode: u32) -> Result<(File, CString), i32> {
        for _ in 0..8 {
            let name = format!(".tmuxedo-{}.tmp", hex::encode(crate::util::random_bytes(8)));
            let c = CString::new(name).map_err(|_| libc::EINVAL)?;
            // SAFETY: `c` is NUL-terminated and outlives the call; the result is checked.
            let fd = unsafe {
                libc::openat(
                    self.dir.as_raw_fd(),
                    c.as_ptr(),
                    libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    0o600 as libc::c_uint,
                )
            };
            if fd < 0 {
                match errno() {
                    libc::EEXIST | libc::EINTR => continue,
                    e => return Err(e),
                }
            }
            // SAFETY: `fd` was just returned by the kernel and nothing else owns it.
            let f = unsafe { File::from_raw_fd(fd) };
            // SAFETY: a valid descriptor; the mode is masked to permission bits.
            check(unsafe { libc::fchmod(f.as_raw_fd(), (mode & 0o777) as libc::mode_t) })?;
            return Ok((f, c));
        }
        Err(libc::EEXIST)
    }

    /// Remove `name` (a file, or with `dir` an empty folder). Never follows.
    pub fn unlink(&self, name: &CStr, dir: bool) -> Result<(), i32> {
        let flags = if dir { libc::AT_REMOVEDIR } else { 0 };
        // SAFETY: `name` is NUL-terminated; the result is checked.
        check(unsafe { libc::unlinkat(self.dir.as_raw_fd(), name.as_ptr(), flags) })
    }

    /// Make the folder at the name (0755 under the umask).
    pub fn mkdir(&self) -> Result<(), i32> {
        // SAFETY: `cname` is NUL-terminated; the result is checked.
        check(unsafe { libc::mkdirat(self.dir.as_raw_fd(), self.cname.as_ptr(), 0o755) })
    }

    /// Rename `from` in this folder to `to`'s name in `to`'s folder, never
    /// replacing anything (`EEXIST` when something is there).
    pub fn rename_noreplace(&self, from: &CStr, to: &Parent) -> Result<(), i32> {
        if injected_failure() {
            return Err(libc::EIO);
        }
        #[cfg(target_os = "linux")]
        {
            // SAFETY: both names are NUL-terminated; the result is checked.
            let r = unsafe {
                renameat2(
                    self.dir.as_raw_fd(),
                    from.as_ptr(),
                    to.dir.as_raw_fd(),
                    to.cname.as_ptr(),
                    libc::RENAME_NOREPLACE,
                )
            };
            check(r)
        }
        #[cfg(not(target_os = "linux"))]
        {
            // No exclusive rename here: look, then rename (a window of microseconds; stated in the handoff).
            if to.kind().map_err(|_| libc::EIO)? != Kind::Missing {
                return Err(libc::EEXIST);
            }
            // SAFETY: both names are NUL-terminated; the result is checked.
            check(unsafe {
                libc::renameat(
                    self.dir.as_raw_fd(),
                    from.as_ptr(),
                    to.dir.as_raw_fd(),
                    to.cname.as_ptr(),
                )
            })
        }
    }

    /// Swap the temporary file `tmp` with the name: afterwards the name is
    /// the new file and `tmp` names the old one. Linux only (`RENAME_EXCHANGE`).
    #[cfg(target_os = "linux")]
    pub fn exchange(&self, tmp: &CStr) -> Result<(), i32> {
        if injected_failure() {
            return Err(libc::EIO);
        }
        // SAFETY: both names are NUL-terminated; the result is checked.
        check(unsafe {
            renameat2(
                self.dir.as_raw_fd(),
                tmp.as_ptr(),
                self.dir.as_raw_fd(),
                self.cname.as_ptr(),
                libc::RENAME_EXCHANGE,
            )
        })
    }

    /// Replace the name with `tmp` by a plain rename (systems without the
    /// exchange; the caller checked the target's identity just before).
    #[cfg(not(target_os = "linux"))]
    pub fn replace(&self, tmp: &CStr) -> Result<(), i32> {
        if injected_failure() {
            return Err(libc::EIO);
        }
        // SAFETY: both names are NUL-terminated; the result is checked.
        check(unsafe {
            libc::renameat(
                self.dir.as_raw_fd(),
                tmp.as_ptr(),
                self.dir.as_raw_fd(),
                self.cname.as_ptr(),
            )
        })
    }

    /// Open `name` in this folder for reading, not following a link.
    pub fn open_named(&self, name: &CStr) -> Result<File, i32> {
        openat(
            self.dir.as_raw_fd(),
            &name.to_owned(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        )
    }

    /// `fsync` the folder, so a rename survives a power cut.
    pub fn sync(&self) -> Result<(), i32> {
        let d = openat(
            self.dir.as_raw_fd(),
            &c".".to_owned(),
            libc::O_RDONLY | libc::O_DIRECTORY,
        )?;
        d.sync_all().map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))
    }
}
