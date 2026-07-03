//! ADR-081 §2 bounded-mass fold gate for implicit feedback.
//!
//! Invariant: at any instant, the decayed implicit feedback mass folded into a
//! posterior for a given accounting key `(profile_id, namespace, target_id)` never
//! exceeds `IMPLICIT_MASS_CAP` — the weight of one explicit event. An incoming
//! implicit event folds at its full weight only if `M(k) + w <= CAP`; otherwise it
//! is recorded in the event log (audit preserved, per the data-vs-view principle)
//! and folded at zero weight.
//!
//! `M(k) = sum(w_i * 2^(-dt_i / T))` is not recomputed from the raw event log on
//! every fold. It is maintained as a single-row-per-key materialized accumulator
//! (`brain_implicit_mass`), read-decayed-written on each gated event, mirroring the
//! existing `brain_profile_snapshots` pattern of a derived accumulator living
//! alongside the append-only `brain_event_log`.
//!
//! Concurrency (ADR-081 §2 — normative): "the mass check and the fold execute in
//! one SQLite transaction opened with `BEGIN IMMEDIATE`... database-level
//! single-writer semantics serialize every check-and-fold against all concurrent
//! writers." `apply_fold_gate` implements this literally: it acquires exactly one
//! `SqlWriter` handle and issues `BEGIN IMMEDIATE`, the mass `SELECT`, the decision
//! (computed in Rust from the row read on *this* connection), the `INSERT ... ON
//! CONFLICT ... DO UPDATE`, and `COMMIT` — all on that single held connection, with
//! `ROLLBACK` on any error. For a file-backed pool, `writer()` opens a standalone
//! real `rusqlite::Connection`; `BEGIN IMMEDIATE` on it acquires SQLite's actual
//! file-level RESERVED lock for the duration, which SQLite enforces **across
//! processes**, not just within one. This is the property production needs:
//! khive-mcp routinely runs multiple concurrent daemon processes against the same
//! database file (issue #407), so an in-process mutex alone (e.g. `dispatch_gate`
//! in `BrainPack::dispatch`) cannot serialize the check-and-fold — only SQLite's
//! own write lock can. `SqlAccess::begin_tx` was not used for this because it
//! returns a distinct `SqlTransaction` type and hard-errors on non-file-backed
//! (in-memory) pools; issuing `BEGIN IMMEDIATE`/`COMMIT`/`ROLLBACK` as ordinary
//! statements through the existing `SqlWriter::execute` on one retained
//! `writer()` handle gets the same held-lock guarantee on file-backed pools,
//! while still functioning correctly (single-caller, no concurrency claim) on
//! the in-memory pools most of this crate's tests run against — the in-memory
//! backend serves every `writer()` call from one pool-wide shared connection
//! (`PoolBackedWriter` re-acquires the same `parking_lot` guard per call), so a
//! *second* concurrent `BEGIN IMMEDIATE` on it would hit SQLite's own
//! transaction-nesting error rather than genuinely racing — which is exactly
//! why the concurrency proof below (`fold_gate_concurrent_writers_never_exceed_cap`)
//! uses a real file-backed `KhiveRuntime`: only the file-backed path opens an
//! independent standalone connection per `writer()` call, the same shape
//! production's multiple concurrent `kkernel mcp` processes have.
//!
//! SQL math functions (`pow`/`exp`/`ln`/`log`) are unavailable on this
//! `rusqlite`/SQLite build (verified empirically: `SELECT pow(2.0, -1.0)` raises
//! "no such function"), which rules out expressing the entire decayed-mass +
//! clamp decision as one `INSERT ... RETURNING` statement with the decay math
//! inlined in SQL. The decay/clamp math instead runs in Rust (`decayed_mass`,
//! `gate_decision`, both pure and unit-tested below) between the `BEGIN IMMEDIATE`
//! and the `INSERT`, reading only the row already fetched on the held connection
//! — so no other writer can observe or mutate that row between the read and the
//! write.
use khive_runtime::RuntimeError;
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

/// Apply the ADR-081 §2 bounded-mass fold gate for one implicit feedback event.
///
/// `profile_id`/`namespace`/`target_id` form the accounting key. `weight` is the
/// nominal implicit weight (`FeedbackEventKind::update_weight()`, ADR-081 §1 —
/// currently `0.1`). `now_us` is the event's timestamp.
///
/// The mass check and the fold write happen inside one `BEGIN IMMEDIATE`
/// transaction held on a single `SqlWriter` connection (module doc above), so a
/// concurrent caller — another daemon process, on the same file-backed database
/// — cannot observe the pre-fold mass and race the write.
pub async fn apply_fold_gate(
    sql: &dyn SqlAccess,
    namespace: &str,
    profile_id: &str,
    target_id: &str,
    weight: f64,
    now_us: i64,
) -> Result<FoldGateOutcome, RuntimeError> {
    let mut writer = sql.writer().await.map_err(|e| sql_err("writer", e))?;

    exec_stmt(writer.as_mut(), "BEGIN IMMEDIATE", vec![], "begin")
        .await
        .map_err(|e| sql_err("begin", e))?;

    let result = fold_within_tx(
        writer.as_mut(),
        namespace,
        profile_id,
        target_id,
        weight,
        now_us,
    )
    .await;

    match result {
        Ok(outcome) => {
            exec_stmt(writer.as_mut(), "COMMIT", vec![], "commit")
                .await
                .map_err(|e| sql_err("commit", e))?;
            Ok(outcome)
        }
        Err(e) => {
            // Best-effort: the connection is dropped either way, but an explicit
            // ROLLBACK avoids leaving a held write lock if the connection is
            // pooled/reused (in-memory backend).
            let _ = exec_stmt(writer.as_mut(), "ROLLBACK", vec![], "rollback").await;
            Err(e)
        }
    }
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
                SqlValue::Integer(now_us),
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

    /// Proves the Finding-1 fix: the check-and-fold is atomic under genuine
    /// concurrent access, not just within one process's async runtime.
    ///
    /// Spawns 30 concurrent tasks, each calling `apply_fold_gate` directly
    /// (bypassing `BrainPack::dispatch`'s in-process `dispatch_gate` mutex
    /// entirely — the point is to prove the fold gate module itself, not an
    /// outer application lock, provides the safety) against the SAME
    /// accounting key on a file-backed runtime, all at the same `now_us` (no
    /// artificial time separation between tasks, to maximize race pressure).
    ///
    /// `IMPLICIT_MASS_CAP` / `WEIGHT` = 1.5 / 0.1 → floor(1.5 / 0.1) = 15 must
    /// pass at full weight and the other 15 must clamp to zero, REGARDLESS of
    /// scheduling order — this is a deterministic arithmetic consequence of
    /// correct serialization, not a timing-sensitive assertion. A reverted
    /// TOCTOU implementation (read-then-write outside a held lock) would very
    /// likely let more than 15 pass, since concurrent readers could observe
    /// the same stale pre-write mass simultaneously.
    #[tokio::test]
    async fn fold_gate_concurrent_writers_never_exceed_cap() {
        use khive_runtime::{BackendId, KhiveRuntime, Namespace, RuntimeConfig};
        use std::sync::Arc;

        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("fold-gate-concurrency.db");

        let rt = KhiveRuntime::new(RuntimeConfig {
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
                apply_fold_gate(
                    sql.as_ref(),
                    "local",
                    "concurrency-profile",
                    "concurrency-target",
                    WEIGHT,
                    now_us,
                )
                .await
                .expect("apply_fold_gate must not error under concurrent access")
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
}
