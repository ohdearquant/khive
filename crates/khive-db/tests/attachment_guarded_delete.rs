use std::sync::Arc;

use async_trait::async_trait;
use khive_db::pool::{ConnectionPool, PoolConfig};
use khive_db::stores::attachment::SqlAttachmentStore;
use khive_storage::{
    Attachment, AttachmentStore, AttachmentSubstrate, ContentRef, StorageCapability, StorageError,
    WriterTaskRequestState,
};
use uuid::Uuid;

fn attachment(record_uuid: Uuid, role: &str, byte: char) -> Attachment {
    Attachment {
        record_uuid,
        substrate: AttachmentSubstrate::Note,
        role: role.into(),
        content_ref: ContentRef::from_hex(byte.to_string().repeat(64)).unwrap(),
        media_type: Some("text/plain".into()),
        size_bytes: Some(7),
        created_at: 123,
    }
}

fn setup(queued: bool) -> (tempfile::TempDir, Arc<ConnectionPool>, SqlAttachmentStore) {
    let dir = tempfile::tempdir().unwrap();
    let pool = Arc::new(
        ConnectionPool::new(PoolConfig {
            path: Some(dir.path().join("guarded-delete.db")),
            write_queue_enabled: Some(queued),
            write_routing_strict: queued,
            ..PoolConfig::for_test()
        })
        .unwrap(),
    );
    pool.writer()
        .unwrap()
        .conn()
        .execute_batch(
            "CREATE TABLE attachments (
           record_uuid TEXT NOT NULL, substrate TEXT NOT NULL, role TEXT NOT NULL,
           content_ref TEXT NOT NULL, media_type TEXT, size_bytes INTEGER,
           created_at INTEGER NOT NULL, PRIMARY KEY(record_uuid, role));",
        )
        .unwrap();
    let store = SqlAttachmentStore::new(pool.clone(), true);
    (dir, pool, store)
}

#[tokio::test]
async fn guarded_delete_preserves_replacements_and_other_owners_on_both_write_routes() {
    for queued in [false, true] {
        let (_dir, pool, store) = setup(queued);
        assert_eq!(pool.writer_task_handle().unwrap().is_some(), queued);
        let id = Uuid::new_v4();
        let old = attachment(id, "quarantine-original", 'a');
        store.upsert_attachment(old.clone()).await.unwrap();
        let other_role = attachment(id, "content", 'a');
        let other_owner = attachment(Uuid::new_v4(), "quarantine-original", 'a');
        store.upsert_attachment(other_role.clone()).await.unwrap();
        store.upsert_attachment(other_owner.clone()).await.unwrap();
        let new = attachment(id, "quarantine-original", 'b');
        store.upsert_attachment(new.clone()).await.unwrap();
        assert!(!store
            .delete_attachment_if(id, &old.role, old.substrate, &old.content_ref)
            .await
            .unwrap());
        assert_eq!(
            store.get_attachment(id, &old.role).await.unwrap(),
            Some(new.clone())
        );
        assert!(!store
            .delete_attachment_if(id, &new.role, AttachmentSubstrate::Entity, &new.content_ref)
            .await
            .unwrap());
        assert_eq!(
            store.get_attachment(id, &new.role).await.unwrap(),
            Some(new.clone())
        );
        assert!(!store
            .delete_attachment_if(id, "absent", new.substrate, &new.content_ref)
            .await
            .unwrap());
        assert!(!store
            .delete_attachment_if(Uuid::new_v4(), &new.role, new.substrate, &new.content_ref)
            .await
            .unwrap());
        assert!(store
            .delete_attachment_if(id, &new.role, new.substrate, &new.content_ref)
            .await
            .unwrap());
        assert!(!store
            .delete_attachment_if(id, &new.role, new.substrate, &new.content_ref)
            .await
            .unwrap());
        assert_eq!(store.list_attachments(id).await.unwrap(), vec![other_role]);
        assert_eq!(
            store
                .get_attachment(other_owner.record_uuid, &other_owner.role)
                .await
                .unwrap(),
            Some(other_owner)
        );
    }
}

