use std::sync::Arc;

use super::*;
use crate::pool::{ConnectionPool, PoolConfig};

fn make_store() -> (Arc<ConnectionPool>, SqliteVecStore) {
    crate::extension::ensure_extensions_loaded();
    let pool = Arc::new(
        ConnectionPool::new(PoolConfig {
            path: None,
            ..PoolConfig::default()
        })
        .expect("in-memory pool"),
    );
    {
        let writer = pool.try_writer().expect("writer");
        writer
            .conn()
            .execute_batch(
                "CREATE VIRTUAL TABLE vec_provenance_test USING vec0(\
                     subject_id TEXT PRIMARY KEY, namespace TEXT NOT NULL, \
                     kind TEXT NOT NULL, field TEXT NOT NULL, \
                     embedding_model TEXT NOT NULL, embedding float[2] distance_metric=cosine)",
            )
            .expect("create vec0 table");
        writer
            .conn()
            .execute_batch(crate::migrations::ANN_WRITE_LOG_DDL)
            .expect("create ANN log");
        writer
            .conn()
            .execute_batch(crate::migrations::VECTOR_PROVENANCE_DDL)
            .expect("create provenance table");
    }
    let store = SqliteVecStore::new(
        Arc::clone(&pool),
        false,
        "provenance_test".into(),
        "model/a".into(),
        2,
        "ns:test".into(),
    )
    .expect("vector store");
    (pool, store)
}

fn record(subject_id: Uuid, text: &str, updated_at: &DateTime<Utc>) -> VectorRecord {
    VectorRecord {
        subject_id,
        kind: SubstrateKind::Entity,
        namespace: "ns:test".into(),
        field: "entity.body".into(),
        embedding_model: Some("model/a".into()),
        vectors: vec![vec![0.1, 0.2]],
        text_fingerprint: Some(VectorRecord::fingerprint_text(text)),
        updated_at: *updated_at,
    }
}

fn joined_provenance_count(
    conn: &rusqlite::Connection,
    table: &str,
    model_key: &str,
    subject_id: Uuid,
    namespace: &str,
) -> i64 {
    let sql = format!(
        "SELECT COUNT(embedding_digest) FROM ({})",
        provenance_read_sql(table, true)
    );
    conn.query_row(
        &sql,
        rusqlite::params![model_key, subject_id.to_string(), namespace],
        |row| row.get(0),
    )
    .expect("count sidecar matches through production read join")
}

