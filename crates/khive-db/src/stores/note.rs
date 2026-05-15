//! SQL-backed `NoteStore` implementation.

use std::sync::Arc;

use async_trait::async_trait;
use uuid::Uuid;

use khive_storage::error::StorageError;
use khive_storage::note::{Note, NoteKind};
use khive_storage::types::{BatchWriteSummary, DeleteMode, Page, PageRequest};
use khive_storage::NoteStore;
use khive_storage::StorageCapability;

use crate::error::SqliteError;
use crate::pool::ConnectionPool;

fn map_err(e: rusqlite::Error, op: &'static str) -> StorageError {
    StorageError::driver(StorageCapability::Notes, op, e)
}

fn map_sqlite_err(e: SqliteError, op: &'static str) -> StorageError {
    StorageError::driver(StorageCapability::Notes, op, e)
}

/// A NoteStore backed by SQLite, scoped to a single namespace.
///
/// Reads and deletes are restricted to `self.namespace`. Writes accept notes
/// whose `namespace` field is used directly (no override), but `get_note` and
/// `delete_note` only operate on records that belong to this store's namespace.
pub struct SqlNoteStore {
    pool: Arc<ConnectionPool>,
    is_file_backed: bool,
    namespace: String,
}

impl SqlNoteStore {
    /// Create a new store scoped to `namespace`.
    pub fn new_scoped(
        pool: Arc<ConnectionPool>,
        is_file_backed: bool,
        namespace: impl Into<String>,
    ) -> Self {
        Self {
            pool,
            is_file_backed,
            namespace: namespace.into(),
        }
    }

    fn open_standalone_reader(&self) -> Result<rusqlite::Connection, StorageError> {
        let config = self.pool.config();
        let path = config.path.as_ref().ok_or_else(|| StorageError::Pool {
            operation: "note_reader".into(),
            message: "in-memory databases do not support standalone connections".into(),
        })?;

        let conn = rusqlite::Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX
                | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )
        .map_err(|e| map_err(e, "open_note_reader"))?;

        conn.busy_timeout(config.busy_timeout)
            .map_err(|e| map_err(e, "open_note_reader"))?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(|e| map_err(e, "open_note_reader"))?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(|e| map_err(e, "open_note_reader"))?;

        Ok(conn)
    }

    /// Write via pool writer (serializes writes through the mutex).
    async fn with_writer<F, R>(&self, op: &'static str, f: F) -> Result<R, StorageError>
    where
        F: FnOnce(&rusqlite::Connection) -> Result<R, rusqlite::Error> + Send + 'static,
        R: Send + 'static,
    {
        let pool = Arc::clone(&self.pool);
        tokio::task::spawn_blocking(move || {
            let guard = pool.try_writer().map_err(|e| map_sqlite_err(e, op))?;
            f(guard.conn()).map_err(|e| map_err(e, op))
        })
        .await
        .map_err(|e| StorageError::driver(StorageCapability::Notes, op, e))?
    }

    async fn with_reader<F, R>(&self, op: &'static str, f: F) -> Result<R, StorageError>
    where
        F: FnOnce(&rusqlite::Connection) -> Result<R, rusqlite::Error> + Send + 'static,
        R: Send + 'static,
    {
        if self.is_file_backed {
            let conn = self.open_standalone_reader()?;
            tokio::task::spawn_blocking(move || f(&conn).map_err(|e| map_err(e, op)))
                .await
                .map_err(|e| StorageError::driver(StorageCapability::Notes, op, e))?
        } else {
            let pool = Arc::clone(&self.pool);
            tokio::task::spawn_blocking(move || {
                let guard = pool.reader().map_err(|e| map_sqlite_err(e, op))?;
                f(guard.conn()).map_err(|e| map_err(e, op))
            })
            .await
            .map_err(|e| StorageError::driver(StorageCapability::Notes, op, e))?
        }
    }
}

// =============================================================================
// Helpers
// =============================================================================

fn parse_note_kind(s: &str, col: usize) -> Result<NoteKind, rusqlite::Error> {
    s.parse::<NoteKind>().map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(col, rusqlite::types::Type::Text, e.into())
    })
}

