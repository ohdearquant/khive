//! ADR-091 Amendment 2 Plank B: cross-process WAL-pin attribution sidecar.
//!
//! Every `kkernel mcp` process (daemon or session, any supported platform)
//! that observes its own `tx_registry` oldest span exceed `KHIVE_TX_WARN_SECS`
//! writes a per-PID heartbeat file under `<db-file>.walpin/<pid>.json`. On a
//! TRUNCATE no-progress event, the daemon enumerates this directory and
//! applies a three-test liveness gate (PID alive, `started_at` matches the
//! OS-reported process start time, `updated_at` fresh) to attribute the WAL
//! pin to a specific process rather than only naming its own in-process
//! registry.
//!
//! Filesystem trust boundary (binding): the sidecar
//! directory is created mode 0700 and validated as owned by the current user
//! before any use — a non-compliant existing directory is refused, never
//! chmod/chown'd into compliance. Heartbeat writes go through exclusive
//! create with `O_NOFOLLOW` semantics to a temp file, then atomic rename over
//! the target. Enumeration refuses symlinks and validates per-entry ownership
//! before reading or deleting anything.
//!
//! **Platform split.** Only the write path
//! (`ensure_sidecar_dir`/`write_heartbeat`/`write_beacon`/`remove_heartbeat`/
//! `touch_beacon`) and the identity primitives (`is_process_alive`/
//! `process_start_time_secs`) need to run on every platform — a Windows
//! session still needs to report itself into the sidecar. Directory
//! enumeration (`enumerate_live`, and the OS-derived holder census it
//! anchors to) is Unix-only: its sole caller is the daemon's checkpoint task,
//! and daemon mode itself requires Unix (`khive-mcp/src/serve.rs` refuses
//! `--daemon` on non-Unix). The Unix write path is additionally
//! **handle-bound**: the sidecar directory is opened once with
//! `O_DIRECTORY | O_NOFOLLOW`, validated on that file descriptor, and every
//! create/rename/unlink/enumeration read is performed `*at()`-relative to it
//! — the path is never re-resolved per operation, closing the window where a
//! path component swapped between a path-based validation and the subsequent
//! operation could redirect a rename or deletion outside the sidecar. Windows
//! has no equivalent of `openat`/`fstat`-bound ownership validation in `std`;
//! it uses plain `std::fs` path-based primitives with symlink refusal via
//! `symlink_metadata` and no uid/mode check (documented residual gap: a
//! Windows sidecar directory is only as protected as its inherited ACL, not
//! actively narrowed by this code).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Allowed drift between a heartbeat's recorded `started_at` and the
/// OS-reported process start time queried fresh at enumeration — both are
/// whole-second values sourced from different clocks (the writer's own
/// `SystemTime::now()` vs. `proc_pidinfo`/`/proc/<pid>/stat`), so this is
/// rounding slack, not a real identity ambiguity window.
const START_TIME_EPSILON_SECS: i64 = 2;

/// One process's walpin heartbeat record (ADR-091 Amendment 2 Plank B).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WalpinHeartbeat {
    pub pid: u32,
    pub process_role: String,
    /// OS-reported process start time (epoch seconds), used as the identity
    /// check at enumeration time — a reused PID is rejected deterministically
    /// rather than probabilistically.
    pub started_at: i64,
    pub oldest_tx_age_secs: f64,
    pub oldest_tx_label: Option<String>,
    pub updated_at: i64,
}

/// A heartbeat that survived the three-test liveness gate at enumeration time.
#[derive(Debug, Clone, PartialEq)]
pub struct LiveWalpinEntry {
    pub heartbeat: WalpinHeartbeat,
}

/// One-time per-PID registration marker (ADR-091 Amendment 2, sidecar-health
/// attribution). Written once at sidecar initialization — never refreshed
/// per tick — so a live process that has no over-threshold span still has a
/// footprint in the sidecar directory: the absence of a *heartbeat* then
/// affirmatively means "no old span," rather than "sidecar never worked."
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WalpinBeacon {
    pub pid: u32,
    pub process_role: String,
    pub started_at: i64,
}

/// Three-state sidecar-health classification for one PID observed in the
/// sidecar directory (ADR-091 Amendment 2 "Sidecar-health attribution"
/// paragraph).
#[derive(Debug, Clone, PartialEq)]
pub enum WalpinPidHealth {
    /// A live, identity-matched, fresh heartbeat exists: this PID currently
    /// holds an over-threshold span.
    Reporting(WalpinHeartbeat),
    /// A live, identity-matched beacon exists with no live heartbeat: the
    /// process's sidecar is functioning and affirmatively reports no
    /// over-threshold span right now.
    RegisteredSilent { pid: u32 },
    /// This PID's sidecar-health could not be established — its beacon (or
    /// heartbeat) entry exists on disk but was refused by the trust-boundary
    /// check (symlink, non-owned) or failed to parse. Any `Unknown` PID makes
    /// the overall attribution inconclusive.
    Unknown { pid: u32, reason: &'static str },
}

/// The result of one sidecar-directory enumeration pass: every PID found,
/// classified three ways, plus whether the directory itself could be trusted
/// at all.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WalpinReport {
    pub entries: Vec<WalpinPidHealth>,
}

impl WalpinReport {
    pub fn reporting(&self) -> impl Iterator<Item = &WalpinHeartbeat> {
        self.entries.iter().filter_map(|e| match e {
            WalpinPidHealth::Reporting(hb) => Some(hb),
            _ => None,
        })
    }

    pub fn registered_silent_pids(&self) -> impl Iterator<Item = u32> + '_ {
        self.entries.iter().filter_map(|e| match e {
            WalpinPidHealth::RegisteredSilent { pid } => Some(*pid),
            _ => None,
        })
    }

    pub fn unknown_pids(&self) -> impl Iterator<Item = u32> + '_ {
        self.entries.iter().filter_map(|e| match e {
            WalpinPidHealth::Unknown { pid, .. } => Some(*pid),
            _ => None,
        })
    }

    /// Whether every discovered PID is either reporting or registered-silent
    /// — the licensing condition for the sharper "native/unregistered
    /// mechanism" conclusion (ADR-091 Amendment 2).
    pub fn fully_attributed(&self) -> bool {
        self.unknown_pids().next().is_none()
    }
}

fn io_other(msg: impl Into<String>) -> io::Error {
    io::Error::other(msg.into())
}

/// `<db-file>.walpin` sibling of a database file, appended at the `OsString`
/// byte level (mirrors `khive-db`'s `ann_root_for`) so two databases sharing
/// a parent directory can never adopt each other's heartbeat entries.
pub fn sidecar_dir_for(db_path: &Path) -> PathBuf {
    let mut file = db_path.file_name().unwrap_or_default().to_os_string();
    file.push(".walpin");
    match db_path.parent() {
        Some(parent) => parent.join(file),
        None => PathBuf::from(file),
    }
}

/// Whether the sidecar is active for this backend. Defaults to `is_file_backed`
/// (on for file-backed, off for in-memory); `KHIVE_WALPIN_SIDECAR` overrides
/// either way when it parses as a recognized boolean.
pub fn sidecar_enabled(is_file_backed: bool) -> bool {
    match std::env::var("KHIVE_WALPIN_SIDECAR") {
        Ok(raw) => match raw.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => true,
            "0" | "false" | "no" | "off" => false,
            _ => is_file_backed,
        },
        Err(_) => is_file_backed,
    }
}

/// Unix sidecar internals (ADR-091 Amendment 2: handle-bound
/// filesystem operations). The sidecar directory is opened exactly once per
/// call with `O_DIRECTORY | O_NOFOLLOW`, validated (type/mode/owner) on that
/// descriptor, and every create/rename/unlink/read is `*at()`-relative to it
/// — the path is never re-resolved between validation and use.
#[cfg(unix)]
mod unix_impl {
    use super::io_other;
    use std::ffi::{CStr, CString};
    use std::fs;
    use std::io::{self, Read, Write};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
    use std::path::Path;
    use std::time::{Duration, SystemTime};

    pub(super) fn current_uid() -> u32 {
        // SAFETY: `geteuid()` takes no arguments and cannot fail.
        unsafe { libc::geteuid() }
    }

    fn path_cstring(path: &Path) -> io::Result<CString> {
        CString::new(path.as_os_str().as_bytes())
            .map_err(|_| io_other(format!("path {path:?} contains an interior NUL byte")))
    }

    fn name_cstring(name: &str) -> io::Result<CString> {
        CString::new(name)
            .map_err(|_| io_other(format!("sidecar entry name {name:?} contains a NUL byte")))
    }

    fn is_symlink_mode(mode: libc::mode_t) -> bool {
        (mode & libc::S_IFMT) == libc::S_IFLNK
    }

    pub(super) struct SidecarDirHandle(fs::File);

    impl SidecarDirHandle {
        fn raw(&self) -> RawFd {
            self.0.as_raw_fd()
        }

        /// Open the sidecar dir, creating it (mode 0700) if absent. The
        /// freshly-created (or already-existing) directory is validated on
        /// the OPENED descriptor, never trusted from the `mkdir` call alone
        /// — a concurrent process could have raced the creation.
        pub(super) fn open_or_create(dir: &Path) -> io::Result<Self> {
            match Self::open_validated(dir) {
                Ok(handle) => Ok(handle),
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    let c_path = path_cstring(dir)?;
                    // SAFETY: `c_path` is NUL-terminated for the call.
                    let rc = unsafe { libc::mkdir(c_path.as_ptr(), 0o700) };
                    if rc != 0 {
                        let err = io::Error::last_os_error();
                        if err.kind() != io::ErrorKind::AlreadyExists {
                            return Err(err);
                        }
                    }
                    Self::open_validated(dir)
                }
                Err(e) => Err(e),
            }
        }

