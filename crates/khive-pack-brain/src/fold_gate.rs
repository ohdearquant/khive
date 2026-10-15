//! ADR-081 §2 bounded-mass fold gate for implicit feedback.
//!
//! Invariant: the decayed implicit feedback mass folded into a posterior for a
//! given `(profile_id, namespace, target_id)` key never exceeds
//! `IMPLICIT_MASS_CAP` (the weight of one explicit event); an event that would
//! exceed the cap is recorded in the event log but folds at zero weight. The
//! whole check-and-fold, including scorer dedup, runs as one
//! `SqlAccess::atomic_unit` under `BEGIN IMMEDIATE` per ADR-081 §2's
//! cross-process single-writer requirement.
//!
//! See `crates/khive-pack-brain/docs/api/fold-gate.md` for the full mass
//! invariant, concurrency proof, why the decay math runs in Rust not SQL, and
//! the scorer-dedup atomicity argument.
use crate::sql::sql;

use khive_runtime::{EventAttribution, RuntimeError};
use khive_storage::event::Event;
use khive_storage::types::{SqlStatement, SqlValue};
use khive_storage::{SqlAccess, SqlWriter};

/// One explicit event's weight (ADR-081 §1) — the clamp's comparator (ADR-081 §2).
pub const IMPLICIT_MASS_CAP: f64 = 1.5;

/// Decay half-life for implicit mass, shared with the serve-ledger suppression
/// window (ADR-081 §2, §4): 7 days, expressed in microseconds.
pub const IMPLICIT_MASS_HALF_LIFE_US: f64 = 7.0 * 24.0 * 3600.0 * 1_000_000.0;

fn sql_err(context: &str, e: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::internal_with_context(format!("fold gate {context}"), e)
}

/// Decay `old_mass` forward by `delta_us` microseconds under the shared half-life.
///
/// `delta_us <= 0` (clock skew, or the very first event) is treated as zero
/// elapsed time — the mass is returned undecayed rather than amplified.
pub fn decayed_mass(old_mass: f64, delta_us: i64) -> f64 {
    if delta_us <= 0 || old_mass == 0.0 {
        return old_mass;
    }
    old_mass * 2f64.powf(-(delta_us as f64) / IMPLICIT_MASS_HALF_LIFE_US)
}

/// Decide whether an incoming implicit event of weight `w` folds at full weight.
///
/// Returns `(effective_weight, new_mass)`. `new_mass` is the mass to persist for
/// this key regardless of the gate outcome: on a pass, the event's own weight is
/// added; on a clamp, the decayed mass is left unchanged (the excess event
/// contributes nothing numerically, matching "recorded... and folded at zero
/// weight").
pub fn gate_decision(decayed: f64, w: f64, cap: f64) -> (f64, f64) {
    // Float-accumulation epsilon: repeated `+=` over many events can land a
    // legitimately-at-cap sum a few ULPs above `cap` (e.g. 15 * 0.1 accumulating
    // to 1.5000000000000002). The invariant is a rate-limit, not a bit-exact
    // boundary, so tolerate that drift rather than clamping one event early.
    const EPSILON: f64 = 1e-9;
    if decayed + w <= cap + EPSILON {
        (w, decayed + w)
    } else {
        (0.0, decayed)
    }
}

/// Result of applying the fold gate to one implicit event.
pub struct FoldGateOutcome {
    /// The weight to actually fold into posteriors (`w` or `0.0`).
    pub effective_weight: f64,
    /// The decayed mass observed for this key immediately before this event
    /// (for audit/observability — stamped onto the event payload).
    pub mass_before: f64,
    /// The mass persisted for this key after this event.
    pub mass_after: f64,
}

/// Outcome of `apply_fold_gate` when a `dedup_key` is supplied.
pub enum GateOutcome {
    /// The `(scorer_run_id, serve_ledger_id)` pair was already claimed by a
    /// prior call (ADR-081 §2/§6 idempotent replay). Neither
    /// `brain_implicit_mass` nor the event log were touched by this call —
    /// the caller must treat this emission as a no-op.
    Deduped,
    /// The dedup claim (if any) succeeded, or no `dedup_key` was supplied, and
    /// the mass check-and-fold ran.
    Folded(FoldGateOutcome),
}

