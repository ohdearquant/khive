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
//! 2. The probe's connection comes from
//!    `ConnectionPool::open_standalone_writer_untracked`, opened without
//!    `SQLITE_OPEN_CREATE`. A missing database yields `checkpoint_probe:
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
//!
//! The counters are process-global statics inside this crate, so a report is
//! only meaningful when built inside the process that owns the checkpoint
//! task (the daemon). Every payload therefore carries
//! [`BuildIdentity`](crate::diagnostics::BuildIdentity), so a reading is
//! self-labeling about which build's counters it describes.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
#[cfg(unix)]
use std::time::{SystemTime, UNIX_EPOCH};

use khive_storage::error::StorageError;
use khive_storage::types::StorageResult;
use khive_storage::StorageCapability;
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
        Self {
            status: WalPinAttributionStatus::Unavailable,
            status_reasons: vec![reason.clone()],
            census: WalPinCensus::Unavailable {
                reason: reason.clone(),
            },
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
    let mut census_uninspectable_pids = census.uninspectable_pids;
    census_uninspectable_pids.sort_unstable();
    census_uninspectable_pids.dedup();
    let census_truncated = census.truncated;

    let mut status_reasons = vec![sidecar_reason];
    let census = if census_is_complete {
        WalPinCensus::Complete {
            holder_pids: census_holder_pids.clone(),
        }
    } else {
        let mut causes = Vec::new();
        if census_truncated {
            causes.push("the OS process walk was truncated".to_string());
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
    let mut census_uninspectable_pids = census.uninspectable_pids;
    census_uninspectable_pids.sort_unstable();
    census_uninspectable_pids.dedup();
    let census_truncated = census.truncated;
    let census_carrier = if census_is_complete {
        WalPinCensus::Complete {
            holder_pids: census_holder_pids.clone(),
        }
    } else {
        let mut causes = Vec::new();
        if census_truncated {
            causes.push("the OS process walk was truncated".to_string());
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
    /// Pooled reader guards live when the snapshot was captured.
    pub active_pooled_reader_checkouts: u64,
    /// Highest observed concurrent pooled-reader guard count.
    pub peak_active_pooled_reader_checkouts: u64,
    /// Pooled guards that completed return/reset.
    pub completed_pooled_reader_checkouts: u64,
    /// Longest completed hold, including return/reset, in microseconds.
    pub max_completed_reader_hold_micros: u64,
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
            reader_admission_capacity: reader.reader_admission_capacity,
            available_reader_admission_slots: reader.available_reader_admission_slots,
            reader_acquisitions: reader.acquisitions,
            pooled_reader_checkouts: reader.pooled_checkouts,
            standalone_reader_opens: reader.standalone_opens,
            infrastructure_standalone_reader_opens: reader.infrastructure_standalone_opens,
            reader_checkout_timeouts: reader.checkout_timeouts,
            active_pooled_reader_checkouts: reader.active_pooled_checkouts,
            peak_active_pooled_reader_checkouts: reader.peak_active_pooled_checkouts,
            completed_pooled_reader_checkouts: reader.completed_pooled_checkouts,
            max_completed_reader_hold_micros: reader.max_completed_hold_micros,
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
/// (`writer_task_begin_busy`, `writer_task_begin_errors`) rather than being
/// absent: refusing to mislabel a failure is not a reason to omit it, and an
/// omitted failure counter fails toward looking healthy, which is the reading
/// an operator believes. `audit_append_failures` is supplied by the runtime
/// because the audit store lives above `khive-db`; direct `khive-db` callers
/// receive `None` plus an explicit reason instead of a fabricated zero.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WriterContentionDiagnostics {
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
    /// Writer-task `BEGIN IMMEDIATE` attempts refused busy or locked. These
    /// are the refusals a caller sees as the retryable `writer_task_begin_busy`
    /// stage, so a nonzero value here has a matching failed request on the
    /// caller's side rather than being visible only from inside the process.
    pub writer_task_begin_busy: u64,
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
    /// Monotonic process-lifetime flag set once any row has been released
    /// degraded. `None` under the same conditions as
    /// `audit_batch_flush_failures`.
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
    pub audit_admission_refused_obligations: Option<u64>,
    /// Why `audit_admission_refused_obligations` is unavailable to this caller.
    pub audit_admission_refused_obligations_unavailable_reason: Option<String>,
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
    pub audit_admission_unresolved_obligations: Option<u64>,
    /// Why `audit_admission_unresolved_obligations` is unavailable to this
    /// caller.
    pub audit_admission_unresolved_obligations_unavailable_reason: Option<String>,
}

/// Process-wide audit-batch health counters, supplied by the runtime layer
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
    /// Admission-degrade-safe read verbs' audit rows that were already
    /// enqueued but had not resolved when the caller's admission wait
    /// deadline elapsed (ADR-103 Amendment 3, ADR-133 Amendment 1). Not a
    /// confirmed loss — the row may still commit — so this is an upper bound
    /// on the eventual undercount. Disjoint from `degraded_rows` and from
    /// `admission_refused_obligations`.
    pub admission_unresolved_obligations: u64,
}

impl WriterContentionDiagnostics {
    fn snapshot(
        pool: &ConnectionPool,
        audit_append_failures: Option<u64>,
        runtime_audit_batch_metrics: Option<RuntimeAuditBatchMetrics>,
    ) -> Self {
        let writer = pool.writer_acquisition_snapshot();
        let unavailable_reason =
            || Some("no audit-batch control is registered with this runtime instance".to_string());
        Self {
            writer_acquisitions: writer.acquisitions,
            pooled_writer_acquisitions: writer.pooled_acquisitions,
            standalone_writer_acquisitions: writer.standalone_acquisitions,
            writer_task_acquisitions: writer.writer_task_acquisitions,
            writer_acquisition_timeouts: writer.timeouts,
            writer_task_begin_busy: writer.writer_task_begin_busy,
            writer_task_begin_errors: writer.writer_task_begin_errors,
            writer_task_request_failures: writer.writer_task_request_failures,
            writer_task_side_effects_unknown: writer.writer_task_side_effects_unknown,
            audit_append_failures,
            audit_append_failures_unavailable_reason: audit_append_failures.is_none().then(|| {
                "runtime audit instrumentation was not supplied to khive-db diagnostics".to_string()
            }),
            audit_batch_flush_failures: runtime_audit_batch_metrics.map(|m| m.flush_failures),
            audit_batch_flush_failures_unavailable_reason: runtime_audit_batch_metrics
                .is_none()
                .then(unavailable_reason)
                .flatten(),
            audit_degraded_rows: runtime_audit_batch_metrics.map(|m| m.degraded_rows),
            audit_degraded_rows_unavailable_reason: runtime_audit_batch_metrics
                .is_none()
                .then(unavailable_reason)
                .flatten(),
            audit_degraded: runtime_audit_batch_metrics.map(|m| m.degraded),
            audit_degraded_unavailable_reason: runtime_audit_batch_metrics
                .is_none()
                .then(unavailable_reason)
                .flatten(),
            audit_admission_refused_obligations: runtime_audit_batch_metrics
                .map(|m| m.admission_refused_obligations),
            audit_admission_refused_obligations_unavailable_reason: runtime_audit_batch_metrics
                .is_none()
                .then(unavailable_reason)
                .flatten(),
            audit_admission_unresolved_obligations: runtime_audit_batch_metrics
                .map(|m| m.admission_unresolved_obligations),
            audit_admission_unresolved_obligations_unavailable_reason: runtime_audit_batch_metrics
                .is_none()
                .then(unavailable_reason)
                .flatten(),
        }
    }
}

/// Live graph-edge rows compared with the durable list-cursor ledger.
///
/// `duplicate_edge_id_groups > 0` is the exact pre-V14 state in which two
/// namespaces share an edge UUID and a multi-namespace cursor walk can drop
/// one row during ID-based deduplication. The two raw row counts are reported
/// separately because sequence rows intentionally survive hard deletion;
/// count inequality by itself is therefore not proof of corruption.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct GraphEdgeIntegrity {
    pub duplicate_edge_id_groups: i64,
    pub graph_edges_rows: i64,
    pub graph_edges_seq_rows: i64,
    pub pre_v14_duplicate_edge_state_detected: bool,
}

fn graph_edge_integrity(conn: &Connection) -> rusqlite::Result<GraphEdgeIntegrity> {
    conn.query_row(
        "SELECT
             (SELECT COUNT(*) FROM (
                 SELECT id FROM graph_edges GROUP BY id HAVING COUNT(*) > 1
             )),
             (SELECT COUNT(*) FROM graph_edges),
             (SELECT COUNT(*) FROM graph_edges_seq)",
        [],
        |row| {
            let duplicate_edge_id_groups = row.get(0)?;
            Ok(GraphEdgeIntegrity {
                duplicate_edge_id_groups,
                graph_edges_rows: row.get(1)?,
                graph_edges_seq_rows: row.get(2)?,
                pre_v14_duplicate_edge_state_detected: duplicate_edge_id_groups > 0,
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

fn database_size_composition(conn: &Connection) -> rusqlite::Result<DatabaseSizeComposition> {
    let page_size = nonnegative_sqlite_integer(
        0,
        conn.query_row("PRAGMA page_size", [], |row| row.get::<_, i64>(0))?,
    )?;
    let page_count = nonnegative_sqlite_integer(
        0,
        conn.query_row("PRAGMA page_count", [], |row| row.get::<_, i64>(0))?,
    )?;
    let freelist_pages = nonnegative_sqlite_integer(
        0,
        conn.query_row("PRAGMA freelist_count", [], |row| row.get::<_, i64>(0))?,
    )?;

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

/// The full database-integrity, reader/writer-contention, and WAL/checkpoint
/// payload.
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
    /// Reader route, checkout saturation, and hold-lifecycle signals.
    pub reader_contention: ReaderContentionDiagnostics,
    /// Writer-pool and best-effort audit persistence signals.
    pub writer_contention: WriterContentionDiagnostics,
    pub size_composition: Option<DatabaseSizeComposition>,
    pub size_composition_error: Option<String>,
    pub graph_edge_integrity: Option<GraphEdgeIntegrity>,
    pub graph_edge_integrity_error: Option<String>,
    pub wal_pin: WalPinAttribution,
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
/// pool has no path: the counters are still real (they are process-global),
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
    crate::ensure_request_read_active("db_diagnostics")?;
    let counters = checkpoint_counters();
    let reader_contention = ReaderContentionDiagnostics::snapshot(&pool);
    let writer_contention = WriterContentionDiagnostics::snapshot(
        &pool,
        Some(audit_append_failures),
        runtime_audit_batch_metrics,
    );

    let Some(path) = pool.config().path.clone() else {
        crate::ensure_request_read_active("db_diagnostics")?;
        return Ok(DbDiagnostics {
            build,
            db_path: None,
            wal_file: None,
            checkpoint_counters: counters,
            checkpoint_probe: None,
            checkpoint_probe_error: Some(
                "in-memory database: no WAL file and no checkpoint to probe".to_string(),
            ),
            reader_contention,
            writer_contention,
            size_composition: None,
            size_composition_error: Some(
                "in-memory database: no file-backed page composition to inspect".to_string(),
            ),
            graph_edge_integrity: None,
            graph_edge_integrity_error: Some(
                "in-memory database: no durable graph-edge ledger to inspect".to_string(),
            ),
            wal_pin: WalPinAttribution::unavailable(
                "in-memory database: no file for the OS holder census",
            ),
        });
    };

    let inspection_pool = Arc::clone(&pool);
    let inspection = crate::read_cancellation::run_interruptible_read(
        StorageCapability::Sql,
        "db_diagnostics.sqlite",
        move |scope| inspect_pool_interruptibly(&inspection_pool, scope),
    )
    .await?;
    crate::ensure_request_read_active("db_diagnostics")?;
    let canonical = operational_db_path(&pool, &path);
    let (wal_file, wal_pin) = inspect_file_state_interruptibly(canonical, sweep_interval).await?;
    crate::ensure_request_read_active("db_diagnostics")?;

    Ok(DbDiagnostics {
        build,
        db_path: Some(path.display().to_string()),
        wal_file: Some(wal_file),
        checkpoint_counters: counters,
        checkpoint_probe: inspection.checkpoint_probe,
        checkpoint_probe_error: inspection.checkpoint_probe_error,
        reader_contention,
        writer_contention,
        size_composition: inspection.size_composition,
        size_composition_error: inspection.size_composition_error,
        graph_edge_integrity: inspection.graph_edge_integrity,
        graph_edge_integrity_error: inspection.graph_edge_integrity_error,
        wal_pin,
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
    let counters = checkpoint_counters();
    let reader_contention = ReaderContentionDiagnostics::snapshot(pool);
    let writer_contention = WriterContentionDiagnostics::snapshot(
        pool,
        audit_append_failures,
        runtime_audit_batch_metrics,
    );

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
            reader_contention,
            writer_contention,
            size_composition: None,
            size_composition_error: Some(
                "in-memory database: no file-backed page composition to inspect".to_string(),
            ),
            graph_edge_integrity: None,
            graph_edge_integrity_error: Some(
                "in-memory database: no durable graph-edge ledger to inspect".to_string(),
            ),
            wal_pin: WalPinAttribution::unavailable(
                "in-memory database: no file for the OS holder census",
            ),
        };
    };

    let inspection = inspect_pool(pool);
    let canonical = operational_db_path(pool, &path);

    DbDiagnostics {
        build,
        db_path: Some(path.display().to_string()),
        wal_file: Some(wal_file_state(&canonical)),
        checkpoint_counters: counters,
        checkpoint_probe: inspection.checkpoint_probe,
        checkpoint_probe_error: inspection.checkpoint_probe_error,
        reader_contention,
        writer_contention,
        size_composition: inspection.size_composition,
        size_composition_error: inspection.size_composition_error,
        graph_edge_integrity: inspection.graph_edge_integrity,
        graph_edge_integrity_error: inspection.graph_edge_integrity_error,
        wal_pin: wal_pin_attribution(&canonical, sweep_interval),
    }
}

struct PoolInspection {
    checkpoint_probe: Option<CheckpointProbe>,
    checkpoint_probe_error: Option<String>,
    size_composition: Option<DatabaseSizeComposition>,
    size_composition_error: Option<String>,
    graph_edge_integrity: Option<GraphEdgeIntegrity>,
    graph_edge_integrity_error: Option<String>,
}

fn inspect_pool_interruptibly(
    pool: &ConnectionPool,
    scope: &crate::read_cancellation::InterruptibleReadScope,
) -> StorageResult<PoolInspection> {
    scope.ensure_active()?;
    let conn = match pool.open_standalone_writer_untracked() {
        Ok(conn) => conn,
        Err(e) => {
            scope.ensure_active()?;
            let reason = format!("guarded standalone open refused: {e}");
            return Ok(PoolInspection {
                checkpoint_probe: None,
                checkpoint_probe_error: Some(reason.clone()),
                size_composition: None,
                size_composition_error: Some(reason.clone()),
                graph_edge_integrity: None,
                graph_edge_integrity_error: Some(reason),
            });
        }
    };
    // Opening is read-only filesystem work but can block. Cancellation that
    // arrived while it was in flight must prevent admission of the following
    // PASSIVE checkpoint, whose backfill I/O is intentionally noninterruptible
    // once started.
    scope.ensure_active()?;

    // PASSIVE can perform write I/O. Never install sqlite3_interrupt for it.
    let (checkpoint_probe, checkpoint_probe_error) = match checkpoint_probe(&conn) {
        Ok(probe) => (Some(probe), None),
        Err(e) => (
            None,
            Some(format!("PRAGMA wal_checkpoint(PASSIVE) failed: {e}")),
        ),
    };
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
    let (integrity, size_composition) = scope.run(&conn, || {
        Ok((
            graph_edge_integrity(&conn),
            database_size_composition(&conn),
        ))
    })?;
    let (graph_edge_integrity, graph_edge_integrity_error) = match integrity {
        Ok(integrity) => (Some(integrity), None),
        Err(e) => (
            None,
            Some(format!("graph-edge integrity query failed: {e}")),
        ),
    };

    let (size_composition, size_composition_error) = match size_composition {
        Ok(composition) => (Some(composition), None),
        Err(error) => (
            None,
            Some(format!("database size composition query failed: {error}")),
        ),
    };

    Ok(PoolInspection {
        checkpoint_probe,
        checkpoint_probe_error,
        size_composition,
        size_composition_error,
        graph_edge_integrity,
        graph_edge_integrity_error,
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

async fn inspect_file_state_interruptibly(
    path: PathBuf,
    sweep_interval: Duration,
) -> StorageResult<(WalFileState, WalPinAttribution)> {
    const OPERATION: &str = "db_diagnostics.wal_holder_census";
    crate::ensure_request_read_active(OPERATION)?;
    let stopped = Arc::new(AtomicBool::new(false));
    let worker_stopped = Arc::clone(&stopped);
    let mut stop_on_drop = StopCensusOnDrop {
        stopped: Arc::clone(&stopped),
        armed: true,
    };
    let mut worker = tokio::task::spawn_blocking(move || {
        let wal_file = wal_file_state(&path);
        if worker_stopped.load(Ordering::SeqCst) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "WAL holder census cancelled",
            ));
        }
        #[cfg(unix)]
        let attribution = match crate::walpin::census_holders_until(&path, || {
            worker_stopped.load(Ordering::SeqCst)
        }) {
            Ok(census) => {
                if worker_stopped.load(Ordering::SeqCst) {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Interrupted,
                        "WAL sidecar inspection cancelled",
                    ));
                }
                if !crate::walpin::sidecar_enabled(true) {
                    wal_pin_attribution_without_sidecar(census, SIDECAR_DISABLED_REASON.to_string())
                } else {
                    let sidecar = crate::walpin::inspect_live(
                        &crate::walpin::sidecar_dir_for(&path),
                        sweep_interval,
                    );
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
        let attribution = wal_pin_attribution(&path, sweep_interval);
        Ok((wal_file, attribution))
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
    let conn = match pool.open_standalone_writer_untracked() {
        Ok(conn) => conn,
        Err(e) => {
            let reason = format!("guarded standalone open refused: {e}");
            return PoolInspection {
                checkpoint_probe: None,
                checkpoint_probe_error: Some(reason.clone()),
                size_composition: None,
                size_composition_error: Some(reason.clone()),
                graph_edge_integrity: None,
                graph_edge_integrity_error: Some(reason),
            };
        }
    };

    let (checkpoint_probe, checkpoint_probe_error) = match checkpoint_probe(&conn) {
        Ok(probe) => (Some(probe), None),
        Err(e) => (
            None,
            Some(format!("PRAGMA wal_checkpoint(PASSIVE) failed: {e}")),
        ),
    };
    let (graph_edge_integrity, graph_edge_integrity_error) = match graph_edge_integrity(&conn) {
        Ok(integrity) => (Some(integrity), None),
        Err(e) => (
            None,
            Some(format!("graph-edge integrity query failed: {e}")),
        ),
    };
    let (size_composition, size_composition_error) = match database_size_composition(&conn) {
        Ok(composition) => (Some(composition), None),
        Err(error) => (
            None,
            Some(format!("database size composition query failed: {error}")),
        ),
    };

    PoolInspection {
        checkpoint_probe,
        checkpoint_probe_error,
        size_composition,
        size_composition_error,
        graph_edge_integrity,
        graph_edge_integrity_error,
    }
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
                     CREATE TABLE graph_edges (
                         namespace TEXT NOT NULL,
                         id TEXT NOT NULL,
                         PRIMARY KEY (namespace, id)
                     ); \
                     CREATE TABLE graph_edges_seq (
                         seq INTEGER PRIMARY KEY AUTOINCREMENT,
                         edge_id TEXT NOT NULL UNIQUE
                     ); \
                     INSERT INTO t VALUES (1), (2), (3);",
                )
                .expect("seed writes");
        }
        (pool, path)
    }

    #[test]
    fn dropping_census_future_guard_requests_cooperative_stop() {
        let stopped = Arc::new(AtomicBool::new(false));
        let guard = StopCensusOnDrop {
            stopped: Arc::clone(&stopped),
            armed: true,
        };

        drop(guard);

        assert!(
            stopped.load(Ordering::SeqCst),
            "dropping diagnostics while its census worker is live must stop the PID/fd walk"
        );
    }

    #[tokio::test]
    async fn runtime_audit_batch_fields_are_additive_and_unavailable_without_a_control() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (pool, _path) = seeded_pool(&dir);
        let pool = Arc::new(pool);

        let without_control = collect_with_audit_append_failures_interruptibly(
            Arc::clone(&pool),
            BuildIdentity::from_env("test", None),
            Duration::from_secs(30),
            0,
        )
        .await
        .expect("diagnostics succeed");
        assert!(without_control
            .writer_contention
            .audit_batch_flush_failures
            .is_none());
        assert!(
            without_control
                .writer_contention
                .audit_batch_flush_failures_unavailable_reason
                .is_some(),
            "no audit-batch control was supplied, so the field must carry a reason, not a \
             fabricated zero"
        );
        assert!(without_control
            .writer_contention
            .audit_degraded_rows
            .is_none());
        assert!(without_control.writer_contention.audit_degraded.is_none());

        let with_control = collect_with_runtime_audit_metrics_interruptibly(
            Arc::clone(&pool),
            BuildIdentity::from_env("test", None),
            Duration::from_secs(30),
            0,
            Some(RuntimeAuditBatchMetrics {
                flush_failures: 3,
                degraded_rows: 7,
                degraded: true,
                admission_refused_obligations: 5,
                admission_unresolved_obligations: 2,
            }),
        )
        .await
        .expect("diagnostics succeed");
        assert_eq!(
            with_control.writer_contention.audit_batch_flush_failures,
            Some(3)
        );
        assert!(with_control
            .writer_contention
            .audit_batch_flush_failures_unavailable_reason
            .is_none());
        assert_eq!(with_control.writer_contention.audit_degraded_rows, Some(7));
        assert_eq!(with_control.writer_contention.audit_degraded, Some(true));
        assert_eq!(
            with_control
                .writer_contention
                .audit_admission_refused_obligations,
            Some(5),
            "an operator must be able to read the admission-refused obligation count from \
             db_diagnostics without a test-only feature gate (ADR-103 Amendment 3)"
        );
        assert!(with_control
            .writer_contention
            .audit_admission_refused_obligations_unavailable_reason
            .is_none());
        assert_eq!(
            with_control
                .writer_contention
                .audit_admission_unresolved_obligations,
            Some(2),
            "an operator must be able to distinguish enqueued-but-unresolved rows from \
             confirmed-refused rows (ADR-103 Amendment 3)"
        );
        assert!(with_control
            .writer_contention
            .audit_admission_unresolved_obligations_unavailable_reason
            .is_none());
        assert!(without_control
            .writer_contention
            .audit_admission_refused_obligations
            .is_none());
        assert!(without_control
            .writer_contention
            .audit_admission_refused_obligations_unavailable_reason
            .is_some());
        assert!(without_control
            .writer_contention
            .audit_admission_unresolved_obligations
            .is_none());
        assert!(without_control
            .writer_contention
            .audit_admission_unresolved_obligations_unavailable_reason
            .is_some());

        // Existing fields must be unaffected by the new ones — additive, not
        // a reshuffle.
        assert_eq!(
            with_control.writer_contention.writer_acquisitions,
            without_control.writer_contention.writer_acquisitions
        );
    }

    #[test]
    fn writer_task_pool_sourced_counters_are_always_populated_directly() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (pool, _path) = seeded_pool(&dir);

        let report = collect(
            &pool,
            BuildIdentity::from_env("9.9.9", None),
            Duration::from_secs(30),
        );

        // Unlike the runtime-supplied audit-batch fields, these two come
        // straight from the pool's own counters and are never `Option`.
        assert_eq!(report.writer_contention.writer_task_request_failures, 0);
        assert_eq!(report.writer_contention.writer_task_side_effects_unknown, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial]
    async fn request_cancellation_after_passive_stops_before_graph_and_census() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (pool, _) = seeded_pool(&dir);
        let pool = Arc::new(pool);
        TEST_REACHED_AFTER_PASSIVE.store(false, Ordering::SeqCst);
        TEST_PAUSE_AFTER_PASSIVE.store(true, Ordering::SeqCst);
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let diagnostic_pool = Arc::clone(&pool);
        let task = tokio::spawn(crate::scope_request_read_cancellation(
            cancel_rx,
            async move {
                collect_with_audit_append_failures_interruptibly(
                    diagnostic_pool,
                    BuildIdentity::from_env("test", None),
                    Duration::from_secs(30),
                    0,
                )
                .await
            },
        ));

        tokio::time::timeout(Duration::from_secs(1), async {
            while !TEST_REACHED_AFTER_PASSIVE.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("diagnostics never completed its admitted PASSIVE phase");
        cancel_tx.send(true).unwrap();
        let result = tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("cancelled diagnostics did not stop promptly")
            .expect("diagnostics task panicked");
        TEST_PAUSE_AFTER_PASSIVE.store(false, Ordering::SeqCst);
        assert!(matches!(result, Err(StorageError::Timeout { .. })));

        let one: i64 = pool
            .reader()
            .expect("diagnostics returned its connection")
            .conn()
            .query_row("SELECT 1", [], |row| row.get(0))
            .unwrap();
        assert_eq!(one, 1);
    }

    #[test]
    fn checkpoint_probe_returns_a_well_formed_triple_on_a_file_backed_db() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (pool, _path) = seeded_pool(&dir);
        let conn = pool
            .open_standalone_writer_untracked()
            .expect("standalone probe connection");

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
    /// of the ADR-091 counters.
    #[test]
    #[serial(checkpoint_skip_metrics)]
    fn checkpoint_probe_does_not_perturb_the_adr091_counters() {
        crate::checkpoint::reset_checkpoint_metrics_for_tests();
        let dir = tempfile::tempdir().expect("tempdir");
        let (pool, _path) = seeded_pool(&dir);
        let conn = pool
            .open_standalone_writer_untracked()
            .expect("standalone probe connection");

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
        let reader_admission_capacity = pool.max_readers().max(1);

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
            "checkpoint_pressure_elevated_ticks",
            "checkpoint_pressure_episodes_started",
            "checkpoint_pressure_episodes_recovered",
            "checkpoint_lifecycle_append_attempts",
            "checkpoint_lifecycle_append_failures",
            "checkpoint_lifecycle_enqueue_drops",
            "read_tx_max_age_evictions",
        ] {
            assert!(counters.get(key).is_some(), "counter {key} must be present");
        }
        assert_eq!(
            report.writer_contention.writer_acquisitions, 1,
            "the seed write checked the finite-wait pooled writer out once"
        );
        assert_eq!(report.writer_contention.pooled_writer_acquisitions, 1);
        assert_eq!(report.writer_contention.standalone_writer_acquisitions, 0);
        assert_eq!(report.writer_contention.writer_task_acquisitions, 0);
        assert_eq!(report.writer_contention.writer_acquisition_timeouts, 0);
        assert_eq!(
            report.reader_contention,
            ReaderContentionDiagnostics {
                reader_admission_capacity,
                available_reader_admission_slots: reader_admission_capacity,
                reader_acquisitions: 0,
                pooled_reader_checkouts: 0,
                standalone_reader_opens: 0,
                infrastructure_standalone_reader_opens: 0,
                reader_checkout_timeouts: 0,
                active_pooled_reader_checkouts: 0,
                peak_active_pooled_reader_checkouts: 0,
                completed_pooled_reader_checkouts: 0,
                max_completed_reader_hold_micros: 0,
                reader_replacement_open_failures: 0,
            },
            "the diagnostics probe itself must not masquerade as request reader traffic"
        );
        assert_eq!(
            report.graph_edge_integrity,
            Some(GraphEdgeIntegrity {
                duplicate_edge_id_groups: 0,
                graph_edges_rows: 0,
                graph_edges_seq_rows: 0,
                pre_v14_duplicate_edge_state_detected: false,
            })
        );
        assert!(report.graph_edge_integrity_error.is_none());
        assert!(
            report.size_composition.is_some(),
            "file-backed diagnostics must include page composition; error was {:?}",
            report.size_composition_error
        );
        assert!(report.size_composition_error.is_none());
        assert!(report.writer_contention.audit_append_failures.is_none());
        assert!(
            report
                .writer_contention
                .audit_append_failures_unavailable_reason
                .is_some(),
            "a direct khive-db snapshot must not fabricate a runtime audit count"
        );
    }

    #[test]
    fn diagnostics_exposes_reader_saturation_and_completed_hold_evidence() {
        let pool = ConnectionPool::new(PoolConfig {
            checkout_timeout: Duration::from_millis(2),
            ..PoolConfig::default()
        })
        .expect("in-memory pool");
        let held = pool.reader().expect("first reader checkout");
        assert!(
            pool.reader().is_err(),
            "the live checkout must exhaust the one-slot degraded reader budget"
        );
        drop(held);

        let report = collect(
            &pool,
            BuildIdentity::from_env("9.9.9", None),
            Duration::from_secs(30),
        );
        let reader = report.reader_contention;
        assert_eq!(reader.reader_admission_capacity, 1);
        assert_eq!(reader.available_reader_admission_slots, 1);
        assert_eq!(reader.reader_acquisitions, 1);
        assert_eq!(reader.pooled_reader_checkouts, 1);
        assert_eq!(reader.standalone_reader_opens, 0);
        assert_eq!(reader.infrastructure_standalone_reader_opens, 0);
        assert_eq!(reader.reader_checkout_timeouts, 1);
        assert_eq!(reader.active_pooled_reader_checkouts, 0);
        assert_eq!(reader.peak_active_pooled_reader_checkouts, 1);
        assert_eq!(reader.completed_pooled_reader_checkouts, 1);
        assert!(reader.max_completed_reader_hold_micros > 0);

        let json = serde_json::to_value(&report).expect("report serializes");
        assert_eq!(
            json.pointer("/reader_contention/reader_admission_capacity"),
            Some(&serde_json::json!(1)),
            "the operator wire payload must expose the reader admission budget"
        );
        assert_eq!(
            json.pointer("/reader_contention/reader_checkout_timeouts"),
            Some(&serde_json::json!(1)),
            "the operator wire payload must expose the reader timeout phase"
        );
        assert!(
            json.pointer("/reader_contention/max_completed_reader_hold_micros")
                .is_some(),
            "the operator wire payload must expose completed hold-time evidence"
        );
    }

    #[test]
    fn diagnostics_composes_file_backed_standalone_acquisitions_without_counting_its_probe() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (pool, _path) = seeded_pool(&dir);

        drop(
            pool.open_standalone_writer()
                .expect("write-traffic standalone connection"),
        );

        let report = collect_with_audit_append_failures(
            &pool,
            BuildIdentity::from_env("9.9.9", None),
            Duration::from_secs(30),
            0,
        );
        assert_eq!(report.writer_contention.writer_acquisitions, 2);
        assert_eq!(report.writer_contention.pooled_writer_acquisitions, 1);
        assert_eq!(report.writer_contention.standalone_writer_acquisitions, 1);
        assert_eq!(report.writer_contention.writer_task_acquisitions, 0);
        assert_eq!(report.writer_contention.writer_acquisition_timeouts, 0);

        let second = collect_with_audit_append_failures(
            &pool,
            BuildIdentity::from_env("9.9.9", None),
            Duration::from_secs(30),
            0,
        );
        assert_eq!(
            second.writer_contention, report.writer_contention,
            "the diagnostics PASSIVE probe must not inflate write-traffic counters"
        );
    }

    #[test]
    fn runtime_aware_collect_exposes_the_supplied_audit_failure_counter() {
        let pool = ConnectionPool::new(PoolConfig::default()).expect("in-memory pool");

        let report = collect_with_audit_append_failures(
            &pool,
            BuildIdentity::from_env("9.9.9", None),
            Duration::from_secs(30),
            17,
        );

        assert_eq!(report.writer_contention.audit_append_failures, Some(17));
        assert!(
            report
                .writer_contention
                .audit_append_failures_unavailable_reason
                .is_none(),
            "a supplied runtime counter must not carry an unavailable reason"
        );
    }

    #[test]
    fn diagnostics_exposes_an_induced_writer_checkout_timeout() {
        let pool = ConnectionPool::new(PoolConfig {
            checkout_timeout: Duration::from_millis(1),
            ..PoolConfig::default()
        })
        .expect("in-memory pool");

        let held = pool.writer().expect("first writer checkout succeeds");
        assert!(
            matches!(
                pool.writer(),
                Err(crate::SqliteError::WriterPoolCheckoutTimeout { .. })
            ),
            "holding the sole writer must exercise the typed timeout path"
        );
        drop(held);

        let report = collect_with_audit_append_failures(
            &pool,
            BuildIdentity::from_env("9.9.9", None),
            Duration::from_secs(30),
            0,
        );
        assert_eq!(report.writer_contention.writer_acquisitions, 1);
        assert_eq!(report.writer_contention.pooled_writer_acquisitions, 1);
        assert_eq!(report.writer_contention.standalone_writer_acquisitions, 0);
        assert_eq!(report.writer_contention.writer_task_acquisitions, 0);
        assert_eq!(report.writer_contention.writer_acquisition_timeouts, 1);
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
            checkpoint_pressure_elevated_ticks: 0,
            checkpoint_pressure_episodes_started: 0,
            checkpoint_pressure_episodes_recovered: 0,
            checkpoint_lifecycle_append_attempts: 0,
            checkpoint_lifecycle_append_failures: 0,
            checkpoint_lifecycle_enqueue_drops: 0,
            read_tx_max_age_evictions: 0,
        };
        let json = serde_json::to_value(counters).expect("serializes");
        assert!(json["last_observed_wal_pages"].is_null());
        assert!(json["checkpoint_last_skip_wal_pages"].is_null());
    }

    #[test]
    fn graph_edge_integrity_detects_the_pre_v14_duplicate_state() {
        let conn = Connection::open_in_memory().expect("in-memory sqlite");
        conn.execute_batch(
            "CREATE TABLE graph_edges (
                 namespace TEXT NOT NULL,
                 id TEXT NOT NULL,
                 PRIMARY KEY (namespace, id)
             );
             CREATE TABLE graph_edges_seq (
                 seq INTEGER PRIMARY KEY AUTOINCREMENT,
                 edge_id TEXT NOT NULL UNIQUE
             );
             INSERT INTO graph_edges(namespace, id)
             VALUES ('alpha', 'shared-edge'), ('beta', 'shared-edge');
             INSERT INTO graph_edges_seq(edge_id) VALUES ('shared-edge');",
        )
        .expect("seed the state possible before the V14 uniqueness guard");

        let integrity = graph_edge_integrity(&conn).expect("integrity query succeeds");

        assert_eq!(integrity.duplicate_edge_id_groups, 1);
        assert_eq!(integrity.graph_edges_rows, 2);
        assert_eq!(integrity.graph_edges_seq_rows, 1);
        assert!(integrity.pre_v14_duplicate_edge_state_detected);
    }

    #[test]
    fn graph_edge_integrity_does_not_mislabel_retained_delete_history_as_a_duplicate() {
        let conn = Connection::open_in_memory().expect("in-memory sqlite");
        conn.execute_batch(
            "CREATE TABLE graph_edges (
                 namespace TEXT NOT NULL,
                 id TEXT NOT NULL,
                 PRIMARY KEY (namespace, id)
             );
             CREATE TABLE graph_edges_seq (
                 seq INTEGER PRIMARY KEY AUTOINCREMENT,
                 edge_id TEXT NOT NULL UNIQUE
             );
             INSERT INTO graph_edges(namespace, id) VALUES ('local', 'live-edge');
             INSERT INTO graph_edges_seq(edge_id)
             VALUES ('deleted-edge'), ('live-edge');",
        )
        .expect("seed a retained sequence row for a hard-deleted edge");

        let integrity = graph_edge_integrity(&conn).expect("integrity query succeeds");

        assert_eq!(integrity.duplicate_edge_id_groups, 0);
        assert_eq!(integrity.graph_edges_rows, 1);
        assert_eq!(integrity.graph_edges_seq_rows, 2);
        assert!(
            !integrity.pre_v14_duplicate_edge_state_detected,
            "ledger rows intentionally survive hard deletion; count mismatch alone is not the \
             pre-V14 duplicate state"
        );
    }

    #[cfg(unix)]
    #[test]
    fn unmeasured_sidecar_cleanup_fields_are_absent_from_the_wire_payload() {
        let pin = wal_pin_attribution_from_census(crate::walpin::CensusResult {
            holders: std::collections::HashSet::new(),
            uninspectable_pids: Vec::new(),
            truncated: false,
        });

        assert_eq!(pin.sidecar_listing_truncated, None);
        assert_eq!(pin.sidecar_entries_cleanup_would_reap, None);
        let json = serde_json::to_value(pin).expect("attribution serializes");
        assert!(
            json.get("sidecar_listing_truncated").is_none(),
            "a skipped enumeration must omit sidecar_listing_truncated, not fabricate false"
        );
        assert!(
            json.get("sidecar_entries_cleanup_would_reap").is_none(),
            "a skipped enumeration must omit sidecar_entries_cleanup_would_reap, not fabricate 0"
        );
    }

    #[cfg(unix)]
    #[test]
    fn wal_pin_census_serializes_only_under_the_nested_carrier() {
        let pin = wal_pin_attribution_from_census(crate::walpin::CensusResult {
            holders: std::collections::HashSet::from([41, 7]),
            uninspectable_pids: vec![99],
            truncated: true,
        });

        let json = serde_json::to_value(pin).expect("attribution serializes");
        assert_eq!(json["census"]["holder_pids"], serde_json::json!([7, 41]));
        assert_eq!(
            json["census"]["uninspectable_pids"],
            serde_json::json!([99])
        );
        assert_eq!(json["census"]["truncated"], true);

        for duplicate in [
            "census_holder_pids",
            "census_uninspectable_pids",
            "census_truncated",
            "census_is_complete",
        ] {
            assert!(
                json.get(duplicate).is_none(),
                "wal_pin.{duplicate} must not duplicate wal_pin.census: {json}"
            );
        }
    }

    /// A missing configured path must never be created by a diagnostic
    /// request. The untracked standalone open omits `SQLITE_OPEN_CREATE`, so
    /// the probe degrades to an error and the file stays absent.
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
        assert_eq!(report.wal_pin.status, WalPinAttributionStatus::Unavailable);
        assert!(matches!(
            report.wal_pin.census,
            WalPinCensus::Unavailable { .. }
        ));
        assert!(report.size_composition.is_none());
        assert!(report
            .size_composition_error
            .as_deref()
            .is_some_and(|reason| reason.contains("no file-backed page composition")));
    }

    #[cfg(unix)]
    #[test]
    fn incomplete_holder_census_is_a_tagged_degraded_result() {
        let census = crate::walpin::CensusResult {
            holders: std::collections::HashSet::from([41, 7]),
            uninspectable_pids: vec![99, 99],
            truncated: true,
        };

        let pin = wal_pin_attribution_from_census(census);

        assert_eq!(pin.status, WalPinAttributionStatus::Degraded);
        assert!(!pin.available);
        assert!(!pin.census_is_complete);
        assert_eq!(pin.census_holder_pids, vec![7, 41]);
        assert_eq!(pin.census_uninspectable_pids, vec![99]);
        assert!(
            pin.unavailable_reason.as_deref().is_some_and(
                |reason| reason.contains("additional database holders cannot be ruled out")
            ),
            "the legacy reason must also fail loud for old consumers: {pin:?}"
        );
        match &pin.census {
            WalPinCensus::Incomplete {
                holder_pids,
                uninspectable_pids,
                truncated,
                reason,
            } => {
                assert_eq!(holder_pids, &vec![7, 41]);
                assert_eq!(uninspectable_pids, &vec![99]);
                assert!(*truncated);
                assert!(reason.contains("additional database holders cannot be ruled out"));
            }
            other => panic!("incomplete scan must serialize as incomplete, got {other:?}"),
        }

        let json = serde_json::to_value(&pin).expect("serializes");
        assert_eq!(json["status"], "degraded");
        assert_eq!(json["census"]["status"], "incomplete");
    }

    #[cfg(unix)]
    #[test]
    fn complete_holder_census_stays_explicit_while_attribution_is_degraded() {
        let census = crate::walpin::CensusResult {
            holders: std::collections::HashSet::from([7]),
            uninspectable_pids: Vec::new(),
            truncated: false,
        };

        let pin = wal_pin_attribution_from_census(census);

        assert_eq!(pin.status, WalPinAttributionStatus::Degraded);
        assert!(pin.census_is_complete);
        assert!(matches!(
            pin.census,
            WalPinCensus::Complete { ref holder_pids } if holder_pids == &vec![7]
        ));
        assert_eq!(
            pin.status_reasons.len(),
            1,
            "only missing sidecar reconciliation degrades a complete OS census"
        );
    }

    #[cfg(unix)]
    #[test]
    fn complete_holder_and_read_only_sidecar_evidence_reconcile_to_complete() {
        let census = crate::walpin::CensusResult {
            holders: std::collections::HashSet::from([7]),
            uninspectable_pids: Vec::new(),
            truncated: false,
        };
        let sidecar = crate::walpin::WalpinReport {
            entries: vec![crate::walpin::WalpinPidHealth::RegisteredSilent { pid: 7 }],
            sidecar_listing_truncated: false,
            cleanup_would_reap: 0,
            orphan_temps_reaped: 0,
        };

        let pin = wal_pin_attribution_from_evidence(census, sidecar);

        assert_eq!(pin.status, WalPinAttributionStatus::Complete);
        assert!(pin.available);
        assert!(pin.fully_attributed);
        assert!(pin.status_reasons.is_empty());
        assert_eq!(pin.registered_silent_pids, vec![7]);
        assert!(pin.census_pids_without_attribution.is_empty());
        assert_eq!(pin.sidecar_listing_truncated, Some(false));
        assert_eq!(pin.sidecar_entries_cleanup_would_reap, Some(0));
    }

    #[cfg(unix)]
    #[test]
    fn complete_census_with_an_unregistered_holder_is_degraded_not_exonerated() {
        let census = crate::walpin::CensusResult {
            holders: std::collections::HashSet::from([7, 41]),
            uninspectable_pids: Vec::new(),
            truncated: false,
        };
        let sidecar = crate::walpin::WalpinReport {
            entries: vec![crate::walpin::WalpinPidHealth::RegisteredSilent { pid: 7 }],
            sidecar_listing_truncated: false,
            cleanup_would_reap: 0,
            orphan_temps_reaped: 0,
        };

        let pin = wal_pin_attribution_from_evidence(census, sidecar);

        assert_eq!(pin.status, WalPinAttributionStatus::Degraded);
        assert!(!pin.available);
        assert!(!pin.fully_attributed);
        assert_eq!(pin.census_pids_without_attribution, vec![41]);
        assert!(pin
            .status_reasons
            .iter()
            .any(|reason| reason.contains("holder(s) have no sidecar attribution")));
    }

    #[test]
    fn database_size_composition_reports_tables_indexes_fts_and_vectors_separately() {
        let conn = Connection::open_in_memory().expect("in-memory sqlite");
        conn.execute_batch(
            "CREATE TABLE docs(id INTEGER PRIMARY KEY, body TEXT NOT NULL); \
             CREATE INDEX idx_docs_body ON docs(body); \
             CREATE TABLE fts_demo_data(id INTEGER PRIMARY KEY, block BLOB); \
             CREATE TABLE vec_demo_chunks(id INTEGER PRIMARY KEY, vectors BLOB); \
             CREATE TABLE knowledge_sections(id INTEGER PRIMARY KEY, embedding BLOB); \
             INSERT INTO docs(body) VALUES (zeroblob(8192)); \
             INSERT INTO fts_demo_data(block) VALUES (zeroblob(8192)); \
             INSERT INTO vec_demo_chunks(vectors) VALUES (zeroblob(8192)); \
             INSERT INTO knowledge_sections(embedding) VALUES (zeroblob(8192));",
        )
        .expect("seed size classes");

        let composition = database_size_composition(&conn).expect("dbstat composition");
        let class_for = |name: &str| {
            composition
                .objects
                .iter()
                .find(|object| object.name == name)
                .map(|object| object.storage_class)
        };

        assert_eq!(class_for("docs"), Some(DatabaseStorageClass::RowTable));
        assert_eq!(
            class_for("idx_docs_body"),
            Some(DatabaseStorageClass::Index)
        );
        assert_eq!(
            class_for("fts_demo_data"),
            Some(DatabaseStorageClass::FullText)
        );
        assert_eq!(
            class_for("vec_demo_chunks"),
            Some(DatabaseStorageClass::Vector)
        );
        assert_eq!(
            class_for("knowledge_sections"),
            Some(DatabaseStorageClass::MixedRowAndEmbedding)
        );
        assert!(composition.vector_bytes > 0);
        assert!(composition.full_text_bytes > 0);
        assert!(composition.mixed_embedding_bytes > 0);
        assert_eq!(
            composition
                .accounted_bytes
                .saturating_add(composition.freelist_bytes)
                .saturating_add(composition.unaccounted_bytes),
            composition.database_bytes
        );
    }

    /// A holder with no sidecar registration remains degraded even though the
    /// read-only sidecar pass itself completed successfully.
    #[cfg(unix)]
    #[test]
    #[serial(khive_walpin_sidecar_env)]
    fn wal_pin_attribution_degrades_when_a_holder_has_no_sidecar_registration() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (pool, path) = seeded_pool(&dir);
        let _ = &pool;

        let pin = wal_pin_attribution(&path, Duration::from_secs(30));

        assert!(
            !pin.fully_attributed,
            "the pool's OS holder has no test sidecar registration"
        );
        assert!(
            pin.unavailable_reason.is_some(),
            "the missing holder attribution must be explained: {pin:?}"
        );
        assert!(pin.sidecar_entries.is_empty());
        assert!(pin.reporting.is_empty());
        assert_eq!(pin.sidecar_listing_truncated, Some(false));
        assert_eq!(pin.sidecar_entries_cleanup_would_reap, Some(0));
        assert_eq!(pin.status, WalPinAttributionStatus::Degraded);
        assert!(matches!(
            pin.census,
            WalPinCensus::Complete { .. } | WalPinCensus::Incomplete { .. }
        ));
    }

    /// `sidecar_dir_for` is a purely lexical derivation from whatever path it
    /// is handed (`pool.rs`'s `sidecar_dir_for_alias_convergence` proves
    /// this at the primitive level). Diagnostics must feed it
    /// `pool.canonical_path()` — the same value the checkpoint sidecar
    /// writers use — never the pool's raw configured path, or a symlinked
    /// database misses its own sidecar evidence entirely.
    #[cfg(unix)]
    #[test]
    #[serial(khive_walpin_sidecar_env)]
    fn diagnostics_finds_sidecar_evidence_through_an_aliased_database_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let real_dir = dir.path().join("real");
        std::fs::create_dir(&real_dir).expect("mkdir real dir");
        let real_path = real_dir.join("diag.db");
        std::fs::write(&real_path, b"").expect("create real file");
        let alias_path = dir.path().join("alias.db");
        std::os::unix::fs::symlink(&real_path, &alias_path).expect("symlink alias");

        let pool = ConnectionPool::new(PoolConfig {
            path: Some(alias_path.clone()),
            ..PoolConfig::default()
        })
        .expect("pool open through symlinked path");
        {
            let writer = pool.try_writer().expect("writer");
            writer
                .conn()
                .execute_batch("CREATE TABLE t (x INTEGER); INSERT INTO t VALUES (1);")
                .expect("seed a write so the WAL file exists");
        }

        let canonical = pool
            .canonical_path()
            .expect("file-backed pool")
            .to_path_buf();
        assert_ne!(
            canonical, alias_path,
            "the alias must actually differ from the canonical path for this test to mean \
             anything"
        );

        let pid = std::process::id();
        let sidecar_dir = crate::walpin::sidecar_dir_for(&canonical);
        let beacon = crate::walpin::WalpinBeacon {
            pid,
            process_role: "session".to_string(),
            started_at: crate::walpin::process_start_time_secs(pid).unwrap_or(0),
            sweep_interval_ms: 5_000,
        };
        crate::walpin::write_beacon(&sidecar_dir, &beacon).expect("seed this process's beacon");

        let report = collect(
            &pool,
            BuildIdentity::from_env("test", None),
            Duration::from_secs(30),
        );

        // The wider OS holder census can be legitimately incomplete in a
        // sandboxed test environment (other, unrelated processes this test
        // has no permission to inspect) — that variability is orthogonal to
        // what this test checks. What must hold regardless is that the
        // sidecar enumeration itself, which is keyed off the canonical
        // path, actually ran, completed untruncated, and found this
        // process's own beacon rather than missing it beside the wrong
        // (aliased) directory.
        assert_eq!(
            report.wal_pin.sidecar_listing_truncated,
            Some(false),
            "the sidecar enumeration must run to completion: {:?}",
            report.wal_pin
        );
        assert!(
            report.wal_pin.registered_silent_pids.contains(&pid),
            "the beacon written beside the canonical path must be found: {:?}",
            report.wal_pin
        );
        assert!(
            report.wal_pin.census_holder_pids.contains(&pid),
            "the OS census must find this process holding its own database open: {:?}",
            report.wal_pin
        );
        assert!(
            !report
                .wal_pin
                .census_pids_without_attribution
                .contains(&pid),
            "this process's own holder entry must be attributed by its own sidecar evidence, \
             not left unexplained: {:?}",
            report.wal_pin
        );
    }

    /// The async collector (`collect_with_audit_append_failures_interruptibly`)
    /// resolves its operational path independently of the sync collector
    /// (`collect`) — see `operational_db_path`. A regression that reintroduced
    /// the raw configured path on only the async side would leave the sync
    /// alias test above green while the async path silently missed its own
    /// sidecar evidence; this exercises the same aliasing scenario through
    /// the async entry point.
    #[cfg(unix)]
    #[tokio::test]
    #[serial(khive_walpin_sidecar_env)]
    async fn diagnostics_finds_sidecar_evidence_through_an_aliased_database_path_async() {
        let dir = tempfile::tempdir().expect("tempdir");
        let real_dir = dir.path().join("real");
        std::fs::create_dir(&real_dir).expect("mkdir real dir");
        let real_path = real_dir.join("diag.db");
        std::fs::write(&real_path, b"").expect("create real file");
        let alias_path = dir.path().join("alias.db");
        std::os::unix::fs::symlink(&real_path, &alias_path).expect("symlink alias");

        let pool = ConnectionPool::new(PoolConfig {
            path: Some(alias_path.clone()),
            ..PoolConfig::default()
        })
        .expect("pool open through symlinked path");
        {
            let writer = pool.try_writer().expect("writer");
            writer
                .conn()
                .execute_batch("CREATE TABLE t (x INTEGER); INSERT INTO t VALUES (1);")
                .expect("seed a write so the WAL file exists");
        }

        let canonical = pool
            .canonical_path()
            .expect("file-backed pool")
            .to_path_buf();
        assert_ne!(
            canonical, alias_path,
            "the alias must actually differ from the canonical path for this test to mean \
             anything"
        );

        let pid = std::process::id();
        let sidecar_dir = crate::walpin::sidecar_dir_for(&canonical);
        let beacon = crate::walpin::WalpinBeacon {
            pid,
            process_role: "session".to_string(),
            started_at: crate::walpin::process_start_time_secs(pid).unwrap_or(0),
            sweep_interval_ms: 5_000,
        };
        crate::walpin::write_beacon(&sidecar_dir, &beacon).expect("seed this process's beacon");

        let pool = Arc::new(pool);
        let report = collect_with_audit_append_failures_interruptibly(
            Arc::clone(&pool),
            BuildIdentity::from_env("test", None),
            Duration::from_secs(30),
            0,
        )
        .await
        .expect("diagnostics succeed");

        // Same rationale as the sync test: the wider OS holder census can be
        // legitimately incomplete in a sandboxed environment, but the sidecar
        // enumeration itself — keyed off the canonical path — must run to
        // completion and find this process's own beacon.
        assert_eq!(
            report.wal_pin.sidecar_listing_truncated,
            Some(false),
            "the sidecar enumeration must run to completion: {:?}",
            report.wal_pin
        );
        assert!(
            report.wal_pin.registered_silent_pids.contains(&pid),
            "the beacon written beside the canonical path must be found: {:?}",
            report.wal_pin
        );
        assert!(
            report.wal_pin.census_holder_pids.contains(&pid),
            "the OS census must find this process holding its own database open: {:?}",
            report.wal_pin
        );
        assert!(
            !report
                .wal_pin
                .census_pids_without_attribution
                .contains(&pid),
            "this process's own holder entry must be attributed by its own sidecar evidence, \
             not left unexplained: {:?}",
            report.wal_pin
        );
    }

    /// ADR-091 Amendment 6: an operator who explicitly disables the sidecar
    /// also disables its collection. Diagnostics must honor that rather than
    /// running `inspect_live` regardless of the operator's setting.
    #[cfg(unix)]
    #[test]
    #[serial(khive_walpin_sidecar_env)]
    fn wal_pin_attribution_reports_disabled_when_the_sidecar_is_explicitly_off() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (pool, path) = seeded_pool(&dir);
        let _ = &pool;
        let _env_guard = crate::walpin::EnvVarGuard::capture("KHIVE_WALPIN_SIDECAR");
        std::env::set_var("KHIVE_WALPIN_SIDECAR", "0");

        let pin = wal_pin_attribution(&path, Duration::from_secs(30));

        assert!(
            !pin.available,
            "an explicitly disabled sidecar can never produce a reconciled answer"
        );
        assert!(
            pin.unavailable_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("disabled")),
            "the reason must name the disabled sidecar, not a generic enumeration failure: \
             {pin:?}"
        );
        assert!(pin.sidecar_entries.is_empty());
        assert_eq!(pin.status, WalPinAttributionStatus::Degraded);
    }
}
