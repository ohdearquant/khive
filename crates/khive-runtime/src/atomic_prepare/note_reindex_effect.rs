//! The post-commit note reindex effect: consumers of the note-mutation hook
//! hear about a committed note change even when its reindex reports failure.
use khive_storage::note::Note;

use super::{KhiveRuntime, NamespaceToken, PostCommitEffect, PostCommitEmbeddingOutcome};
use crate::curation::note_reindex::NoteReindexReport;
use crate::error::RuntimeResult;
#[cfg(test)]
use crate::retrieval::EmbeddingTruncationReport;
use uuid::Uuid;

/// Reindex the committed note at `version`, then notify the hook if that
/// version is still current. A stale or missing note is skipped without
/// reindexing, and a reindex failure is always returned to the caller.
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
    let reindex = runtime.reindex_note_with_report(token, &note).await;
    notify_if_current(runtime, token, &note, reindex).await
}

/// Atomic note updates bypass the regular `update_note` hook, so in-process
/// consumers are notified here. Fire the hook once when `note` is still the
/// current version after its reindex ran, then hand back the reindex result
/// unchanged. When the version moved during the reindex nothing is fired: a
/// successful reindex yields no outcome and a failed one still returns its
/// error.
async fn notify_if_current(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    note: &Note,
    reindex: RuntimeResult<impl Into<NoteReindexReport>>,
) -> RuntimeResult<Option<PostCommitEmbeddingOutcome>> {
    let current = match runtime.notes(token)?.get_note(note.id).await {
        Ok(current) => current,
        // A failed re-read must not hide the reindex failure.
        Err(error) => return reindex.and(Err(error.into())),
    };
    if current.is_none_or(|current| current.version != note.version) {
        return reindex.map(|_| None);
    }
    runtime.fire_note_mutation_hook(&note.kind, note.id).await;
    reindex.map(|report| {
        let report = report.into();
        Some(PostCommitEmbeddingOutcome {
            effect: PostCommitEffect::ReindexNote {
                note_id: note.id,
                version: note.version,
            },
            truncation: report.truncation,
            failures: report.failures,
        })
    })
}

#[cfg(test)]
#[path = "note_reindex_effect_tests.rs"]
mod tests;
