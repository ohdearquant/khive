use super::{
    direction_sort_rank, normalize_symmetric_direction, DirectedNeighborHit, Direction,
    EdgeRelation, Entity, FromStr, GraphPath, HashMap, KhiveRuntime, NamespaceToken,
    NeighborCursor, NeighborHit, NeighborQuery, Note, RuntimeError, RuntimeResult, SqlStatement,
    SqlValue, TraversalRequest, Uuid,
};

fn note_graph_name(note: &Note) -> String {
    note.name
        .as_deref()
        .filter(|name| !name.trim().is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("[{}]", note.kind))
}

/// Collapse per-namespace `GraphPath`s from [`KhiveRuntime::traverse`] down to exactly
/// one entry per distinct `root_id`, merging by `(root_id, node_id)` (shallowest depth
/// wins), BFS-ordering the result, and re-applying `limit`. See
/// docs/operations.md#merge_traversal_paths_by_root for why naive concatenation is unsound.
pub(super) fn merge_traversal_paths_by_root(
    paths: Vec<GraphPath>,
    limit: Option<u32>,
) -> Vec<GraphPath> {
    let mut order: Vec<Uuid> = Vec::new();
    let mut merged: HashMap<Uuid, GraphPath> = HashMap::new();
    // root_id -> (node_id -> index into merged[root_id].nodes), so a
    // shallower depth for an already-seen node updates in place instead of
    // rebuilding a seen-set from every prior namespace's contribution.
    let mut node_index: HashMap<Uuid, HashMap<Uuid, usize>> = HashMap::new();

    for path in paths {
        let existing = merged.entry(path.root_id).or_insert_with(|| {
            order.push(path.root_id);
            GraphPath {
                root_id: path.root_id,
                nodes: Vec::new(),
                total_weight: 0.0,
            }
        });
        let index = node_index.entry(path.root_id).or_default();
        for node in path.nodes {
            match index.get(&node.node_id) {
                Some(&i) => {
                    if node.depth < existing.nodes[i].depth {
                        existing.nodes[i] = node;
                    }
                }
                None => {
                    index.insert(node.node_id, existing.nodes.len());
                    existing.nodes.push(node);
                }
            }
        }
    }

    order
        .into_iter()
        .filter_map(|root_id| merged.remove(&root_id))
        .map(|mut path| {
            // BFS order: ascending depth, stable within a depth.
            path.nodes.sort_by_key(|n| n.depth);
            if let Some(lim) = limit {
                let lim = lim as usize;
                let mut non_root_kept = 0usize;
                path.nodes.retain(|n| {
                    if n.depth == 0 {
                        return true;
                    }
                    if non_root_kept < lim {
                        non_root_kept += 1;
                        true
                    } else {
                        false
                    }
                });
            }
            recompute_total_weight(&mut path);
            path
        })
        .collect()
}

/// Set `total_weight` to the maximum cumulative path weight among the nodes
/// the path currently holds, matching how storage derives it for a
/// single-namespace traversal.
///
/// Call this after any edit to `nodes`. Carrying a weight across an edit is
/// what lets the field describe a node the caller was never shown: the
/// highest-weighted candidate is exactly the one a `limit` or a
/// soft-delete screen can remove while the summary keeps quoting it.
fn recompute_total_weight(path: &mut GraphPath) {
    path.total_weight = path.nodes.iter().map(|n| n.weight).fold(0.0_f64, f64::max);
}

impl KhiveRuntime {
    /// Returns `true` if `id` resolves to a live substrate record in the
    /// caller's visible namespace set.
    ///
    /// Covers entity, note, event (via `resolve`) and edge (via `get_edge_visible`).
    /// Only records that are accessible to the caller (primary or configured visible
    /// namespaces) return `true`; absent or foreign-invisible records return `false`.
    pub(crate) async fn substrate_exists_in_ns(
        &self,
        token: &NamespaceToken,
        id: Uuid,
    ) -> RuntimeResult<bool> {
        if self.resolve(token, id).await?.is_some() {
            return Ok(true);
        }
        match self.get_edge_visible(token, id).await {
            Ok(Some(_)) => Ok(true),
            Ok(None) | Err(RuntimeError::NotFound(_)) => Ok(false),
            Err(err) => Err(err),
        }
    }

