//! Non-mutating-by-intent database, WAL, and checkpoint diagnostics (read-only
//! operator surface).
//!
//! Answers "is a reader pinning the checkpoint / why is the WAL at 64MiB"
//! without raw SQL against a production store.
//!
//! NOT read-only. [`checkpoint_probe`](crate::diagnostics::checkpoint_probe) issues a real
//! `PRAGMA wal_checkpoint(PASSIVE)`, and a PASSIVE checkpoint that succeeds
//! BACKFILLS WAL frames into the main database — ordinary database-page
//! writes, on the happy path. That I/O is the point: the busy/log_frames/
//! checkpointed_frames triple reports a one-row backfill gap; a sustained
//! pinned-frame report additionally requires a matching checkpoint run.
//! SQLite exposes no read-only API for those counters. The
//! guarantee is that nothing here changes logical state or destroys
//! evidence, not that nothing touches the disk.
//!
//! What IS guaranteed, and what the narrowings below buy:
//!
//! * never creates a missing database file,
//! * never escalates to TRUNCATE,
//! * never perturbs the counters it reports,
//! * never increments write-traffic acquisition counters,
//! * never deletes a walpin sidecar entry.
//!
//! Deliberate narrowings make those claims true rather than aspirational:
//!
//! 1. [`checkpoint_probe`](crate::diagnostics::checkpoint_probe) runs a
//!    single `PRAGMA wal_checkpoint(PASSIVE)` —
//!    which never blocks readers or writers — and does NOT route through
//!    [`crate::checkpoint::checkpoint_once`]: that path mutates
//!    `TruncateState`, may escalate to TRUNCATE, and double-counts the
//!    ADR-091 process-global counters. A verb that reports state must not
//!    perturb the state it reports.
//! 2. The pool's complete diagnostic collector opens its private connection
//!    without `SQLITE_OPEN_CREATE` and returns data rather than a SQL handle.
//!    A missing database yields `checkpoint_probe:
//!    null` plus a `checkpoint_probe_error`, never a freshly created file.
//! 3. WAL-pin attribution combines the read-only OS holder census with
//!    `walpin::inspect_live`, a separate bounded sidecar enumerator whose
//!    purpose flag prohibits every unlink. It applies the same descriptor-
//!    bound trust and liveness checks as checkpoint attribution, but reports
//!    stale cleanup candidates rather than consuming them. A complete census
//!    plus a complete, conclusive sidecar walk can therefore report complete;
//!    truncation, unknown entries, or holders absent from the sidecar degrade
//!    explicitly.
//! 4. Graph-edge integrity uses three scalar SELECTs on the same guarded
//!    standalone connection. It exposes the exact pre-V14 duplicate-ID group
//!    count plus raw live-edge/list-ledger counts and never repairs or deletes
//!    data.
//! 5. FTS5 segment diagnostics decode each index's documented one-row
//!    structure record (`%_data.id = 10`). They never run `COUNT(*)` over or
//!    scan a `%_idx` table, so observing segment health is bounded even when
//!    the corpus itself is large.
//!
//! Checkpoint counters are process-global, while reader and writer acquisition
//! counters belong to the supplied pool and can reset when it is reconstructed.
//! Every payload carries [`BuildIdentity`](crate::diagnostics::BuildIdentity) and
//! [`ProcessIdentity`](crate::diagnostics::ProcessIdentity) for the
//! process producing the reading. The PID, OS start time, and main-pool generation
//! identify the main pool's counter window; a secondary pool's reader/writer
//! counters have their own reconstruction window. Checkpoint counters remain global.

#[path = "diagnostics/disk_guard.rs"]
mod disk_guard;
#[path = "diagnostics/size_composition.rs"]
mod size_composition;
#[path = "diagnostics/writer_admission.rs"]
mod writer_admission;
pub use writer_admission::WriterAdmissionDiagnostics;
#[path = "diagnostics/writer_contention.rs"]
mod writer_contention;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
#[cfg(unix)]
use std::time::{SystemTime, UNIX_EPOCH};

use khive_storage::error::StorageError;
use khive_storage::types::StorageResult;
use khive_storage::StorageCapability;
use rusqlite::Connection;
use serde::Serialize;

use crate::checkpoint;
use crate::pool::{ConnectionPool, WalCeilingSource};

pub use disk_guard::DiskGuardDiagnostics;

/// Raw `PRAGMA wal_checkpoint(PASSIVE)` return row.
///
/// SQLite returns three columns: `busy` (1 when the checkpoint could not
/// obtain the checkpoint lock or read the WAL-index header), `log` (frames
/// currently in the WAL), and `checkpointed` (frames moved into the database
/// by THIS call). The one-row backfill gap is `log - checkpointed`; that
/// difference alone does not establish a reader pin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CheckpointProbe {
    pub busy: i64,
    pub log_frames: i64,
    pub checkpointed_frames: i64,
}

impl CheckpointProbe {
    /// Frames still awaiting backfill in this row. Negative frame counts
    /// (an in-memory or non-WAL database reports `-1`) return 0. Even when
    /// busy, populated frame counts describe only a gap, never a reader pin.
    pub fn backfill_gap_frames(&self) -> i64 {
        if self.log_frames < 0 || self.checkpointed_frames < 0 {
            return 0;
        }
        self.log_frames
            .saturating_sub(self.checkpointed_frames)
            .max(0)
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

/// ADR-091 checkpoint counters, read as one snapshot.
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
    pub checkpoint_pressure_elevated_ticks: u64,
    pub checkpoint_pressure_episodes_started: u64,
    pub checkpoint_pressure_episodes_recovered: u64,
    pub checkpoint_lifecycle_append_attempts: u64,
    pub checkpoint_lifecycle_append_failures: u64,
    pub checkpoint_lifecycle_enqueue_drops: u64,
    /// Cached-reader read transactions rolled back on reuse for exceeding
    /// `read_tx_max_age` (#1846) — the count of WAL snapshots actually
    /// released by that bound, not merely logged as stale.
    pub read_tx_max_age_evictions: u64,
}

/// Snapshot the process-global ADR-091 counters.
pub fn checkpoint_counters() -> CheckpointCounters {
    CheckpointCounters {
        last_observed_wal_pages: checkpoint::last_observed_wal_pages(),
        truncate_attempts: checkpoint::truncate_attempts(),
        truncate_consecutive_failures: checkpoint::truncate_consecutive_failures(),
        checkpoint_skipped_ticks: checkpoint::checkpoint_skipped_ticks(),
        checkpoint_consecutive_skips: checkpoint::checkpoint_consecutive_skips(),
        checkpoint_last_skip_wal_pages: checkpoint::checkpoint_last_skip_wal_pages(),
        checkpoint_pressure_elevated_ticks: checkpoint::checkpoint_pressure_elevated_ticks(),
        checkpoint_pressure_episodes_started: checkpoint::checkpoint_pressure_episodes_started(),
        checkpoint_pressure_episodes_recovered: checkpoint::checkpoint_pressure_episodes_recovered(
        ),
        checkpoint_lifecycle_append_attempts: checkpoint::checkpoint_lifecycle_append_attempts(),
        checkpoint_lifecycle_append_failures: checkpoint::checkpoint_lifecycle_append_failures(),
        checkpoint_lifecycle_enqueue_drops: checkpoint::checkpoint_lifecycle_enqueue_drops(),
        read_tx_max_age_evictions: checkpoint::read_tx_max_age_evictions(),
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

/// The serving OS process and the main pool's reader/writer counter generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProcessIdentity {
    pub pid: u32,
    /// OS-reported process creation time in whole Unix epoch seconds (UTC).
    /// This is neither pool creation time nor the time of the first request.
    pub started_at: Option<i64>,
    /// Present when the OS lookup is unsupported or unavailable. An unknown
    /// start time is never replaced by zero or the current wall clock.
    pub started_at_unavailable_reason: Option<String>,
    pub pool_generation: u64,
}

impl ProcessIdentity {
    pub fn current(pool: &ConnectionPool) -> Self {
        let pid = std::process::id();
        Self::from_start_time(
            pid,
            crate::walpin::process_start_time_secs(pid),
            pool.main_pool_generation(),
        )
    }

    fn from_start_time(pid: u32, started_at: Option<i64>, pool_generation: u64) -> Self {
        Self {
            pid,
            started_at,
            started_at_unavailable_reason: started_at.is_none().then(|| {
                format!(
                    "OS process start time is unsupported or unavailable on {}",
                    std::env::consts::OS
                )
            }),
            pool_generation,
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
/// `status`, `status_reasons`, and the tagged `census` field are the
/// authoritative wire contract. The older sibling booleans and PID arrays
/// remain available to Rust callers but are not serialized. Sidecar evidence
/// is collected through the handle-checked, bounded, non-mutating diagnostics
/// enumerator and reconciled with the OS holder census.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WalPinAttribution {
    /// Authoritative quality of the complete attribution answer.
    pub status: WalPinAttributionStatus,
    /// Machine-adjacent reasons why the answer is degraded or unavailable.
    pub status_reasons: Vec<String>,
    /// Authoritative tagged result of the OS holder census.
    pub census: WalPinCensus,
    /// PID spelling used by the process census for the reporting process.
    pub reporting_pid: u32,
    /// Whether the reporting process appears among confirmed file holders.
    pub reporting_process_is_holder: Option<bool>,
    /// Why the census could not determine whether the reporter is a holder.
    pub reporting_process_is_holder_unavailable_reason: Option<String>,
    /// Raw start-time values for every confirmed holder PID.
    pub census_process_start_times: Vec<WalPinCensusProcessStart>,
    /// Resolution of the operating system's process start-time value.
    pub start_time_resolution_secs: Option<u64>,
    /// Why the platform does not provide process start-time values.
    pub start_time_resolution_unavailable_reason: Option<String>,
    /// `false` plus an `unavailable_reason` whenever the OS census failed,
    /// or when only a partial (census-only) answer is available.
    pub available: bool,
    pub unavailable_reason: Option<String>,
    /// OS-derived census of every PID holding the DB file open.
    #[serde(skip_serializing)]
    pub census_holder_pids: Vec<u32>,
    #[serde(skip_serializing)]
    pub census_uninspectable_pids: Vec<u32>,
    #[serde(skip_serializing)]
    pub census_truncated: bool,
    #[serde(skip_serializing)]
    pub census_is_complete: bool,
    /// Live, identity-matched heartbeats found in the sidecar.
    pub reporting: Vec<WalPinHolder>,
    /// Live, identity-matched beacons with no over-threshold heartbeat.
    pub registered_silent_pids: Vec<u32>,
    /// Sidecar entries whose identity or freshness could not be established.
    pub unknown_pids: Vec<u32>,
    /// OS-confirmed holders absent from every sidecar classification.
    pub census_pids_without_attribution: Vec<u32>,
    /// Whether both evidence sources completed and every holder was present
    /// in a conclusive sidecar classification.
    pub fully_attributed: bool,
    /// Machine-readable sidecar classifications, ordered by PID and status.
    pub sidecar_entries: Vec<serde_json::Value>,
    /// Present only when this request actually enumerated the sidecar.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sidecar_listing_truncated: Option<bool>,
    /// Number of stale producer temps a housekeeping pass would reap. The
    /// diagnostic pass itself never performs that cleanup.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sidecar_entries_cleanup_would_reap: Option<usize>,
}

/// OS-reported start time for one holder confirmed by the process census.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WalPinCensusProcessStart {
    /// PID confirmed by the OS holder census.
    pub pid: u32,
    /// Process start time as epoch seconds, if the OS reported it.
    pub process_start_time_secs: Option<i64>,
    /// Why the process start time is unavailable.
    pub process_start_time_unavailable_reason: Option<String>,
}

#[cfg(unix)]
fn census_process_start_times(holder_pids: &[u32]) -> Vec<WalPinCensusProcessStart> {
    holder_pids
        .iter()
        .map(|pid| {
            let process_start_time_secs = crate::walpin::process_start_time_secs(*pid);
            let process_start_time_unavailable_reason =
                process_start_time_secs.is_none().then(|| {
                    if crate::walpin::start_time_resolution_secs().is_none() {
                        "process start time is unavailable on this platform".to_string()
                    } else {
                        "the operating system did not report a start time for this process"
                            .to_string()
                    }
                });
            WalPinCensusProcessStart {
                pid: *pid,
                process_start_time_secs,
                process_start_time_unavailable_reason,
            }
        })
        .collect()
}

/// Overall quality of the WAL-pin attribution answer.
///
/// A result is `complete` only when both the OS census and read-only sidecar
/// enumeration completed and every holder has sidecar evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WalPinAttributionStatus {
    /// Holder census and sidecar reconciliation both completed.
    Complete,
    /// Some useful evidence is present, but full attribution is impossible.
    Degraded,
    /// No holder-census evidence could be collected.
    Unavailable,
}

/// Tagged OS holder-census result.
///
/// An incomplete scan retains the partial holder evidence, but its wire shape
/// cannot be mistaken for a complete census without ignoring the explicit
/// `status` tag. This is the fail-loud direction required by ADR-091: a
/// truncated walk or an uninspectable PID is inconclusive, never evidence
/// that no additional holder exists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum WalPinCensus {
    /// Every visible process was inspected and the walk was not truncated.
    Complete {
        /// PIDs confirmed to hold the database file open.
        holder_pids: Vec<u32>,
    },
    /// Partial evidence from an inconclusive process walk.
    Incomplete {
        /// PIDs confirmed to hold the database file open.
        holder_pids: Vec<u32>,
        /// PIDs for which inspection failed outright.
        uninspectable_pids: Vec<u32>,
        /// Whether process enumeration had positive evidence of truncation.
        truncated: bool,
        /// Why additional holders cannot be ruled out.
        reason: String,
    },
    /// The platform or census operation supplied no holder evidence.
    Unavailable {
        /// Why the census could not run.
        reason: String,
    },
}

/// Name the reason a holder census stopped short.
///
/// A budget stop and an enumeration failure both set `truncated`, and an
/// operator reading one sentence has to be able to tell them apart: the first
/// is a configurable trade this process made on purpose and is expected on a
/// busy box, the second is evidence that process enumeration itself
/// misbehaved. Describing both as "truncated" would make the report cry wolf
/// on every bounded call, which is how the sentence that matters stops being
/// read.
#[cfg(any(unix, test))]
fn census_truncation_cause(budget_exhausted: bool) -> String {
    if budget_exhausted {
        "the OS process walk stopped at its wall-clock budget (see          collection_cost.wal_pin_census_budget_ms)"
            .to_string()
    } else {
        "the OS process walk was truncated".to_string()
    }
}

/// One PID's live heartbeat as reported to an operator.
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
        let reason = reason.into();
        let start_time_resolution_secs = crate::walpin::start_time_resolution_secs();
        Self {
            status: WalPinAttributionStatus::Unavailable,
            status_reasons: vec![reason.clone()],
            census: WalPinCensus::Unavailable {
                reason: reason.clone(),
            },
            reporting_pid: crate::walpin::reporting_pid(),
            reporting_process_is_holder: None,
            reporting_process_is_holder_unavailable_reason: Some(
                "the OS holder census supplied no holder evidence".to_string(),
            ),
            census_process_start_times: Vec::new(),
            start_time_resolution_secs,
            start_time_resolution_unavailable_reason: start_time_resolution_secs
                .is_none()
                .then(|| "process start time is unavailable on this platform".to_string()),
            available: false,
            unavailable_reason: Some(reason),
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
            sidecar_listing_truncated: None,
            sidecar_entries_cleanup_would_reap: None,
        }
    }
}

