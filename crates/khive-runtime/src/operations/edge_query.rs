use std::collections::HashMap;

use serde::Serialize;
use uuid::Uuid;

use khive_db::stores::graph::edge_hard_delete_statement;
use khive_storage::graph::{SymmetricEdgeUpdateOutcome, SymmetricEdgeUpdateRequest};
use khive_storage::types::{
    DeleteMode, EdgeSortField, LinkId, PageRequest, SeekCursor, SortOrder, SqlRow, SqlStatement,
    SqlValue,
};
use khive_storage::{Edge, EdgeRelation};
use khive_types::{EventKind, SubstrateKind};

use crate::error::{RuntimeError, RuntimeResult};
use crate::runtime::{KhiveRuntime, NamespaceToken};

use super::{canonical_edge_endpoints, static_impossible_edge_pattern_warnings, EdgeReadWindow};

/// Result of a GQL/SPARQL query with optional validation warnings.
#[derive(Clone, Debug, Serialize)]
pub struct QueryResult {
    pub rows: Vec<SqlRow>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    /// Zero-based offset of this deterministic result page.
    pub offset: usize,
    /// Effective payload bound after composing query `LIMIT` and server page size.
    pub page_size: usize,
    /// `true` when at least one additional match exists after this page.
    pub has_more: bool,
    /// GQL continuation offset. Present exactly when GQL has another page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_offset: Option<usize>,
    /// Backward-compatible alias for `has_more` (#1168, #1247, #1601).
    pub truncated: bool,
}

impl KhiveRuntime {
    // ---- Query operations ----

    /// Execute a GQL or SPARQL query string, returning raw SQL rows.
    ///
    /// The query is compiled to SQL with namespace and own-mailbox scopes applied.
    /// GQL syntax: `MATCH (a:concept)-[e:extends]->(b) RETURN a, b LIMIT 10`
    /// SPARQL syntax: `SELECT ?a WHERE { ?a :kind "concept" . }`
    pub async fn query(&self, token: &NamespaceToken, query: &str) -> RuntimeResult<Vec<SqlRow>> {
        Ok(self
            .query_with_metadata(token, query, khive_query::CompileOptions::default())
            .await?
            .rows)
    }

    /// Execute a GQL/SPARQL query, returning rows and any validation warnings.
    pub async fn query_with_metadata(
        &self,
        token: &NamespaceToken,
        query: &str,
        mut opts: khive_query::CompileOptions,
    ) -> RuntimeResult<QueryResult> {
        use khive_query::QueryValue;
        use khive_storage::types::SqlValue;

        let (language, ast) = khive_query::language::parse_auto_with_language(query)?;
        if opts.max_limit == 0 {
            return Err(RuntimeError::InvalidInput(
                "query page size must be at least 1".into(),
            ));
        }
        let offset = ast.offset;
        let page_size = ast.limit.unwrap_or(opts.max_limit).min(opts.max_limit);
        opts.scopes = token
            .visible_namespaces()
            .iter()
            .map(|ns| ns.as_str().to_string())
            .collect();
        let mailbox = crate::MailboxView {
            actor_id: token.actor().id.clone(),
            delegated: false,
            maintainer_actor: self.config().actor_id.clone().filter(|id| !id.is_empty()),
        }
        .note_scope(token);
        let compiled = khive_query::compile_with_mailbox(
            &ast,
            &opts,
            &khive_query::QueryMailboxScope {
                actor_id: mailbox.actor_id,
                legacy_local: mailbox.legacy_local,
                maintainer_actor: mailbox.maintainer_actor,
            },
        )?;
        let mut warnings = compiled.warnings;
        let truncation_check = compiled.truncation_check;

        warnings.extend(self.with_pack_edge_rules(|pack_rules| {
            static_impossible_edge_pattern_warnings(language, &ast.pattern, pack_rules)
        }));

        // Convert QueryValue params (query-layer type) to SqlValue (storage-layer type)
        // at the query–storage boundary.
        let params: Vec<SqlValue> = compiled
            .params
            .into_iter()
            .map(|qv| match qv {
                QueryValue::Null => SqlValue::Null,
                QueryValue::Integer(n) => SqlValue::Integer(n),
                QueryValue::Float(f) => SqlValue::Float(f),
                QueryValue::Text(s) => SqlValue::Text(s),
                QueryValue::Blob(b) => SqlValue::Blob(b),
            })
            .collect();

        let mut reader = self.sql().reader().await?;
        let stmt = SqlStatement {
            sql: compiled.sql,
            params,
            label: None,
        };
        let mut rows = reader.query_all(stmt).await?;

        // When the effective page size was the binding constraint, the compiled
        // SQL asked for one extra (sentinel) row. Its presence in the actual
        // result set — not the requested LIMIT — is the continuation signal.
        let mut truncated = false;
        if let Some(check) = truncation_check {
            if rows.len() > check.max_limit {
                rows.truncate(check.max_limit);
                truncated = true;
            }
        }

        let next_offset = if truncated && language == khive_query::QueryLanguage::Gql {
            let next = offset.checked_add(rows.len()).ok_or_else(|| {
                RuntimeError::InvalidInput("GQL next_offset exceeds usize::MAX".into())
            })?;
            if next == offset {
                return Err(RuntimeError::InvalidInput(
                    "query page did not advance; page size must be at least 1".into(),
                ));
            }
            i64::try_from(next).map_err(|_| {
                RuntimeError::InvalidInput("GQL next_offset exceeds i64::MAX".into())
            })?;
            Some(next)
        } else {
            None
        };

        if truncated {
            let Some(check) = truncation_check else {
                return Err(RuntimeError::Internal(
                    "truncated query result is missing sentinel metadata".into(),
                ));
            };
            let bound = match check.requested_limit {
                Some(requested) => {
                    format!("requested query LIMIT {requested} exceeds the effective page size")
                }
                None => "the query has no explicit LIMIT".to_string(),
            };
            let warning = match language {
                khive_query::QueryLanguage::Gql => {
                    let Some(next) = next_offset else {
                        return Err(RuntimeError::Internal(
                            "truncated GQL result is missing its continuation offset".into(),
                        ));
                    };
                    format!(
                        "result page capped at {} rows because {bound}; more matches exist. \
                         Continue the same GQL query with `SKIP {next}` (the machine-readable \
                         `next_offset`) and keep the same page size.",
                        check.max_limit
                    )
                }
                khive_query::QueryLanguage::Sparql => format!(
                    "result page capped at {} rows because {bound}; more matches exist. \
                     SPARQL OFFSET paging is not part of the supported dialect.",
                    check.max_limit
                ),
            };
            warnings.push(warning);
        }

        Ok(QueryResult {
            rows,
            warnings,
            offset,
            page_size,
            has_more: truncated,
            next_offset,
            truncated,
        })
    }

