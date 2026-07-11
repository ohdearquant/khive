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
/// candidates (F7 of the unified-verb draft ADR) — `Ambiguous` always lists
/// what it found instead of guessing.
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
/// it, the sole candidate is still surfaced, just as `Ambiguous` (never
/// silently accepted, never silently dropped — see F7). Ring-stage only:
/// ring scores are fixed constants on this same 0..1 scale
/// (`RING_EXACT_CONFIDENCE` / `RING_SUBSTRING_CONFIDENCE`), so comparing them
/// against a fixed bar is meaningful. The search stage below uses its own,
/// deliberately separate rule — see `SEARCH_RESOLVED_CONFIDENCE`.
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
/// Hybrid-search hits below this score never enter the candidate set at all.
const SEARCH_SCORE_FLOOR: f64 = 0.0;
/// Confidence reported on a search-stage `Resolved` outcome. NOT the raw RRF
/// score — RRF is `sum 1/(k + rank)` (`khive-fusion::rrf`), so a rank-1
/// single-source hit scores ~1/61 (~0.0164) and a strong two-source hit
/// ~2/61 (~0.0328): comparing that against a 0..1 "confidence" bar the way
/// the ring stage does would make the search fallback functionally never
/// resolve (the bug this constant fixes). Instead, a lone hybrid-search
/// candidate (nothing to be ambiguous against) or a decisive-margin winner
/// (see `SEARCH_MARGIN_RATIO`) resolves at this fixed, documented
/// search-stage confidence — deliberately below both ring bands, so callers
/// can distinguish "the ring recognized this" from "search picked this out"
/// by confidence alone. The raw RRF value is never discarded: it is exactly
/// what `ReferenceCandidate.score` carries in `Ambiguous` listings.
const SEARCH_RESOLVED_CONFIDENCE: f64 = 0.6;

/// Resolve one natural-language reference for `token`'s actor.
///
/// `limit` bounds the hybrid-search fallback candidate count (Layer-0 stage
/// 3); it has no effect on the id-string or ring stages, which are always
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
    let trimmed = nl_ref.trim();
    if trimmed.is_empty() {
        return Ok(ReferenceResolution::NotFound);
    }

    // Stage 1: id-string passthrough (UUID / 8+ hex prefix) — the existing
    // by-ID path, never reimplemented. A ref shaped like an id but absent
    // from storage is NotFound, not a fall-through to ring/search: the
    // caller named a specific id, so a miss there is the true answer.
    //
    // Scope: entity ids only, both branches identically (review finding,
    // 2026-07-09 fix round). `resolve_by_id` alone (entity + note) and
    // `resolve_prefix_unfiltered` alone (entity + note + event + edge) used
    // to disagree — a graph edge's full UUID returned `NotFound` while its
    // short prefix resolved. The ring is entity-only by the same S1 contract
    // (`reference_ring::substrate_admits_as_entity`), so id-string
    // passthrough is narrowed to match: an edge or event id-string is
    // `NotFound` here regardless of whether it arrived full or as a prefix.
    // A caller that needs to resolve a non-entity id already has `get`.
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
    if let Some(resolution) = exact_name_match(runtime, token, trimmed, entity_kind).await? {
        return Ok(resolution);
    }

    // Stage 4: hybrid-search fallback over the namespace.
    let hits = runtime
        .hybrid_search(
            token,
            trimmed,
            None,
            limit.max(1),
            entity_kind,
            None,
            &[],
            None,
        )
        .await?;
    let candidates: Vec<ReferenceCandidate> = hits
        .into_iter()
        .filter(|h| h.score.to_f64() > SEARCH_SCORE_FLOOR)
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
) -> RuntimeResult<Option<ReferenceResolution>> {
    let filter = EntityFilter {
        name_exact: Some(name.to_string()),
        kinds: entity_kind.map(|k| vec![k.to_string()]).unwrap_or_default(),
        ..EntityFilter::default()
    };
    // A storage-level `name = ?` predicate (not `name_prefix` + in-memory
    // filter) so a namespace with many newer case variants of `name` can
    // never page the exact target out from under a `created_at DESC` sort
    // (#849, #852) — every row this query returns already equals `name`.
    // The page is small (10, not 1): a single hit still needs `Resolved`,
    // but multiple hits must surface every candidate for `Ambiguous`, the
    // same way the ring and search stages report their candidate sets.
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

    let exact: Vec<ReferenceCandidate> = page
        .items
        .into_iter()
        .map(|e| ReferenceCandidate {
            id: e.id,
            name: Some(e.name),
            score: EXACT_NAME_CONFIDENCE,
        })
        .collect();

    Ok(match exact.len() {
        0 => None,
        1 => Some(ReferenceResolution::Resolved {
            id: exact[0].id,
            confidence: EXACT_NAME_CONFIDENCE,
        }),
        _ => Some(ReferenceResolution::Ambiguous { candidates: exact }),
    })
}

