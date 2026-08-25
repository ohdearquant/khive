//! SQL-backed `NoteStore` implementation.

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use rusqlite::OptionalExtension;
use uuid::Uuid;

use khive_storage::attachment::AttachmentSubstrate;
use khive_storage::error::{StorageError, WriterTaskRequestState};
use khive_storage::note::{FilterOp, Note, NoteFilter, SortDir};
use khive_storage::types::{
    BatchWriteSummary, DeleteMode, Page, PageRequest, SeekCursor, SeekPage, SqlStatement, SqlValue,
};
use khive_storage::NoteStore;
use khive_storage::StorageCapability;

use crate::error::SqliteError;
use crate::pool::ConnectionPool;
use crate::sql_bridge::bind_params;
use crate::stores::attachment::delete_record_attachments_statement;
use crate::writer_task::{execute_wrapped_transaction, WriterTaskHandle};

fn map_err(e: rusqlite::Error, op: &'static str) -> StorageError {
    StorageError::driver(StorageCapability::Notes, op, e)
}

fn map_sqlite_err(e: SqliteError, op: &'static str) -> StorageError {
    StorageError::driver(StorageCapability::Notes, op, e)
}

const NAMESPACE_COUNT_CHUNK_SIZE: usize = 500;

// ---------------------------------------------------------------------------
// Pure statement builders (ADR-099 B3 r6 structural cut) — see entity.rs's
// sibling block for the full rationale. `upsert_note`/`delete_note` below
// and ADR-099's atomic prepare path (`khive-runtime`) both call these.
// ---------------------------------------------------------------------------

/// The single true UPSERT every note writer (single, batch, and note-merge)
/// issues against `notes` (ADR-116 memory-ANN-generation-coherence prereq).
///
/// `INSERT OR REPLACE` is a SQLite DELETE-then-INSERT on a conflicting `id`:
/// it fires DELETE-path triggers (spuriously invalidating ANN generation
/// state once ADR-116's liveness triggers land) and discards the row's
/// original `created_at`/rowid identity. This form updates in place on a
/// primary-key conflict instead. `created_at` is deliberately absent from
/// the `DO UPDATE SET` list — it is bound for the INSERT branch only and
/// left untouched by the UPDATE branch, so an existing row keeps its
/// original `created_at` across any number of upserts.
pub const NOTE_UPSERT_SQL: &str = "INSERT INTO notes \
     (id, namespace, kind, status, name, content, salience, decay_factor, expires_at, \
      properties, created_at, updated_at, deleted_at) \
     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13) \
     ON CONFLICT(id) DO UPDATE SET \
       namespace = excluded.namespace, \
       kind = excluded.kind, \
       status = excluded.status, \
       name = excluded.name, \
       content = excluded.content, \
       salience = excluded.salience, \
       decay_factor = excluded.decay_factor, \
       expires_at = excluded.expires_at, \
       properties = excluded.properties, \
       updated_at = excluded.updated_at, \
       deleted_at = excluded.deleted_at";

/// The exact true UPSERT this store's `upsert_note` issues.
pub fn note_upsert_statement(note: &Note) -> SqlStatement {
    let properties_str = note
        .properties
        .as_ref()
        .map(|v| serde_json::to_string(v).unwrap_or_default());
    SqlStatement {
        sql: NOTE_UPSERT_SQL.to_string(),
        params: vec![
            SqlValue::Text(note.id.to_string()),
            SqlValue::Text(note.namespace.clone()),
            SqlValue::Text(note.kind.to_string()),
            SqlValue::Text(note.status.clone()),
            match &note.name {
                Some(n) => SqlValue::Text(n.clone()),
                None => SqlValue::Null,
            },
            SqlValue::Text(note.content.clone()),
            match note.salience {
                Some(s) => SqlValue::Float(s),
                None => SqlValue::Null,
            },
            match note.decay_factor {
                Some(d) => SqlValue::Float(d),
                None => SqlValue::Null,
            },
            match note.expires_at {
                Some(e) => SqlValue::Integer(e),
                None => SqlValue::Null,
            },
            match properties_str {
                Some(p) => SqlValue::Text(p),
                None => SqlValue::Null,
            },
            SqlValue::Integer(note.created_at),
            SqlValue::Integer(note.updated_at),
            match note.deleted_at {
                Some(d) => SqlValue::Integer(d),
                None => SqlValue::Null,
            },
        ],
        label: Some("note-upsert".to_string()),
    }
}

