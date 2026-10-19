#[test]
fn edge_provenance_invariant_is_enforced_by_rust_and_schema() {
    let invalid = serde_json::json!({
        "id": "edge",
        "source": "source",
        "target": "target",
        "relation": "contains",
        "weight": 1.0,
        "origin": "derived",
        "derivation": null
    });
    assert!(serde_json::from_value::<GraphEdge>(invalid.clone()).is_err());
    let mut missing_nullable_field = invalid.clone();
    missing_nullable_field["origin"] = serde_json::json!("ingested");
    missing_nullable_field
        .as_object_mut()
        .unwrap()
        .remove("derivation");
    assert!(serde_json::from_value::<GraphEdge>(missing_nullable_field).is_err());

    let schema = khive_repo_showcase::json_schema();
    let validator = jsonschema::validator_for(&schema).unwrap();
    let (_dir, request) = fixture();
    let mut bundle = serde_json::to_value(export(&request).unwrap()).unwrap();
    bundle["graph"]["structure_edges"]["items"][0]["origin"] = serde_json::json!("derived");
    bundle["graph"]["structure_edges"]["items"][0]["derivation"] = serde_json::Value::Null;
    assert!(!validator.is_valid(&bundle));
}

#[test]
fn graph_edge_weight_boundaries_preserve_bits_and_error_precedence() {
    let edge = serde_json::json!({
        "id": "edge",
        "source": "source",
        "target": "target",
        "relation": "contains",
        "weight": 0.5,
        "origin": "ingested",
        "derivation": null
    });
    for weight in [-0.0_f64, 0.0, f64::from_bits(1), 0.5, 1.0] {
        let mut value = edge.clone();
        value["weight"] = serde_json::json!(weight);
        let decoded = serde_json::from_value::<GraphEdge>(value).unwrap();
        assert_eq!(decoded.weight.to_bits(), weight.to_bits());
    }
    for weight in [-f64::from_bits(1), 1.5] {
        let mut value = edge.clone();
        value["weight"] = serde_json::json!(weight);
        let error = serde_json::from_value::<GraphEdge>(value).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("graph edge weight must be a finite value in [0,1]"),
            "{error}"
        );
    }
    let mut both_invalid = edge;
    both_invalid["weight"] = serde_json::json!(1.5);
    both_invalid["origin"] = serde_json::json!("derived");
    let error = serde_json::from_value::<GraphEdge>(both_invalid).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("graph edge weight must be a finite value in [0,1]"),
        "{error}"
    );
}

#[test]
fn completed_empty_is_distinct_from_skipped() {
    let (_dir, mut request) = fixture();
    if let Availability::Available { value } = &mut request.provenance.git_digest {
        value.sources.issues = SourceCoverage::Completed;
    } else {
        panic!("fixture supplies git.digest provenance");
    }
    let completed = export(&request).unwrap();
    assert!(matches!(
        completed.graph.issues.total_count,
        Availability::Available { value: 0 }
    ));
    assert_eq!(
        completed.graph.issues.disclosure.status,
        DisclosureStatus::Complete
    );
    assert!(completed
        .aggregates
        .cadence_timeline
        .issues_opened
        .items
        .is_empty());
    assert!(matches!(
        completed
            .aggregates
            .cadence_timeline
            .issues_opened
            .total_count,
        Availability::Available { value: 0 }
    ));

    if let Availability::Available { value } = &mut request.provenance.git_digest {
        value.sources.issues = SourceCoverage::Skipped {
            reason: "forge source disabled".into(),
        };
    }
    let skipped = export(&request).unwrap();
    assert!(matches!(
        skipped.graph.issues.total_count,
        Availability::Unavailable { .. }
    ));
    assert_eq!(
        skipped.graph.issues.disclosure.status,
        DisclosureStatus::Unavailable
    );
    assert_eq!(
        skipped
            .aggregates
            .cadence_timeline
            .issues_opened
            .disclosure
            .status,
        DisclosureStatus::Unavailable
    );
}

#[test]
fn completed_source_with_missing_required_timestamp_marks_only_that_series_unavailable() {
    let (_dir, mut request) = fixture();
    let history = Connection::open(&request.history_db).unwrap();
    insert_note(
        &history,
        "20000000-0000-4000-8000-000000000010",
        "issue",
        "timestamp missing",
        serde_json::json!({"number": 10, "title": "timestamp missing"}),
    );
    insert_edge(
        &history,
        "20000000-0000-4000-8000-000000000011",
        "20000000-0000-4000-8000-000000000010",
        "10000000-0000-4000-8000-000000000001",
        "annotates",
    );
    if let Availability::Available { value } = &mut request.provenance.git_digest {
        value.sources.issues = SourceCoverage::Completed;
    }

    let bundle = export(&request).unwrap();
    assert_eq!(
        bundle
            .aggregates
            .cadence_timeline
            .issues_opened
            .disclosure
            .status,
        DisclosureStatus::Unavailable
    );
    assert_eq!(
        bundle
            .aggregates
            .cadence_timeline
            .issues_closed
            .disclosure
            .status,
        DisclosureStatus::Complete
    );
}

#[test]
fn zero_history_ownership_metrics_are_unavailable_not_zero() {
    let (_dir, request) = fixture();
    let history = Connection::open(&request.history_db).unwrap();
    history.execute("DELETE FROM graph_edges", []).unwrap();
    history.execute("DELETE FROM notes", []).unwrap();

    let bundle = export(&request).unwrap();
    assert!(matches!(
        bundle.aggregates.ownership.repository_author_concentration,
        Availability::Unavailable { .. }
    ));
    assert!(matches!(
        bundle.aggregates.ownership.repository_bus_factor,
        Availability::Unavailable { .. }
    ));
    assert!(bundle
        .aggregates
        .ownership
        .modules
        .items
        .iter()
        .all(
            |row| matches!(row.author_concentration, Availability::Unavailable { .. })
                && matches!(row.bus_factor, Availability::Unavailable { .. })
        ));
}
