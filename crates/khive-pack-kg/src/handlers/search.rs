//! `search` verb handler.

use std::collections::HashMap;

/// Maximum candidate window used when property/tag filters are active.
/// See `docs/api/scan-cliff.md`.
const FILTERED_SCAN_CAP: u32 = 500;

use std::time::Instant;

use serde_json::{json, Value};
use uuid::Uuid;

use khive_runtime::{micros_to_iso, KhiveRuntime, NamespaceToken, RuntimeError, VerbRegistry};
use khive_storage::types::PageRequest;
use khive_storage::EntityFilter;

use super::common::{
    canonical_entity_kind, canonical_note_kind, deser, missing_kind_error, props_match,
    reconcile_specific, resolve_kind_spec, tags_match_any, to_json, validate_entity_type, KindSpec,
    SearchParams,
};
use crate::KgPack;

impl KgPack {
    pub(crate) async fn handle_search(
        &self,
        token: &NamespaceToken,
        params: Value,
        registry: &VerbRegistry,
    ) -> Result<Value, RuntimeError> {
        let search_start = Instant::now();
        let p: SearchParams = deser(params)?;
        let limit = p.limit.unwrap_or(10).min(100);
        let kind_raw = p
            .kind
            .as_deref()
            .ok_or_else(|| missing_kind_error("kind", registry))?;
        let spec = resolve_kind_spec(kind_raw, registry)?;
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
                    // See docs/api/scan-cliff.md.
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
                let entity_meta: HashMap<Uuid, (String, Option<Value>, Vec<String>, i64)> =
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
                            .map(|e| (e.id, (e.kind, e.properties, e.tags, e.created_at)))
                            .collect()
                    };

                let filtered_hits = if props_filter.is_some() || tag_filter.is_some() {
                    hits.into_iter()
                        .filter(|h| {
                            let Some((_, props, tags, _)) = entity_meta.get(&h.entity_id) else {
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
                            entity_meta.get(&h.entity_id).map(|(k, _, _, _)| k.as_str());
                        let created_at = entity_meta
                            .get(&h.entity_id)
                            .map(|(_, _, _, c)| micros_to_iso(*c));
                        serde_json::json!({
                            "id": h.entity_id.to_string(),
                            // `kind`/`name` match the list()/get() row shape (#1174);
                            // `entity_kind`/`title` are kept for compatibility.
                            "kind": entity_kind,
                            "entity_kind": entity_kind,
                            "name": h.title,
                            "score": h.score.to_f64(),
                            "title": h.title,
                            "snippet": h.snippet,
                            "created_at": created_at,
                        })
                    })
                    .collect();
                self.track_search_serve(
                    token,
                    &p.query,
                    "entity",
                    &result,
                    search_start.elapsed().as_micros() as i64,
                );
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
                    // See docs/api/scan-cliff.md.
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
                let note_meta: HashMap<Uuid, (String, Option<Value>, Option<String>, i64)> =
                    if hits.is_empty() {
                        HashMap::new()
                    } else {
                        let candidate_ids: Vec<Uuid> = hits.iter().map(|h| h.note_id).collect();
                        let note_store = self.runtime.notes(token)?;
                        note_store
                            .get_notes_batch(&candidate_ids)
                            .await
                            .map_err(RuntimeError::Storage)?
                            .into_iter()
                            .map(|n| (n.id, (n.kind, n.properties, n.name, n.created_at)))
                            .collect()
                    };

                let filtered_hits: Vec<_> = if props_filter.is_some() || tag_filter.is_some() {
                    hits.into_iter()
                        .filter(|h| {
                            let Some((_, props, _, _)) = note_meta.get(&h.note_id) else {
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
                        let meta = note_meta.get(&h.note_id);
                        let note_kind = meta.map(|(k, _, _, _)| k.as_str());
                        let name = meta.and_then(|(_, _, name, _)| name.clone());
                        let created_at = meta.map(|(_, _, _, c)| micros_to_iso(*c));
                        serde_json::json!({
                            "id": h.note_id.to_string(),
                            // `kind`/`name` match the list()/get() row shape (#1174);
                            // `note_kind`/`title` are kept for compatibility.
                            "kind": note_kind,
                            "note_kind": note_kind,
                            "name": name,
                            "score": h.score.to_f64(),
                            "title": h.title,
                            "snippet": h.snippet,
                            "created_at": created_at,
                        })
                    })
                    .collect();
                self.track_search_serve(
                    token,
                    &p.query,
                    "note",
                    &result,
                    search_start.elapsed().as_micros() as i64,
                );
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

    /// Fire-and-forget `search_executed` telemetry (ADR-103 event plane),
    /// mirroring `memory.recall`'s `track_recall_serve` seam (#866): the
    /// event append runs off the response path via `track_background_task`
    /// so a slow or failing event store never affects a served search.
    fn track_search_serve(
        &self,
        token: &NamespaceToken,
        query_raw: &str,
        result_kind: &'static str,
        results: &[Value],
        latency_us: i64,
    ) {
        let selected: Vec<String> = results
            .iter()
            .filter_map(|r| r.get("id").and_then(Value::as_str).map(str::to_string))
            .collect();
        let result_count = selected.len();
        let query = query_raw.to_string();
        let actor = format!("{}:{}", token.actor().kind, token.actor().id);
        let runtime = self.runtime.clone();
        let token = token.clone();

        khive_runtime::track_background_task(async move {
            emit_search_executed_event(
                &runtime,
                &token,
                actor,
                query,
                result_kind,
                selected,
                result_count,
                latency_us,
            )
            .await;
        });
    }
}

/// Append best-effort search telemetry without affecting the search response.
#[allow(clippy::too_many_arguments)]
async fn emit_search_executed_event(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    actor: String,
    query: String,
    result_kind: &'static str,
    selected: Vec<String>,
    result_count: usize,
    latency_us: i64,
) {
    let store = match rt.events(token) {
        Ok(store) => store,
        Err(err) => {
            tracing::warn!(
                error = %err,
                namespace = token.namespace().as_str(),
                event_kind = "search_executed",
                "search_executed event store acquisition failed; search result is unaffected"
            );
            return;
        }
    };
    let payload = json!({
        "actor": actor,
        "served_by_profile_id": Value::Null,
        "query": query,
        "result_kind": result_kind,
        "result_count": result_count,
        "candidates": selected,
        "selected": selected,
        "latency_us": latency_us,
    });
    let event = khive_storage::Event::new(
        token.namespace().as_str(),
        "search",
        khive_types::EventKind::SearchExecuted,
        khive_types::SubstrateKind::Event,
        actor,
    )
    .with_payload(payload)
    .with_duration_us(latency_us);
    if let Err(err) = store.append_event(event).await {
        tracing::warn!(
            error = %err,
            "search_executed event append failed; search result is unaffected"
        );
    }
}
