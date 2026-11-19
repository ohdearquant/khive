use std::num::NonZeroU32;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use khive_storage::{
    encode_f32_native, scope_request_read_cancellation, scope_request_read_deadline,
    StorageCapability, StorageError, VectorScanPage, VectorScanRequest, VectorStore,
};
use khive_types::SubstrateKind;
use uuid::Uuid;

use super::SqliteVecStore;
use crate::pool::{ConnectionPool, PoolConfig};

const MODEL: &str = "scan_test";

fn pool(path: Option<&Path>) -> Arc<ConnectionPool> {
    crate::extension::ensure_extensions_loaded();
    Arc::new(
        ConnectionPool::new(PoolConfig {
            path: path.map(Path::to_owned),
            write_queue_enabled: Some(false),
            ..PoolConfig::for_test()
        })
        .unwrap(),
    )
}

fn store(pool: &Arc<ConnectionPool>, dimensions: usize) -> Arc<dyn VectorStore> {
    Arc::new(
        SqliteVecStore::new(
            Arc::clone(pool),
            false,
            MODEL.into(),
            MODEL.into(),
            dimensions,
            "local".into(),
        )
        .unwrap(),
    )
}

fn schema(pool: &Arc<ConnectionPool>, ordinary: bool) {
    let writer = pool.try_writer().unwrap();
    let conn = writer.conn();
    if ordinary {
        // vec0 correctly rejects malformed dimensions at insertion. A corrupt
        // persisted table with the same columns exercises the reader boundary.
        conn.execute_batch(
            "CREATE TABLE vec_scan_test (
            subject_id TEXT PRIMARY KEY, namespace TEXT, kind TEXT, field TEXT,
            embedding_model TEXT, embedding BLOB)",
        )
        .unwrap();
    } else {
        conn.execute_batch(
            "CREATE VIRTUAL TABLE vec_scan_test USING vec0(
            subject_id TEXT PRIMARY KEY, namespace TEXT NOT NULL, kind TEXT NOT NULL,
            field TEXT NOT NULL, embedding_model TEXT NOT NULL,
            embedding float[4] distance_metric=cosine)",
        )
        .unwrap();
    }
    conn.execute_batch(crate::migrations::VECTOR_PROVENANCE_DDL)
        .unwrap();
    conn.execute_batch(crate::migrations::ANN_WRITE_LOG_DDL)
        .unwrap();
}

fn seed(
    pool: &Arc<ConnectionPool>,
    id: &str,
    namespace: &str,
    model: &str,
    field: &str,
    kind: &str,
    bytes: &[u8],
) {
    pool.try_writer()
        .unwrap()
        .conn()
        .execute(
            "INSERT INTO vec_scan_test
         (subject_id, namespace, embedding_model, field, kind, embedding)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![id, namespace, model, field, kind, bytes],
        )
        .unwrap();
}

fn request(limit: u32) -> VectorScanRequest {
    VectorScanRequest {
        namespace: "local".into(),
        embedding_model: MODEL.into(),
        field: "body".into(),
        kind: None,
        after: None,
        limit: NonZeroU32::new(limit).unwrap(),
    }
}

fn ids(page: &VectorScanPage) -> Vec<Uuid> {
    page.items.iter().map(|item| item.subject_id).collect()
}

fn assert_conversion(error: StorageError, index: usize, data_type: rusqlite::types::Type) {
    match error {
        StorageError::Driver {
            capability,
            operation,
            source,
        } => {
            assert_eq!(capability, StorageCapability::Vectors);
            assert_eq!(operation, "vec_scan_vectors");
            match source.downcast_ref::<rusqlite::Error>().unwrap() {
                rusqlite::Error::FromSqlConversionFailure(actual_index, actual_type, _) => {
                    assert_eq!(*actual_index, index);
                    assert_eq!(*actual_type, data_type);
                }
                other => panic!("unexpected native source: {other:?}"),
            }
        }
        other => panic!("unexpected storage error: {other:?}"),
    }
}

