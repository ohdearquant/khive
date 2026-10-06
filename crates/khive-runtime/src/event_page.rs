use std::cmp::Ordering;
use std::io::Write;

use khive_storage::event::{EventOrderKey, EventPageQuery, EventPageRow, EventPageWindow};
use khive_storage::{Event, EventStore, StorageCapability, StorageError, StorageResult};
use khive_types::{Details, EventKind, KhiveError};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{KhiveRuntime, Namespace, NamespaceToken, RuntimeError, RuntimeResult};

const MAX_NAMESPACES: usize = 16;
const MAX_EXCLUSIONS: usize = 32;
const MAX_LIMIT: u32 = 1000;
const MAX_CURSOR_BYTES: usize = 512;
const MAX_AGGREGATE_BYTES: usize = 32 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct EventReadPageRequest {
    pub since_us: i64,
    pub until_us: Option<i64>,
    pub kinds: Vec<EventKind>,
    pub actors: Vec<String>,
    pub namespaces: Option<Vec<String>>,
    pub exclude_namespaces: Vec<String>,
    pub limit: u32,
    pub after: Option<String>,
}

#[derive(Clone, Debug)]
pub struct EventReadPageResult {
    pub events: Vec<Event>,
    pub has_more: bool,
    pub next_after: Option<String>,
    pub since_us: i64,
    pub until_us: i64,
    pub namespaces: Vec<String>,
    /// Cursor positioned at the returned row when the page holds exactly one row.
    /// Only used to build a `row_exceeds_budget` refusal; never part of a response body.
    pub single_row_cursor: Option<String>,
}

/// Refusal for a page whose rows together pass a byte budget. Carries no cursor:
/// a smaller `limit` reaches the same rows without skipping any.
pub fn page_budget_exceeded_error() -> RuntimeError {
    RuntimeError::Khive(
        KhiveError::invalid_input(
            "event page exceeds its byte budget; retry with a smaller `limit`",
        )
        .with_details(Details::new([("reason", "page_budget_exceeded")])),
    )
}

/// Refusal for a single event whose row cannot be served within a byte budget.
/// `resume_after` continues strictly after that event, which stays readable by id.
pub fn row_exceeds_budget_error(event_id: Uuid, resume_after: String) -> RuntimeError {
    RuntimeError::Khive(
        KhiveError::invalid_input(
            "an event exceeds the event page byte budget and cannot be returned in a page; \
             continue from `resume_after` to skip it, or read it by id",
        )
        .with_details(Details::new_owned([
            ("reason", "row_exceeds_budget".to_owned()),
            ("event_id", event_id.to_string()),
            ("resume_after", resume_after),
        ])),
    )
}

