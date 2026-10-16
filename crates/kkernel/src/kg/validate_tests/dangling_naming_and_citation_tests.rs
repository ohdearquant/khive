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
