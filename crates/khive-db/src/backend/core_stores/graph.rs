use super::super::StorageBackend;
use super::map_open_error;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use khive_storage::graph::{
    CommitAnnotationGuard, CommitAnnotationInsertOutcome, GraphStore, SymmetricEdgeUpdateOutcome,
    SymmetricEdgeUpdateRequest,
};
use khive_storage::types::EdgeEndpointBaseCounts;
use khive_storage::{
    BatchWriteSummary, DeleteMode, DirectedNeighborHit, Edge, EdgeFilter, EdgeSeekPage,
    EdgeSortField, EdgeUpsertRequest, EdgeUpsertResult, GraphPath, GuardedBatchOutcome,
    GuardedEdgeBatchUpsertOutcome, GuardedEdgeUpsertOutcome, GuardedWriteOutcome, LinkId,
    NeighborCursor, NeighborHit, NeighborQuery, Page, PageRequest, SeekCursor, SeekPage, SortOrder,
    StorageCapability, StorageResult, TraversalRequest,
};
use khive_types::EdgeRelation;
use uuid::Uuid;

#[async_trait]
impl GraphStore for StorageBackend {
    async fn latest_annotating_note(
        &self,
        _node_id: Uuid,
        _kind: &str,
        _tag: &str,
    ) -> StorageResult<Option<(Uuid, i64)>> {
        self.graph()
            .map_err(|error| map_open_error(error, StorageCapability::Graph, "graph"))?
            .latest_annotating_note(_node_id, _kind, _tag)
            .await
    }

    async fn latest_annotating_note_with_property(
        &self,
        _node_id: Uuid,
        _kind: &str,
        _tag: &str,
        _property_key: &str,
        _property_value: &str,
    ) -> StorageResult<Option<(Uuid, i64)>> {
        self.graph()
            .map_err(|error| map_open_error(error, StorageCapability::Graph, "graph"))?
            .latest_annotating_note_with_property(
                _node_id,
                _kind,
                _tag,
                _property_key,
                _property_value,
            )
            .await
    }

    async fn upsert_edge(&self, edge: Edge) -> StorageResult<()> {
        self.graph()
            .map_err(|error| map_open_error(error, StorageCapability::Graph, "graph"))?
            .upsert_edge(edge)
            .await
    }

    async fn insert_edge_if_absent(&self, _edge: Edge) -> StorageResult<bool> {
        self.graph()
            .map_err(|error| map_open_error(error, StorageCapability::Graph, "graph"))?
            .insert_edge_if_absent(_edge)
            .await
    }

    async fn insert_commit_annotation_if_absent(
        &self,
        _edge: Edge,
        _guard: CommitAnnotationGuard,
    ) -> StorageResult<CommitAnnotationInsertOutcome> {
        self.graph()
            .map_err(|error| map_open_error(error, StorageCapability::Graph, "graph"))?
            .insert_commit_annotation_if_absent(_edge, _guard)
            .await
    }

    async fn upsert_edges(&self, edges: Vec<Edge>) -> StorageResult<BatchWriteSummary> {
        self.graph()
            .map_err(|error| map_open_error(error, StorageCapability::Graph, "graph"))?
            .upsert_edges(edges)
            .await
    }

    async fn upsert_edge_observed(
        &self,
        _request: EdgeUpsertRequest,
    ) -> StorageResult<EdgeUpsertResult> {
        self.graph()
            .map_err(|error| map_open_error(error, StorageCapability::Graph, "graph"))?
            .upsert_edge_observed(_request)
            .await
    }

    async fn replace_edge_if_unchanged(
        &self,
        _edge: Edge,
        _expected_updated_at: DateTime<Utc>,
        _expected_deleted_at: Option<DateTime<Utc>>,
    ) -> StorageResult<bool> {
        self.graph()
            .map_err(|error| map_open_error(error, StorageCapability::Graph, "graph"))?
            .replace_edge_if_unchanged(_edge, _expected_updated_at, _expected_deleted_at)
            .await
    }

    async fn update_symmetric_edge_if_unchanged(
        &self,
        _request: SymmetricEdgeUpdateRequest,
    ) -> StorageResult<SymmetricEdgeUpdateOutcome> {
        self.graph()
            .map_err(|error| map_open_error(error, StorageCapability::Graph, "graph"))?
            .update_symmetric_edge_if_unchanged(_request)
            .await
    }