impl KhiveRuntime {
    /// Read a live ordered window. Actor aliases are authorized by the caller;
    /// this boundary rechecks namespace visibility and cursor scope on every page.
    pub async fn page_events(
        &self,
        token: &NamespaceToken,
        mut request: EventReadPageRequest,
    ) -> RuntimeResult<EventReadPageResult> {
        if !(1..=MAX_LIMIT).contains(&request.limit) {
            return Err(invalid("event page limit must be between 1 and 1000"));
        }
        let requested = request
            .namespaces
            .take()
            .unwrap_or_else(|| vec![token.namespace().as_str().to_owned()]);
        let candidates = normalize_names(requested, MAX_NAMESPACES)?
            .into_iter()
            .filter(|name| {
                token
                    .visible_namespaces()
                    .iter()
                    .any(|ns| ns.as_str() == name.as_str())
            })
            .collect::<Vec<_>>();
        let exclusions = normalize_names(request.exclude_namespaces, MAX_EXCLUSIONS)?;
        request.kinds.sort_by_key(|kind| kind.name());
        request.kinds.dedup();
        request.actors.sort();
        request.actors.dedup();
        let cursor = request.after.as_deref().map(decode_cursor).transpose()?;
        let until_us = match (&cursor, request.until_us) {
            (Some(cursor), Some(until)) if cursor.until_us != until => {
                return Err(invalid_cursor());
            }
            (Some(cursor), _) => cursor.until_us,
            (None, Some(until)) => until,
            (None, None) => chrono::Utc::now().timestamp_micros(),
        };
        if !valid_time(request.since_us) || !valid_time(until_us) || request.since_us >= until_us {
            return Err(invalid("invalid event page time window"));
        }
        let binding = filter_binding(
            token,
            request.since_us,
            until_us,
            &request.kinds,
            &request.actors,
            &candidates,
            &exclusions,
        )?;
        if cursor.as_ref().is_some_and(|cursor| {
            cursor.binding != binding
                || cursor.key.created_at_us < request.since_us
                || cursor.key.created_at_us >= until_us
        }) {
            return Err(invalid_cursor());
        }
        let query = EventPageQuery {
            since_us: request.since_us,
            until_us,
            kinds: request.kinds,
            actors: request.actors,
            exclude_namespaces: exclusions.clone(),
            after: cursor.map(|cursor| cursor.key),
            max_rows: request.limit + 1,
        };
        let mut rows = Vec::new();
        let mut stops = Vec::new();
        let mut budget = ByteBudget(MAX_AGGREGATE_BYTES);
        for namespace in &candidates {
            // with_namespace transfers capability; intersection above is the policy check.
            let scoped = token.with_namespace(
                Namespace::parse(namespace).map_err(|_| invalid("invalid namespace"))?,
            );
            let mut window = self
                .events(&scoped)?
                .query_event_page(query.clone())
                .await?;
            validate_window(&query, Some(namespace), &window)?;
            serde_json::to_writer(&mut budget, &window.rows)
                .map_err(|_| page_budget_exceeded_error())?;
            stops.extend(window.budget_stop.take());
            rows.extend(window.rows);
        }
        rows.sort_by(|a, b| compare_keys(&a.order_key, &b.order_key));
        reject_duplicate_keys(&rows)?;
        reject_stop_collisions(&rows, &stops)?;
        let limit = request.limit as usize;
        let stop = stops.into_iter().min_by(compare_keys);
        if let Some(stop) = &stop {
            let servable =
                rows.partition_point(|row| compare_keys(&row.order_key, stop) == Ordering::Less);
            rows.truncate(servable);
            if servable == 0 {
                let event_id = physical_uuid(&stop.physical_id)
                    .ok_or_else(|| page_error("event page row invariant violated"))?;
                return Err(row_exceeds_budget_error(
                    event_id,
                    encode_cursor(until_us, stop, &binding),
                ));
            }
            if servable < limit {
                return Err(page_budget_exceeded_error());
            }
        }
        let has_more = stop.is_some() || rows.len() > limit;
        rows.truncate(limit);
        let next_after = if has_more {
            rows.last()
                .map(|row| encode_cursor(until_us, &row.order_key, &binding))
        } else {
            None
        };
        let single_row_cursor = match rows.as_slice() {
            [row] => Some(encode_cursor(until_us, &row.order_key, &binding)),
            _ => None,
        };
        Ok(EventReadPageResult {
            events: rows.into_iter().map(|row| row.event).collect(),
            has_more,
            next_after,
            since_us: request.since_us,
            until_us,
            namespaces: candidates
                .into_iter()
                .filter(|namespace| !exclusions.contains(namespace))
                .collect(),
            single_row_cursor,
        })
    }
}

fn normalize_names(names: Vec<String>, cap: usize) -> RuntimeResult<Vec<String>> {
    if names.len() > cap {
        return Err(invalid("event page namespace list exceeds its bound"));
    }
    let mut names = names
        .into_iter()
        .map(|name| {
            Namespace::parse(&name)
                .map(|ns| ns.as_str().to_owned())
                .map_err(|_| invalid("invalid event page namespace"))
        })
        .collect::<RuntimeResult<Vec<_>>>()?;
    names.sort();
    names.dedup();
    Ok(names)
}

fn valid_time(time: i64) -> bool {
    chrono::DateTime::<chrono::Utc>::from_timestamp_micros(time).is_some()
}

fn invalid(message: &str) -> RuntimeError {
    RuntimeError::InvalidInput(message.to_owned())
}

fn invalid_cursor() -> RuntimeError {
    invalid("invalid event page cursor or changed query scope")
}

struct Cursor {
    until_us: i64,
    key: EventOrderKey,
    binding: String,
}

