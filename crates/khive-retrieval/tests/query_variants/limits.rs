#[test]
fn string_limits_count_raw_utf8_bytes_before_trim_and_dedup() {
    for exact in ["x".repeat(256), "é".repeat(128), "🦀".repeat(64)] {
        let normalized =
            normalize_query_variants("original", std::slice::from_ref(&exact), 1, &limits());
        assert_eq!(normalized.rejection(), None);
        assert_eq!(normalized.variants()[1].text, exact);
        assert_rejected(
            "original",
            &[format!("{exact}x")],
            1,
            &limits(),
            QueryVariantRejection::VariantByteLimit,
        );
    }
    assert_rejected(
        "x",
        &[format!("{}x", " ".repeat(256))],
        1,
        &limits(),
        QueryVariantRejection::VariantByteLimit,
    );

    let lower = QueryVariantLimits::try_new(3, 4, 1_024, 192, 127).unwrap();
    assert_eq!(
        normalize_query_variants("original", &strings(&[" é "]), 1, &lower).variants()[1].text,
        "é"
    );
    assert_rejected(
        "original",
        &strings(&["  é "]),
        1,
        &lower,
        QueryVariantRejection::VariantByteLimit,
    );
}

#[test]
fn aggregate_limits_count_duplicate_and_trimmed_away_bytes() {
    let exact = QueryVariantLimits::try_new(3, 256, 6, 192, 127).unwrap();
    let generated = strings(&[" x ", " x "]);
    let normalized = normalize_query_variants("x", &generated, 2, &exact);
    assert_eq!(normalized.rejection(), None);
    assert_eq!(normalized.variants().len(), 1);
    let lower = QueryVariantLimits::try_new(3, 256, 5, 192, 127).unwrap();
    assert_rejected(
        "x",
        &generated,
        2,
        &lower,
        QueryVariantRejection::AggregateByteLimit,
    );

    let full = vec!["a".repeat(256), "b".repeat(256), "c".repeat(256)];
    assert_eq!(
        normalize_query_variants("original", &full, 192, &limits())
            .variants()
            .len(),
        4
    );
}

#[test]
fn token_bound_is_measured_metadata_not_a_character_heuristic() {
    let generated = strings(&["query"]);
    assert_eq!(
        normalize_query_variants("original", &generated, 192, &limits()).rejection(),
        None
    );
    assert_rejected(
        "original",
        &generated,
        193,
        &limits(),
        QueryVariantRejection::OutputTokenLimit,
    );
    assert_rejected(
        "original",
        &[],
        193,
        &limits(),
        QueryVariantRejection::OutputTokenLimit,
    );
    let lower = QueryVariantLimits::try_new(3, 256, 1_024, 1, 127).unwrap();
    assert_rejected(
        "original",
        &generated,
        2,
        &lower,
        QueryVariantRejection::OutputTokenLimit,
    );
}

#[test]
fn limits_require_every_explicit_positive_value_within_its_ceiling() {
    let ceilings = [3, 256, 1_024, 192, 127];
    for index in 0..ceilings.len() {
        for invalid in [0, ceilings[index] + 1, usize::MAX] {
            let mut values = ceilings;
            values[index] = invalid;
            let error =
                QueryVariantLimits::try_new(values[0], values[1], values[2], values[3], values[4])
                    .unwrap_err();
            assert!(matches!(&error, RetrievalError::Configuration(_)));
            assert_eq!(error.kind(), ErrorKind::Permanent);
        }
    }
    for values in [ceilings, [1; 5]] {
        let checked =
            QueryVariantLimits::try_new(values[0], values[1], values[2], values[3], values[4])
                .unwrap();
        assert_eq!(
            [
                checked.max_generated_variants(),
                checked.max_variant_bytes(),
                checked.max_generated_bytes(),
                checked.max_output_tokens(),
                checked.max_fused_candidates()
            ],
            values
        );
    }
}

#[test]
fn empty_and_fully_deduplicated_results_are_valid_original_only_sets() {
    for generated in [vec![], strings(&["original", " original ", "original\n"])] {
        let normalized = normalize_query_variants("original", &generated, 0, &limits());
        assert_eq!(normalized.rejection(), None);
        assert_eq!(normalized.variants().len(), 1);
        assert_eq!(
            merge_query_variants(&normalized, &[list(0, &[], &[], &[])], &limits()).unwrap(),
            None
        );
    }
}
