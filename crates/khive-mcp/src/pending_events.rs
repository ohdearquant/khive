//! Scheduled event drain — `kkernel exec --pending-events` (one-shot) and the
//! daemon-resident tick (ADR-106, [`schedule_tick_loop`]).
//!
//! Scans all `scheduled_event` notes with `status="pending"` whose `trigger_at`
//! is at or before now, dispatches scheduled actions or delivers reminders to
//! their creating actors through `comm.send`, and durably records each action
//! outcome before finalizing the event lifecycle. Successful one-shots become
//! `"fired"`; failed one-shots retry only when the domain outcome permits it;
//! named repeats advance to their next occurrence. Events overdue by more than
//! the configured grace window are never dispatched, per the missed-event
//! policy below.
//!
//! Full design rationale (module placement, invocation-mode tradeoffs,
//! namespace-isolation and missed-event-policy background) lives in
//! `crates/khive-mcp/docs/pending-events.md`; the drain's API-level contract
//! rationale (the `rt`/`server` pair) lives in
//! `crates/khive-mcp/docs/api/pending-events.md`.
//!
//! ## Invocation modes
//!
//! - **One-shot** (`kkernel exec --pending-events`, cron-friendly): call
//!   [`run_pending_events`] directly.
//! - **Daemon-resident tick** (ADR-106): [`schedule_tick_loop`] calls
//!   [`run_pending_events_on`] on a fixed interval for the lifetime of the
//!   warm `khived` daemon process. Running both an external cron entry and
//!   the daemon tick at once is safe: the drain's `pending -> firing` CAS
//!   claim (`claim_pending_event`) makes concurrent or overlapping
//!   invocations harmless by construction — at most one caller ever wins a
//!   given row.
//!
//! ## Namespace isolation
//!
//! Each event fires in its own namespace, injected as the dispatched action's
//! `namespace=` parameter. Replay derives its actor from an immutable,
//! target-bound provenance event written by the schedule handler;
//! `created_by_actor` note metadata is never an authorization source. A
//! generic legacy row without provenance fails closed instead of inheriting
//! daemon authority.
//!
//! ## Repeat advancement
//!
//! One parser, `khive_pack_schedule::repeat`, decides what a `repeat` value
//! means for creation and for this executor:
//! - `"daily"`   → `trigger_at + 1 day`
//! - `"weekly"`  → `trigger_at + 7 days`
//! - `"monthly"` → `repeat_anchor` plus n calendar months, clamped per month
//! - `"every:<N><s|m|h|d>"` → `trigger_at + N units`
//! - a five-field cron expression → the next match after `trigger_at`, in UTC
//!
//! Unsupported repeat expressions are rejected at schedule creation and fail
//! closed for legacy rows rather than silently degrading to one-shot delivery.
//!
//! ## Missed-event policy (ADR-106 amendment)
//!
//! An event is "missed" when discovered overdue by more than
//! `KHIVE_FIRE_GRACE_SECS` (default 300s). A missed event is **never
//! dispatched** — it is marked `status="missed"` with `missed_at` stamped and
//! `fired_at` left null. A missed *repeating* event is re-armed at the next
//! occurrence strictly after now rather than firing a catch-up burst. The
//! creator-identity fence runs first for generic actions: an unattributed
//! legacy row becomes `failed`, not `missed`, even when stale.

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, FixedOffset, Utc};
use serde_json::{json, Value};

use crate::server::KhiveMcpServer;
use crate::tools::request::RequestParams;
use khive_runtime::{KhiveRuntime, Namespace, VerifiedActor};
use khive_storage::types::{SqlStatement, SqlValue};
use khive_types::{EventKind, EventOutcome, SubstrateKind};

mod dispatch;
mod drain;
mod receipt;
mod reclaim;
mod recurrence;

use dispatch::{
    action_error_disposition_may_have_committed, dispatch_with_renewable_lease,
    stored_action_is_non_single, verified_creator_for_event, DispatchLeaseTarget,
};
#[cfg(test)]
use dispatch::{action_failures, dispatch_action};

#[cfg(test)]
use drain::run_pending_events_on_with_lease;
pub use drain::{run_pending_events, run_pending_events_on, run_pending_events_with_config};

