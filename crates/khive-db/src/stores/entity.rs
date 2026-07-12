//! SQL-backed `EntityStore` implementation.

use std::sync::Arc;

use async_trait::async_trait;
use uuid::Uuid;

use khive_storage::entity::{Entity, EntityFilter};
use khive_storage::error::StorageError;
use khive_storage::types::{
    BatchWriteSummary, DeleteMode, Page, PageRequest, SqlStatement, SqlValue,
};
use khive_storage::EntityStore;
use khive_storage::StorageCapability;

use crate::error::SqliteError;
use crate::pool::ConnectionPool;
use crate::sql_bridge::bind_params;
use crate::writer_task::WriterTaskHandle;

fn map_err(e: rusqlite::Error, op: &'static str) -> StorageError {
    StorageError::driver(StorageCapability::Entities, op, e)
}

fn map_sqlite_err(e: SqliteError, op: &'static str) -> StorageError {
    StorageError::driver(StorageCapability::Entities, op, e)
}

// ---------------------------------------------------------------------------
// Pure statement builders (ADR-099 B3 r6 structural cut)
//
// These carry NO I/O — they turn an already-computed `Entity` (or a bare id)
// into the exact `SqlStatement` this store executes. `upsert_entity` and
// `delete_entity` below call them and execute the result; ADR-099's atomic
// prepare path (`khive-runtime`) calls them too, to build the same statement
// for its own guarded, synchronous apply. One statement generator, two
// execution mechanisms (async trait dispatch vs. synchronous atomic unit) —
// per ADR-099's accepted "handler-logic-duplication objection" text, the
// bulk-apply path reuses the handler's existing statement generation instead
// of re-deriving it.
// ---------------------------------------------------------------------------

/// The exact `INSERT OR REPLACE` this store's `upsert_entity` issues.
pub fn entity_upsert_statement(entity: &Entity) -> SqlStatement {
    let properties_str = entity
        .properties
        .as_ref()
        .map(|v| serde_json::to_string(v).unwrap_or_default());
    let tags_str = serde_json::to_string(&entity.tags).unwrap_or_else(|_| "[]".to_string());
    SqlStatement {
        sql: "INSERT OR REPLACE INTO entities \
              (id, namespace, kind, entity_type, name, description, properties, tags, \
               created_at, updated_at, deleted_at, merged_into, merge_event_id) \
              VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)"
            .to_string(),
        params: vec![
            SqlValue::Text(entity.id.to_string()),
            SqlValue::Text(entity.namespace.clone()),
            SqlValue::Text(entity.kind.clone()),
            match &entity.entity_type {
                Some(t) => SqlValue::Text(t.clone()),
                None => SqlValue::Null,
            },
            SqlValue::Text(entity.name.clone()),
            match &entity.description {
                Some(d) => SqlValue::Text(d.clone()),
                None => SqlValue::Null,
            },
            match properties_str {
                Some(p) => SqlValue::Text(p),
                None => SqlValue::Null,
            },
            SqlValue::Text(tags_str),
            SqlValue::Integer(entity.created_at),
            SqlValue::Integer(entity.updated_at),
            match entity.deleted_at {
                Some(d) => SqlValue::Integer(d),
                None => SqlValue::Null,
            },
            match entity.merged_into {
                Some(u) => SqlValue::Text(u.to_string()),
                None => SqlValue::Null,
            },
            match entity.merge_event_id {
                Some(u) => SqlValue::Text(u.to_string()),
                None => SqlValue::Null,
            },
        ],
        label: Some("entity-upsert".to_string()),
    }
}

/// The exact soft-delete `UPDATE` this store's `delete_entity(Soft)` issues.
pub fn entity_soft_delete_statement(id: Uuid, deleted_at: i64) -> SqlStatement {
    SqlStatement {
        sql: "UPDATE entities SET deleted_at = ?1 WHERE id = ?2 AND deleted_at IS NULL".to_string(),
        params: vec![
            SqlValue::Integer(deleted_at),
            SqlValue::Text(id.to_string()),
        ],
        label: Some("entity-delete-soft".to_string()),
    }
}

