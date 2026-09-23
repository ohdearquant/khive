//! `search` verb handler.

use std::collections::HashMap;

/// Maximum candidate window used when property/tag filters are active.
/// See `docs/api/scan-cliff.md`.
const FILTERED_SCAN_CAP: u32 = 500;

use std::time::Instant;

use serde_json::{json, Value};
use uuid::Uuid;

use khive_runtime::{
    micros_to_iso, KhiveRuntime, NamespaceToken, RankScoreKind, RuntimeError, SearchSignals,
    SearchSource, VerbRegistry,
};
use khive_score::DeterministicScore;
use khive_storage::types::PageRequest;
use khive_storage::EntityFilter;

use super::common::{
    canonical_entity_kind, canonical_note_kind, deser, missing_kind_error, props_match,
    reconcile_specific, resolve_kind_spec, tags_match_any, to_json, validate_entity_type_filter,
    KindSpec, SearchParams,
};
use crate::KgPack;

/// Canonical search ranking fields shared by KG and coordinated search.
/// These floats are wire projections only, never inputs to rank decisions.
pub fn search_rank_fields(
    score: DeterministicScore,
    kind: RankScoreKind,
    signals: SearchSignals,
) -> Value {
    let rank_score = score.to_f64();
    let mut evidence = serde_json::Map::new();
    if let Some(score) = signals.vector_similarity {
        evidence.insert("vector_similarity".to_string(), json!(score.to_f64()));
    }
    if let Some(score) = signals.keyword_score {
        evidence.insert("keyword_score".to_string(), json!(score.to_f64()));
    }
    json!({
        "rank_score": rank_score,
        "score": rank_score,
        "rank_score_kind": kind.as_str(),
        "signals": evidence,
    })
}

/// Search substrate after the public `kind` discriminator and compatibility
/// filters have been reconciled against the loaded pack registry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchSubstrate {
    /// Entity storage and retrieval path.
    Entity,
    /// Note storage and retrieval path.
    Note,
}

/// Order the handler applies to search hits before imposing the caller limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchOrder {
    /// Relevance score, highest first. The default.
    Score,
    /// Record `updated_at`, most recently updated first.
    UpdatedAt,
    /// Record `created_at`, most recently created first.
    CreatedAt,
}

impl SearchOrder {
    /// The record timestamp this order sorts on, or `None` for the relevance
    /// order, which does not read the record at all.
    fn timestamp_of(self, created_at: i64, updated_at: i64) -> Option<i64> {
        match self {
            Self::Score => None,
            Self::UpdatedAt => Some(updated_at),
            Self::CreatedAt => Some(created_at),
        }
    }
}

/// Reorder hits by a record timestamp, most recent first, then impose `limit`.
///
/// A hit whose record was absent from the fetched batch has no timestamp and
/// sorts last. The sort is stable, so hits sharing a timestamp keep the
/// relevance order they arrived in, which makes the result deterministic.
fn apply_time_order<T>(hits: &mut Vec<T>, limit: usize, timestamp: impl Fn(&T) -> Option<i64>) {
    hits.sort_by_key(|hit| std::cmp::Reverse(timestamp(hit)));
    hits.truncate(limit);
}

/// Entity fields the hit render and the time orders read, fetched once per
/// candidate batch.
struct EntityMeta {
    kind: String,
    properties: Option<Value>,
    tags: Vec<String>,
    created_at: i64,
    updated_at: i64,
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
    source: Option<SearchSource>,
    min_score: f64,
    min_rank_score: DeterministicScore,
    order_by: SearchOrder,
}

