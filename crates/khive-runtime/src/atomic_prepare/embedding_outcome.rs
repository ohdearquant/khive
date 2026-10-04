use crate::atomic_plan::PostCommitEffect;
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
