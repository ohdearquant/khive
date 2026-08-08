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

/// Search substrate after the public `kind` discriminator and compatibility
/// filters have been reconciled against the loaded pack registry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchSubstrate {
    /// Entity storage and retrieval path.
    Entity,
    /// Note storage and retrieval path.
    Note,
}

/// Strict, canonical search request shared by the KG handler and the
/// multi-backend coordinator boundary.
///
/// Construction performs the same deny-unknown-fields deserialization,
/// granular-kind reconciliation, entity-type validation, and substrate-field
/// validation for every dispatch path. Downstream coordinator code receives
/// this type rather than rebuilding a narrower payload from raw JSON.
#[derive(Clone, Debug, PartialEq)]
pub struct ValidatedSearchRequest {
    query: String,
    limit: u32,
    substrate: SearchSubstrate,
    kind_filter: Option<String>,
    entity_type: Option<String>,
    include_superseded: bool,
    properties: Option<Value>,
    tags: Vec<String>,
    min_score: f64,
}

impl ValidatedSearchRequest {
    /// Parse and validate the canonical KG search wire contract.
    pub fn from_value(params: Value, registry: &VerbRegistry) -> Result<Self, RuntimeError> {
        let p: SearchParams = deser(params)?;
        let kind_raw = p
            .kind
            .as_deref()
            .ok_or_else(|| missing_kind_error("kind", registry))?;
        let properties = match p.properties {
            Some(Value::Object(map)) if !map.is_empty() => Some(Value::Object(map)),
            Some(Value::Object(_)) | None => None,
            Some(_) => {
                return Err(RuntimeError::InvalidInput(
                    "properties must be an object when provided".to_string(),
                ));
            }
        };
        let tags = p.tags.unwrap_or_default();
        let limit = p.limit.unwrap_or(10).min(100);
        let min_score = p.min_score.unwrap_or(0.0).max(0.0);

        match resolve_kind_spec(kind_raw, registry)? {
            KindSpec::Entity { specific } => {
                reject_search_field_for_substrate(
                    p.note_kind.as_ref(),
                    "note_kind",
                    SearchSubstrate::Entity,
                )?;
                reject_search_field_for_substrate(
                    p.include_superseded.as_ref(),
                    "include_superseded",
                    SearchSubstrate::Entity,
                )?;
                let kind_filter = reconcile_specific(
                    specific,
                    p.entity_kind.as_deref(),
                    |s| canonical_entity_kind(s, registry),
                    "entity_kind",
                )?;
                let entity_type = if let Some(ref raw_entity_type) = p.entity_type {
                    if let Some(ref kind) = kind_filter {
                        validate_entity_type(kind, Some(raw_entity_type), registry)?
                    } else {
                        Some(raw_entity_type.trim().to_ascii_lowercase())
                    }
                } else {
                    None
                };
                Ok(Self {
                    query: p.query,
                    limit,
                    substrate: SearchSubstrate::Entity,
                    kind_filter,
                    entity_type,
                    include_superseded: false,
                    properties,
                    tags,
                    min_score,
                })
            }
            KindSpec::Note { specific } => {
                reject_search_field_for_substrate(
                    p.entity_kind.as_ref(),
                    "entity_kind",
                    SearchSubstrate::Note,
                )?;
                reject_search_field_for_substrate(
                    p.entity_type.as_ref(),
                    "entity_type",
                    SearchSubstrate::Note,
                )?;
                let kind_filter = reconcile_specific(
                    specific,
                    p.note_kind.as_deref().filter(|kind| !kind.is_empty()),
                    |s| canonical_note_kind(s, registry),
                    "note_kind",
                )?;
                Ok(Self {
                    query: p.query,
                    limit,
                    substrate: SearchSubstrate::Note,
                    kind_filter,
                    entity_type: None,
                    include_superseded: p.include_superseded.unwrap_or(false),
                    properties,
                    tags,
                    min_score,
                })
            }
            KindSpec::Edge => Err(RuntimeError::InvalidInput(
                "search does not support kind=edge — use `list(kind=\"edge\", ...)` for edge browsing"
                    .into(),
            )),
            KindSpec::Event => Err(RuntimeError::InvalidInput(
                "search does not support kind=event — use `list(kind=\"event\", ...)` for event browsing"
                    .into(),
            )),
            KindSpec::Proposal => Err(RuntimeError::InvalidInput(
                "search does not support kind=proposal — use `list(kind=\"proposal\", ...)` for proposal browsing"
                    .into(),
            )),
        }
    }

    /// Free-text query supplied by the caller.
    pub fn query(&self) -> &str {
        &self.query
    }

