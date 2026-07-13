//! ProposalApplyWorker implementation.

use uuid::Uuid;

use khive_runtime::{
    atomic_prepare::{
        apply_post_commit_effects, prepare_add_entity, prepare_add_note, prepare_op,
        prepare_update_entity_plan,
    },
    curation::{ContentMergeStrategy, EntityDedupMergePolicy, EntityPatch},
    AtomicOpPlan, AtomicRunOutcome, EdgeListFilter, KhiveRuntime, NamespaceToken, RuntimeError,
    VerbRegistry,
};
use khive_storage::types::PageRequest;
use khive_storage::{EdgeRelation, EventFilter};
use khive_types::{
    ApplyResult, EventKind, Id128, ProposalAppliedPayload, ProposalChangeset,
    ProposalCreatedPayload, Timestamp,
};

use super::budget::{count_new_entries, has_multi_step_compound, WriteBudget};
use crate::projection_worker::ProposalsProjectionWorker;

pub(super) enum PreparedApply {
    Atomic {
        plans: Vec<AtomicOpPlan>,
        created_records: Vec<PendingCreatedRecord>,
    },
    CanonicalMerge {
        into_id: Uuid,
        from_id: Uuid,
    },
}

pub(super) enum PendingCreatedRecord {
    Id(Uuid),
    Edge {
        source_id: Uuid,
        target_id: Uuid,
        relation: EdgeRelation,
    },
}

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

        if has_multi_step_compound(&changeset) {
            self.emit_apply_failed(
                token,
                proposal_id,
                "multi-step Compound proposals are not supported until atomic proposal apply is available"
                    .to_string(),
                0,
            )
            .await;
            return Ok(());
        }

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

    async fn apply_changeset(
        &self,
        token: &NamespaceToken,
        changeset: &ProposalChangeset,
        registry: &VerbRegistry,
        budget: &mut WriteBudget,
    ) -> Result<Vec<Uuid>, RuntimeError> {
        match self
            .prepare_changeset(token, changeset, registry, budget)
            .await?
        {
            PreparedApply::Atomic {
                plans,
                created_records,
            } => {
                let outcome = khive_runtime::run_atomic_unit(self.runtime.sql().as_ref(), plans)
                    .await
                    .map_err(|error| RuntimeError::Storage(error.0))?;
                match outcome {
                    AtomicRunOutcome::Committed { post_commit } => {
                        apply_post_commit_effects(&self.runtime, token, post_commit).await?;
                        self.resolve_created_records(token, created_records).await
                    }
                    AtomicRunOutcome::RolledBack {
                        failed_op_index,
                        failure,
                    } => Err(RuntimeError::Internal(format!(
                        "atomic proposal apply rolled back at plan {failed_op_index}: {failure:?}"
                    ))),
                }
            }
            PreparedApply::CanonicalMerge { into_id, from_id } => {
                self.runtime
                    .merge_entity(
                        token,
                        into_id,
                        from_id,
                        EntityDedupMergePolicy::PreferInto,
                        ContentMergeStrategy::Append,
                        false,
                        None,
                    )
                    .await?;
                Ok(vec![])
            }
        }
    }

    pub(super) fn prepare_changeset<'a>(
        &'a self,
        token: &'a NamespaceToken,
        changeset: &'a ProposalChangeset,
        registry: &'a VerbRegistry,
        budget: &'a mut WriteBudget,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<PreparedApply, RuntimeError>> + Send + 'a>,
    > {
        Box::pin(async move {
            match changeset {
                ProposalChangeset::AddEntity { entity } => {
                    let kind =
                        crate::handlers::canonical_entity_kind(entity.kind.as_str(), registry)?;
                    budget.consume_new_entry()?;
                    let args = serde_json::json!({
                        "kind": kind,
                        "name": entity.name,
                        "description": entity.description,
                        "properties": entity.properties,
                        "tags": entity.tags,
                    });
                    let plan = prepare_add_entity(&self.runtime, token, &args).await?;
                    let entity_id = match &plan {
                        AtomicOpPlan::AddEntity(plan) => plan.entity_id,
                        _ => unreachable!("prepare_add_entity returned a different plan variant"),
                    };
                    Ok(PreparedApply::Atomic {
                        plans: vec![plan],
                        created_records: vec![PendingCreatedRecord::Id(entity_id)],
                    })
                }
                ProposalChangeset::UpdateEntity { id, patch } => {
                    let entity_id = Uuid::from_u128(id.to_u128());
                    let plan = prepare_update_entity_plan(
                        &self.runtime,
                        token,
                        entity_id,
                        EntityPatch {
                            name: patch.name.clone(),
                            description: patch.description.clone(),
                            properties: patch.properties.clone(),
                            tags: patch.tags.clone(),
                        },
                    )
                    .await?;
                    Ok(PreparedApply::Atomic {
                        plans: vec![plan],
                        created_records: vec![],
                    })
                }
                ProposalChangeset::AddEdge {
                    source,
                    target,
                    relation,
                    weight,
                } => {
                    let source_id = Uuid::from_u128(source.to_u128());
                    let target_id = Uuid::from_u128(target.to_u128());
                    let relation = crate::handlers::parse_relation(relation.as_str())?;
                    let args = serde_json::json!({
                        "source_id": source_id,
                        "target_id": target_id,
                        "relation": relation.as_str(),
                        "weight": weight.map(f64::from),
                    });
                    let plan = prepare_op(&self.runtime, token, "link", &args).await?;
                    let (source_id, target_id) = match &plan {
                        AtomicOpPlan::Link(plan) => (plan.source_id, plan.target_id),
                        _ => unreachable!("link prepare returned a different plan variant"),
                    };
                    Ok(PreparedApply::Atomic {
                        plans: vec![plan],
                        created_records: vec![PendingCreatedRecord::Edge {
                            source_id,
                            target_id,
                            relation,
                        }],
                    })
                }
                ProposalChangeset::AddNote { note } => {
                    let kind =
                        crate::handlers::canonical_note_kind(note.kind.as_str(), registry)?;
                    budget.consume_new_entry()?;
                    let args = serde_json::json!({
                        "kind": kind,
                        "name": note.name,
                        "content": note.content,
                        "properties": note.properties,
                    });
                    let plan = prepare_add_note(&self.runtime, token, &args).await?;
                    let note_id = match &plan {
                        AtomicOpPlan::AddNote(plan) => plan.note_id,
                        _ => unreachable!("prepare_add_note returned a different plan variant"),
                    };
                    Ok(PreparedApply::Atomic {
                        plans: vec![plan],
                        created_records: vec![PendingCreatedRecord::Id(note_id)],
                    })
                }
                ProposalChangeset::MergeEntities { into, from } => {
                    Ok(PreparedApply::CanonicalMerge {
                        into_id: Uuid::from_u128(into.to_u128()),
                        from_id: Uuid::from_u128(from.to_u128()),
                    })
                }
                ProposalChangeset::SupersedeEntity { old, new } => {
                    let old_id = Uuid::from_u128(old.to_u128());
                    let new_id = Uuid::from_u128(new.to_u128());
                    let args = serde_json::json!({
                        "source_id": new_id,
                        "target_id": old_id,
                        "relation": EdgeRelation::Supersedes.as_str(),
                    });
                    let plan = prepare_op(&self.runtime, token, "link", &args).await?;
                    Ok(PreparedApply::Atomic {
                        plans: vec![plan],
                        created_records: vec![],
                    })
                }
                ProposalChangeset::Compound { steps } => match steps.as_slice() {
                    [] => Ok(PreparedApply::Atomic {
                        plans: vec![],
                        created_records: vec![],
                    }),
                    [step] => self.prepare_changeset(token, step, registry, budget).await,
                    _ => Err(RuntimeError::InvalidInput(
                        "multi-step Compound proposals are not supported until atomic proposal apply is available"
                            .to_string(),
                    )),
                },
            }
        })
    }

    async fn resolve_created_records(
        &self,
        token: &NamespaceToken,
        records: Vec<PendingCreatedRecord>,
    ) -> Result<Vec<Uuid>, RuntimeError> {
        let mut ids = Vec::with_capacity(records.len());
        for record in records {
            match record {
                PendingCreatedRecord::Id(id) => ids.push(id),
                PendingCreatedRecord::Edge {
                    source_id,
                    target_id,
                    relation,
                } => {
                    let edge = self
                        .runtime
                        .list_edges(
                            token,
                            EdgeListFilter {
                                source_id: Some(source_id),
                                target_id: Some(target_id),
                                relations: vec![relation],
                                ..Default::default()
                            },
                            1,
                            0,
                        )
                        .await?
                        .into_iter()
                        .next()
                        .ok_or_else(|| {
                            RuntimeError::Internal(
                                "committed proposal edge was not found by natural key".to_string(),
                            )
                        })?;
                    ids.push(edge.id.0);
                }
            }
        }
        Ok(ids)
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
