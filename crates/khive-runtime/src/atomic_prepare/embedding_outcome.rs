use super::{apply_one_post_commit_effect, KhiveRuntime, NamespaceToken};
use crate::atomic_plan::PostCommitEffect;
use crate::atomic_runner::CommittedPostCommitEffects;
use crate::error::RuntimeError;
use crate::retrieval::EmbeddingTruncationReport;

/// The eligible-model stage that prevented a note's vector refresh.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReindexModelStage {
    Embedding,
    VectorValidation,
    VectorStore,
    VectorPublication,
}

impl ReindexModelStage {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Embedding => "embedding",
            Self::VectorValidation => "vector_validation",
            Self::VectorStore => "vector_store",
            Self::VectorPublication => "vector_publication",
        }
    }
}

/// An eligible-model failure after the note and its lexical index committed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReindexModelFailure {
    pub model: String,
    pub stage: ReindexModelStage,
    pub error: String,
}

/// Metadata produced by a reindex effect whose fail-closed stages succeeded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PostCommitEmbeddingOutcome {
    pub effect: PostCommitEffect,
    pub truncation: EmbeddingTruncationReport,
    /// Partial vector failures do not undo a committed mutation. Report-aware
    /// callers can disclose them while legacy callers retain best-effort success.
    pub failures: Vec<ReindexModelFailure>,
}

/// Everything one committed unit's post-commit pass produced. A failing effect
/// never discards the outcomes of the effects that completed, so callers can
/// still disclose their model failures and truncation advisories.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PostCommitEffectsReport {
    /// Outcomes of the reindex effects that completed, in effect order.
    pub outcomes: Vec<PostCommitEmbeddingOutcome>,
    /// One `effect[<index>] <effect>: <error>` entry per effect that failed.
    pub failures: Vec<String>,
}

impl PostCommitEffectsReport {
    /// The aggregate error for the effects that failed, or `None` when every
    /// effect completed.
    pub fn failure_error(&self) -> Option<RuntimeError> {
        if self.failures.is_empty() {
            return None;
        }
        Some(RuntimeError::Internal(format!(
            "post-commit effects failed after commit: {}",
            self.failures.join("; ")
        )))
    }
}

/// Failure-preserving form of [`super::apply_post_commit_effects_with_report`].
/// Runs every effect, collecting the outcomes of those that completed and the
/// error of each that did not, instead of replacing the outcomes with the
/// first failure.
pub async fn apply_post_commit_effects_with_failures(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    effects: CommittedPostCommitEffects,
) -> PostCommitEffectsReport {
    let mut report = PostCommitEffectsReport {
        outcomes: Vec::new(),
        failures: Vec::new(),
    };
    for (index, effect) in effects.into_effects().into_iter().enumerate() {
        let identity = format!("{effect:?}");
        match apply_one_post_commit_effect(runtime, token, effect).await {
            Ok(Some(outcome)) => report.outcomes.push(outcome),
            Ok(None) => {}
            Err(error) => report
                .failures
                .push(format!("effect[{index}] {identity}: {error}")),
        }
    }
    report
}
