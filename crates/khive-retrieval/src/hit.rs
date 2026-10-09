//! Search result types returned by hybrid retrieval pipelines.
//!
//! Search evidence and built-in strategy labels are shared here so pipelines can retain
//! their own ranking and duplicate-signal policies without depending on a higher layer.

use std::collections::{hash_map::Entry, HashMap};

use khive_fusion::FusionStrategy;
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
    /// Rank kind for a built-in strategy, or the borrowed custom executor arguments.
    ///
    /// Custom executors declare their own kind. Returning their name and parameters
    /// leaves resolution and fallback policy with the caller; `Err` is not a refusal.
    /// Callers that intentionally fall back to RRF can use `unwrap_or(Self::Rrf)`.
    pub fn of(strategy: &FusionStrategy) -> Result<Self, (&str, &serde_json::Value)> {
        match strategy {
            FusionStrategy::Rrf { .. } | FusionStrategy::WeightedRrf { .. } => Ok(Self::Rrf),
            FusionStrategy::VectorOnly => Ok(Self::Vector),
            FusionStrategy::KeywordOnly => Ok(Self::Keyword),
            FusionStrategy::Weighted { .. } => Ok(Self::Weighted),
            FusionStrategy::Union => Ok(Self::Union),
            FusionStrategy::Custom { name, params } => Err((name, params)),
        }
    }

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

/// How repeated appearances contribute their per-leg signals.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignalMerge {
    /// Keep each first present signal, including a measured zero.
    FirstPresent,
    /// Keep the maximum present signal from each retrieval leg.
    Maximum,
}

/// Accumulate evidence by ID without changing the first hit's score or rank kind.
///
/// Every appearance widens the source and fills missing title/snippet fields, even
/// on a repeated visit from the same leg. Present empty strings are retained.
/// This differs from `combine_leg_first_appearance`, which ignores a repeated leg
/// entirely. Ranking and the final result limit remain the caller's responsibility.
pub fn merge_hit_metadata(
    metadata: &mut HashMap<Uuid, SearchHit>,
    hit: SearchHit,
    signals: SignalMerge,
) {
    match metadata.entry(hit.entity_id) {
        Entry::Occupied(mut entry) => {
            let existing = entry.get_mut();
            existing.source = existing.source.union(hit.source);
            existing.signals.vector_similarity = match signals {
                SignalMerge::FirstPresent => existing
                    .signals
                    .vector_similarity
                    .or(hit.signals.vector_similarity),
                SignalMerge::Maximum => existing
                    .signals
                    .vector_similarity
                    .max(hit.signals.vector_similarity),
            };
            existing.signals.keyword_score = match signals {
                SignalMerge::FirstPresent => {
                    existing.signals.keyword_score.or(hit.signals.keyword_score)
                }
                SignalMerge::Maximum => existing
                    .signals
                    .keyword_score
                    .max(hit.signals.keyword_score),
            };
            if existing.title.is_none() {
                existing.title = hit.title;
            }
            if existing.snippet.is_none() {
                existing.snippet = hit.snippet;
            }
        }
        Entry::Vacant(entry) => {
            entry.insert(hit);
        }
    }
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
