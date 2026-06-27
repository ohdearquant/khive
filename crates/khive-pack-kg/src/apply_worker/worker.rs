//! ProposalApplyWorker implementation.

use std::str::FromStr;

use uuid::Uuid;

use khive_runtime::{
    curation::{EntityDedupMergePolicy, EntityPatch},
    KhiveRuntime, NamespaceToken, RuntimeError, VerbRegistry,
};
use khive_storage::types::PageRequest;
use khive_storage::{EdgeRelation, EventFilter};
use khive_types::{
    ApplyResult, EntityDraft, EntityKind, EventKind, Id128, NoteDraft, ProposalAppliedPayload,
    ProposalChangeset, ProposalCreatedPayload, ProposalEntityPatch, Timestamp,
};

use super::budget::{count_new_entries, WriteBudget};
use crate::projection_worker::ProposalsProjectionWorker;

/// Worker that applies approved proposal changesets.
pub struct ProposalApplyWorker {
    pub(crate) runtime: KhiveRuntime,
    pub(crate) projection: ProposalsProjectionWorker,
}

impl ProposalApplyWorker {
    /// Create a new apply worker backed by the given runtime.
    pub fn new(runtime: KhiveRuntime) -> Self {
        let projection = ProposalsProjectionWorker::new(runtime.clone());
        Self {
            runtime,
            projection,
        }
    }

