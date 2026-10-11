use khive_score::DeterministicScore;
use khive_storage::note::Note;
use khive_storage::types::{Direction, SqlStatement, SqlValue};
use khive_storage::{EdgeRelation, Entity, Event};
use uuid::Uuid;

use crate::error::{RuntimeError, RuntimeResult};
#[cfg(doc)]
use crate::runtime::KhiveRuntime;

/// A note search result with UUID, salience-weighted RRF score, and display text.
#[derive(Clone, Debug)]
pub struct NoteSearchHit {
    pub note_id: Uuid,
    pub score: DeterministicScore,
    pub rank_score_kind: crate::RankScoreKind,
    pub signals: crate::SearchSignals,
    pub source: crate::SearchSource,
    pub title: Option<String>,
    pub snippet: Option<String>,
}

pub(super) fn salience_weighted_rank(
    score: DeterministicScore,
    salience: Option<f64>,
) -> DeterministicScore {
    const SCALE_RAW: i128 = 1_i128 << 32;
    let salience = DeterministicScore::from_f64(salience.unwrap_or(0.5));
    let weight_raw = SCALE_RAW / 2 + i128::from(salience.to_raw()) / 2;
    // Match khive-score's fixed-point multiplication and saturation without
    // converting the derived weight or ranking score back to floating point.
    let weighted_raw = i128::from(score.to_raw()) * weight_raw / SCALE_RAW;
    DeterministicScore::from_raw(weighted_raw.clamp(
        i128::from(DeterministicScore::NEG_INF.to_raw()),
        i128::from(DeterministicScore::MAX.to_raw()),
    ) as i64)
}

/// Result of [`KhiveRuntime::search_notes_outcome`]: the fused hits — text
/// hits alone when the vector arm failed — plus the vector arm's error, if
/// any. Mirrors [`crate::HybridSearchOutcome`] for the note substrate.
#[derive(Clone, Debug)]
pub struct NoteSearchOutcome {
    pub hits: Vec<NoteSearchHit>,
    pub vector_error: Option<String>,
}

/// Re-insert hyphens at canonical UUID positions (8-4-4-4-12) into a
/// hyphen-free hex prefix, so a `LIKE '<pattern>%'` scan against the
/// hyphenated `id` column matches correctly. Prefixes that already
/// contain a hyphen are passed through unchanged. No-op for len <= 8
/// (already correct). Input longer than 32 hex chars is NOT truncated: the
/// extra hex chars are appended past the canonical 12-char final segment
/// with no further hyphen, so the resulting `LIKE` pattern requires literal
/// characters beyond position 36 that no real (36-char) UUID string can
/// ever have — the scan naturally fails closed instead of silently
/// resolving `<valid-32-hex><extra-hex>` to the valid UUID.
pub fn hex_prefix_to_uuid_pattern(prefix: &str) -> String {
    if prefix.contains('-') {
        return prefix.to_string();
    }
    const BOUNDARIES: [usize; 4] = [8, 13, 18, 23]; // post-hyphen-insertion offsets
    let mut out = String::with_capacity(36);
    for c in prefix.chars() {
        if BOUNDARIES.contains(&out.len()) {
            out.push('-');
        }
        out.push(c);
    }
    out
}

/// Return the inclusive lower and exclusive upper bounds for a UUID prefix
/// stored as a canonical lowercase UUID string under SQLite's BINARY collation.
///
/// Compact hexadecimal prefixes and prefixes of the canonical dashed spelling
/// are accepted. The returned bounds are canonicalized to lowercase; malformed
/// dashed spellings and prefixes longer than one UUID fail closed. The upper
/// bound is the shortest lexicographic successor, so a trailing run of `f`
/// digits carries into the preceding digit. `g` is the exclusive sentinel when
/// the prefix is all `f`, because canonical UUID strings contain only `0`-`f`.
pub fn uuid_prefix_bounds(prefix: &str) -> Option<(String, String)> {
    const HYPHEN_POSITIONS: [usize; 4] = [8, 13, 18, 23];

    let compact = if prefix.contains('-') {
        if prefix.len() > 36 {
            return None;
        }
        let mut compact = String::with_capacity(32);
        for (index, byte) in prefix.bytes().enumerate() {
            if HYPHEN_POSITIONS.contains(&index) {
                if byte != b'-' {
                    return None;
                }
            } else if byte.is_ascii_hexdigit() {
                compact.push(char::from(byte.to_ascii_lowercase()));
            } else {
                return None;
            }
        }
        compact
    } else {
        if prefix.is_empty()
            || prefix.len() > 32
            || !prefix.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return None;
        }
        prefix.to_ascii_lowercase()
    };

    if compact.is_empty() || compact.len() > 32 {
        return None;
    }

    let lower = hex_prefix_to_uuid_pattern(&compact);
    let mut successor = compact.into_bytes();
    let mut carried_past_start = true;
    for index in (0..successor.len()).rev() {
        let next = match successor[index] {
            b'0'..=b'8' | b'a'..=b'e' => Some(successor[index] + 1),
            b'9' => Some(b'a'),
            b'f' => None,
            _ => return None,
        };
        if let Some(next) = next {
            successor[index] = next;
            successor.truncate(index + 1);
            carried_past_start = false;
            break;
        }
    }

    let upper = if carried_past_start {
        "g".to_string()
    } else {
        let compact_upper = String::from_utf8(successor).ok()?;
        hex_prefix_to_uuid_pattern(&compact_upper)
    };
    Some((lower, upper))
}

