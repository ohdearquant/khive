//! Graph storage capability — edge CRUD and traversal.

use async_trait::async_trait;
use khive_types::EdgeRelation;
use uuid::Uuid;

use crate::capability::StorageCapability;
use crate::error::StorageError;
use crate::types::{
    BatchWriteSummary, DeleteMode, DirectedNeighborHit, Direction, Edge, EdgeFilter, EdgeSeekPage,
    EdgeSortField, GraphPath, GuardedBatchOutcome, GuardedWriteOutcome, LinkId, NeighborHit,
    NeighborQuery, Page, PageRequest, SeekCursor, SeekPage, SortOrder, StorageResult,
    TraversalRequest,
};

/// Directed edge CRUD and graph traversal over the knowledge graph.
#[async_trait]
pub trait GraphStore: Send + Sync + 'static {
    /// Insert or update a single edge.
    async fn upsert_edge(&self, edge: Edge) -> StorageResult<()>;
    /// Insert or update a batch of edges.
    async fn upsert_edges(&self, edges: Vec<Edge>) -> StorageResult<BatchWriteSummary>;
    /// Insert or update a single edge, re-checking that both endpoints still
    /// exist (and are not soft-deleted) as part of the same write, not a
    /// separate prior read. Closes the TOCTOU window between an async
    /// prepare-time existence check and a later, unconditional write: a
    /// concurrent hard-delete of an endpoint that lands between the two can
    /// otherwise leave a durably dangling edge (#769).
    ///
    /// Returns [`GuardedWriteOutcome::Refused`] naming exactly which
    /// endpoint(s) were missing, determined by the guard's own in-transaction
    /// probe — never reconstructed by a caller re-reading the endpoints after
    /// the write already failed, since a concurrent write landing between the
    /// refusal and any such later read could misreport which endpoint was
    /// actually missing at write time.
    ///
    /// Default returns `StorageError::Unsupported`: a backend that does not
    /// override this method cannot honor the endpoint-existence guarantee,
    /// and silently falling back to [`GraphStore::upsert_edge`] would
    /// reintroduce the TOCTOU window this method exists to close.
    async fn upsert_edge_guarded(&self, _edge: Edge) -> StorageResult<GuardedWriteOutcome> {
        Err(StorageError::Unsupported {
            capability: StorageCapability::Graph,
            operation: "upsert_edge_guarded".into(),
            message: "this backend does not implement guarded edge writes".into(),
        })
    }
    /// Batch form of [`GraphStore::upsert_edge_guarded`]. All-or-nothing:
    /// if any edge's endpoints are missing at write time, no edge from the
    /// batch is persisted, `BatchWriteSummary::affected` is `0`, and
    /// `GuardedBatchOutcome::refused` names the first failing batch entry and
    /// its missing endpoint(s) — determined by the same in-transaction
    /// pre-check that aborted the batch, not a post-hoc re-read.
    ///
    /// Default returns `StorageError::Unsupported`, for the same reason as
    /// [`GraphStore::upsert_edge_guarded`]'s default.
    async fn upsert_edges_guarded(&self, _edges: Vec<Edge>) -> StorageResult<GuardedBatchOutcome> {
        Err(StorageError::Unsupported {
            capability: StorageCapability::Graph,
            operation: "upsert_edges_guarded".into(),
            message: "this backend does not implement guarded edge writes".into(),
        })
    }
    /// Fetch an edge by link ID, returning `None` if absent. Filters soft-deleted rows.
    async fn get_edge(&self, id: LinkId) -> StorageResult<Option<Edge>>;
    /// Fetch an edge by link ID including soft-deleted rows. Used by the runtime hard-delete path
    /// to locate and namespace-check an already-soft-deleted edge before purging it.
    async fn get_edge_including_deleted(&self, id: LinkId) -> StorageResult<Option<Edge>>;
    /// Fetch an edge by natural key (namespace, source, target, relation) including
    /// soft-deleted rows. Used by the atomic-apply result renderer for a symmetric-relation
    /// update whose surviving canonical row may be tombstoned (ADR-039 DO NOTHING) — the
    /// normal `query_edges`/`list_edges` path filters `deleted_at IS NULL` and would report
    /// "not found" for exactly that row.
    ///
    /// `namespace` is the natural key's own `namespace` column value (part of the
    /// `UNIQUE(namespace, source_id, target_id, relation)` constraint this method queries by)
    /// — it is passed explicitly rather than implied by whichever store instance `self` is,
    /// so a caller who resolved the record's namespace independently of its own ambient token
    /// (the atomic-apply renderer, which knows the committed edge's namespace from its prepare-
    /// time `EdgeNaturalKey`, not from the caller's token) cannot accidentally query the wrong
    /// namespace by relying on implicit store scoping.
    async fn get_edge_by_natural_key_including_deleted(
        &self,
        namespace: &str,
        source_id: Uuid,
        target_id: Uuid,
        relation: EdgeRelation,
    ) -> StorageResult<Option<Edge>>;
    /// Delete an edge by link ID using the specified delete mode.
    async fn delete_edge(&self, id: LinkId, mode: DeleteMode) -> StorageResult<bool>;
    /// Query edges with filter, sort, and pagination.
    async fn query_edges(
        &self,
        filter: EdgeFilter,
        sort: Vec<SortOrder<EdgeSortField>>,
        page: PageRequest,
    ) -> StorageResult<Page<Edge>>;
    /// Query edges across the given namespaces in one deterministic query
    /// with real SQL paging. The multi-namespace analogue of
    /// [`Self::query_edges`]: a single statement with `namespace IN (...)`
    /// keeps `offset` continuation coherent, where fetching per-namespace
    /// prefixes and slicing a client-side merge floats the window between
    /// calls (silent duplicate/skip enumeration). Backends without batched
    /// namespace support retain the single-namespace path and reject
    /// multi-namespace requests explicitly.
    async fn query_edges_in_namespaces(
        &self,
        namespaces: &[String],
        filter: EdgeFilter,
        sort: Vec<SortOrder<EdgeSortField>>,
        page: PageRequest,
    ) -> StorageResult<Page<Edge>> {
        match namespaces.len() {
            0 => Ok(Page {
                items: Vec::new(),
                total: Some(0),
            }),
            1 => self.query_edges(filter, sort, page).await,
            _ => Err(StorageError::Unsupported {
                capability: StorageCapability::Graph,
                operation: "query_edges_in_namespaces".into(),
                message: "this backend does not implement batched namespace edge queries".into(),
            }),
        }
    }
    /// Count edges matching the given filter.
    async fn count_edges(&self, filter: EdgeFilter) -> StorageResult<u64>;
    /// Count edges across the given namespaces in one aggregate query.
    /// Backends without batched namespace support retain the single-namespace
    /// path and reject multi-namespace requests explicitly.
    async fn count_edges_in_namespaces(
        &self,
        namespaces: &[String],
        filter: EdgeFilter,
    ) -> StorageResult<u64> {
        match namespaces.len() {
            0 => Ok(0),
            1 => self.count_edges(filter).await,
            _ => Err(StorageError::Unsupported {
                capability: StorageCapability::Graph,
                operation: "count_edges_in_namespaces".into(),
                message: "this backend does not implement batched namespace edge counts".into(),
            }),
        }
    }
    /// Count edges grouped by relation, ignoring soft-deleted rows. Cheap
    /// aggregate (`GROUP BY relation`) used to report the true per-relation
    /// population for full-graph audits (#702.3).
    async fn count_edges_by_relation(&self) -> StorageResult<Vec<(EdgeRelation, u64)>>;
    /// Count edges grouped by relation across the given namespaces in one
    /// aggregate query.
    async fn count_edges_by_relation_in_namespaces(
        &self,
        namespaces: &[String],
    ) -> StorageResult<Vec<(EdgeRelation, u64)>> {
        match namespaces.len() {
            0 => Ok(Vec::new()),
            1 => self.count_edges_by_relation().await,
            _ => Err(StorageError::Unsupported {
                capability: StorageCapability::Graph,
                operation: "count_edges_by_relation_in_namespaces".into(),
                message: "this backend does not implement batched namespace relation counts".into(),
            }),
        }
    }
    /// Seek-pagination page of edges ordered by `id` ascending, using an
    /// indexed range scan (`id > after`) against the `(namespace, id)`
    /// primary key instead of `OFFSET`. `after` is exclusive; `None` starts
    /// from the beginning of the set. This remains an efficient compatibility
    /// path for a fixed edge set, but random UUIDs inserted concurrently may
    /// sort behind an issued boundary. Public concurrent walks use
    /// [`Self::query_edges_sequence_after`] instead (#1424).
    async fn query_edges_after(
        &self,
        filter: EdgeFilter,
        after: Option<Uuid>,
        limit: u32,
    ) -> StorageResult<EdgeSeekPage>;
    /// Resolve an edge id to its immutable insertion sequence.
    async fn edge_sequence(&self, _id: Uuid) -> StorageResult<Option<i64>> {
        Err(StorageError::Unsupported {
            capability: StorageCapability::Graph,
            operation: "edge_sequence".into(),
            message: "this backend does not implement edge insertion sequences".into(),
        })
    }
    /// Resolve edge ids to immutable insertion sequences. Implementations may
    /// override this to batch the lookup; the default preserves correctness.
    async fn edge_sequences(&self, ids: &[Uuid]) -> StorageResult<Vec<(Uuid, i64)>> {
        let mut resolved = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(sequence) = self.edge_sequence(*id).await? {
                resolved.push((*id, sequence));
            }
        }
        Ok(resolved)
    }
    /// Seek-pagination page ordered by immutable insertion sequence. This is
    /// the stable public-list contract for walks overlapping inserts (#1424).
    async fn query_edges_sequence_after(
        &self,
        _filter: EdgeFilter,
        _after: Option<SeekCursor>,
        _limit: u32,
    ) -> StorageResult<SeekPage<Edge>> {
        Err(StorageError::Unsupported {
            capability: StorageCapability::Graph,
            operation: "query_edges_sequence_after".into(),
            message: "this backend does not implement insertion-sequence edge pagination".into(),
        })
    }
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
    /// Bounded multi-hop BFS traversal from the given roots.
    ///
    /// Implementations must validate [`TraversalRequest::validate`], count
    /// adjacency rows before first-visit de-duplication against the request's
    /// shared execution budget, stop a root as soon as its effective result
    /// limit is filled, and return an error rather than partial paths when the
    /// work or time budget expires. Minimum-depth BFS selection is normative;
    /// same-depth tie ordering is not.
    async fn traverse(&self, request: TraversalRequest) -> StorageResult<Vec<GraphPath>>;
    /// Hard-delete every incident edge (source or target) for `node_id`, regardless of soft-delete
    /// state. Used during endpoint hard-delete to prevent dangling `graph_edges` rows (ADR-002
    /// no-dangling-references contract).
    async fn purge_incident_edges(&self, node_id: Uuid) -> StorageResult<u64>;
}