#[cfg(all(unix, test))]
fn wal_pin_attribution_from_census(census: crate::walpin::CensusResult) -> WalPinAttribution {
    wal_pin_attribution_without_sidecar(
        census,
        "read-only sidecar enumeration did not run for this attribution snapshot".to_string(),
    )
}

#[cfg(unix)]
fn wal_pin_attribution_without_sidecar(
    census: crate::walpin::CensusResult,
    sidecar_reason: String,
) -> WalPinAttribution {
    let census_is_complete = census.is_complete();
    let mut census_holder_pids: Vec<u32> = census.holders.iter().copied().collect();
    census_holder_pids.sort_unstable();
    let reporting_pid = crate::walpin::reporting_pid();
    let reporting_process_is_holder = census_holder_pids
        .contains(&reporting_pid)
        .then_some(true)
        .or_else(|| census_is_complete.then_some(false));
    let reporting_process_is_holder_unavailable_reason =
        reporting_process_is_holder.is_none().then(|| {
            "the reporting process was absent from the incomplete OS holder census".to_string()
        });
    let census_process_start_times = census_process_start_times(&census_holder_pids);
    let start_time_resolution_secs = crate::walpin::start_time_resolution_secs();
    let mut census_uninspectable_pids = census.uninspectable_pids;
    census_uninspectable_pids.sort_unstable();
    census_uninspectable_pids.dedup();
    let census_truncated = census.truncated;
    let census_budget_exhausted = census.budget_exhausted;

    let mut status_reasons = vec![sidecar_reason];
    let census = if census_is_complete {
        WalPinCensus::Complete {
            holder_pids: census_holder_pids.clone(),
        }
    } else {
        let mut causes = Vec::new();
        if census_truncated {
            causes.push(census_truncation_cause(census_budget_exhausted));
        }
        if !census_uninspectable_pids.is_empty() {
            causes.push(format!(
                "{} PID(s) could not be inspected",
                census_uninspectable_pids.len()
            ));
        }
        let reason = format!(
            "OS holder census is incomplete: {}; additional database holders cannot be ruled out",
            causes.join("; ")
        );
        status_reasons.push(reason.clone());
        WalPinCensus::Incomplete {
            holder_pids: census_holder_pids.clone(),
            uninspectable_pids: census_uninspectable_pids.clone(),
            truncated: census_truncated,
            reason,
        }
    };

    WalPinAttribution {
        status: WalPinAttributionStatus::Degraded,
        unavailable_reason: Some(status_reasons.join("; ")),
        status_reasons,
        census,
        reporting_pid,
        reporting_process_is_holder,
        reporting_process_is_holder_unavailable_reason,
        census_process_start_times,
        start_time_resolution_secs,
        start_time_resolution_unavailable_reason: start_time_resolution_secs
            .is_none()
            .then(|| "process start time is unavailable on this platform".to_string()),
        available: false,
        census_holder_pids,
        census_uninspectable_pids,
        census_truncated,
        census_is_complete,
        reporting: Vec::new(),
        registered_silent_pids: Vec::new(),
        unknown_pids: Vec::new(),
        census_pids_without_attribution: Vec::new(),
        fully_attributed: false,
        sidecar_entries: Vec::new(),
        sidecar_listing_truncated: None,
        sidecar_entries_cleanup_would_reap: None,
    }
}

#[cfg(unix)]
fn wal_pin_attribution_from_evidence(
    census: crate::walpin::CensusResult,
    sidecar: crate::walpin::WalpinReport,
) -> WalPinAttribution {
    use std::collections::BTreeSet;

    let census_is_complete = census.is_complete();
    let mut census_holder_pids: Vec<u32> = census.holders.iter().copied().collect();
    census_holder_pids.sort_unstable();
    let reporting_pid = crate::walpin::reporting_pid();
    let reporting_process_is_holder = census_holder_pids
        .contains(&reporting_pid)
        .then_some(true)
        .or_else(|| census_is_complete.then_some(false));
    let reporting_process_is_holder_unavailable_reason =
        reporting_process_is_holder.is_none().then(|| {
            "the reporting process was absent from the incomplete OS holder census".to_string()
        });
    let census_process_start_times = census_process_start_times(&census_holder_pids);
    let start_time_resolution_secs = crate::walpin::start_time_resolution_secs();
    let mut census_uninspectable_pids = census.uninspectable_pids;
    census_uninspectable_pids.sort_unstable();
    census_uninspectable_pids.dedup();
    let census_truncated = census.truncated;
    let census_budget_exhausted = census.budget_exhausted;
    let census_carrier = if census_is_complete {
        WalPinCensus::Complete {
            holder_pids: census_holder_pids.clone(),
        }
    } else {
        let mut causes = Vec::new();
        if census_truncated {
            causes.push(census_truncation_cause(census_budget_exhausted));
        }
        if !census_uninspectable_pids.is_empty() {
            causes.push(format!(
                "{} PID(s) could not be inspected",
                census_uninspectable_pids.len()
            ));
        }
        let reason = format!(
            "OS holder census is incomplete: {}; additional database holders cannot be ruled out",
            causes.join("; ")
        );
        WalPinCensus::Incomplete {
            holder_pids: census_holder_pids.clone(),
            uninspectable_pids: census_uninspectable_pids.clone(),
            truncated: census_truncated,
            reason,
        }
    };

    let now_epoch_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0);
    let sidecar_listing_truncated = sidecar.sidecar_listing_truncated;
    let sidecar_entries_cleanup_would_reap = sidecar.cleanup_would_reap;
    let mut reporting = Vec::new();
    let mut registered_silent_pids = Vec::new();
    let mut unknown_pids = Vec::new();
    let mut sidecar_entries = Vec::new();
    let mut sidecar_known_pids = BTreeSet::new();

    for entry in sidecar.entries {
        match entry {
            crate::walpin::WalpinPidHealth::Reporting(heartbeat) => {
                let current_oldest_tx_age_secs =
                    heartbeat.current_oldest_tx_age_secs(now_epoch_secs);
                let attribution_is_evidence_backed = heartbeat.attribution_is_evidence_backed();
                sidecar_known_pids.insert(heartbeat.pid);
                reporting.push(WalPinHolder {
                    pid: heartbeat.pid,
                    process_role: heartbeat.process_role.clone(),
                    current_oldest_tx_age_secs,
                    oldest_tx_label: heartbeat.oldest_tx_label.clone(),
                    attribution_is_evidence_backed,
                });
                sidecar_entries.push((
                    heartbeat.pid,
                    0u8,
                    serde_json::json!({
                        "pid": heartbeat.pid,
                        "status": "reporting",
                        "process_role": heartbeat.process_role,
                        "current_oldest_tx_age_secs": current_oldest_tx_age_secs,
                        "oldest_tx_label": heartbeat.oldest_tx_label,
                        "attribution_is_evidence_backed": attribution_is_evidence_backed,
                    }),
                ));
            }
            crate::walpin::WalpinPidHealth::RegisteredSilent { pid } => {
                sidecar_known_pids.insert(pid);
                registered_silent_pids.push(pid);
                sidecar_entries.push((
                    pid,
                    1u8,
                    serde_json::json!({"pid": pid, "status": "registered_silent"}),
                ));
            }
            crate::walpin::WalpinPidHealth::Unknown { pid, reason } => {
                sidecar_known_pids.insert(pid);
                unknown_pids.push(pid);
                sidecar_entries.push((
                    pid,
                    2u8,
                    serde_json::json!({"pid": pid, "status": "unknown", "reason": reason}),
                ));
            }
        }
    }

    reporting.sort_by_key(|holder| holder.pid);
    reporting.dedup_by_key(|holder| holder.pid);
    registered_silent_pids.sort_unstable();
    registered_silent_pids.dedup();
    unknown_pids.sort_unstable();
    unknown_pids.dedup();
    sidecar_entries.sort_by_key(|(pid, status_rank, _)| (*pid, *status_rank));
    let sidecar_entries = sidecar_entries
        .into_iter()
        .map(|(_, _, entry)| entry)
        .collect();
    let census_pids_without_attribution: Vec<u32> = census_holder_pids
        .iter()
        .copied()
        .filter(|pid| !sidecar_known_pids.contains(pid))
        .collect();

    let mut status_reasons = Vec::new();
    if let WalPinCensus::Incomplete { reason, .. } = &census_carrier {
        status_reasons.push(reason.clone());
    }
    if sidecar_listing_truncated {
        status_reasons.push(
            "read-only sidecar enumeration reached its entry cap; additional entries may exist"
                .to_string(),
        );
    }
    if !unknown_pids.is_empty() {
        status_reasons.push(format!(
            "{} sidecar PID(s) could not be classified conclusively",
            unknown_pids.len()
        ));
    }
    if !census_pids_without_attribution.is_empty() {
        status_reasons.push(format!(
            "{} OS-confirmed holder(s) have no sidecar attribution",
            census_pids_without_attribution.len()
        ));
    }

    let fully_attributed = census_is_complete
        && !sidecar_listing_truncated
        && unknown_pids.is_empty()
        && census_pids_without_attribution.is_empty();
    let status = if fully_attributed {
        WalPinAttributionStatus::Complete
    } else {
        WalPinAttributionStatus::Degraded
    };
    let unavailable_reason = (!fully_attributed).then(|| status_reasons.join("; "));

    WalPinAttribution {
        status,
        status_reasons,
        census: census_carrier,
        reporting_pid,
        reporting_process_is_holder,
        reporting_process_is_holder_unavailable_reason,
        census_process_start_times,
        start_time_resolution_secs,
        start_time_resolution_unavailable_reason: start_time_resolution_secs
            .is_none()
            .then(|| "process start time is unavailable on this platform".to_string()),
        available: fully_attributed,
        unavailable_reason,
        census_holder_pids,
        census_uninspectable_pids,
        census_truncated,
        census_is_complete,
        reporting,
        registered_silent_pids,
        unknown_pids,
        census_pids_without_attribution,
        fully_attributed,
        sidecar_entries,
        sidecar_listing_truncated: Some(sidecar_listing_truncated),
        sidecar_entries_cleanup_would_reap: Some(sidecar_entries_cleanup_would_reap),
    }
}

