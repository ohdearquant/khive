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
    let name: String = row.get(3)?;
    let description: Option<String> = row.get(4)?;
    let properties_str: Option<String> = row.get(5)?;
    let tags_str: String = row.get(6)?;
    let created_at: i64 = row.get(7)?;
    let updated_at: i64 = row.get(8)?;
    let deleted_at: Option<i64> = row.get(9)?;

    let id = parse_uuid(&id_str)?;

    let properties = properties_str
        .map(|s| {
            serde_json::from_str(&s).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    5,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })
        })
        .transpose()?;

    let tags: Vec<String> = serde_json::from_str(&tags_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(6, rusqlite::types::Type::Text, Box::new(e))
    })?;

    Ok(Entity {
        id,
        namespace,
        kind,
        name,
        description,
        properties,
        tags,
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

    if let Some(ref prefix) = filter.name_prefix {
        params.push(Box::new(format!("{}%", prefix)));
        conditions.push(format!("name LIKE ?{}", params.len()));
    }

    if !filter.tags_any.is_empty() {
        let placeholders: Vec<String> = filter
            .tags_any
            .iter()
            .map(|t| {
                params.push(Box::new(t.clone()));
                format!("?{}", params.len())
            })
            .collect();
        conditions.push(format!(
            "EXISTS (SELECT 1 FROM json_each(tags) WHERE json_each.value IN ({}))",
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

        self.with_writer("upsert_entity", move |conn| {
            conn.execute(
                "INSERT OR REPLACE INTO entities \
                 (id, namespace, kind, name, description, properties, tags, \
                  created_at, updated_at, deleted_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                rusqlite::params![
                    id_str,
                    namespace,
                    entity.kind,
                    entity.name,
                    entity.description,
                    properties_str,
                    tags_str,
                    entity.created_at,
                    entity.updated_at,
                    entity.deleted_at,
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

                match conn.execute(
                    "INSERT OR REPLACE INTO entities \
                     (id, namespace, kind, name, description, properties, tags, \
                      created_at, updated_at, deleted_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                    rusqlite::params![
                        id_str,
                        &entity.namespace,
                        entity.kind,
                        entity.name,
                        entity.description,
                        properties_str,
                        tags_str,
                        entity.created_at,
                        entity.updated_at,
                        entity.deleted_at,
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
                "SELECT id, namespace, kind, name, description, properties, tags, \
                 created_at, updated_at, deleted_at \
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
                "SELECT id, namespace, kind, name, description, properties, tags, \
                 created_at, updated_at, deleted_at \
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
        name TEXT NOT NULL,\
        description TEXT,\
        properties TEXT,\
        tags TEXT NOT NULL DEFAULT '[]',\
        created_at INTEGER NOT NULL,\
        updated_at INTEGER NOT NULL,\
        deleted_at INTEGER\
    );\
    CREATE INDEX IF NOT EXISTS idx_entities_namespace ON entities(namespace);\
    CREATE INDEX IF NOT EXISTS idx_entities_kind ON entities(namespace, kind);\
    CREATE INDEX IF NOT EXISTS idx_entities_name ON entities(namespace, name);\
    CREATE INDEX IF NOT EXISTS idx_entities_created ON entities(created_at DESC);\
";

pub(crate) fn ensure_entities_schema(conn: &rusqlite::Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(ENTITIES_DDL)
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
            writer.conn().execute_batch(ENTITIES_DDL).unwrap();
        }
        pool
    }

    fn setup_memory_store() -> SqlEntityStore {
        SqlEntityStore::new(setup_pool(), false)
    }

    fn setup_memory_store_ns(_ns: &str) -> SqlEntityStore {
        SqlEntityStore::new(setup_pool(), false)
    }

    fn make_entity(namespace: &str, kind: &str, name: &str) -> Entity {
        let now = chrono::Utc::now().timestamp_micros();
        Entity {
            id: Uuid::new_v4(),
            namespace: namespace.to_string(),
            kind: kind.to_string(),
            name: name.to_string(),
            description: None,
            properties: None,
            tags: Vec::new(),
            created_at: now,
            updated_at: now,
            deleted_at: None,
        }
    }

    #[tokio::test]
    async fn test_upsert_and_get_entity() {
        let store = setup_memory_store();

        let entity = make_entity("default", "concept", "LoRA");
        let id = entity.id;

        store.upsert_entity(entity).await.unwrap();

        let fetched = store.get_entity(id).await.unwrap();
        assert!(fetched.is_some());
        let fetched = fetched.unwrap();
        assert_eq!(fetched.id, id);
        assert_eq!(fetched.name, "LoRA");
        assert_eq!(fetched.kind, "concept");
    }

    #[tokio::test]
    async fn test_upsert_with_builder() {
        let store = setup_memory_store();

        let props = serde_json::json!({"domain": "fine-tuning", "type": "technique"});
        let entity = Entity::new("default", "concept", "QLoRA")
            .with_description("Quantized LoRA")
            .with_properties(props.clone())
            .with_tags(vec!["fine-tuning".to_string(), "quantization".to_string()]);
        let id = entity.id;

        store.upsert_entity(entity).await.unwrap();

        let fetched = store.get_entity(id).await.unwrap().unwrap();
        assert_eq!(fetched.description.as_deref(), Some("Quantized LoRA"));
        assert_eq!(fetched.properties, Some(props));
        assert_eq!(fetched.tags, vec!["fine-tuning", "quantization"]);
    }

    #[tokio::test]
    async fn test_soft_delete() {
        let store = setup_memory_store();

        let entity = make_entity("default", "concept", "to-delete");
        let id = entity.id;
        store.upsert_entity(entity).await.unwrap();

        let deleted = store.delete_entity(id, DeleteMode::Soft).await.unwrap();
        assert!(deleted);

        let fetched = store.get_entity(id).await.unwrap();
        assert!(fetched.is_none());
    }

    #[tokio::test]
    async fn test_hard_delete() {
        let store = setup_memory_store();

        let entity = make_entity("default", "concept", "to-hard-delete");
        let id = entity.id;
        store.upsert_entity(entity).await.unwrap();

        let deleted = store.delete_entity(id, DeleteMode::Hard).await.unwrap();
        assert!(deleted);

        let fetched = store.get_entity(id).await.unwrap();
        assert!(fetched.is_none());
    }

    #[tokio::test]
    async fn test_query_entities_basic() {
        let store = setup_memory_store_ns("ns1");

        for name in &["Alpha", "Beta", "Gamma"] {
            store
                .upsert_entity(make_entity("ns1", "concept", name))
                .await
                .unwrap();
        }
        store
            .upsert_entity(make_entity("ns1", "document", "Paper1"))
            .await
            .unwrap();

        let page = store
            .query_entities(
                "ns1",
                EntityFilter::default(),
                PageRequest {
                    offset: 0,
                    limit: 10,
                },
            )
            .await
            .unwrap();
        assert_eq!(page.items.len(), 4);
        assert_eq!(page.total, Some(4));

        // Filter by kind
        let concepts = store
            .query_entities(
                "ns1",
                EntityFilter {
                    kinds: vec!["concept".to_string()],
                    ..Default::default()
                },
                PageRequest::default(),
            )
            .await
            .unwrap();
        assert_eq!(concepts.items.len(), 3);
    }

    #[tokio::test]
    async fn test_query_by_name_prefix() {
        let store = setup_memory_store_ns("ns1");

        // "Alpha" and "AlphaGo" both start with "Alpha"; "Beta" does not
        for &name in &["Alpha", "AlphaGo", "Beta"] {
            store
                .upsert_entity(make_entity("ns1", "concept", name))
                .await
                .unwrap();
        }

        let result = store
            .query_entities(
                "ns1",
                EntityFilter {
                    name_prefix: Some("Alpha".to_string()),
                    ..Default::default()
                },
                PageRequest::default(),
            )
            .await
            .unwrap();
        assert_eq!(result.items.len(), 2);
        let names: Vec<&str> = result.items.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"Alpha"), "Alpha not found in {names:?}");
        assert!(names.contains(&"AlphaGo"), "AlphaGo not found in {names:?}");
        assert!(!names.contains(&"Beta"));
    }

    #[tokio::test]
    async fn test_count_entities() {
        let store = setup_memory_store_ns("ns1");

        for _ in 0..5 {
            store
                .upsert_entity(make_entity("ns1", "concept", "X"))
                .await
                .unwrap();
        }

        let count = store
            .count_entities("ns1", EntityFilter::default())
            .await
            .unwrap();
        assert_eq!(count, 5);

        // Namespace is the caller's responsibility — querying "ns2" returns 0
        // because no entities were inserted in that namespace.
        let count_other = store
            .count_entities("ns2", EntityFilter::default())
            .await
            .unwrap();
        assert_eq!(count_other, 0);
    }

    #[tokio::test]
    async fn test_batch_upsert() {
        let store = setup_memory_store_ns("batch_ns");

        let entities: Vec<Entity> = (0..10)
            .map(|i| make_entity("batch_ns", "concept", &format!("entity_{i}")))
            .collect();

        let summary = store.upsert_entities(entities).await.unwrap();
        assert_eq!(summary.attempted, 10);
        assert_eq!(summary.affected, 10);
        assert_eq!(summary.failed, 0);

        let count = store
            .count_entities("batch_ns", EntityFilter::default())
            .await
            .unwrap();
        assert_eq!(count, 10);
    }

    /// One store, two namespaces — each query sees only its own.
    #[tokio::test]
    async fn test_namespace_isolation() {
        let pool = setup_pool();
        let store = SqlEntityStore::new(Arc::clone(&pool), false);

        store
            .upsert_entity(make_entity("ns_a", "concept", "EntityA"))
            .await
            .unwrap();
        store
            .upsert_entity(make_entity("ns_b", "concept", "EntityB"))
            .await
            .unwrap();

        // Namespace is the caller's responsibility — pass it in the query.
        let count_a = store
            .count_entities("ns_a", EntityFilter::default())
            .await
            .unwrap();
        let count_b = store
            .count_entities("ns_b", EntityFilter::default())
            .await
            .unwrap();

        assert_eq!(count_a, 1);
        assert_eq!(count_b, 1);

        let page_a = store
            .query_entities("ns_a", EntityFilter::default(), PageRequest::default())
            .await
            .unwrap();
        assert_eq!(page_a.items[0].name, "EntityA");

        let page_b = store
            .query_entities("ns_b", EntityFilter::default(), PageRequest::default())
            .await
            .unwrap();
        assert_eq!(page_b.items[0].name, "EntityB");
    }

    #[tokio::test]
    async fn test_query_by_tags() {
        let store = setup_memory_store_ns("tags_ns");

        let mut e1 = make_entity("tags_ns", "concept", "Tagged1");
        e1.tags = vec!["rust".to_string(), "systems".to_string()];
        let mut e2 = make_entity("tags_ns", "concept", "Tagged2");
        e2.tags = vec!["python".to_string(), "ml".to_string()];
        let mut e3 = make_entity("tags_ns", "concept", "Tagged3");
        e3.tags = vec!["rust".to_string(), "ml".to_string()];

        store.upsert_entity(e1).await.unwrap();
        store.upsert_entity(e2).await.unwrap();
        store.upsert_entity(e3).await.unwrap();

        // Filter by "rust" tag — should match Tagged1 and Tagged3
        let result = store
            .query_entities(
                "tags_ns",
                EntityFilter {
                    tags_any: vec!["rust".to_string()],
                    ..Default::default()
                },
                PageRequest::default(),
            )
            .await
            .unwrap();
        assert_eq!(result.items.len(), 2);
        let names: Vec<&str> = result.items.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"Tagged1"));
        assert!(names.contains(&"Tagged3"));
        assert!(!names.contains(&"Tagged2"));

        // Filter by "ml" tag — should match Tagged2 and Tagged3
        let result = store
            .query_entities(
                "tags_ns",
                EntityFilter {
                    tags_any: vec!["ml".to_string()],
                    ..Default::default()
                },
                PageRequest::default(),
            )
            .await
            .unwrap();
        assert_eq!(result.items.len(), 2);

        // Filter by both "rust" and "python" (union) — should match all three
        let result = store
            .query_entities(
                "tags_ns",
                EntityFilter {
                    tags_any: vec!["rust".to_string(), "python".to_string()],
                    ..Default::default()
                },
                PageRequest::default(),
            )
            .await
            .unwrap();
        assert_eq!(result.items.len(), 3);
    }

    #[tokio::test]
    async fn test_query_by_ids() {
        let store = setup_memory_store_ns("ns1");

        let e1 = make_entity("ns1", "concept", "E1");
        let e2 = make_entity("ns1", "concept", "E2");
        let e3 = make_entity("ns1", "concept", "E3");
        let ids = vec![e1.id, e3.id];

        store.upsert_entity(e1).await.unwrap();
        store.upsert_entity(e2).await.unwrap();
        store.upsert_entity(e3).await.unwrap();

        let result = store
            .query_entities(
                "ns1",
                EntityFilter {
                    ids,
                    ..Default::default()
                },
                PageRequest::default(),
            )
            .await
            .unwrap();
        assert_eq!(result.items.len(), 2);
        let names: Vec<&str> = result.items.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"E1"));
        assert!(names.contains(&"E3"));
        assert!(!names.contains(&"E2"));
    }

    /// UUID is globally unique (id TEXT PRIMARY KEY). Upserting the same UUID in a
    /// different namespace overwrites the row (INSERT OR REPLACE). get_entity by ID
    /// returns whichever namespace currently owns that UUID.
    #[tokio::test]
    async fn test_same_id_upsert_replaces_row() {
        let pool = setup_pool();
        let store = SqlEntityStore::new(Arc::clone(&pool), false);

        let shared_id = Uuid::new_v4();
        let now = chrono::Utc::now().timestamp_micros();

        let entity_a = Entity {
            id: shared_id,
            namespace: "ns_a".to_string(),
            kind: "concept".to_string(),
            name: "SharedInA".to_string(),
            description: None,
            properties: None,
            tags: Vec::new(),
            created_at: now,
            updated_at: now,
            deleted_at: None,
        };
        store.upsert_entity(entity_a).await.unwrap();

        // At this point the row is in ns_a.
        let fetched = store.get_entity(shared_id).await.unwrap().unwrap();
        assert_eq!(fetched.namespace, "ns_a");
        assert_eq!(fetched.name, "SharedInA");

        // Upsert same UUID into ns_b — INSERT OR REPLACE replaces the row.
        let entity_b = Entity {
            id: shared_id,
            namespace: "ns_b".to_string(),
            kind: "concept".to_string(),
            name: "SharedInB".to_string(),
            description: None,
            properties: None,
            tags: Vec::new(),
            created_at: now,
            updated_at: now,
            deleted_at: None,
        };
        store.upsert_entity(entity_b).await.unwrap();

        // Now the row is in ns_b — get_entity returns ns_b regardless of which namespace
        // you query from (namespace is caller's responsibility).
        let fetched = store.get_entity(shared_id).await.unwrap().unwrap();
        assert_eq!(fetched.namespace, "ns_b");
        assert_eq!(fetched.name, "SharedInB");

        // ns_a now has 0 entities; ns_b has 1.
        let count_a = store
            .count_entities("ns_a", EntityFilter::default())
            .await
            .unwrap();
        let count_b = store
            .count_entities("ns_b", EntityFilter::default())
            .await
            .unwrap();
        assert_eq!(count_a, 0);
        assert_eq!(count_b, 1);
    }
}
