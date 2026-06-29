// FILE SIZE JUSTIFICATION: all five replay primitives (weights_as_of, replay, diff,
// rank_history, regression_check) share a single SQLite connection type and the same
// weight_events schema; splitting them would duplicate schema definitions and connection
// wiring. The drift-metrics sub-functions (jaccard_stability_7d, atom_rank_variance,
// adjustment_rate_per_day) are tightly coupled to the same table and cannot be moved
// without duplicating the SQL helpers. Co-location is intentional.

//! Temporal replay APIs: reconstruct past weight state and diff against present.

// REASON: the `engine` feature is a future integration point (EmbeddedEngine not yet ported);
// the cfg is intentionally undeclared so the gate never activates during normal builds.
#![allow(unexpected_cfgs)]

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use chrono::{DateTime, NaiveDate, Utc};
use parking_lot::Mutex;
#[cfg(feature = "engine")]
use rusqlite::OptionalExtension as _;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::persist::PersistError as EngineError;
use crate::weights::WEIGHT_FLOOR;
// TODO(port-engine): EmbeddedEngine not yet in khive-retrieval scope; stub for compilation.
// Tracked: port blocked on khive-inference crate landing.
// REASON: type alias is referenced by `#[cfg(feature = "engine")]` items that are not compiled by default
#[allow(dead_code)]
type EmbeddedEngine = ();

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
            .prepare(
                "SELECT atom_id, weight_after
                 FROM (
                     SELECT atom_id, weight_after,
                            ROW_NUMBER() OVER (PARTITION BY atom_id ORDER BY ts DESC) as rn
                     FROM weight_events
                     WHERE namespace = ?1 AND ts <= ?2
                 )
                 WHERE rn = 1",
            )
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

// ---------------------------------------------------------------------------
// Namespace isolation helper (B1 fix — must come before replay)
// ---------------------------------------------------------------------------

/// Return the subset of `candidate_ids` whose atoms are owned by `namespace`.
///
/// Queries `atoms WHERE namespace = ?1 AND id IN (?) AND deleted_at IS NULL`.
/// Preserves no particular order — the caller re-orders by HNSW rank after
/// filtering.
///
/// The in-memory HNSW snapshot is global (it indexes all atoms regardless of
/// namespace).  Without this post-filter, `replay()` would return atoms from
/// any namespace that happen to be semantically close to the query, leaking
/// cross-tenant atom UUIDs to the requesting lambda.
// REASON: called only from the `#[cfg(feature = "engine")]` replay() function; without the
// feature gate the caller is compiled out, making this function appear dead to rustc.
#[allow(dead_code)]
fn filter_atoms_by_namespace(
    conn: &Connection,
    namespace: &str,
    candidate_ids: &[Uuid],
) -> Result<HashSet<Uuid>, EngineError> {
    if candidate_ids.is_empty() {
        return Ok(HashSet::new());
    }

    // Build the IN clause with per-item placeholders.
    // SQLITE_SAFE_BIND_LIMIT is 999; candidate_k is at most top_k*4 (≤400 for top_k=100).
    let placeholders: Vec<String> = candidate_ids.iter().map(|_| "?".to_string()).collect();
    let sql = format!(
        "SELECT id FROM atoms WHERE namespace = ? AND id IN ({}) AND deleted_at IS NULL",
        placeholders.join(", ")
    );

    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| EngineError::Internal(format!("filter_atoms_by_namespace prepare: {e}")))?;

    // Collect all bind values as Strings so they have a uniform owned type.
    // namespace goes first, then the UUID strings for the IN clause.
    let id_strings: Vec<String> = candidate_ids.iter().map(|u| u.to_string()).collect();
    let all_values: Vec<&str> = std::iter::once(namespace)
        .chain(id_strings.iter().map(|s| s.as_str()))
        .collect();

    let rows = stmt
        .query_map(rusqlite::params_from_iter(all_values.iter()), |row| {
            row.get::<_, String>(0)
        })
        .map_err(|e| EngineError::Internal(format!("filter_atoms_by_namespace query: {e}")))?;

    let owned: HashSet<Uuid> = rows
        .filter_map(|r| r.ok())
        .filter_map(|s| s.parse::<Uuid>().ok())
        .collect();

    Ok(owned)
}

