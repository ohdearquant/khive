//! `search` verb handler.

use std::collections::HashMap;

/// Maximum candidate window used when property/tag filters are active.
/// See `docs/handlers-common.md#filtered_scan_cap-and-the-scan-cliff-widening-handlerssearchrs`.
const FILTERED_SCAN_CAP: u32 = 500;

use serde_json::Value;
use uuid::Uuid;

use khive_runtime::{NamespaceToken, RuntimeError, VerbRegistry};
use khive_storage::types::PageRequest;
use khive_storage::EntityFilter;

use super::common::{
    canonical_entity_kind, canonical_note_kind, deser, props_match, reconcile_specific,
    resolve_kind_spec, tags_match_any, to_json, validate_entity_type, KindSpec, SearchParams,
};
use crate::KgPack;

impl KgPack {
    pub(crate) async fn handle_search(
        &self,
        token: &NamespaceToken,
        params: Value,
        registry: &VerbRegistry,
    ) -> Result<Value, RuntimeError> {
        let p: SearchParams = deser(params)?;
        let limit = p.limit.unwrap_or(10).min(100);
        let spec = resolve_kind_spec(&p.kind, registry)?;
        match spec {
            KindSpec::Entity { specific } => {
                let kind_filter = reconcile_specific(
                    specific,
                    p.entity_kind.as_deref(),
                    |s| canonical_entity_kind(s, registry),
                    "entity_kind",
                )?;
                let validated_et: Option<String> = if let Some(ref raw_et) = p.entity_type {
                    if let Some(ref kf) = kind_filter {
                        validate_entity_type(kf, Some(raw_et), registry)?
                    } else {
                        let norm = raw_et.trim().to_ascii_lowercase();
                        Some(norm)
                    }
                } else {
                    None
                };
                let props_filter = p.properties.as_ref().and_then(|v| {
                    if v.as_object().is_some_and(|m| !m.is_empty()) {
                        Some(v)
                    } else {
                        None
                    }
                });
                let tag_filter = p.tags.as_ref().filter(|tags| !tags.is_empty());
                let search_limit = if props_filter.is_some() || tag_filter.is_some() {
                    // See docs/handlers-common.md#filtered_scan_cap-and-the-scan-cliff-widening-handlerssearchrs.
                    (limit * 50).min(FILTERED_SCAN_CAP)
                } else {
                    limit
                };
                let hits = self
                    .runtime
                    .hybrid_search(
                        token,
                        &p.query,
                        None,
                        search_limit,
                        kind_filter.as_deref(),
                        validated_et.as_deref(),
                        tag_filter.map(|t| t.as_slice()).unwrap_or(&[]),
                        props_filter,
                    )
                    .await?;

                let candidate_ids: Vec<Uuid> = hits.iter().map(|h| h.entity_id).collect();
                let entity_meta: HashMap<Uuid, (String, Option<Value>, Vec<String>)> =
                    if candidate_ids.is_empty() {
                        HashMap::new()
                    } else {
                        let entities_page = self
                            .runtime
                            .entities(token)?
                            .query_entities(
                                token.namespace().as_str(),
                                EntityFilter {
                                    ids: candidate_ids,
                                    namespaces: token
                                        .visible_namespace_strs()
                                        .iter()
                                        .map(|s| s.to_string())
                                        .collect(),
                                    ..EntityFilter::default()
                                },
                                PageRequest {
                                    offset: 0u64,
                                    limit: hits.len() as u32,
                                },
                            )
                            .await
                            .map_err(RuntimeError::Storage)?;
                        entities_page
                            .items
                            .into_iter()
                            .map(|e| (e.id, (e.kind, e.properties, e.tags)))
                            .collect()
                    };

                let filtered_hits = if props_filter.is_some() || tag_filter.is_some() {
                    hits.into_iter()
                        .filter(|h| {
                            let Some((_, props, tags)) = entity_meta.get(&h.entity_id) else {
                                return false;
                            };
                            props_filter
                                .is_none_or(|pf| props_match(props.as_ref(), pf))
                                && tag_filter
                                    .is_none_or(|wanted| tags_match_any(tags, wanted))
                        })
                        .take(limit as usize)
                        .collect::<Vec<_>>()
                } else {
                    hits
                };

                let score_floor = p.min_score.unwrap_or(0.0).max(0.0);
                let result: Vec<Value> = filtered_hits
                    .iter()
                    .filter(|h| h.score.to_f64() >= score_floor)
                    .map(|h| {
                        let entity_kind =
                            entity_meta.get(&h.entity_id).map(|(k, _, _)| k.as_str());
                        serde_json::json!({
                            "id": h.entity_id.to_string(),
                            "entity_kind": entity_kind,
                            "score": h.score.to_f64(),
                            "title": h.title,
                            "snippet": h.snippet,
                        })
                    })
                    .collect();
                to_json(&result)
            }
            KindSpec::Note { specific } => {
                let kind_filter = reconcile_specific(
                    specific,
                    p.note_kind.as_deref().filter(|s| !s.is_empty()),
                    |s| canonical_note_kind(s, registry),
                    "note_kind",
                )?;
                let props_filter = p.properties.as_ref().and_then(|v| {
                    if v.as_object().is_some_and(|m| !m.is_empty()) {
                        Some(v)
                    } else {
                        None
                    }
                });
                let tag_filter = p.tags.as_ref().filter(|tags| !tags.is_empty());
                let search_limit = if props_filter.is_some() || tag_filter.is_some() {
                    // See docs/handlers-common.md#filtered_scan_cap-and-the-scan-cliff-widening-handlerssearchrs.
                    (limit * 50).min(FILTERED_SCAN_CAP)
                } else {
                    limit
                };
                let hits = self
                    .runtime
                    .search_notes(
                        token,
                        &p.query,
                        None,
                        search_limit,
                        kind_filter.as_deref(),
                        p.include_superseded.unwrap_or(false),
                        tag_filter.map(|t| t.as_slice()).unwrap_or(&[]),
                        props_filter,
                    )
                    .await?;

                // Batch-fetch all candidate notes in one IN(...) query instead of
                // N individual gets. Notes absent from the batch result (deleted
                // between the search and the fetch) are simply absent from the map
                // and filtered out by the `note_meta.get` guard below.
                let note_meta: HashMap<Uuid, (String, Option<Value>)> = if hits.is_empty() {
                    HashMap::new()
                } else {
                    let candidate_ids: Vec<Uuid> = hits.iter().map(|h| h.note_id).collect();
                    let note_store = self.runtime.notes(token)?;
                    note_store
                        .get_notes_batch(&candidate_ids)
                        .await
                        .map_err(RuntimeError::Storage)?
                        .into_iter()
                        .map(|n| (n.id, (n.kind, n.properties)))
                        .collect()
                };

                let filtered_hits: Vec<_> = if props_filter.is_some() || tag_filter.is_some() {
                    hits.into_iter()
                        .filter(|h| {
                            let Some((_, props)) = note_meta.get(&h.note_id) else {
                                return false;
                            };
                            let props_ok = props_filter
                                .is_none_or(|pf| props_match(props.as_ref(), pf));
                            let tags_ok = tag_filter.is_none_or(|wanted| {
                                let note_tags: Vec<String> = props
                                    .as_ref()
                                    .and_then(|p| p.get("tags"))
                                    .and_then(Value::as_array)
                                    .map(|arr| {
                                        arr.iter()
                                            .filter_map(Value::as_str)
                                            .map(str::to_owned)
                                            .collect()
                                    })
                                    .unwrap_or_default();
                                tags_match_any(&note_tags, wanted)
                            });
                            props_ok && tags_ok
                        })
                        .take(limit as usize)
                        .collect()
                } else {
                    hits
                };

                let score_floor = p.min_score.unwrap_or(0.0).max(0.0);
                let result: Vec<Value> = filtered_hits
                    .iter()
                    .filter(|h| h.score.to_f64() >= score_floor)
                    .map(|h| {
                        let note_kind =
                            note_meta.get(&h.note_id).map(|(k, _)| k.as_str());
                        serde_json::json!({
                            "id": h.note_id.to_string(),
                            "note_kind": note_kind,
                            "score": h.score.to_f64(),
                            "title": h.title,
                            "snippet": h.snippet,
                        })
                    })
                    .collect();
                to_json(&result)
            }
            KindSpec::Edge => Err(RuntimeError::InvalidInput(
                "search does not support kind=edge — use `list(kind=\"edge\", ...)` for edge browsing".into(),
            )),
            KindSpec::Event => Err(RuntimeError::InvalidInput(
                "search does not support kind=event — use `list(kind=\"event\", ...)` for event browsing".into(),
            )),
            KindSpec::Proposal => Err(RuntimeError::InvalidInput(
                "search does not support kind=proposal — use `list(kind=\"proposal\", ...)` for proposal browsing".into(),
            )),
        }
    }
}
