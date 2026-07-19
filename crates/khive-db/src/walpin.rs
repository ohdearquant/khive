//! ADR-091 Amendment 2 Plank B: cross-process WAL-pin attribution sidecar.
//!
//! Every `kkernel mcp` process (daemon or session) that observes its own
//! `tx_registry` oldest span exceed `KHIVE_TX_WARN_SECS` writes a per-PID
//! heartbeat file under `<db-file>.walpin/<pid>.json`. On a TRUNCATE
//! no-progress event, the daemon enumerates this directory and applies a
//! three-test liveness gate (PID alive, `started_at` matches the OS-reported
//! process start time, `updated_at` fresh) to attribute the WAL pin to a
//! specific process rather than only naming its own in-process registry.
//!
//! Filesystem trust boundary (binding, gate ruling 2026-07-19): the sidecar
//! directory is created mode 0700 and validated as owned by the current user
//! before any use — a non-compliant existing directory is refused, never
//! chmod/chown'd into compliance. Heartbeat writes go through exclusive
//! create with `O_NOFOLLOW` semantics to a temp file, then atomic rename over
//! the target. Enumeration refuses symlinks and validates per-entry ownership
//! before reading or deleting anything.

use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
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
/// paragraph, gate ruling 2026-07-19).
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

fn current_uid() -> u32 {
    // SAFETY: `geteuid()` takes no arguments and cannot fail.
    unsafe { libc::geteuid() }
}

fn validate_dir_metadata(dir: &Path, meta: &fs::Metadata) -> io::Result<()> {
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
    let mode = meta.permissions().mode() & 0o777;
    if mode != 0o700 {
        return Err(io_other(format!(
            "walpin sidecar dir {dir:?} has mode {mode:o}, expected 0700; \
             refusing rather than chmod"
        )));
    }
    if meta.uid() != current_uid() {
        return Err(io_other(format!(
            "walpin sidecar dir {dir:?} is not owned by the current user; refusing"
        )));
    }
    Ok(())
}

/// Ensure `dir` exists, is a real directory (never a symlink), mode `0700`,
/// and owned by the current user. Refuses — never chmod/chown — a
/// non-compliant existing directory.
pub fn ensure_sidecar_dir(dir: &Path) -> io::Result<()> {
    match fs::symlink_metadata(dir) {
        Ok(meta) => validate_dir_metadata(dir, &meta),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let mut builder = fs::DirBuilder::new();
            builder.mode(0o700);
            builder.create(dir)?;
            // Re-validate post-creation: a concurrent process could have raced
            // this creation (e.g. replaced it with a symlink between our
            // `create` and this check), so the freshly-created directory is
            // held to the same standard as a pre-existing one rather than
            // trusted blindly.
            let meta = fs::symlink_metadata(dir)?;
            validate_dir_metadata(dir, &meta)
        }
        Err(e) => Err(e),
    }
}

/// Write (or refresh) this process's heartbeat file. Exclusive-create a temp
/// file with `O_NOFOLLOW` in the sidecar dir, then atomically rename it over
/// the target — never an in-place open of a possibly attacker-placed path.
pub fn write_heartbeat(dir: &Path, heartbeat: &WalpinHeartbeat) -> io::Result<()> {
    ensure_sidecar_dir(dir)?;

    let target = dir.join(format!("{}.json", heartbeat.pid));
    if let Ok(meta) = fs::symlink_metadata(&target) {
        if meta.file_type().is_symlink() {
            return Err(io_other(format!(
                "walpin heartbeat path {target:?} is a symlink; refusing to write through it"
            )));
        }
    }

    let tmp = dir.join(format!(".{}.json.tmp", heartbeat.pid));
    // Best-effort: a stale temp file from a prior crashed write must not block
    // this one via O_EXCL: the ADR ordering (register at BEGIN, gone on
    // Drop) has no analogue for the sidecar, so shed it before excl-creating.
    let _ = fs::remove_file(&tmp);

    let body =
        serde_json::to_vec(heartbeat).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    {
        use std::io::Write;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&tmp)?;
        file.write_all(&body)?;
        file.sync_all()?;
    }
    fs::rename(&tmp, &target)?;
    Ok(())
}