use receipt::{
    apply_final_disposition, claim_pending_event, completion_from_receipt,
    dispatch_error_property_keys, dispatch_occurrence_id, final_properties_after_dispatch,
    mark_dispatch_invoking, mark_dispatch_receipt_indeterminate, mark_recurrence_failure,
    mask_json_content, persist_dispatch_outcome, renew_dispatch_lease, validate_dispatch_receipt,
};

use reclaim::{
    check_fixed_path_whole_object_snapshot, current_note_properties_text,
    current_properties_for_finalize, expected_properties_value, finalize_fired_event,
    reclaim_stale_firing_events,
};
#[cfg(test)]
use reclaim::{finalize_corrupt_receipt, finalize_expired_firing_event, requeue_legacy_claim};

#[cfg(test)]
use recurrence::{
    advance_repeat_past_missed, next_trigger_at, reminder_subject, INVALID_MONTHLY_ANCHOR,
};
use recurrence::{
    advance_repeat_past_missed_for_event, append_reminder_delivery_failure_event,
    next_trigger_at_for_event, reminder_delivery_action, UNADVANCEABLE_REPEAT,
};

/// Default renewable dispatch-lease duration. A live invocation renews at one
/// third of this interval, so a slow handler is never reclaimed merely because
/// it runs for more than five minutes. A dead claimant becomes recoverable
/// after its last durable lease deadline passes.
const DEFAULT_DISPATCH_LEASE_SECS: u64 = 5 * 60;

/// Legacy rows claimed before renewable leases existed carry only
/// `firing_at`. Keep their historical five-minute reclaim threshold while new
/// rows use the explicit `lease_expires_at` deadline.
const LEGACY_STALE_FIRING_TIMEOUT_MICROS: i64 = 5 * 60 * 1_000_000;

const DISPATCH_RECEIPT_VERSION: u64 = 1;

#[derive(Clone, Copy, Debug)]
struct DispatchLeaseConfig {
    ttl: std::time::Duration,
    renew_every: std::time::Duration,
}

impl DispatchLeaseConfig {
    fn from_env() -> Self {
        let ttl_secs = std::env::var("KHIVE_SCHEDULE_LEASE_SECS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(DEFAULT_DISPATCH_LEASE_SECS);
        let ttl = std::time::Duration::from_secs(ttl_secs);
        let renew_micros = (ttl.as_micros() / 3).max(1).min(u128::from(u64::MAX));
        Self {
            ttl,
            renew_every: std::time::Duration::from_micros(renew_micros as u64),
        }
    }

    fn expires_at(self, now_micros: i64) -> i64 {
        let ttl_micros = i64::try_from(self.ttl.as_micros()).unwrap_or(i64::MAX);
        now_micros.saturating_add(ttl_micros)
    }
}

#[derive(Clone, Debug)]
struct DispatchClaim {
    firing_at: i64,
    occurrence_id: uuid::Uuid,
    invocation_id: uuid::Uuid,
    actor: String,
}

#[derive(Clone, Copy, Debug)]
struct RecoverySnapshot<'a> {
    expired_at: i64,
    properties: &'a str,
}

impl DispatchClaim {
    fn claimed_receipt(&self) -> Value {
        json!({
            "version": DISPATCH_RECEIPT_VERSION,
            "occurrence_id": self.occurrence_id,
            "invocation_id": self.invocation_id,
            "actor": self.actor.as_str(),
            "state": DispatchReceiptState::Claimed.as_str(),
            "claimed_at": self.firing_at,
        })
    }