#[tokio::test]
async fn scan_reopened_file_returns_all_stored_bits_in_uuid_order() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("vectors.db");
    let values = [0.1_f32, -0.0, f32::MIN_POSITIVE, -3.5];
    {
        let pool = pool(Some(&path));
        schema(&pool, false);
        let store = store(&pool, 4);
        for id in [40, 10, 30, 20] {
            store
                .insert(
                    Uuid::from_u128(id),
                    SubstrateKind::Entity,
                    "local",
                    "body",
                    vec![values.to_vec()],
                )
                .await
                .unwrap();
        }
    }
    let pool = pool(Some(&path));
    let store = store(&pool, 4);
    let observation = pool.observe_test_statement_starts(100).unwrap();
    let mut query = request(2);
    let first = store.scan_vectors(query.clone()).await.unwrap();
    assert_eq!(ids(&first), vec![Uuid::from_u128(10), Uuid::from_u128(20)]);
    assert_eq!(first.next_after, Some(Uuid::from_u128(20)));
    query.after = first.next_after;
    let second = store.scan_vectors(query).await.unwrap();
    assert_eq!(ids(&second), vec![Uuid::from_u128(30), Uuid::from_u128(40)]);
    assert_eq!(
        second.next_after, None,
        "an exactly full terminal page has no continuation"
    );
    for item in first.items.iter().chain(second.items.iter()) {
        assert_eq!(item.vectors.len(), 1);
        assert_eq!(
            item.vectors[0]
                .iter()
                .map(|v| v.to_bits())
                .collect::<Vec<_>>(),
            values.map(f32::to_bits)
        );
    }
    let statements = observation.started_statements().unwrap();
    let scan_statements: Vec<_> = statements
        .iter()
        .filter(|s| {
            s.sql
                .starts_with("SELECT subject_id, embedding FROM vec_scan_test ")
        })
        .collect();
    assert_eq!(scan_statements.len(), 2, "one stored-row SELECT per page");
}

#[tokio::test]
async fn scan_filters_exact_metadata_before_limit_in_memory() {
    let pool = pool(None);
    schema(&pool, false);
    let bytes = encode_f32_native(&[1.0, 0.0, 0.0, 0.0]);
    for id in 1..=160 {
        let (namespace, model, field, kind) = match id % 4 {
            0 => ("other", MODEL, "body", "note"),
            1 => ("local", "other", "body", "note"),
            2 => ("local", MODEL, "other", "note"),
            _ => ("local", MODEL, "body", "entity"),
        };
        seed(
            &pool,
            &Uuid::from_u128(id).to_string(),
            namespace,
            model,
            field,
            kind,
            &bytes,
        );
    }
    seed(
        &pool,
        &Uuid::from_u128(200).to_string(),
        "local",
        MODEL,
        "body",
        "note",
        &bytes,
    );
    seed(
        &pool,
        &Uuid::from_u128(300).to_string(),
        "local",
        MODEL,
        "body",
        "note",
        &bytes,
    );
    let store = store(&pool, 4);
    let mut query = request(1);
    query.kind = Some(SubstrateKind::Note);
    let first = store.scan_vectors(query.clone()).await.unwrap();
    assert_eq!(ids(&first), vec![Uuid::from_u128(200)]);
    assert_eq!(first.items[0].vectors, vec![vec![1.0, 0.0, 0.0, 0.0]]);
    assert_eq!(first.next_after, Some(Uuid::from_u128(200)));
    query.after = first.next_after;
    let last = store.scan_vectors(query.clone()).await.unwrap();
    assert_eq!(ids(&last), vec![Uuid::from_u128(300)]);
    assert_eq!(last.next_after, None);
    query.after = None;
    query.kind = None;
    assert_eq!(
        ids(&store.scan_vectors(query.clone()).await.unwrap()),
        vec![Uuid::from_u128(3)]
    );
    query.embedding_model = "other".into();
    assert_eq!(
        ids(&store.scan_vectors(query).await.unwrap()),
        vec![Uuid::from_u128(1)],
        "the required model is an exact row filter, not the store's default"
    );
}

#[tokio::test]
async fn scan_empty_and_punctuation_selectors_are_bound_literals() {
    let pool = pool(None);
    schema(&pool, false);
    let bytes = encode_f32_native(&[1.0, 0.0, 0.0, 0.0]);
    seed(
        &pool,
        &Uuid::from_u128(1).to_string(),
        "",
        "m'_%",
        "x' OR 1=1 --",
        "entity",
        &bytes,
    );
    seed(
        &pool,
        &Uuid::from_u128(2).to_string(),
        " ",
        "m'_%",
        "x' OR 1=1 --",
        "entity",
        &bytes,
    );
    let store = store(&pool, 4);
    let mut query = request(u32::MAX);
    query.namespace.clear();
    query.embedding_model = "m'_%".into();
    query.field = "x' OR 1=1 --".into();
    let page = store.scan_vectors(query.clone()).await.unwrap();
    assert_eq!(ids(&page), vec![Uuid::from_u128(1)]);
    assert_eq!(page.next_after, None);
    query.field = "x%".into();
    assert!(store.scan_vectors(query).await.unwrap().items.is_empty());
}

