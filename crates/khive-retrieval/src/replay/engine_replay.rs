//! Historical weight reconstruction, rank history, and drift metrics.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, NaiveDate, Utc};
use parking_lot::Mutex;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::persist::PersistError as EngineError;
use crate::weights::WEIGHT_FLOOR;

/// Per-day weight-event counts; loaded here, outside the `metrics` module, because
/// static SQL loaders are resolved at a scope without glob imports.
/// SQLite: integer division gives the day bucket (micros / 86_400_000_000).
const WEIGHT_EVENTS_COUNT_BY_DAY_SQL: &str =
    include_str!("../../sql/weight_events_count_by_day.sql");
// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Per-atom weight change record in chronological order.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RankHistoryPoint {
    /// Timestamp of the adjustment (UTC).
    pub ts: DateTime<Utc>,
    /// Weight after this adjustment was applied.
    pub weight_after: f32,
    /// Raw delta that was applied.
    pub delta: f32,
    /// Channel that emitted this adjustment (`ambient`, `explicit`, `ground_truth`).
    pub channel: String,
    /// Optional context identifier carried by the caller.
    pub context_id: Option<String>,
    /// Optional brain_events UUID that triggered this adjustment.
    pub event_id: Option<Uuid>,
}

/// Diff report comparing two temporal rank lists for the same query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffReport {
    /// Jaccard similarity: |A ∩ B| / |A ∪ B|.
    pub jaccard: f32,
    /// Atoms present in the t2 result but absent from t1.
    pub added: Vec<Uuid>,
    /// Atoms present in the t1 result but absent from t2.
    pub dropped: Vec<Uuid>,
    /// Per-atom rank change from t1 → t2 (negative = moved up).
    pub rank_deltas: Vec<(Uuid, i32)>,
    /// Ordered top-K atom IDs at t1.
    pub top_k_at_t1: Vec<Uuid>,
    /// Ordered top-K atom IDs at t2.
    pub top_k_at_t2: Vec<Uuid>,
}

/// Report comparing a stored compose event's original top_atoms against
/// the same query re-run with current weights.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegressionReport {
    /// Brain-events row UUID that was replayed.
    pub event_id: Uuid,
    /// Query text recorded at compose time (empty string if none).
    pub query_text: String,
    /// Ordered atom list stored in the original compose event.
    pub original_top_atoms: Vec<Uuid>,
    /// Ordered atom list from re-running the query with current weights.
    pub current_top_atoms: Vec<Uuid>,
    /// Jaccard similarity between the two lists.
    pub jaccard: f32,
    /// Atoms present in current but absent from original.
    pub added: Vec<Uuid>,
    /// Atoms present in original but absent from current.
    pub dropped: Vec<Uuid>,
    /// UTC timestamp when the original compose event was recorded.
    pub timestamp_original: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// weights_as_of
// ---------------------------------------------------------------------------

/// Reconstruct the weight state for a lambda at a given point in time.
///
/// For each (lambda_id, atom_id) pair, selects the latest `weight_events` row
/// with `ts ≤ at_time` and returns `weight_after`.  Atoms with no history
/// before `at_time` are absent from the map; callers should treat absence as
/// the implicit default of 1.0.
///
/// # SQL
///
/// ```sql
/// SELECT atom_id, weight_after
/// FROM (
///     SELECT atom_id, weight_after,
///            ROW_NUMBER() OVER (PARTITION BY atom_id ORDER BY ts DESC) as rn
///     FROM weight_events
///     WHERE lambda_id = ?1 AND ts <= ?2
/// )
/// WHERE rn = 1
/// ```
pub async fn weights_as_of(
    conn: &Arc<Mutex<Connection>>,
    namespace: &str,
    at_time: DateTime<Utc>,
) -> Result<HashMap<Uuid, f32>, EngineError> {
    let conn = Arc::clone(conn);
    let namespace_str = namespace.to_string();
    let at_time_us = at_time.timestamp_micros();

    tokio::task::spawn_blocking(move || {
        let conn = conn.lock();
        let mut result = HashMap::new();

        let mut stmt = conn
            .prepare(include_str!("../../sql/weight_events_latest_as_of.sql"))
            .map_err(|e| EngineError::Internal(format!("weights_as_of prepare: {e}")))?;

        let mut rows = stmt
            .query(params![namespace_str, at_time_us])
            .map_err(|e| EngineError::Internal(format!("weights_as_of query: {e}")))?;

        while let Some(row) = rows
            .next()
            .map_err(|e| EngineError::Internal(format!("weights_as_of row: {e}")))?
        {
            let atom_id_str: String = row
                .get(0)
                .map_err(|e| EngineError::Internal(format!("weights_as_of col 0: {e}")))?;
            let weight_after: f64 = row
                .get(1)
                .map_err(|e| EngineError::Internal(format!("weights_as_of col 1: {e}")))?;

            if let Ok(uuid) = atom_id_str.parse::<Uuid>() {
                let clamped =
                    (weight_after as f32).clamp(WEIGHT_FLOOR, crate::weights::WEIGHT_CEIL);
                result.insert(uuid, clamped);
            }
        }

        Ok(result)
    })
    .await
    .map_err(|e| EngineError::Internal(format!("weights_as_of join: {e}")))?
}

