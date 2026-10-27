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
    /// η = 0.50 — an offline evaluation or a manual CLI trigger.
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
/// Use this when a runtime config loader supplies a `knowledge.toml`
/// override of Channel C's default 0.50. Same algorithm
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
#[path = "engine_weights/tests.rs"]
mod tests;
