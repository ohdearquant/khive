//! Graph edge types: edges, filters, traversal configuration, and path results.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use khive_types::EdgeRelation;

use super::BatchWriteSummary;

/// Maximum number of roots accepted by one public graph traversal.
pub const MAX_TRAVERSAL_ROOTS: usize = 100;
/// Maximum breadth-first depth accepted by graph traversal.
pub const MAX_TRAVERSAL_DEPTH: usize = 10;
/// Per-root non-root result cap used when the caller omits `limit`.
pub const DEFAULT_TRAVERSAL_LIMIT: u32 = 100;
/// Largest per-root non-root result cap accepted from a caller.
pub const MAX_TRAVERSAL_LIMIT: u32 = 1_000;
/// Maximum adjacency rows one public traversal may consume across backends.
pub const MAX_TRAVERSAL_WORK: u64 = 100_000;
/// Maximum wall-clock execution window for one public traversal.
pub const MAX_TRAVERSAL_MILLIS: u64 = 5_000;

/// Shared, one-shot execution budget carried by clones of a traversal request.
///
/// Runtime traversal fans one public request out over each visible namespace.
/// Clones therefore share the same row counter and start instant so work and
/// time limits apply to the whole public operation, rather than resetting for
/// every backend. The field is skipped on the wire; callers control result
/// shape through `limit`, while these hard ceilings remain server policy.
#[derive(Clone, Debug)]
pub struct TraversalExecutionBudget {
    work_limit: u64,
    remaining_work: Arc<AtomicU64>,
    started_at: Instant,
    max_duration: Duration,
}

impl Default for TraversalExecutionBudget {
    fn default() -> Self {
        Self::new(
            MAX_TRAVERSAL_WORK,
            Duration::from_millis(MAX_TRAVERSAL_MILLIS),
        )
    }
}

impl TraversalExecutionBudget {
    /// Create a one-shot budget. Values below the public ceilings are useful
    /// to internal callers and deterministic boundary tests.
    pub fn new(work_limit: u64, max_duration: Duration) -> Self {
        Self {
            work_limit,
            remaining_work: Arc::new(AtomicU64::new(work_limit)),
            started_at: Instant::now(),
            max_duration,
        }
    }

    pub fn work_limit(&self) -> u64 {
        self.work_limit
    }

    pub fn remaining_work(&self) -> u64 {
        self.remaining_work.load(Ordering::Relaxed)
    }

    pub fn max_duration(&self) -> Duration {
        self.max_duration
    }

    pub fn is_expired(&self) -> bool {
        self.started_at.elapsed() >= self.max_duration
    }

    /// Consume one adjacency row. Returns `false` once the shared budget is
    /// exhausted; the caller must fail the traversal rather than return a
    /// partial path set.
    pub fn try_consume_row(&self) -> bool {
        self.remaining_work
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok()
    }

    fn validate(&self) -> Result<(), String> {
        if self.work_limit > MAX_TRAVERSAL_WORK {
            return Err(format!(
                "TraversalRequest: work_limit must be <= {MAX_TRAVERSAL_WORK}, got {}",
                self.work_limit
            ));
        }
        let max_duration = Duration::from_millis(MAX_TRAVERSAL_MILLIS);
        if self.max_duration > max_duration {
            return Err(format!(
                "TraversalRequest: max_duration must be <= {MAX_TRAVERSAL_MILLIS}ms, got {}ms",
                self.max_duration.as_millis()
            ));
        }
        Ok(())
    }
}

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

/// A page of edges returned by keyset (seek) pagination, ordered by `id`
/// ascending — an indexed range scan against the `(namespace, id)` primary
/// key rather than an `OFFSET` skip. `next_after` is `Some(last_id)` when
/// more rows remain past this page.
#[derive(Clone, Debug, Default)]
pub struct EdgeSeekPage {
    pub items: Vec<Edge>,
    pub next_after: Option<Uuid>,
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
///
/// Optional enrichment: `entity_type` is populated by the runtime when the
/// caller passes `include_entity_type=true` to the `neighbors` verb. It is
/// absent from the wire when `None` so the default result shape is unchanged.
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entity_type: Option<String>,
}

