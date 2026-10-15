// ── Wave-1 hygiene fixes (v024) ────────────────────────────────────────────────

/// Fix 1 (Critical): remember(source_id=) accepts 8-char short IDs.
///
/// The chain `create → remember(source_id=$prev.id)` broke because agent-mode
/// responses carry an 8-char short ID (first 8 hex chars of the full UUID) and
/// `remember` was parsing it as a full UUID, which always fails.
#[tokio::test]
async fn test_remember_source_id_accepts_short_id() {
    let rt = make_runtime();
    let registry = make_registry(rt.clone());

    // Create an entity; the internal test registry returns the full UUID in "id".
    let entity = registry
        .dispatch(
            "create",
            json!({
                "kind": "concept",
                "name": "attention mechanism",
                "description": "QKV self-attention"
            }),
        )
        .await
        .expect("create entity");

    let full_id = entity["id"].as_str().expect("entity has id");
    // Simulate agent-mode short ID: first 8 hex chars of the UUID (strip dashes).
    let short_id: String = full_id.chars().filter(|c| c != &'-').take(8).collect();
    assert_eq!(short_id.len(), 8, "derived short_id must be 8 chars");

    // remember with short id — must NOT return an error (previously did)
    let result = registry
        .dispatch(
            "memory.remember",
            json!({
                "content": "attention uses Q K V matrices",
                "source_id": short_id,
            }),
        )
        .await
        .expect("remember with 8-char short source_id must succeed");

    let note_id_str = result["id"].as_str().expect("has note_id");

    // Verify the annotates edge was created: neighbors(note, direction=out) returns
    // an array of NeighborHit; each hit carries "id" (the neighbor's UUID) and "relation".
    let neighbors = registry
        .dispatch(
            "neighbors",
            json!({
                "id": note_id_str,
                "direction": "out",
            }),
        )
        .await
        .expect("neighbors call succeeds");

    // response is a direct JSON array (not wrapped in an object)
    let hits = neighbors.as_array().expect("neighbors returns array");
    let found = hits.iter().any(|h| h["id"].as_str() == Some(full_id));
    assert!(
        found,
        "annotates edge to entity {full_id} must appear in note neighbors; got: {hits:?}\n\
         (short_id used: {short_id}, note_id: {note_id_str})"
    );
}

/// Fix 2: recall(help=true) must expose all params the recall handler accepts.
#[test]
fn test_handler_def_recall_params_complete() {
    use khive_types::Pack;

    let recall_def = khive_pack_memory::MemoryPack::HANDLERS
        .iter()
        .find(|h| h.name == "memory.recall")
        .expect("recall handler must be registered");

    let param_names: Vec<&str> = recall_def.params.iter().map(|p| p.name).collect();

    assert!(
        param_names.contains(&"top_k"),
        "recall HandlerDef must expose top_k param; got: {param_names:?}"
    );
    assert!(
        param_names.contains(&"score_floor"),
        "recall HandlerDef must expose score_floor param; got: {param_names:?}"
    );
    assert!(
        param_names.contains(&"fusion_strategy"),
        "recall HandlerDef must expose fusion_strategy param; got: {param_names:?}"
    );
    assert!(
        param_names.contains(&"embedding_model"),
        "recall HandlerDef must expose embedding_model param; got: {param_names:?}"
    );
    // Verb-level presentation renamed to include_breakdown.
    assert!(
        param_names.contains(&"include_breakdown"),
        "recall HandlerDef must expose include_breakdown param (not presentation); got: {param_names:?}"
    );
    assert!(
        !param_names.contains(&"presentation"),
        "recall HandlerDef must not expose verb-level presentation (ambiguous with MCP envelope); got: {param_names:?}"
    );
}

#[test]
fn test_handler_def_remember_params_complete() {
    use khive_types::Pack;

    let remember_def = khive_pack_memory::MemoryPack::HANDLERS
        .iter()
        .find(|h| h.name == "memory.remember")
        .expect("remember handler must be registered");

    let param_names: Vec<&str> = remember_def.params.iter().map(|p| p.name).collect();
    assert!(
        param_names.contains(&"embedding_model"),
        "remember HandlerDef must expose embedding_model param; got: {param_names:?}"
    );

    // Issue #70: decay_factor defaults are now type-differentiated; description must
    // document both episodic (0.02) and semantic (0.005) defaults, not the old flat 0.01.
    let decay_def = remember_def
        .params
        .iter()
        .find(|p| p.name == "decay_factor")
        .expect("decay_factor param must exist");
    assert!(
        decay_def.description.contains("0.02"),
        "decay_factor description must document episodic default 0.02, got: {:?}",
        decay_def.description
    );
    assert!(
        decay_def.description.contains("0.005"),
        "decay_factor description must document semantic default 0.005, got: {:?}",
        decay_def.description
    );
    assert!(
        !decay_def
            .description
            .starts_with("Decay rate 0.0–1.0 (default 0.1)"),
        "decay_factor description must NOT say 'default 0.1', got: {:?}",
        decay_def.description
    );
}

