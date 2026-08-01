//! Non-mutating-by-intent WAL/checkpoint diagnostics (read-only operator
//! surface).
//!
//! Answers "is a reader pinning the checkpoint / why is the WAL at 64MiB"
//! without raw SQL against a production store.
//!
//! NOT read-only. [`checkpoint_probe`] issues a real
//! `PRAGMA wal_checkpoint(PASSIVE)`, and a PASSIVE checkpoint that succeeds
//! BACKFILLS WAL frames into the main database — ordinary database-page
//! writes, on the happy path. That I/O is the point: the busy/log_frames/
//! checkpointed_frames triple is the pin-depth signal this surface exists to
//! report, and SQLite exposes no read-only API for those counters. The
//! guarantee is that nothing here changes logical state or destroys
//! evidence, not that nothing touches the disk.
//!
//! What IS guaranteed, and what the narrowings below buy:
//!
//! * never creates a missing database file,
//! * never escalates to TRUNCATE,
//! * never perturbs the counters it reports,
//! * never deletes a walpin sidecar entry.
//!
//! Deliberate narrowings make those claims true rather than aspirational:
//!
//! 1. [`checkpoint_probe`] runs a single `PRAGMA wal_checkpoint(PASSIVE)` —
//!    which never blocks readers or writers — and does NOT route through
//!    [`crate::checkpoint::checkpoint_once`]: that path mutates
//!    `TruncateState`, may escalate to TRUNCATE, and double-counts the
//!    ADR-091 process-global counters. A verb that reports state must not
//!    perturb the state it reports.
//! 2. The probe's connection comes from
//!    `ConnectionPool::open_standalone_writer`, opened without
//!    `SQLITE_OPEN_CREATE`. A missing database yields `checkpoint_probe:
//!    null` plus a `checkpoint_probe_error`, never a freshly created file.
//! 3. The WAL-pin sidecar directory in this crate is enumerated only through
//!    the read-only OS holder census (`walpin::census_holders`). This
//!    tree's `khive-db` exposes sidecar directory enumeration only via
//!    `walpin::enumerate_live`, which unlinks malformed, dead,
//!    identity-mismatched, and stale entries as part of its cleanup pass —
//!    a diagnostic request must not be what destroys that forensic
//!    evidence, so `wal_pin_attribution` deliberately does not call it.
//!    Sidecar-to-holder reconciliation (`reporting`/`registered_silent_pids`/
//!    `sidecar_entries`/`fully_attributed`) is therefore reported empty with
//!    an explicit `unavailable_reason` rather than fabricated from a
//!    mutating enumeration.
//!
//! The counters are process-global statics inside this crate, so a report is
//! only meaningful when built inside the process that owns the checkpoint
//! task (the daemon). Every payload therefore carries [`BuildIdentity`], so a
//! reading is self-labeling about which build's counters it describes.

use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::Connection;
use serde::Serialize;

use crate::checkpoint;
use crate::pool::ConnectionPool;

/// Raw `PRAGMA wal_checkpoint(PASSIVE)` return row.
///
/// SQLite returns three columns: `busy` (1 when the checkpoint could not run
/// to completion because a writer/reader held it back), `log` (frames
/// currently in the WAL), and `checkpointed` (frames moved into the database
/// by THIS call). Pin depth is `log - checkpointed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CheckpointProbe {
    pub busy: i64,
    pub log_frames: i64,
    pub checkpointed_frames: i64,
}

impl CheckpointProbe {
    /// Frames still pinned behind the backfill boundary. Negative components
    /// (an in-memory or non-WAL database reports `-1`) clamp to 0.
    pub fn pin_depth(&self) -> i64 {
        (self.log_frames - self.checkpointed_frames).max(0)
    }
}

