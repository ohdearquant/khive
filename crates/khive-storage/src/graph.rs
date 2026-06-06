//! Graph storage capability — edge CRUD and traversal.

use async_trait::async_trait;
use uuid::Uuid;

use crate::types::{
    BatchWriteSummary, DeleteMode, Edge, EdgeFilter, EdgeSortField, GraphPath, LinkId, NeighborHit,
    NeighborQuery, Page, PageRequest, SortOrder, StorageResult, TraversalRequest,
};

/// Directed-edge CRUD and graph traversal over the entity/note substrates.
#[async_trait]
pub trait GraphStore: Send + Sync + 'static {
    /// Insert or update a single edge.
    async fn upsert_edge(&self, edge: Edge) -> StorageResult<()>;
    /// Insert or update a batch of edges.
    async fn upsert_edges(&self, edges: Vec<Edge>) -> StorageResult<BatchWriteSummary>;
    /// Fetch an edge by its link ID, returning `None` if absent.
    async fn get_edge(&self, id: LinkId) -> StorageResult<Option<Edge>>;
    /// Delete an edge by link ID using the specified delete mode.
    async fn delete_edge(&self, id: LinkId, mode: DeleteMode) -> StorageResult<bool>;
    /// Query edges matching a filter with sort and pagination.
    async fn query_edges(
        &self,
        filter: EdgeFilter,
        sort: Vec<SortOrder<EdgeSortField>>,
        page: PageRequest,
    ) -> StorageResult<Page<Edge>>;
    /// Count edges matching a filter.
    async fn count_edges(&self, filter: EdgeFilter) -> StorageResult<u64>;
    /// Return immediate graph neighbors of a node.
    async fn neighbors(
        &self,
        node_id: Uuid,
        query: NeighborQuery,
    ) -> StorageResult<Vec<NeighborHit>>;
    /// Run a multi-hop BFS traversal from the given root nodes.
    async fn traverse(&self, request: TraversalRequest) -> StorageResult<Vec<GraphPath>>;
}
