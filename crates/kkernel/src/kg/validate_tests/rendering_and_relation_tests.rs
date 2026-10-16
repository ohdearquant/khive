// ── Record identifier in rendered output ─────────────────────────

#[test]
fn violation_message_includes_entity_id_and_name() {
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    write_entities(
        &kg_dir,
        &[(
            "bbbbbbbb-0000-0000-0000-000000000002",
            "nonsense",
            "BadEntity",
        )],
    );
    let kinds = base_entity_kinds();
    let result = check_valid_entity_kinds(&kg_dir.join("entities.ndjson"), &kinds);
    assert!(!result.passed);
    let msg = &result.violations[0].message;
    assert!(
        msg.contains("bbbbbbbb-0000-0000-0000-000000000002"),
        "violation message must include entity id: {msg}"
    );
    assert!(
        msg.contains("BadEntity"),
        "violation message must include entity name: {msg}"
    );
}

#[test]
fn edge_violation_message_includes_source_target() {
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    write_entities(
        &kg_dir,
        &[
            ("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A"),
            ("bbbbbbbb-0000-0000-0000-000000000002", "concept", "B"),
        ],
    );
    write_edges(
        &kg_dir,
        &[(
            "aaaaaaaa-0000-0000-0000-000000000001",
            "bbbbbbbb-0000-0000-0000-000000000002",
            "not_a_real_relation",
        )],
    );
    let result = check_valid_edge_relations(&kg_dir.join("edges.ndjson"));
    assert!(!result.passed);
    let msg = &result.violations[0].message;
    assert!(
        msg.contains("aaaaaaaa-0000-0000-0000-000000000001"),
        "edge violation message must include source_id: {msg}"
    );
    assert!(
        msg.contains("bbbbbbbb-0000-0000-0000-000000000002"),
        "edge violation message must include target_id: {msg}"
    );
}

// ── Edge relation tests (preserved from prior PR) ─────────────────────────

#[test]
fn invalid_edge_relation_is_rejected() {
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    write_entities(
        &kg_dir,
        &[
            ("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A"),
            ("bbbbbbbb-0000-0000-0000-000000000002", "concept", "B"),
        ],
    );
    write_edges(
        &kg_dir,
        &[
            (
                "aaaaaaaa-0000-0000-0000-000000000001",
                "bbbbbbbb-0000-0000-0000-000000000002",
                "extends",
            ),
            (
                "aaaaaaaa-0000-0000-0000-000000000001",
                "bbbbbbbb-0000-0000-0000-000000000002",
                "not_a_real_relation",
            ),
        ],
    );
    let result = check_valid_edge_relations(&kg_dir.join("edges.ndjson"));
    assert!(!result.passed, "invalid edge relation must fail");
    assert_eq!(result.violations.len(), 1);
    assert!(
        result.violations[0].message.contains("not_a_real_relation"),
        "violation message should name the bad relation: {}",
        result.violations[0].message
    );
    assert!(
        result.violations[0].message.contains("extends"),
        "violation message should list valid relations: {}",
        result.violations[0].message
    );
}

#[test]
fn valid_edge_relations_all_pass() {
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    write_entities(
        &kg_dir,
        &[
            ("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A"),
            ("bbbbbbbb-0000-0000-0000-000000000002", "concept", "B"),
        ],
    );
    write_edges(
        &kg_dir,
        &[
            (
                "aaaaaaaa-0000-0000-0000-000000000001",
                "bbbbbbbb-0000-0000-0000-000000000002",
                "extends",
            ),
            (
                "aaaaaaaa-0000-0000-0000-000000000001",
                "bbbbbbbb-0000-0000-0000-000000000002",
                "variant_of",
            ),
        ],
    );
    let result = check_valid_edge_relations(&kg_dir.join("edges.ndjson"));
    assert!(result.passed, "valid edge relations must pass");
    assert!(result.violations.is_empty());
}

#[test]
fn structural_checks_include_taxonomy_rules() {
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    write_entities(
        &kg_dir,
        &[("aaaaaaaa-0000-0000-0000-000000000001", "nonsense", "Bad")],
    );
    write_edges(
        &kg_dir,
        &[(
            "aaaaaaaa-0000-0000-0000-000000000001",
            "aaaaaaaa-0000-0000-0000-000000000001",
            "not_valid",
        )],
    );
    let taxonomy = real_taxonomy();
    let results = structural_checks(
        &kg_dir.join("entities.ndjson"),
        &kg_dir.join("edges.ndjson"),
        &kg_dir.join("notes.ndjson"),
        &taxonomy,
    );
    let ids: Vec<&str> = results.iter().map(|r| r.id.as_str()).collect();
    assert!(
        ids.contains(&"valid-entity-kinds"),
        "structural_checks must include valid-entity-kinds"
    );
    assert!(
        ids.contains(&"valid-edge-relations"),
        "structural_checks must include valid-edge-relations"
    );
    let entity_kind_result = results
        .iter()
        .find(|r| r.id == "valid-entity-kinds")
        .unwrap();
    assert!(!entity_kind_result.passed, "nonsense kind must fail");
    let edge_rel_result = results
        .iter()
        .find(|r| r.id == "valid-edge-relations")
        .unwrap();
    assert!(!edge_rel_result.passed, "invalid relation must fail");
}
