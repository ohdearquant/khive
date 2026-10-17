use chrono::Utc;
use uuid::Uuid;

use khive_db::stores::entity::entity_hard_delete_statement;
use khive_db::stores::event::hard_delete_lineage_warning_statements;
use khive_db::stores::graph::purge_incident_edges_statement;
use khive_db::stores::note::note_hard_delete_statement;
use khive_db::stores::text::insert_document_statements;
use khive_storage::types::{DeleteMode, SqlStatement, SqlValue};
use khive_storage::{AttachmentSubstrate, Edge, Entity};
use khive_types::{EventKind, SubstrateKind};

use crate::atomic_plan::{
    AffectedRowGuard, DeletePlan, PlanStatement, PostCommitEffect, UpdatePlan,
};
use crate::atomic_runner::{run_atomic_unit, AtomicOpFailure, AtomicOpPlan, AtomicRunOutcome};
use crate::curation::entity_fts_document;
use crate::error::{RuntimeError, RuntimeResult};
use crate::runtime::{KhiveRuntime, NamespaceToken};

#[cfg(any(test, feature = "fault-injection"))]
use super::{consume_fault, FTS_FAIL_NS, ROLLBACK_CLEANUP_FAIL_NS};
use super::{
    legacy_post_commit_result, live_merged_entity_refused, merge_tombstone_restore_refused,
    record_post_commit_degradation, restore_reindex_failed, PostCommitDegradation,
};

impl KhiveRuntime {
    /// Hard-delete a single graph node (entity, note, or edge-as-node row) AND purge its
    /// incident edges in ONE write transaction — closes a race where a concurrent guarded
    /// write could insert a fresh edge against the endpoint between two separately
    /// committed calls; see docs/operations.md#atomic_hard_delete_with_edge_purge.
    ///
    /// `row_statement` is the exact hard-delete `DELETE` for the target row
    /// (entity, note, or edge). Before the incident-edge purge, the same plan
    /// appends any ADR-002 lineage-loss warnings from the still-present edge
    /// rows, so the warning payload and cascade commit or roll back together.
    /// Returns `Ok(true)` if the row was deleted, `Ok(false)` if it no longer
    /// existed (lost a race with a concurrent delete of the same row).
    pub(super) async fn atomic_hard_delete_with_edge_purge(
        &self,
        row_statement: SqlStatement,
        node_id: Uuid,
        namespace: &str,
        actor: &str,
        substrate: SubstrateKind,
    ) -> RuntimeResult<bool> {
        let mut statements = vec![PlanStatement {
            statement: row_statement,
            guard: Some(AffectedRowGuard::exactly(1)),
        }];
        if matches!(substrate, SubstrateKind::Entity | SubstrateKind::Note) {
            statements.push(PlanStatement {
                statement: khive_db::stores::attachment::delete_record_attachments_statement(
                    node_id,
                    if substrate == SubstrateKind::Entity {
                        AttachmentSubstrate::Entity
                    } else {
                        AttachmentSubstrate::Note
                    },
                ),
                guard: None,
            });
        }
        statements.extend(
            hard_delete_lineage_warning_statements(namespace, actor, node_id, substrate)
                .into_iter()
                .map(|statement| PlanStatement {
                    statement,
                    guard: None,
                }),
        );
        statements.push(PlanStatement {
            statement: purge_incident_edges_statement(node_id),
            guard: None,
        });
        let plan = AtomicOpPlan::Delete(DeletePlan {
            target_id: node_id,
            statements,
            post_commit: PostCommitEffect::None,
        });
        match run_atomic_unit(self.sql().as_ref(), vec![plan]).await {
            Ok(AtomicRunOutcome::Committed { .. }) => Ok(true),
            Ok(AtomicRunOutcome::RolledBack {
                failure: AtomicOpFailure::NoteConflict(conflict),
                ..
            }) => Err(conflict.into_error().into()),
            Ok(AtomicRunOutcome::RolledBack {
                failure: AtomicOpFailure::EntityConflict(conflict),
                ..
            }) => Err(conflict.into_error().into()),
            Ok(AtomicRunOutcome::RolledBack {
                failure: AtomicOpFailure::GuardFailed { .. },
                ..
            }) => Ok(false),
            Ok(AtomicRunOutcome::RolledBack {
                failure: AtomicOpFailure::SqlError { message, .. },
                ..
            }) => Err(RuntimeError::Internal(format!(
                "hard delete + edge purge for {node_id} failed: {message}"
            ))),
            // Only canonical change-set plans compare preimages or owner snapshots.
            Ok(AtomicRunOutcome::RolledBack {
                failure:
                    failure @ (AtomicOpFailure::PreimageMismatch { .. }
                    | AtomicOpFailure::OwnerSnapshotConflict { .. }),
                ..
            }) => Err(RuntimeError::Internal(format!(
                "hard delete + edge purge for {node_id}: unexpected canonical failure {failure:?}"
            ))),
            Err(e) => Err(RuntimeError::Internal(format!(
                "hard delete + edge purge for {node_id}: atomic unit seam failure: {}",
                e.0
            ))),
        }
    }

