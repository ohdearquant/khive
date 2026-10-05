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

mod drain;
mod receipt;

#[cfg(test)]
use drain::run_pending_events_on_with_lease;
pub use drain::{run_pending_events, run_pending_events_on, run_pending_events_with_config};

use receipt::{
    apply_final_disposition, claim_pending_event, completion_from_receipt,
    dispatch_error_property_keys, dispatch_occurrence_id, final_properties_after_dispatch,
    mark_dispatch_invoking, mark_dispatch_receipt_indeterminate, mark_recurrence_failure,
    mask_json_content, persist_dispatch_outcome, renew_dispatch_lease, validate_dispatch_receipt,
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

async fn requeue_legacy_claim(
    rt: &KhiveRuntime,
    namespace: &str,
    id: uuid::Uuid,
    firing_at: i64,
    selected_properties: &str,
) -> Result<bool> {
    let updated_at = Utc::now().timestamp_micros();
    check_fixed_path_whole_object_snapshot(selected_properties)?;
    let mut writer = rt
        .sql()
        .writer()
        .await
        .context("pending-events: open SQL writer for legacy reclaim")?;
    let rows = writer
        .execute(SqlStatement {
            sql: "UPDATE notes \
                  SET properties = json_remove( \
                        json_set(properties, '$.status', 'pending'), \
                        '$.firing_at', '$.lease_expires_at' \
                      ), \
                      updated_at = ?1 \
                  WHERE id = ?2 \
                    AND namespace = ?3 \
                    AND kind = 'scheduled_event' \
                    AND deleted_at IS NULL \
                    AND json_extract(properties, '$.status') = 'firing' \
                    AND (json_extract(properties, '$.firing_at') IS NULL \
                         OR CAST(json_extract(properties, '$.firing_at') AS INTEGER) = ?4) \
                    AND json_extract(properties, '$.dispatch_receipt') IS NULL \
                    AND properties = ?5"
                .to_string(),
            params: vec![
                SqlValue::Integer(updated_at),
                SqlValue::Text(id.to_string()),
                SqlValue::Text(namespace.to_string()),
                SqlValue::Integer(firing_at),
                SqlValue::Text(selected_properties.to_string()),
            ],
            label: Some("pending_events_requeue_legacy_claim".into()),
        })
        .await
        .context("pending-events: requeue legacy firing claim")?;
    Ok(rows == 1)
}

async fn finalize_corrupt_receipt(
    rt: &KhiveRuntime,
    namespace: &str,
    id: uuid::Uuid,
    firing_at: i64,
    properties: &Value,
    expired_at: i64,
    selected_properties: &str,
) -> Result<bool> {
    let mut properties = properties.clone();
    if let Some(object) = properties.as_object_mut() {
        object.remove("firing_at");
        object.remove("lease_expires_at");
    }
    khive_runtime::secret_gate::reject_reserved_secret_gate_property(Some(&properties))?;
    let serialized = serde_json::to_string(&properties)
        .context("pending-events: serialize corrupt receipt failure state")?;
    let updated_at = Utc::now().timestamp_micros();
    let mut writer = rt
        .sql()
        .writer()
        .await
        .context("pending-events: open SQL writer for corrupt receipt")?;
    let rows = writer
        .execute(SqlStatement {
            sql: "UPDATE notes SET properties = ?1, updated_at = ?2 \
                  WHERE id = ?3 \
                    AND namespace = ?4 \
                    AND kind = 'scheduled_event' \
                    AND deleted_at IS NULL \
                    AND json_extract(properties, '$.status') = 'firing' \
                    AND (json_extract(properties, '$.firing_at') IS NULL \
                         OR CAST(json_extract(properties, '$.firing_at') AS INTEGER) = ?5) \
                    AND ( \
                      (json_extract(properties, '$.lease_expires_at') IS NOT NULL \
                       AND CAST(json_extract(properties, '$.lease_expires_at') AS INTEGER) <= ?6) \
                      OR \
                      (json_extract(properties, '$.lease_expires_at') IS NULL \
                       AND (json_extract(properties, '$.firing_at') IS NULL \
                            OR CAST(json_extract(properties, '$.firing_at') AS INTEGER) < ?7)) \
                    ) \
                    AND properties = ?8"
                .to_string(),
            params: vec![
                SqlValue::Text(serialized),
                SqlValue::Integer(updated_at),
                SqlValue::Text(id.to_string()),
                SqlValue::Text(namespace.to_string()),
                SqlValue::Integer(firing_at),
                SqlValue::Integer(expired_at),
                SqlValue::Integer(expired_at.saturating_sub(LEGACY_STALE_FIRING_TIMEOUT_MICROS)),
                SqlValue::Text(selected_properties.to_string()),
            ],
            label: Some("pending_events_finalize_corrupt_receipt".into()),
        })
        .await
        .context("pending-events: finalize corrupt dispatch receipt")?;
    Ok(rows == 1)
}

/// Reconcile expired firing leases without blindly replaying an invocation.
/// A receipt with a durable outcome resumes finalization; `claimed` is safe to
/// retry because invocation never began; `invoking` is terminally
/// indeterminate because generic verb dispatch cannot prove whether its side
/// effect committed before the claimant disappeared.
async fn reclaim_stale_firing_events(rt: &KhiveRuntime, now_micros: i64) -> Result<ReclaimSummary> {
    let legacy_stale_before = now_micros.saturating_sub(LEGACY_STALE_FIRING_TIMEOUT_MICROS);
    let rows = {
        let mut reader = rt
            .sql()
            .reader()
            .await
            .context("pending-events: open SQL reader for expired leases")?;
        reader
            .query_all(SqlStatement {
                sql: "SELECT id, namespace, properties FROM notes \
                      WHERE kind = 'scheduled_event' \
                        AND deleted_at IS NULL \
                        AND json_extract(properties, '$.status') = 'firing' \
                        AND ( \
                          (json_extract(properties, '$.lease_expires_at') IS NOT NULL \
                           AND CAST(json_extract(properties, '$.lease_expires_at') AS INTEGER) <= ?1) \
                          OR \
                          (json_extract(properties, '$.lease_expires_at') IS NULL \
                           AND (json_extract(properties, '$.firing_at') IS NULL \
                                OR CAST(json_extract(properties, '$.firing_at') AS INTEGER) < ?2)) \
                        ) \
                      ORDER BY created_at ASC, id ASC"
                    .to_string(),
                params: vec![
                    SqlValue::Integer(now_micros),
                    SqlValue::Integer(legacy_stale_before),
                ],
                label: Some("pending_events_expired_dispatch_leases".into()),
            })
            .await
            .context("pending-events: query expired dispatch leases")?
    };

    let mut summary = ReclaimSummary::default();
    for row in rows {
        let id = match row.get("id") {
            Some(SqlValue::Text(value)) => uuid::Uuid::parse_str(value)
                .with_context(|| format!("pending-events: invalid stale event id {value:?}"))?,
            other => {
                return Err(anyhow::anyhow!(
                    "pending-events: expired lease has invalid id column {other:?}"
                ));
            }
        };
        let namespace = match row.get("namespace") {
            Some(SqlValue::Text(value)) => value.clone(),
            other => {
                return Err(anyhow::anyhow!(
                    "pending-events: expired lease {id} has invalid namespace {other:?}"
                ));
            }
        };
        let selected_properties = match row.get("properties") {
            Some(SqlValue::Text(value)) => value.clone(),
            other => {
                return Err(anyhow::anyhow!(
                    "pending-events: expired lease {id} has invalid properties {other:?}"
                ));
            }
        };
        let mut properties: Value = serde_json::from_str(&selected_properties)
            .with_context(|| format!("pending-events: parse expired receipt for {id}"))?;
        let firing_at = properties
            .get("firing_at")
            .and_then(Value::as_i64)
            .unwrap_or_default();
        let Some(receipt) = properties.get("dispatch_receipt").cloned() else {
            match requeue_legacy_claim(rt, &namespace, id, firing_at, &selected_properties).await {
                Ok(true) => {
                    summary.rows += 1;
                    summary.retry_pending += 1;
                    summary.finalized += 1;
                }
                Ok(false) => {}
                Err(error) => {
                    tracing::error!(
                        scheduled_event_id = %id,
                        namespace,
                        error = %error,
                        "pending-events: legacy expired-claim recovery failed; continuing"
                    );
                    summary.failed += 1;
                }
            }
            continue;
        };

        let validated = match validate_dispatch_receipt(id, firing_at, &properties, receipt) {
            Ok(validated) => validated,
            Err(validation_error) => {
                let error = format!("{validation_error}; refusing automatic replay");
                let invalid_receipt = properties
                    .get("dispatch_receipt")
                    .cloned()
                    .unwrap_or(Value::Null);
                mark_dispatch_receipt_indeterminate(
                    &mut properties,
                    invalid_receipt,
                    &error,
                    now_micros,
                );
                match finalize_corrupt_receipt(
                    rt,
                    &namespace,
                    id,
                    firing_at,
                    &properties,
                    now_micros,
                    &selected_properties,
                )
                .await
                {
                    Ok(true) => {
                        summary.rows += 1;
                        summary.indeterminate += 1;
                        summary.outcomes_persisted += 1;
                        summary.finalized += 1;
                        summary.failed += 1;
                    }
                    Ok(false) => {}
                    Err(error) => {
                        tracing::error!(
                            scheduled_event_id = %id,
                            namespace,
                            error = %error,
                            "pending-events: corrupt-receipt quarantine failed; continuing"
                        );
                        summary.failed += 1;
                    }
                }
                continue;
            }
        };
        let ValidatedDispatchReceipt {
            value: mut receipt,
            occurrence_id,
            invocation_id,
            actor,
            state,
        } = validated;
        let claim = DispatchClaim {
            firing_at,
            occurrence_id,
            invocation_id,
            actor,
        };
        if state == DispatchReceiptState::Claimed {
            let error =
                "dispatch claimant expired before invocation began; occurrence is retryable";
            receipt["state"] = json!(DispatchReceiptState::NotInvoked.as_str());
            receipt["completed_at"] = json!(now_micros);
            receipt["error"] = json!(error);
            receipt["error_payload"] = Value::Null;
            properties["dispatch_receipt"] = receipt;
            properties["status"] = json!("pending");
            match finalize_expired_firing_event(
                rt,
                &namespace,
                id,
                &properties,
                Utc::now().timestamp_micros(),
                &claim,
                RecoverySnapshot {
                    expired_at: now_micros,
                    properties: &selected_properties,
                },
            )
            .await
            {
                Ok(true) => {
                    summary.rows += 1;
                    summary.retry_pending += 1;
                    summary.finalized += 1;
                }
                Ok(false) => {}
                Err(error) => {
                    tracing::error!(
                        scheduled_event_id = %id,
                        namespace,
                        error = %error,
                        "pending-events: pre-invocation expired-claim recovery failed; continuing"
                    );
                    summary.failed += 1;
                }
            }
            continue;
        }

        if matches!(
            state,
            DispatchReceiptState::NotInvoked | DispatchReceiptState::Missed
        ) {
            let error = format!(
                "completed dispatch receipt state {} cannot remain attached to a firing row; \
                 refusing automatic replay",
                state.as_str()
            );
            mark_dispatch_receipt_indeterminate(&mut properties, receipt, &error, now_micros);
            match finalize_corrupt_receipt(
                rt,
                &namespace,
                id,
                firing_at,
                &properties,
                now_micros,
                &selected_properties,
            )
            .await
            {
                Ok(true) => {
                    summary.rows += 1;
                    summary.indeterminate += 1;
                    summary.outcomes_persisted += 1;
                    summary.finalized += 1;
                    summary.failed += 1;
                }
                Ok(false) => {}
                Err(error) => {
                    tracing::error!(
                        scheduled_event_id = %id,
                        namespace,
                        error = %error,
                        "pending-events: completed pre-invocation receipt quarantine failed; continuing"
                    );
                    summary.failed += 1;
                }
            }
            continue;
        }

        let recovery_persisted_outcome = match state {
            DispatchReceiptState::Invoking => {
                let completion = completion_from_receipt(&receipt);
                receipt["state"] = json!(DispatchReceiptState::Indeterminate.as_str());
                receipt["completed_at"] = json!(now_micros);
                receipt["error"] = json!(match &completion {
                    DispatchCompletion::Indeterminate(error) => error.as_str(),
                    DispatchCompletion::Succeeded => "",
                    DispatchCompletion::Failed(error) => error.as_str(),
                });
                receipt["error_payload"] = Value::Null;
                true
            }
            DispatchReceiptState::Succeeded
            | DispatchReceiptState::Failed
            | DispatchReceiptState::Indeterminate => false,
            DispatchReceiptState::Claimed
            | DispatchReceiptState::NotInvoked
            | DispatchReceiptState::Missed => unreachable!("states handled above"),
        };
        let completion = completion_from_receipt(&receipt);
        let trigger_at_fixed = properties
            .get("trigger_at")
            .and_then(Value::as_str)
            .and_then(|value| value.parse::<DateTime<FixedOffset>>().ok());
        let repeat = properties
            .get("repeat")
            .and_then(Value::as_str)
            .map(str::to_string);
        let (final_properties, disposition) = match trigger_at_fixed {
            Some(trigger_at_fixed) => final_properties_after_dispatch(
                properties,
                receipt,
                &completion,
                trigger_at_fixed.with_timezone(&Utc),
                *trigger_at_fixed.offset(),
                &repeat,
            ),
            None => {
                properties["dispatch_receipt"] = receipt;
                properties["status"] = json!("failed");
                let (error_key, error_at_key) = dispatch_error_property_keys(&properties);
                properties[error_key] =
                    json!("cannot recover dispatch outcome: trigger_at is invalid");
                properties[error_at_key] = json!(Utc::now().to_rfc3339());
                (properties, FinalDisposition::Indeterminate)
            }
        };
        if disposition == FinalDisposition::RecurrenceFailed {
            tracing::error!(
                scheduled_event_id = %id,
                namespace,
                error = %final_properties["recurrence_error"].as_str().unwrap_or(UNADVANCEABLE_REPEAT),
                "pending-events: recurrence advancement failed during expired-claim recovery"
            );
        }
        match finalize_expired_firing_event(
            rt,
            &namespace,
            id,
            &final_properties,
            Utc::now().timestamp_micros(),
            &claim,
            RecoverySnapshot {
                expired_at: now_micros,
                properties: &selected_properties,
            },
        )
        .await
        {
            Ok(true) => {
                summary.rows += 1;
                if recovery_persisted_outcome {
                    summary.outcomes_persisted += 1;
                }
                summary.finalized += 1;
                match disposition {
                    FinalDisposition::Fired => summary.fired += 1,
                    FinalDisposition::Advanced => summary.advanced += 1,
                    FinalDisposition::RetryPending => summary.retry_pending += 1,
                    FinalDisposition::Indeterminate => summary.indeterminate += 1,
                    FinalDisposition::RecurrenceFailed => {}
                }
                if disposition == FinalDisposition::RecurrenceFailed
                    || !matches!(completion, DispatchCompletion::Succeeded)
                {
                    summary.failed += 1;
                }
            }
            Ok(false) => {}
            Err(error) => {
                tracing::error!(
                    scheduled_event_id = %id,
                    namespace,
                    error = %error,
                    "pending-events: expired dispatch outcome finalization failed; continuing"
                );
                summary.failed += 1;
            }
        }
    }
    Ok(summary)
}

/// Read a `scheduled_event` note's CURRENT `properties` column, verbatim as
/// stored (no round-trip through `serde_json` re-serialization), so a caller
/// can use the exact byte string as an exact-equality CAS guard on a later
/// write. Returns `Ok(None)` when the row is absent, soft-deleted, or no
/// longer a `scheduled_event` note.
async fn current_note_properties_text(
    rt: &KhiveRuntime,
    namespace: &str,
    id: uuid::Uuid,
) -> Result<Option<String>> {
    let mut reader = rt
        .sql()
        .reader()
        .await
        .map_err(|e| anyhow::anyhow!("pending-events: open SQL reader: {e}"))?;
    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT properties FROM notes \
                  WHERE id = ?1 AND namespace = ?2 AND kind = 'scheduled_event' \
                    AND deleted_at IS NULL"
                .to_string(),
            params: vec![
                SqlValue::Text(id.to_string()),
                SqlValue::Text(namespace.to_string()),
            ],
            label: Some("pending_events_current_properties".into()),
        })
        .await
        .map_err(|e| anyhow::anyhow!("pending-events: read current properties: {e}"))?;
    match rows.as_slice() {
        [] => Ok(None),
        [row] => match row.get("properties") {
            Some(SqlValue::Text(value)) => Ok(Some(value.clone())),
            Some(SqlValue::Null) | None => Ok(None),
            other => Err(anyhow::anyhow!(
                "pending-events: unexpected properties column shape: {other:?}"
            )),
        },
        _ => Err(anyhow::anyhow!(
            "pending-events: multiple rows for scheduled_event {id}"
        )),
    }
}

