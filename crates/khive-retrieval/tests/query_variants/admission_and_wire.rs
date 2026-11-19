#[test]
fn attribution_preserves_every_channel_occurrence_without_admitting_trace_only_ids() {
    let a = ids(&[10]);
    let b = ids(&[20]);
    let lexical0 = ids(&[99, 10, 10]);
    let ann0 = ids(&[10]);
    let lexical1 = ids(&[10, 20]);
    let ann1 = ids(&[20, 10, 10]);
    let lexical2 = ids(&[10]);
    let lists = [
        list(2, &[], &lexical2, &[]),
        list(1, &b, &lexical1, &ann1),
        list(0, &a, &lexical0, &ann0),
    ];
    let admitted = merge_query_variants(&variants(), &lists, &limits())
        .unwrap()
        .unwrap();
    assert_eq!(order(&admitted), ids(&[10, 20]));
    assert_eq!(
        admitted[0].variant_attribution,
        vec![
            occurrence(0, VariantSource::Lexical, 2),
            occurrence(0, VariantSource::Lexical, 3),
            occurrence(0, VariantSource::Ann, 1),
            occurrence(1, VariantSource::Lexical, 1),
            occurrence(1, VariantSource::Ann, 2),
            occurrence(1, VariantSource::Ann, 3),
            occurrence(2, VariantSource::Lexical, 1),
        ]
    );
    assert_eq!(
        admitted[1].variant_attribution,
        vec![
            occurrence(1, VariantSource::Lexical, 2),
            occurrence(1, VariantSource::Ann, 1),
        ]
    );
    for hit in &admitted {
        assert!(hit
            .variant_attribution
            .windows(2)
            .all(|pair| pair[0] < pair[1]));
    }
}

#[test]
fn candidate_limit_applies_after_fusion_and_retains_winning_late_candidates() {
    let original: Vec<_> = (1..=140).map(Uuid::from_u128).collect();
    let late = ids(&[140]);
    let lists = [
        list(0, &original, &original, &[]),
        list(1, &late, &late, &[]),
    ];
    let capped = merge_query_variants(&variants(), &lists, &limits())
        .unwrap()
        .unwrap();
    assert_eq!(capped.len(), 127);
    // A single generated vote plus original rank 140 beats every original-only candidate.
    let mut expected = ids(&[140]);
    expected.extend((1..=126).map(Uuid::from_u128));
    assert_eq!(order(&capped), expected);
    for cap in [1, 2, 12] {
        let lower = QueryVariantLimits::try_new(3, 256, 1_024, 192, cap).unwrap();
        let actual = merge_query_variants(&variants(), &lists, &lower)
            .unwrap()
            .unwrap();
        assert_eq!(actual, capped[..cap]);
    }
}

#[test]
fn no_generated_ranked_contribution_returns_none_without_reconstructing_baseline() {
    let a = ids(&[10, 20]);
    let cases = [
        vec![list(0, &a, &a, &[])],
        vec![list(0, &a, &a, &[]), list(1, &[], &[], &[])],
        vec![list(0, &a, &a, &[]), list(2, &[], &a, &a)],
    ];
    for lists in cases {
        assert_eq!(
            merge_query_variants(&variants(), &lists, &limits()).unwrap(),
            None
        );
    }
    let generated = [list(0, &[], &[], &[]), list(3, &a, &[], &a)];
    assert_eq!(
        order(
            &merge_query_variants(&variants(), &generated, &limits())
                .unwrap()
                .unwrap()
        ),
        a
    );
}

#[test]
fn malformed_lists_return_permanent_text_free_errors_before_fallback() {
    let a = ids(&[10]);
    let b = ids(&[20]);
    let secret = "do-not-echo-query-or-rephrase";
    let normalized = normalize_query_variants(
        secret,
        &strings(&[secret, "private-generated"]),
        2,
        &limits(),
    );
    let cases = [
        vec![],
        vec![list(1, &a, &a, &[])],
        vec![list(0, &[], &[], &[]), list(0, &[], &[], &[])],
        vec![
            list(0, &a, &a, &[]),
            list(1, &[], &[], &[]),
            list(1, &[], &[], &[]),
        ],
        vec![list(0, &a, &a, &[]), list(2, &[], &[], &[])],
        vec![list(0, &a, &a, &[]), list(255, &a, &a, &[])],
        vec![list(0, &a, &[], &[])],
        vec![list(0, &a, &b, &[]), list(1, &a, &a, &[])],
        vec![list(0, &a, &a, &[]), list(1, &b, &a, &[])],
    ];
    for lists in cases {
        let error = merge_query_variants(&normalized, &lists, &limits()).unwrap_err();
        assert!(matches!(&error, RetrievalError::InvalidQuery(_)));
        assert_eq!(error.kind(), ErrorKind::Permanent);
        for diagnostic in [error.to_string(), format!("{error:?}")] {
            assert!(!diagnostic.contains(secret));
            assert!(!diagnostic.contains("private-generated"));
            assert!(!diagnostic.contains(&a[0].to_string()));
            assert!(!diagnostic.contains(&b[0].to_string()));
        }
    }
}

#[test]
fn rejected_generated_result_cannot_be_reintroduced_by_forging_a_ranked_list() {
    let invalid = normalize_query_variants(
        "original",
        &strings(&["one", "two", "three", "four"]),
        4,
        &limits(),
    );
    let id = ids(&[10]);
    let lists = [list(0, &[], &[], &[]), list(1, &id, &id, &[])];
    let error = merge_query_variants(&invalid, &lists, &limits()).unwrap_err();
    assert_eq!(
        error.to_string(),
        "invalid query: unknown query variant list"
    );
}

#[test]
fn output_wire_contains_only_canonical_id_and_ordered_occurrences_never_scores() {
    let a = ids(&[10]);
    let lists = [list(0, &a, &a, &a), list(1, &a, &a, &[])];
    let admitted = merge_query_variants(&variants(), &lists, &limits())
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::to_value(&admitted).unwrap(),
        json!([{
            "id": "00000000-0000-0000-0000-00000000000a",
            "variant_attribution": [
                {"variant_id": 0, "source": "lexical", "source_rank": 1},
                {"variant_id": 0, "source": "ann", "source_rank": 1},
                {"variant_id": 1, "source": "lexical", "source_rank": 1}
            ]
        }])
    );
    let wire = serde_json::to_string(&admitted).unwrap();
    for forbidden in [
        "score",
        "rank_score",
        "relevance",
        "both",
        "original",
        "generated",
    ] {
        assert!(
            !wire.contains(forbidden),
            "unexpected field or value: {forbidden}"
        );
    }
}
