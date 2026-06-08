//! SQL-backed `EventStore` implementation.
//!
//! FILE SIZE JUSTIFICATION: Event store covers append, query-by-filter,
//! observation recording, and paginated listing with shared row-mapping and
//! timestamp serialization helpers. The event schema has complex JSON data
//! columns (observations, referent kinds, outcomes) whose parsing is shared
//! across all read paths, making a split impractical without duplicating the
//! deserialization logic.

use std::sync::Arc;

use async_trait::async_trait;
use uuid::Uuid;

use khive_storage::error::StorageError;
use khive_storage::event::{Event, EventFilter, EventObservation, ObservationRole, ReferentKind};
use khive_storage::types::{BatchWriteSummary, Page, PageRequest};
use khive_storage::EventStore;
use khive_storage::StorageCapability;
use khive_types::{EventKind, EventOutcome, SubstrateKind};

use crate::error::SqliteError;
use crate::pool::ConnectionPool;

fn map_err(e: rusqlite::Error, op: &'static str) -> StorageError {
    StorageError::driver(StorageCapability::Events, op, e)
}

fn map_sqlite_err(e: SqliteError, op: &'static str) -> StorageError {
    StorageError::driver(StorageCapability::Events, op, e)
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
                .map_err(|e| StorageError::driver(StorageCapability::Events, op, e))?
        } else {
            let pool = Arc::clone(&self.pool);
            tokio::task::spawn_blocking(move || {
                let guard = pool.try_writer().map_err(|e| map_sqlite_err(e, op))?;
                f(guard.conn()).map_err(|e| map_err(e, op))
            })
            .await
            .map_err(|e| StorageError::driver(StorageCapability::Events, op, e))?
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
                .map_err(|e| StorageError::driver(StorageCapability::Events, op, e))?
        } else {
            let pool = Arc::clone(&self.pool);
            tokio::task::spawn_blocking(move || {
                let guard = pool.reader().map_err(|e| map_sqlite_err(e, op))?;
                f(guard.conn()).map_err(|e| map_err(e, op))
            })
            .await
            .map_err(|e| StorageError::driver(StorageCapability::Events, op, e))?
        }
    }
}

// =============================================================================
// Helpers: parse SubstrateKind / EventOutcome / EventKind from DB strings
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

fn kind_from_str(s: &str) -> Result<EventKind, rusqlite::Error> {
    s.parse::<EventKind>().map_err(|_| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            format!("unknown EventKind: {s}").into(),
        )
    })
}

fn parse_uuid(s: &str) -> Result<Uuid, rusqlite::Error> {
    Uuid::parse_str(s).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })
}

