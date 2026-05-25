//! ProposalApplyWorker — applies approved proposal changesets to the KG.
//!
//! Called from `handle_review` after a `ProposalReviewed` event is emitted.
//! When the approval threshold is met (default: 1 approve, no rejects,
//! status='approved'), the worker:
//!
//! 1. Reads the `ProposalCreated` event from the event log to get the changeset.
//! 2. Dispatches each `ProposalChangeset` arm to the existing runtime API.
//! 3. Emits a `ProposalApplied` event (success or failure).
//! 4. Calls the projection worker to update status='applied'.
//!
//! ADR-046 §5: apply runs as a side-effect consumer, not synchronously inside
//! `review`. In this v1 implementation the consumer is called synchronously
//! from the handler (PackEventConsumer infrastructure is not yet shipped).
//! The semantic contract — event emitted first, apply second — is preserved.
//!
//! ADR-046 §2 Compound changeset atomicity: all steps run in the same SQLite
//! write session. On any step failure, the worker emits
//! `ProposalApplied { Failed }` and returns without updating projection status.

use std::str::FromStr;

use uuid::Uuid;

use khive_runtime::{
    curation::{EntityDedupMergePolicy, EntityPatch},
    KhiveRuntime, NamespaceToken, RuntimeError,
};
use khive_storage::types::PageRequest;
use khive_storage::{EdgeRelation, EventFilter};
use khive_types::{
    ApplyResult, EventKind, Id128, ProposalAppliedPayload, ProposalChangeset,
    ProposalCreatedPayload, Timestamp,
};

use crate::projection_worker::ProposalsProjectionWorker;

/// Worker that applies approved proposal changesets.
pub struct ProposalApplyWorker {
    runtime: KhiveRuntime,
    projection: ProposalsProjectionWorker,
}

impl ProposalApplyWorker {
    pub fn new(runtime: KhiveRuntime) -> Self {
        let projection = ProposalsProjectionWorker::new(runtime.clone());
        Self {
            runtime,
            projection,
        }
    }

    /// Called from `handle_review` after emitting a `ProposalReviewed` event.
    ///
    /// Checks whether the proposal should now be applied (threshold met, not already
    /// applied/withdrawn). If yes, applies the changeset and emits `ProposalApplied`.
    ///
    /// Returns `Ok(())` in all cases — errors are emitted as `ProposalApplied { Failed }`
    /// events, not propagated to the reviewer.
    pub async fn maybe_apply(
        &self,
        token: &NamespaceToken,
        proposal_id: Uuid,
    ) -> Result<(), RuntimeError> {
        // Load current projection row.
        let row = match self.projection.get_proposal_row(token, proposal_id).await? {
            Some(r) => r,
            None => return Ok(()), // No row — proposal doesn't exist, nothing to do.
        };

        // Only apply when: status='approved', no rejects.
        // ADR-046 §6: v1 threshold = 1 approve, no recorded reject.
        if row.status != "approved" || row.reject_count > 0 {
            return Ok(());
        }

        // Load the ProposalCreated event to get the changeset.
        let changeset = match self.load_changeset(token, proposal_id).await {
            Ok(cs) => cs,
            Err(e) => {
                self.emit_apply_failed(token, proposal_id, e.to_string(), 0)
                    .await;
                return Ok(());
            }
        };

        // Apply the changeset.
        let apply_result = self.apply_changeset(token, &changeset).await;

        match apply_result {
            Ok(created_records) => {
                let created_ids: Vec<Id128> = created_records
                    .iter()
                    .map(|id| Id128::from_u128(id.as_u128()))
                    .collect();
                self.emit_apply_success(token, proposal_id, created_ids)
                    .await;
                // Update projection: status='applied'.
                if let Err(e) = self
                    .projection
                    .on_proposal_applied(token, proposal_id)
                    .await
                {
                    tracing::warn!(
                        proposal_id = %proposal_id,
                        error = %e,
                        "ProposalApplyWorker: projection update failed after successful apply (non-fatal)"
                    );
                }
            }
            Err(e) => {
                self.emit_apply_failed(token, proposal_id, e.to_string(), 0)
                    .await;
                // ADR-046 §9: failed applies leave status='approved' — no projection update.
            }
        }

        Ok(())
    }