/// Issue one PASSIVE checkpoint on `conn` and return the raw triple.
///
/// PASSIVE never blocks writers or readers, so this is safe against a live
/// daemon. Touches none of the ADR-091 counters — unlike the periodic
/// checkpoint task's own observation path, this does not mirror into the
/// process-global gauges.
///
/// This WRITES. A PASSIVE checkpoint that makes progress backfills WAL
/// frames into the main database file; that is normal checkpoint I/O, and on
/// the happy path it is what the reported `checkpointed_frames` counts. The
/// call is non-mutating in the sense that matters for a diagnostic — no
/// logical state changes, no escalation, no evidence destroyed — but it is
/// not write-free, and callers must not describe it as such.
pub fn checkpoint_probe(conn: &Connection) -> rusqlite::Result<CheckpointProbe> {
    conn.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| {
        Ok(CheckpointProbe {
            busy: row.get(0)?,
            log_frames: row.get(1)?,
            checkpointed_frames: row.get(2)?,
        })
    })
}

/// The six ADR-091 checkpoint counters, read as one snapshot.
///
/// The two `Option` fields carry the `u64::MAX` "never observed" sentinel as
/// `None`, so a caller serializing this never sees `18446744073709551615`
/// where it means "no observation yet".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CheckpointCounters {
    pub last_observed_wal_pages: Option<u64>,
    pub truncate_attempts: u64,
    pub truncate_consecutive_failures: u64,
    pub checkpoint_skipped_ticks: u64,
    pub checkpoint_consecutive_skips: u64,
    pub checkpoint_last_skip_wal_pages: Option<u64>,
}

/// Snapshot the six process-global ADR-091 counters.
pub fn checkpoint_counters() -> CheckpointCounters {
    CheckpointCounters {
        last_observed_wal_pages: checkpoint::last_observed_wal_pages(),
        truncate_attempts: checkpoint::truncate_attempts(),
        truncate_consecutive_failures: checkpoint::truncate_consecutive_failures(),
        checkpoint_skipped_ticks: checkpoint::checkpoint_skipped_ticks(),
        checkpoint_consecutive_skips: checkpoint::checkpoint_consecutive_skips(),
        checkpoint_last_skip_wal_pages: checkpoint::checkpoint_last_skip_wal_pages(),
    }
}

/// Which build produced this reading.
///
/// The counters above are process-global: a stale daemon reports a stale
/// build's state. `build_hash` is `None` unless the binary was stamped with
/// one — this crate does not introduce a build-metadata mechanism, it only
/// reports whatever the caller already has.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BuildIdentity {
    pub version: String,
    pub build_hash: Option<String>,
}

impl BuildIdentity {
    /// Identity of the crate that compiled this call site.
    pub fn from_env(version: &str, build_hash: Option<&str>) -> Self {
        Self {
            version: version.to_string(),
            build_hash: build_hash.map(str::to_string),
        }
    }
}

/// Absolute-free WAL file state for one database path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WalFileState {
    pub wal_path: String,
    /// `None` when the `-wal` sidecar does not exist or could not be stat'd —
    /// the reason is in `unavailable_reason`.
    pub wal_size_bytes: Option<u64>,
    pub unavailable_reason: Option<String>,
}

/// Stat `<db_path>-wal` and report its size in bytes.
pub fn wal_file_state(db_path: &Path) -> WalFileState {
    let wal_path = wal_sidecar_path(db_path);
    match std::fs::metadata(&wal_path) {
        Ok(md) => WalFileState {
            wal_path: wal_path.display().to_string(),
            wal_size_bytes: Some(md.len()),
            unavailable_reason: None,
        },
        Err(e) => WalFileState {
            wal_path: wal_path.display().to_string(),
            wal_size_bytes: None,
            unavailable_reason: Some(e.to_string()),
        },
    }
}

/// `<db_path>-wal`, built by suffixing the file name (not by replacing an
/// extension — `khive.db` must map to `khive.db-wal`).
fn wal_sidecar_path(db_path: &Path) -> PathBuf {
    let mut s = db_path.as_os_str().to_os_string();
    s.push("-wal");
    PathBuf::from(s)
}