/// Apply the ADR-081 §2 bounded-mass fold gate for one implicit feedback event.
///
/// `profile_id`/`namespace`/`target_id` form the accounting key. `weight` is the
/// nominal implicit weight (`FeedbackEventKind::update_weight()`, ADR-081 §1 —
/// currently `0.1`). `now_us` is the event's timestamp. `dedup_key`, when
/// supplied, is `(scorer_run_id, serve_ledger_id)` (ADR-081 §2/§6), claimed
/// atomically in the same transaction as the mass check-and-fold; `None` means
/// ordinary non-scorer feedback with no dedup. The whole claim+check+fold runs
/// inside one held `BEGIN IMMEDIATE` transaction (see module docs and
/// `crates/khive-pack-brain/docs/api/fold-gate.md`).
pub async fn apply_fold_gate(
    sql: &dyn SqlAccess,
    namespace: &str,
    profile_id: &str,
    target_id: &str,
    weight: f64,
    now_us: i64,
    dedup_key: Option<(&str, &str)>,
) -> Result<GateOutcome, RuntimeError> {
    // ADR-067 Component A: one `atomic_unit` closure, not a hand-rolled
    // BEGIN IMMEDIATE/COMMIT/ROLLBACK — see docs/api/fold-gate.md.
    let namespace = namespace.to_string();
    let profile_id = profile_id.to_string();
    let target_id = target_id.to_string();
    let dedup_key = dedup_key.map(|(a, b)| (a.to_string(), b.to_string()));

    let op: khive_storage::AtomicUnitOp = Box::new(move |writer| {
        Box::pin(async move {
            let dedup_ref = dedup_key.as_ref().map(|(a, b)| (a.as_str(), b.as_str()));
            apply_gate_within_tx(
                writer,
                &namespace,
                &profile_id,
                &target_id,
                weight,
                now_us,
                dedup_ref,
            )
            .await
            .map(|outcome| Box::new(outcome) as Box<dyn std::any::Any + Send>)
            .map_err(|e| {
                khive_storage::StorageError::driver(
                    khive_storage::StorageCapability::Sql,
                    "fold_gate_apply",
                    e,
                )
            })
        })
    });

    let boxed = sql.atomic_unit(op).await?;
    Ok(*boxed
        .downcast::<GateOutcome>()
        .expect("atomic_unit op for apply_fold_gate must return GateOutcome"))
}

/// Which gating applies to the implicit event participating in the ADR-081
/// §2/§6 atomic claim+fold+event-append unit
/// (`apply_fold_gate_and_append_event`). Explicit/correction signals never
/// reach this because they have no dedup claim or implicit mass to update.
pub enum FeedbackGateMode {
    /// The nominal implicit weight, subject to the ADR-081 §2 mass cap.
    Nominal(f64),
    /// ADR-081 §4 fail-safe: the serve ledger row has no resolvable
    /// accounting profile. Always folds at zero weight and never writes
    /// `brain_implicit_mass` — but it still
    /// participates in the `(scorer_run_id, serve_ledger_id)` dedup claim,
    /// atomically with the event append, so two concurrent forced-zero
    /// submissions for the same pair cannot both append an audit event.
    ForcedZero,
}

/// Result of the claim+fold(-or-skip) step inside
/// `apply_fold_gate_and_append_event`, handed to the caller's `build_event`
/// closure so the appended event's payload can carry the gate outcome.
pub struct GateAndAppendResult {
    /// `Some` for `FeedbackGateMode::Nominal` (the real mass check-and-fold
    /// ran). `None` for `FeedbackGateMode::ForcedZero` (no mass write — the
    /// zero-weight fail-safe never touches `brain_implicit_mass`).
    pub fold_outcome: Option<FoldGateOutcome>,
    /// `true` for `FeedbackGateMode::ForcedZero`.
    pub forced_zero: bool,
    /// The event this call appended (same value `build_event` returned).
    pub event: Event,
}

/// Outcome of `apply_fold_gate_and_append_event`.
pub enum GateAndAppendOutcome {
    /// The `(scorer_run_id, serve_ledger_id)` pair was already claimed by a
    /// prior call. Neither `brain_implicit_mass` nor the event log were
    /// touched by this call — the caller must treat this emission as a
    /// no-op.
    Deduped,
    /// The claim (if any) succeeded, the gate ran (or was skipped per
    /// `ForcedZero`), and the feedback event was appended — all inside the
    /// one held transaction, which is now committed. Boxed: `Event` carries
    /// an owned `serde_json::Value` payload, making this variant much
    /// larger than `Deduped`.
    Applied(Box<GateAndAppendResult>),
}

