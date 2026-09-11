//! SQL-backed `AttachmentStore` implementation.

use std::str::FromStr;
use std::sync::Arc;

use async_trait::async_trait;
use rusqlite::OptionalExtension;
use uuid::Uuid;

use khive_storage::attachment::{
    validate_attachment_role, Attachment, AttachmentStore, AttachmentSubstrate,
};
use khive_storage::error::StorageError;
use khive_storage::types::{SqlStatement, SqlValue};
use khive_storage::{ContentRef, StorageCapability};

use crate::error::SqliteError;
use crate::pool::ConnectionPool;
use crate::sql_bridge::bind_params;
use crate::writer_task::WriterTaskHandle;

fn map_err(error: rusqlite::Error, operation: &'static str) -> StorageError {
    StorageError::driver(StorageCapability::Attachments, operation, error)
}

fn map_sqlite_err(error: SqliteError, operation: &'static str) -> StorageError {
    StorageError::driver(StorageCapability::Attachments, operation, error)
}

/// Build the canonical upsert for one already-validated attachment.
pub fn attachment_upsert_statement(attachment: &Attachment) -> Result<SqlStatement, StorageError> {
    attachment.validate()?;
    let size_bytes = attachment
        .size_bytes
        .map(|size| {
            i64::try_from(size).map_err(|_| StorageError::InvalidInput {
                capability: StorageCapability::Attachments,
                operation: "upsert_attachment".into(),
                message: "attachment size_bytes must fit SQLite INTEGER".to_string(),
            })
        })
        .transpose()?;
    Ok(SqlStatement {
        sql: "INSERT INTO attachments \
              (record_uuid, substrate, role, content_ref, media_type, size_bytes, created_at) \
              VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
              ON CONFLICT(record_uuid, role) DO UPDATE SET \
                substrate = excluded.substrate, \
                content_ref = excluded.content_ref, \
                media_type = excluded.media_type, \
                size_bytes = excluded.size_bytes, \
                created_at = excluded.created_at"
            .to_string(),
        params: vec![
            SqlValue::Text(attachment.record_uuid.to_string()),
            SqlValue::Text(attachment.substrate.as_str().to_string()),
            SqlValue::Text(attachment.role.clone()),
            SqlValue::Text(attachment.content_ref.to_string()),
            attachment
                .media_type
                .clone()
                .map_or(SqlValue::Null, SqlValue::Text),
            size_bytes.map_or(SqlValue::Null, SqlValue::Integer),
            SqlValue::Integer(attachment.created_at),
        ],
        label: Some("attachment-upsert".to_string()),
    })
}

/// Build deletion of one role from one record.
pub fn delete_attachment_statement(record_uuid: Uuid, role: &str) -> SqlStatement {
    SqlStatement {
        sql: "DELETE FROM attachments WHERE record_uuid = ?1 AND role = ?2".to_string(),
        params: vec![
            SqlValue::Text(record_uuid.to_string()),
            SqlValue::Text(role.to_string()),
        ],
        label: Some("attachment-delete-role".to_string()),
    }
}

/// Build deletion of every attachment owned by one record substrate.
pub fn delete_record_attachments_statement(
    record_uuid: Uuid,
    substrate: AttachmentSubstrate,
) -> SqlStatement {
    SqlStatement {
        sql: "DELETE FROM attachments WHERE record_uuid = ?1 AND substrate = ?2".to_string(),
        params: vec![
            SqlValue::Text(record_uuid.to_string()),
            SqlValue::Text(substrate.as_str().to_string()),
        ],
        label: Some(format!("attachment-delete-record-{}", substrate.as_str())),
    }
}

/// SQLite attachment store. Schema installation is owned by the coordinated
/// core migration, not by this accessor.
pub struct SqlAttachmentStore {
    pool: Arc<ConnectionPool>,
    writer_task: Option<WriterTaskHandle>,
}

impl SqlAttachmentStore {
    pub fn new(pool: Arc<ConnectionPool>, _is_file_backed: bool) -> Self {
        let writer_task = pool.writer_task_handle().ok().flatten();
        Self { pool, writer_task }
    }

    fn current_writer_task(
        &self,
        operation: &'static str,
    ) -> Result<Option<WriterTaskHandle>, StorageError> {
        self.pool
            .writer_task_for_write(self.writer_task.as_ref(), operation)
    }

