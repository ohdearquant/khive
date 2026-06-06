//! Shared types used across storage capability traits.

use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use khive_types::{EdgeRelation, SubstrateKind};

use crate::error::StorageError;

/// Convenience alias for `Result<T, StorageError>` used throughout this crate.
pub type StorageResult<T> = Result<T, StorageError>;

/// Aggregate outcome of a batch write operation.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BatchWriteSummary {
    pub attempted: u64,
    pub affected: u64,
    pub failed: u64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub first_error: String,
}

/// Controls whether a delete operation removes the record immediately or marks it as deleted.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DeleteMode {
    /// Mark `deleted_at`; record remains queryable with explicit soft-delete filter.
    Soft,
    /// Physically remove the row and cascade incident edges.
    Hard,
}

// -- SQL primitives --

/// A tagged SQL column value that can round-trip through serde and SQLite bindings.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SqlValue {
    Null,
    Bool(bool),
    Integer(i64),
    Float(f64),
    Text(String),
    Blob(Vec<u8>),
    Json(Value),
    Uuid(Uuid),
    Timestamp(DateTime<Utc>),
}

/// A parameterized SQL statement with optional diagnostic label.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SqlStatement {
    pub sql: String,
    pub params: Vec<SqlValue>,
    pub label: Option<String>,
}

/// A single named column in a SQL result row.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SqlColumn {
    pub name: String,
    pub value: SqlValue,
}

/// A row of named columns returned by a raw SQL query.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SqlRow {
    pub columns: Vec<SqlColumn>,
}

impl SqlRow {
    pub fn get(&self, name: &str) -> Option<&SqlValue> {
        self.columns
            .iter()
            .find(|c| c.name == name)
            .map(|c| &c.value)
    }
}

/// Transaction isolation level hint for SQL backends that support it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SqlIsolation {
    Default,
    ReadCommitted,
    RepeatableRead,
    Serializable,
}

/// Options passed to a SQL transaction begin call.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SqlTxOptions {
    pub read_only: bool,
    pub isolation: SqlIsolation,
    pub label: Option<String>,
}

impl Default for SqlTxOptions {
    fn default() -> Self {
        Self {
            read_only: false,
            isolation: SqlIsolation::Default,
            label: None,
        }
    }
}

// -- Vector types --

/// Discriminant for the ANN index algorithm used by a vector backend.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VectorIndexKind {
    Hnsw,
    SqliteVec,
    Flat,
}

/// Backend capability declaration for vector stores.
///
/// Returned by [`VectorStore::capabilities`]. Higher-level retrieval policy
/// (hybrid search, HyDE fan-out, etc.) introspects this struct at construction
/// time to select the optimal code path without relying on error-type matching.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VectorStoreCapabilities {
    /// Supports metadata pre-filter pushdown into the index scan.
    pub supports_filter: bool,
    /// Supports batch search (multiple query vectors in one call).
    pub supports_batch_search: bool,
    /// Supports quantization (reduces memory; may trade recall).
    pub supports_quantization: bool,
    /// Supports in-place update without a delete+insert round-trip.
    pub supports_update: bool,
    /// Supports orphan sweep (deleting vectors with no live subject).
    pub supports_orphan_sweep: bool,
    /// Supports multiple named fields per subject (e.g. `entity.title` and
    /// `entity.body` stored as separate vectors). sqlite-vec backends use a
    /// `subject_id PRIMARY KEY` table and therefore only support one vector
    /// per subject per namespace — this field is `false` for those backends.
    #[serde(default)]
    pub supports_multi_field: bool,
    /// Maximum supported embedding dimension, or `None` if unbounded.
    pub max_dimensions: Option<u32>,
    /// Index algorithms available in this backend.
    pub index_kinds: Vec<VectorIndexKind>,
}

