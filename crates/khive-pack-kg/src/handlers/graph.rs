//! `neighbors`, `traverse`, and `query` verb handlers.

use std::collections::HashMap;

use serde::Serialize;
use serde_json::Value;
use uuid::Uuid;

use khive_runtime::{KgNeighborRead, NamespaceToken, Resolved, RuntimeError, VerbRegistry};
use khive_storage::types::{
    LimitReport, NeighborCursor, NeighborHit, NeighborQuery, TraversalExecutionBudget,
    TraversalOptions, TraversalRequest, DEFAULT_TRAVERSAL_LIMIT, MAX_TRAVERSAL_DEPTH,
    MAX_TRAVERSAL_LIMIT, MAX_TRAVERSAL_ROOTS,
};

use super::common::{
    deser, parse_direction, parse_relation, render_query_result, resolve_uuid_async, to_json,
    DoublingWindowIter, NeighborProjection, NeighborsParams, QueryParams, TraverseParams, HARD_CAP,
};
use super::message_scope::{
    message_endpoint_permitted, message_neighbor_permitted, resolve_mailbox_graph_id,
};
use crate::KgPack;

const NEIGHBOR_LIMIT_CAP: u32 = 1000;
const MESSAGE_NEIGHBOR_SCAN_CAP: u32 = 10_000;

#[derive(Serialize)]
struct NeighborHitResponse {
    origin_id: Uuid,
    /// Stored endpoints of the edge behind this hit. `None` (serialized as
    /// `null`) means the edge could not be read back, not that the edge runs
    /// between nil nodes: the whole point of these fields is to convey stored
    /// direction, so an unknown direction must be distinguishable from a
    /// known one rather than being reported as a pair of nil UUIDs.
    source_id: Option<Uuid>,
    target_id: Option<Uuid>,
    #[serde(flatten)]
    hit: NeighborHit,
}

