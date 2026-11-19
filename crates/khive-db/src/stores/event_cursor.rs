use std::sync::Arc;

use khive_storage::event::{EventOrderKey, EventPageQuery, EventPageRow, EventPageWindow};
use khive_storage::{StorageCapability, StorageError};
use rusqlite::types::ValueRef;
use uuid::Uuid;

use super::{map_err, read_event, SqlEventStore};

const QUERY: &str = include_str!("../../sql/events-cursor-page.sql");
const MAX_ROWS: u32 = 4096;
const MAX_RAW_TEXT_BYTES: usize = 1024 * 1024;
const OPERATION: &str = "query_event_page";

fn invalid(message: &'static str) -> StorageError {
    StorageError::InvalidInput {
        capability: StorageCapability::Events,
        operation: OPERATION.into(),
        message: message.into(),
    }
}

fn physical_uuid(id: &str) -> Option<Uuid> {
    if !matches!(id.len(), 32 | 36 | 38 | 45) {
        return None;
    }
    Uuid::parse_str(id).ok()
}

fn validate(query: &EventPageQuery) -> Result<(), StorageError> {
    if !(1..=MAX_ROWS).contains(&query.max_rows) {
        return Err(invalid("event page max_rows must be in 1..=4096"));
    }
    if query.since_us >= query.until_us {
        return Err(invalid("event page requires since_us < until_us"));
    }
    if query.exclude_namespaces.len() > 32 {
        return Err(invalid(
            "event page permits at most 32 namespace exclusions",
        ));
    }
    if query.after.as_ref().is_some_and(|after| {
        after.created_at_us < query.since_us
            || after.created_at_us >= query.until_us
            || physical_uuid(&after.physical_id).is_none()
    }) {
        return Err(invalid("invalid event page ordering key"));
    }
    Ok(())
}

pub(super) async fn query_event_page(
    store: &SqlEventStore,
    query: EventPageQuery,
) -> Result<EventPageWindow, StorageError> {
    validate(&query)?;
    let namespace = store.namespace.clone();
    super::super::run_pooled_store_read(
        Arc::clone(&store.pool),
        StorageCapability::Events,
        OPERATION,
        move |conn| read_window(conn, &namespace, &query),
    )
    .await
}

fn read_window(
    conn: &rusqlite::Connection,
    namespace: &str,
    query: &EventPageQuery,
) -> Result<EventPageWindow, StorageError> {
    let kinds = serde_json::to_string(
        &query
            .kinds
            .iter()
            .map(|kind| kind.name())
            .collect::<Vec<_>>(),
    )
    .map_err(|error| StorageError::driver(StorageCapability::Events, OPERATION, error))?;
    let actors = serde_json::to_string(&query.actors)
        .map_err(|error| StorageError::driver(StorageCapability::Events, OPERATION, error))?;
    let exclusions = serde_json::to_string(&query.exclude_namespaces)
        .map_err(|error| StorageError::driver(StorageCapability::Events, OPERATION, error))?;
    let mut statement = conn
        .prepare(QUERY)
        .map_err(|error| map_err(error, OPERATION))?;
    let mut rows = statement
        .query(rusqlite::params![
            namespace,
            query.since_us,
            query.until_us,
            kinds,
            actors,
            exclusions,
            query.after.as_ref().map(|key| key.created_at_us),
            query.after.as_ref().map(|key| key.physical_id.as_str()),
            i64::from(query.max_rows),
        ])
        .map_err(|error| map_err(error, OPERATION))?;
    let mut result = Vec::new();
    let mut raw_bytes = 0usize;
    let mut budget_stop = None;
    while let Some(row) = rows.next().map_err(|error| map_err(error, OPERATION))? {
        let mut row_bytes = 0usize;
        for column in 0..18 {
            let bytes = match row
                .get_ref(column)
                .map_err(|error| map_err(error, OPERATION))?
            {
                ValueRef::Text(value) | ValueRef::Blob(value) => value.len(),
                _ => 0,
            };
            row_bytes = row_bytes.saturating_add(bytes);
        }
        if raw_bytes.saturating_add(row_bytes) > MAX_RAW_TEXT_BYTES {
            let physical_id: String = row.get(0).map_err(|error| map_err(error, OPERATION))?;
            physical_uuid(&physical_id)
                .ok_or_else(|| invalid("invalid stored event page ordering key"))?;
            let created_at_us: i64 = row.get(15).map_err(|error| map_err(error, OPERATION))?;
            budget_stop = Some(EventOrderKey {
                created_at_us,
                physical_id,
            });
            break;
        }
        raw_bytes += row_bytes;
        let physical_id: String = row.get(0).map_err(|error| map_err(error, OPERATION))?;
        let id = physical_uuid(&physical_id)
            .ok_or_else(|| invalid("invalid stored event page ordering key"))?;
        let created_at_us: i64 = row.get(15).map_err(|error| map_err(error, OPERATION))?;
        let event = read_event(row).map_err(|error| map_err(error, OPERATION))?;
        if event.id != id || event.created_at != created_at_us {
            return Err(invalid(
                "event page row does not match its stored ordering key",
            ));
        }
        result.push(EventPageRow {
            order_key: EventOrderKey {
                created_at_us,
                physical_id,
            },
            event,
        });
    }
    Ok(EventPageWindow {
        rows: result,
        budget_stop,
    })
}

#[cfg(test)]
#[path = "event_cursor_tests.rs"]
mod tests;