/// The exact hard-delete `DELETE` this store's `delete_entity(Hard)` issues
/// (no `deleted_at` predicate — purges live and already-tombstoned rows).
pub fn entity_hard_delete_statement(id: Uuid) -> SqlStatement {
    SqlStatement {
        sql: "DELETE FROM entities WHERE id = ?1".to_string(),
        params: vec![SqlValue::Text(id.to_string())],
        label: Some("entity-delete-hard".to_string()),
    }
}

/// An EntityStore backed by SQLite. Namespace is the caller's responsibility.
///
/// UUID is globally unique — get/delete by ID alone. Query/count use the
/// namespace parameter as passed. The store is just a pool + is_file_backed.
pub struct SqlEntityStore {
    pool: Arc<ConnectionPool>,
    is_file_backed: bool,
    writer_task: Option<WriterTaskHandle>,
}

impl SqlEntityStore {
    /// Create a new store.
    ///
    /// When `KHIVE_WRITE_QUEUE=1` (`PoolConfig::write_queue_enabled`), every
    /// write path on this store — the batch `upsert_entities` (its own
    /// explicit flag check) AND every single-row write routed through the
    /// shared `with_writer` helper (`upsert_entity`, `delete_entity`) —
    /// routes through the pool-wide `WriterTask`
    /// (`ConnectionPool::writer_task_handle`) instead of the legacy
    /// pool-mutex path. The handle is a clone of the ONE writer task owned
    /// by `pool` — constructing multiple stores (or multiple namespaces)
    /// over the same pool never spawns more than one writer task; see
    /// `ConnectionPool::writer_task_handle`'s doc comment for why that
    /// matters. `None` (falling back to the legacy path for every write)
    /// if the flag is off, or if the writer task failed to spawn (for
    /// example, an in-memory pool, which has no standalone-connection
    /// support) — the flag is a best-effort opt-in, not a hard requirement.
    pub fn new(pool: Arc<ConnectionPool>, is_file_backed: bool) -> Self {
        // Best-effort opt-in (slice 1 policy, unchanged): a missing writer
        // task — whether the flag is off, spawn degraded (e.g. in-memory
        // pool), or no Tokio runtime was available at this first access
        // (ADR-067 Component A runtime-handle guard) — degrades to the
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
            operation: "entity_reader".into(),
            message: "in-memory databases do not support standalone connections".into(),
        })?;

        let conn = rusqlite::Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX
                | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )
        .map_err(|e| map_err(e, "open_entity_reader"))?;

        conn.busy_timeout(config.busy_timeout)
            .map_err(|e| map_err(e, "open_entity_reader"))?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(|e| map_err(e, "open_entity_reader"))?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(|e| map_err(e, "open_entity_reader"))?;

        Ok(conn)
    }

    /// Route a single-row write through the pool-wide `WriterTask` when
    /// `KHIVE_WRITE_QUEUE=1` and a handle is available; otherwise fall back
    /// to the legacy pool-mutex path.
    ///
    /// ADR-067 Component A (Fork C slice 2): this is the ONE routing point
    /// for every `with_writer` caller in this store — `upsert_entity`,
    /// `delete_entity` (soft/hard) all reach the WriterTask through this
    /// helper rather than each duplicating the flag check. `f` must be
    /// DML-only (a single statement, no bare `BEGIN IMMEDIATE`): on the
    /// flag-on path it runs inside the WriterTask's own transaction, and a
    /// nested `BEGIN IMMEDIATE` would violate SQLite's nested-transaction
    /// rule. `upsert_entities` (the batch method) does its OWN flag check
    /// and returns early on `Some`, so its fallback call into this helper
    /// only ever executes on the flag-off path (`self.writer_task` is
    /// `None` by construction whenever that call is reached) — no
    /// double-routing.
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
        .map_err(|e| StorageError::driver(StorageCapability::Entities, op, e))?
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
                .map_err(|e| StorageError::driver(StorageCapability::Entities, op, e))?
        } else {
            let pool = Arc::clone(&self.pool);
            tokio::task::spawn_blocking(move || {
                let guard = pool.reader().map_err(|e| map_sqlite_err(e, op))?;
                f(guard.conn()).map_err(|e| map_err(e, op))
            })
            .await
            .map_err(|e| StorageError::driver(StorageCapability::Entities, op, e))?
        }
    }
}