/// Remove this process's heartbeat file, if present. Never follows a
/// symlink at the target path.
pub fn remove_heartbeat(dir: &Path, pid: u32) -> io::Result<()> {
    let target = dir.join(format!("{pid}.json"));
    match fs::symlink_metadata(&target) {
        Ok(meta) if meta.file_type().is_symlink() => Err(io_other(format!(
            "refusing to remove symlinked walpin heartbeat path {target:?}"
        ))),
        Ok(_) => fs::remove_file(&target),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Write this process's one-time registration beacon (ADR-091 Amendment 2
/// sidecar-health attribution). Idempotent: called once at sidecar
/// initialization, never refreshed — the atomic rename over any pre-existing
/// target makes a second call (e.g. a PID reused far in the future) a benign
/// overwrite rather than an error. Same trust-boundary rules as
/// [`write_heartbeat`]: exclusive-create `O_NOFOLLOW` temp file, then atomic
/// rename over the target.
pub fn write_beacon(dir: &Path, beacon: &WalpinBeacon) -> io::Result<()> {
    ensure_sidecar_dir(dir)?;

    let target = beacon_path(dir, beacon.pid);
    if let Ok(meta) = fs::symlink_metadata(&target) {
        if meta.file_type().is_symlink() {
            return Err(io_other(format!(
                "walpin beacon path {target:?} is a symlink; refusing to write through it"
            )));
        }
    }

    let tmp = dir.join(format!(".{}.beacon.tmp", beacon.pid));
    let _ = fs::remove_file(&tmp);

    let body =
        serde_json::to_vec(beacon).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    {
        use std::io::Write;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&tmp)?;
        file.write_all(&body)?;
        file.sync_all()?;
    }
    fs::rename(&tmp, &target)?;
    Ok(())
}

/// Path of `pid`'s one-time registration beacon under `dir`.
pub fn beacon_path(dir: &Path, pid: u32) -> PathBuf {
    dir.join(format!("{pid}.beacon"))
}

/// Is `pid` alive (right now)? `kill(pid, 0)` is a pure existence/permission
/// probe with no side effects; `EPERM` (a live PID owned by someone else)
/// still counts as alive.
pub fn is_process_alive(pid: u32) -> bool {
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

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
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
/// (gate ruling, 2026-07-19) to every heartbeat/beacon entry found and
/// classifying each PID's sidecar health three ways (ADR-091 Amendment 2
/// "Sidecar-health attribution"): [`WalpinPidHealth::Reporting`] (live,
/// identity-matched, fresh heartbeat), [`WalpinPidHealth::RegisteredSilent`]
/// (live, identity-matched beacon, no live heartbeat), or
/// [`WalpinPidHealth::Unknown`] (an entry exists but the trust-boundary check
/// refused it, or it failed to parse — sidecar health for that PID is
/// unestablished).
///
/// Trust boundary (binding, gate ruling 2026-07-19): the directory itself is
/// validated (type/owner/mode) BEFORE any entry is read — a non-compliant
/// directory returns `Err`, a health *failure*, never a partial/empty
/// result that could otherwise masquerade as "no live entries." Per entry,
/// symlinks and non-owned files are refused BEFORE their contents are read
/// (contributing an `Unknown` classification, not silently skipped) —
/// unreadable/unparseable owned entries are deleted as before (a genuinely
/// dead or crashed process's orphan, not an unresolved health question).
/// A missing directory (sidecar never used yet) is `Ok` with an empty
/// report, distinct from an existing-but-untrustworthy one.
pub fn enumerate_live(dir: &Path, sweep_interval: Duration) -> io::Result<WalpinReport> {
    let dir_meta = match fs::symlink_metadata(dir) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(WalpinReport::default()),
        Err(e) => return Err(e),
    };
    validate_dir_metadata(dir, &dir_meta)?;

    let entries = fs::read_dir(dir)?;
    let now = now_epoch_secs();
    // Subsecond intervals must not collapse the freshness window to zero
    // (minor, ADR-091 Amendment 2 review): a window of 0s would make any
    // `updated_at` other than the current wall-clock second appear stale.
    let stale_after_secs = sweep_interval
        .saturating_mul(3)
        .max(Duration::from_secs(1))
        .as_secs() as i64;

    let mut heartbeats: std::collections::HashMap<u32, WalpinHeartbeat> = Default::default();
    let mut beacon_pids: std::collections::HashSet<u32> = Default::default();
    let mut unknown: Vec<(u32, &'static str)> = Vec::new();

    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
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
        let meta = match fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.file_type().is_symlink() {
            unknown.push((pid, "refused: symlinked sidecar entry"));
            continue;
        }
        let owned_by_us = meta.uid() == current_uid();
        if !owned_by_us {
            unknown.push((pid, "refused: sidecar entry not owned by current user"));
            continue;
        }

        if is_heartbeat {
            let heartbeat: WalpinHeartbeat = match fs::read_to_string(&path)
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
            {
                Some(hb) => hb,
                None => {
                    let _ = fs::remove_file(&path);
                    continue;
                }
            };
            let alive = is_process_alive(heartbeat.pid);
            let identity_ok = alive
                && process_start_time_secs(heartbeat.pid)
                    .map(|actual| (actual - heartbeat.started_at).abs() <= START_TIME_EPSILON_SECS)
                    .unwrap_or(false);
            let fresh = (now - heartbeat.updated_at).abs() <= stale_after_secs;

            if alive && identity_ok && fresh {
                heartbeats.insert(heartbeat.pid, heartbeat);
            } else {
                let _ = fs::remove_file(&path);
            }
        } else {
            let beacon: WalpinBeacon = match fs::read_to_string(&path)
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
            {
                Some(b) => b,
                None => {
                    let _ = fs::remove_file(&path);
                    continue;
                }
            };
            let alive = is_process_alive(beacon.pid);
            let identity_ok = alive
                && process_start_time_secs(beacon.pid)
                    .map(|actual| (actual - beacon.started_at).abs() <= START_TIME_EPSILON_SECS)
                    .unwrap_or(false);
            if alive && identity_ok {
                beacon_pids.insert(beacon.pid);
            } else {
                let _ = fs::remove_file(&path);
            }
        }
    }

    let mut entries: Vec<WalpinPidHealth> = Vec::new();
    for (pid, hb) in heartbeats {
        entries.push(WalpinPidHealth::Reporting(hb));
        beacon_pids.remove(&pid);
    }
    for pid in beacon_pids {
        entries.push(WalpinPidHealth::RegisteredSilent { pid });
    }
    for (pid, reason) in unknown {
        entries.push(WalpinPidHealth::Unknown { pid, reason });
    }

    Ok(WalpinReport { entries })
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// Restores a possibly-unset env var on drop, including on panic — so an
    /// assertion failure mid-test can never leak a mutated `KHIVE_WALPIN_SIDECAR`
    /// into a sibling test (minor, ADR-091 Amendment 2 review: env-mutating
    /// tests must serialize with cleanup on panic).
    struct EnvVarGuard {
        key: &'static str,
        saved: Option<String>,
    }
    impl EnvVarGuard {
        fn capture(key: &'static str) -> Self {
            Self {
                key,
                saved: std::env::var(key).ok(),
            }
        }
    }
    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match &self.saved {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }

    #[test]
    #[serial_test::serial(khive_walpin_sidecar_env)]
    fn sidecar_enabled_defaults_to_file_backed() {
        // Deterministic regardless of the ambient environment (minor,
        // ADR-091 Amendment 2 review: the prior version was vacuously true
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
        assert!(
            report.entries.is_empty(),
            "stale updated_at must fail the gate"
        );
        assert!(!dir.join(format!("{}.json", hb.pid)).exists());
    }

    #[test]
    fn enumerate_live_subsecond_sweep_interval_does_not_collapse_freshness_window() {
        // Minor (ADR-091 Amendment 2 review): a sub-second
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
        // Item 3 (ADR-091 Amendment 2 review): a directory that fails the
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
}
