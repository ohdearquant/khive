//! Notify current committed notes even when their indexing reports failure.
use super::{KhiveRuntime, NamespaceToken, PostCommitEffect, PostCommitEmbeddingOutcome};
use crate::error::RuntimeResult;
use uuid::Uuid;

pub(super) async fn apply(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    note_id: Uuid,
    version: i64,
) -> RuntimeResult<Option<PostCommitEmbeddingOutcome>> {
    let Some(note) = runtime.notes(token)?.get_note(note_id).await? else {
        return Ok(None);
    };
    if note.version != version {
        return Ok(None);
    }
    let reindex = runtime.reindex_note(token, &note).await;
    if runtime
        .notes(token)?
        .get_note(note_id)
        .await?
        .is_none_or(|current| current.version != version)
    {
        return Ok(None);
    }
    // The hook describes the committed current mutation, including a partial
    // or wholly failed reindex. Preserve its error after notifying consumers.
    runtime.fire_note_mutation_hook(&note.kind, note.id).await;
    reindex.map(|truncation| {
        Some(PostCommitEmbeddingOutcome {
            effect: PostCommitEffect::ReindexNote { note_id, version },
            truncation,
        })
    })
}
