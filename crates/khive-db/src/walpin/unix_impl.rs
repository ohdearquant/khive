use super::io_other;
use khive_fs::directory_walk::{
    walk_to_directory, AncestorLinkPolicy, AncestorWalkEndpoint, BudgetExhausted,
    ANCESTOR_LINK_BUDGET,
};
use khive_fs::fd_relative::{clear_errno, current_errno, errno_location};
use std::ffi::{CStr, CString};
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::path::Path;
use std::time::{Duration, SystemTime};

/// Hard cap on one sidecar entry's byte size. Real heartbeat/beacon
/// JSON bodies are well under 1 KiB; anything larger is not a record
/// this module wrote, and reading it unboundedly would let a same-uid
/// process balloon checkpoint-time enumeration.
pub(super) const MAX_SIDECAR_ENTRY_BYTES: u64 = 64 * 1024;

/// Every raw directory entry — hidden or not — counts toward a scan
/// bound of `RAW_SCAN_FACTOR * max` in `list_names`, so a flood of
/// dot-files cannot extend the `readdir` loop unboundedly even though
/// hidden names never consume the retained-name budget itself.
const RAW_SCAN_FACTOR: usize = 8;

pub(super) fn current_uid() -> u32 {
    // SAFETY: `geteuid()` takes no arguments and cannot fail.
    unsafe { libc::geteuid() }
}

#[cfg(test)]
thread_local! {
    /// Test-only seam for `remove_if_same`'s documented residual race:
    /// POSIX gives no way to force a second process to replace a file at
    /// the exact instant between the device/inode recheck and the
    /// unlink, so a test runs arbitrary code from here instead,
    /// synchronously, on the thread already inside `remove_if_same`.
    static REMOVE_IF_SAME_RACE_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(super) fn set_remove_if_same_race_hook(hook: impl FnOnce() + 'static) {
    REMOVE_IF_SAME_RACE_HOOK.with(|cell| *cell.borrow_mut() = Some(Box::new(hook)));
}

#[cfg(test)]
fn take_remove_if_same_race_hook() -> Option<Box<dyn FnOnce()>> {
    REMOVE_IF_SAME_RACE_HOOK.with(|cell| cell.borrow_mut().take())
}

#[cfg(test)]
thread_local! {
    /// Test-only seam for a `readdir()` read failure mid-walk: forcing a
    /// real directory read to fail from a portable unit test isn't
    /// practical, so this one-shot override makes `list_names`'s next
    /// loop iteration observe a NULL entry with this errno already set,
    /// exactly as a genuine failed `readdir()` would leave it — the
    /// downstream `readdir_null_is_error` branch that decides Err vs.
    /// end-of-directory runs unmodified against the forced state.
    static LIST_NAMES_READDIR_FAULT: std::cell::Cell<Option<libc::c_int>> =
        const { std::cell::Cell::new(None) };
}

#[cfg(test)]
pub(super) fn set_list_names_readdir_fault(errno: libc::c_int) {
    LIST_NAMES_READDIR_FAULT.with(|cell| cell.set(Some(errno)));
}

fn take_list_names_readdir_fault() -> Option<libc::c_int> {
    #[cfg(test)]
    {
        LIST_NAMES_READDIR_FAULT.with(|cell| cell.take())
    }
    #[cfg(not(test))]
    {
        None
    }
}

/// Whether a NULL `readdir` return denotes a genuine read error rather
/// than end-of-directory: POSIX overloads NULL for both and the only
/// distinguishing signal is `errno`, which stays `0` at end-of-directory
/// because the loop clears it immediately before every call. Split out
/// so a test can drive both branches directly — forcing a real `readdir`
/// to fail mid-walk from a portable unit test isn't practical, so this
/// pure predicate is the seam.
pub(super) fn readdir_null_is_error(errno: libc::c_int) -> bool {
    errno != 0
}

fn name_cstring(name: &str) -> io::Result<CString> {
    CString::new(name)
        .map_err(|_| io_other(format!("sidecar entry name {name:?} contains a NUL byte")))
}

/// Byte-exact `CString` construction for a path component that may come
/// from an arbitrary database file name (never lossy `to_string_lossy`
/// conversion): the sidecar directory's own final component is derived
/// from the caller's database path (see `sidecar_dir_for`), and this
/// project explicitly supports non-UTF-8 database paths (`pool.rs`'s
/// `mint_db_identity_non_utf8_path_round_trips`). A lossy conversion
/// here would map distinct non-UTF-8 names to the same replacement-
/// character byte sequence, colliding two different databases onto one
/// sidecar directory.
fn name_cstring_os(name: &std::ffi::OsStr) -> io::Result<CString> {
    CString::new(name.as_bytes())
        .map_err(|_| io_other(format!("sidecar entry name {name:?} contains a NUL byte")))
}

fn is_symlink_mode(mode: libc::mode_t) -> bool {
    (mode & libc::S_IFMT) == libc::S_IFLNK
}

/// The walk reports a spent link budget as its own error type; the
/// sidecar keeps the wording it has always reported for that case.
fn sidecar_walk_error(error: io::Error) -> io::Error {
    let component = error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<BudgetExhausted>())
        .map(|exhausted| exhausted.component.clone());
    match component {
        Some(name) => io_other(format!(
            "walpin sidecar ancestor {name:?} exceeded the symlink resolution depth budget"
        )),
        None => error,
    }
}

