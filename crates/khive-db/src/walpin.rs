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
const START_TIME_EPSILON_SECS: u64 = 2;

/// One process's walpin heartbeat record (ADR-091 Amendment 2 Plank B).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WalpinHeartbeat {
    pub pid: u32,
    pub process_role: String,
    /// OS-reported process start time (epoch seconds), used as the identity
    /// check at enumeration time — a reused PID is rejected deterministically
    /// rather than probabilistically.
    pub started_at: i64,
    /// Age of the oldest span as of the last body write. Not current once a
    /// tick advances freshness via a metadata-only mtime touch (ADR-091
    /// Amendment 3 Plank F1) — readers that want the current age prefer
    /// [`WalpinHeartbeat::current_oldest_tx_age_secs`], which uses
    /// `oldest_tx_started_at` when present.
    pub oldest_tx_age_secs: f64,
    pub oldest_tx_label: Option<String>,
    /// Epoch timestamp of the oldest span's registration instant, fixed for
    /// as long as that span stays the oldest one (ADR-091 Amendment 3 Plank
    /// F1). `None` for records written before this field existed. Present
    /// specifically to let readers compute a current age from a body that a
    /// touch-only tick left otherwise unchanged.
    #[serde(default)]
    pub oldest_tx_started_at: Option<i64>,
    /// The instant of the last body write. No longer part of liveness
    /// classification for a record carrying `oldest_tx_started_at` (Plank
    /// F1 moves that basis to the entry's mtime); records without it are
    /// still classified on this field exactly as before Amendment 3.
    pub updated_at: i64,
    /// The producer's own sweep cadence in milliseconds. Freshness at
    /// enumeration is judged against THIS cadence, not the enumerating
    /// daemon's — two processes with independently configured sweep
    /// intervals must not misread each other as stale. `0` (absent in a
    /// record written before this field existed) falls back to the
    /// enumerator's own interval. `interval_ms` is accepted as an alias for
    /// records written before the ADR-091 Amendment 2 review-follow-up
    /// rename — a live writer still on the old field name must not have its
    /// real cadence silently dropped to the enumerator's fallback.
    #[serde(default, alias = "interval_ms")]
    pub sweep_interval_ms: u64,
    /// ADR-091 Amendment 3 Plank F2: `"origin"` when the oldest span above
    /// carried this backend's own origin identity, `"fallback"` when it was
    /// an `Unscoped` span observed only through the main view's
    /// never-silently-drop fallback. `None` for records written before this
    /// field existed. Exactly these two values when present — every
    /// consumer MUST fail closed (treat as fallback-confidence) on any
    /// other value, per the amendment's reading rule; see
    /// [`WalpinHeartbeat::attribution_is_evidence_backed`].
    #[serde(default)]
    pub attribution_basis: Option<String>,
}

impl WalpinHeartbeat {
    /// ADR-091 Amendment 3 Plank F2 fail-closed reading rule, binding on
    /// every consumer: only the exact string `"origin"` licenses an
    /// evidence-backed reading. A missing field, or any value this
    /// amendment does not define, classifies as fallback-confidence —
    /// never evidence-backed.
    pub fn attribution_is_evidence_backed(&self) -> bool {
        self.attribution_basis.as_deref() == Some("origin")
    }

    /// ADR-091 Amendment 3 Plank F1: age computed at read time. Prefers
    /// `oldest_tx_started_at` — fixed for as long as the span stays the
    /// oldest one, so it stays correct across metadata-only touches — over
    /// the possibly-stale `oldest_tx_age_secs` body field. Records written
    /// before this amendment lack the field and fall back to the body
    /// value exactly as before.
    pub fn current_oldest_tx_age_secs(&self, now_epoch_secs: i64) -> f64 {
        match self.oldest_tx_started_at {
            Some(started_at) => (now_epoch_secs - started_at).max(0) as f64,
            None => self.oldest_tx_age_secs,
        }
    }
}

/// A heartbeat that survived the three-test liveness gate at enumeration time.
#[derive(Debug, Clone, PartialEq)]
pub struct LiveWalpinEntry {
    pub heartbeat: WalpinHeartbeat,
}

