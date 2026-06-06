//! Integration, property, and regression tests for the khive-fusion crate.
//!
// INLINE TEST JUSTIFICATION: tests were moved here from src/tests.rs to satisfy
// the crate-level test layout contract (coding-standards.md §file_layout).
// Public-API access is sufficient; no pub(crate) visibility is required.

mod integration_tests {
    use khive_fusion::{
        fuse, reciprocal_rank_fusion, union_fusion, weighted_fusion, FusionStrategy,
    };
    use khive_score::DeterministicScore;

    pub(super) fn make_results<Id: Clone>(items: Vec<(Id, f64)>) -> Vec<(Id, DeterministicScore)> {
        items
            .into_iter()
            .map(|(id, score)| (id, DeterministicScore::from_f64(score)))
            .collect()
    }

    // =========================================================================
    // RETRIEVAL-01: Deterministic Tie-Breaking Tests
    // =========================================================================

    #[test]
    fn test_rrf_deterministic_tie_breaking() {
        // When two documents have equal RRF scores, they should be ordered by ID
        let source1 = make_results(vec![("doc_a", 0.9)]); // rank 1
        let source2 = make_results(vec![("doc_b", 0.8)]); // rank 1

        // Run multiple times to verify consistency
        for _ in 0..10 {
            let fused = reciprocal_rank_fusion(vec![source1.clone(), source2.clone()], 60);

            assert_eq!(fused.len(), 2);
            // Both have same RRF score (1/61), so order should be by ID
            assert_eq!(fused[0].1, fused[1].1, "Scores should be equal");
            assert_eq!(
                fused[0].0, "doc_a",
                "doc_a should come first (lexicographic order)"
            );
            assert_eq!(fused[1].0, "doc_b", "doc_b should come second");
        }
    }

    #[test]
    fn test_weighted_deterministic_tie_breaking() {
        // Two documents with equal weighted scores
        let source = make_results(vec![("z_doc", 0.5), ("a_doc", 0.5)]);

        for _ in 0..10 {
            let fused = weighted_fusion(vec![source.clone()], &[1.0]);

            assert_eq!(fused.len(), 2);
            assert_eq!(fused[0].1, fused[1].1, "Scores should be equal");
            assert_eq!(
                fused[0].0, "a_doc",
                "a_doc should come first (lexicographic order)"
            );
            assert_eq!(fused[1].0, "z_doc", "z_doc should come second");
        }
    }

    #[test]
    fn test_union_deterministic_tie_breaking() {
        // Two documents with equal max scores
        let source1 = make_results(vec![("charlie", 0.8)]);
        let source2 = make_results(vec![("alpha", 0.8)]);

        for _ in 0..10 {
            let fused = union_fusion(vec![source1.clone(), source2.clone()]);

            assert_eq!(fused.len(), 2);
            assert_eq!(fused[0].1, fused[1].1, "Scores should be equal");
            assert_eq!(fused[0].0, "alpha", "alpha should come first");
            assert_eq!(fused[1].0, "charlie", "charlie should come second");
        }
    }

    #[test]
    fn test_fuse_deterministic_with_many_ties() {
        // Multiple documents all at same score
        let source: Vec<(&str, DeterministicScore)> = vec![
            ("delta", DeterministicScore::from_f64(0.5)),
            ("alpha", DeterministicScore::from_f64(0.5)),
            ("charlie", DeterministicScore::from_f64(0.5)),
            ("bravo", DeterministicScore::from_f64(0.5)),
        ];

        for _ in 0..10 {
            let fused = fuse(vec![source.clone()], &FusionStrategy::union(), 10).unwrap();

            assert_eq!(fused.len(), 4);
            // All have same score, should be in lexicographic order
            assert_eq!(fused[0].0, "alpha");
            assert_eq!(fused[1].0, "bravo");
            assert_eq!(fused[2].0, "charlie");
            assert_eq!(fused[3].0, "delta");
        }
    }

    #[test]
    fn test_rrf_large_number_of_results() {
        // Test with many results to ensure no overflow/precision issues
        let source: Vec<(String, DeterministicScore)> = (0..1000)
            .map(|i| {
                (
                    format!("doc_{i}"),
                    DeterministicScore::from_f64(1.0 - i as f64 / 1000.0),
                )
            })
            .collect();

        let fused = fuse(vec![source], &FusionStrategy::rrf(), 100).unwrap();

        assert_eq!(fused.len(), 100);
        assert_eq!(fused[0].0, "doc_0");
    }

