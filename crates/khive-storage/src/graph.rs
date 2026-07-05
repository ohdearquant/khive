//! Graph storage capability — edge CRUD and traversal.

use async_trait::async_trait;
use uuid::Uuid;

use crate::types::{
    BatchWriteSummary, DeleteMode, DirectedNeighborHit, Direction, Edge, EdgeFilter, EdgeSortField,
    GraphPath, LinkId, NeighborHit, NeighborQuery, Page, PageRequest, SortOrder, StorageResult,
    TraversalRequest,
};

/// Directed edge CRUD and graph traversal over the knowledge graph.
#[async_trait]
pub trait GraphStore: Send + Sync + 'static {
    /// Insert or update a single edge.
    async fn upsert_edge(&self, edge: Edge) -> StorageResult<()>;
    /// Insert or update a batch of edges.
    async fn upsert_edges(&self, edges: Vec<Edge>) -> StorageResult<BatchWriteSummary>;
    /// Fetch an edge by link ID, returning `None` if absent. Filters soft-deleted rows.
    async fn get_edge(&self, id: LinkId) -> StorageResult<Option<Edge>>;
    /// Fetch an edge by link ID including soft-deleted rows. Used by the runtime hard-delete path
    /// to locate and namespace-check an already-soft-deleted edge before purging it.
    async fn get_edge_including_deleted(&self, id: LinkId) -> StorageResult<Option<Edge>>;
    /// Delete an edge by link ID using the specified delete mode.
    async fn delete_edge(&self, id: LinkId, mode: DeleteMode) -> StorageResult<bool>;
    /// Query edges with filter, sort, and pagination.
    async fn query_edges(
        &self,
        filter: EdgeFilter,
        sort: Vec<SortOrder<EdgeSortField>>,
        page: PageRequest,
    ) -> StorageResult<Page<Edge>>;
    /// Count edges matching the given filter.
    async fn count_edges(&self, filter: EdgeFilter) -> StorageResult<u64>;
    /// Return immediate neighbors of a graph node.
    async fn neighbors(
        &self,
        node_id: Uuid,
        query: NeighborQuery,
    ) -> StorageResult<Vec<NeighborHit>>;
    /// Return neighbors in BOTH directions in a single call, each tagged with
    /// the direction (`Out`/`In`) it was found in. `query.direction` is
    /// ignored — this always fetches both directions.
    ///
    /// Exists so a caller that needs both-direction neighbors labeled by
    /// direction (e.g. the `context` verb) can do so with one storage query
    /// instead of two separate direction-scoped `neighbors` calls. The
    /// default implementation preserves the original two-call behavior for
    /// backends that don't override it; `SqlGraphStore` overrides this with a
    /// single `UNION ALL` query that projects a direction literal per arm.
    async fn neighbors_both_directions(
        &self,
        node_id: Uuid,
        query: NeighborQuery,
    ) -> StorageResult<Vec<DirectedNeighborHit>> {
        let mut out_query = query.clone();
        out_query.direction = Direction::Out;
        let mut in_query = query;
        in_query.direction = Direction::In;
        let mut result = Vec::new();
        for hit in self.neighbors(node_id, out_query).await? {
            result.push(DirectedNeighborHit {
                hit,
                direction: Direction::Out,
            });
        }
        for hit in self.neighbors(node_id, in_query).await? {
            result.push(DirectedNeighborHit {
                hit,
                direction: Direction::In,
            });
        }
        Ok(result)
    }
    /// Fetch multiple edges by their link IDs in a single round-trip.
    ///
    /// IDs that are not found (absent or soft-deleted) are silently skipped;
    /// the returned `Vec` may be shorter than `ids`. Backends that support
    /// batched `IN (...)` queries should override this; the default loops
    /// `get_edge` so non-SQLite backends keep compiling unchanged.
    ///
    /// Callers must chunk large ID lists before calling if they need a strict
    /// size bound; this method does not enforce a maximum.
    async fn get_edges(&self, ids: &[LinkId]) -> StorageResult<Vec<Edge>> {
        let mut out = Vec::with_capacity(ids.len());
        for &id in ids {
            if let Some(edge) = self.get_edge(id).await? {
                out.push(edge);
            }
        }
        Ok(out)
    }
    /// Return neighbors for multiple source nodes in a single round-trip,
    /// yielding `(source_id, hit)` pairs.
    ///
    /// The `query` parameters (direction, relations, min_weight) are applied
    /// uniformly to every source node. `query.limit` is applied **per source**:
    /// each source returns at most `limit` hits. Backends that support batched
    /// `source_id IN (...)` queries should override this; the default loops
    /// `neighbors` so non-SQLite backends keep compiling unchanged.
    async fn batch_neighbors(
        &self,
        sources: &[Uuid],
        query: NeighborQuery,
    ) -> StorageResult<Vec<(Uuid, NeighborHit)>> {
        let mut out = Vec::new();
        for &src in sources {
            let hits = self.neighbors(src, query.clone()).await?;
            for hit in hits {
                out.push((src, hit));
            }
        }
        Ok(out)
    }
    /// Multi-hop BFS traversal from the given roots.
    async fn traverse(&self, request: TraversalRequest) -> StorageResult<Vec<GraphPath>>;
    /// Hard-delete every incident edge (source or target) for `node_id`, regardless of soft-delete
    /// state. Used during endpoint hard-delete to prevent dangling `graph_edges` rows (ADR-002
    /// no-dangling-references contract).
    async fn purge_incident_edges(&self, node_id: Uuid) -> StorageResult<u64>;
}
