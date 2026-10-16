use khive_retrieval::{
    merge_query_variants, normalize_query_variants, reciprocal_rank_fusion, AdmittedVariantHit,
    ErrorKind, QueryVariantLimits, QueryVariantOrigin, QueryVariantRejection, QueryVariants,
    RetrievalError, VariantAttribution, VariantRankedList, VariantSource,
};
use khive_score::DeterministicScore;
use serde_json::json;
use std::num::NonZeroUsize;
use uuid::Uuid;

fn limits() -> QueryVariantLimits {
    QueryVariantLimits::try_new(3, 256, 1_024, 192, 127).unwrap()
}

fn strings(texts: &[&str]) -> Vec<String> {
    texts.iter().map(|text| (*text).to_owned()).collect()
}

fn variants() -> QueryVariants {
    normalize_query_variants(
        "original",
        &strings(&["one", "two", "three"]),
        10,
        &limits(),
    )
}

fn ids(numbers: &[u128]) -> Vec<Uuid> {
    numbers.iter().copied().map(Uuid::from_u128).collect()
}

fn list<'a>(
    variant_id: u8,
    ranked_ids: &'a [Uuid],
    lexical_ids: &'a [Uuid],
    ann_ids: &'a [Uuid],
) -> VariantRankedList<'a> {
    VariantRankedList {
        variant_id,
        ranked_ids,
        lexical_ids,
        ann_ids,
    }
}

fn order(hits: &[AdmittedVariantHit]) -> Vec<Uuid> {
    hits.iter().map(|hit| hit.id).collect()
}

fn occurrence(variant_id: u8, source: VariantSource, source_rank: usize) -> VariantAttribution {
    VariantAttribution {
        variant_id,
        source,
        source_rank: NonZeroUsize::new(source_rank).unwrap(),
    }
}

fn assert_rejected(
    original: &str,
    generated: &[String],
    tokens: usize,
    limits: &QueryVariantLimits,
    reason: QueryVariantRejection,
) {
    let normalized = normalize_query_variants(original, generated, tokens, limits);
    assert_eq!(normalized.rejection(), Some(reason));
    assert_eq!(
        serde_json::to_value(normalized.variants()).unwrap(),
        json!([{"id": 0, "origin": "original", "text": original}])
    );
    assert_eq!(
        merge_query_variants(&normalized, &[list(0, &[], &[], &[])], limits).unwrap(),
        None
    );
}

#[test]
fn original_bytes_are_preserved_and_never_receive_generated_limits() {
    let original = format!(" \t\r\n\u{a0}UPPER\0{} e\u{301}\u{a0}\n ", "x".repeat(300));
    let normalized = normalize_query_variants(&original, &[], 0, &limits());
    assert_eq!(normalized.rejection(), None);
    assert_eq!(normalized.variants().len(), 1);
    assert_eq!(
        normalized.variants()[0].text.as_bytes(),
        original.as_bytes()
    );
    assert_eq!(normalized.variants()[0].id, 0);
    assert_eq!(
        normalized.variants()[0].origin,
        QueryVariantOrigin::Original
    );

    let original = " original ";
    let normalized = normalize_query_variants(original, &strings(&[" original "]), 1, &limits());
    assert_eq!(normalized.variants()[0].text, original);
    assert_eq!(normalized.variants()[1].text, "original");
}

#[test]
fn ascii_trim_and_exact_dedup_keep_provider_order_with_dense_ids() {
    let normalized = normalize_query_variants(
        "original",
        &strings(&[" \toriginal\r\n", " \rsecond\t ", "second\n"]),
        3,
        &limits(),
    );
    assert_eq!(normalized.rejection(), None);
    assert_eq!(
        serde_json::to_value(normalized.variants()).unwrap(),
        json!([
            {"id": 0, "origin": "original", "text": "original"},
            {"id": 1, "origin": "generated", "text": "second"}
        ])
    );

    let normalized = normalize_query_variants(
        "original",
        &strings(&[" \nz-last\r ", "a-first", "m-middle"]),
        3,
        &limits(),
    );
    let rows: Vec<_> = normalized
        .variants()
        .iter()
        .map(|variant| (variant.id, variant.text.as_str()))
        .collect();
    assert_eq!(
        rows,
        vec![
            (0, "original"),
            (1, "z-last"),
            (2, "a-first"),
            (3, "m-middle")
        ]
    );
}