/// These direct SQL updates change only fixed, non-reserved JSON paths. The
/// final object's top-level reserved-key membership is therefore identical to
/// this snapshot's. Each caller either already has an exact-properties CAS or
/// adds one to its UPDATE, so a concurrent writer cannot change the object
/// between this check and the write.
fn check_fixed_path_whole_object_snapshot(properties: &str) -> Result<()> {
    let value: Value = serde_json::from_str(properties)
        .context("pending-events: parse properties for reservation check")?;
    khive_runtime::secret_gate::reject_reserved_secret_gate_property(Some(&value))?;
    Ok(())
}

/// Parses a finalizer's freshly read current-properties CAS snapshot into the
/// `Value` base a terminal write's field mutations are applied to. Callers
/// must build their write on this value, not on the page-query snapshot taken
/// before the claim — a property written between that snapshot and this read
/// still passes the CAS fence (it is part of what "current" means by the time
/// this is called) but would otherwise be silently discarded by a write whose
/// base predates it. Returns `None` (and logs) if the stored text is not
/// valid JSON; the caller must treat that as a failed finalization.
fn expected_properties_value(expected_properties: &str, id: uuid::Uuid) -> Option<Value> {
    match serde_json::from_str(expected_properties) {
        Ok(value) => Some(value),
        Err(error) => {
            tracing::error!(
                scheduled_event_id = %id,
                error = %error,
                "pending-events: could not parse current properties for finalization"
            );
            None
        }
    }
}