/// ADR-081 §2/§6 (PR #497): claim the `(scorer_run_id, serve_ledger_id)` dedup
/// key (if supplied), run the bounded-mass fold gate (or skip it for
/// `ForcedZero`), append the resulting `brain.feedback` event, and backfill
/// the scorer grade when supplied — as ONE
/// atomic, all-or-nothing unit on a single held `BEGIN IMMEDIATE` writer
/// transaction, mirroring `apply_fold_gate`'s commit/rollback shape.
///
/// `build_event` runs inside the transaction with the gate outcome, so its
/// event payload can carry the fold's numbers, and an append error aborts the
/// whole unit (claim included). On a claim conflict, returns `Deduped` before
/// running the fold or calling `build_event` at all. See
/// `crates/khive-pack-brain/docs/api/fold-gate.md` for the claim-rollback and
/// forced-zero interaction details.
#[allow(clippy::too_many_arguments)]
pub async fn apply_fold_gate_and_append_event<F>(
    sql: &dyn SqlAccess,
    namespace: &str,
    attribution: EventAttribution,
    profile_id: &str,
    target_id: &str,
    gate_mode: FeedbackGateMode,
    now_us: i64,
    dedup_key: Option<(&str, &str)>,
    grade: Option<&str>,
    build_event: F,
) -> Result<GateAndAppendOutcome, RuntimeError>
where
    F: FnOnce(Option<&FoldGateOutcome>, bool) -> Event + Send + 'static,
{
    // ADR-067 Component A: same atomic_unit conversion as `apply_fold_gate`.
    let namespace = namespace.to_string();
    let profile_id = profile_id.to_string();
    let target_id = target_id.to_string();
    let dedup_key = dedup_key.map(|(a, b)| (a.to_string(), b.to_string()));
    let grade = grade.map(str::to_owned);

    let op: khive_storage::AtomicUnitOp = Box::new(move |writer| {
        Box::pin(async move {
            let dedup_ref = dedup_key.as_ref().map(|(a, b)| (a.as_str(), b.as_str()));
            apply_gate_and_append_within_tx(
                writer,
                &namespace,
                &attribution,
                &profile_id,
                &target_id,
                gate_mode,
                now_us,
                dedup_ref,
                grade.as_deref(),
                build_event,
            )
            .await
            .map(|outcome| Box::new(outcome) as Box<dyn std::any::Any + Send>)
            .map_err(|e| {
                khive_storage::StorageError::driver(
                    khive_storage::StorageCapability::Sql,
                    "fold_gate_apply_event",
                    e,
                )
            })
        })
    });

    let boxed = sql.atomic_unit(op).await?;
    Ok(*boxed.downcast::<GateAndAppendOutcome>().expect(
        "atomic_unit op for apply_fold_gate_and_append_event must return GateAndAppendOutcome",
    ))
}

/// Run the dedup claim (if any), the fold-or-skip, and the event append on
/// `writer`, which must already be inside an open transaction. Does not
/// commit or roll back — the caller owns transaction boundaries.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn apply_gate_and_append_within_tx<F>(
    writer: &mut dyn SqlWriter,
    namespace: &str,
    attribution: &EventAttribution,
    profile_id: &str,
    target_id: &str,
    gate_mode: FeedbackGateMode,
    now_us: i64,
    dedup_key: Option<(&str, &str)>,
    grade: Option<&str>,
    build_event: F,
) -> Result<GateAndAppendOutcome, RuntimeError>
where
    F: FnOnce(Option<&FoldGateOutcome>, bool) -> Event,
{
    if let Some((scorer_run_id, serve_ledger_id)) = dedup_key {
        let claimed = claim_dedup_within_tx(writer, scorer_run_id, serve_ledger_id, now_us).await?;
        if !claimed {
            return Ok(GateAndAppendOutcome::Deduped);
        }
    }

    let (fold_outcome, forced_zero) = match gate_mode {
        FeedbackGateMode::Nominal(weight) => {
            let outcome =
                fold_within_tx(writer, namespace, profile_id, target_id, weight, now_us).await?;
            (Some(outcome), false)
        }
        FeedbackGateMode::ForcedZero => (None, true),
    };

    let event = attribution.stamp(build_event(fold_outcome.as_ref(), forced_zero));

    khive_db::stores::event::append_event_on_writer(writer, &event).await?;

    if let (Some((scorer_run_id, serve_ledger_id)), Some(grade)) = (dedup_key, grade) {
        crate::serve_ledger::backfill_grade_on_writer(
            writer,
            serve_ledger_id,
            grade,
            now_us,
            scorer_run_id,
        )
        .await?;
    }

    Ok(GateAndAppendOutcome::Applied(Box::new(
        GateAndAppendResult {
            fold_outcome,
            forced_zero,
            event,
        },
    )))
}

/// Run the dedup claim (if any) followed by the mass `SELECT` + decision +
/// `INSERT ... ON CONFLICT ... DO UPDATE` on `writer`, which must already be
/// inside an open transaction. Does not commit or roll back — the caller owns
/// transaction boundaries.
async fn apply_gate_within_tx(
    writer: &mut dyn SqlWriter,
    namespace: &str,
    profile_id: &str,
    target_id: &str,
    weight: f64,
    now_us: i64,
    dedup_key: Option<(&str, &str)>,
) -> Result<GateOutcome, RuntimeError> {
    if let Some((scorer_run_id, serve_ledger_id)) = dedup_key {
        let claimed = claim_dedup_within_tx(writer, scorer_run_id, serve_ledger_id, now_us).await?;
        if !claimed {
            return Ok(GateOutcome::Deduped);
        }
    }
    let outcome = fold_within_tx(writer, namespace, profile_id, target_id, weight, now_us).await?;
    Ok(GateOutcome::Folded(outcome))
}