/// Build the WAL-pin attribution for `db_path`.
///
/// Unix-only: the OS census requires it. Everywhere else this degrades to
/// `available: false` with a reason rather than failing the whole report.
/// An operator who has explicitly disabled the sidecar (`KHIVE_WALPIN_SIDECAR`)
/// also disables this request's sidecar collection, per ADR-091 Amendment 6 —
/// the census still runs, but there is no sidecar evidence to reconcile it
/// against.
#[cfg(unix)]
pub fn wal_pin_attribution(db_path: &Path, sweep_interval: Duration) -> WalPinAttribution {
    use crate::walpin;

    let census = match walpin::census_holders(db_path) {
        Ok(c) => c,
        Err(e) => return WalPinAttribution::unavailable(format!("census_holders failed: {e}")),
    };
    if !walpin::sidecar_enabled(true) {
        return wal_pin_attribution_without_sidecar(census, SIDECAR_DISABLED_REASON.to_string());
    }
    match walpin::inspect_live(&walpin::sidecar_dir_for(db_path), sweep_interval) {
        Ok(sidecar) => wal_pin_attribution_from_evidence(census, sidecar),
        Err(error) => wal_pin_attribution_without_sidecar(
            census,
            format!("read-only sidecar enumeration failed: {error}"),
        ),
    }
}

/// Shared with the async collection path in `inspect_file_state_interruptibly`.
#[cfg(unix)]
const SIDECAR_DISABLED_REASON: &str =
    "walpin sidecar is explicitly disabled (KHIVE_WALPIN_SIDECAR); attribution has no sidecar \
     evidence to reconcile against the OS holder census";

#[cfg(not(unix))]
pub fn wal_pin_attribution(_db_path: &Path, _sweep_interval: Duration) -> WalPinAttribution {
    WalPinAttribution::unavailable("WAL-pin attribution requires a Unix platform")
}

/// One typed snapshot of reader route, saturation, and hold-lifecycle signals.
///
/// Every field is pool-scoped. Monotonic counters reset only when the
/// [`ConnectionPool`] is reconstructed; the active value is point-in-time.
/// Infrastructure standalone opens are kept separate from request traffic so
/// a boot/schema probe cannot make a hot path look like it churned readers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ReaderContentionDiagnostics {
    /// Reader connections requested in this pool's configuration. The
    /// effective admission capacity below can be smaller in degraded mode.
    pub configured_reader_cap: usize,
    /// Configured wait for a reader slot, in milliseconds.
    pub configured_checkout_timeout_ms: u64,
    /// Configured SQLite busy-handler wait, in milliseconds. This is separate
    /// from the reader-slot checkout deadline.
    pub configured_busy_timeout_ms: u64,
    /// Configured total reader admission budget shared by pooled readers and
    /// explicit raw-SQL read transactions.
    pub reader_admission_capacity: usize,
    /// Point-in-time permits not held when this snapshot was captured.
    pub available_reader_admission_slots: usize,
    /// Successful request-path acquisitions (pooled plus enumerated
    /// standalone request exceptions; infrastructure excluded).
    pub reader_acquisitions: u64,
    /// Successful bounded pooled-reader checkouts.
    pub pooled_reader_checkouts: u64,
    /// Successful request-path standalone reader opens. Under ADR-165 Slice
    /// 2 this is limited to the explicit raw-SQL read-transaction exception.
    pub standalone_reader_opens: u64,
    /// Successful standalone opens owned by an enumerated boot/diagnostic
    /// infrastructure exception.
    pub infrastructure_standalone_reader_opens: u64,
    /// Pool-wide reader-admission waits that exhausted `checkout_timeout`
    /// before work began. Cooperative request cancellation is excluded.
    pub reader_checkout_timeouts: u64,
    /// Queries on a checked-out pooled reader that SQLite refused with
    /// `SQLITE_BUSY` after `configured_busy_timeout_ms` elapsed. Counted after
    /// checkout succeeded, so it is disjoint from `reader_checkout_timeouts`.
    /// Writer refusals are not included; see `direct_writer_busy_refusals` and
    /// `writer_task_begin_busy`.
    pub reader_busy_timeouts: u64,
    /// Pooled reader guards live when the snapshot was captured.
    pub active_pooled_reader_checkouts: u64,
    /// Highest observed concurrent pooled-reader guard count.
    pub peak_active_pooled_reader_checkouts: u64,
    /// Pooled guards that completed return/reset.
    pub completed_pooled_reader_checkouts: u64,
    /// Longest completed hold, including return/reset, in microseconds.
    pub max_completed_reader_hold_micros: u64,
    /// The typed-store operation that held the checkout this maximum came
    /// from. `None` means that hold came through a route carrying no
    /// operation name — the pool's own internal checkout — which is the
    /// answer to "which read was it", not a gap in the reading (#2793).
    pub max_completed_reader_hold_operation: Option<&'static str>,
    /// Dedicated readers discarded on return for age, checkout count, failed
    /// cleanup/health checks, or an explicitly non-reusable lease. Counts
    /// replacement attempts, including failed opens; excludes pool shutdown.
    pub reader_discards: u64,
    /// A disqualified pooled-reader return whose replacement connection then
    /// also failed to open, permanently shrinking the physical pool by one
    /// slot below `max_readers`. Non-zero here means the pool has fewer
    /// physical reader connections than configured.
    pub reader_replacement_open_failures: u64,
}

impl ReaderContentionDiagnostics {
    fn snapshot(pool: &ConnectionPool) -> Self {
        let reader = pool.reader_acquisition_snapshot();
        Self {
            configured_reader_cap: pool.config().max_readers,
            configured_checkout_timeout_ms: u64::try_from(
                pool.config().checkout_timeout.as_millis(),
            )
            .unwrap_or(u64::MAX),
            configured_busy_timeout_ms: u64::try_from(pool.config().busy_timeout.as_millis())
                .unwrap_or(u64::MAX),
            reader_admission_capacity: reader.reader_admission_capacity,
            available_reader_admission_slots: reader.available_reader_admission_slots,
            reader_acquisitions: reader.acquisitions,
            pooled_reader_checkouts: reader.pooled_checkouts,
            standalone_reader_opens: reader.standalone_opens,
            infrastructure_standalone_reader_opens: reader.infrastructure_standalone_opens,
            reader_checkout_timeouts: reader.checkout_timeouts,
            reader_busy_timeouts: reader.busy_timeouts,
            active_pooled_reader_checkouts: reader.active_pooled_checkouts,
            peak_active_pooled_reader_checkouts: reader.peak_active_pooled_checkouts,
            completed_pooled_reader_checkouts: reader.completed_pooled_checkouts,
            max_completed_reader_hold_micros: reader.max_completed_hold_micros,
            max_completed_reader_hold_operation: reader.max_completed_hold_operation,
            reader_discards: reader.reader_discards,
            reader_replacement_open_failures: reader.reader_replacement_open_failures,
        }
    }
}

