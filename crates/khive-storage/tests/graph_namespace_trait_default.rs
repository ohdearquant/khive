//! #2089: any `GraphStore` backend that does not
//! override `query_edges_in_namespaces` (the trait default) must still
//! behave correctly for the 0/1-namespace cases, and must reject a
//! multi-namespace request with `StorageError::Unsupported { operation:
//! "query_edges_in_namespaces", .. }` — the exact shape
//! `khive-runtime::operations::KhiveRuntime::list_edges` matches on to
//! decide whether to fall back to a per-namespace merge. A backend that
//! doesn't implement the batched query is a real, if uncompiled-in-this-
//! workspace, case: `khive-merge` is forward-deployed outside the workspace
//! members (see `crates/khive-merge`), so the runtime cannot assume every
//! `GraphStore` it is handed overrides this method.

use std::sync::Mutex;

use async_trait::async_trait;
use uuid::Uuid;

use khive_storage::types::{
    BatchWriteSummary, DeleteMode, Edge, EdgeFilter, EdgeSeekPage, EdgeSortField, GraphPath,
    LinkId, NeighborHit, NeighborQuery, Page, PageRequest, SortDirection, SortOrder,
    TraversalRequest,
};
use khive_storage::{GraphStore, StorageCapability, StorageError, StorageResult};
use khive_types::EdgeRelation;

/// A minimal `GraphStore` that only implements the methods with no trait
/// default (plus `query_edges`, needed for `query_edges_in_namespaces`'s
/// 1-namespace delegation). It deliberately does NOT override
/// `query_edges_in_namespaces` or `count_edges_in_namespaces` — exactly the
/// "backend exercising the trait default" this suite exists to cover.
struct TraitDefaultOnlyGraphStore {
    edges: Mutex<Vec<Edge>>,
}

impl TraitDefaultOnlyGraphStore {
    fn new(edges: Vec<Edge>) -> Self {
        Self {
            edges: Mutex::new(edges),
        }
    }
}

#[async_trait]
impl GraphStore for TraitDefaultOnlyGraphStore {
    async fn upsert_edge(&self, edge: Edge) -> StorageResult<()> {
        self.edges.lock().unwrap().push(edge);
        Ok(())
    }

    async fn upsert_edges(&self, edges: Vec<Edge>) -> StorageResult<BatchWriteSummary> {
        let count = edges.len() as u64;
        self.edges.lock().unwrap().extend(edges);
        Ok(BatchWriteSummary {
            affected: count,
            ..Default::default()
        })
    }

    async fn get_edge(&self, id: LinkId) -> StorageResult<Option<Edge>> {
        Ok(self
            .edges
            .lock()
            .unwrap()
            .iter()
            .find(|e| e.id == id)
            .cloned())
    }

    async fn get_edge_including_deleted(&self, id: LinkId) -> StorageResult<Option<Edge>> {
        self.get_edge(id).await
    }

    async fn get_edge_by_natural_key_including_deleted(
        &self,
        _namespace: &str,
        _source_id: Uuid,
        _target_id: Uuid,
        _relation: EdgeRelation,
    ) -> StorageResult<Option<Edge>> {
        Ok(None)
    }

    async fn delete_edge(&self, _id: LinkId, _mode: DeleteMode) -> StorageResult<bool> {
        Ok(false)
    }

    async fn query_edges(
        &self,
        filter: EdgeFilter,
        _sort: Vec<SortOrder<EdgeSortField>>,
        page: PageRequest,
    ) -> StorageResult<Page<Edge>> {
        let mut matching: Vec<Edge> = self
            .edges
            .lock()
            .unwrap()
            .iter()
            .filter(|e| filter.relations.is_empty() || filter.relations.contains(&e.relation))
            .cloned()
            .collect();
        matching.sort_by_key(|e| (e.created_at, Uuid::from(e.id)));
        let total = matching.len() as u64;
        let start = (page.offset as usize).min(matching.len());
        let end = (start + page.limit as usize).min(matching.len());
        Ok(Page {
            items: matching[start..end].to_vec(),
            total: Some(total),
        })
    }

    async fn count_edges(&self, filter: EdgeFilter) -> StorageResult<u64> {
        Ok(self
            .edges
            .lock()
            .unwrap()
            .iter()
            .filter(|e| filter.relations.is_empty() || filter.relations.contains(&e.relation))
            .count() as u64)
    }

    async fn count_edges_by_relation(&self) -> StorageResult<Vec<(EdgeRelation, u64)>> {
        Ok(Vec::new())
    }

    async fn query_edges_after(
        &self,
        _filter: EdgeFilter,
        _after: Option<Uuid>,
        _limit: u32,
    ) -> StorageResult<EdgeSeekPage> {
        Ok(EdgeSeekPage::default())
    }

    async fn neighbors(
        &self,
        _node_id: Uuid,
        _query: NeighborQuery,
    ) -> StorageResult<Vec<NeighborHit>> {
        Ok(Vec::new())
    }

    async fn traverse(&self, _request: TraversalRequest) -> StorageResult<Vec<GraphPath>> {
        Ok(Vec::new())
    }

    async fn purge_incident_edges(&self, _node_id: Uuid) -> StorageResult<u64> {
        Ok(0)
    }
}

fn make_edge(namespace: &str, id: Uuid, created_at_micros: i64) -> Edge {
    let created_at =
        chrono::DateTime::<chrono::Utc>::from_timestamp_micros(created_at_micros).unwrap();
    Edge {
        id: id.into(),
        namespace: namespace.to_string(),
        source_id: Uuid::nil(),
        target_id: Uuid::nil(),
        relation: EdgeRelation::Extends,
        weight: 0.5,
        created_at,
        updated_at: created_at,
        deleted_at: None,
        metadata: None,
        target_backend: None,
    }
}

#[tokio::test]
async fn query_edges_in_namespaces_default_rejects_multi_namespace_by_name() {
    let store = TraitDefaultOnlyGraphStore::new(vec![make_edge("ns-a", Uuid::new_v4(), 1_000_000)]);

    let sort = vec![SortOrder {
        field: EdgeSortField::CreatedAt,
        direction: SortDirection::Asc,
    }];

    // Empty visibility set: the default yields an empty page, not an error.
    let empty = store
        .query_edges_in_namespaces(
            &[],
            EdgeFilter::default(),
            sort.clone(),
            PageRequest::default(),
        )
        .await
        .unwrap();
    assert_eq!(empty.items.len(), 0);
    assert_eq!(empty.total, None);

    // Single namespace: the default delegates to `query_edges` and succeeds.
    let single = store
        .query_edges_in_namespaces(
            &["ns-a".to_string()],
            EdgeFilter::default(),
            sort.clone(),
            PageRequest::default(),
        )
        .await
        .unwrap();
    assert_eq!(single.items.len(), 1);

    // Multi-namespace: the default must reject explicitly, using the exact
    // operation name `khive-runtime::operations::KhiveRuntime::list_edges`
    // matches on to trigger its per-namespace merge fallback. A rename here
    // that isn't mirrored in the runtime's `if operation == "..."` guard
    // would silently stop the fallback from firing.
    let err = store
        .query_edges_in_namespaces(
            &["ns-a".to_string(), "ns-b".to_string()],
            EdgeFilter::default(),
            sort,
            PageRequest::default(),
        )
        .await
        .unwrap_err();
    match err {
        StorageError::Unsupported {
            capability,
            operation,
            ..
        } => {
            assert_eq!(capability, StorageCapability::Graph);
            assert_eq!(operation, "query_edges_in_namespaces");
        }
        other => panic!("expected StorageError::Unsupported, got {other:?}"),
    }
}
