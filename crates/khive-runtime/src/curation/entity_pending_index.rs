//! Durable publication protocol for deferred entity updates (ADR-014 Amendment 7).

use std::any::Any;

use khive_db::pending_index::{
    self, PendingEntityIndex, PendingIndexTarget, PendingIndexTargetKind,
};
use khive_storage::{AtomicUnitOp, Entity, SqlStatement, StorageError, WriterTaskRequestState};
use serde::Serialize;

use crate::retrieval::EmbeddingTruncationReport;
use crate::{KhiveRuntime, NamespaceToken, PostCommitDegradation, RuntimeError, RuntimeResult};

#[cfg(test)]
tokio::task_local! {
    static UNKNOWN_AFTER_PUBLISH: bool;
}

/// One required index whose publication was not established by this update.
#[derive(Clone, Debug, Serialize)]
pub struct EntityIndexFailure {
    pub target: PendingIndexTargetKind,
    /// Sanitized stage; never entity content or provider error text.
    pub stage: String,
}

/// The indexing outcome of the committed entity revision, not a backlog census.
#[derive(Clone, Debug, Serialize)]
pub struct EntityIndexReport {
    pub status: &'static str,
    pub unfinished: Vec<EntityIndexFailure>,
}

impl EntityIndexReport {
    pub(super) fn complete(required: bool) -> Self {
        Self {
            status: if required {
                "complete"
            } else {
                "not_requested"
            },
            unfinished: Vec::new(),
        }
    }

    fn unfinished(&mut self, target: &PendingIndexTargetKind, stage: &str) {
        self.status = "unfinished";
        self.unfinished.push(EntityIndexFailure {
            target: target.clone(),
            stage: stage.into(),
        });
    }
}

/// Report-aware updates retain their committed entity even when indexes fail.
#[derive(Clone, Debug, Serialize)]
pub struct EntityUpdateReport {
    pub embedding: EmbeddingTruncationReport,
    pub indexing: EntityIndexReport,
    pub post_commit_degradations: Vec<PostCommitDegradation>,
}

impl EntityUpdateReport {
    pub(super) fn complete(embedding: EmbeddingTruncationReport, required: bool) -> Self {
        Self {
            embedding,
            indexing: EntityIndexReport::complete(required),
            post_commit_degradations: Vec::new(),
        }
    }

    /// Compatibility callers cannot silently discard committed-but-unfinished work.
    pub(super) fn into_legacy(
        self,
        entity: Entity,
    ) -> RuntimeResult<(Entity, EmbeddingTruncationReport)> {
        let mut degradations = self.post_commit_degradations;
        degradations.extend(self.indexing.unfinished.into_iter().map(|failure| {
            PostCommitDegradation {
                stage: "entity_index_unfinished",
                error: serde_json::json!(failure).to_string(),
            }
        }));
        crate::operations::legacy_post_commit_result(
            "update_entity",
            entity.id,
            (entity, self.embedding),
            degradations,
        )
    }
}

fn unsettled(error: &RuntimeError) -> bool {
    error
        .writer_task_failure_context()
        .is_some_and(|context| context.request_state == WriterTaskRequestState::SideEffectsUnknown)
        || matches!(error.refusal_source(), RuntimeError::Storage(error)
        if error.sqlite_write_failure().is_some_and(|failure| failure.settlement_unknown))
}