/// One typed snapshot of writer-contention signals.
///
/// `writer_acquisitions` is the aggregate of the three explicit connection
/// classes below. `writer_acquisition_timeouts` remains specific to the
/// finite-wait pool-mutex stage; standalone SQLite failures and writer-task
/// `BEGIN` failures have different ADR-135 F6 stages and are not mislabeled as
/// pool checkout timeouts. Those stages now carry their OWN failure counters
/// (`writer_task_begin_busy`, `writer_task_begin_busy_absorbed`,
/// `writer_task_begin_errors`) rather than being absent: refusing to mislabel
/// a failure is not a reason to omit it, and an omitted failure counter fails
/// toward looking healthy, which is the reading an operator believes.
/// `audit_append_failures` is supplied by the runtime
/// because the audit store lives above `khive-db`; direct `khive-db` callers
/// receive `None` plus an explicit reason instead of a fabricated zero.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WriterContentionDiagnostics {
    /// FIFO writer lifetime observations; reading never starts the writer.
    pub writer_admission: Option<WriterAdmissionDiagnostics>,
    /// Disabled, not initialized, unavailable writer, or invalid telemetry state.
    pub writer_admission_unavailable_reason: Option<String>,
    /// Successful acquisitions across pooled, standalone, and writer-task
    /// connection classes.
    pub writer_acquisitions: u64,
    /// Successful finite-wait main-pool mutex checkouts.
    pub pooled_writer_acquisitions: u64,
    /// Successful per-operation file-backed standalone writer opens.
    pub standalone_writer_acquisitions: u64,
    /// Successful writer-task ownership acquisitions.
    pub writer_task_acquisitions: u64,
    /// Main-pool writer checkouts that exhausted their finite deadline.
    pub writer_acquisition_timeouts: u64,
    /// Writer acquisitions refused because another writer held the volume
    /// lease past the guard deadline (`CapacityUnavailable`, phase `lock`).
    /// Every writer class takes the lease before its connection, so ordinary
    /// same-volume writer contention is counted here, not in
    /// `writer_acquisition_timeouts`.
    pub writer_lease_timeouts: u64,
    /// The guard deadline a writer waits for the volume lease, in ms; `None`
    /// when the pool takes no lease (in-memory or read-only).
    pub configured_guard_deadline_ms: Option<u64>,
    /// `checkout_timeout`, which bounds only the pool-mutex wait that comes
    /// after the lease.
    pub configured_checkout_timeout_ms: u64,
    /// The effective pooled-writer wait bound under contention: the guard
    /// deadline for the lease, then `checkout_timeout` for the pool mutex. A
    /// pool configured with a 50 ms `checkout_timeout` and the default 2000 ms
    /// guard deadline can wait about 2050 ms before refusing.
    pub effective_writer_wait_bound_ms: u64,
    /// Final instrumented direct execution refusals retaining primary SQLITE_BUSY,
    /// once per operation; excludes SQLITE_LOCKED and task/reader/open/admission errors.
    pub direct_writer_busy_refusals: u64,
    /// Every writer-task `BEGIN IMMEDIATE` attempt refused busy or locked,
    /// whether or not a subsequent bounded retry absorbed it. A refusal not
    /// absorbed by a retry also surfaces to the caller as the retryable
    /// `writer_task_begin_busy` stage.
    pub writer_task_begin_busy: u64,
    /// Subset of `writer_task_begin_busy` absorbed by the bounded retry
    /// before the request closure ran, so the caller never observed that
    /// particular refusal. `writer_task_begin_busy - writer_task_begin_busy_absorbed`
    /// is the count of refusals a caller actually observed.
    pub writer_task_begin_busy_absorbed: u64,
    /// Writer-task `BEGIN IMMEDIATE` attempts that failed for a reason other
    /// than busy or locked.
    pub writer_task_begin_errors: u64,
    /// Dequeued writer-task requests that reached the writer seam and
    /// returned error, counted once per request. Sourced directly from the
    /// pool's own acquisition-site counters, so — unlike the runtime-supplied
    /// fields below — it is populated identically for every caller.
    pub writer_task_request_failures: u64,
    /// Subset of `writer_task_request_failures` whose terminal state was
    /// `WriterTaskRequestState::SideEffectsUnknown`.
    pub writer_task_side_effects_unknown: u64,
    /// Process-wide audit appends whose errors were logged and swallowed —
    /// pure-observability rows only (config-lock rows, best-effort recall
    /// telemetry). An obligation-bearing row's commit failure (a gate
    /// denial's own audit row, a dispatch outcome, an unknown-verb row, or a
    /// `git.digest` receipt) is never counted here: those either fail the
    /// dispatch that produced them directly (visible to the caller as an
    /// error, not as this counter moving) or, for a denial whose dispatch
    /// already fails independent of the row, are tracked by the runtime's
    /// own separate obligation-failure counter instead. Summing this field
    /// with `audit_batch_flush_failures` therefore does not double-count an
    /// obligation-bearing generation failure against this one.
    pub audit_append_failures: Option<u64>,
    /// Why `audit_append_failures` is unavailable to this caller.
    pub audit_append_failures_unavailable_reason: Option<String>,
    /// Process-wide obligation-bearing audit commit failures, including a
    /// denial's audit failure even though the denial already refuses the call.
    /// This counts failed producer submissions, not failed batch generations;
    /// do not add it to `audit_batch_flush_failures` as a disjoint total.
    pub audit_obligation_append_failures: Option<u64>,
    /// Why the runtime-owned obligation counter is unavailable to this caller.
    pub audit_obligation_append_failures_unavailable_reason: Option<String>,
    /// Accepted audit-batch generations that reached a terminal non-commit
    /// outcome after retry, including driver death; excludes preflight and
    /// admission rejection. `None` for a direct `khive-db` caller, or when no
    /// runtime audit-batch control has been wired into diagnostics.
    pub audit_batch_flush_failures: Option<u64>,
    /// Why `audit_batch_flush_failures` is unavailable to this caller.
    pub audit_batch_flush_failures_unavailable_reason: Option<String>,
    /// Pure-observability audit rows released without a commit. `None` under
    /// the same conditions as `audit_batch_flush_failures`.
    pub audit_degraded_rows: Option<u64>,
    /// Why `audit_degraded_rows` is unavailable to this caller.
    pub audit_degraded_rows_unavailable_reason: Option<String>,
    /// Monotonic process-lifetime flag set by a failed audit append or driver
    /// failure, or by accepted pure-observability rows released without a commit.
    /// Abandoned or shed generations containing only obligation-bearing rows
    /// increment `audit_batch_flush_failures` without setting this flag.
    /// `None` under the same conditions as `audit_batch_flush_failures`.
    pub audit_degraded: Option<bool>,
    /// Why `audit_degraded` is unavailable to this caller.
    pub audit_degraded_unavailable_reason: Option<String>,
    /// Per-dispatch audit rows for an explicitly allowlisted, domain-write-free
    /// read verb (`VerbRegistry::ADMISSION_DEGRADE_SAFE_VERBS`) that were
    /// **refused before they could be enqueued** on the audit lane
    /// (`AuditTerminalReason::QueueAdmissionExhausted`) while the dispatch
    /// still reported its own successful result (ADR-103 Amendment 3, ADR-133
    /// Amendment 1). This is a confirmed, terminal accounting loss: the row
    /// never shared a generation with anyone and will never commit, so it
    /// undercounts `brain.event_counts`'s cost totals for exactly the rows
    /// counted here. Disjoint from `audit_degraded_rows` (a different reason:
    /// persistent commit failure of a pure-observability row, not admission
    /// pressure) and from `audit_admission_unresolved_obligations` (a row that
    /// was enqueued and may still commit). `None` under the same conditions as
    /// `audit_batch_flush_failures`.
    ///
    /// CUMULATIVE since process start. Nothing decrements it, so a value that
    /// holds steady under traffic means no refusal occurred in that window
    /// rather than a stalled subsystem (#2791); read
    /// `audit_admission_refused_obligations_last_at_ms` beside it to tell the
    /// two apart.
    pub audit_admission_refused_obligations: Option<u64>,
    /// Wall-clock milliseconds at which `audit_admission_refused_obligations`
    /// last moved in the serving process, or `None` if it has never moved.
    /// `None` alongside a count of zero is the ordinary quiet case; `None`
    /// alongside a non-zero count cannot occur and would indicate the two are
    /// being produced from different processes.
    pub audit_admission_refused_obligations_last_at_ms: Option<u64>,
    /// Why `audit_admission_refused_obligations` is unavailable to this caller.
    pub audit_admission_refused_obligations_unavailable_reason: Option<String>,
    /// Successful admission-degrade-safe reads whose audit generation exhausted
    /// bounded transient retries. Cumulative since process start; these rows
    /// received no commit acknowledgement (ADR-154 Amendment 2).
    pub audit_retry_exhausted_obligations: Option<u64>,
    /// Last increment in wall-clock milliseconds, or `None` before the first.
    pub audit_retry_exhausted_obligations_last_at_ms: Option<u64>,
    /// Why the retry-exhausted obligation count is unavailable to this caller.
    pub audit_retry_exhausted_obligations_unavailable_reason: Option<String>,
    /// Per-dispatch audit rows for an explicitly allowlisted, domain-write-free
    /// read verb (`VerbRegistry::ADMISSION_DEGRADE_SAFE_VERBS`) that were
    /// **already enqueued but had not resolved when the caller's admission
    /// wait deadline elapsed** (`AuditTerminalReason::AdmissionDeadlineExpired`)
    /// while the dispatch still reported its own successful result (ADR-103
    /// Amendment 3, ADR-133 Amendment 1). Unlike
    /// `audit_admission_refused_obligations`, a row counted here is not a
    /// confirmed loss — it may still be committed by the generation driver
    /// independently of the caller's timeout — so this field is an upper
    /// bound on the eventual undercount, not the undercount itself. `None`
    /// under the same conditions as `audit_batch_flush_failures`.
    ///
    /// CUMULATIVE since process start, despite the set-shaped name. There is no
    /// live set of unresolved obligations and nothing resolves this counter:
    /// each increment records one past deadline expiry, and the row it counted
    /// most likely committed afterwards. A steady value under traffic means no
    /// deadline expired in that window, which is the healthy reading (#2791).
    /// Read `audit_admission_unresolved_obligations_last_at_ms` beside it: a
    /// non-zero count whose mark is old is history, the same count with a
    /// recent mark is an active condition.
    pub audit_admission_unresolved_obligations: Option<u64>,
    /// Wall-clock milliseconds at which
    /// `audit_admission_unresolved_obligations` last moved in the serving
    /// process, or `None` if it has never moved.
    pub audit_admission_unresolved_obligations_last_at_ms: Option<u64>,
    /// Why `audit_admission_unresolved_obligations` is unavailable to this
    /// caller.
    pub audit_admission_unresolved_obligations_unavailable_reason: Option<String>,
    /// Process-wide omitted success decisions by verb; unavailable without registry metrics.
    pub audit_success_rows_omitted: Option<std::collections::BTreeMap<&'static str, u64>>,
    /// Process-wide unselected, unforced success decisions by verb.
    pub audit_success_rows_sampled_out: Option<std::collections::BTreeMap<&'static str, u64>>,
    /// Process-wide force-only success decisions, including rows that do not commit.
    pub audit_success_rows_forced: Option<std::collections::BTreeMap<&'static str, u64>>,
}

/// Process-wide audit health and success-row decision counters, supplied by the runtime layer
/// that owns the audit-batch control. `khive-db` never produces these itself
/// — a direct `khive-db` caller always sees the corresponding
/// `WriterContentionDiagnostics` fields as `None` plus an explicit reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeAuditBatchMetrics {
    /// Accepted generations reaching terminal non-commit after retry,
    /// including driver death; excludes preflight/admission rejection.
    pub flush_failures: u64,
    /// Pure-observability rows released without commit.
    pub degraded_rows: u64,
    /// Monotonic process-lifetime degradation flag.
    pub degraded: bool,
    /// Admission-degrade-safe read verbs' audit rows refused before enqueue
    /// under transient audit-lane admission pressure (ADR-103 Amendment 3,
    /// ADR-133 Amendment 1) — a confirmed, terminal accounting loss. Disjoint
    /// from `degraded_rows` and from `admission_unresolved_obligations`.
    pub admission_refused_obligations: u64,
    /// Wall-clock ms at which `admission_refused_obligations` last moved;
    /// `None` until it moves. A total cannot say when it was last earned.
    pub admission_refused_obligations_last_at_ms: Option<u64>,
    /// Safe-read audit rows whose generation exhausted bounded transient retries.
    pub retry_exhausted_obligations: u64,
    /// Last increment in wall-clock milliseconds, or `None` before the first.
    pub retry_exhausted_obligations_last_at_ms: Option<u64>,
    /// Admission-degrade-safe read verbs' audit rows that were already
    /// enqueued but had not resolved when the caller's admission wait
    /// deadline elapsed (ADR-103 Amendment 3, ADR-133 Amendment 1). Not a
    /// confirmed loss — the row may still commit — so this is an upper bound
    /// on the eventual undercount. Disjoint from `degraded_rows` and from
    /// `admission_refused_obligations`.
    pub admission_unresolved_obligations: u64,
    /// Wall-clock ms at which `admission_unresolved_obligations` last moved;
    /// `None` until it moves.
    pub admission_unresolved_obligations_last_at_ms: Option<u64>,
    /// Bounded per-verb decision snapshots (ADR-103 Amendment 7), independent of commits.
    pub audit_success_rows_omitted: [(&'static str, u64); 4],
    /// Sampled successes that produced no row.
    pub audit_success_rows_sampled_out: [(&'static str, u64); 2],
    /// Successes retained only by their forcing rule.
    pub audit_success_rows_forced: [(&'static str, u64); 2],
}

/// Live graph-edge rows compared with the durable list-cursor ledger.
///
/// `duplicate_edge_id_groups > 0` is the exact pre-V14 state in which two
/// namespaces share an edge UUID and a multi-namespace cursor walk can drop
/// one row during ID-based deduplication. The two raw row counts are reported
/// separately because sequence rows intentionally survive hard deletion;
/// count inequality by itself is therefore not proof of corruption.
///
/// `live_entities_carrying_merged_into` counts entity rows that are live
/// (`deleted_at IS NULL`) while still carrying merge provenance. A merge
/// tombstones its source; a restore that ran before restore refused merge
/// tombstones cleared the tombstone and left the provenance, so such rows
/// read as merged by `get` and as live by `list` and `search`. Restore now
/// names them as `live_merged_entity`; this count is where an operator finds
/// them across all namespaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct GraphEdgeIntegrity {
    pub duplicate_edge_id_groups: i64,
    pub graph_edges_rows: i64,
    pub graph_edges_seq_rows: i64,
    pub pre_v14_duplicate_edge_state_detected: bool,
    pub live_entities_carrying_merged_into: i64,
}

fn graph_edge_integrity(conn: &Connection) -> rusqlite::Result<GraphEdgeIntegrity> {
    conn.query_row(
        "SELECT
             (SELECT COUNT(*) FROM (
                 SELECT id FROM graph_edges GROUP BY id HAVING COUNT(*) > 1
             )),
             (SELECT COUNT(*) FROM graph_edges),
             (SELECT COUNT(*) FROM graph_edges_seq),
             (SELECT COUNT(*) FROM entities
              WHERE deleted_at IS NULL AND merged_into IS NOT NULL)",
        [],
        |row| {
            let duplicate_edge_id_groups = row.get(0)?;
            Ok(GraphEdgeIntegrity {
                duplicate_edge_id_groups,
                graph_edges_rows: row.get(1)?,
                graph_edges_seq_rows: row.get(2)?,
                pre_v14_duplicate_edge_state_detected: duplicate_edge_id_groups > 0,
                live_entities_carrying_merged_into: row.get(3)?,
            })
        },
    )
}

const MAX_DATABASE_SIZE_OBJECTS: usize = 4_096;

/// SQLite b-tree role reported by the size-composition diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DatabaseObjectKind {
    Table,
    Index,
    Internal,
}

/// Operational storage grouping. `mixed_row_and_embedding` is deliberately
/// separate: SQLite cannot attribute bytes within a table page to one column,
/// so counting all of `knowledge_sections` as pure vector bytes would be a
/// false precision claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DatabaseStorageClass {
    RowTable,
    Index,
    FullText,
    Vector,
    MixedRowAndEmbedding,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DatabaseObjectSize {
    pub name: String,
    pub owner_table: Option<String>,
    pub object_kind: DatabaseObjectKind,
    pub storage_class: DatabaseStorageClass,
    pub pages: u64,
    pub bytes: u64,
}

/// Page-accounted file-size composition from SQLite's read-only `dbstat`
/// virtual table in aggregate mode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DatabaseSizeComposition {
    pub page_size_bytes: u64,
    pub page_count: u64,
    pub freelist_pages: u64,
    pub database_bytes: u64,
    pub freelist_bytes: u64,
    pub accounted_bytes: u64,
    pub unaccounted_bytes: u64,
    pub row_table_bytes: u64,
    pub index_bytes: u64,
    pub full_text_bytes: u64,
    pub vector_bytes: u64,
    pub mixed_embedding_bytes: u64,
    pub internal_bytes: u64,
    pub objects: Vec<DatabaseObjectSize>,
    pub objects_truncated: bool,
    pub objects_omitted: usize,
}

fn nonnegative_sqlite_integer(column: usize, value: i64) -> rusqlite::Result<u64> {
    u64::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(column, value))
}

/// The file's page size, page count and free-list length: one pragma each, so
/// the cost does not grow with the file. Reported for every file-backed
/// database whether or not the `dbstat` size composition was collected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct DatabasePageAccounting {
    pub page_size_bytes: u64,
    pub page_count: u64,
    /// Pages later writes can reuse that stay in the file until a `VACUUM`.
    pub freelist_pages: u64,
}