pub(super) fn resolve_prefix_statement(
    table: &str,
    has_deleted_at: bool,
    include_deleted: bool,
    namespaces: Option<&[String]>,
    lower: &str,
    upper: &str,
) -> SqlStatement {
    let namespace_clause = namespaces.map(|namespaces| {
        let placeholders: Vec<String> = (0..namespaces.len())
            .map(|index| format!("?{}", index + 3))
            .collect();
        format!(" AND namespace IN ({})", placeholders.join(", "))
    });
    let deleted_filter = if has_deleted_at && !include_deleted {
        " AND deleted_at IS NULL"
    } else {
        ""
    };
    let mut params = vec![
        SqlValue::Text(lower.to_owned()),
        SqlValue::Text(upper.to_owned()),
    ];
    if let Some(namespaces) = namespaces {
        params.extend(
            namespaces
                .iter()
                .map(|namespace| SqlValue::Text(namespace.clone())),
        );
    }

    SqlStatement {
        sql: format!(
            "SELECT id FROM {table} \
             WHERE id >= ?1 AND id < ?2{namespace_clause}{deleted_filter} ORDER BY id LIMIT 2",
            namespace_clause = namespace_clause.as_deref().unwrap_or("")
        ),
        params,
        label: Some("resolve_prefix".into()),
    }
}

fn text_preview(text: &str, max_chars: usize) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.chars().take(max_chars).collect())
    }
}

/// Symmetric relations (`competes_with`, `composed_with`) are stored with a
/// canonical source (lower UUID wins), so a directed `Out` or `In` query may
/// miss results. When the relations filter is non-empty and contains **only**
/// symmetric relations, override direction to `Both` so callers always see all
/// edges for these relations regardless of storage canonicalization.
pub(super) fn normalize_symmetric_direction(
    direction: Direction,
    relations: Option<&[EdgeRelation]>,
) -> Direction {
    let Some(rels) = relations else {
        return direction;
    };
    if rels.is_empty() {
        return direction;
    }
    let all_symmetric = rels
        .iter()
        .all(|r| matches!(r, EdgeRelation::CompetesWith | EdgeRelation::ComposedWith));
    if all_symmetric {
        Direction::Both
    } else {
        direction
    }
}

/// Stable tie-break rank for [`Direction`] — `Out` before `In` — used to make
/// the both-direction sort/dedup key total over self-loop edges. A self-loop
/// (`source_id == target_id == node_id`) produces two `UNION ALL` rows with
/// the same `(node_id, edge_id)` but opposite directions; without direction in
/// the key, sort-then-dedup collapses them to one and drops the direction
/// parity a separate `Out` call plus a separate `In` call would preserve.
pub(super) fn direction_sort_rank(direction: &Direction) -> u8 {
    match direction {
        Direction::Out => 0,
        Direction::In => 1,
        Direction::Both => 2,
    }
}

pub(super) fn note_title(note: &Note) -> Option<String> {
    note.name
        .clone()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| Some(format!("[{}]", note.kind.as_str())))
}

pub(super) fn note_snippet(note: &Note) -> Option<String> {
    text_preview(&note.content, 200)
}

/// Result of resolving a UUID to its substrate kind.
#[derive(Clone, Debug)]
pub enum Resolved {
    Entity(Entity),
    Note(Note),
    Event(Event),
    /// A record owned by a pack's private tables.
    ///
    /// `pack` identifies the owning pack by name, `kind` is the pack-local
    /// record type (e.g. "domain", "atom"), and `data` is the full record as
    /// a JSON Value. Pack-private records are not valid edge endpoints,
    /// annotates sources, or task context entities.
    PackRecord {
        pack: String,
        kind: String,
        data: serde_json::Value,
    },
}

/// Await every spawned multi-model embed task in `join_set`, returning one
/// vector per model (in model order) on full success.
///
/// `join_set` entries are `(model_index, embed_result)` — the index lets
/// completion order (which is arrival order, not spawn order) be reassembled
/// into the caller's model order. On the first failure (an embed error or a
/// task panic), every remaining handle is aborted and detached so the error
/// return is not gated on a sibling reaching a cancellation point. A sibling
/// already inside synchronous native inference may finish that call in the
/// background. Embed calls are counted when issued, before the provider
/// await, so detached completion cannot change the operation's usage count.
/// Each task owns cloned runtime/provider state and only computes an embedding;
/// storage writes remain in the parent after this drain succeeds.
pub(super) async fn drain_embed_join_set<T: Send + 'static>(
    mut join_set: tokio::task::JoinSet<(usize, RuntimeResult<T>)>,
    model_count: usize,
) -> RuntimeResult<Vec<T>> {
    let mut vectors: Vec<Option<T>> = (0..model_count).map(|_| None).collect();

    while let Some(joined) = join_set.join_next().await {
        match joined {
            Ok((idx, Ok(vector))) => vectors[idx] = Some(vector),
            Ok((_idx, Err(e))) => {
                join_set.abort_all();
                return Err(e);
            }
            Err(join_err) => {
                join_set.abort_all();
                return Err(RuntimeError::Internal(format!(
                    "embed task panicked: {join_err}"
                )));
            }
        }
    }

    Ok(vectors
        .into_iter()
        .map(|v| v.expect("every model index observed exactly once by join_set drain"))
        .collect())
}
