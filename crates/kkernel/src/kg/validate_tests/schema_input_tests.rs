// ── Schema-compliance tests (#437) ────────────────────────────────────────

#[test]
fn schema_compliance_rejects_malformed_entities_ndjson() {
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    std::fs::write(kg_dir.join("entities.ndjson"), "not-valid-json\n").unwrap();
    std::fs::write(kg_dir.join("edges.ndjson"), "").unwrap();

    let taxonomy = KgTaxonomy {
        entity_kinds: base_entity_kinds(),
        note_kinds: base_note_kinds(),
    };
    let results = structural_checks(
        &kg_dir.join("entities.ndjson"),
        &kg_dir.join("edges.ndjson"),
        &kg_dir.join("notes.ndjson"),
        &taxonomy,
    );

    let schema_rule = results
        .iter()
        .find(|r| r.id == "schema-compliance")
        .expect("schema-compliance rule must always run");
    assert!(
        !schema_rule.passed,
        "malformed NDJSON must fail schema-compliance"
    );
    assert!(
        schema_rule.violations[0]
            .message
            .contains("entities.ndjson line 1"),
        "violation must point at the malformed line: {}",
        schema_rule.violations[0].message
    );
}

#[test]
fn schema_compliance_passes_well_formed_kg_and_absent_notes() {
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    write_entities(
        &kg_dir,
        &[("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A")],
    );
    write_edges(&kg_dir, &[]);

    let result = check_schema_compliance(
        &kg_dir.join("entities.ndjson"),
        &kg_dir.join("edges.ndjson"),
        &kg_dir.join("notes.ndjson"),
    );
    assert!(
        result.passed,
        "well-formed KG with absent notes.ndjson must pass: {:?}",
        result.violations
    );
}

#[test]
fn required_input_files_reject_missing_and_unreadable_paths() {
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    let entities_path = kg_dir.join("entities.ndjson");
    let edges_path = kg_dir.join("edges.ndjson");
    std::fs::create_dir(&entities_path).unwrap();

    let result =
        check_required_input_files(&entities_path, &edges_path, &kg_dir.join("notes.ndjson"));

    assert!(!result.passed);
    assert_eq!(result.violations.len(), 2);
    assert!(result
        .violations
        .iter()
        .any(|violation| violation.message.contains("entities.ndjson")));
    assert!(result
        .violations
        .iter()
        .any(|violation| violation.message.contains("edges.ndjson")));
}

#[test]
fn required_input_files_rejects_unreadable_optional_notes_when_present() {
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    let entities_path = kg_dir.join("entities.ndjson");
    let edges_path = kg_dir.join("edges.ndjson");
    let notes_path = kg_dir.join("notes.ndjson");
    std::fs::write(&entities_path, "").unwrap();
    std::fs::write(&edges_path, "").unwrap();
    std::fs::write(&notes_path, [0xff, 0xfe]).unwrap();

    let result = check_required_input_files(&entities_path, &edges_path, &notes_path);

    assert!(!result.passed);
    assert_eq!(result.violations.len(), 1);
    assert!(result.violations[0].message.contains("notes.ndjson"));
    assert!(result.violations[0].message.contains("when present"));
}

#[test]
fn fix_sort_order_refuses_malformed_json() {
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    let path = kg_dir.join("entities.ndjson");
    std::fs::write(&path, "not-valid-json\n").unwrap();

    let err = fix_sort_order(&path, "id").expect_err("fix must refuse malformed JSON");
    assert!(err.to_string().contains("line 1"));
    // The file must be left untouched, not truncated/dropped.
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "not-valid-json\n");
}
