use super::{
    apply_post_commit_effects_with_failures, note_reindex_effect, CommittedPostCommitEffects,
    KhiveRuntime, NamespaceToken, PostCommitEffect, PostCommitEmbeddingOutcome, RuntimeResult,
};

/// Run every deferred [`PostCommitEffect`] after a committed atomic unit.
pub async fn apply_post_commit_effects(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    effects: CommittedPostCommitEffects,
) -> RuntimeResult<()> {
    apply_post_commit_effects_with_report(runtime, token, effects)
        .await
        .map(|_| ())
}

/// Embedding-reporting form of [`apply_post_commit_effects`]. Re-fetches each
/// target's now-committed row outside any transaction and reuses the existing
/// `reindex_entity`/`reindex_note` (FTS + embedding, same as the non-atomic
/// path) for exact parity. Returns the typed embedding outcome for each reindex
/// effect so callers can preserve truncation advisories and partial model
/// failures instead of discarding them after commit. Model failures remain
/// best-effort; lexical indexing and excluded-model cleanup errors propagate.
/// When any effect fails, the other effects' outcomes are dropped with the
/// returned error; [`apply_post_commit_effects_with_failures`] keeps both.
pub async fn apply_post_commit_effects_with_report(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    effects: CommittedPostCommitEffects,
) -> RuntimeResult<Vec<PostCommitEmbeddingOutcome>> {
    let report = apply_post_commit_effects_with_failures(runtime, token, effects).await;
    match report.failure_error() {
        Some(error) => Err(error),
        None => Ok(report.outcomes),
    }
}

pub(super) async fn apply_one_post_commit_effect(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    effect: PostCommitEffect,
) -> RuntimeResult<Option<PostCommitEmbeddingOutcome>> {
    match effect {
        PostCommitEffect::None => Ok(None),
        PostCommitEffect::NoteChanged { note_id, kind } => {
            runtime.fire_note_mutation_hook(&kind, note_id).await;
            Ok(None)
        }
        PostCommitEffect::ReindexEntity { entity_id } => {
            let Some(entity) = runtime.entities(token)?.get_entity(entity_id).await? else {
                return Ok(None);
            };
            let truncation = runtime.reindex_entity(token, &entity).await?;
            Ok(Some(PostCommitEmbeddingOutcome {
                effect: PostCommitEffect::ReindexEntity { entity_id },
                truncation,
                failures: Vec::new(),
            }))
        }
        PostCommitEffect::ReindexNote { note_id, version } => {
            note_reindex_effect::apply(runtime, token, note_id, version).await
        }
        PostCommitEffect::NoteDeleted { note_id, kind } => {
            // The committed row may already be gone; use the captured kind.
            runtime.fire_note_mutation_hook(&kind, note_id).await;
            Ok(None)
        }
        PostCommitEffect::GtdAudit { .. } => {
            // The kkernel caller owns the GTD pack's separate audit side write.
            Ok(None)
        }
    }
}