    async fn upsert_edge_guarded(&self, _edge: Edge) -> StorageResult<GuardedWriteOutcome> {
        self.graph()
            .map_err(|error| map_open_error(error, StorageCapability::Graph, "graph"))?
            .upsert_edge_guarded(_edge)
            .await
    }

    async fn upsert_edge_guarded_observed(
        &self,
        _request: EdgeUpsertRequest,
    ) -> StorageResult<GuardedEdgeUpsertOutcome> {
        self.graph()
            .map_err(|error| map_open_error(error, StorageCapability::Graph, "graph"))?
            .upsert_edge_guarded_observed(_request)
            .await
    }

    async fn upsert_edges_guarded(&self, _edges: Vec<Edge>) -> StorageResult<GuardedBatchOutcome> {
        self.graph()
            .map_err(|error| map_open_error(error, StorageCapability::Graph, "graph"))?
            .upsert_edges_guarded(_edges)
            .await
    }

    async fn upsert_edges_guarded_observed(
        &self,
        _requests: Vec<EdgeUpsertRequest>,
    ) -> StorageResult<GuardedEdgeBatchUpsertOutcome> {
        self.graph()
            .map_err(|error| map_open_error(error, StorageCapability::Graph, "graph"))?
            .upsert_edges_guarded_observed(_requests)
            .await
    }

    async fn get_edge(&self, id: LinkId) -> StorageResult<Option<Edge>> {
        self.graph()
            .map_err(|error| map_open_error(error, StorageCapability::Graph, "graph"))?
            .get_edge(id)
            .await
    }

    async fn get_edge_including_deleted(&self, id: LinkId) -> StorageResult<Option<Edge>> {
        self.graph()
            .map_err(|error| map_open_error(error, StorageCapability::Graph, "graph"))?
            .get_edge_including_deleted(id)
            .await
    }

    async fn get_edge_by_natural_key_including_deleted(
        &self,
        namespace: &str,
        source_id: Uuid,
        target_id: Uuid,
        relation: EdgeRelation,
    ) -> StorageResult<Option<Edge>> {
        self.graph()
            .map_err(|error| map_open_error(error, StorageCapability::Graph, "graph"))?
            .get_edge_by_natural_key_including_deleted(namespace, source_id, target_id, relation)
            .await
    }

    async fn delete_edge(&self, id: LinkId, mode: DeleteMode) -> StorageResult<bool> {
        self.graph()
            .map_err(|error| map_open_error(error, StorageCapability::Graph, "graph"))?
            .delete_edge(id, mode)
            .await
    }

    async fn query_edges(
        &self,
        filter: EdgeFilter,
        sort: Vec<SortOrder<EdgeSortField>>,
        page: PageRequest,
    ) -> StorageResult<Page<Edge>> {
        self.graph()
            .map_err(|error| map_open_error(error, StorageCapability::Graph, "graph"))?
            .query_edges(filter, sort, page)
            .await
    }

    async fn query_edges_in_namespaces(
        &self,
        namespaces: &[String],
        filter: EdgeFilter,
        sort: Vec<SortOrder<EdgeSortField>>,
        page: PageRequest,
    ) -> StorageResult<Page<Edge>> {
        self.graph()
            .map_err(|error| map_open_error(error, StorageCapability::Graph, "graph"))?
            .query_edges_in_namespaces(namespaces, filter, sort, page)
            .await
    }

    async fn count_edges(&self, filter: EdgeFilter) -> StorageResult<u64> {
        self.graph()
            .map_err(|error| map_open_error(error, StorageCapability::Graph, "graph"))?
            .count_edges(filter)
            .await
    }

    async fn count_edges_in_namespaces(
        &self,
        namespaces: &[String],
        filter: EdgeFilter,
    ) -> StorageResult<u64> {
        self.graph()
            .map_err(|error| map_open_error(error, StorageCapability::Graph, "graph"))?
            .count_edges_in_namespaces(namespaces, filter)
            .await
    }

    async fn count_edges_by_relation(&self) -> StorageResult<Vec<(EdgeRelation, u64)>> {
        self.graph()
            .map_err(|error| map_open_error(error, StorageCapability::Graph, "graph"))?
            .count_edges_by_relation()
            .await
    }

    async fn count_edges_by_relation_in_namespaces(
        &self,
        namespaces: &[String],
    ) -> StorageResult<Vec<(EdgeRelation, u64)>> {
        self.graph()
            .map_err(|error| map_open_error(error, StorageCapability::Graph, "graph"))?
            .count_edges_by_relation_in_namespaces(namespaces)
            .await
    }

