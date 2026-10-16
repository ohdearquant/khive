// ── Wave-2 regression tests (M-C1..M-C4) ──────────────────────────────────────

/// M-C1: memory_type="procedural" must be rejected with a clear error listing
/// the valid values ("episodic" | "semantic").
#[tokio::test]
async fn test_remember_procedural_memory_type_rejected() {
    let rt = make_runtime();
    let registry = make_registry(rt);

    let result = registry
        .dispatch(
            "memory.remember",
            json!({
                "content": "procedural memory of how to deploy",
                "memory_type": "procedural"
            }),
        )
        .await;

    assert!(
        result.is_err(),
        "memory_type='procedural' must be rejected; got ok: {result:?}"
    );
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("episodic") && msg.contains("semantic"),
        "error must list valid memory_type values (episodic, semantic); got: {msg}"
    );
}

/// M-C1: recall with memory_type="procedural" must also be rejected.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn test_recall_procedural_memory_type_filter_rejected() {
    let rt = make_runtime();
    let registry = make_registry(rt);

    let result = registry
        .dispatch(
            "memory.recall",
            json!({
                "query": "deploy procedure",
                "memory_type": "procedural"
            }),
        )
        .await;

    assert!(
        result.is_err(),
        "recall with memory_type='procedural' must be rejected; got ok: {result:?}"
    );
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("episodic") && msg.contains("semantic"),
        "error must list valid memory_type values; got: {msg}"
    );
}

/// M-C3: composite scores returned by recall are always in [0, 1].
///
/// Verifies that the final_score is bounded regardless of fusion strategy.
/// Specifically, after normalize_relevance + weighted combination, scores must
/// not exceed 1.0.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn test_recall_composite_score_bounded_to_unit_interval() {
    let rt = make_runtime();
    let registry = make_registry(rt);

    // Store several memories to exercise the scoring path.
    for i in 0..5 {
        registry
            .dispatch(
                "memory.remember",
                json!({
                    "content": format!("bounded score test memory number {i}"),
                    "salience": 0.5 + 0.1 * (i as f64),
                    "decay_factor": 0.0,
                }),
            )
            .await
            .expect("memory.remember");
    }

    let result = registry
        .dispatch(
            "memory.recall",
            json!({ "query": "bounded score test memory", "limit": 10 }),
        )
        .await
        .expect("recall succeeds");

    let hits = result.as_array().expect("array of hits");
    assert!(!hits.is_empty(), "must have hits for bounded score test");

    for hit in hits {
        let score = hit["score"].as_f64().expect("hit has score");
        assert!(
            (0.0..=1.0).contains(&score),
            "composite score must be in [0, 1]; got {score}. \
             If score > 1.0, normalize_relevance or weighted combination is broken."
        );
    }
}

/// M-C3: HandlerDef description for min_score must not claim a fixed 0.0-1.0 range
/// without the qualification that it applies to the composite (not raw fusion) score.
#[test]
fn test_handler_def_min_score_description_clarified() {
    use khive_types::Pack;

    let recall_def = khive_pack_memory::MemoryPack::HANDLERS
        .iter()
        .find(|h| h.name == "memory.recall")
        .expect("recall handler must be registered");

    let min_score_param = recall_def
        .params
        .iter()
        .find(|p| p.name == "min_score")
        .expect("min_score param must exist");

    // The description must NOT just say "0.0–1.0" without qualification —
    // it must mention "composite" so callers understand the score applies to
    // the final weighted output, not the raw FTS/vector fusion score.
    assert!(
        min_score_param.description.contains("composite")
            || min_score_param.description.contains("[0,1]"),
        "min_score description must clarify the score is composite/[0,1]; got: {:?}",
        min_score_param.description
    );
}

/// M-C1: HandlerDef description for remember.memory_type must list exact valid values.
#[test]
fn test_handler_def_remember_memory_type_description_lists_valid_values() {
    use khive_types::Pack;

    let remember_def = khive_pack_memory::MemoryPack::HANDLERS
        .iter()
        .find(|h| h.name == "memory.remember")
        .expect("remember handler must be registered");

    let mt_param = remember_def
        .params
        .iter()
        .find(|p| p.name == "memory_type")
        .expect("memory_type param must exist");

    // Must list both valid values explicitly so help text is accurate.
    assert!(
        mt_param.description.contains("episodic") && mt_param.description.contains("semantic"),
        "memory_type description must list valid values 'episodic' and 'semantic'; got: {:?}",
        mt_param.description
    );
    // Must indicate these are the only valid values (not just examples).
    assert!(
        !mt_param.description.contains("e.g."),
        "memory_type description must not use 'e.g.' — values are exhaustive; got: {:?}",
        mt_param.description
    );
}