/// Build a [`DiffReport`] from two ordered atom lists.
#[cfg(test)]
fn compute_diff_report(top_k_at_t1: Vec<Uuid>, top_k_at_t2: Vec<Uuid>) -> DiffReport {
    use std::collections::HashSet;

    let set_t1: HashSet<Uuid> = top_k_at_t1.iter().copied().collect();
    let set_t2: HashSet<Uuid> = top_k_at_t2.iter().copied().collect();

    let intersection_size = set_t1.intersection(&set_t2).count();
    let union_size = set_t1.union(&set_t2).count();
    let jaccard = if union_size == 0 {
        1.0_f32
    } else {
        intersection_size as f32 / union_size as f32
    };

    let added: Vec<Uuid> = set_t2.difference(&set_t1).copied().collect();
    let dropped: Vec<Uuid> = set_t1.difference(&set_t2).copied().collect();

    // Build rank maps (0-indexed).
    let rank_t1: HashMap<Uuid, usize> = top_k_at_t1
        .iter()
        .enumerate()
        .map(|(i, &id)| (id, i))
        .collect();
    let rank_t2: HashMap<Uuid, usize> = top_k_at_t2
        .iter()
        .enumerate()
        .map(|(i, &id)| (id, i))
        .collect();

    // Rank deltas only for atoms present in both.
    let rank_deltas: Vec<(Uuid, i32)> = set_t1
        .intersection(&set_t2)
        .filter_map(|&id| {
            let r1 = *rank_t1.get(&id)?;
            let r2 = *rank_t2.get(&id)?;
            Some((id, r2 as i32 - r1 as i32))
        })
        .collect();

    DiffReport {
        jaccard,
        added,
        dropped,
        rank_deltas,
        top_k_at_t1,
        top_k_at_t2,
    }
}

// ---------------------------------------------------------------------------
// rank_history
// ---------------------------------------------------------------------------

/// Return the full weight-change history for a single (namespace, atom_id) pair
/// in ascending timestamp order.
///
/// Useful for answering "why did this atom's rank change?" — each row captures
/// the delta, resulting weight, channel, and optional originating context/event.
pub async fn rank_history(
    conn: &Arc<Mutex<Connection>>,
    namespace: &str,
    atom_id: Uuid,
) -> Result<Vec<RankHistoryPoint>, EngineError> {
    let conn = Arc::clone(conn);
    let namespace_str = namespace.to_string();
    let atom_id_str = atom_id.to_string();

    tokio::task::spawn_blocking(move || {
        let conn = conn.lock();
        let mut stmt = conn
            .prepare(include_str!("../../sql/weight_events_history_by_atom.sql"))
            .map_err(|e| EngineError::Internal(format!("rank_history prepare: {e}")))?;

        let rows = stmt
            .query_map(params![namespace_str, atom_id_str], |row| {
                let ts_us: i64 = row.get(0)?;
                let weight_after: f64 = row.get(1)?;
                let delta: f64 = row.get(2)?;
                let channel: String = row.get(3)?;
                let context_id: Option<String> = row.get(4)?;
                let event_id_str: Option<String> = row.get(5)?;
                Ok((
                    ts_us,
                    weight_after,
                    delta,
                    channel,
                    context_id,
                    event_id_str,
                ))
            })
            .map_err(|e| EngineError::Internal(format!("rank_history query: {e}")))?;

        let mut points = Vec::new();
        for row in rows {
            let (ts_us, weight_after, delta, channel, context_id, event_id_str) =
                row.map_err(|e| EngineError::Internal(format!("rank_history row: {e}")))?;

            let ts = DateTime::from_timestamp_micros(ts_us).unwrap_or_else(Utc::now);

            let event_id = event_id_str.and_then(|s| s.parse::<Uuid>().ok());

            points.push(RankHistoryPoint {
                ts,
                weight_after: weight_after as f32,
                delta: delta as f32,
                channel,
                context_id,
                event_id,
            });
        }

        Ok(points)
    })
    .await
    .map_err(|e| EngineError::Internal(format!("rank_history join: {e}")))?
}