/// Read the row's raw current properties at the same read boundary as a
/// pending-action finalization decision, for use as `finalize_fired_event`'s
/// mandatory exact-properties CAS fence. Returns `None` (and logs) on a read
/// error or a vanished row; the caller must treat that as a failed
/// finalization rather than retry with a stale or synthetic snapshot.
async fn current_properties_for_finalize(
    rt: &KhiveRuntime,
    namespace: &str,
    id: uuid::Uuid,
    context: &'static str,
) -> Option<String> {
    match current_note_properties_text(rt, namespace, id).await {
        Ok(Some(text)) => Some(text),
        Ok(None) => {
            tracing::error!(
                scheduled_event_id = %id,
                "pending-events: row vanished before {context} finalization"
            );
            None
        }
        Err(error) => {
            tracing::error!(
                scheduled_event_id = %id,
                error = %error,
                "pending-events: could not read current properties before {context} finalization"
            );
            None
        }
    }
}

/// CAS-persist the post-drain state of a claimed event: `firing -> {fired |
/// pending | missed | failed}` (`pending` is an advanced repeat; `failed` is
/// the unattributed-generic-action policy state). `claimed_firing_at` is
/// the claim token from `claim_pending_event`; the CAS requires the row's
/// CURRENT `firing_at` to still equal it, not merely `status='firing'`.
/// Clears `firing_at` on the terminal write. Returns
/// `Ok(true)` iff exactly one row was updated. See
/// `crates/khive-mcp/docs/api/pending-events.md`.
/// Bundles `finalize_firing_event`'s two independent CAS guard inputs — the
/// recovery-only legacy-stale timing predicate and the exact-properties
/// equality predicate any caller may supply — into one parameter so the
/// function stays under clippy's argument-count lint.
#[derive(Clone, Copy, Default)]
struct FinalizeGuard<'a> {
    expired_at: Option<i64>,
    expected_properties: Option<&'a str>,
}