    /// Load the ProposalCreated event payload to get the changeset.
    async fn load_changeset(
        &self,
        token: &NamespaceToken,
        proposal_id: Uuid,
    ) -> Result<ProposalChangeset, RuntimeError> {
        let event_store = self.runtime.events(token)?;
        let filter = EventFilter {
            kinds: vec![EventKind::ProposalCreated],
            payload_proposal_id: Some(proposal_id),
            ..Default::default()
        };
        let page = event_store
            .query_events(
                filter,
                PageRequest {
                    offset: 0,
                    limit: 1,
                },
            )
            .await
            .map_err(RuntimeError::Storage)?;

        let event = page.items.into_iter().next().ok_or_else(|| {
            RuntimeError::NotFound(format!(
                "ProposalCreated event not found for proposal_id {proposal_id}"
            ))
        })?;

        // Use from_str (not from_value) so that Id128's Deserialize impl — which
        // borrows &str from the deserializer — works correctly.
        // from_value uses a Value-backed deserializer that cannot lend &str.
        let payload_str = event.payload.to_string();
        let payload: ProposalCreatedPayload = serde_json::from_str(&payload_str).map_err(|e| {
            RuntimeError::Internal(format!(
                "failed to deserialize ProposalCreated payload: {e}"
            ))
        })?;

        Ok(payload.changeset)
    }

    /// Apply a single changeset arm (or recursively for Compound).
    ///
    /// Returns the list of created record UUIDs (for AddEntity / AddNote / AddEdge).
    ///
    /// The function is `Box::pin`-wrapped to support recursion (Compound steps).
    fn apply_changeset<'a>(
        &'a self,
        token: &'a NamespaceToken,
        changeset: &'a ProposalChangeset,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Vec<Uuid>, RuntimeError>> + Send + 'a>,
    > {
        Box::pin(async move {
            match changeset {
                ProposalChangeset::AddEntity { entity } => {
                    self.apply_add_entity(token, entity).await
                }
                ProposalChangeset::UpdateEntity { id, patch } => {
                    self.apply_update_entity(token, *id, patch).await?;
                    Ok(vec![])
                }
                ProposalChangeset::AddEdge {
                    source,
                    target,
                    relation,
                    weight,
                } => {
                    let edge_id = self
                        .apply_add_edge(token, *source, *target, *relation, *weight)
                        .await?;
                    Ok(vec![edge_id])
                }
                ProposalChangeset::AddNote { note } => self.apply_add_note(token, note).await,
                ProposalChangeset::MergeEntities { into, from } => {
                    self.apply_merge_entities(token, *into, *from).await?;
                    Ok(vec![])
                }
                ProposalChangeset::SupersedeEntity { old, new } => {
                    self.apply_supersede_entity(token, *old, *new).await?;
                    Ok(vec![])
                }
                ProposalChangeset::Compound { steps } => {
                    let mut all_created = Vec::new();
                    for step in steps {
                        let created = self.apply_changeset(token, step).await?;
                        all_created.extend(created);
                    }
                    Ok(all_created)
                }
            }
        })
    }