    #[test]
    fn test_multiple_sources_all_same_document() {
        let source1 = make_results(vec![("doc_a", 0.9), ("doc_b", 0.8)]);
        let source2 = make_results(vec![("doc_b", 0.95), ("doc_a", 0.7)]);
        let source3 = make_results(vec![("doc_a", 0.85)]);

        let fused = reciprocal_rank_fusion(vec![source1, source2, source3], 60);

        let doc_a = fused.iter().find(|(id, _)| *id == "doc_a").unwrap();
        let doc_b = fused.iter().find(|(id, _)| *id == "doc_b").unwrap();

        assert!(doc_a.1 > doc_b.1); // doc_a appears in more sources
    }

    #[test]
    fn test_sorted_output() {
        let source1 = make_results(vec![("doc_c", 0.7), ("doc_a", 0.9), ("doc_b", 0.8)]);

        let fused = reciprocal_rank_fusion(vec![source1], 60);

        // Input order determines rank, so doc_c is rank 1, doc_a rank 2, doc_b rank 3
        assert_eq!(fused[0].0, "doc_c");
        assert_eq!(fused[1].0, "doc_a");
        assert_eq!(fused[2].0, "doc_b");
    }

    #[test]
    fn test_rrf_document_only_in_one_source() {
        let source1 = make_results(vec![("doc_a", 0.9)]);
        let source2 = make_results(vec![("doc_b", 0.8)]);

        let fused = reciprocal_rank_fusion(vec![source1, source2], 60);

        // Both at rank 1 in their respective sources -> same RRF score
        assert_eq!(fused.len(), 2);
        assert_eq!(fused[0].1, fused[1].1);
    }

    #[test]
    fn test_rrf_custom_k() {
        let source = make_results(vec![("doc_a", 0.9), ("doc_b", 0.8)]);

        let fused_k20 = reciprocal_rank_fusion(vec![source.clone()], 20);
        let fused_k100 = reciprocal_rank_fusion(vec![source], 100);

        let ratio_k20 = fused_k20[0].1.to_f64() / fused_k20[1].1.to_f64();
        let ratio_k100 = fused_k100[0].1.to_f64() / fused_k100[1].1.to_f64();

        // Smaller k -> larger ratio (more difference between ranks)
        assert!(ratio_k20 > ratio_k100);
    }
}

// =============================================================================
// Property Tests (Issue #746)
// TODO(port): proptest not yet added as a dev-dependency; the proptest macro
// forms below have been converted to deterministic unit tests covering the same
// properties. Re-introduce proptest once it is added to Cargo.toml [dev-dependencies].
// =============================================================================

mod property_tests {
    use khive_fusion::{reciprocal_rank_fusion, union_fusion, weighted_fusion};
    use khive_score::DeterministicScore;
    use std::collections::HashSet;