/// WAL-pin attribution: who currently holds the database open, and how
/// complete that answer is.
///
/// Field names and shape mirror the reference diagnostics surface this was
/// ported from, so external consumers parsing this payload keep working.
/// This tree's `khive-db` lacks a read-only sidecar enumeration primitive
/// (see the module docs), so the sidecar-derived fields below are always
/// empty here; `available`/`unavailable_reason` say so explicitly rather
/// than silently reporting an incomplete answer as complete.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WalPinAttribution {
    /// `false` plus an `unavailable_reason` whenever the OS census failed,
    /// or when only a partial (census-only) answer is available.
    pub available: bool,
    pub unavailable_reason: Option<String>,
    /// OS-derived census of every PID holding the DB file open.
    pub census_holder_pids: Vec<u32>,
    pub census_uninspectable_pids: Vec<u32>,
    pub census_truncated: bool,
    pub census_is_complete: bool,
    /// Always empty in this port — see the module docs.
    pub reporting: Vec<WalPinHolder>,
    /// Always empty in this port — see the module docs.
    pub registered_silent_pids: Vec<u32>,
    /// Always empty in this port — see the module docs.
    pub unknown_pids: Vec<u32>,
    /// Always empty in this port — see the module docs.
    pub census_pids_without_attribution: Vec<u32>,
    /// Always `false` in this port: sidecar reconciliation never ran, so
    /// completeness can never be claimed.
    pub fully_attributed: bool,
    /// Always empty in this port — see the module docs.
    pub sidecar_entries: Vec<serde_json::Value>,
    pub sidecar_listing_truncated: bool,
    pub sidecar_entries_cleanup_would_reap: usize,
}

/// One PID's heartbeat as reported to an operator. Retained for shape
/// compatibility with the reference payload; this port never populates it
/// (see [`WalPinAttribution`] docs).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WalPinHolder {
    pub pid: u32,
    pub process_role: String,
    pub current_oldest_tx_age_secs: f64,
    pub oldest_tx_label: Option<String>,
    pub attribution_is_evidence_backed: bool,
}

impl WalPinAttribution {
    fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            available: false,
            unavailable_reason: Some(reason.into()),
            census_holder_pids: Vec::new(),
            census_uninspectable_pids: Vec::new(),
            census_truncated: false,
            census_is_complete: false,
            reporting: Vec::new(),
            registered_silent_pids: Vec::new(),
            unknown_pids: Vec::new(),
            census_pids_without_attribution: Vec::new(),
            fully_attributed: false,
            sidecar_entries: Vec::new(),
            sidecar_listing_truncated: false,
            sidecar_entries_cleanup_would_reap: 0,
        }
    }
}

/// Build the WAL-pin attribution for `db_path`.
///
/// Unix-only: the OS census requires it. Everywhere else this degrades to
/// `available: false` with a reason rather than failing the whole report.
#[cfg(unix)]
pub fn wal_pin_attribution(db_path: &Path, _sweep_interval: Duration) -> WalPinAttribution {
    use crate::walpin;

    let census = match walpin::census_holders(db_path) {
        Ok(c) => c,
        Err(e) => return WalPinAttribution::unavailable(format!("census_holders failed: {e}")),
    };

    let mut census_holder_pids: Vec<u32> = census.holders.iter().copied().collect();
    census_holder_pids.sort_unstable();

    WalPinAttribution {
        available: false,
        unavailable_reason: Some(
            "sidecar-to-holder reconciliation not available: this tree's khive-db exposes \
             sidecar enumeration only via walpin::enumerate_live, which deletes stale/malformed \
             entries as part of its cleanup pass; a diagnostics probe must not delete forensic \
             sidecar evidence, so only the OS holder census below was collected"
                .to_string(),
        ),
        census_holder_pids,
        census_uninspectable_pids: census.uninspectable_pids.clone(),
        census_truncated: census.truncated,
        census_is_complete: census.is_complete(),
        reporting: Vec::new(),
        registered_silent_pids: Vec::new(),
        unknown_pids: Vec::new(),
        census_pids_without_attribution: Vec::new(),
        fully_attributed: false,
        sidecar_entries: Vec::new(),
        sidecar_listing_truncated: false,
        sidecar_entries_cleanup_would_reap: 0,
    }
}