/// Atomically claim `(scorer_run_id, serve_ledger_id)` in `brain_scorer_dedup`
/// via `INSERT OR IGNORE`, on `writer`, which must already be inside an open
/// transaction. Returns `true` if this call claimed the key (first time
/// seen), `false` if a prior call already holds it (0 rows affected — the
/// primary key rejected the conflicting insert).
pub(crate) async fn claim_dedup_within_tx(
    writer: &mut dyn SqlWriter,
    scorer_run_id: &str,
    serve_ledger_id: &str,
    now_us: i64,
) -> Result<bool, RuntimeError> {
    let rows_affected = writer
        .execute(SqlStatement {
            sql: sql!("brain_scorer_dedup_claim").into(),
            params: vec![
                SqlValue::Text(scorer_run_id.to_string()),
                SqlValue::Text(serve_ledger_id.to_string()),
                SqlValue::Integer(now_us),
            ],
            label: Some("brain_scorer_dedup_claim".into()),
        })
        .await?;
    Ok(rows_affected > 0)
}

/// Run the mass `SELECT` + decision + `INSERT ... ON CONFLICT ... DO UPDATE` on
/// `writer`, which must already be inside an open transaction. Does not commit or
/// roll back — the caller owns transaction boundaries.
async fn fold_within_tx(
    writer: &mut dyn SqlWriter,
    namespace: &str,
    profile_id: &str,
    target_id: &str,
    weight: f64,
    now_us: i64,
) -> Result<FoldGateOutcome, RuntimeError> {
    let row = writer
        .query_row(SqlStatement {
            sql: sql!("brain_implicit_mass_read").into(),
            params: vec![
                SqlValue::Text(profile_id.to_string()),
                SqlValue::Text(namespace.to_string()),
                SqlValue::Text(target_id.to_string()),
            ],
            label: Some("brain_implicit_mass_read".into()),
        })
        .await?;

    let (old_mass, last_event_at) = match row {
        None => (0.0, now_us),
        Some(r) => {
            let mass = r
                .f64("mass")
                .map_err(|_| sql_err("read mass", "missing mass column"))?;
            let last = r
                .i64("last_event_at")
                .map_err(|_| sql_err("read mass", "missing last_event_at column"))?;
            (mass, last)
        }
    };

    let mass_before = decayed_mass(old_mass, now_us - last_event_at);
    let (effective_weight, mass_after) = gate_decision(mass_before, weight, IMPLICIT_MASS_CAP);

    // Never move `last_event_at` backwards. A
    // negative `delta_us` above (clock skew) already keeps `mass_before`
    // undecayed and the event clamps/folds conservatively, but persisting
    // `now_us` verbatim would still drag the accumulator's clock back to the
    // skewed caller's earlier time — so the *next* event, even one from a
    // correctly-clocked caller arriving before the original future
    // `last_event_at`, would see the row as older than it is and decay mass
    // that should still be at full weight, letting it pass the clamp early.
    // Persisting `max(last_event_at, now_us)` keeps the accumulator's clock
    // monotonic regardless of how skewed an individual writer's `now_us` is.
    let persisted_last_event_at = last_event_at.max(now_us);

    writer
        .execute(SqlStatement {
            sql: sql!("brain_implicit_mass_upsert").into(),
            params: vec![
                SqlValue::Text(profile_id.to_string()),
                SqlValue::Text(namespace.to_string()),
                SqlValue::Text(target_id.to_string()),
                SqlValue::Float(mass_after),
                SqlValue::Integer(persisted_last_event_at),
                SqlValue::Float(effective_weight),
            ],
            label: Some("brain_implicit_mass_upsert".into()),
        })
        .await?;

    Ok(FoldGateOutcome {
        effective_weight,
        mass_before,
        mass_after,
    })
}

// ADR-067 Component A (Fork C slice 2): `apply_fold_gate`/
// `apply_fold_gate_and_append_event` no longer issue their own manual
// `BEGIN IMMEDIATE`/`COMMIT`/`ROLLBACK`; that logic lives in
// `SqlBridge::atomic_unit`'s `run_manual_atomic_unit`. The `exec_stmt`
// test-seeding helper that once lived here is gone with the last manual
// seed transaction: under a write-queue-enabled pool every statement is
// wrapped in the writer task's own transaction, so test seeding issues
// plain DML through the writer handle instead.

#[cfg(test)]
#[path = "fold_gate/tests.rs"]
mod tests;
