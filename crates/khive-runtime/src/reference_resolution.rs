//! `resolve_reference`: the Layer-0 deterministic reference resolver from the
//! "unified-verb" draft ADR (Slice 1 — resolver + ring).
//!
//! Turns a natural-language reference into an id through four ordered
//! stages, never guessing among close candidates:
//!
//! 1. **Id-string passthrough.** A ref that already looks like a UUID or an
//!    8+ hex-char prefix resolves through the existing by-ID path
//!    (`KhiveRuntime::resolve_by_id` / `resolve_prefix_unfiltered`) instead of
//!    being treated as free text — it must not error just because it arrived
//!    through `resolve_reference` rather than `get`. Scoped to entity ids
//!    only, identically for the full-UUID and prefix forms (matching the
//!    ring's entity-only contract); a note/edge/event id-string is
//!    `NotFound` here, not an error — a caller resolving those uses `get`.
//! 2. **Recently-referenced ring.** An exact (case-insensitive) or substring
//!    match against this actor's ring (`reference_ring::ReferenceRing`).
//! 3. **Exact-name storage lookup.** A deterministic, case-sensitive match
//!    against `entities.name` in the caller's namespace (`deleted_at IS
//!    NULL`) — covers any entity that already exists but was never
//!    created/get/updated/deleted/merged/linked by this actor in this
//!    session, so stage 2's ring never saw it (#849).
//! 4. **Hybrid-search fallback.** `KhiveRuntime::hybrid_search` over the
//!    caller's namespace, ranked by RRF score.
//!
//! A single candidate clearing the stage's confidence bar resolves; multiple
//! viable candidates or none never silently pick — they return `Ambiguous`
//! or `NotFound` for the caller to disambiguate.

use std::str::FromStr;

use uuid::Uuid;

use khive_storage::types::PageRequest;
use khive_storage::EntityFilter;

use crate::error::{RuntimeError, RuntimeResult};
use crate::operations::Resolved;
use crate::reference_ring::ReferenceRing;
use crate::runtime::{KhiveRuntime, NamespaceToken};

/// A candidate id surfaced when a reference did not resolve outright.
#[derive(Clone, Debug, PartialEq)]
pub struct ReferenceCandidate {
    pub id: Uuid,
    pub name: Option<String>,
    pub score: f64,
}

/// Outcome of `resolve_reference`. Never a silent pick among close
/// candidates: `Ambiguous` always lists what it found instead of guessing.
#[derive(Clone, Debug, PartialEq)]
pub enum ReferenceResolution {
    Resolved { id: Uuid, confidence: f64 },
    Ambiguous { candidates: Vec<ReferenceCandidate> },
    NotFound,
}

/// Ring-match confidence for an exact (case-insensitive) name match.
const RING_EXACT_CONFIDENCE: f64 = 0.95;
/// Ring-match confidence for a substring match (either direction).
const RING_SUBSTRING_CONFIDENCE: f64 = 0.7;
/// A single ring candidate auto-resolves only at or above this bar; below
/// it the candidate is still surfaced as `Ambiguous` rather than silently
/// accepted or dropped. Ring scores are fixed constants on a 0..1 scale, so
/// a fixed bar is meaningful here: the search stage below needs a
/// different rule (`SEARCH_RESOLVED_CONFIDENCE`) because RRF scores aren't
/// on that scale.
const RING_AUTO_RESOLVE_CONFIDENCE: f64 = 0.7;
/// Confidence for a stage-3 exact-name storage match: a deterministic,
/// case-sensitive equality on `entities.name` — stronger evidence than the
/// ring's case-insensitive session cache (`RING_EXACT_CONFIDENCE`), so it
/// sits above both ring bands, but still below the absolute certainty of an
/// id-string passthrough (1.0), which the caller supplied directly rather
/// than by name.
const EXACT_NAME_CONFIDENCE: f64 = 0.98;
/// Hybrid-search fallback: the top hit auto-resolves over a runner-up only
/// when it leads by at least this ratio — RRF scores are not on a fixed
/// 0..1 confidence scale, so a fixed absolute bar can't express "decisively
/// best" the way it can for the ring. Below the margin, every hit above the
/// score floor is surfaced as a candidate instead.
const SEARCH_MARGIN_RATIO: f64 = 2.0;
/// Vector hits below this raw cosine-similarity score are removed before RRF
/// fusion. Lexical hits remain admissible because RRF magnitude encodes rank,
/// not textual relevance, and genuine partial-name matches score below this floor.
///
/// This is a raw cosine value in `[-1.0, 1.0]`, matching the canonical
/// vector-store score produced by `khive-db` (`1 - cosine_distance`).
const SEARCH_VECTOR_SIMILARITY_FLOOR: f64 = 0.3;
/// Confidence reported on a search-stage `Resolved` outcome: not the raw
/// RRF score, which lives on a much smaller scale (`sum 1/(k + rank)`, e.g.
/// ~0.016-0.033) and would never clear a 0..1 confidence bar. Fixed below
/// both ring bands so callers can tell "the ring recognized this" from
/// "search picked this out" by confidence alone; the raw RRF value is still
/// preserved in `ReferenceCandidate.score` for `Ambiguous` listings.
const SEARCH_RESOLVED_CONFIDENCE: f64 = 0.6;
/// Floor on the retrieval depth stage 4 asks `hybrid_search` for, independent
/// of the caller's requested `limit`.
///
/// `KhiveRuntime::hybrid_search` truncates its returned hit list to exactly
/// the `limit` it is called with (its final step is `fused.truncate(limit as
/// usize)`), so that `limit` doubles as both "how deep to search" and "how
/// many hits to hand back" — for `resolve`, whose caller-facing default
/// `limit` is 5 (`khive_pack_kg::handlers::resolve::DEFAULT_LIMIT`), a
/// caller asking for a short candidate list was silently also asking for a
/// shallow search. A genuine canonical-name match ranked, say, 6th-to-10th —
/// exactly where a qualified natural-language ref (a project prefix ahead of
/// a short canonical name) lands when it beats the exact-name stage above
/// but only partially matches the text leg and is corroborated by the vector
/// leg — was truncated out of the stage-4 candidate set entirely even though
/// the same query against a wider `limit` (e.g. the `search` verb's own
/// default of 10) surfaces it as the top hit (#908). Retrieving at this
/// floor decouples "how deep to search" from "how many candidates the caller
/// asked for". The full pool is retained for the decisiveness check, while an
/// `Ambiguous` payload is truncated back to the caller's `limit` before it is
/// returned.
const STAGE4_MIN_SEARCH_LIMIT: u32 = 20;

