// ── PR #444 regressions ──────────────────────────────────────────────────────

/// PR #444 regression: a runtime with no lattice
/// `embedding_model` in config but a custom registered embedder must fan out
/// `remember` through that embedder and store a vector.
///
/// Previously the fan-out gate checked `config().embedding_model.is_some()`;
/// custom-only runtimes fell through to `vec![]`.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn test_custom_embedder_only_runtime_fanout_remember_recall() {
    const MODEL_A: &str = "custom-enc-a";
    const DIMS: usize = 4;

    // Runtime with no lattice model, only a custom embedder.
    let rt = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        embedding_model: None,
        additional_embedding_models: vec![],
        ..RuntimeConfig::default()
    })
    .expect("runtime");
    rt.register_embedder(ConstVecProvider::new(MODEL_A, DIMS, 0.9));

    assert!(rt.config().embedding_model.is_none());
    assert!(
        rt.registered_embedding_model_names()
            .contains(&MODEL_A.to_string()),
        "custom embedder must be in registry"
    );

    let registry = make_registry(rt.clone());

    // remember — must not fail even with no lattice model.
    let result = registry
        .dispatch(
            "memory.remember",
            json!({
                "content": "custom embedder fanout regression test content alpha",
                "salience": 0.8
            }),
        )
        .await
        .expect("remember with custom-only embedder must succeed");

    let note_id = result["id"].as_str().expect("note_id present");
    assert!(!note_id.is_empty());

    // recall — custom embedder must have participated: at least the text path
    // should return the note.
    let recall_result = registry
        .dispatch(
            "memory.recall",
            json!({ "query": "custom embedder fanout regression" }),
        )
        .await
        .expect("recall after custom-embedder remember");

    let hits = recall_result.as_array().expect("array");
    let ids: Vec<&str> = hits.iter().map(|h| h["id"].as_str().unwrap()).collect();
    assert!(
        ids.contains(&note_id),
        "recall must find the note created via custom embedder; got: {ids:?}"
    );
}

/// PR #444 regression: Weighted fusion with N > 1
/// vector models must not zero-weight the text source.
///
/// Previously `fuse_candidates` passed [vec_a, vec_b, text] as 3 sources to
/// `fuse_search_results(Weighted)`.  `normalized_weights()` returns exactly 2
/// weights; sources beyond index 1 received weight 0.0, silently dropping text.
///
/// After the fix, N > 1 vector sources are Union-combined into one before
/// passing [combined_vector, text] — preserving the 2-source Weighted contract.
///
/// This test verifies that a memory created with two registered embedders is
/// returned by recall under the Weighted strategy (text contributes).
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn test_weighted_fusion_multi_model_text_not_zeroed() {
    const MODEL_A: &str = "enc-model-a";
    const MODEL_B: &str = "enc-model-b";
    const DIMS: usize = 4;

    // Runtime with two custom embedders and no lattice model.
    let rt = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        embedding_model: None,
        additional_embedding_models: vec![],
        ..RuntimeConfig::default()
    })
    .expect("runtime");
    rt.register_embedder(ConstVecProvider::new(MODEL_A, DIMS, 0.5));
    rt.register_embedder(ConstVecProvider::new(MODEL_B, DIMS, 0.6));

    assert_eq!(rt.registered_embedding_model_names().len(), 2);

    let registry = make_registry(rt.clone());

    // Store a memory with distinctive text content.
    let result = registry
        .dispatch(
            "memory.remember",
            json!({
                "content": "weighted fusion multi model text contribution regression beta",
                "salience": 0.7
            }),
        )
        .await
        .expect("remember with two custom embedders");

    let note_id = result["id"].as_str().expect("note_id");

    // Recall with explicit Weighted strategy — text must not be zeroed.
    let recall = registry
        .dispatch(
            "memory.recall",
            json!({
                "query": "weighted fusion multi model text",
                "fusion_strategy": "weighted",
                "limit": 10
            }),
        )
        .await
        .expect("recall with weighted fusion and 2 vector models");

    let hits = recall.as_array().expect("array");
    let ids: Vec<&str> = hits.iter().map(|h| h["id"].as_str().unwrap()).collect();
    assert!(
        ids.contains(&note_id),
        "weighted fusion with N>1 vector models must not zero-weight text — \
         note {note_id} must appear in results; got: {ids:?}"
    );
}