    /// Restore an entity tombstone owned by the caller's primary namespace.
    ///
    /// The restore is guarded by both the tombstone's id/namespace and the
    /// current uniqueness state, so a caller cannot resurrect over a newer
    /// live record. Indexes are rebuilt only after the row restore commits.
    pub async fn restore_entity(
        &self,
        token: &NamespaceToken,
        id: Uuid,
    ) -> RuntimeResult<Option<(Entity, bool)>> {
        let Some(entity) = self
            .entities(token)?
            .get_entity_including_deleted(id)
            .await?
        else {
            return Ok(None);
        };
        if entity.namespace != token.namespace().as_str() {
            return Ok(None);
        }
        // A merge tombstone is not a plain soft delete: the source row carries
        // merge provenance and its content already lives on the kept entity.
        // Clearing only `deleted_at` would bring the source back as a live
        // duplicate that still claims to have been merged. Refuse and name
        // the kept id; restore does not undo a merge.
        //
        // The merge check runs before the already-live short cut on purpose:
        // a live row that still carries `merged_into` is what an earlier
        // restore left behind before this guard existed, and answering it
        // "already live" would hide the invariant violation from the one
        // caller who is looking at the row. Name it instead.
        if let Some(kept_id) = entity.merged_into {
            if entity.deleted_at.is_none() {
                return Err(live_merged_entity_refused(id, kept_id));
            }
            return Err(merge_tombstone_restore_refused(id, kept_id));
        }
        if entity.deleted_at.is_none() {
            return Ok(Some((entity, false)));
        }
        let updated_at =
            Utc::now()
                .timestamp_micros()
                .max(entity.updated_at.checked_add(1).ok_or_else(|| {
                    RuntimeError::Internal(format!(
                        "entity {id} updated_at is already at i64::MAX and cannot advance"
                    ))
                })?);
        let mut restored = entity;
        restored.deleted_at = None;
        restored.updated_at = updated_at;
        restored.version = restored
            .version
            .checked_add(1)
            .ok_or_else(|| RuntimeError::InvalidInput("entity version overflow".into()))?;
        let mut statements = vec![PlanStatement {
            statement: SqlStatement {
                sql: "UPDATE entities SET deleted_at=NULL, updated_at=?1, version=version+1 \
                      WHERE id=?2 AND namespace=?3 AND deleted_at IS NOT NULL AND version=?4"
                    .into(),
                params: vec![
                    SqlValue::Integer(updated_at),
                    SqlValue::Text(id.to_string()),
                    SqlValue::Text(token.namespace().as_str().to_owned()),
                    SqlValue::Integer(restored.version - 1),
                ],
                label: Some("entity-restore".into()),
            },
            guard: Some(AffectedRowGuard::exactly(1)),
        }];
        // The soft delete removed the FTS row, so the text index is published
        // in the same unit as the row: a live row that search cannot find is
        // not a state this verb can leave behind. Order-sensitive pair — see
        // `insert_document_statements`'s adjacency contract.
        for statement in khive_db::stores::text::delete_document_statements(
            "fts_entities",
            &restored.namespace,
            id,
        )
        .into_iter()
        .chain(insert_document_statements(
            "fts_entities",
            &entity_fts_document(&restored),
        )) {
            statements.push(PlanStatement {
                statement,
                guard: None,
            });
        }
        let plan = AtomicOpPlan::Update(Box::new(UpdatePlan {
            graph_effects: Vec::new(),
            target_id: id,
            statements,
            post_commit: PostCommitEffect::None,
            edge_natural_key: None,
            idempotent_noop: false,
            entity_guard: None,
            note_guard: None,
            note_vector_purge: None,
            note_embedding_inheritance: None,
        }));
        match run_atomic_unit(self.sql().as_ref(), vec![plan]).await {
            Ok(AtomicRunOutcome::Committed { .. }) => {
                // Embeddings are rebuilt after the commit; the row and its
                // text index are already live, so a failure here names that.
                #[cfg(any(test, feature = "fault-injection"))]
                if consume_fault(&FTS_FAIL_NS, &restored.namespace) {
                    return Err(restore_reindex_failed(
                        "entity",
                        id,
                        RuntimeError::Internal("injected FTS failure".to_string()),
                    ));
                }
                self.reindex_entity(token, &restored)
                    .await
                    .map_err(|e| restore_reindex_failed("entity", id, e))?;
                Ok(Some((restored, true)))
            }
            Ok(AtomicRunOutcome::RolledBack { failure, .. }) => Err(RuntimeError::Internal(
                format!("entity restore rolled back: {failure:?}"),
            )),
            Err(error) => Err(RuntimeError::Storage(error.0)),
        }
    }

