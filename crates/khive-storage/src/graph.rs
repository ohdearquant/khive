//! Graph storage capability — edge CRUD and traversal.

use async_trait::async_trait;
use uuid::Uuid;

use crate::types::{
    BatchWriteSummary, Edge, EdgeFilter, EdgeSortField, GraphPath, LinkId, NeighborHit,
    NeighborQuery, Page, PageRequest, SortOrder, StorageResult, TraversalRequest,
};

#[async_trait]
pub trait GraphStore: Send + Sync + 'static {
    async fn upsert_edge(&self, edge: Edge) -> StorageResult<()>;
    async fn upsert_edges(&self, edges: Vec<Edge>) -> StorageResult<BatchWriteSummary>;
    async fn get_edge(&self, id: LinkId) -> StorageResult<Option<Edge>>;
    async fn delete_edge(&self, id: LinkId) -> StorageResult<bool>;
    async fn query_edges(
        &self,
        filter: EdgeFilter,
        sort: Vec<SortOrder<EdgeSortField>>,
        page: PageRequest,
    ) -> StorageResult<Page<Edge>>;
    async fn count_edges(&self, filter: EdgeFilter) -> StorageResult<u64>;
    async fn neighbors(
        &self,
        node_id: Uuid,
        query: NeighborQuery,
    ) -> StorageResult<Vec<NeighborHit>>;
    async fn traverse(&self, request: TraversalRequest) -> StorageResult<Vec<GraphPath>>;
}
