//! `list` verb handler.

use serde_json::Value;

use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError, VerbRegistry};
use khive_storage::note::Note;
use khive_storage::types::{PageRequest, SqlStatement, SqlValue};
use khive_storage::EntityFilter;

use khive_runtime::EdgeListFilter;

use super::common::{
    canonical_entity_kind, canonical_note_kind, deser, entity_type_filter_matches,
    event_filter_from_params, normalize_entity_timestamps, normalize_entity_timestamps_array,
    normalize_event_timestamps_array, parse_note_content, parse_relation, reconcile_specific,
    remap_note_status, resolve_kind_spec, resolve_uuid_async, tags_match_any, to_json,
    validate_entity_type_filter, KindSpec, ListParams,
};
use crate::sql::sql;
use crate::KgPack;

const ENTITY_LIST_CAP: u32 = 500;
const NOTE_LIST_CAP: u32 = 200;
const EVENT_LIST_CAP: u32 = 1000;

fn effective_list_limit(requested: u32, cap: u32) -> u32 {
    requested.min(cap)
}

/// How many rows to ask the store for when the caller wants `limit` of them.
///
/// The extra row is never returned. Its only job is to answer "is there
/// more", which `limit_clamped` cannot: a caller that passes exactly the cap
/// gets `limit_clamped: false` whether the population held 500 rows or 9,285.
/// That is the one limit value at which the disclosure was guaranteed to say
/// nothing, and it is the value a caller enumerating a population picks,
/// because it is the largest page it can get.
pub(super) fn overfetch_limit(limit: u32) -> u32 {
    limit.saturating_add(1)
}

/// Splits an over-fetched page into the rows to return and whether the
/// population continued past them.
pub(super) fn split_overfetched<T>(mut items: Vec<T>, limit: u32) -> (Vec<T>, bool) {
    let has_more = items.len() > limit as usize;
    items.truncate(limit as usize);
    (items, has_more)
}

pub(super) fn render_list_response(
    items: Value,
    requested: u32,
    effective: u32,
    has_more: bool,
) -> Value {
    serde_json::json!({
        "items": items,
        "requested_limit": requested,
        "effective_limit": effective,
        "limit_clamped": requested > effective,
        "has_more": has_more,
    })
}

pub(super) fn add_list_limit_metadata(
    response: &mut Value,
    requested: u32,
    effective: u32,
    has_more: bool,
) {
    response["requested_limit"] = serde_json::json!(requested);
    response["effective_limit"] = serde_json::json!(effective);
    response["limit_clamped"] = serde_json::json!(requested > effective);
    // Separate causes, because a caller can act on one and can only page
    // through the other: `limit_clamped` says the cap reduced the request,
    // `has_more` says the population did not fit. They are independent, and a
    // page can be truncated with neither, either, or both set.
    response["has_more"] = serde_json::json!(has_more);
}

fn parse_after_cursor(raw: &str) -> Result<Option<uuid::Uuid>, RuntimeError> {
    if raw.is_empty() {
        return Ok(None);
    }
    uuid::Uuid::parse_str(raw).map(Some).map_err(|error| {
        RuntimeError::InvalidInput(format!(
            "after must be a full UUID because a short-prefix resolution can miss or be \
             ambiguous, while keyset pagination needs the exact stable insertion boundary; \
             got {raw:?}: {error}"
        ))
    })
}