// ---------------------------------------------------------------------------
// Drift Metrics
// ---------------------------------------------------------------------------

/// Drift metrics for the Three Observables feedback loop.
pub mod metrics {
    use super::*;

    /// M2: Rank variance for an atom across all compose events where it appeared.
    ///
    /// High variance = context-sensitive atom; low variance = reliably ranked.
    /// Variance is computed over the 0-indexed rank positions in `top_atoms`
    /// arrays stored in `brain_events.payload`.
    ///
    /// Returns 0.0 when the atom has appeared in fewer than 2 events.
    pub async fn atom_rank_variance(
        conn: &Arc<Mutex<Connection>>,
        namespace: &str,
        atom_id: Uuid,
    ) -> Result<f32, EngineError> {
        let conn = Arc::clone(conn);
        let atom_id_str = atom_id.to_string();
        let namespace_str = namespace.to_string();

        tokio::task::spawn_blocking(move || {
            let c = conn.lock();
            let mut stmt = c
                .prepare("SELECT payload FROM brain_events WHERE kind = 'ComposeEvent' AND json_extract(payload, '$.lambda_id') = ?1")
                .map_err(|e| EngineError::Internal(format!("atom_rank_variance prepare: {e}")))?;

            let rows = stmt
                .query_map(params![namespace_str], |row| row.get::<_, String>(0))
                .map_err(|e| EngineError::Internal(format!("atom_rank_variance query: {e}")))?;

            let mut ranks: Vec<f32> = Vec::new();
            for row in rows.filter_map(|r| r.ok()) {
                let payload: serde_json::Value =
                    serde_json::from_str(&row).unwrap_or(serde_json::json!({}));
                if let Some(top_atoms) = payload.get("top_atoms").and_then(|v| v.as_array()) {
                    if let Some(pos) = top_atoms
                        .iter()
                        .position(|v| v.as_str() == Some(&atom_id_str))
                    {
                        ranks.push(pos as f32);
                    }
                }
            }

            if ranks.len() < 2 {
                return Ok(0.0_f32);
            }

            let mean = ranks.iter().sum::<f32>() / ranks.len() as f32;
            let variance =
                ranks.iter().map(|r| (r - mean).powi(2)).sum::<f32>() / ranks.len() as f32;
            Ok(variance)
        })
        .await
        .map_err(|e| EngineError::Internal(format!("atom_rank_variance join: {e}")))?
    }

    /// M3: Count of weight_events per calendar day over the last `days` days.
    ///
    /// A sudden spike in adjustment rate signals a potential runaway feedback loop.
    /// Returns a vec of `(NaiveDate, count)` sorted by date ascending.
    pub async fn adjustment_rate_per_day(
        conn: &Arc<Mutex<Connection>>,
        namespace: &str,
        days: u32,
    ) -> Result<Vec<(NaiveDate, u64)>, EngineError> {
        let conn = Arc::clone(conn);
        let namespace_str = namespace.to_string();
        let days_i64 = days as i64;

        tokio::task::spawn_blocking(move || {
            let c = conn.lock();
            let cutoff_us = (Utc::now() - chrono::Duration::days(days_i64)).timestamp_micros();

            let mut stmt = c.prepare(WEIGHT_EVENTS_COUNT_BY_DAY_SQL).map_err(|e| {
                EngineError::Internal(format!("adjustment_rate_per_day prepare: {e}"))
            })?;

            let rows = stmt
                .query_map(params![namespace_str, cutoff_us], |row| {
                    let day_bucket: i64 = row.get(0)?;
                    let cnt: i64 = row.get(1)?;
                    Ok((day_bucket, cnt as u64))
                })
                .map_err(|e| {
                    EngineError::Internal(format!("adjustment_rate_per_day query: {e}"))
                })?;

            let mut result = Vec::new();
            for row in rows.filter_map(|r| r.ok()) {
                let (day_bucket, cnt) = row;
                // day_bucket = days since Unix epoch.
                // NaiveDate::from_num_days_from_ce expects days from year 1, so offset.
                // Unix epoch (1970-01-01) = day 719_163 in from_num_days_from_ce.
                const UNIX_EPOCH_CE_DAYS: i32 = 719_163;
                let date =
                    NaiveDate::from_num_days_from_ce_opt(UNIX_EPOCH_CE_DAYS + day_bucket as i32)
                        .unwrap_or(NaiveDate::from_ymd_opt(1970, 1, 1).unwrap());
                result.push((date, cnt));
            }

            Ok(result)
        })
        .await
        .map_err(|e| EngineError::Internal(format!("adjustment_rate_per_day join: {e}")))?
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[path = "engine_replay/tests.rs"]
mod tests;