// Column order: id(0), namespace(1), verb(2), substrate(3), actor(4),
//               kind(5), outcome(6), payload(7), payload_schema_version(8),
//               profile_state_version(9), duration_us(10), target_id(11),
//               session_id(12), aggregate_kind(13), aggregate_id(14), created_at(15)
fn read_event(row: &rusqlite::Row<'_>) -> Result<Event, rusqlite::Error> {
    let id_str: String = row.get(0)?;
    let namespace: String = row.get(1)?;
    let verb: String = row.get(2)?;
    let substrate_str: String = row.get(3)?;
    let actor: String = row.get(4)?;
    let kind_str: String = row.get(5)?;
    let outcome_str: String = row.get(6)?;
    let payload_str: String = row.get(7)?;
    let payload_schema_version: i64 = row.get(8)?;
    let profile_state_version: Option<i64> = row.get(9)?;
    let duration_us: i64 = row.get(10)?;
    let target_str: Option<String> = row.get(11)?;
    let session_str: Option<String> = row.get(12)?;
    let aggregate_kind: Option<String> = row.get(13)?;
    let aggregate_str: Option<String> = row.get(14)?;
    let created_at: i64 = row.get(15)?;

    let id = parse_uuid(&id_str)?;
    let substrate = substrate_from_str(&substrate_str)?;
    let kind = kind_from_str(&kind_str)?;
    let outcome = outcome_from_str(&outcome_str)?;
    let payload: serde_json::Value = serde_json::from_str(&payload_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(7, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let target_id = target_str.as_deref().map(parse_uuid).transpose()?;
    let session_id = session_str.as_deref().map(parse_uuid).transpose()?;
    let aggregate_id = aggregate_str.as_deref().map(parse_uuid).transpose()?;
    let payload_schema_version_u32: u32 = payload_schema_version.try_into().map_err(|_| {
        rusqlite::Error::FromSqlConversionFailure(
            8,
            rusqlite::types::Type::Integer,
            format!("payload_schema_version {payload_schema_version} out of u32 range").into(),
        )
    })?;
    let profile_state_version_u64: Option<u64> = profile_state_version
        .map(|v| {
            u64::try_from(v).map_err(|_| {
                rusqlite::Error::FromSqlConversionFailure(
                    9,
                    rusqlite::types::Type::Integer,
                    format!("profile_state_version {v} out of u64 range").into(),
                )
            })
        })
        .transpose()?;

    Ok(Event {
        id,
        namespace,
        verb,
        substrate,
        actor,
        kind,
        outcome,
        payload,
        payload_schema_version: payload_schema_version_u32,
        profile_state_version: profile_state_version_u64,
        duration_us,
        target_id,
        session_id,
        aggregate_kind,
        aggregate_id,
        created_at,
    })
}

// =============================================================================
// Helpers: observation projection write path
// =============================================================================

fn insert_event_with_observations(
    conn: &rusqlite::Connection,
    event: &Event,
) -> Result<(), rusqlite::Error> {
    let id_str = event.id.to_string();
    let substrate_str = event.substrate.name().to_string();
    let kind_str = event.kind.name().to_string();
    let outcome_str = event.outcome.name().to_string();
    let payload_str = event.payload.to_string();
    let target_str = event.target_id.map(|u| u.to_string());
    let session_str = event.session_id.map(|u| u.to_string());
    let aggregate_str = event.aggregate_id.map(|u| u.to_string());
    let profile_state_version = event.profile_state_version.map(|v| v as i64);

    conn.execute(
        "INSERT INTO events \
         (id, namespace, verb, substrate, actor, kind, outcome, payload, payload_schema_version, \
          profile_state_version, duration_us, target_id, session_id, aggregate_kind, aggregate_id, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
        rusqlite::params![
            id_str,
            &event.namespace,
            &event.verb,
            substrate_str,
            &event.actor,
            kind_str,
            outcome_str,
            payload_str,
            event.payload_schema_version as i64,
            profile_state_version,
            event.duration_us,
            target_str,
            session_str,
            &event.aggregate_kind,
            aggregate_str,
            event.created_at,
        ],
    )?;

    for observation in decode_event_observations(event)? {
        conn.execute(
            "INSERT INTO event_observations \
             (event_id, entity_id, referent_kind, role, position) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                observation.event_id.to_string(),
                observation.entity_id.to_string(),
                observation.referent_kind.name(),
                observation.role.name(),
                observation.position as i64,
            ],
        )?;
    }

    Ok(())
}

fn decode_event_observations(event: &Event) -> Result<Vec<EventObservation>, rusqlite::Error> {
    match event.kind {
        EventKind::RerankExecuted => decode_rank_observations(event),
        EventKind::RecallExecuted | EventKind::SearchExecuted => decode_rank_observations(event),
        EventKind::LinkCreated => decode_link_observations(event),
        EventKind::EntityCreated
        | EventKind::EntityUpdated
        | EventKind::EntityDeleted
        | EventKind::NoteCreated
        | EventKind::NoteUpdated
        | EventKind::NoteDeleted
        | EventKind::TaskTransitioned => decode_target_observation(event),
        EventKind::FeedbackExplicit => decode_signal_observation(event),
        _ => Ok(Vec::new()),
    }
}

fn payload_uuid_array(event: &Event, field: &'static str) -> Result<Vec<Uuid>, rusqlite::Error> {
    let Some(values) = event.payload.get(field) else {
        return Ok(Vec::new());
    };
    let Some(array) = values.as_array() else {
        return Err(invalid_payload(event.kind, field, "expected array"));
    };

    array
        .iter()
        .map(|value| {
            value
                .as_str()
                .ok_or_else(|| invalid_payload(event.kind, field, "expected UUID string"))
                .and_then(|s| Uuid::parse_str(s).map_err(|e| invalid_payload(event.kind, field, e)))
        })
        .collect()
}

fn payload_uuid(event: &Event, field: &'static str) -> Result<Option<Uuid>, rusqlite::Error> {
    let Some(value) = event.payload.get(field) else {
        return Ok(None);
    };
    let Some(s) = value.as_str() else {
        return Err(invalid_payload(event.kind, field, "expected UUID string"));
    };
    Uuid::parse_str(s)
        .map(Some)
        .map_err(|e| invalid_payload(event.kind, field, e))
}

fn decode_rank_observations(event: &Event) -> Result<Vec<EventObservation>, rusqlite::Error> {
    let mut rows = Vec::new();

    for (position, entity_id) in payload_uuid_array(event, "candidates")?
        .into_iter()
        .enumerate()
    {
        rows.push(EventObservation {
            event_id: event.id,
            entity_id,
            referent_kind: ReferentKind::Note,
            role: ObservationRole::Candidate,
            position: position as u32,
        });
    }

    let selected = payload_uuid_array(event, "selected")
        .or_else(|_| payload_uuid_array(event, "reranked"))
        .or_else(|_| payload_uuid_array(event, "final_scores"))?;
    for (position, entity_id) in selected.into_iter().enumerate() {
        rows.push(EventObservation {
            event_id: event.id,
            entity_id,
            referent_kind: ReferentKind::Note,
            role: ObservationRole::Selected,
            position: position as u32,
        });
    }

    Ok(rows)
}

fn decode_link_observations(event: &Event) -> Result<Vec<EventObservation>, rusqlite::Error> {
    let mut rows = Vec::new();
    if let Some(source) = payload_uuid(event, "source_id")? {
        rows.push(EventObservation {
            event_id: event.id,
            entity_id: source,
            referent_kind: ReferentKind::Entity,
            role: ObservationRole::Target,
            position: 0,
        });
    }
    if let Some(target) = payload_uuid(event, "target_id")? {
        rows.push(EventObservation {
            event_id: event.id,
            entity_id: target,
            referent_kind: ReferentKind::Entity,
            role: ObservationRole::Target,
            position: 1,
        });
    }
    Ok(rows)
}

fn decode_target_observation(event: &Event) -> Result<Vec<EventObservation>, rusqlite::Error> {
    let Some(entity_id) = event.target_id.or(payload_uuid(event, "target_id")?) else {
        return Ok(Vec::new());
    };
    Ok(vec![EventObservation {
        event_id: event.id,
        entity_id,
        referent_kind: if event.substrate == SubstrateKind::Note {
            ReferentKind::Note
        } else {
            ReferentKind::Entity
        },
        role: ObservationRole::Target,
        position: 0,
    }])
}

fn decode_signal_observation(event: &Event) -> Result<Vec<EventObservation>, rusqlite::Error> {
    let Some(entity_id) = payload_uuid(event, "about_id")? else {
        return Ok(Vec::new());
    };
    Ok(vec![EventObservation {
        event_id: event.id,
        entity_id,
        referent_kind: ReferentKind::Entity,
        role: ObservationRole::Signal,
        position: 0,
    }])
}

fn invalid_payload(
    kind: EventKind,
    field: &'static str,
    reason: impl std::fmt::Display,
) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(
        format!("invalid payload for {}.{field}: {reason}", kind.name()).into(),
    )
}

