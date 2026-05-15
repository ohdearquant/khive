//! SQL-backed `EventStore` implementation.

use std::sync::Arc;

use async_trait::async_trait;
use uuid::Uuid;

use khive_storage::error::StorageError;
use khive_storage::event::{Event, EventFilter};
use khive_storage::types::{BatchWriteSummary, Page, PageRequest};
use khive_storage::EventStore;
use khive_storage::StorageCapability;
use khive_types::{EventOutcome, SubstrateKind};

use crate::error::SqliteError;
use crate::pool::ConnectionPool;

fn map_err(e: rusqlite::Error, op: &'static str) -> StorageError {
    StorageError::driver(StorageCapability::Event, op, e)
}

fn map_sqlite_err(e: SqliteError, op: &'static str) -> StorageError {
    StorageError::driver(StorageCapability::Event, op, e)
}

/// An EventStore backed by SQLite tables.
pub struct SqlEventStore {
    pool: Arc<ConnectionPool>,
    is_file_backed: bool,
    namespace: String,
}

impl SqlEventStore {
    /// Create a new store scoped to one namespace.
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

    fn open_standalone_writer(&self) -> Result<rusqlite::Connection, StorageError> {
        let config = self.pool.config();
        let path = config.path.as_ref().ok_or_else(|| StorageError::Pool {
            operation: "event_writer".into(),
            message: "in-memory databases do not support standalone connections".into(),
        })?;

        let conn = rusqlite::Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
                | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX
                | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )
        .map_err(|e| map_err(e, "open_event_writer"))?;

        conn.busy_timeout(config.busy_timeout)
            .map_err(|e| map_err(e, "open_event_writer"))?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(|e| map_err(e, "open_event_writer"))?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(|e| map_err(e, "open_event_writer"))?;

        Ok(conn)
    }

    fn open_standalone_reader(&self) -> Result<rusqlite::Connection, StorageError> {
        let config = self.pool.config();
        let path = config.path.as_ref().ok_or_else(|| StorageError::Pool {
            operation: "event_reader".into(),
            message: "in-memory databases do not support standalone connections".into(),
        })?;

        let conn = rusqlite::Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX
                | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )
        .map_err(|e| map_err(e, "open_event_reader"))?;

        conn.busy_timeout(config.busy_timeout)
            .map_err(|e| map_err(e, "open_event_reader"))?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(|e| map_err(e, "open_event_reader"))?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(|e| map_err(e, "open_event_reader"))?;

        Ok(conn)
    }

    async fn with_writer<F, R>(&self, op: &'static str, f: F) -> Result<R, StorageError>
    where
        F: FnOnce(&rusqlite::Connection) -> Result<R, rusqlite::Error> + Send + 'static,
        R: Send + 'static,
    {
        if self.is_file_backed {
            let conn = self.open_standalone_writer()?;
            tokio::task::spawn_blocking(move || f(&conn).map_err(|e| map_err(e, op)))
                .await
                .map_err(|e| StorageError::driver(StorageCapability::Event, op, e))?
        } else {
            let pool = Arc::clone(&self.pool);
            tokio::task::spawn_blocking(move || {
                let guard = pool.try_writer().map_err(|e| map_sqlite_err(e, op))?;
                f(guard.conn()).map_err(|e| map_err(e, op))
            })
            .await
            .map_err(|e| StorageError::driver(StorageCapability::Event, op, e))?
        }
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
                .map_err(|e| StorageError::driver(StorageCapability::Event, op, e))?
        } else {
            let pool = Arc::clone(&self.pool);
            tokio::task::spawn_blocking(move || {
                let guard = pool.reader().map_err(|e| map_sqlite_err(e, op))?;
                f(guard.conn()).map_err(|e| map_err(e, op))
            })
            .await
            .map_err(|e| StorageError::driver(StorageCapability::Event, op, e))?
        }
    }
}

// =============================================================================
// Helpers: parse SubstrateKind / EventOutcome from DB strings
// =============================================================================

fn substrate_from_str(s: &str) -> Result<SubstrateKind, rusqlite::Error> {
    s.parse::<SubstrateKind>().map_err(|_| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            format!("unknown SubstrateKind: {s}").into(),
        )
    })
}

fn outcome_from_str(s: &str) -> Result<EventOutcome, rusqlite::Error> {
    match s {
        "success" => Ok(EventOutcome::Success),
        "denied" => Ok(EventOutcome::Denied),
        "error" => Ok(EventOutcome::Error),
        other => Err(rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            format!("unknown EventOutcome: {other}").into(),
        )),
    }
}

