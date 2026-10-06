use serde::Deserialize;
use serde_json::{json, Value};

use khive_runtime::{
    micros_to_iso, EventReadPageRequest, EventReadPageResult, NamespaceToken, RuntimeError,
    RuntimeResult,
};
use khive_types::{EventKind, HandlerDef, IdResolutionMode, ParamDef, VerbCategory, Visibility};

use super::parse_rfc3339_micros;
use crate::event_read_scope::event_actor_read_scope;
use crate::BrainPack;

const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

pub(super) const HANDLER: HandlerDef = HandlerDef {
    name: "brain.event_page",
    description: "Page stored event rows and payloads in ascending timestamp/ID order. The cursor \
        is a live-window position, not a snapshot: only greater stored keys can appear later. \
        Current and historical GTD audit rows lack task ID and prior/new status; no fields are inferred.",
    visibility: Visibility::Verb,
    category: VerbCategory::Assertive,
    params: &[
        ParamDef {
            name: "since",
            param_type: "string",
            required: true,
            description: "Inclusive window start, RFC3339 or a date in the display timezone.",
            resolution_mode: IdResolutionMode::NotApplicable,
        },
        ParamDef {
            name: "until",
            param_type: "string",
            required: false,
            description: "Exclusive window end. First omission freezes now; continuation omission keeps that bound.",
            resolution_mode: IdResolutionMode::NotApplicable,
        },
        ParamDef {
            name: "kind",
            param_type: "string",
            required: false,
            description: "One EventKind; combined with kinds and deduplicated.",
            resolution_mode: IdResolutionMode::NotApplicable,
        },
        ParamDef {
            name: "kinds",
            param_type: "array",
            required: false,
            description: "EventKind names; omit for all kinds.",
            resolution_mode: IdResolutionMode::NotApplicable,
        },
        ParamDef {
            name: "namespaces",
            param_type: "array",
            required: false,
            description: "At most 16 requested namespaces, intersected with current visibility. Default is the request namespace; [] is empty.",
            resolution_mode: IdResolutionMode::NotApplicable,
        },
        ParamDef {
            name: "exclude_namespaces",
            param_type: "array",
            required: false,
            description: "At most 32 namespaces excluded in the query before paging.",
            resolution_mode: IdResolutionMode::NotApplicable,
        },
        ParamDef {
            name: "actor",
            param_type: "string",
            required: false,
            description: "Same actor aliases and visibility policy as brain.event_counts; default is the caller.",
            resolution_mode: IdResolutionMode::NotApplicable,
        },
        ParamDef {
            name: "all_actors",
            param_type: "boolean",
            required: false,
            description: "Requires the serving brain.fleet_readers allowlist; cannot be combined with actor and does not widen namespaces.",
            resolution_mode: IdResolutionMode::NotApplicable,
        },
        ParamDef {
            name: "limit",
            param_type: "integer",
            required: false,
            description: "Page size 1 through 1000; default 100.",
            resolution_mode: IdResolutionMode::NotApplicable,
        },
        ParamDef {
            name: "after",
            param_type: "string",
            required: false,
            description: "Opaque continuation from next_after; filters, principal and current visibility must still match.",
            resolution_mode: IdResolutionMode::NotApplicable,
        },
    ],
};

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

impl BrainPack {
    pub(crate) async fn handle_event_page(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> RuntimeResult<Value> {
        let p: EventPageParams = serde_json::from_value(params)
            .map_err(|e| RuntimeError::InvalidInput(e.to_string()))?;
        let actor_scope = event_actor_read_scope(
            &self.runtime,
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
        let display_tz = self.runtime.config().display_timezone;
        let since_us = parse_rfc3339_micros("since", since_raw, false, display_tz)?;
        let until_us = p
            .until
            .as_deref()
            .map(|raw| parse_rfc3339_micros("until", raw, true, display_tz))
            .transpose()?;
        let mut kinds = Vec::new();
        for raw in p.kind.into_iter().chain(p.kinds.into_iter().flatten()) {
            let kind = raw
                .parse::<EventKind>()
                .map_err(|e| RuntimeError::InvalidInput(format!("invalid `kind`: {e}")))?;
            if !kinds.contains(&kind) {
                kinds.push(kind);
            }
        }
        let result = self
            .runtime
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

fn event_page_json(result: EventReadPageResult) -> RuntimeResult<Value> {
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
        return Err(RuntimeError::InvalidInput(
            "event page response exceeds the 4194304-byte limit".into(),
        ));
    }
    Ok(value)
}

#[cfg(test)]
#[path = "event_page_tests.rs"]
mod tests;
