use serde::Deserialize;
use serde_json::{json, Value};

use crate::{
    micros_to_iso, page_budget_exceeded_error, row_exceeds_budget_error, EventReadPageRequest,
    EventReadPageResult, KhiveRuntime, NamespaceToken, RuntimeError, RuntimeResult,
};

use super::parsing::{event_actor_read_scope, parse_event_kind, parse_rfc3339_micros};

pub(super) const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EventPageParams {
    since: Option<String>,
    until: Option<String>,
    kind: Option<String>,
    kinds: Option<Vec<String>>,
    namespaces: Option<Vec<String>>,
    exclude_namespaces: Option<Vec<String>>,
    actor: Option<String>,
    all_actors: Option<bool>,
    limit: Option<u32>,
    after: Option<String>,
}

impl KhiveRuntime {
    /// Read and format the brain event page through the authorized token.
    pub async fn read_event_page_response(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> RuntimeResult<Value> {
        let p: EventPageParams = serde_json::from_value(params)
            .map_err(|e| RuntimeError::InvalidInput(e.to_string()))?;
        let actor_scope = event_actor_read_scope(
            self,
            token,
            p.actor.as_deref(),
            p.all_actors.unwrap_or(false),
        )?;
        let since_raw = p.since.as_deref().ok_or_else(|| {
            RuntimeError::InvalidInput(
                "missing `since`: required ISO-8601/RFC-3339 datetime; expected e.g. \
                 \"2026-07-01T00:00:00Z\""
                    .to_string(),
            )
        })?;
        let display_tz = self.config().display_timezone;
        let since_us = parse_rfc3339_micros("since", since_raw, false, display_tz)?;
        let until_us = p
            .until
            .as_deref()
            .map(|raw| parse_rfc3339_micros("until", raw, true, display_tz))
            .transpose()?;
        let mut kinds = Vec::new();
        for raw in p.kind.into_iter().chain(p.kinds.into_iter().flatten()) {
            let kind = parse_event_kind(&raw)?;
            if !kinds.contains(&kind) {
                kinds.push(kind);
            }
        }
        let result = self
            .page_events(
                token,
                EventReadPageRequest {
                    since_us,
                    until_us,
                    kinds,
                    actors: actor_scope.actors,
                    namespaces: p.namespaces,
                    exclude_namespaces: p.exclude_namespaces.unwrap_or_default(),
                    limit: p.limit.unwrap_or(100),
                    after: p.after,
                },
            )
            .await?;
        event_page_json(result)
    }
}

pub(super) fn event_page_json(result: EventReadPageResult) -> RuntimeResult<Value> {
    let single_row = match (result.events.as_slice(), result.single_row_cursor) {
        ([event], Some(cursor)) => Some((event.id, cursor)),
        _ => None,
    };
    let mut events = Vec::with_capacity(result.events.len());
    for event in result.events {
        let created_at = event.created_at;
        let mut value = serde_json::to_value(event).map_err(|_| {
            RuntimeError::InvalidInput("event page response serialization failed".into())
        })?;
        value["created_at"] = json!(micros_to_iso(created_at));
        events.push(value);
    }
    let value = json!({
        "count": events.len(),
        "events": events,
        "has_more": result.has_more,
        "next_after": result.next_after,
        "since": micros_to_iso(result.since_us),
        "until": micros_to_iso(result.until_us),
        "scope": { "namespaces": result.namespaces },
        "consistency": "live_ordered_window",
    });
    let encoded = serde_json::to_vec(&value).map_err(|_| {
        RuntimeError::InvalidInput("event page response serialization failed".into())
    })?;
    if encoded.len() > MAX_RESPONSE_BYTES {
        return Err(match single_row {
            Some((event_id, cursor)) => row_exceeds_budget_error(event_id, cursor),
            None => page_budget_exceeded_error(),
        });
    }
    Ok(value)
}