/// Full-note compare-and-swap update used after caller-side normalization was
/// derived from a read snapshot. Unlike [`note_upsert_statement`], this never
/// inserts and cannot overwrite a row whose revision or deletion marker moved
/// after the snapshot was read. The replacement revision must also be strictly
/// greater than the persisted snapshot revision; equality is a refused CAS,
/// never a successful write with an unchanged concurrency token.
pub fn note_replace_if_unchanged_statement(
    note: &Note,
    expected_updated_at: i64,
    expected_deleted_at: Option<i64>,
) -> SqlStatement {
    let properties_str = note
        .properties
        .as_ref()
        .map(|v| serde_json::to_string(v).unwrap_or_default());
    SqlStatement {
        sql: "UPDATE notes SET \
                namespace = ?1, kind = ?2, status = ?3, name = ?4, content = ?5, \
                salience = ?6, decay_factor = ?7, expires_at = ?8, properties = ?9, \
                updated_at = ?10, deleted_at = ?11 \
              WHERE id = ?12 AND updated_at = ?13 AND deleted_at IS ?14 \
                AND ?10 > updated_at"
            .to_string(),
        params: vec![
            SqlValue::Text(note.namespace.clone()),
            SqlValue::Text(note.kind.to_string()),
            SqlValue::Text(note.status.clone()),
            match &note.name {
                Some(name) => SqlValue::Text(name.clone()),
                None => SqlValue::Null,
            },
            SqlValue::Text(note.content.clone()),
            match note.salience {
                Some(value) => SqlValue::Float(value),
                None => SqlValue::Null,
            },
            match note.decay_factor {
                Some(value) => SqlValue::Float(value),
                None => SqlValue::Null,
            },
            match note.expires_at {
                Some(value) => SqlValue::Integer(value),
                None => SqlValue::Null,
            },
            match properties_str {
                Some(value) => SqlValue::Text(value),
                None => SqlValue::Null,
            },
            SqlValue::Integer(note.updated_at),
            match note.deleted_at {
                Some(value) => SqlValue::Integer(value),
                None => SqlValue::Null,
            },
            SqlValue::Text(note.id.to_string()),
            SqlValue::Integer(expected_updated_at),
            match expected_deleted_at {
                Some(value) => SqlValue::Integer(value),
                None => SqlValue::Null,
            },
        ],
        label: Some("note-replace-if-unchanged".to_string()),
    }
}

/// The exact `properties`/`updated_at` `UPDATE` this store's
/// `update_note_properties` issues. The row is patched in place without
/// rewriting any other note column or its stable row identity (#780).
/// The `comm.probe` cursor is keyed on `notes_seq.seq`, which is fixed at
/// first insert and survives a delete+reinsert of the same note id, so this
/// is defensive rather than load-bearing for cursor correctness; a metadata
/// patch should never rewrite the row regardless.
pub fn note_update_properties_statement(
    id: Uuid,
    properties: &Option<serde_json::Value>,
    updated_at: i64,
) -> SqlStatement {
    let properties_str = properties
        .as_ref()
        .map(|v| serde_json::to_string(v).unwrap_or_default());
    SqlStatement {
        sql: "UPDATE notes SET properties = ?1, updated_at = ?2 \
              WHERE id = ?3 AND deleted_at IS NULL"
            .to_string(),
        params: vec![
            match properties_str {
                Some(p) => SqlValue::Text(p),
                None => SqlValue::Null,
            },
            SqlValue::Integer(updated_at),
            SqlValue::Text(id.to_string()),
        ],
        label: Some("note-update-properties".to_string()),
    }
}

/// The atomic top-level JSON-property `UPDATE` issued by
/// [`NoteStore::set_note_property`]. The key is encoded as one quoted JSON
/// path segment, so punctuation is literal rather than interpreted as nested
/// path syntax. `json(?2)` preserves the bound value's JSON type instead of
/// storing objects, arrays, booleans, or numbers as JSON strings.
///
/// SQL-NULL documents start from `{}`. JSON arrays/scalars/null are not
/// property objects and are left untouched; the caller receives `false`.
pub fn note_set_property_statement(
    id: Uuid,
    key: &str,
    value: &serde_json::Value,
    updated_at: i64,
) -> Result<SqlStatement, StorageError> {
    // SQLite JSON-path object labels terminate at U+0000. Passing such a key
    // through `json_set` can therefore select a shorter sibling key instead
    // of the requested literal key, so reject it before constructing SQL.
    if key.contains('\0') {
        return Err(StorageError::InvalidInput {
            capability: StorageCapability::Notes,
            operation: "set_note_property".into(),
            message: "property key must not contain U+0000".to_string(),
        });
    }
    let path = format!("$.{}", serde_json::Value::String(key.to_string()));
    Ok(SqlStatement {
        sql: "UPDATE notes \
              SET properties = json_set(COALESCE(properties, '{}'), ?1, json(?2)), \
                  updated_at = ?3 \
              WHERE id = ?4 AND deleted_at IS NULL \
                AND (properties IS NULL OR json_type(properties) = 'object')"
            .to_string(),
        params: vec![
            SqlValue::Text(path),
            SqlValue::Text(value.to_string()),
            SqlValue::Integer(updated_at),
            SqlValue::Text(id.to_string()),
        ],
        label: Some("note-set-property".to_string()),
    })
}

/// The exact soft-delete `UPDATE` this store's `delete_note(Soft)` issues.
pub fn note_soft_delete_statement(id: Uuid, deleted_at: i64) -> SqlStatement {
    SqlStatement {
        sql: "UPDATE notes SET status = 'deleted', deleted_at = ?1 \
              WHERE id = ?2 AND deleted_at IS NULL"
            .to_string(),
        params: vec![
            SqlValue::Integer(deleted_at),
            SqlValue::Text(id.to_string()),
        ],
        label: Some("note-delete-soft".to_string()),
    }
}