impl ValidatedSearchRequest {
    /// Parse and validate the canonical KG search wire contract.
    pub fn from_value(params: Value, registry: &VerbRegistry) -> Result<Self, RuntimeError> {
        if params.get("min_rank_score").is_some() && params.get("min_score").is_some() {
            return Err(RuntimeError::InvalidInput(
                "supply only min_rank_score; min_score is its deprecated alias".to_string(),
            ));
        }
        let floor_name = if params.get("min_rank_score").is_some() {
            "min_rank_score"
        } else {
            "min_score"
        };
        let p: SearchParams = deser(params)?;
        super::common::require_object_param(p.properties.as_ref(), "properties")?;
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
        let min_score = match p.min_rank_score.or(p.min_score) {
            None => 0.0,
            Some(value) if value.is_finite() && (0.0..=1.0).contains(&value) => value,
            Some(value) => {
                return Err(RuntimeError::InvalidInput(format!(
                    "{floor_name} must be between 0.0 and 1.0; got {value}"
                )))
            }
        };
        let min_rank_score = DeterministicScore::from_f64(min_score);
        let source = match p.source.as_deref() {
            None => None,
            Some("text") => Some(SearchSource::Text),
            Some("vector") => Some(SearchSource::Vector),
            Some("both") => Some(SearchSource::Both),
            Some(_) => {
                return Err(RuntimeError::InvalidInput(
                    "source must be one of: text, vector, both".to_string(),
                ));
            }
        };
        // Both time orders are descending: the question they exist to answer is
        // "what is the most recent state", so there is no ascending spelling to
        // choose between. An unrecognised value is refused rather than falling
        // back to the score order, which would answer a different question and
        // read as success.
        let order_by = match p.order_by.as_deref() {
            None | Some("score") => SearchOrder::Score,
            Some("updated_at") => SearchOrder::UpdatedAt,
            Some("created_at") => SearchOrder::CreatedAt,
            Some(_) => {
                return Err(RuntimeError::InvalidInput(
                    "order_by must be one of: score, updated_at, created_at".to_string(),
                ));
            }
        };

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
                let entity_type = validate_entity_type_filter(
                    kind_filter.as_deref(),
                    p.entity_type.as_deref(),
                    registry,
                )?;
                Ok(Self {
                    query: p.query,
                    limit,
                    substrate: SearchSubstrate::Entity,
                    kind_filter,
                    entity_type,
                    include_superseded: false,
                    properties,
                    tags,
                    source,
                    min_score,
                    min_rank_score,
                    order_by,
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
                    source,
                    min_score,
                    min_rank_score,
                    order_by,
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

    /// Exact retrieval leg membership required by the caller.
    pub fn source(&self) -> Option<SearchSource> {
        self.source
    }

    /// Original validated wire floor, retained for compatibility callers.
    /// New ranking decisions should use [`Self::min_rank_score`] directly.
    pub fn min_score(&self) -> f64 {
        self.min_score
    }

    /// Inclusive strategy-local floor, quantized once from validated input.
    pub fn min_rank_score(&self) -> DeterministicScore {
        self.min_rank_score
    }

    /// Order applied to hits before the caller limit is imposed.
    pub fn order_by(&self) -> SearchOrder {
        self.order_by
    }

    /// Bounded backend candidate window used to preserve filtered-result recall.
    pub fn candidate_limit(&self) -> u32 {
        // A time order widens the window for the same reason a filter does: it
        // selects a different subset than the score order, so re-ranking only
        // the top `limit` scored hits would answer "the most recent of the most
        // relevant few" instead of the question asked.
        if self.properties.is_some()
            || !self.tags.is_empty()
            || self.source.is_some()
            || self.order_by != SearchOrder::Score
        {
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
                let source_filter = request.source();
                let mut hits = self
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
                hits.retain(|hit| hit.score >= request.min_rank_score());

                let candidate_ids: Vec<Uuid> = hits.iter().map(|h| h.entity_id).collect();
                let entity_meta: HashMap<Uuid, EntityMeta> = if candidate_ids.is_empty() {
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
                        .map(|e| {
                            (
                                e.id,
                                EntityMeta {
                                    kind: e.kind,
                                    properties: e.properties,
                                    tags: e.tags,
                                    created_at: e.created_at,
                                    updated_at: e.updated_at,
                                },
                            )
                        })
                        .collect()
                };

                let mut filtered_hits =
                    if props_filter.is_some() || tag_filter.is_some() || source_filter.is_some() {
                        let kept = hits.into_iter().filter(|h| {
                            if source_filter.is_some_and(|source| h.source != source) {
                                return false;
                            }
                            let Some(meta) = entity_meta.get(&h.entity_id) else {
                                return false;
                            };
                            props_filter.is_none_or(|pf| props_match(meta.properties.as_ref(), pf))
                                && tag_filter
                                    .is_none_or(|wanted| tags_match_any(&meta.tags, wanted))
                        });
                        if request.order_by() == SearchOrder::Score {
                            // Score order imposes the caller limit as candidates
                            // stream past the filters, as it always has.
                            kept.take(request.limit() as usize).collect::<Vec<_>>()
                        } else {
                            // A time order has to see every candidate that passed
                            // the filters before it can pick the most recent ones.
                            kept.collect::<Vec<_>>()
                        }
                    } else {
                        hits
                    };

                if request.order_by() != SearchOrder::Score {
                    apply_time_order(&mut filtered_hits, request.limit() as usize, |h| {
                        entity_meta.get(&h.entity_id).and_then(|meta| {
                            request
                                .order_by()
                                .timestamp_of(meta.created_at, meta.updated_at)
                        })
                    });
                }

                let result: Vec<Value> = filtered_hits
                    .iter()
                    .map(|h| {
                        let meta = entity_meta.get(&h.entity_id);
                        let entity_kind = meta.map(|m| m.kind.as_str());
                        let created_at = meta.map(|m| micros_to_iso(m.created_at));
                        let updated_at = meta.map(|m| micros_to_iso(m.updated_at));
                        let mut row = serde_json::json!({
                            "id": h.entity_id.to_string(),
                            // `kind`/`name` match the list()/get() row shape (#1174);
                            // `entity_kind`/`title` are kept for compatibility.
                            "kind": entity_kind,
                            "entity_kind": entity_kind,
                            "name": h.title,
                            "source": h.source.as_str(),
                            "title": h.title,
                            "snippet": h.snippet,
                            "created_at": created_at,
                            "updated_at": updated_at,
                            // Entities carry no persisted revision — the column
                            // exists on notes only — so the field is present for
                            // row-shape parity across substrates and always null.
                            "version": Value::Null,
                        });
                        row.as_object_mut()
                            .expect("search row is an object")
                            .extend(
                                search_rank_fields(h.score, h.rank_score_kind, h.signals)
                                    .as_object()
                                    .expect("ranking fields are an object")
                                    .clone(),
                            );
                        row
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
                let source_filter = request.source();
                let mut hits = self
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
                hits.retain(|hit| hit.score >= request.min_rank_score());

                // Batch-fetch all candidate notes in one IN(...) query instead of
                // N individual gets. Notes absent from the batch result (deleted
                // between the search and the fetch) are simply absent from the map
                // and filtered out by the `note_meta.get` guard below.
                let note_meta: HashMap<Uuid, khive_storage::note::Note> = if hits.is_empty() {
                    HashMap::new()
                } else {
                    let candidate_ids: Vec<Uuid> = hits.iter().map(|h| h.note_id).collect();
                    let note_store = self.runtime.notes(token)?;
                    note_store
                        .get_notes_batch(&candidate_ids)
                        .await
                        .map_err(RuntimeError::Storage)?
                        .into_iter()
                        .map(|n| (n.id, n))
                        .collect()
                };

                let mut filtered_hits: Vec<_> =
                    if props_filter.is_some() || tag_filter.is_some() || source_filter.is_some() {
                        let kept = hits.into_iter().filter(|h| {
                            if source_filter.is_some_and(|source| h.source != source) {
                                return false;
                            }
                            let Some(note) = note_meta.get(&h.note_id) else {
                                return false;
                            };
                            let props = &note.properties;
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
                        });
                        if request.order_by() == SearchOrder::Score {
                            // Score order imposes the caller limit as candidates
                            // stream past the filters, as it always has.
                            kept.take(request.limit() as usize).collect()
                        } else {
                            // A time order has to see every candidate that passed
                            // the filters before it can pick the most recent ones.
                            kept.collect()
                        }
                    } else {
                        hits
                    };

                if request.order_by() != SearchOrder::Score {
                    apply_time_order(&mut filtered_hits, request.limit() as usize, |h| {
                        note_meta.get(&h.note_id).and_then(|note| {
                            request
                                .order_by()
                                .timestamp_of(note.created_at, note.updated_at)
                        })
                    });
                }

                let result: Vec<Value> = filtered_hits
                    .iter()
                    .filter_map(|h| {
                        let note = note_meta.get(&h.note_id)?;
                        let note_kind = note.kind.as_str();
                        let name = &note.name;
                        let created_at = micros_to_iso(note.created_at);
                        let updated_at = micros_to_iso(note.updated_at);
                        let mut row = serde_json::json!({
                            "id": h.note_id.to_string(),
                            // `kind`/`name` match the list()/get() row shape (#1174);
                            // `note_kind`/`title` are kept for compatibility.
                            "kind": note_kind,
                            "note_kind": note_kind,
                            "name": name,
                            "source": h.source.as_str(),
                            "title": h.title,
                            "snippet": h.snippet,
                            "created_at": created_at,
                            "updated_at": updated_at,
                            "version": note.version,
                        });
                        row.as_object_mut()
                            .expect("search row is an object")
                            .extend(
                                search_rank_fields(h.score, h.rank_score_kind, h.signals)
                                    .as_object()
                                    .expect("ranking fields are an object")
                                    .clone(),
                            );
                        Some(row)
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

        khive_runtime::track_named_background_task("kg_search_event_append", async move {
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