/// Finalize a row this process's own claim is dispatching, guarded on the
/// row's exact current properties as well as claim identity. `expected_properties`
/// is mandatory — not `Option` — so a future branch cannot silently drop the
/// content fence by passing `None`: the claim token alone is an ownership
/// fence, not a substitute for detecting a concurrent property writer that
/// landed between claim and finalization (ADR-106). Callers must read the
/// row's raw current properties at the same read boundary as their
/// finalization decision — see `current_note_properties_text` — and pass
/// that snapshot here.
async fn finalize_fired_event(
    rt: &KhiveRuntime,
    namespace: &str,
    id: uuid::Uuid,
    properties: &Value,
    updated_at: i64,
    claim: &DispatchClaim,
    expected_properties: &str,
) -> Result<bool> {
    finalize_firing_event(
        rt,
        namespace,
        id,
        properties,
        updated_at,
        claim,
        FinalizeGuard {
            expired_at: None,
            expected_properties: Some(expected_properties),
        },
    )
    .await
}

/// Finalize a row selected by the expired-lease recovery pass, but only while
/// its CURRENT properties still exactly match the expired snapshot selected
/// by that pass. A renewal or outcome write between the recovery SELECT and
/// this CAS wins and makes recovery a no-op.
async fn finalize_expired_firing_event(
    rt: &KhiveRuntime,
    namespace: &str,
    id: uuid::Uuid,
    properties: &Value,
    updated_at: i64,
    claim: &DispatchClaim,
    snapshot: RecoverySnapshot<'_>,
) -> Result<bool> {
    finalize_firing_event(
        rt,
        namespace,
        id,
        properties,
        updated_at,
        claim,
        FinalizeGuard {
            expired_at: Some(snapshot.expired_at),
            expected_properties: Some(snapshot.properties),
        },
    )
    .await
}