fn is_hex_prefix(s: &str) -> bool {
    s.len() >= 8 && s.chars().all(|c| c.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::NamespaceToken as TokenCtor;
    use khive_gate::ActorRef;
    use khive_types::namespace::Namespace;

    fn actor_token(actor_id: &str) -> NamespaceToken {
        TokenCtor::mint_authorized(Namespace::local(), ActorRef::new("agent", actor_id))
    }

    #[tokio::test]
    async fn id_string_passthrough_resolves_full_uuid() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let token = actor_token("resolver-test");
        let ring = ReferenceRing::new();

        let entity = rt
            .create_entity(
                &token,
                "concept",
                None,
                "PassthroughTarget",
                None,
                None,
                vec![],
            )
            .await
            .expect("create entity");

        let resolution = resolve_reference(&rt, &ring, &token, &entity.id.to_string(), 5, None)
            .await
            .expect("resolve_reference");
        assert_eq!(
            resolution,
            ReferenceResolution::Resolved {
                id: entity.id,
                confidence: 1.0
            }
        );
    }

    #[tokio::test]
    async fn id_string_passthrough_never_errors_on_a_miss() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let token = actor_token("resolver-test");
        let ring = ReferenceRing::new();

        let missing = Uuid::new_v4();
        let resolution = resolve_reference(&rt, &ring, &token, &missing.to_string(), 5, None)
            .await
            .expect("must not error, only report NotFound");
        assert_eq!(resolution, ReferenceResolution::NotFound);
    }

    #[tokio::test]
    async fn ring_exact_match_resolves_without_search() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let token = actor_token("resolver-test");
        let ring = ReferenceRing::new();
        let actor = token.actor();
        let actor_key = format!("{}:{}", actor.kind, actor.id);

        let id = Uuid::new_v4();
        ring.admit(
            token.namespace().as_str(),
            &actor_key,
            id,
            Some("the old record".to_string()),
        );

        let resolution = resolve_reference(&rt, &ring, &token, "the old record", 5, None)
            .await
            .expect("resolve_reference");
        assert_eq!(
            resolution,
            ReferenceResolution::Resolved {
                id,
                confidence: RING_EXACT_CONFIDENCE
            }
        );
    }

    #[tokio::test]
    async fn ring_ambiguous_on_multiple_exact_matches() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let token = actor_token("resolver-test");
        let ring = ReferenceRing::new();
        let actor = token.actor();
        let actor_key = format!("{}:{}", actor.kind, actor.id);

        let id_a = Uuid::new_v4();
        let id_b = Uuid::new_v4();
        ring.admit(
            token.namespace().as_str(),
            &actor_key,
            id_a,
            Some("duplicate name".to_string()),
        );
        ring.admit(
            token.namespace().as_str(),
            &actor_key,
            id_b,
            Some("duplicate name".to_string()),
        );

        let resolution = resolve_reference(&rt, &ring, &token, "duplicate name", 5, None)
            .await
            .expect("resolve_reference");
        match resolution {
            ReferenceResolution::Ambiguous { candidates } => {
                assert_eq!(candidates.len(), 2);
            }
            other => panic!("expected Ambiguous, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn no_ring_entry_and_no_search_hit_is_not_found() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let token = actor_token("resolver-test");
        let ring = ReferenceRing::new();

        let resolution =
            resolve_reference(&rt, &ring, &token, "nothing matches this at all", 5, None)
                .await
                .expect("resolve_reference");
        assert_eq!(resolution, ReferenceResolution::NotFound);
    }

    #[tokio::test]
    async fn actor_isolation_blocks_cross_actor_ring_reads() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let token_a = actor_token("actor-a");
        let token_b = actor_token("actor-b");
        let ring = ReferenceRing::new();
        let actor_a = token_a.actor();
        let actor_key_a = format!("{}:{}", actor_a.kind, actor_a.id);

        let id = Uuid::new_v4();
        ring.admit(
            token_a.namespace().as_str(),
            &actor_key_a,
            id,
            Some("shared-namespace-name".to_string()),
        );

        // actor-b, same namespace, must NOT resolve via actor-a's ring entry.
        let resolution = resolve_reference(&rt, &ring, &token_b, "shared-namespace-name", 5, None)
            .await
            .expect("resolve_reference");
        assert_eq!(resolution, ReferenceResolution::NotFound);
    }

    // Regression for #849/#852: the stage-3 exact-name lookup used to filter
    // `name_prefix` (`LIKE 'RoLoRA%'`) in memory, and `query_entities` ranks
    // a `name_prefix` page by `CASE WHEN LOWER(name) = prefix THEN 0 ELSE 1
    // END, created_at DESC`. Case-insensitive variants of the target name
    // tie for priority 0 with the true exact match, so 100+ *newer*
    // lowercase variants can fill the `LIMIT 100` page and page the older,
    // case-exact target out entirely — the stage then falls through to
    // hybrid search instead of resolving deterministically. The fix issues a
    // storage-level `name = ?` (binary) predicate instead, so decoys that
    // merely match case-insensitively never enter the result set at all.
    #[tokio::test]
    async fn exact_name_stage_survives_many_newer_case_variant_decoys() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let token = actor_token("resolver-test");
        let ring = ReferenceRing::new();

        let target = rt
            .create_entity(&token, "concept", None, "RoLoRA", None, None, vec![])
            .await
            .expect("create target entity");

        // Case variants of the same name, not suffixed variants: SQLite's
        // `LIKE` is case-insensitive for ASCII, so a `rolora`-named decoy
        // still matches the `LIKE 'RoLoRA%'` pattern the buggy `name_prefix`
        // stage used, and the exact-match-ranking `CASE WHEN LOWER(name) =
        // ...` ties every one of these decoys with the true target at
        // priority 0 — leaving `created_at DESC` as the only tiebreak.
        let decoy_cases = ["rolora", "ROLORA", "RoLoRa", "roLORA"];
        for i in 0..120 {
            rt.create_entity(
                &token,
                "concept",
                None,
                decoy_cases[i % decoy_cases.len()],
                None,
                None,
                vec![],
            )
            .await
            .expect("create decoy entity");
        }

        let resolution = resolve_reference(&rt, &ring, &token, "RoLoRA", 5, None)
            .await
            .expect("resolve_reference");
        assert_eq!(
            resolution,
            ReferenceResolution::Resolved {
                id: target.id,
                confidence: EXACT_NAME_CONFIDENCE,
            }
        );
    }
}