/// Resolve one natural-language reference for `token`'s actor.
///
/// `limit` bounds the hybrid-search fallback candidate count (Layer-0 stage
/// 4); it has no effect on the id-string or ring stages, which are always
/// exact-or-nothing / small in-memory scans. `entity_kind`, if set, restricts
/// stage 3 to that entity kind (e.g. `"concept"`); the id-string and ring
/// stages are kind-agnostic by construction (a ring entry or an explicit id
/// is not filtered by kind).
pub async fn resolve_reference(
    runtime: &KhiveRuntime,
    ring: &ReferenceRing,
    token: &NamespaceToken,
    nl_ref: &str,
    limit: u32,
    entity_kind: Option<&str>,
) -> RuntimeResult<ReferenceResolution> {
    resolve_reference_with_entity_type(runtime, ring, token, nl_ref, limit, entity_kind, None).await
}

/// Resolve with an optional canonical entity subtype in the exact-name and
/// search stages. ID and ring matches retain their existing kind-agnostic
/// behavior, so a subtype filter cannot turn an explicit ID into a fuzzy hit.
#[allow(clippy::too_many_arguments)]
pub async fn resolve_reference_with_entity_type(
    runtime: &KhiveRuntime,
    ring: &ReferenceRing,
    token: &NamespaceToken,
    nl_ref: &str,
    limit: u32,
    entity_kind: Option<&str>,
    entity_type: Option<&str>,
) -> RuntimeResult<ReferenceResolution> {
    let trimmed = nl_ref.trim();
    if trimmed.is_empty() {
        return Ok(ReferenceResolution::NotFound);
    }

    // Stage 1: id-string passthrough (UUID / 8+ hex prefix) via the existing
    // by-ID path. A ref shaped like an id but absent from storage is
    // NotFound, not a fallthrough to ring/search: the caller named a
    // specific id, so a miss there is the true answer. Scoped to entity ids
    // only (both full-UUID and prefix forms) to match the ring's entity-only
    // contract (`reference_ring::substrate_admits_as_entity`); a non-entity
    // id-string is `NotFound` here: callers needing those already have `get`.
    if let Ok(uuid) = Uuid::from_str(trimmed) {
        return match runtime.resolve_by_id(token, uuid).await? {
            Some(Resolved::Entity(_)) => Ok(ReferenceResolution::Resolved {
                id: uuid,
                confidence: 1.0,
            }),
            Some(_) | None => Ok(ReferenceResolution::NotFound),
        };
    }
    if is_hex_prefix(trimmed) {
        return match runtime.resolve_prefix_unfiltered(trimmed).await {
            Ok(Some(uuid)) => match runtime.resolve_by_id(token, uuid).await? {
                Some(Resolved::Entity(_)) => Ok(ReferenceResolution::Resolved {
                    id: uuid,
                    confidence: 1.0,
                }),
                Some(_) | None => Ok(ReferenceResolution::NotFound),
            },
            Ok(None) => Ok(ReferenceResolution::NotFound),
            Err(RuntimeError::AmbiguousPrefix { matches, .. }) => {
                let mut entity_matches = Vec::with_capacity(matches.len());
                for id in matches {
                    if matches!(
                        runtime.resolve_by_id(token, id).await?,
                        Some(Resolved::Entity(_))
                    ) {
                        entity_matches.push(id);
                    }
                }
                match entity_matches.len() {
                    0 => Ok(ReferenceResolution::NotFound),
                    1 => Ok(ReferenceResolution::Resolved {
                        id: entity_matches[0],
                        confidence: 1.0,
                    }),
                    _ => Ok(ReferenceResolution::Ambiguous {
                        candidates: entity_matches
                            .into_iter()
                            .map(|id| ReferenceCandidate {
                                id,
                                name: None,
                                score: 1.0,
                            })
                            .collect(),
                    }),
                }
            }
            Err(e) => Err(e),
        };
    }

    // Stage 2: recently-referenced ring.
    let actor = token.actor();
    let actor_key = format!("{}:{}", actor.kind, actor.id);
    let ring_entries = ring.snapshot(token.namespace().as_str(), &actor_key);
    let needle = trimmed.to_ascii_lowercase();

    let exact: Vec<ReferenceCandidate> = ring_entries
        .iter()
        .filter(|e| {
            e.name
                .as_deref()
                .is_some_and(|n| n.to_ascii_lowercase() == needle)
        })
        .map(|e| ReferenceCandidate {
            id: e.id,
            name: e.name.clone(),
            score: RING_EXACT_CONFIDENCE,
        })
        .collect();
    if let Some(resolution) = resolve_from_candidates(exact) {
        return Ok(resolution);
    }

    let substring: Vec<ReferenceCandidate> = ring_entries
        .iter()
        .filter(|e| {
            e.name.as_deref().is_some_and(|n| {
                let n_lower = n.to_ascii_lowercase();
                n_lower.contains(&needle) || needle.contains(&n_lower)
            })
        })
        .map(|e| ReferenceCandidate {
            id: e.id,
            name: e.name.clone(),
            score: RING_SUBSTRING_CONFIDENCE,
        })
        .collect();
    if let Some(resolution) = resolve_from_candidates(substring) {
        return Ok(resolution);
    }

    // Stage 3: exact-name storage lookup (#849) — a deterministic,
    // case-sensitive match against `entities.name` in the caller's
    // namespace, run before the hybrid-search fallback so an existing exact
    // name always resolves regardless of FTS ranking, RRF score, or whether
    // this actor's session ever referenced the entity (the ring's blind
    // spot). Single match resolves; multiple exact matches are `Ambiguous`;
    // none falls through to hybrid search unchanged.
    if let Some(resolution) =
        exact_name_match(runtime, token, trimmed, entity_kind, entity_type).await?
    {
        return Ok(resolution);
    }

    // Stage 4: hybrid-search fallback over the namespace. Search deeper than
    // the caller's requested `limit` (see `STAGE4_MIN_SEARCH_LIMIT`) so a
    // genuine match ranked just outside a small `limit` isn't truncated out
    // of the pool before it's even ranked against the alternatives; the
    // caller's `limit` still bounds how many candidates get rendered below.
    let candidate_limit = limit.max(1);
    let search_limit = candidate_limit.max(STAGE4_MIN_SEARCH_LIMIT);
    let hits = runtime
        .hybrid_search_with_vector_similarity_floor(
            token,
            trimmed,
            None,
            search_limit,
            entity_kind,
            entity_type,
            &[],
            None,
            SEARCH_VECTOR_SIMILARITY_FLOOR,
        )
        .await?;
    let candidates: Vec<ReferenceCandidate> = hits
        .into_iter()
        .map(|h| ReferenceCandidate {
            id: h.entity_id,
            name: h.title,
            score: h.score.to_f64(),
        })
        .collect();

    match candidates.len() {
        0 => Ok(ReferenceResolution::NotFound),
        // A lone hit is presence-decisive: there is no competing candidate to
        // be ambiguous against, regardless of its raw RRF magnitude (see
        // `SEARCH_RESOLVED_CONFIDENCE`).
        1 => Ok(ReferenceResolution::Resolved {
            id: candidates[0].id,
            confidence: SEARCH_RESOLVED_CONFIDENCE,
        }),
        _ => {
            let top_score = candidates[0].score;
            let second_score = candidates[1].score;
            let decisive =
                second_score <= f64::EPSILON || top_score / second_score >= SEARCH_MARGIN_RATIO;
            if decisive {
                Ok(ReferenceResolution::Resolved {
                    id: candidates[0].id,
                    confidence: SEARCH_RESOLVED_CONFIDENCE,
                })
            } else {
                // Non-exact ambiguity: bound the payload to the caller's
                // `limit`. Any deterministic identity (an exact canonical
                // name) was already resolved by stage 3 above, so nothing
                // withheld here is "the" answer — outside exact matches there
                // is no oracle for a single canonical candidate. Raising
                // `limit` surfaces deeper ranks (#970; resolve() contract).
                let mut candidates = candidates;
                candidates.truncate(candidate_limit as usize);
                Ok(ReferenceResolution::Ambiguous { candidates })
            }
        }
    }
}

