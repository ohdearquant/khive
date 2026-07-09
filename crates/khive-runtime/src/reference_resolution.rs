//! `resolve_reference`: the Layer-0 deterministic reference resolver from the
//! "unified-verb" draft ADR (Slice 1 — resolver + ring).
//!
//! Turns a natural-language reference into an id through three ordered
//! stages, never guessing among close candidates:
//!
//! 1. **Id-string passthrough.** A ref that already looks like a UUID or an
//!    8+ hex-char prefix resolves through the existing by-ID path
//!    (`KhiveRuntime::resolve_by_id` / `resolve_prefix_unfiltered`) instead of
//!    being treated as free text — it must not error just because it arrived
//!    through `resolve_reference` rather than `get`.
//! 2. **Recently-referenced ring.** An exact (case-insensitive) or substring
//!    match against this actor's ring (`reference_ring::ReferenceRing`).
//! 3. **Hybrid-search fallback.** `KhiveRuntime::hybrid_search` over the
//!    caller's namespace, ranked by RRF score.
//!
//! A single candidate clearing the stage's confidence bar resolves; multiple
//! viable candidates or none never silently pick — they return `Ambiguous`
//! or `NotFound` for the caller to disambiguate.

use std::str::FromStr;

use uuid::Uuid;

use crate::error::{RuntimeError, RuntimeResult};
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
/// A single ring/search candidate auto-resolves only at or above this bar;
/// below it, the sole candidate is still surfaced, just as `Ambiguous` (never
/// silently accepted, never silently dropped — see F7).
const AUTO_RESOLVE_CONFIDENCE: f64 = 0.7;
/// Hybrid-search fallback: the top hit auto-resolves only when it leads the
/// runner-up by at least this ratio — RRF scores are not on a fixed 0..1
/// confidence scale, so a fixed absolute bar can't express "decisively best"
/// the way it can for the ring. Below the margin, every hit above the score
/// floor is surfaced as a candidate instead.
const SEARCH_MARGIN_RATIO: f64 = 2.0;
/// Hybrid-search hits below this score never enter the candidate set at all.
const SEARCH_SCORE_FLOOR: f64 = 0.0;

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
    if let Ok(uuid) = Uuid::from_str(trimmed) {
        return match runtime.resolve_by_id(token, uuid).await? {
            Some(_) => Ok(ReferenceResolution::Resolved {
                id: uuid,
                confidence: 1.0,
            }),
            None => Ok(ReferenceResolution::NotFound),
        };
    }
    if is_hex_prefix(trimmed) {
        return match runtime.resolve_prefix_unfiltered(trimmed).await {
            Ok(Some(uuid)) => Ok(ReferenceResolution::Resolved {
                id: uuid,
                confidence: 1.0,
            }),
            Ok(None) => Ok(ReferenceResolution::NotFound),
            Err(RuntimeError::AmbiguousPrefix { matches, .. }) => {
                Ok(ReferenceResolution::Ambiguous {
                    candidates: matches
                        .into_iter()
                        .map(|id| ReferenceCandidate {
                            id,
                            name: None,
                            score: 1.0,
                        })
                        .collect(),
                })
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

    // Stage 3: hybrid-search fallback over the namespace.
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
        1 => {
            let top = &candidates[0];
            if top.score >= AUTO_RESOLVE_CONFIDENCE {
                Ok(ReferenceResolution::Resolved {
                    id: top.id,
                    confidence: top.score,
                })
            } else {
                Ok(ReferenceResolution::Ambiguous { candidates })
            }
        }
        _ => {
            let top_score = candidates[0].score;
            let second_score = candidates[1].score;
            let decisive =
                second_score <= f64::EPSILON || top_score / second_score >= SEARCH_MARGIN_RATIO;
            if decisive && top_score > SEARCH_SCORE_FLOOR {
                Ok(ReferenceResolution::Resolved {
                    id: candidates[0].id,
                    confidence: top_score,
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
            Some(if top.score >= AUTO_RESOLVE_CONFIDENCE {
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
}