// =============================================================================
// Helpers
// =============================================================================

fn read_entity(row: &rusqlite::Row<'_>) -> Result<Entity, rusqlite::Error> {
    let id_str: String = row.get(0)?;
    let namespace: String = row.get(1)?;
    let kind: String = row.get(2)?;
    let entity_type: Option<String> = row.get(3)?;
    let name: String = row.get(4)?;
    let description: Option<String> = row.get(5)?;
    let properties_str: Option<String> = row.get(6)?;
    let tags_str: String = row.get(7)?;
    let created_at: i64 = row.get(8)?;
    let updated_at: i64 = row.get(9)?;
    let deleted_at: Option<i64> = row.get(10)?;
    let merged_into_str: Option<String> = row.get(11)?;
    let merge_event_id_str: Option<String> = row.get(12)?;

    let id = parse_uuid(&id_str)?;

    let properties = properties_str
        .map(|s| {
            serde_json::from_str(&s).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    6,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })
        })
        .transpose()?;

    let tags: Vec<String> = serde_json::from_str(&tags_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(7, rusqlite::types::Type::Text, Box::new(e))
    })?;

    let merged_into = merged_into_str
        .as_deref()
        .map(Uuid::parse_str)
        .transpose()
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(10, rusqlite::types::Type::Text, Box::new(e))
        })?;

    let merge_event_id = merge_event_id_str
        .as_deref()
        .map(Uuid::parse_str)
        .transpose()
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(11, rusqlite::types::Type::Text, Box::new(e))
        })?;

    Ok(Entity {
        id,
        namespace,
        kind,
        entity_type,
        name,
        description,
        properties,
        tags,
        created_at,
        updated_at,
        deleted_at,
        merged_into,
        merge_event_id,
    })
}