    /// Restore an edge tombstone owned by the caller's primary namespace.
    pub async fn restore_edge(
        &self,
        token: &NamespaceToken,
        id: Uuid,
    ) -> RuntimeResult<Option<(Edge, bool)>> {
        let Some(edge) = self.get_edge_including_deleted(token, id).await? else {
            return Ok(None);
        };
        if edge.namespace != token.namespace().as_str() {
            return Ok(None);
        }
        if edge.deleted_at.is_none() {
            return Ok(Some((edge, false)));
        }
        let updated_at = Utc::now();
        let plan = AtomicOpPlan::Update(Box::new(UpdatePlan {
            graph_effects: Vec::new(),
            target_id: id,
            statements: vec![PlanStatement {
                statement: SqlStatement {
                    sql: "UPDATE graph_edges SET deleted_at=NULL, updated_at=?1 \
                          WHERE id=?2 AND namespace=?3 AND deleted_at IS NOT NULL"
                        .into(),
                    params: vec![
                        SqlValue::Integer(updated_at.timestamp_micros()),
                        SqlValue::Text(id.to_string()),
                        SqlValue::Text(edge.namespace.clone()),
                    ],
                    label: Some("edge-restore".into()),
                },
                guard: Some(AffectedRowGuard::exactly(1)),
            }],
            post_commit: PostCommitEffect::None,
            edge_natural_key: None,
            idempotent_noop: false,
            entity_guard: None,
            note_guard: None,
            note_vector_purge: None,
            note_embedding_inheritance: None,
        }));
        match run_atomic_unit(self.sql().as_ref(), vec![plan]).await {
            Ok(AtomicRunOutcome::Committed { .. }) => {
                let mut restored = edge;
                restored.deleted_at = None;
                restored.updated_at = updated_at;
                Ok(Some((restored, true)))
            }
            Ok(AtomicRunOutcome::RolledBack { failure, .. }) => Err(RuntimeError::Internal(
                format!("edge restore rolled back: {failure:?}"),
            )),
            Err(error) => Err(RuntimeError::Storage(error.0)),
        }
    }