    async fn count_edges_by_endpoint_base(&self) -> StorageResult<EdgeEndpointBaseCounts> {
        self.graph()
            .map_err(|error| map_open_error(error, StorageCapability::Graph, "graph"))?
            .count_edges_by_endpoint_base()
            .await
    }

    async fn count_edges_by_endpoint_base_in_namespaces(
        &self,
        namespaces: &[String],
    ) -> StorageResult<EdgeEndpointBaseCounts> {
        self.graph()
            .map_err(|error| map_open_error(error, StorageCapability::Graph, "graph"))?
            .count_edges_by_endpoint_base_in_namespaces(namespaces)
            .await
    }

    async fn query_edges_after(
        &self,
        filter: EdgeFilter,
        after: Option<Uuid>,
        limit: u32,
    ) -> StorageResult<EdgeSeekPage> {
        self.graph()
            .map_err(|error| map_open_error(error, StorageCapability::Graph, "graph"))?
            .query_edges_after(filter, after, limit)
            .await
    }

    async fn edge_sequence(&self, _id: Uuid) -> StorageResult<Option<i64>> {
        self.graph()
            .map_err(|error| map_open_error(error, StorageCapability::Graph, "graph"))?
            .edge_sequence(_id)
            .await
    }

    async fn edge_sequences(&self, ids: &[Uuid]) -> StorageResult<Vec<(Uuid, i64)>> {
        self.graph()
            .map_err(|error| map_open_error(error, StorageCapability::Graph, "graph"))?
            .edge_sequences(ids)
            .await
    }

    async fn query_edges_sequence_after(
        &self,
        _filter: EdgeFilter,
        _after: Option<SeekCursor>,
        _limit: u32,
    ) -> StorageResult<SeekPage<Edge>> {
        self.graph()
            .map_err(|error| map_open_error(error, StorageCapability::Graph, "graph"))?
            .query_edges_sequence_after(_filter, _after, _limit)
            .await
    }

    async fn neighbors(
        &self,
        node_id: Uuid,
        query: NeighborQuery,
    ) -> StorageResult<Vec<NeighborHit>> {
        self.graph()
            .map_err(|error| map_open_error(error, StorageCapability::Graph, "graph"))?
            .neighbors(node_id, query)
            .await
    }

    async fn neighbors_page(
        &self,
        node_id: Uuid,
        query: NeighborQuery,
        after: Option<NeighborCursor>,
        neighbor_kinds: Option<Vec<String>>,
    ) -> StorageResult<Vec<NeighborHit>> {
        self.graph()
            .map_err(|error| map_open_error(error, StorageCapability::Graph, "graph"))?
            .neighbors_page(node_id, query, after, neighbor_kinds)
            .await
    }

    async fn neighbors_both_directions(
        &self,
        node_id: Uuid,
        query: NeighborQuery,
    ) -> StorageResult<Vec<DirectedNeighborHit>> {
        self.graph()
            .map_err(|error| map_open_error(error, StorageCapability::Graph, "graph"))?
            .neighbors_both_directions(node_id, query)
            .await
    }

    async fn get_edges(&self, ids: &[LinkId]) -> StorageResult<Vec<Edge>> {
        self.graph()
            .map_err(|error| map_open_error(error, StorageCapability::Graph, "graph"))?
            .get_edges(ids)
            .await
    }

    async fn get_edge_read_outcomes(
        &self,
        ids: &[LinkId],
    ) -> StorageResult<Vec<StorageResult<Option<Edge>>>> {
        self.graph()
            .map_err(|error| map_open_error(error, StorageCapability::Graph, "graph"))?
            .get_edge_read_outcomes(ids)
            .await
    }

    async fn batch_neighbors(
        &self,
        sources: &[Uuid],
        query: NeighborQuery,
    ) -> StorageResult<Vec<(Uuid, NeighborHit)>> {
        self.graph()
            .map_err(|error| map_open_error(error, StorageCapability::Graph, "graph"))?
            .batch_neighbors(sources, query)
            .await
    }

    async fn traverse(&self, request: TraversalRequest) -> StorageResult<Vec<GraphPath>> {
        self.graph()
            .map_err(|error| map_open_error(error, StorageCapability::Graph, "graph"))?
            .traverse(request)
            .await
    }

    async fn purge_incident_edges(&self, node_id: Uuid) -> StorageResult<u64> {
        self.graph()
            .map_err(|error| map_open_error(error, StorageCapability::Graph, "graph"))?
            .purge_incident_edges(node_id)
            .await
    }
}