// =============================================================================
// Helpers: filter SQL builder
// =============================================================================

fn build_event_filter_sql(
    conn: &rusqlite::Connection,
    default_namespace: &str,
    filter: &EventFilter,
) -> Result<(String, Vec<Box<dyn rusqlite::types::ToSql>>), rusqlite::Error> {
    reject_missing_event_filter_schema(conn, filter)?;

    let mut conditions: Vec<String> = Vec::new();
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

    params.push(Box::new(default_namespace.to_string()));
    conditions.push(format!("namespace = ?{}", params.len()));

    push_in_clause(
        &mut conditions,
        &mut params,
        "id",
        filter.ids.iter().map(Uuid::to_string),
    );
    push_in_clause(
        &mut conditions,
        &mut params,
        "kind",
        filter.kinds.iter().map(|kind| kind.name().to_string()),
    );
    push_in_clause(
        &mut conditions,
        &mut params,
        "verb",
        filter.verbs.iter().cloned(),
    );
    push_in_clause(
        &mut conditions,
        &mut params,
        "substrate",
        filter.substrates.iter().map(|s| s.name().to_string()),
    );
    push_in_clause(
        &mut conditions,
        &mut params,
        "actor",
        filter.actors.iter().cloned(),
    );

    if let Some(after) = filter.after {
        params.push(Box::new(after));
        conditions.push(format!("created_at > ?{}", params.len()));
    }

    if let Some(before) = filter.before {
        params.push(Box::new(before));
        conditions.push(format!("created_at < ?{}", params.len()));
    }

    if let Some(session_id) = filter.session_id {
        params.push(Box::new(session_id.to_string()));
        conditions.push(format!("session_id = ?{}", params.len()));
    }

    push_observation_exists(&mut conditions, &mut params, "candidate", &filter.observed);
    push_observation_exists(&mut conditions, &mut params, "selected", &filter.selected);

    if let Some(proposal_id) = filter.payload_proposal_id {
        params.push(Box::new(proposal_id.to_string()));
        conditions.push(format!(
            "json_extract(payload, '$.proposal_id') = ?{}",
            params.len()
        ));
    }

    let clause = format!(" WHERE {}", conditions.join(" AND "));
    Ok((clause, params))
}