fn database_page_accounting(conn: &Connection) -> rusqlite::Result<DatabasePageAccounting> {
    let read = |pragma: &str| -> rusqlite::Result<u64> {
        nonnegative_sqlite_integer(0, conn.query_row(pragma, [], |row| row.get::<_, i64>(0))?)
    };
    Ok(DatabasePageAccounting {
        page_size_bytes: read("PRAGMA page_size")?,
        page_count: read("PRAGMA page_count")?,
        freelist_pages: read("PRAGMA freelist_count")?,
    })
}

fn declares_embedding_blob(sql: Option<&str>) -> bool {
    let Some(sql) = sql else {
        return false;
    };
    let tokens: Vec<_> = sql
        .split(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
        .filter(|token| !token.is_empty())
        .collect();
    tokens.windows(2).any(|pair| {
        pair[0].eq_ignore_ascii_case("embedding") && pair[1].eq_ignore_ascii_case("blob")
    })
}

fn classify_database_object(
    name: &str,
    sqlite_type: &str,
    sql: Option<&str>,
) -> (DatabaseObjectKind, DatabaseStorageClass) {
    let object_kind = match sqlite_type {
        "table" => DatabaseObjectKind::Table,
        "index" => DatabaseObjectKind::Index,
        _ => DatabaseObjectKind::Internal,
    };
    let lower_name = name.to_ascii_lowercase();
    let lower_sql = sql.unwrap_or_default().to_ascii_lowercase();
    let storage_class = if lower_name.starts_with("fts_") || lower_sql.contains("using fts5") {
        DatabaseStorageClass::FullText
    } else if lower_name.starts_with("vec_")
        || lower_name == "_embedding_models"
        || lower_sql.contains("using vec0")
    {
        DatabaseStorageClass::Vector
    } else if declares_embedding_blob(sql) {
        DatabaseStorageClass::MixedRowAndEmbedding
    } else if object_kind == DatabaseObjectKind::Index {
        DatabaseStorageClass::Index
    } else if name.starts_with("sqlite_") || object_kind == DatabaseObjectKind::Internal {
        DatabaseStorageClass::Internal
    } else {
        DatabaseStorageClass::RowTable
    };
    (object_kind, storage_class)
}

fn database_size_composition(
    conn: &Connection,
    accounting: DatabasePageAccounting,
) -> rusqlite::Result<DatabaseSizeComposition> {
    let DatabasePageAccounting {
        page_size_bytes: page_size,
        page_count,
        freelist_pages,
    } = accounting;
    #[cfg(test)]
    size_composition::before_dbstat();

    let mut statement = conn.prepare(
        "SELECT d.name, COALESCE(s.type, 'internal'), s.tbl_name, s.sql, d.pageno, d.pgsize
         FROM dbstat AS d
         LEFT JOIN sqlite_schema AS s ON s.name = d.name
         WHERE d.aggregate = TRUE
         ORDER BY d.name",
    )?;
    let mut rows = statement.query([])?;
    let mut objects = Vec::new();
    let mut objects_omitted = 0usize;
    let mut accounted_bytes = 0u64;
    let mut row_table_bytes = 0u64;
    let mut index_bytes = 0u64;
    let mut full_text_bytes = 0u64;
    let mut vector_bytes = 0u64;
    let mut mixed_embedding_bytes = 0u64;
    let mut internal_bytes = 0u64;

    while let Some(row) = rows.next()? {
        let name: String = row.get(0)?;
        let sqlite_type: String = row.get(1)?;
        let owner_table: Option<String> = row.get(2)?;
        let sql: Option<String> = row.get(3)?;
        let pages = nonnegative_sqlite_integer(4, row.get(4)?)?;
        let bytes = nonnegative_sqlite_integer(5, row.get(5)?)?;
        let (object_kind, storage_class) =
            classify_database_object(&name, &sqlite_type, sql.as_deref());
        accounted_bytes = accounted_bytes.saturating_add(bytes);
        let class_total = match storage_class {
            DatabaseStorageClass::RowTable => &mut row_table_bytes,
            DatabaseStorageClass::Index => &mut index_bytes,
            DatabaseStorageClass::FullText => &mut full_text_bytes,
            DatabaseStorageClass::Vector => &mut vector_bytes,
            DatabaseStorageClass::MixedRowAndEmbedding => &mut mixed_embedding_bytes,
            DatabaseStorageClass::Internal => &mut internal_bytes,
        };
        *class_total = class_total.saturating_add(bytes);

        if objects.len() < MAX_DATABASE_SIZE_OBJECTS {
            objects.push(DatabaseObjectSize {
                name,
                owner_table,
                object_kind,
                storage_class,
                pages,
                bytes,
            });
        } else {
            objects_omitted = objects_omitted.saturating_add(1);
        }
    }

    let database_bytes = page_count.saturating_mul(page_size);
    let freelist_bytes = freelist_pages.saturating_mul(page_size);
    let unaccounted_bytes = database_bytes
        .saturating_sub(freelist_bytes)
        .saturating_sub(accounted_bytes);
    Ok(DatabaseSizeComposition {
        page_size_bytes: page_size,
        page_count,
        freelist_pages,
        database_bytes,
        freelist_bytes,
        accounted_bytes,
        unaccounted_bytes,
        row_table_bytes,
        index_bytes,
        full_text_bytes,
        vector_bytes,
        mixed_embedding_bytes,
        internal_bytes,
        objects,
        objects_truncated: objects_omitted > 0,
        objects_omitted,
    })
}

/// What this report cost to assemble, per section, in milliseconds.
///
/// `db_diagnostics` is the verb an operator reaches for when the system feels
/// slow, and its own cost was measured varying more than five-fold between
/// consecutive calls against one unchanged store on one box. A surface that
/// varies that much has to say which part varied; otherwise the reader is left
/// to guess, and the cheapest guess is "the store is stalled", which is the
/// conclusion this verb exists to test rather than to suggest.
///
/// Every field is wall-clock and measured in this process, so it includes time
/// spent waiting on a contended machine. That is deliberate: the contention is
/// the thing being reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CollectionCost {
    /// Whole-report assembly, from the first section to the last.
    pub total_ms: u64,
    /// SQLite-side inspection: the PASSIVE checkpoint probe, page composition,
    /// the graph-edge ledger counts and the FTS5 segment records.
    pub sqlite_ms: u64,
    /// `stat` of the WAL file. The cheap sibling of the census; reported so the
    /// comparison beside it is legible rather than asserted.
    pub wal_file_stat_ms: u64,
    /// The OS holder census: a walk of every process on the host.
    pub wal_pin_census_ms: u64,
    /// The sidecar directory enumeration the census is reconciled against.
    pub wal_pin_sidecar_ms: u64,
    /// The wall-clock budget the census was given, or `None` when it ran
    /// unbounded. An `Option` rather than a sentinel `0` because a producer
    /// that failed to record a budget would write `0`, which is exactly the
    /// value "unbounded" would have claimed.
    pub wal_pin_census_budget_ms: Option<u64>,
    /// Whether the census stopped because that budget was spent. When true the
    /// holder list is partial by design and `wal_pin.census.status` is
    /// `incomplete` for that reason and no other.
    pub wal_pin_census_budget_exhausted: bool,
}

impl CollectionCost {
    /// A report whose file-backed sections never ran (an in-memory backend):
    /// the SQLite side still costs what it costs, and the census did not run
    /// at all rather than running fast.
    fn in_memory(total_ms: u64) -> Self {
        Self {
            total_ms,
            sqlite_ms: 0,
            wal_file_stat_ms: 0,
            wal_pin_census_ms: 0,
            wal_pin_sidecar_ms: 0,
            wal_pin_census_budget_ms: None,
            wal_pin_census_budget_exhausted: false,
        }
    }
}

/// The default wall-clock budget for the OS holder census on the request path.
///
/// Sized from the measurements in the report that asked for the bound: five
/// consecutive samples on a 971-process box cost 3.4s to 18.6s, while every
/// contention counter the same report carried read zero. Two seconds keeps the
/// verb interactive on a busy machine and completes untruncated on an idle one.
const DEFAULT_CENSUS_BUDGET: Duration = Duration::from_millis(2000);

/// Environment override for [`DEFAULT_CENSUS_BUDGET`], in milliseconds.
///
/// `0` disables the bound and restores the unbounded full-machine walk, for an
/// operator who would rather wait than read a partial holder list. An
/// unparseable value is ignored in favour of the default rather than failing
/// the request: a diagnostic verb that refuses to answer because its own
/// tuning knob is malformed is worse than one that answers on the default and
/// says which budget it used — which the report does, in
/// `collection_cost.wal_pin_census_budget_ms`.
const CENSUS_BUDGET_ENV: &str = "KHIVE_WALPIN_CENSUS_BUDGET_MS";

fn request_census_budget() -> Option<Duration> {
    match std::env::var(CENSUS_BUDGET_ENV) {
        Ok(raw) => match raw.trim().parse::<u64>() {
            Ok(0) => None,
            Ok(ms) => Some(Duration::from_millis(ms)),
            Err(_) => Some(DEFAULT_CENSUS_BUDGET),
        },
        Err(_) => Some(DEFAULT_CENSUS_BUDGET),
    }
}

/// The full database-integrity, reader/writer-contention, and WAL/checkpoint
/// payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct WalCeilingDiagnostics {
    /// Resolved configuration value, retained even on a read-only backend.
    pub configured_bytes: u64,
    /// Active writer-policy limit; zero on read-only backends and when disabled.
    pub effective_bytes: u64,
    pub source: WalCeilingSource,
    pub enabled: bool,
    /// Why the configured policy is active or inactive for this backend.
    pub status: &'static str,
}

impl WalCeilingDiagnostics {
    fn from_pool(pool: &ConnectionPool) -> Self {
        let policy = pool.config().wal_ceiling;
        let read_only = pool.config().read_only;
        let effective_bytes = policy.effective_bytes(read_only);
        Self {
            configured_bytes: policy.bytes,
            effective_bytes,
            source: policy.source,
            enabled: effective_bytes > 0,
            status: if effective_bytes > 0 {
                "enforced"
            } else if read_only && policy.bytes > 0 {
                "read_only_not_enforced"
            } else {
                "disabled"
            },
        }
    }
}