    /// Returns `true` if `id` resolves to a live substrate record, by ID, with
    /// no namespace filter.
    ///
    /// Used from `annotates` endpoint validation (`link` and `create`'s
    /// `annotates` targets), which consume a by-ID endpoint and so must follow
    /// the same namespace-agnostic by-ID contract as `get()`.
    pub(crate) async fn substrate_exists_by_id(
        &self,
        token: &NamespaceToken,
        id: Uuid,
    ) -> RuntimeResult<bool> {
        if self.resolve_edge_endpoint(token, id).await?.is_some() {
            return Ok(true);
        }
        match self.get_edge(token, id).await {
            Ok(Some(_)) => Ok(true),
            Ok(None) | Err(RuntimeError::NotFound(_)) => Ok(false),
            Err(err) => Err(err),
        }
    }

    /// Find the newest live annotation note with an exact kind and tag across
    /// the token's visible edge namespaces, on this runtime's bound backend.
    /// Each store selects one eligible candidate before returning; note bodies
    /// and the complete annotation history are never hydrated here.
    pub async fn latest_annotating_note(
        &self,
        token: &NamespaceToken,
        node_id: Uuid,
        kind: &str,
        tag: &str,
    ) -> RuntimeResult<Option<Uuid>> {
        self.latest_annotating_note_inner(token, node_id, kind, tag, None)
            .await
    }

    /// Select a latest annotation only after its exact top-level string
    /// property has been checked by the bound store.
    pub async fn latest_annotating_note_with_property(
        &self,
        token: &NamespaceToken,
        node_id: Uuid,
        kind: &str,
        tag: &str,
        property_key: &str,
        property_value: &str,
    ) -> RuntimeResult<Option<Uuid>> {
        self.latest_annotating_note_inner(
            token,
            node_id,
            kind,
            tag,
            Some((property_key, property_value)),
        )
        .await
    }

    async fn latest_annotating_note_inner(
        &self,
        token: &NamespaceToken,
        node_id: Uuid,
        kind: &str,
        tag: &str,
        required_property: Option<(&str, &str)>,
    ) -> RuntimeResult<Option<Uuid>> {
        if !self.substrate_exists_in_ns(token, node_id).await? {
            return Ok(None);
        }
        let mut latest: Option<(Uuid, i64)> = None;
        for namespace in token.visible_namespaces() {
            let scoped = NamespaceToken::for_namespace(namespace.clone());
            let graph = self.graph(&scoped)?;
            let candidate = match required_property {
                Some((key, value)) => {
                    graph
                        .latest_annotating_note_with_property(node_id, kind, tag, key, value)
                        .await?
                }
                None => graph.latest_annotating_note(node_id, kind, tag).await?,
            };
            if let Some(candidate) = candidate {
                if latest.is_none_or(|(id, created_at)| {
                    candidate.1 > created_at || (candidate.1 == created_at && candidate.0 < id)
                }) {
                    latest = Some(candidate);
                }
            }
        }
        Ok(latest.map(|(id, _)| id))
    }

    /// Get immediate neighbors of a node, optionally filtered by relation type.
    ///
    /// Pass `relations: Some(vec![EdgeRelation::Annotates])` to retrieve only
    /// annotation edges, enabling cross-substrate navigation.
    ///
    /// Symmetric relations (`competes_with`, `composed_with`) are stored
    /// with the canonical source as the lower UUID. Direction normalization is
    /// applied in `neighbors_with_query` so both callers see correct results.
    pub async fn neighbors(
        &self,
        token: &NamespaceToken,
        node_id: Uuid,
        direction: Direction,
        limit: Option<u32>,
        relations: Option<Vec<EdgeRelation>>,
    ) -> RuntimeResult<Vec<NeighborHit>> {
        self.neighbors_with_query(
            token,
            node_id,
            NeighborQuery {
                direction,
                relations,
                limit,
                min_weight: None,
            },
        )
        .await
    }

