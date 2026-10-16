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
