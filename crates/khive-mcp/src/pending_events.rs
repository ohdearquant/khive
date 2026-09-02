//! Scheduled event drain — `kkernel exec --pending-events` (one-shot) and the
//! daemon-resident tick (ADR-106, [`schedule_tick_loop`]).
//!
//! Scans all `scheduled_event` notes with `status="pending"` whose `trigger_at`
//! is at or before now, dispatches scheduled actions or delivers reminders to
//! their creating actors through `comm.send`, and durably records each action
//! outcome before finalizing the event lifecycle. Successful one-shots become
//! `"fired"`; failed one-shots remain `"pending"` for recovery; named repeats
//! advance to their next occurrence. Events overdue by more than the configured
//! grace window are never dispatched, per the missed-event policy below.
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
//! Named aliases are advanced as follows:
//! - `"daily"`   → `trigger_at + 1 day`
//! - `"weekly"`  → `trigger_at + 7 days`
//! - `"monthly"` → `trigger_at + 1 calendar month`
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
use chrono::{DateTime, Duration, FixedOffset, Months, Utc};
use serde_json::{json, Value};

use crate::server::KhiveMcpServer;
use crate::tools::request::RequestParams;
use khive_runtime::{KhiveRuntime, Namespace, VerifiedActor};
use khive_storage::types::{SqlStatement, SqlValue};
use khive_types::{EventKind, EventOutcome, SubstrateKind};

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
        }
    }

    fn uncertain(failure: DispatchFailure) -> Self {
        Self {
            failure,
            outcome_uncertain: true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FinalDisposition {
    Fired,
    Advanced,
    RetryPending,
    Failed,
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

/// One-shot drain: fire all pending, due scheduled events.
///
/// - Scans for `scheduled_event` notes with `status="pending"` and
///   `trigger_at <= now`.
/// - Dispatches the stored action DSL or reminder inbox delivery in the event's namespace.
/// - Persists a claim-bound dispatch receipt before lifecycle finalization.
/// - Marks successful one-shots `"fired"`; failed one-shots return to
///   `"pending"` for a later pass.
/// - For repeating events with named aliases (`"daily"` / `"weekly"` /
///   `"monthly"`), resets status to `"pending"` and advances `trigger_at`.
///   Unsupported recurrence is rejected at creation and fails closed for
///   legacy rows (see module-level documentation).
///
/// Per-event failures accumulate in the returned [`DrainSummary`] without
/// aborting the drain.
pub async fn run_pending_events(
    db: Option<&str>,
    namespace: &str,
    verbose: bool,
) -> Result<DrainSummary> {
    run_pending_events_with_config(db, None, namespace, verbose).await
}

/// One-shot drain with an explicit configuration-file selection.
///
/// This is the `kkernel exec --pending-events --config …` entrypoint. The
/// compatibility wrapper above retains the original discovery behavior for
/// callers that do not carry a config path.
pub async fn run_pending_events_with_config(
    db: Option<&str>,
    config: Option<&std::path::Path>,
    namespace: &str,
    verbose: bool,
) -> Result<DrainSummary> {
    // Resolves through the same multi-backend-aware construction the daemon
    // boot path uses, with the namespace marked explicit but NOT an actor
    // override — `namespace_explicit: true, actor_explicit: false` — so a
    // `"local"`-resolved default namespace still falls through to the
    // project-configured actor. See "Server construction: explicit namespace,
    // implicit actor" in `crates/khive-mcp/docs/pending-events.md`.
    let ns = Namespace::parse(namespace)
        .map_err(|e| anyhow::anyhow!("pending-events: invalid namespace {namespace:?}: {e}"))?;
    let args = crate::args::Args {
        db: db.map(str::to_string),
        actor: None,
        namespace: None,
        no_embed: false,
        pack: Vec::new(),
        config: config.map(std::path::Path::to_path_buf),
        daemon: false,
        transport: None,
        bind: None,
        brain_profile: None,
        resumed_generation: None,
    };
    // A `DatabaseOverrideConflict` raised by the builder must pass through
    // unchanged so `kkernel exec`'s caller receives it as the top-level error
    // and `db_override_refusal_envelope`'s `downcast_ref` recognizes it,
    // emitting the documented JSON refusal envelope. Every other build
    // failure keeps the generic "pending-events: build server" provenance.
    let (server, schedule_rt) =
        match crate::serve::build_server_with_explicit_namespace(&args, ns, true, false).await {
            Ok(built) => built,
            Err(error) => {
                if error
                    .downcast_ref::<crate::serve::DatabaseOverrideConflict>()
                    .is_some()
                {
                    return Err(error);
                }
                return Err(error.context("pending-events: build server"));
            }
        };
    tracing::info!(target: "khive.boot", "{}", crate::serve::resolved_actor_disclosure(server.actor_id()));
    let rt = schedule_rt.ok_or_else(|| {
        anyhow::anyhow!(
            "pending-events: resolved pack set does not include \"schedule\"; nothing to drain"
        )
    })?;
    run_pending_events_on(&rt, &server, verbose).await
}

/// One-shot drain against an already-constructed [`KhiveRuntime`] +
/// [`KhiveMcpServer`] pair (ADR-106).
///
/// The caller supplies an already-resolved, already-validated pair — both by
/// reference — so the drain's storage target, actor identity, and pack set
/// are always identical to the server it is ticking for. `rt` and `server`
/// serve two different roles that must NOT be collapsed into one: `rt` is
/// the **schedule pack's own runtime** (the scan/claim/finalize SQL below
/// reads and CAS-writes `scheduled_event` notes directly through it) while
/// `server` is the **daemon's live, fully-wired `KhiveMcpServer`**, used
/// only for `dispatch_action` (replaying a stored action's DSL) — building a
/// second server from `rt` alone would misroute replayed actions in a
/// multi-backend deployment. See
/// `crates/khive-mcp/docs/api/pending-events.md` for the full rationale.
pub async fn run_pending_events_on(
    rt: &KhiveRuntime,
    server: &KhiveMcpServer,
    verbose: bool,
) -> Result<DrainSummary> {
    run_pending_events_on_with_lease(rt, server, verbose, DispatchLeaseConfig::from_env()).await
}

async fn run_pending_events_on_with_lease(
    rt: &KhiveRuntime,
    server: &KhiveMcpServer,
    verbose: bool,
    lease: DispatchLeaseConfig,
) -> Result<DrainSummary> {
    let now = Utc::now();
    let grace = fire_grace_from_env();
    let mut summary = DrainSummary::default();

    // ── Step 0: reconcile claims whose renewable lease expired ───────────
    // A durable succeeded/failed outcome is finalized without invoking the
    // action again. An `invoking` receipt has an ambiguous crash boundary and
    // fails closed instead of risking a duplicate side effect. Only legacy
    // pre-receipt claims retain the historical pending retry behavior.
    let reclaimed = reclaim_stale_firing_events(rt, now.timestamp_micros()).await?;
    summary.reclaimed = reclaimed.rows;
    summary.outcomes_persisted += reclaimed.outcomes_persisted;
    summary.fired += reclaimed.fired;
    summary.advanced += reclaimed.advanced;
    summary.retry_pending += reclaimed.retry_pending;
    summary.indeterminate += reclaimed.indeterminate;
    summary.finalized += reclaimed.finalized;
    summary.failed += reclaimed.failed;
    if verbose && summary.reclaimed > 0 {
        eprintln!(
            "[pending-events] reconciled {} expired \"firing\" row(s)",
            summary.reclaimed
        );
    }

    // ── Step 1: discover all distinct namespaces with pending scheduled_event notes ──
    let namespaces = discover_pending_namespaces(rt, now).await?;

    if verbose {
        eprintln!(
            "[pending-events] scan: now={}, namespaces_with_pending={}",
            now.to_rfc3339(),
            namespaces.len()
        );
    }

    // ── Step 2: per-namespace drain ──────────────────────────────────────────
    for ns_str in &namespaces {
        if let Err(e) = Namespace::parse(ns_str) {
            if verbose {
                eprintln!("[pending-events] skip invalid namespace {ns_str:?}: {e}");
            }
            continue;
        }

        // Bounded, mutation-immune keyset pagination: the due-ness predicate
        // (`trigger_at <= now`) runs in SQL directly so future events are
        // never fetched, and pages advance on the immutable `(created_at,
        // id)` keyset rather than `LIMIT/OFFSET`, so a row mutated between
        // pages can never shift a later page's boundary. See "Keyset
        // pagination and due-ness comparison" in
        // `crates/khive-mcp/docs/pending-events.md`.
        const PAGE_SIZE: u32 = 200;
        let now_rfc = now.to_rfc3339();
        let mut cursor: Option<(i64, String)> = None;
        loop {
            let (sql, params): (String, Vec<SqlValue>) = match &cursor {
                // Due-ness compares via SQLite's `datetime()`, not a raw
                // string `<=`: stored `trigger_at` values are not normalized
                // to UTC, so a raw lexicographic compare mis-ranks non-UTC
                // offsets. `datetime()` returns NULL for an unparseable
                // value; the `OR ... IS NULL` clause keeps such a row in the
                // candidate set instead of silently dropping it.
                None => (
                    "SELECT id, content, properties, created_at FROM notes \
                     WHERE namespace = ?1 AND kind = 'scheduled_event' \
                       AND deleted_at IS NULL \
                       AND json_extract(properties, '$.status') = 'pending' \
                       AND ( \
                         datetime(json_extract(properties, '$.trigger_at')) <= datetime(?2) \
                         OR datetime(json_extract(properties, '$.trigger_at')) IS NULL \
                       ) \
                     ORDER BY created_at ASC, id ASC LIMIT ?3"
                        .to_string(),
                    vec![
                        SqlValue::Text(ns_str.clone()),
                        SqlValue::Text(now_rfc.clone()),
                        SqlValue::Integer(i64::from(PAGE_SIZE)),
                    ],
                ),
                Some((c_created_at, c_id)) => (
                    "SELECT id, content, properties, created_at FROM notes \
                     WHERE namespace = ?1 AND kind = 'scheduled_event' \
                       AND deleted_at IS NULL \
                       AND json_extract(properties, '$.status') = 'pending' \
                       AND ( \
                         datetime(json_extract(properties, '$.trigger_at')) <= datetime(?2) \
                         OR datetime(json_extract(properties, '$.trigger_at')) IS NULL \
                       ) \
                       AND (created_at > ?3 OR (created_at = ?3 AND id > ?4)) \
                     ORDER BY created_at ASC, id ASC LIMIT ?5"
                        .to_string(),
                    vec![
                        SqlValue::Text(ns_str.clone()),
                        SqlValue::Text(now_rfc.clone()),
                        SqlValue::Integer(*c_created_at),
                        SqlValue::Text(c_id.clone()),
                        SqlValue::Integer(i64::from(PAGE_SIZE)),
                    ],
                ),
            };

            let rows = {
                let mut reader = rt
                    .sql()
                    .reader()
                    .await
                    .context("pending-events: open SQL reader for candidate page")?;
                reader
                    .query_all(SqlStatement {
                        sql,
                        params,
                        label: Some("pending_events_candidate_page".into()),
                    })
                    .await
                    .with_context(|| {
                        format!("pending-events: candidate page query failed for ns={ns_str}")
                    })?
            };

            let page_len = rows.len();
            if page_len == 0 {
                break;
            }

            for row in &rows {
                let id_str = match row.get("id") {
                    Some(SqlValue::Text(s)) => s.clone(),
                    other => {
                        if verbose {
                            eprintln!(
                                "[pending-events] skip row with unexpected id column {other:?}"
                            );
                        }
                        continue;
                    }
                };
                let row_created_at = match row.get("created_at") {
                    Some(SqlValue::Integer(v)) => *v,
                    other => {
                        if verbose {
                            eprintln!(
                                "[pending-events] skip row {id_str}: unexpected created_at \
                                 column {other:?}"
                            );
                        }
                        continue;
                    }
                };
                // Advance the cursor even when this row fails downstream
                // parsing/processing below: the cursor is a pure positional
                // marker over `(created_at, id)`, not a per-row success
                // marker, so a single malformed row can never wedge the pass
                // by being re-fetched on every subsequent page query.
                cursor = Some((row_created_at, id_str.clone()));

                let id = match uuid::Uuid::parse_str(&id_str) {
                    Ok(u) => u,
                    Err(e) => {
                        if verbose {
                            eprintln!("[pending-events] skip row: unparseable id {id_str:?}: {e}");
                        }
                        continue;
                    }
                };
                let properties: Option<Value> = match row.get("properties") {
                    Some(SqlValue::Text(s)) => match serde_json::from_str(s) {
                        Ok(v) => Some(v),
                        Err(e) => {
                            if verbose {
                                eprintln!(
                                    "[pending-events] skip note {id}: unparseable properties: {e}"
                                );
                            }
                            continue;
                        }
                    },
                    Some(SqlValue::Null) | None => None,
                    other => {
                        if verbose {
                            eprintln!(
                                "[pending-events] skip note {id}: unexpected properties column \
                                 {other:?}"
                            );
                        }
                        continue;
                    }
                };
                let content = match row.get("content") {
                    Some(SqlValue::Text(s)) => s.clone(),
                    other => {
                        tracing::error!(
                            scheduled_event_id = %id,
                            content_column = ?other,
                            "pending-events: scheduled event has invalid content"
                        );
                        summary.failed += 1;
                        continue;
                    }
                };

                summary.scanned += 1;

                let trigger_at_str = properties
                    .as_ref()
                    .and_then(|p| p.get("trigger_at"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                // Parsed as `DateTime<FixedOffset>`, not straight to
                // `DateTime<Utc>`, so the caller's original offset survives
                // repeat advancement instead of being silently rewritten to
                // UTC. Uses the relaxed RFC 3339 grammar matching the write
                // boundary, not the strict parser, since already-persisted
                // strings may use the relaxed form. See "Offset preservation
                // and relaxed RFC 3339 parsing" in
                // `crates/khive-mcp/docs/pending-events.md`.
                let trigger_at_fixed = match trigger_at_str.parse::<DateTime<FixedOffset>>() {
                    Ok(dt) => dt,
                    Err(_) => {
                        if verbose {
                            eprintln!(
                                "[pending-events] skip note {id}: unparseable trigger_at {trigger_at_str:?}"
                            );
                        }
                        summary.skipped_not_due += 1;
                        continue;
                    }
                };
                let trigger_at = trigger_at_fixed.with_timezone(&Utc);
                let trigger_offset = *trigger_at_fixed.offset();
                // Owned copy of the exact bytes this page snapshot saw, so the
                // claim below can fence on them however `properties` is
                // borrowed or moved in between.
                let snapshot_trigger_at = trigger_at_str.to_string();

                if trigger_at > now {
                    summary.skipped_not_due += 1;
                    continue;
                }

                // ── Missed-event grace policy (ADR-106 amendment) ─────────
                // An event overdue by more than `grace` is never dispatched:
                // agent-facing side effects (outbound mail, spawned actions)
                // must not fire late en masse after a daemon outage or a
                // first boot against a large stale backlog. See the
                // module-level "Missed-event policy" docs.
                let overdue = now.signed_duration_since(trigger_at);
                let is_missed = overdue > grace;

                // ── Determine what to dispatch ───────────────────────────
                let event_type = properties
                    .as_ref()
                    .and_then(|p| p.get("event_type"))
                    .and_then(Value::as_str)
                    .unwrap_or("remind");

                // Resolve replay/delivery authority only from the immutable
                // pack-written provenance event. `created_by_actor` remains
                // display metadata and never an authority source; the generic
                // KG mutation fence separately prevents a valid provenance
                // record from authorizing rewritten executable intent.
                // Generic actions require provenance even on the missed path
                // (legacy rows fail closed before any lifecycle transition).
                // Reminders also resolve provenance when missed: although no
                // delivery occurs, the durable receipt must still identify
                // the creator rather than whichever daemon happened to run
                // the grace-policy transition. Only genuinely legacy rows
                // without a provenance event use the scheduler fallback.
                let creator = match verified_creator_for_event(rt, ns_str, id, event_type).await {
                    Ok(actor) => actor,
                    Err(e) => {
                        if verbose {
                            eprintln!(
                                "[pending-events] creator provenance lookup failed for note \
                                 {id}: {e}"
                            );
                        }
                        summary.failed += 1;
                        continue;
                    }
                };
                let reminder_actor = if event_type == "remind" && !is_missed {
                    match creator.as_ref() {
                        Some(actor) => Some(actor.recipient_id.clone()),
                        None => {
                            // Compatibility for reminders written before
                            // immutable provenance existed: deliver only to
                            // the configured daemon owner. Never honor the
                            // row's forgeable `created_by_actor` claim.
                            let fallback = server
                                .actor_id()
                                .filter(|actor| !actor.trim().is_empty())
                                .unwrap_or("local")
                                .to_string();
                            tracing::warn!(
                                scheduled_event_id = %id,
                                fallback_actor = %fallback,
                                "pending-events: reminder lacks immutable creator provenance; \
                                 ignoring note actor metadata and using the scheduler actor"
                            );
                            Some(fallback)
                        }
                    }
                } else {
                    None
                };
                let action_dsl: Option<String> = if is_missed {
                    None
                } else if event_type == "schedule" {
                    properties
                        .as_ref()
                        .and_then(|p| p.get("payload"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                } else {
                    reminder_actor
                        .as_deref()
                        .map(|actor| reminder_delivery_action(actor, &content))
                };

                // ── Determine repeat (read before claim) ──
                // This value gates ADMISSION only — which finalize branch is
                // eligible. It must never reach anything WRITTEN: the value the
                // finalizer schedules from is re-derived at the write, from the
                // same fresh read the CAS is guarded on. See the re-derivation
                // just before `final_properties_after_dispatch`.
                let repeat = properties
                    .as_ref()
                    .and_then(|p| p.get("repeat"))
                    .and_then(Value::as_str)
                    .map(str::to_string);

                // `properties` above is a page-query snapshot; CAS-claim
                // pending -> firing now so a concurrent `schedule.cancel`
                // cannot land between the read and this point (whichever
                // side wins the CAS proceeds; the loser skips). The same
                // claim gates the missed path too. The claim also fences on
                // the snapshot's `trigger_at`, so a writer that reschedules
                // the event in that same window makes the claim a no-op
                // instead of stamping this occurrence id onto a row that is
                // now scheduled for a different instant.
                let occurrence_id = dispatch_occurrence_id(id, trigger_at);
                let receipt_actor = creator
                    .as_ref()
                    .map(|creator| creator.audit_actor.clone())
                    .unwrap_or_else(|| {
                        if event_type == "remind" {
                            server
                                .actor_id()
                                .filter(|actor| !actor.trim().is_empty())
                                .map(|actor| format!("actor:{actor}"))
                                .unwrap_or_else(|| "anonymous:local".to_string())
                        } else {
                            "anonymous:local".to_string()
                        }
                    });
                #[cfg(test)]
                race_seam::pause_before_claim().await;
                let claim = match claim_pending_event(
                    rt,
                    ns_str,
                    id,
                    occurrence_id,
                    &snapshot_trigger_at,
                    &receipt_actor,
                    lease,
                )
                .await
                {
                    Ok(c) => c,
                    Err(e) => {
                        if verbose {
                            eprintln!("[pending-events] claim failed for note {id}: {e}");
                        }
                        summary.failed += 1;
                        continue;
                    }
                };
                let Some(claim) = claim else {
                    if verbose {
                        eprintln!(
                            "[pending-events] skip note {id}: no longer pending (concurrent \
                             cancel or claim)"
                        );
                    }
                    summary.skipped_race += 1;
                    continue;
                };

                if repeat
                    .as_deref()
                    .is_some_and(|repeat| !matches!(repeat, "daily" | "weekly" | "monthly"))
                {
                    let error = "scheduled event uses an unsupported repeat expression; only daily, weekly, and monthly are executable";
                    summary.failed += 1;
                    let Some(expected_properties) =
                        current_properties_for_finalize(rt, ns_str, id, "unsupported-repeat").await
                    else {
                        continue;
                    };
                    let Some(mut props) = expected_properties_value(&expected_properties, id)
                    else {
                        continue;
                    };
                    props["status"] = json!("failed");
                    let (error_key, error_at_key) = dispatch_error_property_keys(&props);
                    props[error_key] = json!(error);
                    props[error_at_key] = json!(Utc::now().to_rfc3339());
                    let completed_at = Utc::now().timestamp_micros();
                    props["dispatch_receipt"] = claim.completed_without_invocation_receipt(
                        DispatchReceiptState::NotInvoked,
                        completed_at,
                        Some(error),
                    );
                    match finalize_fired_event(
                        rt,
                        ns_str,
                        id,
                        &props,
                        completed_at,
                        &claim,
                        &expected_properties,
                    )
                    .await
                    {
                        Ok(true) => summary.finalized += 1,
                        Ok(false) => summary.skipped_race += 1,
                        Err(error) => tracing::error!(
                            scheduled_event_id = %id,
                            error = %error,
                            "pending-events: unsupported-repeat finalization failed"
                        ),
                    }
                    continue;
                }

                // Generic scheduled actions must replay as the creator from
                // immutable pack provenance, never as the daemon and never
                // from caller-editable note properties. Legacy/hand-written
                // rows cannot satisfy that identity fence, so fail closed.
                if event_type == "schedule" && creator.is_none() {
                    let error = "scheduled action is missing immutable creator provenance; row cannot be replayed safely";
                    tracing::error!(
                        scheduled_event_id = %id,
                        "pending-events: refusing unattributed scheduled action replay"
                    );
                    if verbose {
                        eprintln!("[pending-events] dispatch refused for note {id}: {error}");
                    }
                    summary.failed += 1;
                    let Some(expected_properties) =
                        current_properties_for_finalize(rt, ns_str, id, "failed-identity").await
                    else {
                        continue;
                    };
                    let Some(mut props) = expected_properties_value(&expected_properties, id)
                    else {
                        continue;
                    };
                    props["status"] = json!("failed");
                    props["dispatch_error"] = json!(error);
                    props["dispatch_failed_at"] = json!(Utc::now().to_rfc3339());
                    let updated_at = Utc::now().timestamp_micros();
                    props["dispatch_receipt"] = claim.completed_without_invocation_receipt(
                        DispatchReceiptState::NotInvoked,
                        updated_at,
                        Some(error),
                    );
                    match finalize_fired_event(
                        rt,
                        ns_str,
                        id,
                        &props,
                        updated_at,
                        &claim,
                        &expected_properties,
                    )
                    .await
                    {
                        Ok(true) => summary.finalized += 1,
                        Ok(false) => tracing::error!(
                            scheduled_event_id = %id,
                            "pending-events: failed-identity finalization lost its firing claim"
                        ),
                        Err(e) => tracing::error!(
                            scheduled_event_id = %id,
                            error = %e,
                            "pending-events: failed-identity finalization failed"
                        ),
                    }
                    continue;
                }

                if is_missed {
                    // ── Missed path: never dispatch. Mark terminally
                    // "missed", or (for a repeat) re-arm past every
                    // accumulated occurrence to the next future one — no
                    // catch-up bursts. ─────────────────────────────────────
                    if verbose {
                        eprintln!(
                            "[pending-events] note {id} overdue by {}s (grace {}s): marking \
                             missed, not dispatching",
                            overdue.num_seconds(),
                            grace.num_seconds()
                        );
                    }
                    let Some(expected_properties) =
                        current_properties_for_finalize(rt, ns_str, id, "missed").await
                    else {
                        summary.failed += 1;
                        continue;
                    };
                    let Some(mut props) = expected_properties_value(&expected_properties, id)
                    else {
                        summary.failed += 1;
                        continue;
                    };
                    props["missed_at"] = json!(now.timestamp_micros());
                    match advance_repeat_past_missed(&repeat, trigger_at, now) {
                        Some(next_at) => {
                            // Repeating event: skip this occurrence, re-arm
                            // pending at the next future one, rendered at the
                            // original offset, not UTC.
                            props["trigger_at"] =
                                json!(next_at.with_timezone(&trigger_offset).to_rfc3339());
                            props["status"] = json!("pending");
                        }
                        None => {
                            // Non-repeating: terminal "missed". Unsupported
                            // recurrence was rejected before this path.
                            // `fired_at` stays null/untouched.
                            props["status"] = json!("missed");
                        }
                    }
                    let updated_at = Utc::now().timestamp_micros();
                    props["dispatch_receipt"] = claim.completed_without_invocation_receipt(
                        DispatchReceiptState::Missed,
                        updated_at,
                        None,
                    );

                    match finalize_fired_event(
                        rt,
                        ns_str,
                        id,
                        &props,
                        updated_at,
                        &claim,
                        &expected_properties,
                    )
                    .await
                    {
                        Ok(true) => {
                            summary.missed.push(id);
                            summary.finalized += 1;
                        }
                        Ok(false) => {
                            if verbose {
                                eprintln!(
                                    "[pending-events] finalize no-op for {id}: row no longer in \
                                     \"firing\" state"
                                );
                            }
                            summary.failed += 1;
                        }
                        Err(e) => {
                            if verbose {
                                eprintln!("[pending-events] finalize failed for {id}: {e}");
                            }
                            summary.failed += 1;
                        }
                    }
                    continue;
                }

                // ── Dispatch the action ──────────────────────────────────
                let dispatch_actor = if event_type == "schedule" {
                    creator.clone().expect("checked above").request_actor
                } else {
                    match creator.clone() {
                        Some(creator) => creator.request_actor,
                        None => server.actor_id().and_then(|actor| {
                            (!actor.trim().is_empty()).then(|| {
                                VerifiedActor::new(actor.to_string())
                                    .expect("non-blank scheduler actor was prevalidated")
                            })
                        }),
                    }
                };
                let Some(dsl) = action_dsl.as_deref() else {
                    let error = "scheduled event has no executable payload";
                    tracing::error!(
                        scheduled_event_id = %id,
                        event_type,
                        "pending-events: refusing empty scheduled-event dispatch"
                    );
                    summary.failed += 1;
                    let Some(expected_properties) =
                        current_properties_for_finalize(rt, ns_str, id, "empty-payload").await
                    else {
                        continue;
                    };
                    let Some(mut props) = expected_properties_value(&expected_properties, id)
                    else {
                        continue;
                    };
                    let (error_key, error_at_key) = dispatch_error_property_keys(&props);
                    props[error_key] = json!(error);
                    props[error_at_key] = json!(Utc::now().to_rfc3339());
                    props["status"] = json!("failed");
                    let completed_at = Utc::now().timestamp_micros();
                    props["dispatch_receipt"] = claim.completed_without_invocation_receipt(
                        DispatchReceiptState::NotInvoked,
                        completed_at,
                        Some(error),
                    );
                    match finalize_fired_event(
                        rt,
                        ns_str,
                        id,
                        &props,
                        completed_at,
                        &claim,
                        &expected_properties,
                    )
                    .await
                    {
                        Ok(true) => summary.finalized += 1,
                        Ok(false) => summary.skipped_race += 1,
                        Err(error) => tracing::error!(
                            scheduled_event_id = %id,
                            error = %error,
                            "pending-events: empty-payload finalization failed"
                        ),
                    }
                    continue;
                };
                if event_type == "schedule" && stored_action_is_non_single(dsl) {
                    let error = "scheduled action contains multiple operations or a chain; legacy batches are not replayable because partial success cannot be retried safely";
                    tracing::error!(
                        scheduled_event_id = %id,
                        "pending-events: refusing non-single scheduled action"
                    );
                    summary.failed += 1;
                    let Some(expected_properties) =
                        current_properties_for_finalize(rt, ns_str, id, "non-single-action").await
                    else {
                        continue;
                    };
                    let Some(mut props) = expected_properties_value(&expected_properties, id)
                    else {
                        continue;
                    };
                    props["dispatch_error"] = json!(error);
                    props["dispatch_failed_at"] = json!(Utc::now().to_rfc3339());
                    props["status"] = json!("failed");
                    let completed_at = Utc::now().timestamp_micros();
                    props["dispatch_receipt"] = claim.completed_without_invocation_receipt(
                        DispatchReceiptState::NotInvoked,
                        completed_at,
                        Some(error),
                    );
                    match finalize_fired_event(
                        rt,
                        ns_str,
                        id,
                        &props,
                        completed_at,
                        &claim,
                        &expected_properties,
                    )
                    .await
                    {
                        Ok(true) => summary.finalized += 1,
                        Ok(false) => summary.skipped_race += 1,
                        Err(error) => tracing::error!(
                            scheduled_event_id = %id,
                            error = %error,
                            "pending-events: non-single-action finalization failed"
                        ),
                    }
                    continue;
                }
                match mark_dispatch_invoking(rt, ns_str, id, &claim, lease).await {
                    Ok(true) => {}
                    Ok(false) => {
                        summary.failed += 1;
                        continue;
                    }
                    Err(error) => {
                        tracing::error!(
                            scheduled_event_id = %id,
                            error = %error,
                            "pending-events: could not persist invocation-start receipt"
                        );
                        summary.failed += 1;
                        continue;
                    }
                }

                summary.invoked += 1;
                let (completion, persisted_outcome) = dispatch_with_renewable_lease(
                    DispatchLeaseTarget {
                        rt,
                        namespace: ns_str,
                        scheduled_event_id: id,
                        claim: &claim,
                    },
                    lease,
                    dsl,
                    dispatch_actor,
                    server,
                    verbose,
                )
                .await;
                let completion_error = match &completion {
                    DispatchCompletion::Succeeded => None,
                    DispatchCompletion::Failed(error)
                    | DispatchCompletion::Indeterminate(error) => {
                        tracing::error!(
                            scheduled_event_id = %id,
                            event_type,
                            recipient_actor = reminder_actor.as_deref(),
                            error = %error,
                            "pending-events: scheduled event delivery failed"
                        );
                        summary.failed += 1;
                        Some(error.as_str().to_string())
                    }
                };

                // The dispatch helper keeps lease renewal active through this
                // outcome write. No secondary audit await occurs before it.
                let receipt = match persisted_outcome {
                    Ok(Some(receipt)) => {
                        summary.outcomes_persisted += 1;
                        receipt
                    }
                    Ok(None) => {
                        summary.failed += 1;
                        continue;
                    }
                    Err(error) => {
                        tracing::error!(
                            scheduled_event_id = %id,
                            error = %error,
                            "pending-events: dispatch outcome receipt persistence failed"
                        );
                        summary.failed += 1;
                        continue;
                    }
                };
                if event_type == "remind" {
                    if let Some(error) = completion_error.as_deref() {
                        append_reminder_delivery_failure_event(
                            server,
                            ns_str,
                            id,
                            reminder_actor.as_deref().unwrap_or("local"),
                            error,
                        )
                        .await;
                    }
                }
                // Re-read the row's CURRENT properties immediately before
                // finalizing, and guard the terminal write on exact equality
                // to that read (mirroring `finalize_corrupt_receipt`'s
                // `selected_properties` guard, #7 in the RMW census). Dispatch
                // may have run for an arbitrary duration and this same process
                // may have renewed the lease meanwhile, so the pre-dispatch
                // `properties` snapshot captured at claim time is expected to
                // have moved; only a read taken right here — after this
                // process's own intervening writes have already landed —
                // can distinguish "nothing else touched this row since I last
                // looked" from a genuine concurrent writer.
                //
                // The race seam parks HERE, not before the claim: a test that
                // pauses earlier lands its concurrent write before the
                // candidate-page snapshot is taken, so the page already carries
                // that write and the test passes whether finalization rebuilds
                // from the stale page or from this fresh read. Parked here, the
                // write is genuinely between the claim and this read, which is
                // the only window that separates the two behaviours.
                #[cfg(test)]
                race_seam::pause_before_finalize_read().await;
                let expected_properties = match current_note_properties_text(rt, ns_str, id).await {
                    Ok(Some(text)) => text,
                    Ok(None) => {
                        summary.failed += 1;
                        continue;
                    }
                    Err(error) => {
                        tracing::error!(
                            scheduled_event_id = %id,
                            error = %error,
                            "pending-events: could not read current properties before finalization"
                        );
                        summary.failed += 1;
                        continue;
                    }
                };
                let Some(expected_value) = expected_properties_value(&expected_properties, id)
                else {
                    summary.failed += 1;
                    continue;
                };
                // Re-derive the SCHEDULING inputs from the fresh read too, not
                // just the properties blob. `trigger_at`, `trigger_offset` and
                // `repeat` above came from the pre-claim page snapshot, and
                // guarding the write on the fresh properties text protects the
                // blob while still letting a stale scheduling decision be
                // computed from it: `final_properties_after_dispatch` uses
                // these three to write the next `trigger_at` and the terminal
                // `status`. A writer that changed `repeat` or `trigger_at`
                // between the page snapshot and the fresh read would have its
                // value retained as the CAS base and then immediately
                // contradicted by a next-occurrence computed from the value it
                // replaced.
                //
                // The earlier values keep their job: they gate ADMISSION (is
                // this due, is it inside the grace window), which is a decision
                // about whether to dispatch at all and is correctly made from
                // what was observed before the claim. What must not come from
                // them is anything WRITTEN.
                let trigger_at_fresh_str = expected_value
                    .get("trigger_at")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let (trigger_at, trigger_offset) = match trigger_at_fresh_str
                    .parse::<DateTime<FixedOffset>>()
                {
                    Ok(fixed) => (fixed.with_timezone(&Utc), *fixed.offset()),
                    Err(_) => {
                        // The row's own `trigger_at` stopped being parseable
                        // between the page read and here. Refuse rather than
                        // fall back to the stale pair: falling back is
                        // exactly the silent-overwrite this guard exists to
                        // prevent, and the dispatch has already happened, so
                        // the honest outcome is a failed finalization that
                        // recovery will re-examine.
                        tracing::error!(
                            scheduled_event_id = %id,
                            trigger_at = %trigger_at_fresh_str,
                            "pending-events: trigger_at not parseable at finalization; refusing \
                             to finalize from the pre-claim snapshot"
                        );
                        summary.failed += 1;
                        continue;
                    }
                };
                // The receipt persisted at claim time names an occurrence
                // derived from the trigger the page query saw. If the row is
                // now scheduled for a different instant, writing a terminal row
                // would pair that receipt with a trigger it does not describe —
                // and terminal rows are past the reach of recovery, whose scan
                // fences on `status = 'firing'`, so nothing would ever
                // re-examine it. Refuse for the same reason and in the same
                // shape as the unparseable-trigger branch above: the dispatch
                // has happened, so the honest outcome is a failed finalization
                // that leaves the row `firing` for the receipt validator to
                // adjudicate once the lease expires.
                let fresh_occurrence_id = dispatch_occurrence_id(id, trigger_at);
                if fresh_occurrence_id != claim.occurrence_id {
                    tracing::error!(
                        scheduled_event_id = %id,
                        trigger_at = %trigger_at_fresh_str,
                        claimed_occurrence_id = %claim.occurrence_id,
                        fresh_occurrence_id = %fresh_occurrence_id,
                        "pending-events: the event was rescheduled after its dispatch was \
                         claimed; refusing to finalize a terminal row whose receipt names a \
                         different occurrence"
                    );
                    summary.failed += 1;
                    continue;
                }

                let repeat = expected_value
                    .get("repeat")
                    .and_then(Value::as_str)
                    .map(str::to_string);

                let (final_props, disposition) = final_properties_after_dispatch(
                    expected_value,
                    receipt,
                    &completion,
                    trigger_at,
                    trigger_offset,
                    &repeat,
                );
                match finalize_fired_event(
                    rt,
                    ns_str,
                    id,
                    &final_props,
                    Utc::now().timestamp_micros(),
                    &claim,
                    &expected_properties,
                )
                .await
                {
                    Ok(true) => apply_final_disposition(&mut summary, disposition),
                    Ok(false) => summary.failed += 1,
                    Err(error) => {
                        tracing::error!(
                            scheduled_event_id = %id,
                            error = %error,
                            "pending-events: finalization failed after durable outcome"
                        );
                        summary.failed += 1;
                    }
                }
            }

            if page_len < PAGE_SIZE as usize {
                break;
            }
        }
    }

    Ok(summary)
}

fn dispatch_occurrence_id(event_id: uuid::Uuid, trigger_at: DateTime<Utc>) -> uuid::Uuid {
    uuid::Uuid::new_v5(&event_id, trigger_at.to_rfc3339().as_bytes())
}

fn receipt_timestamp(receipt: &Value, field: &str) -> std::result::Result<i64, String> {
    let value = receipt
        .get(field)
        .and_then(Value::as_i64)
        .ok_or_else(|| format!("dispatch receipt {field} is missing or not an integer"))?;
    DateTime::<Utc>::from_timestamp_micros(value).ok_or_else(|| {
        format!("dispatch receipt {field} is outside the supported timestamp range")
    })?;
    Ok(value)
}

fn validate_dispatch_receipt(
    event_id: uuid::Uuid,
    firing_at: i64,
    properties: &Value,
    receipt: Value,
) -> std::result::Result<ValidatedDispatchReceipt, String> {
    if !receipt.is_object() {
        return Err("dispatch receipt is not an object".to_string());
    }
    if properties.get("firing_at").and_then(Value::as_i64) != Some(firing_at) {
        return Err("dispatch firing claim timestamp is missing or malformed".to_string());
    }
    if receipt.get("version").and_then(Value::as_u64) != Some(DISPATCH_RECEIPT_VERSION) {
        return Err("dispatch receipt version is missing or unsupported".to_string());
    }

    let occurrence_id = receipt
        .get("occurrence_id")
        .and_then(Value::as_str)
        .and_then(|value| uuid::Uuid::parse_str(value).ok())
        .ok_or_else(|| "dispatch receipt occurrence_id is missing or malformed".to_string())?;
    let invocation_id = receipt
        .get("invocation_id")
        .and_then(Value::as_str)
        .and_then(|value| uuid::Uuid::parse_str(value).ok())
        .ok_or_else(|| "dispatch receipt invocation_id is missing or malformed".to_string())?;
    let actor = receipt
        .get("actor")
        .and_then(Value::as_str)
        .filter(|actor| {
            *actor == "anonymous:local"
                || actor
                    .strip_prefix("actor:")
                    .is_some_and(|identity| !identity.trim().is_empty())
        })
        .ok_or_else(|| "dispatch receipt actor is missing or malformed".to_string())?
        .to_string();
    let claimed_at = receipt_timestamp(&receipt, "claimed_at")?;
    if claimed_at != firing_at {
        return Err("dispatch receipt claimed_at does not match the firing claim".to_string());
    }

    let trigger_at = properties
        .get("trigger_at")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            "scheduled event trigger_at is missing during receipt validation".to_string()
        })?
        .parse::<DateTime<FixedOffset>>()
        .map_err(|_| {
            "scheduled event trigger_at is malformed during receipt validation".to_string()
        })?
        .with_timezone(&Utc);
    if occurrence_id != dispatch_occurrence_id(event_id, trigger_at) {
        return Err(
            "dispatch receipt occurrence_id does not match the event and scheduled instant"
                .to_string(),
        );
    }

    let state = receipt
        .get("state")
        .and_then(Value::as_str)
        .and_then(DispatchReceiptState::parse)
        .ok_or_else(|| "dispatch receipt state is missing or unsupported".to_string())?;
    match state {
        DispatchReceiptState::Claimed => {
            if receipt
                .get("error_payload")
                .is_some_and(|payload| !payload.is_null())
            {
                return Err(
                    "dispatch receipt state claimed cannot carry an error payload".to_string(),
                );
            }
        }
        DispatchReceiptState::Invoking => {
            receipt_timestamp(&receipt, "invocation_started_at")?;
            if receipt
                .get("error_payload")
                .is_some_and(|payload| !payload.is_null())
            {
                return Err(
                    "dispatch receipt state invoking cannot carry an error payload".to_string(),
                );
            }
        }
        DispatchReceiptState::Succeeded | DispatchReceiptState::Missed => {
            receipt_timestamp(&receipt, "completed_at")?;
            if receipt.get("error") != Some(&Value::Null) {
                return Err(format!(
                    "dispatch receipt state {} requires error=null",
                    state.as_str()
                ));
            }
            if receipt
                .get("error_payload")
                .is_some_and(|payload| !payload.is_null())
            {
                return Err(format!(
                    "dispatch receipt state {} cannot carry an error payload",
                    state.as_str()
                ));
            }
        }
        DispatchReceiptState::Failed | DispatchReceiptState::Indeterminate => {
            receipt_timestamp(&receipt, "completed_at")?;
            if receipt
                .get("error")
                .and_then(Value::as_str)
                .is_none_or(|error| error.trim().is_empty())
            {
                return Err(format!(
                    "dispatch receipt state {} requires a non-empty error",
                    state.as_str()
                ));
            }
        }
        DispatchReceiptState::NotInvoked => {
            receipt_timestamp(&receipt, "completed_at")?;
            if receipt
                .get("error")
                .and_then(Value::as_str)
                .is_none_or(|error| error.trim().is_empty())
            {
                return Err(
                    "dispatch receipt state not_invoked requires a non-empty error".to_string(),
                );
            }
            if receipt
                .get("error_payload")
                .is_some_and(|payload| !payload.is_null())
            {
                return Err(
                    "dispatch receipt state not_invoked cannot carry an action error payload"
                        .to_string(),
                );
            }
        }
    }

    Ok(ValidatedDispatchReceipt {
        value: receipt,
        occurrence_id,
        invocation_id,
        actor,
        state,
    })
}

/// CAS-claim a pending scheduled event and atomically persist the occurrence
/// and invocation identity before any action future can be polled.
///
/// `expected_trigger_at` is the raw `trigger_at` string the caller's page
/// snapshot saw, and the claim refuses unless the row still carries those exact
/// bytes. That is what keeps the persisted receipt's `occurrence_id` — derived
/// from the snapshot's instant — describing the same occurrence the row is
/// scheduled for. Without it a writer landing between the page query and this
/// claim reschedules the event while the claim stamps the old occurrence onto
/// it, and the resulting terminal row fails receipt validation and is
/// quarantined as indeterminate rather than read as the dispatch it was.
/// A refusal costs nothing: the row stays `pending` and the next drain picks it
/// up from the value the writer actually left. The comparison is on bytes, not
/// on the parsed instant, so a rewrite to a different spelling of the same
/// instant also refuses; that is stricter than the invariant strictly needs and
/// the extra refusals cost one drain interval each.
async fn claim_pending_event(
    rt: &KhiveRuntime,
    namespace: &str,
    id: uuid::Uuid,
    occurrence_id: uuid::Uuid,
    expected_trigger_at: &str,
    actor: &str,
    lease: DispatchLeaseConfig,
) -> Result<Option<DispatchClaim>> {
    let updated_at = Utc::now().timestamp_micros();
    let claim = DispatchClaim {
        firing_at: updated_at,
        occurrence_id,
        invocation_id: uuid::Uuid::new_v4(),
        actor: actor.to_string(),
    };
    let lease_expires_at = lease.expires_at(updated_at);
    let receipt = claim.claimed_receipt();
    let receipt_json = serde_json::to_string(&receipt)
        .context("pending-events: serialize dispatch claim receipt")?;
    let mut writer = rt
        .sql()
        .writer()
        .await
        .map_err(|e| anyhow::anyhow!("pending-events: open SQL writer: {e}"))?;
    let rows = writer
        .execute(SqlStatement {
            sql: "UPDATE notes \
                  SET properties = json_set( \
                        COALESCE(properties, '{}'), \
                        '$.status', 'firing', \
                        '$.firing_at', ?1, \
                        '$.lease_expires_at', ?2, \
                        '$.dispatch_receipt', json(?3) \
                      ), \
                      updated_at = ?1 \
                  WHERE id = ?4 \
                    AND namespace = ?5 \
                    AND kind = 'scheduled_event' \
                    AND deleted_at IS NULL \
                    AND json_extract(properties, '$.status') = 'pending' \
                    AND json_extract(properties, '$.trigger_at') = ?6"
                .to_string(),
            params: vec![
                SqlValue::Integer(updated_at),
                SqlValue::Integer(lease_expires_at),
                SqlValue::Text(receipt_json),
                SqlValue::Text(id.to_string()),
                SqlValue::Text(namespace.to_string()),
                SqlValue::Text(expected_trigger_at.to_string()),
            ],
            label: Some("pending_events_claim_firing".into()),
        })
        .await
        .map_err(|e| anyhow::anyhow!("pending-events: claim conditional update: {e}"))?;
    Ok((rows == 1).then_some(claim))
}

async fn mark_dispatch_invoking(
    rt: &KhiveRuntime,
    namespace: &str,
    id: uuid::Uuid,
    claim: &DispatchClaim,
    lease: DispatchLeaseConfig,
) -> Result<bool> {
    let now = Utc::now().timestamp_micros();
    let mut writer = rt
        .sql()
        .writer()
        .await
        .context("pending-events: open SQL writer for invocation receipt")?;
    let rows = writer
        .execute(SqlStatement {
            sql: "UPDATE notes \
                  SET properties = json_set( \
                        properties, \
                        '$.dispatch_receipt.state', 'invoking', \
                        '$.dispatch_receipt.invocation_started_at', ?1, \
                        '$.lease_expires_at', ?2 \
                      ), \
                      updated_at = ?1 \
                  WHERE id = ?3 \
                    AND namespace = ?4 \
                    AND kind = 'scheduled_event' \
                    AND deleted_at IS NULL \
                    AND json_extract(properties, '$.status') = 'firing' \
                    AND CAST(json_extract(properties, '$.firing_at') AS INTEGER) = ?5 \
                    AND json_extract(properties, '$.dispatch_receipt.invocation_id') = ?6 \
                    AND json_extract(properties, '$.dispatch_receipt.state') = 'claimed'"
                .to_string(),
            params: vec![
                SqlValue::Integer(now),
                SqlValue::Integer(lease.expires_at(now)),
                SqlValue::Text(id.to_string()),
                SqlValue::Text(namespace.to_string()),
                SqlValue::Integer(claim.firing_at),
                SqlValue::Text(claim.invocation_id.to_string()),
            ],
            label: Some("pending_events_mark_invoking".into()),
        })
        .await
        .context("pending-events: persist invocation-start receipt")?;
    Ok(rows == 1)
}

async fn renew_dispatch_lease(
    rt: &KhiveRuntime,
    namespace: &str,
    id: uuid::Uuid,
    claim: &DispatchClaim,
    lease: DispatchLeaseConfig,
) -> Result<bool> {
    let now = Utc::now().timestamp_micros();
    let mut writer = rt
        .sql()
        .writer()
        .await
        .context("pending-events: open SQL writer for lease renewal")?;
    let rows = writer
        .execute(SqlStatement {
            sql: "UPDATE notes \
                  SET properties = json_set(properties, '$.lease_expires_at', ?1), \
                      updated_at = ?2 \
                  WHERE id = ?3 \
                    AND namespace = ?4 \
                    AND kind = 'scheduled_event' \
                    AND deleted_at IS NULL \
                    AND json_extract(properties, '$.status') = 'firing' \
                    AND CAST(json_extract(properties, '$.firing_at') AS INTEGER) = ?5 \
                    AND json_extract(properties, '$.dispatch_receipt.invocation_id') = ?6 \
                    AND json_extract(properties, '$.dispatch_receipt.state') = 'invoking'"
                .to_string(),
            params: vec![
                SqlValue::Integer(lease.expires_at(now)),
                SqlValue::Integer(now),
                SqlValue::Text(id.to_string()),
                SqlValue::Text(namespace.to_string()),
                SqlValue::Integer(claim.firing_at),
                SqlValue::Text(claim.invocation_id.to_string()),
            ],
            label: Some("pending_events_renew_dispatch_lease".into()),
        })
        .await
        .context("pending-events: renew dispatch lease")?;
    Ok(rows == 1)
}

async fn persist_dispatch_outcome(
    rt: &KhiveRuntime,
    namespace: &str,
    id: uuid::Uuid,
    claim: &DispatchClaim,
    completion: &DispatchCompletion,
) -> Result<Option<Value>> {
    let completed_at = Utc::now().timestamp_micros();
    let (state, error, error_payload) = match completion {
        DispatchCompletion::Succeeded => ("succeeded", Value::Null, Value::Null),
        DispatchCompletion::Failed(error) => (
            "failed",
            json!(error.as_str()),
            error.payload.clone().unwrap_or(Value::Null),
        ),
        DispatchCompletion::Indeterminate(error) => (
            "indeterminate",
            json!(error.as_str()),
            error.payload.clone().unwrap_or(Value::Null),
        ),
    };
    let receipt = json!({
        "version": DISPATCH_RECEIPT_VERSION,
        "occurrence_id": claim.occurrence_id,
        "invocation_id": claim.invocation_id,
        "actor": claim.actor.as_str(),
        "state": state,
        "claimed_at": claim.firing_at,
        "completed_at": completed_at,
        "error": error,
        "error_payload": error_payload,
    });
    let receipt_json = serde_json::to_string(&receipt)
        .context("pending-events: serialize dispatch outcome receipt")?;
    let mut writer = rt
        .sql()
        .writer()
        .await
        .context("pending-events: open SQL writer for dispatch outcome")?;
    let rows = writer
        .execute(SqlStatement {
            sql: "UPDATE notes \
                  SET properties = json_set( \
                        properties, \
                        '$.dispatch_receipt', json(?1), \
                        '$.lease_expires_at', ?2 \
                      ), \
                      updated_at = ?2 \
                  WHERE id = ?3 \
                    AND namespace = ?4 \
                    AND kind = 'scheduled_event' \
                    AND deleted_at IS NULL \
                    AND json_extract(properties, '$.status') = 'firing' \
                    AND CAST(json_extract(properties, '$.firing_at') AS INTEGER) = ?5 \
                    AND json_extract(properties, '$.dispatch_receipt.invocation_id') = ?6 \
                    AND json_extract(properties, '$.dispatch_receipt.state') = 'invoking'"
                .to_string(),
            params: vec![
                SqlValue::Text(receipt_json),
                SqlValue::Integer(completed_at),
                SqlValue::Text(id.to_string()),
                SqlValue::Text(namespace.to_string()),
                SqlValue::Integer(claim.firing_at),
                SqlValue::Text(claim.invocation_id.to_string()),
            ],
            label: Some("pending_events_persist_dispatch_outcome".into()),
        })
        .await
        .context("pending-events: persist dispatch outcome")?;
    Ok((rows == 1).then_some(receipt))
}

fn completion_from_receipt(receipt: &Value) -> DispatchCompletion {
    let error = || {
        let message = receipt
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("scheduled dispatch failed without an error message")
            .to_string();
        let payload = receipt
            .get("error_payload")
            .filter(|payload| !payload.is_null())
            .cloned();
        DispatchFailure { message, payload }
    };
    match receipt.get("state").and_then(Value::as_str) {
        Some("succeeded") => DispatchCompletion::Succeeded,
        Some("failed") => DispatchCompletion::Failed(error()),
        Some("indeterminate") => DispatchCompletion::Indeterminate(error()),
        Some("claimed") => DispatchCompletion::Failed(DispatchFailure::plain(
            "dispatch claimant expired before invocation began; occurrence is retryable",
        )),
        Some("invoking") => DispatchCompletion::Indeterminate(DispatchFailure::plain(
            "dispatch lease expired without a durable outcome; refusing automatic replay because the side effect may already have occurred",
        )),
        other => DispatchCompletion::Indeterminate(DispatchFailure::plain(format!(
            "dispatch receipt has unsupported state {other:?}; refusing automatic replay"
        ))),
    }
}

fn dispatch_error_property_keys(properties: &Value) -> (&'static str, &'static str) {
    if properties
        .get("event_type")
        .and_then(Value::as_str)
        .unwrap_or("remind")
        == "remind"
    {
        ("delivery_error", "delivery_failed_at")
    } else {
        ("dispatch_error", "dispatch_failed_at")
    }
}

fn mark_dispatch_receipt_indeterminate(
    properties: &mut Value,
    invalid_receipt: Value,
    error: &str,
    completed_at: i64,
) {
    properties["dispatch_receipt"] = json!({
        "version": DISPATCH_RECEIPT_VERSION,
        "state": DispatchReceiptState::Indeterminate.as_str(),
        "completed_at": completed_at,
        "error": error,
        "error_payload": null,
        "invalid_receipt": invalid_receipt,
    });
    properties["status"] = json!("failed");
    let (error_key, error_at_key) = dispatch_error_property_keys(properties);
    properties[error_key] = json!(error);
    properties[error_at_key] = json!(Utc::now().to_rfc3339());
}

fn final_properties_after_dispatch(
    mut properties: Value,
    receipt: Value,
    completion: &DispatchCompletion,
    trigger_at: DateTime<Utc>,
    trigger_offset: FixedOffset,
    repeat: &Option<String>,
) -> (Value, FinalDisposition) {
    let completed_at = receipt
        .get("completed_at")
        .and_then(Value::as_i64)
        .and_then(DateTime::<Utc>::from_timestamp_micros)
        .unwrap_or_else(Utc::now);
    let completed_at_rfc = completed_at.to_rfc3339();
    properties["dispatch_receipt"] = receipt;
    properties["last_attempted_at"] = json!(completed_at_rfc);

    let (error_key, error_at_key) = dispatch_error_property_keys(&properties);

    match completion {
        DispatchCompletion::Succeeded => {
            if let Some(object) = properties.as_object_mut() {
                object.remove(error_key);
                object.remove(error_at_key);
            }
            properties["fired_at"] = json!(completed_at_rfc);
            match next_trigger_at(repeat, trigger_at) {
                Some(next_at) => {
                    properties["trigger_at"] =
                        json!(next_at.with_timezone(&trigger_offset).to_rfc3339());
                    properties["status"] = json!("pending");
                    (properties, FinalDisposition::Advanced)
                }
                None => {
                    properties["status"] = json!("fired");
                    (properties, FinalDisposition::Fired)
                }
            }
        }
        DispatchCompletion::Failed(error) => {
            properties[error_key] = json!(error.as_str());
            properties[error_at_key] = json!(completed_at_rfc);
            match next_trigger_at(repeat, trigger_at) {
                Some(next_at) => {
                    properties["trigger_at"] =
                        json!(next_at.with_timezone(&trigger_offset).to_rfc3339());
                    properties["status"] = json!("pending");
                    (properties, FinalDisposition::Advanced)
                }
                None => {
                    properties["status"] = json!("pending");
                    (properties, FinalDisposition::RetryPending)
                }
            }
        }
        DispatchCompletion::Indeterminate(error) => {
            properties[error_key] = json!(error.as_str());
            properties[error_at_key] = json!(completed_at_rfc);
            properties["status"] = json!("failed");
            (properties, FinalDisposition::Failed)
        }
    }
}

fn apply_final_disposition(summary: &mut DrainSummary, disposition: FinalDisposition) {
    summary.finalized += 1;
    match disposition {
        FinalDisposition::Fired => summary.fired += 1,
        FinalDisposition::Advanced => summary.advanced += 1,
        FinalDisposition::RetryPending => summary.retry_pending += 1,
        FinalDisposition::Failed => summary.indeterminate += 1,
    }
}

async fn requeue_legacy_claim(
    rt: &KhiveRuntime,
    namespace: &str,
    id: uuid::Uuid,
    firing_at: i64,
    selected_properties: &str,
) -> Result<bool> {
    let updated_at = Utc::now().timestamp_micros();
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
                (properties, FinalDisposition::Failed)
            }
        };
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
                    FinalDisposition::Failed => summary.indeterminate += 1,
                }
                if !matches!(completion, DispatchCompletion::Succeeded) {
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
/// Returns `Some(next)` for named aliases `"daily"` / `"weekly"` / `"monthly"`.
/// Returns `None` for an absent repeat. Unsupported expressions are rejected
/// by schedule creation and fail closed before dispatch for legacy rows.
fn next_trigger_at(repeat: &Option<String>, current: DateTime<Utc>) -> Option<DateTime<Utc>> {
    match repeat.as_deref() {
        Some("daily") => Some(current + Duration::days(1)),
        Some("weekly") => Some(current + Duration::weeks(1)),
        Some("monthly") => {
            // Add one calendar month. chrono::Months handles month-boundary
            // arithmetic (e.g. Jan 31 + 1 month = Feb 28/29).
            current.checked_add_months(Months::new(1))
        }
        _ => None,
    }
}

/// Advance a missed repeating event's `trigger_at` past every occurrence at
/// or before `now`, landing on the first occurrence strictly after `now`
/// (ADR-106 missed-event amendment) — avoids firing a catch-up burst.
/// Returns `None` when the event does not repeat; the caller then marks it
/// terminally `"missed"`.
/// See `crates/khive-mcp/docs/api/pending-events.md` for the termination
/// argument.
fn advance_repeat_past_missed(
    repeat: &Option<String>,
    current: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    let mut current = current;
    loop {
        let next = next_trigger_at(repeat, current)?;
        if next > now {
            return Some(next);
        }
        current = next;
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
        recipient_actor,
    )
    .with_outcome(EventOutcome::Error)
    .with_target(scheduled_event_id)
    .with_payload(json!({
        "scheduled_event_id": scheduled_event_id,
        "recipient_actor": recipient_actor,
        "error": error,
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
            Err(error) if error.outcome_uncertain => {
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

fn action_failures(failures: &[&Value]) -> DispatchActionError {
    let errors: Vec<Value> = failures
        .iter()
        .map(|failure| {
            failure
                .get("error")
                .cloned()
                .unwrap_or_else(|| (*failure).clone())
        })
        .collect();
    let outcome_uncertain = errors.iter().any(action_error_outcome_is_uncertain);
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
    if outcome_uncertain {
        DispatchActionError::uncertain(failure)
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
        DispatchActionError::known(DispatchFailure::plain(format!(
            "pending-events: action DSL parse error ({error}): {action_dsl:?}"
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
                return Err(DispatchActionError::known(DispatchFailure::plain(format!(
                    "pending-events: non-literal scheduled action argument {k:?} is not \
                     replayable: {action_dsl:?}"
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
pub(crate) mod race_seam {
    use std::sync::Arc;
    use tokio::sync::Barrier;

    /// Which of the drain's windows a gate is armed for. A gate trips at the
    /// point it names and nowhere else, so a drain that passes through both
    /// seams parks once, at the one the test asked for. Without this a test
    /// arming the earlier window would also be caught by the later one and
    /// hang waiting for a second handshake it never planned to perform.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) enum PausePoint {
        /// Between the page-query snapshot and the CAS claim.
        BeforeClaim,
        /// After claim and dispatch, immediately before the finalizer's fresh
        /// current-properties read.
        BeforeFinalizeRead,
    }

    /// Two-phase handshake: `reached` lets the driving test learn the drain
    /// task has arrived at the pause point (i.e. genuinely parked, not just
    /// scheduled) before it performs a concurrent write; `release` then lets
    /// the driving test resume the drain task only once that write has
    /// landed. A single shared `Barrier` cannot express this — both parties
    /// would resume together with no window for the test to act in between.
    #[derive(Clone)]
    pub(crate) struct PauseGate {
        pub(crate) at: PausePoint,
        pub(crate) reached: Arc<Barrier>,
        pub(crate) release: Arc<Barrier>,
    }

    tokio::task_local! {
        pub(crate) static PAUSE_GATE: PauseGate;
    }

    async fn pause_at(point: PausePoint) {
        if let Ok(gate) = PAUSE_GATE.try_with(Clone::clone) {
            if gate.at == point {
                gate.reached.wait().await;
                gate.release.wait().await;
            }
        }
    }

    pub(crate) async fn pause_before_claim() {
        pause_at(PausePoint::BeforeClaim).await;
    }

    pub(crate) async fn pause_before_finalize_read() {
        pause_at(PausePoint::BeforeFinalizeRead).await;
    }
}

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use khive_runtime::{Gate, GateDecision, GateError, GateRequest, RuntimeConfig};
    use khive_storage::event::EventFilter;
    use khive_storage::types::PageRequest;
    use khive_types::{Details, HandlerDef, KhiveError, VerbCategory, Visibility};
    use tempfile::NamedTempFile;
    use tokio_util::sync::CancellationToken;

    #[derive(Debug)]
    struct DenyCommSendGate;

    impl Gate for DenyCommSendGate {
        fn check(&self, request: &GateRequest) -> Result<GateDecision, GateError> {
            if request.verb == "comm.send" {
                Ok(GateDecision::deny(
                    "comm.send denied by delivery-failure test",
                ))
            } else {
                Ok(GateDecision::allow())
            }
        }
    }

    #[derive(Debug)]
    struct DenyCreatorCreateGate;

    impl Gate for DenyCreatorCreateGate {
        fn check(&self, request: &GateRequest) -> Result<GateDecision, GateError> {
            if request.verb == "create" && request.actor.id == "lambda:schedule-owner" {
                Ok(GateDecision::deny(
                    "creator is not authorized to replay create",
                ))
            } else {
                Ok(GateDecision::allow())
            }
        }
    }

    #[derive(Debug)]
    struct DenyAttackerCreateGate;

    impl Gate for DenyAttackerCreateGate {
        fn check(&self, request: &GateRequest) -> Result<GateDecision, GateError> {
            if request.verb == "create" && request.actor.id == "lambda:schedule-attacker" {
                Ok(GateDecision::deny(
                    "attacker is not authorized to replay create",
                ))
            } else {
                Ok(GateDecision::allow())
            }
        }
    }

    #[derive(Debug, Default)]
    struct CaptureReplayIdentityGate {
        creates: std::sync::Mutex<Vec<(String, String, String)>>,
    }

    impl Gate for CaptureReplayIdentityGate {
        fn check(&self, request: &GateRequest) -> Result<GateDecision, GateError> {
            if request.verb == "create" {
                self.creates.lock().expect("capture lock").push((
                    request.actor.kind.clone(),
                    request.actor.id.clone(),
                    request.namespace.as_str().to_string(),
                ));
            }
            Ok(GateDecision::allow())
        }
    }

    #[derive(Default)]
    struct AsyncBlockingSideEffectState {
        invocations: std::sync::atomic::AtomicUsize,
        entered: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }

    struct ReleaseAsyncBlockingVerbOnDrop(std::sync::Arc<AsyncBlockingSideEffectState>);

    impl Drop for ReleaseAsyncBlockingVerbOnDrop {
        fn drop(&mut self) {
            self.0.release.notify_one();
        }
    }

    struct AsyncBlockingSideEffectPack {
        runtime: KhiveRuntime,
        marker: String,
        state: std::sync::Arc<AsyncBlockingSideEffectState>,
    }

    impl khive_types::Pack for AsyncBlockingSideEffectPack {
        const NAME: &'static str = "async-blocking-side-effect-test";
        const NOTE_KINDS: &'static [&'static str] = &[];
        const ENTITY_KINDS: &'static [&'static str] = &[];
        const HANDLERS: &'static [HandlerDef] = &[HandlerDef {
            name: "test.async_blocking_side_effect",
            description: "wait asynchronously, then commit one marker",
            visibility: Visibility::Verb,
            category: VerbCategory::Assertive,
            params: &[],
        }];
    }

    #[async_trait::async_trait]
    impl khive_runtime::PackRuntime for AsyncBlockingSideEffectPack {
        fn name(&self) -> &str {
            <Self as khive_types::Pack>::NAME
        }

        fn note_kinds(&self) -> &'static [&'static str] {
            <Self as khive_types::Pack>::NOTE_KINDS
        }

        fn entity_kinds(&self) -> &'static [&'static str] {
            <Self as khive_types::Pack>::ENTITY_KINDS
        }

        fn handlers(&self) -> &'static [HandlerDef] {
            <Self as khive_types::Pack>::HANDLERS
        }

        async fn dispatch(
            &self,
            verb: &str,
            _params: Value,
            _registry: &khive_runtime::VerbRegistry,
            token: &khive_runtime::NamespaceToken,
        ) -> std::result::Result<Value, khive_runtime::RuntimeError> {
            debug_assert_eq!(verb, "test.async_blocking_side_effect");
            self.state
                .invocations
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.state.entered.notify_one();
            self.state.release.notified().await;
            let note = self
                .runtime
                .create_note(token, "observation", None, &self.marker, None, None, vec![])
                .await?;
            Ok(json!({"id": note.id}))
        }
    }

    #[derive(Debug, Default)]
    struct FailFirstCreateGate {
        invocations: std::sync::atomic::AtomicUsize,
    }

    impl Gate for FailFirstCreateGate {
        fn check(&self, request: &GateRequest) -> Result<GateDecision, GateError> {
            if request.verb == "create" {
                let attempt = self
                    .invocations
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if attempt == 0 {
                    return Ok(GateDecision::deny("first scheduled create fails"));
                }
            }
            Ok(GateDecision::allow())
        }
    }

    struct AmbiguousSideEffectPack {
        runtime: KhiveRuntime,
        marker: String,
        outbound_id: uuid::Uuid,
        invocations: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl khive_types::Pack for AmbiguousSideEffectPack {
        const NAME: &'static str = "ambiguous-side-effect-test";
        const NOTE_KINDS: &'static [&'static str] = &[];
        const ENTITY_KINDS: &'static [&'static str] = &[];
        const HANDLERS: &'static [HandlerDef] = &[HandlerDef {
            name: "test.ambiguous_side_effect",
            description: "commit a marker, then return a side_effects_unknown error",
            visibility: Visibility::Verb,
            category: VerbCategory::Commissive,
            params: &[],
        }];
    }

    #[async_trait::async_trait]
    impl khive_runtime::PackRuntime for AmbiguousSideEffectPack {
        fn name(&self) -> &str {
            <Self as khive_types::Pack>::NAME
        }

        fn note_kinds(&self) -> &'static [&'static str] {
            <Self as khive_types::Pack>::NOTE_KINDS
        }

        fn entity_kinds(&self) -> &'static [&'static str] {
            <Self as khive_types::Pack>::ENTITY_KINDS
        }

        fn handlers(&self) -> &'static [HandlerDef] {
            <Self as khive_types::Pack>::HANDLERS
        }

        async fn dispatch(
            &self,
            _verb: &str,
            _params: Value,
            _registry: &khive_runtime::VerbRegistry,
            token: &khive_runtime::NamespaceToken,
        ) -> std::result::Result<Value, khive_runtime::RuntimeError> {
            self.invocations
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.runtime
                .create_note(token, "observation", None, &self.marker, None, None, vec![])
                .await?;
            Err(khive_runtime::RuntimeError::Khive(
                KhiveError::conflict(format!(
                    "dual_write delivery outcome is uncertain (side_effects_unknown); \
                     call comm.delivered(id=\"{}\") before retrying",
                    self.outbound_id
                ))
                .with_details(Details::new_owned([(
                    "outbound_id",
                    self.outbound_id.to_string(),
                )])),
            ))
        }
    }

    fn tmp_db() -> (NamedTempFile, String) {
        let f = NamedTempFile::new().expect("tempfile");
        let path = f.path().to_str().expect("utf8 path").to_string();
        (f, path)
    }

    /// Due, but inside the default missed-event grace window, so callers land
    /// on the normal fire/advance path rather than the missed path.
    fn due_rfc3339() -> String {
        (Utc::now() - Duration::seconds(5)).to_rfc3339()
    }

    /// "Now" formatted like the candidate-page query's own bind parameter,
    /// for offset-sorting regressions to assert against independently.
    fn now_rfc3339_for_ordering_check() -> String {
        Utc::now().to_rfc3339()
    }

    async fn make_rt(db_path: &str) -> KhiveRuntime {
        make_rt_with_actor(db_path, None).await
    }

    async fn make_rt_with_actor(db_path: &str, actor_id: Option<&str>) -> KhiveRuntime {
        let cfg = RuntimeConfig {
            db_path: Some(std::path::PathBuf::from(db_path)),
            default_namespace: Namespace::parse("local").unwrap(),
            embedding_model: None,
            additional_embedding_models: vec![],
            actor_id: actor_id.map(str::to_string),
            // Pin the pack list explicitly rather than inheriting `KHIVE_PACKS`
            // from the ambient environment: these tests drive schedule.remind
            // / schedule.cancel through the drain path and assert delivery
            // lands in the creator's comm inbox.
            packs: vec!["kg".to_string(), "schedule".to_string(), "comm".to_string()],
            ..Default::default()
        };
        KhiveRuntime::new(cfg).expect("runtime")
    }

    /// Drives one drain pass directly through [`run_pending_events_on`],
    /// bypassing [`run_pending_events`]'s TOML-aware config resolution (which
    /// depends on process `HOME`/cwd, unisolated here) since these tests
    /// target drain semantics, not CLI config resolution.
    async fn drain_for_test(db_path: &str) -> Result<DrainSummary> {
        let rt = make_rt(db_path).await;
        let server = KhiveMcpServer::new(rt.clone()).map_err(|e| anyhow::anyhow!("{e}"))?;
        run_pending_events_on(&rt, &server, false).await
    }

    async fn agenda_ticker_last_tick_at(server: &KhiveMcpServer) -> Option<DateTime<Utc>> {
        let response = server
            .dispatch_request_local(RequestParams {
                ops: "schedule.agenda()".to_string(),
                presentation: Some("verbose".to_string()),
                presentation_per_op: None,
                save_to: None,
                format: None,
                format_per_op: None,
                request_id: None,
            })
            .await
            .expect("agenda dispatch");
        let envelope: Value = serde_json::from_str(&response).expect("agenda response JSON");
        assert_eq!(envelope["results"][0]["ok"], true, "{envelope:?}");
        envelope["results"][0]["result"]["ticker"]["last_tick_at"]
            .as_str()
            .map(|timestamp| {
                timestamp
                    .parse::<DateTime<Utc>>()
                    .expect("last_tick_at is RFC 3339")
            })
    }

    async fn wait_for_agenda_tick_after(
        server: &KhiveMcpServer,
        after: Option<DateTime<Utc>>,
    ) -> DateTime<Utc> {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if let Some(tick) = agenda_ticker_last_tick_at(server).await {
                    if after.as_ref().is_none_or(|prior| tick > *prior) {
                        return tick;
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("schedule ticker heartbeat did not advance")
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn quiet_schedule_tick_loop_surfaces_an_advancing_then_stale_heartbeat() {
        let (_file, db_path) = tmp_db();
        let rt = make_rt(&db_path).await;
        let server = KhiveMcpServer::new(rt.clone()).expect("server");
        assert_eq!(agenda_ticker_last_tick_at(&server).await, None);

        let interval = std::time::Duration::from_millis(15);
        let cancellation = CancellationToken::new();
        let health = crate::components::HealthReporter::default();
        let ctx = crate::components::HostContext::new(
            server.clone(),
            cancellation.clone(),
            "schedule-tick",
            health.clone(),
        );
        let task = tokio::spawn(schedule_tick_loop(rt, ctx, interval));
        let first = wait_for_agenda_tick_after(&server, None).await;
        let second = wait_for_agenda_tick_after(&server, Some(first)).await;
        assert!(second > first);
        assert!(
            health
                .status("schedule-tick")
                .and_then(|status| status.last_heartbeat)
                .is_some(),
            "a successful quiet drain must heartbeat through component health"
        );

        cancellation.cancel();
        task.await
            .expect("tick task joins")
            .expect("cooperative cancellation is a clean stop");
        let stopped_at = agenda_ticker_last_tick_at(&server)
            .await
            .expect("the loop recorded at least two ticks before stopping");
        assert!(stopped_at >= second);
        tokio::time::sleep(interval.saturating_mul(2)).await;
        assert_eq!(
            agenda_ticker_last_tick_at(&server).await,
            Some(stopped_at),
            "a stopped loop must leave a stale timestamp, not fabricate liveness"
        );
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn schedule_ticker_heartbeat_is_process_local_and_missing_without_a_loop() {
        let (_file, db_path) = tmp_db();
        let rt = make_rt(&db_path).await;
        let server = KhiveMcpServer::new(rt).expect("server");
        assert_eq!(agenda_ticker_last_tick_at(&server).await, None);

        server.record_schedule_ticker_tick();
        assert!(agenda_ticker_last_tick_at(&server).await.is_some());

        let replacement_rt = make_rt(&db_path).await;
        let replacement = KhiveMcpServer::new(replacement_rt).expect("replacement server");
        assert_eq!(
            agenda_ticker_last_tick_at(&replacement).await,
            None,
            "a replacement process must not inherit its predecessor's heartbeat"
        );
    }

    /// Create a scheduled_event note directly via runtime.create_note, replicating
    /// the exact property schema used by handle_schedule / handle_remind in
    /// khive-pack-schedule.
    async fn create_scheduled_event(
        rt: &KhiveRuntime,
        namespace: &str,
        trigger_at: &str,
        action_dsl: Option<&str>,
        repeat: Option<&str>,
        event_type: &str,
    ) -> uuid::Uuid {
        let ns = Namespace::parse(namespace).expect("ns");
        let token = rt.authorize(ns).expect("authorize");
        let props = json!({
            "trigger_at": trigger_at,
            "repeat": repeat,
            "status": "pending",
            "event_type": event_type,
            "created_by_actor": token.actor().id.clone(),
            "payload": action_dsl,
            "fired_at": null,
            "cancelled_at": null,
        });

        let content = action_dsl.unwrap_or("test reminder");
        let note = rt
            .create_note(
                &token,
                "scheduled_event",
                None,
                content,
                None,
                Some(props),
                vec![],
            )
            .await
            .expect("create_note");

        // Production schedule verbs append this immutable actor binding
        // before activating a row. Most drain tests create fixtures directly
        // through the runtime to target claim/finalize behavior, so mirror
        // that provenance explicitly. Tests for legacy/forged rows construct
        // their own notes and intentionally omit it.
        let provenance = khive_storage::Event::new(
            namespace,
            khive_pack_schedule::CREATOR_PROVENANCE_VERB,
            EventKind::Audit,
            SubstrateKind::Note,
            format!("{}:{}", token.actor().kind, token.actor().id),
        )
        .with_target(note.id)
        .with_payload(json!({
            "provenance": khive_pack_schedule::CREATOR_PROVENANCE_MARKER_V1,
            "event_type": event_type,
        }));
        rt.events(&token)
            .expect("events")
            .append_event(provenance)
            .await
            .expect("append creator provenance");

        note.id
    }

    /// Fetch a note's properties from the store.
    async fn get_note_props(rt: &KhiveRuntime, id: uuid::Uuid) -> Value {
        let ns = Namespace::parse("local").unwrap();
        let token = rt.authorize(ns).expect("authorize");
        let store = rt.notes(&token).expect("notes");
        let note = store
            .get_note(id)
            .await
            .expect("get_note")
            .expect("note exists");
        note.properties.unwrap_or(json!({}))
    }

    async fn get_raw_note_properties(rt: &KhiveRuntime, id: uuid::Uuid) -> String {
        let mut reader = rt.sql().reader().await.expect("open SQL reader");
        let rows = reader
            .query_all(SqlStatement {
                sql: "SELECT properties FROM notes WHERE id = ?1".to_string(),
                params: vec![SqlValue::Text(id.to_string())],
                label: Some("test_get_raw_note_properties".into()),
            })
            .await
            .expect("query raw note properties");
        match rows.as_slice() {
            [row] => match row.get("properties") {
                Some(SqlValue::Text(value)) => value.clone(),
                other => panic!("unexpected properties column: {other:?}"),
            },
            other => panic!("expected one note row, got {other:?}"),
        }
    }

    async fn inbound_reminder_messages(rt: &KhiveRuntime, actor: &str) -> Vec<(String, Value)> {
        let mut reader = rt.sql().reader().await.expect("open SQL reader");
        let rows = reader
            .query_all(SqlStatement {
                sql: "SELECT content, properties FROM notes \
                      WHERE kind = 'message' \
                        AND json_extract(properties, '$.direction') = 'inbound' \
                        AND json_extract(properties, '$.to_actor') = ?1 \
                      ORDER BY created_at ASC, id ASC"
                    .to_string(),
                params: vec![SqlValue::Text(actor.to_string())],
                label: Some("test_inbound_reminder_messages".into()),
            })
            .await
            .expect("query reminder messages");
        rows.into_iter()
            .map(|row| {
                let content = match row.get("content") {
                    Some(SqlValue::Text(value)) => value.clone(),
                    other => panic!("unexpected content column: {other:?}"),
                };
                let properties = match row.get("properties") {
                    Some(SqlValue::Text(value)) => {
                        serde_json::from_str(value).expect("message properties JSON")
                    }
                    other => panic!("unexpected properties column: {other:?}"),
                };
                (content, properties)
            })
            .collect()
    }

    async fn note_content_count_in_namespace(
        rt: &KhiveRuntime,
        namespace: &str,
        kind: &str,
        content: &str,
    ) -> usize {
        let token = rt
            .authorize(Namespace::parse(namespace).expect("namespace"))
            .expect("authorize");
        rt.notes(&token)
            .expect("notes")
            .query_notes(
                namespace,
                Some(kind),
                PageRequest {
                    limit: 200,
                    offset: 0,
                },
            )
            .await
            .expect("query notes")
            .items
            .into_iter()
            .filter(|note| note.content == content)
            .count()
    }

    async fn note_content_count(rt: &KhiveRuntime, kind: &str, content: &str) -> usize {
        note_content_count_in_namespace(rt, "local", kind, content).await
    }

    async fn make_repeat_due_again(rt: &KhiveRuntime, id: uuid::Uuid) {
        let mut writer = rt.sql().writer().await.expect("open SQL writer");
        let rows = writer
            .execute(SqlStatement {
                sql: "UPDATE notes \
                      SET properties = json_set(properties, '$.trigger_at', ?1) \
                      WHERE id = ?2"
                    .to_string(),
                params: vec![
                    SqlValue::Text(due_rfc3339()),
                    SqlValue::Text(id.to_string()),
                ],
                label: Some("test_repeat_due_again".into()),
            })
            .await
            .expect("make repeat due again");
        assert_eq!(rows, 1, "repeat fixture row updated");
    }

    async fn claim_for_test(rt: &KhiveRuntime, id: uuid::Uuid, trigger_at: &str) -> DispatchClaim {
        let parsed = trigger_at
            .parse::<DateTime<Utc>>()
            .expect("trigger timestamp");
        claim_pending_event(
            rt,
            "local",
            id,
            dispatch_occurrence_id(id, parsed),
            trigger_at,
            "anonymous:local",
            DispatchLeaseConfig::from_env(),
        )
        .await
        .expect("claim query")
        .expect("claim must succeed on a fresh pending row")
    }

    fn short_test_lease() -> DispatchLeaseConfig {
        DispatchLeaseConfig {
            ttl: std::time::Duration::from_millis(300),
            renew_every: std::time::Duration::from_millis(30),
        }
    }

    #[test]
    fn final_disposition_counters_are_branch_local() {
        let mut summary = DrainSummary {
            fired: 7,
            advanced: 11,
            ..DrainSummary::default()
        };
        apply_final_disposition(&mut summary, FinalDisposition::Advanced);
        assert_eq!(summary.fired, 7, "advance must not alter prior fire count");
        assert_eq!(summary.advanced, 12);
        assert_eq!(summary.finalized, 1);

        apply_final_disposition(&mut summary, FinalDisposition::Fired);
        assert_eq!(summary.fired, 8);
        assert_eq!(
            summary.advanced, 12,
            "fire must not alter prior advance count"
        );
        assert_eq!(summary.finalized, 2);
    }

    async fn expire_dispatch_lease_for_test(rt: &KhiveRuntime, id: uuid::Uuid) {
        let mut writer = rt.sql().writer().await.expect("writer");
        let rows = writer
            .execute(SqlStatement {
                sql: "UPDATE notes SET properties = json_set( \
                        properties, '$.lease_expires_at', ?1) WHERE id = ?2"
                    .to_string(),
                params: vec![
                    SqlValue::Integer(Utc::now().timestamp_micros() - 1),
                    SqlValue::Text(id.to_string()),
                ],
                label: Some("test_expire_dispatch_lease".into()),
            })
            .await
            .expect("expire dispatch lease");
        assert_eq!(rows, 1);
    }

    async fn overwrite_dispatch_receipt_and_expire_for_test(
        rt: &KhiveRuntime,
        id: uuid::Uuid,
        receipt: &Value,
    ) {
        let mut writer = rt.sql().writer().await.expect("writer");
        let rows = writer
            .execute(SqlStatement {
                sql: "UPDATE notes SET properties = json_set( \
                        properties, '$.dispatch_receipt', json(?1), \
                        '$.lease_expires_at', ?2) WHERE id = ?3"
                    .to_string(),
                params: vec![
                    SqlValue::Text(serde_json::to_string(receipt).expect("serialize test receipt")),
                    SqlValue::Integer(Utc::now().timestamp_micros() - 1),
                    SqlValue::Text(id.to_string()),
                ],
                label: Some("test_overwrite_and_expire_dispatch_receipt".into()),
            })
            .await
            .expect("overwrite and expire dispatch receipt");
        assert_eq!(rows, 1);
    }

    async fn create_marker_directly(rt: &KhiveRuntime, content: &str) {
        let token = rt.authorize(Namespace::local()).expect("authorize marker");
        rt.create_note(&token, "observation", None, content, None, None, vec![])
            .await
            .expect("create simulated side effect");
    }

    #[test]
    fn reminder_subject_marks_and_truncates_the_content_head() {
        let content = format!("  {}\n tail", "x".repeat(90));
        let subject = reminder_subject(&content);
        assert!(subject.starts_with("[Reminder] "));
        assert!(subject.ends_with('…'));
        assert_eq!(subject.chars().count(), "[Reminder] ".chars().count() + 81);
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn fired_reminder_delivers_to_creator_after_daemon_actor_changes() {
        let (_tmp, db_path) = tmp_db();
        let creator = "lambda:reminder-owner";
        let daemon_actor = "lambda:replacement-daemon";
        let id = {
            let creator_rt = make_rt_with_actor(&db_path, Some(creator)).await;
            let creator_server = KhiveMcpServer::new(creator_rt.clone()).expect("creator server");
            let remind_ops = serde_json::to_string(&json!([{
                "tool": "schedule.remind",
                "args": {
                    "content": "test reminder",
                    "at": "2099-01-01T00:00:00Z"
                }
            }]))
            .expect("serialize reminder op");
            let result = creator_server
                .dispatch_request_local(RequestParams {
                    ops: remind_ops,
                    ..Default::default()
                })
                .await
                .expect("create reminder through schedule.remind");
            let result: Value = serde_json::from_str(&result).expect("reminder result JSON");
            assert_eq!(result["results"][0]["ok"], true, "{result}");
            let id = result["results"][0]["result"]["full_id"]
                .as_str()
                .expect("reminder full_id")
                .parse()
                .expect("reminder UUID");
            let props = get_note_props(&creator_rt, id).await;
            assert_eq!(props["created_by_actor"], creator, "{props}");
            make_repeat_due_again(&creator_rt, id).await;
            id
        };

        let rt = make_rt_with_actor(&db_path, Some(daemon_actor)).await;
        let server = KhiveMcpServer::new(rt.clone()).expect("replacement daemon server");

        let summary = run_pending_events_on(&rt, &server, false)
            .await
            .expect("drain");

        assert_eq!(summary.fired, 1);
        assert_eq!(summary.failed, 0);
        let messages = inbound_reminder_messages(&rt, creator).await;
        let daemon_messages = inbound_reminder_messages(&rt, daemon_actor).await;
        let local_messages = inbound_reminder_messages(&rt, "local").await;
        assert_eq!(
            messages.len(),
            1,
            "one inbound delivery for the creator; daemon={daemon_messages:?}, local={local_messages:?}"
        );
        assert_eq!(messages[0].0, "test reminder");
        assert_eq!(messages[0].1["direction"], "inbound");
        assert_eq!(messages[0].1["to_actor"], creator);
        assert_eq!(messages[0].1["subject"], "[Reminder] test reminder");
        assert!(daemon_messages.is_empty());
        assert!(local_messages.is_empty());
        let props = get_note_props(&rt, id).await;
        assert_eq!(props["status"], "fired");
        assert!(props["fired_at"].as_str().is_some());
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn unprovenanced_reminder_ignores_forged_actor_property() {
        let (_tmp, db_path) = tmp_db();
        let daemon_actor = "lambda:daemon-owner";
        let forged_victim = "lambda:forged-victim";
        let rt = make_rt_with_actor(&db_path, Some(daemon_actor)).await;
        let server = KhiveMcpServer::new(rt.clone()).expect("server");
        let token = rt
            .authorize(Namespace::local())
            .expect("authorize reminder fixture");
        let note = rt
            .create_note(
                &token,
                "scheduled_event",
                None,
                "unprovenanced reminder",
                None,
                Some(json!({
                    "trigger_at": due_rfc3339(),
                    "repeat": null,
                    "status": "pending",
                    "event_type": "remind",
                    "created_by_actor": forged_victim,
                    "payload": null,
                    "fired_at": null,
                    "cancelled_at": null,
                })),
                vec![],
            )
            .await
            .expect("create hand-written reminder");

        let summary = run_pending_events_on(&rt, &server, false)
            .await
            .expect("drain");
        assert_eq!(summary.fired, 1);
        assert_eq!(summary.failed, 0);
        assert!(
            inbound_reminder_messages(&rt, forged_victim)
                .await
                .is_empty(),
            "mutable created_by_actor metadata must not select a recipient"
        );
        let daemon_messages = inbound_reminder_messages(&rt, daemon_actor).await;
        assert_eq!(daemon_messages.len(), 1);
        assert_eq!(daemon_messages[0].0, "unprovenanced reminder");
        assert_eq!(
            get_note_props(&rt, note.id).await["dispatch_receipt"]["actor"],
            format!("actor:{daemon_actor}"),
            "the scheduler fallback remains available only for a genuinely legacy reminder"
        );
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn repeating_reminder_delivers_on_consecutive_fires() {
        let (_tmp, db_path) = tmp_db();
        let actor = "lambda:repeat-owner";
        let rt = make_rt_with_actor(&db_path, Some(actor)).await;
        let server = KhiveMcpServer::new(rt.clone()).expect("server");
        let id =
            create_scheduled_event(&rt, "local", &due_rfc3339(), None, Some("daily"), "remind")
                .await;

        let first = run_pending_events_on(&rt, &server, false)
            .await
            .expect("first drain");
        assert_eq!(first.advanced, 1);
        assert_eq!(inbound_reminder_messages(&rt, actor).await.len(), 1);

        make_repeat_due_again(&rt, id).await;
        let second = run_pending_events_on(&rt, &server, false)
            .await
            .expect("second drain");

        assert_eq!(second.advanced, 1);
        assert_eq!(second.failed, 0);
        assert_eq!(
            inbound_reminder_messages(&rt, actor).await.len(),
            2,
            "each fire delivers one inbound message"
        );
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn reminder_delivery_failure_is_persisted_audited_and_drain_continues() {
        let (_tmp, db_path) = tmp_db();
        let actor = "lambda:failure-owner";
        let cfg = RuntimeConfig {
            db_path: Some(std::path::PathBuf::from(&db_path)),
            default_namespace: Namespace::parse("local").unwrap(),
            embedding_model: None,
            additional_embedding_models: vec![],
            gate: std::sync::Arc::new(DenyCommSendGate),
            actor_id: Some(actor.to_string()),
            ..Default::default()
        };
        let rt = KhiveRuntime::new(cfg).expect("runtime");
        let packs = vec!["kg".to_string(), "comm".to_string(), "schedule".to_string()];
        let server = KhiveMcpServer::with_packs(rt.clone(), &packs)
            .expect("server with required reminder delivery pack");
        let id = create_scheduled_event(&rt, "local", &due_rfc3339(), None, None, "remind").await;
        let action_id = create_scheduled_event(
            &rt,
            "local",
            &due_rfc3339(),
            Some("stats()"),
            None,
            "schedule",
        )
        .await;
        let mut writer = rt.sql().writer().await.expect("open SQL writer");
        let reordered = writer
            .execute(SqlStatement {
                sql: "UPDATE notes SET created_at = CASE id WHEN ?1 THEN 1 WHEN ?2 THEN 2 END \
                      WHERE id IN (?1, ?2)"
                    .to_string(),
                params: vec![
                    SqlValue::Text(id.to_string()),
                    SqlValue::Text(action_id.to_string()),
                ],
                label: Some("test_reminder_failure_precedes_valid_action".into()),
            })
            .await
            .expect("order reminder before action");
        assert_eq!(reordered, 2);
        drop(writer);

        let summary = run_pending_events_on(&rt, &server, false)
            .await
            .expect("drain continues after failure");

        assert_eq!(summary.scanned, 2);
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.fired, 1);
        assert_eq!(summary.retry_pending, 1);
        assert!(inbound_reminder_messages(&rt, actor).await.is_empty());
        let props = get_note_props(&rt, id).await;
        assert_eq!(
            props["status"], "pending",
            "failed one-shot must remain retryable"
        );
        assert!(
            props["delivery_error"]
                .as_str()
                .is_some_and(|error| error.contains("denied by delivery-failure test")),
            "delivery error must be visible on the reminder row: {props:?}"
        );
        assert!(props["delivery_failed_at"].as_str().is_some());
        let action_props = get_note_props(&rt, action_id).await;
        assert_eq!(action_props["status"], "fired");
        assert!(action_props["fired_at"].as_str().is_some());

        let token = rt.authorize(Namespace::local()).expect("authorize");
        let events = rt
            .events(&token)
            .expect("event store")
            .query_events(
                EventFilter {
                    verbs: vec!["schedule.remind.fire".to_string()],
                    ..Default::default()
                },
                PageRequest {
                    limit: 10,
                    offset: 0,
                },
            )
            .await
            .expect("query reminder failure events");
        assert!(events
            .items
            .iter()
            .any(|event| { event.outcome == EventOutcome::Error && event.target_id == Some(id) }));
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn due_event_is_fired() {
        let (_tmp, db_path) = tmp_db();
        let rt = make_rt(&db_path).await;

        // Create a past-due schedule event. Use stats() as the action since it's
        // a valid, registered verb that has no side-effects that need a
        // namespace argument check. `due_rfc3339` is only a few seconds
        // overdue — inside the missed-event grace window — so this exercises
        // the normal fire path, not the ADR-106 missed path.
        let past = due_rfc3339();
        let id =
            create_scheduled_event(&rt, "local", &past, Some("stats()"), None, "schedule").await;

        let summary = drain_for_test(&db_path).await.expect("drain");

        assert!(summary.scanned >= 1, "must have scanned the due event");
        assert!(
            summary.fired >= 1 || summary.advanced >= 1,
            "must fire or advance"
        );

        let props = get_note_props(&rt, id).await;
        let status = props["status"].as_str().unwrap_or("");
        assert!(
            status == "fired" || status == "pending",
            "status must be fired or pending (repeat), got {status:?}"
        );
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn future_event_is_skipped() {
        let (_tmp, db_path) = tmp_db();
        let rt = make_rt(&db_path).await;

        let future = "2099-01-01T00:00:00Z";
        let id =
            create_scheduled_event(&rt, "local", future, Some("stats()"), None, "schedule").await;

        let summary = drain_for_test(&db_path).await.expect("drain");

        // The future event must not be fired. The drain may skip it via the SQL
        // pre-filter (scanned=0, skipped_not_due=0) or via the Rust timestamp
        // check (scanned=1, skipped_not_due=1) — either is correct; the key
        // invariant is that fired=0, advanced=0.
        assert_eq!(summary.fired, 0, "future event must not be fired");
        assert_eq!(summary.advanced, 0, "future event must not be advanced");

        let props = get_note_props(&rt, id).await;
        assert_eq!(
            props["status"].as_str(),
            Some("pending"),
            "future event must remain pending"
        );
    }

    /// A due event stored with a positive `trigger_at` offset (whose RFC 3339
    /// string sorts lexicographically after a UTC "now" string) must still
    /// fire — proves the SQL due-ness predicate compares chronologically via
    /// `datetime(...)`, not as raw text.
    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn due_event_with_positive_offset_trigger_at_fires() {
        let (_tmp, db_path) = tmp_db();
        let rt = make_rt(&db_path).await;

        // Chronologically 10s overdue (well inside the default 300s grace
        // window), but formatted at +04:00 wall time so the RFC 3339 string
        // sorts AFTER a UTC `now` string as raw text.
        let trigger_instant = Utc::now() - Duration::seconds(10);
        let plus_four = FixedOffset::east_opt(4 * 3600).expect("valid offset");
        let trigger_at = trigger_instant.with_timezone(&plus_four).to_rfc3339();
        assert!(
            trigger_at.as_str() > now_rfc3339_for_ordering_check().as_str(),
            "test setup: {trigger_at:?} must sort AFTER a UTC now-string as raw text \
             for this to exercise the lexicographic-ordering bug"
        );

        let id =
            create_scheduled_event(&rt, "local", &trigger_at, Some("stats()"), None, "schedule")
                .await;

        let summary = drain_for_test(&db_path).await.expect("drain");

        assert!(
            summary.fired >= 1 || summary.advanced >= 1,
            "a due event stored with a positive offset must still fire, got {summary:?}"
        );

        let props = get_note_props(&rt, id).await;
        let status = props["status"].as_str().unwrap_or("");
        assert!(
            status == "fired" || status == "pending",
            "status must be fired or pending (repeat), got {status:?}"
        );
    }

    /// A future event stored with a negative `trigger_at` offset (whose RFC
    /// 3339 string sorts lexicographically before a UTC "now" string) must
    /// NOT fire — the mirror case of the positive-offset test above, with the
    /// Rust-side `trigger_at > now` re-check as an additional backstop.
    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn future_event_with_negative_offset_trigger_at_is_not_fired() {
        let (_tmp, db_path) = tmp_db();
        let rt = make_rt(&db_path).await;

        // Chronologically 2h in the future, but formatted at -08:00 wall
        // time so the RFC 3339 string sorts BEFORE a UTC `now` string as raw
        // text (a false positive under naive lexicographic comparison).
        let trigger_instant = Utc::now() + Duration::hours(2);
        let minus_eight = FixedOffset::west_opt(8 * 3600).expect("valid offset");
        let trigger_at = trigger_instant.with_timezone(&minus_eight).to_rfc3339();
        assert!(
            trigger_at.as_str() < now_rfc3339_for_ordering_check().as_str(),
            "test setup: {trigger_at:?} must sort BEFORE a UTC now-string as raw text \
             for this to exercise the false-positive path"
        );

        let id =
            create_scheduled_event(&rt, "local", &trigger_at, Some("stats()"), None, "schedule")
                .await;

        let summary = drain_for_test(&db_path).await.expect("drain");

        assert_eq!(
            summary.fired, 0,
            "a chronologically future event must not be fired, got {summary:?}"
        );
        assert_eq!(
            summary.advanced, 0,
            "a chronologically future event must not be advanced, got {summary:?}"
        );

        let props = get_note_props(&rt, id).await;
        assert_eq!(
            props["status"].as_str(),
            Some("pending"),
            "future event must remain pending"
        );
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn fired_event_is_idempotent() {
        let (_tmp, db_path) = tmp_db();
        let rt = make_rt(&db_path).await;

        let past = due_rfc3339();
        let id =
            create_scheduled_event(&rt, "local", &past, Some("stats()"), None, "schedule").await;

        // First drain — fires the event.
        let s1 = drain_for_test(&db_path).await.expect("drain 1");
        assert!(s1.scanned >= 1);

        // Second drain — event is now status="fired", not "pending"; must not re-fire.
        let s2 = drain_for_test(&db_path).await.expect("drain 2");
        assert_eq!(s2.scanned, 0, "no pending events on second drain");
        assert_eq!(s2.fired, 0, "no new fires on second drain");

        let props = get_note_props(&rt, id).await;
        let fired_at_1 = props["fired_at"].as_str().unwrap_or("").to_string();
        assert!(
            !fired_at_1.is_empty(),
            "fired_at must be set after first drain"
        );

        // fired_at must not change on the second drain (idempotent).
        let props2 = get_note_props(&rt, id).await;
        assert_eq!(
            props2["fired_at"].as_str().unwrap_or(""),
            fired_at_1.as_str(),
            "fired_at must not change on second drain"
        );
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn daily_repeat_advances() {
        let (_tmp, db_path) = tmp_db();
        let rt = make_rt(&db_path).await;

        // Use a past (but in-grace) trigger_at with daily repeat.
        let past = due_rfc3339();
        let id = create_scheduled_event(
            &rt,
            "local",
            &past,
            Some("stats()"),
            Some("daily"),
            "schedule",
        )
        .await;

        let summary = drain_for_test(&db_path).await.expect("drain");

        assert!(
            summary.advanced >= 1,
            "daily event must be advanced, not fired"
        );

        let props = get_note_props(&rt, id).await;
        assert_eq!(
            props["status"].as_str(),
            Some("pending"),
            "after advance, status must be pending"
        );
        let new_trigger = props["trigger_at"]
            .as_str()
            .expect("trigger_at must be set");
        let new_ts: DateTime<Utc> = new_trigger.parse().expect("parseable ts");
        let original: DateTime<Utc> = past.parse().unwrap();
        assert_eq!(
            new_ts,
            original + Duration::days(1),
            "daily advance must add 1 day"
        );
    }

    /// Repeat advancement must preserve the original
    /// `trigger_at` timezone offset — not silently re-serialize the advanced
    /// occurrence as UTC. A `+04:00` schedule that fires and advances must
    /// still carry `+04:00` (and the same local wall-clock hour) on its next
    /// occurrence, not drift to a different wall-clock hour under `+00:00`.
    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn daily_repeat_advance_preserves_original_offset() {
        let (_tmp, db_path) = tmp_db();
        let rt = make_rt(&db_path).await;

        // Chronologically a few seconds ago (in-grace), formatted at a
        // non-UTC +04:00 wall-clock offset — the exact shape
        // `khive-pack-schedule` round-trips verbatim from the caller.
        let plus_four = FixedOffset::east_opt(4 * 3600).expect("valid offset");
        let trigger_instant = Utc::now() - Duration::seconds(5);
        let past = trigger_instant.with_timezone(&plus_four).to_rfc3339();

        let id = create_scheduled_event(
            &rt,
            "local",
            &past,
            Some("stats()"),
            Some("daily"),
            "schedule",
        )
        .await;

        let summary = drain_for_test(&db_path).await.expect("drain");
        assert!(
            summary.advanced >= 1,
            "daily event with a non-UTC offset must be advanced, not fired"
        );

        let props = get_note_props(&rt, id).await;
        let new_trigger = props["trigger_at"]
            .as_str()
            .expect("trigger_at must be set");

        // The advanced occurrence must still carry the ORIGINAL +04:00
        // offset, not be silently re-serialized as UTC (+00:00).
        assert!(
            new_trigger.ends_with("+04:00"),
            "advanced trigger_at must preserve the original +04:00 offset, got {new_trigger:?}"
        );

        let new_dt = DateTime::parse_from_rfc3339(new_trigger).expect("parseable advanced ts");
        let original_dt = DateTime::parse_from_rfc3339(&past).expect("parseable original ts");
        assert_eq!(
            *new_dt.offset(),
            plus_four,
            "advanced trigger_at offset must equal the original +04:00 offset"
        );
        assert_eq!(
            new_dt.with_timezone(&Utc),
            original_dt.with_timezone(&Utc) + Duration::days(1),
            "daily advance must add exactly 1 day to the chronological instant"
        );
        // Wall-clock hour must be unchanged (the drift this issue reports):
        // same local time-of-day at the same offset, one day later.
        assert_eq!(
            new_dt.time(),
            original_dt.time(),
            "advanced occurrence must retain the same local wall-clock time"
        );
    }

    /// The drain must keep accepting the *same* `trigger_at` grammar the
    /// write boundary validates with (`khive-pack-schedule`'s
    /// `at.parse::<DateTime<Utc>>()`, which is chrono's relaxed RFC 3339
    /// form) — not narrow to strict `DateTime::parse_from_rfc3339`. A
    /// legacy stored timestamp using a space instead of `T` and an offset
    /// without a colon (e.g. `2026-07-14 09:00:00+0400`) must still be
    /// recognized as due and advanced, not silently skipped forever as
    /// "unparseable".
    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn relaxed_legacy_grammar_repeat_advance_preserves_offset() {
        let (_tmp, db_path) = tmp_db();
        let rt = make_rt(&db_path).await;

        let plus_four = FixedOffset::east_opt(4 * 3600).expect("valid offset");
        // Whole-second precision: the relaxed `%z` format below drops
        // fractional seconds, so the fixture must match what actually
        // round-trips through it.
        let trigger_instant =
            DateTime::from_timestamp((Utc::now() - Duration::seconds(5)).timestamp(), 0)
                .expect("valid timestamp");
        let past_relaxed = trigger_instant
            .with_timezone(&plus_four)
            .format("%Y-%m-%d %H:%M:%S%z")
            .to_string();
        assert!(
            past_relaxed.contains(' ') && !past_relaxed.contains('T'),
            "fixture must use the relaxed space separator, got {past_relaxed:?}"
        );

        let id = create_scheduled_event(
            &rt,
            "local",
            &past_relaxed,
            Some("stats()"),
            Some("daily"),
            "schedule",
        )
        .await;

        let summary = drain_for_test(&db_path).await.expect("drain");
        assert!(
            summary.advanced >= 1,
            "a relaxed-grammar legacy trigger_at must still be recognized as due and \
             advanced, not skipped as unparseable"
        );
        assert_eq!(
            summary.skipped_not_due, 0,
            "relaxed-grammar trigger_at must not be treated as unparseable"
        );

        let props = get_note_props(&rt, id).await;
        let new_trigger = props["trigger_at"]
            .as_str()
            .expect("trigger_at must be set");
        assert!(
            new_trigger.ends_with("+04:00"),
            "advanced trigger_at must preserve the original +04:00 offset, got {new_trigger:?}"
        );

        let new_dt = DateTime::parse_from_rfc3339(new_trigger).expect("parseable advanced ts");
        assert_eq!(
            new_dt.with_timezone(&Utc),
            trigger_instant + Duration::days(1),
            "daily advance must add exactly 1 day to the chronological instant"
        );
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn namespace_isolation() {
        let (_tmp, db_path) = tmp_db();
        let rt = make_rt(&db_path).await;

        // Create a due event in namespace "ns-a". The action is stats() which
        // doesn't create notes, so we can't verify write-landing-in-ns-a directly
        // through this drain. Instead we verify the drain scans and fires the event
        // in ns-a without touching the ns-b namespace counts.
        let ns_a = "ns-a";
        let ns_b = "ns-b";
        let past = due_rfc3339();

        let id_a =
            create_scheduled_event(&rt, ns_a, &past, Some("stats()"), None, "schedule").await;

        // Create a future event in ns-b that must not be fired.
        let _id_b = create_scheduled_event(
            &rt,
            ns_b,
            "2099-01-01T00:00:00Z",
            Some("stats()"),
            None,
            "schedule",
        )
        .await;

        let summary = drain_for_test(&db_path).await.expect("drain");

        // Only the ns-a event should have been processed.
        assert!(summary.scanned >= 1);
        assert!(summary.fired >= 1 || summary.advanced >= 1);

        // ns-a event is fired.
        let token_a = rt.authorize(Namespace::parse(ns_a).unwrap()).expect("auth");
        let store_a = rt.notes(&token_a).expect("notes");
        let note_a = store_a.get_note(id_a).await.expect("get").expect("exists");
        let status_a = note_a
            .properties
            .as_ref()
            .and_then(|p| p.get("status"))
            .and_then(Value::as_str)
            .unwrap_or("");
        assert!(
            status_a == "fired" || status_a == "pending",
            "ns-a event must be fired or advanced, got {status_a:?}"
        );
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn concurrent_replay_preserves_each_events_actor_and_namespace() {
        let (_tmp, db_path) = tmp_db();
        let creator_runtime = |actor: &str| {
            KhiveRuntime::new(RuntimeConfig {
                db_path: Some(std::path::PathBuf::from(&db_path)),
                default_namespace: Namespace::local(),
                embedding_model: None,
                additional_embedding_models: vec![],
                actor_id: Some(actor.to_string()),
                packs: vec!["kg".to_string(), "schedule".to_string()],
                ..Default::default()
            })
            .expect("creator runtime")
        };
        let actor_a = "lambda:scheduled-a";
        let actor_b = "lambda:scheduled-b";
        let ns_a = "schedule-tenant-a";
        let ns_b = "schedule-tenant-b";
        let rt_a = creator_runtime(actor_a);
        let rt_b = creator_runtime(actor_b);
        create_scheduled_event(
            &rt_a,
            ns_a,
            &due_rfc3339(),
            Some("create(kind=\"observation\", content=\"isolated-a\")"),
            None,
            "schedule",
        )
        .await;
        create_scheduled_event(
            &rt_b,
            ns_b,
            &due_rfc3339(),
            Some("create(kind=\"observation\", content=\"isolated-b\")"),
            None,
            "schedule",
        )
        .await;

        let gate = std::sync::Arc::new(CaptureReplayIdentityGate::default());
        let daemon_rt = KhiveRuntime::new(RuntimeConfig {
            db_path: Some(std::path::PathBuf::from(&db_path)),
            default_namespace: Namespace::local(),
            embedding_model: None,
            additional_embedding_models: vec![],
            gate: gate.clone(),
            actor_id: Some("lambda:daemon".to_string()),
            packs: vec!["kg".to_string(), "schedule".to_string()],
            ..Default::default()
        })
        .expect("daemon runtime");
        let server = KhiveMcpServer::new(daemon_rt.clone()).expect("server");

        let (drain_a, drain_b) = tokio::join!(
            run_pending_events_on(&daemon_rt, &server, false),
            run_pending_events_on(&daemon_rt, &server, false),
        );
        let drain_a = drain_a.expect("drain A");
        let drain_b = drain_b.expect("drain B");
        assert_eq!(
            drain_a.failed + drain_b.failed,
            0,
            "{drain_a:?} {drain_b:?}"
        );
        assert_eq!(
            drain_a.fired + drain_b.fired,
            2,
            "both isolated scheduled actions must fire exactly once"
        );

        let mut seen = gate.creates.lock().expect("capture lock").clone();
        seen.sort();
        let mut expected = vec![
            ("actor".to_string(), actor_a.to_string(), ns_a.to_string()),
            ("actor".to_string(), actor_b.to_string(), ns_b.to_string()),
        ];
        expected.sort();
        assert_eq!(
            seen, expected,
            "concurrent replay must not swap actor or namespace identities"
        );
        assert_eq!(
            note_content_count_in_namespace(&daemon_rt, ns_a, "observation", "isolated-a").await,
            1
        );
        assert_eq!(
            note_content_count_in_namespace(&daemon_rt, ns_b, "observation", "isolated-b").await,
            1
        );
        assert_eq!(
            note_content_count_in_namespace(&daemon_rt, ns_a, "observation", "isolated-b").await,
            0,
            "tenant B's action must not land in tenant A"
        );
        assert_eq!(
            note_content_count_in_namespace(&daemon_rt, ns_b, "observation", "isolated-a").await,
            0,
            "tenant A's action must not land in tenant B"
        );
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn anonymous_creator_replay_preserves_anonymous_actor_kind() {
        let (_tmp, db_path) = tmp_db();
        let creator_rt = make_rt(&db_path).await;
        create_scheduled_event(
            &creator_rt,
            "local",
            &due_rfc3339(),
            Some("create(kind=\"observation\", content=\"anonymous replay marker\")"),
            None,
            "schedule",
        )
        .await;

        let gate = std::sync::Arc::new(CaptureReplayIdentityGate::default());
        let daemon_rt = KhiveRuntime::new(RuntimeConfig {
            db_path: Some(std::path::PathBuf::from(&db_path)),
            default_namespace: Namespace::local(),
            embedding_model: None,
            additional_embedding_models: vec![],
            gate: gate.clone(),
            actor_id: Some("lambda:daemon".to_string()),
            packs: vec!["kg".to_string(), "schedule".to_string()],
            ..Default::default()
        })
        .expect("daemon runtime");
        let server = KhiveMcpServer::new(daemon_rt.clone()).expect("server");

        let summary = run_pending_events_on(&daemon_rt, &server, false)
            .await
            .expect("drain");
        assert_eq!(summary.fired, 1);
        assert_eq!(summary.failed, 0);
        assert_eq!(
            gate.creates.lock().expect("capture lock").as_slice(),
            &[(
                "anonymous".to_string(),
                "local".to_string(),
                "local".to_string(),
            )],
            "verified anonymous provenance must not become authenticated actor:local"
        );
        assert_eq!(
            note_content_count(&daemon_rt, "observation", "anonymous replay marker").await,
            1
        );
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn dispatch_failure_does_not_abort_drain() {
        let (_tmp, db_path) = tmp_db();
        let rt = make_rt(&db_path).await;

        // Create a past-due (but in-grace) event with an invalid action DSL
        // (verb not registered).
        let past = due_rfc3339();
        let _id_bad = create_scheduled_event(
            &rt,
            "local",
            &past,
            Some("stats()"), // valid — but let's add a second event with a broken action
            None,
            "schedule",
        )
        .await;
        // Second event with broken action.
        let id_bad2 = create_scheduled_event(
            &rt,
            "local",
            &past,
            Some("this_verb_does_not_exist(foo=\"bar\")"),
            None,
            "schedule",
        )
        .await;

        let summary = drain_for_test(&db_path)
            .await
            .expect("drain must not abort");

        // Both events were scanned. The bad one produced a failure.
        assert!(summary.scanned >= 2, "both events must be scanned");
        assert!(
            summary.failed >= 1 || summary.fired >= 1,
            "at least one event processed (failed or fired)"
        );

        // The drain still ran to completion (no panic / early return).
        let props_bad2 = get_note_props(&rt, id_bad2).await;
        let _ = props_bad2["status"].as_str(); // just verify it's accessible
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn legacy_scheduled_action_without_creator_fails_closed() {
        let (_tmp, db_path) = tmp_db();
        let rt = make_rt_with_actor(&db_path, Some("lambda:daemon")).await;
        let server = KhiveMcpServer::new(rt.clone()).expect("server");
        let action = "create(kind=\"observation\", content=\"legacy action marker\")";
        let token = rt
            .authorize(Namespace::parse("local").expect("namespace"))
            .expect("authorize");
        let note = rt
            .create_note(
                &token,
                "scheduled_event",
                None,
                action,
                None,
                Some(json!({
                    "trigger_at": "2000-01-01T00:00:00Z",
                    "repeat": null,
                    "status": "pending",
                    "event_type": "schedule",
                    "payload": action,
                    "fired_at": null,
                    "cancelled_at": null,
                })),
                vec![],
            )
            .await
            .expect("create legacy scheduled action");

        let summary = run_pending_events_on(&rt, &server, false)
            .await
            .expect("drain");

        assert_eq!(summary.failed, 1, "legacy row must report one failure");
        assert_eq!(summary.fired, 0, "unsafe action must never count as fired");
        assert_eq!(
            note_content_count(&rt, "observation", "legacy action marker").await,
            0,
            "missing attribution must never inherit daemon authority"
        );
        let props = get_note_props(&rt, note.id).await;
        assert_eq!(props["status"], "failed", "{props}");
        assert!(
            props["dispatch_error"]
                .as_str()
                .is_some_and(|error| error.contains("immutable creator provenance")),
            "policy error must explain why replay was refused: {props}"
        );
        assert!(props["dispatch_failed_at"].as_str().is_some(), "{props}");
        assert_eq!(
            props["dispatch_receipt"]["state"],
            DispatchReceiptState::NotInvoked.as_str(),
            "the durable claim receipt must survive provenance refusal: {props}"
        );
        assert_eq!(
            props["dispatch_receipt"]["actor"], "anonymous:local",
            "a refused generic row has no verified creator and must not inherit daemon attribution: {props}"
        );
        assert!(
            props["dispatch_receipt"]["completed_at"].as_i64().is_some(),
            "{props}"
        );
        assert!(
            props["dispatch_receipt"]["error"]
                .as_str()
                .is_some_and(|error| error.contains("immutable creator provenance")),
            "{props}"
        );
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn forged_created_by_actor_property_cannot_authorize_replay() {
        let (_tmp, db_path) = tmp_db();
        let rt = make_rt_with_actor(&db_path, Some("lambda:daemon")).await;
        let server = KhiveMcpServer::new(rt.clone()).expect("server");
        let action = "create(kind=\"observation\", content=\"forged actor marker\")";
        let token = rt
            .authorize(Namespace::parse("local").expect("namespace"))
            .expect("authorize");
        let note = rt
            .create_note(
                &token,
                "scheduled_event",
                None,
                action,
                None,
                Some(json!({
                    "trigger_at": due_rfc3339(),
                    "repeat": null,
                    "status": "pending",
                    "event_type": "schedule",
                    // Generic note properties are writable by the caller.
                    // This claim has no pack-written provenance event.
                    "created_by_actor": "lambda:privileged-victim",
                    "payload": action,
                    "fired_at": null,
                    "cancelled_at": null,
                })),
                vec![],
            )
            .await
            .expect("create forged scheduled action");

        let summary = run_pending_events_on(&rt, &server, false)
            .await
            .expect("drain");

        assert_eq!(summary.failed, 1);
        assert_eq!(summary.fired, 0);
        assert_eq!(
            note_content_count(&rt, "observation", "forged actor marker").await,
            0,
            "caller-editable actor metadata must never become replay authority"
        );
        let props = get_note_props(&rt, note.id).await;
        assert_eq!(props["status"], "failed", "{props}");
        assert!(
            props["dispatch_error"]
                .as_str()
                .is_some_and(|error| error.contains("immutable creator provenance")),
            "{props}"
        );
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn second_actor_cannot_rewrite_provenanced_schedule_intent() {
        let (_tmp, db_path) = tmp_db();
        let gate = std::sync::Arc::new(DenyAttackerCreateGate);
        let owner = "lambda:schedule-owner";
        let attacker = "lambda:schedule-attacker";
        let original_action =
            "create(kind=\"observation\", content=\"owner-approved schedule intent\")";
        let forged_action =
            "create(kind=\"observation\", content=\"attacker-selected schedule intent\")";

        let owner_rt = KhiveRuntime::new(RuntimeConfig {
            db_path: Some(std::path::PathBuf::from(&db_path)),
            default_namespace: Namespace::local(),
            embedding_model: None,
            additional_embedding_models: vec![],
            gate: gate.clone(),
            actor_id: Some(owner.to_string()),
            packs: vec!["kg".to_string(), "schedule".to_string()],
            ..Default::default()
        })
        .expect("owner runtime");
        let note_id = create_scheduled_event(
            &owner_rt,
            "local",
            &due_rfc3339(),
            Some(original_action),
            None,
            "schedule",
        )
        .await;

        // Actor B is allowed to call generic `update`, but is denied the
        // target `create` verb. Before the schedule-managed mutation fence,
        // this patch replaced actor A's payload while retaining A's immutable
        // provenance, so trigger-time Gate evaluation ran as A and allowed it.
        let attacker_rt = KhiveRuntime::new(RuntimeConfig {
            db_path: Some(std::path::PathBuf::from(&db_path)),
            default_namespace: Namespace::local(),
            embedding_model: None,
            additional_embedding_models: vec![],
            gate: gate.clone(),
            actor_id: Some(attacker.to_string()),
            packs: vec!["kg".to_string(), "schedule".to_string()],
            ..Default::default()
        })
        .expect("attacker runtime");
        let attacker_server = KhiveMcpServer::new(attacker_rt).expect("attacker server");
        let update_ops = json!([{
            "tool": "update",
            "args": {
                "id": note_id.to_string(),
                "kind": "note",
                "properties": {
                    "payload": forged_action,
                    "trigger_at": due_rfc3339(),
                    "repeat": "daily",
                    "status": "pending",
                    "event_type": "schedule"
                }
            }
        }])
        .to_string();
        let update_response = attacker_server
            .dispatch_request_local(RequestParams {
                ops: update_ops,
                ..Default::default()
            })
            .await
            .expect("generic update returns an operation envelope");
        let update_response: Value =
            serde_json::from_str(&update_response).expect("update response JSON");
        assert_eq!(
            update_response["results"][0]["ok"], false,
            "{update_response}"
        );
        // Two layered defenses reject this: the KG update handler refuses the
        // `scheduled_event` kind outright, and the runtime curation fence
        // refuses schedule-managed notes. Whichever layer fires first, the
        // rejection must name the scheduled-event trust boundary.
        assert!(
            update_response["results"][0]["error"]
                .as_str()
                .is_some_and(|error| error.contains("schedule-managed")
                    || error.contains("scheduled_event notes are not editable")),
            "the generic mutation fence must reject executable schedule changes: \
             {update_response}"
        );

        let unchanged = get_note_props(&owner_rt, note_id).await;
        assert_eq!(unchanged["payload"], original_action, "{unchanged}");
        assert_eq!(unchanged["repeat"], Value::Null, "{unchanged}");

        let daemon_rt = KhiveRuntime::new(RuntimeConfig {
            db_path: Some(std::path::PathBuf::from(&db_path)),
            default_namespace: Namespace::local(),
            embedding_model: None,
            additional_embedding_models: vec![],
            gate,
            actor_id: Some("lambda:daemon".to_string()),
            packs: vec!["kg".to_string(), "schedule".to_string()],
            ..Default::default()
        })
        .expect("daemon runtime");
        let daemon_server = KhiveMcpServer::new(daemon_rt.clone()).expect("daemon server");
        let summary = run_pending_events_on(&daemon_rt, &daemon_server, false)
            .await
            .expect("drain");

        assert_eq!(summary.failed, 0, "{summary:?}");
        assert_eq!(summary.fired, 1, "{summary:?}");
        assert_eq!(
            note_content_count(&daemon_rt, "observation", "owner-approved schedule intent").await,
            1
        );
        assert_eq!(
            note_content_count(
                &daemon_rt,
                "observation",
                "attacker-selected schedule intent"
            )
            .await,
            0,
            "actor B's rejected payload must never replay with actor A's authority"
        );
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn scheduled_action_replay_uses_creator_not_daemon_identity() {
        let (_tmp, db_path) = tmp_db();
        let creator_cfg = RuntimeConfig {
            db_path: Some(std::path::PathBuf::from(&db_path)),
            default_namespace: Namespace::parse("local").unwrap(),
            embedding_model: None,
            additional_embedding_models: vec![],
            gate: std::sync::Arc::new(DenyCreatorCreateGate),
            actor_id: Some("lambda:schedule-owner".to_string()),
            packs: vec!["kg".to_string(), "schedule".to_string()],
            ..Default::default()
        };
        let creator_rt = KhiveRuntime::new(creator_cfg).expect("creator runtime");
        let action = "create(kind=\"observation\", content=\"identity fence marker\")";
        let note_id = create_scheduled_event(
            &creator_rt,
            "local",
            &due_rfc3339(),
            Some(action),
            None,
            "schedule",
        )
        .await;

        let daemon_cfg = RuntimeConfig {
            db_path: Some(std::path::PathBuf::from(&db_path)),
            default_namespace: Namespace::parse("local").unwrap(),
            embedding_model: None,
            additional_embedding_models: vec![],
            gate: std::sync::Arc::new(DenyCreatorCreateGate),
            actor_id: Some("lambda:daemon".to_string()),
            packs: vec!["kg".to_string(), "schedule".to_string()],
            ..Default::default()
        };
        let rt = KhiveRuntime::new(daemon_cfg).expect("daemon runtime");
        let server = KhiveMcpServer::new(rt.clone()).expect("server");

        let summary = run_pending_events_on(&rt, &server, false)
            .await
            .expect("drain");

        assert_eq!(summary.failed, 1, "creator gate denial must be visible");
        assert_eq!(
            note_content_count(&rt, "observation", "identity fence marker").await,
            0,
            "replay as the daemon would bypass the creator's denial"
        );
        let props = get_note_props(&rt, note_id).await;
        assert_eq!(
            props["status"], "pending",
            "failed one-shot remains retryable"
        );
        assert_eq!(summary.retry_pending, 1);
        assert!(
            props["dispatch_error"]
                .as_str()
                .is_some_and(|error| error.contains("creator is not authorized")),
            "dispatch failure must be persisted: {props}"
        );
        assert!(props["dispatch_failed_at"].as_str().is_some(), "{props}");
    }

    /// A canonical `schedule.schedule` payload that passes write-time
    /// validation must dispatch with zero failures at trigger time, proving
    /// write-time acceptance and trigger-time replay agree. Runs serially
    /// with a raised writer-pool checkout timeout to remove CI scheduler
    /// contention as a source of flakiness — see "Writer-pool checkout
    /// contention under CI" in `crates/khive-mcp/docs/pending-events.md`.
    #[tokio::test]
    #[serial_test::serial]
    #[serial_test::serial(config_ledger)]
    async fn replayable_action_dispatches_without_failure_at_trigger_time() {
        struct RestoreTimeout(Option<String>);
        impl Drop for RestoreTimeout {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(v) => std::env::set_var("KHIVE_CHECKOUT_TIMEOUT_SECS", v),
                    None => std::env::remove_var("KHIVE_CHECKOUT_TIMEOUT_SECS"),
                }
            }
        }
        let prior_timeout = std::env::var("KHIVE_CHECKOUT_TIMEOUT_SECS").ok();
        let _restore = RestoreTimeout(prior_timeout.clone());
        // #705: an instrumented coverage run (cargo llvm-cov --workspace) runs
        // this test's binary alongside every other workspace test binary, and
        // instrumentation overhead widens the contention window this test's
        // 120s floor was sized for on a plain (uninstrumented) run. Rather than
        // unconditionally clobbering down to "120" — which would silently
        // discard a larger value the coverage job set specifically for this
        // path — take the max of the ambient value (if any) and the 120s
        // floor, so a caller can raise it further without this test undoing
        // that raise.
        let effective_timeout = prior_timeout
            .as_deref()
            .and_then(|v| v.parse::<u64>().ok())
            .map(|ambient| ambient.max(120))
            .unwrap_or(120);
        std::env::set_var("KHIVE_CHECKOUT_TIMEOUT_SECS", effective_timeout.to_string());

        let (_tmp, db_path) = tmp_db();
        let rt = make_rt(&db_path).await;

        let past = due_rfc3339();
        let id = create_scheduled_event(
            &rt,
            "local",
            &past,
            Some("schedule.remind(content=\"ping\", at=\"2099-01-01T00:00:00Z\")"),
            None,
            "schedule",
        )
        .await;

        let summary = drain_for_test(&db_path).await.expect("drain");

        assert_eq!(
            summary.failed, 0,
            "a write-time-replayable action must dispatch cleanly at trigger time"
        );
        assert!(
            summary.fired >= 1 || summary.advanced >= 1,
            "the event must be processed"
        );

        let props = get_note_props(&rt, id).await;
        assert_eq!(props["status"].as_str(), Some("fired"));
    }

    /// A legacy stored action containing a `$prev` reference must be
    /// rejected by `dispatch_action` with an error naming the non-literal
    /// argument, not silently dropped and dispatched with missing/wrong
    /// data — asserted on the specific error text so a downstream handler's
    /// unrelated "missing argument" rejection can't mask a reintroduced
    /// silent-drop bug.
    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn dispatch_action_rejects_non_literal_prev_reference() {
        let (_tmp, db_path) = tmp_db();
        let rt = make_rt(&db_path).await;
        let server = KhiveMcpServer::new(rt.clone()).expect("server");

        let err = dispatch_action(
            "stats() | get(id=$prev.id)",
            "local",
            Some(VerifiedActor::new("lambda:test").expect("verified actor")),
            &server,
            false,
        )
        .await
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("not replayable"),
            "expected the specific non-literal-argument rejection message, got: {msg}"
        );
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn replay_defense_rejects_legacy_internal_subhandler_payload() {
        let (_tmp, db_path) = tmp_db();
        let rt = make_rt(&db_path).await;
        let server = KhiveMcpServer::new(rt).expect("server");

        let err = dispatch_action(
            "comm.ingest(namespace=\"local\", from=\"email:a@example.com\", \
             to=\"email:b@example.com\", content=\"forged inbound\")",
            "local",
            Some(VerifiedActor::new("lambda:scheduler").expect("verified actor")),
            &server,
            false,
        )
        .await
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("internal subhandler") && msg.contains("comm.ingest"),
            "replay must preserve public-surface visibility for legacy/hand-written rows: {msg}"
        );
    }

    /// Same scenario end-to-end through the drain: confirms the rejection
    /// surfaces as a counted failure rather than aborting the drain or being
    /// swallowed, and that the drain still completes.
    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn dispatch_rejects_legacy_prev_reference_instead_of_dropping_it() {
        let (_tmp, db_path) = tmp_db();
        let rt = make_rt(&db_path).await;

        let past = due_rfc3339();
        let _id = create_scheduled_event(
            &rt,
            "local",
            &past,
            Some("stats() | get(id=$prev.id)"),
            None,
            "schedule",
        )
        .await;

        let summary = drain_for_test(&db_path)
            .await
            .expect("drain must not abort or panic on a legacy $prev row");

        assert!(
            summary.failed >= 1,
            "a legacy $prev reference must surface as a dispatch failure, not a silent drop"
        );
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn legacy_multi_op_action_is_terminally_refused_without_partial_replay() {
        let (_tmp, db_path) = tmp_db();
        let rt = make_rt(&db_path).await;
        let server = KhiveMcpServer::new(rt.clone()).expect("server");
        let marker = "legacy-batch-success-must-never-run";
        let action = json!([
            {
                "tool": "create",
                "args": {"kind": "observation", "content": marker}
            },
            {"tool": "this_verb_does_not_exist", "args": {}}
        ])
        .to_string();
        let id = create_scheduled_event(
            &rt,
            "local",
            &due_rfc3339(),
            Some(&action),
            None,
            "schedule",
        )
        .await;

        let first = run_pending_events_on(&rt, &server, false)
            .await
            .expect("first drain");
        assert_eq!(
            first.invoked, 0,
            "a stored batch must be refused pre-invocation"
        );
        assert_eq!(first.failed, 1);
        assert_eq!(note_content_count(&rt, "observation", marker).await, 0);
        let props = get_note_props(&rt, id).await;
        assert_eq!(props["status"], "failed", "{props}");
        assert_eq!(
            props["dispatch_receipt"]["state"],
            DispatchReceiptState::NotInvoked.as_str(),
            "{props}"
        );
        assert!(props["dispatch_receipt"]["error"]
            .as_str()
            .is_some_and(|error| error.contains("multiple operations")));

        let second = run_pending_events_on(&rt, &server, false)
            .await
            .expect("second drain");
        assert_eq!(second.invoked, 0);
        assert_eq!(note_content_count(&rt, "observation", marker).await, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[serial_test::serial(config_ledger)]
    async fn renewable_lease_prevents_live_overrun_reclaim_and_double_dispatch() {
        let (_tmp, db_path) = tmp_db();
        let rt = make_rt(&db_path).await;
        let marker = "renewable-lease-single-invocation";
        let state = std::sync::Arc::new(AsyncBlockingSideEffectState::default());
        let _release_verb_on_unwind = ReleaseAsyncBlockingVerbOnDrop(state.clone());
        let mut builder = khive_runtime::VerbRegistryBuilder::new();
        builder.with_default_namespace("local");
        builder.register(AsyncBlockingSideEffectPack {
            runtime: rt.clone(),
            marker: marker.to_string(),
            state: state.clone(),
        });
        let server = KhiveMcpServer::from_registry(builder.build().expect("test registry"));
        let action = "test.async_blocking_side_effect()";
        let id =
            create_scheduled_event(&rt, "local", &due_rfc3339(), Some(action), None, "schedule")
                .await;
        let lease = short_test_lease();
        let entered = state.entered.notified();
        let drain_rt = rt.clone();
        let drain_server = server.clone();
        let first = tokio::spawn(async move {
            run_pending_events_on_with_lease(&drain_rt, &drain_server, false, lease).await
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), entered)
            .await
            .expect("dispatch entered async blocking verb");

        let invoking_props = get_note_props(&rt, id).await;
        assert_eq!(invoking_props["status"], "firing");
        let invocation_started_at = invoking_props["dispatch_receipt"]["invocation_started_at"]
            .as_i64()
            .expect("invocation start timestamp");
        let ttl_micros = i64::try_from(lease.ttl.as_micros()).expect("test TTL fits in i64");
        let original_deadline = invocation_started_at
            .checked_add(ttl_micros)
            .expect("test lease deadline fits in i64");
        let proof_horizon = original_deadline
            .checked_add(ttl_micros)
            .expect("multi-TTL proof horizon fits in i64");
        let renewal_margin_micros = i64::try_from(lease.renew_every.as_micros())
            .expect("test renewal interval fits in i64");
        let (live_props, observed_at, live_deadline) =
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    let props = get_note_props(&rt, id).await;
                    let observed_at = Utc::now().timestamp_micros();
                    let future_margin = observed_at
                        .checked_add(renewal_margin_micros)
                        .expect("future-margin timestamp fits in i64");
                    let deadline = props["lease_expires_at"].as_i64().unwrap_or(i64::MIN);
                    if observed_at > proof_horizon && deadline > future_margin {
                        break (props, observed_at, deadline);
                    }
                    tokio::time::sleep(lease.renew_every.min(std::time::Duration::from_millis(10)))
                        .await;
                }
            })
            .await
            .expect("live dispatch lease did not remain renewable beyond two lease durations");
        assert_eq!(live_props["status"], "firing");
        assert!(
            observed_at > proof_horizon,
            "proof must observe the dispatch after two original lease durations"
        );
        assert!(
            live_deadline
                > observed_at
                    .checked_add(renewal_margin_micros)
                    .expect("future-margin timestamp fits in i64"),
            "live dispatch must retain a future lease after the multi-TTL horizon: {live_props}"
        );

        let second = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            run_pending_events_on_with_lease(&rt, &server, false, lease),
        )
        .await
        .expect("competing drain blocked, indicating a duplicate invocation")
        .expect("second drain");
        assert_eq!(second.reclaimed, 0, "live lease must not be reclaimed");
        assert_eq!(second.invoked, 0, "second drain must not invoke the action");

        state.release.notify_one();
        let first = tokio::time::timeout(std::time::Duration::from_secs(2), first)
            .await
            .expect("first drain completes")
            .expect("first drain task joins")
            .expect("first drain succeeds");
        assert_eq!(first.invoked, 1);
        assert_eq!(first.outcomes_persisted, 1);
        assert_eq!(first.finalized, 1);
        assert_eq!(first.fired, 1);
        assert_eq!(
            state.invocations.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the target verb must be entered exactly once"
        );
        assert_eq!(note_content_count(&rt, "observation", marker).await, 1);
        let final_props = get_note_props(&rt, id).await;
        assert_eq!(final_props["dispatch_receipt"]["state"], "succeeded");
        assert!(final_props.get("firing_at").is_none());
        assert!(final_props.get("lease_expires_at").is_none());
    }

    #[tokio::test]
    async fn renewal_between_reclaim_scan_and_finalize_preserves_live_owner() {
        let (_tmp, db_path) = tmp_db();
        let rt = make_rt(&db_path).await;
        let trigger = due_rfc3339();
        let id =
            create_scheduled_event(&rt, "local", &trigger, Some("stats()"), None, "schedule").await;
        let claim = claim_for_test(&rt, id, &trigger).await;
        let lease = short_test_lease();
        assert!(mark_dispatch_invoking(&rt, "local", id, &claim, lease)
            .await
            .expect("mark invoking"));
        expire_dispatch_lease_for_test(&rt, id).await;

        // Model a reclaim pass that selected the expired row and retained its
        // stale snapshot, then lost the writer race to the live owner's
        // renewal. Recovery must re-check the deadline in its final CAS.
        let observed_expired_at = Utc::now().timestamp_micros();
        let selected_properties = get_raw_note_properties(&rt, id).await;
        let mut stale_properties: Value =
            serde_json::from_str(&selected_properties).expect("selected properties JSON");
        let mut stale_receipt = stale_properties["dispatch_receipt"].clone();
        let stale_completion = completion_from_receipt(&stale_receipt);
        stale_receipt["state"] = json!("indeterminate");
        stale_receipt["completed_at"] = json!(observed_expired_at);
        stale_receipt["error"] = json!(match &stale_completion {
            DispatchCompletion::Indeterminate(error) => error.as_str(),
            _ => "unexpected stale receipt state",
        });
        let trigger_fixed = trigger
            .parse::<DateTime<FixedOffset>>()
            .expect("fixed-offset trigger");
        let (stale_final, _) = final_properties_after_dispatch(
            std::mem::take(&mut stale_properties),
            stale_receipt,
            &stale_completion,
            trigger_fixed.with_timezone(&Utc),
            *trigger_fixed.offset(),
            &None,
        );

        assert!(renew_dispatch_lease(&rt, "local", id, &claim, lease)
            .await
            .expect("live owner renews"));
        assert!(
            !finalize_expired_firing_event(
                &rt,
                "local",
                id,
                &stale_final,
                Utc::now().timestamp_micros(),
                &claim,
                RecoverySnapshot {
                    expired_at: observed_expired_at,
                    properties: &selected_properties,
                },
            )
            .await
            .expect("stale recovery finalize"),
            "a renewal newer than the recovery snapshot must fence stale finalization"
        );
        let live = get_note_props(&rt, id).await;
        assert_eq!(live["status"], "firing");
        assert_eq!(live["dispatch_receipt"]["state"], "invoking");
        assert!(live["lease_expires_at"]
            .as_i64()
            .is_some_and(|deadline| deadline > observed_expired_at));

        let receipt =
            persist_dispatch_outcome(&rt, "local", id, &claim, &DispatchCompletion::Succeeded)
                .await
                .expect("persist live outcome")
                .expect("owner still holds receipt");
        let (final_properties, _) = final_properties_after_dispatch(
            live,
            receipt,
            &DispatchCompletion::Succeeded,
            trigger_fixed.with_timezone(&Utc),
            *trigger_fixed.offset(),
            &None,
        );
        let expected_properties = get_raw_note_properties(&rt, id).await;
        assert!(finalize_fired_event(
            &rt,
            "local",
            id,
            &final_properties,
            Utc::now().timestamp_micros(),
            &claim,
            &expected_properties,
        )
        .await
        .expect("live owner finalizes"));
        assert_eq!(get_note_props(&rt, id).await["status"], "fired");
    }

    #[tokio::test]
    async fn durable_success_after_reclaim_scan_fences_stale_recovery_finalize() {
        let (_tmp, db_path) = tmp_db();
        let rt = make_rt(&db_path).await;
        let trigger = due_rfc3339();
        let id =
            create_scheduled_event(&rt, "local", &trigger, Some("stats()"), None, "schedule").await;
        let claim = claim_for_test(&rt, id, &trigger).await;
        assert!(
            mark_dispatch_invoking(&rt, "local", id, &claim, short_test_lease())
                .await
                .expect("mark invoking")
        );
        expire_dispatch_lease_for_test(&rt, id).await;

        let selected_properties = get_raw_note_properties(&rt, id).await;
        let mut stale_properties: Value =
            serde_json::from_str(&selected_properties).expect("selected properties JSON");
        let mut stale_receipt = stale_properties["dispatch_receipt"].clone();
        let stale_completion = completion_from_receipt(&stale_receipt);

        let durable_receipt =
            persist_dispatch_outcome(&rt, "local", id, &claim, &DispatchCompletion::Succeeded)
                .await
                .expect("persist success")
                .expect("claim still owned");
        let observed_expired_at = Utc::now().timestamp_micros();
        stale_receipt["state"] = json!(DispatchReceiptState::Indeterminate.as_str());
        stale_receipt["completed_at"] = json!(observed_expired_at);
        stale_receipt["error"] = json!(match &stale_completion {
            DispatchCompletion::Indeterminate(error) => error.as_str(),
            _ => "unexpected selected receipt state",
        });
        stale_receipt["error_payload"] = Value::Null;
        let trigger_fixed = trigger
            .parse::<DateTime<FixedOffset>>()
            .expect("fixed-offset trigger");
        let (stale_final, _) = final_properties_after_dispatch(
            std::mem::take(&mut stale_properties),
            stale_receipt,
            &stale_completion,
            trigger_fixed.with_timezone(&Utc),
            *trigger_fixed.offset(),
            &None,
        );

        assert!(
            !finalize_expired_firing_event(
                &rt,
                "local",
                id,
                &stale_final,
                Utc::now().timestamp_micros(),
                &claim,
                RecoverySnapshot {
                    expired_at: observed_expired_at,
                    properties: &selected_properties,
                },
            )
            .await
            .expect("stale recovery finalize"),
            "recovery selected an invoking snapshot and must not overwrite a later durable success"
        );
        let current = get_note_props(&rt, id).await;
        assert_eq!(current["status"], "firing", "{current}");
        assert_eq!(current["dispatch_receipt"], durable_receipt, "{current}");
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn persisted_success_outcome_resumes_finalization_without_reinvocation() {
        let (_tmp, db_path) = tmp_db();
        let rt = make_rt(&db_path).await;
        let server = KhiveMcpServer::new(rt.clone()).expect("server");
        let marker = "success-receipt-crash-marker";
        let trigger = due_rfc3339();
        let action = format!("create(kind=\"observation\", content=\"{marker}\")");
        let id =
            create_scheduled_event(&rt, "local", &trigger, Some(&action), None, "schedule").await;
        let claim = claim_for_test(&rt, id, &trigger).await;
        assert!(
            mark_dispatch_invoking(&rt, "local", id, &claim, short_test_lease())
                .await
                .expect("mark invoking")
        );
        create_marker_directly(&rt, marker).await;
        assert!(
            persist_dispatch_outcome(&rt, "local", id, &claim, &DispatchCompletion::Succeeded,)
                .await
                .expect("persist outcome")
                .is_some()
        );

        let recovered = run_pending_events_on(&rt, &server, false)
            .await
            .expect("recover finalized outcome");
        assert_eq!(recovered.reclaimed, 1);
        assert_eq!(recovered.invoked, 0);
        assert_eq!(recovered.fired, 1);
        assert_eq!(recovered.finalized, 1);
        assert_eq!(note_content_count(&rt, "observation", marker).await, 1);
        let props = get_note_props(&rt, id).await;
        assert_eq!(props["status"], "fired");
        assert_eq!(props["dispatch_receipt"]["state"], "succeeded");
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn expired_row_finalize_failure_does_not_wedge_later_due_work() {
        let (_tmp, db_path) = tmp_db();
        let rt = make_rt(&db_path).await;
        let server = KhiveMcpServer::new(rt.clone()).expect("server");
        let poison_trigger = due_rfc3339();
        let poison_id = create_scheduled_event(
            &rt,
            "local",
            &poison_trigger,
            Some("stats()"),
            None,
            "schedule",
        )
        .await;
        let poison_claim = claim_for_test(&rt, poison_id, &poison_trigger).await;
        assert!(
            mark_dispatch_invoking(&rt, "local", poison_id, &poison_claim, short_test_lease())
                .await
                .expect("mark poison invoking")
        );
        assert!(persist_dispatch_outcome(
            &rt,
            "local",
            poison_id,
            &poison_claim,
            &DispatchCompletion::Succeeded,
        )
        .await
        .expect("persist poison success")
        .is_some());

        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        let marker = "due-work-after-poison-expired-row";
        let action = format!("create(kind=\"observation\", content=\"{marker}\")");
        let later_id = create_scheduled_event(
            &rt,
            "local",
            &due_rfc3339(),
            Some(&action),
            None,
            "schedule",
        )
        .await;

        {
            let mut writer = rt.sql().writer().await.expect("writer");
            writer
                .execute(SqlStatement {
                    sql: format!(
                        "CREATE TRIGGER test_fail_expired_outcome_finalize \
                         BEFORE UPDATE OF properties ON notes \
                         WHEN OLD.id = '{poison_id}' \
                           AND json_extract(OLD.properties, '$.status') = 'firing' \
                         BEGIN \
                           SELECT RAISE(FAIL, 'injected expired finalization failure'); \
                         END"
                    ),
                    params: vec![],
                    label: Some("test_install_expired_finalize_failure".into()),
                })
                .await
                .expect("install expired finalization failure trigger");
        }

        let summary = run_pending_events_on(&rt, &server, false)
            .await
            .expect("row-local recovery failure is absorbed");
        assert_eq!(summary.failed, 1, "{summary:?}");
        assert_eq!(summary.fired, 1, "later due work must still fire");
        assert_eq!(note_content_count(&rt, "observation", marker).await, 1);
        assert_eq!(get_note_props(&rt, poison_id).await["status"], "firing");
        assert_eq!(get_note_props(&rt, later_id).await["status"], "fired");
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn malformed_terminal_receipts_fail_indeterminate_without_replay() {
        let (_tmp, db_path) = tmp_db();
        let rt = make_rt(&db_path).await;
        let server = KhiveMcpServer::new(rt.clone()).expect("server");
        let marker = "malformed-terminal-receipt-must-not-dispatch";
        let action = format!("create(kind=\"observation\", content=\"{marker}\")");
        let mut ids = Vec::new();

        for case in 0..5 {
            let trigger = due_rfc3339();
            let id =
                create_scheduled_event(&rt, "local", &trigger, Some(&action), None, "schedule")
                    .await;
            claim_for_test(&rt, id, &trigger).await;
            let mut receipt = get_note_props(&rt, id).await["dispatch_receipt"].clone();
            match case {
                0 => {
                    receipt["state"] = json!(DispatchReceiptState::Succeeded.as_str());
                    receipt["error"] = Value::Null;
                    receipt
                        .as_object_mut()
                        .expect("receipt object")
                        .remove("completed_at");
                }
                1 => {
                    receipt["state"] = json!(DispatchReceiptState::Succeeded.as_str());
                    receipt["completed_at"] = json!("not-a-timestamp");
                    receipt["error"] = Value::Null;
                }
                2 => {
                    receipt["state"] = json!(DispatchReceiptState::Failed.as_str());
                    receipt["error"] = json!("simulated dispatch failure");
                    receipt
                        .as_object_mut()
                        .expect("receipt object")
                        .remove("completed_at");
                }
                3 => {
                    receipt["state"] = json!(DispatchReceiptState::Failed.as_str());
                    receipt["completed_at"] = json!(Utc::now().timestamp_micros());
                    receipt
                        .as_object_mut()
                        .expect("receipt object")
                        .remove("error");
                }
                4 => {
                    receipt["state"] = json!(DispatchReceiptState::Succeeded.as_str());
                    receipt["completed_at"] = json!(Utc::now().timestamp_micros());
                    receipt["error"] = Value::Null;
                    receipt["occurrence_id"] = json!(uuid::Uuid::new_v4());
                }
                _ => unreachable!(),
            }
            overwrite_dispatch_receipt_and_expire_for_test(&rt, id, &receipt).await;
            ids.push(id);
        }

        let recovered = run_pending_events_on(&rt, &server, false)
            .await
            .expect("malformed receipts are quarantined per row");
        assert_eq!(recovered.reclaimed, 5);
        assert_eq!(recovered.invoked, 0);
        assert_eq!(recovered.outcomes_persisted, 5);
        assert_eq!(recovered.indeterminate, 5);
        assert_eq!(recovered.finalized, 5);
        assert_eq!(recovered.fired, 0);
        assert_eq!(recovered.retry_pending, 0);
        assert_eq!(recovered.failed, 5);
        assert_eq!(note_content_count(&rt, "observation", marker).await, 0);

        for id in ids {
            let props = get_note_props(&rt, id).await;
            assert_eq!(props["status"], "failed", "{props}");
            assert_eq!(
                props["dispatch_receipt"]["state"],
                DispatchReceiptState::Indeterminate.as_str(),
                "{props}"
            );
            assert!(
                props["dispatch_receipt"]["completed_at"].as_i64().is_some(),
                "{props}"
            );
            assert!(
                props["dispatch_receipt"]["error"]
                    .as_str()
                    .is_some_and(|error| error.contains("refusing automatic replay")),
                "{props}"
            );
            assert!(
                props["dispatch_receipt"]["invalid_receipt"].is_object(),
                "the malformed source receipt must remain available for diagnosis: {props}"
            );
        }
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn expired_invoking_receipt_fails_indeterminate_without_double_dispatch() {
        let (_tmp, db_path) = tmp_db();
        let rt = make_rt(&db_path).await;
        let server = KhiveMcpServer::new(rt.clone()).expect("server");
        let marker = "indeterminate-crash-marker";
        let trigger = due_rfc3339();
        let action = format!("create(kind=\"observation\", content=\"{marker}\")");
        let id =
            create_scheduled_event(&rt, "local", &trigger, Some(&action), None, "schedule").await;
        let claim = claim_for_test(&rt, id, &trigger).await;
        assert!(
            mark_dispatch_invoking(&rt, "local", id, &claim, short_test_lease())
                .await
                .expect("mark invoking")
        );
        create_marker_directly(&rt, marker).await;
        expire_dispatch_lease_for_test(&rt, id).await;

        let recovered = run_pending_events_on(&rt, &server, false)
            .await
            .expect("reconcile ambiguous crash");
        assert_eq!(recovered.reclaimed, 1);
        assert_eq!(recovered.invoked, 0);
        assert_eq!(recovered.outcomes_persisted, 1);
        assert_eq!(recovered.indeterminate, 1);
        assert_eq!(note_content_count(&rt, "observation", marker).await, 1);
        let props = get_note_props(&rt, id).await;
        assert_eq!(props["status"], "failed", "ambiguous outcome fails closed");
        assert_eq!(props["dispatch_receipt"]["state"], "indeterminate");

        let again = run_pending_events_on(&rt, &server, false)
            .await
            .expect("terminal row is not replayed");
        assert_eq!(again.invoked, 0);
        assert_eq!(note_content_count(&rt, "observation", marker).await, 1);
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn failed_one_shot_is_retryable_and_succeeds_once_on_later_drain() {
        let (_tmp, db_path) = tmp_db();
        let gate = std::sync::Arc::new(FailFirstCreateGate::default());
        let rt = KhiveRuntime::new(RuntimeConfig {
            db_path: Some(std::path::PathBuf::from(&db_path)),
            default_namespace: Namespace::local(),
            embedding_model: None,
            additional_embedding_models: vec![],
            gate: gate.clone(),
            packs: vec!["kg".to_string(), "schedule".to_string()],
            ..Default::default()
        })
        .expect("runtime");
        let server = KhiveMcpServer::new(rt.clone()).expect("server");
        let marker = "failed-one-shot-recovery-marker";
        let action = format!("create(kind=\"observation\", content=\"{marker}\")");
        let id = create_scheduled_event(
            &rt,
            "local",
            &due_rfc3339(),
            Some(&action),
            None,
            "schedule",
        )
        .await;

        let first = run_pending_events_on(&rt, &server, false)
            .await
            .expect("first drain");
        assert_eq!(first.invoked, 1);
        assert_eq!(first.outcomes_persisted, 1);
        assert_eq!(first.retry_pending, 1);
        assert_eq!(first.fired, 0);
        let first_props = get_note_props(&rt, id).await;
        assert_eq!(first_props["status"], "pending");
        let first_occurrence = first_props["dispatch_receipt"]["occurrence_id"]
            .as_str()
            .expect("occurrence receipt")
            .to_string();
        let first_invocation = first_props["dispatch_receipt"]["invocation_id"]
            .as_str()
            .expect("invocation receipt")
            .to_string();
        assert_eq!(note_content_count(&rt, "observation", marker).await, 0);

        let second = run_pending_events_on(&rt, &server, false)
            .await
            .expect("retry drain");
        assert_eq!(second.invoked, 1);
        assert_eq!(second.outcomes_persisted, 1);
        assert_eq!(second.fired, 1);
        assert_eq!(note_content_count(&rt, "observation", marker).await, 1);
        let second_props = get_note_props(&rt, id).await;
        assert_eq!(second_props["status"], "fired");
        assert_eq!(
            second_props["dispatch_receipt"]["occurrence_id"].as_str(),
            Some(first_occurrence.as_str()),
            "retries share one deterministic occurrence identity"
        );
        assert_ne!(
            second_props["dispatch_receipt"]["invocation_id"].as_str(),
            Some(first_invocation.as_str()),
            "each retry receives a distinct invocation identity"
        );
        assert_eq!(
            gate.invocations.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "one failed invocation and one successful retry"
        );
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn ambiguous_side_effect_is_indeterminate_and_never_blindly_retried() {
        let (_tmp, db_path) = tmp_db();
        let rt = make_rt(&db_path).await;
        let marker = "ambiguous-outcome-single-side-effect";
        let outbound_id = uuid::Uuid::new_v4();
        let invocations = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut builder = khive_runtime::VerbRegistryBuilder::new();
        builder.with_default_namespace("local");
        builder.register(AmbiguousSideEffectPack {
            runtime: rt.clone(),
            marker: marker.to_string(),
            outbound_id,
            invocations: invocations.clone(),
        });
        let server = KhiveMcpServer::from_registry(builder.build().expect("test registry"));
        let id = create_scheduled_event(
            &rt,
            "local",
            &due_rfc3339(),
            Some("test.ambiguous_side_effect()"),
            None,
            "schedule",
        )
        .await;

        let first = run_pending_events_on(&rt, &server, false)
            .await
            .expect("first drain");
        assert_eq!(first.invoked, 1);
        assert_eq!(first.outcomes_persisted, 1);
        assert_eq!(first.indeterminate, 1);
        assert_eq!(first.retry_pending, 0);
        assert_eq!(first.fired, 0);
        assert_eq!(note_content_count(&rt, "observation", marker).await, 1);
        assert_eq!(invocations.load(std::sync::atomic::Ordering::SeqCst), 1);
        let props = get_note_props(&rt, id).await;
        assert_eq!(props["status"], "failed", "{props}");
        assert_eq!(props["dispatch_receipt"]["state"], "indeterminate");
        assert_eq!(
            props["dispatch_receipt"]["error_payload"]["details"]["outbound_id"],
            outbound_id.to_string(),
            "the durable receipt must retain the comm.delivered correlation id: {props}"
        );

        let second = run_pending_events_on(&rt, &server, false)
            .await
            .expect("second drain");
        assert_eq!(second.invoked, 0);
        assert_eq!(note_content_count(&rt, "observation", marker).await, 1);
        assert_eq!(
            invocations.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "an ambiguous committed side effect must never be retried automatically"
        );
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn legacy_cron_row_fails_closed_before_action_invocation() {
        let (_tmp, db_path) = tmp_db();
        let rt = make_rt(&db_path).await;
        let server = KhiveMcpServer::new(rt.clone()).expect("server");
        let marker = "legacy-cron-must-not-dispatch";
        let action = format!("create(kind=\"observation\", content=\"{marker}\")");
        let id = create_scheduled_event(
            &rt,
            "local",
            &due_rfc3339(),
            Some(&action),
            Some("0 9 * * 1"),
            "schedule",
        )
        .await;

        let summary = run_pending_events_on(&rt, &server, false)
            .await
            .expect("legacy cron reconciliation");
        assert_eq!(summary.invoked, 0);
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.finalized, 1);
        assert_eq!(note_content_count(&rt, "observation", marker).await, 0);
        let props = get_note_props(&rt, id).await;
        assert_eq!(props["status"], "failed");
        assert!(props["dispatch_error"]
            .as_str()
            .is_some_and(|error| error.contains("unsupported repeat")));
        assert_eq!(
            props["dispatch_receipt"]["state"],
            DispatchReceiptState::NotInvoked.as_str(),
            "the durable claim receipt must survive unsupported-repeat refusal: {props}"
        );
        assert!(props["dispatch_receipt"]["occurrence_id"]
            .as_str()
            .is_some());
        assert!(props["dispatch_receipt"]["invocation_id"]
            .as_str()
            .is_some());
        assert!(props["dispatch_receipt"]["completed_at"].as_i64().is_some());
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn empty_payload_finalization_retains_not_invoked_receipt() {
        let (_tmp, db_path) = tmp_db();
        let rt = make_rt(&db_path).await;
        let server = KhiveMcpServer::new(rt.clone()).expect("server");
        let id = create_scheduled_event(&rt, "local", &due_rfc3339(), None, None, "schedule").await;

        let summary = run_pending_events_on(&rt, &server, false)
            .await
            .expect("empty payload is finalized per row");
        assert_eq!(summary.invoked, 0);
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.finalized, 1);
        let props = get_note_props(&rt, id).await;
        assert_eq!(props["status"], "failed", "{props}");
        assert_eq!(
            props["dispatch_receipt"]["state"],
            DispatchReceiptState::NotInvoked.as_str(),
            "{props}"
        );
        assert!(props["dispatch_receipt"]["completed_at"].as_i64().is_some());
        assert!(props["dispatch_receipt"]["error"]
            .as_str()
            .is_some_and(|error| error.contains("no executable payload")));
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn unsupported_repeat_finalize_failure_does_not_abort_later_rows() {
        let (_tmp, db_path) = tmp_db();
        let rt = make_rt(&db_path).await;
        let server = KhiveMcpServer::new(rt.clone()).expect("server");
        let cron_id = create_scheduled_event(
            &rt,
            "local",
            &due_rfc3339(),
            Some("stats()"),
            Some("0 9 * * 1"),
            "schedule",
        )
        .await;
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        let marker = "row-after-cron-finalize-failure";
        let action = format!("create(kind=\"observation\", content=\"{marker}\")");
        let later_id = create_scheduled_event(
            &rt,
            "local",
            &due_rfc3339(),
            Some(&action),
            None,
            "schedule",
        )
        .await;

        {
            let mut writer = rt.sql().writer().await.expect("writer");
            writer
                .execute(SqlStatement {
                    sql: format!(
                        "CREATE TRIGGER test_fail_unsupported_repeat_finalize \
                         BEFORE UPDATE OF properties ON notes \
                         WHEN OLD.id = '{cron_id}' \
                           AND json_extract(OLD.properties, '$.status') = 'firing' \
                           AND json_extract(NEW.properties, '$.status') = 'failed' \
                         BEGIN \
                           SELECT RAISE(FAIL, 'injected cron finalization failure'); \
                         END"
                    ),
                    params: vec![],
                    label: Some("test_install_cron_finalize_failure".into()),
                })
                .await
                .expect("install finalization failure trigger");
        }

        let summary = run_pending_events_on(&rt, &server, false)
            .await
            .expect("one row-level finalization failure must not abort the drain");
        assert_eq!(summary.scanned, 2);
        assert_eq!(summary.invoked, 1);
        assert_eq!(summary.fired, 1);
        assert_eq!(summary.failed, 1);
        assert_eq!(note_content_count(&rt, "observation", marker).await, 1);
        assert_eq!(get_note_props(&rt, cron_id).await["status"], "firing");
        assert_eq!(get_note_props(&rt, later_id).await["status"], "fired");
    }

    /// A `schedule.cancel` arriving after the drain has already CAS-claimed
    /// the row for firing must fail — proves a cancel can never be lost to a
    /// fire that was already in flight.
    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn fire_claim_wins_race_against_concurrent_cancel() {
        let (_tmp, db_path) = tmp_db();
        let rt = make_rt(&db_path).await;
        let server = KhiveMcpServer::new(rt.clone()).expect("server");

        let past = "2000-01-01T00:00:00Z";
        let id =
            create_scheduled_event(&rt, "local", past, Some("stats()"), None, "schedule").await;

        // Simulate the drain's claim (pending -> firing), which in the real
        // drain happens right after the page read and before dispatch.
        let claim = claim_for_test(&rt, id, past).await;

        // A `schedule.cancel` arriving after the claim in this race window
        // must now fail instead of clobbering the
        // in-flight fire.
        let cancel_ops = serde_json::to_string(&serde_json::json!([
            { "tool": "schedule.cancel", "args": { "id": id.to_string() } }
        ]))
        .expect("serialize cancel op");
        let cancel_result = server
            .dispatch_request_local(RequestParams {
                ops: cancel_ops,
                presentation: None,
                presentation_per_op: None,
                save_to: None,
                format: None,
                format_per_op: None,
                request_id: None,
            })
            .await
            .expect("dispatch_request_local must not error at the RPC layer");
        let cancel_json: Value = serde_json::from_str(&cancel_result).expect("valid JSON");
        let op_result = &cancel_json["results"][0];
        assert_eq!(
            op_result["ok"], false,
            "cancel of a claimed (firing) event must fail, not silently succeed: {cancel_json}"
        );
        let cancel_err = op_result["error"].as_str().unwrap_or("");
        assert!(
            cancel_err.contains("not pending"),
            "cancel must report the event is no longer pending; got: {cancel_err}"
        );

        // Finalize the fire as the drain would, then confirm the terminal
        // state is "fired" — the cancel never got a chance to overwrite it.
        let expected_properties = get_raw_note_properties(&rt, id).await;
        let finalized = finalize_fired_event(
            &rt,
            "local",
            id,
            &serde_json::json!({
                "trigger_at": past,
                "repeat": null,
                "status": "fired",
                "event_type": "schedule",
                "payload": "stats()",
                "fired_at": Utc::now().to_rfc3339(),
                "cancelled_at": null,
            }),
            Utc::now().timestamp_micros(),
            &claim,
            &expected_properties,
        )
        .await
        .expect("finalize query");
        assert!(
            finalized,
            "finalize must succeed on a row still in \"firing\""
        );

        let props = get_note_props(&rt, id).await;
        assert_eq!(
            props["status"].as_str().unwrap_or(""),
            "fired",
            "terminal state must be \"fired\"; cancel must not have won the race"
        );
    }

    /// Directly set a note's `properties` via raw SQL, bypassing the normal
    /// claim/finalize CAS paths. Used to deterministically fabricate a
    /// stale-`firing` row (as if a drain claimed it and then crashed before
    /// finalizing) without depending on wall-clock sleeps.
    async fn force_set_properties(rt: &KhiveRuntime, id: uuid::Uuid, properties: &Value) {
        let props_json = serde_json::to_string(properties).expect("serialize");
        let mut writer = rt.sql().writer().await.expect("writer");
        let rows = writer
            .execute(SqlStatement {
                sql: "UPDATE notes SET properties = ?1 WHERE id = ?2".to_string(),
                params: vec![SqlValue::Text(props_json), SqlValue::Text(id.to_string())],
                label: Some("test_force_set_properties".into()),
            })
            .await
            .expect("force update");
        assert_eq!(rows, 1, "test setup: row must exist");
    }

    /// A row claimed by a drain that then crashed before finalizing —
    /// `status="firing"` with a `firing_at` older than the stale timeout —
    /// must be reclaimed back to `pending` and fired on the next pass,
    /// instead of being wedged forever.
    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn stale_firing_row_is_reclaimed_and_fired() {
        let (_tmp, db_path) = tmp_db();
        let rt = make_rt(&db_path).await;

        let past = due_rfc3339();
        let id =
            create_scheduled_event(&rt, "local", &past, Some("stats()"), None, "schedule").await;

        // Simulate a drain claiming the row, then crashing before finalize:
        // status="firing" with a firing_at well past the stale timeout.
        let stale_firing_at =
            Utc::now().timestamp_micros() - (LEGACY_STALE_FIRING_TIMEOUT_MICROS * 2);
        force_set_properties(
            &rt,
            id,
            &json!({
                "trigger_at": past,
                "repeat": null,
                "status": "firing",
                "event_type": "schedule",
                "created_by_actor": "local",
                "payload": "stats()",
                "fired_at": null,
                "cancelled_at": null,
                "firing_at": stale_firing_at,
            }),
        )
        .await;

        let summary = drain_for_test(&db_path).await.expect("drain");

        assert!(
            summary.reclaimed >= 1,
            "the stale firing row must be reclaimed, got summary={summary:?}"
        );
        assert!(
            summary.fired >= 1 || summary.advanced >= 1,
            "the reclaimed row must be fired (or advanced) in the same pass, \
             got summary={summary:?}"
        );

        let props = get_note_props(&rt, id).await;
        assert_eq!(
            props["status"].as_str(),
            Some("fired"),
            "a reclaimed non-repeating event must end in \"fired\", got {props:?}"
        );
    }

    /// A row claimed *recently* (fresh `firing_at`, well within the stale
    /// timeout) must NOT be reclaimed — a live drain's in-flight claim is
    /// never stolen by the reclaim sweep.
    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn fresh_firing_row_is_not_reclaimed() {
        let (_tmp, db_path) = tmp_db();
        let rt = make_rt(&db_path).await;

        let past = "2000-01-01T00:00:00Z";
        let id =
            create_scheduled_event(&rt, "local", past, Some("stats()"), None, "schedule").await;

        // Fresh claim: firing_at = now, well under the stale threshold.
        let _claim = claim_for_test(&rt, id, past).await;

        let summary = drain_for_test(&db_path).await.expect("drain");

        assert_eq!(
            summary.reclaimed, 0,
            "a fresh firing row must not be reclaimed, got summary={summary:?}"
        );
        assert_eq!(
            summary.fired, 0,
            "a fresh firing row must not be fired by a drain pass that did not claim it"
        );

        let props = get_note_props(&rt, id).await;
        assert_eq!(
            props["status"].as_str(),
            Some("firing"),
            "a fresh firing row must remain firing (owned by the process that claimed it), \
             got {props:?}"
        );
    }

    /// Finalize must be bound to the owning claim token, not just
    /// `status='firing'`: a stale claimant (A) that resumes after a reclaim
    /// pass has already let a fresh claimant (B) re-claim the row must have
    /// its finalize become a no-op, leaving B's claim untouched.
    #[tokio::test]
    async fn stale_claimant_cannot_finalize_over_a_fresh_reclaim() {
        let (_tmp, db_path) = tmp_db();
        let rt = make_rt(&db_path).await;

        let past = "2000-01-01T00:00:00Z";
        let id =
            create_scheduled_event(&rt, "local", past, Some("stats()"), None, "schedule").await;

        let a_claim = claim_for_test(&rt, id, past).await;
        let mut writer = rt.sql().writer().await.expect("writer");
        assert_eq!(
            writer
                .execute(SqlStatement {
                    sql: "UPDATE notes SET properties = json_set( \
                            properties, '$.lease_expires_at', ?1) WHERE id = ?2"
                        .to_string(),
                    params: vec![
                        SqlValue::Integer(Utc::now().timestamp_micros() - 1),
                        SqlValue::Text(id.to_string()),
                    ],
                    label: Some("test_expire_a_dispatch_lease".into()),
                })
                .await
                .expect("expire A lease"),
            1
        );
        drop(writer);

        // A reclaim pass runs (as a live drain's periodic sweep would),
        // moving the row back to "pending" since A's firing_at is stale.
        let reclaimed = reclaim_stale_firing_events(&rt, Utc::now().timestamp_micros())
            .await
            .expect("reclaim query");
        assert_eq!(reclaimed.rows, 1, "A's stale claim must be reclaimed");
        assert_eq!(reclaimed.retry_pending, 1);
        assert_eq!(
            reclaimed.failed, 0,
            "an expired claimant that never began invocation is retryable, not a failed action"
        );
        let reclaimed_props = get_note_props(&rt, id).await;
        assert_eq!(reclaimed_props["status"], "pending", "{reclaimed_props}");
        assert_eq!(
            reclaimed_props["dispatch_receipt"]["state"],
            DispatchReceiptState::NotInvoked.as_str(),
            "claim expiry before invocation must be recorded truthfully: {reclaimed_props}"
        );
        assert!(
            reclaimed_props["dispatch_receipt"]["error"]
                .as_str()
                .is_some_and(|error| error.contains("before invocation began")),
            "the durable receipt must explain why no invocation occurred: {reclaimed_props}"
        );

        // Drain B re-claims the now-pending row, minting a fresh firing_at
        // token that differs from A's stale one.
        let b_claim = claim_for_test(&rt, id, past).await;
        assert_ne!(
            a_claim.invocation_id, b_claim.invocation_id,
            "B's invocation token must differ from A's stale token"
        );

        // A resumes (unaware it was reclaimed) and attempts to finalize using
        // its own stale claim token. This must be a no-op: it must NOT match
        // B's current firing_at, and must NOT clobber B's live claim.
        let expected_properties_for_a = get_raw_note_properties(&rt, id).await;
        let a_finalize_result = finalize_fired_event(
            &rt,
            "local",
            id,
            &json!({
                "trigger_at": past,
                "repeat": null,
                "status": "fired",
                "event_type": "schedule",
                "payload": "stats()",
                "fired_at": Utc::now().to_rfc3339(),
                "cancelled_at": null,
            }),
            Utc::now().timestamp_micros(),
            &a_claim,
            &expected_properties_for_a,
        )
        .await
        .expect("finalize query must not error");
        assert!(
            !a_finalize_result,
            "A's finalize with a stale claim token must be a no-op, not a successful write"
        );

        // B's claim must be completely intact: still "firing", still stamped
        // with B's own firing_at — A's stale finalize must not have touched it.
        let props_after_a = get_note_props(&rt, id).await;
        assert_eq!(
            props_after_a["status"].as_str(),
            Some("firing"),
            "B's claim must survive A's stale finalize attempt untouched, got {props_after_a:?}"
        );
        assert_eq!(
            props_after_a["firing_at"].as_i64(),
            Some(b_claim.firing_at),
            "B's firing_at token must be unchanged by A's stale finalize attempt"
        );

        // B now finalizes with its own (correct) claim token — this must
        // succeed, proving the fix doesn't wedge legitimate finalization.
        let expected_properties_for_b = get_raw_note_properties(&rt, id).await;
        let b_finalize_result = finalize_fired_event(
            &rt,
            "local",
            id,
            &json!({
                "trigger_at": past,
                "repeat": null,
                "status": "fired",
                "event_type": "schedule",
                "payload": "stats()",
                "fired_at": Utc::now().to_rfc3339(),
                "cancelled_at": null,
            }),
            Utc::now().timestamp_micros(),
            &b_claim,
            &expected_properties_for_b,
        )
        .await
        .expect("finalize query must not error");
        assert!(
            b_finalize_result,
            "B's finalize with its own claim token must succeed"
        );

        let final_props = get_note_props(&rt, id).await;
        assert_eq!(
            final_props["status"].as_str(),
            Some("fired"),
            "terminal state must be \"fired\" via B's own claim, got {final_props:?}"
        );
        assert!(
            final_props.get("firing_at").is_none() || final_props["firing_at"].is_null(),
            "firing_at must be cleared on terminal finalize, got {final_props:?}"
        );
    }

    /// Regression for the normal-finalization lost-update race (khive #1753).
    /// `finalize_fired_event` reads the row's CURRENT properties immediately
    /// before finalizing (see the call site above `final_properties_after_dispatch`
    /// in the main drain loop) and must refuse the terminal write if a
    /// concurrent writer changed properties since that read, even though the
    /// claim tokens (`firing_at`/`invocation_id`/lease) are still valid —
    /// those predicates alone do not detect an out-of-band property change.
    /// This deterministically reproduces "two reads from one revision": the
    /// snapshot captured here (`expected_properties`) is used for the stale
    /// finalize attempt AFTER a concurrent write has already landed, so the
    /// exact-equality predicate must fail. Before threading a real snapshot
    /// through, normal finalization always called with `snapshot=None`,
    /// which makes `AND (?9 IS NULL OR properties = ?9)` unconditionally
    /// true — this test reddens if that call reverts to `None`: the stale
    /// finalize would then succeed and the concurrent writer's
    /// `concurrent_marker` field would be silently discarded.
    #[tokio::test]
    async fn normal_finalize_refuses_when_a_concurrent_writer_changed_properties_since_the_read() {
        let (_tmp, db_path) = tmp_db();
        let rt = make_rt(&db_path).await;

        let past = "2000-01-01T00:00:00Z";
        let id =
            create_scheduled_event(&rt, "local", past, Some("stats()"), None, "schedule").await;
        let claim = claim_for_test(&rt, id, past).await;

        // The finalizer's fresh pre-write read (what `current_note_properties_text`
        // returns right before finalizing in production).
        let expected_properties = get_raw_note_properties(&rt, id).await;

        // A concurrent writer mutates the row AFTER that read while leaving
        // every claim predicate (status/firing_at/invocation_id/lease)
        // valid — e.g. an external property patch racing the finalizer.
        let mut writer = rt.sql().writer().await.expect("writer");
        let rows = writer
            .execute(SqlStatement {
                sql: "UPDATE notes SET properties = json_set(properties, \
                      '$.concurrent_marker', 'yes') WHERE id = ?1"
                    .to_string(),
                params: vec![SqlValue::Text(id.to_string())],
                label: Some("test_concurrent_property_write".into()),
            })
            .await
            .expect("concurrent write");
        assert_eq!(rows, 1);
        drop(writer);

        let final_props = json!({
            "trigger_at": past,
            "repeat": null,
            "status": "fired",
            "event_type": "schedule",
            "payload": "stats()",
            "fired_at": Utc::now().to_rfc3339(),
            "cancelled_at": null,
        });
        let finalized = finalize_fired_event(
            &rt,
            "local",
            id,
            &final_props,
            Utc::now().timestamp_micros(),
            &claim,
            &expected_properties,
        )
        .await
        .expect("finalize query must not error");
        assert!(
            !finalized,
            "finalize must refuse a terminal write when properties changed since the read it guards on"
        );

        let props_after = get_note_props(&rt, id).await;
        assert_eq!(
            props_after["status"].as_str(),
            Some("firing"),
            "the row must remain firing, not silently finalized over the concurrent writer's \
             change: {props_after:?}"
        );
        assert_eq!(
            props_after["concurrent_marker"].as_str(),
            Some("yes"),
            "the concurrent writer's field must survive the refused finalize: {props_after:?}"
        );

        // A finalize guarded on the CURRENT properties (as the real drain
        // loop does, re-reading right before this call) must still succeed.
        let fresh_properties = get_raw_note_properties(&rt, id).await;
        let finalized_fresh = finalize_fired_event(
            &rt,
            "local",
            id,
            &final_props,
            Utc::now().timestamp_micros(),
            &claim,
            &fresh_properties,
        )
        .await
        .expect("finalize query must not error");
        assert!(
            finalized_fresh,
            "finalize with a fresh snapshot must succeed"
        );
        assert_eq!(
            get_note_props(&rt, id).await["status"].as_str(),
            Some("fired")
        );
    }

    /// Regression test driven through the PRODUCTION
    /// drain entry point (`run_pending_events_on`) rather than calling
    /// `final_properties_after_dispatch` directly — this closes a gap a
    /// primitive-level test cannot: it would still pass unchanged if the
    /// drain loop's call site reverted to building `final_props` from the
    /// stale pre-claim `properties` snapshot instead of the freshly read
    /// `expected_properties`, since it would never invoke that call site at
    /// all. Uses `race_seam::pause_before_finalize_read` (test-only,
    /// compiled out of non-test builds) to force the concurrent property
    /// write to land deterministically between claim/dispatch and the
    /// finalizer's fresh current-properties read — no sleeps, no reliance on
    /// scheduler ordering.
    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn production_drain_preserves_a_property_written_between_claim_and_current_read() {
        let (_tmp, db_path) = tmp_db();
        let rt = make_rt(&db_path).await;
        let id = create_scheduled_event(
            &rt,
            "local",
            &due_rfc3339(),
            Some("stats()"),
            None,
            "schedule",
        )
        .await;

        let gate = race_seam::PauseGate {
            at: race_seam::PausePoint::BeforeFinalizeRead,
            reached: std::sync::Arc::new(tokio::sync::Barrier::new(2)),
            release: std::sync::Arc::new(tokio::sync::Barrier::new(2)),
        };

        let drain_task = {
            let rt = rt.clone();
            let gate = gate.clone();
            tokio::spawn(race_seam::PAUSE_GATE.scope(gate, async move {
                let server = KhiveMcpServer::new(rt.clone()).map_err(|e| anyhow::anyhow!("{e}"))?;
                run_pending_events_on(&rt, &server, false).await
            }))
        };

        // Block until the drain task has genuinely parked at the seam — which
        // sits AFTER the candidate-page query and the claim, immediately
        // before the fresh pre-finalize read — THEN write, THEN release it.
        // That placement is what makes the write land strictly between the
        // page-query snapshot and the fresh read: parked any earlier, the
        // write would already be inside the page snapshot and the test would
        // pass whether finalization used the stale page or the fresh read.
        gate.reached.wait().await;

        let mut writer = rt.sql().writer().await.expect("writer");
        let rows = writer
            .execute(SqlStatement {
                sql: "UPDATE notes SET properties = json_set(properties, \
                      '$.custom', 'added-concurrently') WHERE id = ?1"
                    .to_string(),
                params: vec![SqlValue::Text(id.to_string())],
                label: Some("test_concurrent_property_add".into()),
            })
            .await
            .expect("concurrent write");
        assert_eq!(rows, 1);
        drop(writer);

        gate.release.wait().await;
        let summary = drain_task
            .await
            .expect("drain task")
            .expect("drain must not error");
        assert_eq!(summary.fired, 1, "the event must have fired: {summary:?}");

        let stored = get_note_props(&rt, id).await;
        assert_eq!(
            stored["custom"].as_str(),
            Some("added-concurrently"),
            "a property written between claim and the finalizer's current-properties read \
             must survive finalization, got {stored:?}"
        );
        assert_eq!(stored["status"].as_str(), Some("fired"));
    }

    /// Guarding the finalizer's write on the freshly-read properties protects
    /// the properties BLOB while still allowing a stale SCHEDULING decision to
    /// be computed over it. `repeat` and `trigger_at` are parsed from the
    /// pre-claim candidate page; if the finalizer keeps using those, a writer
    /// who cancels the repeat in the claim window has their edit retained as
    /// the CAS base and then immediately contradicted by a next occurrence
    /// scheduled from the value they replaced.
    ///
    /// This fixture makes the two behaviours produce different terminal states
    /// rather than different timestamps, so the assertion cannot pass by
    /// rounding: the event is seeded `repeat: "daily"`, and the concurrent
    /// write clears `repeat` while the drain is parked at the seam. Scheduling
    /// from the fresh read yields a terminal `fired`; scheduling from the stale
    /// page yields a rescheduled `pending` with an advanced `trigger_at`.
    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn production_drain_schedules_from_the_fresh_read_not_the_page_snapshot() {
        let (_tmp, db_path) = tmp_db();
        let rt = make_rt(&db_path).await;
        let id = create_scheduled_event(
            &rt,
            "local",
            &due_rfc3339(),
            Some("stats()"),
            Some("daily"),
            "schedule",
        )
        .await;

        let gate = race_seam::PauseGate {
            at: race_seam::PausePoint::BeforeFinalizeRead,
            reached: std::sync::Arc::new(tokio::sync::Barrier::new(2)),
            release: std::sync::Arc::new(tokio::sync::Barrier::new(2)),
        };

        let drain_task = {
            let rt = rt.clone();
            let gate = gate.clone();
            tokio::spawn(race_seam::PAUSE_GATE.scope(gate, async move {
                let server = KhiveMcpServer::new(rt.clone()).map_err(|e| anyhow::anyhow!("{e}"))?;
                run_pending_events_on(&rt, &server, false).await
            }))
        };

        gate.reached.wait().await;

        let mut writer = rt.sql().writer().await.expect("writer");
        let rows = writer
            .execute(SqlStatement {
                sql:
                    "UPDATE notes SET properties = json_set(properties, '$.repeat', json('null')) \
                      WHERE id = ?1"
                        .to_string(),
                params: vec![SqlValue::Text(id.to_string())],
                label: Some("test_concurrent_repeat_clear".into()),
            })
            .await
            .expect("concurrent write");
        assert_eq!(rows, 1);
        drop(writer);

        gate.release.wait().await;
        let summary = drain_task
            .await
            .expect("drain task")
            .expect("drain must not error");
        assert_eq!(summary.fired, 1, "the event must have fired: {summary:?}");

        let stored = get_note_props(&rt, id).await;
        assert!(
            stored.get("repeat").is_none() || stored["repeat"].is_null(),
            "the concurrent clear of `repeat` must survive finalization, got {stored:?}"
        );
        assert_eq!(
            stored["status"].as_str(),
            Some("fired"),
            "finalization must schedule from the repeat it read fresh (cleared, so terminal), \
             not from the pre-claim page snapshot (\"daily\", which would reschedule to \
             pending): got {stored:?}"
        );
    }

    /// The claim's `trigger_at` fence, isolated. The receipt's `occurrence_id`
    /// is derived from the caller's page snapshot, so a claim that lands on a
    /// row whose `trigger_at` has since moved would persist an occurrence id
    /// describing an instant the row is no longer scheduled for. Receipt
    /// validation rejects exactly that pairing, so such a row can only ever be
    /// quarantined as indeterminate; refusing the claim is what keeps it out of
    /// the durable record in the first place.
    #[tokio::test]
    async fn claim_refuses_when_a_concurrent_writer_rescheduled_since_the_page_read() {
        let (_tmp, db_path) = tmp_db();
        let rt = make_rt(&db_path).await;
        let snapshot_trigger = "2000-01-01T00:00:00Z";
        let id = create_scheduled_event(
            &rt,
            "local",
            snapshot_trigger,
            Some("stats()"),
            None,
            "schedule",
        )
        .await;

        // Positive control in the same test: the fence admits the claim when
        // the row still carries the snapshot's bytes. Without this arm a
        // refusal below would be consistent with a fence that refuses
        // everything, which proves nothing about the race.
        let admitted = claim_pending_event(
            &rt,
            "local",
            id,
            dispatch_occurrence_id(id, snapshot_trigger.parse::<DateTime<Utc>>().unwrap()),
            snapshot_trigger,
            "actor:test",
            short_test_lease(),
        )
        .await
        .expect("claim query must not error");
        assert!(
            admitted.is_some(),
            "the claim must be admitted when the row still holds the snapshot's trigger_at"
        );

        // Put the row back to pending so the refusal arm is testing the
        // trigger_at predicate and not the status one.
        let rescheduled_trigger = "2000-06-01T00:00:00Z";
        force_set_properties(
            &rt,
            id,
            &json!({
                "trigger_at": rescheduled_trigger,
                "status": "pending",
                "action": "stats()",
                "event_type": "schedule",
            }),
        )
        .await;

        let refused = claim_pending_event(
            &rt,
            "local",
            id,
            dispatch_occurrence_id(id, snapshot_trigger.parse::<DateTime<Utc>>().unwrap()),
            snapshot_trigger,
            "actor:test",
            short_test_lease(),
        )
        .await
        .expect("claim query must not error");
        assert!(
            refused.is_none(),
            "the claim must refuse once the row's trigger_at has moved away from the snapshot"
        );

        let stored = get_note_props(&rt, id).await;
        assert_eq!(
            stored["status"].as_str(),
            Some("pending"),
            "a refused claim must leave the row claimable by the next drain: {stored:?}"
        );
        assert_eq!(
            stored["trigger_at"].as_str(),
            Some(rescheduled_trigger),
            "a refused claim must leave the writer's reschedule intact: {stored:?}"
        );
        assert!(
            stored.get("dispatch_receipt").is_none() || stored["dispatch_receipt"].is_null(),
            "a refused claim must persist no receipt: {stored:?}"
        );
    }

    /// The same refusal, driven through the production drain rather than the
    /// claim primitive, so it would fail if the drain's call site stopped
    /// passing the page snapshot's `trigger_at` down. Parks at
    /// `PausePoint::BeforeClaim` so the concurrent reschedule lands strictly
    /// between the candidate-page query and the claim: that is the window in
    /// which the occurrence id is already derived but not yet persisted.
    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn production_drain_refuses_to_claim_an_event_rescheduled_in_the_claim_window() {
        let (_tmp, db_path) = tmp_db();
        let rt = make_rt(&db_path).await;
        let id = create_scheduled_event(
            &rt,
            "local",
            &due_rfc3339(),
            Some("stats()"),
            None,
            "schedule",
        )
        .await;

        let gate = race_seam::PauseGate {
            at: race_seam::PausePoint::BeforeClaim,
            reached: std::sync::Arc::new(tokio::sync::Barrier::new(2)),
            release: std::sync::Arc::new(tokio::sync::Barrier::new(2)),
        };

        let drain_task = {
            let rt = rt.clone();
            let gate = gate.clone();
            tokio::spawn(race_seam::PAUSE_GATE.scope(gate, async move {
                let server = KhiveMcpServer::new(rt.clone()).map_err(|e| anyhow::anyhow!("{e}"))?;
                run_pending_events_on(&rt, &server, false).await
            }))
        };

        gate.reached.wait().await;

        // Still due, so the row stays a drain candidate and the refusal cannot
        // be confused with the event simply not being ready.
        let rescheduled_trigger = "2001-01-01T00:00:00Z";
        let mut writer = rt.sql().writer().await.expect("writer");
        let rows = writer
            .execute(SqlStatement {
                sql: "UPDATE notes SET properties = json_set(properties, '$.trigger_at', ?2) \
                      WHERE id = ?1"
                    .to_string(),
                params: vec![
                    SqlValue::Text(id.to_string()),
                    SqlValue::Text(rescheduled_trigger.to_string()),
                ],
                label: Some("test_concurrent_reschedule".into()),
            })
            .await
            .expect("concurrent write");
        assert_eq!(rows, 1);
        drop(writer);

        gate.release.wait().await;
        let summary = drain_task
            .await
            .expect("drain task")
            .expect("drain must not error");
        assert_eq!(
            summary.fired, 0,
            "an event rescheduled inside the claim window must not fire on this pass: {summary:?}"
        );
        assert_eq!(
            summary.skipped_race, 1,
            "the pass must record the refusal as a lost race, not as a failure or a silent \
             no-candidate pass: {summary:?}"
        );
        assert_eq!(
            summary.failed, 0,
            "a refused claim is not an error: {summary:?}"
        );

        let stored = get_note_props(&rt, id).await;
        assert_eq!(
            stored["status"].as_str(),
            Some("pending"),
            "the refused row must stay pending for the next drain: {stored:?}"
        );
        assert_eq!(
            stored["trigger_at"].as_str(),
            Some(rescheduled_trigger),
            "the writer's reschedule must survive: {stored:?}"
        );
        assert!(
            stored.get("dispatch_receipt").is_none() || stored["dispatch_receipt"].is_null(),
            "no receipt may be persisted for a claim that never succeeded: {stored:?}"
        );
    }

    /// The post-claim half of the same invariant, and the one that cannot be
    /// left to recovery.
    ///
    /// A reschedule landing after the claim but before the finalizer's fresh
    /// read is INSIDE that read, so every finalization CAS predicate passes:
    /// status, `firing_at`, invocation id, lease, and the exact-properties
    /// fence all match. Committing there would write a terminal `fired` row
    /// whose `dispatch_receipt.occurrence_id` names the old instant while
    /// `trigger_at` names the new one — and the receipt validator that would
    /// catch that pairing is only ever reached through the recovery scan, which
    /// fences on `status = 'firing'`. A terminal row is past it forever, so the
    /// mismatch would never be adjudicated at all.
    ///
    /// So the drain must refuse instead, leaving the row `firing` for recovery.
    /// This differs from
    /// `production_drain_refuses_to_claim_an_event_rescheduled_in_the_claim_window`
    /// only in WHICH seam the write lands at, which is what makes the two
    /// windows separately load-bearing.
    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn production_drain_refuses_to_finalize_an_event_rescheduled_after_the_claim() {
        let (_tmp, db_path) = tmp_db();
        let rt = make_rt(&db_path).await;
        let id = create_scheduled_event(
            &rt,
            "local",
            &due_rfc3339(),
            Some("stats()"),
            None,
            "schedule",
        )
        .await;

        let gate = race_seam::PauseGate {
            at: race_seam::PausePoint::BeforeFinalizeRead,
            reached: std::sync::Arc::new(tokio::sync::Barrier::new(2)),
            release: std::sync::Arc::new(tokio::sync::Barrier::new(2)),
        };

        let drain_task = {
            let rt = rt.clone();
            let gate = gate.clone();
            tokio::spawn(race_seam::PAUSE_GATE.scope(gate, async move {
                let server = KhiveMcpServer::new(rt.clone()).map_err(|e| anyhow::anyhow!("{e}"))?;
                run_pending_events_on(&rt, &server, false).await
            }))
        };

        // Parked AFTER claim and dispatch, so the claim's trigger_at fence has
        // already passed and this write cannot be caught by it. Still a valid
        // parseable instant, so the refusal cannot be confused with the
        // unparseable-trigger branch.
        gate.reached.wait().await;
        let rescheduled_trigger = "2002-01-01T00:00:00Z";
        let mut writer = rt.sql().writer().await.expect("writer");
        let rows = writer
            .execute(SqlStatement {
                sql: "UPDATE notes SET properties = json_set(properties, '$.trigger_at', ?2) \
                      WHERE id = ?1"
                    .to_string(),
                params: vec![
                    SqlValue::Text(id.to_string()),
                    SqlValue::Text(rescheduled_trigger.to_string()),
                ],
                label: Some("test_reschedule_after_claim".into()),
            })
            .await
            .expect("concurrent write");
        assert_eq!(rows, 1);
        drop(writer);

        gate.release.wait().await;
        let summary = drain_task
            .await
            .expect("drain task")
            .expect("drain must not error");
        assert_eq!(
            summary.fired, 0,
            "no terminal row may be written for an occurrence the row no longer names: {summary:?}"
        );
        assert_eq!(
            summary.failed, 1,
            "the refusal must be recorded as a failed finalization, which is what leaves the row \
             for recovery: {summary:?}"
        );

        let stored = get_note_props(&rt, id).await;
        assert_eq!(
            stored["status"].as_str(),
            Some("firing"),
            "the row must stay firing so the recovery scan, which fences on status='firing', can \
             still reach it; a terminal row would be past that scan forever: {stored:?}"
        );
        assert_eq!(
            stored["trigger_at"].as_str(),
            Some(rescheduled_trigger),
            "the writer's reschedule must survive: {stored:?}"
        );
        assert!(
            stored.get("dispatch_receipt").is_some(),
            "the claim receipt stays on the row for the validator to adjudicate: {stored:?}"
        );
    }

    /// `schedule.cancel` on a row that is currently `status="firing"` — even
    /// a *stale* one — must still fail cleanly: reclaim only happens as part
    /// of a drain pass, so cancel itself never reclaims.
    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn cancel_on_stale_firing_row_still_fails_cleanly() {
        let (_tmp, db_path) = tmp_db();
        let rt = make_rt(&db_path).await;
        let server = KhiveMcpServer::new(rt.clone()).expect("server");

        let past = "2000-01-01T00:00:00Z";
        let id =
            create_scheduled_event(&rt, "local", past, Some("stats()"), None, "schedule").await;

        let stale_firing_at =
            Utc::now().timestamp_micros() - (LEGACY_STALE_FIRING_TIMEOUT_MICROS * 2);
        force_set_properties(
            &rt,
            id,
            &json!({
                "trigger_at": past,
                "repeat": null,
                "status": "firing",
                "event_type": "schedule",
                "payload": "stats()",
                "fired_at": null,
                "cancelled_at": null,
                "firing_at": stale_firing_at,
            }),
        )
        .await;

        let cancel_ops = serde_json::to_string(&serde_json::json!([
            { "tool": "schedule.cancel", "args": { "id": id.to_string() } }
        ]))
        .expect("serialize cancel op");
        let cancel_result = server
            .dispatch_request_local(RequestParams {
                ops: cancel_ops,
                presentation: None,
                presentation_per_op: None,
                save_to: None,
                format: None,
                format_per_op: None,
                request_id: None,
            })
            .await
            .expect("dispatch_request_local must not error at the RPC layer");
        let cancel_json: Value = serde_json::from_str(&cancel_result).expect("valid JSON");
        let op_result = &cancel_json["results"][0];
        assert_eq!(
            op_result["ok"], false,
            "cancel of a stale-but-still-firing event must fail, not silently succeed \
             (reclaim happens on drain, not cancel): {cancel_json}"
        );
        let cancel_err = op_result["error"].as_str().unwrap_or("");
        assert!(
            cancel_err.contains("not pending"),
            "cancel must report the event is no longer pending; got: {cancel_err}"
        );

        // The row is still "firing" (untouched by the failed cancel attempt).
        let props = get_note_props(&rt, id).await;
        assert_eq!(
            props["status"].as_str().unwrap_or(""),
            "firing",
            "a failed cancel must not alter the row's status"
        );
    }

    // Unit tests for next_trigger_at

    #[test]
    fn next_trigger_at_daily() {
        let base: DateTime<Utc> = "2026-06-01T09:00:00Z".parse().unwrap();
        let next = next_trigger_at(&Some("daily".to_string()), base).unwrap();
        assert_eq!(next, base + Duration::days(1));
    }

    #[test]
    fn next_trigger_at_weekly() {
        let base: DateTime<Utc> = "2026-06-01T09:00:00Z".parse().unwrap();
        let next = next_trigger_at(&Some("weekly".to_string()), base).unwrap();
        assert_eq!(next, base + Duration::weeks(1));
    }

    #[test]
    fn next_trigger_at_monthly() {
        let base: DateTime<Utc> = "2026-06-01T09:00:00Z".parse().unwrap();
        let next = next_trigger_at(&Some("monthly".to_string()), base).unwrap();
        // June 1 + 1 month = July 1
        let expected: DateTime<Utc> = "2026-07-01T09:00:00Z".parse().unwrap();
        assert_eq!(next, expected);
    }

    #[test]
    fn next_trigger_at_none_repeat_returns_none() {
        let base: DateTime<Utc> = "2026-06-01T09:00:00Z".parse().unwrap();
        assert!(next_trigger_at(&None, base).is_none());
    }

    #[test]
    fn next_trigger_at_cron_returns_none() {
        let base: DateTime<Utc> = "2026-06-01T09:00:00Z".parse().unwrap();
        // Write-time validation rejects cron; legacy rows fail closed before dispatch.
        assert!(next_trigger_at(&Some("0 9 * * 1".to_string()), base).is_none());
    }

    // ── ADR-106 missed-event policy ─────────────────────────────────────────

    /// Deterministic unit test for `advance_repeat_past_missed`: 14 daily
    /// occurrences accumulated while an event was undrained must be skipped
    /// in a single advance to the first occurrence strictly after `now` —
    /// never a multi-fire catch-up burst.
    #[test]
    fn advance_repeat_past_missed_skips_all_accumulated_occurrences() {
        let now: DateTime<Utc> = "2026-06-15T09:00:00Z".parse().unwrap();
        let original: DateTime<Utc> = "2026-06-01T09:00:00Z".parse().unwrap();
        let next = advance_repeat_past_missed(&Some("daily".to_string()), original, now).unwrap();
        assert!(next > now, "advanced occurrence must be strictly future");
        assert!(
            next <= now + Duration::days(1),
            "must land on the very next occurrence, not skip further than one interval past now"
        );
        assert_eq!(
            next,
            original + Duration::days(15),
            "must be exactly the first daily occurrence after now (single advance, no burst)"
        );
    }

    /// No `repeat` never advances, so the caller marks a stale one-shot missed.
    #[test]
    fn advance_repeat_past_missed_no_repeat_returns_none() {
        let now: DateTime<Utc> = "2026-06-15T09:00:00Z".parse().unwrap();
        let original: DateTime<Utc> = "2026-06-01T09:00:00Z".parse().unwrap();
        assert!(advance_repeat_past_missed(&None, original, now).is_none());
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn missed_reminder_receipt_retains_creator_not_daemon_actor() {
        let (_tmp, db_path) = tmp_db();
        let creator_rt = make_rt_with_actor(&db_path, Some("lambda:reminder-owner")).await;
        let id = create_scheduled_event(
            &creator_rt,
            "local",
            "2000-01-01T00:00:00Z",
            None,
            None,
            "remind",
        )
        .await;

        let daemon_rt = make_rt_with_actor(&db_path, Some("lambda:scheduler-daemon")).await;
        let server = KhiveMcpServer::new(daemon_rt.clone()).expect("daemon server");
        let summary = run_pending_events_on(&daemon_rt, &server, false)
            .await
            .expect("missed reminder drain");
        assert_eq!(summary.invoked, 0);
        assert_eq!(summary.missed, vec![id]);

        let props = get_note_props(&daemon_rt, id).await;
        assert_eq!(props["status"], "missed", "{props}");
        assert_eq!(
            props["dispatch_receipt"]["actor"], "actor:lambda:reminder-owner",
            "a grace-policy receipt is still creator-attributed: {props}"
        );
        assert!(
            inbound_reminder_messages(&daemon_rt, "lambda:reminder-owner")
                .await
                .is_empty(),
            "a missed reminder must not dispatch"
        );
        assert!(
            inbound_reminder_messages(&daemon_rt, "lambda:scheduler-daemon")
                .await
                .is_empty(),
            "the daemon actor must neither receive nor own the missed reminder"
        );
    }

    /// 9 non-repeating events overdue well beyond the default grace window
    /// (the first-boot-against-a-large-backlog scenario) must ALL be marked
    /// `"missed"` and NONE dispatched — asserted by the absence of the
    /// side-effecting action's write, not just zeroed summary counters.
    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn nine_overdue_events_beyond_grace_are_missed_with_zero_dispatch() {
        let (_tmp, db_path) = tmp_db();
        let rt = make_rt(&db_path).await;

        let past = "2000-01-01T00:00:00Z";
        let marker = "nine-overdue-zero-dispatch-marker";
        let action_dsl = format!("create(kind=\"observation\", content=\"{marker}\")");
        let mut ids = Vec::new();
        for _ in 0..9 {
            let id = create_scheduled_event(
                &rt,
                "local",
                past,
                Some(action_dsl.as_str()),
                None,
                "schedule",
            )
            .await;
            ids.push(id);
        }

        let summary = drain_for_test(&db_path).await.expect("drain");

        assert_eq!(summary.scanned, 9, "all 9 overdue rows must be scanned");
        assert_eq!(summary.fired, 0, "zero dispatches: nothing may be fired");
        assert_eq!(
            summary.advanced, 0,
            "zero dispatches: nothing may be advanced"
        );
        assert_eq!(summary.failed, 0, "the missed path is not a failure");
        assert_eq!(
            summary.missed.len(),
            9,
            "all 9 overdue rows must be marked missed, got summary={summary:?}"
        );
        for id in &ids {
            assert!(
                summary.missed.contains(id),
                "missed list must name every overdue id"
            );
        }

        for id in ids {
            let props = get_note_props(&rt, id).await;
            assert_eq!(
                props["status"].as_str(),
                Some("missed"),
                "note {id} must end in status=missed, got {props:?}"
            );
            assert!(
                props["missed_at"].as_i64().is_some(),
                "note {id} must have missed_at stamped, got {props:?}"
            );
            assert!(
                props["fired_at"].is_null(),
                "note {id} must never have fired_at set (never dispatched), got {props:?}"
            );
            assert_eq!(
                props["dispatch_receipt"]["state"],
                DispatchReceiptState::Missed.as_str(),
                "the durable claim receipt must survive missed finalization: {props}"
            );
            assert!(props["dispatch_receipt"]["completed_at"].as_i64().is_some());
            assert!(props["dispatch_receipt"]["error"].is_null());
        }

        // Strongest evidence: the side-effecting action's own output record
        // must be entirely absent — not merely "summary says zero fired".
        let ns = Namespace::parse("local").unwrap();
        let token = rt.authorize(ns).expect("authorize");
        let store = rt.notes(&token).expect("notes");
        let page = store
            .query_notes(
                "local",
                Some("observation"),
                PageRequest {
                    limit: 50,
                    offset: 0,
                },
            )
            .await
            .expect("query observation notes");
        let marker_hits: Vec<_> = page.items.iter().filter(|n| n.content == marker).collect();
        assert!(
            marker_hits.is_empty(),
            "the missed action must never dispatch: found {} marker note(s): {marker_hits:?}",
            marker_hits.len()
        );
    }

    /// An event overdue by less than the grace window must still fire
    /// normally — the missed policy only applies beyond the grace threshold.
    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn overdue_within_grace_still_fires() {
        let (_tmp, db_path) = tmp_db();
        let rt = make_rt(&db_path).await;

        // 60s overdue is comfortably inside the 300s default grace window.
        let trigger_at = (Utc::now() - Duration::seconds(60)).to_rfc3339();
        let id =
            create_scheduled_event(&rt, "local", &trigger_at, Some("stats()"), None, "schedule")
                .await;

        let summary = drain_for_test(&db_path).await.expect("drain");

        assert!(
            summary.missed.is_empty(),
            "an event within grace must never be marked missed, got summary={summary:?}"
        );
        assert!(
            summary.fired >= 1 || summary.advanced >= 1,
            "an event within grace must be dispatched normally, got summary={summary:?}"
        );

        let props = get_note_props(&rt, id).await;
        assert_eq!(
            props["status"].as_str(),
            Some("fired"),
            "non-repeating in-grace event must end fired, got {props:?}"
        );
        assert!(
            props["fired_at"].as_str().is_some(),
            "in-grace event must have fired_at set, got {props:?}"
        );
    }

    /// End-to-end (drain-level) confirmation that a missed *repeating* event
    /// is re-armed at a future occurrence instead of ending terminally
    /// missed — complements the deterministic
    /// `advance_repeat_past_missed_skips_all_accumulated_occurrences` unit
    /// test above with the full claim/finalize wiring.
    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn missed_repeat_is_rearmed_at_next_future_occurrence() {
        let (_tmp, db_path) = tmp_db();
        let rt = make_rt(&db_path).await;

        // 10 days overdue with a daily repeat: 10 accumulated occurrences,
        // all missed, must collapse into exactly one future re-arm.
        let original_trigger: DateTime<Utc> = Utc::now() - Duration::days(10);
        let id = create_scheduled_event(
            &rt,
            "local",
            &original_trigger.to_rfc3339(),
            Some("stats()"),
            Some("daily"),
            "schedule",
        )
        .await;

        let summary = drain_for_test(&db_path).await.expect("drain");

        assert_eq!(summary.fired, 0, "a missed repeat must not fire");
        assert_eq!(
            summary.advanced, 0,
            "a missed repeat's re-arm is counted as missed, not advanced"
        );
        assert_eq!(
            summary.missed.len(),
            1,
            "exactly one missed occurrence recorded"
        );
        assert!(summary.missed.contains(&id));

        let props = get_note_props(&rt, id).await;
        assert_eq!(
            props["status"].as_str(),
            Some("pending"),
            "a missed repeat must be re-armed to pending, not left terminal, got {props:?}"
        );
        assert!(
            props["missed_at"].as_i64().is_some(),
            "missed_at must be stamped even though the row is re-armed, got {props:?}"
        );
        let new_trigger: DateTime<Utc> = props["trigger_at"]
            .as_str()
            .expect("trigger_at must be set")
            .parse()
            .expect("parseable trigger_at");
        let now = Utc::now();
        assert!(
            new_trigger > now,
            "re-armed trigger_at must be strictly in the future, got {new_trigger} (now={now})"
        );
        assert!(
            new_trigger <= now + Duration::days(1),
            "re-armed trigger_at must be the very next occurrence, not skip further \
             (no catch-up burst), got {new_trigger} (now={now})"
        );
    }

    /// Issue #792 (missed-path variant): a missed repeat's re-arm must also
    /// preserve the original `trigger_at` offset, not just the normal
    /// fire-and-advance path — both call through the same
    /// `next_trigger_at`-derived arithmetic and must both render at the
    /// caller's original offset.
    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn missed_repeat_rearm_preserves_original_offset() {
        let (_tmp, db_path) = tmp_db();
        let rt = make_rt(&db_path).await;

        // 10 days overdue with a daily repeat, formatted at a non-UTC
        // +04:00 wall-clock offset.
        let plus_four = FixedOffset::east_opt(4 * 3600).expect("valid offset");
        let original_trigger = (Utc::now() - Duration::days(10)).with_timezone(&plus_four);
        let id = create_scheduled_event(
            &rt,
            "local",
            &original_trigger.to_rfc3339(),
            Some("stats()"),
            Some("daily"),
            "schedule",
        )
        .await;

        let summary = drain_for_test(&db_path).await.expect("drain");
        assert_eq!(
            summary.missed.len(),
            1,
            "exactly one missed occurrence recorded"
        );

        let props = get_note_props(&rt, id).await;
        let new_trigger = props["trigger_at"]
            .as_str()
            .expect("trigger_at must be set");
        assert!(
            new_trigger.ends_with("+04:00"),
            "re-armed trigger_at must preserve the original +04:00 offset, got {new_trigger:?}"
        );
        let new_dt = DateTime::parse_from_rfc3339(new_trigger).expect("parseable re-armed ts");
        assert_eq!(
            *new_dt.offset(),
            plus_four,
            "re-armed trigger_at offset must equal the original +04:00 offset"
        );
        assert_eq!(
            new_dt.time(),
            original_trigger.time(),
            "re-armed occurrence must retain the same local wall-clock time"
        );
    }

    /// A backlog larger than the drain's internal page size (200) must be
    /// fully processed in ONE drain pass, not silently truncated at the page
    /// boundary — 201 rows exercises the exact boundary.
    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn backlog_larger_than_page_size_is_fully_drained_in_one_pass() {
        let (_tmp, db_path) = tmp_db();
        let rt = make_rt(&db_path).await;

        const OVERDUE_ROW_COUNT: usize = 201; // PAGE_SIZE (200) + 1
        let past = "2000-01-01T00:00:00Z"; // far beyond the missed-event grace window
        let mut ids = Vec::with_capacity(OVERDUE_ROW_COUNT);
        for _ in 0..OVERDUE_ROW_COUNT {
            let id =
                create_scheduled_event(&rt, "local", past, Some("stats()"), None, "schedule").await;
            ids.push(id);
        }

        let summary = drain_for_test(&db_path).await.expect("drain");

        assert_eq!(
            summary.scanned, OVERDUE_ROW_COUNT as u64,
            "every overdue row across both pages must be scanned in one pass, got \
             summary={summary:?}"
        );
        assert_eq!(
            summary.missed.len(),
            OVERDUE_ROW_COUNT,
            "every overdue row across both pages must be marked missed in one pass \
             (the page-boundary row must not be skipped), got summary={summary:?}"
        );
        for id in &ids {
            assert!(
                summary.missed.contains(id),
                "missed list must name every row, including ones beyond the first page"
            );
        }
        for id in ids {
            let props = get_note_props(&rt, id).await;
            assert_eq!(
                props["status"].as_str(),
                Some("missed"),
                "note {id} must end in status=missed (not left pending past the page \
                 boundary), got {props:?}"
            );
        }
    }

    /// Two concurrent drain passes over the same store must never double-fire
    /// a row: the `pending -> firing` CAS claim makes exactly one of the two
    /// concurrent callers win each row. Each action is a genuinely
    /// side-effecting write (not a read-only op) so the test can assert
    /// exactly ONE marker note per event exists, rather than trusting summary
    /// counters alone to catch a double-dispatch-one-finalize regression.
    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn concurrent_drains_fire_each_row_exactly_once() {
        let (_tmp, db_path) = tmp_db();
        let rt = make_rt(&db_path).await;

        const ROW_COUNT: usize = 20;
        let past = due_rfc3339(); // in-grace: exercises the normal fire path, not missed
        let mut ids = Vec::with_capacity(ROW_COUNT);
        let mut markers = Vec::with_capacity(ROW_COUNT);
        for i in 0..ROW_COUNT {
            let marker = format!("concurrent-drain-marker-{i}");
            let action_dsl = format!("create(kind=\"observation\", content=\"{marker}\")");
            let id = create_scheduled_event(
                &rt,
                "local",
                &past,
                Some(action_dsl.as_str()),
                None,
                "schedule",
            )
            .await;
            ids.push(id);
            markers.push(marker);
        }

        let db_path_a = db_path.clone();
        let db_path_b = db_path.clone();
        let (summary_a, summary_b) = tokio::join!(
            async move { drain_for_test(&db_path_a).await },
            async move { drain_for_test(&db_path_b).await },
        );
        let summary_a = summary_a.expect("drain A");
        let summary_b = summary_b.expect("drain B");

        let total_dispatched =
            summary_a.fired + summary_a.advanced + summary_b.fired + summary_b.advanced;
        assert_eq!(
            total_dispatched, ROW_COUNT as u64,
            "every row must be dispatched exactly once across both concurrent drains, \
             got a={summary_a:?} b={summary_b:?}"
        );
        assert_eq!(
            summary_a.failed + summary_b.failed,
            0,
            "the CAS claim must make the losing drain skip cleanly (skipped_race), \
             never fail: a={summary_a:?} b={summary_b:?}"
        );

        for id in &ids {
            let props = get_note_props(&rt, *id).await;
            assert_eq!(
                props["status"].as_str(),
                Some("fired"),
                "note {id} must end fired exactly once, got {props:?}"
            );
        }

        // Strongest evidence: exactly one marker note per row. A
        // double-dispatch-one-finalize bug would leave the CAS-tracked
        // `status`/summary counters looking clean while still writing the
        // action's side effect twice for the row that raced — this is the
        // only assertion that would catch it.
        let ns = Namespace::parse("local").unwrap();
        let token = rt.authorize(ns).expect("authorize");
        let store = rt.notes(&token).expect("notes");
        let page = store
            .query_notes(
                "local",
                Some("observation"),
                PageRequest {
                    limit: (ROW_COUNT as u32) + 10,
                    offset: 0,
                },
            )
            .await
            .expect("query observation notes");
        for marker in &markers {
            let hits: Vec<_> = page.items.iter().filter(|n| &n.content == marker).collect();
            assert_eq!(
                hits.len(),
                1,
                "marker {marker:?} must appear exactly once (double-dispatch check), \
                 found {}: {hits:?}",
                hits.len()
            );
        }
    }

    // `run_pending_events`'s wrapper seam must not misread a default
    // namespace as an explicit actor override. These tests exercise the real
    // config-discovery path (process cwd / `HOME`); the helpers below mirror
    // `serve.rs`'s own equivalents, kept local since they are test-only.

    /// RAII guard: redirects process cwd and `HOME` to isolated locations so
    /// the real machine's global `~/.khive/config.toml` never leaks into a
    /// test. Restores both on drop, even on panic/unwind.
    struct SeatEnv {
        original_cwd: std::path::PathBuf,
        original_home: Option<std::ffi::OsString>,
        _isolated_home: tempfile::TempDir,
    }

    impl SeatEnv {
        fn enter(project_root: &std::path::Path) -> Self {
            let original_cwd = std::env::current_dir().expect("read cwd");
            let original_home = std::env::var_os("HOME");
            let isolated_home = tempfile::tempdir().expect("isolated HOME tempdir");
            std::env::set_current_dir(project_root).expect("chdir into seat project root");
            std::env::set_var("HOME", isolated_home.path());
            Self {
                original_cwd,
                original_home,
                _isolated_home: isolated_home,
            }
        }
    }

    impl Drop for SeatEnv {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.original_cwd);
            match &self.original_home {
                Some(h) => std::env::set_var("HOME", h),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    /// Write a project-local `.khive/config.toml` declaring `[actor] id`.
    fn write_project_actor_config(project_root: &std::path::Path, actor_id: &str) {
        std::fs::create_dir_all(project_root.join(".khive")).expect("mkdir .khive");
        std::fs::write(
            project_root.join(".khive/config.toml"),
            format!("[actor]\nid = \"{actor_id}\"\n"),
        )
        .expect("write project actor config");
    }

    /// Regression: a `DatabaseOverrideConflict` raised by the builder must
    /// leave `run_pending_events_with_config` as the top-level error so
    /// `kkernel exec`'s refusal-envelope downcast recognizes it.
    #[tokio::test]
    #[serial_test::serial]
    #[serial_test::serial(config_ledger)]
    async fn run_pending_events_keeps_db_override_conflict_top_level() {
        std::env::remove_var("KHIVE_DB");
        std::env::remove_var("KHIVE_PACKS");
        std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR");

        let seat_dir = tempfile::tempdir().expect("seat tempdir");
        let _seat_env = SeatEnv::enter(seat_dir.path());
        let config_dir = tempfile::tempdir().expect("config tempdir");
        let config_path = config_dir.path().join("backends.toml");
        std::fs::write(
            &config_path,
            "[[backends]]\nname = \"main\"\n\n[[backends]]\nname = \"sessions\"\n",
        )
        .expect("write multi-backend config");

        let error = run_pending_events_with_config(
            Some("/tmp/definitely-not-the-main.db"),
            Some(&config_path),
            "local",
            false,
        )
        .await
        .expect_err("a divergent concrete --db override must be refused");

        assert!(
            error
                .downcast_ref::<crate::serve::DatabaseOverrideConflict>()
                .is_some(),
            "the conflict must remain the top-level error for the refusal envelope: {error:?}"
        );
        assert!(
            crate::serve::db_override_refusal_envelope(&error).is_some(),
            "the documented JSON refusal envelope must be derivable: {error:?}"
        );
    }

    /// Sibling regression for the provenance half of the same seam: build
    /// failures that are NOT the typed conflict keep the generic
    /// "pending-events: build server" context (an invalid explicit config
    /// surfaces as `config error: ...` underneath).
    #[tokio::test]
    #[serial_test::serial]
    #[serial_test::serial(config_ledger)]
    async fn run_pending_events_wraps_non_conflict_build_errors_with_context() {
        std::env::remove_var("KHIVE_DB");
        std::env::remove_var("KHIVE_PACKS");
        std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR");

        let seat_dir = tempfile::tempdir().expect("seat tempdir");
        let _seat_env = SeatEnv::enter(seat_dir.path());
        let config_dir = tempfile::tempdir().expect("config tempdir");
        let config_path = config_dir.path().join("broken.toml");
        std::fs::write(&config_path, "this is not [valid toml\n").expect("write malformed config");

        let error = run_pending_events_with_config(None, Some(&config_path), "local", false)
            .await
            .expect_err("an invalid explicit config must fail the build");

        assert!(
            error
                .downcast_ref::<crate::serve::DatabaseOverrideConflict>()
                .is_none(),
            "not a database-override conflict: {error:?}"
        );
        let rendered = format!("{error:#}");
        assert!(
            rendered.contains("pending-events: build server"),
            "non-conflict build failures keep the generic provenance: {rendered}"
        );
        assert!(
            rendered.contains("config error"),
            "the underlying config failure must remain in the chain: {rendered}"
        );
    }

    /// An explicit `--config` naming a MISSING file must fail loud, not run
    /// the drain with defaults; the error surfaces wrapped in the generic
    /// build context, not as a `DatabaseOverrideConflict`.
    #[tokio::test]
    #[serial_test::serial]
    #[serial_test::serial(config_ledger)]
    async fn run_pending_events_fails_loud_for_missing_explicit_config() {
        std::env::remove_var("KHIVE_DB");
        std::env::remove_var("KHIVE_PACKS");
        std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR");

        let seat_dir = tempfile::tempdir().expect("seat tempdir");
        let _seat_env = SeatEnv::enter(seat_dir.path());
        let config_dir = tempfile::tempdir().expect("config tempdir");
        let missing_config = config_dir.path().join("does-not-exist.toml");

        let error = run_pending_events_with_config(None, Some(&missing_config), "local", false)
            .await
            .expect_err("a missing explicit config must fail loud, not run with defaults");

        assert!(
            error
                .downcast_ref::<crate::serve::DatabaseOverrideConflict>()
                .is_none(),
            "not a database-override conflict: {error:?}"
        );
        let rendered = format!("{error:#}");
        assert!(
            rendered.contains("pending-events: build server"),
            "non-conflict build failures keep the generic provenance: {rendered}"
        );
        assert!(
            rendered.contains("does not exist"),
            "the underlying missing-file failure must name the selected path: {rendered}"
        );
        assert!(
            rendered.contains("does-not-exist.toml"),
            "the error must name the missing file the operator selected: {rendered}"
        );
    }

    /// The wrapper seam (`build_server_with_explicit_namespace`, called by
    /// `run_pending_events` with `namespace_explicit: true, actor_explicit:
    /// false`) must let a `"local"`-resolved default namespace fall through
    /// to the project-configured actor — never clear it the way a genuine
    /// `--actor`/`--namespace` CLI override would (`build_server`'s own,
    /// correctly-narrower semantic). Regression for PR #782:
    /// before this fix, `run_pending_events` called
    /// `build_server` directly with a synthesized `namespace: Some("local")`,
    /// which `resolve_cli_namespace` reported as `explicit = true` and
    /// `build_server` then fed into BOTH `namespace_explicit` AND
    /// `actor_explicit`, tripping the "genuinely explicit actor tier
    /// requesting anonymous" branch in `resolve_runtime_config` and silently
    /// discarding the configured `[actor] id`.
    #[tokio::test]
    #[serial_test::serial]
    #[serial_test::serial(config_ledger)]
    async fn wrapper_seam_falls_through_to_project_actor_instead_of_clearing_it() {
        std::env::remove_var("KHIVE_ACTOR");
        std::env::remove_var("KHIVE_DB");
        std::env::remove_var("KHIVE_PACKS");
        std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR");

        let seat_dir = tempfile::tempdir().expect("seat tempdir");
        write_project_actor_config(seat_dir.path(), "lambda:pending-events-tenant");
        let _seat_env = SeatEnv::enter(seat_dir.path());

        let args = crate::args::Args {
            db: Some(":memory:".to_string()),
            actor: None,
            namespace: None,
            no_embed: false,
            pack: Vec::new(),
            config: None,
            daemon: false,
            transport: None,
            bind: None,
            brain_profile: None,
            resumed_generation: None,
        };
        let ns = Namespace::parse("local").expect("local namespace");

        // The seam `run_pending_events` actually calls: namespace is a real
        // default (`namespace_explicit: true`) but NOT an actor override
        // (`actor_explicit: false`).
        let (_server, schedule_rt) =
            crate::serve::build_server_with_explicit_namespace(&args, ns, true, false)
                .await
                .expect("build_server_with_explicit_namespace must succeed");
        let rt = schedule_rt.expect("\"schedule\" pack is in the default pack set");
        assert_eq!(
            rt.config().actor_id.as_deref(),
            Some("lambda:pending-events-tenant"),
            "a default namespace resolving to \"local\" must fall through to the \
             project-configured [actor] id, not clear it as if it were an explicit \
             --actor/--namespace override"
        );
    }

    /// Positive control for the failure mode the fix above closes: routing
    /// the same inputs through `build_server` (the genuine CLI-flag seam,
    /// unchanged by this fix) DOES clear the actor, because there a
    /// present namespace value really does mean "the operator typed
    /// --namespace". This documents why `run_pending_events` must not reuse
    /// that entry point for a synthesized, non-CLI-parsed namespace default.
    #[tokio::test]
    #[serial_test::serial]
    #[serial_test::serial(config_ledger)]
    async fn build_server_cli_seam_clears_actor_for_explicit_local_namespace() {
        std::env::remove_var("KHIVE_ACTOR");
        std::env::remove_var("KHIVE_DB");
        std::env::remove_var("KHIVE_PACKS");
        std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR");

        let seat_dir = tempfile::tempdir().expect("seat tempdir");
        write_project_actor_config(seat_dir.path(), "lambda:pending-events-tenant");
        let _seat_env = SeatEnv::enter(seat_dir.path());

        let args = crate::args::Args {
            db: Some(":memory:".to_string()),
            actor: None,
            namespace: Some("local".to_string()),
            no_embed: false,
            pack: Vec::new(),
            config: None,
            daemon: false,
            transport: None,
            bind: None,
            brain_profile: None,
            resumed_generation: None,
        };

        let (_server, schedule_rt) = crate::serve::build_server(&args)
            .await
            .expect("build_server must succeed");
        let rt = schedule_rt.expect("\"schedule\" pack is in the default pack set");
        assert_eq!(
            rt.config().actor_id,
            None,
            "build_server's genuine CLI-flag seam must still treat a present --namespace \
             value as an explicit actor override and clear the actor for \"local\" — this \
             is correct CLI behavior, unaffected by the wrapper-seam fix"
        );
    }

    /// `run_pending_events` (the actual `kkernel exec --pending-events`
    /// entry point, not the lower-level `drain_for_test` helper) must
    /// succeed under strict actor mode when a project `[actor] id` is
    /// configured — proving the wrapper's server construction no longer
    /// spuriously trips `enforce_strict_actor_mode` the way routing through
    /// `build_server`'s actor-clearing path would have.
    #[tokio::test]
    #[serial_test::serial]
    #[serial_test::serial(config_ledger)]
    async fn wrapper_succeeds_under_strict_actor_mode_with_configured_project_actor() {
        std::env::remove_var("KHIVE_ACTOR");
        std::env::remove_var("KHIVE_DB");
        std::env::remove_var("KHIVE_PACKS");
        let prev_strict = std::env::var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR").ok();
        std::env::set_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR", "1");

        let seat_dir = tempfile::tempdir().expect("seat tempdir");
        write_project_actor_config(seat_dir.path(), "lambda:pending-events-tenant");
        let _seat_env = SeatEnv::enter(seat_dir.path());

        let result = run_pending_events(Some(":memory:"), "local", false).await;

        match prev_strict {
            Some(v) => std::env::set_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR", v),
            None => std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR"),
        }

        result.expect(
            "run_pending_events must succeed under strict actor mode when a project \
             [actor] id is configured — the same config a live `kkernel mcp --daemon` \
             boot in this project would resolve",
        );
    }
}