    /// Get neighbors with full query control (includes `min_weight`).
    ///
    /// Applies symmetric-relation direction normalization: if the
    /// relations filter contains only symmetric relations the direction is
    /// overridden to `Both` so edges stored in canonical order are always found.
    ///
    /// Soft-deleted entity nodes are excluded from results unless the caller
    /// explicitly requested them (future: `include_deleted` flag; currently
    /// always false).
    pub async fn neighbors_with_query(
        &self,
        token: &NamespaceToken,
        node_id: Uuid,
        query: NeighborQuery,
    ) -> RuntimeResult<Vec<NeighborHit>> {
        self.neighbors_with_query_page(token, node_id, query, None, None, true)
            .await
    }

    /// Get a deterministic neighbor page, optionally applying a continuation
    /// cursor and filtering entity/note kinds before the storage limit.
    /// `enrich` is false for the lightweight edge projection.
    pub async fn neighbors_with_query_page(
        &self,
        token: &NamespaceToken,
        node_id: Uuid,
        query: NeighborQuery,
        after: Option<NeighborCursor>,
        neighbor_kinds: Option<Vec<String>>,
        enrich: bool,
    ) -> RuntimeResult<Vec<NeighborHit>> {
        // A full-UUID anchor follows get's by-ID lookup. Only the adjacency
        // expansion below is scoped to the caller's visible namespaces.
        if !self.substrate_exists_by_id(token, node_id).await? {
            return Err(RuntimeError::NotFound(format!(
                "neighbor anchor {node_id} not found"
            )));
        }

        self.neighbors_for_resolved_kg_read(
            token,
            node_id,
            crate::KgNeighborRead {
                query,
                after,
                neighbor_kinds,
                enrich,
                namespace: None,
            },
        )
        .await
    }

    /// Expand an already resolved live KG origin on this runtime's graph.
    ///
    /// The caller must verify the origin's existence and apply any record-kind
    /// read scope before calling. The original caller token is retained for
    /// namespace selection and enrichment; an optional namespace may only
    /// narrow its visible set. Like the ordinary neighbor read, resolution and
    /// adjacency are separate reads rather than an atomic record snapshot.
    pub async fn neighbors_for_resolved_kg_read(
        &self,
        token: &NamespaceToken,
        node_id: Uuid,
        options: crate::KgNeighborRead,
    ) -> RuntimeResult<Vec<NeighborHit>> {
        self.neighbors_for_resolved_kg_read_inner(token, node_id, options, false)
            .await
            .map(|(hits, _)| hits)
    }

    /// Expand a resolved origin and return visible live entity-kind hints for
    /// mailbox endpoint checks. Lightweight projections obtain these hints in
    /// the existing deletion-screen read, without enriching the returned hits.
    /// Missing hints still require the owning message-note backend's policy read.
    pub async fn neighbors_for_resolved_kg_read_with_entity_kinds(
        &self,
        token: &NamespaceToken,
        node_id: Uuid,
        options: crate::KgNeighborRead,
    ) -> RuntimeResult<(Vec<NeighborHit>, HashMap<Uuid, String>)> {
        self.neighbors_for_resolved_kg_read_inner(token, node_id, options, true)
            .await
    }