/// A [`NeighborHit`] tagged with the direction it was found in, relative to
/// the queried node. Returned by [`crate::GraphStore::neighbors_both_directions`],
/// which fetches both directions in a single `UNION ALL` query instead of two
/// separate direction-scoped calls — the tag lets a caller (e.g. the `context`
/// verb) label each hit `outgoing`/`incoming` without paying for the second
/// query. Only `Direction::Out` and `Direction::In` are ever populated here.
#[derive(Clone, Debug)]
pub struct DirectedNeighborHit {
    pub hit: NeighborHit,
    pub direction: Direction,
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
        if raw.max_depth > MAX_TRAVERSAL_DEPTH {
            return Err(format!(
                "TraversalOptions: max_depth must be <= {MAX_TRAVERSAL_DEPTH}, got {}",
                raw.max_depth
            ));
        }
        if let Some(limit) = raw.limit {
            if limit > MAX_TRAVERSAL_LIMIT {
                return Err(format!(
                    "TraversalOptions: limit must be <= {MAX_TRAVERSAL_LIMIT}, got {limit}"
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

    /// Finite per-root non-root result cap used by every traversal.
    pub fn effective_limit(&self) -> u32 {
        self.limit.unwrap_or(DEFAULT_TRAVERSAL_LIMIT)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.max_depth > MAX_TRAVERSAL_DEPTH {
            return Err(format!(
                "TraversalOptions: max_depth must be <= {MAX_TRAVERSAL_DEPTH}, got {}",
                self.max_depth
            ));
        }
        if let Some(limit) = self.limit {
            if limit > MAX_TRAVERSAL_LIMIT {
                return Err(format!(
                    "TraversalOptions: limit must be <= {MAX_TRAVERSAL_LIMIT}, got {limit}"
                ));
            }
        }
        if let Some(relations) = &self.relations {
            if relations.len() > EdgeRelation::ALL.len() {
                return Err(format!(
                    "TraversalOptions: relations must contain at most {} entries, got {}",
                    EdgeRelation::ALL.len(),
                    relations.len()
                ));
            }
        }
        if let Some(weight) = self.min_weight {
            if !weight.is_finite() || !(0.0..=1.0).contains(&weight) {
                return Err(format!(
                    "TraversalOptions: min_weight must be finite and in [0.0, 1.0], got {weight}"
                ));
            }
        }
        Ok(())
    }
}

/// Raw deserialization target for [`TraversalRequest`].
#[derive(Deserialize)]
struct TraversalRequestRaw {
    roots: Vec<Uuid>,
    options: TraversalOptionsRaw,
    include_roots: bool,
    #[serde(default)]
    include_properties: bool,
}

impl TryFrom<TraversalRequestRaw> for TraversalRequest {
    type Error = String;

    fn try_from(raw: TraversalRequestRaw) -> Result<Self, Self::Error> {
        let request = Self {
            roots: raw.roots,
            options: TraversalOptions::try_from(raw.options)?,
            include_roots: raw.include_roots,
            include_properties: raw.include_properties,
            execution_budget: TraversalExecutionBudget::default(),
        };
        request.validate()?;
        Ok(request)
    }
}

/// A graph traversal request from a set of root nodes.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(try_from = "TraversalRequestRaw")]
pub struct TraversalRequest {
    pub roots: Vec<Uuid>,
    pub options: TraversalOptions,
    pub include_roots: bool,
    /// When `true`, `enrich_path_nodes` populates the `properties` map on each
    /// `PathNode`. Default `false`; the wire shape is unchanged when absent.
    #[serde(default)]
    pub include_properties: bool,
    /// Shared by runtime clones so work/time ceilings cover all visible
    /// namespaces. This is execution state, not part of the serialized API.
    #[serde(skip)]
    pub execution_budget: TraversalExecutionBudget,
}

impl TraversalRequest {
    pub fn validate(&self) -> Result<(), String> {
        if self.roots.len() > MAX_TRAVERSAL_ROOTS {
            return Err(format!(
                "TraversalRequest: roots must contain at most {MAX_TRAVERSAL_ROOTS} entries, got {}",
                self.roots.len()
            ));
        }
        self.options.validate()?;
        self.execution_budget.validate()
    }
}

/// One node along a traversal path.
///
/// Field naming (#148): JSON wire serialization is `id`. Enrichment (#162,
/// #1484): `name`/`kind` are filled from entity or note records by the runtime
/// layer after the storage call.
///
/// Optional enrichment: `properties` is populated by the runtime when the
/// caller passes `include_properties=true` to the `traverse` verb. It is
/// absent from the wire when `None` so the default result shape is unchanged.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PathNode {
    #[serde(rename = "id")]
    pub node_id: Uuid,
    pub via_edge: Option<Uuid>,
    pub depth: usize,
    /// Cumulative edge weight along the path that reached this node.
    ///
    /// Not part of the wire shape — it exists so that `GraphPath.total_weight`
    /// stays derivable after the node list is edited. Every layer above
    /// storage edits that list (the visible-namespace merge, the merged limit
    /// re-application, the soft-deleted-node screen), and a `total_weight`
    /// carried forward from before an edit can end up describing a node the
    /// caller was never shown.
    #[serde(skip)]
    pub weight: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub properties: Option<Value>,
}

/// A complete traversal path from one root node to its reachable descendants.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GraphPath {
    pub root_id: Uuid,
    pub nodes: Vec<PathNode>,
    pub total_weight: f64,
}

/// Which of a would-be edge's two endpoints were missing when a guarded
/// write's in-transaction existence check refused it (#769). Produced by
/// the guard's own commit-time probe, not a
/// post-hoc read after the write already failed, so a concurrent
/// hard-delete landing after the guard ran cannot make this outcome lie
/// about which endpoint was actually missing at write time.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MissingEndpoints {
    pub source: bool,
    pub target: bool,
}

impl MissingEndpoints {
    /// True if at least one endpoint was reported missing.
    pub fn any(&self) -> bool {
        self.source || self.target
    }
}

/// Outcome of [`crate::GraphStore::upsert_edge_guarded`], determined entirely
/// inside the guard's own storage transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GuardedWriteOutcome {
    /// The edge was inserted or updated; both endpoints existed at write time.
    Written,
    /// The write was refused; `MissingEndpoints` names which endpoint(s) were
    /// gone at write time.
    Refused(MissingEndpoints),
}

