//! Integration tests and property tests for fusion module.

#[cfg(test)]
mod integration_tests {
    use crate::{fuse, reciprocal_rank_fusion, union_fusion, weighted_fusion, FusionStrategy};
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
        let source: Vec<(&str, DeterministicScore)> = vec![
            ("delta", DeterministicScore::from_f64(0.5)),
            ("alpha", DeterministicScore::from_f64(0.5)),
            ("charlie", DeterministicScore::from_f64(0.5)),
            ("bravo", DeterministicScore::from_f64(0.5)),
        ];

        for _ in 0..10 {
            let fused = fuse(vec![source.clone()], &FusionStrategy::union(), 10).unwrap();

            assert_eq!(fused.len(), 4);
            assert_eq!(fused[0].0, "alpha");
            assert_eq!(fused[1].0, "bravo");
            assert_eq!(fused[2].0, "charlie");
            assert_eq!(fused[3].0, "delta");
        }
    }

    #[test]
    fn test_rrf_large_number_of_results() {
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

// Property tests: deterministic coverage of RRF/union/weighted properties.
// Proptest integration deferred (see issue #746).

#[cfg(test)]
mod property_tests {
    use crate::{reciprocal_rank_fusion, union_fusion, weighted_fusion};
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
}