fn parse_uuid(s: &str) -> Result<Uuid, rusqlite::Error> {
    Uuid::parse_str(s).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })
}

// Column order: id(0), namespace(1), verb(2), substrate(3), actor(4),
//               outcome(5), data(6), duration_us(7), target_id(8), created_at(9)
fn read_event(row: &rusqlite::Row<'_>) -> Result<Event, rusqlite::Error> {
    let id_str: String = row.get(0)?;
    let namespace: String = row.get(1)?;
    let verb: String = row.get(2)?;
    let substrate_str: String = row.get(3)?;
    let actor: String = row.get(4)?;
    let outcome_str: String = row.get(5)?;
    let data_str: Option<String> = row.get(6)?;
    let duration_us: i64 = row.get(7)?;
    let target_str: Option<String> = row.get(8)?;
    let created_at: i64 = row.get(9)?;

    let id = parse_uuid(&id_str)?;
    let substrate = substrate_from_str(&substrate_str)?;
    let outcome = outcome_from_str(&outcome_str)?;
    let data = data_str
        .as_deref()
        .map(serde_json::from_str)
        .transpose()
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(6, rusqlite::types::Type::Text, Box::new(e))
        })?;
    let target_id = target_str.as_deref().map(parse_uuid).transpose()?;

    Ok(Event {
        id,
        namespace,
        verb,
        substrate,
        actor,
        outcome,
        data,
        duration_us,
        target_id,
        created_at,
    })
}

fn build_event_filter_sql(
    default_namespace: &str,
    filter: &EventFilter,
) -> (String, Vec<Box<dyn rusqlite::types::ToSql>>) {
    let mut conditions: Vec<String> = Vec::new();
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

    if filter.namespaces.is_empty() {
        params.push(Box::new(default_namespace.to_string()));
        conditions.push(format!("namespace = ?{}", params.len()));
    } else {
        let placeholders: Vec<String> = filter
            .namespaces
            .iter()
            .map(|ns| {
                params.push(Box::new(ns.clone()));
                format!("?{}", params.len())
            })
            .collect();
        conditions.push(format!("namespace IN ({})", placeholders.join(",")));
    }

    if !filter.ids.is_empty() {
        let placeholders: Vec<String> = filter
            .ids
            .iter()
            .map(|id| {
                params.push(Box::new(id.to_string()));
                format!("?{}", params.len())
            })
            .collect();
        conditions.push(format!("id IN ({})", placeholders.join(",")));
    }

    if !filter.verbs.is_empty() {
        let placeholders: Vec<String> = filter
            .verbs
            .iter()
            .map(|v| {
                params.push(Box::new(v.clone()));
                format!("?{}", params.len())
            })
            .collect();
        conditions.push(format!("verb IN ({})", placeholders.join(",")));
    }

    if !filter.substrates.is_empty() {
        let placeholders: Vec<String> = filter
            .substrates
            .iter()
            .map(|s| {
                params.push(Box::new(s.name().to_string()));
                format!("?{}", params.len())
            })
            .collect();
        conditions.push(format!("substrate IN ({})", placeholders.join(",")));
    }

    if !filter.actors.is_empty() {
        let placeholders: Vec<String> = filter
            .actors
            .iter()
            .map(|a| {
                params.push(Box::new(a.clone()));
                format!("?{}", params.len())
            })
            .collect();
        conditions.push(format!("actor IN ({})", placeholders.join(",")));
    }

    if let Some(after) = filter.after {
        params.push(Box::new(after));
        conditions.push(format!("created_at > ?{}", params.len()));
    }

    if let Some(before) = filter.before {
        params.push(Box::new(before));
        conditions.push(format!("created_at < ?{}", params.len()));
    }

    let clause = format!(" WHERE {}", conditions.join(" AND "));
    (clause, params)
}

// =============================================================================
// EventStore implementation
// =============================================================================

