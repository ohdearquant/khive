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
//! Concurrency: the read-decide-write sequence is wrapped in a single
//! `writer().execute_batch(..)` call for the write half. `execute_batch` already
//! wraps its statements in `BEGIN IMMEDIATE ... COMMIT` at the `SqlBridge` layer
//! (both file-backed and in-memory pool backends — verified by inspection of
//! `crates/khive-db/src/sql_bridge.rs`), so the write itself is atomic on both
//! backends. `SqlAccess::begin_tx` was considered for a literal read+write
//! transaction handle, but it hard-errors for non-file-backed (in-memory) pools,
//! which the entire pack-brain test suite (and `KhiveRuntime::memory()` more
//! broadly) depends on — so it cannot be used unconditionally without breaking
//! every existing in-memory-backed test. The residual TOCTOU window between the
//! read and the write is accepted and documented: `dispatch_gate`
//! (`BrainPack::dispatch`) already serializes every `brain.*` call within one
//! daemon process today, and khive-mcp's production deployment is always
//! file-backed, so genuine concurrent writers to the same accounting key are not
//! a live path. This mirrors the ADR's own tolerance for the invariant being a
//! soft rate-limit (mis-tuning under-trains rather than corrupts) rather than a
//! hard security boundary.
use khive_runtime::RuntimeError;
use khive_storage::types::{SqlStatement, SqlValue};
use khive_storage::SqlAccess;

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
pub async fn apply_fold_gate(
    sql: &dyn SqlAccess,
    namespace: &str,
    profile_id: &str,
    target_id: &str,
    weight: f64,
    now_us: i64,
) -> Result<FoldGateOutcome, RuntimeError> {
    let mut reader = sql.reader().await.map_err(|e| sql_err("reader", e))?;
    let row = reader
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

    let mut writer = sql.writer().await.map_err(|e| sql_err("writer", e))?;
    writer
        .execute_batch(vec![SqlStatement {
            sql: "INSERT INTO brain_implicit_mass (profile_id, namespace, target_id, mass, last_event_at) \
                  VALUES (?1, ?2, ?3, ?4, ?5) \
                  ON CONFLICT(profile_id, namespace, target_id) \
                  DO UPDATE SET mass = excluded.mass, last_event_at = excluded.last_event_at"
                .into(),
            params: vec![
                SqlValue::Text(profile_id.to_string()),
                SqlValue::Text(namespace.to_string()),
                SqlValue::Text(target_id.to_string()),
                SqlValue::Float(mass_after),
                SqlValue::Integer(now_us),
            ],
            label: Some("brain_implicit_mass_upsert".into()),
        }])
        .await
        .map_err(|e| sql_err("write mass", e))?;

    Ok(FoldGateOutcome {
        effective_weight,
        mass_before,
        mass_after,
    })
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
}