/// The exact hard-delete `DELETE` this store's `delete_note(Hard)` issues.
pub fn note_hard_delete_statement(id: Uuid) -> SqlStatement {
    SqlStatement {
        sql: "DELETE FROM notes WHERE id = ?1".to_string(),
        params: vec![SqlValue::Text(id.to_string())],
        label: Some("note-delete-hard".to_string()),
    }
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
        // Enabled by default for file-backed pools; explicit off/degraded
        // fallback remains possible (ADR-067 Component A, mirrors
        // entity.rs policy): a missing writer task — explicitly disabled,
        // spawn degraded, or no Tokio runtime available at this first
        // access — is cached without failing construction. Every write
        // re-resolves it and applies strict/compatibility policy then.
        let writer_task = pool.writer_task_handle().ok().flatten();

        Self {
            pool,
            is_file_backed,
            writer_task,
        }
    }

    fn current_writer_task(
        &self,
        operation: &'static str,
    ) -> Result<Option<WriterTaskHandle>, StorageError> {
        self.pool
            .writer_task_for_write(self.writer_task.as_ref(), operation)
    }

    /// Route a single-row write through the pool-wide `WriterTask` when
    /// the write queue is enabled and a handle is available. Strict mode
    /// refuses a missing handle; compatibility mode falls back to the legacy
    /// pool-mutex path (ADR-067 Component A, Fork C slice 2).
    ///
    /// This is the routing point for single-statement `with_writer` callers
    /// in this store (`update_note_properties`, `set_note_property`,
    /// `delete_note`). `f` must be DML-only — on the flag-on path it runs
    /// inside the WriterTask's own transaction, so a bare `BEGIN IMMEDIATE`
    /// would violate SQLite's nested-transaction rule. `upsert_notes` (the
    /// batch method) performs the same write-time lookup first; a non-strict
    /// `None` then falls through this helper, which records the actual
    /// compatibility fallback. Strict mode returns before the direct-writer
    /// seam. Callers whose `f` issues more than one
    /// DML statement that must land atomically together (`upsert_note`,
    /// `try_insert_note`, `patch_note_property_atomic`) use
    /// [`Self::with_writer_tx`] instead — see its doc comment (khive #827,
    /// #1387).
    async fn with_writer<F, R>(&self, op: &'static str, f: F) -> Result<R, StorageError>
    where
        F: FnOnce(&rusqlite::Connection) -> Result<R, rusqlite::Error> + Send + 'static,
        R: Send + 'static,
    {
        if let Some(writer_task) = self.current_writer_task(op)? {
            return writer_task
                .send_bounded(move |conn| f(conn).map_err(|e| map_err(e, op)))
                .await;
        }

        self.pool
            .record_direct_route(crate::timeout_sink::Site::DirectRouteNote);
        let pool = Arc::clone(&self.pool);
        tokio::task::spawn_blocking(move || {
            let guard = pool.try_writer().map_err(|e| map_sqlite_err(e, op))?;
            f(guard.conn()).map_err(|e| map_err(e, op))
        })
        .await
        .map_err(|e| StorageError::driver(StorageCapability::Notes, op, e))?
    }

    /// Like [`Self::with_writer`], but for callers whose closure issues more
    /// than one DML statement that must land atomically together (khive
    /// #827, #1387), such as a note insert plus `assign_note_seq` or a guarded
    /// property patch across several notes. On the flag-on path the
    /// WriterTask already wraps every request in its own `BEGIN
    /// IMMEDIATE`/`COMMIT`/`ROLLBACK`, so `f` is sent unwrapped, same as
    /// `with_writer`. On the flag-off (pool-mutex) path, `with_writer` runs
    /// `f` in SQLite's default autocommit mode -- each statement inside `f`
    /// is its own implicit transaction -- so a later failure could leave an
    /// earlier statement committed. This wraps that path in one explicit
    /// transaction, matching `upsert_notes`' own flag-off branch.
    async fn with_writer_tx<F, R>(&self, op: &'static str, f: F) -> Result<R, StorageError>
    where
        F: FnOnce(&rusqlite::Connection) -> Result<R, rusqlite::Error> + Send + 'static,
        R: Send + 'static,
    {
        self.with_writer_tx_storage(op, move |conn| f(conn).map_err(|error| map_err(error, op)))
            .await
    }

    async fn with_writer_tx_storage<F, R>(&self, op: &'static str, f: F) -> Result<R, StorageError>
    where
        F: FnOnce(&rusqlite::Connection) -> Result<R, StorageError> + Send + 'static,
        R: Send + 'static,
    {
        if let Some(writer_task) = self.current_writer_task(op)? {
            return writer_task.send_bounded(f).await;
        }

        self.pool
            .record_direct_route(crate::timeout_sink::Site::DirectRouteNote);
        let pool = Arc::clone(&self.pool);
        tokio::task::spawn_blocking(move || {
            let guard = pool.try_writer().map_err(|e| map_sqlite_err(e, op))?;
            let conn = guard.conn();
            if !conn.is_autocommit() {
                pool.retire_pooled_writer(conn);
                return Err(StorageError::WriterTaskTerminated {
                    request_state: WriterTaskRequestState::SideEffectsUnknown,
                });
            }
            if let Err(begin_error) = conn.execute_batch("BEGIN IMMEDIATE") {
                if !conn.is_autocommit() {
                    pool.retire_pooled_writer(conn);
                    return Err(StorageError::WriterTaskTerminated {
                        request_state: WriterTaskRequestState::SideEffectsUnknown,
                    });
                }
                return Err(map_err(begin_error, op));
            }

            let (result, terminal_state) = execute_wrapped_transaction(conn, op, f);
            if terminal_state.is_some() {
                pool.retire_pooled_writer(conn);
            }
            result
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
            let pool = Arc::clone(&self.pool);
            crate::read_cancellation::run_declared_interruptible_read(
                StorageCapability::Notes,
                op,
                move |scope| {
                    scope.ensure_active()?;
                    let conn = pool
                        .open_standalone_reader()
                        .map_err(|error| map_sqlite_err(error, op))?;
                    scope.run(&conn, || f(&conn).map_err(|e| map_err(e, op)))
                },
            )
            .await
        } else {
            let pool = Arc::clone(&self.pool);
            crate::read_cancellation::run_declared_interruptible_read(
                StorageCapability::Notes,
                op,
                move |scope| {
                    let mut guard = pool
                        .reader_until(|| scope.should_stop())
                        .map_err(|e| map_sqlite_err(e, op))?
                        .ok_or_else(|| StorageError::Timeout {
                            operation: op.into(),
                        })?;
                    scope.run_pooled_reader(&mut guard, |conn| f(conn).map_err(|e| map_err(e, op)))
                },
            )
            .await
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

fn query_note_page_snapshot(
    conn: &rusqlite::Connection,
    operation: &'static str,
    namespace: &str,
    count_sql: &str,
    count_params: &[Box<dyn rusqlite::types::ToSql>],
    data_sql: &str,
    data_params: &[Box<dyn rusqlite::types::ToSql>],
) -> Result<Page<Note>, rusqlite::Error> {
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Deferred)?;

    let total: i64 = {
        let mut stmt = tx.prepare(count_sql)?;
        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            count_params.iter().map(|param| param.as_ref()).collect();
        stmt.query_row(param_refs.as_slice(), |row| row.get(0))?
    };

    #[cfg(test)]
    tests::page_snapshot_seam::hook(operation, namespace);
    #[cfg(not(test))]
    let _ = (operation, namespace);

    let items = {
        let mut stmt = tx.prepare(data_sql)?;
        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            data_params.iter().map(|param| param.as_ref()).collect();
        let rows = stmt.query_map(param_refs.as_slice(), read_note)?;
        rows.collect::<Result<Vec<_>, _>>()?
    };

    tx.commit()?;
    Ok(Page {
        items,
        total: Some(total as u64),
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

    // Prepare the UPSERT once for the whole batch — `Connection::execute`
    // re-parses and re-plans the statement on every call, which dominates
    // wall time at conflict-heavy batch sizes (measured 536ms vs 260ms at
    // 50k conflicts; see PR #1082 review).
    let mut stmt = conn.prepare_cached(NOTE_UPSERT_SQL)?;

    for note in notes {
        let id_str = note.id.to_string();
        let kind_str = note.kind.to_string();
        let status_str = note.status.clone();
        let properties_str = note
            .properties
            .as_ref()
            .map(|v| serde_json::to_string(v).unwrap_or_default());

        match stmt.execute(rusqlite::params![
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
        ]) {
            Ok(_) => {
                assign_note_seq(conn, &id_str)?;
                affected += 1;
            }
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

/// Assign a note id its durable, non-reusing sequence number the first time
/// it is inserted (khive #827 — see `sql/007-notes-seq.sql`). `INSERT OR
/// IGNORE` makes this idempotent across repeated upserts of the same note
/// id: the sequence value is fixed at the note's first insert and never
/// reassigned, unlike `notes`' own implicit rowid.
fn assign_note_seq(conn: &rusqlite::Connection, note_id: &str) -> Result<(), rusqlite::Error> {
    conn.execute(
        "INSERT OR IGNORE INTO notes_seq (note_id) VALUES (?1)",
        rusqlite::params![note_id],
    )?;
    Ok(())
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

fn build_note_where_for_namespaces(
    namespaces: &[String],
    kind: Option<&str>,
) -> (String, Vec<Box<dyn rusqlite::types::ToSql>>) {
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = namespaces
        .iter()
        .map(|namespace| -> Box<dyn rusqlite::types::ToSql> { Box::new(namespace.clone()) })
        .collect();
    let namespace_condition = match namespaces.len() {
        0 => "0".to_string(),
        1 => "namespace = ?1".to_string(),
        _ => {
            let placeholders: Vec<String> =
                (1..=namespaces.len()).map(|i| format!("?{i}")).collect();
            format!("namespace IN ({})", placeholders.join(", "))
        }
    };
    let mut conditions = vec![namespace_condition, "deleted_at IS NULL".to_string()];

    if let Some(kind) = kind {
        params.push(Box::new(kind.to_string()));
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

/// Validate a value destined for inline comparison against `json_type()`.
/// The only admissible values are SQLite's own json_type result strings —
/// a closed vocabulary — so a validated value can be inlined into SQL text
/// without any injection surface. Anything else is a caller bug, rejected
/// rather than parameterized.
fn json_type_literal(value: &SqlValue) -> Result<&str, rusqlite::Error> {
    const JSON_TYPES: [&str; 8] = [
        "true", "false", "integer", "real", "text", "array", "object", "null",
    ];
    match value {
        SqlValue::Text(s) if JSON_TYPES.contains(&s.as_str()) => Ok(s.as_str()),
        other => Err(rusqlite::Error::InvalidParameterName(format!(
            "json_type comparison value must be one of SQLite's json_type strings \
             ({JSON_TYPES:?}), got {other:?}"
        ))),
    }
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
        match &pf.op {
            FilterOp::EqOrMissing => {
                let expr = json_extract_expr(&pf.json_path);
                params.push(sql_value_param(&pf.value)?);
                // `ifnull(expr, '')` collapses the missing case into the
                // empty string so the whole match-or-missing disjunction
                // becomes IN ranges over ONE indexable expression: the
                // recipient-scoped unread-probe index
                // (`idx_notes_unread_probe_recipient`) carries this exact
                // expression as a key column, so recipient scoping happens
                // inside the index instead of row-by-row across every
                // actor's unread rows. Collapsing empty into missing is
                // deliberate and matches every consumer's read model: actor
                // labels are validated non-empty at write time, and gtd
                // already renders an empty `priority` as the default.
                conditions.push(format!(
                    "ifnull({expr}, '') IN (?{n}, '')",
                    n = params.len()
                ));
            }
            FilterOp::TextEqOrNonText => {
                let expr = json_extract_expr(&pf.json_path);
                let type_expr = json_type_expr(&pf.json_path);
                params.push(sql_value_param(&pf.value)?);
                let n = params.len();
                conditions.push(format!(
                    "CASE WHEN {type_expr} = 'text' THEN {expr} ELSE ?{n} END = ?{n}"
                ));
            }
            FilterOp::JsonTypeEq => {
                let type_expr = json_type_expr(&pf.json_path);
                params.push(sql_value_param(&pf.value)?);
                conditions.push(format!("{type_expr} = ?{}", params.len()));
            }
            FilterOp::JsonTypeNeMissing => {
                let type_expr = json_type_expr(&pf.json_path);
                // Inlined as a validated literal, NOT a parameter: the
                // partial unread index (`idx_notes_unread_probe_recipient`) carries
                // this exact predicate in its WHERE clause, and SQLite can
                // only prove a query implies an index predicate when the
                // compared value is known at plan time — a bound parameter
                // defeats the index and the scan degrades to
                // mailbox-proportional work. The value domain is SQLite's
                // closed json_type vocabulary, so inlining is injection-safe
                // by construction.
                let literal = json_type_literal(&pf.value)?;
                conditions.push(format!(
                    "({type_expr} IS NULL OR {type_expr} != '{literal}')"
                ));
            }
            FilterOp::In(values) => {
                let expr = json_extract_expr(&pf.json_path);
                if values.is_empty() {
                    // An empty set can never match any row.
                    conditions.push("0".to_string());
                    continue;
                }
                let mut placeholders = Vec::with_capacity(values.len());
                for v in values {
                    params.push(sql_value_param(v)?);
                    placeholders.push(format!("?{}", params.len()));
                }
                conditions.push(format!("{expr} IN ({})", placeholders.join(", ")));
            }
            FilterOp::NotInOrMissing(values) => {
                let expr = json_extract_expr(&pf.json_path);
                if values.is_empty() {
                    // Nothing to exclude — every row (including missing) matches.
                    continue;
                }
                let mut placeholders = Vec::with_capacity(values.len());
                for v in values {
                    params.push(sql_value_param(v)?);
                    placeholders.push(format!("?{}", params.len()));
                }
                conditions.push(format!(
                    "({expr} IS NULL OR {expr} NOT IN ({}))",
                    placeholders.join(", ")
                ));
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
                    FilterOp::EqOrMissing
                    | FilterOp::TextEqOrNonText
                    | FilterOp::JsonTypeEq
                    | FilterOp::JsonTypeNeMissing
                    | FilterOp::In(_)
                    | FilterOp::NotInOrMissing(_) => {
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

fn execute_filtered_note_property_patch(
    conn: &rusqlite::Connection,
    id: Uuid,
    namespace: &str,
    filter: &NoteFilter,
    json_path: &str,
    value_json: &str,
    updated_at: i64,
) -> Result<usize, rusqlite::Error> {
    let (where_clause, mut params) = build_note_filter_where(namespace, filter)?;

    let base = params.len();
    let sql = format!(
        "UPDATE notes SET properties = json_set(COALESCE(properties, '{{}}'), ?{p1}, json(?{p2})), \
         updated_at = ?{p3} {where_clause} \
         AND (properties IS NULL OR json_type(properties) = 'object') AND id = ?{p4}",
        p1 = base + 1,
        p2 = base + 2,
        p3 = base + 3,
        p4 = base + 4,
    );
    params.push(Box::new(json_path.to_string()));
    params.push(Box::new(value_json.to_string()));
    params.push(Box::new(updated_at));
    params.push(Box::new(id.to_string()));

    let mut stmt = conn.prepare_cached(&sql)?;
    let param_refs: Vec<&dyn rusqlite::types::ToSql> =
        params.iter().map(|param| param.as_ref()).collect();
    stmt.execute(param_refs.as_slice())
}

// =============================================================================
// NoteStore implementation
// =============================================================================

#[async_trait]
impl NoteStore for SqlNoteStore {
    async fn upsert_note(&self, note: Note) -> Result<(), StorageError> {
        let id_str = note.id.to_string();
        let statement = note_upsert_statement(&note);
        self.with_writer_tx("upsert_note", move |conn| {
            let mut stmt = conn.prepare_cached(&statement.sql)?;
            bind_params(&mut stmt, &statement.params)?;
            stmt.raw_execute()?;
            assign_note_seq(conn, &id_str)?;
            Ok(())
        })
        .await
    }

    async fn replace_note_if_unchanged(
        &self,
        note: Note,
        expected_updated_at: i64,
        expected_deleted_at: Option<i64>,
    ) -> Result<bool, StorageError> {
        let statement =
            note_replace_if_unchanged_statement(&note, expected_updated_at, expected_deleted_at);
        self.with_writer("replace_note_if_unchanged", move |conn| {
            let mut stmt = conn.prepare(&statement.sql)?;
            bind_params(&mut stmt, &statement.params)?;
            Ok(stmt.raw_execute()? > 0)
        })
        .await
    }

    async fn update_note_properties(
        &self,
        id: Uuid,
        properties: Option<serde_json::Value>,
        updated_at: i64,
    ) -> Result<bool, StorageError> {
        let statement = note_update_properties_statement(id, &properties, updated_at);
        self.with_writer("update_note_properties", move |conn| {
            let mut stmt = conn.prepare(&statement.sql)?;
            bind_params(&mut stmt, &statement.params)?;
            Ok(stmt.raw_execute()? > 0)
        })
        .await
    }

    async fn set_note_property(
        &self,
        id: Uuid,
        key: &str,
        value: serde_json::Value,
        updated_at: i64,
    ) -> Result<bool, StorageError> {
        let statement = note_set_property_statement(id, key, &value, updated_at)?;
        self.with_writer("set_note_property", move |conn| {
            let mut stmt = conn.prepare(&statement.sql)?;
            bind_params(&mut stmt, &statement.params)?;
            Ok(stmt.raw_execute()? > 0)
        })
        .await
    }

    async fn try_patch_note_property(
        &self,
        id: Uuid,
        namespace: &str,
        filter: &NoteFilter,
        json_path: &str,
        value: serde_json::Value,
        updated_at: i64,
    ) -> Result<bool, StorageError> {
        let namespace = namespace.to_string();
        let filter = filter.clone();
        let value_json = serde_json::to_string(&value).map_err(|e| {
            StorageError::driver(StorageCapability::Notes, "try_patch_note_property", e)
        })?;
        let json_path = json_path.to_string();

        self.with_writer("try_patch_note_property", move |conn| {
            execute_filtered_note_property_patch(
                conn,
                id,
                &namespace,
                &filter,
                &json_path,
                &value_json,
                updated_at,
            )
            .map(|rows| rows > 0)
        })
        .await
    }

    async fn patch_note_property_atomic(
        &self,
        mut ids: Vec<Uuid>,
        namespace: &str,
        filter: &NoteFilter,
        json_path: &str,
        value: serde_json::Value,
        updated_at: i64,
    ) -> Result<(), StorageError> {
        let mut seen = HashSet::with_capacity(ids.len());
        ids.retain(|id| seen.insert(*id));
        if ids.is_empty() {
            return Err(StorageError::InvalidInput {
                capability: StorageCapability::Notes,
                operation: "patch_note_property_atomic".into(),
                message: "at least one note id is required".to_string(),
            });
        }

        let namespace = namespace.to_string();
        let filter = filter.clone();
        let value_json = serde_json::to_string(&value).map_err(|e| {
            StorageError::driver(StorageCapability::Notes, "patch_note_property_atomic", e)
        })?;
        let json_path = json_path.to_string();

        self.with_writer_tx_storage("patch_note_property_atomic", move |conn| {
            for id in ids {
                let rows = execute_filtered_note_property_patch(
                    conn,
                    id,
                    &namespace,
                    &filter,
                    &json_path,
                    &value_json,
                    updated_at,
                )
                .map_err(|error| map_err(error, "patch_note_property_atomic"))?;
                if rows != 1 {
                    return Err(StorageError::Conflict {
                        capability: StorageCapability::Notes,
                        operation: "patch_note_property_atomic".into(),
                        message: format!(
                            "precondition failed for note {id}: guarded update changed {rows} rows; expected 1"
                        ),
                    });
                }
            }
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

        self.with_writer_tx("try_insert_note", move |conn| {
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
                assign_note_seq(conn, &id_str)?;
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

        // khive #827: route through `with_writer_tx`
        // instead of hand-rolling BEGIN IMMEDIATE/COMMIT/ROLLBACK here. The
        // old flag-off path only rolled back when the final COMMIT failed —
        // an earlier error from `batch_upsert_notes` (e.g. a failed
        // `assign_note_seq`) propagated via `?` straight out of the closure,
        // skipping ROLLBACK entirely and leaving BEGIN IMMEDIATE open on the
        // shared pool-mutex connection, poisoning every later write on that
        // connection. `with_writer_tx` rolls back on ANY error from `f`, on
        // both the flag-on (WriterTask, which wraps its own transaction) and
        // flag-off (pool-mutex) paths.
        let origin = self.pool.origin();
        self.with_writer_tx("upsert_notes", move |conn| {
            let _tx_handle = khive_storage::tx_registry::register_scoped(
                Some("note_upsert_batch".to_string()),
                origin,
            );
            batch_upsert_notes(conn, &notes, attempted)
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

    async fn note_sequence(&self, id: Uuid) -> Result<Option<i64>, StorageError> {
        let id = id.to_string();
        self.with_reader("note_sequence", move |conn| {
            conn.query_row(
                "SELECT seq FROM notes_seq WHERE note_id = ?1",
                rusqlite::params![id],
                |row| row.get(0),
            )
            .optional()
        })
        .await
    }

    async fn get_notes_batch(&self, ids: &[Uuid]) -> Result<Vec<Note>, StorageError> {
        if ids.is_empty() {
            return Ok(vec![]);
        }
        // SQLite SQLITE_MAX_VARIABLE_NUMBER defaults to 999; chunk below that
        // ceiling so callers can safely hydrate arbitrarily large ID sets.
        const CHUNK: usize = 900;
        let id_strings: Vec<String> = ids.iter().map(|id| id.to_string()).collect();

        let mut result = Vec::with_capacity(ids.len());
        for chunk in id_strings.chunks(CHUNK) {
            let chunk_owned = chunk.to_vec();
            let notes = self
                .with_reader("get_notes_batch", move |conn| {
                    let placeholders: String = (1..=chunk_owned.len())
                        .map(|i| format!("?{i}"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    let sql = format!(
                        "SELECT id, namespace, kind, status, name, content, salience, decay_factor, expires_at, \
                         properties, created_at, updated_at, deleted_at \
                         FROM notes WHERE id IN ({placeholders}) AND deleted_at IS NULL"
                    );
                    let mut stmt = conn.prepare(&sql)?;
                    let params: Vec<&dyn rusqlite::types::ToSql> = chunk_owned
                        .iter()
                        .map(|s| s as &dyn rusqlite::types::ToSql)
                        .collect();
                    let rows = stmt.query_map(params.as_slice(), read_note)?;
                    let mut notes = Vec::new();
                    for row in rows {
                        notes.push(row?);
                    }
                    Ok(notes)
                })
                .await?;
            result.extend(notes);
        }
        Ok(result)
    }

    async fn delete_note(&self, id: Uuid, mode: DeleteMode) -> Result<bool, StorageError> {
        match mode {
            DeleteMode::Soft => {
                let now = chrono::Utc::now().timestamp_micros();
                let statement = note_soft_delete_statement(id, now);
                self.with_writer("delete_note_soft", move |conn| {
                    let mut stmt = conn.prepare(&statement.sql)?;
                    bind_params(&mut stmt, &statement.params)?;
                    Ok(stmt.raw_execute()? > 0)
                })
                .await
            }
            DeleteMode::Hard => {
                let note_statement = note_hard_delete_statement(id);
                let attachment_statement =
                    delete_record_attachments_statement(id, AttachmentSubstrate::Note);
                self.with_writer_tx("delete_note_hard", move |conn| {
                    let mut note_stmt = conn.prepare(&note_statement.sql)?;
                    bind_params(&mut note_stmt, &note_statement.params)?;
                    let deleted = note_stmt.raw_execute()? > 0;
                    drop(note_stmt);
                    if deleted {
                        let mut attachment_stmt = conn.prepare(&attachment_statement.sql)?;
                        bind_params(&mut attachment_stmt, &attachment_statement.params)?;
                        attachment_stmt.raw_execute()?;
                    }
                    Ok(deleted)
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
            let count_sql = format!("SELECT COUNT(*) FROM notes{count_sql}");

            let (where_sql, mut data_params) = build_note_where(&namespace, kind.as_deref());
            data_params.push(Box::new(limit_i64));
            data_params.push(Box::new(offset_i64));

            let limit_idx = data_params.len() - 1;
            let offset_idx = data_params.len();

            let data_sql = format!(
                "SELECT id, namespace, kind, status, name, content, salience, decay_factor, expires_at, \
                 properties, created_at, updated_at, deleted_at \
                 FROM notes{} ORDER BY created_at DESC, id ASC LIMIT ?{} OFFSET ?{}",
                where_sql, limit_idx, offset_idx,
            );

            query_note_page_snapshot(
                conn,
                "query_notes",
                &namespace,
                &count_sql,
                &count_params,
                &data_sql,
                &data_params,
            )
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
            let count_sql = format!("SELECT COUNT(*) FROM notes{count_sql}");

            let (where_sql, mut data_params) = build_note_filter_where(&namespace, &filter)?;
            data_params.push(Box::new(limit_i64));
            data_params.push(Box::new(offset_i64));

            let order_clause = match &filter.order_by {
                Some((path, dir)) => {
                    let dir_str = match dir {
                        SortDir::Asc => "ASC",
                        SortDir::Desc => "DESC",
                    };
                    // #1671: append `id` as the final tiebreak in the sort
                    // field's direction so offset pages form a deterministic
                    // total order even when the JSON sort value repeats. The
                    // total order removes tie-order instability only — offset
                    // paging can still duplicate or skip rows under concurrent
                    // inserts/deletes or sort-key updates (that would need
                    // snapshot isolation or keyset pagination).
                    format!(
                        " ORDER BY {} {dir_str}, id {dir_str}",
                        json_extract_expr(path)
                    )
                }
                // #1671: intentionally left unchanged — `id ASC` over the
                // primary key already makes this clause a deterministic total
                // order; flipping the direction would change the observable
                // default order for existing consumers without fixing
                // anything.
                None => " ORDER BY created_at DESC, id ASC".to_string(),
            };

            let limit_idx = data_params.len() - 1;
            let offset_idx = data_params.len();
            let data_sql = format!(
                "SELECT id, namespace, kind, status, name, content, salience, decay_factor, \
                 expires_at, properties, created_at, updated_at, deleted_at \
                 FROM notes{}{order_clause} LIMIT ?{} OFFSET ?{}",
                where_sql, limit_idx, offset_idx,
            );

            query_note_page_snapshot(
                conn,
                "query_notes_filtered",
                &namespace,
                &count_sql,
                &count_params,
                &data_sql,
                &data_params,
            )
        })
        .await
    }

    async fn query_notes_filtered_after(
        &self,
        namespace: &str,
        filter: &NoteFilter,
        after: Option<SeekCursor>,
        limit: u32,
    ) -> Result<SeekPage<Note>, StorageError> {
        if limit == 0 {
            return Ok(SeekPage::default());
        }
        if filter.order_by.is_some() {
            return Err(StorageError::InvalidInput {
                capability: StorageCapability::Notes,
                operation: "query_notes_filtered_after".into(),
                message: "custom order_by is not compatible with insertion-sequence pagination"
                    .into(),
            });
        }
        for property_filter in &filter.property_filters {
            validate_json_path(&property_filter.json_path)?;
        }

        let namespace = namespace.to_string();
        let filter = filter.clone();
        let limit_usize = limit as usize;
        let probe_limit_i64 = i64::from(limit) + 1;
        self.with_reader("query_notes_filtered_after", move |conn| {
            let (mut where_sql, mut params) = build_note_filter_where(&namespace, &filter)?;
            if let Some(cursor) = after {
                params.push(Box::new(cursor.sequence));
                where_sql.push_str(&format!(" AND notes_seq.seq > ?{}", params.len()));
            }
            params.push(Box::new(probe_limit_i64));
            let limit_idx = params.len();
            // CROSS JOIN fixes the ledger as the outer loop, preserving an
            // indexed `seq > boundary` scan with no full-match sort.
            let sql = format!(
                "SELECT id, namespace, kind, status, name, content, salience, decay_factor, \
                 expires_at, properties, created_at, updated_at, deleted_at, notes_seq.seq \
                 FROM notes_seq CROSS JOIN notes ON notes.id = notes_seq.note_id{where_sql} \
                 ORDER BY notes_seq.seq ASC LIMIT ?{limit_idx}"
            );
            let mut stmt = conn.prepare(&sql)?;
            let param_refs: Vec<&dyn rusqlite::types::ToSql> =
                params.iter().map(|param| param.as_ref()).collect();
            let rows = stmt.query_map(param_refs.as_slice(), |row| {
                Ok((read_note(row)?, row.get::<_, i64>(13)?))
            })?;
            let mut entries = rows.collect::<Result<Vec<_>, _>>()?;
            let has_more = entries.len() > limit_usize;
            if has_more {
                entries.truncate(limit_usize);
            }
            let next_after = if has_more {
                entries.last().map(|(note, sequence)| SeekCursor {
                    sequence: *sequence,
                    id: note.id,
                })
            } else {
                None
            };
            let items = entries.into_iter().map(|(note, _)| note).collect();
            Ok(SeekPage { items, next_after })
        })
        .await
    }

    async fn query_notes_filtered_bounded(
        &self,
        namespace: &str,
        filter: &NoteFilter,
        max_rows: u32,
    ) -> Result<Vec<Note>, StorageError> {
        for pf in &filter.property_filters {
            validate_json_path(&pf.json_path)?;
        }
        if let Some((path, _)) = &filter.order_by {
            validate_json_path(path)?;
        }

        let namespace = namespace.to_string();
        let filter = filter.clone();
        let limit_i64 = i64::from(max_rows) + 1;

        self.with_reader("query_notes_filtered_bounded", move |conn| {
            let (where_sql, mut data_params) = build_note_filter_where(&namespace, &filter)?;
            data_params.push(Box::new(limit_i64));
            let limit_idx = data_params.len();

            // Tie-break on `id` in addition to the primary sort key so the
            // snapshot ordering is fully deterministic even when many rows
            // share the same `created_at` (or the same custom sort value).
            let order_clause = match &filter.order_by {
                Some((path, dir)) => {
                    let dir_str = match dir {
                        SortDir::Asc => "ASC",
                        SortDir::Desc => "DESC",
                    };
                    format!(" ORDER BY {} {dir_str}, id ASC", json_extract_expr(path))
                }
                None => " ORDER BY created_at DESC, id ASC".to_string(),
            };

            let data_sql = format!(
                "SELECT id, namespace, kind, status, name, content, salience, decay_factor, \
                 expires_at, properties, created_at, updated_at, deleted_at \
                 FROM notes{where_sql}{order_clause} LIMIT ?{limit_idx}",
            );

            let mut stmt = conn.prepare(&data_sql)?;
            let param_refs: Vec<&dyn rusqlite::types::ToSql> =
                data_params.iter().map(|p| p.as_ref()).collect();
            let rows = stmt.query_map(param_refs.as_slice(), read_note)?;

            let mut items = Vec::new();
            for row in rows {
                items.push(row?);
            }
            Ok(items)
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

    async fn count_notes_in_namespaces(
        &self,
        namespaces: &[String],
        kind: Option<&str>,
    ) -> Result<u64, StorageError> {
        let namespaces: Vec<String> = namespaces
            .iter()
            .cloned()
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        let kind = kind.map(str::to_string);

        self.with_reader("count_notes_in_namespaces", move |conn| {
            let mut total = 0;
            for chunk in namespaces.chunks(NAMESPACE_COUNT_CHUNK_SIZE) {
                let (where_sql, params) = build_note_where_for_namespaces(chunk, kind.as_deref());
                let sql = format!("SELECT COUNT(*) FROM notes{where_sql}");
                let mut stmt = conn.prepare(&sql)?;
                let param_refs: Vec<&dyn rusqlite::types::ToSql> =
                    params.iter().map(|p| p.as_ref()).collect();
                let count: i64 = stmt.query_row(param_refs.as_slice(), |row| row.get(0))?;
                total += count as u64;
            }
            Ok(total)
        })
        .await
    }
}

// =============================================================================
// DDL
// =============================================================================

const NOTES_DDL: &str = include_str!("../../sql/notes-ddl.sql");

/// Same anti-join repair as `sql/008-notes-seq-repair.sql` (the V8 forward
/// migration) -- shared via `include_str!` from that single source file
/// rather than duplicated as SQL text. `INSERT OR IGNORE` targets notes
/// still missing a `notes_seq` row specifically, so it is correct to run
/// against a fresh ledger, a partially populated one, or an already fully
/// repaired one.
const NOTES_SEQ_REPAIR_DDL: &str = include_str!("../../sql/008-notes-seq-repair.sql");

pub(crate) fn ensure_notes_schema(conn: &rusqlite::Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(NOTES_DDL)
}

/// Anti-join backfill of `notes_seq` for any note still missing a row
/// (khive #827). Scans `notes` in full, so callers MUST gate this to
/// run at most once per backend/pool rather than on every store acquisition
/// (khive #827) -- see
/// `StorageBackend::notes_for_namespace`.
pub(crate) fn repair_notes_seq(conn: &rusqlite::Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(NOTES_SEQ_REPAIR_DDL)
}

#[cfg(test)]
#[path = "note_tests.rs"]
mod tests;