    fn completed_without_invocation_receipt(
        &self,
        state: DispatchReceiptState,
        completed_at: i64,
        error: Option<&str>,
    ) -> Value {
        debug_assert!(matches!(
            state,
            DispatchReceiptState::NotInvoked | DispatchReceiptState::Missed
        ));
        json!({
            "version": DISPATCH_RECEIPT_VERSION,
            "occurrence_id": self.occurrence_id,
            "invocation_id": self.invocation_id,
            "actor": self.actor.as_str(),
            "state": state.as_str(),
            "claimed_at": self.firing_at,
            "completed_at": completed_at,
            "error": error,
            "error_payload": null,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DispatchReceiptState {
    Claimed,
    Invoking,
    Succeeded,
    Failed,
    Indeterminate,
    NotInvoked,
    Missed,
}

impl DispatchReceiptState {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "claimed" => Some(Self::Claimed),
            "invoking" => Some(Self::Invoking),
            "succeeded" => Some(Self::Succeeded),
            "failed" => Some(Self::Failed),
            "indeterminate" => Some(Self::Indeterminate),
            "not_invoked" => Some(Self::NotInvoked),
            "missed" => Some(Self::Missed),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Claimed => "claimed",
            Self::Invoking => "invoking",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Indeterminate => "indeterminate",
            Self::NotInvoked => "not_invoked",
            Self::Missed => "missed",
        }
    }
}

struct ValidatedDispatchReceipt {
    value: Value,
    occurrence_id: uuid::Uuid,
    invocation_id: uuid::Uuid,
    actor: String,
    state: DispatchReceiptState,
}

/// CancellationToken itself does not cancel when its last handle is dropped.
/// Keep this guard in the dispatch future so aborting/dropping that future
/// cannot detach a lease-renewal task that would keep an abandoned claim alive
/// forever.
struct CancelOnDrop(tokio_util::sync::CancellationToken);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

#[derive(Debug)]
enum DispatchCompletion {
    Succeeded,
    Failed(DispatchFailure),
    Indeterminate(DispatchFailure),
}

#[derive(Clone, Debug)]
struct DispatchFailure {
    message: String,
    /// Original structured per-op error payload, when the handler returned
    /// one. Keeping it alongside the human-readable message preserves
    /// correlation values such as `comm.send`'s `details.outbound_id` for
    /// durable reconciliation.
    payload: Option<Value>,
}

impl DispatchFailure {
    fn plain(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            payload: None,
        }
    }

    fn with_payload(message: impl Into<String>, payload: Value) -> Self {
        Self {
            message: message.into(),
            payload: Some(payload),
        }
    }

    fn as_str(&self) -> &str {
        &self.message
    }
}

impl std::fmt::Display for DispatchFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

#[derive(Debug)]
struct DispatchActionError {
    failure: DispatchFailure,
    outcome_uncertain: bool,
    /// The only uncertainty is a domain disposition on a per-op error. Defer
    /// its replay decision until finalization reads the current repeat value:
    /// a repeat advances, while a one-shot must end indeterminate.
    disposition_only_uncertain: bool,
}

impl std::fmt::Display for DispatchActionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.failure.fmt(formatter)
    }
}

impl DispatchActionError {
    fn known(failure: DispatchFailure) -> Self {
        Self {
            failure,
            outcome_uncertain: false,
            disposition_only_uncertain: false,
        }
    }

    fn uncertain(failure: DispatchFailure) -> Self {
        Self {
            failure,
            outcome_uncertain: true,
            disposition_only_uncertain: false,
        }
    }