impl KhiveRuntime {
    /// The sole pending-target consumer, shared by inline work and future recovery.
    /// The closure performs only synchronous-ready SQL under one writer transaction.
    pub(super) async fn publish_pending_entity_target(
        &self,
        observed: &PendingEntityIndex,
        target: &PendingIndexTargetKind,
        statements: Vec<SqlStatement>,
    ) -> RuntimeResult<bool> {
        let observed = observed.clone();
        let target = target.clone();
        let op: AtomicUnitOp = Box::new(move |writer| {
            Box::pin(async move {
                let Some(current) = pending_index::read(writer, observed.entity_id).await? else {
                    return Ok(Box::new(false) as Box<dyn Any + Send>);
                };
                if current.generation != observed.generation
                    || current.namespace != observed.namespace
                    || current.source_version != observed.source_version
                    || !current.targets.iter().any(|entry| {
                        entry.kind == target && !entry.done && !entry.kind.is_blocked()
                    })
                    || !pending_index::live_matches(writer, &current).await?
                {
                    return Ok(Box::new(false) as Box<dyn Any + Send>);
                }
                for statement in statements {
                    writer.execute(statement).await?;
                }
                if !pending_index::finish(writer, current.entity_id, current.generation, &target)
                    .await?
                {
                    // Never commit index writes without consuming the matching target.
                    return Err(StorageError::Internal(
                        "pending index consumption guard failed".into(),
                    ));
                }
                Ok(Box::new(true) as Box<dyn Any + Send>)
            })
        });
        let outcome = self.sql().atomic_unit(op).await?;
        #[cfg(test)]
        if UNKNOWN_AFTER_PUBLISH
            .try_with(|enabled| *enabled)
            .unwrap_or(false)
        {
            return Err(RuntimeError::Storage(StorageError::writer_task_terminated(
                WriterTaskRequestState::SideEffectsUnknown,
            )));
        }
        outcome
            .downcast::<bool>()
            .map(|result| *result)
            .map_err(|_| RuntimeError::Internal("invalid pending index publication outcome".into()))
    }

    async fn record_pending_entity_failure(
        &self,
        observed: &PendingEntityIndex,
        target: &PendingIndexTarget,
        category: &'static str,
    ) -> RuntimeResult<bool> {
        let id = observed.entity_id;
        let generation = observed.generation;
        let kind = target.kind.clone();
        // Slice 1 schedules no recovery. Preserve any existing delay; the later
        // supervised recovery component owns its bounded backoff calculation.
        let next_eligible_at = target.next_eligible_at;
        let op: AtomicUnitOp = Box::new(move |writer| {
            Box::pin(async move {
                let recorded = pending_index::record_failure(
                    writer,
                    id,
                    generation,
                    &kind,
                    category,
                    next_eligible_at,
                )
                .await?;
                Ok(Box::new(recorded) as Box<dyn Any + Send>)
            })
        });
        self.sql()
            .atomic_unit(op)
            .await?
            .downcast::<bool>()
            .map(|result| *result)
            .map_err(|_| RuntimeError::Internal("invalid pending index failure outcome".into()))
    }

    async fn prepare_pending_entity_target(
        &self,
        token: &NamespaceToken,
        entity: &Entity,
        target: &PendingIndexTargetKind,
        snapshot: &[(String, usize)],
        report: &mut EmbeddingTruncationReport,
    ) -> Result<Vec<SqlStatement>, (&'static str, RuntimeError)> {
        match target {
            PendingIndexTargetKind::Fts => {
                #[cfg(test)]
                if crate::operations::consume_fts_fail_fault(&entity.namespace) {
                    return Err((
                        "fts_prepare",
                        RuntimeError::Internal("injected FTS failure".into()),
                    ));
                }
                self.text(token).map_err(|error| ("fts_prepare", error))?;
                let doc = super::entity_fts_document(entity);
                Ok(khive_db::stores::text::delete_document_statements(
                    "fts_entities",
                    &entity.namespace,
                    entity.id,
                )
                .into_iter()
                .chain(khive_db::stores::text::insert_document_statements(
                    "fts_entities",
                    &doc,
                ))
                .collect())
            }
            PendingIndexTargetKind::Vector { model, dimensions } => {
                let Some((_, captured_dimensions)) =
                    snapshot.iter().find(|(name, _)| name == model)
                else {
                    return Err((
                        "blocked_model_missing",
                        RuntimeError::UnknownModel(model.clone()),
                    ));
                };
                if dimensions.as_slice() != [*captured_dimensions] {
                    return Err((
                        "blocked_dimensions",
                        RuntimeError::InvalidInput("pending model dimensions conflict".into()),
                    ));
                }
                let (storage_name, live_dimensions) = self
                    .vector_model_metadata(model)
                    .map_err(|error| ("blocked_model_missing", error))?;
                if storage_name != *model || live_dimensions != *captured_dimensions {
                    return Err((
                        "blocked_dimensions",
                        RuntimeError::InvalidInput("pending model dimensions changed".into()),
                    ));
                }
                let outcome = self
                    .embed_document_with_model_outcome_for_token(
                        token,
                        model,
                        &super::entity_fts_document(entity).body,
                    )
                    .await
                    .map_err(|error| ("embedding", error))?;
                report.observe(&outcome);
                if outcome.vector.len() != *captured_dimensions
                    || crate::atomic_message::non_finite_index(&outcome.vector).is_some()
                {
                    return Err((
                        "invalid_vector",
                        RuntimeError::InvalidInput("invalid pending entity vector".into()),
                    ));
                }
                // Provider resolution pins the serving registry entry. Recheck after
                // it resolves so a concurrent setup replacement cannot change shape.
                if self
                    .vector_model_metadata(model)
                    .map_err(|error| ("vector_prepare", error))?
                    != (model.clone(), *captured_dimensions)
                {
                    return Err((
                        "blocked_dimensions",
                        RuntimeError::InvalidInput("pending model dimensions changed".into()),
                    ));
                }
                self.vectors_for_model(token, model)
                    .map_err(|error| ("vector_prepare", error))?;
                let table = format!("vec_{}", crate::config::sanitize_key(model));
                Ok(Self::entity_vector_insert_statements(
                    &table,
                    entity,
                    model,
                    &outcome.vector,
                ))
            }
        }
    }