    async fn neighbors_for_resolved_kg_read_inner(
        &self,
        token: &NamespaceToken,
        node_id: Uuid,
        options: crate::KgNeighborRead,
        with_entity_kinds: bool,
    ) -> RuntimeResult<(Vec<NeighborHit>, HashMap<Uuid, String>)> {
        let crate::KgNeighborRead {
            mut query,
            after,
            neighbor_kinds,
            enrich,
            namespace,
        } = options;
        let namespaces = crate::kg_read::neighbor_read_namespaces(token, namespace.as_ref())?;
        query.direction =
            normalize_symmetric_direction(query.direction, query.relations.as_deref());
        let mut hits = Vec::new();
        for ns in namespaces {
            let temp = NamespaceToken::for_namespace(ns.clone());
            let mut ns_hits = self
                .graph(&temp)?
                .neighbors_page(node_id, query.clone(), after, neighbor_kinds.clone())
                .await?;
            hits.append(&mut ns_hits);
        }
        hits.sort_by_key(|h| (h.node_id, h.edge_id));
        hits.dedup_by_key(|h| (h.node_id, h.edge_id));
        if enrich {
            self.enrich_neighbor_hits(token, &mut hits).await;
        }
        // Filter out soft-deleted entity nodes.
        let candidate_ids: Vec<Uuid> = hits.iter().map(|h| h.node_id).collect();
        let (deleted, entity_kinds) = self
            .neighbor_node_screen(
                candidate_ids,
                (with_entity_kinds && !enrich).then_some(token),
            )
            .await?;
        if !deleted.is_empty() {
            hits.retain(|h| !deleted.contains(&h.node_id));
        }
        // Restore the weight-descending, node_id-ascending order the storage
        // layer established (khive-db graph.rs `ORDER BY weight DESC, node_id
        // ASC`) — the (node_id, edge_id) sort above exists only to make
        // `dedup_by_key` adjacent-comparable and otherwise discards it. This
        // ordering contract must hold at every call site of this op (context
        // and neighbors verb alike), for every direction.
        hits.sort_by(|a, b| {
            b.weight
                .partial_cmp(&a.weight)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.node_id.cmp(&b.node_id))
                .then(a.edge_id.cmp(&b.edge_id))
        });
        Ok((hits, entity_kinds))
    }

    /// Find live `annotates` edges targeting one record without applying a
    /// namespace predicate.
    ///
    /// This is the graph counterpart to the namespace-agnostic by-ID `get`
    /// contract (ADR-007 Rev 6). Multi-record neighbor traversal remains
    /// visibility-scoped; callers should use this only after resolving a live
    /// target through a by-ID operation.
    pub async fn annotation_neighbors_by_target_id(
        &self,
        target_id: Uuid,
    ) -> RuntimeResult<Vec<NeighborHit>> {
        let mut reader = self.sql().reader().await?;
        let rows = reader
            .query_all(SqlStatement {
                sql: "SELECT source_id, id, weight FROM graph_edges \
                      WHERE target_id = ?1 AND relation = ?2 AND deleted_at IS NULL \
                      ORDER BY weight DESC, source_id ASC"
                    .to_string(),
                params: vec![
                    SqlValue::Text(target_id.to_string()),
                    SqlValue::Text(EdgeRelation::Annotates.to_string()),
                ],
                label: Some("annotations.by_target_id_unfiltered".into()),
            })
            .await?;

        rows.into_iter()
            .map(|row| {
                let parse_uuid = |name: &str| match row.get(name) {
                    Some(SqlValue::Text(value)) => Uuid::from_str(value).map_err(|error| {
                        RuntimeError::Internal(format!("graph_edges.{name} is not a UUID: {error}"))
                    }),
                    Some(value) => Err(RuntimeError::Internal(format!(
                        "graph_edges.{name} has unexpected SQL value {value:?}"
                    ))),
                    None => Err(RuntimeError::Internal(format!(
                        "graph_edges row missing {name}"
                    ))),
                };
                let weight = match row.get("weight") {
                    Some(SqlValue::Float(value)) => Ok(*value),
                    Some(value) => Err(RuntimeError::Internal(format!(
                        "graph_edges.weight has unexpected SQL value {value:?}"
                    ))),
                    None => Err(RuntimeError::Internal(
                        "graph_edges row missing weight".into(),
                    )),
                }?;

                Ok(NeighborHit {
                    node_id: parse_uuid("source_id")?,
                    edge_id: parse_uuid("id")?,
                    relation: EdgeRelation::Annotates,
                    weight,
                    name: None,
                    kind: None,
                    entity_type: None,
                })
            })
            .collect()
    }

    /// Get both-direction neighbors, each tagged with the direction (`Out`/
    /// `In`) it was found in, via a single storage query per visible
    /// namespace instead of two separate direction-scoped `neighbors_with_query`
    /// calls: halving the neighbor SELECT count for `context(direction="both")`
    /// expansion. `query.direction` is ignored: always both.
    ///
    /// Mirrors `neighbors_with_query`'s dedup/enrich/soft-delete-filter/order
    /// pipeline exactly, carrying the per-hit direction tag through unchanged.
    pub async fn neighbors_with_query_directed(
        &self,
        token: &NamespaceToken,
        node_id: Uuid,
        query: NeighborQuery,
    ) -> RuntimeResult<Vec<(NeighborHit, Direction)>> {
        if !self.substrate_exists_by_id(token, node_id).await? {
            return Err(RuntimeError::NotFound(format!(
                "neighbor anchor {node_id} not found"
            )));
        }

        self.directed_neighbors_for_resolved_kg_read(token, node_id, query, None)
            .await
    }

    pub(crate) async fn directed_neighbors_for_resolved_kg_read(
        &self,
        token: &NamespaceToken,
        node_id: Uuid,
        query: NeighborQuery,
        namespace: Option<&crate::Namespace>,
    ) -> RuntimeResult<Vec<(NeighborHit, Direction)>> {
        let namespaces = crate::kg_read::neighbor_read_namespaces(token, namespace)?;
        let mut hits: Vec<DirectedNeighborHit> = Vec::new();
        for ns in namespaces {
            let temp = NamespaceToken::for_namespace(ns.clone());
            let mut ns_hits = self
                .graph(&temp)?
                .neighbors_both_directions(node_id, query.clone())
                .await?;
            hits.append(&mut ns_hits);
        }
        // Direction is part of the key (not just node_id/edge_id) so a
        // self-loop's Out row and In row — same node_id and edge_id, opposite
        // direction: sort adjacent but distinct and both survive dedup.
        hits.sort_by_key(|h| {
            (
                h.hit.node_id,
                h.hit.edge_id,
                direction_sort_rank(&h.direction),
            )
        });
        hits.dedup_by_key(|h| {
            (
                h.hit.node_id,
                h.hit.edge_id,
                direction_sort_rank(&h.direction),
            )
        });

        let mut plain_hits: Vec<NeighborHit> = hits.iter().map(|h| h.hit.clone()).collect();
        self.enrich_neighbor_hits(token, &mut plain_hits).await;
        for (dh, enriched) in hits.iter_mut().zip(plain_hits) {
            dh.hit = enriched;
        }

        // Filter out soft-deleted entity nodes.
        let candidate_ids: Vec<Uuid> = hits.iter().map(|h| h.hit.node_id).collect();
        let deleted = self.deleted_entity_ids(candidate_ids).await?;
        if !deleted.is_empty() {
            hits.retain(|h| !deleted.contains(&h.hit.node_id));
        }
        // Same global weight-descending/node_id-ascending restore as
        // `neighbors_with_query`: the (node_id, edge_id, direction) sort above
        // exists only to make `dedup_by_key` adjacent-comparable.
        hits.sort_by(|a, b| {
            b.hit
                .weight
                .partial_cmp(&a.hit.weight)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.hit.node_id.cmp(&b.hit.node_id))
                .then(a.hit.edge_id.cmp(&b.hit.edge_id))
        });
        Ok(hits.into_iter().map(|h| (h.hit, h.direction)).collect())
    }

    /// Traverse the graph from a set of root nodes.
    ///
    /// Full-UUID roots use the by-ID contract; expansion and returned edges
    /// remain scoped to the caller's visible namespaces. Missing roots refuse.
    /// Soft-deleted entity nodes are excluded from results.
    pub async fn traverse(
        &self,
        token: &NamespaceToken,
        request: TraversalRequest,
    ) -> RuntimeResult<Vec<GraphPath>> {
        let mut request = request;
        request.validate().map_err(RuntimeError::InvalidInput)?;
        let mut roots = Vec::with_capacity(request.roots.len());
        let mut seen_roots = std::collections::HashSet::with_capacity(request.roots.len());
        for root in request.roots.drain(..) {
            if seen_roots.insert(root) {
                if !self.substrate_exists_by_id(token, root).await? {
                    return Err(RuntimeError::NotFound(format!(
                        "traverse root {root} not found"
                    )));
                }
                roots.push(root);
            }
        }
        request.roots = roots;
        if request.roots.is_empty() {
            return Ok(Vec::new());
        }

        let mut paths = Vec::new();
        for ns in token.visible_namespaces() {
            let temp = NamespaceToken::for_namespace(ns.clone());
            let mut ns_paths = self.graph(&temp)?.traverse(request.clone()).await?;
            paths.append(&mut ns_paths);
        }
        // Reconcile the per-namespace GraphPaths back down to one per
        // distinct root_id (see merge_traversal_paths_by_root for why this
        // is needed and what it enforces).
        let mut paths =
            merge_traversal_paths_by_root(paths, Some(request.options.effective_limit()));
        self.enrich_path_nodes(token, &mut paths, request.include_properties)
            .await;
        // Filter out soft-deleted entity nodes from all path nodes.
        let all_node_ids: Vec<Uuid> = paths
            .iter()
            .flat_map(|p| p.nodes.iter().map(|n| n.node_id))
            .collect();
        let deleted = self.deleted_entity_ids(all_node_ids).await?;
        if !deleted.is_empty() {
            for path in paths.iter_mut() {
                path.nodes.retain(|n| !deleted.contains(&n.node_id));
                recompute_total_weight(path);
            }
            paths.retain(|p| !p.nodes.is_empty());
        }
        Ok(paths)
    }

    /// Batch-query for soft-deleted UUIDs in `ids`, across BOTH the entities
    /// and notes tables.
    ///
    /// Neighbor/traverse candidates can be note-kind nodes (e.g. reached via
    /// `annotates` edges) as well as entities; a screen that only consults
    /// `entities` lets soft-deleted note targets leak through and hydrate as
    /// blank/missing hits. This is a view-layer read-only screen: it does
    /// not touch edges or mutate any data.
    ///
    /// Returns the subset of `ids` that have `deleted_at IS NOT NULL` in
    /// either table. Takes `Vec<Uuid>` (not an iterator) so the async state
    /// machine holds only owned data — no iterator borrow across yields.
    ///
    /// Propagates reader-admission and other storage errors instead of
    /// treating them as "nothing is deleted": on a saturated pool this query
    /// now runs on the bounded reader pool like any other read, and silently
    /// swallowing its failure would let soft-deleted nodes back into
    /// `neighbors`/`traverse` results instead of surfacing the retryable
    /// admission failure those callers otherwise promise.
    pub(super) async fn deleted_entity_ids(
        &self,
        ids: Vec<Uuid>,
    ) -> RuntimeResult<std::collections::HashSet<Uuid>> {
        self.neighbor_node_screen(ids, None)
            .await
            .map(|(deleted, _)| deleted)
    }

    /// Share the deletion-screen statement with optional live entity-kind
    /// hints. Only the caller's visible entity namespaces supply hints; note
    /// kinds are not inferred from this backend when notes may route elsewhere.
    async fn neighbor_node_screen(
        &self,
        ids: Vec<Uuid>,
        kind_token: Option<&NamespaceToken>,
    ) -> RuntimeResult<(std::collections::HashSet<Uuid>, HashMap<Uuid, String>)> {
        if ids.is_empty() {
            return Ok((std::collections::HashSet::new(), HashMap::new()));
        }
        let id_strs: Vec<String> = ids.iter().map(|u| u.to_string()).collect();
        let n = id_strs.len();
        // Each UNION half gets its OWN numbered-placeholder block (?1..?n for
        // entities, ?(n+1)..?(2n) for notes) — numbered SQLite params bind by
        // index, so reusing the same numbers across halves would silently
        // collapse to a single shared block instead of binding the full list
        // twice.
        let entities_placeholders = (0..n)
            .map(|i| format!("?{}", i + 1))
            .collect::<Vec<_>>()
            .join(",");
        let notes_placeholders = (0..n)
            .map(|i| format!("?{}", n + i + 1))
            .collect::<Vec<_>>()
            .join(",");
        let sql_str = if kind_token.is_some() {
            format!(
                "SELECT id, kind, namespace, deleted_at IS NOT NULL AS is_deleted \
                 FROM entities WHERE id IN ({entities_placeholders}) \
                 UNION ALL \
                 SELECT id, NULL, NULL, 1 FROM notes \
                 WHERE id IN ({notes_placeholders}) AND deleted_at IS NOT NULL"
            )
        } else {
            format!(
                "SELECT id FROM entities WHERE id IN ({entities_placeholders}) AND deleted_at IS NOT NULL \
                 UNION \
                 SELECT id FROM notes WHERE id IN ({notes_placeholders}) AND deleted_at IS NOT NULL"
            )
        };
        // Same id list bound twice — once per UNION arm's independent placeholder block.
        let params: Vec<SqlValue> = id_strs
            .iter()
            .chain(id_strs.iter())
            .cloned()
            .map(SqlValue::Text)
            .collect();
        let stmt = SqlStatement {
            sql: sql_str,
            params,
            label: Some("deleted_entity_ids".into()),
        };
        let mut out = std::collections::HashSet::new();
        let mut entity_kinds = HashMap::new();
        let sql = self.sql();
        let mut reader = sql.reader().await?;
        let rows = reader.query_all(stmt).await?;
        for row in rows {
            if let Some(col) = row.columns.first() {
                if let SqlValue::Text(s) = &col.value {
                    if let Ok(u) = s.parse::<Uuid>() {
                        if kind_token.is_none()
                            || matches!(
                                row.columns.get(3).map(|col| &col.value),
                                Some(SqlValue::Integer(1))
                            )
                        {
                            out.insert(u);
                        } else if let (
                            Some(token),
                            Some(SqlValue::Text(kind)),
                            Some(SqlValue::Text(namespace)),
                            Some(SqlValue::Integer(0)),
                        ) = (
                            kind_token,
                            row.columns.get(1).map(|col| &col.value),
                            row.columns.get(2).map(|col| &col.value),
                            row.columns.get(3).map(|col| &col.value),
                        ) {
                            if token
                                .visible_namespaces()
                                .iter()
                                .any(|ns| ns.as_str() == namespace.as_str())
                            {
                                entity_kinds.insert(u, kind.clone());
                            }
                        }
                    }
                }
            }
        }
        Ok((out, entity_kinds))
    }

    /// Populate `name` and `kind` on each `NeighborHit` from the corresponding
    /// entity or note record. Best-effort: unresolved IDs leave the fields `None`.
    ///
    /// Uses a single batched entity lookup via `get_entities_by_ids_visible`
    /// (scoped to the token's full visible-namespace set so that neighbors in
    /// extra-visible namespaces are enriched), then a batched note lookup
    /// (`get_notes_batch`) for the residual IDs not resolved as entities.
    /// Order and identity of hits is preserved via `HashMap` re-index.
    pub(super) async fn enrich_neighbor_hits(
        &self,
        token: &NamespaceToken,
        hits: &mut [NeighborHit],
    ) {
        if hits.is_empty() {
            return;
        }

        // Deduplicated IDs for the batch call.
        let unique_ids: Vec<Uuid> = {
            let mut seen = std::collections::HashSet::new();
            hits.iter()
                .filter_map(|h| {
                    if seen.insert(h.node_id) {
                        Some(h.node_id)
                    } else {
                        None
                    }
                })
                .collect()
        };

        let entity_map: HashMap<Uuid, Entity> = self
            .get_entities_by_ids_visible(token, &unique_ids)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|e| (e.id, e))
            .collect();

        // Batch note lookup for IDs not found as entities.
        let residual_ids: Vec<Uuid> = unique_ids
            .iter()
            .filter(|id| !entity_map.contains_key(id))
            .copied()
            .collect();

        let note_map: HashMap<Uuid, Note> = if !residual_ids.is_empty() {
            if let Ok(store) = self.notes(token) {
                store
                    .get_notes_batch(&residual_ids)
                    .await
                    .unwrap_or_default()
                    .into_iter()
                    .map(|n| (n.id, n))
                    .collect()
            } else {
                HashMap::new()
            }
        } else {
            HashMap::new()
        };

        for hit in hits.iter_mut() {
            if let Some(entity) = entity_map.get(&hit.node_id) {
                hit.name = Some(entity.name.clone());
                hit.kind = Some(entity.kind.clone());
                hit.entity_type = entity.entity_type.clone();
            } else if let Some(note) = note_map.get(&hit.node_id) {
                hit.name = Some(note_graph_name(note));
                hit.kind = Some(note.kind.clone());
            }
        }
    }

    /// Populate `name` and `kind` on each `PathNode` from the corresponding
    /// entity or note record. Same best-effort policy as `enrich_neighbor_hits`.
    ///
    /// Uses `get_entities_by_ids_visible` so that path nodes whose entities
    /// live in extra-visible namespaces are enriched correctly. Node IDs that
    /// repeat across paths are fetched exactly once.
    ///
    /// `include_properties` gates whether `entity.properties` is cloned onto
    /// each node. When `false` (the default), the potentially large JSON blob
    /// is never read from the map, keeping the hot path allocation-free.
    pub(super) async fn enrich_path_nodes(
        &self,
        token: &NamespaceToken,
        paths: &mut [GraphPath],
        include_properties: bool,
    ) {
        if paths.is_empty() {
            return;
        }

        // Deduplicate node IDs across all paths before the batch call.
        let unique_ids: Vec<Uuid> = {
            let mut seen = std::collections::HashSet::new();
            paths
                .iter()
                .flat_map(|p| p.nodes.iter())
                .filter_map(|n| {
                    if seen.insert(n.node_id) {
                        Some(n.node_id)
                    } else {
                        None
                    }
                })
                .collect()
        };

        let entity_map: HashMap<Uuid, Entity> = self
            .get_entities_by_ids_visible(token, &unique_ids)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|e| (e.id, e))
            .collect();

        let residual_ids: Vec<Uuid> = unique_ids
            .iter()
            .filter(|id| !entity_map.contains_key(id))
            .copied()
            .collect();

        let note_map: HashMap<Uuid, Note> = if !residual_ids.is_empty() {
            if let Ok(store) = self.notes(token) {
                store
                    .get_notes_batch(&residual_ids)
                    .await
                    .unwrap_or_default()
                    .into_iter()
                    .map(|n| (n.id, n))
                    .collect()
            } else {
                HashMap::new()
            }
        } else {
            HashMap::new()
        };

        for path in paths.iter_mut() {
            for node in path.nodes.iter_mut() {
                if let Some(entity) = entity_map.get(&node.node_id) {
                    node.name = Some(entity.name.clone());
                    node.kind = Some(entity.kind.clone());
                    if include_properties {
                        node.properties = entity.properties.clone();
                    }
                } else if let Some(note) = note_map.get(&node.node_id) {
                    node.name = Some(note_graph_name(note));
                    node.kind = Some(note.kind.clone());
                }
            }
        }
    }
}
