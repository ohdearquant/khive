//! Graph storage capability — edge CRUD and traversal.

use async_trait::async_trait;
use uuid::Uuid;

use crate::types::{
    BatchWriteSummary, DeleteMode, Edge, EdgeFilter, EdgeSortField, GraphPath, LinkId, NeighborHit,
    NeighborQuery, Page, PageRequest, SortOrder, StorageResult, TraversalRequest,
};

/// Directed edge CRUD and graph traversal over the knowledge graph.
#[async_trait]
pub trait GraphStore: Send + Sync + 'static {
    /// Insert or update a single edge.
    async fn upsert_edge(&self, edge: Edge) -> StorageResult<()>;
    /// Insert or update a batch of edges.
    async fn upsert_edges(&self, edges: Vec<Edge>) -> StorageResult<BatchWriteSummary>;
    /// Fetch an edge by link ID, returning `None` if absent.
    async fn get_edge(&self, id: LinkId) -> StorageResult<Option<Edge>>;
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
    /// Multi-hop BFS traversal from the given roots.
    async fn traverse(&self, request: TraversalRequest) -> StorageResult<Vec<GraphPath>>;
    /// Hard-delete every incident edge (source or target) for `node_id`, regardless of soft-delete
    /// state. Used during endpoint hard-delete to prevent dangling `graph_edges` rows (ADR-002
    /// no-dangling-references contract).
    async fn purge_incident_edges(&self, node_id: Uuid) -> StorageResult<u64>;
}