#[cfg(not(unix))]
pub fn wal_pin_attribution(_db_path: &Path, _sweep_interval: Duration) -> WalPinAttribution {
    WalPinAttribution::unavailable("WAL-pin attribution requires a Unix platform")
}

/// The full diagnostics payload.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DbDiagnostics {
    pub build: BuildIdentity,
    /// `None` for an in-memory backend — the file-backed sections then carry
    /// their own unavailability reasons.
    pub db_path: Option<String>,
    pub wal_file: Option<WalFileState>,
    pub checkpoint_counters: CheckpointCounters,
    pub checkpoint_probe: Option<CheckpointProbe>,
    pub checkpoint_probe_error: Option<String>,
    pub wal_pin: WalPinAttribution,
}

/// Assemble the report for `pool`'s database.
///
/// The probe target is the pool's own configured path — one source of truth,
/// so the report can never describe a file the pool is not bound to. An
/// in-memory pool has no path: the counters are still real (they are
/// process-global), but every file-backed section degrades to an explicit
/// "unavailable" with a reason rather than being silently omitted.
///
/// The PASSIVE probe goes through `ConnectionPool::open_standalone_writer`,
/// opened WITHOUT `SQLITE_OPEN_CREATE`, so a diagnostic request against a
/// missing file returns `checkpoint_probe: null` with a
/// `checkpoint_probe_error` instead of creating a database. Running on a
/// standalone connection also keeps checkpoint I/O off the pooled writer
/// mutex.
pub fn collect(
    pool: &ConnectionPool,
    build: BuildIdentity,
    sweep_interval: Duration,
) -> DbDiagnostics {
    let counters = checkpoint_counters();

    let Some(path) = pool.config().path.clone() else {
        return DbDiagnostics {
            build,
            db_path: None,
            wal_file: None,
            checkpoint_counters: counters,
            checkpoint_probe: None,
            checkpoint_probe_error: Some(
                "in-memory database: no WAL file and no checkpoint to probe".to_string(),
            ),
            wal_pin: WalPinAttribution::unavailable(
                "in-memory database: no file for the OS holder census",
            ),
        };
    };

    let (probe, probe_error) = match probe_pool(pool) {
        Ok(p) => (Some(p), None),
        Err(e) => (None, Some(e)),
    };

    DbDiagnostics {
        build,
        db_path: Some(path.display().to_string()),
        wal_file: Some(wal_file_state(&path)),
        checkpoint_counters: counters,
        checkpoint_probe: probe,
        checkpoint_probe_error: probe_error,
        wal_pin: wal_pin_attribution(&path, sweep_interval),
    }
}

/// Run one PASSIVE probe on a guarded standalone connection.
///
/// Every failure — missing file, read-only pool, in-memory pool, or the
/// pragma itself — comes back as an error string the caller surfaces as
/// `checkpoint_probe_error`. Nothing here can create a file.
fn probe_pool(pool: &ConnectionPool) -> Result<CheckpointProbe, String> {
    let conn = pool
        .open_standalone_writer()
        .map_err(|e| format!("guarded standalone open refused: {e}"))?;
    checkpoint_probe(&conn).map_err(|e| format!("PRAGMA wal_checkpoint(PASSIVE) failed: {e}"))
}

#[cfg(test)]
mod tests {
    use serial_test::serial;

    use super::*;
    use crate::pool::{ConnectionPool, PoolConfig};

