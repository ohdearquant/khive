use std::path::PathBuf;

use khive_types::EndpointKind;
use tempfile::TempDir;

use super::*;

// ── Taxonomy helpers ──────────────────────────────────────────────────────

/// Build the real pack-registry taxonomy. Tests that need it call this once.
fn real_taxonomy() -> KgTaxonomy {
    build_taxonomy().expect("build_taxonomy must succeed in test environment")
}

/// Minimal entity-kind set covering the 8 base kinds + `resource` (ADR-048).
fn base_entity_kinds() -> HashSet<String> {
    [
        "concept", "document", "dataset", "project", "person", "org", "artifact", "service",
        "resource",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// Minimal note-kind set covering base KG kinds + pack additions.
fn base_note_kinds() -> HashSet<String> {
    [
        "observation",
        "insight",
        "question",
        "decision",
        "reference",
        "task",
        "memory",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

fn make_kg_dir(tmp: &TempDir) -> PathBuf {
    let kg_dir = tmp.path().join(".khive/kg");
    std::fs::create_dir_all(&kg_dir).unwrap();
    kg_dir
}

fn write_entities(kg_dir: &std::path::Path, entities: &[(&str, &str, &str)]) {
    let content: String = entities
        .iter()
        .map(|(id, kind, name)| format!(r#"{{"id":"{id}","kind":"{kind}","name":"{name}"}}"#))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(kg_dir.join("entities.ndjson"), content + "\n").unwrap();
}

fn write_edges(kg_dir: &std::path::Path, edges: &[(&str, &str, &str)]) {
    let content: String = edges
        .iter()
        .map(|(src, tgt, rel)| {
            format!(r#"{{"source_id":"{src}","target_id":"{tgt}","relation":"{rel}"}}"#)
        })
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(kg_dir.join("edges.ndjson"), content + "\n").unwrap();
}

/// #1225: the canonical wire spelling every real NDJSON writer emits
/// (`khive-vcs::sync::NdjsonEdge`, `kkernel::kg::archive::NdjsonEdge`, and
/// the runtime portability `ExportedEdge`) is `source`/`target`, not the
/// `source_id`/`target_id` [`write_edges`] uses. This helper writes the
/// spelling real producers actually emit, so round-trip tests exercise
/// what `kg validate` is actually validating in production.
fn write_edges_canonical(kg_dir: &std::path::Path, edges: &[(&str, &str, &str)]) {
    let content: String = edges
        .iter()
        .map(|(src, tgt, rel)| {
            format!(r#"{{"source":"{src}","target":"{tgt}","relation":"{rel}"}}"#)
        })
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(kg_dir.join("edges.ndjson"), content + "\n").unwrap();
}

fn write_notes(kg_dir: &std::path::Path, notes: &[(&str, &str)]) {
    let content: String = notes
        .iter()
        .map(|(id, kind)| format!(r#"{{"id":"{id}","kind":"{kind}"}}"#))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(kg_dir.join("notes.ndjson"), content + "\n").unwrap();
}

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

// ── edge-endpoint-types ────────────────────────────────────────────────────

#[test]
fn edge_endpoint_types_passes_base_allowed_pair() {
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
            "extends",
        )],
    );
    let cfg = EdgeEndpointTypesConfig {
        enabled: true,
        severity: "error".into(),
    };
    let result = check_edge_endpoint_types(
        &kg_dir.join("entities.ndjson"),
        &kg_dir.join("notes.ndjson"),
        &kg_dir.join("edges.ndjson"),
        &[],
        &cfg,
    );
    assert!(
        result.passed,
        "concept -[extends]-> concept is base-allowed: {:?}",
        result.violations
    );
}

#[test]
fn edge_endpoint_types_rejects_disallowed_pair() {
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
    let cfg = EdgeEndpointTypesConfig {
        enabled: true,
        severity: "error".into(),
    };
    let result = check_edge_endpoint_types(
        &kg_dir.join("entities.ndjson"),
        &kg_dir.join("notes.ndjson"),
        &kg_dir.join("edges.ndjson"),
        &[],
        &cfg,
    );
    assert!(
        !result.passed,
        "person -[extends]-> person is not in the base allowlist"
    );
    assert_eq!(result.violations.len(), 1);
    assert_eq!(result.severity, "error");
}

#[test]
fn edge_endpoint_types_severity_config_downgrades_to_warning() {
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
    let cfg = EdgeEndpointTypesConfig {
        enabled: true,
        severity: "warning".into(),
    };
    let result = check_edge_endpoint_types(
        &kg_dir.join("entities.ndjson"),
        &kg_dir.join("notes.ndjson"),
        &kg_dir.join("edges.ndjson"),
        &[],
        &cfg,
    );
    assert!(!result.passed);
    assert_eq!(result.severity, "warning");
    assert_eq!(result.violations[0].severity, "warning");
}

#[test]
fn edge_endpoint_types_accepts_pack_extended_note_to_note_pair() {
    // GTD-shaped pack rule: depends_on between two `task` notes. Proves
    // `check_edge_endpoint_types` genuinely consults `pack_rules` (via the
    // reused `endpoint_matches`), not just the base entity-only table.
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
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
    let pack_rules = vec![EdgeEndpointRule {
        relation: EdgeRelation::DependsOn,
        source: EndpointKind::NoteOfKind("task"),
        target: EndpointKind::NoteOfKind("task"),
    }];
    let cfg = EdgeEndpointTypesConfig {
        enabled: true,
        severity: "error".into(),
    };
    let result = check_edge_endpoint_types(
        &kg_dir.join("entities.ndjson"),
        &kg_dir.join("notes.ndjson"),
        &kg_dir.join("edges.ndjson"),
        &pack_rules,
        &cfg,
    );
    assert!(
        result.passed,
        "pack-extended task->task depends_on must pass: {:?}",
        result.violations
    );
}

#[test]
fn edge_endpoint_types_rejects_entity_annotates_edge_endpoint() {
    // Regression for the edge-substrate endpoint bypass (commit 4e11ee38,
    // A `concept -[annotates]-> <edge_id>` edge must
    // fail — `annotates` requires a NOTE source (operations.rs:1226-1236),
    // and an entity source is invalid regardless of what the target is.
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    write_entities(
        &kg_dir,
        &[
            ("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A"),
            ("bbbbbbbb-0000-0000-0000-000000000002", "concept", "B"),
            ("cccccccc-0000-0000-0000-000000000003", "concept", "C"),
        ],
    );
    std::fs::write(
            kg_dir.join("edges.ndjson"),
            [
                // The referenced edge — gives us a known edge_id to target.
                r#"{"edge_id":"edge-0000-0000-0000-000000000099","source_id":"aaaaaaaa-0000-0000-0000-000000000001","target_id":"bbbbbbbb-0000-0000-0000-000000000002","relation":"extends"}"#,
                // concept -[annotates]-> <edge_id>: must be rejected.
                r#"{"source_id":"cccccccc-0000-0000-0000-000000000003","target_id":"edge-0000-0000-0000-000000000099","relation":"annotates"}"#,
            ]
            .join("\n")
                + "\n",
        )
        .unwrap();
    let cfg = EdgeEndpointTypesConfig {
        enabled: true,
        severity: "error".into(),
    };
    let result = check_edge_endpoint_types(
        &kg_dir.join("entities.ndjson"),
        &kg_dir.join("notes.ndjson"),
        &kg_dir.join("edges.ndjson"),
        &[],
        &cfg,
    );
    assert!(
        !result.passed,
        "entity -[annotates]-> edge must fail (annotates source must be a note)"
    );
    assert_eq!(result.violations.len(), 1);
}

#[test]
fn edge_endpoint_types_rejects_edge_as_endpoint_of_extends() {
    // Regression for the edge-substrate endpoint bypass: a
    // non-`annotates` relation naming a known edge ID as an endpoint must
    // fail — every relation other than `annotates` requires entity
    // endpoints (operations.rs:1355-1402); an edge endpoint is invalid
    // regardless of pack `EDGE_RULES` (`endpoint_matches` never matches
    // substrate `"edge"`).
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    write_entities(
        &kg_dir,
        &[
            ("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A"),
            ("bbbbbbbb-0000-0000-0000-000000000002", "concept", "B"),
            ("cccccccc-0000-0000-0000-000000000003", "concept", "C"),
        ],
    );
    std::fs::write(
            kg_dir.join("edges.ndjson"),
            [
                r#"{"edge_id":"edge-0000-0000-0000-000000000099","source_id":"aaaaaaaa-0000-0000-0000-000000000001","target_id":"bbbbbbbb-0000-0000-0000-000000000002","relation":"extends"}"#,
                // <edge_id> -[extends]-> concept: must be rejected.
                r#"{"source_id":"edge-0000-0000-0000-000000000099","target_id":"cccccccc-0000-0000-0000-000000000003","relation":"extends"}"#,
            ]
            .join("\n")
                + "\n",
        )
        .unwrap();
    let cfg = EdgeEndpointTypesConfig {
        enabled: true,
        severity: "error".into(),
    };
    let result = check_edge_endpoint_types(
        &kg_dir.join("entities.ndjson"),
        &kg_dir.join("notes.ndjson"),
        &kg_dir.join("edges.ndjson"),
        &[],
        &cfg,
    );
    assert!(
        !result.passed,
        "edge -[extends]-> entity must fail (extends requires entity endpoints)"
    );
    assert_eq!(result.violations.len(), 1);
}

#[test]
fn edge_endpoint_types_accepts_note_annotates_edge_endpoint() {
    // Runtime parity (operations.rs:1249-1254): `annotates` target may be
    // ANY substrate, including an edge — only the SOURCE must be a note.
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    write_entities(
        &kg_dir,
        &[
            ("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A"),
            ("bbbbbbbb-0000-0000-0000-000000000002", "concept", "B"),
        ],
    );
    write_notes(
        &kg_dir,
        &[("note0001-0000-0000-0000-000000000001", "observation")],
    );
    std::fs::write(
            kg_dir.join("edges.ndjson"),
            [
                r#"{"edge_id":"edge-0000-0000-0000-000000000099","source_id":"aaaaaaaa-0000-0000-0000-000000000001","target_id":"bbbbbbbb-0000-0000-0000-000000000002","relation":"extends"}"#,
                // note -[annotates]-> <edge_id>: must pass.
                r#"{"source_id":"note0001-0000-0000-0000-000000000001","target_id":"edge-0000-0000-0000-000000000099","relation":"annotates"}"#,
            ]
            .join("\n")
                + "\n",
        )
        .unwrap();
    let cfg = EdgeEndpointTypesConfig {
        enabled: true,
        severity: "error".into(),
    };
    let result = check_edge_endpoint_types(
        &kg_dir.join("entities.ndjson"),
        &kg_dir.join("notes.ndjson"),
        &kg_dir.join("edges.ndjson"),
        &[],
        &cfg,
    );
    assert!(
        result.passed,
        "note -[annotates]-> edge must pass: {:?}",
        result.violations
    );
}

// ── edge-direction-conventions ─────────────────────────────────────────────

fn direction_cfg(severity: &str) -> EdgeDirectionConventionsConfig {
    EdgeDirectionConventionsConfig {
        enabled: true,
        severity: severity.to_owned(),
        relations: vec![DirectionRuleConfig {
            relation: "introduced_by".into(),
            forward_source_kinds: vec!["concept".into(), "artifact".into(), "service".into()],
            forward_target_kinds: vec!["document".into(), "person".into()],
        }],
    }
}

#[test]
fn edge_direction_conventions_passes_forward_direction() {
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    write_entities(
        &kg_dir,
        &[
            ("aaaaaaaa-0000-0000-0000-000000000001", "concept", "A"),
            ("bbbbbbbb-0000-0000-0000-000000000002", "document", "B"),
        ],
    );
    write_edges(
        &kg_dir,
        &[(
            "aaaaaaaa-0000-0000-0000-000000000001",
            "bbbbbbbb-0000-0000-0000-000000000002",
            "introduced_by",
        )],
    );
    let cfg = direction_cfg("warning");
    let result = check_edge_direction_conventions(
        &kg_dir.join("entities.ndjson"),
        &kg_dir.join("notes.ndjson"),
        &kg_dir.join("edges.ndjson"),
        &cfg,
    );
    assert!(
        result.passed,
        "concept -[introduced_by]-> document is the forward direction: {:?}",
        result.violations
    );
}

#[test]
fn edge_direction_conventions_passes_service_forward_direction() {
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    write_entities(
        &kg_dir,
        &[
            ("aaaaaaaa-0000-0000-0000-000000000001", "service", "A"),
            ("bbbbbbbb-0000-0000-0000-000000000002", "document", "B"),
        ],
    );
    write_edges(
        &kg_dir,
        &[(
            "aaaaaaaa-0000-0000-0000-000000000001",
            "bbbbbbbb-0000-0000-0000-000000000002",
            "introduced_by",
        )],
    );
    let cfg = direction_cfg("warning");
    let result = check_edge_direction_conventions(
        &kg_dir.join("entities.ndjson"),
        &kg_dir.join("notes.ndjson"),
        &kg_dir.join("edges.ndjson"),
        &cfg,
    );
    assert!(
        result.passed,
        "service -[introduced_by]-> document is the forward direction: {:?}",
        result.violations
    );
}

#[test]
fn edge_direction_conventions_flags_reversed_service_direction() {
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    write_entities(
        &kg_dir,
        &[
            ("aaaaaaaa-0000-0000-0000-000000000001", "document", "A"),
            ("bbbbbbbb-0000-0000-0000-000000000002", "service", "B"),
        ],
    );
    write_edges(
        &kg_dir,
        &[(
            "aaaaaaaa-0000-0000-0000-000000000001",
            "bbbbbbbb-0000-0000-0000-000000000002",
            "introduced_by",
        )],
    );
    let cfg = direction_cfg("warning");
    let result = check_edge_direction_conventions(
        &kg_dir.join("entities.ndjson"),
        &kg_dir.join("notes.ndjson"),
        &kg_dir.join("edges.ndjson"),
        &cfg,
    );
    assert!(
        !result.passed,
        "document -[introduced_by]-> service is the reversed direction and must flag"
    );
}

#[test]
fn edge_direction_conventions_flags_reversed_direction() {
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    write_entities(
        &kg_dir,
        &[
            ("aaaaaaaa-0000-0000-0000-000000000001", "document", "A"),
            ("bbbbbbbb-0000-0000-0000-000000000002", "concept", "B"),
        ],
    );
    write_edges(
        &kg_dir,
        &[(
            "aaaaaaaa-0000-0000-0000-000000000001",
            "bbbbbbbb-0000-0000-0000-000000000002",
            "introduced_by",
        )],
    );
    let cfg = direction_cfg("warning");
    let result = check_edge_direction_conventions(
        &kg_dir.join("entities.ndjson"),
        &kg_dir.join("notes.ndjson"),
        &kg_dir.join("edges.ndjson"),
        &cfg,
    );
    assert!(
        !result.passed,
        "document -[introduced_by]-> concept matches the reversed pattern"
    );
    assert_eq!(result.violations.len(), 1);
    assert_eq!(result.severity, "warning");
}

#[test]
fn edge_direction_conventions_severity_config_escalates_to_error() {
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    write_entities(
        &kg_dir,
        &[
            ("aaaaaaaa-0000-0000-0000-000000000001", "document", "A"),
            ("bbbbbbbb-0000-0000-0000-000000000002", "concept", "B"),
        ],
    );
    write_edges(
        &kg_dir,
        &[(
            "aaaaaaaa-0000-0000-0000-000000000001",
            "bbbbbbbb-0000-0000-0000-000000000002",
            "introduced_by",
        )],
    );
    let cfg = direction_cfg("error");
    let result = check_edge_direction_conventions(
        &kg_dir.join("entities.ndjson"),
        &kg_dir.join("notes.ndjson"),
        &kg_dir.join("edges.ndjson"),
        &cfg,
    );
    assert!(!result.passed);
    assert_eq!(result.severity, "error");
    assert_eq!(result.violations[0].severity, "error");
}

// ── dangling-refs ──────────────────────────────────────────────────────────

#[test]
fn dangling_refs_passes_when_all_endpoints_resolve() {
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
            "extends",
        )],
    );
    let cfg = DanglingRefsConfig {
        enabled: true,
        severity: "error".into(),
    };
    let result = check_dangling_refs(
        &kg_dir.join("entities.ndjson"),
        &kg_dir.join("notes.ndjson"),
        &kg_dir.join("edges.ndjson"),
        &cfg,
    );
    assert!(result.passed, "{:?}", result.violations);
}

#[test]
fn dangling_refs_flags_unresolved_target_and_names_it_not_in_dataset() {
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
    let cfg = DanglingRefsConfig {
        enabled: true,
        severity: "error".into(),
    };
    let result = check_dangling_refs(
        &kg_dir.join("entities.ndjson"),
        &kg_dir.join("notes.ndjson"),
        &kg_dir.join("edges.ndjson"),
        &cfg,
    );
    assert!(!result.passed);
    assert_eq!(result.violations.len(), 1);
    assert!(
        result.violations[0].message.contains("not in dataset"),
        "message must distinguish dataset-scoped resolution: {}",
        result.violations[0].message
    );
}

#[test]
fn dangling_refs_severity_config_downgrades_to_info() {
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
    let cfg = DanglingRefsConfig {
        enabled: true,
        severity: "info".into(),
    };
    let result = check_dangling_refs(
        &kg_dir.join("entities.ndjson"),
        &kg_dir.join("notes.ndjson"),
        &kg_dir.join("edges.ndjson"),
        &cfg,
    );
    assert!(!result.passed);
    assert_eq!(result.severity, "info");
}

// ── naming-conventions ─────────────────────────────────────────────────────

fn naming_cfg(severity: &str) -> NamingConventionsConfig {
    NamingConventionsConfig {
        enabled: true,
        severity: severity.to_owned(),
        max_length: 20,
        no_leading_trailing_whitespace: true,
        no_parenthetical_suffix: true,
        kinds: std::collections::BTreeMap::new(),
    }
}

#[test]
fn naming_conventions_passes_clean_name() {
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    write_entities(
        &kg_dir,
        &[("aaaaaaaa-0000-0000-0000-000000000001", "concept", "Clean")],
    );
    let cfg = naming_cfg("warning");
    let result = check_naming_conventions(&kg_dir.join("entities.ndjson"), &cfg);
    assert!(result.passed, "{:?}", result.violations);
}

#[test]
fn naming_conventions_flags_whitespace_and_parenthetical_suffix() {
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    let entities = r#"{"id":"aaaaaaaa-0000-0000-0000-000000000001","kind":"concept","name":" Foo (2024 paper) "}"#;
    std::fs::write(kg_dir.join("entities.ndjson"), entities.to_owned() + "\n").unwrap();
    let cfg = naming_cfg("warning");
    let result = check_naming_conventions(&kg_dir.join("entities.ndjson"), &cfg);
    assert!(!result.passed);
    // Both the whitespace and parenthetical-suffix predicates fire.
    assert_eq!(result.violations.len(), 2, "{:?}", result.violations);
}

#[test]
fn naming_conventions_flags_empty_name() {
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    let entities = r#"{"id":"aaaaaaaa-0000-0000-0000-000000000001","kind":"concept","name":"   "}"#;
    std::fs::write(kg_dir.join("entities.ndjson"), entities.to_owned() + "\n").unwrap();
    let cfg = naming_cfg("warning");
    let result = check_naming_conventions(&kg_dir.join("entities.ndjson"), &cfg);
    assert!(!result.passed);
    assert_eq!(result.violations.len(), 1);
    assert!(result.violations[0].message.contains("empty"));
}

#[test]
fn naming_conventions_max_length_kind_override() {
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    // 12 chars — passes the global max_length=20 but fails a per-kind
    // override of 5 for "concept".
    write_entities(
        &kg_dir,
        &[(
            "aaaaaaaa-0000-0000-0000-000000000001",
            "concept",
            "TwelveChars!",
        )],
    );
    let mut cfg = naming_cfg("warning");
    cfg.kinds.insert(
        "concept".to_string(),
        NamingConventionsOverride {
            max_length: Some(5),
            no_leading_trailing_whitespace: None,
            no_parenthetical_suffix: None,
        },
    );
    let result = check_naming_conventions(&kg_dir.join("entities.ndjson"), &cfg);
    assert!(!result.passed, "per-kind max_length override must apply");
    assert_eq!(result.violations.len(), 1);
    assert!(result.violations[0].message.contains("max length 5"));
}

#[test]
fn naming_conventions_severity_config_is_error() {
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    let entities =
        r#"{"id":"aaaaaaaa-0000-0000-0000-000000000001","kind":"concept","name":" Bad "}"#;
    std::fs::write(kg_dir.join("entities.ndjson"), entities.to_owned() + "\n").unwrap();
    let cfg = naming_cfg("error");
    let result = check_naming_conventions(&kg_dir.join("entities.ndjson"), &cfg);
    assert!(!result.passed);
    assert_eq!(result.severity, "error");
}

// ── citation-date-lint ─────────────────────────────────────────────────────

fn citation_cfg(severity: &str) -> CitationDateLintConfig {
    CitationDateLintConfig {
        enabled: true,
        severity: severity.to_owned(),
        fields: vec!["year".into(), "date".into()],
    }
}

#[test]
fn citation_date_lint_passes_past_year() {
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    let entities = r#"{"id":"aaaaaaaa-0000-0000-0000-000000000001","kind":"document","name":"D","properties":{"year":2020}}"#;
    std::fs::write(kg_dir.join("entities.ndjson"), entities.to_owned() + "\n").unwrap();
    let cfg = citation_cfg("warning");
    let result = check_citation_date_lint(
        &kg_dir.join("entities.ndjson"),
        &kg_dir.join("notes.ndjson"),
        &cfg,
    );
    assert!(result.passed, "{:?}", result.violations);
}

#[test]
fn citation_date_lint_flags_forward_dated_year() {
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    let entities = r#"{"id":"aaaaaaaa-0000-0000-0000-000000000001","kind":"document","name":"D","properties":{"year":9999}}"#;
    std::fs::write(kg_dir.join("entities.ndjson"), entities.to_owned() + "\n").unwrap();
    let cfg = citation_cfg("warning");
    let result = check_citation_date_lint(
        &kg_dir.join("entities.ndjson"),
        &kg_dir.join("notes.ndjson"),
        &cfg,
    );
    assert!(!result.passed);
    assert_eq!(result.violations.len(), 1);
    assert!(result.violations[0].message.contains("9999"));
}

#[test]
fn citation_date_lint_flags_forward_dated_iso_date_on_a_note() {
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    std::fs::write(kg_dir.join("entities.ndjson"), "").unwrap();
    let notes = r#"{"id":"note-0001","kind":"observation","properties":{"date":"2999-01-01"}}"#;
    std::fs::write(kg_dir.join("notes.ndjson"), notes.to_owned() + "\n").unwrap();
    let cfg = citation_cfg("warning");
    let result = check_citation_date_lint(
        &kg_dir.join("entities.ndjson"),
        &kg_dir.join("notes.ndjson"),
        &cfg,
    );
    assert!(!result.passed, "note properties must be checked too");
    assert_eq!(result.violations.len(), 1);
}

#[test]
fn citation_date_lint_severity_config_is_error() {
    let tmp = TempDir::new().unwrap();
    let kg_dir = make_kg_dir(&tmp);
    let entities = r#"{"id":"aaaaaaaa-0000-0000-0000-000000000001","kind":"document","name":"D","properties":{"year":9999}}"#;
    std::fs::write(kg_dir.join("entities.ndjson"), entities.to_owned() + "\n").unwrap();
    let cfg = citation_cfg("error");
    let result = check_citation_date_lint(
        &kg_dir.join("entities.ndjson"),
        &kg_dir.join("notes.ndjson"),
        &cfg,
    );
    assert!(!result.passed);
    assert_eq!(result.severity, "error");
}

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