/// Issue #1115 regression: a CJK query must fuse across every configured
/// embedding engine rather than narrowing to a single model. A note embedded
/// only through the primary engine must still be found by a CJK query, even
/// though FTS5 has no CJK tokenizer and cannot rescue it via the text leg.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn test_cjk_query_finds_note_embedded_by_primary_engine() {
    const PRIMARY: &str = "enc-primary";
    const SECONDARY: &str = "enc-multilingual-b";
    const DIMS: usize = 4;

    let rt = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        embedding_model: None,
        additional_embedding_models: vec![],
        ..RuntimeConfig::default()
    })
    .expect("runtime");
    // Only the primary engine is registered when the note is written — this
    // mirrors a secondary engine being added to config after existing notes
    // were already embedded, before any backfill runs.
    rt.register_embedder(ConstVecProvider::new(PRIMARY, DIMS, 0.5));

    let registry = make_registry(rt.clone());

    let result = registry
        .dispatch(
            "memory.remember",
            json!({
                "content": "cjk fusion regression note embedded via primary engine only",
                "salience": 0.7,
            }),
        )
        .await
        .expect("remember with only the primary engine registered");
    let note_id = result["id"].as_str().expect("note_id");

    // The secondary engine becomes available only now — this note has no
    // vector row in its table.
    rt.register_embedder(ConstVecProvider::new(SECONDARY, DIMS, 0.6));

    let recall = registry
        .dispatch(
            "memory.recall",
            json!({ "query": "你好世界的中文查询", "limit": 10 }),
        )
        .await
        .expect("recall with cjk query");

    let hits = recall.as_array().expect("array");
    let ids: Vec<&str> = hits.iter().map(|h| h["id"].as_str().unwrap()).collect();
    assert!(
        ids.contains(&note_id),
        "cjk query must fuse across every configured engine and find a note \
         embedded only via the primary engine; got: {ids:?}"
    );
}

async fn recalled_ids(registry: &VerbRegistry, args: Value) -> Vec<String> {
    let recall = registry
        .dispatch("memory.recall", args)
        .await
        .expect("recall");
    recall
        .as_array()
        .expect("array")
        .iter()
        .map(|hit| hit["id"].as_str().expect("hit id").to_owned())
        .collect()
}

/// A CJK-dominant query leaves an English-only engine's candidates out of fusion when another
/// engine contributed candidates. An English query and an explicit `embedding_model` keep them.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn test_cjk_query_leaves_english_only_engine_out_of_fusion() {
    const ENGLISH_ONLY: &str = "all-minilm-l6-v2";
    const MULTILINGUAL: &str = "paraphrase-multilingual-minilm-l12-v2";

    let rt = make_runtime();
    rt.register_embedder(ConstVecProvider::new(
        ENGLISH_ONLY,
        EmbeddingModel::AllMiniLmL6V2.dimensions(),
        0.5,
    ));
    rt.register_embedder(ConstVecProvider::new(
        MULTILINGUAL,
        EmbeddingModel::ParaphraseMultilingualMiniLmL12V2.dimensions(),
        0.6,
    ));
    let registry = make_registry(rt.clone());

    // Each note is embedded through one engine only, so each engine's candidate list holds
    // exactly one note. Neither note shares a word with the queries below, so the text leg
    // returns nothing.
    let mut note_ids = Vec::new();
    for (content, model) in [
        ("note indexed through the english only engine", ENGLISH_ONLY),
        ("note indexed through the multilingual engine", MULTILINGUAL),
    ] {
        let created = registry
            .dispatch(
                "memory.remember",
                json!({ "content": content, "salience": 0.7, "embedding_model": model }),
            )
            .await
            .expect("remember through one engine");
        note_ids.push(created["id"].as_str().expect("note id").to_owned());
    }
    let (english_only_note, multilingual_note) = (&note_ids[0], &note_ids[1]);

    let cjk = recalled_ids(
        &registry,
        json!({ "query": "你好世界的中文查询", "limit": 10 }),
    )
    .await;
    assert!(
        cjk.contains(multilingual_note),
        "the multilingual engine's candidate must be returned; got: {cjk:?}"
    );
    assert!(
        !cjk.contains(english_only_note),
        "a CJK-dominant query must not return a candidate only the English-only engine has; \
         got: {cjk:?}"
    );

    let explicit = recalled_ids(
        &registry,
        json!({ "query": "你好世界的中文查询", "limit": 10, "embedding_model": ENGLISH_ONLY }),
    )
    .await;
    assert!(
        explicit.contains(english_only_note),
        "an explicit embedding_model request must be respected; got: {explicit:?}"
    );

    let english = recalled_ids(
        &registry,
        json!({ "query": "zebra giraffe walrus", "limit": 10 }),
    )
    .await;
    assert!(
        english.contains(english_only_note) && english.contains(multilingual_note),
        "an English query must keep every engine's candidates; got: {english:?}"
    );

    // memory.recall_fuse fuses through the same rule.
    let fused = registry
        .dispatch(
            "memory.recall_fuse",
            json!({ "query": "你好世界的中文查询" }),
        )
        .await
        .expect("memory.recall_fuse");
    let fused_ids: Vec<String> = fused["fused_candidates"]
        .as_array()
        .expect("fused array")
        .iter()
        .map(|hit| hit["id"].as_str().expect("fused id").to_owned())
        .collect();
    assert!(
        fused_ids.contains(multilingual_note) && !fused_ids.contains(english_only_note),
        "recall_fuse must leave the English-only engine out for a CJK-dominant query; \
         got: {fused_ids:?}"
    );
}