    // ---- Edge CRUD operations ----

    /// Fetch a single edge by id.
    ///
    /// UUID v4 is globally unique: returns the edge regardless of which
    /// namespace the token carries. `Ok(None)` means the edge does not exist at all.
    pub async fn get_edge(
        &self,
        _token: &NamespaceToken,
        edge_id: Uuid,
    ) -> RuntimeResult<Option<Edge>> {
        self.get_edge_by_id_inner(edge_id, false).await
    }

    /// Read live edges by ID in input order, without a visibility predicate.
    ///
    /// Stored namespaces are validated before the corresponding edge decode.
    /// Each namespace group uses its own graph capability; missing rows remain
    /// `None`. Group failures belong to their first input, and the earliest
    /// input error wins after all groups in that bounded window are observed.
    /// Metadata statement failures are fatal batch errors. Windows are separate
    /// read observations, not a snapshot of the whole request.
    pub async fn get_edges_by_id(
        &self,
        _token: &NamespaceToken,
        ids: &[Uuid],
    ) -> RuntimeResult<Vec<Option<Edge>>> {
        let mut edges = Vec::with_capacity(ids.len());
        for chunk in ids.chunks(900) {
            let window = self.prepare_edge_read_window(chunk).await?;
            edges.extend(
                Self::hydrate_edge_read_window(chunk, window, |record_token| {
                    self.graph(record_token)
                })
                .await?,
            );
        }
        Ok(edges)
    }

    pub(super) async fn prepare_edge_read_window(
        &self,
        ids: &[Uuid],
    ) -> RuntimeResult<EdgeReadWindow> {
        let placeholders = (1..=ids.len())
            .map(|index| format!("?{index}"))
            .collect::<Vec<_>>()
            .join(",");
        let mut reader = self.sql().reader().await?;
        let rows = reader
            .query_all(SqlStatement {
                sql: format!(
                    "SELECT id, namespace FROM graph_edges WHERE id IN ({placeholders}) AND deleted_at IS NULL"
                ),
                params: ids.iter().map(|id| SqlValue::Text(id.to_string())).collect(),
                label: Some("get_edge_namespace".into()),
            })
            .await?;
        let mut namespaces = HashMap::with_capacity(rows.len());
        for row in rows {
            let Some(SqlValue::Text(id)) = row.columns.first().map(|column| &column.value) else {
                return Err(RuntimeError::Internal(
                    "edge namespace lookup returned an invalid id".into(),
                ));
            };
            let id = Uuid::parse_str(id).map_err(|e| {
                RuntimeError::Internal(format!("edge namespace lookup returned an invalid id: {e}"))
            })?;
            let value = row
                .columns
                .get(1)
                .map(|column| column.value.clone())
                .unwrap_or(SqlValue::Null);
            namespaces.insert(id, value);
        }
        let mut window = EdgeReadWindow {
            outcomes: (0..ids.len()).map(|_| Some(Ok(None))).collect(),
            groups: Vec::new(),
        };
        let mut group_indices = HashMap::new();
        for (index, id) in ids.iter().enumerate() {
            let Some(SqlValue::Text(record_ns)) = namespaces.get(id) else {
                continue;
            };
            match khive_types::Namespace::parse(record_ns) {
                Ok(namespace) => {
                    let next_group = window.groups.len();
                    let group = *group_indices.entry(record_ns.clone()).or_insert(next_group);
                    if group == next_group {
                        window.groups.push((namespace, Vec::new()));
                    }
                    window.groups[group].1.push(index);
                    window.outcomes[index] = None;
                }
                Err(error) => {
                    window.outcomes[index] = Some(Err(RuntimeError::Internal(format!(
                        "edge namespace invalid: {error}"
                    ))));
                }
            }
        }
        Ok(window)
    }