// TODO(port-engine): replay, diff, regression_check, load_brain_event, and
// jaccard_stability_7d require EmbeddedEngine which is not yet ported to
// khive-retrieval scope. Gated behind "engine" feature until ported.
#[cfg(feature = "engine")]
/// When `weight_override` is `Some(map)`, each atom's raw similarity score is
/// multiplied by the weight from the map (absent atoms default to 1.0).  When
/// `None`, current `atom_weights` rows are used via `batch_load_weights`.
///
/// Returns atom IDs in descending weighted-score order.
pub async fn replay(
    engine: &EmbeddedEngine,
    namespace: &str,
    query_text: &str,
    at_time: Option<DateTime<Utc>>,
    top_k: usize,
) -> Result<Vec<Uuid>, EngineError> {
    // Step 1: embed the query.
    let query_vec = engine
        .embed_query(query_text)
        .await
        .map_err(|e| EngineError::Embedding(format!("replay embed: {e}")))?;

    // Step 2: vector search via HNSW for a broad candidate set.
    // Do this first so we know which atom IDs to load weights for.
    let candidate_k = (top_k * 4).max(20);
    let raw_results = engine
        .search_by_vector(&query_vec, candidate_k)
        .await
        .map_err(|e| EngineError::Retrieval(format!("replay search: {e}")))?;

    // Step 2b (B1 fix): filter to atoms owned by this lambda's namespace.
    //
    // The HNSW snapshot is global — it contains atoms from every namespace
    // stored in this engine instance.  Without this filter, `replay()` would
    // leak cross-tenant atom UUIDs into the ranked result (they default to
    // weight 1.0 when absent from the weight map, potentially outranking the
    // requesting lambda's own down-weighted atoms).
    //
    // A single engine instance may serve multiple lambdas whose atoms co-exist
    // in SQLite but whose HNSW vectors are interleaved.
    let raw_results = {
        let conn_guard = engine.store().conn();
        let c = conn_guard.lock();
        let all_candidate_ids: Vec<Uuid> = raw_results.iter().map(|h| h.id).collect();
        let owned = filter_atoms_by_namespace(&c, namespace, &all_candidate_ids)?;
        // Re-filter raw_results (preserving HNSW rank order).
        raw_results
            .into_iter()
            .filter(|h| owned.contains(&h.id))
            .collect::<Vec<_>>()
    };

    // Step 3: resolve weights for the candidate atom IDs.
    // Propagate DB errors instead of silently falling back to all-1.0 weights:
    // unwrap_or_default here would silently change rankings whenever the DB is
    // temporarily unavailable, making drift look like genuine weight changes.
    let candidate_ids: Vec<Uuid> = raw_results.iter().map(|h| h.id).collect();
    let weights: HashMap<Uuid, f32> = match at_time {
        Some(t) => weights_as_of(&engine.store().conn(), namespace, t).await?,
        None => {
            crate::weights::batch_load_weights(&engine.store().conn(), namespace, &candidate_ids)
                .await?
        }
    };

    // Step 4: apply weight multiplier.
    let mut scored: Vec<(Uuid, f32)> = raw_results
        .into_iter()
        .map(|hit| {
            let w = weights.get(&hit.id).copied().unwrap_or(1.0_f32);
            (hit.id, hit.score * w)
        })
        .collect();

    // Step 5: sort descending and truncate.
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(top_k);

    Ok(scored.into_iter().map(|(id, _)| id).collect())
}

/// Build a [`DiffReport`] from two ordered atom lists.
// REASON: called only from the `#[cfg(feature = "engine")]` diff() function which is not yet active.
#[allow(dead_code)]
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
// diff — engine-dependent, gated
// ---------------------------------------------------------------------------

