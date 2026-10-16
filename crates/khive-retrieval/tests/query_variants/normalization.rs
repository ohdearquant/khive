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