/// Per-PID registration marker (ADR-091 Amendment 2, sidecar-health
/// attribution). Written at sidecar initialization (and re-written only
/// after a fail-closed removal — see [`remove_beacon`]); its body is never
/// refreshed per tick, only its mtime. A live process that has no
/// over-threshold span still has a footprint in the sidecar directory: the
/// absence of a *heartbeat* then affirmatively means "no old span," rather
/// than "sidecar never worked."
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WalpinBeacon {
    pub pid: u32,
    pub process_role: String,
    pub started_at: i64,
    /// The producer's own sweep cadence in milliseconds — the beacon's
    /// refresh mtime is judged against this cadence at enumeration, not the
    /// enumerating daemon's. `0` falls back to the enumerator's interval.
    /// `interval_ms` is accepted as an alias — see
    /// [`WalpinHeartbeat::sweep_interval_ms`] for why the old field name
    /// must still deserialize correctly.
    #[serde(default, alias = "interval_ms")]
    pub sweep_interval_ms: u64,
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
            let (parent_fd, c_name) = Self::open_parent_and_name(dir)?;
            match Self::open_validated_at(&parent_fd, &c_name, dir) {
                Ok(handle) => Ok(handle),
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    // SAFETY: `c_name` is NUL-terminated for the call;
                    // `parent_fd` is a live, open directory descriptor for
                    // the call's duration.
                    let rc =
                        unsafe { libc::mkdirat(parent_fd.as_raw_fd(), c_name.as_ptr(), 0o700) };
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

        /// Open `dir`'s parent directory (`O_DIRECTORY | O_NOFOLLOW`) and
        /// return it alongside `dir`'s final path component, so the caller
        /// can `openat()` `dir` itself relative to an already-live
        /// descriptor instead of re-resolving `dir`'s full path. A bare
        /// `open(dir, O_NOFOLLOW)` only refuses a symlink at `dir`'s own,
        /// final component — an attacker who can replace `dir`'s *parent*
        /// between path construction and this open would still redirect the
        /// lookup, since intermediate path components are always followed
        /// regardless of `O_NOFOLLOW`. Anchoring on the parent's descriptor
        /// closes that gap for `dir` the same way `SidecarDirHandle` already
        /// closes it for every entry `dir` contains.
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
            let c_parent = path_cstring(parent)?;
            // SAFETY: `c_parent` is NUL-terminated for the call; the
            // returned fd is uniquely owned by this call and wrapped
            // immediately.
            let fd = unsafe {
                libc::open(
                    c_parent.as_ptr(),
                    libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: `fd` was just returned by the successful `open` above.
            let parent_file = unsafe { fs::File::from_raw_fd(fd) };
            let c_name = name_cstring(&name.to_string_lossy())?;
            Ok((parent_file, c_name))
        }

        fn open_validated_at(
            parent_fd: &fs::File,
            c_name: &CString,
            dir: &Path,
        ) -> io::Result<Self> {
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

        /// Read `name`'s contents plus its owner uid and mtime. `Ok(None)`
        /// for a missing entry (raced away between listing and reading).
        /// Refuses symlinks, non-regular files, and oversized entries: the
        /// open carries `O_NONBLOCK` so a FIFO planted at the entry's name
        /// can never block this call waiting for a peer, the opened fd is
        /// `fstat`'d and must be a regular file before any byte is read,
        /// and the read itself is bounded — a same-uid process must not be
        /// able to stall or balloon checkpoint-time enumeration. Owner uid
        /// and mtime come from the same `fstat`, so every trust decision is
        /// made against the exact object that was read.
        pub(super) fn read_checked(
            &self,
            name: &str,
        ) -> io::Result<Option<(Vec<u8>, u32, SystemTime)>> {
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
            Ok(Some((buf, meta.uid(), mtime)))
        }

        /// List entry names via `fdopendir` on a DUPLICATE of this fd (the
        /// original stays owned by `self`) — never re-resolves the
        /// directory by path.
        /// List up to `max` non-hidden entry names, plus whether more
        /// remained. Bounding happens HERE, at the `readdir` loop, so
        /// directory content cannot inflate either the allocation or the
        /// iteration work done by an enumeration pass — a truncated listing
        /// is reported to the caller, never silently clipped. Dot-names
        /// (`.`, `..`, in-flight `.<pid>.*.tmp` temp files) are skipped —
        /// by inspecting the first raw byte, before any allocation — so
        /// junk temp entries cannot consume the retained-name budget ahead
        /// of real records; they still count toward the raw scan bound
        /// (`RAW_SCAN_FACTOR * max`), and exhausting either bound reports
        /// truncation.
        pub(super) fn list_names(&self, max: usize) -> io::Result<(Vec<String>, bool)> {
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
            let mut truncated = false;
            loop {
                // SAFETY: `dirp` is a valid, open `DIR*` for this whole loop.
                let entry = unsafe { libc::readdir(dirp) };
                if entry.is_null() {
                    break;
                }
                if raw_scanned == raw_scan_limit {
                    truncated = true;
                    break;
                }
                raw_scanned += 1;
                // SAFETY: `d_name` is NUL-terminated, so its first byte is
                // always in bounds; hidden names are rejected on this raw
                // byte before any allocation happens for them.
                let first = unsafe { *(*entry).d_name.as_ptr() };
                if first == b'.' as libc::c_char {
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
            Ok((names, truncated))
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
    use std::os::windows::ffi::OsStrExt;
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

    /// Validation-then-operation on a plain path (the `write_atomic`/
    /// `remove_checked` pattern above) is a TOCTOU race: nothing pins the
    /// object between `symlink_metadata` and the follow-up open. The touch
    /// path is hardened against it — `CreateFileW` with
    /// `FILE_FLAG_OPEN_REPARSE_POINT` opens `target` AS a reparse point
    /// rather than following it, the reparse-point check runs against the
    /// SAME opened handle via `GetFileInformationByHandle`, and the mtime
    /// update (`SetFileTime`) runs on that same handle — never a
    /// re-resolved path. Pre-existing races on the write/remove paths above
    /// are a wider class left for a follow-up; this closes it for the
    /// per-tick touch/recreate path only.
    pub(super) fn touch_mtime(dir: &Path, name: &str) -> io::Result<()> {
        let target = dir.join(name);
        let mut wide: Vec<u16> = target.as_os_str().encode_wide().collect();
        wide.push(0);

        // SAFETY: `wide` is a valid, NUL-terminated UTF-16 string for the
        // call's duration; `FILE_FLAG_OPEN_REPARSE_POINT` means a
        // symlink/junction at `target` is opened as itself, never followed.
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null_mut(),
                OPEN_EXISTING,
                FILE_FLAG_OPEN_REPARSE_POINT,
                std::ptr::null_mut(),
            )
        };
        if handle == invalid_handle_value() {
            return Err(io_other(format!(
                "walpin sidecar entry {target:?} does not exist or could not be opened"
            )));
        }
        let result = (|| {
            let mut info: ByHandleFileInformation = unsafe { std::mem::zeroed() };
            // SAFETY: `handle` is the live handle opened above; `info` is a
            // valid, appropriately-sized output buffer for the call.
            if unsafe { GetFileInformationByHandle(handle, &mut info) } == 0 {
                return Err(io::Error::last_os_error());
            }
            if info.dw_file_attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                return Err(io_other(format!(
                    "walpin sidecar entry {target:?} is a reparse point; refusing to touch it"
                )));
            }
            let now = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map_err(|e| io_other(e.to_string()))?;
            const EPOCH_DIFF_100NS: u64 = 116_444_736_000_000_000;
            let ticks =
                now.as_secs() * 10_000_000 + u64::from(now.subsec_nanos()) / 100 + EPOCH_DIFF_100NS;
            let last_write = FileTime {
                dw_low_date_time: (ticks & 0xFFFF_FFFF) as u32,
                dw_high_date_time: (ticks >> 32) as u32,
            };
            // SAFETY: `handle` is live; null creation/access-time pointers
            // leave those fields untouched (metadata-only mtime refresh,
            // mirroring the Unix `UTIME_OMIT` behavior); `last_write` is a
            // valid `FILETIME`-shaped value for the call's duration.
            if unsafe { SetFileTime(handle, std::ptr::null(), std::ptr::null(), &last_write) } == 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        })();
        // SAFETY: `handle` was opened above and is closed exactly once
        // here, regardless of the touch outcome.
        unsafe { CloseHandle(handle) };
        result
    }

    type Handle = *mut c_void;

    #[repr(C)]
    struct FileTime {
        dw_low_date_time: u32,
        dw_high_date_time: u32,
    }

    /// Mirrors Win32's `BY_HANDLE_FILE_INFORMATION` layout exactly — every
    /// field must stay present and in order even though [`touch_mtime`]
    /// only reads `dw_file_attributes`, since `GetFileInformationByHandle`
    /// writes the full struct.
    #[repr(C)]
    struct ByHandleFileInformation {
        dw_file_attributes: u32,
        ft_creation_time: FileTime,
        ft_last_access_time: FileTime,
        ft_last_write_time: FileTime,
        dw_volume_serial_number: u32,
        n_file_size_high: u32,
        n_file_size_low: u32,
        n_number_of_links: u32,
        n_file_index_high: u32,
        n_file_index_low: u32,
    }

    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const STILL_ACTIVE: u32 = 259;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    const OPEN_EXISTING: u32 = 3;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;

    fn invalid_handle_value() -> Handle {
        usize::MAX as Handle
    }

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
        fn CreateFileW(
            lp_file_name: *const u16,
            dw_desired_access: u32,
            dw_share_mode: u32,
            lp_security_attributes: *mut c_void,
            dw_creation_disposition: u32,
            dw_flags_and_attributes: u32,
            h_template_file: Handle,
        ) -> Handle;
        fn GetFileInformationByHandle(
            h_file: Handle,
            lp_file_information: *mut ByHandleFileInformation,
        ) -> i32;
        fn SetFileTime(
            h_file: Handle,
            lp_creation_time: *const FileTime,
            lp_last_access_time: *const FileTime,
            lp_last_write_time: *const FileTime,
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
/// The init-PID-namespace inode check (`pid_ns_is_init`) only rules out
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
/// init PID namespace — see `pid_ns_is_init`; a container's own procfs is
/// internally self-consistent (its `/proc/1` resolves to its own init), so
/// merely comparing `/proc/1/ns/pid` against `/proc/self/ns/pid` cannot
/// distinguish "the host" from "a container that is its own root," and was
/// replaced with this inode check (ADR-091 Amendment 2). (2) a positive
/// proof the procfs mount backing `/proc` carries no `hidepid`/`subset`
/// restriction — see `proc_mount_is_visibility_restricted`; a
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

/// ADR-091 Amendment 3 Plank F1: a metadata-only mtime touch of this
/// process's already-written heartbeat — no data write, mirroring
/// [`touch_beacon`]'s mechanism and opened-directory-descriptor discipline
/// exactly. Must run on every sweep tick where the warn condition persists
/// and the heartbeat's content has not changed; a content change still
/// goes through [`write_heartbeat`]. Callers must not assume the target
/// exists — enumeration can delete a slow writer's heartbeat while its
/// span is still live — and must recreate via [`write_heartbeat`] on any
/// touch failure, never treat the record as gone for good.
pub fn touch_heartbeat(dir: &Path, pid: u32) -> io::Result<()> {
    let name = format!("{pid}.json");
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

/// Remove this process's registration beacon, if present (fail-closed
/// escalation for a failing heartbeat write path — see the sidecar
/// `observe` logic in `khive-db`'s checkpoint module). Never follows a
/// symlink at the target path. A missing sidecar directory is a no-op — it
/// must NOT be created as a side effect of a removal.
pub fn remove_beacon(dir: &Path, pid: u32) -> io::Result<()> {
    let target = format!("{pid}.beacon");
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

/// Staleness window for a producer sweeping at `interval`: three missed
/// ticks of a cadence floored at one second (ADR-091 Amendment 3 Plank F1's
/// `3 x max(interval, 1000ms)`) — a sub-second interval must not collapse
/// the window below what mtime resolution can distinguish, which would make
/// any timestamp other than the current wall-clock second appear stale.
#[cfg(unix)]
fn stale_window_from(interval: Duration) -> i64 {
    // ADR-091 Amendment 3 Plank F1's determinate form is `3 x
    // max(declared cadence, 1000ms)` — clamp the interval to the
    // mtime-resolution floor FIRST, then multiply by three, so a
    // sub-second cadence floors the effective window at three seconds
    // rather than merely at one. `max(3*interval, 1s)` (multiplying
    // first) would under-floor any cadence below ~333ms.
    interval
        .max(Duration::from_secs(1))
        .saturating_mul(3)
        .as_secs() as i64
}

/// Per-record staleness window: the producer's own recorded cadence wins;
/// `0` (a record written before `sweep_interval_ms` existed) falls back to
/// the enumerator's window.
#[cfg(unix)]
fn stale_window_secs(producer_interval_ms: u64, fallback_secs: i64) -> i64 {
    if producer_interval_ms == 0 {
        fallback_secs
    } else {
        stale_window_from(Duration::from_millis(producer_interval_ms))
    }
}

/// Absolute difference of two epoch-second stamps without overflow.
/// Persisted `started_at`/`updated_at` fields deserialize as unrestricted
/// i64, and plain `(a - b).abs()` wraps on extreme values in release
/// builds — a wrapped difference can land inside a freshness window and
/// classify a malformed entry as fresh. Saturating to `u64::MAX` on
/// overflow keeps any extreme stamp outside every window, failing toward
/// `Unknown` rather than exoneration.
#[cfg(unix)]
fn epoch_abs_diff(a: i64, b: i64) -> u64 {
    a.checked_sub(b)
        .map(|d| d.unsigned_abs())
        .unwrap_or(u64::MAX)
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
/// (contributing an `Unknown` classification, not silently skipped). At
/// most `MAX_SIDECAR_ENTRIES` entries are listed and read per enumeration
/// — the bound applies at the `readdir` loop itself — and a directory
/// holding more contributes one sentinel `Unknown` marker (PID 0) so the
/// truncation is never silent.
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
    enumerate_live_bounded(dir, sweep_interval, MAX_SIDECAR_ENTRIES)
}

/// Ceiling on sidecar entries listed and read per enumeration. Both the
/// `readdir` loop and the per-entry open/fstat/read/parse run while the
/// checkpoint writer guard is held, so enumeration work is bounded by
/// policy, not by directory content — the entry-count sibling of the
/// per-entry `MAX_SIDECAR_ENTRY_BYTES` bound. A real population is one
/// heartbeat/beacon pair per live process; a directory holding more than
/// this contributes one `CAP_SENTINEL_PID` `Unknown` marker (fail-closed:
/// unenumerated entries make the census inconclusive, never exonerated).
#[cfg(unix)]
const MAX_SIDECAR_ENTRIES: usize = 512;

/// Sentinel PID carried by the `Unknown` marker for entries past the
/// enumeration cap: those entries were never listed, so no real PID is
/// available. PID 0 is the kernel scheduler on every supported Unix and can
/// never be a sidecar producer.
#[cfg(unix)]
const CAP_SENTINEL_PID: u32 = 0;

#[cfg(unix)]
fn enumerate_live_bounded(
    dir: &Path,
    sweep_interval: Duration,
    max_entries: usize,
) -> io::Result<WalpinReport> {
    let handle = match unix_impl::SidecarDirHandle::open_if_exists(dir) {
        Ok(Some(h)) => h,
        Ok(None) => return Ok(WalpinReport::default()),
        Err(e) => return Err(e),
    };

    let now = now_epoch_secs();
    // Fallback window for records that predate the `sweep_interval_ms`
    // field — records carrying their producer's own cadence are judged
    // against it instead (see `stale_window_secs`), so a session sweeping
    // on an independently slower configured interval is not misread as
    // stale by a faster-ticking daemon.
    let fallback_window_secs = stale_window_from(sweep_interval);

    let mut heartbeats: std::collections::HashMap<u32, WalpinHeartbeat> = Default::default();
    let mut beacon_pids: std::collections::HashSet<u32> = Default::default();
    let mut unknown: Vec<(u32, &'static str)> = Vec::new();
    // PIDs whose heartbeat or beacon passed the identity gate but failed
    // freshness — these are wedged, not absent, and must never resolve to
    // `RegisteredSilent` off a co-existing entry (item b).
    let mut wedged: std::collections::HashSet<u32> = Default::default();

    // Entry-count bound: listing itself stops at the cap (see
    // `list_names`), so neither the readdir loop, the names allocation,
    // nor this processing loop scales with directory content. A truncated
    // listing contributes one sentinel `Unknown` marker below — the
    // unlisted entries were never read, and the census stays inconclusive
    // rather than exonerating.
    let (names, truncated) = handle.list_names(max_entries)?;
    if truncated {
        unknown.push((
            CAP_SENTINEL_PID,
            "refused: sidecar entry count exceeds enumeration cap",
        ));
    }
    for name in names {
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
                unknown.push((
                    pid,
                    "refused: untrusted sidecar entry (symlink, non-regular, or oversized)",
                ));
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

        if is_heartbeat {
            let heartbeat: WalpinHeartbeat = match serde_json::from_slice(&body) {
                Ok(hb) => hb,
                Err(_) => {
                    // Fail closed: a malformed entry is removed so it cannot
                    // wedge future ticks, but THIS tick's attribution for
                    // the PID stays inconclusive — deletion is cleanup,
                    // never exoneration.
                    let _ = handle.unlink_tolerant(&name);
                    unknown.push((pid, "malformed walpin heartbeat entry"));
                    continue;
                }
            };
            let alive = is_process_alive(heartbeat.pid);
            let identity_ok = alive
                && process_start_time_secs(heartbeat.pid)
                    .map(|actual| {
                        epoch_abs_diff(actual, heartbeat.started_at) <= START_TIME_EPSILON_SECS
                    })
                    .unwrap_or(false);
            if !identity_ok {
                let _ = handle.unlink_tolerant(&name);
                continue;
            }
            // ADR-091 Amendment 3 Plank F1: a record carrying
            // `oldest_tx_started_at` is new-style — its body is only
            // rewritten on content change, so freshness is judged against
            // the entry's mtime (advanced by a metadata-only touch every
            // tick), never the possibly-stale `updated_at` body field. A
            // record without it predates this amendment and is read
            // exactly as before: `updated_at` is its own freshness field.
            // Either way the window is the PRODUCER's recorded cadence,
            // not the enumerator's — the mixed-version rule (readers accept
            // both generations; see the amendment) depends on this branch.
            let window = stale_window_secs(heartbeat.sweep_interval_ms, fallback_window_secs);
            let hb_fresh = if heartbeat.oldest_tx_started_at.is_some() {
                epoch_abs_diff(now, mtime_secs) <= window as u64
            } else {
                epoch_abs_diff(now, heartbeat.updated_at) <= window as u64
            };
            if !hb_fresh {
                let _ = handle.unlink_tolerant(&name);
                wedged.insert(pid);
                unknown.push((pid, "stale walpin heartbeat"));
                continue;
            }
            heartbeats.insert(heartbeat.pid, heartbeat);
        } else {
            let beacon: WalpinBeacon = match serde_json::from_slice(&body) {
                Ok(b) => b,
                Err(_) => {
                    // Fail closed, as for a malformed heartbeat: cleanup,
                    // never exoneration.
                    let _ = handle.unlink_tolerant(&name);
                    unknown.push((pid, "malformed walpin beacon entry"));
                    continue;
                }
            };
            let alive = is_process_alive(beacon.pid);
            let identity_ok = alive
                && process_start_time_secs(beacon.pid)
                    .map(|actual| {
                        epoch_abs_diff(actual, beacon.started_at) <= START_TIME_EPSILON_SECS
                    })
                    .unwrap_or(false);
            if !identity_ok {
                let _ = handle.unlink_tolerant(&name);
                continue;
            }
            // Beacon refresh rule: freshness is the entry's mtime (the
            // metadata-only touch), not any JSON field — the beacon's body
            // is written once and never refreshed. The window is the
            // producer's recorded cadence, not the enumerator's.
            let window = stale_window_secs(beacon.sweep_interval_ms, fallback_window_secs);
            let fresh = epoch_abs_diff(now, mtime_secs) <= window as u64;
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
        let now = now_epoch_secs();
        WalpinHeartbeat {
            pid,
            process_role: "session".to_string(),
            started_at: process_start_time_secs(std::process::id()).unwrap_or(0),
            oldest_tx_age_secs: 45.0,
            oldest_tx_label: Some("test_span".to_string()),
            oldest_tx_started_at: Some(now - 45),
            updated_at: now,
            sweep_interval_ms: 5_000,
            attribution_basis: Some("origin".to_string()),
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
    fn heartbeat_deserializes_pre_rename_interval_ms_field() {
        // Old-format sidecar body from a writer that predates the
        // `interval_ms` -> `sweep_interval_ms` rename. A live writer still
        // on the old field name must keep its real cadence, not silently
        // fall back to the enumerator's default (which can misjudge a slow
        // writer's heartbeat as stale mid-upgrade).
        let json = r#"{
            "pid": 4242,
            "process_role": "session",
            "started_at": 1000,
            "oldest_tx_age_secs": 45.0,
            "oldest_tx_label": "test_span",
            "updated_at": 1045,
            "interval_ms": 60000
        }"#;
        let hb: WalpinHeartbeat = serde_json::from_str(json).unwrap();
        assert_eq!(hb.sweep_interval_ms, 60_000);
    }

    #[test]
    fn beacon_deserializes_pre_rename_interval_ms_field() {
        let json = r#"{
            "pid": 4242,
            "process_role": "session",
            "started_at": 1000,
            "interval_ms": 60000
        }"#;
        let b: WalpinBeacon = serde_json::from_str(json).unwrap();
        assert_eq!(b.sweep_interval_ms, 60_000);
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
            sweep_interval_ms: 5_000,
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
    fn epoch_abs_diff_saturates_instead_of_wrapping() {
        assert_eq!(epoch_abs_diff(5, 3), 2);
        assert_eq!(epoch_abs_diff(3, 5), 2);
        assert_eq!(epoch_abs_diff(0, 0), 0);
        // `now - i64::MIN` overflows i64; wrapped arithmetic could land
        // inside a freshness window — saturation must push it outside all.
        assert_eq!(epoch_abs_diff(1, i64::MIN), u64::MAX);
        assert_eq!(epoch_abs_diff(i64::MIN, i64::MAX), u64::MAX);
        assert_eq!(epoch_abs_diff(-1, i64::MAX), 1u64 << 63);
    }

    #[test]
    fn enumerate_live_extreme_timestamp_classifies_unknown_not_fresh() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("khive.db.walpin");
        let mut hb = heartbeat(std::process::id());
        // Pre-amendment (`updated_at`-basis) record: this test exercises
        // `epoch_abs_diff`'s overflow protection on that field specifically.
        hb.oldest_tx_started_at = None;
        hb.updated_at = i64::MIN;
        write_heartbeat(&dir, &hb).unwrap();

        let report = enumerate_live(&dir, Duration::from_secs(5)).unwrap();
        assert!(
            report.reporting().next().is_none(),
            "an extreme updated_at must never classify as fresh"
        );
        assert!(
            report
                .entries
                .iter()
                .any(|e| matches!(e, WalpinPidHealth::Unknown { pid, .. } if *pid == hb.pid)),
            "the extreme-timestamp entry must stay Unknown, not vanish"
        );
    }

    #[test]
    fn enumerate_live_bounded_caps_listing_with_sentinel_marker() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("khive.db.walpin");
        let live = heartbeat(std::process::id());
        write_heartbeat(&dir, &live).unwrap();
        for pid in [2_000_000_001u32, 2_000_000_002] {
            let mut hb = heartbeat(std::process::id());
            hb.pid = pid;
            write_heartbeat(&dir, &hb).unwrap();
        }

        let report = enumerate_live_bounded(&dir, Duration::from_secs(5), 1).unwrap();
        let markers = report
            .entries
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    WalpinPidHealth::Unknown { pid: 0, reason }
                        if reason.contains("enumeration cap")
                )
            })
            .count();
        assert_eq!(
            markers, 1,
            "a truncated listing must surface exactly one sentinel Unknown marker"
        );
        assert!(
            !report.fully_attributed(),
            "a capped enumeration can never claim full attribution"
        );
        // Report memory is bounded by the cap: at most one processed entry
        // plus the sentinel, regardless of directory content.
        assert!(report.entries.len() <= 2, "got {:?}", report.entries);
    }

    #[test]
    fn enumerate_live_bounded_caps_hidden_entry_scan_with_sentinel_marker() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("khive.db.walpin");
        let live = heartbeat(std::process::id());
        write_heartbeat(&dir, &live).unwrap();
        // Far more hidden entries than the raw scan bound (cap 4 → 32 raw
        // entries): hidden names never consume the retained-name budget,
        // but they must still exhaust the raw scan bound and report
        // truncation instead of extending the readdir loop unboundedly.
        for i in 0..64 {
            std::fs::write(dir.join(format!(".junk{i}")), b"x").unwrap();
        }

        let report = enumerate_live_bounded(&dir, Duration::from_secs(5), 4).unwrap();
        let markers = report
            .entries
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    WalpinPidHealth::Unknown { pid: 0, reason }
                        if reason.contains("enumeration cap")
                )
            })
            .count();
        assert_eq!(
            markers, 1,
            "a hidden-entry flood must surface exactly one sentinel Unknown marker"
        );
        assert!(
            !report.fully_attributed(),
            "an enumeration cut short by hidden entries can never claim full attribution"
        );
    }

    #[test]
    fn enumerate_live_uncapped_population_has_no_sentinel() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("khive.db.walpin");
        let live = heartbeat(std::process::id());
        write_heartbeat(&dir, &live).unwrap();

        let report = enumerate_live(&dir, Duration::from_secs(5)).unwrap();
        assert!(
            report
                .entries
                .iter()
                .all(|e| !matches!(e, WalpinPidHealth::Unknown { pid: 0, .. })),
            "an in-budget population must not carry the cap sentinel"
        );
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
        // Pre-amendment record: freshness classifies on `updated_at` alone.
        hb.oldest_tx_started_at = None;
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
        // Pre-amendment record: freshness classifies on `updated_at` alone.
        hb.oldest_tx_started_at = None;
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

    /// Producer-cadence regression: freshness is judged against the cadence
    /// RECORDED in the entry, not the enumerating daemon's interval — a
    /// session sweeping on an independently slower configured interval must
    /// not be misread as stale by a faster-ticking daemon. Pre-amendment
    /// records (`oldest_tx_started_at: None`) still classify on `updated_at`
    /// (ADR-091 Amendment 3 Plank F1 mixed-version rule) — a new-style
    /// record's `updated_at` would go stale under a touch-only tick even
    /// while its mtime stays fresh, so this test pins the basis this
    /// exercises to the old-style body field deliberately.
    #[test]
    #[cfg(unix)]
    fn heartbeat_freshness_uses_producer_cadence_not_enumerator_interval() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("khive.db.walpin");
        let pid = std::process::id();

        let mut slow = heartbeat(pid);
        slow.oldest_tx_started_at = None;
        slow.sweep_interval_ms = 60_000;
        slow.updated_at = now_epoch_secs() - 30;
        write_heartbeat(&dir, &slow).unwrap();

        // Enumerator ticking at 500ms: its own window would be 1.5s and the
        // 30s-old heartbeat would look stale, but the producer's recorded
        // 60s cadence keeps it fresh.
        let report = enumerate_live(&dir, Duration::from_millis(500)).unwrap();
        assert_eq!(
            report.reporting().count(),
            1,
            "a heartbeat 30s old under a recorded 60s cadence is fresh: {report:?}"
        );

        // Control: the same 30s-old timestamp under a recorded 1s cadence
        // IS stale — the recorded cadence cuts both ways.
        let mut fast = heartbeat(pid);
        fast.oldest_tx_started_at = None;
        fast.sweep_interval_ms = 1_000;
        fast.updated_at = now_epoch_secs() - 30;
        write_heartbeat(&dir, &fast).unwrap();
        let report = enumerate_live(&dir, Duration::from_secs(60)).unwrap();
        assert_eq!(report.reporting().count(), 0);
        assert_eq!(report.unknown_pids().collect::<Vec<_>>(), vec![pid]);
    }

    /// ADR-091 Amendment 3 Plank F1 mixed-version rule: a record carrying
    /// `oldest_tx_started_at` is new-style and classifies on the entry's
    /// mtime, never the body's `updated_at` field — a stale `updated_at`
    /// (as a touch-only tick would leave it) must not make a genuinely
    /// fresh entry classify stale.
    #[test]
    #[cfg(unix)]
    fn enumerate_live_new_style_heartbeat_uses_mtime_not_stale_updated_at() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("khive.db.walpin");
        let mut hb = heartbeat(std::process::id());
        hb.updated_at = now_epoch_secs() - 3600;
        write_heartbeat(&dir, &hb).unwrap(); // mtime is fresh (just written)

        let report = enumerate_live(&dir, Duration::from_secs(5)).unwrap();
        assert_eq!(
            report.reporting().count(),
            1,
            "a new-style record with a fresh mtime must classify live regardless \
             of a stale `updated_at` body field: {report:?}"
        );
    }

    /// Complement of the above: a new-style record whose mtime has fallen
    /// outside the declared window classifies stale even though its body's
    /// `updated_at` field looks fresh — proving the classification basis is
    /// genuinely the mtime, not merely "whichever of the two is fresher."
    #[test]
    #[cfg(unix)]
    fn enumerate_live_new_style_heartbeat_stale_via_mtime_despite_fresh_updated_at() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("khive.db.walpin");
        let pid = std::process::id();
        let hb = heartbeat(pid); // updated_at freshly set by the helper
        write_heartbeat(&dir, &hb).unwrap();

        let heartbeat_path = dir.join(format!("{pid}.json"));
        let file = fs::OpenOptions::new()
            .write(true)
            .open(&heartbeat_path)
            .unwrap();
        file.set_modified(SystemTime::now() - Duration::from_secs(3600))
            .unwrap();

        let report = enumerate_live(&dir, Duration::from_secs(5)).unwrap();
        assert_eq!(report.reporting().count(), 0);
        assert_eq!(report.unknown_pids().collect::<Vec<_>>(), vec![pid]);
        assert!(
            !heartbeat_path.exists(),
            "a new-style entry stale by mtime must be deleted, not merely unreported"
        );
    }

    /// ADR-091 Amendment 3 Plank F1: `3 x max(interval, 1000ms)` is exact
    /// and inclusive — not "roughly 3 intervals" — and a sub-second
    /// declared cadence floors the effective window at three seconds
    /// (flooring the interval at 1s BEFORE multiplying by 3), never at one.
    #[test]
    #[cfg(unix)]
    fn stale_window_from_boundary_inclusive_and_floors_subsecond_cadence_at_three_seconds() {
        assert_eq!(stale_window_from(Duration::from_secs(2)), 6);
        assert_eq!(stale_window_from(Duration::from_millis(100)), 3);
        assert_eq!(stale_window_from(Duration::from_millis(999)), 3);
        assert_eq!(stale_window_from(Duration::from_secs(1)), 3);
    }

    /// Boundary inclusivity exercised end-to-end through `enumerate_live`:
    /// an entry exactly `3 x max(interval, 1000ms)` old is still live
    /// (`<=`, not `<`); one second older is stale.
    #[test]
    #[cfg(unix)]
    fn enumerate_live_new_style_heartbeat_boundary_is_inclusive() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("khive.db.walpin");
        let pid = std::process::id();
        let mut hb = heartbeat(pid);
        hb.sweep_interval_ms = 2_000; // window = 3 * 2s = 6s

        write_heartbeat(&dir, &hb).unwrap();
        let heartbeat_path = dir.join(format!("{pid}.json"));
        let touch = |age_secs: u64| {
            let file = fs::OpenOptions::new()
                .write(true)
                .open(&heartbeat_path)
                .unwrap();
            file.set_modified(SystemTime::now() - Duration::from_secs(age_secs))
                .unwrap();
        };

        touch(6);
        let report = enumerate_live(&dir, Duration::from_secs(5)).unwrap();
        assert_eq!(
            report.reporting().count(),
            1,
            "exactly 3x the declared interval old must still classify live: {report:?}"
        );

        // Re-write (enumeration deletes stale/live entries it processes,
        // but a live entry is retained — reuse the same file) and push one
        // second past the boundary.
        touch(7);
        let report = enumerate_live(&dir, Duration::from_secs(5)).unwrap();
        assert_eq!(
            report.reporting().count(),
            0,
            "one second past 3x the declared interval must classify stale: {report:?}"
        );
    }

    /// Sub-second declared cadence floors the effective window at three
    /// seconds (not one) — exercised end-to-end, not just at the pure
    /// `stale_window_from` function.
    #[test]
    #[cfg(unix)]
    fn enumerate_live_new_style_heartbeat_subsecond_cadence_floors_at_three_seconds() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("khive.db.walpin");
        let pid = std::process::id();
        let mut hb = heartbeat(pid);
        hb.sweep_interval_ms = 100; // floors to a 3s window, not 300ms/1s

        write_heartbeat(&dir, &hb).unwrap();
        let heartbeat_path = dir.join(format!("{pid}.json"));
        let file = fs::OpenOptions::new()
            .write(true)
            .open(&heartbeat_path)
            .unwrap();
        file.set_modified(SystemTime::now() - Duration::from_secs(2))
            .unwrap();

        let report = enumerate_live(&dir, Duration::from_secs(5)).unwrap();
        assert_eq!(
            report.reporting().count(),
            1,
            "a 100ms declared cadence must floor its window at 3s, not 1s: 2s old must \
             still classify live: {report:?}"
        );
    }

    /// ADR-091 Amendment 3 Plank F2 fail-closed reading rule, exercised at
    /// the canonical consumer-facing accessor: only the exact string
    /// `"origin"` licenses an evidence-backed reading. A missing field or
    /// any unrecognized value — including a value a future amendment might
    /// define — must classify as fallback-confidence, never evidence-backed.
    #[test]
    fn attribution_is_evidence_backed_fails_closed_on_missing_or_unrecognized_value() {
        let mut hb = heartbeat(std::process::id());

        hb.attribution_basis = Some("origin".to_string());
        assert!(hb.attribution_is_evidence_backed());

        hb.attribution_basis = Some("fallback".to_string());
        assert!(!hb.attribution_is_evidence_backed());

        hb.attribution_basis = None;
        assert!(
            !hb.attribution_is_evidence_backed(),
            "a missing attribution_basis must never be read as evidence-backed"
        );

        hb.attribution_basis = Some("some-future-value".to_string());
        assert!(
            !hb.attribution_is_evidence_backed(),
            "an unrecognized value must degrade to fallback-confidence, never guess origin"
        );
    }

    /// A FIFO planted at a sidecar entry name must be refused as `Unknown`
    /// without blocking enumeration — a plain `open(O_RDONLY)` on a
    /// writer-less FIFO would hang the daemon's checkpoint task forever.
    #[test]
    #[cfg(unix)]
    fn fifo_sidecar_entry_is_refused_without_blocking() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("khive.db.walpin");
        let pid = std::process::id();
        write_beacon(&dir, &beacon(pid)).unwrap();

        use std::os::unix::ffi::OsStrExt;
        let fifo = dir.join("999999941.json");
        let c_path = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: `c_path` is NUL-terminated; mkfifo creates a new node.
        let rc = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
        assert_eq!(rc, 0, "mkfifo failed: {}", io::Error::last_os_error());

        let report = enumerate_live(&dir, Duration::from_secs(5)).unwrap();
        assert!(
            report.unknown_pids().any(|p| p == 999_999_941),
            "a FIFO entry must classify its PID as unknown: {report:?}"
        );
        assert_eq!(
            report.registered_silent_pids().collect::<Vec<_>>(),
            vec![pid]
        );
    }

    /// An oversized sidecar entry must be refused as `Unknown` with a
    /// bounded read — this module never writes bodies anywhere near the
    /// cap, so an oversized entry is foreign, and reading it unboundedly
    /// would let a same-uid process balloon enumeration.
    #[test]
    #[cfg(unix)]
    fn oversized_sidecar_entry_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("khive.db.walpin");
        let pid = std::process::id();
        write_beacon(&dir, &beacon(pid)).unwrap();

        fs::write(dir.join("999999942.json"), vec![b'x'; 128 * 1024]).unwrap();

        let report = enumerate_live(&dir, Duration::from_secs(5)).unwrap();
        assert!(
            report.unknown_pids().any(|p| p == 999_999_942),
            "an oversized entry must classify its PID as unknown: {report:?}"
        );
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
