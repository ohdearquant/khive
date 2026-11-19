// ── Entity kind tests ─────────────────────────────────────────────────────

#[test]
fn duplicate_uuid_detected() {
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    write_entities(
        &kg_dir,
        &[
            ("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A"),
            ("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A-dup"),
        ],
    );
    let result = check_no_duplicate_uuids(&kg_dir.join("entities.ndjson"));
    assert!(!result.passed, "duplicate UUID should fail");
    assert_eq!(result.violations.len(), 1);
}

#[test]
fn no_duplicates_passes() {
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    write_entities(
        &kg_dir,
        &[
            ("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A"),
            ("bbbbbbbb-0000-0000-0000-000000000002", "concept", "B"),
        ],
    );
    let result = check_no_duplicate_uuids(&kg_dir.join("entities.ndjson"));
    assert!(result.passed);
}

#[test]
fn referential_integrity_catches_missing_target() {
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    write_entities(
        &kg_dir,
        &[("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A")],
    );
    write_edges(
        &kg_dir,
        &[(
            "aaaaaaaa-0000-0000-0000-000000000001",
            "bbbbbbbb-0000-0000-0000-000000000002",
            "extends",
        )],
    );
    let result = check_referential_integrity(
        &kg_dir.join("entities.ndjson"),
        &kg_dir.join("notes.ndjson"),
        &kg_dir.join("edges.ndjson"),
    );
    assert!(!result.passed);
    assert_eq!(result.violations.len(), 1);
}

/// #1225: edges.ndjson written in the CANONICAL `source`/`target` spelling
/// (what `khive-vcs::sync`, `kkernel::kg::archive`, and the runtime
/// portability exporter all actually emit) must be evaluated by
/// referential-integrity, not silently skipped. Same fixture as
/// `referential_integrity_catches_missing_target` above, just in the
/// spelling real writers use.
#[test]
fn referential_integrity_catches_missing_target_in_canonical_spelling() {
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    write_entities(
        &kg_dir,
        &[("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A")],
    );
    write_edges_canonical(
        &kg_dir,
        &[(
            "aaaaaaaa-0000-0000-0000-000000000001",
            "bbbbbbbb-0000-0000-0000-000000000002",
            "extends",
        )],
    );
    let result = check_referential_integrity(
        &kg_dir.join("entities.ndjson"),
        &kg_dir.join("notes.ndjson"),
        &kg_dir.join("edges.ndjson"),
    );
    assert!(
        !result.passed,
        "a dangling target in canonical source/target spelling must not be silently skipped"
    );
    assert_eq!(result.violations.len(), 1);
}

/// #1225: a well-formed edge in canonical `source`/`target` spelling must
/// round-trip cleanly through referential-integrity AND schema-compliance
/// — the two checks that previously either flagged it as missing required
/// fields or silently excluded it from endpoint evaluation.
#[test]
fn canonical_spelling_edge_round_trips_clean() {
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    write_entities(
        &kg_dir,
        &[
            ("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A"),
            ("bbbbbbbb-0000-0000-0000-000000000002", "concept", "B"),
        ],
    );
    write_edges_canonical(
        &kg_dir,
        &[(
            "aaaaaaaa-0000-0000-0000-000000000001",
            "bbbbbbbb-0000-0000-0000-000000000002",
            "extends",
        )],
    );
    let ref_result = check_referential_integrity(
        &kg_dir.join("entities.ndjson"),
        &kg_dir.join("notes.ndjson"),
        &kg_dir.join("edges.ndjson"),
    );
    assert!(
        ref_result.passed,
        "canonical-spelling edge with valid endpoints must pass; violations: {:?}",
        ref_result.violations
    );

    let schema_result = check_schema_compliance(
        &kg_dir.join("entities.ndjson"),
        &kg_dir.join("edges.ndjson"),
        &kg_dir.join("notes.ndjson"),
    );
    assert!(
        schema_result.passed,
        "canonical-spelling edge must not be reported as missing source_id/target_id; \
             violations: {:?}",
        schema_result.violations
    );
}

#[test]
fn task_note_depends_on_passes_referential_integrity() {
    // Regression for ADR-017 + GTD pack: `depends_on` between two `task`
    // notes is a valid pack-extended edge. The referential-integrity check
    // must resolve note IDs from notes.ndjson, not only from entities.ndjson.
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    // No entity records — the edge endpoints live in notes only.
    std::fs::write(kg_dir.join("entities.ndjson"), "").unwrap();
    write_notes(
        &kg_dir,
        &[
            ("task-0001-0000-0000-0000-000000000001", "task"),
            ("task-0002-0000-0000-0000-000000000002", "task"),
        ],
    );
    write_edges(
        &kg_dir,
        &[(
            "task-0001-0000-0000-0000-000000000001",
            "task-0002-0000-0000-0000-000000000002",
            "depends_on",
        )],
    );
    let result = check_referential_integrity(
        &kg_dir.join("entities.ndjson"),
        &kg_dir.join("notes.ndjson"),
        &kg_dir.join("edges.ndjson"),
    );
    assert!(
        result.passed,
        "task note depends_on must pass referential integrity; violations: {:?}",
        result.violations
    );
    assert!(result.violations.is_empty());
}

#[test]
fn note_annotates_edge_passes_referential_integrity() {
    // Regression for ADR-002: `annotates` source is a note, target may be an
    // edge record. The referential-integrity check must include edge IDs
    // (keyed by `edge_id`) in the known-ID set, not only entity/note IDs.
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    // Two entity records connected by an `extends` edge that carries an edge_id.
    write_entities(
        &kg_dir,
        &[
            ("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A"),
            ("bbbbbbbb-0000-0000-0000-000000000002", "concept", "B"),
        ],
    );
    // The extends edge with an explicit edge_id.
    let edges = r#"{"edge_id":"eeeeeeee-0000-0000-0000-000000000001","source_id":"aaaaaaaa-0000-0000-0000-000000000001","target_id":"bbbbbbbb-0000-0000-0000-000000000002","relation":"extends"}
{"source_id":"note-obs-0000-0000-0000-000000000001","target_id":"eeeeeeee-0000-0000-0000-000000000001","relation":"annotates"}
"#;
    std::fs::write(kg_dir.join("edges.ndjson"), edges).unwrap();
    // The observation note that is the annotates source.
    write_notes(
        &kg_dir,
        &[("note-obs-0000-0000-0000-000000000001", "observation")],
    );
    let result = check_referential_integrity(
        &kg_dir.join("entities.ndjson"),
        &kg_dir.join("notes.ndjson"),
        &kg_dir.join("edges.ndjson"),
    );
    assert!(
        result.passed,
        "note annotates edge must pass referential integrity; violations: {:?}",
        result.violations
    );
    assert!(result.violations.is_empty());
}