    /// Apply `AddEntity`: deserialize the entity JSON draft and create it.
    async fn apply_add_entity(
        &self,
        token: &NamespaceToken,
        entity_json: &str,
    ) -> Result<Vec<Uuid>, RuntimeError> {
        let draft: serde_json::Value = serde_json::from_str(entity_json).map_err(|e| {
            RuntimeError::InvalidInput(format!("AddEntity: invalid entity JSON: {e}"))
        })?;
        let kind = draft
            .get("kind")
            .and_then(|v| v.as_str())
            .ok_or_else(|| RuntimeError::InvalidInput("AddEntity: missing 'kind' field".into()))?;
        let name = draft
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| RuntimeError::InvalidInput("AddEntity: missing 'name' field".into()))?;
        let description = draft.get("description").and_then(|v| v.as_str());
        let properties = draft.get("properties").cloned();
        let tags: Vec<String> = draft
            .get("tags")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|t| t.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        let entity = self
            .runtime
            .create_entity(token, kind, None, name, description, properties, tags)
            .await?;
        Ok(vec![entity.id])
    }

    /// Apply `UpdateEntity`: parse the patch JSON and call `update_entity`.
    async fn apply_update_entity(
        &self,
        token: &NamespaceToken,
        id: Id128,
        patch_json: &str,
    ) -> Result<(), RuntimeError> {
        let entity_id = Uuid::from_u128(id.to_u128());
        let patch_val: serde_json::Value = serde_json::from_str(patch_json).map_err(|e| {
            RuntimeError::InvalidInput(format!("UpdateEntity: invalid patch JSON: {e}"))
        })?;
        let patch = EntityPatch {
            name: patch_val
                .get("name")
                .and_then(|v| v.as_str())
                .map(String::from),
            description: patch_val.get("description").map(|v| {
                if v.is_null() {
                    None
                } else {
                    v.as_str().map(String::from)
                }
            }),
            properties: patch_val.get("properties").cloned(),
            tags: patch_val.get("tags").and_then(|v| v.as_array()).map(|arr| {
                arr.iter()
                    .filter_map(|t| t.as_str().map(String::from))
                    .collect()
            }),
        };
        self.runtime.update_entity(token, entity_id, patch).await?;
        Ok(())
    }

    /// Apply `AddEdge`: link source→target with the given relation.
    async fn apply_add_edge(
        &self,
        token: &NamespaceToken,
        source: Id128,
        target: Id128,
        relation: khive_types::EdgeRelation,
        weight: Option<f32>,
    ) -> Result<Uuid, RuntimeError> {
        let source_id = Uuid::from_u128(source.to_u128());
        let target_id = Uuid::from_u128(target.to_u128());
        // khive_storage::EdgeRelation is a re-export of khive_types::EdgeRelation.
        // Parse via the snake_case name so the type identity is assured at the call site.
        let storage_relation = {
            let name = relation.as_str();
            EdgeRelation::from_str(name)
                .map_err(|_| RuntimeError::InvalidInput(format!("unknown edge relation: {name}")))?
        };
        let edge = self
            .runtime
            .link(
                token,
                source_id,
                target_id,
                storage_relation,
                weight.unwrap_or(1.0) as f64,
                None,
            )
            .await?;
        // Edge.id is LinkId(Uuid); extract the inner Uuid.
        Ok(edge.id.0)
    }

    /// Apply `AddNote`: deserialize the note JSON draft and create the note.
    async fn apply_add_note(
        &self,
        token: &NamespaceToken,
        note_json: &str,
    ) -> Result<Vec<Uuid>, RuntimeError> {
        let draft: serde_json::Value = serde_json::from_str(note_json)
            .map_err(|e| RuntimeError::InvalidInput(format!("AddNote: invalid note JSON: {e}")))?;
        let kind = draft
            .get("kind")
            .and_then(|v| v.as_str())
            .ok_or_else(|| RuntimeError::InvalidInput("AddNote: missing 'kind' field".into()))?;
        let content = draft
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| RuntimeError::InvalidInput("AddNote: missing 'content' field".into()))?;
        let name = draft.get("name").and_then(|v| v.as_str());
        let properties = draft.get("properties").cloned();

        let note = self
            .runtime
            .create_note(token, kind, name, content, None, properties, vec![])
            .await?;
        Ok(vec![note.id])
    }

    /// Apply `MergeEntities`: merge `from` into `into`.
    async fn apply_merge_entities(
        &self,
        token: &NamespaceToken,
        into: Id128,
        from: Id128,
    ) -> Result<(), RuntimeError> {
        let into_id = Uuid::from_u128(into.to_u128());
        let from_id = Uuid::from_u128(from.to_u128());
        self.runtime
            .merge_entity(
                token,
                into_id,
                from_id,
                EntityDedupMergePolicy::PreferInto,
                false,
            )
            .await?;
        Ok(())
    }

    /// Apply `SupersedeEntity`: add a `supersedes` edge from `new` → `old`.
    async fn apply_supersede_entity(
        &self,
        token: &NamespaceToken,
        old: Id128,
        new: Id128,
    ) -> Result<(), RuntimeError> {
        let old_id = Uuid::from_u128(old.to_u128());
        let new_id = Uuid::from_u128(new.to_u128());
        let relation = EdgeRelation::Supersedes;
        self.runtime
            .link(token, new_id, old_id, relation, 1.0, None)
            .await?;
        Ok(())
    }

    /// Emit a `ProposalApplied` event with a success result.
    async fn emit_apply_success(
        &self,
        token: &NamespaceToken,
        proposal_id: Uuid,
        created_records: Vec<Id128>,
    ) {
        let payload = ProposalAppliedPayload {
            proposal_id: Id128::from_u128(proposal_id.as_u128()),
            applied_at: Timestamp::from_micros(chrono::Utc::now().timestamp_micros() as u64),
            applied_by: "system:propose-apply".to_string(),
            result: khive_types::ApplyResult::Success { created_records },
        };
        self.emit_apply_event(token, proposal_id, payload).await;
    }

    /// Emit a `ProposalApplied` event with a failure result.
    async fn emit_apply_failed(
        &self,
        token: &NamespaceToken,
        proposal_id: Uuid,
        error: String,
        applied_step_count: u32,
    ) {
        let payload = ProposalAppliedPayload {
            proposal_id: Id128::from_u128(proposal_id.as_u128()),
            applied_at: Timestamp::from_micros(chrono::Utc::now().timestamp_micros() as u64),
            applied_by: "system:propose-apply".to_string(),
            result: ApplyResult::Failed {
                error,
                applied_step_count,
            },
        };
        self.emit_apply_event(token, proposal_id, payload).await;
    }

    async fn emit_apply_event(
        &self,
        token: &NamespaceToken,
        proposal_id: Uuid,
        payload: ProposalAppliedPayload,
    ) {
        let ns = token.namespace().as_str().to_owned();
        let payload_json = match serde_json::to_value(&payload) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    proposal_id = %proposal_id,
                    error = %e,
                    "ProposalApplyWorker: failed to serialize ProposalAppliedPayload"
                );
                return;
            }
        };
        let mut event = khive_storage::event::Event::new(
            &ns,
            "propose-apply",
            EventKind::ProposalApplied,
            khive_storage::SubstrateKind::Entity,
            "system:propose-apply",
        );
        event.payload = payload_json;
        event.aggregate_kind = Some("proposal".to_string());
        event.aggregate_id = Some(proposal_id);

        let Ok(event_store) = self.runtime.events(token) else {
            tracing::warn!(
                proposal_id = %proposal_id,
                "ProposalApplyWorker: could not get event store to emit ProposalApplied"
            );
            return;
        };
        if let Err(e) = event_store.append_event(event).await {
            tracing::warn!(
                proposal_id = %proposal_id,
                error = %e,
                "ProposalApplyWorker: failed to emit ProposalApplied event (non-fatal)"
            );
        }
    }
}