async fn resolve_message_thread_filter(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    raw: &str,
    primary_only: bool,
) -> Result<String, RuntimeError> {
    // Message-scope invariant: this resolver ONLY serves the message thread
    // filter, so the DISTINCT scan binds kind='message' unconditionally. The
    // caller's kind_filter can legitimately be None (list(kind="note")), and
    // scanning every note kind would let a non-message note carrying a
    // `thread_id` property inject candidates — producing false ambiguity
    // errors or resolutions against rows the message filter can never return.
    if let Ok(thread_id) = raw.parse::<uuid::Uuid>() {
        return Ok(thread_id.as_hyphenated().to_string());
    }
    if raw.len() < 8 || !raw.chars().all(|character| character.is_ascii_hexdigit()) {
        // Legacy non-UUID thread labels were historically accepted by this
        // filter. Preserve their exact-match behavior without treating them as
        // UUID prefixes.
        return Ok(raw.to_string());
    }

    let normalized_prefix = raw.to_ascii_lowercase();
    // Resolve over the SAME visibility scope the subsequent note listing
    // reads (`['local'] ∪ visible_namespaces`): resolving against only the
    // primary namespace rejects prefixes of threads the list itself would
    // return, and silently hides a cross-namespace prefix collision.
    let visible = if primary_only {
        vec![token.namespace().as_str()]
    } else {
        token.visible_namespace_strs()
    };
    let visible_json = serde_json::to_string(&visible).map_err(|error| {
        RuntimeError::Internal(format!("serialize visible namespaces: {error}"))
    })?;
    let mut reader = runtime
        .sql()
        .reader()
        .await
        .map_err(RuntimeError::Storage)?;
    let rows = reader
        .query_all(SqlStatement {
            sql: sql!("message_threads_list").to_string(),
            params: vec![SqlValue::Text(visible_json)],
            label: Some("list.resolve_message_thread_filter".to_string()),
        })
        .await
        .map_err(RuntimeError::Storage)?;

    // Exact-match precedence for legacy stored labels: an all-hex >=8-char
    // label like "deadbeef" is not a UUID, but it may be stored verbatim as a
    // thread_id by pre-v1 rows. If any stored value equals the filter string
    // byte-for-byte, it is an exact label match — not a UUID-prefix query —
    // and must resolve to itself even when a UUID in the namespace happens to
    // carry the same hex prefix. Case-only variants are collected too: when no
    // UUID prefix matches, the stored spelling is returned so downstream
    // filtering stays exact-string coherent.
    let mut case_variant_label: Option<String> = None;
    let mut resolved: Option<uuid::Uuid> = None;
    for row in &rows {
        let Some(stored) = row.get("thread_id").and_then(|value| match value {
            SqlValue::Text(value) => Some(value.as_str()),
            _ => None,
        }) else {
            continue;
        };
        if stored == raw {
            return Ok(raw.to_string());
        }
        if case_variant_label.is_none()
            && stored.len() == raw.len()
            && stored.eq_ignore_ascii_case(raw)
        {
            case_variant_label = Some(stored.to_string());
        }
        let Ok(candidate) = stored.parse::<uuid::Uuid>() else {
            continue;
        };
        if !candidate
            .simple()
            .to_string()
            .starts_with(&normalized_prefix)
        {
            continue;
        }
        match resolved {
            None => resolved = Some(candidate),
            Some(existing) if existing == candidate => {}
            Some(_) => {
                return Err(RuntimeError::InvalidInput(format!(
                    "list: ambiguous thread_id prefix {raw:?} across the caller's visible \
                     namespaces; use a full UUID to identify one exact thread"
                )))
            }
        }
    }

    if let Some(id) = resolved {
        return Ok(id.as_hyphenated().to_string());
    }

    if let Some(stored) = case_variant_label {
        return Ok(stored);
    }

    Err(RuntimeError::InvalidInput(format!(
        "list: no message thread matches prefix {raw:?} in the caller's visible \
         namespaces; a prefix can miss, so use the full thread UUID"
    )))
}