    fn make_results(items: Vec<(&'static str, f64)>) -> Vec<(String, DeterministicScore)> {
        items
            .into_iter()
            .map(|(id, score)| (id.to_string(), DeterministicScore::from_f64(score)))
            .collect()
    }

    /// RRF is commutative: source order should not affect final rankings.
    ///
    /// Verifies the `sum_perm` property from RRF.lean.
    #[test]
    fn prop_rrf_is_commutative() {
        let sources = vec![
            make_results(vec![("doc_a", 0.9), ("doc_b", 0.8)]),
            make_results(vec![("doc_b", 0.95), ("doc_c", 0.7)]),
            make_results(vec![("doc_a", 0.6), ("doc_c", 0.5)]),
        ];

        let fused_orig = reciprocal_rank_fusion(sources.clone(), 60);

        let mut reversed = sources.clone();
        reversed.reverse();
        let fused_reversed = reciprocal_rank_fusion(reversed, 60);

        let orig_set: HashSet<_> = fused_orig
            .iter()
            .map(|(id, score)| (id.clone(), score.to_raw()))
            .collect();
        let rev_set: HashSet<_> = fused_reversed
            .iter()
            .map(|(id, score)| (id.clone(), score.to_raw()))
            .collect();

        assert_eq!(
            orig_set, rev_set,
            "RRF results should be identical regardless of source order"
        );
    }

    /// Documents in more sources should score higher than those in fewer.
    ///
    /// Verifies the `present_gt_absent` property from RRF.lean.
    #[test]
    fn prop_rrf_more_sources_higher_score() {
        let source1: Vec<(String, DeterministicScore)> =
            vec![("doc_common".to_string(), DeterministicScore::from_f64(0.9))];
        let source2: Vec<(String, DeterministicScore)> = vec![
            ("doc_common".to_string(), DeterministicScore::from_f64(0.9)),
            ("doc_single".to_string(), DeterministicScore::from_f64(0.8)),
        ];

        let fused = reciprocal_rank_fusion(vec![source1, source2], 60);

        let common = fused.iter().find(|(id, _)| id == "doc_common").unwrap();
        let single = fused.iter().find(|(id, _)| id == "doc_single").unwrap();

        assert!(
            common.1 >= single.1,
            "Document in more sources should score >= document in fewer"
        );
    }

    /// RRF scores should always be non-negative.
    #[test]
    fn prop_rrf_scores_nonnegative() {
        let sources = vec![
            make_results(vec![("doc_a", 0.9), ("doc_b", 0.1)]),
            make_results(vec![("doc_b", 0.5), ("doc_c", 0.0)]),
        ];
        let fused = reciprocal_rank_fusion(sources, 60);

        for (id, score) in &fused {
            assert!(
                score.to_f64() >= 0.0,
                "RRF score for {} should be non-negative, got {}",
                id,
                score.to_f64()
            );
        }
    }

    /// Union fusion should include all unique documents.
    #[test]
    fn prop_union_includes_all_docs() {
        let sources = vec![
            make_results(vec![("doc_a", 0.9), ("doc_b", 0.8)]),
            make_results(vec![("doc_b", 0.7), ("doc_c", 0.6)]),
            make_results(vec![("doc_d", 0.5)]),
        ];

        let expected_ids: HashSet<_> = sources
            .iter()
            .flat_map(|s| s.iter().map(|(id, _)| id.clone()))
            .collect();

        let fused = union_fusion(sources);
        let result_ids: HashSet<_> = fused.iter().map(|(id, _)| id.clone()).collect();

        assert_eq!(
            expected_ids, result_ids,
            "Union should contain all unique documents"
        );
    }

    /// With per-source min-max normalization, single-element sources always
    /// map to 1.0, so a document present in both sources receives a combined
    /// score of sum(weight_i * 1.0) = total_weight = 1.0 for equal weights.
    #[test]
    fn prop_weighted_single_element_sources_score_one() {
        for (s1, s2) in [(0.0f64, 0.0f64), (0.5, 1.0), (0.9, 0.1), (1.0, 1.0)] {
            let source1: Vec<(String, DeterministicScore)> =
                vec![("doc".to_string(), DeterministicScore::from_f64(s1))];
            let source2: Vec<(String, DeterministicScore)> =
                vec![("doc".to_string(), DeterministicScore::from_f64(s2))];

            let fused = weighted_fusion(vec![source1, source2], &[0.5, 0.5]);

            if let Some((_, score)) = fused.first() {
                let actual = score.to_f64();
                assert!(
                    (actual - 1.0).abs() < 1e-9,
                    "Single-element source always normalizes to 1.0; combined = 1.0, got {} (inputs: {}, {})",
                    actual, s1, s2
                );
            }
        }
    }

    /// "More sources" property holds only for equal-rank documents.
    ///
    /// A document at rank 1 in a single source outscores a document at rank 1000
    /// in three sources. The correct invariant: equal-rank docs score proportionally
    /// to source count (finding #8 — fixed from an overly broad "any rank" claim).
    #[test]
    fn prop_rrf_more_sources_higher_score_equal_ranks() {
        // doc_common at rank 1 in both sources; doc_single at rank 1 in one source.
        // doc_common: 1/61 + 1/61 = 2/61 > doc_single: 1/61
        let source1: Vec<(String, DeterministicScore)> =
            vec![("doc_common".to_string(), DeterministicScore::from_f64(0.9))];
        let source2: Vec<(String, DeterministicScore)> = vec![
            ("doc_common".to_string(), DeterministicScore::from_f64(0.9)),
            ("doc_single".to_string(), DeterministicScore::from_f64(0.8)),
        ];

        let fused = reciprocal_rank_fusion(vec![source1, source2], 60);
        let common = fused.iter().find(|(id, _)| id == "doc_common").unwrap();
        let single = fused.iter().find(|(id, _)| id == "doc_single").unwrap();

        assert!(
            common.1 > single.1,
            "Equal-rank doc in more sources must outscore equal-rank doc in fewer sources"
        );
    }

    /// A deep-rank multi-source document can lose to a shallow-rank single-source
    /// document — the broad "more sources always wins" property is false (finding #8).
    #[test]
    fn prop_rrf_deep_rank_multi_source_can_lose_to_rank1_single_source() {
        // doc_single at rank 1 in one source: 1/61 ≈ 0.01639
        // doc_multi at rank 1000 in three sources: 3/1060 ≈ 0.00283
        let single_source = vec![("doc_single".to_string(), DeterministicScore::from_f64(0.99))];
        // Three sources, doc_multi at the very end of each (rank 1000 = index 999)
        let make_deep_source = || -> Vec<(String, DeterministicScore)> {
            let mut v: Vec<(String, DeterministicScore)> = (0..999)
                .map(|i| {
                    (
                        format!("filler_{i}"),
                        DeterministicScore::from_f64(1.0 - i as f64 / 1000.0),
                    )
                })
                .collect();
            v.push(("doc_multi".to_string(), DeterministicScore::from_f64(0.001)));
            v
        };

        let fused = reciprocal_rank_fusion(
            vec![
                single_source,
                make_deep_source(),
                make_deep_source(),
                make_deep_source(),
            ],
            60,
        );

        let doc_single = fused.iter().find(|(id, _)| id == "doc_single").unwrap();
        let doc_multi = fused.iter().find(|(id, _)| id == "doc_multi").unwrap();

        assert!(
            doc_single.1 > doc_multi.1,
            "rank-1 single-source doc ({:.5}) should beat rank-1000 three-source doc ({:.5})",
            doc_single.1.to_f64(),
            doc_multi.1.to_f64()
        );
    }
}

/// Regression tests for correctness fixes.
mod regression_tests {
    use khive_fusion::{fuse, reciprocal_rank_fusion, weighted_fusion, FusionStrategy};
    use khive_score::{rrf_score, DeterministicScore};

