// ---------------------------------------------------------------------------
// Test 13 - new_with_valid_kinds accepts a caller-supplied pack kind (#530)
// ---------------------------------------------------------------------------

#[test]
fn test_json_adapter_default_new_still_rejects_resource_kind() {
    // Plain `new()` (no injected registry) must keep rejecting kinds outside
    // the base ADR-001 set: `resource` is pack-registered (ADR-048), not a
    // base kind or alias.
    let mut adapter = JsonFormatAdapter::new(r#"[{"kind":"resource","name":"X"}]"#)
        .expect("structurally valid JSON must construct");
    let first = adapter.entities().next().expect("one record present");
    assert!(
        matches!(first, Err(AdapterError::UnknownKind { kind, .. }) if kind == "resource"),
        "resource must be rejected without an injected valid-kinds registry"
    );
}

#[test]
fn test_json_adapter_new_with_valid_kinds_accepts_resource_kind() {
    let valid_kinds = vec!["resource".to_string()];
    let mut adapter = JsonFormatAdapter::new_with_valid_kinds(
        r#"[{"kind":"resource","name":"X"}]"#,
        &valid_kinds,
    )
    .expect("structurally valid JSON must construct");
    let first = adapter
        .entities()
        .next()
        .expect("one record present")
        .expect("resource kind must be accepted when present in the injected registry");
    assert_eq!(first.kind, "resource");
}

#[test]
fn test_json_adapter_new_with_valid_kinds_still_rejects_unregistered_kind() {
    // A kind that is neither a base ADR-001 kind nor present in the injected
    // registry must still be rejected: the injected registry widens, it does
    // not disable, validation.
    let valid_kinds = vec!["resource".to_string()];
    let mut adapter =
        JsonFormatAdapter::new_with_valid_kinds(r#"[{"kind":"gadget","name":"X"}]"#, &valid_kinds)
            .expect("structurally valid JSON must construct");
    let first = adapter.entities().next().expect("one record present");
    assert!(
        matches!(first, Err(AdapterError::UnknownKind { kind, .. }) if kind == "gadget"),
        "unregistered kind must still be rejected with a non-empty valid-kinds registry"
    );
}

// ---------------------------------------------------------------------------
// #1758 — fail-closed identity, discriminator, and timestamp validation
// ---------------------------------------------------------------------------

#[test]
fn test_json_adapter_rejects_whitespace_only_entity_name() {
    let mut adapter = JsonFormatAdapter::new(r#"[{"kind":"concept","name":" \t\n "}]"#)
        .expect("structurally valid JSON must construct");
    let first = adapter.entities().next().expect("one entity record");
    assert!(
        matches!(first, Err(AdapterError::InvalidField { field, reason, .. })
            if field == "name" && reason.contains("non-blank")),
        "whitespace-only names must fail closed at the adapter boundary"
    );
}

#[test]
fn test_json_adapter_entity_signature_wins_over_from_to_metadata() {
    let mut adapter = JsonFormatAdapter::new(
        r#"[{"kind":"concept","name":"Lineage","from":"source-system","to":"target-system"}]"#,
    )
    .expect("entity metadata must not be misclassified as an edge");

    let entities: Vec<_> = adapter
        .entities()
        .map(|record| record.expect("entity must parse"))
        .collect();
    let edges: Vec<_> = adapter.edges().collect();

    assert_eq!(entities.len(), 1);
    assert!(edges.is_empty());
    assert_eq!(entities[0].properties["from"], "source-system");
    assert_eq!(entities[0].properties["to"], "target-system");
}

#[test]
fn test_json_adapter_rejects_ambiguous_entity_and_edge_signatures() {
    let err = match JsonFormatAdapter::new(
        r#"[{"kind":"concept","name":"Ambiguous","source":"aa","target":"bb","relation":"extends"}]"#,
    ) {
        Err(err) => err,
        Ok(_) => panic!("a record with complete entity and edge signatures must be rejected"),
    };

    assert!(
        matches!(err, AdapterError::InvalidField { index: 0, field, reason }
            if field == "$record" && reason.contains("ambiguous")),
        "the fatal error must identify the ambiguous record shape"
    );
}

#[test]
fn test_json_adapter_canonical_source_target_still_dispatches_edge() {
    let mut adapter =
        JsonFormatAdapter::new(r#"[{"source":"aa","target":"bb","relation":"extends"}]"#)
            .expect("canonical edge signature must remain accepted");

    assert!(adapter.entities().next().is_none());
    let edge = adapter
        .edges()
        .next()
        .expect("one edge")
        .expect("edge must parse");
    assert_eq!(edge.source, "aa");
    assert_eq!(edge.target, "bb");
}

#[test]
fn test_json_adapter_rejects_whitespace_only_edge_source() {
    let mut adapter =
        JsonFormatAdapter::new(r#"[{"source":" \t\n ","target":"bb","relation":"extends"}]"#)
            .expect("structurally valid JSON must construct");
    let error = adapter
        .edges()
        .next()
        .expect("one edge")
        .expect_err("whitespace-only source must fail closed");

    assert!(matches!(
        error,
        AdapterError::InvalidField { field, reason, .. }
            if field == "source" && reason.contains("non-blank")
    ));
}

#[test]
fn test_json_adapter_rejects_whitespace_only_edge_target() {
    let mut adapter =
        JsonFormatAdapter::new(r#"[{"source":"aa","target":" \t\n ","relation":"extends"}]"#)
            .expect("structurally valid JSON must construct");
    let error = adapter
        .edges()
        .next()
        .expect("one edge")
        .expect_err("whitespace-only target must fail closed");

    assert!(matches!(
        error,
        AdapterError::InvalidField { field, reason, .. }
            if field == "target" && reason.contains("non-blank")
    ));
}

#[test]
fn test_json_adapter_preserves_accepted_edge_endpoint_bytes() {
    let mut adapter = JsonFormatAdapter::new(
        r#"[{"source":"  source-id\t","target":"\ttarget-id  ","relation":"extends"}]"#,
    )
    .expect("structurally valid JSON must construct");
    let edge = adapter
        .edges()
        .next()
        .expect("one edge")
        .expect("non-blank endpoints must parse");

    assert_eq!(edge.source, "  source-id\t");
    assert_eq!(edge.target, "\ttarget-id  ");
}

#[test]
fn test_json_adapter_rejects_malformed_or_non_string_timestamps() {
    for (json, expected_field) in [
        (
            r#"[{"kind":"concept","name":"Bad time","created_at":"not-rfc3339"}]"#,
            "created_at",
        ),
        (
            r#"[{"source":"aa","target":"bb","relation":"extends","updated_at":42}]"#,
            "updated_at",
        ),
    ] {
        let mut adapter = JsonFormatAdapter::new(json).expect("structurally valid JSON");
        let error = if expected_field == "created_at" {
            adapter
                .entities()
                .next()
                .expect("one entity")
                .expect_err("malformed entity timestamp must fail")
        } else {
            adapter
                .edges()
                .next()
                .expect("one edge")
                .expect_err("malformed edge timestamp must fail")
        };
        assert!(
            matches!(error, AdapterError::InvalidField { field, reason, .. }
                if field == expected_field && reason.contains("RFC3339")),
            "present malformed timestamps must be rejected, not defaulted or warned away"
        );
    }
}