    /// Caller limit after applying the public cap of 100.
    pub fn limit(&self) -> u32 {
        self.limit
    }

    /// Resolved entity or note substrate.
    pub fn substrate(&self) -> SearchSubstrate {
        self.substrate
    }

    /// Canonical granular entity/note kind, or `None` for a substrate-wide search.
    pub fn kind_filter(&self) -> Option<&str> {
        self.kind_filter.as_deref()
    }

    /// Canonical entity subtype filter; always `None` for note searches.
    pub fn entity_type(&self) -> Option<&str> {
        self.entity_type.as_deref()
    }

    /// Whether notes targeted by a `supersedes` edge remain eligible.
    pub fn include_superseded(&self) -> bool {
        self.include_superseded
    }

    /// Non-empty property-superset filter.
    pub fn properties(&self) -> Option<&Value> {
        self.properties.as_ref()
    }

    /// OR-matched tag filter; empty means unrestricted.
    pub fn tags(&self) -> &[String] {
        &self.tags
    }

    /// Non-negative result-score floor.
    pub fn min_score(&self) -> f64 {
        self.min_score
    }

    /// Bounded backend candidate window used to preserve filtered-result recall.
    pub fn candidate_limit(&self) -> u32 {
        if self.properties.is_some() || !self.tags.is_empty() {
            self.limit.saturating_mul(50).min(FILTERED_SCAN_CAP)
        } else {
            self.limit
        }
    }
}

fn reject_search_field_for_substrate<T>(
    value: Option<&T>,
    field: &str,
    substrate: SearchSubstrate,
) -> Result<(), RuntimeError> {
    if value.is_some() {
        let required = match substrate {
            SearchSubstrate::Entity => "note",
            SearchSubstrate::Note => "entity",
        };
        return Err(RuntimeError::InvalidInput(format!(
            "{field} is only valid when kind resolves to {required}"
        )));
    }
    Ok(())
}

impl KgPack {
    pub(crate) async fn handle_search(
        &self,
        token: &NamespaceToken,
        params: Value,
        registry: &VerbRegistry,
    ) -> Result<Value, RuntimeError> {
        let search_start = Instant::now();
        let request = ValidatedSearchRequest::from_value(params, registry)?;
        match request.substrate() {
            SearchSubstrate::Entity => {
                let props_filter = request.properties();
                let tag_filter = (!request.tags().is_empty()).then_some(request.tags());
                let hits = self
                    .runtime
                    .hybrid_search(
                        token,
                        request.query(),
                        None,
                        request.candidate_limit(),
                        request.kind_filter(),
                        request.entity_type(),
                        tag_filter.unwrap_or(&[]),
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
                            props_filter.is_none_or(|pf| props_match(props.as_ref(), pf))
                                && tag_filter.is_none_or(|wanted| tags_match_any(tags, wanted))
                        })
                        .take(request.limit() as usize)
                        .collect::<Vec<_>>()
                } else {
                    hits
                };

                let result: Vec<Value> = filtered_hits
                    .iter()
                    .filter(|h| h.score.to_f64() >= request.min_score())
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
                            "source": h.source.as_str(),
                            "title": h.title,
                            "snippet": h.snippet,
                            "created_at": created_at,
                        })
                    })
                    .collect();
                self.track_search_serve(
                    token,
                    request.query(),
                    "entity",
                    &result,
                    search_start.elapsed().as_micros() as i64,
                );
                to_json(&result)
            }
            SearchSubstrate::Note => {
                let props_filter = request.properties();
                let tag_filter = (!request.tags().is_empty()).then_some(request.tags());
                let hits = self
                    .runtime
                    .search_notes(
                        token,
                        request.query(),
                        None,
                        request.candidate_limit(),
                        request.kind_filter(),
                        request.include_superseded(),
                        tag_filter.unwrap_or(&[]),
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
                            let props_ok =
                                props_filter.is_none_or(|pf| props_match(props.as_ref(), pf));
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
                        .take(request.limit() as usize)
                        .collect()
                } else {
                    hits
                };

                let result: Vec<Value> = filtered_hits
                    .iter()
                    .filter(|h| h.score.to_f64() >= request.min_score())
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
                            "source": h.source.as_str(),
                            "title": h.title,
                            "snippet": h.snippet,
                            "created_at": created_at,
                        })
                    })
                    .collect();
                self.track_search_serve(
                    token,
                    request.query(),
                    "note",
                    &result,
                    search_start.elapsed().as_micros() as i64,
                );
                to_json(&result)
            }
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