#[test]
fn non_ascii_whitespace_case_internal_bytes_and_unicode_forms_stay_distinct() {
    for candidates in [
        ["\u{a0}word\u{a0}", "\u{b}word\u{b}", "\u{c}word\u{c}"],
        ["Word", "word  word", "word\tword"],
        ["é", "e\u{301}", "\u{a0}"],
    ] {
        let normalized = normalize_query_variants("word", &strings(&candidates), 3, &limits());
        assert_eq!(normalized.rejection(), None);
        assert_eq!(normalized.variants().len(), 4);
        for (variant, expected) in normalized.variants()[1..].iter().zip(candidates) {
            assert_eq!(variant.text.as_bytes(), expected.as_bytes());
        }
    }
}

#[test]
fn fourth_raw_item_rejects_everything_even_when_dedup_would_remove_it() {
    for generated in [
        strings(&["one", "two", "three", "four"]),
        strings(&["same", "same", "same", "same"]),
        strings(&["original", " original ", "\toriginal", "original\n"]),
        strings(&["", "", "", ""]),
    ] {
        assert_rejected(
            "original",
            &generated,
            4,
            &limits(),
            QueryVariantRejection::TooManyItems,
        );
    }

    let lower = QueryVariantLimits::try_new(1, 256, 1_024, 192, 127).unwrap();
    assert_rejected(
        "original",
        &strings(&["original", "original"]),
        2,
        &lower,
        QueryVariantRejection::TooManyItems,
    );
}

#[test]
fn invalid_item_at_any_position_discards_all_other_generated_items() {
    for (invalid, reason) in [
        ("".to_owned(), QueryVariantRejection::EmptyVariant),
        (" \t\r\n".to_owned(), QueryVariantRejection::EmptyVariant),
        ("safe\0secret".to_owned(), QueryVariantRejection::NulByte),
        ("x".repeat(257), QueryVariantRejection::VariantByteLimit),
    ] {
        for index in 0..3 {
            let mut generated = strings(&["one", "two", "three"]);
            generated[index] = invalid.clone();
            assert_rejected("original", &generated, 3, &limits(), reason);
        }
    }
}

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

#[test]
fn worked_rrf60_example_votes_once_per_variant_and_matches_shared_primitive() {
    // B: 1/61 + 1/62; A: 1/61 + 1/63; D: 1/62; C: 1/64.
    // A's duplicate retains C's rank four, and A's dual-channel provenance adds no outer vote.
    let original = ids(&[10, 20, 10, 30]);
    let generated = ids(&[20, 40, 10]);
    let ann = ids(&[10]);
    let lists = [
        list(0, &original, &original, &ann),
        list(1, &generated, &generated, &[]),
    ];
    let admitted = merge_query_variants(&variants(), &lists, &limits())
        .unwrap()
        .unwrap();
    assert_eq!(order(&admitted), ids(&[20, 10, 40, 30]));
    let sources = [&original, &generated]
        .into_iter()
        .map(|source| {
            source
                .iter()
                .map(|id| (*id, DeterministicScore::ZERO))
                .collect()
        })
        .collect();
    let expected: Vec<_> = reciprocal_rank_fusion(sources, 60)
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    assert_eq!(order(&admitted), expected);

    let without_ann = [list(0, &original, &original, &[]), lists[1]];
    let without_ann = merge_query_variants(&variants(), &without_ann, &limits())
        .unwrap()
        .unwrap();
    assert_eq!(order(&admitted), order(&without_ann));
    assert_ne!(
        admitted[1].variant_attribution,
        without_ann[1].variant_attribution
    );
}

#[test]
fn duplicate_ranks_do_not_compress_later_candidates() {
    let repeated = ids(&[30, 30, 10]);
    let second = ids(&[30, 20]);
    let lists = [
        list(0, &repeated, &repeated, &[]),
        list(1, &second, &second, &[]),
    ];
    let admitted = merge_query_variants(&variants(), &lists, &limits())
        .unwrap()
        .unwrap();
    // 20's rank two beats 10's rank three; compression would tie them and put UUID 10 first.
    assert_eq!(order(&admitted), ids(&[30, 20, 10]));
}

