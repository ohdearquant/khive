//! Search result types returned by hybrid retrieval pipelines.
//!
//! These types depend only on `uuid` and `khive-score`, so any pipeline that produces
//! ranked hits can return them without depending on a higher layer.

use khive_score::DeterministicScore;
use uuid::Uuid;

/// The strategy that produced a hit's ordering score, including local modifiers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RankScoreKind {
    /// Reciprocal rank fusion of the retrieval legs.
    Rrf,
    /// Vector similarity alone.
    Vector,
    /// Keyword score alone.
    Keyword,
    /// Weighted combination of the retrieval legs.
    Weighted,
    /// Union of the retrieval legs.
    Union,
}

impl RankScoreKind {
    /// Lowercase wire representation used by search serializers.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Rrf => "rrf",
            Self::Vector => "vector",
            Self::Keyword => "keyword",
            Self::Weighted => "weighted",
            Self::Union => "union",
        }
    }
}

/// Retained component scores before fusion and strategy-local modifiers.
/// An absent retrieval leg has no score, which is distinct from a measured zero.
/// Scores belong to the backend and model that produced the retained hit;
/// vector similarities from different embedding models are not comparable.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SearchSignals {
    /// Vector similarity, or `None` when the vector leg did not return the hit.
    pub vector_similarity: Option<DeterministicScore>,
    /// Keyword score, or `None` when the text leg did not return the hit.
    pub keyword_score: Option<DeterministicScore>,
}

/// A unified search result combining vector and text signals.
#[derive(Clone, Debug)]
pub struct SearchHit {
    /// Identifier of the matched record.
    pub entity_id: Uuid,
    /// Ordering score assigned by the fusion strategy.
    pub score: DeterministicScore,
    /// The strategy that produced `score`.
    pub rank_score_kind: RankScoreKind,
    /// Component scores retained before fusion.
    pub signals: SearchSignals,
    /// The retrieval leg or legs that returned the hit.
    pub source: SearchSource,
    /// Title of the matched record, when the text leg carried one.
    pub title: Option<String>,
    /// Excerpt of the matched text, when the text leg carried one.
    pub snippet: Option<String>,
}

/// Result of a hybrid search: the fused hits — text hits alone when the vector
/// arm failed — plus the vector arm's error, if any.
#[derive(Clone, Debug)]
pub struct HybridSearchOutcome {
    /// The fused hits.
    pub hits: Vec<SearchHit>,
    /// The vector arm's error message, when that arm failed.
    pub vector_error: Option<String>,
}

/// Which retrieval path(s) contributed to a hit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchSource {
    /// Returned by the vector leg only.
    Vector,
    /// Returned by the text leg only.
    Text,
    /// Returned by both legs.
    Both,
}

impl SearchSource {
    /// Combine retrieval-leg membership from two appearances of the same hit.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        match (self, other) {
            (Self::Text, Self::Text) => Self::Text,
            (Self::Vector, Self::Vector) => Self::Vector,
            _ => Self::Both,
        }
    }

    /// Lowercase wire representation used by search serializers.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Vector => "vector",
            Self::Text => "text",
            Self::Both => "both",
        }
    }
}