#[tokio::test]
async fn scan_cursor_extremes_are_exclusive_without_boundary_row_lookup() {
    let pool = pool(None);
    schema(&pool, false);
    let store = store(&pool, 4);
    assert_eq!(
        store.scan_vectors(request(1)).await.unwrap().next_after,
        None
    );
    for id in [Uuid::from_u128(u128::MAX), Uuid::nil(), Uuid::from_u128(2)] {
        store
            .insert(
                id,
                SubstrateKind::Entity,
                "local",
                "body",
                vec![vec![1.0, 0.0, 0.0, 0.0]],
            )
            .await
            .unwrap();
    }
    let first = store.scan_vectors(request(1)).await.unwrap();
    assert_eq!(ids(&first), vec![Uuid::nil()]);
    assert_eq!(first.next_after, Some(Uuid::nil()));
    let mut query = request(1);
    query.after = Some(Uuid::from_u128(1));
    let second = store.scan_vectors(query.clone()).await.unwrap();
    assert_eq!(ids(&second), vec![Uuid::from_u128(2)]);
    assert_eq!(second.next_after, Some(Uuid::from_u128(2)));
    store.delete(Uuid::from_u128(2)).await.unwrap();
    query.after = second.next_after;
    let last = store.scan_vectors(query.clone()).await.unwrap();
    assert_eq!(ids(&last), vec![Uuid::from_u128(u128::MAX)]);
    assert_eq!(last.next_after, None);
    query.after = Some(Uuid::from_u128(u128::MAX));
    let empty = store.scan_vectors(query).await.unwrap();
    assert!(empty.items.is_empty());
    assert_eq!(empty.next_after, None);
}

#[tokio::test]
async fn scan_pages_observe_later_mutations_without_repeating_emitted_ids() {
    let pool = pool(None);
    schema(&pool, false);
    let store = store(&pool, 4);
    let old = vec![1.0, 0.0, 0.0, 0.0];
    for id in [10, 20, 30, 40] {
        store
            .insert(
                Uuid::from_u128(id),
                SubstrateKind::Entity,
                "local",
                "body",
                vec![old.clone()],
            )
            .await
            .unwrap();
    }
    let first = store.scan_vectors(request(2)).await.unwrap();
    assert_eq!(ids(&first), vec![Uuid::from_u128(10), Uuid::from_u128(20)]);
    store.delete(Uuid::from_u128(20)).await.unwrap();
    store.delete(Uuid::from_u128(30)).await.unwrap();
    for id in [15, 25] {
        store
            .insert(
                Uuid::from_u128(id),
                SubstrateKind::Entity,
                "local",
                "body",
                vec![old.clone()],
            )
            .await
            .unwrap();
    }
    let replacement = vec![0.0, -1.0, 0.5, 0.0];
    store
        .insert(
            Uuid::from_u128(40),
            SubstrateKind::Entity,
            "local",
            "body",
            vec![replacement.clone()],
        )
        .await
        .unwrap();
    let mut query = request(2);
    query.after = first.next_after;
    let page = store.scan_vectors(query.clone()).await.unwrap();
    assert_eq!(ids(&page), vec![Uuid::from_u128(25), Uuid::from_u128(40)]);
    assert_eq!(page.items[1].vectors, vec![replacement]);
    assert_eq!(page.next_after, None);
    store
        .insert(
            Uuid::from_u128(40),
            SubstrateKind::Entity,
            "local",
            "other",
            vec![old],
        )
        .await
        .unwrap();
    assert_eq!(
        ids(&store.scan_vectors(query.clone()).await.unwrap()),
        vec![Uuid::from_u128(25)]
    );
    store.delete(Uuid::from_u128(25)).await.unwrap();
    assert!(store.scan_vectors(query).await.unwrap().items.is_empty());
}

#[tokio::test]
async fn scan_does_not_require_or_hide_deleted_owner_rows() {
    let pool = pool(None);
    schema(&pool, false);
    let store = store(&pool, 4);
    let id = Uuid::from_u128(2);
    for subject_id in [Uuid::from_u128(1), id] {
        store
            .insert(
                subject_id,
                SubstrateKind::Entity,
                "local",
                "body",
                vec![vec![1.0, 0.0, 0.0, 0.0]],
            )
            .await
            .unwrap();
    }
    let first = store.scan_vectors(request(1)).await.unwrap();
    assert_eq!(ids(&first), vec![Uuid::from_u128(1)]);
    assert_eq!(first.next_after, Some(Uuid::from_u128(1)));
    {
        let writer = pool.try_writer().unwrap();
        writer
            .conn()
            .execute_batch(include_str!("../../../sql/entities-ddl.sql"))
            .unwrap();
        writer.conn().execute("INSERT INTO entities (id, namespace, kind, name, created_at, updated_at) VALUES (?1, 'local', 'concept', 'owner', 1, 1)", [id.to_string()]).unwrap();
        writer.conn().execute("UPDATE entities SET deleted_at = 2, updated_at = 2, version = version + 1 WHERE id = ?1", [id.to_string()]).unwrap();
    }
    let mut query = request(1);
    query.after = first.next_after;
    assert_eq!(
        ids(&store.scan_vectors(query.clone()).await.unwrap()),
        vec![id]
    );
    assert!(store.delete(id).await.unwrap());
    let page = store.scan_vectors(query).await.unwrap();
    assert!(page.items.is_empty());
    assert_eq!(page.next_after, None);
}

