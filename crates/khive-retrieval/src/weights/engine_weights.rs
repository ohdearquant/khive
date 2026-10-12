//! EMA-based atom weight store with audit log for ambient, explicit, and ground-truth feedback.

use std::any::Any;
use std::collections::HashMap;

use khive_storage::types::{SqlStatement, SqlValue};
use khive_storage::{AtomicUnitOp, SqlAccess, StorageError};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::persist::PersistError;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Lower bound for atom weights — prevents a weight from reaching zero.
pub const WEIGHT_FLOOR: f32 = 0.1;

/// Upper bound for atom weights — prevents runaway boosting.
pub const WEIGHT_CEIL: f32 = 5.0;

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

/// Create the `atom_weights` and `weight_events` tables if they do not exist.
///
/// Legacy bootstrap for standalone retrieval databases, retained for callers
/// that own a raw connection. Shared khive-db databases create these tables
/// through core migrations; routed weight operations require that preparation
/// and do not run DDL. The replay module also reads `weight_events`.
pub fn init_weight_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(include_str!("../../sql/atom_weights_create.sql"))?;
    conn.execute_batch(include_str!("../../sql/weight_events_create.sql"))
}

// ---------------------------------------------------------------------------
// WeightChannel
// ---------------------------------------------------------------------------

/// Three feedback channels each with a distinct learning rate η.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WeightChannel {
    /// η = 0.003 — every recall / compose invocation.
    Ambient,
    /// η = 0.10 — voluntary quality signal via note.create / note.correct.
    Explicit,
    /// η = 0.50 — Atlas eval or manual CLI trigger.
    GroundTruth,
}

impl WeightChannel {
    /// Learning rate η for this channel.
    pub fn eta(self) -> f32 {
        match self {
            Self::Ambient => 0.003,
            Self::Explicit => 0.10,
            Self::GroundTruth => 0.50,
        }
    }

    /// Canonical snake_case string stored in `weight_events.channel`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ambient => "ambient",
            Self::Explicit => "explicit",
            Self::GroundTruth => "ground_truth",
        }
    }
}

// ---------------------------------------------------------------------------
// apply_weight_delta
// ---------------------------------------------------------------------------

/// Apply an EMA weight update for `(namespace, atom_id)` and append an audit row. Returns `(new_weight, row_id)`.
pub async fn apply_weight_delta(
    sql: &dyn SqlAccess,
    namespace: &str,
    atom_id: Uuid,
    delta: f32,
    channel: WeightChannel,
    event_id: Option<Uuid>,
    context_id: Option<&str>,
) -> Result<(f32, i64), PersistError> {
    apply_weight_delta_with_eta(
        sql,
        namespace,
        atom_id,
        delta,
        channel,
        channel.eta(),
        event_id,
        context_id,
    )
    .await
}