#[test]
fn admission_uses_rrf60_rather_than_a_different_rrf_constant() {
    let a = Uuid::from_u128(900);
    let b = Uuid::from_u128(800);
    let mut original: Vec<_> = (10_000..10_100).map(Uuid::from_u128).collect();
    let mut generated: Vec<_> = (20_000..20_100).map(Uuid::from_u128).collect();
    original[0] = a;
    original[19] = b;
    generated[19] = b;
    generated[99] = a;
    let lists = [
        list(0, &original, &original, &[]),
        list(1, &generated, &generated, &[]),
    ];
    let cap = QueryVariantLimits::try_new(3, 256, 1_024, 192, 2).unwrap();
    let admitted = merge_query_variants(&variants(), &lists, &cap)
        .unwrap()
        .unwrap();
    // k=60: B's 2/80 exceeds A's 1/61 + 1/160; k=10 or k=1 would reverse them.
    // Every other ID occurs only once, so its contribution is below both A and B.
    assert_eq!(order(&admitted), vec![b, a]);
}

fn permutations<T: Copy>(items: &mut [T], start: usize, visit: &mut impl FnMut(&[T])) {
    if start == items.len() {
        visit(items);
    } else {
        for index in start..items.len() {
            items.swap(start, index);
            permutations(items, start + 1, visit);
            items.swap(start, index);
        }
    }
}

#[test]
fn every_outer_permutation_preserves_ids_occurrences_and_serialized_bytes() {
    let a = ids(&[4, 1, 9]);
    let b = ids(&[1, 4, 9]);
    let c = ids(&[9, 2]);
    let d = ids(&[2, 9]);
    let traces = [
        list(0, &a, &a, &b),
        list(1, &b, &b, &a),
        list(2, &c, &[], &c),
        list(3, &d, &d, &[]),
    ];
    for count in 1..=4 {
        let mut lists = traces[..count].to_vec();
        let expected = merge_query_variants(&variants(), &lists, &limits()).unwrap();
        let bytes = serde_json::to_vec(&expected).unwrap();
        let mut visited = 0;
        permutations(&mut lists, 0, &mut |permuted| {
            let actual = merge_query_variants(&variants(), permuted, &limits()).unwrap();
            assert_eq!(actual, expected);
            assert_eq!(serde_json::to_vec(&actual).unwrap(), bytes);
            visited += 1;
        });
        assert_eq!(visited, (1..=count).product::<usize>());
    }
}

#[test]
fn equal_rrf_scores_use_canonical_uuid_order_including_across_uuid_fields() {
    let low = Uuid::parse_str("00000000-ffff-ffff-ffff-ffffffffffff").unwrap();
    let high = Uuid::parse_str("00000001-0000-0000-0000-000000000000").unwrap();
    let a = [high, low];
    let b = [low, high];
    let admitted = merge_query_variants(
        &variants(),
        &[list(0, &a, &a, &[]), list(1, &b, &b, &[])],
        &limits(),
    )
    .unwrap()
    .unwrap();
    assert!(low.to_string() < high.to_string());
    assert_eq!(order(&admitted), vec![low, high]);
}

#[test]
fn changing_inner_rank_order_is_a_different_input() {
    let forward = ids(&[20, 10]);
    let reverse = ids(&[10, 20]);
    let forward_hits = merge_query_variants(
        &variants(),
        &[list(0, &[], &[], &[]), list(1, &forward, &forward, &[])],
        &limits(),
    )
    .unwrap()
    .unwrap();
    let reverse_hits = merge_query_variants(
        &variants(),
        &[list(0, &[], &[], &[]), list(1, &reverse, &reverse, &[])],
        &limits(),
    )
    .unwrap()
    .unwrap();
    assert_eq!(order(&forward_hits), forward);
    assert_eq!(order(&reverse_hits), reverse);
    assert_ne!(forward_hits, reverse_hits);
}

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
