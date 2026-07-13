//! `list` verb handler.

use serde_json::Value;

use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError, VerbRegistry};
use khive_storage::types::PageRequest;
use khive_storage::EntityFilter;

use khive_runtime::EdgeListFilter;

use super::common::{
    canonical_entity_kind, canonical_note_kind, deser, event_filter_from_params,
    normalize_entity_timestamps, normalize_entity_timestamps_array,
    normalize_event_timestamps_array, parse_relation, reconcile_specific, remap_note_status,
    resolve_kind_spec, resolve_uuid_async, to_json, validate_entity_type, KindSpec, ListParams,
};
use crate::KgPack;

const ENTITY_LIST_CAP: u32 = 500;
const NOTE_LIST_CAP: u32 = 200;
const EVENT_LIST_CAP: u32 = 1000;

fn effective_list_limit(requested: u32, cap: u32) -> u32 {
    requested.min(cap)
}

fn render_list_response(items: Value, requested: u32, effective: u32) -> Value {
    if requested <= effective {
        return items;
    }
    serde_json::json!({
        "items": items,
        "requested_limit": requested,
        "effective_limit": effective,
        "limit_clamped": true,
    })
}

fn add_list_limit_metadata(response: &mut Value, requested: u32, effective: u32) {
    if requested <= effective {
        return;
    }
    response["requested_limit"] = serde_json::json!(requested);
    response["effective_limit"] = serde_json::json!(effective);
    response["limit_clamped"] = serde_json::json!(true);
}