fn push_in_clause<I>(
    conditions: &mut Vec<String>,
    params: &mut Vec<Box<dyn rusqlite::types::ToSql>>,
    column: &'static str,
    values: I,
) where
    I: IntoIterator<Item = String>,
{
    let placeholders: Vec<String> = values
        .into_iter()
        .map(|value| {
            params.push(Box::new(value));
            format!("?{}", params.len())
        })
        .collect();
    if !placeholders.is_empty() {
        conditions.push(format!("{column} IN ({})", placeholders.join(",")));
    }
}

fn push_observation_exists(
    conditions: &mut Vec<String>,
    params: &mut Vec<Box<dyn rusqlite::types::ToSql>>,
    role: &'static str,
    entity_ids: &[Uuid],
) {
    if entity_ids.is_empty() {
        return;
    }
    let placeholders: Vec<String> = entity_ids
        .iter()
        .map(|id| {
            params.push(Box::new(id.to_string()));
            format!("?{}", params.len())
        })
        .collect();
    conditions.push(format!(
        "EXISTS (SELECT 1 FROM event_observations o \
         WHERE o.event_id = events.id AND o.role = '{role}' AND o.entity_id IN ({}))",
        placeholders.join(",")
    ));
}

fn reject_missing_event_filter_schema(
    conn: &rusqlite::Connection,
    filter: &EventFilter,
) -> Result<(), rusqlite::Error> {
    if filter.session_id.is_some() && !has_column(conn, "events", "session_id")? {
        return Err(schema_absent("events.session_id"));
    }
    if (!filter.observed.is_empty() || !filter.selected.is_empty())
        && !has_table(conn, "event_observations")?
    {
        return Err(schema_absent("event_observations"));
    }
    if filter.payload_proposal_id.is_some() && !has_column(conn, "events", "payload")? {
        return Err(schema_absent("events.payload"));
    }
    Ok(())
}

fn has_table(conn: &rusqlite::Connection, table: &'static str) -> Result<bool, rusqlite::Error> {
    conn.query_row(
        "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [table],
        |row| row.get(0),
    )
}

fn has_column(
    conn: &rusqlite::Connection,
    table: &'static str,
    column: &'static str,
) -> Result<bool, rusqlite::Error> {
    conn.query_row(
        "SELECT COUNT(*) > 0 FROM pragma_table_info(?1) WHERE name = ?2",
        rusqlite::params![table, column],
        |row| row.get(0),
    )
}

fn schema_absent(name: &'static str) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(
        format!("event filter requires missing schema element {name}; run migrations").into(),
    )
}

// =============================================================================
// EventStore implementation
// =============================================================================

#[async_trait]
impl EventStore for SqlEventStore {
    async fn append_event(&self, event: Event) -> Result<(), StorageError> {
        self.with_writer("append_event", move |conn| {
            conn.execute_batch("BEGIN IMMEDIATE")?;
            if let Err(e) = insert_event_with_observations(conn, &event) {
                let _ = conn.execute_batch("ROLLBACK");
                return Err(e);
            }
            conn.execute_batch("COMMIT")?;
            Ok(())
        })
        .await
    }

