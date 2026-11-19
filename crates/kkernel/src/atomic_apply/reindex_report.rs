use super::{AtomicDegradation, PostCommitEffect, Value};
use serde_json::json;

pub(super) fn add_post_commit_embedding_warning(
    result: &mut Value,
    effect: Option<&PostCommitEffect>,
    outcomes: &[khive_runtime::atomic_prepare::PostCommitEmbeddingOutcome],
) {
    // More than one atomic update may schedule the same target effect. Treat
    // those outcomes as one aggregate advisory: a late model registration can
    // make a later duplicate reindex truncate even when the first did not, and
    // first-match lookup would silently lose that real outcome.
    let truncated = effect.is_some_and(|effect| {
        outcomes
            .iter()
            .filter(|outcome| &outcome.effect == effect)
            .any(|outcome| outcome.truncation.any_truncated())
    });
    if !truncated {
        return;
    }
    if let Some(object) = result.as_object_mut() {
        object.insert(
            "warnings".to_string(),
            json!([khive_runtime::retrieval::EMBEDDING_INPUT_TRUNCATED_WARNING]),
        );
    }
}

pub(super) fn model_degradations(
    outcomes: &[khive_runtime::atomic_prepare::PostCommitEmbeddingOutcome],
) -> Vec<AtomicDegradation> {
    outcomes
        .iter()
        .filter_map(|outcome| {
            if outcome.failures.is_empty() {
                return None;
            }
            let failures = outcome
                .failures
                .iter()
                .map(|failure| {
                    format!(
                        "model {} {}: {}",
                        failure.model,
                        failure.stage.as_str(),
                        failure.error
                    )
                })
                .collect::<Vec<_>>()
                .join("; ");
            Some(AtomicDegradation::post_commit_reindex(anyhow::anyhow!(
                "{:?}: {failures}",
                outcome.effect
            )))
        })
        .collect()
}
