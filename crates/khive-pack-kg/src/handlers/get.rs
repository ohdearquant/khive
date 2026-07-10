//! `get` verb handler.

use std::str::FromStr;

use serde_json::Value;
use uuid::Uuid;

use khive_runtime::{
    hex_prefix_to_uuid_pattern, NamespaceToken, Resolved, RuntimeError, VerbRegistry,
};
use khive_storage::event::Event;
use khive_storage::types::{SqlRow, SqlStatement, SqlValue};
use khive_types::EventKind;

use super::common::{
    deser, flatten_get_result, normalize_entity_timestamps, normalize_event_timestamps,
    parse_event_kind, parse_event_outcome, parse_event_substrate, remap_note_status,
    resolve_uuid_unfiltered, to_json, GetParams,
};
use crate::KgPack;

impl KgPack {
    pub(crate) async fn handle_get(
        &self,
        token: &NamespaceToken,
        graph_token: &NamespaceToken,
        params: Value,
        registry: &VerbRegistry,
    ) -> Result<Value, RuntimeError> {
        let p: GetParams = deser(params)?;

        // By-ID resolution (including the hex-prefix form) is namespace-agnostic
        // (ADR-007 Rev 6 / #391 §3) — the Gate is the authz seam, not this lookup.
        let id = if let Ok(id) = resolve_uuid_unfiltered(&p.id, &self.runtime, graph_token).await {
            id
        } else if let Ok(id) = resolve_uuid_unfiltered(&p.id, &self.runtime, token).await {
            id
        } else {
            if let Some(payload_val) = self.try_get_proposal_payload(token, &p.id).await? {
                return Ok(payload_val);
            }
            return Err(RuntimeError::NotFound(format!("not found: {}", p.id)));
        };

        let include_deleted = p.include_deleted.unwrap_or(false);

        match self.runtime.get_entity(graph_token, id).await {
            Ok(entity) => {
                return flatten_get_result(
                    "entity",
                    normalize_entity_timestamps(to_json(&entity)?),
                );
            }
            Err(RuntimeError::NotFound(_) | RuntimeError::NamespaceMismatch { .. }) => {
                if include_deleted {
                    if let Some(deleted) = self
                        .runtime
                        .get_entity_including_deleted(graph_token, id)
                        .await?
                    {
                        return flatten_get_result(
                            "entity",
                            normalize_entity_timestamps(to_json(&deleted)?),
                        );
                    }
                }
            }
            Err(e) => return Err(e),
        }

        // PR-A1: by-ID get returns the note regardless of namespace (UUID v4 is globally unique).
        // Visible-set gating removed here; list/search keep their namespace filter (PR-B scope).
        if let Some(note) = self
            .runtime
            .notes(token)?
            .get_note(id)
            .await
            .map_err(RuntimeError::Storage)?
        {
            let note_val = normalize_entity_timestamps(to_json(&note)?);
            let remapped = remap_note_status(note_val);
            return flatten_get_result("note", remapped);
        }

        // PR-A1: by-ID edge get returns the edge regardless of namespace.
        if let Some(edge) = self.runtime.get_edge(token, id).await? {
            return flatten_get_result("edge", to_json(&edge)?);
        }

        if let Some(event) = self.get_event_unfiltered_by_id(id).await? {
            return flatten_get_result("event", normalize_event_timestamps(to_json(&event)?));
        }

        // Pack-resolver probe: ask each registered resolver for pack-private records
        // (e.g. knowledge_atoms, knowledge_domains) that the standard substrates cannot see.
        for (_pack_name, resolver) in registry.resolvers() {
            if let Some(Resolved::PackRecord { kind, data, .. }) =
                resolver.resolve_by_id(id).await?
            {
                return flatten_get_result(&kind, data);
            }
        }

        if let Some(payload_val) = self.try_get_proposal_payload(token, &p.id).await? {
            return Ok(payload_val);
        }

        Err(RuntimeError::NotFound(format!("not found: {}", p.id)))
    }

