//! ProposalApplyWorker -- applies approved proposal changesets to the KG.
//!
//! Called from `handle_review` when the approval threshold is met. Reads the
//! changeset from the event log, dispatches each arm to the runtime API, emits
//! `ProposalApplied`, and updates projection status.

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

use crate::projection_worker::ProposalsProjectionWorker;

// ---- WriteBudget ----

/// Per-apply write budget. Tracks how many new entity/note rows may still be
/// created in this apply run. `None` means unlimited.
///
/// `Compound` passes the same `&mut WriteBudget` to every nested step so
/// consumption is cumulative across the whole changeset tree.
#[derive(Debug, Clone, Copy)]
struct WriteBudget {
    max_new_entries: Option<u64>,
    consumed_new_entries: u64,
}

impl WriteBudget {
    fn new(max_new_entries: Option<u64>) -> Self {
        Self {
            max_new_entries,
            consumed_new_entries: 0,
        }
    }

    /// Attempt to consume one entry from the budget.
    ///
    /// Returns `RuntimeError::WriteBudgetExceeded` if adding one more entry
    /// would exceed `max_new_entries`. `None` budget always succeeds.
    fn consume_new_entry(&mut self) -> Result<(), RuntimeError> {
        if let Some(max) = self.max_new_entries {
            let next = self.consumed_new_entries + 1;
            if next > max {
                return Err(RuntimeError::WriteBudgetExceeded {
                    max_new_entries: max,
                    attempted_new_entries: next,
                });
            }
            self.consumed_new_entries = next;
        }
        Ok(())
    }
}

/// Count the total number of `AddEntity` + `AddNote` steps in a changeset tree.
///
/// Used for the pre-flight budget check in `maybe_apply` to guarantee zero rows
/// are written when the budget would be exceeded (all-or-nothing guarantee).
fn count_new_entries(changeset: &ProposalChangeset) -> u64 {
    match changeset {
        ProposalChangeset::AddEntity { .. } => 1,
        ProposalChangeset::AddNote { .. } => 1,
        ProposalChangeset::Compound { steps } => steps.iter().map(count_new_entries).sum(),
        _ => 0,
    }
}

// ---- ProposalApplyWorker ----

/// Worker that applies approved proposal changesets.
pub struct ProposalApplyWorker {
    runtime: KhiveRuntime,
    projection: ProposalsProjectionWorker,
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

    /// Called from `handle_review` after emitting a `ProposalReviewed` event.
    ///
    /// Checks whether the proposal should now be applied (threshold met, not already
    /// applied/withdrawn). If yes, applies the changeset and emits `ProposalApplied`.
    ///
    /// `max_new_entries`: caller-supplied write budget. Cloud passes remaining headroom
    /// so the OSS apply worker enforces the cap without learning tenant plan details.
    /// `None` means unlimited — standalone khive default, zero behaviour change.
    ///
    /// Returns `Ok(())` in all cases — errors are emitted as `ProposalApplied { Failed }`
    /// events, not propagated to the reviewer.
    pub async fn maybe_apply(
        &self,
        token: &NamespaceToken,
        proposal_id: Uuid,
        registry: &VerbRegistry,
        max_new_entries: Option<u64>,
    ) -> Result<(), RuntimeError> {
        // Load current projection row.
        let row = match self.projection.get_proposal_row(token, proposal_id).await? {
            Some(r) => r,
            None => return Ok(()), // No row — proposal doesn't exist, nothing to do.
        };

        // Only apply when: status='approved', no rejects.
        // v1 approval threshold = 1 approve, no recorded reject.
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

        // Pre-flight write-budget check (before CAS so no revert is needed on rejection).
        //
        // Count AddEntity + AddNote ops recursively. If the total exceeds the caller-
        // supplied budget, emit ProposalApplied{Failed} and return — no KG writes occur,
        // no CAS transition happens, and status remains 'approved' for future retries.
        // This guarantees zero entity/note rows are written when the budget is exceeded
        // (all-or-nothing contract).
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

        // H1 fix (apply/withdraw race — pre-apply CAS):
        //
        // Atomically transition status='approved' → 'applying' before touching the KG.
        // This closes the race window between the first status check above and the KG
        // mutation below.  If withdraw lands between those two points, it will find
        // status='applying' and its own CAS (on_proposal_withdrawn) will return false —
        // the withdrawal is rejected with an error.  Only this worker can transition
        // out of 'applying' (to 'applied'), so the KG mutation is now exclusively owned.
        //
        // If the CAS fails here it means: (a) a concurrent withdraw already moved to
        // 'withdrawn', (b) another apply worker won the race (shouldn't happen in v1's
        // synchronous call-from-review model), or (c) the status changed for another
        // reason.  In all cases we abort without any KG mutation.
        let claimed = self.projection.pre_apply_cas(token, proposal_id).await?;
        if !claimed {
            tracing::debug!(
                proposal_id = %proposal_id,
                "ProposalApplyWorker: pre-apply CAS missed — proposal already in \
                 non-approved state (withdrawn or applied concurrently); skipping"
            );
            return Ok(());
        }

        // Apply the changeset — we exclusively own the 'applying' state now.
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
                // Update projection: status='applying' → 'applied'.
                // on_proposal_applied uses CAS WHERE status='applying'; since only this
                // worker can exit 'applying', this must succeed — but we log if it doesn't.
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
                // Failed applies leave status='applying' — revert to 'approved'
                // so the proposal is not stuck.  Best-effort; log on failure.
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
    /// `budget` tracks remaining write capacity. `AddEntity` and `AddNote` each consume
    /// one entry before the runtime create call; `Compound` passes the same budget
    /// through each nested step so consumption is cumulative across the whole tree.
    /// The pre-flight check in `maybe_apply` ensures the budget is never actually
    /// exhausted here; the inline checks are defense-in-depth.
    ///
    /// The function is `Box::pin`-wrapped to support recursion (Compound steps).
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

        // C2: Validate kind against the closed entity-kind taxonomy.
        // Direct `create` rejects invalid kinds; the apply worker must enforce the
        // same invariant so proposals cannot bypass taxonomy validation.
        EntityKind::from_str(kind).map_err(|_| {
            let valid: Vec<&str> = EntityKind::ALL.iter().map(|k| k.name()).collect();
            RuntimeError::InvalidInput(format!(
                "AddEntity: unknown entity_kind {kind:?}; valid: {}",
                valid.join(" | ")
            ))
        })?;

        // Consume one budget entry before the runtime write (defense-in-depth;
        // pre-flight in maybe_apply already validated the total).
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

    /// Apply `AddNote`: create the note from the structured draft.
    ///
    /// C3: Validate note kind against all loaded pack vocabularies before
    /// creating the note.  The normal `create` path runs this validation via
    /// `canonical_note_kind`; the apply worker must enforce the same invariant
    /// so proposals cannot bypass pack note-kind validation (Issue #478).
    async fn apply_add_note(
        &self,
        token: &NamespaceToken,
        draft: &NoteDraft,
        registry: &VerbRegistry,
        budget: &mut WriteBudget,
    ) -> Result<Vec<Uuid>, RuntimeError> {
        let kind = draft.kind.as_str();
        // Validate note kind against registry (base kg kinds + all loaded pack kinds).
        let canonical_kind = crate::handlers::canonical_note_kind(kind, registry)?;

        // Consume one budget entry before the runtime write (defense-in-depth;
        // pre-flight in maybe_apply already validated the total).
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

#[cfg(test)]
mod tests;