pub(super) fn note_matches_list_filters(note: &Note, params: &ListParams) -> bool {
    let properties = note.properties.as_ref();
    if let Some(wanted) = params.tags.as_deref().filter(|tags| !tags.is_empty()) {
        let stored = properties
            .and_then(|value| value.get("tags"))
            .and_then(Value::as_array)
            .map(|tags| {
                tags.iter()
                    .filter_map(Value::as_str)
                    .map(ToOwned::to_owned)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let matches = match params.tag_mode.unwrap_or_default() {
            khive_storage::note::NoteTagMode::Any => tags_match_any(&stored, wanted),
            khive_storage::note::NoteTagMode::All => wanted.iter().all(|wanted| {
                stored
                    .iter()
                    .any(|stored| stored.eq_ignore_ascii_case(wanted))
            }),
        };
        if !matches {
            return false;
        }
    }
    if let Some(wanted_thread) = params.thread_id.as_deref() {
        let Some(stored) = properties
            .and_then(|value| value.get("thread_id"))
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        else {
            return false;
        };
        let matches = match wanted_thread.parse::<uuid::Uuid>() {
            Ok(wanted) => stored
                .parse::<uuid::Uuid>()
                .is_ok_and(|stored| stored == wanted),
            Err(_) => stored == wanted_thread,
        };
        if !matches {
            return false;
        }
    }
    if let Some(wanted) = params.direction.as_deref() {
        let stored = properties
            .and_then(|value| value.get("direction"))
            .and_then(Value::as_str)
            .unwrap_or("");
        if stored != wanted {
            return false;
        }
    }
    if let Some(wanted) = params.from.as_deref() {
        let stored = properties
            .and_then(|value| value.get("from"))
            .and_then(Value::as_str)
            .unwrap_or("");
        if stored != wanted {
            return false;
        }
    }
    if let Some(wanted) = params.to.as_deref() {
        let stored = properties
            .and_then(|value| value.get("to"))
            .and_then(Value::as_str)
            .unwrap_or("");
        if stored != wanted {
            return false;
        }
    }
    if let Some(wanted) = params.read {
        let stored = properties
            .and_then(|value| value.get("read"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if stored != wanted {
            return false;
        }
    }
    if let Some(wanted) = params.delivered {
        let stored = properties
            .and_then(|value| value.get("delivered_at"))
            .is_some_and(|value| !value.is_null());
        if stored != wanted {
            return false;
        }
    }
    true
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

        let mut p: ListParams = deser(params)?;
        if p.after.is_some() && p.offset.is_some() {
            return Err(RuntimeError::InvalidInput(
                "after and offset are mutually exclusive pagination modes".into(),
            ));
        }
        if p.after.is_some() && p.limit == Some(0) {
            return Err(RuntimeError::InvalidInput(
                "cursor pagination requires limit greater than zero".into(),
            ));
        }
        let spec = resolve_kind_spec(&p.kind, registry)?;
        if !matches!(&spec, KindSpec::Note { .. })
            && (p.key_prefix.is_some()
                || p.after_key.is_some()
                || p.created_after.is_some()
                || p.updated_after.is_some()
                || p.tag_mode.is_some())
        {
            return Err(RuntimeError::InvalidInput(
                "key, timestamp and tag_mode filters require notes".into(),
            ));
        }
        if p.after_key.is_some() && p.key_prefix.is_none() {
            return Err(RuntimeError::InvalidInput(
                "after_key requires key_prefix".into(),
            ));
        }
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
                let validated_et = validate_entity_type_filter(
                    kind_filter.as_deref(),
                    p.entity_type.as_deref(),
                    registry,
                )?;
                let filter = EntityFilter {
                    kinds: kind_filter
                        .as_deref()
                        .map(|kind| vec![kind.to_string()])
                        .unwrap_or_default(),
                    entity_types_by_kind: entity_type_filter_matches(
                        kind_filter.as_deref(),
                        validated_et.as_deref(),
                        registry,
                    ),
                    legacy_entity_type_fallback: true,
                    tags_any: p.tags.clone().unwrap_or_default(),
                    ..Default::default()
                };
                let requested = p.limit.unwrap_or(50);
                let limit = effective_list_limit(requested, ENTITY_LIST_CAP);
                if let Some(after_raw) = p.after.as_deref() {
                    let after = parse_after_cursor(after_raw)?;
                    let (entities, next_after) = self
                        .runtime
                        .list_entities_after_filtered(token, filter, after, limit)
                        .await?;
                    let mut response = serde_json::json!({
                        "entities": normalize_entity_timestamps_array(to_json(&entities)?),
                        "next_after": next_after,
                    });
                    // Keyset mode already carries the answer: a continuation
                    // boundary exists exactly when the population continued.
                    add_list_limit_metadata(&mut response, requested, limit, next_after.is_some());
                    return Ok(response);
                }
                let offset = p.offset.unwrap_or(0);
                let fetch = overfetch_limit(limit);
                let entities = self
                    .runtime
                    .list_entities_filtered(token, filter, fetch, offset)
                    .await?;
                let (entities, has_more) = split_overfetched(entities, limit);
                Ok(render_list_response(
                    normalize_entity_timestamps_array(to_json(&entities)?),
                    requested,
                    limit,
                    has_more,
                ))
            }
            KindSpec::Edge => {
                if p.tags.as_ref().is_some_and(|tags| !tags.is_empty()) {
                    return Err(RuntimeError::InvalidInput(
                        "tags filter is valid only for entity and note lists".into(),
                    ));
                }
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
                    let after = parse_after_cursor(after_str)?;
                    let (edges, next_after) = self
                        .runtime
                        .list_edges_after(token, filter, after, limit)
                        .await?;
                    let mut out = serde_json::json!({
                        "edges": to_json(&edges)?,
                        "next_after": next_after,
                    });
                    add_list_limit_metadata(&mut out, requested, limit, next_after.is_some());
                    Ok(out)
                } else {
                    let offset = p.offset.unwrap_or(0);
                    let edges = self
                        .runtime
                        .list_edges(token, filter, overfetch_limit(limit), offset)
                        .await?;
                    let (edges, has_more) = split_overfetched(edges, limit);
                    Ok(render_list_response(
                        to_json(&edges)?,
                        requested,
                        limit,
                        has_more,
                    ))
                }
            }
            KindSpec::Note { specific } => {
                let kind_filter = reconcile_specific(
                    specific,
                    p.note_kind.as_deref().filter(|s| !s.is_empty()),
                    |s| canonical_note_kind(s, registry),
                    "note_kind",
                )?;
                if let Some(raw_thread_id) = p.thread_id.clone() {
                    p.thread_id = Some(
                        resolve_message_thread_filter(
                            &self.runtime,
                            token,
                            &raw_thread_id,
                            p.key_prefix.is_some(),
                        )
                        .await?,
                    );
                }
                let requested = p.limit.unwrap_or(20);
                let limit = effective_list_limit(requested, NOTE_LIST_CAP);
                let filter = super::note_list::note_filter(&p, kind_filter.as_deref())?;
                if p.key_prefix.is_some() {
                    return super::note_list::list_keyed_notes(
                        &self.runtime,
                        token,
                        &p,
                        &filter,
                        requested,
                        limit,
                    )
                    .await;
                }
                let has_note_filter = p.tags.as_ref().is_some_and(|tags| !tags.is_empty())
                    || p.thread_id.is_some()
                    || p.direction.is_some()
                    || p.from.is_some()
                    || p.to.is_some()
                    || p.read.is_some()
                    || p.delivered.is_some();
                const PAGE_SIZE: u32 = 200;
                const MAX_SCAN_TOTAL: u32 = 10_000;

                if let Some(after_raw) = p.after.as_deref() {
                    let after = parse_after_cursor(after_raw)?;
                    let (mut notes, next_after, scan_incomplete) = if has_note_filter {
                        let mut collected = Vec::new();
                        let mut raw_after = after;
                        let mut scanned = 0u32;
                        let mut last_scanned = None;
                        let target = (limit as usize).saturating_add(1);

                        let raw_more = loop {
                            if scanned >= MAX_SCAN_TOTAL || collected.len() >= target {
                                break collected.len() >= target;
                            }
                            let scan_limit = MAX_SCAN_TOTAL.saturating_sub(scanned).min(PAGE_SIZE);
                            let (page, next_raw_after) = self
                                .runtime
                                .list_notes_filtered_after(
                                    token,
                                    filter.clone(),
                                    raw_after,
                                    scan_limit,
                                )
                                .await?;
                            if page.is_empty() {
                                break false;
                            }
                            for note in page {
                                scanned = scanned.saturating_add(1);
                                last_scanned = Some(note.id);
                                if note_matches_list_filters(&note, &p) {
                                    collected.push(note);
                                    if collected.len() >= target {
                                        break;
                                    }
                                }
                            }
                            if collected.len() >= target {
                                break true;
                            }
                            match next_raw_after {
                                Some(next) => {
                                    raw_after = Some(next);
                                    if scanned >= MAX_SCAN_TOTAL {
                                        break true;
                                    }
                                }
                                None => break false,
                            }
                        };

                        let has_more_match = collected.len() > limit as usize;
                        let continuation = if has_more_match && limit > 0 {
                            collected.get(limit as usize - 1).map(|note| note.id)
                        } else if raw_more && scanned >= MAX_SCAN_TOTAL {
                            last_scanned
                        } else {
                            None
                        };
                        collected.truncate(limit as usize);
                        (
                            collected,
                            continuation,
                            !has_more_match && raw_more && scanned >= MAX_SCAN_TOTAL,
                        )
                    } else {
                        let (notes, next_after) = self
                            .runtime
                            .list_notes_filtered_after(token, filter.clone(), after, limit)
                            .await?;
                        (notes, next_after, false)
                    };

                    let remapped: Vec<Value> = notes
                        .drain(..)
                        .map(|note| {
                            parse_note_content(
                                to_json(&note)
                                    .map(normalize_entity_timestamps)
                                    .map(remap_note_status)
                                    .unwrap_or_else(|_| serde_json::json!({})),
                                p.parse_content,
                            )
                        })
                        .collect::<Result<_, _>>()?;
                    let mut response = serde_json::json!({
                        "notes": remapped,
                        "next_after": next_after,
                    });
                    if scan_incomplete {
                        response["scan_incomplete"] = Value::Bool(true);
                    }
                    // A continuation boundary means more matches; a scan that
                    // hit its ceiling means more rows were never examined.
                    // Either way the caller has not seen the whole population.
                    add_list_limit_metadata(
                        &mut response,
                        requested,
                        limit,
                        next_after.is_some() || scan_incomplete,
                    );
                    return Ok(response);
                }

                let offset = p.offset.unwrap_or(0);
                let mut scan_incomplete = false;
                let notes: Vec<_> = if has_note_filter {
                    let mut collected: Vec<_> = Vec::new();
                    let mut db_offset: u32 = 0;
                    // One past the page, so a full page is distinguishable
                    // from a complete one. The extra match is dropped below.
                    let target_after_skip = offset as usize + limit as usize + 1;
                    loop {
                        let remaining_scan =
                            MAX_SCAN_TOTAL.saturating_sub(db_offset).min(PAGE_SIZE);
                        if remaining_scan == 0 {
                            scan_incomplete = true;
                            break;
                        }
                        let page = self
                            .runtime
                            .list_notes_filtered(token, filter.clone(), remaining_scan, db_offset)
                            .await?;
                        let fetched = page.len() as u32;
                        for note in page {
                            if note.deleted_at.is_some() {
                                continue;
                            }
                            if note_matches_list_filters(&note, &p) {
                                collected.push(note);
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
                        .list_notes_filtered(token, filter.clone(), overfetch_limit(limit), offset)
                        .await?
                };

                // Computed from the RAW fetch, before soft-deleted rows are
                // dropped below. The unfiltered branch's query returns deleted
                // notes, so a page can come back short of `limit` while the
                // population continues; asking the raw count keeps `has_more`
                // answering "is there another page" rather than "was this page
                // full", and it cannot report complete on a population that
                // is not.
                let has_more = if has_note_filter {
                    notes.len() > offset as usize + limit as usize
                } else {
                    notes.len() > limit as usize
                };

                let remapped: Vec<Value> = if has_note_filter {
                    notes
                        .into_iter()
                        .skip(offset as usize)
                        .take(limit as usize)
                        .map(|n| {
                            parse_note_content(
                                to_json(&n)
                                    .map(normalize_entity_timestamps)
                                    .map(remap_note_status)
                                    .unwrap_or_else(|_| serde_json::json!({})),
                                p.parse_content,
                            )
                        })
                        .collect::<Result<_, _>>()?
                } else {
                    notes
                        .iter()
                        .filter(|n| n.deleted_at.is_none())
                        .take(limit as usize)
                        .map(|n| {
                            parse_note_content(
                                to_json(n)
                                    .map(normalize_entity_timestamps)
                                    .map(remap_note_status)
                                    .unwrap_or_else(|_| serde_json::json!({})),
                                p.parse_content,
                            )
                        })
                        .collect::<Result<_, _>>()?
                };
                let mut response =
                    render_list_response(to_json(&remapped)?, requested, limit, has_more);
                if scan_incomplete {
                    response["scan_incomplete"] = Value::Bool(true);
                }
                Ok(response)
            }
            KindSpec::Proposal => unreachable!("kind=proposal fast-pathed before deser"),
            KindSpec::Event => {
                if p.tags.as_ref().is_some_and(|tags| !tags.is_empty()) {
                    return Err(RuntimeError::InvalidInput(
                        "tags filter is valid only for entity and note lists".into(),
                    ));
                }
                if p.after.is_some() {
                    return Err(RuntimeError::InvalidInput(
                        "after cursor pagination is supported only for entity, note, and edge lists"
                            .into(),
                    ));
                }
                let requested = p.limit.unwrap_or(100);
                let limit = effective_list_limit(requested, EVENT_LIST_CAP);
                let offset = p.offset.unwrap_or(0);
                let (filter, outcome) = event_filter_from_params(&p)?;

                let mut scan_incomplete = false;
                let items = if let Some(wanted_outcome) = outcome {
                    let mut items = Vec::new();
                    let mut skipped = 0u32;
                    let mut raw_offset = 0u32;
                    let scan_ceiling = offset.saturating_add(limit).saturating_mul(20);
                    let want = overfetch_limit(limit);

                    while (items.len() as u32) < want {
                        let remaining = scan_ceiling.saturating_sub(raw_offset);
                        if remaining == 0 {
                            scan_incomplete = true;
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
                            if (items.len() as u32) >= want {
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
                                limit: overfetch_limit(limit),
                                offset: offset.into(),
                            },
                        )
                        .await?;
                    page.items
                };
                let (items, has_more) = split_overfetched(items, limit);
                let mut response = render_list_response(
                    normalize_event_timestamps_array(to_json(&items)?),
                    requested,
                    limit,
                    has_more,
                );
                if scan_incomplete {
                    response["scan_incomplete"] = Value::Bool(true);
                }
                Ok(response)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse_after_cursor;
    use super::{
        add_list_limit_metadata, overfetch_limit, render_list_response, split_overfetched,
    };
    use crate::handlers::common::{event_filter_from_params, ListParams};
    use crate::sql::sql;

    /// The defect this field exists for: at `limit == cap` the three older
    /// disclosure fields are identical for a complete page and a truncated one.
    #[test]
    fn a_full_page_at_the_cap_is_distinguishable_from_a_complete_one() {
        let truncated = render_list_response(serde_json::json!([]), 500, 500, true);
        let complete = render_list_response(serde_json::json!([]), 500, 500, false);

        for page in [&truncated, &complete] {
            assert_eq!(page["requested_limit"], 500);
            assert_eq!(page["effective_limit"], 500);
            assert_eq!(page["limit_clamped"], false);
        }
        assert_ne!(
            truncated, complete,
            "a truncated page and a complete one must not serialize identically"
        );
        assert_eq!(truncated["has_more"], true);
        assert_eq!(complete["has_more"], false);
    }

    /// The two truncation causes are independent: the cap clamp is something a
    /// caller can act on, the population is something it can only page through.
    #[test]
    fn the_cap_clamp_and_the_population_are_reported_separately() {
        let clamped_and_complete = render_list_response(serde_json::json!([]), 600, 500, false);
        assert_eq!(clamped_and_complete["limit_clamped"], true);
        assert_eq!(clamped_and_complete["has_more"], false);

        let unclamped_and_truncated = render_list_response(serde_json::json!([]), 10, 10, true);
        assert_eq!(unclamped_and_truncated["limit_clamped"], false);
        assert_eq!(unclamped_and_truncated["has_more"], true);
    }

    #[test]
    fn the_added_metadata_form_carries_the_same_pair() {
        let mut response = serde_json::json!({"entities": [], "next_after": null});
        add_list_limit_metadata(&mut response, 500, 500, true);
        assert_eq!(response["limit_clamped"], false);
        assert_eq!(response["has_more"], true);
        assert_eq!(response["entities"], serde_json::json!([]));
    }

    #[test]
    fn the_over_fetched_row_is_never_returned_and_is_the_whole_signal() {
        let exactly_full: Vec<u32> = (0..5).collect();
        let (page, has_more) = split_overfetched(exactly_full, 5);
        assert_eq!(page.len(), 5);
        assert!(!has_more, "a page the store could not extend is complete");

        let one_over: Vec<u32> = (0..6).collect();
        let (page, has_more) = split_overfetched(one_over, 5);
        assert_eq!(page, vec![0, 1, 2, 3, 4], "the extra row must not escape");
        assert!(has_more);

        let short: Vec<u32> = (0..2).collect();
        let (page, has_more) = split_overfetched(short, 5);
        assert_eq!(page.len(), 2);
        assert!(!has_more);
    }

    #[test]
    fn the_over_fetch_asks_for_exactly_one_more_and_cannot_overflow() {
        assert_eq!(overfetch_limit(0), 1);
        assert_eq!(overfetch_limit(500), 501);
        assert_eq!(overfetch_limit(u32::MAX), u32::MAX);
    }

    #[test]
    fn after_cursor_rejects_prefix_with_keyset_consequence() {
        let error = parse_after_cursor("deadbeef").expect_err("prefix is not an exact cursor");
        let message = error.to_string();
        assert!(message.contains("can miss or be ambiguous"), "{message}");
        assert!(
            message.contains("exact stable insertion boundary"),
            "{message}"
        );
    }

    #[test]
    fn event_filters_reject_prefixes_with_exact_record_consequence() {
        for (field, value) in [
            ("session_id", serde_json::json!("deadbeef")),
            ("observed", serde_json::json!(["deadbeef"])),
            ("selected", serde_json::json!(["deadbeef"])),
        ] {
            let mut args = serde_json::json!({"kind": "event"});
            args[field] = value;
            let params: ListParams = serde_json::from_value(args).expect("list params");
            let error = event_filter_from_params(&params)
                .expect_err("prefix is not an exact event-filter identifier");
            let message = error.to_string();
            assert!(message.contains(field), "{message}");
            assert!(message.contains("can miss or be ambiguous"), "{message}");
            assert!(message.contains("exact stable record"), "{message}");
        }
    }

    #[tokio::test]
    async fn message_thread_namespace_json_scope_handles_empty_single_and_past_bind_limit() {
        use khive_runtime::{KhiveRuntime, Namespace};
        use khive_storage::types::{SqlStatement, SqlValue};

        let runtime = KhiveRuntime::memory().expect("in-memory runtime");
        let namespace = Namespace::parse("scope-one").expect("valid namespace");
        let token = runtime.authorize(namespace).expect("authorized namespace");
        runtime
            .create_note(
                &token,
                "message",
                None,
                "threaded message",
                None,
                Some(serde_json::json!({
                    "thread_id": "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee"
                })),
                vec![],
            )
            .await
            .expect("create message note");

        let past_bind_limit = (0..33_000)
            .map(|index| format!("scope-{index}"))
            .chain(std::iter::once("scope-one".to_string()))
            .collect::<Vec<_>>();
        for (case, namespaces, expected_rows) in [
            ("empty", Vec::<String>::new(), 0),
            ("single", vec!["scope-one".to_string()], 1),
            ("past SQLite bind limit", past_bind_limit, 1),
        ] {
            let namespaces_json =
                serde_json::to_string(&namespaces).expect("serialize namespace scope");
            let mut reader = runtime.sql().reader().await.expect("SQL reader");
            let rows = reader
                .query_all(SqlStatement {
                    sql: sql!("message_threads_list").to_string(),
                    params: vec![SqlValue::Text(namespaces_json)],
                    label: Some("test.message_threads_list".into()),
                })
                .await
                .expect("message thread scope query");
            assert_eq!(rows.len(), expected_rows, "{case} namespace scope");
        }
    }
}
