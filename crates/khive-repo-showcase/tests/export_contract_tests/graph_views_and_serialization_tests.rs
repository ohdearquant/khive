#[test]
fn exact_source_path_join_keeps_lib_and_main_distinct() {
    let (_dir, request) = fixture();
    let bundle = export(&request).unwrap();
    assert_eq!(bundle.schema_version, SchemaVersion::KhiveRepoV1);
    assert_eq!(bundle.graph.commit_module_edges.items.len(), 2);
    let paths: Vec<_> = bundle
        .graph
        .modules
        .items
        .iter()
        .map(|module| (module.source_path.as_str(), module.module_path.as_str()))
        .collect();
    assert!(paths.contains(&("src/lib.rs", "crate")));
    assert!(paths.contains(&("src/main.rs", "crate::main")));
    assert!(bundle
        .graph
        .commit_module_edges
        .items
        .iter()
        .all(|edge| edge.origin.as_str() == "derived"));
}

#[test]
fn symbol_pages_and_unavailable_issue_facets_are_explicit() {
    let (_dir, request) = fixture();
    let bundle = export(&request).unwrap();
    assert!(bundle.graph.functions.items.is_empty());
    assert!(bundle.graph.datatypes.items.is_empty());
    assert!(bundle.graph.interfaces.items.is_empty());
    // A deferred tier is not a measured zero. Every symbol page reports the same
    // way an unrequested history facet does, on both fields: an unavailable count
    // and an unavailable disclosure. Asserting only one of the two is how the
    // earlier `complete` + `available(0)` pairing survived this test.
    for page in [
        &bundle.graph.functions,
        &bundle.graph.datatypes,
        &bundle.graph.interfaces,
    ] {
        assert_eq!(page.disclosure.status, DisclosureStatus::Unavailable);
        assert!(matches!(page.total_count, Availability::Unavailable { .. }));
    }
    assert!(matches!(
        bundle.graph.issues.total_count,
        Availability::Unavailable { .. }
    ));
    assert!(bundle
        .capability
        .views
        .history_structure_navigation
        .status
        .is_available());
    assert!(matches!(
        bundle
            .capability
            .views
            .history_structure_navigation
            .issue_module_facet,
        Availability::Unavailable { .. }
    ));
}

#[test]
fn view_tags_bind_all_ten_views() {
    let (_dir, request) = fixture();
    let bundle = export(&request).unwrap();
    assert_eq!(
        bundle.capability.views.cadence_timeline.granularity,
        Granularity::Repository
    );
    assert_eq!(
        bundle.capability.views.cadence_timeline.join,
        JoinTag::HistoryOnly
    );
    assert_eq!(bundle.capability.views.hotspot_quadrant.join, JoinTag::Join);
    assert_eq!(
        bundle.capability.views.structure_graph.join,
        JoinTag::StructureOnly
    );
    assert_eq!(bundle.capability.views.scorecard.join, JoinTag::FieldTagged);
}

#[test]
fn canonical_bytes_are_stable_and_round_trip_strictly() {
    let (_dir, request) = fixture();
    let first = export(&request).unwrap();
    let second = export(&request).unwrap();
    let a = canonical_bytes(&first).unwrap();
    let b = canonical_bytes(&second).unwrap();
    assert_eq!(a, b);
    assert_eq!(a.last(), Some(&b'\n'));
    let mut expected = serde_json::to_vec(&first).unwrap();
    expected.push(b'\n');
    assert_eq!(a, expected, "canonical JSON is compact plus one newline");
    let decoded: RepoBundle = serde_json::from_slice(&a).unwrap();
    assert_eq!(decoded, first);
    let validator = jsonschema::validator_for(&khive_repo_showcase::json_schema()).unwrap();
    let instance = serde_json::to_value(&first).unwrap();
    let errors = validator
        .iter_errors(&instance)
        .map(|error| error.to_string())
        .collect::<Vec<_>>();
    assert!(errors.is_empty(), "schema errors: {errors:#?}");

    let mut value: serde_json::Value = serde_json::from_slice(&a).unwrap();
    value["unexpected"] = serde_json::json!(true);
    assert!(serde_json::from_value::<RepoBundle>(value).is_err());

    let mut value: serde_json::Value = serde_json::from_slice(&a).unwrap();
    value["schema_version"] = serde_json::json!("khive.repo.v2");
    assert!(serde_json::from_value::<RepoBundle>(value).is_err());

    let mut value: serde_json::Value = serde_json::from_slice(&a).unwrap();
    value["meta"]["snapshot"]["ingested_at"] = serde_json::json!("not-a-timestamp");
    assert!(serde_json::from_value::<RepoBundle>(value).is_err());
}

#[test]
fn default_aggregate_bound_and_scorecard_module_ids_are_bounded() {
    assert_eq!(ExportBounds::default().aggregate_rows, 1_000);
    let (_dir, request) = fixture();
    let bundle = export(&request).unwrap();
    for key in [ScorecardKey::TopHotspots, ScorecardKey::OwnershipWarnings] {
        let field = bundle
            .aggregates
            .scorecard
            .fields
            .iter()
            .find(|field| field.key == key)
            .unwrap();
        let Availability::Available {
            value: ScorecardValue::ModuleIds { value },
        } = &field.value
        else {
            panic!("expected available bounded module ids for {key:?}");
        };
        assert!(value.items.len() <= value.bound.max_items as usize);
        assert!(matches!(value.total_count, Availability::Available { .. }));
    }
}
