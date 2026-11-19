/// `router_state` and `adapter_set` survive a snapshot round-trip byte-for-byte.
#[test]
fn router_state_and_adapter_set_round_trip() {
    let mut state = BrainState::new(8);

    let blob = RouterStateBlob {
        schema_version: 1,
        gate_bytes: vec![1, 2, 3],
    };
    state
        .router_state
        .insert("profile-x".to_owned(), blob.clone());

    let record = AdapterRecord {
        adapter_id: "lora-42".to_owned(),
        slot: 0,
        content_hash: "abc123".to_owned(),
    };
    state
        .adapter_set
        .insert("profile-x".to_owned(), vec![record.clone()]);

    let snapshot = state.to_snapshot();
    let restored = BrainState::from_snapshot(snapshot, 8);

    assert_eq!(
        restored.router_state.get("profile-x"),
        Some(&blob),
        "router_state must survive round-trip"
    );
    assert_eq!(
        restored.adapter_set.get("profile-x"),
        Some(&vec![record]),
        "adapter_set must survive round-trip"
    );
}

/// Omitting `router_state`/`adapter_set` from JSON must deserialize to
/// empty maps via `#[serde(default)]`, with no migration required.
#[test]
fn snapshot_missing_router_and_adapter_fields_defaults_to_empty() {
    let state = BrainState::new(8);
    let snapshot = state.to_snapshot();

    let mut value: serde_json::Value = serde_json::to_value(&snapshot).expect("serialize snapshot");

    let obj = value.as_object_mut().expect("snapshot is a JSON object");
    obj.remove("router_state");
    obj.remove("adapter_set");

    let restored: BrainStateSnapshot =
        serde_json::from_value(value).expect("deserialize old-format snapshot");

    assert!(
        restored.router_state.is_empty(),
        "router_state must default to empty map when absent from JSON"
    );
    assert!(
        restored.adapter_set.is_empty(),
        "adapter_set must default to empty map when absent from JSON"
    );
}
