#[test]
fn syntax_errors_retain_toml_sources_and_reject_unknown_nested_fields() {
    for text in [
        "not toml",
        "mystery = true",
        "[[rules]]\nid='x'\nkind='entity'\nsevertiy='error'",
        "[edge_endpoint_types]\nallow_all=true",
        "[edge_direction_conventions]\nunknown=true",
        "[[edge_direction_conventions.relations]]\nrelation='contains'\nforward_source_kind=['concept']",
        "[dangling_refs]\nunknown=true",
        "[naming_conventions.kinds.concept]\nmax_lenght=3",
        "[citation_date_lint]\nfield=['year']",
    ] {
        let error = Rules::parse_toml(text).unwrap_err();
        assert!(matches!(&error, RulesError::Syntax(_)), "{text}: {error}");
        assert!(error.source().is_some());
    }
}

#[test]
fn direction_validation_is_ordered_and_runs_even_when_disabled() {
    let entries = [
        ("not-a-relation", "[]", "[]", "edge_direction_conventions.relations[0]: \"not-a-relation\" is not a valid edge relation"),
        ("contains", "[]", "[]", "edge_direction_conventions.relations[0] (\"contains\"): forward_source_kinds must be non-empty"),
        ("contains", "['concept']", "[]", "edge_direction_conventions.relations[0] (\"contains\"): forward_target_kinds must be non-empty"),
    ];
    for (relation, source, target, expected) in entries {
        let text = format!("[edge_direction_conventions]\nenabled=false\n[[edge_direction_conventions.relations]]\nrelation='{relation}'\nforward_source_kinds={source}\nforward_target_kinds={target}");
        let error = Rules::parse_toml(&text).unwrap_err();
        assert!(matches!(&error, RulesError::Direction(_)));
        assert_eq!(error.to_string(), expected);
        assert!(error.source().is_none());
    }
}

#[test]
fn host_requirements_only_activate_enabled_valid_sections() {
    for text in [
        "",
        "[edge_endpoint_types]\nenabled=false\n[citation_date_lint]\nenabled=false",
        "[edge_endpoint_types]\nseverity='typo'\n[citation_date_lint]\nseverity='typo'",
    ] {
        let rules = Rules::parse_toml(text).unwrap();
        assert!(!rules.needs_pack_edge_rules());
        assert!(!rules.needs_current_time());
    }
    let endpoint = Rules::parse_toml("[edge_endpoint_types]").unwrap();
    assert!(endpoint.needs_pack_edge_rules());
    assert!(!endpoint.needs_current_time());
    let citation = Rules::parse_toml("[citation_date_lint]").unwrap();
    assert!(!citation.needs_pack_edge_rules());
    assert!(citation.needs_current_time());
    let disabled = "[edge_endpoint_types]\nenabled=false\n[edge_direction_conventions]\nenabled=false\n[dangling_refs]\nenabled=false\n[naming_conventions]\nenabled=false\n[citation_date_lint]\nenabled=false";
    assert!(run(
        disabled,
        GOLDEN_ENTITIES,
        GOLDEN_EDGES,
        GOLDEN_NOTES,
        EvaluationMode::FullDataset,
        &[]
    )
    .is_empty());
}

#[test]
fn severity_errors_precede_kind_and_all_builtin_results_keep_order() {
    let config = r#"
[[rules]]
id="bad-severity"
kind="note"
severity="invalid"
[[rules]]
id="bad-kind"
kind="note"
[edge_endpoint_types]
severity="invalid"
[edge_direction_conventions]
severity="invalid"
[dangling_refs]
severity="invalid"
[naming_conventions]
severity="invalid"
[citation_date_lint]
severity="invalid"
"#;
    let results = run(config, "", "", "", EvaluationMode::PartialView, &[]);
    assert_eq!(
        results.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
        [
            "bad-severity",
            "bad-kind",
            "edge-endpoint-types",
            "edge-direction-conventions",
            "dangling-refs",
            "naming-conventions",
            "citation-date-lint"
        ]
    );
    assert!(results
        .iter()
        .all(|r| !r.passed && r.severity == "error" && r.violations.len() == 1));
    assert!(results[0].violations[0]
        .message
        .contains("invalid severity"));
    assert_eq!(
        results[1].violations[0].message,
        "Rule \"bad-kind\": unknown kind \"note\"; must be \"entity\" or \"edge\""
    );
}