impl KgPack {
    pub(crate) async fn handle_list(
        &self,
        token: &NamespaceToken,
        params: Value,
        registry: &VerbRegistry,
    ) -> Result<Value, RuntimeError> {
        let raw_kind = params
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        if raw_kind == "proposal" {
            return self.handle_list_proposals(token, params).await;
        }

        let p: ListParams = deser(params)?;
        let spec = resolve_kind_spec(&p.kind, registry)?;
        match spec {
            KindSpec::Entity { specific } => {
                if p.note_kind.as_deref().is_some_and(|s| !s.is_empty()) {
                    return Err(RuntimeError::InvalidInput(
                        "note_kind filter is not valid when kind=entity; use kind=note to list notes".into(),
                    ));
                }
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
                let requested = p.limit.unwrap_or(50);
                let limit = effective_list_limit(requested, ENTITY_LIST_CAP);
                let offset = p.offset.unwrap_or(0);
                let entities = if let Some(ref tag_list) = p.tags {
                    if tag_list.is_empty() {
                        self.runtime
                            .list_entities(
                                token,
                                kind_filter.as_deref(),
                                validated_et.as_deref(),
                                limit,
                                offset,
                            )
                            .await?
                    } else {
                        let filter = EntityFilter {
                            kinds: kind_filter
                                .as_deref()
                                .map(|k| vec![k.to_string()])
                                .unwrap_or_default(),
                            entity_types: validated_et
                                .as_deref()
                                .map(|t| vec![t.to_string()])
                                .unwrap_or_default(),
                            tags_any: tag_list.clone(),
                            namespaces: token
                                .visible_namespace_strs()
                                .iter()
                                .map(|s| s.to_string())
                                .collect(),
                            ..Default::default()
                        };
                        let page = self
                            .runtime
                            .entities(token)?
                            .query_entities(
                                token.namespace().as_str(),
                                filter,
                                PageRequest {
                                    offset: offset.into(),
                                    limit,
                                },
                            )
                            .await
                            .map_err(RuntimeError::Storage)?;
                        page.items
                    }
                } else {
                    self.runtime
                        .list_entities(
                            token,
                            kind_filter.as_deref(),
                            validated_et.as_deref(),
                            limit,
                            offset,
                        )
                        .await?
                };
                Ok(render_list_response(
                    normalize_entity_timestamps_array(to_json(&entities)?),
                    requested,
                    limit,
                ))
            }
            KindSpec::Edge => {
                let source_id = match p.source_id.as_deref() {
                    Some(s) => Some(resolve_uuid_async(s, &self.runtime, token).await?),
                    None => None,
                };
                let target_id = match p.target_id.as_deref() {
                    Some(s) => Some(resolve_uuid_async(s, &self.runtime, token).await?),
                    None => None,
                };
                let relations: Vec<_> = p
                    .relations
                    .unwrap_or_default()
                    .iter()
                    .map(|s| parse_relation(s))
                    .collect::<Result<Vec<_>, _>>()?;
                let filter = EdgeListFilter {
                    source_id,
                    target_id,
                    relations,
                    min_weight: p.min_weight,
                    max_weight: p.max_weight,
                };
                let requested = p.limit.unwrap_or(100);
                let cap = KhiveRuntime::EDGE_LIST_MAX_LIMIT;
                let limit = effective_list_limit(requested, cap);
                if let Some(ref after_str) = p.after {
                    // An empty string opts into cursor-mode pagination while
                    // starting from the beginning of the set (no prior page).
                    let after = if after_str.is_empty() {
                        None
                    } else {
                        Some(uuid::Uuid::parse_str(after_str).map_err(|e| {
                            RuntimeError::InvalidInput(format!(
                                "after: invalid UUID {after_str:?}: {e}"
                            ))
                        })?)
                    };
                    let (edges, next_after) = self
                        .runtime
                        .list_edges_after(token, filter, after, limit)
                        .await?;
                    let mut out = serde_json::json!({
                        "edges": to_json(&edges)?,
                        "next_after": next_after,
                    });
                    add_list_limit_metadata(&mut out, requested, limit);
                    Ok(out)
                } else {
                    let offset = p.offset.unwrap_or(0);
                    let edges = self
                        .runtime
                        .list_edges(token, filter, limit, offset)
                        .await?;
                    Ok(render_list_response(to_json(&edges)?, requested, limit))
                }
            }
            KindSpec::Note { specific } => {
                let kind_filter = reconcile_specific(
                    specific,
                    p.note_kind.as_deref().filter(|s| !s.is_empty()),
                    |s| canonical_note_kind(s, registry),
                    "note_kind",
                )?;
                let requested = p.limit.unwrap_or(20);
                let limit = effective_list_limit(requested, NOTE_LIST_CAP);
                let offset = p.offset.unwrap_or(0);

                let has_msg_filter = p.thread_id.is_some()
                    || p.direction.is_some()
                    || p.from.is_some()
                    || p.to.is_some()
                    || p.read.is_some()
                    || p.delivered.is_some();

                let thread_id_filter = p.thread_id.as_deref();
                let direction_filter = p.direction.as_deref();
                let from_filter = p.from.as_deref();
                let to_filter = p.to.as_deref();
                let read_filter = p.read;
                let delivered_filter = p.delivered;

                const PAGE_SIZE: u32 = 200;
                const MAX_SCAN_TOTAL: u32 = 10_000;

                let notes: Vec<_> = if has_msg_filter {
                    let mut collected: Vec<_> = Vec::new();
                    let mut db_offset: u32 = 0;
                    let target_after_skip = offset as usize + limit as usize;
                    loop {
                        let remaining_scan =
                            MAX_SCAN_TOTAL.saturating_sub(db_offset).min(PAGE_SIZE);
                        if remaining_scan == 0 {
                            break;
                        }
                        let page = self
                            .runtime
                            .list_notes(token, kind_filter.as_deref(), remaining_scan, db_offset)
                            .await?;
                        let fetched = page.len() as u32;
                        for n in page {
                            if n.deleted_at.is_some() {
                                continue;
                            }
                            let props = n.properties.as_ref();
                            let passes = (|| {
                                if let Some(wanted_thread) = thread_id_filter {
                                    let stored = match props
                                        .and_then(|p| p.get("thread_id"))
                                        .and_then(Value::as_str)
                                        .filter(|s| !s.is_empty())
                                    {
                                        Some(s) => s,
                                        None => return false,
                                    };
                                    let matches = stored == wanted_thread
                                        || matches!(
                                            (stored.get(..8), wanted_thread.get(..8)),
                                            (Some(a), Some(b)) if a == b
                                        );
                                    if !matches {
                                        return false;
                                    }
                                }
                                if let Some(wanted_dir) = direction_filter {
                                    let stored = props
                                        .and_then(|p| p.get("direction"))
                                        .and_then(Value::as_str)
                                        .unwrap_or("");
                                    if stored != wanted_dir {
                                        return false;
                                    }
                                }
                                if let Some(wanted_from) = from_filter {
                                    let stored = props
                                        .and_then(|p| p.get("from"))
                                        .and_then(Value::as_str)
                                        .unwrap_or("");
                                    if stored != wanted_from {
                                        return false;
                                    }
                                }
                                if let Some(wanted_to) = to_filter {
                                    let stored = props
                                        .and_then(|p| p.get("to"))
                                        .and_then(Value::as_str)
                                        .unwrap_or("");
                                    if stored != wanted_to {
                                        return false;
                                    }
                                }
                                if let Some(wanted_read) = read_filter {
                                    let stored = props
                                        .and_then(|p| p.get("read"))
                                        .and_then(Value::as_bool)
                                        .unwrap_or(false);
                                    if stored != wanted_read {
                                        return false;
                                    }
                                }
                                if let Some(wanted_delivered) = delivered_filter {
                                    let is_delivered = props
                                        .and_then(|p| p.get("delivered_at"))
                                        .map(|v| !v.is_null())
                                        .unwrap_or(false);
                                    if is_delivered != wanted_delivered {
                                        return false;
                                    }
                                }
                                true
                            })();
                            if passes {
                                collected.push(n);
                                if collected.len() >= target_after_skip {
                                    break;
                                }
                            }
                        }
                        if collected.len() >= target_after_skip || fetched < PAGE_SIZE {
                            break;
                        }
                        db_offset += fetched;
                    }
                    collected
                } else {
                    self.runtime
                        .list_notes(token, kind_filter.as_deref(), limit, offset)
                        .await?
                };

                let remapped: Vec<Value> = if has_msg_filter {
                    notes
                        .into_iter()
                        .skip(offset as usize)
                        .take(limit as usize)
                        .map(|n| {
                            to_json(&n)
                                .map(normalize_entity_timestamps)
                                .map(remap_note_status)
                                .unwrap_or_else(|_| serde_json::json!({}))
                        })
                        .collect()
                } else {
                    notes
                        .iter()
                        .filter(|n| n.deleted_at.is_none())
                        .map(|n| {
                            to_json(n)
                                .map(normalize_entity_timestamps)
                                .map(remap_note_status)
                                .unwrap_or_else(|_| serde_json::json!({}))
                        })
                        .collect()
                };
                Ok(render_list_response(to_json(&remapped)?, requested, limit))
            }
            KindSpec::Proposal => unreachable!("kind=proposal fast-pathed before deser"),
            KindSpec::Event => {
                let requested = p.limit.unwrap_or(100).max(1);
                let limit = effective_list_limit(requested, EVENT_LIST_CAP);
                let offset = p.offset.unwrap_or(0);
                let (filter, outcome) = event_filter_from_params(&p)?;

                let items = if let Some(wanted_outcome) = outcome {
                    let mut items = Vec::new();
                    let mut skipped = 0u32;
                    let mut raw_offset = 0u32;
                    let scan_ceiling = offset.saturating_add(limit).saturating_mul(20);

                    while (items.len() as u32) < limit {
                        let remaining = scan_ceiling.saturating_sub(raw_offset);
                        if remaining == 0 {
                            break;
                        }
                        let batch_size = 100u32.min(remaining);
                        let page = self
                            .runtime
                            .list_events(
                                token,
                                filter.clone(),
                                PageRequest {
                                    limit: batch_size,
                                    offset: raw_offset.into(),
                                },
                            )
                            .await?;
                        let batch_len = page.items.len() as u32;
                        if batch_len == 0 {
                            break;
                        }
                        raw_offset = raw_offset.saturating_add(batch_len);
                        let eof = batch_len < batch_size;

                        for event in page.items {
                            if event.outcome != wanted_outcome {
                                continue;
                            }
                            if skipped < offset {
                                skipped += 1;
                                continue;
                            }
                            items.push(event);
                            if (items.len() as u32) >= limit {
                                break;
                            }
                        }

                        if eof {
                            break;
                        }
                    }
                    items
                } else {
                    let page = self
                        .runtime
                        .list_events(
                            token,
                            filter,
                            PageRequest {
                                limit,
                                offset: offset.into(),
                            },
                        )
                        .await?;
                    page.items
                };
                Ok(render_list_response(
                    normalize_event_timestamps_array(to_json(&items)?),
                    requested,
                    limit,
                ))
            }
        }
    }
}
