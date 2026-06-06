// FILE SIZE JUSTIFICATION: apply_worker.rs contains the full proposal application pipeline
// (load changeset, dispatch each arm, emit ProposalApplied, update projection). Each arm
// (create/update/delete/link) has non-trivial validation and runtime call sequences. Splitting
// into sub-modules would fragment the sequential flow that must be read and reasoned about as a
// unit. The inline test section requires access to private helpers and internal state.

//! ProposalApplyWorker — applies approved proposal changesets to the KG.
//!
//! Called from `handle_review` when the approval threshold is met. Reads the
//! changeset from the event log, dispatches each arm to the runtime API, emits
//! `ProposalApplied`, and updates projection status. All steps run atomically;
//! any failure emits `ProposalApplied { Failed }` without touching the projection.

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

// ---- Tests ------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use khive_runtime::{KhiveRuntime, Namespace, VerbRegistryBuilder};
    use khive_storage::types::{PageRequest, SqlStatement, SqlValue};
    use khive_types::{Id128, NoteDraft, ProposalChangeset, ProposalCreatedPayload};
    use uuid::Uuid;

    fn setup() -> (KhiveRuntime, NamespaceToken) {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let tok = rt.authorize(Namespace::local()).unwrap();
        (rt, tok)
    }

    fn build_registry(rt: &KhiveRuntime) -> VerbRegistry {
        let mut builder = VerbRegistryBuilder::new();
        builder.register(crate::KgPack::new(rt.clone()));
        builder.build().expect("registry build")
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

        let registry = build_registry(&rt);
        let worker = ProposalApplyWorker::new(rt.clone());
        worker
            .maybe_apply(&tok, proposal_id, &registry, None)
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

        let registry = build_registry(&rt);
        let worker = ProposalApplyWorker::new(rt.clone());
        worker
            .maybe_apply(&tok, proposal_id, &registry, None)
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

    /// C2 regression: apply worker must reject proposals whose AddEntity changeset
    /// carries an invalid entity kind.  Direct `create(kind="invalidkind")` correctly
    /// errors; the proposal apply path must enforce the same invariant.
    #[tokio::test]
    async fn apply_worker_rejects_invalid_entity_kind() {
        let (rt, tok) = setup();
        ensure_schema(&rt).await;

        let proposal_id = Uuid::new_v4();
        // Changeset references an invalid entity kind (not in the closed taxonomy).
        let changeset = ProposalChangeset::AddEntity {
            entity: EntityDraft {
                kind: "invalidkind".to_string(),
                name: "BadEntity".to_string(),
                description: Some("should fail".to_string()),
                properties: None,
                tags: vec![],
            },
        };

        seed_proposal_created_event(&rt, &tok, proposal_id, changeset).await;
        insert_projection_row(&rt, &tok, proposal_id, "approved").await;

        let registry = build_registry(&rt);
        let worker = ProposalApplyWorker::new(rt.clone());
        worker
            .maybe_apply(&tok, proposal_id, &registry, None)
            .await
            .expect("maybe_apply itself must succeed (errors emitted as ProposalApplied{Failed})");

        // The apply must have emitted a ProposalApplied{Failed} event, not success.
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
            "ProposalApplied event must be emitted"
        );

        // Verify no entity with that name was created.
        let entities = rt
            .list_entities(&tok, None, None, 100, 0)
            .await
            .expect("list_entities");
        assert!(
            !entities.iter().any(|e| e.name == "BadEntity"),
            "entity with invalid kind must not be created in the KG"
        );
    }

    /// H2 regression: apply worker must NOT mutate the KG when the proposal was
    /// withdrawn after approval but before the worker runs.
    ///
    /// Sequence: approve (status='approved') → withdraw (status='withdrawn') →
    /// maybe_apply() → assert no entity created, no ProposalApplied event emitted.
    #[tokio::test]
    async fn apply_worker_skips_kg_mutation_when_withdrawn_after_approve() {
        let (rt, tok) = setup();
        ensure_schema(&rt).await;

        let proposal_id = Uuid::new_v4();
        let changeset = ProposalChangeset::AddEntity {
            entity: EntityDraft {
                kind: "concept".to_string(),
                name: "ShouldNotExist".to_string(),
                description: Some("withdrawn before apply".to_string()),
                properties: None,
                tags: vec![],
            },
        };

        seed_proposal_created_event(&rt, &tok, proposal_id, changeset).await;

        // Start in 'withdrawn' status — simulates: approve → withdraw both landed
        // before the apply worker runs.
        insert_projection_row(&rt, &tok, proposal_id, "withdrawn").await;

        let registry = build_registry(&rt);
        let worker = ProposalApplyWorker::new(rt.clone());
        worker
            .maybe_apply(&tok, proposal_id, &registry, None)
            .await
            .expect("maybe_apply must succeed without error");

        // Assert: no ProposalApplied event was emitted (worker bailed out early).
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
            .expect("query applied events");
        assert_eq!(
            applied_events.items.len(),
            0,
            "H2: no ProposalApplied event must be emitted when proposal is withdrawn"
        );

        // Assert: no entity was created in the KG.
        let entities = rt
            .list_entities(&tok, None, None, 100, 0)
            .await
            .expect("list_entities");
        assert!(
            !entities.iter().any(|e| e.name == "ShouldNotExist"),
            "H2: KG must not be mutated when proposal was withdrawn before apply"
        );
    }

    /// C3 regression: apply worker must reject proposals whose AddNote changeset
    /// carries an invalid note kind.  Direct `create(kind="badnote")` correctly
    /// errors; the proposal apply path must enforce the same invariant so that
    /// pack-owned note kinds (memory, task, message, scheduled_event) cannot be
    /// bypassed via the proposal mechanism when their owning pack is not loaded.
    #[tokio::test]
    async fn apply_worker_rejects_invalid_note_kind() {
        let (rt, tok) = setup();
        ensure_schema(&rt).await;

        let proposal_id = Uuid::new_v4();
        let changeset = ProposalChangeset::AddNote {
            note: NoteDraft {
                kind: "invalidnotekind".to_string(),
                name: Some("BadNote".to_string()),
                content: "should fail".to_string(),
                properties: None,
            },
        };

        seed_proposal_created_event(&rt, &tok, proposal_id, changeset).await;
        insert_projection_row(&rt, &tok, proposal_id, "approved").await;

        let registry = build_registry(&rt);
        let worker = ProposalApplyWorker::new(rt.clone());
        worker
            .maybe_apply(&tok, proposal_id, &registry, None)
            .await
            .expect("maybe_apply itself must succeed (errors emitted as ProposalApplied{Failed})");

        // The apply must have emitted a ProposalApplied{Failed} event, not success.
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
            "C3: ProposalApplied event must be emitted"
        );

        // Verify no note with that name was created.
        let notes = rt
            .notes(&tok)
            .expect("notes store")
            .query_notes(
                tok.namespace().as_str(),
                None,
                PageRequest {
                    offset: 0,
                    limit: 100,
                },
            )
            .await
            .expect("query_notes");
        assert!(
            !notes
                .items
                .iter()
                .any(|n| n.name.as_deref() == Some("BadNote")),
            "C3: note with invalid kind must not be created in the KG"
        );
    }

    // ---- Write-budget tests ------------------------------------------------

    fn make_entity_draft(name: &str) -> EntityDraft {
        EntityDraft {
            kind: "concept".to_string(),
            name: name.to_string(),
            description: None,
            properties: None,
            tags: vec![],
        }
    }

    /// Over-budget flat Compound: 3 AddEntity steps, budget=Some(2).
    /// Expects: ProposalApplied{Failed} with WriteBudgetExceeded, zero new entities.
    #[tokio::test]
    async fn budget_exceeded_flat_compound_creates_zero_rows() {
        let (rt, tok) = setup();
        ensure_schema(&rt).await;

        let proposal_id = Uuid::new_v4();
        let changeset = ProposalChangeset::Compound {
            steps: vec![
                ProposalChangeset::AddEntity {
                    entity: make_entity_draft("BudgetA"),
                },
                ProposalChangeset::AddEntity {
                    entity: make_entity_draft("BudgetB"),
                },
                ProposalChangeset::AddEntity {
                    entity: make_entity_draft("BudgetC"),
                },
            ],
        };

        seed_proposal_created_event(&rt, &tok, proposal_id, changeset).await;
        insert_projection_row(&rt, &tok, proposal_id, "approved").await;

        let entities_before = rt
            .list_entities(&tok, None, None, 100, 0)
            .await
            .expect("list_entities");

        let registry = build_registry(&rt);
        let worker = ProposalApplyWorker::new(rt.clone());
        worker
            .maybe_apply(&tok, proposal_id, &registry, Some(2))
            .await
            .expect("maybe_apply must succeed (budget error emitted as ProposalApplied{Failed})");

        // Verify: ProposalApplied{Failed} was emitted.
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
            "budget: ProposalApplied event must be emitted on over-budget"
        );
        let payload_str = applied_events.items[0].payload.to_string();
        assert!(
            payload_str.contains("WriteBudgetExceeded")
                || payload_str.contains("write budget exceeded"),
            "budget: failure payload must mention WriteBudgetExceeded; got: {payload_str}"
        );

        // Verify: zero new entity rows (all-or-nothing guarantee).
        let entities_after = rt
            .list_entities(&tok, None, None, 100, 0)
            .await
            .expect("list_entities");
        assert_eq!(
            entities_before.len(),
            entities_after.len(),
            "budget: over-budget apply must create zero new entity rows"
        );
    }

    /// In-budget flat Compound: 2 AddEntity steps, budget=Some(2).
    /// Expects: ProposalApplied{Success}, two new entities.
    #[tokio::test]
    async fn budget_in_budget_flat_compound_applies_fully() {
        let (rt, tok) = setup();
        ensure_schema(&rt).await;

        let proposal_id = Uuid::new_v4();
        let changeset = ProposalChangeset::Compound {
            steps: vec![
                ProposalChangeset::AddEntity {
                    entity: make_entity_draft("InBudgetA"),
                },
                ProposalChangeset::AddEntity {
                    entity: make_entity_draft("InBudgetB"),
                },
            ],
        };

        seed_proposal_created_event(&rt, &tok, proposal_id, changeset).await;
        insert_projection_row(&rt, &tok, proposal_id, "approved").await;

        let registry = build_registry(&rt);
        let worker = ProposalApplyWorker::new(rt.clone());
        worker
            .maybe_apply(&tok, proposal_id, &registry, Some(2))
            .await
            .expect("maybe_apply must succeed");

        let entities = rt
            .list_entities(&tok, None, None, 100, 0)
            .await
            .expect("list_entities");
        assert!(
            entities.iter().any(|e| e.name == "InBudgetA"),
            "budget: InBudgetA must be created"
        );
        assert!(
            entities.iter().any(|e| e.name == "InBudgetB"),
            "budget: InBudgetB must be created"
        );
    }

    /// Nested Compound: outer has 1 AddEntity + inner Compound with 2 AddEntity.
    /// Total = 3. budget=Some(2) → fail before any write.
    #[tokio::test]
    async fn budget_nested_compound_counts_recursively() {
        let (rt, tok) = setup();
        ensure_schema(&rt).await;

        let proposal_id = Uuid::new_v4();
        let changeset = ProposalChangeset::Compound {
            steps: vec![
                ProposalChangeset::AddEntity {
                    entity: make_entity_draft("NestedOuter"),
                },
                ProposalChangeset::Compound {
                    steps: vec![
                        ProposalChangeset::AddEntity {
                            entity: make_entity_draft("NestedInnerA"),
                        },
                        ProposalChangeset::AddEntity {
                            entity: make_entity_draft("NestedInnerB"),
                        },
                    ],
                },
            ],
        };

        seed_proposal_created_event(&rt, &tok, proposal_id, changeset).await;
        insert_projection_row(&rt, &tok, proposal_id, "approved").await;

        let entities_before = rt
            .list_entities(&tok, None, None, 100, 0)
            .await
            .expect("list_entities");

        let registry = build_registry(&rt);
        let worker = ProposalApplyWorker::new(rt.clone());
        worker
            .maybe_apply(&tok, proposal_id, &registry, Some(2))
            .await
            .expect("maybe_apply must succeed (error as ProposalApplied{Failed})");

        let entities_after = rt
            .list_entities(&tok, None, None, 100, 0)
            .await
            .expect("list_entities");
        assert_eq!(
            entities_before.len(),
            entities_after.len(),
            "budget: nested over-budget must create zero rows"
        );
    }

    /// Some(0) budget: AddEdge-only changeset still applies; no new entity rows needed.
    #[tokio::test]
    async fn budget_some_zero_allows_edge_only_changeset() {
        let (rt, tok) = setup();
        ensure_schema(&rt).await;

        // Pre-create two entities outside the proposal.
        let e1 = rt
            .create_entity(&tok, "concept", None, "EdgeSrc", None, None, vec![])
            .await
            .expect("create e1");
        let e2 = rt
            .create_entity(&tok, "concept", None, "EdgeDst", None, None, vec![])
            .await
            .expect("create e2");

        let proposal_id = Uuid::new_v4();
        let changeset = ProposalChangeset::AddEdge {
            source: Id128::from_u128(e1.id.as_u128()),
            target: Id128::from_u128(e2.id.as_u128()),
            relation: khive_types::EdgeRelation::Extends,
            weight: Some(1.0),
        };

        seed_proposal_created_event(&rt, &tok, proposal_id, changeset).await;
        insert_projection_row(&rt, &tok, proposal_id, "approved").await;

        let registry = build_registry(&rt);
        let worker = ProposalApplyWorker::new(rt.clone());
        worker
            .maybe_apply(&tok, proposal_id, &registry, Some(0))
            .await
            .expect("maybe_apply must succeed");

        // Verify edge was created despite budget=Some(0).
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
            "budget: Some(0) must not block AddEdge-only changeset"
        );
    }

    /// WriteBudget unit test: consume_new_entry() honours the limit.
    #[test]
    fn write_budget_consume_enforces_limit() {
        let mut budget = WriteBudget::new(Some(2));
        assert!(budget.consume_new_entry().is_ok(), "first consume ok");
        assert!(budget.consume_new_entry().is_ok(), "second consume ok");
        let err = budget.consume_new_entry().expect_err("third must fail");
        match err {
            RuntimeError::WriteBudgetExceeded {
                max_new_entries,
                attempted_new_entries,
            } => {
                assert_eq!(max_new_entries, 2);
                assert_eq!(attempted_new_entries, 3);
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    /// WriteBudget unit test: None budget never fails.
    #[test]
    fn write_budget_none_is_unlimited() {
        let mut budget = WriteBudget::new(None);
        for _ in 0..1000 {
            assert!(budget.consume_new_entry().is_ok());
        }
    }

    /// count_new_entries unit test: flat and nested Compound.
    #[test]
    fn count_new_entries_recursive() {
        let flat = ProposalChangeset::Compound {
            steps: vec![
                ProposalChangeset::AddEntity {
                    entity: make_entity_draft("X"),
                },
                ProposalChangeset::AddNote {
                    note: khive_types::NoteDraft {
                        kind: "observation".to_string(),
                        name: None,
                        content: "c".to_string(),
                        properties: None,
                    },
                },
                ProposalChangeset::AddEdge {
                    source: Id128::from_u128(0),
                    target: Id128::from_u128(1),
                    relation: khive_types::EdgeRelation::Extends,
                    weight: None,
                },
            ],
        };
        assert_eq!(
            count_new_entries(&flat),
            2,
            "AddEntity + AddNote = 2; AddEdge = 0"
        );

        let nested = ProposalChangeset::Compound {
            steps: vec![
                ProposalChangeset::AddEntity {
                    entity: make_entity_draft("Y"),
                },
                ProposalChangeset::Compound {
                    steps: vec![
                        ProposalChangeset::AddEntity {
                            entity: make_entity_draft("Z"),
                        },
                        ProposalChangeset::AddNote {
                            note: khive_types::NoteDraft {
                                kind: "observation".to_string(),
                                name: None,
                                content: "c".to_string(),
                                properties: None,
                            },
                        },
                    ],
                },
            ],
        };
        assert_eq!(count_new_entries(&nested), 3, "1 + 2 nested = 3");
    }
}
