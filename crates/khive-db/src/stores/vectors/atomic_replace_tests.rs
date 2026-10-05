use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use khive_storage::types::VectorRecord;
use khive_storage::VectorStore;
use khive_types::SubstrateKind;
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use uuid::Uuid;

use super::*;

type AnnWriteLogRow = (String, String, String, String, String);

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

fn clear_ann_write_log(pool: &Arc<crate::pool::ConnectionPool>) {
    pool.try_writer()
        .expect("pool writer")
        .conn()
        .execute("DELETE FROM ann_write_log", [])
        .expect("clear ann_write_log");
}

fn ann_write_log_rows(
    pool: &Arc<crate::pool::ConnectionPool>,
    subject_id: Uuid,
) -> Vec<AnnWriteLogRow> {
    let writer = pool.try_writer().expect("pool writer");
    let mut stmt = writer
        .conn()
        .prepare(
            "SELECT namespace, embedding_model, kind, field, op \
                 FROM ann_write_log WHERE subject_id = ?1 ORDER BY seq",
        )
        .expect("prepare ann_write_log query");
    stmt.query_map(rusqlite::params![subject_id.to_string()], |row| {
        Ok((
            row.get(0)?,
            row.get(1)?,
            row.get(2)?,
            row.get(3)?,
            row.get(4)?,
        ))
    })
    .expect("query ann_write_log")
    .collect::<Result<Vec<_>, _>>()
    .expect("read ann_write_log")
}

fn ann_write_log_row(namespace: &str, model: &str, op: &str) -> AnnWriteLogRow {
    (
        namespace.to_string(),
        model.to_string(),
        "entity".to_string(),
        "body".to_string(),
        op.to_string(),
    )
}

/// insert_batch: a record with wrong dimensions fails its INSERT but must not
/// lose the previously stored vector (no-worse-than-stale guarantee for batch).
///
/// Setup: insert a good vector for `id_existing` via the single-record path.
/// Then call insert_batch with two records: `id_existing` with wrong dimensions
/// (forced failure), and `id_new` with correct dimensions.
/// Expected: `id_existing`'s old vector survives; `id_new` is inserted;
/// BatchWriteSummary reflects 1 affected / 1 failed.
#[tokio::test]
async fn insert_batch_failed_record_preserves_prior_vector() {
    let pool = make_vec_pool();
    let model_key = "atomic_batch_test";
    let dims = 4;
    let ns = "ns:atomic";

    create_vec_table(&pool, model_key, dims);

    let store = SqliteVecStore::new(
        Arc::clone(&pool),
        false,
        model_key.to_string(),
        model_key.to_string(),
        dims,
        ns.to_string(),
    )
    .expect("SqliteVecStore::new");

    let id_existing = Uuid::new_v4();
    let id_new = Uuid::new_v4();
    let original_vec = vec![0.1f32, 0.2, 0.3, 0.4];

    store
        .insert(
            id_existing,
            SubstrateKind::Entity,
            ns,
            "body",
            vec![original_vec.clone()],
        )
        .await
        .expect("initial insert");

    let summary = store
        .insert_batch(vec![
            VectorRecord {
                subject_id: id_existing,
                kind: SubstrateKind::Entity,
                namespace: ns.to_string(),
                field: "body".to_string(),
                embedding_model: None,
                vectors: vec![vec![9.9f32; dims + 1]],
                text_fingerprint: None,
                updated_at: chrono::Utc::now(),
            },
            VectorRecord {
                subject_id: id_new,
                kind: SubstrateKind::Entity,
                namespace: ns.to_string(),
                field: "body".to_string(),
                embedding_model: None,
                vectors: vec![vec![0.5f32, 0.6, 0.7, 0.8]],
                text_fingerprint: None,
                updated_at: chrono::Utc::now(),
            },
        ])
        .await
        .expect("insert_batch");

    assert_eq!(summary.attempted, 2);
    assert_eq!(summary.affected, 1, "only id_new should succeed");
    assert_eq!(summary.failed, 1, "id_existing with wrong dims must fail");

    let existing_still_present = store
        .batch_exists(&[id_existing], ns)
        .await
        .expect("batch_exists");
    assert!(
        existing_still_present.contains(&id_existing),
        "prior vector for id_existing must survive a failed batch replace"
    );

    let new_present = store
        .batch_exists(&[id_new], ns)
        .await
        .expect("batch_exists for id_new");
    assert!(
        new_present.contains(&id_new),
        "id_new with valid dims must be inserted"
    );
}

