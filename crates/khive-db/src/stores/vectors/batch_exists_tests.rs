use std::collections::HashSet;
use std::sync::Arc;

use khive_types::SubstrateKind;
use uuid::Uuid;

use super::*;

fn make_vec_pool() -> Arc<crate::pool::ConnectionPool> {
    use crate::pool::{ConnectionPool, PoolConfig};
    crate::extension::ensure_extensions_loaded();
    let config = PoolConfig {
        path: None,
        ..PoolConfig::default()
    };
    Arc::new(ConnectionPool::new(config).expect("in-memory pool"))
}

fn create_vec_table(pool: &Arc<crate::pool::ConnectionPool>, model_key: &str, dims: usize) {
    let writer = pool.try_writer().expect("pool writer");
    let ddl = format!(
        "CREATE VIRTUAL TABLE IF NOT EXISTS vec_{} USING vec0(\
             subject_id TEXT PRIMARY KEY, \
             namespace TEXT NOT NULL, \
             kind TEXT NOT NULL, \
             field TEXT NOT NULL, \
             embedding_model TEXT NOT NULL, \
             embedding float[{}] distance_metric=cosine)",
        model_key, dims
    );
    writer.conn().execute_batch(&ddl).expect("create vec table");
    writer
        .conn()
        .execute_batch(crate::migrations::VECTOR_PROVENANCE_DDL)
        .expect("create vector_provenance");
    writer
        .conn()
        .execute_batch(crate::migrations::ANN_WRITE_LOG_DDL)
        .expect("create ann_write_log");
}

#[tokio::test]
async fn sqlite_vector_paths_use_canonical_opposite_cosine_score() {
    let pool = make_vec_pool();
    let model_key = "canonical_cosine_score";
    let namespace = "ns:canonical-score";
    create_vec_table(&pool, model_key, 2);
    let store = SqliteVecStore::new(
        pool,
        false,
        model_key.to_string(),
        model_key.to_string(),
        2,
        namespace.to_string(),
    )
    .unwrap();
    let opposite_id = Uuid::from_u128(1);
    store
        .insert(
            opposite_id,
            SubstrateKind::Entity,
            namespace,
            "body",
            vec![vec![-1.0, 0.0]],
        )
        .await
        .unwrap();

    let candidate_hits = store
        .score_candidates(&[1.0, 0.0], &[opposite_id])
        .await
        .unwrap();
    assert_eq!(candidate_hits.len(), 1);
    assert_eq!(candidate_hits[0].score, DeterministicScore::from_f64(-1.0));

    let search_hits = store
        .search(VectorSearchRequest {
            query_vectors: vec![vec![1.0, 0.0]],
            top_k: 1,
            namespace: Some(namespace.to_string()),
            kind: Some(SubstrateKind::Entity),
            embedding_model: None,
            filter: None,
            backend_hints: None,
        })
        .await
        .unwrap();
    assert_eq!(search_hits.len(), 1);
    assert_eq!(search_hits[0].score, DeterministicScore::from_f64(-1.0));
}