/// Compute the diff between two temporal replays of the same query.
#[cfg(feature = "engine")]
pub async fn diff(
    engine: &EmbeddedEngine,
    namespace: &str,
    query_text: &str,
    t1: DateTime<Utc>,
    t2: DateTime<Utc>,
    top_k: usize,
) -> Result<DiffReport, EngineError> {
    let (top_k_at_t1, top_k_at_t2) = tokio::try_join!(
        replay(engine, namespace, query_text, Some(t1), top_k),
        replay(engine, namespace, query_text, Some(t2), top_k),
    )?;
    Ok(compute_diff_report(top_k_at_t1, top_k_at_t2))
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
            .prepare(
                "SELECT ts, weight_after, delta, channel, context_id, event_id
                 FROM weight_events
                 WHERE namespace = ?1 AND atom_id = ?2
                 ORDER BY ts ASC",
            )
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
// regression_check — engine-dependent, gated
// ---------------------------------------------------------------------------

/// Re-run the query from a stored compose event against current weights.
#[cfg(feature = "engine")]
pub async fn regression_check(
    engine: &EmbeddedEngine,
    event_id: Uuid,
) -> Result<RegressionReport, EngineError> {
    // Step 1: load brain_events row.
    // load_brain_event now returns InvalidData on malformed payload (B4 fix)
    // and includes the stored embedding_model for validation (B5 fix).
    let (query_text, original_top_atoms, namespace, created_at_us, stored_model) =
        load_brain_event(engine, event_id).await?;

    // Step 2 (B5 fix): validate embedding model compatibility.
    //
    // If the stored row recorded an embedding_model AND it differs from the
    // engine's current model, the query re-embedding would produce a vector in
    // a different space, making the resulting Jaccard meaningless.  We surface
    // this as a distinct error so callers can skip or re-embed rather than
    // silently reporting catastrophic drift.
    //
    // Legacy rows (stored_model == None, i.e. pre-Phase-2 events) are accepted
    // with a warning — we cannot validate compatibility but also cannot reject
    // all historical data.
    if let Some(ref stored) = stored_model {
        let current = engine.embedding_model();
        if stored != current {
            return Err(EngineError::IncompatibleEmbeddingModel {
                stored: stored.clone(),
                current: current.to_string(),
            });
        }
    } else {
        tracing::warn!(
            event_id = %event_id,
            "regression_check: brain_events row has no embedding_model (legacy row); \
             proceeding without model compatibility check"
        );
    }

    // Step 3: replay with current weights.
    let current_top_atoms = replay(
        engine,
        &namespace,
        &query_text,
        None, // current weights
        original_top_atoms.len().max(10),
    )
    .await?;

    // Step 4: compute Jaccard.
    let report = compute_diff_report(original_top_atoms.clone(), current_top_atoms.clone());

    let timestamp_original =
        DateTime::from_timestamp_micros(created_at_us).unwrap_or_else(Utc::now);

    Ok(RegressionReport {
        event_id,
        query_text,
        original_top_atoms,
        current_top_atoms,
        jaccard: report.jaccard,
        added: report.added,
        dropped: report.dropped,
        timestamp_original,
    })
}

/// Load a brain_events row and extract replay inputs. (engine-gated)
#[cfg(feature = "engine")]
async fn load_brain_event(
    engine: &EmbeddedEngine,
    event_id: Uuid,
) -> Result<(String, Vec<Uuid>, String, i64, Option<String>), EngineError> {
    // Use the legacy conn() path (Arc<Mutex<Connection>>) which is Send + Clone.
    let conn = engine.store().conn();
    let event_id_str = event_id.to_string();

    tokio::task::spawn_blocking(move || {
        let conn = conn.lock();
        let guard = &*conn;

        // Select query_text, payload (top_atoms lives in payload JSON),
        // actor_id (our namespace proxy), created_at, and the embedding_model
        // column added in migration v27.
        let result = guard
            .query_row(
                "SELECT query_text, payload, actor_id, created_at, embedding_model
                 FROM brain_events WHERE id = ?1",
                params![event_id_str.clone()],
                |row| {
                    let query_text: Option<String> = row.get(0)?;
                    let payload_str: String = row.get(1)?;
                    let actor_id: Option<String> = row.get(2)?;
                    let created_at: i64 = row.get(3)?;
                    let embedding_model: Option<String> = row.get(4)?;
                    Ok((
                        query_text,
                        payload_str,
                        actor_id,
                        created_at,
                        embedding_model,
                    ))
                },
            )
            .optional()
            .map_err(|e| EngineError::Internal(format!("load_brain_event query: {e}")))?;

        let (query_text_opt, payload_str, actor_id_opt, created_at, stored_model) = result
            .ok_or_else(|| {
                EngineError::NotFound(format!("brain_events row not found: {event_id}"))
            })?;

        let query_text = query_text_opt.unwrap_or_default();

        // B4 fix: propagate JSON parse errors instead of silently substituting
        // `{}`, which would cause `top_atoms` to be empty and `regression_check`
        // to report false 100% drift.
        let payload: serde_json::Value = serde_json::from_str(&payload_str).map_err(|e| {
            EngineError::InvalidData(format!(
                "brain_events row {event_id} has unparseable payload JSON: {e}"
            ))
        })?;

        // top_atoms in payload is an array of UUID strings.
        let top_atoms: Vec<Uuid> = payload
            .get("top_atoms")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().and_then(|s| s.parse::<Uuid>().ok()))
                    .collect()
            })
            .unwrap_or_default();

        // B4 fix (continued): an empty top_atoms list is also invalid data —
        // it would produce a trivially-true jaccard=0 without indicating real drift.
        if top_atoms.is_empty() {
            return Err(EngineError::InvalidData(format!(
                "brain_events row {event_id} has missing or empty top_atoms in payload"
            )));
        }

        // namespace from payload field (most reliable) or actor_id column.
        // Note: stored payload uses key "lambda_id" (legacy; kept for DB compat).
        let namespace = payload
            .get("lambda_id")
            .or_else(|| payload.get("namespace"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .or(actor_id_opt)
            .unwrap_or_default();

        Ok((query_text, top_atoms, namespace, created_at, stored_model))
    })
    .await
    .map_err(|e| EngineError::Internal(format!("load_brain_event join: {e}")))?
}