/// update: a vector with wrong dimensions must fail without deleting the prior
/// vector (no-worse-than-stale guarantee for the update override).
#[tokio::test]
async fn update_failed_preserves_prior_vector() {
    let pool = make_vec_pool();
    let model_key = "atomic_update_test";
    let dims = 4;
    let ns = "ns:atomic_upd";

    create_vec_table(&pool, model_key, dims);

    let store = SqliteVecStore::new(
        Arc::clone(&pool),
        false,
        model_key.to_string(),
        model_key.to_string(),
        dims,
        ns.to_string(),
    )
    .expect("SqliteVecStore::new");

    let id = Uuid::new_v4();

    store
        .insert(
            id,
            SubstrateKind::Entity,
            ns,
            "body",
            vec![vec![0.1f32, 0.2, 0.3, 0.4]],
        )
        .await
        .expect("initial insert");

    let result = store
        .update(
            id,
            SubstrateKind::Entity,
            ns,
            "body",
            vec![vec![9.9f32; dims + 1]],
        )
        .await;

    assert!(result.is_err(), "update with wrong dims must fail");

    let still_present = store
        .batch_exists(&[id], ns)
        .await
        .expect("batch_exists after failed update");
    assert!(
        still_present.contains(&id),
        "prior vector must survive a failed update"
    );
}

/// insert_batch atomically replaces stale namespace metadata because the
/// vec0 primary key is the globally unique subject ID.
#[tokio::test]
async fn insert_batch_replaces_cross_namespace_row() {
    let pool = make_vec_pool();
    let model_key = "atomic_pk_batch";
    let dims = 4;
    let ns_a = "ns:pk_a";
    let ns_b = "ns:pk_b";

    create_vec_table(&pool, model_key, dims);

    let store = SqliteVecStore::new(
        Arc::clone(&pool),
        false,
        model_key.to_string(),
        model_key.to_string(),
        dims,
        ns_a.to_string(),
    )
    .expect("SqliteVecStore::new");

    let id_x = Uuid::new_v4();
    let stale_vec = vec![0.1f32, 0.2, 0.3, 0.4];

    // Store stale row in ns:a — this occupies id_X in the vec0 PK.
    store
        .insert(
            id_x,
            SubstrateKind::Entity,
            ns_a,
            "body",
            vec![stale_vec.clone()],
        )
        .await
        .expect("stale insert");
    clear_ann_write_log(&pool);

    let replacement_vec = vec![0.5f32, 0.6, 0.7, 0.8];
    let summary = store
        .insert_batch(vec![VectorRecord {
            subject_id: id_x,
            kind: SubstrateKind::Entity,
            namespace: ns_b.to_string(),
            field: "body".to_string(),
            embedding_model: None,
            vectors: vec![replacement_vec.clone()],
            text_fingerprint: None,
            updated_at: chrono::Utc::now(),
        }])
        .await
        .expect("insert_batch must complete (outer tx must commit)");

    assert_eq!(summary.attempted, 1);
    assert_eq!(summary.affected, 1);
    assert_eq!(summary.failed, 0);

    let stale = store
        .batch_exists(&[id_x], ns_a)
        .await
        .expect("batch_exists ns:a");
    assert!(
        !stale.contains(&id_x),
        "the stale namespace row must be replaced"
    );
    let replacement = store
        .batch_exists(&[id_x], ns_b)
        .await
        .expect("batch_exists ns:b");
    assert!(replacement.contains(&id_x));

    assert_eq!(
        ann_write_log_rows(&pool, id_x),
        vec![
            ann_write_log_row(ns_a, model_key, "delete"),
            ann_write_log_row(ns_b, model_key, "upsert"),
        ],
        "committed replacement must invalidate the old ANN identity before upserting the new one"
    );

    let hits = store
        .search(VectorSearchRequest {
            query_vectors: vec![replacement_vec],
            top_k: 1,
            namespace: Some(ns_b.to_string()),
            kind: Some(SubstrateKind::Entity),
            embedding_model: None,
            filter: None,
            backend_hints: None,
        })
        .await
        .expect("search ns:b after batch");

    assert_eq!(hits.len(), 1, "replacement vector must be searchable");
    assert_eq!(hits[0].subject_id, id_x);
    let sim = hits[0].score.to_f64();
    assert!(
        sim > 0.999,
        "cosine similarity of the replacement to itself must be ~1.0 (got {sim:.6})"
    );
}