fn decode_cursor(raw: &str) -> RuntimeResult<Cursor> {
    if raw.len() > MAX_CURSOR_BYTES {
        return Err(invalid_cursor());
    }
    let fields = raw.split(':').collect::<Vec<_>>();
    if fields.len() != 5 || fields[0] != "ep1" {
        return Err(invalid_cursor());
    }
    let parse_time = |value: &str| -> RuntimeResult<i64> {
        let time = value.parse::<i64>().map_err(|_| invalid_cursor())?;
        if time.to_string() != value || !valid_time(time) {
            return Err(invalid_cursor());
        }
        Ok(time)
    };
    let until_us = parse_time(fields[1])?;
    let created_at_us = parse_time(fields[2])?;
    if !matches!(fields[3].len(), 64 | 72 | 76 | 90)
        || !lower_hex(fields[3])
        || fields[4].len() != 64
        || !lower_hex(fields[4])
    {
        return Err(invalid_cursor());
    }
    let id_bytes = fields[3]
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let pair = std::str::from_utf8(pair).map_err(|_| invalid_cursor())?;
            u8::from_str_radix(pair, 16).map_err(|_| invalid_cursor())
        })
        .collect::<RuntimeResult<Vec<_>>>()?;
    let physical_id = String::from_utf8(id_bytes).map_err(|_| invalid_cursor())?;
    if physical_uuid(&physical_id).is_none() {
        return Err(invalid_cursor());
    }
    let key = EventOrderKey {
        created_at_us,
        physical_id,
    };
    if encode_cursor(until_us, &key, fields[4]) != raw {
        return Err(invalid_cursor());
    }
    Ok(Cursor {
        until_us,
        key,
        binding: fields[4].to_owned(),
    })
}

fn lower_hex(value: &str) -> bool {
    value
        .bytes()
        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 15) as usize] as char);
    }
    output
}

fn encode_cursor(until_us: i64, key: &EventOrderKey, binding: &str) -> String {
    format!(
        "ep1:{until_us}:{}:{}:{binding}",
        key.created_at_us,
        hex(key.physical_id.as_bytes())
    )
}

fn filter_binding(
    token: &NamespaceToken,
    since_us: i64,
    until_us: i64,
    kinds: &[EventKind],
    actors: &[String],
    namespaces: &[String],
    exclusions: &[String],
) -> RuntimeResult<String> {
    let kinds = kinds.iter().map(|kind| kind.name()).collect::<Vec<_>>();
    let bytes = serde_json::to_vec(&(
        "event-page-v1",
        &token.actor().kind,
        &token.actor().id,
        since_us,
        until_us,
        kinds,
        actors,
        namespaces,
        exclusions,
    ))
    .map_err(|_| invalid("event page filter encoding failed"))?;
    Ok(hex(&Sha256::digest(bytes)))
}

pub(crate) fn page_error(message: &str) -> StorageError {
    StorageError::InvalidInput {
        capability: StorageCapability::Events,
        operation: "query_event_page".into(),
        message: message.to_owned(),
    }
}

pub(crate) fn compare_keys(a: &EventOrderKey, b: &EventOrderKey) -> Ordering {
    a.created_at_us
        .cmp(&b.created_at_us)
        .then_with(|| a.physical_id.as_bytes().cmp(b.physical_id.as_bytes()))
}

fn physical_uuid(value: &str) -> Option<Uuid> {
    matches!(value.len(), 32 | 36 | 38 | 45)
        .then(|| Uuid::parse_str(value).ok())
        .flatten()
}

pub(crate) fn validate_page_query(query: &EventPageQuery) -> StorageResult<()> {
    if !(1..=crate::events_split::MAX_QUERY_EVENTS_PAGE_ROWS).contains(&query.max_rows)
        || !valid_time(query.since_us)
        || !valid_time(query.until_us)
        || query.since_us >= query.until_us
        || query.exclude_namespaces.len() > MAX_EXCLUSIONS
        || query
            .exclude_namespaces
            .iter()
            .any(|ns| Namespace::parse(ns).is_err())
        || query.after.as_ref().is_some_and(|key| {
            key.created_at_us < query.since_us
                || key.created_at_us >= query.until_us
                || physical_uuid(&key.physical_id).is_none()
        })
    {
        return Err(page_error("invalid bounded event page query"));
    }
    Ok(())
}

