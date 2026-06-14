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

/// Raw deserialization target for [`Edge`].
#[derive(Deserialize)]
struct EdgeRaw {
    id: LinkId,
    namespace: String,
    source_id: Uuid,
    target_id: Uuid,
    relation: EdgeRelation,
    weight: f64,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    deleted_at: Option<DateTime<Utc>>,
    metadata: Option<Value>,
    target_backend: Option<String>,
}

impl TryFrom<EdgeRaw> for Edge {
    type Error = String;

    fn try_from(raw: EdgeRaw) -> Result<Self, Self::Error> {
        if !raw.weight.is_finite() {
            return Err(format!("Edge: weight must be finite, got {}", raw.weight));
        }
        if !(0.0..=1.0).contains(&raw.weight) {
            return Err(format!(
                "Edge: weight must be in [0.0, 1.0], got {}",
                raw.weight
            ));
        }
        Ok(Self {
            id: raw.id,
            namespace: raw.namespace,
            source_id: raw.source_id,
            target_id: raw.target_id,
            relation: raw.relation,
            weight: raw.weight,
            created_at: raw.created_at,
            updated_at: raw.updated_at,
            deleted_at: raw.deleted_at,
            metadata: raw.metadata,
            target_backend: raw.target_backend,
        })
    }
}

/// A directed edge in the graph. Deserialization rejects non-finite weights.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(try_from = "EdgeRaw")]
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
    /// Validate that weight bounds are finite, within [0.0, 1.0], and ordered correctly.
    /// Returns the first violation.
    pub fn validate(&self) -> Result<(), String> {
        if let Some(w) = self.min_weight {
            if !w.is_finite() {
                return Err(format!("EdgeFilter: min_weight is non-finite ({w})"));
            }
            if !(0.0..=1.0).contains(&w) {
                return Err(format!(
                    "EdgeFilter: min_weight must be in [0.0, 1.0], got {w}"
                ));
            }
        }
        if let Some(w) = self.max_weight {
            if !w.is_finite() {
                return Err(format!("EdgeFilter: max_weight is non-finite ({w})"));
            }
            if !(0.0..=1.0).contains(&w) {
                return Err(format!(
                    "EdgeFilter: max_weight must be in [0.0, 1.0], got {w}"
                ));
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

/// Raw deserialization target for [`NeighborQuery`].
#[derive(Deserialize)]
struct NeighborQueryRaw {
    direction: Direction,
    relations: Option<Vec<EdgeRelation>>,
    limit: Option<u32>,
    min_weight: Option<f64>,
}

impl TryFrom<NeighborQueryRaw> for NeighborQuery {
    type Error = String;

    fn try_from(raw: NeighborQueryRaw) -> Result<Self, Self::Error> {
        if let Some(w) = raw.min_weight {
            if !w.is_finite() {
                return Err(format!("NeighborQuery: min_weight must be finite, got {w}"));
            }
            if !(0.0..=1.0).contains(&w) {
                return Err(format!(
                    "NeighborQuery: min_weight must be in [0.0, 1.0], got {w}"
                ));
            }
        }
        Ok(Self {
            direction: raw.direction,
            relations: raw.relations,
            limit: raw.limit,
            min_weight: raw.min_weight,
        })
    }
}

/// Parameters for a single-hop graph neighbor lookup. Deserialization rejects non-finite min_weight.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(try_from = "NeighborQueryRaw")]
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

/// Raw deserialization target for [`TraversalOptions`].
#[derive(Deserialize)]
struct TraversalOptionsRaw {
    max_depth: usize,
    direction: Direction,
    relations: Option<Vec<EdgeRelation>>,
    min_weight: Option<f64>,
    limit: Option<u32>,
}

impl TryFrom<TraversalOptionsRaw> for TraversalOptions {
    type Error = String;

    fn try_from(raw: TraversalOptionsRaw) -> Result<Self, Self::Error> {
        if let Some(w) = raw.min_weight {
            if !w.is_finite() {
                return Err(format!(
                    "TraversalOptions: min_weight must be finite, got {w}"
                ));
            }
            if !(0.0..=1.0).contains(&w) {
                return Err(format!(
                    "TraversalOptions: min_weight must be in [0.0, 1.0], got {w}"
                ));
            }
        }
        Ok(Self {
            max_depth: raw.max_depth,
            direction: raw.direction,
            relations: raw.relations,
            min_weight: raw.min_weight,
            limit: raw.limit,
        })
    }
}

/// BFS traversal configuration controlling depth, direction, and edge filters.
/// Deserialization rejects non-finite min_weight.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(try_from = "TraversalOptionsRaw")]
pub struct TraversalOptions {
    pub max_depth: usize,
    pub direction: Direction,
    pub relations: Option<Vec<EdgeRelation>>,
    pub min_weight: Option<f64>,
    pub limit: Option<u32>,
}

impl Default for TraversalOptions {
    fn default() -> Self {
        Self {
            max_depth: 3,
            direction: Direction::Out,
            relations: None,
            min_weight: None,
            limit: None,
        }
    }
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

/// Raw deserialization target for [`TraversalRequest`].
#[derive(Deserialize)]
struct TraversalRequestRaw {
    roots: Vec<Uuid>,
    options: TraversalOptionsRaw,
    include_roots: bool,
}

impl TryFrom<TraversalRequestRaw> for TraversalRequest {
    type Error = String;

    fn try_from(raw: TraversalRequestRaw) -> Result<Self, Self::Error> {
        Ok(Self {
            roots: raw.roots,
            options: TraversalOptions::try_from(raw.options)?,
            include_roots: raw.include_roots,
        })
    }
}

/// A graph traversal request from a set of root nodes.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(try_from = "TraversalRequestRaw")]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traversal_options_default_max_depth_is_three() {
        assert_eq!(TraversalOptions::default().max_depth, 3);
    }
}