/// Sequential replacements of one subject keep the final record coherent.
#[tokio::test]
async fn insert_batch_cross_namespace_replacements_are_ordered() {
    let pool = make_vec_pool();
    let model_key = "atomic_sib_batch";
    let dims = 4;
    let ns_a = "ns:sib_a";
    let ns_b = "ns:sib_b";

    create_vec_table(&pool, model_key, dims);

    let store = SqliteVecStore::new(
        Arc::clone(&pool),
        false,
        model_key.to_string(),
        model_key.to_string(),
        dims,
        ns_a.to_string(),
    )
    .expect("SqliteVecStore::new");

    let id_x = Uuid::new_v4();
    let stale_vec = vec![0.1f32, 0.2, 0.3, 0.4];
    let new_vec = vec![0.9f32, 0.1, 0.1, 0.1];

    // Stale row occupies id_X in ns:a.
    store
        .insert(
            id_x,
            SubstrateKind::Entity,
            ns_a,
            "body",
            vec![stale_vec.clone()],
        )
        .await
        .expect("stale insert");

    let summary = store
        .insert_batch(vec![
            VectorRecord {
                subject_id: id_x,
                kind: SubstrateKind::Entity,
                namespace: ns_b.to_string(),
                field: "body".to_string(),
                embedding_model: None,
                vectors: vec![vec![0.5f32, 0.6, 0.7, 0.8]],
                text_fingerprint: None,
                updated_at: chrono::Utc::now(),
            },
            VectorRecord {
                subject_id: id_x,
                kind: SubstrateKind::Entity,
                namespace: ns_a.to_string(),
                field: "body".to_string(),
                embedding_model: None,
                vectors: vec![new_vec.clone()],
                text_fingerprint: None,
                updated_at: chrono::Utc::now(),
            },
        ])
        .await
        .expect("insert_batch");

    assert_eq!(summary.attempted, 2);
    assert_eq!(summary.affected, 2);
    assert_eq!(summary.failed, 0);

    // Record B's new_vec must be in the DB with correct embedding bytes.
    let hits = store
        .search(VectorSearchRequest {
            query_vectors: vec![new_vec.clone()],
            top_k: 1,
            namespace: Some(ns_a.to_string()),
            kind: Some(SubstrateKind::Entity),
            embedding_model: None,
            filter: None,
            backend_hints: None,
        })
        .await
        .expect("search after batch");

    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].subject_id, id_x);
    let sim = hits[0].score.to_f64();
    assert!(
        sim > 0.999,
        "new_vec similarity to itself must be ~1.0 (got {sim:.6})"
    );
}

/// update atomically replaces stale namespace metadata for a subject.
#[tokio::test]
async fn update_replaces_cross_namespace_row() {
    let pool = make_vec_pool();
    let model_key = "atomic_upd_pk";
    let dims = 4;
    let ns_a = "ns:upk_a";
    let ns_b = "ns:upk_b";

    create_vec_table(&pool, model_key, dims);

    let store = SqliteVecStore::new(
        Arc::clone(&pool),
        false,
        model_key.to_string(),
        model_key.to_string(),
        dims,
        ns_a.to_string(),
    )
    .expect("store");

    let id_x = Uuid::new_v4();
    let stale_vec = vec![0.1f32, 0.2, 0.3, 0.4];

    // Store stale row in ns:a.
    store
        .insert(
            id_x,
            SubstrateKind::Entity,
            ns_a,
            "body",
            vec![stale_vec.clone()],
        )
        .await
        .expect("stale insert");
    clear_ann_write_log(&pool);

    let replacement_vec = vec![0.5f32, 0.6, 0.7, 0.8];
    store
        .update(
            id_x,
            SubstrateKind::Entity,
            ns_b,
            "body",
            vec![replacement_vec.clone()],
        )
        .await
        .expect("replace stale namespace row");

    let stale = store
        .batch_exists(&[id_x], ns_a)
        .await
        .expect("batch_exists old namespace");
    assert!(
        !stale.contains(&id_x),
        "stale namespace metadata must be removed"
    );
    let replacement = store
        .batch_exists(&[id_x], ns_b)
        .await
        .expect("batch_exists replacement namespace");
    assert!(replacement.contains(&id_x));

    assert_eq!(
        ann_write_log_rows(&pool, id_x),
        vec![
            ann_write_log_row(ns_a, model_key, "delete"),
            ann_write_log_row(ns_b, model_key, "upsert"),
        ],
        "committed replacement must invalidate the old ANN identity before upserting the new one"
    );

    let hits = store
        .search(VectorSearchRequest {
            query_vectors: vec![replacement_vec],
            top_k: 1,
            namespace: Some(ns_b.to_string()),
            kind: Some(SubstrateKind::Entity),
            embedding_model: None,
            filter: None,
            backend_hints: None,
        })
        .await
        .expect("search after update");

    assert_eq!(hits.len(), 1, "replacement vector must be searchable");
    assert_eq!(hits[0].subject_id, id_x);
    let sim = hits[0].score.to_f64();
    assert!(
        sim > 0.999,
        "cosine similarity of the replacement to itself must be ~1.0 (got {sim:.6})"
    );
}