async fn finalize_firing_event(
    rt: &KhiveRuntime,
    namespace: &str,
    id: uuid::Uuid,
    properties: &Value,
    updated_at: i64,
    claim: &DispatchClaim,
    guard: FinalizeGuard<'_>,
) -> Result<bool> {
    let FinalizeGuard {
        expired_at,
        expected_properties,
    } = guard;
    let legacy_stale_before =
        expired_at.map(|value| value.saturating_sub(LEGACY_STALE_FIRING_TIMEOUT_MICROS));
    let mut properties = properties.clone();
    if let Some(obj) = properties.as_object_mut() {
        obj.remove("firing_at");
        obj.remove("lease_expires_at");
    }
    // Same direct-SQL seam as `persist_dispatch_outcome` above, mask-not-
    // block credential content for the same reason: this is the terminal write shared by
    // fresh-dispatch finalization and expired-lease recovery, no caller can
    // act on a content-scan refusal here, and refusing would leave the row `firing`
    // forever instead of recording the outcome that already happened. It
    // re-embeds the same handler-supplied failure content
    // (`dispatch_receipt.error`/`error_payload`, plus the legacy flat
    // `dispatch_error`/`delivery_error` mirror) into a fresh properties blob.
    // Mask exactly those four fields, not the whole blob: the rest of
    // `properties` already passed this scan when it was originally written.
    //
    // `dispatch_receipt.error`/`error_payload` may already have been masked
    // by `persist_dispatch_outcome` on the fresh-dispatch path (the receipt
    // it returns is what `final_properties_after_dispatch` copies into
    // `properties["dispatch_receipt"]` before this function runs); the
    // expired-lease recovery path can also reach here with an
    // already-masked receipt carried over from an earlier pass. A second
    // `mask_json_content` pass over already-masked text is a no-op:
    // `bounded_masked_log_text` (secret_gate.rs:770-791) masks through
    // `mask_secrets` (secret_gate.rs:569-586), whose span collector matches
    // only known credential shapes and high-entropy runs beside a trigger
    // word (`TRIGGER_WORDS`, secret_gate.rs:1313-1326); `REDACTION_MARKER`
    // ("***MASKED***", secret_gate.rs:251) is plain uppercase ASCII with no
    // digit/hex/base64 shape and contains none of those trigger substrings,
    // so `collect_mask_spans` finds no span in it and `mask_secrets` returns
    // its input unchanged (`Cow::Borrowed`, secret_gate.rs:571-572).
    // `neutralize_log_unsafe_chars` (secret_gate.rs:877-879) is idempotent
    // the same way: its `\u{XXXX}` escape output contains no Cc/Cf/Zl/Zp
    // codepoint, so nothing it has already escaped is escaped again.
    //
    // `dispatch_error`/`delivery_error` are the legacy flat mirror and
    // reach this function raw from `final_properties_after_dispatch`
    // (which derives them from the original, unmasked
    // `DispatchCompletion`), so this is their first and only mask.
    let receipt_error = properties
        .pointer("/dispatch_receipt/error")
        .cloned()
        .map(|v| mask_json_content(&v));
    let receipt_error_payload = properties
        .pointer("/dispatch_receipt/error_payload")
        .cloned()
        .map(|v| mask_json_content(&v));
    let dispatch_error = properties
        .get("dispatch_error")
        .cloned()
        .map(|v| mask_json_content(&v));
    let delivery_error = properties
        .get("delivery_error")
        .cloned()
        .map(|v| mask_json_content(&v));
    if let Some(masked) = &receipt_error {
        if let Some(slot) = properties.pointer_mut("/dispatch_receipt/error") {
            *slot = masked.clone();
        }
    }
    if let Some(masked) = &receipt_error_payload {
        if let Some(slot) = properties.pointer_mut("/dispatch_receipt/error_payload") {
            *slot = masked.clone();
        }
    }
    if let Some(masked) = &dispatch_error {
        properties["dispatch_error"] = masked.clone();
    }
    if let Some(masked) = &delivery_error {
        properties["delivery_error"] = masked.clone();
    }
    if let Err(gate_error) = khive_runtime::secret_gate::check_json_at(
        &json!({
            "dispatch_receipt.error": &receipt_error,
            "dispatch_receipt.error_payload": &receipt_error_payload,
            "dispatch_error": &dispatch_error,
            "delivery_error": &delivery_error,
        }),
        "scheduled_event",
        "dispatch_receipt",
    ) {
        // Same masking-invariant posture as `persist_dispatch_outcome`
        // above: the masker was supposed to make this input pass and did
        // not. `gate_error`'s `Display` never echoes scanned content (see
        // the comment there), so it is safe to log in full. Persist the
        // masked record anyway -- it is the record about to be written
        // either way, and a durable terminal outcome is worth more than a
        // belt-and-braces assert.
        tracing::error!(
            scheduled_event_id = %id,
            error = %gate_error,
            "pending-events: masking invariant failed on finalized dispatch receipt; \
             persisting the masked record anyway"
        );
    }
    khive_runtime::secret_gate::reject_reserved_secret_gate_property(Some(&properties))?;
    let props_json = serde_json::to_string(&properties)
        .map_err(|e| anyhow::anyhow!("pending-events: serialize properties: {e}"))?;
    let mut writer = rt
        .sql()
        .writer()
        .await
        .map_err(|e| anyhow::anyhow!("pending-events: open SQL writer: {e}"))?;
    let rows = writer
        .execute(SqlStatement {
            sql: "UPDATE notes \
                  SET properties = ?1, updated_at = ?2 \
                  WHERE id = ?3 \
                    AND namespace = ?4 \
                    AND kind = 'scheduled_event' \
                    AND deleted_at IS NULL \
                    AND json_extract(properties, '$.status') = 'firing' \
                    AND CAST(json_extract(properties, '$.firing_at') AS INTEGER) = ?5 \
                    AND json_extract(properties, '$.dispatch_receipt.invocation_id') = ?6 \
                    AND ( \
                      ?7 IS NULL OR ( \
                        (json_extract(properties, '$.lease_expires_at') IS NOT NULL \
                         AND CAST(json_extract(properties, '$.lease_expires_at') AS INTEGER) <= ?7) \
                        OR \
                        (json_extract(properties, '$.lease_expires_at') IS NULL \
                         AND (json_extract(properties, '$.firing_at') IS NULL \
                              OR CAST(json_extract(properties, '$.firing_at') AS INTEGER) < ?8)) \
                      ) \
                    ) \
                    AND (?9 IS NULL OR properties = ?9)"
                .to_string(),
            params: vec![
                SqlValue::Text(props_json),
                SqlValue::Integer(updated_at),
                SqlValue::Text(id.to_string()),
                SqlValue::Text(namespace.to_string()),
                SqlValue::Integer(claim.firing_at),
                SqlValue::Text(claim.invocation_id.to_string()),
                expired_at.map_or(SqlValue::Null, SqlValue::Integer),
                legacy_stale_before.map_or(SqlValue::Null, SqlValue::Integer),
                expected_properties.map_or(SqlValue::Null, |value| SqlValue::Text(value.to_string())),
            ],
            label: Some("pending_events_finalize_fired".into()),
        })
        .await
        .map_err(|e| anyhow::anyhow!("pending-events: finalize conditional update: {e}"))?;
    Ok(rows == 1)
}

/// Compute the next `trigger_at` for a repeating event, given the current
/// `trigger_at` and the `repeat` spec.
///
/// Returns `None` for an absent, malformed, or exhausted repeat. Callers that
/// finalize a row must use `next_trigger_at_for_event` to distinguish a
/// one-shot from a stored recurrence that cannot advance.
fn next_trigger_at(repeat: &Option<String>, current: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let repeat = khive_pack_schedule::repeat::parse_repeat(repeat.as_deref()?).ok()?;
    repeat.next_after(current)
}

const INVALID_MONTHLY_ANCHOR: &str =
    "monthly repeat_anchor must be a valid timestamp no later than trigger_at";
const INVALID_STORED_REPEAT: &str = "stored repeat must be a string";
const UNADVANCEABLE_REPEAT: &str = "stored repeat has no representable next occurrence";

fn is_monthly_repeat(repeat: &Option<String>) -> bool {
    repeat
        .as_deref()
        .is_some_and(|value| value.trim() == "monthly")
}

/// Each monthly candidate is computed from the original anchor, never from
/// the preceding (possibly clamped) trigger. A legacy row adopts its current
/// trigger in the same finalization that first advances it.
fn monthly_next_after(
    properties: &mut Value,
    current: DateTime<Utc>,
    bound: DateTime<Utc>,
) -> std::result::Result<Option<DateTime<Utc>>, &'static str> {
    let (anchor_text, legacy) = match properties.get("repeat_anchor") {
        Some(value) => (
            value.as_str().ok_or(INVALID_MONTHLY_ANCHOR)?.to_string(),
            false,
        ),
        None => (
            properties
                .get("trigger_at")
                .and_then(Value::as_str)
                .ok_or(INVALID_MONTHLY_ANCHOR)?
                .to_string(),
            true,
        ),
    };
    let anchor = anchor_text
        .parse::<DateTime<Utc>>()
        .map_err(|_| INVALID_MONTHLY_ANCHOR)?;
    if anchor > current || (legacy && anchor != current) {
        return Err(INVALID_MONTHLY_ANCHOR);
    }
    let next = khive_pack_schedule::repeat::Repeat::Monthly.first_after(anchor, bound);
    if next.is_some() && legacy {
        properties["repeat_anchor"] = json!(anchor_text);
    }
    Ok(next)
}