/// Process-wide code-map VFS counters, sampled independently under each ledger's
/// mutex. This observe-only diagnostic block has no stability contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CodeMapVfsDiagnostics {
    pub quarantine_occupancy: usize,
    pub quarantine_cap: usize,
    pub registrations: usize,
    pub registration_cap: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DbDiagnostics {
    pub build: BuildIdentity,
    pub process: ProcessIdentity,
    pub code_map_vfs: CodeMapVfsDiagnostics,
    /// Process-lifetime note-search vector route counts, independent of this
    /// report's database file and reset only with the serving process.
    pub note_search_ann_route_total: u64,
    pub note_search_fallback_route_total: u64,
    /// Pool-scoped, monotonic coordinator dispatch and note-candidate
    /// hydration counts used by ADR-166 G4/G5. Reset on pool reconstruction.
    pub search_mechanism: crate::pool::SearchMechanismSnapshot,
    /// `None` for an in-memory backend — the file-backed sections then carry
    /// their own unavailability reasons.
    pub db_path: Option<String>,
    /// Explicit WAL ceiling policy for this already-open database.
    pub wal_ceiling: WalCeilingDiagnostics,
    pub disk_guard: DiskGuardDiagnostics,
    pub wal_file: Option<WalFileState>,
    pub checkpoint_counters: CheckpointCounters,
    pub checkpoint_probe: Option<CheckpointProbe>,
    pub checkpoint_probe_error: Option<String>,
    #[serde(flatten)]
    /// Frame pin fields serialized beside `checkpoint_probe` at the report root.
    pub checkpoint_pin: CheckpointPinDiagnostics,
    /// Reader route, checkout saturation, and hold-lifecycle signals.
    pub reader_contention: ReaderContentionDiagnostics,
    /// Writer-pool and best-effort audit persistence signals.
    pub writer_contention: WriterContentionDiagnostics,
    pub size_composition: Option<DatabaseSizeComposition>,
    pub size_composition_error: Option<String>,
    /// Page size, page count and free-list length from three pragmas, present
    /// whether or not `size_composition` was collected.
    pub page_accounting: Option<DatabasePageAccounting>,
    pub page_accounting_error: Option<String>,
    pub graph_edge_integrity: Option<GraphEdgeIntegrity>,
    pub graph_edge_integrity_error: Option<String>,
    /// Current FTS5 segment structure, decoded from each index's documented
    /// one-row structure record (shadow-table row id 10). This does not scan
    /// either corpus or `%_idx` table.
    pub fts_segments: Option<crate::FtsSegmentDiagnostics>,
    pub fts_segments_error: Option<String>,
    /// Process-lifetime outcomes from the checkpoint task's bounded FTS5
    /// maintenance steps.
    pub fts_maintenance: crate::FtsMaintenanceCounters,
    pub wal_pin: WalPinAttribution,
    /// Per-section assembly cost for this report. See [`CollectionCost`].
    pub collection_cost: CollectionCost,
}

/// Checkpoint-row evidence used to report a sustained backfill ceiling.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CheckpointPinDiagnostics {
    /// The probe row's checkpointed frame when it shows unbackfilled frames.
    pub backfill_ceiling: Option<i64>,
    /// Why the probe row did not establish a backfill ceiling.
    pub backfill_ceiling_unavailable_reason: Option<String>,
    /// The ceiling after a matching checkpoint run has lasted at least one second.
    pub oldest_pinned_frame: Option<i64>,
    /// Why the current probe did not establish an oldest pinned frame.
    pub oldest_pinned_frame_unavailable_reason: Option<String>,
    /// The first observation for the reported frame.
    pub oldest_pinned_frame_run: Option<crate::checkpoint::CheckpointRun>,
    /// Why the matching checkpoint run is unavailable.
    pub oldest_pinned_frame_run_unavailable_reason: Option<String>,
    /// Frames from the sustained pin to the current probe's log end.
    pub pin_depth: Option<i64>,
    /// Why this report cannot claim a pin depth.
    pub pin_depth_unavailable_reason: Option<String>,
}

fn checkpoint_pin_diagnostics(
    pool: &ConnectionPool,
    probe: Option<&CheckpointProbe>,
    probe_error: Option<&str>,
) -> CheckpointPinDiagnostics {
    let (run_status, run_age) = checkpoint::checkpoint_run_snapshot(pool);
    checkpoint_pin_diagnostics_for_run(probe, probe_error, run_status, run_age)
}

fn checkpoint_pin_diagnostics_for_run(
    probe: Option<&CheckpointProbe>,
    probe_error: Option<&str>,
    run_status: checkpoint::CheckpointRunStatus,
    run_age: Option<Duration>,
) -> CheckpointPinDiagnostics {
    let (backfill_ceiling, backfill_reason) = match probe {
        None => (
            None,
            Some(
                probe_error
                    .unwrap_or("checkpoint probe did not return a row")
                    .to_string(),
            ),
        ),
        Some(probe) if probe.busy != 0 => (
            None,
            Some(format!("PASSIVE checkpoint returned busy={}", probe.busy)),
        ),
        Some(probe) if probe.log_frames < 0 || probe.checkpointed_frames < 0 => (
            None,
            Some("PASSIVE checkpoint returned a negative frame count".to_string()),
        ),
        Some(probe) if probe.log_frames <= probe.checkpointed_frames => (
            None,
            Some("PASSIVE checkpoint found no frames beyond the backfill ceiling".to_string()),
        ),
        Some(probe) => (Some(probe.checkpointed_frames), None),
    };

    let (oldest_pinned_frame, oldest_reason, oldest_pinned_frame_run, run_reason) =
        match backfill_ceiling {
            None => {
                let reason = backfill_reason
                    .clone()
                    .unwrap_or_else(|| "backfill ceiling is unavailable".to_string());
                (None, Some(reason.clone()), None, Some(reason))
            }
            Some(ceiling) => match run_status {
                checkpoint::CheckpointRunStatus::NoTask => {
                    let reason = "no checkpoint task in this process".to_string();
                    (None, Some(reason.clone()), None, Some(reason))
                }
                checkpoint::CheckpointRunStatus::NoObservation => {
                    let reason = "no checkpoint run has been observed for this backend".to_string();
                    (None, Some(reason.clone()), None, Some(reason))
                }
                checkpoint::CheckpointRunStatus::Observed(run) if run.frame != ceiling => {
                    let reason = "checkpoint run frame does not match the current backfill ceiling";
                    (
                        None,
                        Some(reason.to_string()),
                        None,
                        Some(reason.to_string()),
                    )
                }
                checkpoint::CheckpointRunStatus::Observed(_)
                    if run_age.is_none_or(|age| age < Duration::from_secs(1)) =>
                {
                    let reason = "checkpoint run has been observed for less than one second";
                    (
                        None,
                        Some(reason.to_string()),
                        None,
                        Some(reason.to_string()),
                    )
                }
                checkpoint::CheckpointRunStatus::Observed(run) => {
                    (Some(ceiling), None, Some(run), None)
                }
            },
        };

    let pin_depth = oldest_pinned_frame.map(|frame| {
        probe
            .expect("a reported pin always has a probe row")
            .log_frames
            .saturating_sub(frame)
            .max(0)
    });
    let pin_depth_reason = if pin_depth.is_none() {
        oldest_reason.clone()
    } else {
        None
    };

    CheckpointPinDiagnostics {
        backfill_ceiling,
        backfill_ceiling_unavailable_reason: backfill_reason,
        oldest_pinned_frame,
        oldest_pinned_frame_unavailable_reason: oldest_reason,
        oldest_pinned_frame_run,
        oldest_pinned_frame_run_unavailable_reason: run_reason,
        pin_depth,
        pin_depth_unavailable_reason: pin_depth_reason,
    }
}

/// Assemble the report for `pool`'s database.
///
/// `db_path` in the returned report is the pool's own configured path, so
/// the report can never claim to describe a file the pool is not bound to.
/// Every operational probe — the WAL file, the sidecar, the OS holder census
/// — instead targets `pool.canonical_path()`, the same value `ConnectionPool`
/// and the checkpoint sidecar writers key off of; a symlinked or otherwise
/// aliased configured path would otherwise send those probes looking beside
/// the alias while the evidence sits beside the canonical file. An in-memory
/// pool has no path: its process- and pool-scoped counters are still real,
/// but every file-backed section degrades to an explicit "unavailable" with
/// a reason rather than being silently omitted.
///
/// The PASSIVE probe goes through the infrastructure-only untracked
/// standalone open, WITHOUT `SQLITE_OPEN_CREATE`, so a diagnostic request
/// against a missing file returns `checkpoint_probe: null` with a
/// `checkpoint_probe_error` instead of creating a database or incrementing
/// the write-traffic acquisition total. Running on a standalone connection
/// also keeps checkpoint I/O off the pooled writer mutex.
pub fn collect(
    pool: &ConnectionPool,
    build: BuildIdentity,
    sweep_interval: Duration,
) -> DbDiagnostics {
    collect_inner(pool, build, sweep_interval, None, None)
}

/// Assemble the report with the runtime's process-wide count of swallowed
/// best-effort audit append failures.
pub fn collect_with_audit_append_failures(
    pool: &ConnectionPool,
    build: BuildIdentity,
    sweep_interval: Duration,
    audit_append_failures: u64,
) -> DbDiagnostics {
    collect_inner(
        pool,
        build,
        sweep_interval,
        Some(audit_append_failures),
        None,
    )
}

/// Assemble diagnostics without allowing request abandonment to leave the
/// graph SELECT or OS holder census running detached.
///
/// The PASSIVE checkpoint is intentionally outside SQLite interruption: once
/// admitted it may backfill database pages and must reach its physical
/// completion. The following graph integrity SELECT registers the common
/// request progress callback on that exact connection. The OS census runs as
/// a second cooperative phase and polls a shared cancellation flag between
/// process and fd entries.
pub async fn collect_with_audit_append_failures_interruptibly(
    pool: Arc<ConnectionPool>,
    build: BuildIdentity,
    sweep_interval: Duration,
    audit_append_failures: u64,
) -> StorageResult<DbDiagnostics> {
    collect_with_runtime_audit_metrics_interruptibly(
        pool,
        build,
        sweep_interval,
        audit_append_failures,
        None,
    )
    .await
}

/// Like [`collect_with_audit_append_failures_interruptibly`], additionally
/// threading through the runtime's audit-batch health counters. `None` when
/// no audit-batch control is registered with the calling runtime instance —
/// the corresponding `writer_contention` fields then report unavailable with
/// a reason, exactly like `audit_append_failures` does for a direct
/// `khive-db` caller.
pub async fn collect_with_runtime_audit_metrics_interruptibly(
    pool: Arc<ConnectionPool>,
    build: BuildIdentity,
    sweep_interval: Duration,
    audit_append_failures: u64,
    runtime_audit_batch_metrics: Option<RuntimeAuditBatchMetrics>,
) -> StorageResult<DbDiagnostics> {
    let process = ProcessIdentity::current(&pool);
    collect_with_runtime_audit_metrics_for_process_interruptibly(
        pool,
        build,
        process,
        sweep_interval,
        audit_append_failures,
        runtime_audit_batch_metrics,
        true,
    )
    .await
}