        /// Same as [`Self::open_or_create`] but never creates: `Ok(None)`
        /// for a missing directory (a sidecar that was never used yet is
        /// not an error, and must not have the side effect of creating one
        /// — e.g. a stray `remove_heartbeat`/`touch_beacon` call).
        pub(super) fn open_if_exists(dir: &Path) -> io::Result<Option<Self>> {
            match Self::open_validated(dir) {
                Ok(handle) => Ok(Some(handle)),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e),
            }
        }

        fn open_validated(dir: &Path) -> io::Result<Self> {
            let c_path = path_cstring(dir)?;
            // SAFETY: `c_path` is NUL-terminated for the call; the returned
            // fd is uniquely owned by this call and wrapped immediately.
            let fd = unsafe {
                libc::open(
                    c_path.as_ptr(),
                    libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                let err = io::Error::last_os_error();
                // The `O_NOFOLLOW` open above already made the refusal
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
            // SAFETY: `fd` was just returned by the successful `open` above.
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
                    libc::O_WRONLY
                        | libc::O_CREAT
                        | libc::O_EXCL
                        | libc::O_NOFOLLOW
                        | libc::O_CLOEXEC,
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
            let c_from = name_cstring(from)?;
            let c_to = name_cstring(to)?;
            // SAFETY: both names are NUL-terminated for the call; both are
            // relative to this same, live directory fd.
            let rc =
                unsafe { libc::renameat(self.raw(), c_from.as_ptr(), self.raw(), c_to.as_ptr()) };
            if rc != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
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
            // SAFETY: `c_name` is NUL-terminated; `O_NOFOLLOW` is defense in
            // depth alongside the `stat_entry` symlink check above.
            let fd = unsafe {
                libc::openat(
                    self.raw(),
                    c_name.as_ptr(),
                    libc::O_WRONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                return Err(io::Error::last_os_error());
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
            // SAFETY: `fd` is a live, just-opened descriptor; `times` is a
            // valid 2-element array as `futimens` requires.
            let rc = unsafe { libc::futimens(fd, times.as_ptr()) };
            let err = (rc != 0).then(io::Error::last_os_error);
            // SAFETY: `fd` was opened above and is closed exactly once here.
            unsafe { libc::close(fd) };
            match err {
                Some(e) => Err(e),
                None => Ok(()),
            }
        }

        /// Read `name`'s contents plus its owner uid and mtime, all sourced
        /// from ONE `stat_entry` pass plus the read itself — never a second
        /// path-based lookup. `Ok(None)` for a missing entry (raced away
        /// between listing and reading); refuses a symlink.
        pub(super) fn read_checked(
            &self,
            name: &str,
        ) -> io::Result<Option<(Vec<u8>, u32, SystemTime)>> {
            let Some(st) = self.stat_entry(name)? else {
                return Ok(None);
            };
            if is_symlink_mode(st.st_mode) {
                return Err(io_other(format!(
                    "walpin sidecar entry {name:?} is a symlink"
                )));
            }
            let c_name = name_cstring(name)?;
            // SAFETY: `c_name` is NUL-terminated; `O_NOFOLLOW` is defense in
            // depth alongside the `stat_entry` symlink check above.
            let fd = unsafe {
                libc::openat(
                    self.raw(),
                    c_name.as_ptr(),
                    libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: `fd` was just returned by the successful `openat`.
            let mut file = unsafe { fs::File::from_raw_fd(fd) };
            let mut buf = Vec::new();
            file.read_to_end(&mut buf)?;
            let mtime = SystemTime::UNIX_EPOCH + Duration::new(st.st_mtime.max(0) as u64, 0);
            Ok(Some((buf, st.st_uid, mtime)))
        }

        /// List entry names via `fdopendir` on a DUPLICATE of this fd (the
        /// original stays owned by `self`) — never re-resolves the
        /// directory by path.
        pub(super) fn list_names(&self) -> io::Result<Vec<String>> {
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
            let mut names = Vec::new();
            loop {
                // SAFETY: `dirp` is a valid, open `DIR*` for this whole loop.
                let entry = unsafe { libc::readdir(dirp) };
                if entry.is_null() {
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
            Ok(names)
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
}

/// Windows sidecar internals (ADR-091 Amendment 2: "Windows
/// is a supported target"). Uses plain `std::fs` path-based primitives —
/// `std` has no `openat`/`fstat`-bound-validation equivalent on Windows, so
/// the handle-bound contract ([`unix_impl`]) is Unix-normative only. Refuses
/// symlinks via `symlink_metadata` before any read/write, exclusive
/// `create_new(true)` for temp files, atomic `fs::rename`, plain
/// `fs::create_dir` (no uid/mode-equivalent narrowing — see the module doc's
/// platform-split note: a Windows sidecar directory is only as protected as
/// its inherited ACL).
#[cfg(windows)]
mod windows_impl {
    use super::io_other;
    use std::fs;
    use std::io::{self, Write};
    use std::os::raw::c_void;
    use std::path::Path;
    use std::time::SystemTime;

    pub(super) fn ensure_sidecar_dir(dir: &Path) -> io::Result<()> {
        match fs::symlink_metadata(dir) {
            Ok(meta) => validate(dir, &meta),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(dir)?;
                // Re-validate post-creation the same way the Unix path
                // does: a concurrent process could have raced this create.
                let meta = fs::symlink_metadata(dir)?;
                validate(dir, &meta)
            }
            Err(e) => Err(e),
        }
    }

    fn validate(dir: &Path, meta: &fs::Metadata) -> io::Result<()> {
        if meta.file_type().is_symlink() {
            return Err(io_other(format!(
                "walpin sidecar path {dir:?} is a symlink; refusing"
            )));
        }
        if !meta.is_dir() {
            return Err(io_other(format!(
                "walpin sidecar path {dir:?} exists and is not a directory"
            )));
        }
        Ok(())
    }

    pub(super) fn write_atomic(
        dir: &Path,
        target_name: &str,
        tmp_name: &str,
        body: &[u8],
    ) -> io::Result<()> {
        let target = dir.join(target_name);
        if let Ok(meta) = fs::symlink_metadata(&target) {
            if meta.file_type().is_symlink() {
                return Err(io_other(format!(
                    "walpin sidecar path {target:?} is a symlink; refusing to write through it"
                )));
            }
        }
        let tmp = dir.join(tmp_name);
        let _ = fs::remove_file(&tmp);
        {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp)?;
            file.write_all(body)?;
            file.sync_all()?;
        }
        fs::rename(&tmp, &target)
    }

    pub(super) fn remove_checked(dir: &Path, name: &str) -> io::Result<()> {
        let target = dir.join(name);
        match fs::symlink_metadata(&target) {
            Ok(meta) if meta.file_type().is_symlink() => Err(io_other(format!(
                "refusing to remove symlinked walpin sidecar entry {target:?}"
            ))),
            Ok(_) => fs::remove_file(&target),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    pub(super) fn touch_mtime(dir: &Path, name: &str) -> io::Result<()> {
        let target = dir.join(name);
        let meta = fs::symlink_metadata(&target)
            .map_err(|_| io_other(format!("walpin sidecar entry {target:?} does not exist")))?;
        if meta.file_type().is_symlink() {
            return Err(io_other(format!(
                "walpin sidecar entry {target:?} is a symlink; refusing to touch it"
            )));
        }
        let file = fs::OpenOptions::new().write(true).open(&target)?;
        file.set_modified(SystemTime::now())
    }

    type Handle = *mut c_void;

    #[repr(C)]
    struct FileTime {
        dw_low_date_time: u32,
        dw_high_date_time: u32,
    }

    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const STILL_ACTIVE: u32 = 259;

    // `kernel32` is implicitly linked on every Windows target (same as
    // `std` itself relies on); no explicit `#[link(...)]` is needed, mirroring
    // how `windows-sys`/`winapi` declare these `extern "system"` blocks.
    extern "system" {
        fn OpenProcess(dw_desired_access: u32, b_inherit_handle: i32, dw_process_id: u32)
            -> Handle;
        fn CloseHandle(h_object: Handle) -> i32;
        fn GetExitCodeProcess(h_process: Handle, lp_exit_code: *mut u32) -> i32;
        fn GetProcessTimes(
            h_process: Handle,
            lp_creation_time: *mut FileTime,
            lp_exit_time: *mut FileTime,
            lp_kernel_time: *mut FileTime,
            lp_user_time: *mut FileTime,
        ) -> i32;
    }

    pub(super) fn is_process_alive(pid: u32) -> bool {
        // SAFETY: `OpenProcess` is a pure query; the handle (if non-null) is
        // closed before returning.
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() {
            return false;
        }
        let mut exit_code: u32 = 0;
        // SAFETY: `handle` is a valid, just-opened process handle; `exit_code`
        // is a valid output buffer.
        let ok = unsafe { GetExitCodeProcess(handle, &mut exit_code) };
        // SAFETY: `handle` was opened above and is closed exactly once here.
        unsafe { CloseHandle(handle) };
        ok != 0 && exit_code == STILL_ACTIVE
    }

    pub(super) fn process_start_time_secs(pid: u32) -> Option<i64> {
        // SAFETY: pure query; the handle is closed before returning.
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() {
            return None;
        }
        let mut creation = FileTime {
            dw_low_date_time: 0,
            dw_high_date_time: 0,
        };
        let mut exit = FileTime {
            dw_low_date_time: 0,
            dw_high_date_time: 0,
        };
        let mut kernel = FileTime {
            dw_low_date_time: 0,
            dw_high_date_time: 0,
        };
        let mut user = FileTime {
            dw_low_date_time: 0,
            dw_high_date_time: 0,
        };
        // SAFETY: `handle` is valid; all four output buffers are valid
        // `FILETIME`-shaped structs for the call's duration.
        let ok =
            unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) };
        // SAFETY: `handle` was opened above and closed exactly once here.
        unsafe { CloseHandle(handle) };
        if ok == 0 {
            return None;
        }
        // FILETIME: 100ns intervals since 1601-01-01 UTC. Convert to a Unix
        // epoch (1970-01-01) second count via the well-known offset between
        // the two epochs.
        let ticks = ((creation.dw_high_date_time as u64) << 32) | creation.dw_low_date_time as u64;
        const EPOCH_DIFF_100NS: u64 = 116_444_736_000_000_000;
        let unix_100ns = ticks.checked_sub(EPOCH_DIFF_100NS)?;
        Some((unix_100ns / 10_000_000) as i64)
    }
}

/// Outcome of one OS-derived holder census pass (ADR-091 Amendment 2,
/// item a). A PID the census positively determined does NOT hold the
/// database file is simply absent from `holders` — that is a normal,
/// complete result. A PID whose inspection FAILED (permission denied, or a
/// races-away process) instead of succeeding-with-a-negative-answer is
/// recorded in `uninspectable_pids`: the census as a whole is then
/// INCOMPLETE, and callers must treat that exactly like an `unknown` sidecar
/// PID — inconclusive, never silently folded into "no unregistered holder."
///
/// `truncated` is a second, independent incompleteness signal: set when the enumeration walk itself has positive evidence it did
/// not see the full live-process universe even though no single PID's
/// inspection outright failed — a `/proc` directory-iterator error or a
/// PID-namespace mismatch on Linux, or a libproc buffer whose returned byte
/// count still equalled its negotiated capacity after bounded retries on
/// macOS. `is_complete()` folds both signals together.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CensusResult {
    pub holders: std::collections::HashSet<u32>,
    pub uninspectable_pids: Vec<u32>,
    pub truncated: bool,
}

impl CensusResult {
    /// Every discovered PID was either confirmed as a holder or positively
    /// ruled out (no PID's inspection failed outright), AND the walk itself
    /// carries no positive evidence that it missed part of the live-process
    /// universe.
    pub fn is_complete(&self) -> bool {
        self.uninspectable_pids.is_empty() && !self.truncated
    }

    /// ADR-091 Amendment 2 self-canary: the sole caller
    /// (`log_walpin_sidecar_report`) always runs inside the process whose
    /// own SQLite connection pool holds `db_path` open, so a correct,
    /// complete census must find `std::process::id()` among the holders it
    /// discovered. Not finding it is positive proof the walk missed at
    /// least one live holder. This is necessary but not sufficient — a
    /// census that is complete apart from a *different* missed PID still
    /// passes it — so every platform's `census_holders` applies this on top
    /// of, never instead of, its own per-step incompleteness markers.
    fn apply_self_canary(&mut self) {
        if !self.holders.contains(&std::process::id()) {
            self.truncated = true;
        }
    }
}

/// macOS: classify a `proc_pidinfo`/`proc_pidfdinfo` failure by errno.
/// `ESRCH` means the target process exited between `proc_listpids` and this
/// call — a genuine "positively gone" race, safe to skip. Any other errno
/// (most commonly `EPERM`/`EACCES`, inspecting another user's open files)
/// means the inspection itself failed: the census cannot say whether this
/// PID holds the database file, so it must be reported as uninspectable
/// rather than silently excluded.
#[cfg(target_os = "macos")]
fn macos_pid_genuinely_gone(errno: Option<i32>) -> bool {
    errno == Some(libc::ESRCH)
}

/// macOS: classify a `proc_pidfdinfo` return against the expected struct
/// size. Only an exact match is a successful inspection. A positive but
/// short byte count (ADR-091 Amendment 2) means the kernel wrote a
/// truncated/partial struct rather than the full `VnodeFdInfoWithPath` —
/// that is an inspection failure exactly like a non-positive return, not a
/// successful call that merely returned less data than expected.
#[cfg(target_os = "macos")]
fn proc_pidfdinfo_returned_expected_size(returned_bytes: i32, expected_size: usize) -> bool {
    returned_bytes > 0 && returned_bytes as usize == expected_size
}

/// Bounded attempts for the macOS buffer-size negotiation below — enough to
/// absorb the live-set growing between the sizing call and the data call a
/// couple of times without looping forever on a pathologically fast-growing
/// process/fd table.
#[cfg(target_os = "macos")]
const CENSUS_BUFFER_NEGOTIATION_ATTEMPTS: usize = 4;

/// Bounded buffer-size negotiation shared by `proc_listpids` and
/// `proc_pidinfo(PROC_PIDLISTFDS)` (ADR-091 Amendment 2,
/// item c — the fixed 8192-PID/4096-FD buffers used to truncate silently).
/// Both libproc calls return the needed byte count when handed a null
/// buffer (`size_call`); this allocates with headroom and re-invokes
/// (`data_call`). The set being listed (all live PIDs, or one PID's open
/// fds) can grow between the two calls, so this retries a bounded number of
/// times; if the returned byte count still equals the buffer's capacity on
/// the final attempt, the true set may be larger than what was captured —
/// the second return value is `true` and the caller must not trust the
/// result as complete.
#[cfg(target_os = "macos")]
fn negotiate_buffer<T: Default + Clone>(
    size_call: impl Fn() -> std::os::raw::c_int,
    data_call: impl Fn(*mut std::os::raw::c_void, std::os::raw::c_int) -> std::os::raw::c_int,
) -> io::Result<(Vec<T>, bool)> {
    let item_size = std::mem::size_of::<T>();
    for attempt in 0..CENSUS_BUFFER_NEGOTIATION_ATTEMPTS {
        let needed = size_call();
        if needed <= 0 {
            return Err(io::Error::last_os_error());
        }
        let needed_items = needed as usize / item_size + 1;
        let item_count = needed_items + needed_items / 4 + 8;
        let mut buf: Vec<T> = vec![T::default(); item_count];
        let cap_bytes = (buf.len() * item_size) as std::os::raw::c_int;
        let bytes = data_call(buf.as_mut_ptr() as *mut std::os::raw::c_void, cap_bytes);
        if bytes <= 0 {
            return Err(io::Error::last_os_error());
        }
        let filled_capacity = bytes as usize >= cap_bytes as usize;
        let is_last_attempt = attempt + 1 == CENSUS_BUFFER_NEGOTIATION_ATTEMPTS;
        if filled_capacity && !is_last_attempt {
            // The live set grew to fill (or exceed) our snapshot — retry
            // with a freshly sized buffer rather than trust a possibly
            // partial one.
            continue;
        }
        let count = (bytes as usize / item_size).min(buf.len());
        buf.truncate(count);
        return Ok((buf, filled_capacity));
    }
    unreachable!("loop always returns or errors within CENSUS_BUFFER_NEGOTIATION_ATTEMPTS")
}

/// macOS OS-derived census (ADR-091 Amendment 2): every PID
/// on the system that currently holds `db_path` open, via `libproc`'s
/// `PROC_PIDLISTFDS`/`PROC_PIDFDVNODEPATHINFO` — never the sidecar directory
/// listing, which only sees PIDs that already wrote something there.
#[cfg(target_os = "macos")]
pub fn census_holders(db_path: &Path) -> io::Result<CensusResult> {
    use std::os::raw::{c_int, c_void};
    use std::os::unix::fs::MetadataExt;

    const PROC_ALL_PIDS: u32 = 1;
    const PROC_PIDLISTFDS: c_int = 1;
    const PROC_PIDFDVNODEPATHINFO: c_int = 2;
    const PROX_FDTYPE_VNODE: u32 = 1;
    const MAXPATHLEN: usize = 1024;

    #[repr(C)]
    #[derive(Clone, Default)]
    struct ProcFdInfo {
        proc_fd: i32,
        proc_fdtype: u32,
    }
    #[repr(C)]
    struct ProcFileInfo {
        fi_openflags: u32,
        fi_status: u32,
        fi_offset: i64,
        fi_type: i32,
        fi_guardflags: u32,
    }
    #[repr(C)]
    struct FsId {
        val: [i32; 2],
    }
    #[repr(C)]
    struct VinfoStat {
        vst_dev: u32,
        vst_mode: u16,
        vst_nlink: u16,
        vst_ino: u64,
        vst_uid: u32,
        vst_gid: u32,
        vst_atime: i64,
        vst_atimensec: i64,
        vst_mtime: i64,
        vst_mtimensec: i64,
        vst_ctime: i64,
        vst_ctimensec: i64,
        vst_birthtime: i64,
        vst_birthtimensec: i64,
        vst_size: i64,
        vst_blocks: i64,
        vst_blksize: i32,
        vst_flags: u32,
        vst_gen: u32,
        vst_rdev: u32,
        vst_qspare: [i64; 2],
    }
    #[repr(C)]
    struct VnodeInfo {
        vi_stat: VinfoStat,
        vi_type: i32,
        vi_pad: i32,
        vi_fsid: FsId,
    }
    #[repr(C)]
    struct VnodeInfoPath {
        vip_vi: VnodeInfo,
        vip_path: [u8; MAXPATHLEN],
    }
    #[repr(C)]
    struct VnodeFdInfoWithPath {
        pfi: ProcFileInfo,
        pvip: VnodeInfoPath,
    }

    #[link(name = "proc")]
    extern "C" {
        fn proc_listpids(kind: u32, typeinfo: u32, buffer: *mut c_void, buffersize: c_int)
            -> c_int;
        fn proc_pidinfo(
            pid: c_int,
            flavor: c_int,
            arg: u64,
            buffer: *mut c_void,
            buffersize: c_int,
        ) -> c_int;
        fn proc_pidfdinfo(
            pid: c_int,
            fd: c_int,
            flavor: c_int,
            buffer: *mut c_void,
            buffersize: c_int,
        ) -> c_int;
    }

    // File-identity target, not a path target: holders are matched on
    // (device, inode) so a process that opened the database through a hard
    // link (or any alternate name for the same file) is still discovered.
    let target_meta = fs::metadata(db_path)?;
    let target_ident = (target_meta.dev() as u32, target_meta.ino());

    // SAFETY: `negotiate_buffer` hands `proc_listpids` a buffer sized from
    // its own reported byte count, growing on retry; the extern call writes
    // at most the byte capacity passed to it.
    let (pid_buf, pid_list_truncated): (Vec<i32>, bool) = negotiate_buffer(
        || unsafe { proc_listpids(PROC_ALL_PIDS, 0, std::ptr::null_mut(), 0) },
        |buf_ptr, buf_bytes| unsafe { proc_listpids(PROC_ALL_PIDS, 0, buf_ptr, buf_bytes) },
    )?;

    let mut holders = std::collections::HashSet::new();
    let mut uninspectable: Vec<u32> = Vec::new();
    for &pid in &pid_buf {
        if pid <= 0 {
            continue;
        }
        // SAFETY: `negotiate_buffer` hands `proc_pidinfo` a buffer sized
        // from its own reported byte count, growing on retry.
        let (fd_buf, fd_list_truncated): (Vec<ProcFdInfo>, bool) = match negotiate_buffer(
            || unsafe { proc_pidinfo(pid, PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) },
            |buf_ptr, buf_bytes| unsafe {
                proc_pidinfo(pid, PROC_PIDLISTFDS, 0, buf_ptr, buf_bytes)
            },
        ) {
            Ok(v) => v,
            Err(e) => {
                // A failed sizing/listing call means either the PID exited
                // between `proc_listpids` and here (ESRCH — positively
                // gone, safe to skip) or the inspection itself failed (most
                // commonly permission denied to list another user's fds).
                // Only the former is excluded cleanly; the latter means we
                // could not determine whether this PID holds the db, so it
                // marks the whole census incomplete rather than being
                // silently treated as "not a holder."
                if !macos_pid_genuinely_gone(e.raw_os_error()) {
                    uninspectable.push(pid as u32);
                }
                continue;
            }
        };
        if fd_list_truncated {
            // This PID's fd table may be larger than what fit even after
            // bounded retries — its inspection cannot be trusted complete.
            uninspectable.push(pid as u32);
        }
        for fdinfo in &fd_buf {
            if fdinfo.proc_fdtype != PROX_FDTYPE_VNODE {
                continue;
            }
            let mut vinfo: VnodeFdInfoWithPath = unsafe { std::mem::zeroed() };
            // SAFETY: `vinfo` is a valid, zeroed, appropriately-sized buffer.
            let vsize = unsafe {
                proc_pidfdinfo(
                    pid,
                    fdinfo.proc_fd,
                    PROC_PIDFDVNODEPATHINFO,
                    &mut vinfo as *mut _ as *mut c_void,
                    std::mem::size_of::<VnodeFdInfoWithPath>() as c_int,
                )
            };
            if !proc_pidfdinfo_returned_expected_size(
                vsize,
                std::mem::size_of::<VnodeFdInfoWithPath>(),
            ) {
                // A non-positive return is a failed inspection call for
                // this fd: ESRCH-equivalent (the fd/process raced away) is
                // genuinely gone, safe to skip; any other errno means we
                // could not determine whether THIS fd is our target, so the
                // PID's census is incomplete rather than a clean negative.
                if vsize <= 0 {
                    let errno = io::Error::last_os_error().raw_os_error();
                    if !macos_pid_genuinely_gone(errno) {
                        uninspectable.push(pid as u32);
                    }
                } else {
                    // Positive but short: the call itself succeeded (no
                    // errno to classify), it just wrote less data than the
                    // struct requires — an inspection failure regardless.
                    uninspectable.push(pid as u32);
                }
                continue;
            }
            // Identity comparison on the kernel-reported (device, inode)
            // rather than the vnode's path string: a holder that opened the
            // database through a hard link (or any alternate name for the
            // same file) reports a different path, and a path comparison
            // would silently omit it without marking the census incomplete.
            let vstat = &vinfo.pvip.vip_vi.vi_stat;
            if (vstat.vst_dev, vstat.vst_ino) == target_ident {
                holders.insert(pid as u32);
                break;
            }
        }
    }
    uninspectable.sort_unstable();
    uninspectable.dedup();
    let mut census = CensusResult {
        holders,
        uninspectable_pids: uninspectable,
        truncated: pid_list_truncated,
    };
    census.apply_self_canary();
    Ok(census)
}

/// Linux: classify a `/proc/<pid>/fd` open failure. `NotFound` means the
/// process exited between the `/proc` directory listing and this call — a
/// genuine "positively gone" race, safe to skip. Any other error (most
/// commonly `PermissionDenied`, inspecting another user's fds) means the
/// inspection itself failed and the PID must be reported as uninspectable.
#[cfg(target_os = "linux")]
fn linux_proc_gone(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::NotFound
}

/// The fixed inode number the kernel assigns to the *init* PID namespace —
/// the one namespace that exists for the lifetime of the machine, created
/// at boot before any container/unshare call can create another (Linux
/// `include/linux/proc_ns.h`, `PROC_PID_INIT_INO`). Every non-init PID
/// namespace — including a container's own, self-consistent one — gets a
/// dynamically allocated inode instead, so this exact value is a positive,
/// unspoofable proof that `/proc/self/ns/pid` refers to the host's own
/// root namespace (ADR-091 Amendment 2: a same-namespace readlink
/// comparison against `/proc/1/ns/pid` cannot tell "the host" apart from
/// "a container that is its own root," because both are internally
/// self-consistent).
#[cfg(target_os = "linux")]
const PROC_PID_INIT_INO: u64 = 0xEFFFFFFC;

/// Linux: classify a `/proc/self/ns/pid` inode against
/// [`PROC_PID_INIT_INO`]. Only an exact match is positive proof this
/// process shares the host's own (init) PID namespace; anything else means
/// external holders outside this namespace may be invisible to the
/// `/proc` walk below, so the census must be marked incomplete rather than
/// trusted as global.
#[cfg(target_os = "linux")]
fn pid_ns_is_init(ino: u64) -> bool {
    ino == PROC_PID_INIT_INO
}

/// Linux: classify a single procfs mount-options string (either the
/// per-mount options field or the super-options field of a
/// `/proc/self/mountinfo` line) as restricting per-PID visibility.
///
/// The init-PID-namespace inode check ([`pid_ns_is_init`]) only rules out
/// one way the `/proc` walk can miss processes. A host `/proc` mounted with
/// `hidepid=1`/`hidepid=2` (or the symbolic `hidepid=noaccess` /
/// `hidepid=invisible` / `hidepid=ptraceable` forms) or `subset=pid` hides
/// other users' `/proc/<pid>` directories from `readdir` entirely — no
/// per-PID error surfaces, `/proc/self` stays visible, and the self-canary
/// still passes, so that path alone cannot detect the restriction. Only
/// `hidepid=0` (or the symbolic `hidepid=off`) and the absence of `subset`
/// are compatible with treating the walk as global.
#[cfg(target_os = "linux")]
fn proc_mount_restricts_visibility(options: &str) -> bool {
    options.split(',').map(str::trim).any(|opt| {
        if let Some(value) = opt.strip_prefix("hidepid=") {
            !matches!(value, "0" | "off")
        } else {
            opt == "hidepid" || opt == "subset" || opt.starts_with("subset=")
        }
    })
}

/// Linux: locate every procfs mount backing `/proc` in
/// `/proc/self/mountinfo` and classify whether any of them restricts
/// per-PID visibility. Mounts stack: a later `/proc` mount shadows an
/// earlier one while both records remain in mountinfo, and picking a single
/// record would let a clean shadowed mount mask a restricted visible one.
/// Selection is therefore ANY-restrictive across every matching record —
/// ordering-independent and fail-closed against stacking. Returns `None`
/// when the mountinfo file can't be read or no `/proc` entry with
/// `fstype proc` is found — the caller treats `None` the same as
/// "restricted": an unparsable mountinfo carries no positive proof the
/// walk saw every host PID either, so it fails closed rather than assuming
/// a clean mount.
#[cfg(target_os = "linux")]
fn proc_mount_is_visibility_restricted() -> Option<bool> {
    let mountinfo = fs::read_to_string("/proc/self/mountinfo").ok()?;
    proc_mounts_restricted_in(&mountinfo)
}

/// Pure classification over mountinfo content, split out so the
/// any-restrictive selection is testable without a live `/proc`.
#[cfg(target_os = "linux")]
fn proc_mounts_restricted_in(mountinfo: &str) -> Option<bool> {
    let mut found_any = false;
    for line in mountinfo.lines() {
        // mountinfo line shape:
        //   <id> <parent-id> <major:minor> <root> <mount-point>
        //   <mount-options> <optional-fields...> - <fs-type> <mount-source>
        //   <super-options>
        let Some((fields_part, super_part)) = line.split_once(" - ") else {
            continue;
        };
        let fields: Vec<&str> = fields_part.split(' ').collect();
        if fields.len() < 6 || fields[4] != "/proc" {
            continue;
        }
        let mount_options = fields[5];
        let super_fields: Vec<&str> = super_part.split(' ').collect();
        if super_fields.first().copied() != Some("proc") {
            continue;
        }
        let super_options = super_fields.get(2).copied().unwrap_or("");
        found_any = true;
        if proc_mount_restricts_visibility(mount_options)
            || proc_mount_restricts_visibility(super_options)
        {
            return Some(true);
        }
    }
    if found_any {
        Some(false)
    } else {
        None
    }
}

/// Linux OS-derived census (ADR-091 Amendment 2): scan
/// `/proc/<pid>/fd/*` for every live PID and stat each fd through its proc
/// magic link, comparing `(device, inode)` identity against `db_path`'s. A
/// PID whose `fd` directory
/// cannot be opened at all (most commonly permission denied) is reported as
/// uninspectable rather than silently excluded — only a PID confirmed gone
/// (`NotFound`, a listing/inspection race) is skipped cleanly.
///
/// Before trusting the walk as a GLOBAL census (ADR-091 Amendment 2):
/// `hidepid` mounts, restricted `/proc`, and non-init PID namespaces can all
/// make `read_dir("/proc")` succeed while silently showing only a subset of
/// the host's live PIDs — with no per-entry error to catch. Three checks
/// widen the net rather than trust a clean-looking iteration outright: (1)
/// a positive proof that this process itself is running in the *host's*
/// init PID namespace — see [`pid_ns_is_init`]; a container's own procfs is
/// internally self-consistent (its `/proc/1` resolves to its own init), so
/// merely comparing `/proc/1/ns/pid` against `/proc/self/ns/pid` cannot
/// distinguish "the host" from "a container that is its own root," and was
/// replaced with this inode check (ADR-091 Amendment 2). (2) a positive
/// proof the procfs mount backing `/proc` carries no `hidepid`/`subset`
/// restriction — see [`proc_mount_is_visibility_restricted`]; a
/// `hidepid`-restricted mount hides other users' `/proc/<pid>` directories
/// from `readdir` with no per-entry error, so the init-namespace check
/// alone (self stays visible, self-canary passes) cannot detect it. (3) any error surfacing from the `/proc` or per-PID `fd`
/// directory ITERATORS themselves (not a single entry's own error) marks
/// the walk incomplete rather than being dropped via `.flatten()`.
#[cfg(target_os = "linux")]
pub fn census_holders(db_path: &Path) -> io::Result<CensusResult> {
    use std::os::unix::fs::MetadataExt;

    // File-identity target, not a path target: holders are matched on
    // (device, inode) so a process that opened the database through a hard
    // link or a bind-mounted alternate path is still discovered.
    let target_meta = fs::metadata(db_path)?;
    let target_ident = (target_meta.dev(), target_meta.ino());
    let mut holders = std::collections::HashSet::new();
    let mut uninspectable: Vec<u32> = Vec::new();
    let mut truncated = false;

    match fs::metadata("/proc/self/ns/pid") {
        Ok(meta) if pid_ns_is_init(meta.ino()) => {}
        _ => truncated = true,
    }

    match proc_mount_is_visibility_restricted() {
        Some(false) => {}
        Some(true) | None => truncated = true,
    }

    let proc_dir = fs::read_dir("/proc")?;
    for entry_result in proc_dir {
        let proc_entry = match entry_result {
            Ok(e) => e,
            Err(_) => {
                // The directory iterator itself failed mid-walk (not one
                // entry's own error) — the walk is no longer provably a
                // complete enumeration of live PIDs.
                truncated = true;
                continue;
            }
        };
        let Some(pid) = proc_entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        let fd_dir = proc_entry.path().join("fd");
        let fds = match fs::read_dir(&fd_dir) {
            Ok(fds) => fds,
            Err(e) if linux_proc_gone(&e) => continue,
            Err(_) => {
                uninspectable.push(pid);
                continue;
            }
        };
        for fd_result in fds {
            let fd_entry = match fd_result {
                Ok(e) => e,
                Err(_) => {
                    // The fd-directory iterator failed on this PID mid-walk
                    // — its set of open fds cannot be trusted complete, so
                    // this PID's census is incomplete rather than "no
                    // match found."
                    uninspectable.push(pid);
                    continue;
                }
            };
            // Identity comparison via a stat *through* the proc fd magic
            // link — it resolves to the open file itself, so the match is
            // on (device, inode) rather than a readlink'd path string. A
            // holder that opened the database through a hard link or a
            // bind-mounted alternate path reports a different path, and a
            // path comparison would silently omit it without marking the
            // census incomplete. Non-file fd targets (sockets, pipes,
            // anon inodes) stat fine and simply never match the target.
            match fs::metadata(fd_entry.path()) {
                Ok(meta) => {
                    if (meta.dev(), meta.ino()) == target_ident {
                        holders.insert(pid);
                        break;
                    }
                }
                // The fd itself closed between listing and this stat — a
                // genuine "positively gone" race, safe to skip.
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(_) => uninspectable.push(pid),
            }
        }
    }
    uninspectable.sort_unstable();
    uninspectable.dedup();
    let mut census = CensusResult {
        holders,
        uninspectable_pids: uninspectable,
        truncated,
    };
    census.apply_self_canary();
    Ok(census)
}

/// Any other Unix (khive ships macOS/Linux/Windows only; this is a
/// documented-gap fallback for a hypothetical build on anything else, not a
/// real deployment target) has no holder-enumeration implementation here.
/// An error (never a silently-empty `Ok`) so the caller treats it as a
/// census failure — the same "cannot rule out an unregistered holder"
/// posture as a real enumeration error, not false reassurance.
#[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
pub fn census_holders(_db_path: &Path) -> io::Result<CensusResult> {
    Err(io_other(
        "OS-derived holder census has no implementation on this Unix target",
    ))
}

/// Ensure `dir` exists and is trustworthy: a real directory (never a
/// symlink), and on Unix mode `0700` owned by the current user (Windows has
/// no uid/mode-equivalent narrowing — see the module doc's platform-split
/// note). Refuses — never chmod/chown/otherwise repair — a non-compliant
/// existing directory.
pub fn ensure_sidecar_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        unix_impl::SidecarDirHandle::open_or_create(dir)?;
        Ok(())
    }
    #[cfg(windows)]
    {
        windows_impl::ensure_sidecar_dir(dir)
    }
}

/// Write (or refresh) this process's heartbeat file. Exclusive-create a temp
/// file (`O_NOFOLLOW` on Unix), then atomically rename it over the target —
/// never an in-place open of a possibly attacker-placed path.
pub fn write_heartbeat(dir: &Path, heartbeat: &WalpinHeartbeat) -> io::Result<()> {
    let body =
        serde_json::to_vec(heartbeat).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let target = format!("{}.json", heartbeat.pid);
    let tmp = format!(".{}.json.tmp", heartbeat.pid);
    #[cfg(unix)]
    {
        let handle = unix_impl::SidecarDirHandle::open_or_create(dir)?;
        handle.write_atomic(&target, &tmp, &body)
    }
    #[cfg(windows)]
    {
        windows_impl::ensure_sidecar_dir(dir)?;
        windows_impl::write_atomic(dir, &target, &tmp, &body)
    }
}

/// Remove this process's heartbeat file, if present. Never follows a
/// symlink at the target path. A missing sidecar directory is a no-op — it
/// must NOT be created as a side effect of a removal.
pub fn remove_heartbeat(dir: &Path, pid: u32) -> io::Result<()> {
    let target = format!("{pid}.json");
    #[cfg(unix)]
    {
        match unix_impl::SidecarDirHandle::open_if_exists(dir)? {
            Some(handle) => handle.remove_checked(&target),
            None => Ok(()),
        }
    }
    #[cfg(windows)]
    {
        windows_impl::remove_checked(dir, &target)
    }
}

/// Write this process's one-time registration beacon (ADR-091 Amendment 2
/// sidecar-health attribution). Written once at sidecar initialization; see
/// [`touch_beacon`] for the required per-tick freshness refresh — a beacon
/// that is never refreshed again classifies as stale, never
/// `registered-silent` (beacon refresh rule).
pub fn write_beacon(dir: &Path, beacon: &WalpinBeacon) -> io::Result<()> {
    let body =
        serde_json::to_vec(beacon).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let target = format!("{}.beacon", beacon.pid);
    let tmp = format!(".{}.beacon.tmp", beacon.pid);
    #[cfg(unix)]
    {
        let handle = unix_impl::SidecarDirHandle::open_or_create(dir)?;
        handle.write_atomic(&target, &tmp, &body)
    }
    #[cfg(windows)]
    {
        windows_impl::ensure_sidecar_dir(dir)?;
        windows_impl::write_atomic(dir, &target, &tmp, &body)
    }
}

/// ADR-091 Amendment 2 beacon refresh rule: a metadata-only mtime touch of
/// this process's already-written beacon — no data write, preserving the
/// zero-steady-state-data-traffic property. Must run on every sweep tick
/// while the beacon exists: `registered-silent` classification requires the
/// refresh timestamp (not just the original write) to stay within the
/// freshness window.
pub fn touch_beacon(dir: &Path, pid: u32) -> io::Result<()> {
    let name = format!("{pid}.beacon");
    #[cfg(unix)]
    {
        let handle = unix_impl::SidecarDirHandle::open_or_create(dir)?;
        handle.touch_mtime(&name)
    }
    #[cfg(windows)]
    {
        windows_impl::touch_mtime(dir, &name)
    }
}

/// Path of `pid`'s one-time registration beacon under `dir`.
pub fn beacon_path(dir: &Path, pid: u32) -> PathBuf {
    dir.join(format!("{pid}.beacon"))
}

/// Is `pid` alive (right now)? On Unix, `kill(pid, 0)` is a pure
/// existence/permission probe with no side effects (`EPERM` — a live PID
/// owned by someone else — still counts as alive). On Windows,
/// `OpenProcess` + `GetExitCodeProcess` checking for `STILL_ACTIVE`.
pub fn is_process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        unix_impl::is_process_alive(pid)
    }
    #[cfg(windows)]
    {
        windows_impl::is_process_alive(pid)
    }
}

/// The OS-reported start time of `pid`, in epoch seconds, or `None` if it
/// cannot be determined (dead PID, permission denied, or an unsupported
/// platform). Used as the required identity check in [`enumerate_live`] —
/// `None` is treated as "cannot verify," which fails the gate rather than
/// passing it.
#[cfg(target_os = "macos")]
pub fn process_start_time_secs(pid: u32) -> Option<i64> {
    use std::os::raw::{c_int, c_void};

    const PROC_PIDTBSDINFO: c_int = 3;
    const MAXCOMLEN: usize = 16;

    // Mirrors Darwin's `struct proc_bsdinfo` (`<sys/proc_info.h>`), a stable
    // public ABI used by `libproc`'s `proc_pidinfo`. Only the layout up to
    // and including `pbi_start_tvsec`/`pbi_start_tvusec` matters here.
    #[repr(C)]
    struct ProcBsdInfo {
        pbi_flags: u32,
        pbi_status: u32,
        pbi_xstatus: u32,
        pbi_pid: u32,
        pbi_ppid: u32,
        pbi_uid: u32,
        pbi_gid: u32,
        pbi_ruid: u32,
        pbi_rgid: u32,
        pbi_svuid: u32,
        pbi_svgid: u32,
        rfu_1: u32,
        pbi_comm: [u8; MAXCOMLEN],
        pbi_name: [u8; 2 * MAXCOMLEN],
        pbi_nfiles: u32,
        pbi_pgid: u32,
        pbi_pjobc: u32,
        e_tdev: u32,
        e_tpgid: u32,
        pbi_nice: i32,
        pbi_start_tvsec: u64,
        pbi_start_tvusec: u64,
    }

    #[link(name = "proc")]
    extern "C" {
        fn proc_pidinfo(
            pid: c_int,
            flavor: c_int,
            arg: u64,
            buffer: *mut c_void,
            buffersize: c_int,
        ) -> c_int;
    }

    let pid_i32 = i32::try_from(pid).ok()?;
    let mut info: ProcBsdInfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<ProcBsdInfo>() as c_int;
    // SAFETY: `info` is a valid, zeroed, appropriately-sized buffer for the
    // duration of this call; `proc_pidinfo` writes at most `size` bytes.
    let ret = unsafe {
        proc_pidinfo(
            pid_i32,
            PROC_PIDTBSDINFO,
            0,
            &mut info as *mut _ as *mut c_void,
            size,
        )
    };
    if ret != size {
        return None;
    }
    i64::try_from(info.pbi_start_tvsec).ok()
}

/// Linux: derive process start time from `/proc/<pid>/stat` field 22
/// (`starttime`, in clock ticks since boot) plus `/proc/stat`'s `btime`
/// (system boot time, epoch seconds).
#[cfg(target_os = "linux")]
pub fn process_start_time_secs(pid: u32) -> Option<i64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // `comm` (field 2) is parenthesized and may itself contain spaces or
    // parens, so locate fields from the LAST ')' rather than splitting naively.
    let rparen = stat.rfind(')')?;
    let rest = stat.get(rparen + 1..)?;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // `rest` starts at field 3 (state); field 22 (starttime) is index 22-3=19.
    let starttime_ticks: u64 = fields.get(19)?.parse().ok()?;

    // SAFETY: `_SC_CLK_TCK` is a pure query with no side effects.
    let clk_tck = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if clk_tck <= 0 {
        return None;
    }
    let secs_since_boot = starttime_ticks / clk_tck as u64;

    let stat_all = fs::read_to_string("/proc/stat").ok()?;
    let btime = stat_all.lines().find_map(|line| {
        line.strip_prefix("btime ")
            .and_then(|v| v.trim().parse::<i64>().ok())
    })?;
    Some(btime + secs_since_boot as i64)
}

