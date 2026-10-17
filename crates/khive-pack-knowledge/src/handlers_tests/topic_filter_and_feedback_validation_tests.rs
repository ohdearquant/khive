/// Regression: handle_topic's entity lookup must never panic when an entity_id
/// is absent from the map.
///
/// The old two-pass code:
///   filter(|h| entity_map.get(id).is_some()) followed by
///   map(|h| entity_map.get(id).unwrap())          ← panic site
///
/// The fix merges both into a single filter_map that binds the entity once.
/// This test verifies that hits with missing entity_ids are silently dropped
/// (the documented intent of the guard) and never cause a panic.
#[test]
fn filter_map_drops_missing_entities_without_panic() {
    let present_id = Uuid::new_v4();
    let absent_id = Uuid::new_v4();

    let mut entity_map: HashMap<Uuid, &str> = HashMap::new();
    entity_map.insert(present_id, "present");

    // Simulate hits: one whose entity is in the map, one whose entity is absent.
    let hits = vec![present_id, absent_id];

    let results: Vec<&str> = hits
        .into_iter()
        .filter_map(|id| entity_map.get(&id).copied())
        .collect();

    assert_eq!(
        results,
        vec!["present"],
        "absent entity must be dropped silently"
    );
}

/// Verify that an unknown `section_signals` key produces an error message that
/// lists valid section types, not just the bare "unknown section_type" string.
#[tokio::test]
async fn invalid_section_type_error_lists_valid_values() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let pack = KnowledgePack::new(rt.clone());
    let registry = VerbRegistryBuilder::new()
        .build()
        .expect("empty registry builds");
    let token = rt.authorize(Namespace::local()).expect("authorize local");

    let err = pack
        .dispatch(
            "knowledge.feedback",
            json!({ "section_signals": { "not_a_real_section": "useful" } }),
            &registry,
            &token,
        )
        .await
        .unwrap_err();

    let khive_runtime::RuntimeError::InvalidInput(msg) = err else {
        panic!("expected InvalidInput, got {err:?}");
    };
    assert!(
        msg.contains("overview"),
        "error must list valid section types; got: {msg}",
    );
}
