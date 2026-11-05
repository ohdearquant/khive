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
        let both_memory_at_check = match (
            self.resolve_by_id_including_deleted(token, edge.source_id)
                .await?,
            self.resolve_by_id_including_deleted(token, edge.target_id)
                .await?,
        ) {
            (Some(crate::Resolved::Note(source)), Some(crate::Resolved::Note(target))) => {
                let both_memory = source.kind == "memory" && target.kind == "memory";
                Self::ensure_memory_note_edge_namespace(
                    token,
                    "restore",
                    &edge.namespace,
                    (edge.source_id, edge.target_id),
                    edge.relation,
                    &source,
                    &target,
                )?;
                both_memory
            }
            _ => false,
        };
        let updated_at = Utc::now();
        let plan = AtomicOpPlan::Update(Box::new(UpdatePlan {
            graph_effects: Vec::new(),
            target_id: id,
            statements: vec![PlanStatement {
                statement: SqlStatement {
                    sql: "UPDATE graph_edges SET deleted_at=NULL, updated_at=?1 \
                          WHERE id=?2 AND namespace=?3 AND deleted_at IS NOT NULL \
                            AND ((?4 = 0 AND ( \
                                  NOT EXISTS (SELECT 1 FROM notes AS source_memory \
                                       WHERE source_memory.id=graph_edges.source_id \
                                         AND source_memory.kind='memory') \
                                  OR NOT EXISTS (SELECT 1 FROM notes AS target_memory \
                                       WHERE target_memory.id=graph_edges.target_id \
                                         AND target_memory.kind='memory') \
                                 )) \
                                 OR EXISTS (SELECT 1 FROM notes AS source_match \
                                      WHERE source_match.id=graph_edges.source_id \
                                        AND source_match.namespace=graph_edges.namespace \
                                        AND source_match.kind='memory' \
                                        AND source_match.deleted_at IS NULL))"
                        .into(),
                    params: vec![
                        SqlValue::Integer(updated_at.timestamp_micros()),
                        SqlValue::Text(id.to_string()),
                        SqlValue::Text(edge.namespace.clone()),
                        SqlValue::Integer(if both_memory_at_check { 1 } else { 0 }),
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
        #[cfg(test)]
        memory_edge_namespace_restore_tests::before_writer(id).await;
        match run_atomic_unit(self.sql().as_ref(), vec![plan]).await {
            Ok(AtomicRunOutcome::Committed { .. }) => {
                let mut restored = edge;
                restored.deleted_at = None;
                restored.updated_at = updated_at;
                Ok(Some((restored, true)))
            }
            Ok(AtomicRunOutcome::RolledBack {
                failure:
                    AtomicOpFailure::GuardFailed {
                        statement_label: Some(label),
                        expected,
                        observed: 0,
                    },
                ..
            }) if label == "edge-restore" && expected == AffectedRowGuard::exactly(1) => {
                Err(RuntimeError::InvalidInput(
                    "edge restore preconditions changed before commit; retry against the current edge and endpoint state".into(),
                ))
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
#[cfg(test)]
pub(super) mod memory_edge_namespace_restore_tests {
    use crate::{KhiveRuntime, Resolved, RuntimeError};
    use khive_storage::{Note, SqlStatement, SqlValue};
    use khive_types::{EdgeRelation, Namespace};
    use std::sync::Mutex;
    use std::time::Duration;
    use tokio::sync::oneshot;
    use uuid::Uuid;

    struct BeforeWriterHook {
        edge_id: Uuid,
        entered: oneshot::Sender<()>,
        release: Option<oneshot::Receiver<()>>,
    }

    static BEFORE_WRITER_HOOKS: Mutex<Vec<BeforeWriterHook>> = Mutex::new(Vec::new());

    fn install_before_writer_hook(
        edge_id: Uuid,
        entered: oneshot::Sender<()>,
        release: Option<oneshot::Receiver<()>>,
    ) {
        let mut hooks = BEFORE_WRITER_HOOKS.lock().unwrap();
        assert!(!hooks.iter().any(|hook| hook.edge_id == edge_id));
        hooks.push(BeforeWriterHook {
            edge_id,
            entered,
            release,
        });
    }

    fn remove_before_writer_hook(edge_id: Uuid) {
        BEFORE_WRITER_HOOKS
            .lock()
            .unwrap()
            .retain(|hook| hook.edge_id != edge_id);
    }

    pub(super) async fn before_writer(edge_id: Uuid) {
        let hook = {
            let mut hooks = BEFORE_WRITER_HOOKS.lock().unwrap();
            hooks
                .iter()
                .position(|hook| hook.edge_id == edge_id)
                .map(|index| hooks.remove(index))
        };
        if let Some(hook) = hook {
            let _ = hook.entered.send(());
            if let Some(release) = hook.release {
                let _ = release.await;
            }
        }
    }

    fn fixture() -> (KhiveRuntime, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let runtime = KhiveRuntime::new_for_test(crate::RuntimeConfig {
            db_path: Some(dir.path().join("restore-edge-memory-namespace.db")),
            embedding_model: None,
            additional_embedding_models: Vec::new(),
            ..Default::default()
        })
        .unwrap();
        (runtime, dir)
    }

    async fn execute_private_write(runtime: &KhiveRuntime, statement: SqlStatement) -> u64 {
        let result = runtime
            .sql()
            .atomic_unit(Box::new(move |writer| {
                Box::pin(async move {
                    let affected = writer.execute(statement).await?;
                    Ok(Box::new(affected) as Box<dyn std::any::Any + Send>)
                })
            }))
            .await
            .unwrap();
        *result.downcast::<u64>().unwrap()
    }

    async fn prepare_tombstone(
        runtime: &KhiveRuntime,
        edge_id: Uuid,
        from_namespace: &str,
        stored_namespace: &str,
    ) -> u64 {
        let statement = SqlStatement {
            sql: "UPDATE graph_edges SET namespace=?1, deleted_at=?2 \
                  WHERE id=?3 AND namespace=?4 AND deleted_at IS NULL"
                .into(),
            params: vec![
                SqlValue::Text(stored_namespace.into()),
                SqlValue::Integer(chrono::Utc::now().timestamp_micros()),
                SqlValue::Text(edge_id.to_string()),
                SqlValue::Text(from_namespace.into()),
            ],
            label: Some("test-prepare-restore-tombstone".into()),
        };
        execute_private_write(runtime, statement).await
    }

    async fn move_memory_source_namespace(
        runtime: &KhiveRuntime,
        source_id: Uuid,
        from_namespace: &str,
        to_namespace: &str,
    ) -> u64 {
        let statement = SqlStatement {
            sql: "UPDATE notes SET namespace=?1 \
                  WHERE id=?2 AND namespace=?3 AND kind='memory' AND deleted_at IS NULL"
                .into(),
            params: vec![
                SqlValue::Text(to_namespace.into()),
                SqlValue::Text(source_id.to_string()),
                SqlValue::Text(from_namespace.into()),
            ],
            label: Some("test-move-memory-source-between-check-and-restore".into()),
        };
        execute_private_write(runtime, statement).await
    }

    async fn soft_delete_memory_source(runtime: &KhiveRuntime, source_id: Uuid) -> u64 {
        let statement = SqlStatement {
            sql: "UPDATE notes SET deleted_at=?1 \
                  WHERE id=?2 AND namespace=?3 AND kind='memory' AND deleted_at IS NULL"
                .into(),
            params: vec![
                SqlValue::Integer(chrono::Utc::now().timestamp_micros()),
                SqlValue::Text(source_id.to_string()),
                SqlValue::Text("local".into()),
            ],
            label: Some("test-soft-delete-memory-source-between-check-and-restore".into()),
        };
        execute_private_write(runtime, statement).await
    }

    async fn change_note_kind(
        runtime: &KhiveRuntime,
        note_id: Uuid,
        from_kind: &str,
        to_kind: &str,
    ) -> u64 {
        let statement = SqlStatement {
            sql: "UPDATE notes SET kind=?1 WHERE id=?2 AND kind=?3 AND deleted_at IS NULL".into(),
            params: vec![
                SqlValue::Text(to_kind.into()),
                SqlValue::Text(note_id.to_string()),
                SqlValue::Text(from_kind.into()),
            ],
            label: Some("test-promote-target-to-memory-between-check-and-restore".into()),
        };
        execute_private_write(runtime, statement).await
    }

    async fn assert_memory_note_in_namespace(
        runtime: &KhiveRuntime,
        token: &crate::NamespaceToken,
        id: Uuid,
        expected_namespace: &str,
    ) {
        match runtime
            .resolve_by_id_including_deleted(token, id)
            .await
            .unwrap()
        {
            Some(Resolved::Note(note)) => {
                assert_eq!(note.namespace, expected_namespace);
                assert_eq!(note.kind, "memory");
                assert!(note.deleted_at.is_none());
            }
            _ => panic!("fixture endpoint was not a live memory note"),
        }
    }

    fn assert_helper_namespace_error(message: &str) {
        assert_eq!(
            message,
            "restore: memory edge source namespace other does not match stored edge namespace local; leave this tombstone deleted and create a replacement link in namespace=other",
            "restore_refusal_names_operation_and_supported_remedy"
        );
    }

    #[tokio::test]
    async fn restore_edge_rejects_legacy_tombstone_with_mismatched_memory_source_namespace() {
        let (runtime, _dir) = fixture();
        // Keep the source namespace readable for this diagnostic assertion.
        let local_token = runtime
            .authorize_with_visibility(Namespace::local(), vec![Namespace::parse("other").unwrap()])
            .unwrap();
        let other_token = runtime
            .authorize(Namespace::parse("other").unwrap())
            .unwrap();

        let source = Note::new("other", "memory", "restore source");
        let source_id = source.id;
        let target = Note::new("other", "memory", "restore target");
        let target_id = target.id;
        runtime
            .notes(&other_token)
            .unwrap()
            .upsert_note(source)
            .await
            .unwrap();
        runtime
            .notes(&other_token)
            .unwrap()
            .upsert_note(target)
            .await
            .unwrap();

        let edge = runtime
            .link(
                &other_token,
                source_id,
                target_id,
                EdgeRelation::Annotates,
                1.0,
                None,
            )
            .await
            .unwrap();
        let edge_id = Uuid::from(edge.id);
        assert_eq!(edge.namespace, "other");
        assert_eq!(edge.source_id, source_id);
        assert_eq!(edge.target_id, target_id);
        assert_eq!(edge.relation, EdgeRelation::Annotates);

        assert_eq!(
            prepare_tombstone(&runtime, edge_id, "other", "local").await,
            1
        );
        let stored_edge = runtime
            .get_edge_including_deleted(&local_token, edge_id)
            .await
            .unwrap()
            .expect("synthetic legacy edge row should remain addressable");
        assert_eq!(stored_edge.namespace, "local");
        assert_eq!(stored_edge.source_id, source_id);
        assert_eq!(stored_edge.target_id, target_id);
        assert_eq!(stored_edge.relation, EdgeRelation::Annotates);
        assert!(stored_edge.deleted_at.is_some());
        assert_memory_note_in_namespace(&runtime, &local_token, source_id, "other").await;
        assert_memory_note_in_namespace(&runtime, &local_token, target_id, "other").await;

        let (entered_tx, mut entered_rx) = oneshot::channel();
        install_before_writer_hook(edge_id, entered_tx, None);
        let outcome = runtime.restore_edge(&local_token, edge_id).await;
        remove_before_writer_hook(edge_id);
        let writer_boundary_reached = matches!(entered_rx.try_recv(), Ok(()));
        match outcome {
            Err(RuntimeError::InvalidInput(message)) => {
                if writer_boundary_reached {
                    panic!("restore_memory_namespace_guard_bypassed_writer_reached");
                }
                assert_helper_namespace_error(&message);
            }
            Ok(Some((_, true))) => panic!("restore_memory_namespace_mismatch_admitted"),
            _ => panic!("fixture did not reach the targeted restore refusal"),
        }

        let after_refusal = runtime
            .get_edge_including_deleted(&local_token, edge_id)
            .await
            .unwrap()
            .expect("refused restore must preserve the tombstone row");
        assert_eq!(after_refusal.namespace, "local");
        assert!(after_refusal.deleted_at.is_some());
    }

    #[tokio::test]
    async fn restore_edge_rechecks_memory_source_namespace_at_writer_after_precheck() {
        let (runtime, _dir) = fixture();
        let local_token = runtime.authorize(Namespace::local()).unwrap();
        let other_token = runtime
            .authorize(Namespace::parse("other").unwrap())
            .unwrap();

        let source = Note::new("local", "memory", "moving restore source");
        let source_id = source.id;
        let target = Note::new("local", "memory", "stable restore target");
        let target_id = target.id;
        runtime
            .notes(&local_token)
            .unwrap()
            .upsert_note(source)
            .await
            .unwrap();
        runtime
            .notes(&local_token)
            .unwrap()
            .upsert_note(target)
            .await
            .unwrap();

        let edge = runtime
            .link(
                &local_token,
                source_id,
                target_id,
                EdgeRelation::Annotates,
                1.0,
                None,
            )
            .await
            .unwrap();
        let edge_id = Uuid::from(edge.id);
        assert_eq!(edge.namespace, "local");
        assert_eq!(edge.source_id, source_id);
        assert_eq!(edge.target_id, target_id);
        assert_eq!(edge.relation, EdgeRelation::Annotates);
        assert_eq!(
            prepare_tombstone(&runtime, edge_id, "local", "local").await,
            1
        );
        let edge_before_restore = runtime
            .get_edge_including_deleted(&local_token, edge_id)
            .await
            .unwrap()
            .expect("tombstone should be present before restore");
        assert_eq!(edge_before_restore.namespace, "local");
        assert!(edge_before_restore.deleted_at.is_some());
        assert_memory_note_in_namespace(&runtime, &local_token, source_id, "local").await;
        assert_memory_note_in_namespace(&runtime, &local_token, target_id, "local").await;

        let (entered_tx, mut entered_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        install_before_writer_hook(edge_id, entered_tx, Some(release_rx));
        let mut restore = Box::pin(runtime.restore_edge(&local_token, edge_id));
        let crossed_precheck = tokio::time::timeout(Duration::from_secs(10), async {
            tokio::select! {
                _ = &mut restore => false,
                reached = &mut entered_rx => reached.is_ok(),
            }
        })
        .await
        .unwrap_or(false);
        if !crossed_precheck {
            remove_before_writer_hook(edge_id);
            panic!("fixture did not positively cross the outside guard before writer");
        }

        assert_eq!(
            move_memory_source_namespace(&runtime, source_id, "local", "other").await,
            1
        );
        assert_memory_note_in_namespace(&runtime, &other_token, source_id, "other").await;
        assert_memory_note_in_namespace(&runtime, &local_token, target_id, "local").await;
        let edge_while_paused = runtime
            .get_edge_including_deleted(&local_token, edge_id)
            .await
            .unwrap()
            .expect("edge must still exist while restore is held before writer");
        assert_eq!(edge_while_paused.namespace, "local");
        assert!(edge_while_paused.deleted_at.is_some());

        assert!(release_tx.send(()).is_ok());
        let outcome = tokio::time::timeout(Duration::from_secs(10), &mut restore)
            .await
            .expect("restore did not complete after release");
        match outcome {
            Err(RuntimeError::InvalidInput(_)) => {}
            Ok(Some((_, true))) => {
                panic!("restore_memory_namespace_drift_admitted_after_precheck")
            }
            _ => panic!("writer-fence fixture did not return the expected refusal class"),
        }

        let after_refusal = runtime
            .get_edge_including_deleted(&local_token, edge_id)
            .await
            .unwrap()
            .expect("writer-fence refusal must preserve the tombstone");
        assert_eq!(after_refusal.namespace, "local");
        assert!(after_refusal.deleted_at.is_some());
    }

    #[tokio::test]
    async fn restore_edge_rechecks_memory_source_liveness_at_writer_after_precheck() {
        let (runtime, _dir) = fixture();
        let local_token = runtime.authorize(Namespace::local()).unwrap();

        let source = Note::new("local", "memory", "soft-deleted restore source");
        let source_id = source.id;
        let target = Note::new("local", "memory", "live restore target");
        let target_id = target.id;
        runtime
            .notes(&local_token)
            .unwrap()
            .upsert_note(source)
            .await
            .unwrap();
        runtime
            .notes(&local_token)
            .unwrap()
            .upsert_note(target)
            .await
            .unwrap();

        let edge = runtime
            .link(
                &local_token,
                source_id,
                target_id,
                EdgeRelation::Annotates,
                1.0,
                None,
            )
            .await
            .unwrap();
        let edge_id = Uuid::from(edge.id);
        assert_eq!(edge.namespace, "local");
        assert_eq!(
            prepare_tombstone(&runtime, edge_id, "local", "local").await,
            1
        );
        assert_memory_note_in_namespace(&runtime, &local_token, source_id, "local").await;
        assert_memory_note_in_namespace(&runtime, &local_token, target_id, "local").await;

        let (entered_tx, mut entered_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        install_before_writer_hook(edge_id, entered_tx, Some(release_rx));
        let mut restore = Box::pin(runtime.restore_edge(&local_token, edge_id));
        let crossed_precheck = tokio::time::timeout(Duration::from_secs(10), async {
            tokio::select! {
                _ = &mut restore => false,
                reached = &mut entered_rx => reached.is_ok(),
            }
        })
        .await
        .unwrap_or(false);
        if !crossed_precheck {
            remove_before_writer_hook(edge_id);
            panic!("fixture did not positively cross the outside guard before writer");
        }

        assert_eq!(soft_delete_memory_source(&runtime, source_id).await, 1);
        match runtime
            .resolve_by_id_including_deleted(&local_token, source_id)
            .await
            .unwrap()
        {
            Some(Resolved::Note(note)) => {
                assert_eq!(note.namespace, "local");
                assert_eq!(note.kind, "memory");
                assert!(note.deleted_at.is_some());
            }
            _ => panic!("fixture source did not remain as an included-deleted memory note"),
        }
        assert_memory_note_in_namespace(&runtime, &local_token, target_id, "local").await;

        assert!(release_tx.send(()).is_ok());
        let outcome = tokio::time::timeout(Duration::from_secs(10), &mut restore)
            .await
            .expect("restore did not complete after release");
        match outcome {
            Err(RuntimeError::InvalidInput(_)) => {}
            Ok(Some((_, true))) => {
                panic!("restore_memory_source_soft_delete_admitted_after_precheck")
            }
            _ => panic!("source-liveness fence fixture did not return the expected refusal class"),
        }
        let after_refusal = runtime
            .get_edge_including_deleted(&local_token, edge_id)
            .await
            .unwrap()
            .expect("source-liveness refusal must preserve the tombstone");
        assert_eq!(after_refusal.namespace, "local");
        assert!(after_refusal.deleted_at.is_some());
    }

    #[tokio::test]
    async fn restore_edge_rejects_preexisting_soft_deleted_memory_source() {
        let (runtime, _dir) = fixture();
        let local_token = runtime.authorize(Namespace::local()).unwrap();
        let source = Note::new("local", "memory", "already deleted restore source");
        let source_id = source.id;
        let target = Note::new("local", "memory", "live restore target");
        let target_id = target.id;
        runtime
            .notes(&local_token)
            .unwrap()
            .upsert_note(source)
            .await
            .unwrap();
        runtime
            .notes(&local_token)
            .unwrap()
            .upsert_note(target)
            .await
            .unwrap();

        let edge = runtime
            .link(
                &local_token,
                source_id,
                target_id,
                EdgeRelation::Annotates,
                1.0,
                None,
            )
            .await
            .unwrap();
        let edge_id = Uuid::from(edge.id);
        assert_eq!(
            prepare_tombstone(&runtime, edge_id, "local", "local").await,
            1
        );
        let stored_edge = runtime
            .get_edge_including_deleted(&local_token, edge_id)
            .await
            .unwrap()
            .expect("fixture tombstone should be addressable before source deletion");
        assert_eq!(stored_edge.namespace, "local");
        assert!(stored_edge.deleted_at.is_some());
        assert_eq!(soft_delete_memory_source(&runtime, source_id).await, 1);
        match runtime
            .resolve_by_id_including_deleted(&local_token, source_id)
            .await
            .unwrap()
        {
            Some(Resolved::Note(note)) => {
                assert_eq!(note.namespace, "local");
                assert_eq!(note.kind, "memory");
                assert!(note.deleted_at.is_some());
            }
            _ => panic!("fixture source did not remain as an included-deleted memory note"),
        }
        assert_memory_note_in_namespace(&runtime, &local_token, target_id, "local").await;

        let (entered_tx, mut entered_rx) = oneshot::channel();
        install_before_writer_hook(edge_id, entered_tx, None);
        let outcome = runtime.restore_edge(&local_token, edge_id).await;
        remove_before_writer_hook(edge_id);
        assert!(
            matches!(entered_rx.try_recv(), Ok(())),
            "fixture did not reach writer fence"
        );
        match outcome {
            Err(RuntimeError::InvalidInput(_)) => {}
            Ok(Some((_, true))) => panic!("restore_memory_predeleted_source_admitted"),
            _ => panic!("predeleted-source fixture did not return the expected refusal class"),
        }
        let after_refusal = runtime
            .get_edge_including_deleted(&local_token, edge_id)
            .await
            .unwrap()
            .expect("predeleted-source refusal must preserve the tombstone");
        assert!(after_refusal.deleted_at.is_some());
    }

    #[tokio::test]
    async fn restore_edge_rechecks_current_memory_pair_after_nonmemory_precheck() {
        let (runtime, _dir) = fixture();
        let local_token = runtime.authorize(Namespace::local()).unwrap();
        let other_token = runtime
            .authorize(Namespace::parse("other").unwrap())
            .unwrap();
        let source = Note::new(
            "other",
            "memory",
            "memory source before current-pair change",
        );
        let source_id = source.id;
        let target = Note::new(
            "other",
            "observation",
            "nonmemory target before current-pair change",
        );
        let target_id = target.id;
        runtime
            .notes(&other_token)
            .unwrap()
            .upsert_note(source)
            .await
            .unwrap();
        runtime
            .notes(&other_token)
            .unwrap()
            .upsert_note(target)
            .await
            .unwrap();

        let edge = runtime
            .link(
                &other_token,
                source_id,
                target_id,
                EdgeRelation::Annotates,
                1.0,
                None,
            )
            .await
            .unwrap();
        let edge_id = Uuid::from(edge.id);
        assert_eq!(
            prepare_tombstone(&runtime, edge_id, "other", "local").await,
            1
        );
        let edge_before_restore = runtime
            .get_edge_including_deleted(&local_token, edge_id)
            .await
            .unwrap()
            .expect("current-pair fixture tombstone should be addressable");
        assert_eq!(edge_before_restore.namespace, "local");
        assert_eq!(edge_before_restore.source_id, source_id);
        assert_eq!(edge_before_restore.target_id, target_id);
        assert!(edge_before_restore.deleted_at.is_some());
        assert_memory_note_in_namespace(&runtime, &local_token, source_id, "other").await;
        match runtime
            .resolve_by_id_including_deleted(&local_token, target_id)
            .await
            .unwrap()
        {
            Some(Resolved::Note(note)) => {
                assert_eq!(note.namespace, "other");
                assert_eq!(note.kind, "observation");
                assert!(note.deleted_at.is_none());
            }
            _ => panic!("fixture target was not the expected nonmemory Note"),
        }

        match runtime.restore_edge(&local_token, edge_id).await {
            Ok(Some((restored, true))) => {
                assert_eq!(restored.namespace, "local");
                assert!(restored.deleted_at.is_none());
            }
            _ => panic!("ordinary memory-to-observation restore should preserve existing behavior"),
        }
        let ordinary_restore = runtime
            .get_edge_including_deleted(&local_token, edge_id)
            .await
            .unwrap()
            .expect("ordinary nonmemory-pair restore should remain readable");
        assert_eq!(ordinary_restore.namespace, "local");
        assert!(ordinary_restore.deleted_at.is_none());
        assert_eq!(
            prepare_tombstone(&runtime, edge_id, "local", "local").await,
            1
        );
        let edge_before_race = runtime
            .get_edge_including_deleted(&local_token, edge_id)
            .await
            .unwrap()
            .expect("retombstoned edge should be addressable before current-pair race");
        assert_eq!(edge_before_race.namespace, "local");
        assert!(edge_before_race.deleted_at.is_some());

        let (entered_tx, mut entered_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        install_before_writer_hook(edge_id, entered_tx, Some(release_rx));
        let mut restore = Box::pin(runtime.restore_edge(&local_token, edge_id));
        let crossed_precheck = tokio::time::timeout(Duration::from_secs(10), async {
            tokio::select! {
                _ = &mut restore => false,
                reached = &mut entered_rx => reached.is_ok(),
            }
        })
        .await
        .unwrap_or(false);
        if !crossed_precheck {
            remove_before_writer_hook(edge_id);
            panic!("fixture did not positively cross the outside check before writer");
        }

        assert_eq!(
            change_note_kind(&runtime, target_id, "observation", "memory").await,
            1
        );
        match runtime
            .resolve_by_id_including_deleted(&local_token, target_id)
            .await
            .unwrap()
        {
            Some(Resolved::Note(note)) => {
                assert_eq!(note.namespace, "other");
                assert_eq!(note.kind, "memory");
                assert!(note.deleted_at.is_none());
            }
            _ => panic!("fixture target did not become a live memory Note"),
        }
        assert!(release_tx.send(()).is_ok());
        let outcome = tokio::time::timeout(Duration::from_secs(10), &mut restore)
            .await
            .expect("restore did not complete after release");
        match outcome {
            Err(RuntimeError::InvalidInput(_)) => {}
            Ok(Some((_, true))) => {
                panic!("restore_current_memory_pair_source_namespace_admitted")
            }
            _ => panic!("current-pair fixture did not return the expected refusal class"),
        }
        let after_refusal = runtime
            .get_edge_including_deleted(&local_token, edge_id)
            .await
            .unwrap()
            .expect("current-pair refusal must preserve the tombstone");
        assert_eq!(after_refusal.namespace, "local");
        assert!(after_refusal.deleted_at.is_some());
    }

    #[tokio::test]
    async fn restore_edge_allows_memory_target_in_another_namespace() {
        let (runtime, _dir) = fixture();
        let local_token = runtime.authorize(Namespace::local()).unwrap();
        let other_token = runtime
            .authorize(Namespace::parse("other").unwrap())
            .unwrap();

        let source = Note::new("local", "memory", "local restore source");
        let source_id = source.id;
        let target = Note::new("other", "memory", "foreign restore target");
        let target_id = target.id;
        runtime
            .notes(&local_token)
            .unwrap()
            .upsert_note(source)
            .await
            .unwrap();
        runtime
            .notes(&other_token)
            .unwrap()
            .upsert_note(target)
            .await
            .unwrap();

        let edge = runtime
            .link(
                &local_token,
                source_id,
                target_id,
                EdgeRelation::Annotates,
                1.0,
                None,
            )
            .await
            .unwrap();
        let edge_id = Uuid::from(edge.id);
        assert_eq!(edge.namespace, "local");
        assert_eq!(edge.source_id, source_id);
        assert_eq!(edge.target_id, target_id);
        assert_eq!(
            prepare_tombstone(&runtime, edge_id, "local", "local").await,
            1
        );
        assert_memory_note_in_namespace(&runtime, &local_token, source_id, "local").await;
        assert_memory_note_in_namespace(&runtime, &local_token, target_id, "other").await;

        match runtime.restore_edge(&local_token, edge_id).await {
            Ok(Some((restored, true))) => {
                assert_eq!(restored.namespace, "local");
                assert!(restored.deleted_at.is_none());
            }
            _ => panic!("source-local memory edge with a cross-namespace target should restore"),
        }
        let after_restore = runtime
            .get_edge_including_deleted(&local_token, edge_id)
            .await
            .unwrap()
            .expect("restored edge should remain readable");
        assert_eq!(after_restore.namespace, "local");
        assert!(after_restore.deleted_at.is_none());
    }

    #[tokio::test]
    async fn restore_refusal_hides_unreadable_memory_source_namespace() {
        let (runtime, _dir) = fixture();
        let source_namespace = "guard-private-source";
        let source_token = runtime
            .authorize(Namespace::parse(source_namespace).unwrap())
            .unwrap();
        let readable = runtime
            .authorize_with_visibility(
                Namespace::local(),
                vec![Namespace::parse(source_namespace).unwrap()],
            )
            .unwrap();
        let unreadable = runtime
            .authorize_with_visibility(Namespace::local(), Vec::new())
            .unwrap();
        assert!(readable
            .visible_namespaces()
            .iter()
            .any(|ns| ns.as_str() == source_namespace));
        assert!(!unreadable
            .visible_namespaces()
            .iter()
            .any(|ns| ns.as_str() == source_namespace));
        let source = Note::new(source_namespace, "memory", "source");
        let target = Note::new(source_namespace, "memory", "target");
        let (source_id, target_id) = (source.id, target.id);
        runtime
            .notes(&source_token)
            .unwrap()
            .upsert_note(source)
            .await
            .unwrap();
        runtime
            .notes(&source_token)
            .unwrap()
            .upsert_note(target)
            .await
            .unwrap();
        assert_memory_note_in_namespace(&runtime, &source_token, source_id, source_namespace).await;
        assert_memory_note_in_namespace(&runtime, &source_token, target_id, source_namespace).await;
        let edge = runtime
            .link(
                &source_token,
                source_id,
                target_id,
                EdgeRelation::Annotates,
                1.0,
                None,
            )
            .await
            .expect("matching source namespace must admit the fixture link");
        assert_eq!(edge.namespace.as_str(), source_namespace);
        let edge_id = Uuid::from(edge.id);
        assert_eq!(
            prepare_tombstone(&runtime, edge_id, source_namespace, "local").await,
            1
        );
        let before = runtime
            .get_edge_including_deleted(&unreadable, edge_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(before.namespace.as_str(), "local");
        assert!(before.deleted_at.is_some());
        let RuntimeError::InvalidInput(message) =
            runtime.restore_edge(&readable, edge_id).await.unwrap_err()
        else {
            panic!("readable mismatch must preserve InvalidInput");
        };
        assert_eq!(message, format!("restore: memory edge source namespace {source_namespace} does not match stored edge namespace local; leave this tombstone deleted and create a replacement link in namespace={source_namespace}"));
        let RuntimeError::InvalidInput(message) = runtime
            .restore_edge(&unreadable, edge_id)
            .await
            .unwrap_err()
        else {
            panic!("unreadable mismatch must preserve InvalidInput");
        };
        assert!(
            !message.contains(source_namespace),
            "restore_unreadable_source_namespace_not_disclosed"
        );
        assert!(!message.contains(&source_id.to_string()));
        assert!(!message.contains(&target_id.to_string()));
        assert_eq!(message, "restore: memory edge source row is in a namespace the caller cannot read; the edge cannot be restored under stored edge namespace local");
        let after = runtime
            .get_edge_including_deleted(&unreadable, edge_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after.namespace.as_str(), "local");
        assert_eq!(after.source_id, source_id);
        assert_eq!(after.target_id, target_id);
        assert!(after.deleted_at.is_some());
    }
}
