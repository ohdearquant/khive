#[test]
fn checked_in_schema_matches_rust_model() {
    let schema_path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/schemas/khive-repo-v1.schema.json");
    let checked_in = fs::read(schema_path).unwrap();
    let mut generated = serde_json::to_vec_pretty(&khive_repo_showcase::json_schema()).unwrap();
    generated.push(b'\n');
    assert_eq!(checked_in, generated);
}

#[test]
fn unknown_provenance_marks_structure_and_join_views_unavailable() {
    let (_dir, mut request) = fixture();
    request.provenance = PipelineProvenance::unknown("fixture revision");
    let bundle = export(&request).unwrap();

    assert_eq!(
        bundle.graph.modules.disclosure.status,
        DisclosureStatus::Unavailable
    );
    assert_eq!(
        bundle.graph.commit_module_edges.disclosure.status,
        DisclosureStatus::Unavailable
    );
    assert!(!bundle.capability.languages.rust.measured);
    assert!(!bundle.capability.languages.rust.module_join);
    assert!(!bundle
        .capability
        .views
        .structure_graph
        .status
        .is_available());
    assert!(!bundle
        .capability
        .views
        .hotspot_quadrant
        .status
        .is_available());
    assert!(!bundle
        .capability
        .views
        .hidden_coupling
        .status
        .is_available());
    assert!(!bundle.capability.views.ownership.status.is_available());
    assert_eq!(
        bundle.graph.history_navigation.by_module.disclosure.status,
        DisclosureStatus::Unavailable
    );
}

#[test]
fn every_page_discloses_its_own_truncation() {
    let (_dir, mut request) = fixture();
    request.bounds.commits = 0;
    let bundle = export(&request).unwrap();
    assert!(bundle.graph.commits.items.is_empty());
    assert!(bundle.graph.commits.truncated);
    assert_eq!(
        bundle.graph.commits.disclosure.status,
        DisclosureStatus::Truncated
    );
    assert!(matches!(
        bundle.graph.commits.total_count,
        Availability::Available { value: 1 }
    ));

    let _: &Page<_> = &bundle.graph.modules;
    assert!(!bundle.graph.modules.truncated);
}
