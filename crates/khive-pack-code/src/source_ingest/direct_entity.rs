//! Guarded entity writes shared by source-map ingestion paths.

use super::{
    BlockedWrite, CodeSourceIngestError, CodeSourceIngestReport, Entity, KhiveRuntime,
    NamespaceToken, RowMutationOutcome, RuntimeError, Uuid, MAX_ROW_REBASE_ATTEMPTS,
};
use khive_runtime::{
    entity_fts_document, EntityCandidateAdmission, EntityCandidateContext, EntityCandidateMutation,
    EntityCandidateOrigin,
};

pub(super) fn candidate_context(namespace: &str) -> EntityCandidateContext {
    EntityCandidateContext::new(namespace, EntityCandidateOrigin::CodeIngest)
}

pub(super) fn gate_check_with_context(
    context: &EntityCandidateContext,
    entity: &Entity,
) -> Result<(), RuntimeError> {
    context.clone().prepare(entity.clone()).map(|_| ())
}

#[cfg(test)]
pub(super) fn gate_check(entity: &Entity) -> Result<(), RuntimeError> {
    gate_check_with_context(&candidate_context(&entity.namespace), entity)
}

fn record_refusal(
    error: RuntimeError,
    file: &str,
    report: &mut CodeSourceIngestReport,
) -> Result<RowMutationOutcome, CodeSourceIngestError> {
    match error {
        RuntimeError::SecretDetected(secret) => {
            report.blocked_count += 1;
            report.blocked.push(BlockedWrite {
                file: file.to_string(),
                detector: secret.detector.to_string(),
                masked_excerpt: secret.masked,
            });
            Ok(RowMutationOutcome::Blocked)
        }
        other => Err(other.into()),
    }
}

fn record_fts_write(entity: &Entity, report: &mut CodeSourceIngestReport) {
    #[cfg(test)]
    super::l2_batch_tests::observe_fts_write(entity.id);
    #[cfg(test)]
    super::l2_recovery_tests::observe_fts_write();
    #[cfg(not(test))]
    let _ = entity;
    report.fts_indexed += 1;
}

async fn index_entity(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    entity: &Entity,
    report: &mut CodeSourceIngestReport,
) -> Result<(), CodeSourceIngestError> {
    rt.text(token)?
        .upsert_document(entity_fts_document(entity))
        .await
        .map_err(|error| CodeSourceIngestError::Storage(format!("entity FTS indexing: {error}")))?;
    record_fts_write(entity, report);
    Ok(())
}

pub(super) fn advancing_entity_revision(
    requested: i64,
    current: i64,
) -> Result<i64, CodeSourceIngestError> {
    let minimum = current.checked_add(1).ok_or_else(|| {
        CodeSourceIngestError::Storage(format!(
            "entity revision {current} cannot advance past i64::MAX"
        ))
    })?;
    Ok(requested.max(minimum))
}

pub(super) async fn mutate_entity<F>(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    id: Uuid,
    file: &str,
    report: &mut CodeSourceIngestReport,
    apply: F,
) -> Result<RowMutationOutcome, CodeSourceIngestError>
where
    F: FnMut(Option<&Entity>) -> Option<Entity>,
{
    let context = candidate_context(token.namespace().as_str());
    mutate_entity_with_context(rt, token, &context, id, file, report, apply).await
}

pub(super) async fn mutate_entity_with_context<F>(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    context: &EntityCandidateContext,
    id: Uuid,
    file: &str,
    report: &mut CodeSourceIngestReport,
    mut apply: F,
) -> Result<RowMutationOutcome, CodeSourceIngestError>
where
    F: FnMut(Option<&Entity>) -> Option<Entity>,
{
    let store = rt.entities(token)?;
    for _ in 0..MAX_ROW_REBASE_ATTEMPTS {
        let current = store
            .get_entity_including_deleted(id)
            .await
            .map_err(|error| CodeSourceIngestError::Storage(error.to_string()))?;
        #[cfg(test)]
        super::l2_batch_tests::observe_row_read(id);
        #[cfg(test)]
        super::race_seam::pause_after_row_read().await;
        #[cfg(test)]
        super::l2_recovery_tests::after_entity_read(id).await;
        let Some(mut replacement) = apply(current.as_ref()) else {
            return Ok(RowMutationOutcome::Unchanged);
        };
        if replacement.id != id {
            return Err(CodeSourceIngestError::Storage(format!(
                "entity mutation for {id} produced replacement {}",
                replacement.id
            )));
        }
        replacement.deleted_at = None;
        let (mutation, outcome) = if let Some(snapshot) = current.as_ref() {
            replacement.created_at = snapshot.created_at;
            replacement.version = snapshot.version;
            replacement.updated_at =
                advancing_entity_revision(replacement.updated_at, snapshot.updated_at)?;
            (
                EntityCandidateMutation::ReplaceIfUnchanged {
                    expected: snapshot.clone(),
                    expected_version: None,
                },
                RowMutationOutcome::Updated,
            )
        } else {
            (
                EntityCandidateMutation::CreateIfAbsent,
                RowMutationOutcome::Created,
            )
        };
        // Each retry rescans its rebased final candidate using the same snapshot
        // as any earlier per-item checks. No admission result is reused for new bytes.
        let prepared = match context.clone().prepare(replacement) {
            Ok(prepared) => prepared,
            Err(error) => return record_refusal(error, file, report),
        };
        let (entity, finalized) = match rt
            .try_commit_manifest_entity_candidate(token, prepared, mutation)
            .await?
        {
            EntityCandidateAdmission::Conflict => continue,
            EntityCandidateAdmission::Committed(entity) => (entity, true),
            EntityCandidateAdmission::Legacy(entity) => {
                let wrote = if let Some(snapshot) = current.as_ref() {
                    store
                        .replace_entity_if_unchanged(
                            entity.clone(),
                            snapshot.updated_at,
                            snapshot.deleted_at,
                        )
                        .await
                } else {
                    store.insert_entity_if_absent(entity.clone()).await
                }
                .map_err(|error| CodeSourceIngestError::Storage(error.to_string()))?;
                if !wrote {
                    continue;
                }
                (entity, false)
            }
        };
        #[cfg(test)]
        super::l2_batch_tests::observe_row_write(id);
        #[cfg(test)]
        super::l2_recovery_tests::after_entity_commit(&entity);
        if finalized {
            // The runtime committed this FTS write with the stamped row and audit.
            record_fts_write(&entity, report);
        } else {
            index_entity(rt, token, &entity, report).await?;
        }
        return Ok(outcome);
    }
    Err(CodeSourceIngestError::Storage(format!(
        "entity {id} changed during all {MAX_ROW_REBASE_ATTEMPTS} code-map rebase attempts"
    )))
}