    fn seeded_pool(dir: &tempfile::TempDir) -> (ConnectionPool, PathBuf) {
        let path = dir.path().join("diag.db");
        let pool = ConnectionPool::new(PoolConfig {
            path: Some(path.clone()),
            ..PoolConfig::default()
        })
        .expect("pool open");
        {
            let writer = pool.try_writer().expect("writer");
            writer
                .conn()
                .execute_batch(
                    "CREATE TABLE t (x INTEGER); \
                     INSERT INTO t VALUES (1), (2), (3);",
                )
                .expect("seed writes");
        }
        (pool, path)
    }

    #[test]
    fn checkpoint_probe_returns_a_well_formed_triple_on_a_file_backed_db() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (pool, _path) = seeded_pool(&dir);
        let conn = pool.open_standalone_writer().expect("standalone");

        let probe = checkpoint_probe(&conn).expect("probe must succeed on a WAL database");

        assert!(
            probe.busy == 0 || probe.busy == 1,
            "busy is a 0/1 flag, got {}",
            probe.busy
        );
        assert!(
            probe.log_frames >= 0,
            "a WAL database must report a non-negative frame count, got {}",
            probe.log_frames
        );
        assert!(
            probe.checkpointed_frames >= 0,
            "checkpointed frames must be non-negative, got {}",
            probe.checkpointed_frames
        );
        assert!(
            probe.checkpointed_frames <= probe.log_frames,
            "a PASSIVE pass cannot checkpoint more frames than the WAL holds: {probe:?}"
        );
        assert!(probe.pin_depth() >= 0, "pin depth clamps at 0: {probe:?}");
    }

    /// The verb must not perturb the state it reports: the probe touches none
    /// of the six ADR-091 counters.
    #[test]
    #[serial(checkpoint_skip_metrics)]
    fn checkpoint_probe_does_not_perturb_the_adr091_counters() {
        crate::checkpoint::reset_checkpoint_metrics_for_tests();
        let dir = tempfile::tempdir().expect("tempdir");
        let (pool, _path) = seeded_pool(&dir);
        let conn = pool.open_standalone_writer().expect("standalone");

        let before = checkpoint_counters();
        for _ in 0..3 {
            checkpoint_probe(&conn).expect("probe must succeed");
        }
        let after = checkpoint_counters();

        assert_eq!(
            before, after,
            "checkpoint_probe must leave every ADR-091 counter untouched"
        );
    }

    #[test]
    fn wal_file_state_reports_the_sidecar_size_for_a_live_db() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (_pool, path) = seeded_pool(&dir);

        let state = wal_file_state(&path);
        assert!(
            state.wal_path.ends_with("diag.db-wal"),
            "WAL path is the db path plus a -wal suffix, got {}",
            state.wal_path
        );
        assert!(
            state.wal_size_bytes.is_some(),
            "a seeded WAL database must have a stat-able -wal file: {state:?}"
        );
        assert!(state.unavailable_reason.is_none(), "{state:?}");
    }

    #[test]
    fn wal_file_state_degrades_with_a_reason_when_the_sidecar_is_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = wal_file_state(&dir.path().join("never-created.db"));
        assert!(state.wal_size_bytes.is_none());
        assert!(
            state.unavailable_reason.is_some(),
            "an absent WAL file must carry a reason, not a silent zero: {state:?}"
        );
    }

    #[test]
    fn collect_on_a_file_backed_db_carries_build_identity_and_every_counter() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (pool, _path) = seeded_pool(&dir);

        let report = collect(
            &pool,
            BuildIdentity::from_env("9.9.9", Some("deadbeef")),
            Duration::from_secs(30),
        );

        assert_eq!(report.build.version, "9.9.9");
        assert_eq!(report.build.build_hash.as_deref(), Some("deadbeef"));
        assert!(report.db_path.is_some());
        assert!(
            report.checkpoint_probe.is_some(),
            "file-backed collect must land a probe; error was {:?}",
            report.checkpoint_probe_error
        );
        assert!(
            report.wal_file.as_ref().and_then(|w| w.wal_size_bytes) >= Some(0),
            "wal_size_bytes must be a non-negative byte count when present"
        );

        let json = serde_json::to_value(&report).expect("report serializes");
        let counters = json
            .get("checkpoint_counters")
            .expect("counters section present");
        for key in [
            "last_observed_wal_pages",
            "truncate_attempts",
            "truncate_consecutive_failures",
            "checkpoint_skipped_ticks",
            "checkpoint_consecutive_skips",
            "checkpoint_last_skip_wal_pages",
        ] {
            assert!(counters.get(key).is_some(), "counter {key} must be present");
        }
    }

    /// The `u64::MAX` never-observed sentinel must serialize as `null`, never
    /// as a huge number an operator would read as a real page count.
    #[test]
    fn never_observed_sentinels_serialize_as_null() {
        let counters = CheckpointCounters {
            last_observed_wal_pages: None,
            truncate_attempts: 0,
            truncate_consecutive_failures: 0,
            checkpoint_skipped_ticks: 0,
            checkpoint_consecutive_skips: 0,
            checkpoint_last_skip_wal_pages: None,
        };
        let json = serde_json::to_value(counters).expect("serializes");
        assert!(json["last_observed_wal_pages"].is_null());
        assert!(json["checkpoint_last_skip_wal_pages"].is_null());
    }

    /// A missing configured path must never be created by a diagnostic
    /// request. `open_standalone_writer` opens without `SQLITE_OPEN_CREATE`,
    /// so the probe degrades to an error and the file stays absent.
    #[test]
    fn probe_refuses_a_missing_configured_path_without_creating_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (pool, path) = seeded_pool(&dir);

        for suffix in ["", "-wal", "-shm"] {
            let mut p = path.as_os_str().to_os_string();
            p.push(suffix);
            let _ = std::fs::remove_file(PathBuf::from(p));
        }
        assert!(!path.exists(), "precondition: the database file is gone");

        let report = collect(
            &pool,
            BuildIdentity::from_env("0.0.0", None),
            Duration::from_secs(30),
        );

        assert!(
            report.checkpoint_probe.is_none(),
            "a missing database must not yield a probe result: {report:?}"
        );
        assert!(
            report.checkpoint_probe_error.is_some(),
            "a missing database must say why there is no probe: {report:?}"
        );
        assert!(
            !path.exists(),
            "a diagnostics request must never create the database it was asked about"
        );
    }

    /// In-memory backends have no WAL file and no census target: the report
    /// still returns, with explicit reasons rather than missing sections.
    #[test]
    fn collect_degrades_gracefully_for_an_in_memory_backend() {
        let pool = ConnectionPool::new(PoolConfig::default()).expect("in-memory pool");
        let report = collect(
            &pool,
            BuildIdentity::from_env("0.0.0", None),
            Duration::from_secs(30),
        );

        assert!(report.db_path.is_none());
        assert!(report.wal_file.is_none());
        assert!(report.checkpoint_probe.is_none());
        assert!(
            report.checkpoint_probe_error.is_some(),
            "an in-memory report must say WHY there is no probe"
        );
        assert!(!report.wal_pin.available);
        assert!(report.wal_pin.unavailable_reason.is_some());
    }

    /// This port's WAL-pin attribution never claims full reconciliation —
    /// see the module docs on why sidecar enumeration is not wired here.
    #[cfg(unix)]
    #[test]
    fn wal_pin_attribution_reports_census_but_never_claims_full_attribution() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (pool, path) = seeded_pool(&dir);
        let _ = &pool;

        let pin = wal_pin_attribution(&path, Duration::from_secs(30));

        assert!(
            !pin.fully_attributed,
            "sidecar reconciliation is not ported: this must never claim completeness"
        );
        assert!(
            pin.unavailable_reason.is_some(),
            "the gap must be explained, not silent: {pin:?}"
        );
        assert!(pin.sidecar_entries.is_empty());
        assert!(pin.reporting.is_empty());
    }
}