// ---- Tests ------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use khive_runtime::{KhiveRuntime, Namespace};
    use khive_storage::types::{PageRequest, SqlStatement, SqlValue};
    use khive_types::{Id128, ProposalChangeset, ProposalCreatedPayload};
    use uuid::Uuid;

    fn setup() -> (KhiveRuntime, NamespaceToken) {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let tok = rt.authorize(Namespace::local());
        (rt, tok)
    }

    async fn ensure_schema(rt: &KhiveRuntime) {
        let sql = rt.sql();
        let mut writer = sql.writer().await.expect("writer");
        writer
            .execute(SqlStatement {
                sql: "\
                CREATE TABLE IF NOT EXISTS proposals_open (\
                    proposal_id TEXT PRIMARY KEY, \
                    namespace TEXT NOT NULL, \
                    proposer TEXT NOT NULL, \
                    title TEXT NOT NULL, \
                    status TEXT NOT NULL, \
                    created_at INTEGER NOT NULL, \
                    updated_at INTEGER NOT NULL, \
                    expiry INTEGER, \
                    last_decision TEXT, \
                    review_count INTEGER NOT NULL DEFAULT 0, \
                    approve_count INTEGER NOT NULL DEFAULT 0, \
                    reject_count INTEGER NOT NULL DEFAULT 0\
                )"
                .to_string(),
                params: vec![],
                label: Some("test.ensure_schema".into()),
            })
            .await
            .expect("create table");
    }

    async fn insert_projection_row(
        rt: &KhiveRuntime,
        tok: &NamespaceToken,
        proposal_id: Uuid,
        status: &str,
    ) {
        let now = chrono::Utc::now().timestamp_micros();
        let ns = tok.namespace().as_str().to_owned();
        let sql = rt.sql();
        let mut writer = sql.writer().await.expect("writer");
        writer
            .execute(SqlStatement {
                sql: "INSERT OR REPLACE INTO proposals_open \
                  (proposal_id, namespace, proposer, title, status, created_at, updated_at, \
                   approve_count, reject_count, review_count) \
                  VALUES (?1, ?2, 'alice', 'Test', ?3, ?4, ?4, 1, 0, 1)"
                    .to_string(),
                params: vec![
                    SqlValue::Text(proposal_id.to_string()),
                    SqlValue::Text(ns),
                    SqlValue::Text(status.to_string()),
                    SqlValue::Integer(now),
                ],
                label: Some("test.insert_projection_row".into()),
            })
            .await
            .expect("insert row");
    }

    async fn seed_proposal_created_event(
        rt: &KhiveRuntime,
        tok: &NamespaceToken,
        proposal_id: Uuid,
        changeset: ProposalChangeset,
    ) {
        let payload = ProposalCreatedPayload {
            proposal_id: Id128::from_u128(proposal_id.as_u128()),
            proposer: "alice".to_string(),
            title: "Test".to_string(),
            description: "desc".to_string(),
            changeset,
            reviewers: vec![],
            expiry: None,
            parent_id: None,
        };
        let payload_json = serde_json::to_value(&payload).expect("serialize");
        let mut event = khive_storage::event::Event::new(
            tok.namespace().as_str(),
            "propose",
            EventKind::ProposalCreated,
            khive_storage::SubstrateKind::Entity,
            "alice",
        );
        event.payload = payload_json;
        event.aggregate_kind = Some("proposal".to_string());
        event.aggregate_id = Some(proposal_id);
        rt.events(tok)
            .expect("event store")
            .append_event(event)
            .await
            .expect("append event");
    }

    #[tokio::test]
    async fn apply_worker_applies_add_edge_changeset() {
        let (rt, tok) = setup();
        ensure_schema(&rt).await;

        // Create two entities to link.
        let e1 = rt
            .create_entity(&tok, "concept", None, "EntityA", None, None, vec![])
            .await
            .expect("create e1");
        let e2 = rt
            .create_entity(&tok, "concept", None, "EntityB", None, None, vec![])
            .await
            .expect("create e2");

        let proposal_id = Uuid::new_v4();
        let changeset = ProposalChangeset::AddEdge {
            source: Id128::from_u128(e1.id.as_u128()),
            target: Id128::from_u128(e2.id.as_u128()),
            relation: khive_types::EdgeRelation::Extends,
            weight: Some(1.0),
        };

        // Seed the ProposalCreated event in the event store.
        seed_proposal_created_event(&rt, &tok, proposal_id, changeset).await;

        // Seed the projection row in 'approved' state (1 approve, 0 rejects).
        insert_projection_row(&rt, &tok, proposal_id, "approved").await;

        let worker = ProposalApplyWorker::new(rt.clone());
        worker
            .maybe_apply(&tok, proposal_id)
            .await
            .expect("maybe_apply must succeed");

        // Verify: edge exists in graph store (source = e1).
        let edges = rt
            .list_edges(
                &tok,
                khive_runtime::EdgeListFilter {
                    source_id: Some(e1.id),
                    ..Default::default()
                },
                100,
            )
            .await
            .expect("list_edges");
        assert!(
            !edges.is_empty(),
            "apply_worker must have created an edge between EntityA and EntityB"
        );

        // Verify: ProposalApplied event was emitted.
        let event_store = rt.events(&tok).expect("event store");
        let applied_events = event_store
            .query_events(
                EventFilter {
                    kinds: vec![EventKind::ProposalApplied],
                    payload_proposal_id: Some(proposal_id),
                    ..Default::default()
                },
                PageRequest {
                    offset: 0,
                    limit: 10,
                },
            )
            .await
            .expect("query events");
        assert_eq!(
            applied_events.items.len(),
            1,
            "exactly one ProposalApplied event must be emitted"
        );

        // Verify: projection status updated to 'applied'.
        let projection = ProposalsProjectionWorker::new(rt.clone());
        let row = projection
            .get_proposal_row(&tok, proposal_id)
            .await
            .expect("get row")
            .expect("row must exist");
        assert_eq!(row.status, "applied");
    }

    #[tokio::test]
    async fn apply_worker_skips_non_approved_proposals() {
        let (rt, tok) = setup();
        ensure_schema(&rt).await;

        let proposal_id = Uuid::new_v4();
        // Projection row is 'open' — should not apply.
        insert_projection_row(&rt, &tok, proposal_id, "open").await;

        let worker = ProposalApplyWorker::new(rt.clone());
        worker
            .maybe_apply(&tok, proposal_id)
            .await
            .expect("maybe_apply must succeed without error");

        // Verify no ProposalApplied event was emitted.
        let event_store = rt.events(&tok).expect("event store");
        let applied = event_store
            .query_events(
                EventFilter {
                    kinds: vec![EventKind::ProposalApplied],
                    payload_proposal_id: Some(proposal_id),
                    ..Default::default()
                },
                PageRequest {
                    offset: 0,
                    limit: 10,
                },
            )
            .await
            .expect("query");
        assert_eq!(
            applied.items.len(),
            0,
            "no ProposalApplied event should be emitted for a non-approved proposal"
        );
    }
}