fn read_note(row: &rusqlite::Row<'_>) -> Result<Note, rusqlite::Error> {
    let id_str: String = row.get(0)?;
    let namespace: String = row.get(1)?;
    let kind_str: String = row.get(2)?;
    let content: String = row.get(3)?;
    let salience: f64 = row.get(4)?;
    let decay_factor: f64 = row.get(5)?;
    let expires_at: Option<i64> = row.get(6)?;
    let properties_str: Option<String> = row.get(7)?;
    let created_at: i64 = row.get(8)?;
    let updated_at: i64 = row.get(9)?;
    let deleted_at: Option<i64> = row.get(10)?;

    let id = parse_uuid(&id_str)?;
    let kind = parse_note_kind(&kind_str, 2)?;

    let properties = properties_str
        .map(|s| {
            serde_json::from_str(&s).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    7,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })
        })
        .transpose()?;

    Ok(Note {
        id,
        namespace,
        kind,
        content,
        salience,
        decay_factor,
        expires_at,
        properties,
        created_at,
        updated_at,
        deleted_at,
    })
}

fn parse_uuid(s: &str) -> Result<Uuid, rusqlite::Error> {
    Uuid::parse_str(s).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })
}

fn build_note_where(
    namespace: &str,
    kind: Option<&str>,
) -> (String, Vec<Box<dyn rusqlite::types::ToSql>>) {
    let mut conditions: Vec<String> = vec![
        "namespace = ?1".to_string(),
        "deleted_at IS NULL".to_string(),
    ];
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(namespace.to_string())];

    if let Some(k) = kind {
        params.push(Box::new(k.to_string()));
        conditions.push(format!("kind = ?{}", params.len()));
    }

    let clause = format!(" WHERE {}", conditions.join(" AND "));
    (clause, params)
}

// =============================================================================
// NoteStore implementation
// =============================================================================

