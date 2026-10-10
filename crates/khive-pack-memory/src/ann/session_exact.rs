//! One-statement exact candidates and visibility proof for session recall.

use std::collections::HashSet;

use khive_runtime::KhiveRuntime;
use khive_storage::types::{SqlStatement, SqlValue};
use uuid::Uuid;

#[cfg(test)]
use super::SESSION_EXACT_STATEMENT_COUNT;
use super::{sanitize_model_key, session_fence_predicate, ANN_CONSUMER, ANN_WILDCARD_NS};

/// Return exact cosine candidates only when the same SQL statement also proves
/// the write fence. An original log row proves the un-compacted tail; this
/// consumer's active wildcard watermark proves a published segment covered a
/// compacted row. The exact scan and both proof alternatives share one SQLite
/// snapshot, so a separate preflight clock cannot certify later candidates.
/// Per-namespace exact scans are materialized before their union is ranked,
/// so a global top-k cannot hide candidates in another visible namespace.
pub(crate) async fn session_exact_candidates(
    rt: &KhiveRuntime,
    model: &str,
    query: &[f32],
    visible_namespaces: &[String],
    receipt_namespace: &str,
    required_seq: u64,
    k: usize,
) -> Result<Option<Vec<(Uuid, f32)>>, String> {
    let Ok(required_seq) = i64::try_from(required_seq) else {
        // SQLite rowids cannot reach this otherwise well-formed future fence.
        return Ok(None);
    };
    if query.is_empty() || query.iter().any(|component| !component.is_finite()) {
        return Err("session exact query vector is empty or non-finite".into());
    }

    let table_name = format!("vec_{}", sanitize_model_key(model));
    let query_blob = khive_storage::encode_f32_native(query);
    let mut params = vec![
        SqlValue::Blob(query_blob),
        SqlValue::Integer(i64::try_from(k).unwrap_or(i64::MAX)),
        SqlValue::Text(model.to_owned()),
        SqlValue::Integer(required_seq),
        SqlValue::Text(receipt_namespace.to_owned()),
        SqlValue::Text(ANN_CONSUMER.into()),
        SqlValue::Text(ANN_WILDCARD_NS.into()),
    ];
    let mut seen = HashSet::new();
    let mut knn_ctes = Vec::new();
    let mut union_arms = Vec::new();
    let scopes = if k == 0 { &[][..] } else { visible_namespaces };
    for namespace in scopes {
        if !seen.insert(namespace.as_str()) {
            continue;
        }
        let index = knn_ctes.len();
        let parameter = params.len() + 1;
        params.push(SqlValue::Text(namespace.clone()));
        knn_ctes.push(format!(
            "session_knn_{index} AS MATERIALIZED ( \
               SELECT v.subject_id, v.namespace AS vector_namespace, \
                      vec_distance_cosine(v.embedding, ?1) AS exact_distance \
                 FROM {table_name} v \
                WHERE namespace = ?{parameter} \
                  AND embedding_model = ?3 AND kind = 'note' AND field = 'note.content' \
                ORDER BY exact_distance, v.subject_id LIMIT ?2)"
        ));
        union_arms.push(format!(
            "SELECT subject_id, vector_namespace, exact_distance AS distance FROM session_knn_{index}"
        ));
    }
    let union = if union_arms.is_empty() {
        "SELECT NULL AS subject_id, NULL AS vector_namespace, NULL AS distance WHERE 0".into()
    } else {
        union_arms.join(" UNION ALL ")
    };
    let knn_ctes = if knn_ctes.is_empty() {
        String::new()
    } else {
        format!("{}, ", knn_ctes.join(", "))
    };
    let proof = session_fence_predicate(3, 4, 5, 6, 7);
    let sql = format!(
        "WITH session_proof AS MATERIALIZED ( \
           SELECT ({proof}) AS has_fence), \
         {knn_ctes}session_union AS MATERIALIZED ({union}), \
         session_ranked AS MATERIALIZED ( \
           SELECT c.subject_id, c.distance FROM session_union c \
           JOIN notes n ON n.id = c.subject_id \
             AND n.namespace = c.vector_namespace AND n.deleted_at IS NULL \
           ORDER BY c.distance, c.subject_id LIMIT ?2) \
         SELECT p.has_fence, r.subject_id, r.distance \
           FROM session_proof p LEFT JOIN session_ranked r ON 1 = 1 \
          ORDER BY r.distance, r.subject_id"
    );
    let mut reader = rt.sql().reader().await.map_err(|error| error.to_string())?;
    #[cfg(test)]
    let _ = SESSION_EXACT_STATEMENT_COUNT.try_with(|count| count.set(count.get() + 1));
    let rows = reader
        .query_all(SqlStatement {
            sql,
            params,
            label: Some("memory_session_exact_snapshot".into()),
        })
        .await
        .map_err(|error| error.to_string())?;
    let first = rows
        .first()
        .ok_or("session exact statement returned no proof row")?;
    match first.get("has_fence") {
        Some(SqlValue::Integer(0)) => return Ok(None),
        Some(SqlValue::Integer(1)) => {}
        other => {
            return Err(format!(
                "session exact proof has unexpected value {other:?}"
            ))
        }
    }

    let mut candidates = Vec::with_capacity(rows.len().min(k));
    for row in rows {
        let (id, distance) = match (row.get("subject_id"), row.get("distance")) {
            (Some(SqlValue::Null), Some(SqlValue::Null)) => continue,
            (Some(SqlValue::Text(id)), Some(SqlValue::Float(distance))) => (id, *distance),
            other => {
                return Err(format!(
                    "session exact candidate has unexpected value {other:?}"
                ))
            }
        };
        let id = Uuid::parse_str(id).map_err(|error| format!("session exact id: {error}"))?;
        let score = khive_score::try_cosine_score_with_f32_tolerance(distance)
            .map_err(|_| format!("session exact cosine distance out of range: {distance}"))?
            .to_f64() as f32;
        candidates.push((id, score));
    }
    Ok(Some(candidates))
}

#[cfg(test)]
#[path = "session_exact_tests.rs"]
mod tests;
