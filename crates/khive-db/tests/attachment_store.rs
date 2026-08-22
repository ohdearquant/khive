use std::sync::Arc;

use khive_db::pool::{ConnectionPool, PoolConfig};
use khive_db::stores::attachment::SqlAttachmentStore;
use khive_storage::{Attachment, AttachmentStore, AttachmentSubstrate, ContentRef, StorageError};
use uuid::Uuid;

const ATTACHMENTS_DDL: &str = r#"
CREATE TABLE attachments (
    record_uuid TEXT NOT NULL,
    substrate   TEXT NOT NULL CHECK (substrate IN ('entity', 'note')),
    role        TEXT NOT NULL,
    content_ref TEXT NOT NULL,
    media_type  TEXT,
    size_bytes  INTEGER,
    created_at  INTEGER NOT NULL,
    PRIMARY KEY (record_uuid, role)
);
CREATE INDEX idx_attachments_content_ref ON attachments(content_ref);
"#;

fn setup() -> (Arc<ConnectionPool>, SqlAttachmentStore) {
    let pool = Arc::new(
        ConnectionPool::new(PoolConfig {
            path: None,
            ..PoolConfig::default()
        })
        .expect("pool"),
    );
    pool.writer()
        .expect("writer")
        .conn()
        .execute_batch(ATTACHMENTS_DDL)
        .expect("attachment schema");
    let store = SqlAttachmentStore::new(pool.clone(), false);
    (pool, store)
}

fn attachment(record_uuid: Uuid, role: &str, byte: char) -> Attachment {
    Attachment {
        record_uuid,
        substrate: AttachmentSubstrate::Entity,
        role: role.to_string(),
        content_ref: ContentRef::from_hex(byte.to_string().repeat(64)).expect("canonical ref"),
        media_type: Some("application/octet-stream".to_string()),
        size_bytes: Some(42),
        created_at: 123,
    }
}

#[tokio::test]
async fn attachment_store_crud_is_role_keyed_and_stably_listed() {
    let (_pool, store) = setup();
    let record_uuid = Uuid::new_v4();
    let content = attachment(record_uuid, "content", 'a');
    let network = attachment(record_uuid, "fann-network", 'b');

    store
        .upsert_attachment(network.clone())
        .await
        .expect("insert network");
    store
        .upsert_attachment(content.clone())
        .await
        .expect("insert content");

    assert_eq!(
        store
            .get_attachment(record_uuid, "content")
            .await
            .expect("get content"),
        Some(content.clone())
    );
    assert_eq!(
        store
            .list_attachments(record_uuid)
            .await
            .expect("list attachments"),
        vec![content.clone(), network]
    );

    let replacement = attachment(record_uuid, "content", 'c');
    store
        .upsert_attachment(replacement.clone())
        .await
        .expect("replace content role");
    assert_eq!(
        store
            .get_attachment(record_uuid, "content")
            .await
            .expect("get replacement"),
        Some(replacement)
    );

    assert!(store
        .delete_attachment(record_uuid, "content")
        .await
        .expect("delete content"));
    assert!(!store
        .delete_attachment(record_uuid, "content")
        .await
        .expect("idempotent missing delete"));
}

#[tokio::test]
async fn attachment_store_rejects_invalid_roles_before_sql() {
    let (_pool, store) = setup();
    let record_uuid = Uuid::new_v4();

    let error = store
        .upsert_attachment(attachment(record_uuid, "bad\nrole", 'd'))
        .await
        .expect_err("control-bearing role must fail");
    assert!(matches!(error, StorageError::InvalidInput { .. }));

    let error = store
        .get_attachment(record_uuid, "")
        .await
        .expect_err("empty lookup role must fail");
    assert!(matches!(error, StorageError::InvalidInput { .. }));
}

#[tokio::test]
async fn attachment_store_rejects_corrupt_negative_size_rows() {
    let (pool, store) = setup();
    let record_uuid = Uuid::new_v4();
    pool.writer()
        .expect("writer")
        .conn()
        .execute(
            "INSERT INTO attachments \
             (record_uuid, substrate, role, content_ref, size_bytes, created_at) \
             VALUES (?1, 'entity', 'content', ?2, -1, 123)",
            rusqlite::params![record_uuid.to_string(), "e".repeat(64)],
        )
        .expect("corrupt fixture");

    store
        .get_attachment(record_uuid, "content")
        .await
        .expect_err("negative persisted size must fail closed");
}
