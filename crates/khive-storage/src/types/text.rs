//! Full-text search types: documents, queries, results, and index metadata.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use khive_types::SubstrateKind;

/// Controls how BM25 candidate rows are gathered before final ranking.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TextGatherMode {
    /// Current behavior: ORDER BY rank LIMIT top_k.
    #[default]
    Ranked,
    /// Cheap gather without BM25 ranking; uniform text score 1.0.
    Unranked,
    /// Gather gather_limit rowids without ranking, then BM25-rank only that subset.
    RankWithinCap,
}

/// Options that tune the two-stage gather + rank strategy for text search.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct TextSearchOptions {
    pub gather_mode: TextGatherMode,
    /// Row limit for the cheap first-stage gather in RankWithinCap mode.
    /// Must be >= top_k. When None, defaults to top_k (no breadth reduction).
    pub gather_limit: Option<u32>,
}

impl Default for TextSearchOptions {
    fn default() -> Self {
        Self {
            gather_mode: TextGatherMode::Ranked,
            gather_limit: None,
        }
    }
}

/// Request to compute per-term document frequency and IDF statistics.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TextTermStatsRequest {
    pub terms: Vec<String>,
    pub filter: Option<TextFilter>,
}

/// Per-term document frequency and IDF statistics.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TextTermStats {
    pub term: String,
    pub sanitized_term: String,
    pub document_frequency: u64,
    pub document_count: u64,
    /// Robertson-Walker IDF: $\ln\!\left(\frac{N - df + 0.5}{df + 0.5} + 1\right)$
    pub inverse_document_frequency: f64,
}

/// A text document to be indexed for full-text search.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TextDocument {
    pub subject_id: Uuid,
    pub kind: SubstrateKind,
    pub namespace: String,
    pub title: Option<String>,
    pub body: String,
    pub tags: Vec<String>,
    pub metadata: Option<Value>,
    pub updated_at: DateTime<Utc>,
}

/// Filter to restrict text search results to a specific set of documents.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TextFilter {
    pub ids: Vec<Uuid>,
    pub kinds: Vec<SubstrateKind>,
    pub namespaces: Vec<String>,
}

/// Controls how the query string is parsed and matched against the FTS index.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TextQueryMode {
    Plain,
    Phrase,
    /// OR-join: each whitespace-separated token is matched independently.
    /// Semantically equivalent to N Plain probes joined by OR but in one query.
    AnyTerm,
}

/// Raw deserialization target for [`TextSearchRequest`].
#[derive(Deserialize)]
struct TextSearchRequestRaw {
    query: String,
    mode: TextQueryMode,
    filter: Option<TextFilter>,
    top_k: u32,
    snippet_chars: usize,
}

impl TryFrom<TextSearchRequestRaw> for TextSearchRequest {
    type Error = String;

    fn try_from(raw: TextSearchRequestRaw) -> Result<Self, Self::Error> {
        if raw.top_k == 0 {
            return Err("TextSearchRequest: top_k must be > 0".into());
        }
        Ok(Self {
            query: raw.query,
            mode: raw.mode,
            filter: raw.filter,
            top_k: raw.top_k,
            snippet_chars: raw.snippet_chars,
        })
    }
}

/// Parameters for a full-text similarity search. Deserialization rejects top_k = 0.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(try_from = "TextSearchRequestRaw")]
pub struct TextSearchRequest {
    pub query: String,
    pub mode: TextQueryMode,
    pub filter: Option<TextFilter>,
    pub top_k: u32,
    pub snippet_chars: usize,
}

/// A single ranked result from a full-text search.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TextSearchHit {
    pub subject_id: Uuid,
    pub score: khive_score::DeterministicScore,
    pub rank: u32,
    pub title: Option<String>,
    pub snippet: Option<String>,
}

/// Metadata and health summary for a text index backend.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TextIndexStats {
    pub document_count: u64,
    pub needs_rebuild: bool,
    pub last_rebuild_at: Option<DateTime<Utc>>,
}

/// Controls which entries are included in an index rebuild operation.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexRebuildScope {
    Full,
    Entities(Vec<Uuid>),
}