pub(crate) fn validate_window(
    query: &EventPageQuery,
    namespace: Option<&str>,
    window: &EventPageWindow,
) -> StorageResult<()> {
    if window.rows.len() > query.max_rows as usize {
        return Err(page_error("event page row bound violated"));
    }
    let mut previous = query.after.as_ref();
    for row in &window.rows {
        let event = &row.event;
        if row.order_key.created_at_us != event.created_at
            || physical_uuid(&row.order_key.physical_id) != Some(event.id)
            || namespace.is_some_and(|ns| ns != event.namespace)
            || query.exclude_namespaces.contains(&event.namespace)
            || (!query.kinds.is_empty() && !query.kinds.contains(&event.kind))
            || (!query.actors.is_empty() && !query.actors.contains(&event.actor))
            || event.created_at < query.since_us
            || event.created_at >= query.until_us
            || query
                .after
                .as_ref()
                .is_some_and(|key| compare_keys(&row.order_key, key) != Ordering::Greater)
            || previous.is_some_and(|key| compare_keys(&row.order_key, key) != Ordering::Greater)
        {
            return Err(page_error("event page row invariant violated"));
        }
        previous = Some(&row.order_key);
    }
    if let Some(stop) = &window.budget_stop {
        if window.rows.len() >= query.max_rows as usize
            || stop.created_at_us < query.since_us
            || stop.created_at_us >= query.until_us
            || physical_uuid(&stop.physical_id).is_none()
            || previous.is_some_and(|key| compare_keys(stop, key) != Ordering::Greater)
        {
            return Err(page_error("event page row invariant violated"));
        }
    }
    Ok(())
}

fn reject_duplicate_keys(rows: &[EventPageRow]) -> StorageResult<()> {
    if rows
        .windows(2)
        .any(|pair| compare_keys(&pair[0].order_key, &pair[1].order_key) == Ordering::Equal)
    {
        return Err(page_error("event page duplicate ordering key"));
    }
    Ok(())
}

/// A stopped row shares its key with a returned row or with another stopped row:
/// a strict cursor cannot represent a position between them.
fn reject_stop_collisions(rows: &[EventPageRow], stops: &[EventOrderKey]) -> StorageResult<()> {
    let collides = stops.iter().enumerate().any(|(index, stop)| {
        stops[index + 1..]
            .iter()
            .any(|other| compare_keys(stop, other) == Ordering::Equal)
            || rows
                .iter()
                .any(|row| compare_keys(&row.order_key, stop) == Ordering::Equal)
    });
    if collides {
        return Err(page_error("event page duplicate ordering key"));
    }
    Ok(())
}

pub(crate) async fn split_page(
    legacy: &dyn EventStore,
    lane: &dyn EventStore,
    query: EventPageQuery,
) -> StorageResult<EventPageWindow> {
    validate_page_query(&query)?;
    let legacy = legacy.query_event_page(query.clone()).await?;
    validate_window(&query, None, &legacy)?;
    let lane = lane.query_event_page(query.clone()).await?;
    validate_window(&query, None, &lane)?;
    let stops = legacy
        .budget_stop
        .iter()
        .chain(lane.budget_stop.iter())
        .cloned()
        .collect::<Vec<_>>();
    let mut rows: Vec<EventPageRow> = legacy.rows;
    rows.extend(lane.rows);
    rows.sort_by(|a, b| compare_keys(&a.order_key, &b.order_key));
    reject_stop_collisions(&rows, &stops)?;
    let stop = stops.into_iter().min_by(compare_keys);
    if let Some(stop) = &stop {
        rows.truncate(
            rows.partition_point(|row| compare_keys(&row.order_key, stop) == Ordering::Less),
        );
    }
    reject_duplicate_keys(&rows)?;
    let max_rows = query.max_rows as usize;
    let budget_stop = if rows.len() >= max_rows { None } else { stop };
    rows.truncate(max_rows);
    Ok(EventPageWindow { rows, budget_stop })
}

struct ByteBudget(usize);

impl Write for ByteBudget {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.0 {
            return Err(std::io::Error::other("event page byte budget exceeded"));
        }
        self.0 -= bytes.len();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
#[path = "event_page_tests.rs"]
mod tests;
