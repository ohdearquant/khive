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
