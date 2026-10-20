//! ANN candidate acquisition, score provenance, fusion, and post-hydration filters.

use super::{
    explicitly_requested_status, json, status_multiplier, vamana, DeterministicScore, HashMap,
    KhiveRuntime, NamespaceToken, ScoredHit, Uuid, Value,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ScoreProvenance {
    pub(super) lexical: bool,
    pub(super) ann: bool,
    pub(super) embedding_rerank: bool,
}

impl ScoreProvenance {
    pub(super) const fn lexical() -> Self {
        Self {
            lexical: true,
            ann: false,
            embedding_rerank: false,
        }
    }

    pub(super) const fn ann() -> Self {
        Self {
            lexical: false,
            ann: true,
            embedding_rerank: false,
        }
    }

    pub(super) fn merge_sources(&mut self, other: Self) {
        self.lexical |= other.lexical;
        self.ann |= other.ann;
        self.embedding_rerank |= other.embedding_rerank;
    }

    pub(super) fn to_json(self) -> Value {
        let mut sources = Vec::with_capacity(2);
        if self.lexical {
            sources.push("lexical");
        }
        if self.ann {
            sources.push("ann");
        }
        json!({
            "sources": sources,
            "embedding_rerank": self.embedding_rerank,
            "normalization": "s_over_s_plus_1",
            "calibrated": false,
        })
    }
}

pub(super) fn candidate_fallback(hits: &[ScoredHit]) -> &'static str {
    if hits.iter().any(|hit| hit.provenance.ann) && !hits.iter().any(|hit| hit.provenance.lexical) {
        "ann"
    } else {
        "none"
    }
}

pub(super) enum AnnAvailability {
    Ready,
    WarmingTimedOut { corpus_non_empty: bool },
    Absent,
}

pub(super) struct AnnSearchState {
    pub(super) hits: Vec<(Uuid, f32)>,
    pub(super) availability: AnnAvailability,
    /// Whether the ANN source itself returned fewer than `k` entries.
    /// Fresh-tail deletes may shrink `hits` afterward without proving that
    /// deeper ANN candidates do not exist.
    pub(super) source_exhausted: bool,
}

struct FreshTailSearchState {
    hits: Vec<(Uuid, f32)>,
    source_exhausted: bool,
}

async fn merge_fresh_tail_for_search(
    runtime: &KhiveRuntime,
    ann: &vamana::SharedAnn,
    key: &vamana::AnnKey,
    query_embedding: &[f32],
    k: usize,
    loaded: Option<(Vec<(Uuid, f32)>, u64)>,
) -> FreshTailSearchState {
    let (candidates, watermark, source_exhausted) = match loaded {
        Some((candidates, watermark)) => {
            let source_exhausted = candidates.len() < k;
            (candidates, Some(watermark), source_exhausted)
        }
        None => (Vec::new(), None, true),
    };
    match vamana::fresh_tail_leg(runtime, ann, key, query_embedding, k, watermark).await {
        vamana::FreshTailOutcome::Ops(ops) => FreshTailSearchState {
            hits: vamana::merge_fresh_tail_off_thread(candidates, query_embedding, ops).await,
            source_exhausted,
        },
        vamana::FreshTailOutcome::Replace {
            candidates,
            source_exhausted,
        } => FreshTailSearchState {
            hits: candidates,
            source_exhausted,
        },
        vamana::FreshTailOutcome::Skipped => FreshTailSearchState {
            hits: candidates,
            source_exhausted,
        },
    }
}

