// ── recall_embed vectors opt-in ──────────────────────────────────────────────

/// Default recall_embed omits embedding vectors, keeps model+dimension metadata.
#[tokio::test]
async fn recall_embed_default_omits_embedding_vectors() {
    const MODEL_A: &str = "embed-a";
    const DIMS: usize = 4;

    let rt = make_runtime();
    rt.register_embedder(ConstVecProvider::new(MODEL_A, DIMS, 0.7));
    let registry = make_registry(rt);

    let result = registry
        .dispatch(
            "memory.recall_embed",
            json!({ "query": "embedding metadata only" }),
        )
        .await
        .expect("recall_embed default");

    // Top-level embedding array must be absent.
    assert!(
        result.get("embedding").is_none(),
        "default recall_embed must not include top-level embedding; got: {result}"
    );
    // Dimension metadata must still be present.
    assert_eq!(
        result["dimensions"].as_u64(),
        Some(DIMS as u64),
        "dimensions must be returned even without embeddings"
    );
    // Per-engine entry must have model and dimensions but no embedding array.
    let engines = result["engines"].as_array().expect("engines array");
    assert_eq!(engines.len(), 1);
    assert_eq!(engines[0]["model"].as_str(), Some(MODEL_A));
    assert_eq!(engines[0]["dimensions"].as_u64(), Some(DIMS as u64));
    assert!(
        engines[0].get("embedding").is_none(),
        "default recall_embed must not include per-engine embedding; got: {}",
        engines[0]
    );
}

/// include_embeddings=true returns full vector payload.
#[tokio::test]
async fn recall_embed_include_embeddings_returns_vectors() {
    const MODEL_A: &str = "embed-a";
    const DIMS: usize = 4;

    let rt = make_runtime();
    rt.register_embedder(ConstVecProvider::new(MODEL_A, DIMS, 0.7));
    let registry = make_registry(rt);

    let result = registry
        .dispatch(
            "memory.recall_embed",
            json!({ "query": "embedding full payload", "include_embeddings": true }),
        )
        .await
        .expect("recall_embed include embeddings");

    // Top-level embedding array must be present.
    let top_vec = result["embedding"]
        .as_array()
        .expect("top-level embedding array");
    assert_eq!(
        top_vec.len(),
        DIMS,
        "top-level embedding length must match dims"
    );
    // Per-engine embedding also present.
    let engines = result["engines"].as_array().expect("engines array");
    assert_eq!(engines.len(), 1);
    let engine_vec = engines[0]["embedding"]
        .as_array()
        .expect("per-engine embedding array");
    assert_eq!(
        engine_vec.len(),
        DIMS,
        "per-engine embedding length must match dims"
    );
}

/// Metadata: memory.recall_embed handler must advertise include_embeddings param.
#[test]
fn recall_embed_handler_metadata_advertises_include_embeddings() {
    let embed_def = khive_pack_memory::MemoryPack::HANDLERS
        .iter()
        .find(|h| h.name == "memory.recall_embed")
        .expect("memory.recall_embed handler must be registered");

    let param_names: Vec<&str> = embed_def.params.iter().map(|p| p.name).collect();
    assert!(
        param_names.contains(&"include_embeddings"),
        "memory.recall_embed must advertise 'include_embeddings' param; got: {param_names:?}"
    );
}
