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
use crate::writer_task::WriterTaskHandle;

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
    writer_task: Option<WriterTaskHandle>,
}

impl SqlNoteStore {
    /// Create a new store.
    pub fn new(pool: Arc<ConnectionPool>, is_file_backed: bool) -> Self {
        // Best-effort opt-in (ADR-067 Component A, mirrors entity.rs slice 1
        // policy): a missing writer task — flag off, spawn degraded, or no
        // Tokio runtime available at this first access — degrades to the
        // legacy pool-mutex path rather than failing construction.
        let writer_task = pool.writer_task_handle().ok().flatten();

        Self {
            pool,
            is_file_backed,
            writer_task,
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

    /// Route a single-row write through the pool-wide `WriterTask` when
    /// `KHIVE_WRITE_QUEUE=1` and a handle is available; otherwise fall back
    /// to the legacy pool-mutex path (ADR-067 Component A, Fork C slice 2).
    ///
    /// This is the ONE routing point for every `with_writer` caller in this
    /// store (`upsert_note`, `try_insert_note`, `delete_note`). `f` must be
    /// DML-only — on the flag-on path it runs inside the WriterTask's own
    /// transaction, so a bare `BEGIN IMMEDIATE` would violate SQLite's
    /// nested-transaction rule. `upsert_notes` (the batch method) does its
    /// own flag check and returns early on `Some`, so its fallback call
    /// into this helper only ever executes on the flag-off path
    /// (`self.writer_task` is `None` by construction whenever that call is
    /// reached) — no double-routing.
    async fn with_writer<F, R>(&self, op: &'static str, f: F) -> Result<R, StorageError>
    where
        F: FnOnce(&rusqlite::Connection) -> Result<R, rusqlite::Error> + Send + 'static,
        R: Send + 'static,
    {
        if let Some(writer_task) = &self.writer_task {
            return writer_task
                .send(move |conn| f(conn).map_err(|e| map_err(e, op)))
                .await;
        }

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

/// DML-only batch upsert loop shared by both the legacy (flag-off) and
/// WriterTask-routed (flag-on) `upsert_notes` paths (ADR-067 Component A).
///
/// Issues no `BEGIN` / `COMMIT` / `ROLLBACK` itself — the caller owns the
/// enclosing transaction. Per-row failures are captured into
/// `BatchWriteSummary::failed`/`first_error` rather than aborting the loop,
/// matching the existing partial-success contract.
fn batch_upsert_notes(
    conn: &rusqlite::Connection,
    notes: &[Note],
    attempted: u64,
) -> Result<BatchWriteSummary, rusqlite::Error> {
    let mut affected = 0u64;
    let mut failed = 0u64;
    let mut first_error = String::new();

    for note in notes {
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

    Ok(BatchWriteSummary {
        attempted,
        affected,
        failed,
        first_error,
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
    // When filter.namespaces is non-empty use `namespace IN (...)` for
    // multi-namespace read visibility. Otherwise fall back to equality.
    let (ns_condition, ns_params): (String, Vec<Box<dyn rusqlite::types::ToSql>>) =
        if !filter.namespaces.is_empty() {
            let placeholders: Vec<String> = (1..=filter.namespaces.len())
                .map(|i| format!("?{i}"))
                .collect();
            let params: Vec<Box<dyn rusqlite::types::ToSql>> = filter
                .namespaces
                .iter()
                .map(|ns| -> Box<dyn rusqlite::types::ToSql> { Box::new(ns.clone()) })
                .collect();
            (
                format!("namespace IN ({})", placeholders.join(", ")),
                params,
            )
        } else {
            (
                "namespace = ?1".to_string(),
                vec![Box::new(namespace.to_string())],
            )
        };

    let mut conditions = vec![ns_condition, "deleted_at IS NULL".to_string()];
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = ns_params;

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

    if let Some(min_ts) = filter.min_created_at {
        params.push(Box::new(min_ts));
        conditions.push(format!("created_at >= ?{}", params.len()));
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

    async fn try_insert_note(&self, note: Note) -> Result<bool, StorageError> {
        let namespace = note.namespace.clone();
        let id_str = note.id.to_string();
        let kind_str = note.kind.to_string();
        let status_str = note.status.clone();
        let properties_str = note
            .properties
            .as_ref()
            .map(|v| serde_json::to_string(v).unwrap_or_default());

        // Extract external_id (if any) for dedup verification after a zero-row insert.
        let ext_id_opt: Option<String> = note
            .properties
            .as_ref()
            .and_then(|v| v.get("external_id"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());

        self.with_writer("try_insert_note", move |conn| {
            let rows = conn.execute(
                "INSERT OR IGNORE INTO notes \
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

            if rows > 0 {
                return Ok(true);
            }

            // Zero rows: the INSERT was silently skipped by OR IGNORE.
            // Only treat this as a dedup hit when a live note with the same
            // non-empty external_id already exists in this namespace and kind.
            // Any other ignored constraint (e.g. a PRIMARY KEY collision) must
            // surface as an error rather than being misreported as a duplicate.
            if let Some(ref ext_id) = ext_id_opt {
                let is_dedup: bool = conn.query_row(
                    "SELECT COUNT(*) > 0 FROM notes \
                     WHERE namespace = ?1 \
                       AND kind = ?2 \
                       AND json_extract(properties, '$.external_id') = ?3 \
                       AND deleted_at IS NULL",
                    rusqlite::params![namespace, kind_str, ext_id],
                    |row| row.get(0),
                )?;
                if is_dedup {
                    return Ok(false);
                }
            }

            // The INSERT was dropped for a reason other than an external_id
            // collision.  Surface it as a constraint error.
            Err(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT),
                Some(
                    "try_insert_note: INSERT ignored for a constraint other than \
                     external_id dedup; not masking as deduplication"
                        .to_string(),
                ),
            ))
        })
        .await
    }

    async fn upsert_notes(&self, notes: Vec<Note>) -> Result<BatchWriteSummary, StorageError> {
        let attempted = notes.len() as u64;

        // ADR-067 Component A: when the write queue is enabled, route
        // through the pool-wide WriterTask. DML-only closure — no BEGIN
        // IMMEDIATE/COMMIT/ROLLBACK here, since the WriterTask's run loop
        // owns the transaction (a bare BEGIN IMMEDIATE here would violate
        // SQLite's nested-transaction rule).
        if let Some(writer_task) = &self.writer_task {
            return writer_task
                .send(move |conn| {
                    batch_upsert_notes(conn, &notes, attempted)
                        .map_err(|e| map_err(e, "upsert_notes"))
                })
                .await;
        }

        // Flag-off (default) path: byte-for-byte unchanged from pre-ADR-067
        // behavior — the closure owns its own BEGIN IMMEDIATE/COMMIT/ROLLBACK
        // via the pool-mutex writer.
        self.with_writer("upsert_notes", move |conn| {
            conn.execute_batch("BEGIN IMMEDIATE")?;
            let _tx_handle =
                khive_storage::tx_registry::register(Some("note_upsert_batch".to_string()));

            let summary = batch_upsert_notes(conn, &notes, attempted)?;

            if let Err(e) = conn.execute_batch("COMMIT") {
                let _ = conn.execute_batch("ROLLBACK");
                return Err(e);
            }
            Ok(summary)
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

    async fn get_note_including_deleted(&self, id: Uuid) -> Result<Option<Note>, StorageError> {
        let id_str = id.to_string();

        self.with_reader("get_note_including_deleted", move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, namespace, kind, status, name, content, salience, decay_factor, expires_at, \
                 properties, created_at, updated_at, deleted_at \
                 FROM notes WHERE id = ?1",
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
        let limit_i64 = i64::from(page.limit);
        let offset_i64 = i64::try_from(page.offset).map_err(|_| StorageError::InvalidInput {
            capability: StorageCapability::Notes,
            operation: "query_notes".into(),
            message: format!(
                "PageRequest: offset must be <= i64::MAX, got {}",
                page.offset
            ),
        })?;

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
            data_params.push(Box::new(limit_i64));
            data_params.push(Box::new(offset_i64));

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
        let limit_i64 = i64::from(page.limit);
        let offset_i64 = i64::try_from(page.offset).map_err(|_| StorageError::InvalidInput {
            capability: StorageCapability::Notes,
            operation: "query_notes_filtered".into(),
            message: format!(
                "PageRequest: offset must be <= i64::MAX, got {}",
                page.offset
            ),
        })?;

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
            data_params.push(Box::new(limit_i64));
            data_params.push(Box::new(offset_i64));

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

const NOTES_DDL: &str = include_str!("../../sql/notes-ddl.sql");

pub(crate) fn ensure_notes_schema(conn: &rusqlite::Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(NOTES_DDL)
}

#[cfg(test)]
#[path = "note_tests.rs"]
mod tests;