#[tokio::test]
async fn persisted_vector_provenance_tracks_exact_embedded_text() {
    let (pool, store) = make_store();
    let subject_id = Uuid::new_v4();
    let timestamp = DateTime::parse_from_rfc3339("2026-09-25T12:34:56Z")
        .unwrap()
        .with_timezone(&Utc);
    let original_text = "rendered title\nbody";
    store
        .insert_batch(vec![record(subject_id, original_text, &timestamp)])
        .await
        .expect("index original text");
    let expected = ContentRef::from_digest_bytes(blake3::hash(original_text.as_bytes()).as_bytes());
    let stored = store.provenance(subject_id).await.unwrap().unwrap();
    assert_eq!(stored.embedding_model, "model/a");
    assert_eq!(stored.field, "entity.body");
    assert_eq!(stored.text_fingerprint, Some(expected.clone()));
    let raw: String = pool
        .try_writer()
        .unwrap()
        .conn()
        .query_row(
            "SELECT text_fingerprint FROM vector_provenance \
                 WHERE model_key = 'provenance_test' AND subject_id = ?1",
            [subject_id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(raw, expected.as_str());
    let (stored_digest, live_embedding): (String, Vec<u8>) = pool
        .try_writer()
        .unwrap()
        .conn()
        .query_row(
            "SELECT p.embedding_digest, v.embedding \
                 FROM vector_provenance AS p JOIN vec_provenance_test AS v \
                   ON v.subject_id = p.subject_id \
                 WHERE p.model_key = 'provenance_test' AND p.subject_id = ?1",
            [subject_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        stored_digest,
        blake3::hash(&live_embedding).to_hex().to_string()
    );

    store
        .insert_batch(vec![record(subject_id, original_text, &timestamp)])
        .await
        .expect("re-embed unchanged text");
    assert_eq!(
        store
            .provenance(subject_id)
            .await
            .unwrap()
            .unwrap()
            .text_fingerprint,
        Some(expected.clone())
    );

    store
        .insert_batch(vec![record(
            subject_id,
            "rendered title\nchanged body",
            &timestamp,
        )])
        .await
        .expect("re-embed changed text");
    let changed = store.provenance(subject_id).await.unwrap().unwrap();
    assert_eq!(
        changed.text_fingerprint,
        Some(ContentRef::from_digest_bytes(
            blake3::hash(b"rendered title\nchanged body").as_bytes()
        ))
    );
    assert_ne!(changed.text_fingerprint, Some(expected));
}

#[tokio::test]
async fn bypass_vector_replacement_makes_stale_sidecar_unknown() {
    let (pool, store) = make_store();
    let subject_id = Uuid::new_v4();
    let timestamp = DateTime::parse_from_rfc3339("2026-09-25T12:34:56Z")
        .unwrap()
        .with_timezone(&Utc);
    store
        .insert_batch(vec![record(subject_id, "original text", &timestamp)])
        .await
        .expect("index original text");

    {
        let writer = pool.try_writer().unwrap();
        writer
            .conn()
            .execute(
                "DELETE FROM vec_provenance_test WHERE subject_id = ?1",
                [subject_id.to_string()],
            )
            .unwrap();
        writer
            .conn()
            .execute(
                "INSERT INTO vec_provenance_test \
                     (subject_id, namespace, kind, field, embedding_model, embedding) \
                     VALUES (?1, 'ns:test', 'entity', 'entity.body', 'model/a', ?2)",
                rusqlite::params![subject_id.to_string(), f32_slice_as_bytes(&[0.7_f32, 0.8])],
            )
            .unwrap();
    }
    let present = store.provenance(subject_id).await.unwrap().unwrap();
    assert_eq!(present.embedding_model, "model/a");
    assert_eq!(present.field, "entity.body");
    assert_eq!(present.text_fingerprint, None);
    assert_eq!(present.updated_at, None);
}

#[tokio::test]
async fn model_scoped_identical_blob_provenance() {
    let (pool, first_store) = make_store();
    pool.try_writer()
        .unwrap()
        .conn()
        .execute_batch(
            "CREATE VIRTUAL TABLE vec_provenance_other USING vec0(\
                 subject_id TEXT PRIMARY KEY, namespace TEXT NOT NULL, \
                 kind TEXT NOT NULL, field TEXT NOT NULL, \
                 embedding_model TEXT NOT NULL, embedding float[2] distance_metric=cosine)",
        )
        .unwrap();
    let second_store = SqliteVecStore::new(
        Arc::clone(&pool),
        false,
        "provenance_other".into(),
        "model/b".into(),
        2,
        "ns:test".into(),
    )
    .unwrap();
    let subject_id = Uuid::new_v4();
    let first_timestamp = DateTime::parse_from_rfc3339("2026-09-25T12:34:56.123456789Z")
        .unwrap()
        .with_timezone(&Utc);
    let second_timestamp = DateTime::parse_from_rfc3339("2026-09-25T12:34:57.987654321Z")
        .unwrap()
        .with_timezone(&Utc);
    first_store
        .insert_batch(vec![record(subject_id, "model a source", &first_timestamp)])
        .await
        .unwrap();
    let mut other_record = record(subject_id, "model b source", &second_timestamp);
    other_record.embedding_model = Some("model/b".into());
    second_store.insert_batch(vec![other_record]).await.unwrap();

    let first = first_store.provenance(subject_id).await.unwrap().unwrap();
    let second = second_store.provenance(subject_id).await.unwrap().unwrap();
    assert_eq!(
        first.text_fingerprint,
        Some(VectorRecord::fingerprint_text("model a source"))
    );
    assert_eq!(first.updated_at, Some(first_timestamp));
    assert_eq!(
        second.text_fingerprint,
        Some(VectorRecord::fingerprint_text("model b source"))
    );
    assert_eq!(second.updated_at, Some(second_timestamp));

    let writer = pool.try_writer().unwrap();
    let first_blob: Vec<u8> = writer
        .conn()
        .query_row(
            "SELECT embedding FROM vec_provenance_test WHERE subject_id = ?1",
            [subject_id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    let second_blob: Vec<u8> = writer
        .conn()
        .query_row(
            "SELECT embedding FROM vec_provenance_other WHERE subject_id = ?1",
            [subject_id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        first_blob, second_blob,
        "the digests alone cannot separate models"
    );
    assert_eq!(
        joined_provenance_count(
            writer.conn(),
            "vec_provenance_test",
            "provenance_test",
            subject_id,
            "ns:test"
        ),
        1
    );
    assert_eq!(
        joined_provenance_count(
            writer.conn(),
            "vec_provenance_other",
            "provenance_other",
            subject_id,
            "ns:test"
        ),
        1
    );
}

#[tokio::test]
async fn namespace_scoped_identical_blob_provenance() {
    let (pool, source_store) = make_store();
    let subject_id = Uuid::new_v4();
    let timestamp = DateTime::parse_from_rfc3339("2026-09-25T12:34:56.123456789Z")
        .unwrap()
        .with_timezone(&Utc);
    source_store
        .insert_batch(vec![record(subject_id, "source namespace", &timestamp)])
        .await
        .unwrap();

    {
        let writer = pool.try_writer().unwrap();
        let live_blob: Vec<u8> = writer
            .conn()
            .query_row(
                "SELECT embedding FROM vec_provenance_test WHERE subject_id = ?1",
                [subject_id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        writer.conn().execute_batch("BEGIN IMMEDIATE").unwrap();
        writer
            .conn()
            .execute(
                "DELETE FROM vec_provenance_test WHERE subject_id = ?1",
                [subject_id.to_string()],
            )
            .unwrap();
        writer
            .conn()
            .execute(
                "INSERT INTO vec_provenance_test \
                     (subject_id, namespace, kind, field, embedding_model, embedding) \
                     VALUES (?1, 'ns:other', 'entity', 'entity.body', 'model/a', ?2)",
                rusqlite::params![subject_id.to_string(), live_blob],
            )
            .unwrap();
        writer.conn().execute_batch("COMMIT").unwrap();
        let sidecar_namespace: String = writer
            .conn()
            .query_row(
                "SELECT namespace FROM vector_provenance \
                     WHERE model_key = 'provenance_test' AND subject_id = ?1",
                [subject_id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(sidecar_namespace, "ns:test");
        assert_eq!(
            joined_provenance_count(
                writer.conn(),
                "vec_provenance_test",
                "provenance_test",
                subject_id,
                "ns:other"
            ),
            0
        );
    }

    let destination_store = SqliteVecStore::new(
        Arc::clone(&pool),
        false,
        "provenance_test".into(),
        "model/a".into(),
        2,
        "ns:other".into(),
    )
    .unwrap();
    assert!(source_store.provenance(subject_id).await.unwrap().is_none());
    let moved = destination_store
        .provenance(subject_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(moved.embedding_model, "model/a");
    assert_eq!(moved.field, "entity.body");
    assert_eq!(moved.text_fingerprint, None);
    assert_eq!(moved.updated_at, None);
}

#[tokio::test]
async fn vector_provenance_timestamp_round_trips_and_unattributed_update_clears_it() {
    let (_, store) = make_store();
    let subject_id = Uuid::new_v4();
    let timestamp = DateTime::parse_from_rfc3339("2026-09-25T12:34:56.123456789Z")
        .unwrap()
        .with_timezone(&Utc);
    store
        .insert_batch(vec![record(subject_id, "indexed body", &timestamp)])
        .await
        .expect("index record");
    let persisted = store.provenance(subject_id).await.unwrap().unwrap();
    assert_eq!(persisted.updated_at, Some(timestamp));

    store
        .update(
            subject_id,
            SubstrateKind::Entity,
            "ns:test",
            "entity.body",
            vec![vec![0.3, 0.4]],
        )
        .await
        .expect("replace without source text");
    let replaced = store.provenance(subject_id).await.unwrap().unwrap();
    assert_eq!(replaced.text_fingerprint, None);
    assert_eq!(replaced.updated_at, None);
}

#[tokio::test]
async fn low_level_same_blob_reinsert_clears_provenance() {
    let (pool, store) = make_store();
    let timestamp = DateTime::parse_from_rfc3339("2026-09-25T12:34:56.123456789Z")
        .unwrap()
        .with_timezone(&Utc);
    for operation in ["insert", "update", "insert_exact_only"] {
        let subject_id = Uuid::new_v4();
        store
            .insert_batch(vec![record(subject_id, "attributed source", &timestamp)])
            .await
            .unwrap();
        let before: Vec<u8> = pool
            .try_writer()
            .unwrap()
            .conn()
            .query_row(
                "SELECT embedding FROM vec_provenance_test WHERE subject_id = ?1",
                [subject_id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        match operation {
            "insert" => {
                store
                    .insert(
                        subject_id,
                        SubstrateKind::Entity,
                        "ns:test",
                        "entity.body",
                        vec![vec![0.1, 0.2]],
                    )
                    .await
                    .unwrap();
            }
            "update" => {
                store
                    .update(
                        subject_id,
                        SubstrateKind::Entity,
                        "ns:test",
                        "entity.body",
                        vec![vec![0.1, 0.2]],
                    )
                    .await
                    .unwrap();
            }
            "insert_exact_only" => {
                store
                    .insert_exact_only(
                        subject_id,
                        SubstrateKind::Entity,
                        "ns:test",
                        "entity.body",
                        vec![vec![0.1, 0.2]],
                    )
                    .await
                    .unwrap();
            }
            _ => unreachable!(),
        }

        let writer = pool.try_writer().unwrap();
        let after: Vec<u8> = writer
            .conn()
            .query_row(
                "SELECT embedding FROM vec_provenance_test WHERE subject_id = ?1",
                [subject_id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(after, before, "{operation} must replace with the same BLOB");
        let fields: (Option<String>, Option<String>) = writer
            .conn()
            .query_row(
                "SELECT text_fingerprint, updated_at FROM vector_provenance \
                     WHERE model_key = 'provenance_test' AND subject_id = ?1",
                [subject_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            fields,
            (None, None),
            "{operation} must clear sidecar fields"
        );
        drop(writer);
        let observed = store.provenance(subject_id).await.unwrap().unwrap();
        assert_eq!(observed.text_fingerprint, None, "{operation}");
        assert_eq!(observed.updated_at, None, "{operation}");
    }
}

#[tokio::test]
async fn failed_replacement_rolls_back_vector_and_bound_provenance() {
    let (_, store) = make_store();
    let subject_id = Uuid::new_v4();
    let timestamp = DateTime::parse_from_rfc3339("2026-09-25T12:34:56Z")
        .unwrap()
        .with_timezone(&Utc);
    store
        .insert_batch(vec![record(subject_id, "original text", &timestamp)])
        .await
        .unwrap();
    let original = store.provenance(subject_id).await.unwrap().unwrap();
    let mut replacement = record(subject_id, "replacement text", &timestamp);
    replacement.vectors = vec![vec![0.7, 0.8]];

    let _guard = failpoint::FailpointGuard::new();
    let summary = store.insert_batch(vec![replacement]).await.unwrap();
    assert_eq!(summary.failed, 1);
    assert_eq!(summary.affected, 0);
    assert_eq!(
        store.provenance(subject_id).await.unwrap().unwrap(),
        original
    );
}

#[tokio::test]
async fn legacy_and_deleted_vectors_have_no_current_provenance() {
    let (pool, store) = make_store();
    let subject_id = Uuid::new_v4();
    {
        let writer = pool.try_writer().unwrap();
        writer
            .conn()
            .execute_batch("DROP TABLE vector_provenance")
            .expect("simulate pre-migration database");
        writer
            .conn()
            .execute(
                "INSERT INTO vec_provenance_test \
                     (subject_id, namespace, kind, field, embedding_model, embedding) \
                     VALUES (?1, 'ns:test', 'entity', 'entity.body', 'model/a', ?2)",
                rusqlite::params![subject_id.to_string(), f32_slice_as_bytes(&[0.1_f32, 0.2])],
            )
            .expect("write legacy vector without provenance");
        writer
            .conn()
            .execute_batch(crate::migrations::VECTOR_PROVENANCE_DDL)
            .expect("migrate provenance sidecar");
    }
    let legacy = store.provenance(subject_id).await.unwrap().unwrap();
    assert_eq!(legacy.text_fingerprint, None);
    assert_eq!(legacy.updated_at, None);

    let timestamp = DateTime::parse_from_rfc3339("2026-09-25T12:34:56Z")
        .unwrap()
        .with_timezone(&Utc);
    store
        .insert_batch(vec![record(subject_id, "new text", &timestamp)])
        .await
        .expect("replace legacy vector");
    let other_namespace = SqliteVecStore::new(
        Arc::clone(&pool),
        false,
        "provenance_test".into(),
        "model/a".into(),
        2,
        "ns:other".into(),
    )
    .unwrap();
    assert!(!other_namespace.delete(subject_id).await.unwrap());
    assert!(store
        .provenance(subject_id)
        .await
        .unwrap()
        .unwrap()
        .text_fingerprint
        .is_some());
    assert!(store.delete(subject_id).await.expect("delete vector"));
    assert!(store.provenance(subject_id).await.unwrap().is_none());
    let sidecar_count: i64 = pool
        .try_writer()
        .unwrap()
        .conn()
        .query_row(
            "SELECT count(*) FROM vector_provenance \
                 WHERE model_key = 'provenance_test' AND subject_id = ?1",
            [subject_id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(sidecar_count, 0);
}