fn next_trigger_at_for_event(
    properties: &mut Value,
    repeat: &Option<String>,
    current: DateTime<Utc>,
) -> std::result::Result<Option<DateTime<Utc>>, &'static str> {
    if repeat.is_none() {
        return if properties
            .get("repeat")
            .is_some_and(|value| !value.is_null())
        {
            Err(INVALID_STORED_REPEAT)
        } else {
            Ok(None)
        };
    }
    if is_monthly_repeat(repeat) {
        monthly_next_after(properties, current, current)?
            .map(Some)
            .ok_or(UNADVANCEABLE_REPEAT)
    } else {
        next_trigger_at(repeat, current)
            .map(Some)
            .ok_or(UNADVANCEABLE_REPEAT)
    }
}

/// Advance a missed repeating event's `trigger_at` past every occurrence at
/// or before `now`, landing on the first occurrence strictly after `now`
/// (ADR-106 missed-event amendment) — avoids firing a catch-up burst.
/// Returns `None` for an absent, malformed, or exhausted repeat. Callers that
/// finalize a row must use `advance_repeat_past_missed_for_event` to distinguish
/// a one-shot from a stored recurrence that cannot advance.
/// See `crates/khive-mcp/docs/api/pending-events.md` for the termination
/// argument.
fn advance_repeat_past_missed(
    repeat: &Option<String>,
    current: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    let repeat = khive_pack_schedule::repeat::parse_repeat(repeat.as_deref()?).ok()?;
    repeat.first_after(current, now)
}

fn advance_repeat_past_missed_for_event(
    properties: &mut Value,
    repeat: &Option<String>,
    current: DateTime<Utc>,
    now: DateTime<Utc>,
) -> std::result::Result<Option<DateTime<Utc>>, &'static str> {
    if repeat.is_none() {
        return if properties
            .get("repeat")
            .is_some_and(|value| !value.is_null())
        {
            Err(INVALID_STORED_REPEAT)
        } else {
            Ok(None)
        };
    }
    if is_monthly_repeat(repeat) {
        monthly_next_after(properties, current, now)?
            .map(Some)
            .ok_or(UNADVANCEABLE_REPEAT)
    } else {
        advance_repeat_past_missed(repeat, current, now)
            .map(Some)
            .ok_or(UNADVANCEABLE_REPEAT)
    }
}

fn reminder_delivery_action(actor: &str, content: &str) -> String {
    let action = json!([{
        "tool": "comm.send",
        "args": {
            "to": actor,
            "subject": reminder_subject(content),
            "content": content,
            "self_send": true,
        }
    }]);
    serde_json::to_string(&action).expect("reminder delivery action is JSON-serializable")
}

fn reminder_subject(content: &str) -> String {
    const MAX_HEAD_CHARS: usize = 80;
    let collapsed = content.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = collapsed.chars();
    let head: String = chars.by_ref().take(MAX_HEAD_CHARS).collect();
    if chars.next().is_some() {
        format!("[Reminder] {head}…")
    } else if head.is_empty() {
        "[Reminder]".to_string()
    } else {
        format!("[Reminder] {head}")
    }
}

async fn append_reminder_delivery_failure_event(
    server: &KhiveMcpServer,
    namespace: &str,
    scheduled_event_id: uuid::Uuid,
    audit_actor: &str,
    recipient_actor: &str,
    error: &str,
) {
    let Some(store) = server.event_store() else {
        return;
    };
    let event = khive_storage::Event::new(
        namespace,
        "schedule.remind.fire",
        EventKind::Audit,
        SubstrateKind::Note,
        audit_actor,
    )
    .with_outcome(EventOutcome::Error)
    .with_target(scheduled_event_id)
    .with_payload(json!({
        "scheduled_event_id": scheduled_event_id,
        "recipient_actor": recipient_actor,
        "error": khive_runtime::secret_gate::bounded_masked_log_text(error),
    }));
    if let Err(trace_error) = store.append_event(event).await {
        tracing::error!(
            scheduled_event_id = %scheduled_event_id,
            error = %trace_error,
            "pending-events: reminder delivery failure event append failed"
        );
    }
}

/// Resolve the actor bound to a scheduled-event note by the schedule pack's
/// immutable provenance event.
///
/// The note's `properties.created_by_actor` field is intentionally ignored:
/// generic note create can forge it, while schedule-managed rows reject generic
/// update/merge. The `events` substrate is append-only and has no public create
/// verb, so a target-bound event written by `schedule.remind`/`schedule.schedule`
/// is the durable out-of-band proof from which the host constructs a verified
/// replay identity. Zero matching rows means legacy or hand-written intent.
/// More than one is corruption and fails the drain pass rather than choosing an
/// identity nondeterministically.
#[derive(Clone, Debug)]
struct VerifiedCreator {
    /// `None` deliberately represents the provenance-verified
    /// `anonymous:local` actor. Request identity resolution must receive
    /// `None`, not `Some("local")`, to preserve the actor kind.
    request_actor: Option<VerifiedActor>,
    recipient_id: String,
    audit_actor: String,
}

