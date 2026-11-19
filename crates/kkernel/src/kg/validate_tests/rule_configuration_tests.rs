// ── rules.toml wiring through configurable_rule_checks ────────────────────

#[test]
fn configurable_rule_checks_wires_up_edge_endpoint_types_from_toml() {
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    write_entities(
        &kg_dir,
        &[
            ("aaaaaaaa-0000-0000-0000-000000000001", "person", "A"),
            ("bbbbbbbb-0000-0000-0000-000000000002", "person", "B"),
        ],
    );
    write_edges(
        &kg_dir,
        &[(
            "aaaaaaaa-0000-0000-0000-000000000001",
            "bbbbbbbb-0000-0000-0000-000000000002",
            "extends",
        )],
    );
    let rules_path = tmp.path().join("rules.toml");
    std::fs::write(&rules_path, "[edge_endpoint_types]\nenabled = true\n").unwrap();

    let results = configurable_rule_checks(
        &kg_dir.join("entities.ndjson"),
        &kg_dir.join("edges.ndjson"),
        &kg_dir.join("notes.ndjson"),
        &rules_path,
    )
    .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].id, "edge-endpoint-types");
    // Default severity for this class is "error" (see default_severity_error).
    assert_eq!(results[0].severity, "error");
    assert!(!results[0].passed);
}

#[test]
fn configurable_rule_checks_section_absent_means_rule_does_not_run() {
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    write_entities(
        &kg_dir,
        &[("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A")],
    );
    std::fs::write(kg_dir.join("edges.ndjson"), "").unwrap();
    let rules_path = tmp.path().join("rules.toml");
    std::fs::write(&rules_path, "rules = []\n").unwrap();

    let results = configurable_rule_checks(
        &kg_dir.join("entities.ndjson"),
        &kg_dir.join("edges.ndjson"),
        &kg_dir.join("notes.ndjson"),
        &rules_path,
    )
    .unwrap();
    assert!(
        results.is_empty(),
        "no built-in rule-class sections declared → none run: {results:?}"
    );
}

#[test]
fn configurable_rule_checks_enabled_false_skips_the_rule() {
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    write_entities(
        &kg_dir,
        &[
            ("aaaaaaaa-0000-0000-0000-000000000001", "person", "A"),
            ("bbbbbbbb-0000-0000-0000-000000000002", "person", "B"),
        ],
    );
    write_edges(
        &kg_dir,
        &[(
            "aaaaaaaa-0000-0000-0000-000000000001",
            "bbbbbbbb-0000-0000-0000-000000000002",
            "extends",
        )],
    );
    let rules_path = tmp.path().join("rules.toml");
    std::fs::write(
        &rules_path,
        "[edge_endpoint_types]\nenabled = false\nseverity = \"error\"\n",
    )
    .unwrap();

    let results = configurable_rule_checks(
        &kg_dir.join("entities.ndjson"),
        &kg_dir.join("edges.ndjson"),
        &kg_dir.join("notes.ndjson"),
        &rules_path,
    )
    .unwrap();
    assert!(results.is_empty(), "enabled = false must skip evaluation");
}

#[test]
fn configurable_rule_checks_invalid_builtin_severity_produces_error_result() {
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    write_entities(
        &kg_dir,
        &[("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A")],
    );
    std::fs::write(kg_dir.join("edges.ndjson"), "").unwrap();
    let rules_path = tmp.path().join("rules.toml");
    std::fs::write(
        &rules_path,
        "[naming_conventions]\nseverity = \"catastrophic\"\n",
    )
    .unwrap();

    let results = configurable_rule_checks(
        &kg_dir.join("entities.ndjson"),
        &kg_dir.join("edges.ndjson"),
        &kg_dir.join("notes.ndjson"),
        &rules_path,
    )
    .unwrap();
    assert_eq!(results.len(), 1);
    assert!(!results[0].passed);
    assert_eq!(results[0].severity, "error");
    assert!(results[0].violations[0]
        .message
        .contains("invalid severity"));
}

#[test]
fn configurable_rule_checks_misspelled_key_fails_the_load() {
    // Regression for commit 4e11ee38: before
    // `#[serde(deny_unknown_fields)]`, a typo like `severtiy` was silently
    // ignored and the field fell back to its default — the class then ran
    // at the DEFAULT severity instead of failing loudly. Now the whole
    // `rules.toml` load must error.
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    write_entities(
        &kg_dir,
        &[("aaaaaaaa-0000-0000-0000-000000000001", "concept", " Bad ")],
    );
    std::fs::write(kg_dir.join("edges.ndjson"), "").unwrap();
    let rules_path = tmp.path().join("rules.toml");
    std::fs::write(&rules_path, "[naming_conventions]\nsevertiy = \"error\"\n").unwrap();

    let err = configurable_rule_checks(
        &kg_dir.join("entities.ndjson"),
        &kg_dir.join("edges.ndjson"),
        &kg_dir.join("notes.ndjson"),
        &rules_path,
    )
    .expect_err("a misspelled key must fail the rules.toml load, not silently default");
    assert!(
        format!("{err:#}").contains("severtiy") || format!("{err:#}").contains("unknown field"),
        "error must name the bad key: {err:#}"
    );
}

#[test]
fn configurable_rule_checks_malformed_direction_entry_fails_the_load() {
    // Regression: `forward_source_kind` (missing the trailing
    // `s`) is not a field `DirectionRuleConfig` recognizes. Before this
    // fix it silently parsed as an unrelated no-op (an entry with an
    // empty `forward_source_kinds` that `check_edge_direction_conventions`
    // skips), producing a green validation for a config that names no
    // real direction rule at all.
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    write_entities(
        &kg_dir,
        &[("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A")],
    );
    std::fs::write(kg_dir.join("edges.ndjson"), "").unwrap();
    let rules_path = tmp.path().join("rules.toml");
    std::fs::write(
        &rules_path,
        "[[edge_direction_conventions.relations]]\n\
             relation = \"introduced_by\"\n\
             forward_source_kind = [\"concept\"]\n\
             forward_target_kinds = [\"document\"]\n",
    )
    .unwrap();

    let err = configurable_rule_checks(
        &kg_dir.join("entities.ndjson"),
        &kg_dir.join("edges.ndjson"),
        &kg_dir.join("notes.ndjson"),
        &rules_path,
    )
    .expect_err("a misspelled direction-entry field must fail the rules.toml load");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("forward_source_kind") || msg.contains("unknown field"),
        "error must name the bad key: {msg}"
    );
}

#[test]
fn configurable_rule_checks_direction_entry_with_empty_kind_list_fails_the_load() {
    // Post-parse validation: a syntactically valid but
    // semantically empty `forward_source_kinds = []` must also fail the
    // load loudly, not silently no-op the whole entry.
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    write_entities(
        &kg_dir,
        &[("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A")],
    );
    std::fs::write(kg_dir.join("edges.ndjson"), "").unwrap();
    let rules_path = tmp.path().join("rules.toml");
    std::fs::write(
        &rules_path,
        "[[edge_direction_conventions.relations]]\n\
             relation = \"introduced_by\"\n\
             forward_source_kinds = []\n\
             forward_target_kinds = [\"document\"]\n",
    )
    .unwrap();

    let err = configurable_rule_checks(
        &kg_dir.join("entities.ndjson"),
        &kg_dir.join("edges.ndjson"),
        &kg_dir.join("notes.ndjson"),
        &rules_path,
    )
    .expect_err("an empty forward_source_kinds entry must fail the rules.toml load");
    assert!(
        format!("{err:#}").contains("forward_source_kinds"),
        "error must name the empty field: {err:#}"
    );
}
