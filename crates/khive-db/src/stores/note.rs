//! SQL-backed `NoteStore` implementation.

use std::sync::Arc;

use async_trait::async_trait;
use uuid::Uuid;

use khive_storage::error::StorageError;
use khive_storage::note::{FilterOp, Note, NoteFilter, SortDir};
use khive_storage::types::{BatchWriteSummary, DeleteMode, Page, PageRequest, SqlValue};
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

/// A NoteStore backed by SQLite. Namespace is the caller's responsibility.
///
/// UUID is globally unique — get/delete by ID alone. Query/count use the
/// namespace parameter as passed. The store is just a pool + is_file_backed.
pub struct SqlNoteStore {
    pool: Arc<ConnectionPool>,
    is_file_backed: bool,
}

impl SqlNoteStore {
    /// Create a new store.
    pub fn new(pool: Arc<ConnectionPool>, is_file_backed: bool) -> Self {
        Self {
            pool,
            is_file_backed,
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

fn read_note(row: &rusqlite::Row<'_>) -> Result<Note, rusqlite::Error> {
    let id_str: String = row.get(0)?;
    let namespace: String = row.get(1)?;
    let kind: String = row.get(2)?;
    let status: String = row.get(3)?;
    let name: Option<String> = row.get(4)?;
    let content: String = row.get(5)?;
    let salience: Option<f64> = row.get(6)?;
    let decay_factor: Option<f64> = row.get(7)?;
    let expires_at: Option<i64> = row.get(8)?;
    let properties_str: Option<String> = row.get(9)?;
    let created_at: i64 = row.get(10)?;
    let updated_at: i64 = row.get(11)?;
    let deleted_at: Option<i64> = row.get(12)?;

    let id = parse_uuid(&id_str)?;

    let properties = properties_str
        .map(|s| {
            serde_json::from_str(&s).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    9,
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
        status,
        name,
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

/// Validate that a json_path is safe to interpolate into SQL.
/// Accepts only `$.field` or `$.field.subfield` paths with alphanumeric/underscore segments.
fn validate_json_path(path: &str) -> Result<(), StorageError> {
    let valid = path.starts_with("$.")
        && path[2..].split('.').all(|part| {
            !part.is_empty() && part.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        });
    if valid {
        Ok(())
    } else {
        Err(StorageError::InvalidInput {
            capability: StorageCapability::Notes,
            operation: "query_notes_filtered".into(),
            message: format!("invalid JSON path for note filter: {path:?}"),
        })
    }
}

fn json_extract_expr(path: &str) -> String {
    format!("json_extract(properties, '{path}')")
}

fn json_type_expr(path: &str) -> String {
    format!("json_type(properties, '{path}')")
}

fn sql_value_param(value: &SqlValue) -> Result<Box<dyn rusqlite::types::ToSql>, rusqlite::Error> {
    Ok(match value {
        SqlValue::Null => Box::new(Option::<String>::None),
        SqlValue::Bool(v) => Box::new(*v as i64),
        SqlValue::Integer(v) => Box::new(*v),
        SqlValue::Float(v) => Box::new(*v),
        SqlValue::Text(v) => Box::new(v.clone()),
        SqlValue::Blob(v) => Box::new(v.clone()),
        SqlValue::Json(v) => Box::new(
            serde_json::to_string(v)
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?,
        ),
        SqlValue::Uuid(v) => Box::new(v.to_string()),
        SqlValue::Timestamp(v) => Box::new(v.timestamp_micros()),
    })
}

fn build_note_filter_where(
    namespace: &str,
    filter: &NoteFilter,
) -> Result<(String, Vec<Box<dyn rusqlite::types::ToSql>>), rusqlite::Error> {
    let mut conditions = vec![
        "namespace = ?1".to_string(),
        "deleted_at IS NULL".to_string(),
    ];
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(namespace.to_string())];

    if let Some(kind) = &filter.kind {
        params.push(Box::new(kind.clone()));
        conditions.push(format!("kind = ?{}", params.len()));
    }

    for pf in &filter.property_filters {
        match pf.op {
            FilterOp::EqOrMissing => {
                let expr = json_extract_expr(&pf.json_path);
                params.push(sql_value_param(&pf.value)?);
                conditions.push(format!(
                    "({expr} = ?{n} OR {expr} IS NULL)",
                    n = params.len()
                ));
            }
            FilterOp::JsonTypeEq => {
                let type_expr = json_type_expr(&pf.json_path);
                params.push(sql_value_param(&pf.value)?);
                conditions.push(format!("{type_expr} = ?{}", params.len()));
            }
            FilterOp::JsonTypeNeMissing => {
                let type_expr = json_type_expr(&pf.json_path);
                params.push(sql_value_param(&pf.value)?);
                let n = params.len();
                conditions.push(format!("({type_expr} IS NULL OR {type_expr} != ?{n})"));
            }
            _ => {
                let expr = json_extract_expr(&pf.json_path);
                let op = match pf.op {
                    FilterOp::Eq => "=",
                    FilterOp::Ne => "!=",
                    FilterOp::Lt => "<",
                    FilterOp::Lte => "<=",
                    FilterOp::Gt => ">",
                    FilterOp::Gte => ">=",
                    FilterOp::EqOrMissing | FilterOp::JsonTypeEq | FilterOp::JsonTypeNeMissing => {
                        unreachable!()
                    }
                };
                params.push(sql_value_param(&pf.value)?);
                conditions.push(format!("{expr} {op} ?{}", params.len()));
            }
        }
    }

    Ok((format!(" WHERE {}", conditions.join(" AND ")), params))
}

// =============================================================================
// NoteStore implementation
// =============================================================================

#[async_trait]
impl NoteStore for SqlNoteStore {
    async fn upsert_note(&self, note: Note) -> Result<(), StorageError> {
        let namespace = note.namespace.clone();
        let id_str = note.id.to_string();
        let kind_str = note.kind.to_string();
        let status_str = note.status.clone();
        let properties_str = note
            .properties
            .as_ref()
            .map(|v| serde_json::to_string(v).unwrap_or_default());

        self.with_writer("upsert_note", move |conn| {
            conn.execute(
                "INSERT OR REPLACE INTO notes \
                 (id, namespace, kind, status, name, content, salience, decay_factor, expires_at, \
                  properties, created_at, updated_at, deleted_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                rusqlite::params![
                    id_str,
                    namespace,
                    kind_str,
                    status_str,
                    note.name,
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

    async fn upsert_notes(&self, notes: Vec<Note>) -> Result<BatchWriteSummary, StorageError> {
        let attempted = notes.len() as u64;

        self.with_writer("upsert_notes", move |conn| {
            conn.execute_batch("BEGIN IMMEDIATE")?;
            let mut affected = 0u64;
            let mut failed = 0u64;
            let mut first_error = String::new();

            for note in &notes {
                let id_str = note.id.to_string();
                let kind_str = note.kind.to_string();
                let status_str = note.status.clone();
                let properties_str = note
                    .properties
                    .as_ref()
                    .map(|v| serde_json::to_string(v).unwrap_or_default());

                match conn.execute(
                    "INSERT OR REPLACE INTO notes \
                     (id, namespace, kind, status, name, content, salience, decay_factor, expires_at, \
                      properties, created_at, updated_at, deleted_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                    rusqlite::params![
                        id_str,
                        &note.namespace,
                        kind_str,
                        status_str,
                        &note.name,
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

        self.with_reader("get_note", move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, namespace, kind, status, name, content, salience, decay_factor, expires_at, \
                 properties, created_at, updated_at, deleted_at \
                 FROM notes WHERE id = ?1 AND deleted_at IS NULL",
            )?;
            let mut rows = stmt.query(rusqlite::params![id_str])?;
            match rows.next()? {
                Some(row) => Ok(Some(read_note(row)?)),
                None => Ok(None),
            }
        })
        .await
    }

    async fn get_notes_batch(&self, ids: &[Uuid]) -> Result<Vec<Note>, StorageError> {
        if ids.is_empty() {
            return Ok(vec![]);
        }
        let id_strings: Vec<String> = ids.iter().map(|id| id.to_string()).collect();

        self.with_reader("get_notes_batch", move |conn| {
            let placeholders: String = (1..=id_strings.len())
                .map(|i| format!("?{i}"))
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                "SELECT id, namespace, kind, status, name, content, salience, decay_factor, expires_at, \
                 properties, created_at, updated_at, deleted_at \
                 FROM notes WHERE id IN ({placeholders}) AND deleted_at IS NULL"
            );
            let mut stmt = conn.prepare(&sql)?;
            let params: Vec<&dyn rusqlite::types::ToSql> = id_strings
                .iter()
                .map(|s| s as &dyn rusqlite::types::ToSql)
                .collect();
            let rows = stmt.query_map(params.as_slice(), read_note)?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
        .await
    }

    async fn delete_note(&self, id: Uuid, mode: DeleteMode) -> Result<bool, StorageError> {
        let id_str = id.to_string();

        match mode {
            DeleteMode::Soft => {
                self.with_writer("delete_note_soft", move |conn| {
                    let now = chrono::Utc::now().timestamp_micros();
                    let deleted = conn.execute(
                        "UPDATE notes SET status = 'deleted', deleted_at = ?1 \
                         WHERE id = ?2 AND deleted_at IS NULL",
                        rusqlite::params![now, id_str],
                    )?;
                    Ok(deleted > 0)
                })
                .await
            }
            DeleteMode::Hard => {
                self.with_writer("delete_note_hard", move |conn| {
                    let deleted =
                        conn.execute("DELETE FROM notes WHERE id = ?1", rusqlite::params![id_str])?;
                    Ok(deleted > 0)
                })
                .await
            }
        }
    }

    async fn query_notes(
        &self,
        namespace: &str,
        kind: Option<&str>,
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
                "SELECT id, namespace, kind, status, name, content, salience, decay_factor, expires_at, \
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

    async fn query_notes_filtered(
        &self,
        namespace: &str,
        filter: &NoteFilter,
        page: PageRequest,
    ) -> Result<Page<Note>, StorageError> {
        // Validate paths before entering spawn_blocking (closures return rusqlite::Error).
        for pf in &filter.property_filters {
            validate_json_path(&pf.json_path)?;
        }
        if let Some((path, _)) = &filter.order_by {
            validate_json_path(path)?;
        }

        let namespace = namespace.to_string();
        let filter = filter.clone();

        self.with_reader("query_notes_filtered", move |conn| {
            let (count_sql, count_params) = build_note_filter_where(&namespace, &filter)?;
            let total: i64 = {
                let sql = format!("SELECT COUNT(*) FROM notes{}", count_sql);
                let mut stmt = conn.prepare(&sql)?;
                let param_refs: Vec<&dyn rusqlite::types::ToSql> =
                    count_params.iter().map(|p| p.as_ref()).collect();
                stmt.query_row(param_refs.as_slice(), |row| row.get(0))?
            };

            let (where_sql, mut data_params) = build_note_filter_where(&namespace, &filter)?;
            data_params.push(Box::new(page.limit as i64));
            data_params.push(Box::new(page.offset as i64));

            let order_clause = match &filter.order_by {
                Some((path, dir)) => {
                    let dir_str = match dir {
                        SortDir::Asc => "ASC",
                        SortDir::Desc => "DESC",
                    };
                    format!(" ORDER BY {} {dir_str}", json_extract_expr(path))
                }
                None => " ORDER BY created_at DESC".to_string(),
            };

            let limit_idx = data_params.len() - 1;
            let offset_idx = data_params.len();
            let data_sql = format!(
                "SELECT id, namespace, kind, status, name, content, salience, decay_factor, \
                 expires_at, properties, created_at, updated_at, deleted_at \
                 FROM notes{}{order_clause} LIMIT ?{} OFFSET ?{}",
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

    async fn count_notes(&self, namespace: &str, kind: Option<&str>) -> Result<u64, StorageError> {
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
}

// =============================================================================
// DDL
// =============================================================================

const NOTES_DDL: &str = "\
    CREATE TABLE IF NOT EXISTS notes (\
        id TEXT PRIMARY KEY,\
        namespace TEXT NOT NULL,\
        kind TEXT NOT NULL,\
        status TEXT NOT NULL DEFAULT 'active',\
        name TEXT,\
        content TEXT NOT NULL DEFAULT '',\
        salience REAL,\
        decay_factor REAL,\
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
        SqlNoteStore::new(setup_pool(), false)
    }

    fn make_note(namespace: &str, kind: &str, content: &str) -> Note {
        Note::new(namespace, kind, content)
    }

    #[tokio::test]
    async fn test_upsert_and_get_note() {
        let store = setup_memory_store();

        let note = make_note("default", "observation", "Hello world");
        let id = note.id;

        store.upsert_note(note).await.unwrap();

        let fetched = store.get_note(id).await.unwrap();
        assert!(fetched.is_some());
        let fetched = fetched.unwrap();
        assert_eq!(fetched.id, id);
        assert_eq!(fetched.content, "Hello world");
        assert_eq!(fetched.kind, "observation");
    }

    #[tokio::test]
    async fn test_kind_roundtrip_all_variants() {
        let store = setup_memory_store();
        for kind in [
            "observation",
            "insight",
            "question",
            "decision",
            "reference",
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

        let note = make_note("default", "observation", "to be deleted");
        let id = note.id;
        store.upsert_note(note).await.unwrap();

        let deleted = store.delete_note(id, DeleteMode::Soft).await.unwrap();
        assert!(deleted);

        let fetched = store.get_note(id).await.unwrap();
        assert!(fetched.is_none());
    }

    #[tokio::test]
    async fn test_hard_delete() {
        let store = setup_memory_store();

        let note = make_note("default", "observation", "to be hard deleted");
        let id = note.id;
        store.upsert_note(note).await.unwrap();

        let deleted = store.delete_note(id, DeleteMode::Hard).await.unwrap();
        assert!(deleted);

        let fetched = store.get_note(id).await.unwrap();
        assert!(fetched.is_none());
    }

    /// Namespace isolation: one store, two namespaces — each query sees only its own.
    #[tokio::test]
    async fn test_namespace_isolation() {
        let pool = setup_pool();
        let store = SqlNoteStore::new(Arc::clone(&pool), false);

        for _ in 0..3 {
            store
                .upsert_note(make_note("ns1", "observation", "content"))
                .await
                .unwrap();
        }
        store
            .upsert_note(make_note("ns2", "observation", "other"))
            .await
            .unwrap();

        let count_ns1 = store.count_notes("ns1", None).await.unwrap();
        assert_eq!(count_ns1, 3);

        let count_ns2 = store.count_notes("ns2", None).await.unwrap();
        assert_eq!(count_ns2, 1);
    }

    /// query_notes and count_notes use the namespace parameter as passed.
    #[tokio::test]
    async fn test_query_and_count_use_caller_namespace() {
        let pool = setup_pool();
        let store = SqlNoteStore::new(Arc::clone(&pool), false);

        store
            .upsert_note(make_note("ns_a", "observation", "A"))
            .await
            .unwrap();
        store
            .upsert_note(make_note("ns_b", "insight", "B"))
            .await
            .unwrap();

        let page_a = store
            .query_notes("ns_a", None, PageRequest::default())
            .await
            .unwrap();
        assert_eq!(page_a.items.len(), 1);
        assert_eq!(page_a.items[0].content, "A");
        assert_eq!(page_a.total, Some(1));

        let page_b = store
            .query_notes("ns_b", None, PageRequest::default())
            .await
            .unwrap();
        assert_eq!(page_b.items.len(), 1);
        assert_eq!(page_b.items[0].content, "B");
        assert_eq!(page_b.total, Some(1));

        let count_a = store.count_notes("ns_a", None).await.unwrap();
        let count_b = store.count_notes("ns_b", None).await.unwrap();
        assert_eq!(count_a, 1);
        assert_eq!(count_b, 1);
    }

    #[tokio::test]
    async fn test_soft_delete_sets_status_deleted() {
        let pool = setup_pool();
        let store = SqlNoteStore::new(Arc::clone(&pool), false);
        let note = make_note("default", "observation", "to delete");
        let id = note.id;
        store.upsert_note(note).await.unwrap();
        let deleted = store.delete_note(id, DeleteMode::Soft).await.unwrap();
        assert!(deleted);
        // Verify directly via raw SQL
        let writer = pool.writer().unwrap();
        let status: String = writer
            .conn()
            .query_row(
                "SELECT status FROM notes WHERE id = ?1",
                [id.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "deleted");
    }

    #[tokio::test]
    async fn test_note_status_field_roundtrip() {
        let store = setup_memory_store();
        let note = make_note("default", "observation", "status test");
        let id = note.id;
        store.upsert_note(note).await.unwrap();
        let fetched = store.get_note(id).await.unwrap().unwrap();
        assert_eq!(fetched.status, "active");
    }

    // -- query_notes_filtered tests --

    fn make_note_with_props(
        namespace: &str,
        kind: &str,
        content: &str,
        props: serde_json::Value,
    ) -> Note {
        Note::new(namespace, kind, content).with_properties(props)
    }

    #[tokio::test]
    async fn test_filtered_namespace_and_kind_isolation() {
        let store = setup_memory_store();
        use khive_storage::note::PropertyFilter as NotePropFilter;
        use khive_storage::note::{FilterOp, NoteFilter};
        use khive_storage::types::{PageRequest, SqlValue};

        // Insert "scheduled_event" with status=pending in ns1
        let n1 = make_note_with_props(
            "ns1",
            "scheduled_event",
            "event1",
            serde_json::json!({"status": "pending", "trigger_at": "2027-01-01T00:00:00Z"}),
        );
        let n2 = make_note_with_props(
            "ns1",
            "scheduled_event",
            "event2",
            serde_json::json!({"status": "done", "trigger_at": "2027-01-02T00:00:00Z"}),
        );
        let n3 = make_note_with_props(
            "ns2",
            "scheduled_event",
            "event3",
            serde_json::json!({"status": "pending", "trigger_at": "2027-01-03T00:00:00Z"}),
        );
        store.upsert_note(n1).await.unwrap();
        store.upsert_note(n2).await.unwrap();
        store.upsert_note(n3).await.unwrap();

        let filter = NoteFilter {
            kind: Some("scheduled_event".to_string()),
            property_filters: vec![NotePropFilter {
                json_path: "$.status".to_string(),
                op: FilterOp::Eq,
                value: SqlValue::Text("pending".to_string()),
            }],
            order_by: None,
        };

        let page = store
            .query_notes_filtered("ns1", &filter, PageRequest::default())
            .await
            .unwrap();
        assert_eq!(
            page.items.len(),
            1,
            "only the pending ns1 event should appear"
        );
        assert_eq!(page.items[0].content, "event1");
        assert_eq!(page.total, Some(1));
    }

    #[tokio::test]
    async fn test_filtered_order_by_json_path_asc() {
        let store = setup_memory_store();
        use khive_storage::note::PropertyFilter as NotePropFilter;
        use khive_storage::note::{FilterOp, NoteFilter, SortDir};
        use khive_storage::types::{PageRequest, SqlValue};

        // Insert in reverse order — filter should return ascending by trigger_at.
        let n3 = make_note_with_props(
            "ns1",
            "scheduled_event",
            "third",
            serde_json::json!({"status": "pending", "trigger_at": "2027-01-03T00:00:00Z"}),
        );
        let n1 = make_note_with_props(
            "ns1",
            "scheduled_event",
            "first",
            serde_json::json!({"status": "pending", "trigger_at": "2027-01-01T00:00:00Z"}),
        );
        let n2 = make_note_with_props(
            "ns1",
            "scheduled_event",
            "second",
            serde_json::json!({"status": "pending", "trigger_at": "2027-01-02T00:00:00Z"}),
        );
        store.upsert_note(n3).await.unwrap();
        store.upsert_note(n1).await.unwrap();
        store.upsert_note(n2).await.unwrap();

        let filter = NoteFilter {
            kind: Some("scheduled_event".to_string()),
            property_filters: vec![NotePropFilter {
                json_path: "$.status".to_string(),
                op: FilterOp::Eq,
                value: SqlValue::Text("pending".to_string()),
            }],
            order_by: Some(("$.trigger_at".to_string(), SortDir::Asc)),
        };

        let page = store
            .query_notes_filtered("ns1", &filter, PageRequest::default())
            .await
            .unwrap();
        assert_eq!(page.items.len(), 3);
        assert_eq!(page.items[0].content, "first");
        assert_eq!(page.items[1].content, "second");
        assert_eq!(page.items[2].content, "third");
    }

    #[tokio::test]
    async fn test_filtered_soft_deleted_excluded() {
        let store = setup_memory_store();
        use khive_storage::note::PropertyFilter as NotePropFilter;
        use khive_storage::note::{FilterOp, NoteFilter};
        use khive_storage::types::{DeleteMode, PageRequest, SqlValue};

        let n = make_note_with_props(
            "ns1",
            "scheduled_event",
            "to_delete",
            serde_json::json!({"status": "pending"}),
        );
        let id = n.id;
        store.upsert_note(n).await.unwrap();
        store.delete_note(id, DeleteMode::Soft).await.unwrap();

        let filter = NoteFilter {
            kind: Some("scheduled_event".to_string()),
            property_filters: vec![NotePropFilter {
                json_path: "$.status".to_string(),
                op: FilterOp::Eq,
                value: SqlValue::Text("pending".to_string()),
            }],
            order_by: None,
        };

        let page = store
            .query_notes_filtered("ns1", &filter, PageRequest::default())
            .await
            .unwrap();
        assert_eq!(page.items.len(), 0, "soft-deleted rows must not appear");
    }

    #[tokio::test]
    async fn test_filtered_invalid_json_path_rejected() {
        let store = setup_memory_store();
        use khive_storage::note::PropertyFilter as NotePropFilter;
        use khive_storage::note::{FilterOp, NoteFilter};
        use khive_storage::types::{PageRequest, SqlValue};

        let filter = NoteFilter {
            kind: None,
            property_filters: vec![NotePropFilter {
                json_path: "DROP TABLE notes".to_string(),
                op: FilterOp::Eq,
                value: SqlValue::Text("x".to_string()),
            }],
            order_by: None,
        };

        let result = store
            .query_notes_filtered("ns1", &filter, PageRequest::default())
            .await;
        assert!(
            result.is_err(),
            "invalid json_path must be rejected before SQL"
        );
    }
}