/// A typed predicate for backend-pushable metadata filtering.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct VectorMetadataFilter {
    /// Restrict to these namespaces.
    pub namespaces: Vec<String>,
    /// Restrict to these substrate kinds.
    pub kinds: Vec<SubstrateKind>,
    /// Typed property predicates.
    pub property_filters: Vec<PropertyFilter>,
}

impl VectorMetadataFilter {
    /// Returns `true` when no predicates are set (filter is a no-op).
    pub fn is_empty(&self) -> bool {
        self.namespaces.is_empty() && self.kinds.is_empty() && self.property_filters.is_empty()
    }
}

/// A single typed metadata predicate used in [`VectorMetadataFilter`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PropertyFilter {
    pub key: String,
    pub op: PropertyOp,
    pub value: serde_json::Value,
}

/// Comparison operators for [`PropertyFilter`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PropertyOp {
    Eq,
    Ne,
    In,
    Range,
    Exists,
}

/// A single vector embedding record for bulk insert operations.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VectorRecord {
    pub subject_id: Uuid,
    pub kind: SubstrateKind,
    pub namespace: String,
    /// Which embedding field this record represents (e.g. `"entity.body"`).
    pub field: String,
    #[serde(default)]
    pub embedding_model: Option<String>,
    /// One or many dense vectors; sqlite-vec backends enforce `vectors.len() == 1`.
    pub vectors: Vec<Vec<f32>>,
    pub updated_at: DateTime<Utc>,
}

/// Parameters for a nearest-neighbor similarity search.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VectorSearchRequest {
    /// One or many query vectors; sqlite-vec backends enforce `query_vectors.len() == 1`.
    pub query_vectors: Vec<Vec<f32>>,
    pub top_k: u32,
    pub namespace: Option<String>,
    pub kind: Option<SubstrateKind>,
    /// Restrict results to this embedding model. Defaults to the store's own model.
    #[serde(default)]
    pub embedding_model: Option<String>,
    /// Optional metadata filter for backends that support pushdown.
    pub filter: Option<VectorMetadataFilter>,
    /// Backend-specific hints (opaque JSON blob, ignored by default).
    pub backend_hints: Option<serde_json::Value>,
}

impl VectorSearchRequest {
    /// Validate documented invariants: non-empty query vectors, finite values,
    /// and non-zero `top_k`.
    ///
    /// Returns `Err` with a human-readable description of the first violation.
    pub fn validate(&self) -> Result<(), String> {
        if self.query_vectors.is_empty() {
            return Err("VectorSearchRequest: query_vectors must not be empty".into());
        }
        if self.top_k == 0 {
            return Err("VectorSearchRequest: top_k must be > 0".into());
        }
        for (qi, qvec) in self.query_vectors.iter().enumerate() {
            for (vi, &v) in qvec.iter().enumerate() {
                if !v.is_finite() {
                    return Err(format!(
                        "VectorSearchRequest: query_vectors[{qi}][{vi}] is non-finite ({v})"
                    ));
                }
            }
        }
        Ok(())
    }
}

/// Configuration for a vector orphan-sweep pass.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OrphanSweepConfig {
    /// Optional allowlist of subject IDs to check. `None` = scan all rows.
    /// `Some(ids)` restricts the sweep to only those IDs; rows not in the list
    /// are untouched even if orphaned.
    pub subject_id_allowlist: Option<Vec<Uuid>>,
    pub namespaces: Vec<String>,
    pub substrate_kinds: Vec<SubstrateKind>,
    pub max_delete: u32,
    pub dry_run: bool,
}

/// Result of a vector orphan-sweep pass.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OrphanSweepResult {
    pub scanned: u64,
    pub deleted: u64,
    pub would_delete: u64,
    pub max_delete_hit: bool,
}

// -- Sparse vector types --

/// A sparse vector represented as parallel indices and values arrays.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SparseVector {
    /// Dimension indices (must be strictly increasing).
    pub indices: Vec<u32>,
    /// Corresponding non-zero values (must be finite).
    pub values: Vec<f32>,
}

