//! `neighbors`, `traverse`, and `query` verb handlers.

use serde_json::Value;

use khive_runtime::{NamespaceToken, RuntimeError};
use khive_storage::types::{NeighborQuery, TraversalOptions, TraversalRequest};

use super::common::{
    deser, parse_direction, parse_relation, render_query_result, resolve_uuid_async, to_json,
    NeighborsParams, QueryParams, TraverseParams, HARD_CAP,
};
use crate::KgPack;

impl KgPack {
    pub(crate) async fn handle_neighbors(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: NeighborsParams = deser(params)?;
        let node_id = resolve_uuid_async(&p.id, &self.runtime, token).await?;
        let direction = parse_direction(p.direction.as_deref());
        let relations = p
            .relations
            .map(|v| {
                v.iter()
                    .map(|s| parse_relation(s))
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()?;
        let mut hits = self
            .runtime
            .neighbors_with_query(
                token,
                node_id,
                NeighborQuery {
                    direction,
                    relations,
                    limit: p.limit,
                    min_weight: p.min_weight,
                },
            )
            .await?;
        // entity_type is a cheap String field already fetched in the same
        // entity batch, so the clear happens handler-side rather than
        // threading a flag down to the runtime layer.
        if !p.include_entity_type.unwrap_or(false) {
            for hit in &mut hits {
                hit.entity_type = None;
            }
        }
        to_json(&hits)
    }

    pub(crate) async fn handle_traverse(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: TraverseParams = deser(params)?;
        let mut roots = Vec::with_capacity(p.roots.len());
        for s in &p.roots {
            roots.push(resolve_uuid_async(s, &self.runtime, token).await?);
        }
        let direction = parse_direction(p.direction.as_deref());
        let relations = p
            .relations
            .map(|v| {
                v.iter()
                    .map(|s| parse_relation(s))
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()?;
        let options = TraversalOptions {
            max_depth: p.max_depth.unwrap_or(3),
            direction,
            relations,
            min_weight: p.min_weight,
            limit: p.limit,
        };
        let request = TraversalRequest {
            roots,
            options,
            include_roots: p.include_roots.unwrap_or(true),
            include_properties: p.include_properties.unwrap_or(false),
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
        let opts = khive_query::CompileOptions {
            max_limit: p.limit.unwrap_or(500).min(HARD_CAP),
            ..Default::default()
        };
        let result = self
            .runtime
            .query_with_metadata(token, &p.query, opts)
            .await?;
        Ok(render_query_result(result))
    }
}