#[tokio::test]
async fn same_identity_replacement_logs_only_upsert() {
    let pool = make_vec_pool();
    let model_key = "atomic_same_identity";
    let dims = 4;
    let ns = "ns:same_identity";

    create_vec_table(&pool, model_key, dims);

    let store = SqliteVecStore::new(
        Arc::clone(&pool),
        false,
        model_key.to_string(),
        model_key.to_string(),
        dims,
        ns.to_string(),
    )
    .expect("store");

    let id = Uuid::new_v4();
    store
        .insert(
            id,
            SubstrateKind::Entity,
            ns,
            "body",
            vec![vec![0.1f32, 0.2, 0.3, 0.4]],
        )
        .await
        .expect("initial insert");
    clear_ann_write_log(&pool);

    let prepared_log_inserts = Arc::new(AtomicUsize::new(0));
    {
        let prepared_log_inserts = Arc::clone(&prepared_log_inserts);
        pool.try_writer()
            .expect("pool writer")
            .conn()
            .authorizer(Some(move |context: AuthContext<'_>| {
                if matches!(
                    context.action,
                    AuthAction::Insert {
                        table_name: "ann_write_log"
                    }
                ) {
                    prepared_log_inserts.fetch_add(1, Ordering::SeqCst);
                }
                Authorization::Allow
            }))
            .expect("install statement authorizer");
    }

    store
        .update(
            id,
            SubstrateKind::Entity,
            ns,
            "body",
            vec![vec![0.5f32, 0.6, 0.7, 0.8]],
        )
        .await
        .expect("same-identity replacement");

    assert_eq!(
        ann_write_log_rows(&pool, id),
        vec![ann_write_log_row(ns, model_key, "upsert")],
        "same-identity replacement must not emit delete/upsert churn"
    );
    assert_eq!(
        prepared_log_inserts.load(Ordering::SeqCst),
        1,
        "the common replacement path must prepare only the required upsert log statement"
    );
    pool.try_writer()
        .expect("pool writer")
        .conn()
        .authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
        .expect("remove statement authorizer");
}

// True ROLLBACK TO SAVEPOINT sentinels (failpoint-driven) — see
// crates/khive-db/docs/api/vectors.md#true-rollback-to-savepoint-sentinels-failpoint-driven