    /// Soft-delete or hard-delete a note by ID.
    ///
    /// On hard delete, cascades to remove all incident edges (both inbound and
    /// outbound) and cleans up FTS and vector indexes, preventing dangling
    /// references for `annotates` edges that target this note.
    /// Soft delete also cleans FTS and vector indexes; edges are left in place.
    ///
    /// UUID v4 is globally unique: no namespace filter on by-ID ops.
    /// Cascade and index cleanup target the RECORD's stored namespace, not the caller token's.
    /// Returns `Ok(false)` if the note does not exist.
    pub async fn delete_note(
        &self,
        token: &NamespaceToken,
        id: Uuid,
        hard: bool,
    ) -> RuntimeResult<bool> {
        let (deleted, degradations) = self
            .delete_note_with_post_commit_report(token, id, hard)
            .await?;
        legacy_post_commit_result("delete_note", id, deleted, degradations)
    }

    /// Delete the note and return diagnostics for any failed work after the
    /// row change committed. A degradation is non-retryable: callers must not
    /// repeat a create or delete because an index or telemetry leg failed.
    pub async fn delete_note_with_post_commit_report(
        &self,
        token: &NamespaceToken,
        id: Uuid,
        hard: bool,
    ) -> RuntimeResult<(bool, Vec<PostCommitDegradation>)> {
        let note_store = self.notes(token)?;
        let note = if hard {
            match note_store.get_note_including_deleted(id).await? {
                Some(n) => n,
                None => return Ok((false, Vec::new())),
            }
        } else {
            match note_store.get_note(id).await? {
                Some(n) => n,
                None => return Ok((false, Vec::new())),
            }
        };
        if let Some(error) = self.stream_member_error(&note).await? {
            return Err(error);
        }
        let mode = if hard {
            DeleteMode::Hard
        } else {
            DeleteMode::Soft
        };

        // Route index cleanup through the RECORD's namespace, not the caller's.
        let record_tok = token.with_namespace(
            khive_types::Namespace::parse(&note.namespace)
                .map_err(|e| RuntimeError::Internal(format!("note namespace invalid: {e}")))?,
        );
        let record_ns = note.namespace.clone();
        let actor = format!("{}:{}", token.actor().kind, token.actor().id);

        // On hard delete, the row delete and the incident-edge cascade (including
        // already-soft-deleted edges) run as ONE write transaction: see
        // `atomic_hard_delete_with_edge_purge`. Index cleanup follows the
        // commit; it is best-effort and idempotent, unlike the row/edge pair.
        let deleted = if hard {
            self.atomic_hard_delete_with_edge_purge(
                note_hard_delete_statement(id),
                id,
                &record_ns,
                &actor,
                SubstrateKind::Note,
            )
            .await?
        } else {
            note_store.delete_note(id, mode).await?
        };
        let mut degradations = Vec::new();
        if deleted {
            let fts_result = match self.text_for_notes(&record_tok) {
                Ok(store) => store
                    .delete_document(&record_ns, id)
                    .await
                    .map_err(RuntimeError::from),
                Err(error) => Err(error),
            };
            if let Err(error) = fts_result {
                record_post_commit_degradation(
                    &mut degradations,
                    "delete_note",
                    id,
                    "fts_cleanup",
                    error,
                );
            }
            // Try every model even when FTS or another model failed.
            for model_name in self.registered_embedding_model_names() {
                let vector_result = match self.vectors_for_model(&record_tok, &model_name) {
                    Ok(store) => store.delete(id).await.map_err(RuntimeError::from),
                    Err(error) => Err(error),
                };
                if let Err(error) = vector_result {
                    record_post_commit_degradation(
                        &mut degradations,
                        "delete_note",
                        id,
                        "vector_cleanup",
                        format!("{model_name}: {error}"),
                    );
                }
            }
            let event = khive_storage::event::Event::new(
                record_ns.clone(),
                "delete",
                EventKind::NoteDeleted,
                SubstrateKind::Note,
                "",
            )
            .with_target(id)
            .with_payload(serde_json::json!({"id": id, "namespace": record_ns, "hard": hard}));
            let event_result = match self.events(&record_tok) {
                Ok(store) => store.append_event(event).await.map_err(RuntimeError::from),
                Err(error) => Err(error),
            };
            if let Err(error) = event_result {
                record_post_commit_degradation(
                    &mut degradations,
                    "delete_note",
                    id,
                    "event_append",
                    error,
                );
            }
            // A soft OR hard delete removes the note's vectors/FTS document
            // above: any pack-owned vector-derived cache (e.g.
            // khive-pack-memory's warm ANN index) needs to know the corpus
            // changed, reached via this generic hook so khive-runtime never
            // takes a dependency on khive-pack-memory. No-op when no pack has
            // installed a hook.
            self.fire_note_mutation_hook(&note.kind, id).await;
        }
        Ok((deleted, degradations))
    }