#[async_trait]
impl EventStore for SqlEventStore {
    async fn append_event(&self, event: Event) -> Result<(), StorageError> {
        let id_str = event.id.to_string();
        let substrate_str = event.substrate.name().to_string();
        let outcome_str = event.outcome.name().to_string();
        let data_str = event.data.as_ref().map(|v| v.to_string());
        let target_str = event.target_id.map(|u| u.to_string());
        let ns = event.namespace.clone();
        let verb = event.verb.clone();
        let actor = event.actor.clone();
        let duration_us = event.duration_us;
        let created_at = event.created_at;

        self.with_writer("append_event", move |conn| {
            conn.execute(
                "INSERT INTO events \
                 (id, namespace, verb, substrate, actor, outcome, data, duration_us, target_id, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                rusqlite::params![
                    id_str,
                    ns,
                    verb,
                    substrate_str,
                    actor,
                    outcome_str,
                    data_str,
                    duration_us,
                    target_str,
                    created_at,
                ],
            )?;
            Ok(())
        })
        .await
    }

    async fn append_events(&self, events: Vec<Event>) -> Result<BatchWriteSummary, StorageError> {
        let attempted = events.len() as u64;

        self.with_writer("append_events", move |conn| {
            conn.execute_batch("BEGIN IMMEDIATE")?;
            let mut affected = 0u64;
            let mut failed = 0u64;
            let mut first_error = String::new();

            for event in &events {
                let id_str = event.id.to_string();
                let substrate_str = event.substrate.name().to_string();
                let outcome_str = event.outcome.name().to_string();
                let data_str = event.data.as_ref().map(|v| v.to_string());
                let target_str = event.target_id.map(|u| u.to_string());

                match conn.execute(
                    "INSERT INTO events \
                     (id, namespace, verb, substrate, actor, outcome, data, duration_us, target_id, created_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                    rusqlite::params![
                        id_str,
                        &event.namespace,
                        &event.verb,
                        substrate_str,
                        &event.actor,
                        outcome_str,
                        data_str,
                        event.duration_us,
                        target_str,
                        event.created_at,
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

    async fn get_event(&self, id: Uuid) -> Result<Option<Event>, StorageError> {
        let namespace = self.namespace.clone();
        let id_str = id.to_string();

        self.with_reader("get_event", move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, namespace, verb, substrate, actor, outcome, data, duration_us, target_id, created_at \
                 FROM events WHERE namespace = ?1 AND id = ?2",
            )?;
            let mut rows = stmt.query(rusqlite::params![namespace, id_str])?;
            match rows.next()? {
                Some(row) => Ok(Some(read_event(row)?)),
                None => Ok(None),
            }
        })
        .await
    }

    async fn query_events(
        &self,
        filter: EventFilter,
        page: PageRequest,
    ) -> Result<Page<Event>, StorageError> {
        let namespace = self.namespace.clone();

        self.with_reader("query_events", move |conn| {
            let (where_clause, filter_params) = build_event_filter_sql(&namespace, &filter);

            let count_sql = format!("SELECT COUNT(*) FROM events{}", where_clause);
            let total: i64 = {
                let mut stmt = conn.prepare(&count_sql)?;
                let param_refs: Vec<&dyn rusqlite::types::ToSql> =
                    filter_params.iter().map(|p| p.as_ref()).collect();
                stmt.query_row(param_refs.as_slice(), |row| row.get(0))?
            };

            let (_, data_filter_params) = build_event_filter_sql(&namespace, &filter);
            let mut all_params: Vec<Box<dyn rusqlite::types::ToSql>> = data_filter_params;
            all_params.push(Box::new(page.limit as i64));
            all_params.push(Box::new(page.offset as i64));

            let limit_idx = all_params.len() - 1;
            let offset_idx = all_params.len();

            let data_sql = format!(
                "SELECT id, namespace, verb, substrate, actor, outcome, data, duration_us, target_id, created_at \
                 FROM events{} ORDER BY created_at DESC LIMIT ?{} OFFSET ?{}",
                where_clause, limit_idx, offset_idx,
            );

            let mut stmt = conn.prepare(&data_sql)?;
            let param_refs: Vec<&dyn rusqlite::types::ToSql> =
                all_params.iter().map(|p| p.as_ref()).collect();
            let rows = stmt.query_map(param_refs.as_slice(), read_event)?;

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

    async fn count_events(&self, filter: EventFilter) -> Result<u64, StorageError> {
        let namespace = self.namespace.clone();

        self.with_reader("count_events", move |conn| {
            let (where_clause, params) = build_event_filter_sql(&namespace, &filter);
            let sql = format!("SELECT COUNT(*) FROM events{}", where_clause);
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

const EVENTS_DDL: &str = "\
    CREATE TABLE IF NOT EXISTS events (\
        id TEXT PRIMARY KEY,\
        namespace TEXT NOT NULL,\
        verb TEXT NOT NULL,\
        substrate TEXT NOT NULL,\
        actor TEXT NOT NULL,\
        outcome TEXT NOT NULL,\
        data TEXT,\
        duration_us INTEGER NOT NULL DEFAULT 0,\
        target_id TEXT,\
        created_at INTEGER NOT NULL\
    );\
    CREATE INDEX IF NOT EXISTS idx_events_namespace ON events(namespace);\
    CREATE INDEX IF NOT EXISTS idx_events_verb ON events(verb);\
    CREATE INDEX IF NOT EXISTS idx_events_substrate ON events(substrate);\
    CREATE INDEX IF NOT EXISTS idx_events_created ON events(created_at DESC);\
";

pub(crate) fn ensure_events_schema(conn: &rusqlite::Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(EVENTS_DDL)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::PoolConfig;

    fn setup_memory_store() -> SqlEventStore {
        let config = PoolConfig {
            path: None,
            ..PoolConfig::default()
        };
        let pool = Arc::new(ConnectionPool::new(config).unwrap());

        {
            let writer = pool.writer().unwrap();
            writer.conn().execute_batch(EVENTS_DDL).unwrap();
        }

        SqlEventStore::new_scoped(pool, false, "default")
    }

    fn make_event(namespace: &str) -> Event {
        Event::new(namespace, "search", SubstrateKind::Note, "agent:test")
    }

    #[tokio::test]
    async fn test_append_and_get_event() {
        let store = setup_memory_store();

        let event = make_event("default");
        let id = event.id;

        store.append_event(event).await.unwrap();

        let fetched = store.get_event(id).await.unwrap();
        assert!(fetched.is_some());
        let fetched = fetched.unwrap();
        assert_eq!(fetched.id, id);
        assert_eq!(fetched.verb, "search");
        assert_eq!(fetched.substrate, SubstrateKind::Note);
        assert_eq!(fetched.actor, "agent:test");
        assert_eq!(fetched.outcome, EventOutcome::Success);
    }

    #[tokio::test]
    async fn test_append_events_batch() {
        let store = setup_memory_store();

        let events: Vec<Event> = (0..3).map(|_| make_event("default")).collect();
        let summary = store.append_events(events).await.unwrap();
        assert_eq!(summary.attempted, 3);
        assert_eq!(summary.affected, 3);
        assert_eq!(summary.failed, 0);
    }

    #[tokio::test]
    async fn test_count_events() {
        let store = setup_memory_store();

        for _ in 0..3 {
            store.append_event(make_event("default")).await.unwrap();
        }

        let count = store.count_events(EventFilter::default()).await.unwrap();
        assert_eq!(count, 3);
    }

    #[tokio::test]
    async fn test_query_events_filter_by_verb() {
        let store = setup_memory_store();

        store.append_event(make_event("default")).await.unwrap();

        let mut create_event = make_event("default");
        create_event.verb = "create".to_string();
        store.append_event(create_event).await.unwrap();

        let filter = EventFilter {
            verbs: vec!["search".to_string()],
            ..EventFilter::default()
        };
        let page = store
            .query_events(
                filter,
                PageRequest {
                    limit: 10,
                    offset: 0,
                },
            )
            .await
            .unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].verb, "search");
    }

    #[tokio::test]
    async fn test_query_events_filter_by_substrate() {
        let store = setup_memory_store();

        store.append_event(make_event("default")).await.unwrap();

        let mut entity_event = make_event("default");
        entity_event.substrate = SubstrateKind::Entity;
        store.append_event(entity_event).await.unwrap();

        let filter = EventFilter {
            substrates: vec![SubstrateKind::Entity],
            ..EventFilter::default()
        };
        let page = store
            .query_events(
                filter,
                PageRequest {
                    limit: 10,
                    offset: 0,
                },
            )
            .await
            .unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].substrate, SubstrateKind::Entity);
    }

    #[tokio::test]
    async fn test_outcome_roundtrip() {
        let store = setup_memory_store();

        let mut denied = make_event("default");
        denied.outcome = EventOutcome::Denied;
        let denied_id = denied.id;
        store.append_event(denied).await.unwrap();

        let fetched = store.get_event(denied_id).await.unwrap().unwrap();
        assert_eq!(fetched.outcome, EventOutcome::Denied);
    }
}
