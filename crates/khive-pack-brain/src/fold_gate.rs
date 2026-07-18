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
use khive_runtime::RuntimeError;
use khive_storage::event::Event;
use khive_storage::types::{SqlStatement, SqlValue};
use khive_storage::{SqlAccess, SqlWriter};

/// One explicit event's weight (ADR-081 §1) — the clamp's comparator (ADR-081 §2).
pub const IMPLICIT_MASS_CAP: f64 = 1.5;

/// Decay half-life for implicit mass, shared with the serve-ledger suppression
/// window (ADR-081 §2, §4): 7 days, expressed in microseconds.
pub const IMPLICIT_MASS_HALF_LIFE_US: f64 = 7.0 * 24.0 * 3600.0 * 1_000_000.0;

fn sql_err(context: &str, e: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::Internal(format!("fold gate {context}: {e}"))
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
/// reach this — `handlers.rs` keeps their append path unchanged, per ADR-081
/// §6: "no dedup claim to keep consistent" for those signals.
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
/// `ForcedZero`), and append the resulting `brain.feedback` event — as ONE
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
    profile_id: &str,
    target_id: &str,
    gate_mode: FeedbackGateMode,
    now_us: i64,
    dedup_key: Option<(&str, &str)>,
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

    let op: khive_storage::AtomicUnitOp = Box::new(move |writer| {
        Box::pin(async move {
            let dedup_ref = dedup_key.as_ref().map(|(a, b)| (a.as_str(), b.as_str()));
            apply_gate_and_append_within_tx(
                writer,
                &namespace,
                &profile_id,
                &target_id,
                gate_mode,
                now_us,
                dedup_ref,
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
async fn apply_gate_and_append_within_tx<F>(
    writer: &mut dyn SqlWriter,
    namespace: &str,
    profile_id: &str,
    target_id: &str,
    gate_mode: FeedbackGateMode,
    now_us: i64,
    dedup_key: Option<(&str, &str)>,
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

    let event = build_event(fold_outcome.as_ref(), forced_zero);

    khive_db::stores::event::append_event_on_writer(writer, &event)
        .await
        .map_err(|e| sql_err("append feedback event", e))?;

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
async fn claim_dedup_within_tx(
    writer: &mut dyn SqlWriter,
    scorer_run_id: &str,
    serve_ledger_id: &str,
    now_us: i64,
) -> Result<bool, RuntimeError> {
    let rows_affected = writer
        .execute(SqlStatement {
            sql: "INSERT OR IGNORE INTO brain_scorer_dedup \
                  (scorer_run_id, serve_ledger_id, claimed_at) \
                  VALUES (?1, ?2, ?3)"
                .into(),
            params: vec![
                SqlValue::Text(scorer_run_id.to_string()),
                SqlValue::Text(serve_ledger_id.to_string()),
                SqlValue::Integer(now_us),
            ],
            label: Some("brain_scorer_dedup_claim".into()),
        })
        .await
        .map_err(|e| sql_err("claim scorer dedup", e))?;
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
            sql: "SELECT mass, last_event_at FROM brain_implicit_mass \
                  WHERE profile_id = ?1 AND namespace = ?2 AND target_id = ?3"
                .into(),
            params: vec![
                SqlValue::Text(profile_id.to_string()),
                SqlValue::Text(namespace.to_string()),
                SqlValue::Text(target_id.to_string()),
            ],
            label: Some("brain_implicit_mass_read".into()),
        })
        .await
        .map_err(|e| sql_err("read mass", e))?;

    let (old_mass, last_event_at) = match row {
        None => (0.0, now_us),
        Some(r) => {
            let mass = match r.get("mass") {
                Some(SqlValue::Float(v)) => *v,
                Some(SqlValue::Integer(v)) => *v as f64,
                _ => return Err(sql_err("read mass", "missing mass column")),
            };
            let last = match r.get("last_event_at") {
                Some(SqlValue::Integer(v)) => *v,
                _ => return Err(sql_err("read mass", "missing last_event_at column")),
            };
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
            sql: "INSERT INTO brain_implicit_mass \
                  (profile_id, namespace, target_id, mass, last_event_at, last_effective_weight) \
                  VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
                  ON CONFLICT(profile_id, namespace, target_id) \
                  DO UPDATE SET mass = excluded.mass, \
                                last_event_at = excluded.last_event_at, \
                                last_effective_weight = excluded.last_effective_weight"
                .into(),
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
        .await
        .map_err(|e| sql_err("write mass", e))?;

    Ok(FoldGateOutcome {
        effective_weight,
        mass_before,
        mass_after,
    })
}

// ADR-067 Component A (Fork C slice 2): `apply_fold_gate`/
// `apply_fold_gate_and_append_event` no longer issue their own manual
// `BEGIN IMMEDIATE`/`COMMIT`/`ROLLBACK` via this helper (that now lives in
// `SqlBridge::atomic_unit`'s `run_manual_atomic_unit`) — this remains only
// as a small test-seeding utility.
#[cfg(test)]
async fn exec_stmt(
    writer: &mut dyn SqlWriter,
    sql: &str,
    params: Vec<SqlValue>,
    label: &str,
) -> khive_storage::StorageResult<()> {
    writer
        .execute(SqlStatement {
            sql: sql.to_string(),
            params,
            label: Some(label.to_string()),
        })
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decayed_mass_zero_delta_is_unchanged() {
        assert!((decayed_mass(1.0, 0) - 1.0).abs() < 1e-12);
    }

    #[test]
    fn decayed_mass_negative_delta_is_unchanged() {
        // Clock skew guard: never amplify mass for a negative elapsed time.
        assert!((decayed_mass(1.0, -100) - 1.0).abs() < 1e-12);
    }

    #[test]
    fn decayed_mass_one_half_life_halves() {
        let m = decayed_mass(1.0, IMPLICIT_MASS_HALF_LIFE_US as i64);
        assert!((m - 0.5).abs() < 1e-9, "expected ~0.5, got {m}");
    }

    #[test]
    fn decayed_mass_two_half_lives_quarters() {
        let m = decayed_mass(1.0, 2 * IMPLICIT_MASS_HALF_LIFE_US as i64);
        assert!((m - 0.25).abs() < 1e-9, "expected ~0.25, got {m}");
    }

    #[test]
    fn gate_decision_passes_under_cap() {
        let (w, mass) = gate_decision(0.0, 0.1, IMPLICIT_MASS_CAP);
        assert!((w - 0.1).abs() < 1e-12);
        assert!((mass - 0.1).abs() < 1e-12);
    }

    #[test]
    fn gate_decision_passes_exactly_at_cap() {
        let (w, mass) = gate_decision(1.4, 0.1, IMPLICIT_MASS_CAP);
        assert!((w - 0.1).abs() < 1e-12, "exactly-at-cap must still pass");
        assert!((mass - 1.5).abs() < 1e-12);
    }

    #[test]
    fn gate_decision_clamps_over_cap() {
        let (w, mass) = gate_decision(1.45, 0.1, IMPLICIT_MASS_CAP);
        assert_eq!(w, 0.0, "over-cap event must fold at zero weight");
        assert!(
            (mass - 1.45).abs() < 1e-12,
            "clamped event must not move the persisted mass"
        );
    }

    #[test]
    fn gate_decision_saturation_after_fifteen_events() {
        // 15 events of weight 0.1 with no decay (delta=0 each time) sum to 1.5 —
        // exactly at the cap. The 16th must clamp to zero.
        let mut mass = 0.0;
        for i in 0..15 {
            let (w, m) = gate_decision(mass, 0.1, IMPLICIT_MASS_CAP);
            assert!((w - 0.1).abs() < 1e-9, "event {i} should pass, mass={mass}");
            mass = m;
        }
        assert!((mass - 1.5).abs() < 1e-9);
        let (w16, mass16) = gate_decision(mass, 0.1, IMPLICIT_MASS_CAP);
        assert_eq!(w16, 0.0, "16th event must clamp");
        assert!((mass16 - 1.5).abs() < 1e-9);
    }

    /// Proves the check-and-fold is atomic under genuine concurrent access
    /// (30 tasks, same key, file-backed runtime, bypassing `dispatch_gate`):
    /// exactly `floor(CAP/WEIGHT)` = 15 must pass at full weight regardless
    /// of scheduling order. See `docs/api/fold-gate.md`.
    #[tokio::test]
    async fn fold_gate_concurrent_writers_never_exceed_cap() {
        use khive_runtime::{BackendId, KhiveRuntime, Namespace, RuntimeConfig};
        use std::sync::Arc;

        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("fold-gate-concurrency.db");

        let rt = KhiveRuntime::new(RuntimeConfig {
            git_write: Default::default(),
            db_path: Some(db_path),
            default_namespace: Namespace::local(),
            embedding_model: None,
            additional_embedding_models: vec![],
            gate: Arc::new(khive_runtime::AllowAllGate),
            packs: vec!["kg".to_string()],
            backend_id: BackendId::main(),
            brain_profile: None,
            visible_namespaces: vec![],
            allowed_outbound_namespaces: vec![],
            actor_id: None,
        })
        .expect("file-backed runtime");

        let sql = rt.sql();
        const N: usize = 30;
        const WEIGHT: f64 = 0.1;
        let now_us: i64 = 1_700_000_000_000_000;

        let mut handles = Vec::with_capacity(N);
        for _ in 0..N {
            let sql = Arc::clone(&sql);
            handles.push(tokio::spawn(async move {
                let outcome = apply_fold_gate(
                    sql.as_ref(),
                    "local",
                    "concurrency-profile",
                    "concurrency-target",
                    WEIGHT,
                    now_us,
                    None,
                )
                .await
                .expect("apply_fold_gate must not error under concurrent access");
                match outcome {
                    GateOutcome::Folded(outcome) => outcome,
                    GateOutcome::Deduped => {
                        panic!("no dedup_key was supplied; must never dedup")
                    }
                }
            }));
        }

        let mut effective_weights = Vec::with_capacity(N);
        for h in handles {
            let outcome = h.await.expect("fold gate task must not panic");
            effective_weights.push(outcome.effective_weight);
        }

        let accepted = effective_weights.iter().filter(|w| **w > 0.0).count();
        let sum: f64 = effective_weights.iter().sum();

        assert_eq!(
            accepted, 15,
            "exactly floor(CAP/WEIGHT)=15 of {N} concurrent same-key events must fold at \
             full weight regardless of scheduling order; got {accepted} accepted \
             (weights: {effective_weights:?})"
        );
        assert!(
            (sum - 15.0 * WEIGHT).abs() < 1e-6,
            "sum of accepted effective weights must equal 15*{WEIGHT}, got {sum}"
        );

        // The persisted mass must match what the concurrent decisions imply —
        // proving the decision and the mass write came from the same atomic
        // statement sequence, not two racing snapshots.
        let mut reader = sql.reader().await.expect("reader");
        let row = khive_storage::SqlReader::query_row(
            reader.as_mut(),
            SqlStatement {
                sql: "SELECT mass FROM brain_implicit_mass \
                      WHERE profile_id = 'concurrency-profile' AND namespace = 'local' \
                      AND target_id = 'concurrency-target'"
                    .into(),
                params: vec![],
                label: None,
            },
        )
        .await
        .expect("read persisted mass")
        .expect("accumulator row must exist after 30 concurrent folds");
        let persisted_mass = match row.get("mass") {
            Some(SqlValue::Float(v)) => *v,
            Some(SqlValue::Integer(v)) => *v as f64,
            other => panic!("unexpected mass column value: {other:?}"),
        };
        assert!(
            (persisted_mass - 15.0 * WEIGHT).abs() < 1e-6,
            "persisted mass must equal 15*{WEIGHT}, got {persisted_mass}"
        );
    }

    /// Shared setup for the file-backed concurrency/skew tests below: only a
    /// real file-backed pool opens a standalone `rusqlite::Connection` per
    /// `writer()` call (module doc above) — the property production needs
    /// and the in-memory pool cannot exercise.
    fn file_backed_runtime(db_name: &str) -> (khive_runtime::KhiveRuntime, tempfile::TempDir) {
        use khive_runtime::{BackendId, KhiveRuntime, Namespace, RuntimeConfig};
        use std::sync::Arc;

        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join(db_name);
        let rt = KhiveRuntime::new(RuntimeConfig {
            git_write: Default::default(),
            db_path: Some(db_path),
            default_namespace: Namespace::local(),
            embedding_model: None,
            additional_embedding_models: vec![],
            gate: Arc::new(khive_runtime::AllowAllGate),
            packs: vec!["kg".to_string()],
            backend_id: BackendId::main(),
            brain_profile: None,
            visible_namespaces: vec![],
            allowed_outbound_namespaces: vec![],
            actor_id: None,
        })
        .expect("file-backed runtime");
        (rt, dir)
    }

    /// Fork C slice 2: proves `apply_fold_gate` — via the new
    /// `SqlAccess::atomic_unit` seam this fix introduced — is actually
    /// enqueued on the pool's shared `WriterTaskHandle` channel when the
    /// write queue is enabled, rather than falling back to
    /// `run_manual_atomic_unit`'s own standalone-connection path.
    ///
    /// Deliberately NOT a wall-clock/occupier-timing test: `atomic_unit`'s
    /// own flag-off/no-writer-task fallback opens a real standalone
    /// connection to the same db file and issues its own `BEGIN IMMEDIATE`,
    /// which would ALSO serialize behind an occupier's held transaction via
    /// SQLite's real file-level locking — indistinguishable by elapsed time
    /// alone from the correctly-routed case (confirmed empirically while
    /// designing this fix's khive-db sibling tests for entity/note/graph).
    /// Instead this reads `WriterTaskHandle::queue_depth` directly while an
    /// occupier deterministically holds the writer task's one drain slot
    /// open (parked on a oneshot via `blocking_recv`, not a sleep/timing
    /// race).
    ///
    /// Deliberately NOT built via `file_backed_runtime` + the
    /// `KHIVE_WRITE_QUEUE` env var: that env var is process-global, and this
    /// crate's other tests are NOT `#[serial]` against it, so a window where
    /// it is set here can leak into a concurrently-scheduled test's own
    /// `KhiveRuntime::new` and unexpectedly flip its pool's write-queue flag
    /// (observed directly: `fold_gate_rolls_back_claim_and_mass_when_event_append_fails`'s
    /// manual seed transaction hit "cannot start a transaction within a
    /// transaction" under `cargo test`'s default parallelism before this test
    /// was rewritten to avoid the env var). Constructing the pool directly
    /// with `write_queue_enabled: true` in the config literal, and driving
    /// `apply_fold_gate` over a bare `SqlBridge` instead of a full
    /// `KhiveRuntime`, sidesteps global mutable state entirely — no
    /// `#[serial]` needed, and no risk to any other test in this binary.
    #[tokio::test]
    async fn fold_gate_apply_routes_through_writer_task_when_flag_enabled() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("fold-gate-write-queue-routing.db");
        let pool_cfg = khive_db::PoolConfig {
            path: Some(db_path),
            write_queue_enabled: true,
            ..khive_db::PoolConfig::default()
        };
        let pool = std::sync::Arc::new(khive_db::ConnectionPool::new(pool_cfg).expect("pool"));
        {
            let mut writer = pool.writer().expect("writer");
            khive_db::run_migrations(writer.conn_mut()).expect("migrations");
        }
        let sql: std::sync::Arc<dyn SqlAccess> =
            std::sync::Arc::new(khive_db::SqlBridge::new(std::sync::Arc::clone(&pool), true));

        let writer_task = pool
            .writer_task_handle()
            .unwrap()
            .expect("writer task must be spawned with the flag on for a file-backed pool");

        let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let occupier = {
            let writer_task = writer_task.clone();
            tokio::spawn(async move {
                writer_task
                    .send(move |_conn| {
                        let _ = started_tx.send(());
                        let _ = release_rx.blocking_recv();
                        Ok::<(), khive_storage::StorageError>(())
                    })
                    .await
            })
        };

        started_rx
            .await
            .expect("occupier must signal it has started running inside the writer task");
        assert_eq!(
            writer_task.queue_depth(),
            0,
            "channel must start empty once the occupier has been dequeued and is running"
        );

        let apply_task = tokio::spawn(async move {
            apply_fold_gate(
                sql.as_ref(),
                "local",
                "write-queue-routing-profile",
                "write-queue-routing-target",
                0.3,
                1_700_000_000_000_000,
                None,
            )
            .await
        });

        let mut saw_enqueued = false;
        for _ in 0..100 {
            if writer_task.queue_depth() >= 1 {
                saw_enqueued = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            saw_enqueued,
            "apply_fold_gate's atomic_unit request never appeared in the writer \
             task's channel while the occupier held the single drain slot — \
             atomic_unit is not routing this call through the shared writer task"
        );

        release_tx
            .send(())
            .expect("occupier must still be waiting on the release signal");
        occupier
            .await
            .expect("occupier task must not panic")
            .expect("occupier write must succeed");

        let outcome = apply_task
            .await
            .expect("apply_task must not panic")
            .expect("apply_fold_gate must succeed once unblocked");
        assert!(
            matches!(outcome, GateOutcome::Folded(_)),
            "expected a fresh (non-deduped) fold to succeed"
        );
    }

    /// Proves the claim-conflict fix: concurrent duplicate scorer
    /// submissions — identical `(scorer_run_id, serve_ledger_id)` — fold
    /// exactly once, regardless of scheduling order.
    ///
    /// Mirrors `fold_gate_concurrent_writers_never_exceed_cap`'s shape (real
    /// file-backed runtime, standalone writer connections per task, no
    /// artificial time separation) but targets the dedup claim rather than
    /// the mass cap: `N` concurrent tasks call `apply_fold_gate` with the
    /// SAME dedup key. Before the fix, the dedup check
    /// (`serve_ledger::resolve`, reading the ledger row's `scorer_run_id`
    /// column) ran outside any transaction the fold gate holds, so every
    /// concurrent caller could observe "not yet graded" and all `N` would
    /// fold. With the fix, the claim on `brain_scorer_dedup` and the fold
    /// share one `BEGIN IMMEDIATE` transaction, so SQLite's own primary-key
    /// enforcement under the held write lock allows exactly one claim to
    /// succeed.
    #[tokio::test]
    async fn fold_gate_concurrent_duplicate_scorer_submissions_fold_once() {
        use std::sync::Arc;

        let (rt, _dir) = file_backed_runtime("fold-gate-dedup-concurrency.db");
        let sql = rt.sql();
        const N: usize = 30;
        const WEIGHT: f64 = 0.1;
        let now_us: i64 = 1_700_000_000_000_000;
        let scorer_run_id = "dup-scorer-run";
        let serve_ledger_id = "dup-serve-ledger-row";

        let mut handles = Vec::with_capacity(N);
        for _ in 0..N {
            let sql = Arc::clone(&sql);
            handles.push(tokio::spawn(async move {
                apply_fold_gate(
                    sql.as_ref(),
                    "local",
                    "dedup-profile",
                    "dedup-target",
                    WEIGHT,
                    now_us,
                    Some((scorer_run_id, serve_ledger_id)),
                )
                .await
                .expect("apply_fold_gate must not error under concurrent access")
            }));
        }

        let mut folded_count = 0;
        let mut deduped_count = 0;
        let mut sum = 0.0;
        for h in handles {
            match h.await.expect("fold gate task must not panic") {
                GateOutcome::Folded(outcome) => {
                    folded_count += 1;
                    sum += outcome.effective_weight;
                }
                GateOutcome::Deduped => deduped_count += 1,
            }
        }

        assert_eq!(
            folded_count, 1,
            "exactly one of {N} concurrent identical (scorer_run_id, serve_ledger_id) \
             submissions must fold; got {folded_count} folded, {deduped_count} deduped"
        );
        assert_eq!(deduped_count, N - 1);
        assert!(
            (sum - WEIGHT).abs() < 1e-9,
            "the one folded event must move the posterior by exactly {WEIGHT}, got {sum}"
        );

        // The persisted mass must reflect exactly one fold — proving the
        // claim and the fold committed together, not that N races happened
        // to net out to the same total.
        let mut reader = sql.reader().await.expect("reader");
        let row = khive_storage::SqlReader::query_row(
            reader.as_mut(),
            SqlStatement {
                sql: "SELECT mass FROM brain_implicit_mass \
                      WHERE profile_id = 'dedup-profile' AND namespace = 'local' \
                      AND target_id = 'dedup-target'"
                    .into(),
                params: vec![],
                label: None,
            },
        )
        .await
        .expect("read persisted mass")
        .expect("accumulator row must exist after the one successful fold");
        let persisted_mass = match row.get("mass") {
            Some(SqlValue::Float(v)) => *v,
            Some(SqlValue::Integer(v)) => *v as f64,
            other => panic!("unexpected mass column value: {other:?}"),
        };
        assert!(
            (persisted_mass - WEIGHT).abs() < 1e-9,
            "persisted mass must equal {WEIGHT} (one fold only), got {persisted_mass}"
        );

        // Exactly one row claimed the dedup key.
        let claim_count: i64 = match khive_storage::SqlReader::query_row(
            reader.as_mut(),
            SqlStatement {
                sql: "SELECT COUNT(*) AS n FROM brain_scorer_dedup \
                      WHERE scorer_run_id = ?1 AND serve_ledger_id = ?2"
                    .into(),
                params: vec![
                    SqlValue::Text(scorer_run_id.to_string()),
                    SqlValue::Text(serve_ledger_id.to_string()),
                ],
                label: None,
            },
        )
        .await
        .expect("read claim count")
        .expect("count row must exist")
        .get("n")
        {
            Some(SqlValue::Integer(v)) => *v,
            other => panic!("unexpected count column value: {other:?}"),
        };
        assert_eq!(
            claim_count, 1,
            "the primary key must admit exactly one claim row for this dedup key"
        );
    }

    /// Proves the clock-skew fix: a negative-clock-skew event
    /// must never drag the accumulator's `last_event_at` backwards, or a
    /// later, correctly-ordered event would decay mass that should still be
    /// at full weight and pass the clamp early.
    ///
    /// Reproduces the exact clock-skew scenario: a fast-clock daemon has
    /// already written `mass=1.5` (at cap) with `last_event_at = t+7d`. A
    /// slow-clock daemon emits an event at `t` (`now_us < last_event_at`,
    /// negative delta): `decayed_mass` correctly returns the mass undecayed,
    /// so the event clamps (mass already at cap) rather than amplifying —
    /// but before the fix, persisting `last_event_at = now_us` verbatim would
    /// still drag the row's clock back to `t`. A second, later event at
    /// `t+1d` (still `< t+7d`, so still legitimately in "the future" from
    /// that first future write's perspective) must ALSO clamp — under the
    /// bug, it would instead see the row as one day old, decay 1.5 toward
    /// ~1.357, and let `+0.1` pass, breaching the ADR-081 §2 invariant.
    #[tokio::test]
    async fn fold_gate_negative_clock_skew_never_moves_last_event_at_backwards() {
        let (rt, _dir) = file_backed_runtime("fold-gate-clock-skew.db");
        let sql = rt.sql();

        let namespace = "local";
        let profile_id = "clock-skew-profile";
        let target_id = "clock-skew-target";

        let slow_now_us: i64 = 1_700_000_000_000_000; // "t"
        let future_last_event_at = slow_now_us + IMPLICIT_MASS_HALF_LIFE_US as i64; // "t+7d"

        // Seed the row as a fast-clock daemon would have left it: at the cap,
        // stamped with a `last_event_at` seven days ahead of the slow
        // daemon's clock.
        {
            let mut writer = sql.writer().await.expect("writer");
            writer
                .execute(SqlStatement {
                    sql: "INSERT INTO brain_implicit_mass \
                          (profile_id, namespace, target_id, mass, last_event_at, \
                           last_effective_weight) \
                          VALUES (?1, ?2, ?3, ?4, ?5, ?6)"
                        .into(),
                    params: vec![
                        SqlValue::Text(profile_id.to_string()),
                        SqlValue::Text(namespace.to_string()),
                        SqlValue::Text(target_id.to_string()),
                        SqlValue::Float(IMPLICIT_MASS_CAP),
                        SqlValue::Integer(future_last_event_at),
                        SqlValue::Float(0.1),
                    ],
                    label: None,
                })
                .await
                .expect("seed future row");
        }

        // The slow-clock daemon's skewed event: now_us < last_event_at.
        let outcome = apply_fold_gate(
            sql.as_ref(),
            namespace,
            profile_id,
            target_id,
            0.1,
            slow_now_us,
            None,
        )
        .await
        .expect("apply_fold_gate must not error");
        let GateOutcome::Folded(outcome) = outcome else {
            panic!("no dedup_key was supplied; must never dedup");
        };
        assert_eq!(
            outcome.effective_weight, 0.0,
            "skewed event at an already-capped mass must clamp, not amplify"
        );
        assert!((outcome.mass_after - IMPLICIT_MASS_CAP).abs() < 1e-9);

        // THE FIX: `last_event_at` persisted after the skewed event must
        // still be the future timestamp, not `slow_now_us`.
        let persisted_last_event_at = {
            let mut reader = sql.reader().await.expect("reader");
            let row = khive_storage::SqlReader::query_row(
                reader.as_mut(),
                SqlStatement {
                    sql: "SELECT last_event_at FROM brain_implicit_mass \
                          WHERE profile_id = ?1 AND namespace = ?2 AND target_id = ?3"
                        .into(),
                    params: vec![
                        SqlValue::Text(profile_id.to_string()),
                        SqlValue::Text(namespace.to_string()),
                        SqlValue::Text(target_id.to_string()),
                    ],
                    label: None,
                },
            )
            .await
            .expect("read last_event_at")
            .expect("row must still exist");
            match row.get("last_event_at") {
                Some(SqlValue::Integer(v)) => *v,
                other => panic!("unexpected last_event_at column value: {other:?}"),
            }
        };
        assert_eq!(
            persisted_last_event_at, future_last_event_at,
            "last_event_at must never move backwards under clock skew"
        );

        // A later event, still before the future last_event_at, must ALSO
        // clamp — the bug this reproduces would instead let it pass because
        // the row's clock had been dragged back to slow_now_us.
        let one_day_us: i64 = 24 * 3600 * 1_000_000;
        let later_now_us = slow_now_us + one_day_us; // "t+1d", still < "t+7d"
        assert!(later_now_us < future_last_event_at);

        let outcome2 = apply_fold_gate(
            sql.as_ref(),
            namespace,
            profile_id,
            target_id,
            0.1,
            later_now_us,
            None,
        )
        .await
        .expect("apply_fold_gate must not error");
        let GateOutcome::Folded(outcome2) = outcome2 else {
            panic!("no dedup_key was supplied; must never dedup");
        };
        assert_eq!(
            outcome2.effective_weight, 0.0,
            "later pre-future event must still clamp; the fix must prevent the mass from \
             decaying as if the row's clock had been dragged back to t"
        );
        assert!((outcome2.mass_after - IMPLICIT_MASS_CAP).abs() < 1e-9);
    }

    /// Proves the PR #497 fix: if the
    /// feedback event append fails AFTER a successful dedup claim, the whole
    /// atomic unit — claim and (skipped-on-clamp-aside) mass write included
    /// — rolls back, so a retry sees no claim and proceeds normally.
    ///
    /// The failure is injected by making `build_event` return an `Event`
    /// whose `id` collides with a row already seeded in `events` (`id` is
    /// that table's `PRIMARY KEY`) — a real, deterministic SQLite
    /// constraint violation on the INSERT, not a mock. `append_event_on_writer`'s
    /// INSERT has no `OR IGNORE`, so the conflict surfaces as an `Err` from
    /// `SqlWriter::execute`, propagating out of `apply_fold_gate_and_append_event`
    /// before `COMMIT` — exercising the previously untested rollback path
    /// (before this fix, the claim+fold committed in their own
    /// transaction before the event append ran in a separate one, so this
    /// scenario could not roll back the claim at all).
    #[tokio::test]
    async fn fold_gate_rolls_back_claim_and_mass_when_event_append_fails() {
        use khive_storage::event::Event;
        use khive_types::{EventKind, SubstrateKind};

        let (rt, _dir) = file_backed_runtime("fold-gate-append-failure-rollback.db");
        let sql = rt.sql();

        let namespace = "local";
        let profile_id = "rollback-profile";
        let target_id = "rollback-target";
        let weight = 0.1;
        let now_us: i64 = 1_700_000_000_000_000;
        let scorer_run_id = "rollback-scorer-run";
        let serve_ledger_id = "rollback-serve-ledger-row";
        let event_target = uuid::Uuid::new_v4();
        let colliding_id = uuid::Uuid::new_v4();

        // Seed a colliding `events` row outside the unit under test.
        {
            let mut writer = sql.writer().await.expect("writer");
            exec_stmt(writer.as_mut(), "BEGIN IMMEDIATE", vec![], "seed_begin")
                .await
                .expect("begin seed txn");
            let seed_event = Event {
                id: colliding_id,
                ..Event::new(
                    namespace.to_string(),
                    "brain.feedback",
                    EventKind::FeedbackExplicit,
                    SubstrateKind::Event,
                    "brain",
                )
            };
            khive_db::stores::event::append_event_on_writer(writer.as_mut(), &seed_event)
                .await
                .expect("seed colliding event");
            exec_stmt(writer.as_mut(), "COMMIT", vec![], "seed_commit")
                .await
                .expect("commit seed txn");
        }

        // First attempt: claim succeeds, mass folds, but the event append
        // hits the seeded PRIMARY KEY collision and the whole unit errors.
        let first_attempt = apply_fold_gate_and_append_event(
            sql.as_ref(),
            namespace,
            profile_id,
            target_id,
            FeedbackGateMode::Nominal(weight),
            now_us,
            Some((scorer_run_id, serve_ledger_id)),
            move |_fold_outcome, _forced_zero| Event {
                id: colliding_id,
                ..Event::new(
                    namespace.to_string(),
                    "brain.feedback",
                    EventKind::FeedbackExplicit,
                    SubstrateKind::Event,
                    "brain",
                )
                .with_target(event_target)
            },
        )
        .await;
        assert!(
            first_attempt.is_err(),
            "event append PK collision must surface as an error, not succeed silently"
        );

        // The claim and the mass write must both have rolled back — neither
        // table shows the failed attempt.
        {
            let mut reader = sql.reader().await.expect("reader");
            let claim_count: i64 = match khive_storage::SqlReader::query_row(
                reader.as_mut(),
                SqlStatement {
                    sql: "SELECT COUNT(*) AS n FROM brain_scorer_dedup \
                          WHERE scorer_run_id = ?1 AND serve_ledger_id = ?2"
                        .into(),
                    params: vec![
                        SqlValue::Text(scorer_run_id.to_string()),
                        SqlValue::Text(serve_ledger_id.to_string()),
                    ],
                    label: None,
                },
            )
            .await
            .expect("read claim count")
            .expect("count row must exist")
            .get("n")
            {
                Some(SqlValue::Integer(v)) => *v,
                other => panic!("unexpected count column value: {other:?}"),
            };
            assert_eq!(
                claim_count, 0,
                "the failed attempt's claim must have rolled back, not stuck as a committed \
                 orphan that would suppress a legitimate retry"
            );

            let mass_count: i64 = match khive_storage::SqlReader::query_row(
                reader.as_mut(),
                SqlStatement {
                    sql: "SELECT COUNT(*) AS n FROM brain_implicit_mass \
                          WHERE profile_id = ?1 AND namespace = ?2 AND target_id = ?3"
                        .into(),
                    params: vec![
                        SqlValue::Text(profile_id.to_string()),
                        SqlValue::Text(namespace.to_string()),
                        SqlValue::Text(target_id.to_string()),
                    ],
                    label: None,
                },
            )
            .await
            .expect("read mass count")
            .expect("count row must exist")
            .get("n")
            {
                Some(SqlValue::Integer(v)) => *v,
                other => panic!("unexpected count column value: {other:?}"),
            };
            assert_eq!(
                mass_count, 0,
                "the failed attempt's mass write must have rolled back with the claim"
            );
        }

        // Retry with the same dedup key: since the claim rolled back, this
        // must proceed normally (not `Deduped`), fold at full weight, and
        // append its own event.
        let retry = apply_fold_gate_and_append_event(
            sql.as_ref(),
            namespace,
            profile_id,
            target_id,
            FeedbackGateMode::Nominal(weight),
            now_us,
            Some((scorer_run_id, serve_ledger_id)),
            move |_fold_outcome, _forced_zero| {
                Event::new(
                    namespace.to_string(),
                    "brain.feedback",
                    EventKind::FeedbackExplicit,
                    SubstrateKind::Event,
                    "brain",
                )
                .with_target(event_target)
            },
        )
        .await
        .expect("retry after rollback must succeed");

        match retry {
            GateAndAppendOutcome::Applied(result) => {
                assert!(!result.forced_zero);
                let outcome = result
                    .fold_outcome
                    .expect("Nominal gate mode always produces a fold outcome");
                assert!((outcome.effective_weight - weight).abs() < 1e-9);
                assert!((outcome.mass_after - weight).abs() < 1e-9);
            }
            GateAndAppendOutcome::Deduped => {
                panic!("retry after a rolled-back claim must not be deduped")
            }
        }

        // Persisted state after the retry: exactly one claim, mass moved
        // exactly once (to `weight`), exactly one durable event for this
        // target — the failed attempt's event never persisted.
        let mut reader = sql.reader().await.expect("reader");
        let claim_count_after: i64 = match khive_storage::SqlReader::query_row(
            reader.as_mut(),
            SqlStatement {
                sql: "SELECT COUNT(*) AS n FROM brain_scorer_dedup \
                      WHERE scorer_run_id = ?1 AND serve_ledger_id = ?2"
                    .into(),
                params: vec![
                    SqlValue::Text(scorer_run_id.to_string()),
                    SqlValue::Text(serve_ledger_id.to_string()),
                ],
                label: None,
            },
        )
        .await
        .expect("read claim count")
        .expect("count row must exist")
        .get("n")
        {
            Some(SqlValue::Integer(v)) => *v,
            other => panic!("unexpected count column value: {other:?}"),
        };
        assert_eq!(claim_count_after, 1, "exactly one claim after the retry");

        let mass_after: f64 = match khive_storage::SqlReader::query_row(
            reader.as_mut(),
            SqlStatement {
                sql: "SELECT mass FROM brain_implicit_mass \
                      WHERE profile_id = ?1 AND namespace = ?2 AND target_id = ?3"
                    .into(),
                params: vec![
                    SqlValue::Text(profile_id.to_string()),
                    SqlValue::Text(namespace.to_string()),
                    SqlValue::Text(target_id.to_string()),
                ],
                label: None,
            },
        )
        .await
        .expect("read mass")
        .expect("row must exist after the retry")
        .get("mass")
        {
            Some(SqlValue::Float(v)) => *v,
            Some(SqlValue::Integer(v)) => *v as f64,
            other => panic!("unexpected mass column value: {other:?}"),
        };
        assert!(
            (mass_after - weight).abs() < 1e-9,
            "mass must have moved exactly once (to {weight}), got {mass_after}"
        );

        let event_count: i64 = match khive_storage::SqlReader::query_row(
            reader.as_mut(),
            SqlStatement {
                sql: "SELECT COUNT(*) AS n FROM events WHERE target_id = ?1".into(),
                params: vec![SqlValue::Text(event_target.to_string())],
                label: None,
            },
        )
        .await
        .expect("read event count")
        .expect("count row must exist")
        .get("n")
        {
            Some(SqlValue::Integer(v)) => *v,
            other => panic!("unexpected count column value: {other:?}"),
        };
        assert_eq!(
            event_count, 1,
            "exactly one durable event for this target: the retry's — the failed \
             attempt's event insert rolled back with everything else in its transaction"
        );
    }

    /// Proves the PR #497 fix: the
    /// ADR-081 §4 forced-zero fail-safe (`FeedbackGateMode::ForcedZero`) now
    /// claims the dedup key atomically, on the SAME held transaction as the
    /// (skipped) mass fold and the event append — so N concurrent identical
    /// `(scorer_run_id, serve_ledger_id)` forced-zero submissions can no
    /// longer all bypass the claim and each append their own zero-weight
    /// audit event; exactly one must land, mirroring
    /// `fold_gate_concurrent_duplicate_scorer_submissions_fold_once`'s shape
    /// for the nominal path.
    #[tokio::test]
    async fn fold_gate_concurrent_forced_zero_duplicate_submissions_append_once() {
        use khive_storage::event::Event;
        use khive_types::{EventKind, SubstrateKind};
        use std::sync::Arc;

        let (rt, _dir) = file_backed_runtime("fold-gate-forced-zero-dedup-concurrency.db");
        let sql = rt.sql();
        const N: usize = 30;
        let now_us: i64 = 1_700_000_000_000_000;
        let scorer_run_id = "forced-zero-scorer-run";
        let serve_ledger_id = "forced-zero-serve-ledger-row";
        let event_target = uuid::Uuid::new_v4();

        let mut handles = Vec::with_capacity(N);
        for _ in 0..N {
            let sql = Arc::clone(&sql);
            handles.push(tokio::spawn(async move {
                apply_fold_gate_and_append_event(
                    sql.as_ref(),
                    "local",
                    "forced-zero-profile",
                    "forced-zero-target",
                    FeedbackGateMode::ForcedZero,
                    now_us,
                    Some((scorer_run_id, serve_ledger_id)),
                    move |fold_outcome, forced_zero| {
                        assert!(
                            fold_outcome.is_none(),
                            "ForcedZero must never run the mass fold"
                        );
                        assert!(forced_zero);
                        Event::new(
                            "local".to_string(),
                            "brain.feedback",
                            EventKind::FeedbackExplicit,
                            SubstrateKind::Event,
                            "brain",
                        )
                        .with_target(event_target)
                        .with_payload(serde_json::json!({
                            "signal": "implicit_negative",
                            "gate": {"forced_zero_weight": true},
                        }))
                    },
                )
                .await
                .expect("apply_fold_gate_and_append_event must not error under concurrent access")
            }));
        }

        let mut applied_count = 0;
        let mut deduped_count = 0;
        for h in handles {
            match h.await.expect("fold gate task must not panic") {
                GateAndAppendOutcome::Applied(result) => {
                    applied_count += 1;
                    assert!(result.forced_zero);
                    assert!(result.fold_outcome.is_none());
                }
                GateAndAppendOutcome::Deduped => deduped_count += 1,
            }
        }

        assert_eq!(
            applied_count, 1,
            "exactly one of {N} concurrent identical forced-zero submissions must append; \
             got {applied_count} applied, {deduped_count} deduped"
        );
        assert_eq!(deduped_count, N - 1);

        let mut reader = sql.reader().await.expect("reader");
        let claim_count: i64 = match khive_storage::SqlReader::query_row(
            reader.as_mut(),
            SqlStatement {
                sql: "SELECT COUNT(*) AS n FROM brain_scorer_dedup \
                      WHERE scorer_run_id = ?1 AND serve_ledger_id = ?2"
                    .into(),
                params: vec![
                    SqlValue::Text(scorer_run_id.to_string()),
                    SqlValue::Text(serve_ledger_id.to_string()),
                ],
                label: None,
            },
        )
        .await
        .expect("read claim count")
        .expect("count row must exist")
        .get("n")
        {
            Some(SqlValue::Integer(v)) => *v,
            other => panic!("unexpected count column value: {other:?}"),
        };
        assert_eq!(
            claim_count, 1,
            "the primary key must admit exactly one claim row for this dedup key"
        );

        let mass_count: i64 = match khive_storage::SqlReader::query_row(
            reader.as_mut(),
            SqlStatement {
                sql: "SELECT COUNT(*) AS n FROM brain_implicit_mass \
                      WHERE profile_id = 'forced-zero-profile' AND namespace = 'local' \
                      AND target_id = 'forced-zero-target'"
                    .into(),
                params: vec![],
                label: None,
            },
        )
        .await
        .expect("read mass count")
        .expect("count row must exist")
        .get("n")
        {
            Some(SqlValue::Integer(v)) => *v,
            other => panic!("unexpected count column value: {other:?}"),
        };
        assert_eq!(
            mass_count, 0,
            "ForcedZero must never write brain_implicit_mass, even for the one applied call"
        );

        let event_count: i64 = match khive_storage::SqlReader::query_row(
            reader.as_mut(),
            SqlStatement {
                sql: "SELECT COUNT(*) AS n FROM events WHERE target_id = ?1".into(),
                params: vec![SqlValue::Text(event_target.to_string())],
                label: None,
            },
        )
        .await
        .expect("read event count")
        .expect("count row must exist")
        .get("n")
        {
            Some(SqlValue::Integer(v)) => *v,
            other => panic!("unexpected count column value: {other:?}"),
        };
        assert_eq!(
            event_count, 1,
            "exactly one zero-weight feedback event must be appended across all {N} \
             concurrent forced-zero submissions"
        );
    }
}