    fn make_results<Id: Clone>(items: Vec<(Id, f64)>) -> Vec<(Id, DeterministicScore)> {
        items
            .into_iter()
            .map(|(id, score)| (id, DeterministicScore::from_f64(score)))
            .collect()
    }

    // ── Finding #2: zero-weight source must not inject docs into output ────────

    #[test]
    fn weighted_zero_weight_source_is_excluded() {
        let a = make_results(vec![("a", 1.0)]);
        let b = make_results(vec![("b", 1.0)]);

        let out = weighted_fusion(vec![a, b], &[1.0, 0.0]);

        assert!(
            out.iter().any(|(id, _)| *id == "a"),
            "doc a must be present"
        );
        assert!(
            !out.iter().any(|(id, _)| *id == "b"),
            "doc b must be absent (zero-weight source)"
        );
    }

    // ── Finding #3: non-finite weights must not panic ──────────────────────────

    #[test]
    fn weighted_inf_weight_does_not_panic() {
        let a = make_results(vec![("a", 1.0)]);
        let b = make_results(vec![("b", 1.0)]);
        // +Inf weight previously caused inf/inf = NaN → panic via .expect()
        let out = weighted_fusion(vec![a, b], &[f64::INFINITY, 1.0]);
        // +Inf is treated as 0.0, leaving only source b with weight 1.0.
        assert!(!out.is_empty(), "result must not be empty");
    }

    #[test]
    fn weighted_nan_weight_does_not_panic() {
        let a = make_results(vec![("a", 1.0)]);
        let out = weighted_fusion(vec![a], &[f64::NAN]);
        // NaN treated as 0 → all weights are 0 → equal distribution → doc a present.
        assert!(!out.is_empty(), "result must not be empty with NaN weight");
    }

    // ── Finding #1: weight/source length mismatch must not mis-score ──────────

    #[test]
    fn weighted_extra_weight_does_not_steal_mass() {
        // One source, two weights: the extra weight previously stole half the mass.
        let a = make_results(vec![("a", 1.0)]);
        let out_mismatch = weighted_fusion(vec![a.clone()], &[0.5, 0.5]);
        let out_correct = weighted_fusion(vec![a], &[1.0]);

        // With the fix, extra weights beyond sources.len() are excluded from the
        // normalization denominator, so source[0] gets full weight in both cases.
        let score_mismatch = out_mismatch[0].1.to_f64();
        let score_correct = out_correct[0].1.to_f64();
        assert!(
            (score_mismatch - score_correct).abs() < 0.01,
            "extra weight must not steal mass: mismatch={score_mismatch:.4} correct={score_correct:.4}"
        );
    }