async fn verified_creator_for_event(
    rt: &KhiveRuntime,
    namespace: &str,
    scheduled_event_id: uuid::Uuid,
    event_type: &str,
) -> Result<Option<VerifiedCreator>> {
    let mut reader = rt
        .sql()
        .reader()
        .await
        .context("pending-events: open SQL reader for creator provenance")?;
    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT actor FROM events \
                  WHERE namespace = ?1 \
                    AND verb = ?2 \
                    AND target_id = ?3 \
                    AND outcome = 'success' \
                    AND json_extract(payload, '$.provenance') = ?4 \
                    AND json_extract(payload, '$.event_type') = ?5 \
                  ORDER BY created_at ASC, id ASC LIMIT 2"
                .to_string(),
            params: vec![
                SqlValue::Text(namespace.to_string()),
                SqlValue::Text(khive_pack_schedule::CREATOR_PROVENANCE_VERB.to_string()),
                SqlValue::Text(scheduled_event_id.to_string()),
                SqlValue::Text(khive_pack_schedule::CREATOR_PROVENANCE_MARKER_V1.to_string()),
                SqlValue::Text(event_type.to_string()),
            ],
            label: Some("pending_events_creator_provenance".into()),
        })
        .await
        .context("pending-events: query creator provenance")?;

    match rows.as_slice() {
        [] => Ok(None),
        [row] => {
            let actor = match row.get("actor") {
                Some(SqlValue::Text(actor)) => actor,
                other => {
                    return Err(anyhow::anyhow!(
                        "pending-events: creator provenance for {scheduled_event_id} has invalid \
                         actor column: {other:?}"
                    ));
                }
            };
            if let Some(actor_id) = actor.strip_prefix("actor:") {
                let verified = VerifiedActor::new(actor_id.to_string()).map_err(|e| {
                    anyhow::anyhow!("pending-events: invalid creator provenance: {e}")
                })?;
                Ok(Some(VerifiedCreator {
                    request_actor: Some(verified),
                    recipient_id: actor_id.to_string(),
                    audit_actor: actor.clone(),
                }))
            } else if actor == "anonymous:local" {
                Ok(Some(VerifiedCreator {
                    request_actor: None,
                    recipient_id: "local".to_string(),
                    audit_actor: actor.clone(),
                }))
            } else {
                Err(anyhow::anyhow!(
                    "pending-events: creator provenance for {scheduled_event_id} has \
                     unsupported actor encoding {actor:?}"
                ))
            }
        }
        _ => Err(anyhow::anyhow!(
            "pending-events: scheduled event {scheduled_event_id} has duplicate creator \
             provenance rows"
        )),
    }
}

/// Dispatch a DSL action string in the given namespace while renewing its
/// claim through the claim-bound durable outcome write.
///
/// The action is wrapped as a JSON-form batch with `namespace` injected into
/// each op's args so the VerbRegistry mints a token scoped to the event's
/// namespace. Dispatch also uses the provenance-verified creator as the
/// effective request identity and preserves public-surface visibility, so a
/// delayed action cannot invoke an internal subhandler. Together these
/// preserve the original authority boundary: writes land in the event's
/// namespace and gate/audit decisions never inherit daemon authority. The
/// returned receipt result is already persisted (or carries the persistence
/// error); callers must not perform another outcome write.
struct DispatchLeaseTarget<'a> {
    rt: &'a KhiveRuntime,
    namespace: &'a str,
    scheduled_event_id: uuid::Uuid,
    claim: &'a DispatchClaim,
}

async fn dispatch_with_renewable_lease(
    target: DispatchLeaseTarget<'_>,
    lease: DispatchLeaseConfig,
    action_dsl: &str,
    creator_actor: Option<VerifiedActor>,
    server: &KhiveMcpServer,
    verbose: bool,
) -> (DispatchCompletion, Result<Option<Value>>) {
    let DispatchLeaseTarget {
        rt,
        namespace,
        scheduled_event_id,
        claim,
    } = target;
    let renewal_rt = rt.clone();
    let renewal_namespace = namespace.to_string();
    let renewal_claim = claim.clone();
    let renewal_cancel = tokio_util::sync::CancellationToken::new();
    let _renewal_cancel_on_drop = CancelOnDrop(renewal_cancel.clone());
    let renewal_stop = renewal_cancel.clone();
    let mut renewal = Some(tokio::spawn(async move {
        let mut renewals = tokio::time::interval_at(
            tokio::time::Instant::now() + lease.renew_every,
            lease.renew_every,
        );
        renewals.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = renewal_stop.cancelled() => return None,
                _ = renewals.tick() => {}
            }
            match renew_dispatch_lease(
                &renewal_rt,
                &renewal_namespace,
                scheduled_event_id,
                &renewal_claim,
                lease,
            )
            .await
            {
                Ok(true) => {}
                Ok(false) => {
                    return Some(
                        "dispatch lease ownership was lost before the action returned".to_string(),
                    );
                }
                Err(error) => {
                    return Some(format!(
                        "dispatch lease renewal failed before the action returned: {error}"
                    ));
                }
            }
        }
    }));

    let dispatch_result =
        dispatch_action(action_dsl, namespace, creator_actor, server, verbose).await;
    // If the renewal task already ended before the action did, its failure is
    // part of the action outcome. Otherwise keep it alive while the durable
    // outcome CAS waits for the writer; relinquishing the lease first would
    // reopen the dispatch/finalize crash window under writer contention.
    let early_lease_failure = if renewal.as_ref().is_some_and(|handle| handle.is_finished()) {
        match renewal.take().expect("renewal handle exists").await {
            Ok(failure) => failure,
            Err(error) => Some(format!("dispatch lease renewal task failed: {error}")),
        }
    } else {
        None
    };
    let completion = if let Some(error) = early_lease_failure {
        DispatchCompletion::Indeterminate(DispatchFailure::plain(error))
    } else {
        match dispatch_result {
            Ok(()) => DispatchCompletion::Succeeded,
            Err(error) if error.outcome_uncertain && !error.disposition_only_uncertain => {
                DispatchCompletion::Indeterminate(error.failure)
            }
            Err(error) => DispatchCompletion::Failed(error.failure),
        }
    };

    let persisted =
        persist_dispatch_outcome(rt, namespace, scheduled_event_id, claim, &completion).await;
    let outcome_is_durable = matches!(&persisted, Ok(Some(_)));
    renewal_cancel.cancel();
    if let Some(renewal) = renewal {
        let late_lease_failure = match renewal.await {
            Ok(failure) => failure,
            Err(error) => Some(format!("dispatch lease renewal task failed: {error}")),
        };
        // A renewal already in flight can observe the just-persisted receipt
        // state and report ownership loss. Once the outcome CAS committed,
        // that is expected and harmless; otherwise retain the diagnostic.
        if !outcome_is_durable {
            if let Some(error) = late_lease_failure {
                tracing::error!(
                    scheduled_event_id = %scheduled_event_id,
                    error,
                    "pending-events: lease renewal ended before outcome became durable"
                );
            }
        }
    }
    (completion, persisted)
}

fn action_error_message(error: &Value) -> String {
    error
        .as_str()
        .or_else(|| error.get("message").and_then(Value::as_str))
        .map(str::to_string)
        .unwrap_or_else(|| {
            serde_json::to_string(error)
                .unwrap_or_else(|_| "scheduled action returned an unreadable error".to_string())
        })
}