    fn disposition_uncertain(failure: DispatchFailure) -> Self {
        Self {
            failure,
            outcome_uncertain: true,
            disposition_only_uncertain: true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FinalDisposition {
    Fired,
    Advanced,
    RetryPending,
    Indeterminate,
    RecurrenceFailed,
}

#[derive(Debug, Default)]
struct ReclaimSummary {
    rows: u64,
    outcomes_persisted: u64,
    fired: u64,
    advanced: u64,
    retry_pending: u64,
    indeterminate: u64,
    finalized: u64,
    failed: u64,
}

/// Default grace window (seconds): an event discovered overdue by more than
/// this is "missed" rather than fired late. Overridable via
/// `KHIVE_FIRE_GRACE_SECS`. See the module-level "Missed-event policy" docs.
const DEFAULT_FIRE_GRACE_SECS: i64 = 300;

/// Resolve the missed-event grace window from `KHIVE_FIRE_GRACE_SECS`,
/// falling back to [`DEFAULT_FIRE_GRACE_SECS`] when unset or unparseable as a
/// non-negative integer.
fn fire_grace_from_env() -> Duration {
    let secs = std::env::var("KHIVE_FIRE_GRACE_SECS")
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
        .filter(|&s| s >= 0)
        .unwrap_or(DEFAULT_FIRE_GRACE_SECS);
    Duration::seconds(secs)
}

/// Summary of a single drain run.
#[derive(Debug, Default)]
pub struct DrainSummary {
    pub scanned: u64,
    /// Dispatch futures entered during this pass. This is intentionally
    /// separate from lifecycle finalization and durable outcome persistence.
    pub invoked: u64,
    /// Invocation outcomes durably written to the scheduled-event receipt,
    /// including crash-recovery classifications produced in this pass.
    pub outcomes_persisted: u64,
    /// Successful claim-bound lifecycle finalizations in this pass.
    pub finalized: u64,
    pub fired: u64,
    pub advanced: u64,
    pub failed: u64,
    /// Failed one-shot occurrences returned to `pending` for a later retry.
    pub retry_pending: u64,
    /// Expired invocations whose durable receipt cannot prove an outcome.
    pub indeterminate: u64,
    pub skipped_not_due: u64,
    pub skipped_race: u64,
    pub reclaimed: u64,
    /// IDs of `scheduled_event` notes marked `"missed"` (or re-armed past a
    /// missed occurrence) this pass — never dispatched. See the module-level
    /// "Missed-event policy" docs.
    pub missed: Vec<uuid::Uuid>,
}

/// Discover all distinct namespaces that have at least one pending, due
/// `scheduled_event` note (i.e. `status="pending"` AND `trigger_at <= now`).
/// The `trigger_at` comparison uses SQLite's `datetime(...)` rather than a
/// raw string comparison, since stored offsets are not normalized to UTC;
/// the Rust layer downstream re-checks each candidate with `DateTime<Utc>`
/// as the final authority. See `crates/khive-mcp/docs/api/pending-events.md`.
async fn discover_pending_namespaces(rt: &KhiveRuntime, now: DateTime<Utc>) -> Result<Vec<String>> {
    use khive_storage::types::{SqlStatement, SqlValue};

    let sql_access = rt.sql();
    let mut reader = sql_access
        .reader()
        .await
        .context("pending-events: open SQL reader")?;

    // This is a pre-filter gate for the per-namespace candidate scan below,
    // not the final due-ness decision — but a namespace excluded HERE never
    // reaches that scan, so it is held to the same `datetime(...)`
    // normalization and NULL-safety as the candidate-page queries. See
    // "Keyset pagination and due-ness comparison" in
    // `crates/khive-mcp/docs/pending-events.md`.
    let now_rfc = now.to_rfc3339();
    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT DISTINCT namespace \
                  FROM notes \
                  WHERE kind = 'scheduled_event' \
                    AND deleted_at IS NULL \
                    AND json_extract(properties, '$.status') = 'pending' \
                    AND ( \
                      datetime(json_extract(properties, '$.trigger_at')) <= datetime(?1) \
                      OR datetime(json_extract(properties, '$.trigger_at')) IS NULL \
                    )"
            .into(),
            params: vec![SqlValue::Text(now_rfc)],
            label: Some("pending_events_namespaces".into()),
        })
        .await
        .context("pending-events: discover namespaces query")?;

    let namespaces: Vec<String> = rows
        .into_iter()
        .filter_map(|row| {
            row.get("namespace").and_then(|v| {
                if let SqlValue::Text(s) = v {
                    Some(s.clone())
                } else {
                    None
                }
            })
        })
        .collect();

    Ok(namespaces)
}

/// Print the drain summary to stdout as JSON.
pub fn print_summary(summary: &DrainSummary) {
    let json = json!({
        "scanned": summary.scanned,
        "invoked": summary.invoked,
        "outcomes_persisted": summary.outcomes_persisted,
        "finalized": summary.finalized,
        "fired": summary.fired,
        "advanced": summary.advanced,
        "failed": summary.failed,
        "retry_pending": summary.retry_pending,
        "indeterminate": summary.indeterminate,
        "skipped_not_due": summary.skipped_not_due,
        "skipped_race": summary.skipped_race,
        "reclaimed": summary.reclaimed,
        "missed_count": summary.missed.len(),
        "missed_ids": summary.missed.iter().map(uuid::Uuid::to_string).collect::<Vec<_>>(),
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&json).expect("serialize")
    );
}

/// Default interval between daemon-resident schedule ticks, in seconds.
/// Matches the cadence the module doc already documents for the external-cron
/// invocation (`* * * * * kkernel exec --pending-events` is minute-grain;
/// 60s is the same order of magnitude for the in-daemon tick).
const DEFAULT_TICK_INTERVAL_SECS: u64 = 60;

/// Resolve the daemon tick interval from `KHIVE_SCHEDULE_TICK_SECS`, falling
/// back to `DEFAULT_TICK_INTERVAL_SECS` (60s) when unset or not a positive
/// integer.
pub fn tick_interval_from_env() -> std::time::Duration {
    let secs = std::env::var("KHIVE_SCHEDULE_TICK_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&s| s > 0)
        .unwrap_or(DEFAULT_TICK_INTERVAL_SECS);
    std::time::Duration::from_secs(secs)
}

/// Daemon-resident periodic drain loop (ADR-106).
///
/// Runs [`run_pending_events_on`] on `interval` for as long as the daemon
/// process lives; only the daemon role spawns this loop. `rt` MUST be the
/// daemon's own already-resolved runtime handle for the `"schedule"` pack.
/// The host context carries the daemon's live [`KhiveMcpServer`] — never a
/// freshly reconstructed server — or replayed actions can silently dispatch
/// against the wrong backend. Ticks on a fixed interval with
/// `Skip`-missed-tick behavior so a long drain cannot make the loop drift
/// behind. Drain-level failures are retryable component failures; individual
/// event failures remain part of a successful drain summary and do not spend
/// the supervisor's restart budget.
/// See `crates/khive-mcp/docs/api/pending-events.md` for the full rationale.
pub async fn schedule_tick_loop(
    rt: KhiveRuntime,
    ctx: crate::components::HostContext,
    interval: std::time::Duration,
) -> Result<(), crate::components::ComponentError> {
    let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + interval, interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = ctx.cancellation().cancelled() => return Ok(()),
            _ = ticker.tick() => {}
        }
        ctx.server().record_schedule_ticker_tick();
        match run_pending_events_on(&rt, ctx.server(), false).await {
            Ok(summary) => {
                ctx.heartbeat();
                if summary.fired > 0
                    || summary.advanced > 0
                    || summary.failed > 0
                    || !summary.missed.is_empty()
                {
                    tracing::info!(
                        scanned = summary.scanned,
                        invoked = summary.invoked,
                        outcomes_persisted = summary.outcomes_persisted,
                        finalized = summary.finalized,
                        fired = summary.fired,
                        advanced = summary.advanced,
                        retry_pending = summary.retry_pending,
                        indeterminate = summary.indeterminate,
                        missed = summary.missed.len(),
                        failed = summary.failed,
                        reclaimed = summary.reclaimed,
                        "schedule tick: drain pass complete"
                    );
                }
            }
            Err(e) => {
                return Err(crate::components::ComponentError::Retryable(format!(
                    "schedule drain pass failed: {e}"
                )));
            }
        }
    }
}

/// Test-only pause points inside a drain iteration, so a concurrent property
/// write landing in one of its races can be reproduced deterministically
/// instead of relying on scheduler luck or sleeps. There are two, and they
/// bracket different windows: `pause_before_claim` parks between the page-query
/// snapshot (`properties`) and the CAS claim, and `pause_before_finalize_read`
/// parks after claim and dispatch and immediately before the finalizer's fresh
/// current-properties read. Each is a
/// no-op unless the calling task runs inside `PAUSE_GATE.scope(...)`;
/// production code never establishes that scope, so this costs nothing
/// outside these regression tests, and it does not exist at all in
/// non-test builds. Mirrors `khive-runtime::curation::race_seam`.
#[cfg(test)]
#[path = "pending_events/race_seam_tests.rs"]
pub(crate) mod race_seam;

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "pending_events_tests.rs"]
mod tests;
