use super::*;
use khive_storage::types::VectorRecord;
use khive_storage::VectorStore;
use khive_types::SubstrateKind;
use uuid::Uuid;

fn make_pool() -> Arc<crate::pool::ConnectionPool> {
    use crate::pool::{ConnectionPool, PoolConfig};
    let config = PoolConfig {
        path: None,
        ..PoolConfig::default()
    };
    Arc::new(ConnectionPool::new(config).expect("in-memory pool"))
}

/// insert_batch must populate `first_error` when records fail the dimension
/// validation check.
///
/// Both records have the wrong number of dimensions, so both hit the
/// `embedding.len() != dims` guard before any SAVEPOINT or vec0 operation.
/// The outer transaction still commits (best-effort batch semantics).
///
/// Regression: before the fix, `first_error` was always `String::new()` even
/// when `failed > 0`.  This test is RED against the unfixed code and GREEN
/// after the fix.
#[tokio::test]
async fn insert_batch_first_error_populated_on_dimension_mismatch() {
    let dims = 4usize;
    let store = SqliteVecStore::new(
        make_pool(),
        false,
        "first_err_vec".into(),
        "first_err_vec".into(),
        dims,
        "ns:test".into(),
    )
    .expect("SqliteVecStore::new");

    // Both records have wrong dimensions, so they fail the pre-SAVEPOINT
    // validation and never touch the vec0 virtual table.
    let first_id = Uuid::new_v4();
    let second_id = Uuid::new_v4();
    let summary = store
        .insert_batch(vec![
            VectorRecord {
                subject_id: first_id,
                kind: SubstrateKind::Entity,
                namespace: "ns:test".to_string(),
                field: "body".to_string(),
                embedding_model: None,
                vectors: vec![vec![0.0f32; dims + 1]],
                text_fingerprint: None,
                updated_at: chrono::Utc::now(),
            },
            VectorRecord {
                subject_id: second_id,
                kind: SubstrateKind::Entity,
                namespace: "ns:test".to_string(),
                field: "body".to_string(),
                embedding_model: None,
                vectors: vec![vec![0.0f32; dims + 2]],
                text_fingerprint: None,
                updated_at: chrono::Utc::now(),
            },
        ])
        .await
        .expect("insert_batch must return Ok (best-effort semantics)");

    assert_eq!(summary.attempted, 2);
    assert_eq!(
        summary.failed, 2,
        "both wrong-dims records must be counted as failed"
    );
    assert_eq!(summary.affected, 0);
    assert!(
        !summary.first_error.is_empty(),
        "first_error must be populated when failed > 0; \
             got empty string; the validation error is silently swallowed"
    );
    assert_eq!(summary.errors.len(), 2);
    assert_eq!(summary.errors[0].index, 0);
    assert_eq!(summary.errors[1].index, 1);
    assert_eq!(summary.errors[0].item_id, Some(first_id.to_string()));
    assert_eq!(summary.errors[1].item_id, Some(second_id.to_string()));
    assert!(summary.errors.iter().all(|error| {
        error.class == khive_storage::BatchWriteErrorClass::InvalidInput
            && error.retryability == khive_storage::BatchWriteRetryability::Permanent
    }));
    assert!(!summary.errors_truncated);
    assert_eq!(summary.errors_omitted, 0);
    assert_eq!(summary.error_counts.len(), 1);
    assert_eq!(summary.error_counts[0].count, 2);
}