/// Variant of [`apply_weight_delta`] that accepts a runtime-overridden `eta`.
///
/// Use this when the caller loads η from runtime config (e.g., atlas's
/// `knowledge.toml` override of Channel C's default 0.50). Same algorithm
/// and transactional guarantees as [`apply_weight_delta`].
#[allow(clippy::too_many_arguments)]
pub async fn apply_weight_delta_with_eta(
    sql: &dyn SqlAccess,
    namespace: &str,
    atom_id: Uuid,
    delta: f32,
    channel: WeightChannel,
    eta: f32,
    event_id: Option<Uuid>,
    context_id: Option<&str>,
) -> Result<(f32, i64), PersistError> {
    if namespace.is_empty() {
        tracing::warn!(
            atom_id = %atom_id,
            channel = %channel.as_str(),
            "apply_weight_delta called with empty namespace — rejecting to avoid dead-namespace pollution"
        );
        return Err(PersistError::Validation(
            "namespace must not be empty".to_string(),
        ));
    }
    if !(0.0..=1.0).contains(&eta) {
        return Err(PersistError::Validation(format!(
            "eta must be in [0.0, 1.0], got {eta}"
        )));
    }
    // Reject non-finite delta to prevent NaN/Inf from being persisted to atom_weights.
    // A NaN delta would silently corrupt the stored weight on every subsequent read.
    if !delta.is_finite() {
        return Err(PersistError::Validation(format!(
            "delta must be finite, got {delta}"
        )));
    }
    let namespace_str = namespace.to_string();
    let atom_id_str = atom_id.to_string();
    let channel_str = channel.as_str();
    let event_id_str = event_id.map(|u| u.to_string());
    let context_id = context_id.map(|s| s.to_string());
    let now_us = chrono::Utc::now().timestamp_micros();

    let op: AtomicUnitOp = Box::new(move |writer| {
        Box::pin(async move {
            // Read current weight (default 1.0 if row absent).
            let row = writer
                .query_row(
                    SqlStatement::new(
                        include_str!("../../sql/atom_weights_select_weight.sql"),
                        vec![
                            SqlValue::Text(namespace_str.clone()),
                            SqlValue::Text(atom_id_str.clone()),
                        ],
                    )
                    .labelled("weights.read_current"),
                )
                .await?;
            let old_weight = row
                .as_ref()
                .map(|row| row.f64("weight"))
                .transpose()
                .map_err(|error| StorageError::Internal(format!("read atom weight: {error}")))?
                .unwrap_or(1.0_f64) as f32;

            // EMA update + clamp.
            let new_weight = (old_weight * (1.0 - eta) + delta).clamp(WEIGHT_FLOOR, WEIGHT_CEIL);

            // Upsert atom_weights — increment version on each write.
            writer
                .execute(
                    SqlStatement::new(
                        include_str!("../../sql/atom_weights_upsert.sql"),
                        vec![
                            SqlValue::Text(namespace_str.clone()),
                            SqlValue::Text(atom_id_str.clone()),
                            SqlValue::Float(new_weight as f64),
                            SqlValue::Integer(now_us),
                        ],
                    )
                    .labelled("weights.upsert"),
                )
                .await?;

            // Append weight_events audit row.
            writer
                .execute(
                    SqlStatement::new(
                        include_str!("../../sql/weight_events_insert.sql"),
                        vec![
                            SqlValue::Text(namespace_str),
                            SqlValue::Text(atom_id_str),
                            SqlValue::Float(delta as f64),
                            SqlValue::Float(new_weight as f64),
                            SqlValue::Text(channel_str.to_owned()),
                            SqlValue::Float(eta as f64),
                            event_id_str.map(SqlValue::Text).unwrap_or(SqlValue::Null),
                            context_id.map(SqlValue::Text).unwrap_or(SqlValue::Null),
                            SqlValue::Integer(now_us),
                        ],
                    )
                    .labelled("weights.audit_insert"),
                )
                .await?;

            // Read on this same admitted writer before the atomic owner commits.
            let row = writer
                .query_row(
                    SqlStatement::new(
                        include_str!("../../sql/weight_events_last_insert_rowid.sql"),
                        vec![],
                    )
                    .labelled("weights.audit_row_id"),
                )
                .await?
                .ok_or_else(|| StorageError::Internal("missing weight audit row ID".into()))?;
            let row_id = row.i64("row_id").map_err(|error| {
                StorageError::Internal(format!("read weight audit row ID: {error}"))
            })?;
            Ok(Box::new((new_weight, row_id)) as Box<dyn Any + Send>)
        })
    });
    let result = sql.atomic_unit(op).await?;
    result
        .downcast::<(f32, i64)>()
        .map(|result| *result)
        .map_err(|_| {
            PersistError::Internal("weight atomic unit returned an unexpected result".into())
        })
}

// ---------------------------------------------------------------------------
// batch_load_weights
// ---------------------------------------------------------------------------