    // ── Finding #4: duplicate IDs in same RRF source count once ───────────────

    #[test]
    fn rrf_duplicate_id_in_same_source_counts_once() {
        // "a" appears twice in the same source; only rank-1 occurrence should count.
        let src = make_results(vec![("a", 1.0), ("a", 0.9), ("b", 0.8)]);
        let out = reciprocal_rank_fusion(vec![src], 60);

        let a_score = out.iter().find(|(id, _)| *id == "a").unwrap().1;
        let expected = rrf_score(1, 60); // rank 1 only
        assert_eq!(
            a_score, expected,
            "duplicate in same source must count as rank-1 only"
        );
    }

    // ── Finding #4: duplicate IDs in same weighted source keep max score ───────

    #[test]
    fn weighted_duplicate_id_in_same_source_keeps_max() {
        // "a" appears twice in source 0 with scores 1.0 and 0.5.
        // Only the max (1.0) should contribute — not both.
        let a = make_results(vec![("a", 1.0), ("a", 0.5)]);
        let out = weighted_fusion(vec![a.clone()], &[1.0]);
        let score_with_dup = out[0].1.to_f64();

        // Reference: single occurrence of "a" at 1.0.
        let a_single = make_results(vec![("a", 1.0)]);
        let out_single = weighted_fusion(vec![a_single], &[1.0]);
        let score_single = out_single[0].1.to_f64();

        assert!(
            (score_with_dup - score_single).abs() < 0.01,
            "duplicate in same source must not double-count: dup={score_with_dup:.4} single={score_single:.4}"
        );
    }

    // ── Finding #5: VectorOnly/KeywordOnly use union_fusion ───────────────────

    #[test]
    fn vector_only_single_source_passes_through() {
        let src = make_results(vec![("a", 0.9), ("b", 0.7)]);
        let out = fuse(vec![src], &FusionStrategy::VectorOnly, 10).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].0, "a");
    }

    #[test]
    fn keyword_only_single_source_passes_through() {
        let src = make_results(vec![("x", 0.8), ("y", 0.6)]);
        let out = fuse(vec![src], &FusionStrategy::KeywordOnly, 10).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].0, "x");
    }

    // ── Finding #6: top_k partial sort returns correct top results ────────────

    #[test]
    fn fuse_top_k_partial_sort_correct_results() {
        let src: Vec<(&str, DeterministicScore)> = vec![
            ("e", DeterministicScore::from_f64(0.5)),
            ("d", DeterministicScore::from_f64(0.6)),
            ("c", DeterministicScore::from_f64(0.7)),
            ("b", DeterministicScore::from_f64(0.8)),
            ("a", DeterministicScore::from_f64(0.9)),
        ];
        let out = fuse(vec![src], &FusionStrategy::rrf(), 3).unwrap();
        assert_eq!(out.len(), 3);
        // RRF ranks by input position: "e" is rank 1, "d" rank 2, "c" rank 3
        assert_eq!(out[0].0, "e");
        assert_eq!(out[1].0, "d");
        assert_eq!(out[2].0, "c");
    }

    // ── KHFUS-AUD-003: try_normalize_weights rejects non-finite inputs ─────────

    #[test]
    fn try_normalize_weights_rejects_nan() {
        let result = khive_fusion::try_normalize_weights(&[0.5, f64::NAN, 0.3]);
        assert!(result.is_err(), "NaN weight must be rejected");
        assert_eq!(result.unwrap_err(), 1);
    }

    #[test]
    fn try_normalize_weights_rejects_infinity() {
        let result = khive_fusion::try_normalize_weights(&[1.0, f64::INFINITY]);
        assert!(result.is_err(), "+Inf weight must be rejected");
        assert_eq!(result.unwrap_err(), 1);
    }

    #[test]
    fn try_normalize_weights_rejects_neg_infinity() {
        let result = khive_fusion::try_normalize_weights(&[f64::NEG_INFINITY, 1.0]);
        assert!(result.is_err(), "-Inf weight must be rejected");
        assert_eq!(result.unwrap_err(), 0);
    }

    #[test]
    fn try_normalize_weights_accepts_finite() {
        let result = khive_fusion::try_normalize_weights(&[0.6, 0.4]);
        assert!(result.is_ok());
        let nw = result.unwrap();
        assert!((nw[0] - 0.6).abs() < 1e-10);
        assert!((nw[1] - 0.4).abs() < 1e-10);
    }
}
