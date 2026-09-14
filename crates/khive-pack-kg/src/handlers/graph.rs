//! `neighbors`, `traverse`, and `query` verb handlers.

use serde::Serialize;
use serde_json::Value;
use uuid::Uuid;

use khive_runtime::{NamespaceToken, RuntimeError};
use khive_storage::types::{
    LimitReport, NeighborCursor, NeighborHit, NeighborQuery, TraversalExecutionBudget,
    TraversalOptions, TraversalRequest, DEFAULT_TRAVERSAL_LIMIT, MAX_TRAVERSAL_DEPTH,
    MAX_TRAVERSAL_LIMIT, MAX_TRAVERSAL_ROOTS,
};

use super::common::{
    deser, parse_direction, parse_relation, render_query_result, resolve_uuid_async, to_json,
    NeighborProjection, NeighborsParams, QueryParams, TraverseParams, HARD_CAP,
};
use crate::KgPack;

const NEIGHBOR_LIMIT_CAP: u32 = 1000;

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
    ) -> Result<Value, RuntimeError> {
        let p: NeighborsParams = deser(params)?;
        let node_id = resolve_uuid_async(&p.id, &self.runtime, token).await?;
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
        let mut hits = if page_mode {
            self.runtime
                .neighbors_with_query_page(
                    token,
                    node_id,
                    query.clone(),
                    after,
                    neighbor_kinds,
                    enrich,
                )
                .await?
        } else {
            query.limit = None;
            self.runtime
                .neighbors_with_query(token, node_id, query)
                .await?
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
        // and discards the other, so the stored source/target must be
        // recovered with a per-hit edge read. This is one read per hit (N+1);
        // `limit` is optional, so the read count is capped only when the
        // caller supplies one. An edge deleted between the two reads reports
        // `null` endpoints rather than failing the whole response; it must not
        // report nil UUIDs, which would be indistinguishable from a real
        // direction and would defeat the purpose of the fields.
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
        let mut responses = Vec::with_capacity(hits.len());
        for hit in hits {
            let endpoints = self
                .runtime
                .get_edge(token, hit.edge_id)
                .await?
                .map(|e| (e.source_id, e.target_id));
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
mod tests {
    use khive_runtime::pack::PackRuntime;
    use khive_runtime::{KhiveRuntime, Namespace, VerbRegistryBuilder};
    use khive_types::EdgeRelation;
    use serde_json::json;

    use super::*;

    /// Regression for #1670: `neighbors` response must carry the edge's
    /// stored `source_id`/`target_id`, independent of `direction` and
    /// `origin_id` (which always echoes the queried node). Without the
    /// `graph.rs` fix, `NeighborHitResponse` serializes only `origin_id` +
    /// the flattened `NeighborHit` — `source_id`/`target_id` are absent.
    #[tokio::test]
    async fn neighbors_stored_direction() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime must succeed");
        let token = rt.authorize(Namespace::local()).expect("authorize");
        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        let registry = builder.build().expect("registry build");
        let pack = KgPack::new(rt);

        let src = pack
            .dispatch(
                "create",
                json!({"kind": "entity", "name": "Src1670", "entity_kind": "concept"}),
                &registry,
                &token,
            )
            .await
            .expect("create source must succeed");
        let tgt = pack
            .dispatch(
                "create",
                json!({"kind": "entity", "name": "Tgt1670", "entity_kind": "concept"}),
                &registry,
                &token,
            )
            .await
            .expect("create target must succeed");
        let src_id = src["id"].as_str().expect("src id").to_string();
        let tgt_id = tgt["id"].as_str().expect("tgt id").to_string();

        pack.dispatch(
            "link",
            json!({"source_id": src_id, "target_id": tgt_id, "relation": "contains", "weight": 1.0}),
            &registry,
            &token,
        )
        .await
        .expect("link must succeed");

        // Querying from src with direction=out: origin_id == src. The
        // stored edge direction is unaffected by the query direction.
        let out = pack
            .dispatch(
                "neighbors",
                json!({"id": src_id, "direction": "out"}),
                &registry,
                &token,
            )
            .await
            .expect("neighbors out must succeed");
        let out_arr = out.as_array().expect("neighbors out returns array");
        let out_hit = out_arr
            .iter()
            .find(|h| h.get("id").and_then(Value::as_str) == Some(tgt_id.as_str()))
            .expect("must find tgt in outbound neighbors");
        assert_eq!(
            out_hit.get("source_id").and_then(Value::as_str),
            Some(src_id.as_str()),
            "source_id must be the edge's stored source; hit={out_hit}"
        );
        assert_eq!(
            out_hit.get("target_id").and_then(Value::as_str),
            Some(tgt_id.as_str()),
            "target_id must be the edge's stored target; hit={out_hit}"
        );
        assert_eq!(
            out_hit.get("origin_id").and_then(Value::as_str),
            Some(src_id.as_str()),
            "origin_id must echo the queried node"
        );

        // Querying from tgt with direction=in: origin_id == tgt, but the
        // stored edge direction (source == src, target == tgt) must not flip.
        let incoming = pack
            .dispatch(
                "neighbors",
                json!({"id": tgt_id, "direction": "in"}),
                &registry,
                &token,
            )
            .await
            .expect("neighbors in must succeed");
        let in_arr = incoming.as_array().expect("neighbors in returns array");
        let in_hit = in_arr
            .iter()
            .find(|h| h.get("id").and_then(Value::as_str) == Some(src_id.as_str()))
            .expect("must find src in inbound neighbors");
        assert_eq!(
            in_hit.get("source_id").and_then(Value::as_str),
            Some(src_id.as_str()),
            "source_id must be the edge's stored source regardless of query direction; hit={in_hit}"
        );
        assert_eq!(
            in_hit.get("target_id").and_then(Value::as_str),
            Some(tgt_id.as_str()),
            "target_id must be the edge's stored target regardless of query direction; hit={in_hit}"
        );
        assert_eq!(
            in_hit.get("origin_id").and_then(Value::as_str),
            Some(tgt_id.as_str()),
            "origin_id must echo the queried node, not the stored source"
        );

        // Omitted direction defaults to `both`, which the storage layer serves
        // through separate UNION ALL SQL rather than the single-direction
        // query, so it needs its own coverage: the stored endpoints must be
        // identical whichever side of the edge the query originates from.
        for (queried, expected_neighbor) in [(&src_id, &tgt_id), (&tgt_id, &src_id)] {
            let both = pack
                .dispatch("neighbors", json!({"id": queried}), &registry, &token)
                .await
                .expect("neighbors with omitted direction must succeed");
            let both_arr = both.as_array().expect("neighbors both returns array");
            let hit = both_arr
                .iter()
                .find(|h| h.get("id").and_then(Value::as_str) == Some(expected_neighbor.as_str()))
                .expect("must find the opposite endpoint under default direction");
            assert_eq!(
                hit.get("source_id").and_then(Value::as_str),
                Some(src_id.as_str()),
                "default-direction hit must carry the stored source; queried={queried} hit={hit}"
            );
            assert_eq!(
                hit.get("target_id").and_then(Value::as_str),
                Some(tgt_id.as_str()),
                "default-direction hit must carry the stored target; queried={queried} hit={hit}"
            );
            assert_eq!(
                hit.get("origin_id").and_then(Value::as_str),
                Some(queried.as_str()),
                "origin_id must echo the queried node under default direction"
            );
        }
    }

    /// Pins the encoding for an edge that could not be read back.
    ///
    /// The neighbor query and the per-hit edge read are two separate reads, so
    /// an edge deleted between them leaves the endpoints unknown. Reporting a
    /// pair of nil UUIDs there would be a value-shaped absence: syntactically a
    /// direction, semantically false, and indistinguishable from a real edge
    /// between nil nodes. Since the entire purpose of these fields is to convey
    /// the STORED direction, an unknown direction has to stay distinguishable
    /// from a known one.
    ///
    /// This asserts the wire format rather than the race: it fails if the
    /// unknown case is ever encoded as `Uuid::nil()` again.
    #[test]
    fn unresolvable_edge_endpoints_serialize_as_null_never_nil_uuids() {
        let response = NeighborHitResponse {
            origin_id: Uuid::from_u128(1),
            source_id: None,
            target_id: None,
            hit: NeighborHit {
                node_id: Uuid::from_u128(2),
                edge_id: Uuid::from_u128(3),
                relation: EdgeRelation::Contains,
                weight: 1.0,
                name: None,
                kind: None,
                entity_type: None,
            },
        };

        let encoded = serde_json::to_value(&response).expect("serialize");
        assert_eq!(
            encoded.get("source_id"),
            Some(&Value::Null),
            "an unreadable edge must report a null source_id; encoded={encoded}"
        );
        assert_eq!(
            encoded.get("target_id"),
            Some(&Value::Null),
            "an unreadable edge must report a null target_id; encoded={encoded}"
        );

        let nil = Uuid::nil().to_string();
        assert_ne!(
            encoded.get("source_id").and_then(Value::as_str),
            Some(nil.as_str()),
            "the unknown case must not be encoded as the nil UUID: a consumer \
             cannot tell that apart from a stored direction"
        );

        // Control: a KNOWN direction still serializes as the plain id, so the
        // assertions above are about the unknown case and not about the field
        // disappearing from the response altogether.
        let known = NeighborHitResponse {
            source_id: Some(Uuid::from_u128(7)),
            target_id: Some(Uuid::from_u128(8)),
            ..response
        };
        let encoded = serde_json::to_value(&known).expect("serialize");
        assert_eq!(
            encoded.get("source_id").and_then(Value::as_str),
            Some(Uuid::from_u128(7).to_string().as_str()),
            "a known source must still serialize as its id; encoded={encoded}"
        );
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn neighbors_limit_cursor_kind_and_projection_contract() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime must succeed");
        let token = rt.authorize(Namespace::local()).expect("authorize");
        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        let registry = builder.build().expect("registry build");
        let pack = KgPack::new(rt);

        let root = pack
            .dispatch(
                "create",
                json!({"kind": "entity", "name": "Root2674", "entity_kind": "concept"}),
                &registry,
                &token,
            )
            .await
            .expect("create root must succeed");
        let root_id = root["id"].as_str().expect("root id").to_string();

        for index in 0..5 {
            let target = pack
                .dispatch(
                    "create",
                    json!({
                        "kind": "entity",
                        "name": format!("Neighbor2674-{index}"),
                        "entity_kind": "concept"
                    }),
                    &registry,
                    &token,
                )
                .await
                .expect("create target must succeed");
            pack.dispatch(
                "link",
                json!({
                    "source_id": root_id,
                    "target_id": target["id"],
                    "relation": "supports",
                    "weight": 1.0
                }),
                &registry,
                &token,
            )
            .await
            .expect("link target must succeed");
        }

        let note = pack
            .dispatch(
                "create",
                json!({
                    "kind": "note",
                    "note_kind": "observation",
                    "content": "Neighbor observation 2674"
                }),
                &registry,
                &token,
            )
            .await
            .expect("create note must succeed");
        pack.dispatch(
            "link",
            json!({
                "source_id": note["id"],
                "target_id": root_id,
                "relation": "annotates",
                "weight": 1.0
            }),
            &registry,
            &token,
        )
        .await
        .expect("link note must succeed");

        let control = pack
            .dispatch(
                "neighbors",
                json!({"id": root_id, "direction": "both"}),
                &registry,
                &token,
            )
            .await
            .expect("control neighbors must succeed");
        let control_ids: Vec<&str> = control
            .as_array()
            .expect("no-limit neighbors remain an array")
            .iter()
            .map(|hit| hit["id"].as_str().expect("control neighbor id"))
            .collect();
        assert_eq!(control_ids.len(), 6);

        let mut page = pack
            .dispatch(
                "neighbors",
                json!({"id": root_id, "direction": "both", "limit": 2}),
                &registry,
                &token,
            )
            .await
            .expect("first neighbor page must succeed");
        assert_eq!(page["requested_limit"], 2);
        assert_eq!(page["effective_limit"], 2);
        assert_eq!(page["limit_clamped"], false);
        let mut paged_ids = Vec::new();
        loop {
            let page_items = page["neighbors"].as_array().expect("neighbor page array");
            assert!(page_items.len() <= 2);
            paged_ids.extend(
                page_items
                    .iter()
                    .map(|hit| hit["id"].as_str().expect("paged neighbor id").to_string()),
            );
            let Some(after) = page["next_after"].as_str().map(str::to_owned) else {
                break;
            };
            page = pack
                .dispatch(
                    "neighbors",
                    json!({
                        "id": root_id,
                        "direction": "both",
                        "limit": 2,
                        "after": after
                    }),
                    &registry,
                    &token,
                )
                .await
                .expect("continuation neighbor page must succeed");
        }
        assert_eq!(paged_ids, control_ids);

        let over_cap = pack
            .dispatch(
                "neighbors",
                json!({"id": root_id, "direction": "both", "limit": 2001}),
                &registry,
                &token,
            )
            .await
            .expect("over-cap neighbors must succeed");
        assert_eq!(over_cap["neighbors"].as_array().unwrap().len(), 6);
        assert_eq!(over_cap["requested_limit"], 2001);
        assert_eq!(over_cap["effective_limit"], 1000);
        assert_eq!(over_cap["limit_clamped"], true);

        let filtered = pack
            .dispatch(
                "neighbors",
                json!({"id": root_id, "direction": "both", "neighbor_kinds": ["observation"]}),
                &registry,
                &token,
            )
            .await
            .expect("kind-filtered neighbors must succeed");
        assert_eq!(filtered.as_array().unwrap().len(), 1);
        assert_eq!(filtered[0]["kind"], "observation");

        let edge_projection = pack
            .dispatch(
                "neighbors",
                json!({"id": root_id, "direction": "out", "limit": 1, "projection": "edge"}),
                &registry,
                &token,
            )
            .await
            .expect("edge projection must succeed");
        assert!(edge_projection["neighbors"][0].get("source_id").is_some());
        assert!(edge_projection["neighbors"][0].get("target_id").is_some());
        assert!(edge_projection["neighbors"][0].get("id").is_none());

        let summary_projection = pack
            .dispatch(
                "neighbors",
                json!({"id": root_id, "direction": "out", "limit": 1, "projection": "summary"}),
                &registry,
                &token,
            )
            .await
            .expect("summary projection must succeed");
        assert!(summary_projection["neighbors"][0].get("id").is_some());
        assert!(summary_projection["neighbors"][0].get("name").is_some());
        assert!(summary_projection["neighbors"][0]
            .get("source_id")
            .is_none());

        let context_over_cap = pack
            .dispatch(
                "context",
                json!({"entity_ids": [root_id], "hops": 0, "limit": 21}),
                &registry,
                &token,
            )
            .await
            .expect("over-cap context must succeed");
        assert_eq!(context_over_cap["requested_limit"], 21);
        assert_eq!(context_over_cap["effective_limit"], 20);
        assert_eq!(context_over_cap["limit_clamped"], true);

        let context_under_cap = pack
            .dispatch(
                "context",
                json!({"entity_ids": [root_id], "hops": 0, "limit": 3}),
                &registry,
                &token,
            )
            .await
            .expect("under-cap context must succeed");
        assert_eq!(context_under_cap["requested_limit"], 3);
        assert_eq!(context_under_cap["effective_limit"], 3);
        assert_eq!(context_under_cap["limit_clamped"], false);
    }
}
