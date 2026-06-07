//! Graph edge types: edges, filters, traversal configuration, and path results.

use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use khive_types::EdgeRelation;

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
///
/// Use [`validate`](EdgeFilter::validate) to check weight-bound invariants
/// before passing to a backend.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(try_from = "EdgeFilterRaw")]
pub struct EdgeFilter {
    pub ids: Vec<LinkId>,
    pub source_ids: Vec<Uuid>,
    pub target_ids: Vec<Uuid>,
    pub relations: Vec<EdgeRelation>,
    pub min_weight: Option<f64>,
    pub max_weight: Option<f64>,
    pub created_at: Option<TimeRange>,
}

/// Raw deserialization target for [`EdgeFilter`].
#[derive(Deserialize, Default)]
struct EdgeFilterRaw {
    #[serde(default)]
    ids: Vec<LinkId>,
    #[serde(default)]
    source_ids: Vec<Uuid>,
    #[serde(default)]
    target_ids: Vec<Uuid>,
    #[serde(default)]
    relations: Vec<EdgeRelation>,
    min_weight: Option<f64>,
    max_weight: Option<f64>,
    created_at: Option<TimeRange>,
}

impl TryFrom<EdgeFilterRaw> for EdgeFilter {
    type Error = String;

    fn try_from(raw: EdgeFilterRaw) -> Result<Self, Self::Error> {
        let ef = Self {
            ids: raw.ids,
            source_ids: raw.source_ids,
            target_ids: raw.target_ids,
            relations: raw.relations,
            min_weight: raw.min_weight,
            max_weight: raw.max_weight,
            created_at: raw.created_at,
        };
        ef.validate()?;
        Ok(ef)
    }
}

impl EdgeFilter {
    /// Validate that weight bounds are finite and ordered correctly. Returns first violation.
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
    /// Create traversal options with the given maximum depth.
    pub fn new(max_depth: usize) -> Self {
        Self {
            max_depth,
            ..Default::default()
        }
    }

    /// Set the traversal direction.
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