    /// Fetch an event by ID without a namespace predicate (ADR-007 Rev 6 pattern).
    /// Only for by-ID `get`; event `list`/`query` surfaces must keep namespace scoping.
    async fn get_event_unfiltered_by_id(&self, id: Uuid) -> Result<Option<Event>, RuntimeError> {
        let sql = self.runtime.sql();
        let mut reader = sql.reader().await.map_err(RuntimeError::Storage)?;
        let row = reader
            .query_row(SqlStatement {
                sql: "SELECT id, namespace, verb, substrate, actor, kind, outcome, payload, \
                      payload_schema_version, profile_state_version, duration_us, target_id, \
                      session_id, aggregate_kind, aggregate_id, created_at \
                      FROM events WHERE id = ?1 LIMIT 1"
                    .to_string(),
                params: vec![SqlValue::Text(id.to_string())],
                label: Some("events.get_unfiltered_by_id".into()),
            })
            .await
            .map_err(RuntimeError::Storage)?;

        let Some(row) = row else {
            return Ok(None);
        };

        Ok(Some(Event {
            id: parse_uuid_column(&row, "id")?,
            namespace: sql_text(&row, "namespace")?,
            verb: sql_text(&row, "verb")?,
            substrate: parse_event_substrate(&sql_text(&row, "substrate")?)
                .map_err(|_| RuntimeError::Internal("stored event substrate is invalid".into()))?,
            actor: sql_text(&row, "actor")?,
            kind: parse_event_kind(&sql_text(&row, "kind")?)
                .map_err(|_| RuntimeError::Internal("stored event kind is invalid".into()))?,
            outcome: parse_event_outcome(&sql_text(&row, "outcome")?)
                .map_err(|_| RuntimeError::Internal("stored event outcome is invalid".into()))?,
            payload: serde_json::from_str(&sql_text(&row, "payload")?).map_err(|e| {
                RuntimeError::Internal(format!("stored event payload is invalid JSON: {e}"))
            })?,
            payload_schema_version: u32::try_from(sql_i64(&row, "payload_schema_version")?)
                .map_err(|_| {
                    RuntimeError::Internal("stored event payload_schema_version is invalid".into())
                })?,
            profile_state_version: sql_optional_i64(&row, "profile_state_version")?
                .map(|v| {
                    u64::try_from(v).map_err(|_| {
                        RuntimeError::Internal(
                            "stored event profile_state_version is invalid".into(),
                        )
                    })
                })
                .transpose()?,
            duration_us: sql_i64(&row, "duration_us")?,
            target_id: sql_optional_uuid(&row, "target_id")?,
            session_id: sql_optional_uuid(&row, "session_id")?,
            aggregate_kind: sql_optional_text(&row, "aggregate_kind")?,
            aggregate_id: sql_optional_uuid(&row, "aggregate_id")?,
            created_at: sql_i64(&row, "created_at")?,
        }))
    }