#[tokio::test]
async fn exact_only_insert_replaces_rows_without_ann_deltas() {
    let pool = make_vec_pool();
    let raw_pool = Arc::clone(&pool);
    let model_key = "exact_only_no_ann_delta";
    let namespace = "ns:exact-only";
    create_vec_table(&pool, model_key, 2);
    let store = SqliteVecStore::new(
        pool,
        false,
        model_key.to_string(),
        model_key.to_string(),
        2,
        namespace.to_string(),
    )
    .unwrap();
    let exact_id = Uuid::from_u128(1);

    store
        .insert_exact_only(
            exact_id,
            SubstrateKind::Entity,
            namespace,
            "visual.descriptor",
            vec![vec![1.0, 0.0]],
        )
        .await
        .unwrap();
    store
        .insert_exact_only(
            exact_id,
            SubstrateKind::Entity,
            "ns:exact-only-repaired",
            "visual.descriptor",
            vec![vec![0.0, 1.0]],
        )
        .await
        .unwrap();

    let writer = raw_pool.try_writer().expect("pool writer");
    let deltas: i64 = writer
        .conn()
        .query_row("SELECT COUNT(*) FROM ann_write_log", [], |row| row.get(0))
        .unwrap();
    assert_eq!(deltas, 0);
    let repaired_namespace: String = writer
        .conn()
        .query_row(
            &format!("SELECT namespace FROM vec_{model_key} WHERE subject_id = ?1"),
            [exact_id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(repaired_namespace, "ns:exact-only-repaired");
    drop(writer);

    store
        .insert(
            Uuid::from_u128(2),
            SubstrateKind::Entity,
            namespace,
            "body",
            vec![vec![1.0, 0.0]],
        )
        .await
        .unwrap();
    let writer = raw_pool.try_writer().expect("pool writer");
    let deltas: i64 = writer
        .conn()
        .query_row("SELECT COUNT(*) FROM ann_write_log", [], |row| row.get(0))
        .unwrap();
    assert_eq!(deltas, 1, "generic inserts must retain ANN logging");
}

/// sqlite-vec's own `distance_cosine_float` (sqlite-vec.c) accumulates
/// `dot`/`aMag`/`bMag` in f32, so a non-axis-aligned vector's self- and
/// opposite-comparison lands a few f32 ULPs off the exact 0/2 endpoint —
/// not the tighter f64 ULP an in-process helper call would produce. This
/// drives the real sqlite-vec extension end to end (not just the
/// conversion helper) so the endpoint tolerance is checked against the
/// roundoff sqlite-vec actually returns.
#[tokio::test]
async fn sqlite_vector_paths_tolerate_real_f32_endpoint_roundoff() {
    let pool = make_vec_pool();
    let model_key = "f32_endpoint_roundoff";
    let namespace = "ns:f32-roundoff";
    create_vec_table(&pool, model_key, 3);
    let store = SqliteVecStore::new(
        pool,
        false,
        model_key.to_string(),
        model_key.to_string(),
        3,
        namespace.to_string(),
    )
    .unwrap();

    let identical_id = Uuid::from_u128(1);
    let opposite_id = Uuid::from_u128(2);
    store
        .insert(
            identical_id,
            SubstrateKind::Entity,
            namespace,
            "body",
            vec![vec![0.1, 0.2, 0.3]],
        )
        .await
        .unwrap();
    store
        .insert(
            opposite_id,
            SubstrateKind::Entity,
            namespace,
            "body",
            vec![vec![-0.1, -0.2, -0.3]],
        )
        .await
        .unwrap();

    let candidate_hits = store
        .score_candidates(&[0.1, 0.2, 0.3], &[identical_id, opposite_id])
        .await
        .unwrap();
    let identical_score = candidate_hits
        .iter()
        .find(|hit| hit.subject_id == identical_id)
        .expect("identical candidate scored")
        .score;
    let opposite_score = candidate_hits
        .iter()
        .find(|hit| hit.subject_id == opposite_id)
        .expect("opposite candidate scored")
        .score;
    assert_eq!(identical_score, DeterministicScore::from_f64(1.0));
    assert_eq!(opposite_score, DeterministicScore::from_f64(-1.0));

    let search_hits = store
        .search(VectorSearchRequest {
            query_vectors: vec![vec![0.1, 0.2, 0.3]],
            top_k: 2,
            namespace: Some(namespace.to_string()),
            kind: Some(SubstrateKind::Entity),
            embedding_model: None,
            filter: None,
            backend_hints: None,
        })
        .await
        .unwrap();
    assert_eq!(search_hits.len(), 2);
    let top_hit = search_hits
        .iter()
        .find(|hit| hit.subject_id == identical_id)
        .expect("identical vector present in search results");
    assert_eq!(top_hit.score, DeterministicScore::from_f64(1.0));
    assert_eq!(top_hit.rank, 1);
}

/// Derivation (checked against sqlite-vec's `distance_cosine_float` in
/// sqlite-vec.c, which accumulates `dot`/`aMag`/`bMag` in f32 but calls
/// the C `sqrt` — not `sqrtf` — on those f32 accumulators, so the
/// square-root, product, and division all run in `double` before the
/// final implicit narrowing back to `f32` on return).
///
/// Because the widening to `double` happens before the square root, a
/// self- or exactly-proportional comparison (`dot == aMag == bMag`
/// bit-for-bit in f32) round-trips through `sqrt` at `double` precision
/// and lands within `f64::EPSILON` of the exact endpoint — nowhere near
/// the `f32::EPSILON` scale. The f32-scale roundoff this driver actually
/// exposes instead comes from the *accumulation* step: `dot` and the two
/// magnitudes are summed independently, so an exactly anti-parallel pair
/// (`query = -1.5 * stored`, mathematical cosine `-1`, ideal distance
/// `2`) can still see `dot` round to a f32 value fractionally larger in
/// magnitude than `sqrt(aMag) * sqrt(bMag)` would predict.
///
/// For `stored = [-4.8, -0.4, -0.4]` and `query = [7.2, 0.6, 0.6]`, that
/// yields a raw distance of `2.0000002384185791` — exactly
/// `2 + 2*f32::EPSILON`. Verified against the vendored
/// `distance_cosine_float` body compiled standalone under every
/// combination of `-O0`/`-O2`/`-O3` and `-ffp-contract=off`/`fast` plus
/// explicit `-mavx2 -mfma`, since whether the target architecture fuses
/// the per-term multiply-add changes the *path* to this result but not
/// the final f32 value — the fixture is architecture-independent. That
/// distance is outside the mathematical `[0, 2]` cosine-distance range,
/// yet its magnitude is within the widened `8*f32::EPSILON` boundary
/// (~9.537e-7) and enormously outside the old `4*f64::EPSILON`
/// tolerance (~8.88e-16) that an earlier version used. A driver that still
/// used the old f64-scale window would reject this distance outright.
#[tokio::test]
async fn sqlite_vector_paths_tolerate_real_f32_endpoint_roundoff_above_two() {
    let pool = make_vec_pool();
    let raw_pool = Arc::clone(&pool);
    let model_key = "f32_endpoint_roundoff_above_two";
    let namespace = "ns:f32-roundoff-above-two";
    create_vec_table(&pool, model_key, 3);
    let store = SqliteVecStore::new(
        pool,
        false,
        model_key.to_string(),
        model_key.to_string(),
        3,
        namespace.to_string(),
    )
    .unwrap();

    let stored_id = Uuid::from_u128(1);
    let stored_vector = vec![-4.8_f32, -0.4, -0.4];
    let query_vector = vec![7.2_f32, 0.6, 0.6];
    store
        .insert(
            stored_id,
            SubstrateKind::Entity,
            namespace,
            "body",
            vec![stored_vector],
        )
        .await
        .unwrap();

    let raw_distance: f64 = {
        let writer = raw_pool.try_writer().expect("pool writer");
        let sql = format!(
            "SELECT vec_distance_cosine(embedding, ?1) FROM vec_{} WHERE subject_id = ?2",
            model_key
        );
        let mut stmt = writer.conn().prepare(&sql).unwrap();
        stmt.raw_bind_parameter(1, encode_f32_native(&query_vector))
            .unwrap();
        stmt.raw_bind_parameter(2, stored_id.to_string().as_str())
            .unwrap();
        let mut rows = stmt.raw_query();
        let row = rows.next().unwrap().expect("one row");
        row.get(0).unwrap()
    };

    assert_eq!(raw_distance, 2.0 + 2.0 * f32::EPSILON as f64);
    assert!(
        !(0.0..=2.0).contains(&raw_distance),
        "fixture must land outside the mathematical [0, 2] cosine range, got {raw_distance}"
    );
    let deviation_above_two = raw_distance - 2.0;
    let boundary_epsilon = 8.0 * f32::EPSILON as f64;
    assert!(
        deviation_above_two <= boundary_epsilon,
        "fixture must stay within the widened f32-scale tolerance, got {raw_distance}"
    );
    let old_tolerance = 4.0 * f64::EPSILON;
    assert!(
        deviation_above_two > old_tolerance,
        "fixture must exceed the old f64-scale tolerance so it would have failed under it, got {raw_distance}"
    );

    let candidate_hits = store
        .score_candidates(&query_vector, &[stored_id])
        .await
        .unwrap();
    assert_eq!(candidate_hits.len(), 1);
    assert_eq!(candidate_hits[0].score, DeterministicScore::from_f64(-1.0));

    let search_hits = store
        .search(VectorSearchRequest {
            query_vectors: vec![query_vector],
            top_k: 1,
            namespace: Some(namespace.to_string()),
            kind: Some(SubstrateKind::Entity),
            embedding_model: None,
            filter: None,
            backend_hints: None,
        })
        .await
        .unwrap();
    assert_eq!(search_hits.len(), 1);
    assert_eq!(search_hits[0].score, DeterministicScore::from_f64(-1.0));
    assert_eq!(search_hits[0].rank, 1);
}

#[tokio::test]
async fn score_candidates_rejects_an_empty_query_dimension() {
    let pool = make_vec_pool();
    let store = SqliteVecStore::new(
        pool,
        false,
        "empty_query_dimension".to_string(),
        "empty_query_dimension".to_string(),
        2,
        "ns:empty-query".to_string(),
    )
    .unwrap();

    let error = store
        .score_candidates(&[], &[Uuid::from_u128(1)])
        .await
        .unwrap_err();
    match error {
        StorageError::InvalidInput {
            operation, message, ..
        } => {
            assert_eq!(operation, "score_candidates");
            assert!(message.contains("query has 0 dims, expected 2"));
        }
        other => panic!("expected dimension error, got {other:?}"),
    }
}

#[tokio::test]
async fn score_candidates_breaks_equal_scores_by_lower_id() {
    let pool = make_vec_pool();
    let model_key = "candidate_score_ties";
    let namespace = "ns:candidate-ties";
    create_vec_table(&pool, model_key, 2);
    let store = SqliteVecStore::new(
        pool,
        false,
        model_key.to_string(),
        model_key.to_string(),
        2,
        namespace.to_string(),
    )
    .unwrap();
    let lower_id = Uuid::from_u128(1);
    let higher_id = Uuid::from_u128(2);
    for id in [higher_id, lower_id] {
        store
            .insert(
                id,
                SubstrateKind::Entity,
                namespace,
                "body",
                vec![vec![1.0, 0.0]],
            )
            .await
            .unwrap();
    }

    let hits = store
        .score_candidates(&[1.0, 0.0], &[higher_id, lower_id])
        .await
        .unwrap();
    assert_eq!(
        hits.iter().map(|hit| hit.subject_id).collect::<Vec<_>>(),
        vec![lower_id, higher_id]
    );
    assert_eq!(
        hits.iter().map(|hit| hit.rank).collect::<Vec<_>>(),
        vec![1, 2]
    );
}

/// Valid (underscored) model key: batch_exists returns the exact set of IDs
/// that have embeddings and excludes IDs that were never inserted.
#[tokio::test]
async fn batch_exists_returns_correct_set_for_underscored_model_key() {
    let pool = make_vec_pool();
    let model_key = "all_minilm_l6_v2";
    let dims = 4;
    let ns = "ns:test";

    create_vec_table(&pool, model_key, dims);

    let store = SqliteVecStore::new(
        pool,
        false,
        model_key.to_string(),
        model_key.to_string(),
        dims,
        ns.to_string(),
    )
    .expect("SqliteVecStore::new");

    let id1 = Uuid::new_v4();
    let id2 = Uuid::new_v4();
    let id_absent = Uuid::new_v4();

    store
        .insert(
            id1,
            SubstrateKind::Entity,
            ns,
            "body",
            vec![vec![0.1, 0.2, 0.3, 0.4]],
        )
        .await
        .expect("insert id1");
    store
        .insert(
            id2,
            SubstrateKind::Entity,
            ns,
            "body",
            vec![vec![0.5, 0.6, 0.7, 0.8]],
        )
        .await
        .expect("insert id2");

    let exists = store
        .batch_exists(&[id1, id2, id_absent], ns)
        .await
        .expect("batch_exists");

    assert!(exists.contains(&id1), "id1 must be found");
    assert!(exists.contains(&id2), "id2 must be found");
    assert!(
        !exists.contains(&id_absent),
        "absent id must not be returned"
    );
    assert_eq!(exists.len(), 2);
}

/// Empty input must return an empty set without hitting the DB.
#[tokio::test]
async fn batch_exists_empty_ids_returns_empty_set() {
    let pool = make_vec_pool();
    let model_key = "empty_test_model";
    create_vec_table(&pool, model_key, 4);

    let store = SqliteVecStore::new(
        pool,
        false,
        model_key.to_string(),
        model_key.to_string(),
        4,
        "ns:test".to_string(),
    )
    .expect("SqliteVecStore::new");

    let exists: HashSet<Uuid> = store
        .batch_exists(&[], "ns:test")
        .await
        .expect("batch_exists");
    assert!(exists.is_empty());
}

/// A nearer vector in namespace A must not starve the top-k result in namespace B.
///
/// Regression for the cross-namespace recall starvation path: sqlite-vec must
/// evaluate the namespace predicate before computing global top-k, not after.
#[tokio::test]
async fn vector_search_namespace_predicate_prevents_recall_starvation() {
    let pool = make_vec_pool();
    let model_key = "knn_namespace_scope";
    let dims = 4;
    create_vec_table(&pool, model_key, dims);

    let store = SqliteVecStore::new(
        pool,
        false,
        model_key.to_string(),
        model_key.to_string(),
        dims,
        "ns:b".to_string(),
    )
    .expect("SqliteVecStore::new");

    let distractor_a = Uuid::new_v4();
    let victim_b = Uuid::new_v4();

    // Insert a nearer vector in namespace A (distractor).
    store
        .insert(
            distractor_a,
            SubstrateKind::Entity,
            "ns:a",
            "body",
            vec![vec![1.0, 0.0, 0.0, 0.0]],
        )
        .await
        .expect("insert nearer cross-namespace vector");

    // Insert a slightly farther vector in namespace B (victim).
    store
        .insert(
            victim_b,
            SubstrateKind::Entity,
            "ns:b",
            "body",
            vec![vec![0.8, 0.2, 0.0, 0.0]],
        )
        .await
        .expect("insert in-namespace vector");

    // top_k=1 search in ns:b must return victim_b, not the nearer distractor_a.
    let hits = store
        .search(VectorSearchRequest {
            query_vectors: vec![vec![1.0, 0.0, 0.0, 0.0]],
            top_k: 1,
            namespace: Some("ns:b".to_string()),
            kind: Some(SubstrateKind::Entity),
            embedding_model: None,
            filter: None,
            backend_hints: None,
        })
        .await
        .expect("search");

    assert_eq!(
        hits.len(),
        1,
        "namespace B must not be starved by namespace A"
    );
    assert_eq!(
        hits[0].subject_id, victim_b,
        "top-1 in ns:b must be victim_b, not cross-namespace distractor_a"
    );
}

/// Hyphenated model_key must be rejected at SqliteVecStore::new(), preventing
/// any table-name divergence between the store and a hand-rolled sanitizer.
#[test]
fn hyphenated_model_key_is_rejected_at_construction() {
    use crate::pool::{ConnectionPool, PoolConfig};
    let pool = Arc::new(
        ConnectionPool::new(PoolConfig {
            path: None,
            ..PoolConfig::default()
        })
        .expect("pool"),
    );

    let result = SqliteVecStore::new(
        pool,
        false,
        "all-minilm-l6-v2".to_string(),
        "all-minilm-l6-v2".to_string(),
        4,
        "ns:test".to_string(),
    );

    assert!(
        result.is_err(),
        "hyphenated model_key 'all-minilm-l6-v2' must be rejected; \
             the store's table_name would differ from what a hand-rolled sanitizer produces"
    );
}