/// SENTINEL — insert_batch: stale row is restored when DELETE succeeds
/// but INSERT is forced to fail via the cfg(test) failpoint. See
/// crates/khive-db/docs/api/vectors.md#insert_batch_rollback_restores_deleted_stale_after_post_delete_insert_failure
#[tokio::test]
async fn insert_batch_rollback_restores_deleted_stale_after_post_delete_insert_failure() {
    let pool = make_vec_pool();
    let model_key = "sentinel_batch_rb";
    let dims = 4;
    let old_ns = "ns:sentinel_batch_old";
    let new_ns = "ns:sentinel_batch_new";

    create_vec_table(&pool, model_key, dims);

    let store = SqliteVecStore::new(
        Arc::clone(&pool),
        false,
        model_key.to_string(),
        model_key.to_string(),
        dims,
        old_ns.to_string(),
    )
    .expect("SqliteVecStore::new");

    let id_x = Uuid::new_v4();
    let vec1 = vec![0.1f32, 0.2, 0.3, 0.4];
    let vec2 = vec![0.9f32, 0.0, 0.0, 0.0];

    // Insert the stale row that must survive.
    store
        .insert(
            id_x,
            SubstrateKind::Entity,
            old_ns,
            "body",
            vec![vec1.clone()],
        )
        .await
        .expect("stale insert");
    clear_ann_write_log(&pool);

    // Arm the failpoint under an RAII guard so it always clears on exit.
    // The guard is dropped AFTER the batch call returns, but `take()` is
    // one-shot — it clears the flag the moment the failpoint fires.
    let _guard = failpoint::FailpointGuard::new();

    // Cross-namespace, correct dims, finite — deletion logging and DELETE
    // run before the failpoint fires.
    let summary = store
        .insert_batch(vec![VectorRecord {
            subject_id: id_x,
            kind: SubstrateKind::Entity,
            namespace: new_ns.to_string(),
            field: "body".to_string(),
            embedding_model: None,
            vectors: vec![vec2.clone()],
            text_fingerprint: None,
            updated_at: chrono::Utc::now(),
        }])
        .await
        .expect("insert_batch must complete (outer tx must commit regardless)");

    drop(_guard); // explicit drop for clarity; flag already cleared by take()

    assert_eq!(summary.attempted, 1);
    assert_eq!(
        summary.affected, 0,
        "failpoint must prevent INSERT from succeeding"
    );
    assert_eq!(
        summary.failed, 1,
        "failed counter must increment after injected failure"
    );

    // ROLLBACK TO SAVEPOINT must have restored the deleted stale row.
    let present = store
        .batch_exists(&[id_x], old_ns)
        .await
        .expect("batch_exists after failpoint");
    assert!(
        present.contains(&id_x),
        "ROLLBACK TO SAVEPOINT must restore the stale row after DELETE + injected failure"
    );
    assert!(
        !store
            .batch_exists(&[id_x], new_ns)
            .await
            .expect("batch_exists replacement namespace after failpoint")
            .contains(&id_x),
        "rolled-back replacement must not leave a new-namespace row"
    );
    assert_eq!(
        ann_write_log_rows(&pool, id_x),
        Vec::<AnnWriteLogRow>::new(),
        "rollback must remove both the old-identity delete and new-identity upsert log rows"
    );

    // Self-similarity with vec1 (not vec2) confirms the original bytes are restored.
    let hits = store
        .search(VectorSearchRequest {
            query_vectors: vec![vec1.clone()],
            top_k: 1,
            namespace: Some(old_ns.to_string()),
            kind: Some(SubstrateKind::Entity),
            embedding_model: None,
            filter: None,
            backend_hints: None,
        })
        .await
        .expect("search after failpoint");

    assert_eq!(
        hits.len(),
        1,
        "stale vector must be searchable after rollback"
    );
    assert_eq!(hits[0].subject_id, id_x);
    let sim = hits[0].score.to_f64();
    assert!(
            sim > 0.999,
            "similarity to vec1 must be ~1.0 (got {sim:.6}); \
             a lower value means the stale embedding was not restored — ROLLBACK TO SAVEPOINT failed"
        );

    // Cross-check: vec2 must NOT be the stored embedding.
    let hits2 = store
        .search(VectorSearchRequest {
            query_vectors: vec![vec2.clone()],
            top_k: 1,
            namespace: Some(old_ns.to_string()),
            kind: Some(SubstrateKind::Entity),
            embedding_model: None,
            filter: None,
            backend_hints: None,
        })
        .await
        .expect("search vec2 after failpoint");
    let sim2 = hits2.first().map(|h| h.score.to_f64()).unwrap_or(0.0);
    assert!(
        sim2 < 0.99,
        "similarity to vec2 must be < 0.99 (got {sim2:.6}); \
             vec2 must not be the stored embedding after a rolled-back INSERT"
    );
}