/// Fix 4: score_floor is portable across fusion strategies.
///
/// Creates 10 memories with varying salience; recall with score_floor=0.3 must
/// return a non-zero comparable number of hits under both RRF and Weighted fusion
/// — not 0 for one and many for the other.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn test_score_floor_portable_across_fusion_strategies() {
    let rt = make_runtime();
    let registry = make_registry(rt.clone());

    // Create 10 memories ALL containing both query terms "attention" and "transformer"
    // so FTS5 returns 10 results and the score span is non-zero. With span > 0 the
    // normalizer maps scores to [0.15, 0.82], so high-salience memories score above
    // 0.3 and low-salience ones score below — the exact split the test verifies.
    // (With only some memories matching both words, FTS5 returns ≤ 1 hit whose span=0;
    // normalize_rank_fusion_scores then clamps to 0.3 * signal_strength, and
    // calculate_score with w_rel=0.7 and the episodic bonus still barely misses 0.3
    // under the correct text-weight=0.3 Weighted mapping.)
    for (i, content) in [
        "transformer architecture uses attention mechanism",
        "attention is all you need for transformer models",
        "feedforward layers in transformer with self-attention",
        "layer normalization helps transformer attention training",
        "residual connections in transformer improve attention flow",
        "positional encoding enables transformer attention over sequences",
        "multi-head attention splits queries in transformer blocks",
        "softmax function normalizes transformer attention scores",
        "token embeddings feed transformer attention layers",
        "output projection combines transformer multi-head attention",
    ]
    .iter()
    .enumerate()
    {
        let salience = 0.4 + 0.06 * (i as f64); // 0.40 to 0.94
        registry
            .dispatch(
                "memory.remember",
                json!({
                    "content": content,
                    "salience": salience,
                    "decay_factor": 0.0,
                }),
            )
            .await
            .expect("memory.remember");
    }

    // Query relevant to several memories
    let rrf_result = registry
        .dispatch(
            "memory.recall",
            json!({
                "query": "attention transformer",
                "score_floor": 0.3_f64,
                "fusion_strategy": "rrf",
                "limit": 20,
            }),
        )
        .await
        .expect("recall rrf");

    let weighted_result = registry
        .dispatch(
            "memory.recall",
            json!({
                "query": "attention transformer",
                "score_floor": 0.3_f64,
                "fusion_strategy": "weighted",
                "limit": 20,
            }),
        )
        .await
        .expect("recall weighted");

    let rrf_hits = rrf_result.as_array().expect("rrf array").len();
    let weighted_hits = weighted_result.as_array().expect("weighted array").len();

    assert!(
        rrf_hits > 0,
        "score_floor=0.3 with RRF strategy must return > 0 hits (got 0); \
         RRF scores are not being normalized to [0,1]"
    );
    assert!(
        weighted_hits > 0,
        "score_floor=0.3 with Weighted strategy must return > 0 hits (got 0)"
    );
}

/// Fix 5: include_breakdown=true includes score breakdown without changing agent-mode shape.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn test_recall_include_breakdown_flag_includes_breakdown() {
    let rt = make_runtime();
    let registry = make_registry(rt);

    registry
        .dispatch(
            "memory.remember",
            json!({ "content": "transformer positional encoding", "salience": 0.8 }),
        )
        .await
        .expect("memory.remember");

    // Default (agent-mode): no breakdown
    let default_result = registry
        .dispatch("memory.recall", json!({ "query": "transformer" }))
        .await
        .expect("recall default");

    let default_hits = default_result.as_array().expect("array");
    assert!(!default_hits.is_empty(), "must have hits");
    assert!(
        default_hits[0].get("breakdown").is_none(),
        "default recall must NOT include breakdown"
    );

    // include_breakdown=true: breakdown present
    let verbose_result = registry
        .dispatch(
            "memory.recall",
            json!({ "query": "transformer", "include_breakdown": true }),
        )
        .await
        .expect("recall with include_breakdown=true");

    let verbose_hits = verbose_result.as_array().expect("array");
    assert!(
        !verbose_hits.is_empty(),
        "include_breakdown=true must have hits"
    );
    let bd = verbose_hits[0]
        .get("breakdown")
        .expect("include_breakdown=true result must include breakdown");
    assert!(
        bd.get("relevance").is_some(),
        "breakdown must have relevance field; got: {bd}"
    );
    assert!(
        bd.get("temporal").is_some(),
        "breakdown must have temporal field; got: {bd}"
    );
}

/// Regression: presentation= must be rejected by deny_unknown_fields.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn recall_presentation_alias_is_rejected_by_deny_unknown_fields() {
    let registry = make_registry(make_runtime());
    let err = registry
        .dispatch(
            "memory.recall",
            json!({ "query": "transformer", "presentation": "verbose" }),
        )
        .await
        .expect_err("presentation alias must be rejected");

    let msg = err.to_string();
    assert!(
        msg.contains("unknown field") && msg.contains("presentation"),
        "error must mention unknown field 'presentation'; got: {msg}"
    );
}