/// Search the loaded ANN slot, waiting a bounded time when its warm is in flight.
pub(super) async fn search_ann_with_warm_wait(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    ann: &vamana::SharedAnn,
    key: &vamana::AnnKey,
    query_embedding: &[f32],
    k: usize,
) -> AnnSearchState {
    if let Some(loaded) = vamana::search_loaded_with_seq(ann, key, query_embedding, k).await {
        let tail =
            merge_fresh_tail_for_search(runtime, ann, key, query_embedding, k, Some(loaded)).await;
        return AnnSearchState {
            hits: tail.hits,
            availability: AnnAvailability::Ready,
            source_exhausted: tail.source_exhausted,
        };
    }
    if !vamana::is_warming_not_loaded(ann, key) {
        let tail = merge_fresh_tail_for_search(runtime, ann, key, query_embedding, k, None).await;
        return AnnSearchState {
            hits: tail.hits,
            availability: AnnAvailability::Absent,
            source_exhausted: tail.source_exhausted,
        };
    }
    if vamana::wait_ready(
        ann,
        key,
        vamana::warm_wait_timeout_ms(),
        vamana::ANN_WARM_WAIT_POLL_MS,
    )
    .await
    {
        let loaded = vamana::search_loaded_with_seq(ann, key, query_embedding, k).await;
        let availability = if loaded.is_some() {
            AnnAvailability::Ready
        } else {
            AnnAvailability::Absent
        };
        let tail = merge_fresh_tail_for_search(runtime, ann, key, query_embedding, k, loaded).await;
        return AnnSearchState {
            hits: tail.hits,
            availability,
            source_exhausted: tail.source_exhausted,
        };
    }

    let corpus_non_empty =
        vamana::compute_fingerprint(runtime, token, runtime.default_embedder_name())
            .await
            .map(|fingerprint| fingerprint.vector_count > 0)
            .unwrap_or(false);
    let tail = merge_fresh_tail_for_search(runtime, ann, key, query_embedding, k, None).await;
    AnnSearchState {
        hits: tail.hits,
        availability: AnnAvailability::WarmingTimedOut { corpus_non_empty },
        source_exhausted: tail.source_exhausted,
    }
}

// ─── ANN fusion (symmetric RRF) ─────────────────────────────────────────────

pub(super) use khive_fusion::DEFAULT_RRF_K as RRF_K;

pub(super) fn normalize_rrf_score(raw: f32, source_count: usize, k: usize) -> f32 {
    if source_count == 0 {
        return 0.0;
    }
    let rank_one = khive_score::rrf_score_one_based(std::num::NonZeroUsize::MIN, k);
    // Checked-add overflow or a reciprocal below fixed-point resolution
    // leaves no normalization scale.
    if rank_one == DeterministicScore::ZERO {
        return 0.0;
    }
    let rank_one = rank_one.to_f64() as f32;
    let theoretical_max = source_count as f32 * rank_one;
    (raw / theoretical_max).clamp(0.0, 1.0)
}

pub(super) fn fuse_ann_hits(fts_hits: &mut Vec<ScoredHit>, ann_hits: &[ScoredHit], min_score: f32) {
    let drained: Vec<ScoredHit> = std::mem::take(fts_hits);

    let fts_source: Vec<(String, DeterministicScore)> = drained
        .iter()
        .map(|hit| (hit.id.clone(), DeterministicScore::from_f32(hit.score)))
        .collect();
    let mut by_id: HashMap<String, ScoredHit> = drained
        .into_iter()
        .map(|hit| (hit.id.clone(), hit))
        .collect();
    let ann_source: Vec<(String, DeterministicScore)> = ann_hits
        .iter()
        .map(|hit| (hit.id.clone(), DeterministicScore::from_f32(hit.score)))
        .collect();
    for hit in ann_hits {
        by_id
            .entry(hit.id.clone())
            .and_modify(|existing| existing.provenance.merge_sources(hit.provenance))
            .or_insert_with(|| hit.clone());
    }

    let source_count = usize::from(!fts_source.is_empty()) + usize::from(!ann_source.is_empty());
    let fused = khive_fusion::reciprocal_rank_fusion(vec![fts_source, ann_source], RRF_K);

    for (id, fused_score) in fused {
        let raw_score = fused_score.to_f64() as f32;
        let score = normalize_rrf_score(raw_score, source_count, RRF_K);
        if score < min_score {
            continue;
        }

        if let Some(mut hit) = by_id.remove(&id) {
            hit.score = score;
            fts_hits.push(hit);
        }
    }
}

// ─── status filtering (post-hydration) ───────────────────────────────────────

/// Remove hits whose `status` is in `exclude_statuses` after hydration.
pub(super) fn filter_by_excluded_statuses(hits: &mut Vec<ScoredHit>, exclude_statuses: &[&str]) {
    if exclude_statuses.is_empty() {
        return;
    }
    hits.retain(|hit| {
        let status = hit.status.as_deref().unwrap_or("");
        !exclude_statuses.contains(&status)
    });
}