// Recall text_candidates must be non-empty when the query partially
// matches a memory note. Previously the conjunction Plain MATCH returned zero
// candidates if the note only contained some of the query terms.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn recall_candidates_text_candidates_non_empty_for_partial_match() {
    let rt = make_runtime();
    let registry = make_registry(rt.clone());

    // Create a memory note whose content matches only the first two words of the query.
    registry
        .dispatch(
            "memory.remember",
            serde_json::json!({
                "content": "attention mechanism in neural networks",
                "salience": 0.9,
                "memory_type": "semantic"
            }),
        )
        .await
        .expect("remember succeeds");

    // The query contains the note's terms plus extras the note doesn't have.
    let result = registry
        .dispatch(
            "memory.recall_candidates",
            serde_json::json!({
                "query": "attention mechanism transformers deep learning architecture"
            }),
        )
        .await
        .expect("recall_candidates succeeds");

    let text_candidates = result["text_candidates"]
        .as_array()
        .expect("text_candidates is array");

    assert!(
        !text_candidates.is_empty(),
        "text_candidates must be non-empty when a memory note partially matches the query; \
         got empty array. Query fanout is likely not working."
    );

    // All returned text candidates must be memory-kind notes.
    for tc in text_candidates {
        let note_id = tc["id"].as_str().expect("note_id present");
        assert!(
            !note_id.is_empty(),
            "text_candidate note_id must be non-empty"
        );
    }
}

// Recall include_breakdown=true must include per-component breakdown.
// presentation= was removed and is now rejected by deny_unknown_fields.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn recall_include_breakdown_true_includes_breakdown() {
    let rt = make_runtime();
    let registry = make_registry(rt.clone());

    registry
        .dispatch(
            "memory.remember",
            serde_json::json!({ "content": "breakdown test memory", "salience": 0.8 }),
        )
        .await
        .expect("remember succeeds");

    let result = registry
        .dispatch(
            "memory.recall",
            serde_json::json!({ "query": "breakdown test memory", "include_breakdown": true }),
        )
        .await
        .expect("recall with include_breakdown=true succeeds");

    let hits = result.as_array().expect("array of hits");
    assert!(!hits.is_empty(), "recall returned results");
    assert!(
        hits[0].get("breakdown").is_some(),
        "include_breakdown=true must include 'breakdown' in results"
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn recall_default_omits_breakdown() {
    let rt = make_runtime();
    let registry = make_registry(rt.clone());

    registry
        .dispatch(
            "memory.remember",
            serde_json::json!({ "content": "no breakdown memory", "salience": 0.8 }),
        )
        .await
        .expect("remember succeeds");

    let result = registry
        .dispatch(
            "memory.recall",
            serde_json::json!({ "query": "no breakdown memory" }),
        )
        .await
        .expect("recall without include_breakdown succeeds");

    let hits = result.as_array().expect("array of hits");
    if !hits.is_empty() {
        assert!(
            hits[0].get("breakdown").is_none(),
            "default recall must not include 'breakdown' in results"
        );
    }
}

#[tokio::test]
async fn recall_handler_metadata_advertises_include_breakdown_not_presentation() {
    let recall_def = khive_pack_memory::MemoryPack::HANDLERS
        .iter()
        .find(|h| h.name == "memory.recall")
        .expect("memory.recall handler must be registered");

    let has_include_breakdown = recall_def
        .params
        .iter()
        .any(|p| p.name == "include_breakdown");
    assert!(
        has_include_breakdown,
        "memory.recall must advertise include_breakdown param in metadata"
    );

    // presentation must no longer be advertised as a public param.
    let has_presentation = recall_def.params.iter().any(|p| p.name == "presentation");
    assert!(
        !has_presentation,
        "memory.recall must not advertise verb-level 'presentation' param to avoid ambiguity with MCP envelope"
    );
}

// search(kind="memory") must resolve when memory pack is loaded.
// The KG resolver is registry-driven: memory kind only appears in all_note_kinds()
// when MemoryPack is registered alongside KgPack. Without it the verb rejects
// "memory" as an unknown kind.
#[tokio::test]
async fn search_kind_memory_resolves_when_memory_pack_loaded() {
    let registry = make_registry(make_runtime());

    assert!(
        registry.all_note_kinds().contains(&"memory"),
        "registry.all_note_kinds() must include \"memory\" when memory pack is loaded; got: {:?}",
        registry.all_note_kinds()
    );

    // search(kind="memory") must succeed — previously failed with "unknown kind".
    let result = registry
        .dispatch(
            "search",
            serde_json::json!({ "kind": "memory", "query": "test" }),
        )
        .await;
    assert!(
        result.is_ok(),
        "search(kind=\"memory\") must succeed when memory pack is loaded; got: {:?}",
        result.err()
    );
}