/// SENTINEL — update: stale row is restored when DELETE succeeds but
/// INSERT is forced to fail via the cfg(test) failpoint. See
/// crates/khive-db/docs/api/vectors.md#update_rollback_restores_deleted_stale_after_post_delete_insert_failure
#[tokio::test]
async fn update_rollback_restores_deleted_stale_after_post_delete_insert_failure() {
    let pool = make_vec_pool();
    let model_key = "sentinel_upd_rb";
    let dims = 4;
    let ns = "ns:sentinel_upd";

    create_vec_table(&pool, model_key, dims);

    let store = SqliteVecStore::new(
        Arc::clone(&pool),
        false,
        model_key.to_string(),
        model_key.to_string(),
        dims,
        ns.to_string(),
    )
    .expect("SqliteVecStore::new");

    let id_x = Uuid::new_v4();
    let vec1 = vec![0.1f32, 0.2, 0.3, 0.4];
    let vec2 = vec![0.9f32, 0.0, 0.0, 0.0];

    // Insert the stale row that must survive.
    store
        .insert(id_x, SubstrateKind::Entity, ns, "body", vec![vec1.clone()])
        .await
        .expect("stale insert");

    // Arm the failpoint under a RAII guard.
    let _guard = failpoint::FailpointGuard::new();

    // Same namespace, correct dims, finite — DELETE will run, then failpoint fires.
    let result = store
        .update(id_x, SubstrateKind::Entity, ns, "body", vec![vec2.clone()])
        .await;

    drop(_guard);

    assert!(
        result.is_err(),
        "update must propagate the injected error back to the caller"
    );

    // Transaction rollback must have restored the deleted stale row.
    let present = store
        .batch_exists(&[id_x], ns)
        .await
        .expect("batch_exists after failpoint");
    assert!(
        present.contains(&id_x),
        "transaction rollback must restore the stale row after DELETE + injected failure"
    );

    // Self-similarity with vec1 confirms the original bytes are intact.
    let hits = store
        .search(VectorSearchRequest {
            query_vectors: vec![vec1.clone()],
            top_k: 1,
            namespace: Some(ns.to_string()),
            kind: Some(SubstrateKind::Entity),
            embedding_model: None,
            filter: None,
            backend_hints: None,
        })
        .await
        .expect("search after failpoint");

    assert_eq!(
        hits.len(),
        1,
        "stale vector must be searchable after rollback"
    );
    assert_eq!(hits[0].subject_id, id_x);
    let sim = hits[0].score.to_f64();
    assert!(
            sim > 0.999,
            "similarity to vec1 must be ~1.0 (got {sim:.6}); \
             a lower value means the stale embedding was not restored — transaction rollback failed"
        );
}

/// #546: `insert` now routes through the shared `replace_vector_row_dml`
/// helper, so the same post-delete-failpoint rollback guarantee that
/// covers `update` must also cover `insert`. See
/// crates/khive-db/docs/api/vectors.md#insert_rollback_restores_deleted_stale_after_post_delete_insert_failure
#[tokio::test]
async fn insert_rollback_restores_deleted_stale_after_post_delete_insert_failure() {
    let pool = make_vec_pool();
    let model_key = "sentinel_ins_rb";
    let dims = 4;
    let ns = "ns:sentinel_ins";

    create_vec_table(&pool, model_key, dims);

    let store = SqliteVecStore::new(
        Arc::clone(&pool),
        false,
        model_key.to_string(),
        model_key.to_string(),
        dims,
        ns.to_string(),
    )
    .expect("SqliteVecStore::new");

    let id_x = Uuid::new_v4();
    let vec1 = vec![0.1f32, 0.2, 0.3, 0.4];
    let vec2 = vec![0.9f32, 0.0, 0.0, 0.0];

    // Insert the stale row that must survive a second, failing `insert`
    // call for the same (subject_id, namespace) — `vec0` has no
    // INSERT OR REPLACE, so a second `insert` is itself a replace.
    store
        .insert(id_x, SubstrateKind::Entity, ns, "body", vec![vec1.clone()])
        .await
        .expect("stale insert");

    // Arm the failpoint under a RAII guard.
    let _guard = failpoint::FailpointGuard::new();

    // Same namespace, correct dims, finite — DELETE will run, then failpoint fires.
    let result = store
        .insert(id_x, SubstrateKind::Entity, ns, "body", vec![vec2.clone()])
        .await;

    drop(_guard);

    assert!(
        result.is_err(),
        "insert must propagate the injected error back to the caller"
    );

    // Transaction rollback must have restored the deleted stale row.
    let present = store
        .batch_exists(&[id_x], ns)
        .await
        .expect("batch_exists after failpoint");
    assert!(
        present.contains(&id_x),
        "transaction rollback must restore the stale row after DELETE + injected failure"
    );

    // Self-similarity with vec1 confirms the original bytes are intact.
    let hits = store
        .search(VectorSearchRequest {
            query_vectors: vec![vec1.clone()],
            top_k: 1,
            namespace: Some(ns.to_string()),
            kind: Some(SubstrateKind::Entity),
            embedding_model: None,
            filter: None,
            backend_hints: None,
        })
        .await
        .expect("search after failpoint");

    assert_eq!(
        hits.len(),
        1,
        "stale vector must be searchable after rollback"
    );
    assert_eq!(hits[0].subject_id, id_x);
    let sim = hits[0].score.to_f64();
    assert!(
            sim > 0.999,
            "similarity to vec1 must be ~1.0 (got {sim:.6}); \
             a lower value means the stale embedding was not restored — transaction rollback failed"
        );
}