impl SparseVector {
    /// Validate the documented invariants: equal-length arrays, strictly
    /// increasing indices, and all values finite.
    ///
    /// Returns `Err` with a human-readable description of the first violation.
    pub fn validate(&self) -> Result<(), String> {
        if self.indices.len() != self.values.len() {
            return Err(format!(
                "SparseVector: indices.len() ({}) != values.len() ({})",
                self.indices.len(),
                self.values.len()
            ));
        }
        for (i, &val) in self.values.iter().enumerate() {
            if !val.is_finite() {
                return Err(format!("SparseVector: values[{i}] is non-finite ({val})"));
            }
        }
        for w in self.indices.windows(2) {
            if w[0] >= w[1] {
                return Err(format!(
                    "SparseVector: indices not strictly increasing at [{}, {}]",
                    w[0], w[1]
                ));
            }
        }
        Ok(())
    }
}

/// A single sparse vector embedding record for bulk insert operations.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SparseRecord {
    pub subject_id: Uuid,
    pub kind: SubstrateKind,
    pub namespace: String,
    pub field: String,
    pub vector: SparseVector,
    pub updated_at: DateTime<Utc>,
}

/// Parameters for a sparse nearest-neighbor similarity search.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SparseSearchRequest {
    pub query: SparseVector,
    pub top_k: u32,
    pub namespace: Option<String>,
    pub kind: Option<SubstrateKind>,
}

/// A single ranked result from a sparse similarity search.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SparseSearchHit {
    pub subject_id: Uuid,
    pub score: khive_score::DeterministicScore,
    pub rank: u32,
}

/// A single ranked result from a dense vector similarity search.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VectorSearchHit {
    pub subject_id: Uuid,
    pub score: khive_score::DeterministicScore,
    pub rank: u32,
}

/// Metadata and health summary for a vector index backend.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VectorStoreInfo {
    pub model_name: String,
    pub dimensions: usize,
    pub index_kind: VectorIndexKind,
    pub entry_count: u64,
    pub needs_rebuild: bool,
    pub last_rebuild_at: Option<DateTime<Utc>>,
}

// -- Text gather types (candidate-gather optimization, additive) --

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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TextTermStats {
    pub term: String,
    pub sanitized_term: String,
    pub document_frequency: u64,
    pub document_count: u64,
    /// Robertson-Walker IDF: $\ln\!\left(\frac{N - df + 0.5}{df + 0.5} + 1\right)$
    pub inverse_document_frequency: f64,
}

// -- Text search types --

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

/// Parameters for a full-text similarity search.
#[derive(Clone, Debug, Serialize, Deserialize)]
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

// -- Pagination --

/// Offset-based pagination cursor for list operations.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PageRequest {
    pub offset: u64,
    pub limit: u32,
}

impl Default for PageRequest {
    fn default() -> Self {
        Self {
            offset: 0,
            limit: 50,
        }
    }
}

/// A paginated result slice with an optional total count.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub total: Option<u64>,
}

// -- Graph types --

/// A type-safe link ID (wraps Uuid).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LinkId(pub Uuid);

impl From<Uuid> for LinkId {
    fn from(u: Uuid) -> Self {
        Self(u)
    }
}

impl From<LinkId> for Uuid {
    fn from(l: LinkId) -> Uuid {
        l.0
    }
}

impl fmt::Display for LinkId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// A directed edge in the graph.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Edge {
    pub id: LinkId,
    pub namespace: String,
    pub source_id: Uuid,
    pub target_id: Uuid,
    pub relation: EdgeRelation,
    pub weight: f64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub deleted_at: Option<DateTime<Utc>>,
    pub metadata: Option<Value>,
    pub target_backend: Option<String>,
}

/// Edge traversal direction relative to the source node.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    #[default]
    Out,
    In,
    Both,
}