/// Apply the shared "single-above-bar resolves, multiple is ambiguous"
/// contract to a candidate set already known to be an exact or substring
/// ring match. Returns `None` when `candidates` is empty — the caller falls
/// through to the next resolution stage instead of reporting `NotFound`
/// prematurely.
fn resolve_from_candidates(candidates: Vec<ReferenceCandidate>) -> Option<ReferenceResolution> {
    match candidates.len() {
        0 => None,
        1 => {
            let top = &candidates[0];
            Some(if top.score >= RING_AUTO_RESOLVE_CONFIDENCE {
                ReferenceResolution::Resolved {
                    id: top.id,
                    confidence: top.score,
                }
            } else {
                ReferenceResolution::Ambiguous { candidates }
            })
        }
        _ => Some(ReferenceResolution::Ambiguous { candidates }),
    }
}

/// Stage 3 of `resolve_reference` (#849): a deterministic, case-sensitive
/// exact match against `entities.name`, scoped to `token.namespace()` (the
/// same single-namespace default the rest of this pipeline and the sibling
/// by-name lookup in `khive-pack-kg`'s `resolve_name_async` use) and to
/// `entity_kind` when the caller filtered by one. `query_entities` already
/// excludes soft-deleted rows (`deleted_at IS NULL` is baked into every
/// query — see `khive-db::stores::entity::build_entity_where`), so no
/// separate filter is needed here. Returns `None` (fall through to the next
/// stage) when nothing matches; `Some(Resolved)` on a single hit; and
/// `Some(Ambiguous)` when the name is not unique.
async fn exact_name_match(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    name: &str,
    entity_kind: Option<&str>,
    entity_type: Option<&str>,
) -> RuntimeResult<Option<ReferenceResolution>> {
    let mut filter = EntityFilter {
        name_exact: Some(name.to_string()),
        kinds: entity_kind.map(|k| vec![k.to_string()]).unwrap_or_default(),
        ..EntityFilter::default()
    };
    if let (Some(kind), Some(entity_type)) = (entity_kind, entity_type) {
        filter
            .entity_types_by_kind
            .insert(kind.to_string(), vec![entity_type.to_string()]);
    }
    // A storage-level `name = ?` predicate (not `name_prefix` + in-memory
    // filter) so a namespace with many newer case variants of `name` can
    // never page the exact target out from under a `created_at DESC` sort
    // (#849, #852) — every row this query returns already equals `name`.
    // The page is small (10, not 1), but the zero/one/many decision is made
    // from `page.total` (the storage-computed COUNT(*) under the same
    // predicate), never from `page.items.len()` — with 11+ byte-identical
    // exact names the fetched page is still only 10 rows, and deciding from
    // its length alone would under-report cardinality. `Ambiguous.candidates`
    // is a bounded sample of up to 10 of the `page.total` matches, not the
    // complete set — the variant carries no total field, so callers must not
    // assume `candidates.len() == page.total` (#852).
    let page = runtime
        .entities(token)?
        .query_entities(
            token.namespace().as_str(),
            filter,
            PageRequest {
                offset: 0,
                limit: 10,
            },
        )
        .await
        .map_err(RuntimeError::Storage)?;

    let total = page.total.unwrap_or(page.items.len() as u64);

    let exact: Vec<ReferenceCandidate> = page
        .items
        .into_iter()
        .map(|e| ReferenceCandidate {
            id: e.id,
            name: Some(e.name),
            score: EXACT_NAME_CONFIDENCE,
        })
        .collect();

    Ok(match total {
        0 => None,
        1 => exact
            .into_iter()
            .next()
            .map(|top| ReferenceResolution::Resolved {
                id: top.id,
                confidence: EXACT_NAME_CONFIDENCE,
            }),
        _ => Some(ReferenceResolution::Ambiguous { candidates: exact }),
    })
}

fn is_hex_prefix(s: &str) -> bool {
    s.len() >= 8 && s.chars().all(|c| c.is_ascii_hexdigit())
}

#[cfg(test)]
#[path = "reference_resolution_tests.rs"]
mod tests;