    pub(super) async fn hydrate_edge_read_window<F>(
        ids: &[Uuid],
        mut window: EdgeReadWindow,
        mut graph: F,
    ) -> RuntimeResult<Vec<Option<Edge>>>
    where
        F: FnMut(&NamespaceToken) -> RuntimeResult<std::sync::Arc<dyn khive_storage::GraphStore>>,
    {
        for (namespace, indices) in window.groups {
            let record_token = NamespaceToken::for_namespace(namespace);
            let group_ids: Vec<LinkId> = indices
                .iter()
                .map(|&index| LinkId::from(ids[index]))
                .collect();
            let outcomes = match graph(&record_token) {
                Ok(store) => store
                    .get_edge_read_outcomes(&group_ids)
                    .await
                    .map_err(RuntimeError::from),
                Err(error) => Err(error),
            };
            match outcomes {
                Ok(outcomes) if outcomes.len() == indices.len() => {
                    for (index, outcome) in indices.into_iter().zip(outcomes) {
                        window.outcomes[index] = Some(outcome.map_err(RuntimeError::from));
                    }
                }
                Ok(_) => {
                    window.outcomes[indices[0]] = Some(Err(RuntimeError::Internal(
                        "edge batch returned an invalid outcome count".into(),
                    )));
                }
                Err(error) => {
                    window.outcomes[indices[0]] = Some(Err(error));
                }
            }
        }
        window
            .outcomes
            .into_iter()
            .map(|outcome| {
                outcome.unwrap_or_else(|| {
                    Err(RuntimeError::Internal(
                        "edge batch omitted an input outcome".into(),
                    ))
                })
            })
            .collect()
    }

    /// Fetch a single edge by id.
    ///
    /// Delegates to `get_edge`: no visible-set check.  By-ID ops are
    /// namespace-agnostic; UUID v4 is globally unique.
    pub async fn get_edge_visible(
        &self,
        token: &NamespaceToken,
        edge_id: Uuid,
    ) -> RuntimeResult<Option<Edge>> {
        self.get_edge(token, edge_id).await
    }

    /// Fetch an edge by UUID including soft-deleted rows.
    ///
    /// Returns the edge regardless of which namespace the token carries:
    /// UUID v4 is globally unique. Used by the hard-delete path so that a
    /// soft-deleted edge can still be purged via its edge ID.
    pub async fn get_edge_including_deleted(
        &self,
        _token: &NamespaceToken,
        edge_id: Uuid,
    ) -> RuntimeResult<Option<Edge>> {
        self.get_edge_by_id_inner(edge_id, true).await
    }

    async fn get_edge_by_id_inner(
        &self,
        edge_id: Uuid,
        include_deleted: bool,
    ) -> RuntimeResult<Option<Edge>> {
        let (sql, label) = if include_deleted {
            (
                "SELECT namespace FROM graph_edges WHERE id = ?1 LIMIT 1",
                "get_edge_including_deleted_namespace",
            )
        } else {
            (
                "SELECT namespace FROM graph_edges \
                 WHERE id = ?1 AND deleted_at IS NULL LIMIT 1",
                "get_edge_namespace",
            )
        };
        let mut reader = self.sql().reader().await?;
        let record_ns = reader
            .query_scalar(SqlStatement {
                sql: sql.into(),
                params: vec![SqlValue::Text(edge_id.to_string())],
                label: Some(label.into()),
            })
            .await?;

        let Some(SqlValue::Text(record_ns)) = record_ns else {
            return Ok(None);
        };
        // Route through the record's own namespace store (no namespace equality check).
        let record_tok = NamespaceToken::for_namespace(
            khive_types::Namespace::parse(&record_ns)
                .map_err(|e| RuntimeError::Internal(format!("edge namespace invalid: {e}")))?,
        );
        let graph = self.graph(&record_tok)?;
        Ok(if include_deleted {
            graph
                .get_edge_including_deleted(LinkId::from(edge_id))
                .await?
        } else {
            graph.get_edge(LinkId::from(edge_id)).await?
        })
    }

    /// Fetch an edge by natural key (namespace, canonical source/target, relation)
    /// including soft-deleted rows. Unlike [`Self::list_edges`]/[`Self::list_edges_after`],
    /// which always filter `deleted_at IS NULL`, this can render a tombstoned symmetric-edge
    /// survivor (ADR-039 DO NOTHING conflict absorption) — used by the atomic-apply
    /// post-commit result renderer, which otherwise reports "not found" for a committed
    /// update whose surviving row happens to be soft-deleted.
    ///
    /// `token` selects the store instance; `namespace` is the natural key's own
    /// namespace and is bound into the query explicitly. These can legitimately differ:
    /// the record namespace is fixed at prepare time (`EdgeNaturalKey::namespace`) and by-ID
    /// edge updates are namespace-agnostic (ADR-007 Rev 6), so the caller's ambient `token`
    /// namespace is never a safe substitute for the record's own — the prior implicit
    /// `self.namespace` scoping is exactly the bug this parameter closes (khive#1213/#1214).
    pub async fn get_edge_by_natural_key_including_deleted(
        &self,
        token: &NamespaceToken,
        namespace: &str,
        source_id: Uuid,
        target_id: Uuid,
        relation: EdgeRelation,
    ) -> RuntimeResult<Option<Edge>> {
        Ok(self
            .graph(token)?
            .get_edge_by_natural_key_including_deleted(namespace, source_id, target_id, relation)
            .await?)
    }

