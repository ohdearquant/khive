use super::tag_contains;

#[test]
fn decoded_elements_and_legacy_text_have_distinct_matching_rules() {
    for tags in [
        r#"["type:domain"]"#,
        r#"["ordinary","type:domain"]"#,
        r#"["type\u003adomain"]"#,
        "broken type:domain",
        r#""type:domain""#,
        r#"{"tag":"type:domain"}"#,
        r#"["type:domain-extra",7]"#,
    ] {
        assert!(tag_contains(tags, "type:domain"), "{tags}");
    }
    for tags in [
        "[]",
        r#"["ordinary"]"#,
        r#"["prefix:type:domain"]"#,
        r#"["type:domain-extra"]"#,
        r#"["TYPE:DOMAIN"]"#,
        r#"[" type:domain "]"#,
        "broken",
        r#""ordinary""#,
        "{}",
        r#"["ordinary",7]"#,
        "null",
        "",
    ] {
        assert!(!tag_contains(tags, "type:domain"), "{tags}");
    }
    assert!(tag_contains(r#"["a\\b","c\"d"]"#, "a\\b"));
    assert!(tag_contains(r#"["a\\b","c\"d"]"#, "c\"d"));
    assert!(!tag_contains(r#"["type:domain"]"#, "type:"));
}
