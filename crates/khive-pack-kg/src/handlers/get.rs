//! `get` verb handler.

use std::str::FromStr;

use serde_json::Value;
use uuid::Uuid;

use khive_runtime::{NamespaceToken, RuntimeError};
use khive_storage::types::{SqlStatement, SqlValue};
use khive_types::EventKind;

use super::common::{
    deser, flatten_get_result, normalize_entity_timestamps, normalize_event_timestamps,
    remap_note_status, resolve_uuid_async, to_json, GetParams,
};
use crate::KgPack;

impl KgPack {
    pub(crate) async fn handle_get(
        &self,
        token: &NamespaceToken,
        graph_token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: GetParams = deser(params)?;

        let id = if let Ok(id) = resolve_uuid_async(&p.id, &self.runtime, graph_token).await {
            id
        } else if let Ok(id) = resolve_uuid_async(&p.id, &self.runtime, token).await {
            id
        } else {
            if let Some(payload_val) = self.try_get_proposal_payload(token, &p.id).await? {
                return Ok(payload_val);
            }
            return Err(RuntimeError::NotFound(format!("not found: {}", p.id)));
        };

        if let Ok(entity) = self.runtime.get_entity(graph_token, id).await {
            return flatten_get_result("entity", normalize_entity_timestamps(to_json(&entity)?));
        }

        if let Some(note) = self
            .runtime
            .notes(token)?
            .get_note(id)
            .await
            .map_err(RuntimeError::Storage)?
        {
            if note.namespace == token.namespace().as_str() {
                let note_val = normalize_entity_timestamps(to_json(&note)?);
                let remapped = remap_note_status(note_val);
                return flatten_get_result("note", remapped);
            }
        }

        if let Some(edge) = self.runtime.get_edge(graph_token, id).await? {
            return flatten_get_result("edge", to_json(&edge)?);
        }

        if let Some(event) = self
            .runtime
            .events(token)?
            .get_event(id)
            .await
            .map_err(RuntimeError::Storage)?
        {
            if event.namespace == token.namespace().as_str() {
                return flatten_get_result("event", normalize_event_timestamps(to_json(&event)?));
            }
        }

        if let Some(payload_val) = self.try_get_proposal_payload(token, &p.id).await? {
            return Ok(payload_val);
        }

        Err(RuntimeError::NotFound(format!("not found: {}", p.id)))
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
            let pattern = format!("{}%", raw_id);
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
            map.insert(
                "kind".to_string(),
                serde_json::Value::String("proposal".to_string()),
            );
        }

        Ok(Some(result))
    }
}