pub(super) struct SidecarDirHandle(fs::File);

/// One opened, validated regular entry plus the filesystem identity used
/// to make a later unlink race-safe. Ownership is checked before this is
/// constructed (see `read_checked_entry`), so every `CheckedEntry` is
/// already known to belong to `current_uid()`.
pub(super) struct CheckedEntry {
    pub(super) body: Vec<u8>,
    pub(super) mtime: SystemTime,
    device: u64,
    inode: u64,
}

impl SidecarDirHandle {
    fn raw(&self) -> RawFd {
        self.0.as_raw_fd()
    }

    /// Open the sidecar dir, creating it (mode 0700) if absent. The
    /// freshly-created (or already-existing) directory is validated on
    /// the OPENED descriptor, never trusted from the `mkdir` call alone
    /// — a concurrent process could have raced the creation.
    pub(super) fn open_or_create(dir: &Path) -> io::Result<Self> {
        let (parent_fd, c_name) = Self::open_parent_and_name(dir)?;
        match Self::open_validated_at(&parent_fd, &c_name, dir) {
            Ok(handle) => Ok(handle),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                // SAFETY: `c_name` is NUL-terminated for the call;
                // `parent_fd` is a live, open directory descriptor for
                // the call's duration.
                let rc = unsafe { libc::mkdirat(parent_fd.as_raw_fd(), c_name.as_ptr(), 0o700) };
                if rc != 0 {
                    let err = io::Error::last_os_error();
                    if err.kind() != io::ErrorKind::AlreadyExists {
                        return Err(err);
                    }
                }
                Self::open_validated_at(&parent_fd, &c_name, dir)
            }
            Err(e) => Err(e),
        }
    }

    /// Same as [`Self::open_or_create`] but never creates: `Ok(None)`
    /// for a missing directory (a sidecar that was never used yet is
    /// not an error, and must not have the side effect of creating one
    /// — e.g. a stray `remove_heartbeat`/`touch_beacon` call).
    pub(super) fn open_if_exists(dir: &Path) -> io::Result<Option<Self>> {
        let (parent_fd, c_name) = Self::open_parent_and_name(dir)?;
        match Self::open_validated_at(&parent_fd, &c_name, dir) {
            Ok(handle) => Ok(Some(handle)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Open `dir`'s parent directory and return it alongside `dir`'s
    /// final path component (as an exact, non-lossy `CString` — see
    /// [`name_cstring_os`]), so the caller can `openat()` `dir` itself
    /// relative to an already-live descriptor instead of re-resolving
    /// `dir`'s full path. The parent is reached via
    /// [`open_dir_component_walk`], which checks each ancestor link
    /// under the shared policy — a bare
    /// `open(parent, O_NOFOLLOW)` only refuses a symlink at `parent`'s
    /// own final component; every component before that is followed by
    /// ordinary kernel path resolution, so an attacker who can replace
    /// any ancestor of `dir` between path construction and this open
    /// would still redirect the lookup. Anchoring on a descriptor at
    /// every level closes that gap for `dir` the same way
    /// `SidecarDirHandle` already closes it for every entry `dir`
    /// contains.
    fn open_parent_and_name(dir: &Path) -> io::Result<(fs::File, CString)> {
        let parent = match dir.parent() {
            Some(p) if !p.as_os_str().is_empty() => p,
            _ => Path::new("."),
        };
        let name = dir.file_name().ok_or_else(|| {
            io_other(format!(
                "walpin sidecar path {dir:?} has no final path component"
            ))
        })?;
        let parent_file = Self::open_dir_component_walk(parent)?;
        let c_name = name_cstring_os(name)?;
        Ok((parent_file, c_name))
    }

    /// Open `path` (a directory) as an owned descriptor by walking
    /// every path component with `openat(.., O_DIRECTORY | O_NOFOLLOW)`
    /// relative to the previous descriptor — anchored at `/` for an
    /// absolute path, or the current directory for a relative one. A
    /// single `open(path, O_NOFOLLOW)` only refuses a symlink at
    /// `path`'s own, final component; every intermediate component is
    /// followed by ordinary kernel path resolution regardless of that
    /// flag. Walking component-by-component makes each individual
    /// `openat` call's "final component" the immediate next segment, so
    /// `O_NOFOLLOW` refuses a symlink at every level, not just the last.
    ///
    /// The walk runs over `path` LITERALLY — never through a
    /// `fs::canonicalize` pre-pass. A pre-resolve pass would itself
    /// follow every ancestor symlink through ordinary kernel path
    /// resolution before the descriptor walk ever starts, silently
    /// validating whatever a hostile symlink planted before this call
    /// pointed at; the walk would then only ever see the
    /// already-attacker-chosen path. Legitimate OS-level ancestor
    /// symlinks (macOS's `/tmp -> private/tmp`, `/var -> private/var`)
    /// still have to work, so a component `openat` refuses with
    /// `ELOOP`/`ENOTDIR` gets exactly one second look, inside
    /// `walk_to_directory`: it `fstatat`s the component (without
    /// following) to confirm it really is a symlink, and
    /// `AncestorLinkPolicy` applies the shared owner, parent, ACL and
    /// identity checks before and after reading the link. This walks
    /// only the sidecar's parent: even its last component is an
    /// ancestor, while `open_validated_at` separately refuses a link
    /// at the actual sidecar name. Total link hops use the common
    /// `ANCESTOR_LINK_BUDGET`.
    fn open_dir_component_walk(path: &Path) -> io::Result<fs::File> {
        let mut policy = AncestorLinkPolicy::new(AncestorWalkEndpoint::TargetParent);
        let walked = walk_to_directory(path, &mut policy, ANCESTOR_LINK_BUDGET);
        let mut pinned = walked.map_err(sidecar_walk_error)?;
        pinned
            .pop()
            .ok_or_else(|| io_other("walpin sidecar ancestor walk pinned no directory"))
    }

    fn open_validated_at(parent_fd: &fs::File, c_name: &CString, dir: &Path) -> io::Result<Self> {
        // SAFETY: `c_name` is NUL-terminated for the call; `parent_fd`
        // is a live, open directory descriptor for the call's duration;
        // the returned fd is uniquely owned by this call and wrapped
        // immediately.
        let fd = unsafe {
            libc::openat(
                parent_fd.as_raw_fd(),
                c_name.as_ptr(),
                libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            let err = io::Error::last_os_error();
            // The `O_NOFOLLOW` openat above already made the refusal
            // decision (a symlink at `dir` cannot have produced a live
            // fd) — this is diagnostic-only, not a second
            // security check, so it carries no TOCTOU risk. It exists
            // because the raw OS errno for a symlinked path
            // (`ELOOP`/`ENOTDIR`, platform-dependent) doesn't say
            // "symlink" on its own.
            if err.kind() != io::ErrorKind::NotFound {
                if let Ok(meta) = fs::symlink_metadata(dir) {
                    if meta.file_type().is_symlink() {
                        return Err(io_other(format!(
                            "walpin sidecar path {dir:?} is a symlink; refusing"
                        )));
                    }
                }
            }
            return Err(err);
        }
        // SAFETY: `fd` was just returned by the successful `openat` above.
        let handle = Self(unsafe { fs::File::from_raw_fd(fd) });
        handle.validate(dir)?;
        Ok(handle)
    }

    fn validate(&self, dir: &Path) -> io::Result<()> {
        let st = self.fstat_self()?;
        if (st.st_mode & libc::S_IFMT) != libc::S_IFDIR {
            return Err(io_other(format!(
                "walpin sidecar path {dir:?} is not a directory"
            )));
        }
        let mode = st.st_mode & 0o777;
        if mode != 0o700 {
            return Err(io_other(format!(
                "walpin sidecar dir {dir:?} has mode {mode:o}, expected 0700; \
                 refusing rather than chmod"
            )));
        }
        if st.st_uid != current_uid() {
            return Err(io_other(format!(
                "walpin sidecar dir {dir:?} is not owned by the current user; refusing"
            )));
        }
        Ok(())
    }

    fn fstat_self(&self) -> io::Result<libc::stat> {
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: `st` is a valid, appropriately-sized zeroed buffer.
        let rc = unsafe { libc::fstat(self.raw(), &mut st) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(st)
    }

    /// `fstatat(dirfd, name, AT_SYMLINK_NOFOLLOW)` relative to this
    /// directory's own fd. `Ok(None)` for a missing entry.
    fn stat_entry(&self, name: &str) -> io::Result<Option<libc::stat>> {
        let c_name = name_cstring(name)?;
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: `st` is a valid, zeroed buffer; `self.raw()` is a
        // live, open directory descriptor for the call's duration.
        let rc = unsafe {
            libc::fstatat(
                self.raw(),
                c_name.as_ptr(),
                &mut st,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if rc != 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::NotFound {
                return Ok(None);
            }
            return Err(err);
        }
        Ok(Some(st))
    }

    /// Exclusive-create `tmp_name`, write `body`, fsync, then atomically
    /// `renameat` it over `target_name`. Refuses a pre-existing symlink
    /// at `target_name` (checked via `stat_entry` on the SAME fd, never
    /// a fresh path lookup) before writing anything.
    pub(super) fn write_atomic(
        &self,
        target_name: &str,
        tmp_name: &str,
        body: &[u8],
    ) -> io::Result<()> {
        if let Some(st) = self.stat_entry(target_name)? {
            if is_symlink_mode(st.st_mode) {
                return Err(io_other(format!(
                    "walpin sidecar entry {target_name:?} is a symlink; refusing to write \
                     through it"
                )));
            }
        }
        // Best-effort: a stale temp file from a prior crashed write
        // must not block this one via O_EXCL.
        let _ = self.unlink_tolerant(tmp_name);

        let c_tmp = name_cstring(tmp_name)?;
        // SAFETY: `c_tmp` is NUL-terminated for the call; the returned
        // fd is uniquely owned and wrapped immediately below.
        let fd = unsafe {
            libc::openat(
                self.raw(),
                c_tmp.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        {
            // SAFETY: `fd` was just returned by the successful `openat`.
            let mut file = unsafe { fs::File::from_raw_fd(fd) };
            file.write_all(body)?;
            file.sync_all()?;
        }
        self.rename_over(tmp_name, target_name)
    }

    fn rename_over(&self, from: &str, to: &str) -> io::Result<()> {
        // Preserve the sidecar's NUL diagnostics before shared component validation.
        let _ = name_cstring(from)?;
        let _ = name_cstring(to)?;
        khive_fs::fd_relative::rename_at(
            &self.0,
            std::ffi::OsStr::new(from),
            &self.0,
            std::ffi::OsStr::new(to),
        )
    }

    pub(super) fn unlink_tolerant(&self, name: &str) -> io::Result<()> {
        let c_name = name_cstring(name)?;
        // SAFETY: `c_name` is NUL-terminated for the call.
        let rc = unsafe { libc::unlinkat(self.raw(), c_name.as_ptr(), 0) };
        if rc != 0 {
            let err = io::Error::last_os_error();
            if err.kind() != io::ErrorKind::NotFound {
                return Err(err);
            }
        }
        Ok(())
    }

    /// Refuse-then-remove, matching the historical `remove_heartbeat`
    /// contract: a symlinked entry is refused rather than unlinked, even
    /// though `unlink` itself never follows symlinks — removing a
    /// suspicious entry is left for a human to look at.
    pub(super) fn remove_checked(&self, name: &str) -> io::Result<()> {
        match self.stat_entry(name)? {
            None => Ok(()),
            Some(st) if is_symlink_mode(st.st_mode) => Err(io_other(format!(
                "refusing to remove symlinked walpin sidecar entry {name:?}"
            ))),
            Some(_) => self.unlink_tolerant(name),
        }
    }

    /// Metadata-only mtime refresh (ADR-091 Amendment 2 beacon refresh
    /// rule) — `futimens` with `UTIME_NOW`/`UTIME_OMIT`, no data write.
    pub(super) fn touch_mtime(&self, name: &str) -> io::Result<()> {
        let st = self
            .stat_entry(name)?
            .ok_or_else(|| io_other(format!("walpin sidecar entry {name:?} does not exist")))?;
        if is_symlink_mode(st.st_mode) {
            return Err(io_other(format!(
                "walpin sidecar entry {name:?} is a symlink; refusing to touch it"
            )));
        }
        let c_name = name_cstring(name)?;
        // SAFETY: `c_name` is NUL-terminated; `O_NOFOLLOW` refuses a
        // symlink at open time, `O_NONBLOCK` keeps a FIFO planted at
        // this name from blocking the open waiting for a reader (a
        // reader-less FIFO fails the open with `ENXIO` instead — an
        // error, never a hang).
        let fd = unsafe {
            libc::openat(
                self.raw(),
                c_name.as_ptr(),
                libc::O_WRONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` was just returned by the successful `openat`;
        // the `File` owns and closes it exactly once.
        let file = unsafe { fs::File::from_raw_fd(fd) };
        if !file.metadata()?.file_type().is_file() {
            return Err(io_other(format!(
                "walpin sidecar entry {name:?} is not a regular file"
            )));
        }
        let times = [
            libc::timespec {
                tv_sec: 0,
                tv_nsec: libc::UTIME_OMIT,
            },
            libc::timespec {
                tv_sec: 0,
                tv_nsec: libc::UTIME_NOW,
            },
        ];
        // SAFETY: the fd is live (owned by `file`); `times` is a valid
        // 2-element array as `futimens` requires.
        let rc = unsafe { libc::futimens(file.as_raw_fd(), times.as_ptr()) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Read `name`'s contents plus its mtime. `Ok(None)` for a missing
    /// entry (raced away between listing and reading). Refuses symlinks,
    /// non-owned files, non-regular files, and oversized entries: the
    /// open carries `O_NONBLOCK` so a FIFO planted at the entry's name
    /// can never block this call waiting for a peer, the opened fd is
    /// `fstat`'d and must be a regular file owned by `current_uid()`
    /// before any byte is read, and the read itself is bounded — a
    /// same-uid process must not be able to stall or balloon
    /// checkpoint-time enumeration. Ownership is checked against the
    /// same `fstat` that classifies the file, and BEFORE its contents
    /// are read, via `io::ErrorKind::PermissionDenied` — never folded
    /// into the generic untrusted-entry error, so a caller can route a
    /// non-owned (or otherwise uninspectable) entry to a distinct
    /// degraded-evidence outcome instead of silently skipping it.
    pub(super) fn read_checked_entry(&self, name: &str) -> io::Result<Option<CheckedEntry>> {
        use std::os::unix::fs::MetadataExt;

        if self.stat_entry(name)?.is_none() {
            return Ok(None);
        }
        let c_name = name_cstring(name)?;
        // SAFETY: `c_name` is NUL-terminated; `O_NOFOLLOW` refuses a
        // symlink at open time, `O_NONBLOCK` makes a FIFO open return
        // immediately instead of blocking for a writer.
        let fd = unsafe {
            libc::openat(
                self.raw(),
                c_name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
            )
        };
        if fd < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::NotFound {
                return Ok(None);
            }
            return Err(err);
        }
        // SAFETY: `fd` was just returned by the successful `openat`.
        let file = unsafe { fs::File::from_raw_fd(fd) };
        let meta = file.metadata()?;
        if !meta.file_type().is_file() {
            return Err(io_other(format!(
                "walpin sidecar entry {name:?} is not a regular file"
            )));
        }
        if meta.uid() != current_uid() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("walpin sidecar entry {name:?} is not owned by the current user"),
            ));
        }
        if meta.len() > MAX_SIDECAR_ENTRY_BYTES {
            return Err(io_other(format!(
                "walpin sidecar entry {name:?} exceeds {MAX_SIDECAR_ENTRY_BYTES} bytes"
            )));
        }
        let mut buf = Vec::new();
        (&file)
            .take(MAX_SIDECAR_ENTRY_BYTES + 1)
            .read_to_end(&mut buf)?;
        if buf.len() as u64 > MAX_SIDECAR_ENTRY_BYTES {
            return Err(io_other(format!(
                "walpin sidecar entry {name:?} exceeds {MAX_SIDECAR_ENTRY_BYTES} bytes"
            )));
        }
        let mtime = SystemTime::UNIX_EPOCH + Duration::new(meta.mtime().max(0) as u64, 0);
        Ok(Some(CheckedEntry {
            body: buf,
            mtime,
            device: meta.dev(),
            inode: meta.ino(),
        }))
    }

    pub(super) fn read_checked(&self, name: &str) -> io::Result<Option<(Vec<u8>, SystemTime)>> {
        Ok(self
            .read_checked_entry(name)?
            .map(|entry| (entry.body, entry.mtime)))
    }

    /// Remove `name` only while it still denotes the exact regular file
    /// opened and classified by `read_checked_entry`. A producer that
    /// replaces its temp before this recheck runs therefore wins: the
    /// new inode is retained instead of being unlinked under a stale
    /// verdict.
    ///
    /// This closes the wide races (stale content lingering, a symlink
    /// swapped in) but not every one: POSIX has no delete-if-still-
    /// this-inode primitive for a plain file, so the recheck above and
    /// the `unlinkat` below are two separate syscalls, and a replacement
    /// landing in the instant between them is still a plain name lookup
    /// that `unlinkat` will happily remove. A producer that loses this
    /// narrow race observes its own `rename` fail because the temp it
    /// just wrote is already gone; every writer here already treats a
    /// missing temp as a transient failure to retry on the next tick
    /// (see `write_heartbeat`'s and `write_beacon`'s callers), never as
    /// data loss, so this is the outcome such a producer must tolerate,
    /// not a race this function actually closes.
    pub(super) fn remove_if_same(&self, name: &str, expected: &CheckedEntry) -> io::Result<bool> {
        let Some(current) = self.stat_entry(name)? else {
            return Ok(false);
        };
        if is_symlink_mode(current.st_mode) || (current.st_mode & libc::S_IFMT) != libc::S_IFREG {
            return Err(io_other(format!(
                "refusing to remove replaced walpin sidecar entry {name:?}"
            )));
        }
        // st_dev is u64 on Linux and i32 on macOS; widening first keeps the
        // conversion real on both targets.
        let current_device = u64::try_from(i128::from(current.st_dev)).unwrap_or(u64::MAX);
        let current_inode = current.st_ino;
        if current_device != expected.device || current_inode != expected.inode {
            return Ok(false);
        }
        #[cfg(test)]
        if let Some(hook) = take_remove_if_same_race_hook() {
            hook();
        }
        self.unlink_tolerant(name)?;
        Ok(true)
    }

    /// List entry names via `fdopendir` on a DUPLICATE of this fd (the
    /// original stays owned by `self`) — never re-resolves the
    /// directory by path.
    /// List up to `max` non-hidden entry names and up to `max` recognized
    /// producer temp names, plus whether more remained. Bounding happens
    /// HERE, at the `readdir` loop, so
    /// directory content cannot inflate either the allocation or the
    /// iteration work done by an enumeration pass — a truncated listing
    /// is reported to the caller, never silently clipped. Unrecognized
    /// dot-names are skipped; the producer-owned
    /// `.<pid>.(json|beacon).tmp` forms are retained in their own bounded
    /// lane so housekeeping can reap proven crash residue without letting
    /// junk consume the ordinary record budget. Every entry still counts
    /// toward the raw `RAW_SCAN_FACTOR * max` scan bound. A `readdir`
    /// read error mid-walk is distinguished from ordinary end-of-
    /// directory via `errno` (cleared before every call) and returned as
    /// `Err`, never folded into a `truncated: false` listing that would
    /// let a caller believe it saw every entry when it did not.
    pub(super) fn list_names(&self, max: usize) -> io::Result<(Vec<String>, Vec<String>, bool)> {
        // SAFETY: duplicates a live, open fd; the duplicate is uniquely
        // owned by this call and handed to `fdopendir` below.
        let dup_fd = unsafe { libc::dup(self.raw()) };
        if dup_fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `dup_fd` is valid and uniquely owned; `fdopendir`
        // takes ownership of it on success.
        let dirp = unsafe { libc::fdopendir(dup_fd) };
        if dirp.is_null() {
            let err = io::Error::last_os_error();
            // SAFETY: `dup_fd` is still owned by us since `fdopendir` failed.
            unsafe { libc::close(dup_fd) };
            return Err(err);
        }
        let raw_scan_limit = max.saturating_mul(RAW_SCAN_FACTOR).max(max);
        let mut raw_scanned: usize = 0;
        let mut names = Vec::new();
        let mut producer_temps = Vec::new();
        let mut truncated = false;
        loop {
            // `readdir` leaves `errno` untouched on EOF; clearing it
            // here is what makes that observable as distinct from an
            // error below.
            clear_errno();
            let entry = if let Some(errno) = take_list_names_readdir_fault() {
                // SAFETY: sets the same thread-local errno cell a
                // genuine failed `readdir()` would have set.
                unsafe { *errno_location() = errno };
                std::ptr::null_mut()
            } else {
                // SAFETY: `dirp` is a valid, open `DIR*` for this whole loop.
                unsafe { libc::readdir(dirp) }
            };
            if entry.is_null() {
                if readdir_null_is_error(current_errno()) {
                    let err = io::Error::last_os_error();
                    // SAFETY: `dirp` is still open and owned by this
                    // call; this is the one closedir on the error path,
                    // matching the one closedir on the `Ok` path below.
                    unsafe { libc::closedir(dirp) };
                    return Err(err);
                }
                break;
            }
            if raw_scanned == raw_scan_limit {
                truncated = true;
                break;
            }
            raw_scanned += 1;
            // SAFETY: `d_name` is NUL-terminated, so its first byte is
            // always in bounds.
            let first = unsafe { *(*entry).d_name.as_ptr() };
            if first == b'.' as libc::c_char {
                // SAFETY: the entry remains valid until the next readdir;
                // copy only a recognized bounded producer-temp name.
                let candidate = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }
                    .to_string_lossy()
                    .into_owned();
                if super::producer_temp_identity(&candidate).is_some() {
                    if producer_temps.len() == max {
                        truncated = true;
                        break;
                    }
                    producer_temps.push(candidate);
                }
                continue;
            }
            if names.len() == max {
                truncated = true;
                break;
            }
            // SAFETY: `entry` is valid until the next `readdir`/
            // `closedir` call; the name is copied out before either.
            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }
                .to_string_lossy()
                .into_owned();
            names.push(name);
        }
        // SAFETY: `dirp` was successfully opened above and not yet closed.
        unsafe { libc::closedir(dirp) };
        Ok((names, producer_temps, truncated))
    }
}

pub(super) fn is_process_alive(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    // SAFETY: signal 0 sends no signal; it only probes existence/permission.
    let rc = unsafe { libc::kill(pid, 0) };
    if rc == 0 {
        return true;
    }
    io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}