/// An inclusive time window for filtering records by timestamp.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TimeRange {
    pub start: Option<DateTime<Utc>>,
    pub end: Option<DateTime<Utc>>,
}

/// Filter to restrict a graph edge query to a matching subset.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct EdgeFilter {
    pub ids: Vec<LinkId>,
    pub source_ids: Vec<Uuid>,
    pub target_ids: Vec<Uuid>,
    pub relations: Vec<EdgeRelation>,
    pub min_weight: Option<f64>,
    pub max_weight: Option<f64>,
    pub created_at: Option<TimeRange>,
}

impl EdgeFilter {
    /// Validate that weight bounds are finite and ordered correctly.
    ///
    /// Returns `Err` with a human-readable description of the first violation.
    pub fn validate(&self) -> Result<(), String> {
        if let Some(w) = self.min_weight {
            if !w.is_finite() {
                return Err(format!("EdgeFilter: min_weight is non-finite ({w})"));
            }
        }
        if let Some(w) = self.max_weight {
            if !w.is_finite() {
                return Err(format!("EdgeFilter: max_weight is non-finite ({w})"));
            }
        }
        if let (Some(lo), Some(hi)) = (self.min_weight, self.max_weight) {
            if lo > hi {
                return Err(format!("EdgeFilter: min_weight ({lo}) > max_weight ({hi})"));
            }
        }
        Ok(())
    }
}

/// Selects which edge attribute is used for sorting results.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EdgeSortField {
    CreatedAt,
    Weight,
    Relation,
}

/// Ascending or descending sort order.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SortDirection {
    Asc,
    Desc,
}

/// A sort specification pairing a field discriminant with a direction.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SortOrder<F> {
    pub field: F,
    pub direction: SortDirection,
}

/// Parameters for a single-hop graph neighbor lookup.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NeighborQuery {
    pub direction: Direction,
    pub relations: Option<Vec<EdgeRelation>>,
    pub limit: Option<u32>,
    pub min_weight: Option<f64>,
}

/// One neighbor returned by a graph query.
///
/// Field naming (#148): on the JSON wire, the node identifier is serialized as
/// `id` (not `node_id`) so it matches the verb-wide identifier convention.
/// Internal Rust code still uses `.node_id` on the struct.
///
/// Enrichment (#162): `name` and `kind` are populated by the runtime layer
/// after the storage call returns. Storage `GraphStore` impls leave them
/// `None`; the runtime batch-fetches the entity rows and fills them in.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NeighborHit {
    #[serde(rename = "id")]
    pub node_id: Uuid,
    pub edge_id: Uuid,
    pub relation: EdgeRelation,
    pub weight: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

/// BFS traversal configuration controlling depth, direction, and edge filters.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TraversalOptions {
    pub max_depth: usize,
    pub direction: Direction,
    pub relations: Option<Vec<EdgeRelation>>,
    pub min_weight: Option<f64>,
    pub limit: Option<u32>,
}

impl TraversalOptions {
    pub fn new(max_depth: usize) -> Self {
        Self {
            max_depth,
            ..Default::default()
        }
    }

    pub fn with_direction(mut self, d: Direction) -> Self {
        self.direction = d;
        self
    }
}

/// A graph traversal request from a set of root nodes.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TraversalRequest {
    pub roots: Vec<Uuid>,
    pub options: TraversalOptions,
    pub include_roots: bool,
}

/// One node along a traversal path.
///
/// Field naming (#148): JSON wire serialization is `id`. Enrichment (#162):
/// `name`/`kind` are filled by the runtime layer after the storage call.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PathNode {
    #[serde(rename = "id")]
    pub node_id: Uuid,
    pub via_edge: Option<Uuid>,
    pub depth: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

/// A complete traversal path from one root node to its reachable descendants.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GraphPath {
    pub root_id: Uuid,
    pub nodes: Vec<PathNode>,
    pub total_weight: f64,
}
