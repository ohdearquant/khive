/// Issue #396 regression: `memory.remember` dispatched through the real KG+memory
/// verb registry must succeed when the runtime is built with
/// `RuntimeConfig::no_embeddings()` — zero embedders registered, no lattice model
/// load attempted. Mirrors `khive-runtime`'s unit-level regression
/// (`no_embeddings_config_registers_zero_embedders` /
/// `no_embeddings_runtime_create_note_succeeds_without_model_fanout`) but drives
/// the actual `MemoryPack::handle_remember` handler through pack registration and
/// verb dispatch, not `create_note` directly.
#[tokio::test]
async fn test_remember_dispatch_succeeds_with_no_embeddings_runtime() {
    let rt = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        packs: vec!["kg".to_string()],
        ..RuntimeConfig::no_embeddings()
    })
    .expect("no_embeddings runtime");

    assert!(
        rt.registered_embedding_model_names().is_empty(),
        "no_embeddings() runtime must register zero embedders"
    );

    let registry = make_registry(rt);

    let result = registry
        .dispatch(
            "memory.remember",
            json!({
                "content": "issue-396 regression: memory.remember via real dispatch with zero embedders",
                "memory_type": "semantic",
                "salience": 0.7
            }),
        )
        .await
        .expect("memory.remember must succeed with zero registered embedders");

    let note_id = result["id"].as_str().expect("has note_id");
    assert!(!note_id.is_empty());
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn test_remember_recall_smoke() {
    let rt = make_runtime();
    let registry = make_registry(rt.clone());

    let result = registry
        .dispatch(
            "memory.remember",
            json!({
                "content": "The attention mechanism in transformers uses Q K V matrices",
                "memory_type": "semantic",
                "salience": 0.8,
                "decay": 0.01
            }),
        )
        .await
        .expect("memory.remember succeeds");

    let note_id = result["id"].as_str().expect("has note_id");
    assert!(!note_id.is_empty());

    let recall_result = registry
        .dispatch(
            "memory.recall",
            json!({ "query": "attention mechanism transformers" }),
        )
        .await
        .expect("memory.recall succeeds");

    let hits = recall_result.as_array().expect("array of hits");
    assert!(!hits.is_empty(), "recall returned at least one result");
    let first_id = hits[0]["id"].as_str().unwrap();
    assert_eq!(first_id, note_id, "recalled the memory we just created");
}