    async fn with_writer<F, R>(&self, operation: &'static str, f: F) -> Result<R, StorageError>
    where
        F: FnOnce(&rusqlite::Connection) -> Result<R, rusqlite::Error> + Send + 'static,
        R: Send + 'static,
    {
        if let Some(writer_task) = self.current_writer_task(operation)? {
            return writer_task
                .send_bounded(move |conn| f(conn).map_err(|error| map_err(error, operation)))
                .await;
        }

        self.pool
            .record_direct_route(crate::timeout_sink::Site::DirectRouteEntity);
        let pool = Arc::clone(&self.pool);
        tokio::task::spawn_blocking(move || {
            let guard = pool
                .try_writer()
                .map_err(|error| map_sqlite_err(error, operation))?;
            f(guard.conn()).map_err(|error| map_err(error, operation))
        })
        .await
        .map_err(|error| StorageError::driver(StorageCapability::Attachments, operation, error))?
    }

    async fn with_reader<F, R>(&self, operation: &'static str, f: F) -> Result<R, StorageError>
    where
        F: FnOnce(&rusqlite::Connection) -> Result<R, rusqlite::Error> + Send + 'static,
        R: Send + 'static,
    {
        super::run_pooled_store_read(
            Arc::clone(&self.pool),
            StorageCapability::Attachments,
            operation,
            move |conn| f(conn).map_err(|error| map_err(error, operation)),
        )
        .await
    }
}

fn conversion_error(
    index: usize,
    value_type: rusqlite::types::Type,
    message: String,
) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        index,
        value_type,
        Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            message,
        )),
    )
}

fn read_attachment(row: &rusqlite::Row<'_>) -> Result<Attachment, rusqlite::Error> {
    let record_uuid_raw: String = row.get(0)?;
    let substrate_raw: String = row.get(1)?;
    let role: String = row.get(2)?;
    let content_ref_raw: String = row.get(3)?;
    let media_type: Option<String> = row.get(4)?;
    let size_bytes_raw: Option<i64> = row.get(5)?;
    let created_at: i64 = row.get(6)?;

    let record_uuid = Uuid::parse_str(&record_uuid_raw)
        .map_err(|error| conversion_error(0, rusqlite::types::Type::Text, error.to_string()))?;
    let substrate = AttachmentSubstrate::from_str(&substrate_raw)
        .map_err(|error| conversion_error(1, rusqlite::types::Type::Text, error))?;
    validate_attachment_role(&role)
        .map_err(|error| conversion_error(2, rusqlite::types::Type::Text, error.to_string()))?;
    let content_ref = ContentRef::from_hex(content_ref_raw)
        .map_err(|error| conversion_error(3, rusqlite::types::Type::Text, error))?;
    let size_bytes = size_bytes_raw
        .map(|size| {
            u64::try_from(size).map_err(|_| {
                conversion_error(
                    5,
                    rusqlite::types::Type::Integer,
                    format!("attachment size_bytes must not be negative, got {size}"),
                )
            })
        })
        .transpose()?;

    Ok(Attachment {
        record_uuid,
        substrate,
        role,
        content_ref,
        media_type,
        size_bytes,
        created_at,
    })
}

#[async_trait]
impl AttachmentStore for SqlAttachmentStore {
    async fn upsert_attachment(&self, attachment: Attachment) -> Result<(), StorageError> {
        let statement = attachment_upsert_statement(&attachment)?;
        self.with_writer("upsert_attachment", move |conn| {
            let mut stmt = conn.prepare(&statement.sql)?;
            bind_params(&mut stmt, &statement.params)?;
            stmt.raw_execute()?;
            Ok(())
        })
        .await
    }

    async fn get_attachment(
        &self,
        record_uuid: Uuid,
        role: &str,
    ) -> Result<Option<Attachment>, StorageError> {
        validate_attachment_role(role)?;
        let record_uuid = record_uuid.to_string();
        let role = role.to_string();
        self.with_reader("get_attachment", move |conn| {
            conn.query_row(
                "SELECT record_uuid, substrate, role, content_ref, media_type, size_bytes, created_at \
                 FROM attachments WHERE record_uuid = ?1 AND role = ?2",
                rusqlite::params![record_uuid, role],
                read_attachment,
            )
            .optional()
        })
        .await
    }

    async fn list_attachments(&self, record_uuid: Uuid) -> Result<Vec<Attachment>, StorageError> {
        let record_uuid = record_uuid.to_string();
        self.with_reader("list_attachments", move |conn| {
            let mut stmt = conn.prepare(
                "SELECT record_uuid, substrate, role, content_ref, media_type, size_bytes, created_at \
                 FROM attachments WHERE record_uuid = ?1 ORDER BY role ASC",
            )?;
            let rows = stmt.query_map([record_uuid], read_attachment)?;
            rows.collect()
        })
        .await
    }

    async fn delete_attachment(&self, record_uuid: Uuid, role: &str) -> Result<bool, StorageError> {
        validate_attachment_role(role)?;
        let statement = delete_attachment_statement(record_uuid, role);
        self.with_writer("delete_attachment", move |conn| {
            let mut stmt = conn.prepare(&statement.sql)?;
            bind_params(&mut stmt, &statement.params)?;
            Ok(stmt.raw_execute()? > 0)
        })
        .await
    }
}