    pub(crate) async fn try_get_proposal_payload(
        &self,
        token: &NamespaceToken,
        raw_id: &str,
    ) -> Result<Option<Value>, RuntimeError> {
        let ns = token.namespace().as_str().to_owned();

        let (sql_str, params) = if Uuid::from_str(raw_id).is_ok() {
            (
                "SELECT proposal_id FROM proposals_open \
                 WHERE proposal_id = ?1 AND namespace = ?2 LIMIT 1"
                    .to_string(),
                vec![SqlValue::Text(raw_id.to_string()), SqlValue::Text(ns)],
            )
        } else if raw_id.len() >= 8 && raw_id.chars().all(|c| c.is_ascii_hexdigit()) {
            let pattern = format!("{}%", hex_prefix_to_uuid_pattern(raw_id));
            (
                "SELECT proposal_id FROM proposals_open \
                 WHERE proposal_id LIKE ?1 AND namespace = ?2 LIMIT 2"
                    .to_string(),
                vec![SqlValue::Text(pattern), SqlValue::Text(ns)],
            )
        } else {
            return Ok(None);
        };

        let sql = self.runtime.sql();
        let rows = {
            let mut reader = match sql.reader().await {
                Ok(r) => r,
                Err(e) => return Err(RuntimeError::Storage(e)),
            };
            match reader
                .query_all(SqlStatement {
                    sql: sql_str,
                    params,
                    label: Some("proposals_open.resolve_for_get".into()),
                })
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    if e.to_string().contains("no such table") {
                        return Ok(None);
                    }
                    return Err(RuntimeError::Storage(e));
                }
            }
        };

        if rows.len() != 1 {
            return Ok(None);
        }

        let full_uuid_str = rows[0]
            .get("proposal_id")
            .and_then(|v| {
                if let SqlValue::Text(s) = v {
                    Some(s.clone())
                } else {
                    None
                }
            })
            .ok_or_else(|| {
                RuntimeError::Internal("proposal_id column missing from proposals_open row".into())
            })?;

        let proposal_uuid = Uuid::from_str(&full_uuid_str).map_err(|e| {
            RuntimeError::Internal(format!("stored proposal_id is not a valid UUID: {e}"))
        })?;

        let event_store = self.runtime.events(token)?;
        let page = event_store
            .query_events(
                khive_storage::EventFilter {
                    kinds: vec![EventKind::ProposalCreated],
                    payload_proposal_id: Some(proposal_uuid),
                    ..Default::default()
                },
                khive_storage::types::PageRequest {
                    offset: 0,
                    limit: 1,
                },
            )
            .await
            .map_err(RuntimeError::Storage)?;

        let event = match page.items.into_iter().next() {
            Some(e) => e,
            None => {
                return Err(RuntimeError::Internal(format!(
                    "ProposalCreated event not found for proposal_id {proposal_uuid}"
                )));
            }
        };

        let payload_str = event.payload.to_string();
        let payload: khive_types::ProposalCreatedPayload = serde_json::from_str(&payload_str)
            .map_err(|e| {
                RuntimeError::Internal(format!(
                    "failed to deserialize ProposalCreated payload: {e}"
                ))
            })?;

        let mut result = serde_json::to_value(&payload).map_err(|e| {
            RuntimeError::Internal(format!(
                "failed to re-serialize ProposalCreatedPayload: {e}"
            ))
        })?;

        if let serde_json::Value::Object(ref mut map) = result {
            if let Some(v) = map.remove("proposal_id") {
                map.insert("id".to_string(), v);
            }
            map.insert(
                "kind".to_string(),
                serde_json::Value::String("proposal".to_string()),
            );
        }

        Ok(Some(result))
    }
}

fn sql_text(row: &SqlRow, name: &str) -> Result<String, RuntimeError> {
    match row.get(name) {
        Some(SqlValue::Text(v)) => Ok(v.clone()),
        Some(other) => Err(RuntimeError::Internal(format!(
            "events.{name} has unexpected SQL value {other:?}"
        ))),
        None => Err(RuntimeError::Internal(format!("events row missing {name}"))),
    }
}

fn sql_optional_text(row: &SqlRow, name: &str) -> Result<Option<String>, RuntimeError> {
    match row.get(name) {
        Some(SqlValue::Null) | None => Ok(None),
        Some(SqlValue::Text(v)) => Ok(Some(v.clone())),
        Some(other) => Err(RuntimeError::Internal(format!(
            "events.{name} has unexpected SQL value {other:?}"
        ))),
    }
}

fn sql_i64(row: &SqlRow, name: &str) -> Result<i64, RuntimeError> {
    match row.get(name) {
        Some(SqlValue::Integer(v)) => Ok(*v),
        Some(other) => Err(RuntimeError::Internal(format!(
            "events.{name} has unexpected SQL value {other:?}"
        ))),
        None => Err(RuntimeError::Internal(format!("events row missing {name}"))),
    }
}

fn sql_optional_i64(row: &SqlRow, name: &str) -> Result<Option<i64>, RuntimeError> {
    match row.get(name) {
        Some(SqlValue::Null) | None => Ok(None),
        Some(SqlValue::Integer(v)) => Ok(Some(*v)),
        Some(other) => Err(RuntimeError::Internal(format!(
            "events.{name} has unexpected SQL value {other:?}"
        ))),
    }
}

fn parse_uuid_column(row: &SqlRow, name: &str) -> Result<Uuid, RuntimeError> {
    Uuid::from_str(&sql_text(row, name)?)
        .map_err(|e| RuntimeError::Internal(format!("events.{name} is not a UUID: {e}")))
}

fn sql_optional_uuid(row: &SqlRow, name: &str) -> Result<Option<Uuid>, RuntimeError> {
    sql_optional_text(row, name)?
        .map(|v| {
            Uuid::from_str(&v)
                .map_err(|e| RuntimeError::Internal(format!("events.{name} is not a UUID: {e}")))
        })
        .transpose()
}