#[async_trait]
impl NoteStore for SqlNoteStore {
    async fn upsert_note(&self, mut note: Note) -> Result<(), StorageError> {
        note.namespace.clone_from(&self.namespace);
        let namespace = note.namespace.clone();
        let id_str = note.id.to_string();
        let kind_str = note.kind.to_string();
        let properties_str = note
            .properties
            .as_ref()
            .map(|v| serde_json::to_string(v).unwrap_or_default());

        self.with_writer("upsert_note", move |conn| {
            conn.execute(
                "INSERT OR REPLACE INTO notes \
                 (id, namespace, kind, content, salience, decay_factor, expires_at, \
                  properties, created_at, updated_at, deleted_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                rusqlite::params![
                    id_str,
                    namespace,
                    kind_str,
                    note.content,
                    note.salience,
                    note.decay_factor,
                    note.expires_at,
                    properties_str,
                    note.created_at,
                    note.updated_at,
                    note.deleted_at,
                ],
            )?;
            Ok(())
        })
        .await
    }

    async fn upsert_notes(&self, mut notes: Vec<Note>) -> Result<BatchWriteSummary, StorageError> {
        for n in &mut notes {
            n.namespace.clone_from(&self.namespace);
        }
        let attempted = notes.len() as u64;

        self.with_writer("upsert_notes", move |conn| {
            conn.execute_batch("BEGIN IMMEDIATE")?;
            let mut affected = 0u64;
            let mut failed = 0u64;
            let mut first_error = String::new();

            for note in &notes {
                let id_str = note.id.to_string();
                let kind_str = note.kind.to_string();
                let properties_str = note
                    .properties
                    .as_ref()
                    .map(|v| serde_json::to_string(v).unwrap_or_default());

                match conn.execute(
                    "INSERT OR REPLACE INTO notes \
                     (id, namespace, kind, content, salience, decay_factor, expires_at, \
                      properties, created_at, updated_at, deleted_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                    rusqlite::params![
                        id_str,
                        &note.namespace,
                        kind_str,
                        note.content,
                        note.salience,
                        note.decay_factor,
                        note.expires_at,
                        properties_str,
                        note.created_at,
                        note.updated_at,
                        note.deleted_at,
                    ],
                ) {
                    Ok(_) => affected += 1,
                    Err(e) => {
                        if first_error.is_empty() {
                            first_error = e.to_string();
                        }
                        failed += 1;
                    }
                }
            }

            if let Err(e) = conn.execute_batch("COMMIT") {
                let _ = conn.execute_batch("ROLLBACK");
                return Err(e);
            }
            Ok(BatchWriteSummary {
                attempted,
                affected,
                failed,
                first_error,
            })
        })
        .await
    }

    async fn get_note(&self, id: Uuid) -> Result<Option<Note>, StorageError> {
        let id_str = id.to_string();
        let namespace = self.namespace.clone();

        self.with_reader("get_note", move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, namespace, kind, content, salience, decay_factor, expires_at, \
                 properties, created_at, updated_at, deleted_at \
                 FROM notes WHERE id = ?1 AND namespace = ?2 AND deleted_at IS NULL",
            )?;
            let mut rows = stmt.query(rusqlite::params![id_str, namespace])?;
            match rows.next()? {
                Some(row) => Ok(Some(read_note(row)?)),
                None => Ok(None),
            }
        })
        .await
    }

    async fn delete_note(&self, id: Uuid, mode: DeleteMode) -> Result<bool, StorageError> {
        let id_str = id.to_string();
        let namespace = self.namespace.clone();

        match mode {
            DeleteMode::Soft => {
                self.with_writer("delete_note_soft", move |conn| {
                    let now = chrono::Utc::now().timestamp_micros();
                    let deleted = conn.execute(
                        "UPDATE notes SET deleted_at = ?1 \
                         WHERE id = ?2 AND namespace = ?3 AND deleted_at IS NULL",
                        rusqlite::params![now, id_str, namespace],
                    )?;
                    Ok(deleted > 0)
                })
                .await
            }
            DeleteMode::Hard => {
                self.with_writer("delete_note_hard", move |conn| {
                    let deleted = conn.execute(
                        "DELETE FROM notes WHERE id = ?1 AND namespace = ?2",
                        rusqlite::params![id_str, namespace],
                    )?;
                    Ok(deleted > 0)
                })
                .await
            }
        }
    }

    async fn query_notes(
        &self,
        namespace: &str,
        kind: Option<NoteKind>,
        page: PageRequest,
    ) -> Result<Page<Note>, StorageError> {
        let namespace = namespace.to_string();
        let kind = kind.map(|k| k.to_string());

        self.with_reader("query_notes", move |conn| {
            let (count_sql, count_params) = build_note_where(&namespace, kind.as_deref());
            let total: i64 = {
                let sql = format!("SELECT COUNT(*) FROM notes{}", count_sql);
                let mut stmt = conn.prepare(&sql)?;
                let param_refs: Vec<&dyn rusqlite::types::ToSql> =
                    count_params.iter().map(|p| p.as_ref()).collect();
                stmt.query_row(param_refs.as_slice(), |row| row.get(0))?
            };

            let (where_sql, mut data_params) = build_note_where(&namespace, kind.as_deref());
            data_params.push(Box::new(page.limit as i64));
            data_params.push(Box::new(page.offset as i64));

            let limit_idx = data_params.len() - 1;
            let offset_idx = data_params.len();

            let data_sql = format!(
                "SELECT id, namespace, kind, content, salience, decay_factor, expires_at, \
                 properties, created_at, updated_at, deleted_at \
                 FROM notes{} ORDER BY created_at DESC LIMIT ?{} OFFSET ?{}",
                where_sql, limit_idx, offset_idx,
            );

            let mut stmt = conn.prepare(&data_sql)?;
            let param_refs: Vec<&dyn rusqlite::types::ToSql> =
                data_params.iter().map(|p| p.as_ref()).collect();
            let rows = stmt.query_map(param_refs.as_slice(), read_note)?;

            let mut items = Vec::new();
            for row in rows {
                items.push(row?);
            }

            Ok(Page {
                items,
                total: Some(total as u64),
            })
        })
        .await
    }

    async fn count_notes(
        &self,
        namespace: &str,
        kind: Option<NoteKind>,
    ) -> Result<u64, StorageError> {
        let namespace = namespace.to_string();
        let kind = kind.map(|k| k.to_string());

        self.with_reader("count_notes", move |conn| {
            let (where_sql, params) = build_note_where(&namespace, kind.as_deref());
            let sql = format!("SELECT COUNT(*) FROM notes{}", where_sql);
            let mut stmt = conn.prepare(&sql)?;
            let param_refs: Vec<&dyn rusqlite::types::ToSql> =
                params.iter().map(|p| p.as_ref()).collect();
            let count: i64 = stmt.query_row(param_refs.as_slice(), |row| row.get(0))?;
            Ok(count as u64)
        })
        .await
    }

    async fn upsert_note_if_below_quota(
        &self,
        mut note: Note,
        max_notes: u64,
    ) -> Result<bool, StorageError> {
        note.namespace.clone_from(&self.namespace);
        let namespace = note.namespace.clone();
        let id_str = note.id.to_string();
        let kind_str = note.kind.to_string();
        let properties_str = note
            .properties
            .as_ref()
            .map(|v| serde_json::to_string(v).unwrap_or_default());

        self.with_writer("upsert_note_if_below_quota", move |conn| {
            let count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM notes WHERE namespace = ?1 AND deleted_at IS NULL",
                [&namespace],
                |row| row.get(0),
            )?;
            if count as u64 >= max_notes {
                return Ok(false);
            }
            conn.execute(
                "INSERT OR REPLACE INTO notes \
                 (id, namespace, kind, content, salience, decay_factor, expires_at, \
                  properties, created_at, updated_at, deleted_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                rusqlite::params![
                    id_str,
                    namespace,
                    kind_str,
                    note.content,
                    note.salience,
                    note.decay_factor,
                    note.expires_at,
                    properties_str,
                    note.created_at,
                    note.updated_at,
                    note.deleted_at,
                ],
            )?;
            Ok(true)
        })
        .await
    }
}