    /// Row-first compensating delete for rolling back a partially-written note
    /// (e.g. `dual_write_message` rollback after a later delivery step fails).
    /// Unlike [`KhiveRuntime::delete_note`], which cleans up graph/FTS/
    /// vector indexes *before* removing the row, this removes the row first so
    /// that a cleanup failure afterward cannot leave the compensated note live.
    ///
    /// Returns `Ok(())` once the row is gone (whether or not cleanup fully
    /// succeeded). Returns `Err(RuntimeError::Internal)` naming the failed
    /// cleanup legs when row removal succeeded but cleanup did not — the
    /// message is gone, but stale index entries may remain and should be
    /// surfaced to the caller rather than silently discarded.
    ///
    /// Returns `Ok(())` immediately, with no cleanup attempted, if the note
    /// does not exist (nothing to compensate).
    ///
    /// Not a general-purpose replacement for `delete_note(..., hard=true)`:
    /// normal hard delete still needs cleanup-first semantics (no dangling
    /// references) since a caller-visible error there should not remove the row.
    pub async fn delete_note_row_first_for_compensation(
        &self,
        token: &NamespaceToken,
        id: Uuid,
    ) -> RuntimeResult<()> {
        let note_store = self.notes(token)?;
        let Some(note) = note_store.get_note_including_deleted(id).await? else {
            return Ok(());
        };
        let record_tok = NamespaceToken::for_namespace(
            khive_types::Namespace::parse(&note.namespace)
                .map_err(|e| RuntimeError::Internal(format!("note namespace invalid: {e}")))?,
        );
        let record_ns = note.namespace.clone();

        // Critical ordering: remove the row before any cleanup that can fail.
        note_store.delete_note(id, DeleteMode::Hard).await?;

        #[cfg(any(test, feature = "fault-injection"))]
        {
            let armed = ROLLBACK_CLEANUP_FAIL_NS.lock().unwrap().take();
            if armed.as_deref() == Some(record_ns.as_str()) {
                return Err(RuntimeError::Internal(
                    "row removed but compensation cleanup failed: injected=true".to_string(),
                ));
            }
        }

        let mut cleanup_errors = Vec::new();
        if let Err(e) = self.graph(&record_tok)?.purge_incident_edges(id).await {
            cleanup_errors.push(format!("graph={e}"));
        }
        if let Err(e) = self
            .text_for_notes(&record_tok)?
            .delete_document(&record_ns, id)
            .await
        {
            cleanup_errors.push(format!("fts={e}"));
        }
        for model_name in self.registered_embedding_model_names() {
            if let Err(e) = self
                .vectors_for_model(&record_tok, &model_name)?
                .delete(id)
                .await
            {
                cleanup_errors.push(format!("vector[{model_name}]={e}"));
            }
        }
        if cleanup_errors.is_empty() {
            Ok(())
        } else {
            Err(RuntimeError::Internal(format!(
                "row removed but compensation cleanup failed: {}",
                cleanup_errors.join("; ")
            )))
        }
    }

    /// Soft-delete or hard-delete an entity by ID (soft delete by default).
    ///
    /// On hard delete, cascades to remove all incident edges (both inbound and
    /// outbound) to prevent dangling references. Soft delete also cleans FTS
    /// and vector indexes; edges are left in place.
    /// Routed attachment cleanup is performed by the registry after ownership resolution.
    ///
    /// UUID v4 is globally unique: no namespace filter on by-ID ops.
    pub async fn delete_entity(
        &self,
        token: &NamespaceToken,
        id: Uuid,
        hard: bool,
    ) -> RuntimeResult<bool> {
        let (deleted, degradations) = self
            .delete_entity_with_post_commit_report(token, id, hard)
            .await?;
        legacy_post_commit_result("delete_entity", id, deleted, degradations)
    }