/// DML-only batch upsert loop shared by both the legacy (flag-off) and
/// WriterTask-routed (flag-on) `upsert_entities` paths (ADR-067 slice 1).
///
/// Issues no `BEGIN` / `COMMIT` / `ROLLBACK` itself — the caller owns the
/// enclosing transaction. Per-row failures are captured into
/// `BatchWriteSummary::failed`/`first_error` rather than aborting the loop,
/// matching the existing partial-success contract: this function's own
/// `Result` is `Ok` unless a caller bug is present, since no branch here
/// returns `Err`.
fn batch_upsert_entities(
    conn: &rusqlite::Connection,
    entities: &[Entity],
    attempted: u64,
) -> Result<BatchWriteSummary, rusqlite::Error> {
    let mut affected = 0u64;
    let mut failed = 0u64;
    let mut first_error = String::new();

    for entity in entities {
        let id_str = entity.id.to_string();
        let properties_str = entity
            .properties
            .as_ref()
            .map(|v| serde_json::to_string(v).unwrap_or_default());
        let tags_str = serde_json::to_string(&entity.tags).unwrap_or_else(|_| "[]".to_string());

        let merged_into_str = entity.merged_into.map(|u| u.to_string());
        let merge_event_id_str = entity.merge_event_id.map(|u| u.to_string());
        match conn.execute(
            "INSERT OR REPLACE INTO entities \
             (id, namespace, kind, entity_type, name, description, properties, tags, \
              created_at, updated_at, deleted_at, merged_into, merge_event_id) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            rusqlite::params![
                id_str,
                &entity.namespace,
                entity.kind,
                entity.entity_type,
                entity.name,
                entity.description,
                properties_str,
                tags_str,
                entity.created_at,
                entity.updated_at,
                entity.deleted_at,
                merged_into_str,
                merge_event_id_str,
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

fn parse_uuid(s: &str) -> Result<Uuid, rusqlite::Error> {
    Uuid::parse_str(s).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })
}

/// Escape SQLite `LIKE` wildcard characters (`%`, `_`) and the escape
/// character itself (`\`) so a caller-supplied name is matched literally
/// under `LIKE ... ESCAPE '\'` rather than as a pattern (#818: an
/// entity named e.g. `a_b` must not also match `aXb`, and a name containing
/// `%` must not silently widen into a broad substring scan).
fn escape_like(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        if matches!(c, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn build_entity_where(
    namespace: &str,
    filter: &EntityFilter,
) -> (String, Vec<Box<dyn rusqlite::types::ToSql>>) {
    // When filter.namespaces is non-empty use `namespace IN (...)` so that
    // multi-namespace read visibility works.  Otherwise fall back to the
    // single-namespace equality check for backward compatibility.
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

    let mut conditions: Vec<String> = vec![ns_condition, "deleted_at IS NULL".to_string()];
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = ns_params;

    if !filter.ids.is_empty() {
        let placeholders: Vec<String> = filter
            .ids
            .iter()
            .map(|id| {
                params.push(Box::new(id.to_string()));
                format!("?{}", params.len())
            })
            .collect();
        conditions.push(format!("id IN ({})", placeholders.join(", ")));
    }

    if !filter.kinds.is_empty() {
        let placeholders: Vec<String> = filter
            .kinds
            .iter()
            .map(|k| {
                params.push(Box::new(k.clone()));
                format!("?{}", params.len())
            })
            .collect();
        conditions.push(format!("kind IN ({})", placeholders.join(", ")));
    }

    if !filter.entity_types.is_empty() {
        let placeholders: Vec<String> = filter
            .entity_types
            .iter()
            .map(|t| {
                params.push(Box::new(t.clone()));
                format!("?{}", params.len())
            })
            .collect();
        conditions.push(format!("entity_type IN ({})", placeholders.join(", ")));
    }

    if let Some(ref prefix) = filter.name_prefix {
        params.push(Box::new(format!("{}%", escape_like(prefix))));
        conditions.push(format!("name LIKE ?{} ESCAPE '\\'", params.len()));
    }

    if let Some(ref exact) = filter.name_exact {
        params.push(Box::new(exact.clone()));
        // `entities.name` has no `COLLATE NOCASE` (see sql/schema.sql), so
        // `=` is already SQLite's default case-sensitive BINARY comparison.
        // `COLLATE BINARY` is spelled out here so this predicate stays
        // correct even if the column's default collation ever changes.
        conditions.push(format!("name = ?{} COLLATE BINARY", params.len()));
    }

    if !filter.names_ci.is_empty() {
        // ADR-104 Stage C, R1: one batched `LOWER(name) IN (...)` predicate,
        // served by `idx_entities_namespace_name_ci (namespace, LOWER(name))`.
        let placeholders: Vec<String> = filter
            .names_ci
            .iter()
            .map(|n| {
                params.push(Box::new(n.to_lowercase()));
                format!("?{}", params.len())
            })
            .collect();
        conditions.push(format!("LOWER(name) IN ({})", placeholders.join(", ")));
    }

    if !filter.tags_any.is_empty() {
        let placeholders: Vec<String> = filter
            .tags_any
            .iter()
            .map(|t| {
                // Normalise to lowercase so the comparison is case-insensitive
                // domain filter must be case-insensitive.
                params.push(Box::new(t.to_lowercase()));
                format!("?{}", params.len())
            })
            .collect();
        conditions.push(format!(
            "EXISTS (SELECT 1 FROM json_each(tags) WHERE LOWER(json_each.value) IN ({}))",
            placeholders.join(", ")
        ));
    }

    let clause = format!(" WHERE {}", conditions.join(" AND "));
    (clause, params)
}

// =============================================================================
// EntityStore implementation
// =============================================================================

#[async_trait]
impl EntityStore for SqlEntityStore {
    async fn upsert_entity(&self, entity: Entity) -> Result<(), StorageError> {
        let statement = entity_upsert_statement(&entity);
        self.with_writer("upsert_entity", move |conn| {
            let mut stmt = conn.prepare(&statement.sql)?;
            bind_params(&mut stmt, &statement.params)?;
            stmt.raw_execute()?;
            Ok(())
        })
        .await
    }

    async fn upsert_entities(
        &self,
        entities: Vec<Entity>,
    ) -> Result<BatchWriteSummary, StorageError> {
        let attempted = entities.len() as u64;

        // ADR-067 slice 1: when the write queue is enabled, route through
        // the WriterTask channel. The closure is DML-only — no BEGIN
        // IMMEDIATE/COMMIT/ROLLBACK here, since the WriterTask's run loop
        // owns the transaction and `WriteRequest::execute_and_reply` owns
        // the commit/rollback decision (a bare BEGIN IMMEDIATE inside this
        // closure would violate SQLite's nested-transaction rule).
        if let Some(writer_task) = &self.writer_task {
            return writer_task
                .send(move |conn| {
                    batch_upsert_entities(conn, &entities, attempted)
                        .map_err(|e| map_err(e, "upsert_entities"))
                })
                .await;
        }

        // Flag-off (default) path: byte-for-byte unchanged from pre-ADR-067
        // behavior — the closure owns its own BEGIN IMMEDIATE/COMMIT/ROLLBACK
        // via the pool-mutex writer.
        self.with_writer("upsert_entities", move |conn| {
            conn.execute_batch("BEGIN IMMEDIATE")?;
            let _tx_handle =
                khive_storage::tx_registry::register(Some("entity_upsert_batch".to_string()));

            let summary = batch_upsert_entities(conn, &entities, attempted)?;

            if let Err(e) = conn.execute_batch("COMMIT") {
                let _ = conn.execute_batch("ROLLBACK");
                return Err(e);
            }
            Ok(summary)
        })
        .await
    }

    async fn get_entity(&self, id: Uuid) -> Result<Option<Entity>, StorageError> {
        let id_str = id.to_string();

        self.with_reader("get_entity", move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, namespace, kind, entity_type, name, description, properties, tags, \
                 created_at, updated_at, deleted_at, merged_into, merge_event_id \
                 FROM entities WHERE id = ?1 AND deleted_at IS NULL",
            )?;
            let mut rows = stmt.query(rusqlite::params![id_str])?;
            match rows.next()? {
                Some(row) => Ok(Some(read_entity(row)?)),
                None => Ok(None),
            }
        })
        .await
    }

    async fn delete_entity(&self, id: Uuid, mode: DeleteMode) -> Result<bool, StorageError> {
        match mode {
            DeleteMode::Soft => {
                let now = chrono::Utc::now().timestamp_micros();
                let statement = entity_soft_delete_statement(id, now);
                self.with_writer("delete_entity_soft", move |conn| {
                    let mut stmt = conn.prepare(&statement.sql)?;
                    bind_params(&mut stmt, &statement.params)?;
                    Ok(stmt.raw_execute()? > 0)
                })
                .await
            }
            DeleteMode::Hard => {
                let statement = entity_hard_delete_statement(id);
                self.with_writer("delete_entity_hard", move |conn| {
                    let mut stmt = conn.prepare(&statement.sql)?;
                    bind_params(&mut stmt, &statement.params)?;
                    Ok(stmt.raw_execute()? > 0)
                })
                .await
            }
        }
    }

    async fn query_entities(
        &self,
        namespace: &str,
        filter: EntityFilter,
        page: PageRequest,
    ) -> Result<Page<Entity>, StorageError> {
        let namespace = namespace.to_string();
        let limit_i64 = i64::from(page.limit);
        let offset_i64 = i64::try_from(page.offset).map_err(|_| StorageError::InvalidInput {
            capability: StorageCapability::Entities,
            operation: "query_entities".into(),
            message: format!(
                "PageRequest: offset must be <= i64::MAX, got {}",
                page.offset
            ),
        })?;

        self.with_reader("query_entities", move |conn| {
            let total = if filter.names_ci.is_empty() {
                let (count_sql, count_params) = build_entity_where(&namespace, &filter);
                let sql = format!("SELECT COUNT(*) FROM entities{count_sql}");
                let mut stmt = conn.prepare(&sql)?;
                let param_refs: Vec<&dyn rusqlite::types::ToSql> =
                    count_params.iter().map(|p| p.as_ref()).collect();
                Some(stmt.query_row(param_refs.as_slice(), |row| row.get::<_, i64>(0))? as u64)
            } else {
                None
            };

            let (where_sql, mut data_params) = build_entity_where(&namespace, &filter);

            // #818: when a name_prefix filter is active, an exact
            // case-insensitive match must never be pushed out of the page by
            // pattern candidates that merely share the prefix. Rank exact
            // matches first (deterministic tiebreak via created_at) so page
            // truncation can never hide the record a caller resolved by name.
            let order_by = if let Some(ref prefix) = filter.name_prefix {
                data_params.push(Box::new(prefix.to_ascii_lowercase()));
                format!(
                    "CASE WHEN LOWER(name) = ?{} THEN 0 ELSE 1 END, created_at DESC",
                    data_params.len()
                )
            } else {
                "created_at DESC".to_string()
            };

            data_params.push(Box::new(limit_i64));
            data_params.push(Box::new(offset_i64));

            let limit_idx = data_params.len() - 1;
            let offset_idx = data_params.len();

            let data_sql = format!(
                "SELECT id, namespace, kind, entity_type, name, description, properties, tags, \
                 created_at, updated_at, deleted_at, merged_into, merge_event_id \
                 FROM entities{} ORDER BY {} LIMIT ?{} OFFSET ?{}",
                where_sql, order_by, limit_idx, offset_idx,
            );

            let mut stmt = conn.prepare(&data_sql)?;
            let param_refs: Vec<&dyn rusqlite::types::ToSql> =
                data_params.iter().map(|p| p.as_ref()).collect();
            let rows = stmt.query_map(param_refs.as_slice(), read_entity)?;

            let mut items = Vec::new();
            for row in rows {
                items.push(row?);
            }

            Ok(Page { items, total })
        })
        .await
    }

    async fn get_entity_including_deleted(&self, id: Uuid) -> Result<Option<Entity>, StorageError> {
        let id_str = id.to_string();

        self.with_reader("get_entity_including_deleted", move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, namespace, kind, entity_type, name, description, properties, tags, \
                 created_at, updated_at, deleted_at, merged_into, merge_event_id \
                 FROM entities WHERE id = ?1",
            )?;
            let mut rows = stmt.query(rusqlite::params![id_str])?;
            match rows.next()? {
                Some(row) => Ok(Some(read_entity(row)?)),
                None => Ok(None),
            }
        })
        .await
    }

    async fn count_entities(
        &self,
        namespace: &str,
        filter: EntityFilter,
    ) -> Result<u64, StorageError> {
        let namespace = namespace.to_string();

        self.with_reader("count_entities", move |conn| {
            let (where_sql, params) = build_entity_where(&namespace, &filter);
            let sql = format!("SELECT COUNT(*) FROM entities{}", where_sql);
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

const ENTITIES_DDL: &str = include_str!("../../sql/entities-ddl.sql");

pub(crate) fn ensure_entities_schema(conn: &rusqlite::Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(ENTITIES_DDL)
}

#[cfg(test)]
#[path = "entity_tests.rs"]
mod tests;
