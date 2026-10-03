use khive_pack_memory::scoring::contains_cjk;

#[test]
fn cjk_boundary_keeps_empty_pure_and_strict_fifteen_percent_classification() {
    for (query, expected) in [
        ("", false),
        ("ordinary latin query", false),
        ("漢字かな한글", true),
        ("界界界abcdefghijklmnopq", false),
        ("界界界abcdefghijklmnopqr", false),
        ("界界界abcdefghijklmnop", true),
    ] {
        assert_eq!(contains_cjk(query), expected, "query={query:?}");
        assert_eq!(
            khive_text::contains_cjk(query),
            expected,
            "shared small boundary query={query:?}"
        );
    }
}

#[test]
fn cjk_near_boundary_preserves_local_f32_classification() {
    let query = format!("{}{}", "界".repeat(600_002), "a".repeat(3_400_011));
    assert_eq!(query.chars().count(), 4_000_013);
    assert!(
        !contains_cjk(&query),
        "local f32 threshold classification changed"
    );
    assert!(
        khive_text::contains_cjk(&query),
        "shared f64 comparison must expose the precision difference"
    );
}