/// Batch-load current weights for a slice of atom IDs under one lambda.
///
/// Only rows that exist in `atom_weights` are returned.  Missing atoms are
/// **not** inserted; callers should treat absent entries as implicit 1.0.
///
/// Uses a single SQL query with a dynamic `IN (...)` clause.  The batch is
/// chunked when `atom_ids` exceeds the SQLite 999-bind-param ceiling.
pub async fn batch_load_weights(
    sql: &dyn SqlAccess,
    namespace: &str,
    atom_ids: &[Uuid],
) -> Result<HashMap<Uuid, f32>, PersistError> {
    if atom_ids.is_empty() {
        return Ok(HashMap::new());
    }

    let namespace_str = namespace.to_string();
    let id_strs: Vec<String> = atom_ids.iter().map(|u| u.to_string()).collect();
    let mut result = HashMap::with_capacity(id_strs.len());
    let mut reader = sql.reader().await?;

    // Chunk to stay within the SQLite 999-parameter limit.
    // Each chunk uses 1 param (namespace) + N params (atom_ids) = N+1 total.
    const CHUNK_SIZE: usize = 998;

    for chunk in id_strs.chunks(CHUNK_SIZE) {
        let placeholders = chunk
            .iter()
            .enumerate()
            .map(|(i, _)| format!("?{}", i + 2))
            .collect::<Vec<_>>()
            .join(", ");

        let statement = format!(
            "SELECT atom_id, weight FROM atom_weights \
             WHERE namespace = ?1 AND atom_id IN ({placeholders})"
        );

        let mut param_values = Vec::with_capacity(chunk.len() + 1);
        param_values.push(SqlValue::Text(namespace_str.clone()));
        for id in chunk {
            param_values.push(SqlValue::Text(id.clone()));
        }

        let rows = reader
            .query_all(SqlStatement::new(statement, param_values).labelled("weights.batch_load"))
            .await?;
        for row in rows {
            let aid = row
                .text("atom_id")
                .map_err(|error| StorageError::Internal(format!("read weight atom ID: {error}")))?;
            let w = row
                .f64("weight")
                .map_err(|error| StorageError::Internal(format!("read atom weight: {error}")))?;
            if let Ok(uuid) = aid.parse::<Uuid>() {
                // Clamp on read — symmetric with write-side invariant. Protects compose
                // from weight=0 rows introduced by manual SQL or future schema drift.
                let clamped = (w as f32).clamp(WEIGHT_FLOOR, WEIGHT_CEIL);
                result.insert(uuid, clamped);
            }
        }
    }

    Ok(result)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use khive_db::StorageBackend;

    fn make_backend() -> StorageBackend {
        let backend = StorageBackend::memory().expect("memory backend");
        backend.prepare_core_schema().expect("core schema");
        backend
    }

    // -------------------------------------------------------------------------
    // Test 1 — ambient channel drives weight above 1.0 over 5 ticks
    // -------------------------------------------------------------------------
    #[tokio::test]
    async fn test_apply_weight_delta_ambient_channel() {
        let conn = make_backend();
        let lambda = "lambda:test";
        let atom = Uuid::new_v4();
        let delta = 0.01_f32; // positive ambient nudge

        let mut last_weight = 1.0_f32;
        for _ in 0..5 {
            let (w, _row_id) = apply_weight_delta(
                &conn,
                lambda,
                atom,
                delta,
                WeightChannel::Ambient,
                None,
                None,
            )
            .await
            .expect("apply_weight_delta should succeed");
            last_weight = w;
        }

        assert!(
            last_weight > 1.0,
            "weight should rise above 1.0 with positive delta, got {last_weight}"
        );
        assert!(
            last_weight < WEIGHT_CEIL,
            "weight should not reach ceiling after 5 ticks"
        );

        // Verify 5 audit rows were written.
        let map = batch_load_weights(&conn, lambda, &[atom])
            .await
            .expect("batch_load_weights");
        assert!(map.contains_key(&atom), "weight row should exist");

        // Count weight_events rows directly.
        let count = conn
            .reader()
            .await
            .expect("reader")
            .count(SqlStatement::new(
                "SELECT COUNT(*) FROM weight_events WHERE namespace = ?1 AND atom_id = ?2 AND channel = 'ambient'",
                vec![SqlValue::Text(lambda.into()), SqlValue::Text(atom.to_string())],
            ))
            .await
            .expect("audit count");
        assert_eq!(count, 5, "expected 5 ambient weight_events rows");
    }

    // -------------------------------------------------------------------------
    // Test 2 — ceiling clamp
    // -------------------------------------------------------------------------
    #[tokio::test]
    async fn test_apply_weight_delta_clamps_at_ceiling() {
        let conn = make_backend();
        let lambda = "lambda:test";
        let atom = Uuid::new_v4();

        // Repeatedly push large positive deltas.
        for _ in 0..100 {
            apply_weight_delta(
                &conn,
                lambda,
                atom,
                5.0, // huge delta
                WeightChannel::GroundTruth,
                None,
                None,
            )
            .await
            .expect("apply_weight_delta should succeed");
        }

        let map = batch_load_weights(&conn, lambda, &[atom])
            .await
            .expect("batch_load_weights");
        let w = *map.get(&atom).expect("atom weight must exist");
        assert_eq!(
            w, WEIGHT_CEIL,
            "weight should be clamped at WEIGHT_CEIL={WEIGHT_CEIL}, got {w}"
        );
    }

    // -------------------------------------------------------------------------
    // Test 3 — namespace isolation
    // -------------------------------------------------------------------------
    #[tokio::test]
    async fn test_apply_weight_delta_namespace_isolation() {
        let conn = make_backend();
        let lambda_a = "lambda:a";
        let lambda_b = "lambda:b";
        let atom = Uuid::new_v4();

        apply_weight_delta(
            &conn,
            lambda_a,
            atom,
            0.5,
            WeightChannel::Explicit,
            None,
            None,
        )
        .await
        .expect("apply for lambda:a");

        // lambda:b should see nothing.
        let map_b = batch_load_weights(&conn, lambda_b, &[atom])
            .await
            .expect("batch_load for lambda:b");
        assert!(
            !map_b.contains_key(&atom),
            "lambda:b should not see lambda:a's weight"
        );

        // lambda:a should see the written weight.
        let map_a = batch_load_weights(&conn, lambda_a, &[atom])
            .await
            .expect("batch_load for lambda:a");
        assert!(
            map_a.contains_key(&atom),
            "lambda:a should see its own weight"
        );
    }

    // -------------------------------------------------------------------------
    // Test 4 — missing atoms are absent (not 1.0 rows) from batch_load result
    // -------------------------------------------------------------------------
    #[tokio::test]
    async fn test_batch_load_weights_missing_atoms_default() {
        let conn = make_backend();
        let lambda = "lambda:test";
        let atom_a = Uuid::new_v4();
        let atom_b = Uuid::new_v4();
        let atom_c = Uuid::new_v4();

        // Write only atom_a.
        apply_weight_delta(
            &conn,
            lambda,
            atom_a,
            0.3,
            WeightChannel::Explicit,
            None,
            None,
        )
        .await
        .expect("apply for atom_a");

        let map = batch_load_weights(&conn, lambda, &[atom_a, atom_b, atom_c])
            .await
            .expect("batch_load");

        assert!(map.contains_key(&atom_a), "atom_a should be present");
        assert!(
            !map.contains_key(&atom_b),
            "atom_b should be absent (caller treats as 1.0)"
        );
        assert!(
            !map.contains_key(&atom_c),
            "atom_c should be absent (caller treats as 1.0)"
        );

        // atom_a weight should be non-default (was boosted).
        let w_a = *map.get(&atom_a).unwrap();
        assert!(w_a != 1.0_f32, "atom_a weight should differ from default");
    }

    // -------------------------------------------------------------------------
    // Test 5 — negative delta writes a weight_events row (B4 regression guard)
    // -------------------------------------------------------------------------
    /// Verifies that apply_weight_delta writes to weight_events even when the
    /// delta is negative, guarding against the B4 Channel-A decay skip bug.
    #[tokio::test]
    async fn test_channel_a_applies_on_negative_delta() {
        let conn = make_backend();
        let lambda = "lambda:test";
        let atom = Uuid::new_v4();

        // First boost the atom so it is above floor.
        apply_weight_delta(
            &conn,
            lambda,
            atom,
            0.5,
            WeightChannel::GroundTruth,
            None,
            None,
        )
        .await
        .expect("initial boost");

        // Now apply a negative ambient delta (simulates decay).
        let (w_after, _) = apply_weight_delta(
            &conn,
            lambda,
            atom,
            -0.1,
            WeightChannel::Ambient,
            None,
            Some("decay_test"),
        )
        .await
        .expect("negative delta must succeed");

        // Weight must be below the post-boost value (started ~1.25, decay should lower it).
        assert!(
            w_after < 1.5,
            "weight should have decayed below post-boost value, got {w_after}"
        );
        assert!(
            w_after >= WEIGHT_FLOOR,
            "weight must not go below WEIGHT_FLOOR, got {w_after}"
        );

        // Confirm a weight_events row was written for the negative delta.
        let count = conn
            .reader()
            .await
            .expect("reader")
            .count(SqlStatement::new(
                "SELECT COUNT(*) FROM weight_events \
                 WHERE namespace = ?1 AND atom_id = ?2 AND delta < 0",
                vec![
                    SqlValue::Text(lambda.into()),
                    SqlValue::Text(atom.to_string()),
                ],
            ))
            .await
            .expect("negative audit count");
        assert_eq!(
            count, 1,
            "expected 1 weight_event row with negative delta, got {count}"
        );
    }

    // -------------------------------------------------------------------------
    // Test 6 — empty namespace returns Validation error (F2 guard)
    // -------------------------------------------------------------------------
    #[tokio::test]
    async fn test_apply_weight_delta_rejects_empty_namespace() {
        let conn = make_backend();
        let atom = Uuid::new_v4();
        let result =
            apply_weight_delta(&conn, "", atom, 0.1, WeightChannel::Ambient, None, None).await;
        assert!(
            matches!(result, Err(PersistError::Validation(_))),
            "expected Validation error for empty namespace, got {result:?}"
        );
    }

    // -------------------------------------------------------------------------
    // Test 7 — NaN delta returns Validation error (prevents DB corruption)
    // -------------------------------------------------------------------------
    #[tokio::test]
    async fn test_apply_weight_delta_rejects_nan_delta() {
        let conn = make_backend();
        let lambda = "lambda:test";
        let atom = Uuid::new_v4();

        let result = apply_weight_delta(
            &conn,
            lambda,
            atom,
            f32::NAN,
            WeightChannel::Ambient,
            None,
            None,
        )
        .await;
        assert!(
            matches!(result, Err(PersistError::Validation(_))),
            "NaN delta must be rejected with Validation error, got {result:?}"
        );
    }

    // -------------------------------------------------------------------------
    // Test 8 — Inf delta returns Validation error
    // -------------------------------------------------------------------------
    #[tokio::test]
    async fn test_apply_weight_delta_rejects_inf_delta() {
        let conn = make_backend();
        let lambda = "lambda:test";
        let atom = Uuid::new_v4();

        let result = apply_weight_delta(
            &conn,
            lambda,
            atom,
            f32::INFINITY,
            WeightChannel::Ambient,
            None,
            None,
        )
        .await;
        assert!(
            matches!(result, Err(PersistError::Validation(_))),
            "Inf delta must be rejected with Validation error, got {result:?}"
        );
    }
}