impl KgPack {
    pub(crate) async fn handle_neighbors(
        &self,
        token: &NamespaceToken,
        params: Value,
        registry: &VerbRegistry,
    ) -> Result<Value, RuntimeError> {
        let mailbox_view =
            self.runtime
                .authorize_mailbox_view(token, "neighbors", None, &params)?;
        let p: NeighborsParams = deser(params)?;
        let node_id =
            resolve_mailbox_graph_id(&self.runtime, registry, token, &mailbox_view, &p.id).await?;
        let direction = parse_direction(p.direction.as_deref())?;
        let relations = p
            .relations
            .map(|v| {
                v.iter()
                    .map(|s| parse_relation(s))
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()?;
        let after = p
            .after
            .as_deref()
            .map(Self::parse_neighbor_cursor)
            .transpose()?
            .flatten();
        if p.after.is_some() && p.limit.is_none() {
            return Err(RuntimeError::InvalidInput(
                "neighbors requires an explicit limit when `after` is supplied".into(),
            ));
        }
        let neighbor_kinds = p.neighbor_kinds.map(|kinds| {
            let mut kinds: Vec<String> = kinds
                .into_iter()
                .map(|kind| kind.trim().to_ascii_lowercase())
                .filter(|kind| !kind.is_empty())
                .collect();
            kinds.sort_unstable();
            kinds.dedup();
            kinds
        });
        let projection = p.projection.unwrap_or(NeighborProjection::Record);
        let requested_limit = p.limit;
        let effective_limit = requested_limit.map(|limit| limit.min(NEIGHBOR_LIMIT_CAP));
        let page_mode = requested_limit.is_some()
            || p.after.is_some()
            || neighbor_kinds
                .as_ref()
                .is_some_and(|kinds| !kinds.is_empty())
            || !matches!(projection, NeighborProjection::Record);
        let enrich = !matches!(projection, NeighborProjection::Edge);
        let mut query = NeighborQuery {
            direction,
            relations,
            limit: effective_limit.map(|limit| limit.saturating_add(1)),
            min_weight: p.min_weight,
        };
        let origin = match self.runtime.resolve_by_id(token, node_id).await? {
            Some(origin) => Some(origin),
            None => {
                registry
                    .resolve_kg_read_by_id(&self.runtime, token, node_id, false)
                    .await?
            }
        };
        let anchor_permitted = match &origin {
            Some(Resolved::Note(note)) => mailbox_view.permits_message_note(token, note),
            Some(_) => true,
            None => {
                message_endpoint_permitted(&self.runtime, registry, token, &mailbox_view, node_id)
                    .await?
            }
        };
        let mut permitted = HashMap::from([(node_id, anchor_permitted)]);
        if !page_mode {
            query.limit = None;
        }
        let admitted_target = query.limit;
        let (mut hits, scan_incomplete) = if !anchor_permitted {
            (Vec::new(), false)
        } else {
            let namespaces = if admitted_target.is_some() {
                token
                    .visible_namespaces()
                    .iter()
                    .cloned()
                    .map(Some)
                    .collect()
            } else {
                vec![None]
            };
            let mut merged = Vec::new();
            let mut scan_incomplete = false;
            for namespace in namespaces {
                let mut windows = admitted_target
                    .map(|first| DoublingWindowIter::new(first, MESSAGE_NEIGHBOR_SCAN_CAP));
                loop {
                    query.limit = windows.as_mut().and_then(|iter| iter.next());
                    let options = KgNeighborRead {
                        query: query.clone(),
                        after,
                        neighbor_kinds: neighbor_kinds.clone(),
                        enrich,
                        namespace: namespace.clone(),
                    };
                    let (raw_hits, entity_kinds) = if origin.is_some() {
                        self.runtime
                            .neighbors_for_resolved_kg_read_with_entity_kinds(
                                token, node_id, options,
                            )
                            .await?
                    } else {
                        registry
                            .neighbors_for_kg_read_with_entity_kinds(
                                &self.runtime,
                                token,
                                node_id,
                                options,
                            )
                            .await?
                    };
                    let raw_count = raw_hits.len();
                    let mut visible_hits = Vec::with_capacity(raw_count);
                    for hit in raw_hits {
                        let allowed = match permitted.get(&hit.node_id) {
                            Some(allowed) => *allowed,
                            None => {
                                let allowed = if entity_kinds
                                    .get(&hit.node_id)
                                    .is_some_and(|kind| kind != "message")
                                {
                                    true
                                } else {
                                    message_neighbor_permitted(
                                        &self.runtime,
                                        registry,
                                        token,
                                        &mailbox_view,
                                        &hit,
                                    )
                                    .await?
                                };
                                permitted.insert(hit.node_id, allowed);
                                allowed
                            }
                        };
                        if allowed {
                            visible_hits.push(hit);
                        }
                    }
                    let Some(window) = query.limit else {
                        merged.extend(visible_hits);
                        break;
                    };
                    if visible_hits.len() >= admitted_target.unwrap_or(window) as usize
                        || raw_count < window as usize
                    {
                        visible_hits.truncate(admitted_target.unwrap_or(window) as usize);
                        merged.extend(visible_hits);
                        break;
                    }
                    if window >= MESSAGE_NEIGHBOR_SCAN_CAP {
                        merged.extend(visible_hits);
                        scan_incomplete = true;
                        break;
                    }
                    if let Some(windows) = windows.as_mut() {
                        windows.advance(window, 0, true);
                    }
                }
            }
            merged.sort_by_key(|hit| (hit.node_id, hit.edge_id));
            merged.dedup_by_key(|hit| (hit.node_id, hit.edge_id));
            merged.sort_by(|a, b| {
                b.weight
                    .partial_cmp(&a.weight)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(a.node_id.cmp(&b.node_id))
                    .then(a.edge_id.cmp(&b.edge_id))
            });
            (merged, scan_incomplete)
        };
        // entity_type is a cheap String field already fetched in the same
        // entity batch, so the clear happens handler-side rather than
        // threading a flag down to the runtime layer.
        if !p.include_entity_type.unwrap_or(false) {
            for hit in &mut hits {
                hit.entity_type = None;
            }
        }
        // #1670: the SQL neighbor query aliases one edge endpoint to `node_id`
        // and discards the other. Record and Edge projections recover stored
        // source/target with one edge read per returned hit; Summary does not
        // return endpoints and needs no edge read. An edge deleted between the
        // adjacency and endpoint reads yields `null` rather than nil UUIDs.
        let has_more = effective_limit.is_some_and(|limit| hits.len() > limit as usize);
        if let Some(limit) = effective_limit {
            hits.truncate(limit as usize);
        }
        let next_after = if has_more {
            hits.last().map(|hit| NeighborCursor {
                weight: hit.weight,
                node_id: hit.node_id,
                edge_id: hit.edge_id,
            })
        } else {
            None
        };
        let endpoints = match projection {
            NeighborProjection::Summary => vec![None; hits.len()],
            _ => {
                let ids: Vec<_> = hits.iter().map(|hit| hit.edge_id).collect();
                self.runtime
                    .get_edges_by_id(token, &ids)
                    .await?
                    .into_iter()
                    .map(|edge| edge.map(|edge| (edge.source_id, edge.target_id)))
                    .collect()
            }
        };
        let mut responses = Vec::with_capacity(hits.len());
        for (hit, endpoints) in hits.into_iter().zip(endpoints) {
            let response = match projection {
                NeighborProjection::Edge => serde_json::json!({
                    "origin_id": node_id,
                    "source_id": endpoints.map(|(source, _)| source),
                    "target_id": endpoints.map(|(_, target)| target),
                    "edge_id": hit.edge_id,
                    "relation": hit.relation,
                    "weight": hit.weight,
                }),
                NeighborProjection::Summary => serde_json::json!({
                    "origin_id": node_id,
                    "id": hit.node_id,
                    "edge_id": hit.edge_id,
                    "relation": hit.relation,
                    "weight": hit.weight,
                    "name": hit.name,
                    "kind": hit.kind,
                    "entity_type": hit.entity_type,
                }),
                NeighborProjection::Record => to_json(&NeighborHitResponse {
                    origin_id: node_id,
                    source_id: endpoints.map(|(source, _)| source),
                    target_id: endpoints.map(|(_, target)| target),
                    hit,
                })?,
            };
            responses.push(response);
        }
        if let Some(requested) = requested_limit {
            let report = LimitReport::new(requested, effective_limit.unwrap_or(requested)).value();
            let mut response = serde_json::json!({
                "neighbors": responses,
                "next_after": next_after.map(|cursor| serde_json::to_string(&cursor)).transpose()
                    .map_err(|error| RuntimeError::Internal(format!("serialize neighbor cursor: {error}")))?,
            });
            if scan_incomplete {
                response["scan_incomplete"] = serde_json::json!(true);
            }
            if let Some(fields) = report.as_object() {
                response
                    .as_object_mut()
                    .expect("limited neighbors response is an object")
                    .extend(fields.clone());
            }
            return Ok(response);
        }
        to_json(&responses)
    }

    fn parse_neighbor_cursor(raw: &str) -> Result<Option<NeighborCursor>, RuntimeError> {
        if raw.is_empty() {
            return Ok(None);
        }
        let cursor: NeighborCursor = serde_json::from_str(raw).map_err(|error| {
        RuntimeError::InvalidInput(format!(
            "after must be an opaque neighbor cursor containing weight, node_id, and edge_id: {error}"
        ))
    })?;
        if !cursor.weight.is_finite() {
            return Err(RuntimeError::InvalidInput(
                "after cursor weight must be finite".into(),
            ));
        }
        Ok(Some(cursor))
    }

    pub(crate) async fn handle_traverse(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: TraverseParams = deser(params)?;
        if p.roots.len() > MAX_TRAVERSAL_ROOTS {
            return Err(RuntimeError::InvalidInput(format!(
                "traverse roots must contain at most {MAX_TRAVERSAL_ROOTS} entries, got {}",
                p.roots.len()
            )));
        }
        let max_depth = p.max_depth.unwrap_or(3);
        if max_depth > MAX_TRAVERSAL_DEPTH {
            return Err(RuntimeError::InvalidInput(format!(
                "traverse max_depth must be <= {MAX_TRAVERSAL_DEPTH}, got {max_depth}"
            )));
        }
        let limit = p.limit.unwrap_or(DEFAULT_TRAVERSAL_LIMIT);
        if limit > MAX_TRAVERSAL_LIMIT {
            return Err(RuntimeError::InvalidInput(format!(
                "traverse limit must be <= {MAX_TRAVERSAL_LIMIT}, got {limit}"
            )));
        }
        let direction = parse_direction(p.direction.as_deref())?;
        let mut relations = p
            .relations
            .map(|v| {
                v.iter()
                    .map(|s| parse_relation(s))
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()?;
        if let Some(relations) = &mut relations {
            let mut seen = std::collections::HashSet::with_capacity(relations.len());
            relations.retain(|relation| seen.insert(*relation));
        }
        let options = TraversalOptions {
            max_depth,
            direction,
            relations,
            min_weight: p.min_weight,
            limit: Some(limit),
        };
        options.validate().map_err(RuntimeError::InvalidInput)?;

        // The raw-root cap is checked before resolution so a caller cannot
        // turn one request into unbounded sequential lookup work. Resolution
        // aliases may still collapse to one UUID, so de-duplicate afterward
        // while preserving the caller's first-root order.
        let mut roots = Vec::with_capacity(p.roots.len());
        let mut seen = std::collections::HashSet::with_capacity(p.roots.len());
        for root in p.roots {
            let id = resolve_uuid_async(&root, &self.runtime, token).await?;
            if seen.insert(id) {
                roots.push(id);
            }
        }
        let request = TraversalRequest {
            roots,
            options,
            include_roots: p.include_roots.unwrap_or(true),
            include_properties: p.include_properties.unwrap_or(false),
            execution_budget: TraversalExecutionBudget::default(),
        };
        let paths = self.runtime.traverse(token, request).await?;
        to_json(&paths)
    }

    pub(crate) async fn handle_query(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: QueryParams = deser(params)?;
        let requested_page_size = match (p.page_size, p.limit) {
            (Some(_), Some(_)) => {
                return Err(RuntimeError::InvalidInput(
                    "query accepts either `page_size` or deprecated `limit`, not both".into(),
                ));
            }
            (Some(page_size), None) | (None, Some(page_size)) => page_size,
            (None, None) => 500,
        };
        if requested_page_size == 0 {
            return Err(RuntimeError::InvalidInput(
                "query page_size must be at least 1".into(),
            ));
        }
        let opts = khive_query::CompileOptions {
            max_limit: requested_page_size.min(HARD_CAP),
            ..Default::default()
        };
        let result = self
            .runtime
            .query_with_metadata(token, &p.query, opts)
            .await?;
        Ok(render_query_result(result))
    }
}

#[cfg(test)]
#[path = "graph_tests.rs"]
mod tests;