#[tokio::test]
async fn scan_refuses_invalid_and_noncanonical_ids_including_lookahead() {
    for spelling in [
        "not-a-uuid".to_string(),
        Uuid::from_u128(0xaa).to_string().to_uppercase(),
    ] {
        let pool = pool(None);
        schema(&pool, true);
        let bytes = encode_f32_native(&[1.0, 0.0, 0.0, 0.0]);
        seed(
            &pool,
            &Uuid::from_u128(1).to_string(),
            "local",
            MODEL,
            "body",
            "entity",
            &bytes,
        );
        seed(&pool, &spelling, "local", MODEL, "body", "entity", &bytes);
        let store = store(&pool, 4);
        assert_conversion(
            store.scan_vectors(request(1)).await.unwrap_err(),
            0,
            rusqlite::types::Type::Text,
        );
        pool.try_writer()
            .unwrap()
            .conn()
            .execute(
                "DELETE FROM vec_scan_test WHERE subject_id = ?1",
                [&spelling],
            )
            .unwrap();
        let healthy = store.scan_vectors(request(1)).await.unwrap();
        assert_eq!(ids(&healthy), vec![Uuid::from_u128(1)]);
        assert_eq!(healthy.next_after, None);
    }
}

#[tokio::test]
async fn scan_refuses_wrong_blob_length_even_when_only_lookahead_is_corrupt() {
    for bytes in [vec![0_u8; 15], vec![0_u8; 20]] {
        let pool = pool(None);
        schema(&pool, true);
        seed(
            &pool,
            &Uuid::from_u128(1).to_string(),
            "local",
            MODEL,
            "body",
            "entity",
            &encode_f32_native(&[1.0, 0.0, 0.0, 0.0]),
        );
        seed(
            &pool,
            &Uuid::from_u128(2).to_string(),
            "local",
            MODEL,
            "body",
            "entity",
            &bytes,
        );
        assert_conversion(
            store(&pool, 4).scan_vectors(request(1)).await.unwrap_err(),
            1,
            rusqlite::types::Type::Blob,
        );
    }
}

#[tokio::test]
async fn scan_dimension_overflow_is_a_driver_conversion_error_before_sql() {
    let pool = pool(None);
    assert_conversion(
        store(&pool, usize::MAX)
            .scan_vectors(request(1))
            .await
            .unwrap_err(),
        1,
        rusqlite::types::Type::Blob,
    );
}

#[tokio::test]
async fn scan_cancellation_and_expired_deadline_precede_missing_table_then_recover() {
    let pool = pool(None);
    let store = store(&pool, 4);
    let (_sender, receiver) = tokio::sync::watch::channel(true);
    let cancelled = scope_request_read_cancellation(receiver, store.scan_vectors(request(1)))
        .await
        .unwrap_err();
    assert!(
        matches!(cancelled, StorageError::Timeout { operation } if operation == "vec_scan_vectors")
    );
    let expired = scope_request_read_deadline(Duration::ZERO, store.scan_vectors(request(1)))
        .await
        .unwrap_err();
    assert!(
        matches!(expired, StorageError::Timeout { operation } if operation == "vec_scan_vectors")
    );
    let missing = store.scan_vectors(request(1)).await.unwrap_err();
    match missing {
        StorageError::Driver {
            capability: StorageCapability::Vectors,
            operation,
            source,
        } => {
            assert_eq!(operation, "vec_scan_vectors");
            assert_eq!(
                source
                    .downcast_ref::<rusqlite::Error>()
                    .unwrap()
                    .sqlite_error()
                    .unwrap()
                    .extended_code,
                rusqlite::ffi::SQLITE_ERROR
            );
        }
        other => panic!("unexpected missing-table error: {other:?}"),
    }
    schema(&pool, false);
    store
        .insert(
            Uuid::from_u128(1),
            SubstrateKind::Note,
            "local",
            "body",
            vec![vec![1.0, 0.0, 0.0, 0.0]],
        )
        .await
        .unwrap();
    assert_eq!(
        ids(&store.scan_vectors(request(1)).await.unwrap()),
        vec![Uuid::from_u128(1)]
    );
}