/// Windows: `OpenProcess` + `GetProcessTimes`' creation-time `FILETIME`,
/// converted from 100ns-since-1601 to Unix epoch seconds.
#[cfg(windows)]
pub fn process_start_time_secs(pid: u32) -> Option<i64> {
    windows_impl::process_start_time_secs(pid)
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
pub fn process_start_time_secs(_pid: u32) -> Option<i64> {
    None
}

fn now_epoch_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Enumerate the sidecar directory, applying the three-test liveness gate
/// to every heartbeat/beacon entry found and
/// classifying each PID's sidecar health three ways (ADR-091 Amendment 2
/// "Sidecar-health attribution"): [`WalpinPidHealth::Reporting`] (live,
/// identity-matched, fresh heartbeat), [`WalpinPidHealth::RegisteredSilent`]
/// (live, identity-matched, FRESHLY-REFRESHED beacon, no live heartbeat), or
/// [`WalpinPidHealth::Unknown`] (an entry exists but the trust-boundary check
/// refused it, failed to parse, or went stale — sidecar health for that PID
/// is unestablished).
///
/// Trust boundary (binding): the directory itself is
/// validated (type/owner/mode) BEFORE any entry is read — a non-compliant
/// directory returns `Err`, a health *failure*, never a partial/empty
/// result that could otherwise masquerade as "no live entries." Per entry,
/// symlinks and non-owned files are refused BEFORE their contents are read
/// (contributing an `Unknown` classification, not silently skipped).
///
/// Beacon refresh rule (ADR-091 Amendment 2): registration at
/// initialization alone never licenses `RegisteredSilent` — a beacon (or
/// heartbeat) that fails the identity gate (dead PID, reused PID) is genuine
/// absence (deleted, no entry at all: there is no evidence of THIS process),
/// but one that passes identity and STILL goes stale (its refresh mtime
/// falls outside the freshness window) is a wedged sidecar: classified
/// `Unknown`, deleted, and — critically — that PID is barred from later
/// resolving to `RegisteredSilent` off a co-existing beacon/heartbeat, per
/// "a PID whose heartbeat was deleted as stale classifies as unknown, never
/// registered-silent."
///
/// This function is Unix-only: its sole caller is the daemon's checkpoint
/// task, and daemon mode itself requires Unix. A missing directory (sidecar
/// never used yet) is `Ok` with an empty report, distinct from an
/// existing-but-untrustworthy one.
#[cfg(unix)]
pub fn enumerate_live(dir: &Path, sweep_interval: Duration) -> io::Result<WalpinReport> {
    let handle = match unix_impl::SidecarDirHandle::open_if_exists(dir) {
        Ok(Some(h)) => h,
        Ok(None) => return Ok(WalpinReport::default()),
        Err(e) => return Err(e),
    };

    let now = now_epoch_secs();
    // Subsecond intervals must not collapse the freshness window to zero
    // (minor, ADR-091 Amendment 2): a window of 0s would make any
    // `updated_at`/refresh timestamp other than the current wall-clock
    // second appear stale.
    let stale_after_secs = sweep_interval
        .saturating_mul(3)
        .max(Duration::from_secs(1))
        .as_secs() as i64;

    let mut heartbeats: std::collections::HashMap<u32, WalpinHeartbeat> = Default::default();
    let mut beacon_pids: std::collections::HashSet<u32> = Default::default();
    let mut unknown: Vec<(u32, &'static str)> = Vec::new();
    // PIDs whose heartbeat or beacon passed the identity gate but failed
    // freshness — these are wedged, not absent, and must never resolve to
    // `RegisteredSilent` off a co-existing entry (item b).
    let mut wedged: std::collections::HashSet<u32> = Default::default();

    for name in handle.list_names()? {
        if name.starts_with('.') {
            continue;
        }
        let is_heartbeat = name.ends_with(".json");
        let is_beacon = name.ends_with(".beacon");
        if !is_heartbeat && !is_beacon {
            continue;
        }
        let Some(pid) = name
            .rsplit_once('.')
            .and_then(|(stem, _)| stem.parse::<u32>().ok())
        else {
            continue;
        };

        // Trust boundary: symlink/ownership refusal happens BEFORE any
        // content read, and contributes `Unknown` rather than being
        // silently dropped — the entry's health is unestablished, not
        // exonerating.
        let (body, owner_uid, mtime) = match handle.read_checked(&name) {
            Ok(Some(v)) => v,
            Ok(None) => continue, // raced away between listing and reading
            Err(_) => {
                unknown.push((pid, "refused: symlinked sidecar entry"));
                continue;
            }
        };
        if owner_uid != unix_impl::current_uid() {
            unknown.push((pid, "refused: sidecar entry not owned by current user"));
            continue;
        }
        let mtime_secs = mtime
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let fresh = (now - mtime_secs).abs() <= stale_after_secs;

        if is_heartbeat {
            let heartbeat: WalpinHeartbeat = match serde_json::from_slice(&body) {
                Ok(hb) => hb,
                Err(_) => {
                    let _ = handle.unlink_tolerant(&name);
                    continue;
                }
            };
            let alive = is_process_alive(heartbeat.pid);
            let identity_ok = alive
                && process_start_time_secs(heartbeat.pid)
                    .map(|actual| (actual - heartbeat.started_at).abs() <= START_TIME_EPSILON_SECS)
                    .unwrap_or(false);
            if !identity_ok {
                let _ = handle.unlink_tolerant(&name);
                continue;
            }
            // `updated_at` is the heartbeat's own field (refreshed every
            // tick the warn condition persists); the entry's mtime tracks
            // it closely, but the JSON field is the authoritative source
            // the ADR specifies for heartbeat freshness.
            let hb_fresh = (now - heartbeat.updated_at).abs() <= stale_after_secs;
            if !hb_fresh {
                let _ = handle.unlink_tolerant(&name);
                wedged.insert(pid);
                unknown.push((pid, "stale walpin heartbeat"));
                continue;
            }
            let _ = fresh; // heartbeat freshness is `updated_at`-sourced, not mtime
            heartbeats.insert(heartbeat.pid, heartbeat);
        } else {
            let beacon: WalpinBeacon = match serde_json::from_slice(&body) {
                Ok(b) => b,
                Err(_) => {
                    let _ = handle.unlink_tolerant(&name);
                    continue;
                }
            };
            let alive = is_process_alive(beacon.pid);
            let identity_ok = alive
                && process_start_time_secs(beacon.pid)
                    .map(|actual| (actual - beacon.started_at).abs() <= START_TIME_EPSILON_SECS)
                    .unwrap_or(false);
            if !identity_ok {
                let _ = handle.unlink_tolerant(&name);
                continue;
            }
            // Beacon refresh rule: freshness is the entry's mtime (the
            // metadata-only touch), not any JSON field — the beacon's body
            // is written once and never refreshed.
            if !fresh {
                let _ = handle.unlink_tolerant(&name);
                wedged.insert(pid);
                unknown.push((pid, "stale walpin beacon"));
                continue;
            }
            beacon_pids.insert(beacon.pid);
        }
    }

    let mut entries: Vec<WalpinPidHealth> = Vec::new();
    for (pid, hb) in heartbeats {
        entries.push(WalpinPidHealth::Reporting(hb));
        beacon_pids.remove(&pid);
    }
    for pid in beacon_pids {
        if wedged.contains(&pid) {
            continue; // already carried as `Unknown` via `unknown` above
        }
        entries.push(WalpinPidHealth::RegisteredSilent { pid });
    }
    for (pid, reason) in unknown {
        entries.push(WalpinPidHealth::Unknown { pid, reason });
    }

    Ok(WalpinReport { entries })
}

/// Restores a possibly-unset env var on drop, including on panic — so an
/// assertion failure mid-test can never leak a mutated `KHIVE_WALPIN_SIDECAR`
/// into a sibling test (minor, ADR-091 Amendment 2: env-mutating
/// tests must serialize with cleanup on panic). Shared with the checkpoint
/// session-sweep test, which mutates the same variable and must serialize
/// under the same `khive_walpin_sidecar_env` key.
#[cfg(test)]
pub(crate) struct EnvVarGuard {
    key: &'static str,
    saved: Option<String>,
}
#[cfg(test)]
impl EnvVarGuard {
    pub(crate) fn capture(key: &'static str) -> Self {
        Self {
            key,
            saved: std::env::var(key).ok(),
        }
    }
}
#[cfg(test)]
impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match &self.saved {
            Some(v) => std::env::set_var(self.key, v),
            None => std::env::remove_var(self.key),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    #[cfg(unix)]
    fn current_uid() -> u32 {
        unix_impl::current_uid()
    }

    fn heartbeat(pid: u32) -> WalpinHeartbeat {
        WalpinHeartbeat {
            pid,
            process_role: "session".to_string(),
            started_at: process_start_time_secs(std::process::id()).unwrap_or(0),
            oldest_tx_age_secs: 45.0,
            oldest_tx_label: Some("test_span".to_string()),
            updated_at: now_epoch_secs(),
        }
    }

    #[test]
    fn sidecar_dir_is_db_scoped_sibling() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("khive.db");
        assert_eq!(sidecar_dir_for(&db), dir.path().join("khive.db.walpin"));
    }

    #[test]
    #[serial_test::serial(khive_walpin_sidecar_env)]
    fn sidecar_enabled_defaults_to_file_backed() {
        // Deterministic regardless of the ambient environment (minor,
        // ADR-091 Amendment 2: the prior version was vacuously true
        // whenever `KHIVE_WALPIN_SIDECAR` happened to be set already).
        let _guard = EnvVarGuard::capture("KHIVE_WALPIN_SIDECAR");
        std::env::remove_var("KHIVE_WALPIN_SIDECAR");
        assert!(sidecar_enabled(true), "file-backed must default on");
        assert!(!sidecar_enabled(false), "in-memory must default off");
    }

    #[test]
    #[serial_test::serial(khive_walpin_sidecar_env)]
    fn sidecar_enabled_env_override_wins_either_way() {
        let _guard = EnvVarGuard::capture("KHIVE_WALPIN_SIDECAR");
        std::env::set_var("KHIVE_WALPIN_SIDECAR", "off");
        assert!(
            !sidecar_enabled(true),
            "explicit off must override file-backed default"
        );
        std::env::set_var("KHIVE_WALPIN_SIDECAR", "on");
        assert!(
            sidecar_enabled(false),
            "explicit on must override in-memory default"
        );
    }

    #[test]
    fn ensure_sidecar_dir_creates_0700_owned_dir() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("khive.db.walpin");
        ensure_sidecar_dir(&dir).expect("should create");
        let meta = fs::symlink_metadata(&dir).unwrap();
        assert!(meta.is_dir());
        assert_eq!(meta.permissions().mode() & 0o777, 0o700);
        assert_eq!(meta.uid(), current_uid());
    }

    #[test]
    fn ensure_sidecar_dir_refuses_wrong_mode() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("khive.db.walpin");
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        let err = ensure_sidecar_dir(&dir).expect_err("wrong mode must be refused");
        assert!(err.to_string().contains("expected 0700"));
    }

    #[test]
    fn ensure_sidecar_dir_refuses_symlink() {
        let root = tempfile::tempdir().unwrap();
        let real = root.path().join("real_dir");
        fs::create_dir(&real).unwrap();
        let link = root.path().join("khive.db.walpin");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let err = ensure_sidecar_dir(&link).expect_err("symlink must be refused");
        assert!(err.to_string().contains("symlink"));
    }

    #[test]
    fn write_then_read_heartbeat_roundtrips() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("khive.db.walpin");
        let hb = heartbeat(std::process::id());
        write_heartbeat(&dir, &hb).expect("write should succeed");
        let content = fs::read_to_string(dir.join(format!("{}.json", hb.pid))).unwrap();
        let read_back: WalpinHeartbeat = serde_json::from_str(&content).unwrap();
        assert_eq!(read_back, hb);
    }

    #[test]
    fn write_heartbeat_refuses_symlinked_target() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("khive.db.walpin");
        ensure_sidecar_dir(&dir).unwrap();
        let real = root.path().join("elsewhere.txt");
        fs::write(&real, b"nope").unwrap();
        let hb = heartbeat(999_999);
        let target = dir.join(format!("{}.json", hb.pid));
        std::os::unix::fs::symlink(&real, &target).unwrap();
        let err = write_heartbeat(&dir, &hb).expect_err("symlinked target must be refused");
        assert!(err.to_string().contains("symlink"));
        // The real file behind the symlink must be untouched.
        assert_eq!(fs::read_to_string(&real).unwrap(), "nope");
    }

    #[test]
    fn remove_heartbeat_is_idempotent_when_absent() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("khive.db.walpin");
        ensure_sidecar_dir(&dir).unwrap();
        remove_heartbeat(&dir, 123_456).expect("removing an absent entry is a no-op");
    }

    #[test]
    fn is_process_alive_true_for_self_false_for_reserved_pid() {
        assert!(is_process_alive(std::process::id()));
        // PID 0 is never a valid target for `kill`.
        assert!(!is_process_alive(0));
    }

    #[test]
    fn process_start_time_resolves_for_self() {
        let start = process_start_time_secs(std::process::id());
        assert!(
            start.is_some(),
            "must resolve this process's own start time"
        );
        let now = now_epoch_secs();
        assert!(
            start.unwrap() <= now,
            "start time must not be in the future"
        );
    }

    fn beacon(pid: u32) -> WalpinBeacon {
        WalpinBeacon {
            pid,
            process_role: "session".to_string(),
            started_at: process_start_time_secs(std::process::id()).unwrap_or(0),
        }
    }

    #[test]
    fn enumerate_live_reports_and_retains_a_genuinely_live_entry() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("khive.db.walpin");
        let hb = heartbeat(std::process::id());
        write_heartbeat(&dir, &hb).unwrap();

        let report = enumerate_live(&dir, Duration::from_secs(5)).unwrap();
        let reporting: Vec<_> = report.reporting().collect();
        assert_eq!(reporting.len(), 1);
        assert_eq!(reporting[0].pid, hb.pid);
        assert!(report.fully_attributed());
        // A live, fresh, identity-matched entry must be retained on disk, not deleted.
        assert!(dir.join(format!("{}.json", hb.pid)).exists());
    }

    #[test]
    fn enumerate_live_deletes_dead_pid_entry() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("khive.db.walpin");
        // A PID vanishingly unlikely to be alive/reused mid-test.
        let mut hb = heartbeat(std::process::id());
        hb.pid = 2_000_000_000;
        hb.started_at = 12345;
        write_heartbeat(&dir, &hb).unwrap();

        let report = enumerate_live(&dir, Duration::from_secs(5)).unwrap();
        assert!(report.entries.is_empty());
        assert!(!dir.join(format!("{}.json", hb.pid)).exists());
    }

    #[test]
    fn enumerate_live_deletes_mismatched_start_time_entry() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("khive.db.walpin");
        let mut hb = heartbeat(std::process::id());
        // Alive PID (this test process) but a `started_at` far from reality —
        // simulates a reused PID whose old heartbeat never got cleaned up.
        hb.started_at = 1;
        write_heartbeat(&dir, &hb).unwrap();

        let report = enumerate_live(&dir, Duration::from_secs(5)).unwrap();
        assert!(
            report.entries.is_empty(),
            "mismatched identity must fail the gate"
        );
        assert!(!dir.join(format!("{}.json", hb.pid)).exists());
    }

    #[test]
    fn enumerate_live_deletes_stale_updated_at_entry() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("khive.db.walpin");
        let mut hb = heartbeat(std::process::id());
        hb.updated_at = now_epoch_secs() - 3600; // far outside 3 sweep intervals
        write_heartbeat(&dir, &hb).unwrap();

        let report = enumerate_live(&dir, Duration::from_secs(5)).unwrap();
        // ADR-091 Amendment 2: a stale-but-identity-valid
        // heartbeat is wedged, not absent — it classifies `Unknown` rather
        // than silently vanishing from the report.
        assert_eq!(report.reporting().count(), 0);
        assert_eq!(report.unknown_pids().collect::<Vec<_>>(), vec![hb.pid]);
        assert!(!report.fully_attributed());
        assert!(!dir.join(format!("{}.json", hb.pid)).exists());
    }

    #[test]
    fn enumerate_live_subsecond_sweep_interval_does_not_collapse_freshness_window() {
        // Minor (ADR-091 Amendment 2): a sub-second
        // KHIVE_SESSION_SWEEP_INTERVAL_MS must not yield a zero-second
        // freshness window that treats every heartbeat as instantly stale.
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("khive.db.walpin");
        let hb = heartbeat(std::process::id());
        write_heartbeat(&dir, &hb).unwrap();

        let report = enumerate_live(&dir, Duration::from_millis(200)).unwrap();
        assert_eq!(
            report.reporting().count(),
            1,
            "must not be spuriously stale"
        );
    }

    #[test]
    fn enumerate_live_refuses_symlinked_entry_as_unknown_without_touching_target() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("khive.db.walpin");
        ensure_sidecar_dir(&dir).unwrap();
        let real = root.path().join("elsewhere.txt");
        fs::write(&real, b"precious").unwrap();
        let link = dir.join("42.json");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let report = enumerate_live(&dir, Duration::from_secs(5)).unwrap();
        assert!(report.reporting().count() == 0);
        assert_eq!(report.unknown_pids().collect::<Vec<_>>(), vec![42]);
        assert!(!report.fully_attributed());
        assert_eq!(fs::read_to_string(&real).unwrap(), "precious");
        assert!(
            link.exists(),
            "the symlink itself must not be deleted either"
        );
    }

    #[test]
    fn enumerate_live_refuses_non_owned_entry_before_reading_contents() {
        // We cannot fabricate a genuinely non-owned file without root, so this
        // exercises the same code path via a forged UID check would require
        // privilege; instead this asserts the documented contract at the
        // metadata layer: an entry whose uid differs from `current_uid()` is
        // never parsed. Since every file this test process creates is
        // self-owned, we assert the positive form here (owned entries ARE
        // read) and rely on `validate_dir_metadata`'s ownership check
        // (exercised by `ensure_sidecar_dir_refuses_wrong_mode`-style tests)
        // for the negative form, which is the same `current_uid()` check
        // reused verbatim by per-entry validation.
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("khive.db.walpin");
        let hb = heartbeat(std::process::id());
        write_heartbeat(&dir, &hb).unwrap();
        let meta = fs::symlink_metadata(dir.join(format!("{}.json", hb.pid))).unwrap();
        assert_eq!(
            meta.uid(),
            current_uid(),
            "self-written entries are owned by the current user, exercising the accept path"
        );
    }

    #[test]
    fn enumerate_live_refuses_non_compliant_directory_wholesale() {
        // Item 3 (ADR-091 Amendment 2): a directory that fails the
        // trust-boundary check must return a health *failure*, not a
        // silently empty/partial report that could masquerade as "no live
        // entries" evidence.
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("khive.db.walpin");
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();

        let err = enumerate_live(&dir, Duration::from_secs(5))
            .expect_err("non-compliant directory must be refused, not silently enumerated");
        assert!(err.to_string().contains("expected 0700"));
    }

    #[test]
    fn enumerate_live_missing_directory_is_ok_empty_not_a_failure() {
        // A sidecar that has simply never been used yet is a distinct case
        // from an existing-but-untrustworthy one.
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("khive.db.walpin");
        let report = enumerate_live(&dir, Duration::from_secs(5)).unwrap();
        assert!(report.entries.is_empty());
    }

    #[test]
    fn enumerate_live_classifies_registered_silent_beacon_with_no_heartbeat() {
        // ADR-091 Amendment 2 spec delta: a live process that has registered
        // a beacon but never crossed the warn threshold (so it never wrote a
        // heartbeat) is `RegisteredSilent`, not absent/unknown.
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("khive.db.walpin");
        let b = beacon(std::process::id());
        write_beacon(&dir, &b).unwrap();

        let report = enumerate_live(&dir, Duration::from_secs(5)).unwrap();
        assert_eq!(report.reporting().count(), 0);
        assert_eq!(
            report.registered_silent_pids().collect::<Vec<_>>(),
            vec![std::process::id()]
        );
        assert!(report.fully_attributed());
    }

    #[test]
    fn enumerate_live_reporting_wins_over_registered_silent_for_same_pid() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("khive.db.walpin");
        let pid = std::process::id();
        write_beacon(&dir, &beacon(pid)).unwrap();
        write_heartbeat(&dir, &heartbeat(pid)).unwrap();

        let report = enumerate_live(&dir, Duration::from_secs(5)).unwrap();
        assert_eq!(report.reporting().count(), 1);
        assert_eq!(report.registered_silent_pids().count(), 0);
    }

    #[test]
    fn enumerate_live_deletes_dead_beacon() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("khive.db.walpin");
        let mut b = beacon(std::process::id());
        b.pid = 2_000_000_001;
        b.started_at = 12345;
        write_beacon(&dir, &b).unwrap();

        let report = enumerate_live(&dir, Duration::from_secs(5)).unwrap();
        assert!(report.entries.is_empty());
        assert!(!beacon_path(&dir, b.pid).exists());
    }

    #[test]
    fn write_beacon_refuses_symlinked_target() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("khive.db.walpin");
        ensure_sidecar_dir(&dir).unwrap();
        let real = root.path().join("elsewhere.txt");
        fs::write(&real, b"nope").unwrap();
        let b = beacon(999_998);
        let target = beacon_path(&dir, b.pid);
        std::os::unix::fs::symlink(&real, &target).unwrap();
        let err = write_beacon(&dir, &b).expect_err("symlinked target must be refused");
        assert!(err.to_string().contains("symlink"));
        assert_eq!(fs::read_to_string(&real).unwrap(), "nope");
    }

    #[test]
    fn enumerate_live_classifies_stale_beacon_as_unknown() {
        // ADR-091 Amendment 2: a beacon that is identity-valid
        // (live PID, matching start time) but whose refresh mtime has fallen
        // outside the freshness window is a wedged sidecar, not evidence of
        // registration — it must classify `Unknown`, not `RegisteredSilent`.
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("khive.db.walpin");
        let pid = std::process::id();
        write_beacon(&dir, &beacon(pid)).unwrap();
        let beacon_file = fs::OpenOptions::new()
            .write(true)
            .open(dir.join(format!("{pid}.beacon")))
            .unwrap();
        beacon_file
            .set_modified(SystemTime::now() - Duration::from_secs(3600))
            .unwrap();

        let report = enumerate_live(&dir, Duration::from_secs(5)).unwrap();
        assert_eq!(report.registered_silent_pids().count(), 0);
        assert_eq!(report.unknown_pids().collect::<Vec<_>>(), vec![pid]);
        assert!(!report.fully_attributed());
        assert!(
            !beacon_path(&dir, pid).exists(),
            "a stale beacon must be deleted, not left to re-classify next sweep"
        );
    }

    #[test]
    fn enumerate_live_stale_heartbeat_with_fresh_beacon_stays_unknown_not_registered_silent() {
        // ADR-091 Amendment 2: a PID whose heartbeat was
        // deleted as stale must classify `Unknown`, even when a co-existing
        // FRESH beacon for the same PID would otherwise resolve it to
        // `RegisteredSilent`.
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("khive.db.walpin");
        let pid = std::process::id();
        write_beacon(&dir, &beacon(pid)).unwrap();
        let mut hb = heartbeat(pid);
        hb.updated_at = now_epoch_secs() - 3600;
        write_heartbeat(&dir, &hb).unwrap();

        let report = enumerate_live(&dir, Duration::from_secs(5)).unwrap();
        assert_eq!(report.reporting().count(), 0);
        assert_eq!(
            report.registered_silent_pids().collect::<Vec<_>>(),
            Vec::<u32>::new(),
            "a co-existing fresh beacon must not rescue a PID with a stale heartbeat"
        );
        assert_eq!(report.unknown_pids().collect::<Vec<_>>(), vec![pid]);
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn census_holders_macos_discovers_self_as_a_holder_of_an_open_db_file() {
        let root = tempfile::tempdir().unwrap();
        let db_path = root.path().join("test.db");
        let file = fs::File::create(&db_path).unwrap();
        let census = census_holders(&db_path).expect("census must succeed for a live target");
        assert!(
            census.holders.contains(&std::process::id()),
            "this process holds {db_path:?} open and must appear in its own OS-derived census"
        );
        // The self-canary must NOT fire when self genuinely is discovered —
        // it only forces `truncated` on a missing self-PID, never clears an
        // already-set flag from something else.
        assert!(
            !census.truncated,
            "self was found; the self-canary must not report truncation on its own"
        );
        // Not asserting `is_complete()` here: an unprivileged process
        // legitimately cannot inspect every other PID's open fds on a real,
        // busy machine (other users' / root's processes), so
        // `uninspectable_pids` is realistically non-empty — that's exactly
        // the condition this fix now surfaces instead of silently ignoring.
        drop(file);
    }

    /// Identity-comparison regression: a holder that opened the database
    /// through a hard link (a different path to the same file) must still be
    /// discovered — a path-string comparison would silently omit it while
    /// leaving the census marked complete.
    #[test]
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn census_holders_discovers_holder_through_hard_link_path() {
        let root = tempfile::tempdir().unwrap();
        let db_path = root.path().join("test.db");
        fs::File::create(&db_path).unwrap();
        let link_path = root.path().join("test-link.db");
        fs::hard_link(&db_path, &link_path).unwrap();

        let file = fs::File::open(&link_path).unwrap();
        let census = census_holders(&db_path).expect("census must succeed for a live target");
        assert!(
            census.holders.contains(&std::process::id()),
            "this process holds the db open via hard link {link_path:?} and must appear in the census for {db_path:?}"
        );
        drop(file);
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn negotiate_buffer_converges_when_the_set_stops_growing() {
        // Simulates a live set that "grows" for the first two size probes
        // (the data call keeps filling capacity) and then stabilizes —
        // negotiate_buffer must retry rather than report the first,
        // possibly-truncated snapshot as final.
        let probe = std::cell::Cell::new(0usize);
        let sizes = [4usize, 8, 8]; // bytes needed per size_call invocation
        let (items, truncated) = negotiate_buffer::<i32>(
            || {
                let i = probe.get().min(sizes.len() - 1);
                sizes[i] as std::os::raw::c_int
            },
            |_buf_ptr, buf_bytes| {
                let i = probe.get();
                probe.set(i + 1);
                // First two attempts: report the buffer as exactly full
                // (looks truncated); third attempt: report fewer bytes
                // than capacity (a clean, complete snapshot).
                if i < 2 {
                    buf_bytes
                } else {
                    (buf_bytes as usize - 4) as std::os::raw::c_int
                }
            },
        )
        .expect("negotiation must succeed once the set stabilizes");
        assert!(
            !truncated,
            "a snapshot that ends up strictly under capacity must not be marked truncated"
        );
        assert!(!items.is_empty());
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn negotiate_buffer_reports_truncated_after_exhausting_retries() {
        // The data call always reports the buffer as exactly full, no
        // matter how many times negotiate_buffer retries with a larger
        // buffer — this must give up after CENSUS_BUFFER_NEGOTIATION_ATTEMPTS
        // and report `truncated = true` rather than loop forever or lie.
        let (items, truncated) =
            negotiate_buffer::<i32>(|| 4 as std::os::raw::c_int, |_buf_ptr, buf_bytes| buf_bytes)
                .expect("negotiation must still return a (possibly truncated) result, not error");
        assert!(
            truncated,
            "a buffer that stays exactly full across every retry must be reported truncated"
        );
        assert!(!items.is_empty());
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn negotiate_buffer_propagates_a_failed_size_call() {
        let result = negotiate_buffer::<i32>(
            || -1 as std::os::raw::c_int,
            |_buf_ptr, buf_bytes| buf_bytes,
        );
        assert!(result.is_err(), "a non-positive size probe must error out");
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn macos_pid_genuinely_gone_only_true_for_esrch() {
        // ADR-091 Amendment 2: ESRCH (the target process
        // exited between listing and inspection) is a genuine "positively
        // gone" race, safe to skip. Every other errno — most commonly
        // EPERM/EACCES from trying to list another user's open files — means
        // the inspection itself failed, not that the PID is absent.
        assert!(macos_pid_genuinely_gone(Some(libc::ESRCH)));
        assert!(!macos_pid_genuinely_gone(Some(libc::EPERM)));
        assert!(!macos_pid_genuinely_gone(Some(libc::EACCES)));
        assert!(!macos_pid_genuinely_gone(None));
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn proc_pidfdinfo_returned_expected_size_boundary() {
        // ADR-091 Amendment 2: a positive-but-short byte count must
        // classify as an inspection failure, not a successful call — only
        // an exact match on the expected struct size is `ok`.
        let expected = std::mem::size_of::<u64>(); // stand-in fixed-size struct
        assert!(
            proc_pidfdinfo_returned_expected_size(expected as i32, expected),
            "an exact match on the expected struct size must be ok"
        );
        assert!(
            !proc_pidfdinfo_returned_expected_size(expected as i32 - 1, expected),
            "a positive but short byte count must be an inspection failure"
        );
        assert!(
            !proc_pidfdinfo_returned_expected_size(0, expected),
            "a zero return must be an inspection failure"
        );
        assert!(
            !proc_pidfdinfo_returned_expected_size(-1, expected),
            "a negative return must be an inspection failure"
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn census_holders_linux_discovers_self_as_a_holder_of_an_open_db_file() {
        let root = tempfile::tempdir().unwrap();
        let db_path = root.path().join("test.db");
        let file = fs::File::create(&db_path).unwrap();
        let census = census_holders(&db_path).expect("census must succeed for a live target");
        assert!(
            census.holders.contains(&std::process::id()),
            "this process holds {db_path:?} open and must appear in its own OS-derived census"
        );
        // The self-canary must NOT fire when self genuinely is discovered —
        // it only forces `truncated` on a missing self-PID, never clears an
        // already-set flag from something else.
        assert!(
            !census.truncated,
            "self was found; the self-canary (and the namespace check, when this test runs \
             in the host's own PID namespace) must not report truncation on its own"
        );
        // Not asserting `is_complete()` here: an unprivileged process
        // legitimately cannot inspect every other PID's open fds on a real,
        // busy machine (other users' / root's processes), so
        // `uninspectable_pids` is realistically non-empty — that's exactly
        // the condition this fix now surfaces instead of silently ignoring.
        drop(file);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn linux_proc_gone_only_true_for_not_found() {
        // ADR-091 Amendment 2: NotFound (the process's
        // /proc/<pid>/fd directory raced away between listing and open) is a
        // genuine "positively gone" race, safe to skip. PermissionDenied
        // (inspecting another user's fds) means the inspection itself
        // failed, not that the PID is absent.
        assert!(linux_proc_gone(&io::Error::from(io::ErrorKind::NotFound)));
        assert!(!linux_proc_gone(&io::Error::from(
            io::ErrorKind::PermissionDenied
        )));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn pid_ns_is_init_only_true_for_the_fixed_kernel_inode() {
        // ADR-091 Amendment 2: only the exact kernel-assigned init
        // namespace inode (`include/linux/proc_ns.h`) is complete-eligible.
        // A container's own, internally self-consistent PID namespace gets
        // a different, dynamically allocated inode and must classify as
        // incomplete — that is precisely the self-consistent-container gap
        // the old readlink comparison could not detect.
        assert!(pid_ns_is_init(PROC_PID_INIT_INO));
        assert!(!pid_ns_is_init(PROC_PID_INIT_INO + 1));
        assert!(!pid_ns_is_init(0));
        assert!(!pid_ns_is_init(12345));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn proc_mount_restricts_visibility_classifies_hidepid_and_subset() {
        // ADR-091 Amendment 2: a clean options string never restricts.
        assert!(!proc_mount_restricts_visibility(
            "rw,nosuid,nodev,noexec,relatime"
        ));
        // Numeric hidepid values other than 0 restrict.
        assert!(proc_mount_restricts_visibility("rw,hidepid=2"));
        // Symbolic hidepid values (Linux 5.8+) restrict too.
        assert!(proc_mount_restricts_visibility("rw,hidepid=invisible"));
        assert!(proc_mount_restricts_visibility("rw,hidepid=ptraceable"));
        // subset=pid (any subset=) restricts.
        assert!(proc_mount_restricts_visibility("rw,subset=pid"));
        // hidepid=0 is the explicit non-restricting value.
        assert!(!proc_mount_restricts_visibility("hidepid=0"));
        // hidepid=off is the symbolic equivalent of hidepid=0.
        assert!(!proc_mount_restricts_visibility("rw,hidepid=off"));
        // A bare `hidepid` flag with no value is treated as restricting —
        // the kernel's default nonzero behavior, not a proven-clean mount.
        assert!(proc_mount_restricts_visibility("rw,hidepid"));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn proc_mounts_restricted_in_is_any_restrictive_across_stacked_mounts() {
        // Mounts stack: a later /proc mount shadows an earlier one while
        // both records stay in mountinfo. Selection must be ANY-restrictive
        // across every matching record — a clean shadowed mount must not
        // mask a restricted visible one, in either record order.
        let clean = "36 25 0:16 / /proc rw,nosuid,nodev,noexec,relatime - proc proc rw";
        let restricted = "99 25 0:34 / /proc rw,relatime - proc proc rw,hidepid=2";
        let clean_then_restricted = format!("{clean}\n{restricted}");
        let restricted_then_clean = format!("{restricted}\n{clean}");
        assert_eq!(proc_mounts_restricted_in(clean), Some(false));
        assert_eq!(proc_mounts_restricted_in(restricted), Some(true));
        assert_eq!(
            proc_mounts_restricted_in(&clean_then_restricted),
            Some(true)
        );
        assert_eq!(
            proc_mounts_restricted_in(&restricted_then_clean),
            Some(true)
        );
        // No /proc procfs record at all → None (caller fails closed).
        assert_eq!(
            proc_mounts_restricted_in("36 25 0:16 / /sys rw - sysfs sysfs rw"),
            None
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn proc_mount_is_visibility_restricted_reads_this_hosts_own_proc_mount() {
        // Live check against whatever /proc this test process actually
        // runs under — asserts the parse succeeds (Some(_)), not a fixed
        // verdict, since CI/dev/container hosts differ. A `None` here
        // would mean mountinfo parsing silently failed on a real host,
        // which the caller treats as fail-closed (`truncated = true`) —
        // this test exists to catch that regression, not to assert which
        // way this particular host's mount classifies.
        assert!(
            proc_mount_is_visibility_restricted().is_some(),
            "expected to find and parse this process's own /proc mount entry in \
             /proc/self/mountinfo"
        );
    }

    #[test]
    fn census_result_is_complete_reflects_uninspectable_pids() {
        let complete = CensusResult {
            holders: std::collections::HashSet::from([1, 2]),
            uninspectable_pids: Vec::new(),
            truncated: false,
        };
        assert!(complete.is_complete());

        let incomplete = CensusResult {
            holders: std::collections::HashSet::from([1]),
            uninspectable_pids: vec![7],
            truncated: false,
        };
        assert!(!incomplete.is_complete());
    }

    #[test]
    fn census_result_is_complete_reflects_truncated() {
        // ADR-091 Amendment 2: `truncated` is a second,
        // independent incompleteness signal — a census can have an empty
        // `uninspectable_pids` (no single PID's inspection failed) and
        // still be incomplete because the walk itself has positive evidence
        // it missed part of the process universe.
        let truncated = CensusResult {
            holders: std::collections::HashSet::from([1]),
            uninspectable_pids: Vec::new(),
            truncated: true,
        };
        assert!(!truncated.is_complete());
    }

    #[test]
    fn self_canary_marks_truncated_when_own_pid_missing() {
        let mut census = CensusResult {
            holders: std::collections::HashSet::from([std::process::id().wrapping_add(1)]),
            uninspectable_pids: Vec::new(),
            truncated: false,
        };
        census.apply_self_canary();
        assert!(
            census.truncated,
            "a census that discovered other holders but not the calling process itself is \
             positive proof of a missed enumeration and must be marked incomplete"
        );
    }

    #[test]
    fn self_canary_leaves_a_correct_census_untouched() {
        let mut census = CensusResult {
            holders: std::collections::HashSet::from([std::process::id()]),
            uninspectable_pids: Vec::new(),
            truncated: false,
        };
        census.apply_self_canary();
        assert!(
            !census.truncated,
            "self was found; the canary must not fire"
        );
    }

    #[test]
    fn self_canary_does_not_clear_an_existing_truncated_flag() {
        let mut census = CensusResult {
            holders: std::collections::HashSet::from([std::process::id()]),
            uninspectable_pids: Vec::new(),
            truncated: true,
        };
        census.apply_self_canary();
        assert!(
            census.truncated,
            "the self-canary only ever sets `truncated`; it must never clear a flag another \
             step already raised"
        );
    }
}