/// Collect one already-open pool while retaining the main pool's process and
/// generation identity. A secondary pool must not claim or advance the main
/// pool generation when its pool-scoped counters are inspected.
///
/// `collect_size_composition = false` skips the `dbstat` walk, whose cost
/// grows with the file because it reads every page; the report then carries
/// [`SIZE_COMPOSITION_NOT_REQUESTED`] in `size_composition_error`, and
/// `page_accounting` still reports the page counts.
pub async fn collect_with_runtime_audit_metrics_for_process_interruptibly(
    pool: Arc<ConnectionPool>,
    build: BuildIdentity,
    process: ProcessIdentity,
    sweep_interval: Duration,
    audit_append_failures: u64,
    runtime_audit_batch_metrics: Option<RuntimeAuditBatchMetrics>,
    collect_size_composition: bool,
) -> StorageResult<DbDiagnostics> {
    crate::ensure_request_read_active("db_diagnostics")?;
    let started = Instant::now();
    let counters = checkpoint_counters();
    let reader_contention = ReaderContentionDiagnostics::snapshot(&pool);
    let search_mechanism = pool.search_mechanism_snapshot();
    let writer_contention = WriterContentionDiagnostics::snapshot(
        &pool,
        Some(audit_append_failures),
        runtime_audit_batch_metrics,
    );

    let Some(path) = pool.config().path.clone() else {
        crate::ensure_request_read_active("db_diagnostics")?;
        return Ok(DbDiagnostics {
            build,
            process,
            code_map_vfs: crate::code_map_vfs::diagnostics(),
            note_search_ann_route_total: 0,
            note_search_fallback_route_total: 0,
            search_mechanism,
            db_path: None,
            wal_ceiling: WalCeilingDiagnostics::from_pool(&pool),
            disk_guard: DiskGuardDiagnostics::snapshot(&pool),
            wal_file: None,
            checkpoint_counters: counters,
            checkpoint_probe: None,
            checkpoint_probe_error: Some(
                "in-memory database: no WAL file and no checkpoint to probe".to_string(),
            ),
            checkpoint_pin: checkpoint_pin_diagnostics(
                &pool,
                None,
                Some("in-memory database: no WAL file and no checkpoint to probe"),
            ),
            reader_contention,
            writer_contention,
            size_composition: None,
            size_composition_error: Some(
                "in-memory database: no file-backed page composition to inspect".to_string(),
            ),
            page_accounting: None,
            page_accounting_error: Some(
                "in-memory database: no file-backed pages to count".to_string(),
            ),
            graph_edge_integrity: None,
            graph_edge_integrity_error: Some(
                "in-memory database: no durable graph-edge ledger to inspect".to_string(),
            ),
            fts_segments: None,
            fts_segments_error: Some(
                "in-memory database: no durable FTS5 indexes to inspect".to_string(),
            ),
            fts_maintenance: crate::fts_maintenance_counters(),
            wal_pin: WalPinAttribution::unavailable(
                "in-memory database: no file for the OS holder census",
            ),
            collection_cost: CollectionCost::in_memory(elapsed_ms(started)),
        });
    };

    let inspection_pool = Arc::clone(&pool);
    let sqlite_started = Instant::now();
    let inspection = crate::read_cancellation::run_interruptible_read(
        StorageCapability::Sql,
        "db_diagnostics.sqlite",
        move |scope| inspect_pool_interruptibly(&inspection_pool, scope, collect_size_composition),
    )
    .await?;
    let sqlite_ms = elapsed_ms(sqlite_started);
    crate::ensure_request_read_active("db_diagnostics")?;
    let canonical = operational_db_path(&pool, &path);
    let budget = request_census_budget();
    let (wal_file, wal_pin, file_state_cost) =
        inspect_file_state_interruptibly(canonical, sweep_interval, budget).await?;
    crate::ensure_request_read_active("db_diagnostics")?;

    Ok(DbDiagnostics {
        build,
        process,
        code_map_vfs: crate::code_map_vfs::diagnostics(),
        note_search_ann_route_total: 0,
        note_search_fallback_route_total: 0,
        search_mechanism,
        db_path: Some(path.display().to_string()),
        wal_ceiling: WalCeilingDiagnostics::from_pool(&pool),
        disk_guard: DiskGuardDiagnostics::snapshot(&pool),
        wal_file: Some(wal_file),
        checkpoint_counters: counters,
        checkpoint_probe: inspection.checkpoint_probe,
        checkpoint_probe_error: inspection.checkpoint_probe_error,
        checkpoint_pin: inspection.checkpoint_pin,
        reader_contention,
        writer_contention,
        size_composition: inspection.size_composition,
        size_composition_error: inspection.size_composition_error,
        page_accounting: inspection.page_accounting,
        page_accounting_error: inspection.page_accounting_error,
        graph_edge_integrity: inspection.graph_edge_integrity,
        graph_edge_integrity_error: inspection.graph_edge_integrity_error,
        fts_segments: inspection.fts_segments,
        fts_segments_error: inspection.fts_segments_error,
        fts_maintenance: crate::fts_maintenance_counters(),
        wal_pin,
        collection_cost: CollectionCost {
            total_ms: elapsed_ms(started),
            sqlite_ms,
            wal_file_stat_ms: file_state_cost.wal_file_stat_ms,
            wal_pin_census_ms: file_state_cost.census_ms,
            wal_pin_sidecar_ms: file_state_cost.sidecar_ms,
            wal_pin_census_budget_ms: budget.map(|b| b.as_millis() as u64),
            wal_pin_census_budget_exhausted: file_state_cost.census_budget_exhausted,
        },
    })
}

/// The path every operational probe (WAL file, sidecar directory, OS holder
/// census) resolves against for `pool`'s database. The checkpoint sidecar
/// writers and `ConnectionPool` itself key the WAL and sidecar directory off
/// `canonical_path()`, not the raw `configured` path — a symlinked or
/// otherwise aliased configured path would otherwise send every probe below
/// looking beside the alias while the evidence sits beside the canonical
/// file. `configured` remains the presentation value the sync and async
/// collectors put in `db_path`, since that is what the caller configured.
/// Both `collect_inner` and `collect_with_runtime_audit_metrics_interruptibly`
/// resolve through this one function so their aliasing behavior cannot drift
/// apart.
fn operational_db_path(pool: &ConnectionPool, configured: &Path) -> PathBuf {
    pool.canonical_path()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| configured.to_path_buf())
}

fn collect_inner(
    pool: &ConnectionPool,
    build: BuildIdentity,
    sweep_interval: Duration,
    audit_append_failures: Option<u64>,
    runtime_audit_batch_metrics: Option<RuntimeAuditBatchMetrics>,
) -> DbDiagnostics {
    let started = Instant::now();
    let process = ProcessIdentity::current(pool);
    let counters = checkpoint_counters();
    let reader_contention = ReaderContentionDiagnostics::snapshot(pool);
    let search_mechanism = pool.search_mechanism_snapshot();
    let writer_contention = WriterContentionDiagnostics::snapshot(
        pool,
        audit_append_failures,
        runtime_audit_batch_metrics,
    );

    let Some(path) = pool.config().path.clone() else {
        return DbDiagnostics {
            build,
            process,
            code_map_vfs: crate::code_map_vfs::diagnostics(),
            note_search_ann_route_total: 0,
            note_search_fallback_route_total: 0,
            search_mechanism,
            db_path: None,
            wal_ceiling: WalCeilingDiagnostics::from_pool(pool),
            disk_guard: DiskGuardDiagnostics::snapshot(pool),
            wal_file: None,
            checkpoint_counters: counters,
            checkpoint_probe: None,
            checkpoint_probe_error: Some(
                "in-memory database: no WAL file and no checkpoint to probe".to_string(),
            ),
            checkpoint_pin: checkpoint_pin_diagnostics(
                pool,
                None,
                Some("in-memory database: no WAL file and no checkpoint to probe"),
            ),
            reader_contention,
            writer_contention,
            size_composition: None,
            size_composition_error: Some(
                "in-memory database: no file-backed page composition to inspect".to_string(),
            ),
            page_accounting: None,
            page_accounting_error: Some(
                "in-memory database: no file-backed pages to count".to_string(),
            ),
            graph_edge_integrity: None,
            graph_edge_integrity_error: Some(
                "in-memory database: no durable graph-edge ledger to inspect".to_string(),
            ),
            fts_segments: None,
            fts_segments_error: Some(
                "in-memory database: no durable FTS5 indexes to inspect".to_string(),
            ),
            fts_maintenance: crate::fts_maintenance_counters(),
            wal_pin: WalPinAttribution::unavailable(
                "in-memory database: no file for the OS holder census",
            ),
            collection_cost: CollectionCost::in_memory(elapsed_ms(started)),
        };
    };

    let sqlite_started = Instant::now();
    let inspection = inspect_pool(pool);
    let sqlite_ms = elapsed_ms(sqlite_started);
    let canonical = operational_db_path(pool, &path);
    let wal_file_started = Instant::now();
    let wal_file = wal_file_state(&canonical);
    let wal_file_stat_ms = elapsed_ms(wal_file_started);
    let census_started = Instant::now();
    let wal_pin = wal_pin_attribution(&canonical, sweep_interval);
    let wal_pin_ms = elapsed_ms(census_started);

    DbDiagnostics {
        build,
        process,
        code_map_vfs: crate::code_map_vfs::diagnostics(),
        note_search_ann_route_total: 0,
        note_search_fallback_route_total: 0,
        search_mechanism,
        db_path: Some(path.display().to_string()),
        wal_ceiling: WalCeilingDiagnostics::from_pool(pool),
        disk_guard: DiskGuardDiagnostics::snapshot(pool),
        wal_file: Some(wal_file),
        checkpoint_counters: counters,
        checkpoint_probe: inspection.checkpoint_probe,
        checkpoint_probe_error: inspection.checkpoint_probe_error,
        checkpoint_pin: inspection.checkpoint_pin,
        reader_contention,
        writer_contention,
        size_composition: inspection.size_composition,
        size_composition_error: inspection.size_composition_error,
        page_accounting: inspection.page_accounting,
        page_accounting_error: inspection.page_accounting_error,
        graph_edge_integrity: inspection.graph_edge_integrity,
        graph_edge_integrity_error: inspection.graph_edge_integrity_error,
        fts_segments: inspection.fts_segments,
        fts_segments_error: inspection.fts_segments_error,
        fts_maintenance: crate::fts_maintenance_counters(),
        wal_pin,
        collection_cost: CollectionCost {
            total_ms: elapsed_ms(started),
            sqlite_ms,
            wal_file_stat_ms,
            // This path does not separate the census from the sidecar read it
            // is reconciled against: it calls the combined `wal_pin_attribution`,
            // so splitting them here would mean inventing a number. The
            // request path, which is the one the operator waits on, does split
            // them.
            wal_pin_census_ms: wal_pin_ms,
            wal_pin_sidecar_ms: 0,
            wal_pin_census_budget_ms: None,
            wal_pin_census_budget_exhausted: false,
        },
    }
}

pub(crate) struct PoolInspection {
    checkpoint_probe: Option<CheckpointProbe>,
    checkpoint_probe_error: Option<String>,
    checkpoint_pin: CheckpointPinDiagnostics,
    size_composition: Option<DatabaseSizeComposition>,
    size_composition_error: Option<String>,
    page_accounting: Option<DatabasePageAccounting>,
    page_accounting_error: Option<String>,
    graph_edge_integrity: Option<GraphEdgeIntegrity>,
    graph_edge_integrity_error: Option<String>,
    fts_segments: Option<crate::FtsSegmentDiagnostics>,
    fts_segments_error: Option<String>,
}

/// Split an FTS segment inspection result into the `(value, error)` pair
/// shape every section of [`PoolInspection`]/[`DbDiagnostics`] uses, shared
/// by the interruptible and plain inspection paths below.
fn split_fts_segments_result(
    result: Result<crate::FtsSegmentDiagnostics, String>,
) -> (Option<crate::FtsSegmentDiagnostics>, Option<String>) {
    match result {
        Ok(segments) => (Some(segments), None),
        Err(error) => (None, Some(error)),
    }
}

fn split_page_accounting_result(
    result: rusqlite::Result<DatabasePageAccounting>,
) -> (Option<DatabasePageAccounting>, Option<String>) {
    match result {
        Ok(pages) => (Some(pages), None),
        Err(error) => (None, Some(format!("page accounting query failed: {error}"))),
    }
}

/// The `size_composition_error` of a report whose caller skipped the `dbstat`
/// walk. The walk reads every page of the file; `page_accounting` still
/// carries the page counts.
pub const SIZE_COMPOSITION_NOT_REQUESTED: &str =
    "not collected: size_composition=false skips the dbstat walk, which reads every page";

