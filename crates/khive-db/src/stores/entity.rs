//! SQL-backed `EntityStore` implementation.

use std::sync::Arc;

use async_trait::async_trait;
use uuid::Uuid;

use khive_storage::entity::{Entity, EntityFilter};
use khive_storage::error::StorageError;
use khive_storage::types::{BatchWriteSummary, DeleteMode, Page, PageRequest};
use khive_storage::EntityStore;
use khive_storage::StorageCapability;

use crate::error::SqliteError;
use crate::pool::ConnectionPool;

fn map_err(e: rusqlite::Error, op: &'static str) -> StorageError {
    StorageError::driver(StorageCapability::Entities, op, e)
}

fn map_sqlite_err(e: SqliteError, op: &'static str) -> StorageError {
    StorageError::driver(StorageCapability::Entities, op, e)
}

/// An EntityStore backed by SQLite. Namespace is the caller's responsibility.
///
/// UUID is globally unique — get/delete by ID alone. Query/count use the
/// namespace parameter as passed. The store is just a pool + is_file_backed.
pub struct SqlEntityStore {
    pool: Arc<ConnectionPool>,
    is_file_backed: bool,
}

impl SqlEntityStore {
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

fn parse_uuid(s: &str) -> Result<Uuid, rusqlite::Error> {
    Uuid::parse_str(s).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })
}

fn build_entity_where(
    namespace: &str,
    filter: &EntityFilter,
) -> (String, Vec<Box<dyn rusqlite::types::ToSql>>) {
    let mut conditions: Vec<String> = vec![
        "namespace = ?1".to_string(),
        "deleted_at IS NULL".to_string(),
    ];
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(namespace.to_string())];

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
        params.push(Box::new(format!("{}%", prefix)));
        conditions.push(format!("name LIKE ?{}", params.len()));
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
        let namespace = entity.namespace.clone();
        let id_str = entity.id.to_string();
        let properties_str = entity
            .properties
            .as_ref()
            .map(|v| serde_json::to_string(v).unwrap_or_default());
        let tags_str = serde_json::to_string(&entity.tags).unwrap_or_else(|_| "[]".to_string());

        let merged_into_str = entity.merged_into.map(|u| u.to_string());
        let merge_event_id_str = entity.merge_event_id.map(|u| u.to_string());

        self.with_writer("upsert_entity", move |conn| {
            conn.execute(
                "INSERT OR REPLACE INTO entities \
                 (id, namespace, kind, entity_type, name, description, properties, tags, \
                  created_at, updated_at, deleted_at, merged_into, merge_event_id) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                rusqlite::params![
                    id_str,
                    namespace,
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
            )?;
            Ok(())
        })
        .await
    }

    async fn upsert_entities(
        &self,
        entities: Vec<Entity>,
    ) -> Result<BatchWriteSummary, StorageError> {
        let attempted = entities.len() as u64;

        self.with_writer("upsert_entities", move |conn| {
            conn.execute_batch("BEGIN IMMEDIATE")?;
            let mut affected = 0u64;
            let mut failed = 0u64;
            let mut first_error = String::new();

            for entity in &entities {
                let id_str = entity.id.to_string();
                let properties_str = entity
                    .properties
                    .as_ref()
                    .map(|v| serde_json::to_string(v).unwrap_or_default());
                let tags_str =
                    serde_json::to_string(&entity.tags).unwrap_or_else(|_| "[]".to_string());

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
        let id_str = id.to_string();

        match mode {
            DeleteMode::Soft => {
                self.with_writer("delete_entity_soft", move |conn| {
                    let now = chrono::Utc::now().timestamp_micros();
                    let deleted = conn.execute(
                        "UPDATE entities SET deleted_at = ?1 \
                         WHERE id = ?2 AND deleted_at IS NULL",
                        rusqlite::params![now, id_str],
                    )?;
                    Ok(deleted > 0)
                })
                .await
            }
            DeleteMode::Hard => {
                self.with_writer("delete_entity_hard", move |conn| {
                    let deleted = conn.execute(
                        "DELETE FROM entities WHERE id = ?1",
                        rusqlite::params![id_str],
                    )?;
                    Ok(deleted > 0)
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

        self.with_reader("query_entities", move |conn| {
            let (count_sql, count_params) = build_entity_where(&namespace, &filter);
            let total: i64 = {
                let sql = format!("SELECT COUNT(*) FROM entities{}", count_sql);
                let mut stmt = conn.prepare(&sql)?;
                let param_refs: Vec<&dyn rusqlite::types::ToSql> =
                    count_params.iter().map(|p| p.as_ref()).collect();
                stmt.query_row(param_refs.as_slice(), |row| row.get(0))?
            };

            let (where_sql, mut data_params) = build_entity_where(&namespace, &filter);
            data_params.push(Box::new(page.limit as i64));
            data_params.push(Box::new(page.offset as i64));

            let limit_idx = data_params.len() - 1;
            let offset_idx = data_params.len();

            let data_sql = format!(
                "SELECT id, namespace, kind, entity_type, name, description, properties, tags, \
                 created_at, updated_at, deleted_at, merged_into, merge_event_id \
                 FROM entities{} ORDER BY created_at DESC LIMIT ?{} OFFSET ?{}",
                where_sql, limit_idx, offset_idx,
            );

            let mut stmt = conn.prepare(&data_sql)?;
            let param_refs: Vec<&dyn rusqlite::types::ToSql> =
                data_params.iter().map(|p| p.as_ref()).collect();
            let rows = stmt.query_map(param_refs.as_slice(), read_entity)?;

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

const ENTITIES_DDL: &str = "\
    CREATE TABLE IF NOT EXISTS entities (\
        id TEXT PRIMARY KEY,\
        namespace TEXT NOT NULL,\
        kind TEXT NOT NULL,\
        entity_type TEXT,\
        name TEXT NOT NULL,\
        description TEXT,\
        properties TEXT,\
        tags TEXT NOT NULL DEFAULT '[]',\
        created_at INTEGER NOT NULL,\
        updated_at INTEGER NOT NULL,\
        deleted_at INTEGER,\
        merged_into TEXT,\
        merge_event_id TEXT\
    );\
    CREATE INDEX IF NOT EXISTS idx_entities_namespace ON entities(namespace);\
    CREATE INDEX IF NOT EXISTS idx_entities_kind ON entities(namespace, kind);\
    CREATE INDEX IF NOT EXISTS idx_entities_kind_entity_type ON entities(namespace, kind, entity_type);\
    CREATE INDEX IF NOT EXISTS idx_entities_name ON entities(namespace, name);\
    CREATE INDEX IF NOT EXISTS idx_entities_created ON entities(created_at DESC);\
    CREATE INDEX IF NOT EXISTS idx_entities_merged_into ON entities(namespace, merged_into);\
";

pub(crate) fn ensure_entities_schema(conn: &rusqlite::Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(ENTITIES_DDL)
}

#[cfg(test)]
#[path = "entity_tests.rs"]
mod tests;
