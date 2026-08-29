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
//! 3. WAL-pin attribution performs only the read-only OS holder census
//!    (`walpin::census_holders`) and does not inspect the sidecar directory.
//!    This tree's `khive-db` has no non-destructive reconciliation primitive:
//!    `walpin::enumerate_live` and `walpin::housekeep_live` both unlink
//!    entries under their respective cleanup policies. A diagnostic request
//!    must not be what destroys that forensic evidence, so
//!    `wal_pin_attribution` deliberately calls neither mutating path.
//!    Sidecar-to-holder reconciliation (`reporting`/`registered_silent_pids`/
//!    `sidecar_entries`/`fully_attributed`) is therefore reported empty with
//!    an explicit `unavailable_reason` rather than fabricated from a
//!    mutating enumeration. Cleanup-derived counters are omitted entirely,
//!    so a skipped measurement never looks like measured `0`/`false`.
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
/// remain available to Rust callers but are not serialized. This tree's
/// `khive-db` lacks a read-only sidecar enumeration primitive (see the module
/// docs), so the sidecar-derived fields below are always empty here.
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
    /// Present only when this request actually enumerated the sidecar.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sidecar_listing_truncated: Option<bool>,
    /// Present only when this request actually ran the mutating cleanup pass.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sidecar_entries_cleanup_would_reap: Option<usize>,
}

/// Overall quality of the WAL-pin attribution answer.
///
/// The current non-mutating diagnostics path can return `degraded` with a
/// useful OS census, but cannot claim `complete` while sidecar reconciliation
/// would require the mutating cleanup enumerator.
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

#[cfg(unix)]
fn wal_pin_attribution_from_census(census: crate::walpin::CensusResult) -> WalPinAttribution {
    const SIDECAR_REASON: &str = "sidecar-to-holder reconciliation not available: this tree's \
        khive-db has no non-destructive sidecar reconciliation primitive; walpin::enumerate_live \
        and walpin::housekeep_live both perform mutating cleanup, so diagnostics called neither \
        and collected only the read-only OS holder census below";

    let census_is_complete = census.is_complete();
    let mut census_holder_pids: Vec<u32> = census.holders.iter().copied().collect();
    census_holder_pids.sort_unstable();
    let mut census_uninspectable_pids = census.uninspectable_pids;
    census_uninspectable_pids.sort_unstable();
    census_uninspectable_pids.dedup();
    let census_truncated = census.truncated;

    let mut status_reasons = vec![SIDECAR_REASON.to_string()];
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
    wal_pin_attribution_from_census(census)
}

#[cfg(not(unix))]
pub fn wal_pin_attribution(_db_path: &Path, _sweep_interval: Duration) -> WalPinAttribution {
    WalPinAttribution::unavailable("WAL-pin attribution requires a Unix platform")
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

/// The full database-integrity, writer-contention, and WAL/checkpoint payload.
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
    /// Writer-pool and best-effort audit persistence signals.
    pub writer_contention: WriterContentionDiagnostics,
    pub graph_edge_integrity: Option<GraphEdgeIntegrity>,
    pub graph_edge_integrity_error: Option<String>,
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
            writer_contention,
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
    let (wal_file, wal_pin) =
        inspect_file_state_interruptibly(path.clone(), sweep_interval).await?;
    crate::ensure_request_read_active("db_diagnostics")?;

    Ok(DbDiagnostics {
        build,
        db_path: Some(path.display().to_string()),
        wal_file: Some(wal_file),
        checkpoint_counters: counters,
        checkpoint_probe: inspection.checkpoint_probe,
        checkpoint_probe_error: inspection.checkpoint_probe_error,
        writer_contention,
        graph_edge_integrity: inspection.graph_edge_integrity,
        graph_edge_integrity_error: inspection.graph_edge_integrity_error,
        wal_pin,
    })
}

fn collect_inner(
    pool: &ConnectionPool,
    build: BuildIdentity,
    sweep_interval: Duration,
    audit_append_failures: Option<u64>,
    runtime_audit_batch_metrics: Option<RuntimeAuditBatchMetrics>,
) -> DbDiagnostics {
    let counters = checkpoint_counters();
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
            writer_contention,
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

    DbDiagnostics {
        build,
        db_path: Some(path.display().to_string()),
        wal_file: Some(wal_file_state(&path)),
        checkpoint_counters: counters,
        checkpoint_probe: inspection.checkpoint_probe,
        checkpoint_probe_error: inspection.checkpoint_probe_error,
        writer_contention,
        graph_edge_integrity: inspection.graph_edge_integrity,
        graph_edge_integrity_error: inspection.graph_edge_integrity_error,
        wal_pin: wal_pin_attribution(&path, sweep_interval),
    }
}

struct PoolInspection {
    checkpoint_probe: Option<CheckpointProbe>,
    checkpoint_probe_error: Option<String>,
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

    // Preserve ordinary diagnostic degradation while allowing the outer
    // request cause to escape as a typed timeout.
    let integrity = scope.run(&conn, || Ok(graph_edge_integrity(&conn)))?;
    let (graph_edge_integrity, graph_edge_integrity_error) = match integrity {
        Ok(integrity) => (Some(integrity), None),
        Err(e) => (
            None,
            Some(format!("graph-edge integrity query failed: {e}")),
        ),
    };

    Ok(PoolInspection {
        checkpoint_probe,
        checkpoint_probe_error,
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
    _sweep_interval: Duration,
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
            Ok(census) => wal_pin_attribution_from_census(census),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => return Err(error),
            Err(error) => WalPinAttribution::unavailable(format!("census_holders failed: {error}")),
        };
        #[cfg(not(unix))]
        let attribution = wal_pin_attribution(&path, _sweep_interval);
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

    PoolInspection {
        checkpoint_probe,
        checkpoint_probe_error,
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
            report.graph_edge_integrity,
            Some(GraphEdgeIntegrity {
                duplicate_edge_id_groups: 0,
                graph_edges_rows: 0,
                graph_edges_seq_rows: 0,
                pre_v14_duplicate_edge_state_detected: false,
            })
        );
        assert!(report.graph_edge_integrity_error.is_none());
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
        assert_eq!(pin.status, WalPinAttributionStatus::Degraded);
        assert!(matches!(
            pin.census,
            WalPinCensus::Complete { .. } | WalPinCensus::Incomplete { .. }
        ));
    }
}
