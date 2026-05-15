//! Shared types used across storage capability traits.

use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use khive_types::{EdgeRelation, SubstrateKind};

use crate::error::StorageError;

pub type StorageResult<T> = Result<T, StorageError>;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BatchWriteSummary {
    pub attempted: u64,
    pub affected: u64,
    pub failed: u64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub first_error: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DeleteMode {
    Soft,
    Hard,
}

// -- SQL primitives --

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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SqlStatement {
    pub sql: String,
    pub params: Vec<SqlValue>,
    pub label: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SqlColumn {
    pub name: String,
    pub value: SqlValue,
}

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

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SqlIsolation {
    Default,
    ReadCommitted,
    RepeatableRead,
    Serializable,
}

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

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VectorIndexKind {
    Hnsw,
    SqliteVec,
    Flat,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VectorRecord {
    pub subject_id: Uuid,
    pub kind: SubstrateKind,
    pub namespace: String,
    pub embedding: Vec<f32>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VectorSearchRequest {
    pub query_embedding: Vec<f32>,
    pub top_k: u32,
    pub namespace: Option<String>,
    pub kind: Option<SubstrateKind>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VectorSearchHit {
    pub subject_id: Uuid,
    pub score: khive_score::DeterministicScore,
    pub rank: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VectorStoreInfo {
    pub model_name: String,
    pub dimensions: usize,
    pub index_kind: VectorIndexKind,
    pub entry_count: u64,
    pub needs_rebuild: bool,
    pub last_rebuild_at: Option<DateTime<Utc>>,
}

// -- Text search types --

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

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TextFilter {
    pub ids: Vec<Uuid>,
    pub kinds: Vec<SubstrateKind>,
    pub namespaces: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TextQueryMode {
    Plain,
    Phrase,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TextSearchRequest {
    pub query: String,
    pub mode: TextQueryMode,
    pub filter: Option<TextFilter>,
    pub top_k: u32,
    pub snippet_chars: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TextSearchHit {
    pub subject_id: Uuid,
    pub score: khive_score::DeterministicScore,
    pub rank: u32,
    pub title: Option<String>,
    pub snippet: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TextIndexStats {
    pub document_count: u64,
    pub needs_rebuild: bool,
    pub last_rebuild_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexRebuildScope {
    Full,
    Entities(Vec<Uuid>),
}

// -- Pagination --

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
    pub source_id: Uuid,
    pub target_id: Uuid,
    pub relation: EdgeRelation,
    pub weight: f64,
    pub created_at: DateTime<Utc>,
    pub metadata: Option<Value>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    #[default]
    Out,
    In,
    Both,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TimeRange {
    pub start: Option<DateTime<Utc>>,
    pub end: Option<DateTime<Utc>>,
}

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

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EdgeSortField {
    CreatedAt,
    Weight,
    Relation,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SortDirection {
    Asc,
    Desc,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SortOrder<F> {
    pub field: F,
    pub direction: SortDirection,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NeighborQuery {
    pub direction: Direction,
    pub relations: Option<Vec<EdgeRelation>>,
    pub limit: Option<u32>,
    pub min_weight: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NeighborHit {
    pub node_id: Uuid,
    pub edge_id: Uuid,
    pub relation: EdgeRelation,
    pub weight: f64,
}

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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TraversalRequest {
    pub roots: Vec<Uuid>,
    pub options: TraversalOptions,
    pub include_roots: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PathNode {
    pub node_id: Uuid,
    pub via_edge: Option<Uuid>,
    pub depth: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GraphPath {
    pub root_id: Uuid,
    pub nodes: Vec<PathNode>,
    pub total_weight: f64,
}
