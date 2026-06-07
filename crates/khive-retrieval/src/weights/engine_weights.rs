//! EMA-based atom weight store with audit log for ambient, explicit, and ground-truth feedback.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use rusqlite::{params, Connection, OptionalExtension};
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
    conn: &Arc<Mutex<Connection>>,
    namespace: &str,
    atom_id: Uuid,
    delta: f32,
    channel: WeightChannel,
    event_id: Option<Uuid>,
    context_id: Option<&str>,
) -> Result<(f32, i64), PersistError> {
    apply_weight_delta_with_eta(
        conn,
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
    conn: &Arc<Mutex<Connection>>,
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
    let conn = Arc::clone(conn);
    let namespace_str = namespace.to_string();
    let atom_id_str = atom_id.to_string();
    let channel_str = channel.as_str();
    let event_id_str = event_id.map(|u| u.to_string());
    let context_id = context_id.map(|s| s.to_string());
    let now_us = chrono::Utc::now().timestamp_micros();

    tokio::task::spawn_blocking(move || {
        let conn = conn.lock();

        let tx =
            rusqlite::Transaction::new_unchecked(&conn, rusqlite::TransactionBehavior::Immediate)?;

        // Read current weight (default 1.0 if row absent).
        let old_weight: f32 = tx
            .query_row(
                "SELECT weight FROM atom_weights WHERE namespace = ?1 AND atom_id = ?2",
                params![namespace_str, atom_id_str],
                |row| row.get::<_, f64>(0),
            )
            .optional()
            .map_err(PersistError::from)?
            .unwrap_or(1.0_f64) as f32;

        // EMA update + clamp.
        let new_weight = (old_weight * (1.0 - eta) + delta).clamp(WEIGHT_FLOOR, WEIGHT_CEIL);

        // Upsert atom_weights — increment version on each write.
        tx.execute(
            "INSERT INTO atom_weights (namespace, atom_id, weight, updated_at, version)
             VALUES (?1, ?2, ?3, ?4, 1)
             ON CONFLICT(namespace, atom_id) DO UPDATE SET
               weight     = excluded.weight,
               updated_at = excluded.updated_at,
               version    = version + 1",
            params![namespace_str, atom_id_str, new_weight as f64, now_us],
        )?;

        // Append weight_events audit row.
        tx.execute(
            "INSERT INTO weight_events
               (namespace, atom_id, delta, weight_after, channel, eta, event_id, context_id, ts)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                namespace_str,
                atom_id_str,
                delta as f64,
                new_weight as f64,
                channel_str,
                eta as f64,
                event_id_str,
                context_id,
                now_us,
            ],
        )?;

        let row_id = tx.last_insert_rowid();
        tx.commit()?;

        Ok((new_weight, row_id))
    })
    .await?
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
    conn: &Arc<Mutex<Connection>>,
    namespace: &str,
    atom_ids: &[Uuid],
) -> Result<HashMap<Uuid, f32>, PersistError> {
    if atom_ids.is_empty() {
        return Ok(HashMap::new());
    }

    let conn = Arc::clone(conn);
    let namespace_str = namespace.to_string();
    // Convert UUIDs to strings once, then move into the blocking closure.
    let id_strs: Vec<String> = atom_ids.iter().map(|u| u.to_string()).collect();

    tokio::task::spawn_blocking(move || {
        let conn = conn.lock();
        let mut result = HashMap::with_capacity(id_strs.len());

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

            let sql = format!(
                "SELECT atom_id, weight FROM atom_weights \
                 WHERE namespace = ?1 AND atom_id IN ({placeholders})"
            );

            let mut stmt = conn.prepare(&sql).map_err(PersistError::from)?;

            let mut param_values: Vec<rusqlite::types::Value> = Vec::with_capacity(chunk.len() + 1);
            param_values.push(rusqlite::types::Value::Text(namespace_str.clone()));
            for s in chunk {
                param_values.push(rusqlite::types::Value::Text(s.clone()));
            }

            let mut rows = stmt
                .query(rusqlite::params_from_iter(param_values))
                .map_err(PersistError::from)?;

            while let Some(row) = rows.next().map_err(PersistError::from)? {
                let aid: String = row.get(0).map_err(PersistError::from)?;
                let w: f64 = row.get(1).map_err(PersistError::from)?;
                if let Ok(uuid) = aid.parse::<Uuid>() {
                    // Clamp on read — symmetric with write-side invariant. Protects compose
                    // from weight=0 rows introduced by manual SQL or future schema drift.
                    let clamped = (w as f32).clamp(WEIGHT_FLOOR, WEIGHT_CEIL);
                    result.insert(uuid, clamped);
                }
            }
        }

        Ok(result)
    })
    .await?
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn make_conn() -> Arc<Mutex<Connection>> {
        let conn = Connection::open_in_memory().expect("open in-memory db");
        conn.execute_batch(
            r#"
            CREATE TABLE atom_weights (
                namespace TEXT NOT NULL,
                atom_id TEXT NOT NULL,
                weight REAL NOT NULL,
                updated_at INTEGER NOT NULL,
                version INTEGER NOT NULL DEFAULT 1,
                PRIMARY KEY(namespace, atom_id)
            );
            CREATE TABLE weight_events (
                namespace TEXT NOT NULL,
                atom_id TEXT NOT NULL,
                delta REAL NOT NULL,
                weight_after REAL NOT NULL,
                channel TEXT NOT NULL,
                eta REAL NOT NULL,
                event_id TEXT,
                context_id TEXT,
                ts INTEGER NOT NULL
            );
            "#,
        )
        .expect("init weight test schema");
        Arc::new(Mutex::new(conn))
    }

    // -------------------------------------------------------------------------
    // Test 1 — ambient channel drives weight above 1.0 over 5 ticks
    // -------------------------------------------------------------------------
    #[tokio::test]
    async fn test_apply_weight_delta_ambient_channel() {
        let conn = make_conn();
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
        let count: i64 = {
            let c = conn.lock();
            c.query_row(
                "SELECT COUNT(*) FROM weight_events WHERE namespace = ?1 AND atom_id = ?2 AND channel = 'ambient'",
                params![lambda, atom.to_string()],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(count, 5, "expected 5 ambient weight_events rows");
    }

    // -------------------------------------------------------------------------
    // Test 2 — ceiling clamp
    // -------------------------------------------------------------------------
    #[tokio::test]
    async fn test_apply_weight_delta_clamps_at_ceiling() {
        let conn = make_conn();
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
        let conn = make_conn();
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
        let conn = make_conn();
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
        let conn = make_conn();
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
        let count: i64 = {
            let c = conn.lock();
            c.query_row(
                "SELECT COUNT(*) FROM weight_events \
                 WHERE namespace = ?1 AND atom_id = ?2 AND delta < 0",
                params![lambda, atom.to_string()],
                |r| r.get(0),
            )
            .unwrap()
        };
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
        let conn = make_conn();
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
        let conn = make_conn();
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
        let conn = make_conn();
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