fn record_diagnostic_checkpoint_probe(
    pool: &ConnectionPool,
    result: &rusqlite::Result<CheckpointProbe>,
) -> (checkpoint::CheckpointRunStatus, Option<Duration>) {
    checkpoint::record_checkpoint_run_result(
        pool,
        result
            .as_ref()
            .ok()
            .map(|probe| (probe.busy, probe.log_frames, probe.checkpointed_frames)),
    );
    checkpoint::checkpoint_run_snapshot(pool)
}

fn inspect_pool_interruptibly(
    pool: &ConnectionPool,
    scope: &crate::read_cancellation::InterruptibleReadScope,
    collect_size_composition: bool,
) -> StorageResult<PoolInspection> {
    pool.inspect_diagnostic_connection_interruptibly(scope, collect_size_composition)
}

pub(crate) fn inspect_open_connection_interruptibly(
    pool: &ConnectionPool,
    opened: Result<rusqlite::Connection, crate::error::SqliteError>,
    scope: &crate::read_cancellation::InterruptibleReadScope,
    collect_size_composition: bool,
) -> StorageResult<PoolInspection> {
    scope.ensure_active()?;
    let conn = match opened {
        Ok(conn) => conn,
        Err(e) => {
            scope.ensure_active()?;
            let reason = format!("guarded standalone open refused: {e}");
            return Ok(PoolInspection {
                checkpoint_probe: None,
                checkpoint_probe_error: Some(reason.clone()),
                checkpoint_pin: checkpoint_pin_diagnostics(pool, None, Some(&reason)),
                size_composition: None,
                size_composition_error: Some(reason.clone()),
                page_accounting: None,
                page_accounting_error: Some(reason.clone()),
                graph_edge_integrity: None,
                graph_edge_integrity_error: Some(reason.clone()),
                fts_segments: None,
                fts_segments_error: Some(reason),
            });
        }
    };
    // Opening is read-only filesystem work but can block. Cancellation that
    // arrived while it was in flight must prevent admission of the following
    // PASSIVE checkpoint, whose backfill I/O is intentionally noninterruptible
    // once started.
    scope.ensure_active()?;

    // PASSIVE can perform write I/O. Never install sqlite3_interrupt for it.
    let probe_result = checkpoint_probe(&conn);
    let (run_status, run_age) = record_diagnostic_checkpoint_probe(pool, &probe_result);
    let (checkpoint_probe, checkpoint_probe_error) = match probe_result {
        Ok(probe) => (Some(probe), None),
        Err(e) => (
            None,
            Some(format!("PRAGMA wal_checkpoint(PASSIVE) failed: {e}")),
        ),
    };
    let checkpoint_pin = checkpoint_pin_diagnostics_for_run(
        checkpoint_probe.as_ref(),
        checkpoint_probe_error.as_deref(),
        run_status,
        run_age,
    );
    #[cfg(test)]
    if TEST_PAUSE_AFTER_PASSIVE.load(Ordering::SeqCst) {
        TEST_REACHED_AFTER_PASSIVE.store(true, Ordering::SeqCst);
        while TEST_PAUSE_AFTER_PASSIVE.load(Ordering::SeqCst) && !scope.should_stop() {
            std::thread::yield_now();
        }
    }
    scope.ensure_active()?;

    // Install one progress/interrupt guard for all logical reads on this
    // connection. The scope deliberately refuses double registration, so
    // keep each query's ordinary SQLite result nested inside one guarded
    // execution while allowing the outer cancellation cause to remain typed.
    let (integrity, pages, fts_segments) = scope.run(&conn, || {
        Ok((
            graph_edge_integrity(&conn),
            size_composition::collect(&conn, collect_size_composition),
            crate::fts_maintenance::inspect_fts_segments(&conn),
        ))
    })?;
    let (graph_edge_integrity, graph_edge_integrity_error) = match integrity {
        Ok(integrity) => (Some(integrity), None),
        Err(e) => (
            None,
            Some(format!("graph-edge integrity query failed: {e}")),
        ),
    };
    let (fts_segments, fts_segments_error) = split_fts_segments_result(fts_segments);
    let size_composition::PageInspection {
        page_accounting,
        page_accounting_error,
        size_composition,
        size_composition_error,
    } = pages;

    Ok(PoolInspection {
        checkpoint_probe,
        checkpoint_probe_error,
        checkpoint_pin,
        size_composition,
        size_composition_error,
        page_accounting,
        page_accounting_error,
        graph_edge_integrity,
        graph_edge_integrity_error,
        fts_segments,
        fts_segments_error,
    })
}

#[cfg(test)]
static TEST_PAUSE_AFTER_PASSIVE: AtomicBool = AtomicBool::new(false);
#[cfg(test)]
static TEST_REACHED_AFTER_PASSIVE: AtomicBool = AtomicBool::new(false);

struct StopCensusOnDrop {
    stopped: Arc<AtomicBool>,
    armed: bool,
}

impl Drop for StopCensusOnDrop {
    fn drop(&mut self) {
        if self.armed {
            self.stopped.store(true, Ordering::SeqCst);
        }
    }
}

/// Wall-clock cost of the file-backed half of a report, split so the census
/// can be told apart from the two cheap reads it sits between.
#[derive(Debug, Clone, Copy, Default)]
struct FileStateCost {
    wal_file_stat_ms: u64,
    census_ms: u64,
    sidecar_ms: u64,
    census_budget_exhausted: bool,
}

fn elapsed_ms(since: Instant) -> u64 {
    since.elapsed().as_millis() as u64
}

async fn inspect_file_state_interruptibly(
    path: PathBuf,
    sweep_interval: Duration,
    census_budget: Option<Duration>,
) -> StorageResult<(WalFileState, WalPinAttribution, FileStateCost)> {
    const OPERATION: &str = "db_diagnostics.wal_holder_census";
    crate::ensure_request_read_active(OPERATION)?;
    let stopped = Arc::new(AtomicBool::new(false));
    let worker_stopped = Arc::clone(&stopped);
    let mut stop_on_drop = StopCensusOnDrop {
        stopped: Arc::clone(&stopped),
        armed: true,
    };
    let mut worker = tokio::task::spawn_blocking(move || {
        let mut cost = FileStateCost::default();
        let wal_file_started = Instant::now();
        let wal_file = wal_file_state(&path);
        cost.wal_file_stat_ms = elapsed_ms(wal_file_started);
        if worker_stopped.load(Ordering::SeqCst) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "WAL holder census cancelled",
            ));
        }
        #[cfg(unix)]
        let census_started = Instant::now();
        #[cfg(unix)]
        let census_result = match census_budget {
            Some(budget) => crate::walpin::census_holders_until_within(
                &path,
                || worker_stopped.load(Ordering::SeqCst),
                budget,
            ),
            None => {
                crate::walpin::census_holders_until(&path, || worker_stopped.load(Ordering::SeqCst))
            }
        };
        #[cfg(unix)]
        {
            cost.census_ms = elapsed_ms(census_started);
        }
        #[cfg(unix)]
        let attribution = match census_result {
            Ok(census) => {
                cost.census_budget_exhausted = census.budget_exhausted;
                if worker_stopped.load(Ordering::SeqCst) {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Interrupted,
                        "WAL sidecar inspection cancelled",
                    ));
                }
                if !crate::walpin::sidecar_enabled(true) {
                    wal_pin_attribution_without_sidecar(census, SIDECAR_DISABLED_REASON.to_string())
                } else {
                    let sidecar_started = Instant::now();
                    let sidecar = crate::walpin::inspect_live(
                        &crate::walpin::sidecar_dir_for(&path),
                        sweep_interval,
                    );
                    cost.sidecar_ms = elapsed_ms(sidecar_started);
                    if worker_stopped.load(Ordering::SeqCst) {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::Interrupted,
                            "WAL sidecar inspection cancelled",
                        ));
                    }
                    match sidecar {
                        Ok(sidecar) => wal_pin_attribution_from_evidence(census, sidecar),
                        Err(error) => wal_pin_attribution_without_sidecar(
                            census,
                            format!("read-only sidecar enumeration failed: {error}"),
                        ),
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => return Err(error),
            Err(error) => WalPinAttribution::unavailable(format!("census_holders failed: {error}")),
        };
        #[cfg(not(unix))]
        let attribution = {
            let _ = census_budget;
            let census_started = Instant::now();
            let attribution = wal_pin_attribution(&path, sweep_interval);
            cost.census_ms = elapsed_ms(census_started);
            attribution
        };
        Ok((wal_file, attribution, cost))
    });

    tokio::select! {
        joined = &mut worker => {
            stop_on_drop.armed = false;
            let result = joined
                .map_err(|error| StorageError::driver(StorageCapability::Sql, OPERATION, error))?
                .map_err(|error| StorageError::driver(StorageCapability::Sql, OPERATION, error))?;
            crate::ensure_request_read_active(OPERATION)?;
            Ok(result)
        }
        _ = crate::wait_for_request_read_cancellation() => {
            stopped.store(true, Ordering::SeqCst);
            if tokio::time::timeout(crate::sqlite_interrupt_grace_from_env(), &mut worker)
                .await
                .is_err()
            {
                worker.abort();
            }
            stop_on_drop.armed = false;
            Err(StorageError::Timeout { operation: OPERATION.into() })
        }
    }
}

/// Run the PASSIVE probe and graph-ledger reads on one guarded standalone
/// connection.
///
/// Every failure — missing file, read-only pool, in-memory pool, or the
/// pragma/query itself — comes back in its section's error field. Nothing
/// here can create a file.
fn inspect_pool(pool: &ConnectionPool) -> PoolInspection {
    pool.inspect_diagnostic_connection(true)
}

pub(crate) fn inspect_open_connection(
    pool: &ConnectionPool,
    opened: Result<rusqlite::Connection, crate::error::SqliteError>,
    collect_size_composition: bool,
) -> PoolInspection {
    let conn = match opened {
        Ok(conn) => conn,
        Err(e) => {
            let reason = format!("guarded standalone open refused: {e}");
            return PoolInspection {
                checkpoint_probe: None,
                checkpoint_probe_error: Some(reason.clone()),
                checkpoint_pin: checkpoint_pin_diagnostics(pool, None, Some(&reason)),
                size_composition: None,
                size_composition_error: Some(reason.clone()),
                page_accounting: None,
                page_accounting_error: Some(reason.clone()),
                graph_edge_integrity: None,
                graph_edge_integrity_error: Some(reason.clone()),
                fts_segments: None,
                fts_segments_error: Some(reason),
            };
        }
    };

    let probe_result = checkpoint_probe(&conn);
    let (run_status, run_age) = record_diagnostic_checkpoint_probe(pool, &probe_result);
    let (checkpoint_probe, checkpoint_probe_error) = match probe_result {
        Ok(probe) => (Some(probe), None),
        Err(e) => (
            None,
            Some(format!("PRAGMA wal_checkpoint(PASSIVE) failed: {e}")),
        ),
    };
    let checkpoint_pin = checkpoint_pin_diagnostics_for_run(
        checkpoint_probe.as_ref(),
        checkpoint_probe_error.as_deref(),
        run_status,
        run_age,
    );
    let (graph_edge_integrity, graph_edge_integrity_error) = match graph_edge_integrity(&conn) {
        Ok(integrity) => (Some(integrity), None),
        Err(e) => (
            None,
            Some(format!("graph-edge integrity query failed: {e}")),
        ),
    };
    let size_composition::PageInspection {
        page_accounting,
        page_accounting_error,
        size_composition,
        size_composition_error,
    } = size_composition::collect(&conn, collect_size_composition);
    let (fts_segments, fts_segments_error) =
        split_fts_segments_result(crate::fts_maintenance::inspect_fts_segments(&conn));

    PoolInspection {
        checkpoint_probe,
        checkpoint_probe_error,
        checkpoint_pin,
        size_composition,
        size_composition_error,
        page_accounting,
        page_accounting_error,
        graph_edge_integrity,
        graph_edge_integrity_error,
        fts_segments,
        fts_segments_error,
    }
}

#[cfg(test)]
#[path = "diagnostics_tests.rs"]
mod tests;