#[tokio::test]
async fn guarded_delete_rejects_invalid_roles_and_propagates_database_failure() {
    for queued in [false, true] {
        let (_dir, pool, store) = setup(queued);
        let row = attachment(Uuid::new_v4(), "content", 'c');
        store.upsert_attachment(row.clone()).await.unwrap();
        for role in ["", "bad\nrole"] {
            let error = store
                .delete_attachment_if(row.record_uuid, role, row.substrate, &row.content_ref)
                .await
                .unwrap_err();
            assert!(matches!(
                error,
                StorageError::InvalidInput {
                    capability: StorageCapability::Attachments,
                    ..
                }
            ));
        }
        assert_eq!(
            store
                .get_attachment(row.record_uuid, &row.role)
                .await
                .unwrap(),
            Some(row.clone())
        );
        // Use the same admitted writer route to install the failure fixture.
        let sql = khive_db::sql_bridge::SqlBridge::new(pool.clone(), true);
        use khive_storage::SqlAccess;
        sql.writer().await.unwrap().execute(khive_storage::types::SqlStatement {
            sql: "CREATE TRIGGER deny_delete BEFORE DELETE ON attachments BEGIN SELECT RAISE(ABORT, 'guarded-delete-fault'); END".into(),
            params: vec![], label: Some("guarded-delete-fault-fixture".into()),
        }).await.unwrap();
        let error = store
            .delete_attachment_if(row.record_uuid, &row.role, row.substrate, &row.content_ref)
            .await
            .expect_err("database failure must not become false");
        let error = if queued {
            let StorageError::WriterTaskRequestFailed {
                request_state: WriterTaskRequestState::TransactionRolledBack,
                source,
            } = error
            else {
                panic!("expected confirmed queue rollback, got {error:?}")
            };
            *source
        } else {
            error
        };
        // The native write failure carries its SQLite stage and codes beside the driver error.
        let StorageError::SqliteWrite { failure, source } = error else {
            panic!("expected staged native write failure, got {error:?}")
        };
        assert_eq!(
            failure.stage,
            khive_storage::error::SqliteWriteStage::Statement
        );
        assert_eq!(
            failure.extended_code,
            rusqlite::ffi::SQLITE_CONSTRAINT_TRIGGER
        );
        let error = *source;
        let StorageError::Driver {
            capability: StorageCapability::Attachments,
            operation,
            source,
        } = error
        else {
            panic!("expected preserved native attachment failure, got {error:?}")
        };
        assert_eq!(operation, "delete_attachment_if");
        let Some(rusqlite::Error::SqliteFailure(code, message)) =
            source.downcast_ref::<rusqlite::Error>()
        else {
            panic!("expected native SQLite cause, got {source:?}")
        };
        assert_eq!(code.extended_code, rusqlite::ffi::SQLITE_CONSTRAINT_TRIGGER);
        assert_eq!(message.as_deref(), Some("guarded-delete-fault"));
        assert_eq!(
            store
                .get_attachment(row.record_uuid, &row.role)
                .await
                .unwrap(),
            Some(row)
        );
    }
}

struct LegacyStore;

#[async_trait]
impl AttachmentStore for LegacyStore {
    async fn upsert_attachment(&self, _: Attachment) -> Result<(), StorageError> {
        panic!("unused")
    }
    async fn get_attachment(&self, _: Uuid, _: &str) -> Result<Option<Attachment>, StorageError> {
        panic!("conditional delete must not read then delete")
    }
    async fn list_attachments(&self, _: Uuid) -> Result<Vec<Attachment>, StorageError> {
        panic!("unused")
    }
    async fn delete_attachment(&self, _: Uuid, _: &str) -> Result<bool, StorageError> {
        panic!("conditional delete must not delegate to an unguarded delete")
    }
}

#[tokio::test]
async fn an_unimplemented_guard_fails_closed() {
    let row = attachment(Uuid::new_v4(), "content", 'd');
    let error = LegacyStore
        .delete_attachment_if(row.record_uuid, &row.role, row.substrate, &row.content_ref)
        .await
        .unwrap_err();
    assert!(matches!(error, StorageError::Unsupported {
        capability: StorageCapability::Attachments, operation, ..
    } if operation == "delete_attachment_if"));
}