    /// Check approval threshold; apply changeset if met. Errors emit `ProposalApplied { Failed }`.
    pub async fn maybe_apply(
        &self,
        token: &NamespaceToken,
        proposal_id: Uuid,
        registry: &VerbRegistry,
        max_new_entries: Option<u64>,
    ) -> Result<(), RuntimeError> {
        let row = match self.projection.get_proposal_row(token, proposal_id).await? {
            Some(r) => r,
            None => return Ok(()),
        };

        if row.status != "approved" || row.reject_count > 0 {
            return Ok(());
        }

        let changeset = match self.load_changeset(token, proposal_id).await {
            Ok(cs) => cs,
            Err(e) => {
                self.emit_apply_failed(token, proposal_id, e.to_string(), 0)
                    .await;
                return Ok(());
            }
        };

        if let Some(max) = max_new_entries {
            let needed = count_new_entries(&changeset);
            if needed > max {
                self.emit_apply_failed(
                    token,
                    proposal_id,
                    RuntimeError::WriteBudgetExceeded {
                        max_new_entries: max,
                        attempted_new_entries: max + 1,
                    }
                    .to_string(),
                    0,
                )
                .await;
                return Ok(());
            }
        }

        let claimed = self.projection.pre_apply_cas(token, proposal_id).await?;
        if !claimed {
            tracing::debug!(
                proposal_id = %proposal_id,
                "ProposalApplyWorker: pre-apply CAS missed — proposal already in \
                 non-approved state (withdrawn or applied concurrently); skipping"
            );
            return Ok(());
        }

        let apply_result = self
            .apply_changeset(
                token,
                &changeset,
                registry,
                &mut WriteBudget::new(max_new_entries),
            )
            .await;

        match apply_result {
            Ok(created_records) => {
                let created_ids: Vec<Id128> = created_records
                    .iter()
                    .map(|id| Id128::from_u128(id.as_u128()))
                    .collect();
                self.emit_apply_success(token, proposal_id, created_ids)
                    .await;
                match self
                    .projection
                    .on_proposal_applied(token, proposal_id)
                    .await
                {
                    Ok(true) => {}
                    Ok(false) => {
                        tracing::warn!(
                            proposal_id = %proposal_id,
                            "ProposalApplyWorker: CAS missed on applied projection update — \
                             unexpected; KG mutations committed but status may not reflect 'applied'"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            proposal_id = %proposal_id,
                            error = %e,
                            "ProposalApplyWorker: projection update failed after successful apply (non-fatal)"
                        );
                    }
                }
            }
            Err(e) => {
                self.emit_apply_failed(token, proposal_id, e.to_string(), 0)
                    .await;
                if let Err(e2) = self
                    .projection
                    .revert_applying_to_approved(token, proposal_id)
                    .await
                {
                    tracing::warn!(
                        proposal_id = %proposal_id,
                        error = %e2,
                        "ProposalApplyWorker: failed to revert 'applying' back to 'approved' \
                         after failed apply — proposal may be stuck in 'applying'"
                    );
                }
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

        let payload_str = event.payload.to_string();
        let payload: ProposalCreatedPayload = serde_json::from_str(&payload_str).map_err(|e| {
            RuntimeError::Internal(format!(
                "failed to deserialize ProposalCreated payload: {e}"
            ))
        })?;

        Ok(payload.changeset)
    }

    /// Apply a single changeset arm, or recursively for `Compound`. Box-pinned for recursion.
    fn apply_changeset<'a>(
        &'a self,
        token: &'a NamespaceToken,
        changeset: &'a ProposalChangeset,
        registry: &'a VerbRegistry,
        budget: &'a mut WriteBudget,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Vec<Uuid>, RuntimeError>> + Send + 'a>,
    > {
        Box::pin(async move {
            match changeset {
                ProposalChangeset::AddEntity { entity } => {
                    self.apply_add_entity(token, entity, budget).await
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
                ProposalChangeset::AddNote { note } => {
                    self.apply_add_note(token, note, registry, budget).await
                }
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
                        let created = self.apply_changeset(token, step, registry, budget).await?;
                        all_created.extend(created);
                    }
                    Ok(all_created)
                }
            }
        })
    }

    /// Apply `AddEntity`: create the entity from the structured draft.
    async fn apply_add_entity(
        &self,
        token: &NamespaceToken,
        draft: &EntityDraft,
        budget: &mut WriteBudget,
    ) -> Result<Vec<Uuid>, RuntimeError> {
        let kind = draft.kind.as_str();
        EntityKind::from_str(kind).map_err(|_| {
            let valid: Vec<&str> = EntityKind::ALL.iter().map(|k| k.name()).collect();
            RuntimeError::InvalidInput(format!(
                "AddEntity: unknown entity_kind {kind:?}; valid: {}",
                valid.join(" | ")
            ))
        })?;
        budget.consume_new_entry()?;
        let entity = self
            .runtime
            .create_entity(
                token,
                kind,
                None,
                draft.name.as_str(),
                draft.description.as_deref(),
                draft.properties.clone(),
                draft.tags.clone(),
            )
            .await?;
        Ok(vec![entity.id])
    }

    /// Apply `UpdateEntity`: apply the structured patch to the entity.
    async fn apply_update_entity(
        &self,
        token: &NamespaceToken,
        id: Id128,
        proposal_patch: &ProposalEntityPatch,
    ) -> Result<(), RuntimeError> {
        let entity_id = Uuid::from_u128(id.to_u128());
        let patch = EntityPatch {
            name: proposal_patch.name.clone(),
            description: proposal_patch.description.clone(),
            properties: proposal_patch.properties.clone(),
            tags: proposal_patch.tags.clone(),
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
        let storage_relation = crate::handlers::parse_relation(relation.as_str())?;
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
        Ok(edge.id.0)
    }

    /// Apply `AddNote`: create the note from the structured draft.
    async fn apply_add_note(
        &self,
        token: &NamespaceToken,
        draft: &NoteDraft,
        registry: &VerbRegistry,
        budget: &mut WriteBudget,
    ) -> Result<Vec<Uuid>, RuntimeError> {
        let kind = draft.kind.as_str();
        let canonical_kind = crate::handlers::canonical_note_kind(kind, registry)?;
        budget.consume_new_entry()?;
        let note = self
            .runtime
            .create_note(
                token,
                &canonical_kind,
                draft.name.as_deref(),
                draft.content.as_str(),
                None,
                draft.properties.clone(),
                vec![],
            )
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