fn action_error_outcome_is_uncertain(error: &Value) -> bool {
    let message = action_error_message(error).to_ascii_lowercase();
    let kind = error
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let code = error
        .get("code")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let request_state = error
        .get("request_state")
        .or_else(|| error.pointer("/details/request_state"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let has_outbound_id = error
        .pointer("/details/outbound_id")
        .and_then(Value::as_str)
        .is_some_and(|value| uuid::Uuid::parse_str(value).is_ok());

    kind == "ambiguous"
        || code == "side_effects_unknown"
        || code == "ambiguous_outcome"
        || request_state == "side_effects_unknown"
        || message.contains("side_effects_unknown")
        || (has_outbound_id
            && (kind == "conflict"
                || message.contains("outcome is uncertain")
                || message.contains("comm.delivered")))
}

fn action_error_disposition_may_have_committed(error: &Value) -> bool {
    match error {
        Value::Array(errors) => errors
            .iter()
            .any(action_error_disposition_may_have_committed),
        Value::Object(fields) => ["domain_disposition", "entry_domain_disposition"]
            .into_iter()
            .filter_map(|key| fields.get(key))
            .any(|disposition| disposition.as_str() != Some("not_committed")),
        _ => false,
    }
}

fn action_failures(failures: &[&Value]) -> DispatchActionError {
    let errors: Vec<Value> = failures
        .iter()
        .map(|failure| {
            let mut error = failure
                .get("error")
                .cloned()
                .unwrap_or_else(|| (*failure).clone());
            if let Some(disposition) = failure.get("domain_disposition") {
                if let Some(fields) = error.as_object_mut() {
                    // Preserve both values if the entry and error disagree:
                    // either may say that the mutation already committed.
                    fields.insert("entry_domain_disposition".into(), disposition.clone());
                } else {
                    error = json!({
                        "message": action_error_message(&error),
                        "original_error": error,
                        "entry_domain_disposition": disposition,
                    });
                }
            }
            error
        })
        .collect();
    let heuristic_uncertain = errors.iter().any(action_error_outcome_is_uncertain);
    let disposition_uncertain = errors
        .iter()
        .any(action_error_disposition_may_have_committed);
    let messages = errors
        .iter()
        .map(action_error_message)
        .collect::<Vec<_>>()
        .join("; ");
    let payload = match errors.as_slice() {
        [error] => error.clone(),
        _ => Value::Array(errors),
    };
    let failure = DispatchFailure::with_payload(
        format!(
            "pending-events: action produced {} failure(s): {messages}",
            failures.len()
        ),
        payload,
    );
    if heuristic_uncertain {
        DispatchActionError::uncertain(failure)
    } else if disposition_uncertain {
        DispatchActionError::disposition_uncertain(failure)
    } else {
        DispatchActionError::known(failure)
    }
}

fn stored_action_is_non_single(action_dsl: &str) -> bool {
    khive_request::parse_request(action_dsl).is_ok_and(|parsed| {
        parsed.mode != khive_request::ExecutionMode::Single || parsed.ops.len() != 1
    })
}

async fn dispatch_action(
    action_dsl: &str,
    namespace: &str,
    creator_actor: Option<VerifiedActor>,
    server: &KhiveMcpServer,
    verbose: bool,
) -> std::result::Result<(), DispatchActionError> {
    let parsed = khive_request::parse_request(action_dsl).map_err(|error| {
        let masked_dsl = khive_runtime::secret_gate::bounded_masked_log_text(action_dsl);
        DispatchActionError::known(DispatchFailure::plain(format!(
            "pending-events: action DSL parse error ({error}): {masked_dsl:?}"
        )))
    })?;

    // `$prev` references are rejected at schedule-creation time, but legacy
    // rows written before that guard may still carry one. Reject rather than
    // silently drop: a dropped arg can dispatch successfully with
    // missing/wrong data, which is worse than a visible replay failure.
    let mut ops_json: Vec<Value> = Vec::with_capacity(parsed.ops.len());
    for op in &parsed.ops {
        let mut args = serde_json::Map::new();
        for (k, v) in &op.args {
            let khive_request::ArgValue::Value(val) = v else {
                let masked_dsl = khive_runtime::secret_gate::bounded_masked_log_text(action_dsl);
                return Err(DispatchActionError::known(DispatchFailure::plain(format!(
                    "pending-events: non-literal scheduled action argument {k:?} is not \
                     replayable: {masked_dsl:?}"
                ))));
            };
            args.insert(k.clone(), val.clone());
        }
        // Inject the event's namespace so the registry writes to it.
        args.insert(
            "namespace".to_string(),
            Value::String(namespace.to_string()),
        );
        ops_json.push(json!({ "tool": op.tool, "args": Value::Object(args) }));
    }

    let ops_str = serde_json::to_string(&ops_json).map_err(|error| {
        DispatchActionError::known(DispatchFailure::plain(format!(
            "pending-events: serialize ops: {error}"
        )))
    })?;

    if verbose {
        eprintln!("[pending-events] dispatch ns={namespace}: {ops_str}");
    }

    let result = server
        .dispatch_request_replay_as(
            RequestParams {
                plan: None,
                ops: ops_str,
                presentation: None,
                presentation_per_op: None,
                save_to: None,
                format: None,
                format_per_op: None,
                request_id: None,
            },
            namespace,
            creator_actor,
        )
        .await
        .map_err(|error| {
            // The replay request was accepted by the in-process host, but no
            // per-op envelope came back. Conservatively retain at-most-once
            // behavior because the action may already have run.
            DispatchActionError::uncertain(DispatchFailure::plain(format!(
                "pending-events: dispatch outcome unavailable: {error}"
            )))
        })?;

    // The MCP response is a JSON string. Check for per-op failures.
    let parsed_result: Value = serde_json::from_str(&result).map_err(|error| {
        DispatchActionError::uncertain(DispatchFailure::with_payload(
            format!("pending-events: dispatch returned invalid JSON: {error}"),
            json!({"raw_response": result.clone()}),
        ))
    })?;
    let results = parsed_result
        .get("results")
        .and_then(Value::as_array)
        .filter(|results| !results.is_empty())
        .ok_or_else(|| {
            DispatchActionError::uncertain(DispatchFailure::with_payload(
                "pending-events: dispatch response omitted per-op results",
                parsed_result.clone(),
            ))
        })?;
    let failures: Vec<_> = results
        .iter()
        .filter(|result| result.get("ok").and_then(Value::as_bool) != Some(true))
        .collect();
    if !failures.is_empty() {
        return Err(action_failures(&failures));
    }

    Ok(())
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