// ---------------------------------------------------------------------------
// Drift Metrics
// ---------------------------------------------------------------------------

/// Drift metrics for the Three Observables feedback loop.
pub mod metrics {
    use super::*;

    /// M1: Rolling 7-day median Jaccard stability. (engine-gated)
    #[cfg(feature = "engine")]
    pub async fn jaccard_stability_7d(
        engine: &EmbeddedEngine,
        namespace: &str,
    ) -> Result<f32, EngineError> {
        let conn = engine.store().conn();
        // brain_events.payload stores namespace under legacy key "lambda_id" (#2536).
        // The JSON key cannot be renamed without a data migration; the column name
        // was already `namespace` in v25 when the table was created.
        let namespace_str = namespace.to_string();

        // Collect event IDs from the last 7 days where actor_id matches namespace.
        let event_ids: Vec<Uuid> = {
            let conn = Arc::clone(&conn);
            tokio::task::spawn_blocking(move || {
                let c = conn.lock();
                let cutoff_us = (Utc::now() - chrono::Duration::days(7)).timestamp_micros();
                let mut stmt = c
                    .prepare(
                        "SELECT id FROM brain_events
                         WHERE kind = 'ComposeEvent'
                           AND created_at >= ?1
                           AND json_extract(payload, '$.lambda_id') = ?2
                         ORDER BY created_at DESC",
                    )
                    .map_err(|e| {
                        EngineError::Internal(format!("jaccard_stability_7d prepare: {e}"))
                    })?;

                let rows = stmt
                    .query_map(params![cutoff_us, namespace_str], |row| {
                        row.get::<_, String>(0)
                    })
                    .map_err(|e| {
                        EngineError::Internal(format!("jaccard_stability_7d query: {e}"))
                    })?;

                let ids: Vec<Uuid> = rows
                    .filter_map(|r| r.ok())
                    .filter_map(|s| s.parse::<Uuid>().ok())
                    .collect();
                Ok::<Vec<Uuid>, EngineError>(ids)
            })
            .await
            .map_err(|e| EngineError::Internal(format!("jaccard_stability_7d join: {e}")))??
        };

        if event_ids.is_empty() {
            return Ok(1.0);
        }

        // Run regression_check on each event; collect Jaccard values.
        let mut jaccards: Vec<f32> = Vec::new();
        for eid in event_ids {
            match regression_check(engine, eid).await {
                Ok(report) => jaccards.push(report.jaccard),
                Err(_) => {
                    // Non-fatal: skip events that fail to replay (e.g., empty query).
                    continue;
                }
            }
        }

        if jaccards.is_empty() {
            return Ok(1.0);
        }

        // Median (sort + mid point).
        jaccards.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let mid = jaccards.len() / 2;
        let median = if jaccards.len() % 2 == 0 {
            (jaccards[mid - 1] + jaccards[mid]) / 2.0
        } else {
            jaccards[mid]
        };

        Ok(median)
    }

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
                .prepare(
                    "SELECT payload FROM brain_events
                     WHERE kind = 'ComposeEvent'
                       AND json_extract(payload, '$.lambda_id') = ?1",
                )
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