/// Apply the complete public status contract to hydrated hits.
///
/// An explicit `status=` is an allowlist, not merely a request to disable the
/// default exclusions. This distinction is load-bearing for ANN candidates,
/// which do not pass through the FTS SQL predicate.
pub(super) fn filter_hits_by_status(
    hits: &mut Vec<ScoredHit>,
    statuses: &[String],
    exclude_statuses: &[&str],
) {
    if statuses.is_empty() {
        filter_by_excluded_statuses(hits, exclude_statuses);
        return;
    }

    hits.retain(|hit| {
        hit.status
            .as_deref()
            .is_some_and(|status| statuses.iter().any(|allowed| allowed == status))
    });
}

pub(super) fn deprecated_allowed_by_status_policy(
    statuses: &[String],
    exclude_statuses: &[&str],
) -> bool {
    if statuses.is_empty() {
        !exclude_statuses.contains(&"deprecated")
    } else {
        explicitly_requested_status(statuses, "deprecated")
    }
}

// ─── type filtering (post-hydration) ─────────────────────────────────────────

/// Remove hits that do not match `type_filter` after hydration.
///
/// Mirrors eligibility in the FTS/SQL path in `fetch_fts_candidates`:
///
/// - `Some("domain")` keeps only domain hits (`hit.is_domain == true`).
/// - `Some(other)` where other is non-empty keeps only non-domain hits.
/// - `None` or `Some("")` is a no-op.
///
/// Applied to hydrated ANN candidates before fusion/refill and again to the
/// fused pool as a final shared-source guard.
pub(super) fn filter_hits_by_type(hits: &mut Vec<ScoredHit>, type_filter: Option<&str>) {
    let filt = match type_filter {
        Some(f) if !f.is_empty() => f,
        _ => return,
    };
    let want_domain = filt == "domain";
    hits.retain(|hit| {
        if want_domain {
            hit.is_domain
        } else {
            !hit.is_domain
        }
    });
}

// ─── status scoring ───────────────────────────────────────────────────────────

pub(super) fn apply_status_multipliers(hits: &mut Vec<ScoredHit>, include_deprecated: bool) {
    hits.retain_mut(|hit| {
        let multiplier = status_multiplier(hit.status.as_deref());
        // Squash raw score to (0,1) via monotonic s/(s+1) before applying the status
        // multiplier so that TF-IDF scores > 1 don't saturate ranking. RRF-normalized
        // scores (already ≤ 1) are squashed at most to 0.5, preserving relative order.
        hit.score = (hit.score / (hit.score + 1.0) * multiplier).clamp(0.0, 1.0);
        include_deprecated || multiplier > 0.0
    });
    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.slug.cmp(&b.slug))
    });
}

/// Re-applies `min_score` after [`apply_status_multipliers`] so it is a genuine
/// floor on the scores returned to the caller.
///
/// The fusion-stage application in `fuse_ann_hits` stays as an early admission
/// filter, but the multiplier step rewrites every surviving score via
/// `s / (s + 1)` (mapping 1.0 to 0.5), so a hit that cleared fusion can land
/// below the caller's floor. Filtering again here — after the rewrite, before
/// the `limit` truncation — guarantees every returned score is >= `min_score`;
/// returning fewer than `limit` hits when the floor removes some is correct.
pub(super) fn enforce_min_score_floor(hits: &mut Vec<ScoredHit>, min_score: f32) {
    hits.retain(|hit| hit.score >= min_score);
}

#[cfg(test)]
mod rrf_normalization_tests {
    use super::*;

    fn legacy_normalization(raw: f32, source_count: usize, k: usize) -> f32 {
        if source_count == 0 {
            return 0.0;
        }
        let theoretical_max = source_count as f32 / (k as f32 + 1.0);
        (raw / theoretical_max).clamp(0.0, 1.0)
    }

    fn assert_legacy_bits(raw: f32, source_count: usize, k: usize) {
        let actual = normalize_rrf_score(raw, source_count, k);
        let expected = legacy_normalization(raw, source_count, k);
        if expected.is_nan() {
            assert!(actual.is_nan(), "raw={raw}, sources={source_count}, k={k}");
        } else {
            assert_eq!(
                actual.to_bits(),
                expected.to_bits(),
                "raw={raw}, sources={source_count}, k={k}"
            );
        }
    }