    /// The committed delete together with non-retryable index/telemetry errors.
    pub async fn delete_entity_with_post_commit_report(
        &self,
        token: &NamespaceToken,
        id: Uuid,
        hard: bool,
    ) -> RuntimeResult<(bool, Vec<PostCommitDegradation>)> {
        let entity = if hard {
            match self
                .entities(token)?
                .get_entity_including_deleted(id)
                .await?
            {
                Some(e) => e,
                None => return Ok((false, Vec::new())),
            }
        } else {
            match self.entities(token)?.get_entity(id).await? {
                Some(e) => e,
                None => return Ok((false, Vec::new())),
            }
        };
        let mode = if hard {
            DeleteMode::Hard
        } else {
            DeleteMode::Soft
        };

        // Route cascade and index cleanup through the RECORD's namespace, not the caller's.
        let record_tok = token.with_namespace(
            khive_types::Namespace::parse(&entity.namespace)
                .map_err(|e| RuntimeError::Internal(format!("entity namespace invalid: {e}")))?,
        );
        let actor = format!("{}:{}", token.actor().kind, token.actor().id);

        // On hard delete, the row delete and the incident-edge cascade (including
        // already-soft-deleted edges) run as ONE write transaction: see
        // `atomic_hard_delete_with_edge_purge`. Index cleanup follows the
        // commit; it is best-effort and idempotent, unlike the row/edge pair.
        let deleted = if hard {
            // Cross-backend attachment cleanup requires the registry's ownership check.
            self.atomic_hard_delete_with_edge_purge(
                entity_hard_delete_statement(id),
                id,
                &entity.namespace,
                &actor,
                SubstrateKind::Entity,
            )
            .await?
        } else {
            self.entities(token)?.delete_entity(id, mode).await?
        };
        let mut degradations = Vec::new();
        if deleted {
            let ns = entity.namespace.clone();
            let fts_result = match self.text(&record_tok) {
                Ok(store) => store
                    .delete_document(&ns, id)
                    .await
                    .map_err(RuntimeError::from),
                Err(error) => Err(error),
            };
            if let Err(error) = fts_result {
                record_post_commit_degradation(
                    &mut degradations,
                    "delete_entity",
                    id,
                    "fts_cleanup",
                    error,
                );
            }
            for model_name in self.registered_embedding_model_names() {
                let vector_result = match self.vectors_for_model(&record_tok, &model_name) {
                    Ok(store) => store.delete(id).await.map_err(RuntimeError::from),
                    Err(error) => Err(error),
                };
                if let Err(error) = vector_result {
                    record_post_commit_degradation(
                        &mut degradations,
                        "delete_entity",
                        id,
                        "vector_cleanup",
                        format!("{model_name}: {error}"),
                    );
                }
            }
            let event = khive_storage::event::Event::new(
                ns.clone(),
                "delete",
                EventKind::EntityDeleted,
                SubstrateKind::Entity,
                "",
            )
            .with_target(id)
            .with_payload(serde_json::json!({"id": id, "namespace": ns, "hard": hard}));
            let event_result = match self.events(&record_tok) {
                Ok(store) => store.append_event(event).await.map_err(RuntimeError::from),
                Err(error) => Err(error),
            };
            if let Err(error) = event_result {
                record_post_commit_degradation(
                    &mut degradations,
                    "delete_entity",
                    id,
                    "event_append",
                    error,
                );
            }
        }
        Ok((deleted, degradations))
    }

    pub(crate) async fn delete_entity_attachments_on_core(&self, id: Uuid) -> RuntimeResult<bool> {
        let core = self.core();
        drop(core.attachments()?);
        let statement = khive_db::stores::attachment::delete_record_attachments_statement(
            id,
            AttachmentSubstrate::Entity,
        );
        Ok(core.sql().writer().await?.execute(statement).await? > 0)
    }
}