            let mut stmt = c
                .prepare(
                    // SQLite: integer division gives day bucket (micros / 86_400_000_000).
                    "SELECT ts / 86400000000 AS day_bucket, COUNT(*) as cnt
                     FROM weight_events
                     WHERE namespace = ?1 AND ts >= ?2
                     GROUP BY day_bucket
                     ORDER BY day_bucket ASC",
                )
                .map_err(|e| {
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
mod tests {
    use super::*;

    fn make_conn() -> Arc<Mutex<Connection>> {
        let conn = Connection::open_in_memory().expect("open in-memory db");
        conn.execute_batch(
            r#"
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
        .expect("init replay test schema");
        Arc::new(Mutex::new(conn))
    }

    fn insert_weight_event(
        conn: &Arc<Mutex<Connection>>,
        namespace: &str,
        atom_id: &str,
        weight_after: f32,
        ts_us: i64,
    ) {
        let c = conn.lock();
        c.execute(
            "INSERT INTO weight_events (namespace, atom_id, delta, weight_after, channel, eta, ts)
             VALUES (?1, ?2, 0.1, ?3, 'explicit', 0.1, ?4)",
            params![namespace, atom_id, weight_after as f64, ts_us],
        )
        .expect("insert weight_event");
    }

    #[tokio::test]
    async fn test_weights_as_of_returns_snapshot_at_time() {
        let conn = make_conn();
        let lambda = "lambda:test";
        let atom = Uuid::new_v4();
        let atom_str = atom.to_string();

        // t0: weight 1.5
        let t0_us: i64 = 1_000_000_000;
        insert_weight_event(&conn, lambda, &atom_str, 1.5, t0_us);

        // t1: weight 2.5 (later)
        let t1_us: i64 = 2_000_000_000;
        insert_weight_event(&conn, lambda, &atom_str, 2.5, t1_us);

        // Query at t0 + 1: should see 1.5.
        let at_t0 = DateTime::from_timestamp_micros(t0_us + 1).unwrap();
        let snapshot = weights_as_of(&conn, lambda, at_t0)
            .await
            .expect("weights_as_of");
        let w = *snapshot.get(&atom).expect("atom must be in snapshot");
        assert!((w - 1.5).abs() < 0.01, "expected 1.5 at t0, got {w}");

        // Query at t1 + 1: should see 2.5.
        let at_t1 = DateTime::from_timestamp_micros(t1_us + 1).unwrap();
        let snapshot2 = weights_as_of(&conn, lambda, at_t1)
            .await
            .expect("weights_as_of at t1");
        let w2 = *snapshot2
            .get(&atom)
            .expect("atom must be in snapshot at t1");
        assert!((w2 - 2.5).abs() < 0.01, "expected 2.5 at t1, got {w2}");
    }

    #[tokio::test]
    async fn test_weights_as_of_before_any_event_is_empty() {
        let conn = make_conn();
        let lambda = "lambda:test";
        let atom = Uuid::new_v4();
        let atom_str = atom.to_string();

        let t1_us: i64 = 2_000_000_000;
        insert_weight_event(&conn, lambda, &atom_str, 2.0, t1_us);

        // Query before t1: no rows.
        let before = DateTime::from_timestamp_micros(t1_us - 1).unwrap();
        let snapshot = weights_as_of(&conn, lambda, before)
            .await
            .expect("weights_as_of");
        assert!(
            snapshot.is_empty(),
            "snapshot before any event should be empty"
        );
    }

    #[tokio::test]
    async fn test_rank_history_returns_ordered_events() {
        let conn = make_conn();
        let lambda = "lambda:rank_hist";
        let atom = Uuid::new_v4();
        let atom_str = atom.to_string();

        insert_weight_event(&conn, lambda, &atom_str, 1.2, 1_000);
        insert_weight_event(&conn, lambda, &atom_str, 1.4, 2_000);
        insert_weight_event(&conn, lambda, &atom_str, 1.1, 3_000);

        let history = rank_history(&conn, lambda, atom)
            .await
            .expect("rank_history");

        assert_eq!(history.len(), 3, "expected 3 history points");
        // Verify ascending timestamp order.
        assert!(history[0].ts <= history[1].ts);
        assert!(history[1].ts <= history[2].ts);
        // Verify weights.
        assert!((history[0].weight_after - 1.2).abs() < 0.01);
        assert!((history[1].weight_after - 1.4).abs() < 0.01);
        assert!((history[2].weight_after - 1.1).abs() < 0.01);
    }

    #[test]
    fn test_compute_diff_report_jaccard() {
        let t1 = vec![
            Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap(),
            Uuid::parse_str("00000000-0000-0000-0000-000000000002").unwrap(),
            Uuid::parse_str("00000000-0000-0000-0000-000000000003").unwrap(),
        ];
        let t2 = vec![
            Uuid::parse_str("00000000-0000-0000-0000-000000000002").unwrap(),
            Uuid::parse_str("00000000-0000-0000-0000-000000000003").unwrap(),
            Uuid::parse_str("00000000-0000-0000-0000-000000000004").unwrap(),
        ];

        let report = compute_diff_report(t1, t2);

        // |intersection| = 2 ({002, 003}), |union| = 4
        assert!(
            (report.jaccard - 0.5).abs() < 0.01,
            "jaccard={}",
            report.jaccard
        );
        assert_eq!(report.added.len(), 1, "one atom added");
        assert_eq!(report.dropped.len(), 1, "one atom dropped");
    }

    #[test]
    fn test_compute_diff_report_identical() {
        let ids: Vec<Uuid> = (1..=3)
            .map(|i| Uuid::parse_str(&format!("00000000-0000-0000-0000-{:012}", i)).unwrap())
            .collect();

        let report = compute_diff_report(ids.clone(), ids);
        assert!((report.jaccard - 1.0).abs() < 0.001);
        assert!(report.added.is_empty());
        assert!(report.dropped.is_empty());
    }

    #[test]
    fn test_compute_diff_report_disjoint() {
        let t1 = vec![Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap()];
        let t2 = vec![Uuid::parse_str("00000000-0000-0000-0000-000000000002").unwrap()];

        let report = compute_diff_report(t1, t2);
        assert!((report.jaccard - 0.0).abs() < 0.001);
        assert_eq!(report.added.len(), 1);
        assert_eq!(report.dropped.len(), 1);
    }

    #[tokio::test]
    async fn test_adjustment_rate_per_day() {
        let conn = make_conn();
        let lambda = "lambda:rate_test";
        let atom = Uuid::new_v4();
        let atom_str = atom.to_string();

        // Insert 3 events: 2 "today" and 1 "yesterday".
        let now_us = Utc::now().timestamp_micros();
        let yesterday_us = now_us - 86_400_000_001_i64; // slightly over 24h ago

        insert_weight_event(&conn, lambda, &atom_str, 1.1, now_us - 100);
        insert_weight_event(&conn, lambda, &atom_str, 1.2, now_us - 50);
        insert_weight_event(&conn, lambda, &atom_str, 1.3, yesterday_us);

        let rates = metrics::adjustment_rate_per_day(&conn, lambda, 7)
            .await
            .expect("adjustment_rate_per_day");

        // At least 2 buckets (today and yesterday within 7 days).
        assert!(
            !rates.is_empty(),
            "expected at least 1 day bucket, got {:?}",
            rates
        );
        // Sum of all counts should be 3.
        let total: u64 = rates.iter().map(|(_, c)| c).sum();
        assert_eq!(total, 3, "expected 3 total events");
    }
}