// =============================================================================
// DDL
// =============================================================================

const NOTES_DDL: &str = "\
    CREATE TABLE IF NOT EXISTS notes (\
        id TEXT PRIMARY KEY,\
        namespace TEXT NOT NULL,\
        kind TEXT NOT NULL,\
        content TEXT NOT NULL DEFAULT '',\
        salience REAL NOT NULL DEFAULT 0.5,\
        decay_factor REAL NOT NULL DEFAULT 0.0,\
        expires_at INTEGER,\
        properties TEXT,\
        created_at INTEGER NOT NULL,\
        updated_at INTEGER NOT NULL,\
        deleted_at INTEGER\
    );\
    CREATE INDEX IF NOT EXISTS idx_notes_namespace ON notes(namespace);\
    CREATE INDEX IF NOT EXISTS idx_notes_kind ON notes(namespace, kind);\
    CREATE INDEX IF NOT EXISTS idx_notes_created ON notes(created_at DESC);\
";

pub(crate) fn ensure_notes_schema(conn: &rusqlite::Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(NOTES_DDL)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::PoolConfig;

    fn setup_pool() -> Arc<ConnectionPool> {
        let config = PoolConfig {
            path: None,
            ..PoolConfig::default()
        };
        let pool = Arc::new(ConnectionPool::new(config).unwrap());
        {
            let writer = pool.writer().unwrap();
            writer.conn().execute_batch(NOTES_DDL).unwrap();
        }
        pool
    }

    fn setup_memory_store() -> SqlNoteStore {
        SqlNoteStore::new_scoped(setup_pool(), false, "default")
    }

    fn setup_memory_store_ns(ns: &str) -> SqlNoteStore {
        SqlNoteStore::new_scoped(setup_pool(), false, ns)
    }

    fn make_note(namespace: &str, kind: NoteKind, content: &str) -> Note {
        Note::new(namespace, kind, content)
    }

    #[tokio::test]
    async fn test_upsert_and_get_note() {
        let store = setup_memory_store();

        let note = make_note("default", NoteKind::Observation, "Hello world");
        let id = note.id;

        store.upsert_note(note).await.unwrap();

        let fetched = store.get_note(id).await.unwrap();
        assert!(fetched.is_some());
        let fetched = fetched.unwrap();
        assert_eq!(fetched.id, id);
        assert_eq!(fetched.content, "Hello world");
        assert_eq!(fetched.kind, NoteKind::Observation);
    }

    #[tokio::test]
    async fn test_kind_roundtrip_all_variants() {
        let store = setup_memory_store();
        for kind in [
            NoteKind::Observation,
            NoteKind::Insight,
            NoteKind::Question,
            NoteKind::Decision,
            NoteKind::Reference,
        ] {
            let note = make_note("default", kind, "content");
            let id = note.id;
            store.upsert_note(note).await.unwrap();
            let fetched = store.get_note(id).await.unwrap().unwrap();
            assert_eq!(fetched.kind, kind);
        }
    }

    #[tokio::test]
    async fn test_soft_delete() {
        let store = setup_memory_store();

        let note = make_note("default", NoteKind::Observation, "to be deleted");
        let id = note.id;
        store.upsert_note(note).await.unwrap();

        let deleted = store.delete_note(id, DeleteMode::Soft).await.unwrap();
        assert!(deleted);

        let fetched = store.get_note(id).await.unwrap();
        assert!(fetched.is_none());
    }

    #[tokio::test]
    async fn test_count_notes() {
        let pool = setup_pool();
        let store_ns1 = SqlNoteStore::new_scoped(Arc::clone(&pool), false, "ns1");
        let store_ns2 = SqlNoteStore::new_scoped(Arc::clone(&pool), false, "ns2");

        for _ in 0..3 {
            store_ns1
                .upsert_note(make_note("ns1", NoteKind::Observation, "content"))
                .await
                .unwrap();
        }

        let count = store_ns1.count_notes("ns1", None).await.unwrap();
        assert_eq!(count, 3);

        let count_other = store_ns2.count_notes("ns2", None).await.unwrap();
        assert_eq!(count_other, 0);
    }

    /// A store scoped to "ns_a" must not see or delete notes belonging to "ns_b".
    #[tokio::test]
    async fn test_cross_namespace_isolation_get_and_delete() {
        let config = PoolConfig {
            path: None,
            ..PoolConfig::default()
        };
        let pool = Arc::new(ConnectionPool::new(config).unwrap());
        {
            let writer = pool.writer().unwrap();
            writer.conn().execute_batch(NOTES_DDL).unwrap();
        }

        let store_a = SqlNoteStore::new_scoped(Arc::clone(&pool), false, "ns_a");
        let store_b = SqlNoteStore::new_scoped(Arc::clone(&pool), false, "ns_b");

        // Write a note that belongs to ns_b via store_b.
        let note_b = make_note("ns_b", NoteKind::Observation, "secret note");
        let id_b = note_b.id;
        store_b.upsert_note(note_b).await.unwrap();

        // store_a (scoped to ns_a) must not see the ns_b note.
        let result = store_a.get_note(id_b).await.unwrap();
        assert!(result.is_none(), "store_a must not read a note from ns_b");

        // store_a must not be able to soft-delete the ns_b note.
        let deleted_soft = store_a.delete_note(id_b, DeleteMode::Soft).await.unwrap();
        assert!(
            !deleted_soft,
            "store_a must not soft-delete a note from ns_b"
        );

        // store_a must not be able to hard-delete the ns_b note.
        let deleted_hard = store_a.delete_note(id_b, DeleteMode::Hard).await.unwrap();
        assert!(
            !deleted_hard,
            "store_a must not hard-delete a note from ns_b"
        );

        // The note must still be visible to store_b.
        let still_there = store_b.get_note(id_b).await.unwrap();
        assert!(
            still_there.is_some(),
            "note must still exist in ns_b after cross-namespace delete attempts"
        );
    }

    #[tokio::test]
    async fn test_quota() {
        let store = setup_memory_store_ns("quota_ns");

        for _ in 0..3 {
            let inserted = store
                .upsert_note_if_below_quota(make_note("quota_ns", NoteKind::Observation, "x"), 3)
                .await
                .unwrap();
            assert!(inserted);
        }

        let inserted = store
            .upsert_note_if_below_quota(make_note("quota_ns", NoteKind::Observation, "x"), 3)
            .await
            .unwrap();
        assert!(!inserted);
    }
}
