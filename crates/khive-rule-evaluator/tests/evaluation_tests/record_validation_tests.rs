#[test]
fn naming_overrides_count_characters_and_preserve_violation_order() {
    let config = r#"
[naming_conventions]
max_length=3
[naming_conventions.kinds.project]
max_length=100
no_parenthetical_suffix=false
no_leading_trailing_whitespace=false
"#;
    let entities = r#"{"id":"blank","kind":"concept","name":"    "}
{"id":"bad","kind":"concept","name":" X (q) "}
{"id":"unicode","kind":"concept","name":"猫猫猫"}
{"id":"override","kind":"project","name":" X (q) "}
{"id":"missing","kind":"concept"}
"#;
    let results = run(config, entities, "", "", EvaluationMode::FullDataset, &[]);
    let findings = &results[0].violations;
    assert_eq!(findings.len(), 4);
    assert_eq!(
        findings[0].message,
        "[blank \"    \"] name is empty or whitespace-only"
    );
    assert_eq!(
        findings[1].message,
        "[bad \" X (q) \"] name has leading/trailing whitespace"
    );
    assert_eq!(findings[2].message, "[bad \" X (q) \"] name carries a parenthetical suffix; use `properties` for qualifiers instead of embedding them in `name`");
    assert_eq!(
        findings[3].message,
        "[bad \" X (q) \"] name exceeds max length 3 (7 chars)"
    );
}

#[test]
fn citation_fixed_clock_handles_strict_boundaries_offsets_shapes_and_substrate_order() {
    let config = r#"[citation_date_lint]
fields=["past","equal_year","equal_date","equal_time","future_year","future_text","future_date","future_time","boolean","array","float","too_long","invalid"]"#;
    let entities = r#"{"id":"e","kind":"document","properties":{"past":2025,"equal_year":2026,"equal_date":"2026-01-02","equal_time":"2026-01-02T13:00:00+01:00","future_year":2027,"future_text":" 2028 ","future_date":"2026-01-03","future_time":"2026-01-02T12:00:00.000000001Z","boolean":true,"array":[2027],"float":2027.5,"too_long":10000,"invalid":"tomorrow"}}"#;
    let notes = r#"{"id":"n","kind":"reference","properties":{"future_year":"2029"}}"#;
    let results = run(
        config,
        entities,
        "",
        notes,
        EvaluationMode::FullDataset,
        &[],
    );
    let findings = &results[0].violations;
    assert_eq!(findings.iter().map(|v| v.message.as_str()).collect::<Vec<_>>(), [
        "[e] property \"future_year\": year 2027 is after the current year 2026",
        "[e] property \"future_text\": year 2028 is after the current year 2026",
        "[e] property \"future_date\": date 2026-01-03 is in the future (validated at 2026-01-02 12:00:00 UTC)",
        "[e] property \"future_time\": date 2026-01-02T12:00:00.000000001Z is in the future (validated at 2026-01-02 12:00:00 UTC)",
        "[n] property \"future_year\": year 2029 is after the current year 2026",
    ]);
    let rules = Rules::parse_toml(config).unwrap();
    let later = evaluate(
        &rules,
        NdjsonState {
            entities,
            edges: "",
            notes,
        },
        EvaluationContext {
            mode: EvaluationMode::FullDataset,
            pack_edge_rules: &[],
            now: DateTime::parse_from_rfc3339("2030-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        },
    );
    assert!(
        later[0].passed,
        "the supplied instant, not an ambient clock, controls every field"
    );
}

#[test]
fn empty_and_malformed_rows_remain_the_structural_pass_responsibility() {
    for input in ["", "\n\n", "malformed\nnull\n[]\n42\n{}"] {
        let results = run("[edge_endpoint_types]\n[edge_direction_conventions]\n[dangling_refs]\n[naming_conventions]\n[citation_date_lint]", input, input, input, EvaluationMode::FullDataset, &[]);
        assert_eq!(results.len(), 5);
        assert!(results.iter().all(|r| r.passed && r.violations.is_empty()));
    }
}

#[test]
fn record_prefix_retains_debug_escaping_and_all_presence_cases() {
    assert_eq!(record_prefix(Some("id"), Some("a\nb")), "[id \"a\\nb\"] ");
    assert_eq!(record_prefix(Some("id"), None), "[id] ");
    assert_eq!(record_prefix(None, Some("name")), "[\"name\"] ");
    assert_eq!(record_prefix(None, None), "");
}
