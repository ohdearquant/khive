#[test]
fn configurable_rule_checks_empty_rules_file_returns_no_results() {
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
    assert!(results.is_empty(), "no rules → no results");
}

#[test]
fn configurable_rule_checks_require_field_detects_missing_description() {
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);

    let entities = r#"{"id":"aaa1","kind":"concept","name":"A","description":"has one"}
{"id":"aaa2","kind":"concept","name":"B"}
"#;
    std::fs::write(kg_dir.join("entities.ndjson"), entities).unwrap();
    std::fs::write(kg_dir.join("edges.ndjson"), "").unwrap();

    let rules_toml = r#"
[[rules]]
id = "concept-must-have-description"
severity = "warning"
kind = "entity"
condition = "kind=concept"
require_field = "description"
message = "Concept {id} missing description"
"#;
    let rules_path = tmp.path().join("rules.toml");
    std::fs::write(&rules_path, rules_toml).unwrap();

    let results = configurable_rule_checks(
        &kg_dir.join("entities.ndjson"),
        &kg_dir.join("edges.ndjson"),
        &kg_dir.join("notes.ndjson"),
        &rules_path,
    )
    .unwrap();

    assert_eq!(results.len(), 1);
    let r = &results[0];
    assert_eq!(r.id, "concept-must-have-description");
    assert!(
        !r.passed,
        "rule should fail when a concept lacks description"
    );
    assert_eq!(r.violations.len(), 1);
    assert_eq!(r.violations[0].entity_id.as_deref(), Some("aaa2"));
}

#[test]
fn configurable_rule_checks_self_loop_sentinel_detects_loop() {
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);

    write_entities(
        &kg_dir,
        &[
            ("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A"),
            ("bbbbbbbb-0000-0000-0000-000000000002", "concept", "B"),
        ],
    );
    let edges = r#"{"source_id":"aaaaaaaa-0000-0000-0000-000000000001","target_id":"aaaaaaaa-0000-0000-0000-000000000001","relation":"extends"}
{"source_id":"aaaaaaaa-0000-0000-0000-000000000001","target_id":"bbbbbbbb-0000-0000-0000-000000000002","relation":"extends"}
"#;
    std::fs::write(kg_dir.join("edges.ndjson"), edges).unwrap();

    let rules_toml = r#"
[[rules]]
id = "no-self-loops"
severity = "error"
kind = "edge"
condition = "source_id=target_id"
message = "Self-loop detected on {id}"
"#;
    let rules_path = tmp.path().join("rules.toml");
    std::fs::write(&rules_path, rules_toml).unwrap();

    let results = configurable_rule_checks(
        &kg_dir.join("entities.ndjson"),
        &kg_dir.join("edges.ndjson"),
        &kg_dir.join("notes.ndjson"),
        &rules_path,
    )
    .unwrap();

    assert_eq!(results.len(), 1);
    let r = &results[0];
    assert!(!r.passed);
    assert_eq!(r.violations.len(), 1, "exactly one self-loop");
}

#[test]
fn configurable_rule_checks_yaml_extension_returns_error() {
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    write_entities(
        &kg_dir,
        &[("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A")],
    );
    std::fs::write(kg_dir.join("edges.ndjson"), "").unwrap();

    let rules_path = tmp.path().join("rules.yaml");
    std::fs::write(&rules_path, "rules: []\n").unwrap();

    let result = configurable_rule_checks(
        &kg_dir.join("entities.ndjson"),
        &kg_dir.join("edges.ndjson"),
        &kg_dir.join("notes.ndjson"),
        &rules_path,
    );
    assert!(result.is_err(), "YAML extension must return an error");
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("YAML") || msg.contains("toml"),
        "error message should mention TOML: {msg}"
    );
}

#[test]
fn configurable_rule_checks_unknown_kind_produces_error_result() {
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    write_entities(
        &kg_dir,
        &[("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A")],
    );
    std::fs::write(kg_dir.join("edges.ndjson"), "").unwrap();

    let rules_toml = r#"
[[rules]]
id = "bad-kind"
severity = "error"
kind = "note"
condition = "kind=concept"
require_field = "description"
message = "bad"
"#;
    let rules_path = tmp.path().join("rules.toml");
    std::fs::write(&rules_path, rules_toml).unwrap();

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
}

#[test]
fn configurable_rule_checks_invalid_severity_produces_error_result() {
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    write_entities(
        &kg_dir,
        &[("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A")],
    );
    std::fs::write(kg_dir.join("edges.ndjson"), "").unwrap();

    let rules_toml = r#"
[[rules]]
id = "bad-severity"
severity = "erorr"
kind = "entity"
require_field = "description"
message = "bad"
"#;
    let rules_path = tmp.path().join("rules.toml");
    std::fs::write(&rules_path, rules_toml).unwrap();

    let results = configurable_rule_checks(
        &kg_dir.join("entities.ndjson"),
        &kg_dir.join("edges.ndjson"),
        &kg_dir.join("notes.ndjson"),
        &rules_path,
    )
    .unwrap();
    assert_eq!(results.len(), 1);
    assert!(!results[0].passed, "invalid severity must fail");
    assert_eq!(results[0].severity, "error");
    assert!(
        results[0].violations[0]
            .message
            .contains("invalid severity"),
        "error message should mention invalid severity"
    );
}

#[test]
fn sort_order_fix_sorts_entities() {
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    write_entities(
        &kg_dir,
        &[
            ("cccccccc-0000-0000-0000-000000000003", "concept", "C"),
            ("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A"),
            ("bbbbbbbb-0000-0000-0000-000000000002", "concept", "B"),
        ],
    );
    std::fs::write(kg_dir.join("edges.ndjson"), "").unwrap();
    fix_sort_order(&kg_dir.join("entities.ndjson"), "id").unwrap();
    let result = check_sort_order(
        &kg_dir.join("entities.ndjson"),
        &kg_dir.join("edges.ndjson"),
    );
    assert!(result.passed, "sort-order should pass after fix");
}