    pub(super) async fn publish_pending_entity_indexes(
        &self,
        token: &NamespaceToken,
        entity: &Entity,
        pending: &PendingEntityIndex,
        snapshot: &[(String, usize)],
    ) -> EntityUpdateReport {
        let mut report = EntityUpdateReport::complete(EmbeddingTruncationReport::default(), true);
        if entity.id != pending.entity_id
            || entity.namespace != pending.namespace
            || entity.version != pending.source_version
            || entity.deleted_at.is_some()
            || entity.merged_into.is_some()
        {
            for target in pending.targets.iter().filter(|target| !target.done) {
                report.indexing.unfinished(&target.kind, "superseded");
            }
            return report;
        }
        let mut unsettled_attempt = false;
        for target in &pending.targets {
            if target.done {
                continue;
            }
            if unsettled_attempt {
                report
                    .indexing
                    .unfinished(&target.kind, "not_attempted_after_unknown");
                continue;
            }
            if target.kind.is_blocked() {
                report
                    .indexing
                    .unfinished(&target.kind, "blocked_dimensions");
                continue;
            }
            if target.next_eligible_at > chrono::Utc::now().timestamp_micros() {
                report.indexing.unfinished(&target.kind, "backoff");
                continue;
            }
            let result = match self
                .prepare_pending_entity_target(
                    token,
                    entity,
                    &target.kind,
                    snapshot,
                    &mut report.embedding,
                )
                .await
            {
                Ok(statements) => {
                    #[cfg(test)]
                    match &target.kind {
                        PendingIndexTargetKind::Fts => {
                            super::race_seam::pause_before_entity_index_publish().await
                        }
                        PendingIndexTargetKind::Vector { .. } => {
                            super::race_seam::pause_before_entity_vector_publish().await
                        }
                    }
                    self.publish_pending_entity_target(pending, &target.kind, statements)
                        .await
                        .map_err(|error| ("index_publish", error))
                }
                Err(error) => Err(error),
            };
            match result {
                Ok(true) => {}
                Ok(false) => report.indexing.unfinished(&target.kind, "superseded"),
                Err((_, error)) if unsettled(&error) => {
                    // No reread, retry metadata write, or absence inference settles
                    // a writer whose acknowledgement was lost.
                    unsettled_attempt = true;
                    report.indexing.unfinished(&target.kind, "outcome_unknown");
                }
                Err((stage, _)) => {
                    match self
                        .record_pending_entity_failure(pending, target, stage)
                        .await
                    {
                        Ok(true) => report.indexing.unfinished(&target.kind, stage),
                        Ok(false) => report.indexing.unfinished(&target.kind, "superseded"),
                        Err(error) => {
                            unsettled_attempt = unsettled(&error);
                            report.indexing.unfinished(
                                &target.kind,
                                if unsettled_attempt {
                                    "failure_record_outcome_unknown"
                                } else {
                                    "failure_record_failed"
                                },
                            );
                        }
                    }
                }
            }
        }
        report
    }
}

#[cfg(test)]
#[path = "entity_pending_index_tests.rs"]
mod tests;