/// Which batch entry a guarded batch write refused on, and why, determined
/// inside the same in-transaction pre-check that aborted the batch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuardedBatchRefusal {
    /// Index of the first batch entry whose endpoint(s) were missing.
    pub entry_index: usize,
    pub missing: MissingEndpoints,
}

/// Outcome of [`crate::GraphStore::upsert_edges_guarded`]. `refused` is
/// `Some` exactly when `summary.affected == 0` after a guard refusal;
/// `None` when every edge in the batch was written.
#[derive(Clone, Debug)]
pub struct GuardedBatchOutcome {
    pub summary: BatchWriteSummary,
    pub refused: Option<GuardedBatchRefusal>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traversal_options_default_max_depth_is_three() {
        assert_eq!(TraversalOptions::default().max_depth, 3);
        assert_eq!(
            TraversalOptions::default().effective_limit(),
            DEFAULT_TRAVERSAL_LIMIT
        );
    }

    #[test]
    fn traverse_max_depth_over_public_cap_rejected() {
        let raw = serde_json::json!({
            "max_depth": MAX_TRAVERSAL_DEPTH + 1,
            "direction": "out",
            "relations": null,
            "min_weight": null,
            "limit": null,
        });
        let result: Result<TraversalOptions, _> = serde_json::from_value(raw);
        assert!(
            result.is_err(),
            "max_depth above the public cap must be rejected, got {result:?}"
        );
    }

    #[test]
    fn traverse_depth_and_limit_boundaries_are_inclusive() {
        let raw = serde_json::json!({
            "max_depth": MAX_TRAVERSAL_DEPTH,
            "direction": "out",
            "relations": null,
            "min_weight": null,
            "limit": MAX_TRAVERSAL_LIMIT,
        });
        let result: Result<TraversalOptions, _> = serde_json::from_value(raw);
        assert!(result.is_ok(), "inclusive public maxima must be accepted");
    }

    #[test]
    fn traverse_limit_over_public_cap_rejected() {
        let raw = serde_json::json!({
            "max_depth": 1,
            "direction": "out",
            "relations": null,
            "min_weight": null,
            "limit": MAX_TRAVERSAL_LIMIT + 1,
        });
        let result: Result<TraversalOptions, _> = serde_json::from_value(raw);
        assert!(
            result.is_err(),
            "limit above the public cap must be rejected"
        );
    }

    #[test]
    fn traverse_root_cap_rejected_during_deserialization() {
        let raw = serde_json::json!({
            "roots": vec![Uuid::nil(); MAX_TRAVERSAL_ROOTS + 1],
            "options": {
                "max_depth": 1,
                "direction": "out",
                "relations": null,
                "min_weight": null,
                "limit": 1,
            },
            "include_roots": false,
        });
        let result: Result<TraversalRequest, _> = serde_json::from_value(raw);
        assert!(
            result.is_err(),
            "root list above the public cap must be rejected"
        );
    }

    #[test]
    fn traversal_execution_budget_is_shared_but_not_serialized() {
        let budget = TraversalExecutionBudget::new(2, Duration::from_secs(1));
        let clone = budget.clone();
        assert!(budget.try_consume_row());
        assert_eq!(clone.remaining_work(), 1);

        let request = TraversalRequest {
            roots: vec![Uuid::nil()],
            options: TraversalOptions::default(),
            include_roots: true,
            include_properties: false,
            execution_budget: budget,
        };
        let value = serde_json::to_value(request).unwrap();
        assert!(value.get("execution_budget").is_none());
    }
}