    async fn append_events(&self, events: Vec<Event>) -> Result<BatchWriteSummary, StorageError> {
        let attempted = events.len() as u64;

        self.with_writer("append_events", move |conn| {
            conn.execute_batch("BEGIN IMMEDIATE")?;
            let mut affected = 0u64;

            for event in &events {
                if let Err(e) = insert_event_with_observations(conn, event) {
                    let _ = conn.execute_batch("ROLLBACK");
                    return Err(e);
                }
                affected += 1;
            }

            conn.execute_batch("COMMIT")?;
            Ok(BatchWriteSummary {
                attempted,
                affected,
                failed: 0,
                first_error: String::new(),
            })
        })
        .await
    }

    async fn get_event(&self, id: Uuid) -> Result<Option<Event>, StorageError> {
        let namespace = self.namespace.clone();
        let id_str = id.to_string();

        self.with_reader("get_event", move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, namespace, verb, substrate, actor, kind, outcome, payload, \
                        payload_schema_version, profile_state_version, duration_us, target_id, \
                        session_id, aggregate_kind, aggregate_id, created_at \
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
            let (where_clause, filter_params) = build_event_filter_sql(conn, &namespace, &filter)?;

            let count_sql = format!("SELECT COUNT(*) FROM events{}", where_clause);
            let total: i64 = {
                let mut stmt = conn.prepare(&count_sql)?;
                let param_refs: Vec<&dyn rusqlite::types::ToSql> =
                    filter_params.iter().map(|p| p.as_ref()).collect();
                stmt.query_row(param_refs.as_slice(), |row| row.get(0))?
            };

            let (_, data_filter_params) = build_event_filter_sql(conn, &namespace, &filter)?;
            let mut all_params: Vec<Box<dyn rusqlite::types::ToSql>> = data_filter_params;
            all_params.push(Box::new(page.limit as i64));
            all_params.push(Box::new(page.offset as i64));

            let limit_idx = all_params.len() - 1;
            let offset_idx = all_params.len();

            let data_sql = format!(
                "SELECT id, namespace, verb, substrate, actor, kind, outcome, payload, \
                        payload_schema_version, profile_state_version, duration_us, target_id, \
                        session_id, aggregate_kind, aggregate_id, created_at \
                 FROM events{} ORDER BY created_at DESC, id DESC LIMIT ?{} OFFSET ?{}",
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
            let (where_clause, params) = build_event_filter_sql(conn, &namespace, &filter)?;
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
        kind TEXT NOT NULL DEFAULT 'audit',\
        outcome TEXT NOT NULL,\
        payload TEXT NOT NULL DEFAULT '{}',\
        payload_schema_version INTEGER NOT NULL DEFAULT 1,\
        profile_state_version INTEGER,\
        duration_us INTEGER NOT NULL DEFAULT 0,\
        target_id TEXT,\
        session_id TEXT,\
        aggregate_kind TEXT,\
        aggregate_id TEXT,\
        created_at INTEGER NOT NULL\
    );\
    CREATE TABLE IF NOT EXISTS event_observations (\
        event_id TEXT NOT NULL,\
        entity_id TEXT NOT NULL,\
        referent_kind TEXT NOT NULL,\
        role TEXT NOT NULL,\
        position INTEGER NOT NULL,\
        PRIMARY KEY (event_id, role, position)\
    );\
    CREATE INDEX IF NOT EXISTS idx_events_namespace ON events(namespace);\
    CREATE INDEX IF NOT EXISTS idx_events_verb ON events(verb);\
    CREATE INDEX IF NOT EXISTS idx_events_kind ON events(kind);\
    CREATE INDEX IF NOT EXISTS idx_events_substrate ON events(substrate);\
    CREATE INDEX IF NOT EXISTS idx_events_created ON events(created_at DESC);\
    CREATE INDEX IF NOT EXISTS idx_events_ns_created_id ON events(namespace, created_at DESC, id DESC);\
    CREATE INDEX IF NOT EXISTS idx_events_session ON events(namespace, session_id, created_at, id);\
    CREATE INDEX IF NOT EXISTS idx_events_payload_proposal_id ON events(json_extract(payload, '$.proposal_id'));\
    CREATE INDEX IF NOT EXISTS idx_event_obs_entity ON event_observations(entity_id, role);\
    CREATE INDEX IF NOT EXISTS idx_event_obs_event_role ON event_observations(event_id, role);\
";

pub(crate) fn ensure_events_schema(conn: &rusqlite::Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(EVENTS_DDL)
}

#[cfg(test)]
#[path = "event_tests.rs"]
mod tests;