    fn quantized_rank(rank: usize) -> DeterministicScore {
        DeterministicScore::from_f64(1.0 / (60 + rank) as f64)
    }

    #[test]
    fn rrf_normalization_keeps_legacy_bits_for_production_arms() {
        assert_eq!(RRF_K, 60, "a new production k needs a new rounding proof");
        let mut raw_scores = vec![
            0.0,
            -0.0,
            f32::MIN_POSITIVE,
            -0.25,
            0.001,
            0.015,
            0.025,
            0.05,
            1.0,
            f32::MAX,
            f32::INFINITY,
            f32::NEG_INFINITY,
            f32::NAN,
        ];
        for rank in [1, 2, 7, 17, 4096] {
            let score = quantized_rank(rank);
            raw_scores.push(score.to_f64() as f32);
            raw_scores.push((score + quantized_rank(3)).to_f64() as f32);
        }
        for source_count in 0..=2 {
            for &raw in &raw_scores {
                assert_legacy_bits(raw, source_count, RRF_K);
            }
        }
    }

    #[test]
    fn rrf_normalization_returns_zero_without_representable_rank_one() {
        for source_count in [1, 2, 3] {
            for raw in [0.001, 0.5, 1.0, f32::INFINITY, f32::NAN] {
                assert_eq!(
                    normalize_rrf_score(raw, source_count, usize::MAX).to_bits(),
                    0.0f32.to_bits(),
                    "checked-add overflow must not divide by a zero rank-one score"
                );
            }
        }
    }

    fn hit(id: &str, provenance: ScoreProvenance) -> ScoredHit {
        ScoredHit {
            id: id.to_owned(),
            slug: id.to_owned(),
            name: id.to_owned(),
            content: None,
            tags: None,
            atom_embed_text: None,
            finalized: true,
            is_domain: false,
            status: None,
            score: 0.5,
            provenance,
        }
    }

    #[test]
    fn rrf_fusion_keeps_single_and_dual_arm_scores_and_floor() {
        for lexical in [true, false] {
            let provenance = if lexical {
                ScoreProvenance::lexical()
            } else {
                ScoreProvenance::ann()
            };
            let source = vec![hit("first", provenance), hit("second", provenance)];
            let (mut hits, ann) = if lexical {
                (source, Vec::new())
            } else {
                (Vec::new(), source)
            };
            fuse_ann_hits(&mut hits, &ann, 0.0);
            assert_eq!(
                hits.iter().map(|hit| hit.id.as_str()).collect::<Vec<_>>(),
                ["first", "second"]
            );
            for (hit, rank) in hits.iter().zip([1, 2]) {
                let expected = legacy_normalization(quantized_rank(rank).to_f64() as f32, 1, 60);
                assert_eq!(hit.score.to_bits(), expected.to_bits());
                assert_eq!(hit.provenance.lexical, lexical);
                assert_eq!(hit.provenance.ann, !lexical);
            }
        }

        let lexical = vec![
            hit("lexical", ScoreProvenance::lexical()),
            hit("shared", ScoreProvenance::lexical()),
        ];
        let ann = vec![
            hit("ann", ScoreProvenance::ann()),
            hit("shared", ScoreProvenance::ann()),
        ];
        let mut hits = lexical.clone();
        fuse_ann_hits(&mut hits, &ann, 0.0);
        assert_eq!(
            hits.iter().map(|hit| hit.id.as_str()).collect::<Vec<_>>(),
            ["shared", "ann", "lexical"]
        );
        let expected = [
            legacy_normalization(
                (quantized_rank(2) + quantized_rank(2)).to_f64() as f32,
                2,
                60,
            ),
            legacy_normalization(quantized_rank(1).to_f64() as f32, 2, 60),
            legacy_normalization(quantized_rank(1).to_f64() as f32, 2, 60),
        ];
        for (hit, expected) in hits.iter().zip(expected) {
            assert_eq!(hit.score.to_bits(), expected.to_bits());
        }
        assert!(hits[0].provenance.lexical && hits[0].provenance.ann);

        let mut filtered = lexical;
        fuse_ann_hits(&mut filtered, &ann, 0.75);
        assert_eq!(
            filtered
                .iter()
                .map(|hit| hit.id.as_str())
                .collect::<Vec<_>>(),
            ["shared"],
            "one-arm-only hits must not clear the two-arm normalized floor"
        );
    }
}