    /// Maximum rows returned by a single [`Self::list_edges`] /
    /// [`Self::list_edges_after`] page. A lower bound the docs promise callers
    /// can rely on; kept as a named constant so tests can exercise pagination
    /// (page tiling, out-of-range offsets) without needing >1000 real rows.
    pub const EDGE_LIST_MAX_LIMIT: u32 = 1000;

    /// List edges matching `filter`, paging by `offset`. `limit` is capped at
    /// [`Self::EDGE_LIST_MAX_LIMIT`]; defaults to 100. For O(1)-at-depth walks
    /// over large edge populations, prefer [`Self::list_edges_after`] instead
    /// of paging offset deep.
    pub async fn list_edges(
        &self,
        token: &NamespaceToken,
        filter: crate::curation::EdgeListFilter,
        limit: u32,
        offset: u32,
    ) -> RuntimeResult<Vec<Edge>> {
        let limit = limit.min(Self::EDGE_LIST_MAX_LIMIT);
        let visible = token.visible_namespaces();

        // Common case: a single visible namespace — page directly against the
        // store so `offset`/`limit` reach SQL unmodified.
        if let [ns] = visible {
            let temp = NamespaceToken::for_namespace(ns.clone());
            let page = self
                .graph(&temp)?
                .query_edges(
                    filter.into(),
                    vec![SortOrder {
                        field: EdgeSortField::CreatedAt,
                        direction: khive_storage::types::SortDirection::Asc,
                    }],
                    PageRequest {
                        offset: offset.into(),
                        limit,
                    },
                )
                .await?;
            return Ok(page.items);
        }

        // Multi-namespace visibility: one deterministic query with
        // `namespace IN (...)` and real SQL paging, mirroring
        // `list_entities`. Fetching per-namespace prefixes and slicing a
        // client-side merge re-sorted by UUID floats the offset window
        // between calls — pages silently duplicate and skip rows, so
        // enumeration never covers the set (#2088).
        let ns_strs: Vec<String> = visible.iter().map(|ns| ns.as_str().to_owned()).collect();
        let sort = vec![SortOrder {
            field: EdgeSortField::CreatedAt,
            direction: khive_storage::types::SortDirection::Asc,
        }];
        let graph = self.graph(token)?;
        match graph
            .query_edges_in_namespaces(
                &ns_strs,
                filter.clone().into(),
                sort.clone(),
                PageRequest {
                    offset: offset.into(),
                    limit,
                },
            )
            .await
        {
            Ok(page) => Ok(page.items),
            Err(khive_storage::StorageError::Unsupported { operation, .. })
                if operation == "query_edges_in_namespaces" =>
            {
                // Backend exercises the trait default (no batched
                // namespace query support): fall back to one `query_edges`
                // call per namespace. Unlike the pre-image fix for #2088,
                // this fetches an `offset + limit` prefix from every
                // namespace and merges by the *same* `(created_at, id)` key
                // each per-namespace fetch already orders by — the
                // pre-image bug sorted the merged set by UUID alone, a key
                // unrelated to the order each per-namespace prefix was cut
                // at, which floated the offset window and silently
                // duplicated/skipped rows across pages. Sorting by the
                // fetch's own order key keeps the top `offset + limit` of
                // the merge exactly equal to the true global prefix.
                let fetch_limit = offset.saturating_add(limit);
                let mut namespace_prefixes = Vec::new();
                for ns in visible {
                    let temp = NamespaceToken::for_namespace(ns.clone());
                    let page = self
                        .graph(&temp)?
                        .query_edges(
                            filter.clone().into(),
                            sort.clone(),
                            PageRequest {
                                offset: 0,
                                limit: fetch_limit,
                            },
                        )
                        .await?;
                    namespace_prefixes.push(page.items);
                }
                Ok(Self::merge_paged_namespace_edges(
                    namespace_prefixes,
                    offset,
                    limit,
                ))
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Merge per-namespace `(created_at, id)`-ordered edge prefixes (each
    /// already fetched up to `offset + limit` from its own namespace, as
    /// [`Self::list_edges`]'s trait-default fallback does) into one global
    /// `[offset, offset + limit)` page.
    ///
    /// Sorting by the *same key each prefix was already cut at* is what
    /// keeps this exact: the top `offset + limit` of the merged set is then
    /// provably equal to the true global prefix (a standard k-way merge
    /// argument — no element beyond position `offset + limit` in any single
    /// namespace can appear before that position in the global order). The
    /// pre-image #2088 bug instead re-sorted the merged set by UUID alone —
    /// a key unrelated to the order each namespace's prefix was fetched in —
    /// which floated the offset window and silently duplicated/skipped rows
    /// across pages.
    pub(super) fn merge_paged_namespace_edges(
        namespace_prefixes: Vec<Vec<Edge>>,
        offset: u32,
        limit: u32,
    ) -> Vec<Edge> {
        let mut results: Vec<Edge> = namespace_prefixes.into_iter().flatten().collect();
        results.sort_by_key(|e| (e.created_at, Uuid::from(e.id)));
        let start = (offset as usize).min(results.len());
        let end = (start + limit as usize).min(results.len());
        results[start..end].to_vec()
    }

    /// Keyset (seek) page of edges matching `filter`, ordered by immutable
    /// database-assigned insertion sequence. `after` is the last edge id from the
    /// previous page (exclusive); omit to start from the beginning. Returns
    /// `(items, next_after)` — `next_after` is `Some` when more rows remain
    /// past this page.
    ///
    /// Unlike [`Self::list_edges`], this is O(log n + limit) at any depth and
    /// genuinely new inserts are appended after already-issued boundaries. The
    /// cursor row is resolved including tombstones; a hard-deleted or
    /// out-of-scope cursor fails explicitly rather than hiding an incomplete
    /// traversal.
    pub async fn list_edges_after(
        &self,
        token: &NamespaceToken,
        filter: crate::curation::EdgeListFilter,
        after: Option<Uuid>,
        limit: u32,
    ) -> RuntimeResult<(Vec<Edge>, Option<Uuid>)> {
        let limit = limit.clamp(1, Self::EDGE_LIST_MAX_LIMIT);
        let visible = token.visible_namespaces();
        let limit_usize = limit as usize;
        let cursor_store = self.graph(token)?;
        let after = match after {
            Some(id) => {
                let edge = self
                    .get_edge_including_deleted(token, id)
                    .await?
                    .ok_or_else(|| RuntimeError::NotFound(format!("edge cursor {id}")))?;
                Self::ensure_namespace_visible(&edge.namespace, token)?;
                let sequence = cursor_store.edge_sequence(id).await?.ok_or_else(|| {
                    RuntimeError::Internal(format!(
                        "edge cursor {id} has no insertion-sequence ledger row"
                    ))
                })?;
                Some(SeekCursor { sequence, id })
            }
            None => None,
        };

        if let [ns] = visible {
            let temp = NamespaceToken::for_namespace(ns.clone());
            let page = self
                .graph(&temp)?
                .query_edges_sequence_after(filter.into(), after, limit)
                .await?;
            return Ok((page.items, page.next_after.map(|cursor| cursor.id)));
        }

        // Multi-namespace visibility: seek each namespace from the same
        // immutable boundary, merge in global insertion order, then take the head of
        // the merged set as this page.
        let probe_limit = limit.saturating_add(1);
        let mut results = Vec::new();
        for ns in visible {
            let temp = NamespaceToken::for_namespace(ns.clone());
            let page = self
                .graph(&temp)?
                .query_edges_sequence_after(filter.clone().into(), after, probe_limit)
                .await?;
            results.extend(page.items);
        }
        let ids = results
            .iter()
            .map(|edge| Uuid::from(edge.id))
            .collect::<Vec<_>>();
        let sequences = cursor_store
            .edge_sequences(&ids)
            .await?
            .into_iter()
            .collect::<HashMap<_, _>>();
        if let Some(missing) = ids.iter().find(|id| !sequences.contains_key(id)) {
            return Err(RuntimeError::Internal(format!(
                "edge {missing} has no insertion-sequence ledger row"
            )));
        }
        results.sort_by_key(|edge| {
            let id = Uuid::from(edge.id);
            (sequences[&id], id)
        });
        results.dedup_by_key(|e| Uuid::from(e.id));
        let has_more = results.len() > limit_usize;
        if has_more {
            results.truncate(limit_usize);
        }
        let next_after = if has_more {
            results.last().map(|e| Uuid::from(e.id))
        } else {
            None
        };
        Ok((results, next_after))
    }

    /// Count edges by relation, ignoring soft-deleted rows. Used by
    /// `stats()` to report the true per-relation population so full-graph
    /// audits know what they're sampling from before they walk it.
    pub async fn count_edges_by_relation(
        &self,
        token: &NamespaceToken,
    ) -> RuntimeResult<std::collections::HashMap<String, u64>> {
        let namespaces: Vec<String> = token
            .visible_namespaces()
            .iter()
            .map(|namespace| namespace.as_str().to_owned())
            .collect();
        let graph = self.graph(token)?;
        let counts = match graph
            .count_edges_by_relation_in_namespaces(&namespaces)
            .await
        {
            Ok(counts) => counts,
            Err(khive_storage::StorageError::Unsupported { operation, .. })
                if operation == "count_edges_by_relation_in_namespaces" =>
            {
                let mut totals = HashMap::new();
                for namespace in token.visible_namespaces() {
                    let scoped = NamespaceToken::for_namespace(namespace.clone());
                    for (relation, count) in self.graph(&scoped)?.count_edges_by_relation().await? {
                        *totals.entry(relation).or_insert(0) += count;
                    }
                }
                return Ok(totals
                    .into_iter()
                    .map(|(relation, count)| (relation.to_string(), count))
                    .collect());
            }
            Err(error) => return Err(error.into()),
        };
        Ok(counts
            .into_iter()
            .map(|(relation, count)| (relation.to_string(), count))
            .collect())
    }

    /// Count edges by the base each endpoint resolves against. Used by
    /// `stats()` so a caller can name the denominator of a density figure
    /// instead of inheriting the flat edge total, which on a real store is
    /// mostly provenance.
    ///
    /// The per-namespace fallback sums the same buckets, so the aggregate and
    /// the fallback are checkable against each other and against
    /// `count_edges` by the invariant that the buckets sum to the total.
    pub async fn count_edges_by_endpoint_base(
        &self,
        token: &NamespaceToken,
    ) -> RuntimeResult<khive_storage::types::EdgeEndpointBaseCounts> {
        use khive_storage::types::EdgeEndpointBaseCounts;

        let namespaces: Vec<String> = token
            .visible_namespaces()
            .iter()
            .map(|namespace| namespace.as_str().to_owned())
            .collect();
        let graph = self.graph(token)?;
        match graph
            .count_edges_by_endpoint_base_in_namespaces(&namespaces)
            .await
        {
            Ok(counts) => Ok(counts),
            Err(khive_storage::StorageError::Unsupported { operation, .. })
                if operation == "count_edges_by_endpoint_base_in_namespaces"
                    || operation == "count_edges_by_endpoint_base" =>
            {
                let mut totals = EdgeEndpointBaseCounts::default();
                for namespace in token.visible_namespaces() {
                    let scoped = NamespaceToken::for_namespace(namespace.clone());
                    let counts = self.graph(&scoped)?.count_edges_by_endpoint_base().await?;
                    totals.entity_entity =
                        totals.entity_entity.saturating_add(counts.entity_entity);
                    totals.entity_note = totals.entity_note.saturating_add(counts.entity_note);
                    totals.note_entity = totals.note_entity.saturating_add(counts.note_entity);
                    totals.note_note = totals.note_note.saturating_add(counts.note_note);
                    totals.unresolved = totals.unresolved.saturating_add(counts.unresolved);
                }
                Ok(totals)
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Return the live edge total, its relation counts and its endpoint-base
    /// counts for `stats`.
    ///
    /// The three describe one population: live edges with live endpoints in
    /// the caller's visible namespaces. A backend with the combined aggregate
    /// answers all of them from one grouped read, and the total is the sum of
    /// its groups, so the invariants `stats` reports on (relation counts and
    /// endpoint-base buckets each summing to the total) hold by construction.
    /// A backend without it gets the three single-purpose counts.
    pub async fn edge_stats_counts(
        &self,
        token: &NamespaceToken,
    ) -> RuntimeResult<super::EdgeStatsCounts> {
        let namespaces: Vec<String> = token
            .visible_namespaces()
            .iter()
            .map(|namespace| namespace.as_str().to_owned())
            .collect();
        let graph = self.graph(token)?;
        match graph
            .count_edges_by_relation_and_endpoint_base_in_namespaces(&namespaces)
            .await
        {
            Ok(groups) => {
                let mut counts = super::EdgeStatsCounts::default();
                for (relation, by_base) in groups {
                    let relation_total = by_base.total();
                    counts.edges = counts.edges.saturating_add(relation_total);
                    let slot = counts
                        .edges_by_relation
                        .entry(relation.to_string())
                        .or_insert(0);
                    *slot = slot.saturating_add(relation_total);
                    let totals = &mut counts.edges_by_endpoint_base;
                    totals.entity_entity =
                        totals.entity_entity.saturating_add(by_base.entity_entity);
                    totals.entity_note = totals.entity_note.saturating_add(by_base.entity_note);
                    totals.note_entity = totals.note_entity.saturating_add(by_base.note_entity);
                    totals.note_note = totals.note_note.saturating_add(by_base.note_note);
                    totals.unresolved = totals.unresolved.saturating_add(by_base.unresolved);
                }
                Ok(counts)
            }
            Err(khive_storage::StorageError::Unsupported { operation, .. })
                if operation == "count_edges_by_relation_and_endpoint_base_in_namespaces" =>
            {
                Ok(super::EdgeStatsCounts {
                    edges: self
                        .count_edges(token, crate::curation::EdgeListFilter::default())
                        .await?,
                    edges_by_relation: self.count_edges_by_relation(token).await?,
                    edges_by_endpoint_base: self.count_edges_by_endpoint_base(token).await?,
                })
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Patch-style edge update. Only `Some(_)` fields are applied.
    ///
    /// When `relation` is `Some(new_rel)`, validates that the edge's existing endpoints
    /// are legal for `new_rel` before persisting. Weight-only updates (`relation = None`)
    /// skip validation. Returns `InvalidInput` if the new relation would violate the
    /// three-case endpoint contract; the edge is NOT mutated on error.
    ///
    /// For symmetric relations (`competes_with`, `composed_with`), endpoint order is
    /// canonicalised to `source_uuid < target_uuid` after validation. If a canonical
    /// row already exists at the target triple, the non-canonical edge is deleted and
    /// the existing canonical row is preserved unchanged (ADR-039 ON CONFLICT DO
    /// NOTHING, mirroring `merge_entity_sql`) — its attributes, including a soft-deleted
    /// `deleted_at`, are never overwritten by the discarded edge's patch.
    pub async fn update_edge(
        &self,
        token: &NamespaceToken,
        edge_id: Uuid,
        patch: crate::curation::EdgePatch,
    ) -> RuntimeResult<Edge> {
        // Fetch the edge by UUID: ID-only, no namespace check.
        // get_edge already uses the record's stored namespace internally.
        let graph_for_fetch = self.graph(token)?;
        let mut edge = graph_for_fetch
            .get_edge(LinkId::from(edge_id))
            .await?
            .ok_or_else(|| crate::RuntimeError::NotFound(format!("edge {edge_id}")))?;
        let expected_updated_at = edge.updated_at;
        let expected_deleted_at = edge.deleted_at;
        #[cfg(test)]
        crate::curation::race_seam::pause_after_read().await;

        // After fetching, all mutations and validation must use the
        // RECORD's namespace, not the caller's.  Derive record_tok from the stored edge
        // namespace so that endpoint validation, raw-SQL predicates, and graph routing
        // all address the correct backend partition.
        let record_ns: String = edge.namespace.clone();
        let record_tok = token.with_namespace(
            khive_types::Namespace::parse(&record_ns)
                .map_err(|e| RuntimeError::Internal(format!("edge namespace invalid: {e}")))?,
        );
        let graph = self.graph(&record_tok)?;

        let mut changed_fields: Vec<&'static str> = Vec::new();
        if let Some(r) = patch.relation {
            // Validate before mutating — use the existing endpoints with the new relation.
            // Use record_tok so that endpoint existence checks look in the edge's own namespace.
            self.validate_edge_relation_endpoints(&record_tok, edge.source_id, edge.target_id, r)
                .await?;
            edge.relation = r;
            changed_fields.push("relation");
        }
        if let Some(w) = patch.weight {
            // Reject non-finite or out-of-range weight explicitly; do not silently
            // clamp invalid caller input (coding-standards §608-622).
            if !khive_types::validate_edge_weight(w) {
                return Err(RuntimeError::InvalidInput(format!(
                    "edge weight must be a finite value in [0.0, 1.0]; got {w}"
                )));
            }
            edge.weight = w;
            changed_fields.push("weight");
        }
        if let Some(props) = patch.properties {
            crate::secret_gate::reject_reserved_secret_gate_property(Some(&props))?;
            edge.metadata = Some(props);
        }

        // For symmetric relations, canonicalise endpoint order and check
        // for natural-key conflicts regardless of whether endpoints were flipped.
        //
        // The guarded graph operation is used for ALL symmetric relations because `upsert_edge`
        // resolves ON CONFLICT(namespace,id) first and cannot detect a duplicate at
        // the natural key (namespace, source_id, target_id, relation) with a different
        // id. Bug-fix: this path must also run when endpoints are already canonical
        // (endpoints_flipped=false) to catch conflicts arising from a relation change
        // that collides with an existing canonical row.
        let (canon_src, canon_tgt) =
            canonical_edge_endpoints(edge.relation, edge.source_id, edge.target_id);

        if edge.relation.is_symmetric() {
            let outcome = graph
                .update_symmetric_edge_if_unchanged(SymmetricEdgeUpdateRequest {
                    namespace: record_ns.clone(),
                    id: edge_id.into(),
                    source_id: canon_src,
                    target_id: canon_tgt,
                    relation: edge.relation,
                    weight: edge.weight,
                    metadata: edge.metadata.clone(),
                    expected_updated_at_micros: expected_updated_at.timestamp_micros(),
                    expected_deleted_at_micros: expected_deleted_at.map(|v| v.timestamp_micros()),
                })
                .await?;

            match outcome {
                SymmetricEdgeUpdateOutcome::Absorbed(sid) => {
                    // A conflict was absorbed (ADR-039 DO NOTHING): re-fetch the surviving
                    // canonical row so the caller receives its real, UNMODIFIED attributes —
                    // including soft-deleted rows, since the survivor's tombstone state (if
                    // any) must not be resurrected by the absorbed update either. Use
                    // record_tok — the surviving row lives in the same namespace as the
                    // original.
                    let surviving_uuid = Uuid::parse_str(&sid).map_err(|e| {
                        RuntimeError::Internal(format!(
                            "update_edge: surviving id parse failed: {e}"
                        ))
                    })?;
                    edge = self
                        .get_edge_including_deleted(&record_tok, surviving_uuid)
                        .await?
                        .ok_or_else(|| {
                            RuntimeError::Internal(format!(
                                "update_edge: surviving canonical row {surviving_uuid} vanished after update"
                            ))
                        })?;
                }
                SymmetricEdgeUpdateOutcome::Updated => {
                    // Reflect canonical endpoints in the returned edge (no conflict absorbed).
                    edge.source_id = canon_src;
                    edge.target_id = canon_tgt;
                }
                SymmetricEdgeUpdateOutcome::Stale => {
                    return Err(crate::curation::stale_edge_snapshot_error(edge_id));
                }
            }
        } else {
            // Non-symmetric: replace_edge_if_unchanged takes namespace from edge.namespace
            // (not from the graph store's routing namespace), so this is already
            // record-namespace correct. `graph` is already self.graph(&record_tok)?.
            // Guarded on the fetched snapshot's revision — a concurrent writer that moved
            // this edge between the fetch above and this write must be refused, not
            // silently overwritten by a full-row replacement derived from stale state.
            // `updated_at` must advance past the snapshot: the guard requires the
            // replacement revision to be strictly greater than the persisted one.
            // Make it strictly advance even when two operations land inside one
            // clock microsecond; saturating to i64::MAX would let the CAS accept
            // a write without advancing its revision, so that is not a valid
            // fallback (mirrors the note path).
            let minimum_updated_at_micros = expected_updated_at
                .timestamp_micros()
                .checked_add(1)
                .ok_or_else(|| {
                RuntimeError::Internal(format!(
                    "edge {edge_id} updated_at is already at i64::MAX and cannot advance"
                ))
            })?;
            let now_micros = chrono::Utc::now()
                .timestamp_micros()
                .max(minimum_updated_at_micros);
            edge.updated_at =
                chrono::DateTime::from_timestamp_micros(now_micros).ok_or_else(|| {
                    RuntimeError::Internal(format!(
                        "edge {edge_id}: computed updated_at {now_micros} is not a valid timestamp"
                    ))
                })?;
            let persisted = graph
                .replace_edge_if_unchanged(edge.clone(), expected_updated_at, expected_deleted_at)
                .await?;
            if !persisted {
                return Err(crate::curation::stale_edge_snapshot_error(edge_id));
            }
        }

        // Audit event: use the record's namespace (record_ns) for the event payload.
        let event_store = self.events(&record_tok)?;
        let event = khive_storage::event::Event::new(
            record_ns.clone(),
            "update",
            EventKind::EdgeUpdated,
            SubstrateKind::Entity,
            "",
        )
        .with_target(edge_id)
        .with_payload(
            serde_json::json!({"id": edge_id, "namespace": record_ns, "changed_fields": changed_fields}),
        );
        event_store.append_event(event).await.map_err(|e| {
            RuntimeError::Internal(format!("update_edge: event store write failed: {e}"))
        })?;

        Ok(edge)
    }

    /// Hard-delete an edge by id.
    ///
    /// Cascades to remove any `annotates` edges whose target is the deleted edge
    /// (`annotates` is note → anything; deleting an edge target leaves annotation
    /// edges dangling if not cleaned up). Returns `true` if the primary
    /// edge was removed.
    ///
    /// If `edge_id` does not refer to an edge (e.g. the caller passes an entity or
    /// note UUID by mistake), this method returns `Ok(false)` immediately with no
    /// side effects — it does **not** cascade inbound edges of the non-edge record.
    pub async fn delete_edge(
        &self,
        token: &NamespaceToken,
        edge_id: Uuid,
        hard: bool,
    ) -> RuntimeResult<bool> {
        let mode = if hard {
            DeleteMode::Hard
        } else {
            DeleteMode::Soft
        };

        // Fetch the edge first to obtain the record's own namespace.
        // By-ID ops cross namespace boundaries; all graph routing and audit
        // events must use the record namespace, not the caller's (mirrors update_edge).
        // For hard delete we also check soft-deleted rows so a soft-deleted edge
        // can still be purged via its edge ID.
        let edge = if hard {
            self.get_edge_including_deleted(token, edge_id).await?
        } else {
            self.get_edge(token, edge_id).await?
        };
        let Some(edge) = edge else {
            return Ok(false);
        };

        // Derive record_ns / record_tok from the fetched edge (mirrors update_edge).
        let record_ns: String = edge.namespace.clone();
        let record_tok = token.with_namespace(
            khive_types::Namespace::parse(&record_ns)
                .map_err(|e| RuntimeError::Internal(format!("edge namespace invalid: {e}")))?,
        );
        let graph = self.graph(&record_tok)?;
        let actor = format!("{}:{}", token.actor().kind, token.actor().id);

        // Cascade: on hard delete, remove ALL annotates edges targeting this edge — including
        // already-soft-deleted ones: to prevent dangling graph_edges rows. The row
        // delete and the cascade purge run as ONE write transaction: see
        // `atomic_hard_delete_with_edge_purge`.
        // On soft delete the cascade is skipped (data-vs-view principle: soft-deleting the base
        // edge does not cascade to annotation edges; only a hard purge cleans up incident rows).
        let deleted = if hard {
            self.atomic_hard_delete_with_edge_purge(
                edge_hard_delete_statement(edge_id),
                edge_id,
                &record_ns,
                &actor,
                SubstrateKind::Entity,
            )
            .await?
        } else {
            graph.delete_edge(LinkId::from(edge_id), mode).await?
        };
        if deleted {
            // Audit event: use the record's namespace (record_ns), not the caller's namespace.
            let event_store = self.events(&record_tok)?;
            let event = khive_storage::event::Event::new(
                record_ns.clone(),
                "delete",
                EventKind::EdgeDeleted,
                SubstrateKind::Entity,
                "",
            )
            .with_target(edge_id)
            .with_payload(serde_json::json!({"id": edge_id, "namespace": record_ns, "hard": hard}));
            event_store.append_event(event).await.map_err(|e| {
                RuntimeError::Internal(format!("delete_edge: event store write failed: {e}"))
            })?;
        }
        Ok(deleted)
    }

    /// Count edges matching `filter` across the caller's visible namespaces.
    pub async fn count_edges(
        &self,
        token: &NamespaceToken,
        filter: crate::curation::EdgeListFilter,
    ) -> RuntimeResult<u64> {
        let namespaces: Vec<String> = token
            .visible_namespaces()
            .iter()
            .map(|namespace| namespace.as_str().to_owned())
            .collect();
        let graph = self.graph(token)?;
        match graph
            .count_edges_in_namespaces(&namespaces, filter.clone().into())
            .await
        {
            Ok(count) => Ok(count),
            Err(khive_storage::StorageError::Unsupported { operation, .. })
                if operation == "count_edges_in_namespaces" =>
            {
                let mut total = 0;
                for namespace in token.visible_namespaces() {
                    let scoped = NamespaceToken::for_namespace(namespace.clone());
                    total += self
                        .graph(&scoped)?
                        .count_edges(filter.clone().into())
                        .await?;
                }
                Ok(total)
            }
            Err(error) => Err(error.into()),
        }
    }
}
