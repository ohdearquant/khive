//! Keyed note pagination is distinct from the ordinary insertion-sequence walk.

use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};
use khive_storage::note::{NoteFilter, NoteKeyCursor};
use khive_storage::PageRequest;
use serde_json::Value;

use super::common::{normalize_entity_timestamps, remap_note_status, to_json, ListParams};
use super::list::{add_list_limit_metadata, note_matches_list_filters, render_list_response};

pub(super) fn note_filter(p: &ListParams, kind: Option<&str>) -> Result<NoteFilter, RuntimeError> {
    fn timestamp(value: Option<&str>, name: &str) -> Result<Option<i64>, RuntimeError> {
        value
            .map(|value| {
                chrono::DateTime::parse_from_rfc3339(value)
                    .map(|time| time.timestamp_micros())
                    .map_err(|error| {
                        RuntimeError::InvalidInput(format!("{name} must be RFC 3339: {error}"))
                    })
            })
            .transpose()
    }
    Ok(NoteFilter {
        kind: kind.map(str::to_owned),
        min_created_at: timestamp(p.created_after.as_deref(), "created_after")?,
        min_updated_at: timestamp(p.updated_after.as_deref(), "updated_after")?,
        tags: p.tags.clone().unwrap_or_default(),
        tag_mode: p.tag_mode.unwrap_or_default(),
        ..Default::default()
    })
}

fn encode_cursor(cursor: &NoteKeyCursor) -> Result<String, RuntimeError> {
    Ok(format!(
        "nk1:{}",
        serde_json::to_string(cursor).map_err(|error| RuntimeError::Internal(error.to_string()))?
    ))
}

fn decode_cursor(raw: &str) -> Result<Option<NoteKeyCursor>, RuntimeError> {
    if raw.is_empty() {
        return Ok(None);
    }
    let invalid =
        || RuntimeError::InvalidInput("after must be a keyed note cursor returned by list".into());
    let cursor: NoteKeyCursor = serde_json::from_str(raw.strip_prefix("nk1:").ok_or_else(invalid)?)
        .map_err(|_| invalid())?;
    if cursor.key.len() > 512 || cursor.key.contains('\0') {
        return Err(invalid());
    }
    Ok(Some(cursor))
}

pub(super) async fn list_keyed_notes(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    p: &ListParams,
    filter: &NoteFilter,
    requested: u32,
    limit: u32,
) -> Result<Value, RuntimeError> {
    let prefix = p.key_prefix.as_deref().expect("keyed list has a prefix");
    if prefix.contains('\0') {
        return Err(RuntimeError::InvalidInput(
            "key_prefix cannot contain U+0000".into(),
        ));
    }
    if p.after_key.is_some() && (p.after.is_some() || p.offset.is_some()) {
        return Err(RuntimeError::InvalidInput(
            "after_key excludes after and offset".into(),
        ));
    }
    if limit == 0 {
        if p.offset.is_some() {
            return Ok(render_list_response(
                serde_json::json!([]),
                requested,
                limit,
            ));
        }
        return Err(RuntimeError::InvalidInput(
            "keyed cursor pagination requires a positive limit".into(),
        ));
    }
    let mut boundary = match p.after_key.as_deref() {
        Some(key) => Some(NoteKeyCursor::from(
            &runtime
                .get_note_by_key(token, key, filter.kind.as_deref(), true)
                .await?,
        )),
        None => p.after.as_deref().map(decode_cursor).transpose()?.flatten(),
    };
    let mut collected = Vec::new();
    let mut skip = p.offset.unwrap_or_default();
    let mut scanned = 0u32;
    let mut last_scanned = None;
    const MAX_SCAN: u32 = 10_000;
    let raw_more = loop {
        let scan_limit = (MAX_SCAN - scanned).min(200);
        let (page, next) = runtime
            .notes(token)?
            .query_keyed_notes(
                token.namespace().as_str(),
                filter,
                prefix,
                boundary.as_ref(),
                PageRequest {
                    limit: scan_limit,
                    offset: 0,
                },
            )
            .await?;
        if page.is_empty() {
            break false;
        }
        for note in page {
            scanned += 1;
            last_scanned = Some(NoteKeyCursor::from(&note));
            if note_matches_list_filters(&note, p) {
                if skip > 0 {
                    skip -= 1;
                } else {
                    collected.push(note);
                }
                if collected.len() > limit as usize {
                    break;
                }
            }
        }
        if collected.len() > limit as usize {
            break true;
        }
        let Some(next) = next else {
            break false;
        };
        if scanned >= MAX_SCAN {
            break true;
        }
        boundary = Some(next);
    };
    let more_matches = collected.len() > limit as usize;
    collected.truncate(limit as usize);
    let incomplete = raw_more && !more_matches && scanned >= MAX_SCAN;
    let next = if more_matches {
        collected.last().map(NoteKeyCursor::from)
    } else if incomplete {
        last_scanned
    } else {
        None
    };
    let notes = collected
        .iter()
        .map(|note| {
            to_json(note)
                .map(normalize_entity_timestamps)
                .map(remap_note_status)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut response = if p.offset.is_some() {
        render_list_response(to_json(&notes)?, requested, limit)
    } else {
        serde_json::json!({"notes": notes, "next_after": next.as_ref().map(encode_cursor).transpose()?})
    };
    add_list_limit_metadata(&mut response, requested, limit);
    if incomplete {
        response["scan_incomplete"] = Value::Bool(true);
    }
    Ok(response)
}
