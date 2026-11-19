// ── Entity-kind registry source of truth ───────────────────────────

#[test]
fn invalid_entity_kind_is_rejected() {
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    write_entities(
        &kg_dir,
        &[
            ("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A"),
            ("bbbbbbbb-0000-0000-0000-000000000002", "nonsense", "B"),
        ],
    );
    let kinds = base_entity_kinds();
    let result = check_valid_entity_kinds(&kg_dir.join("entities.ndjson"), &kinds);
    assert!(!result.passed, "invalid entity kind must fail");
    assert_eq!(result.violations.len(), 1);
    assert!(
        result.violations[0].message.contains("nonsense"),
        "violation message should name the bad kind: {}",
        result.violations[0].message
    );
    assert!(
        result.violations[0].message.contains("concept"),
        "violation message should list valid kinds: {}",
        result.violations[0].message
    );
}

#[test]
fn resource_kind_is_accepted_as_pack_registered() {
    // ADR-048: `resource` is registered by the KG pack and must not be rejected.
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    write_entities(
        &kg_dir,
        &[
            ("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A"),
            ("bbbbbbbb-0000-0000-0000-000000000002", "resource", "R"),
        ],
    );
    let taxonomy = real_taxonomy();
    assert!(
        taxonomy.entity_kinds.contains("resource"),
        "VerbRegistry must include 'resource' from KG pack (ADR-048)"
    );
    let result = check_valid_entity_kinds(&kg_dir.join("entities.ndjson"), &taxonomy.entity_kinds);
    assert!(
        result.passed,
        "pack-registered kind 'resource' must pass; violations: {:?}",
        result.violations
    );
    assert!(result.violations.is_empty());
}

#[test]
fn valid_entity_kinds_all_pass() {
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    write_entities(
        &kg_dir,
        &[
            ("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A"),
            ("bbbbbbbb-0000-0000-0000-000000000002", "document", "B"),
            ("cccccccc-0000-0000-0000-000000000003", "dataset", "C"),
            ("dddddddd-0000-0000-0000-000000000004", "project", "D"),
            ("eeeeeeee-0000-0000-0000-000000000005", "person", "E"),
            ("ffffffff-0000-0000-0000-000000000006", "org", "F"),
            ("11111111-0000-0000-0000-000000000007", "artifact", "G"),
            ("22222222-0000-0000-0000-000000000008", "service", "H"),
        ],
    );
    let kinds = base_entity_kinds();
    let result = check_valid_entity_kinds(&kg_dir.join("entities.ndjson"), &kinds);
    assert!(result.passed, "all 8 canonical kinds must pass");
    assert!(result.violations.is_empty());
}

// ── Note-kind validation ───────────────────────────────────────────

#[test]
fn invalid_note_kind_is_rejected() {
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    write_notes(
        &kg_dir,
        &[("note-0001", "observation"), ("note-0002", "bogus_kind")],
    );
    let kinds = base_note_kinds();
    let result = check_valid_note_kinds(&kg_dir.join("notes.ndjson"), &kinds);
    assert!(!result.passed, "invalid note kind must fail");
    assert_eq!(result.violations.len(), 1);
    assert!(
        result.violations[0].message.contains("bogus_kind"),
        "violation message should name the bad kind: {}",
        result.violations[0].message
    );
    assert!(
        result.violations[0].message.contains("observation"),
        "violation message should list valid kinds: {}",
        result.violations[0].message
    );
}

#[test]
fn valid_note_kinds_all_pass() {
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    write_notes(
        &kg_dir,
        &[
            ("note-0001", "observation"),
            ("note-0002", "insight"),
            ("note-0003", "question"),
            ("note-0004", "decision"),
            ("note-0005", "reference"),
            ("note-0006", "task"),
            ("note-0007", "memory"),
        ],
    );
    let kinds = base_note_kinds();
    let result = check_valid_note_kinds(&kg_dir.join("notes.ndjson"), &kinds);
    assert!(result.passed, "all registered note kinds must pass");
    assert!(result.violations.is_empty());
}

#[test]
fn note_kind_task_is_accepted_as_pack_registered() {
    // `task` is registered by the GTD pack — must be accepted by the registry check.
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    write_notes(&kg_dir, &[("note-0001", "task")]);
    let taxonomy = real_taxonomy();
    assert!(
        taxonomy.note_kinds.contains("task"),
        "VerbRegistry must include 'task' from GTD pack"
    );
    let result = check_valid_note_kinds(&kg_dir.join("notes.ndjson"), &taxonomy.note_kinds);
    assert!(
        result.passed,
        "pack-registered note kind 'task' must pass; violations: {:?}",
        result.violations
    );
}

#[test]
fn note_kind_memory_is_accepted_as_pack_registered() {
    // `memory` is registered by the memory pack — must be accepted by the registry check.
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    write_notes(&kg_dir, &[("note-0001", "memory")]);
    let taxonomy = real_taxonomy();
    assert!(
        taxonomy.note_kinds.contains("memory"),
        "VerbRegistry must include 'memory' from memory pack"
    );
    let result = check_valid_note_kinds(&kg_dir.join("notes.ndjson"), &taxonomy.note_kinds);
    assert!(
        result.passed,
        "pack-registered note kind 'memory' must pass; violations: {:?}",
        result.violations
    );
}

#[test]
fn structural_checks_skips_note_check_when_notes_file_absent() {
    // Without notes.ndjson present, structural_checks must not add a note-kind rule.
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    write_entities(
        &kg_dir,
        &[("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A")],
    );
    std::fs::write(kg_dir.join("edges.ndjson"), "").unwrap();
    let taxonomy = real_taxonomy();
    let results = structural_checks(
        &kg_dir.join("entities.ndjson"),
        &kg_dir.join("edges.ndjson"),
        &kg_dir.join("notes.ndjson"),
        &taxonomy,
    );
    let ids: Vec<&str> = results.iter().map(|r| r.id.as_str()).collect();
    assert!(
        !ids.contains(&"valid-note-kinds"),
        "valid-note-kinds must not appear when notes.ndjson is absent"
    );
}

#[test]
fn structural_checks_includes_note_check_when_notes_file_present() {
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    write_entities(
        &kg_dir,
        &[("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A")],
    );
    std::fs::write(kg_dir.join("edges.ndjson"), "").unwrap();
    write_notes(&kg_dir, &[("note-0001", "observation")]);
    let taxonomy = real_taxonomy();
    let results = structural_checks(
        &kg_dir.join("entities.ndjson"),
        &kg_dir.join("edges.ndjson"),
        &kg_dir.join("notes.ndjson"),
        &taxonomy,
    );
    let ids: Vec<&str> = results.iter().map(|r| r.id.as_str()).collect();
    assert!(
        ids.contains(&"valid-note-kinds"),
        "valid-note-kinds must appear when notes.ndjson is present"
    );
}
