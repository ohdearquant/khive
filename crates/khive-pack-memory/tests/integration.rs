use khive_pack_brain::tunable::PackTunable;
use khive_pack_kg::KgPack;
use khive_pack_memory::MemoryPack;
use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig, VerbRegistryBuilder};
use khive_types::Pack;
use serde_json::json;
use uuid::Uuid;

fn make_runtime() -> KhiveRuntime {
    KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        embedding_model: None,
        ..RuntimeConfig::default()
    })
    .expect("in-memory runtime")
}

fn make_registry(rt: KhiveRuntime) -> khive_runtime::VerbRegistry {
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(MemoryPack::new(rt));
    builder.build().expect("registry builds")
}

#[tokio::test]
async fn test_remember_recall_smoke() {
    let rt = make_runtime();
    let registry = make_registry(rt.clone());

    let result = registry
        .dispatch(
            "remember",
            json!({
                "content": "The attention mechanism in transformers uses Q K V matrices",
                "memory_type": "semantic",
                "importance": 0.8,
                "decay": 0.01
            }),
        )
        .await
        .expect("remember succeeds");

    let note_id = result["note_id"].as_str().expect("has note_id");
    assert!(!note_id.is_empty());

    let recall_result = registry
        .dispatch(
            "recall",
            json!({ "query": "attention mechanism transformers" }),
        )
        .await
        .expect("recall succeeds");

    let hits = recall_result.as_array().expect("array of hits");
    assert!(!hits.is_empty(), "recall returned at least one result");
    let first_id = hits[0]["note_id"].as_str().unwrap();
    assert_eq!(first_id, note_id, "recalled the memory we just created");
}

#[tokio::test]
async fn test_recall_decay_ranking() {
    let rt = make_runtime();
    let registry = make_registry(rt.clone());

    // Create fresh memory with low decay
    let fresh = registry
        .dispatch(
            "remember",
            json!({
                "content": "fresh memory about neural networks",
                "importance": 0.7,
                "decay": 0.01
            }),
        )
        .await
        .expect("fresh remember");
    let fresh_id = fresh["note_id"].as_str().unwrap().to_string();

    // Create old memory (simulate 90 days ago) with high decay
    let old = registry
        .dispatch(
            "remember",
            json!({
                "content": "old memory about neural networks",
                "importance": 0.7,
                "decay": 0.1
            }),
        )
        .await
        .expect("old remember");
    let old_id = old["note_id"].as_str().unwrap().to_string();

    // Manually backdate the old note to simulate age
    let old_uuid: uuid::Uuid = old_id.parse().unwrap();
    let note_store = rt.notes(&rt.authorize(Namespace::local())).unwrap();
    let mut old_note = note_store.get_note(old_uuid).await.unwrap().unwrap();
    old_note.created_at -= 90 * 86_400_000_000i64; // 90 days in microseconds
    note_store.upsert_note(old_note).await.unwrap();

    let recall_result = registry
        .dispatch("recall", json!({ "query": "neural networks" }))
        .await
        .expect("recall succeeds");

    let hits = recall_result.as_array().expect("array");
    let ids: Vec<&str> = hits
        .iter()
        .map(|h| h["note_id"].as_str().unwrap())
        .collect();
    let fresh_pos = ids
        .iter()
        .position(|&id| id == fresh_id)
        .expect("fresh in results");
    let old_pos = ids
        .iter()
        .position(|&id| id == old_id)
        .expect("old in results");
    assert!(
        fresh_pos < old_pos,
        "fresh memory should rank higher than 90-day-old high-decay memory"
    );
}

#[tokio::test]
async fn test_recall_salience_ranking() {
    let rt = make_runtime();
    let registry = make_registry(rt.clone());

    let high = registry
        .dispatch(
            "remember",
            json!({
                "content": "concept about knowledge representation",
                "importance": 0.9,
                "decay": 0.0
            }),
        )
        .await
        .expect("high salience remember");
    let high_id = high["note_id"].as_str().unwrap().to_string();

    let low = registry
        .dispatch(
            "remember",
            json!({
                "content": "concept about knowledge representation",
                "importance": 0.1,
                "decay": 0.0
            }),
        )
        .await
        .expect("low salience remember");
    let low_id = low["note_id"].as_str().unwrap().to_string();

    let recall_result = registry
        .dispatch("recall", json!({ "query": "knowledge representation" }))
        .await
        .expect("recall succeeds");

    let hits = recall_result.as_array().expect("array");
    let ids: Vec<&str> = hits
        .iter()
        .map(|h| h["note_id"].as_str().unwrap())
        .collect();
    let high_pos = ids
        .iter()
        .position(|&id| id == high_id)
        .expect("high in results");
    let low_pos = ids
        .iter()
        .position(|&id| id == low_id)
        .expect("low in results");
    assert!(
        high_pos <= low_pos,
        "high salience memory should rank >= low salience"
    );
}

#[tokio::test]
async fn test_recall_memory_type_filter() {
    let rt = make_runtime();
    let registry = make_registry(rt.clone());

    registry
        .dispatch(
            "remember",
            json!({
                "content": "episodic event about meeting with Alice",
                "memory_type": "episodic",
                "importance": 0.7
            }),
        )
        .await
        .expect("episodic remember");

    let semantic = registry
        .dispatch(
            "remember",
            json!({
                "content": "semantic fact about meeting protocols",
                "memory_type": "semantic",
                "importance": 0.7
            }),
        )
        .await
        .expect("semantic remember");
    let semantic_id = semantic["note_id"].as_str().unwrap().to_string();

    let filtered = registry
        .dispatch(
            "recall",
            json!({ "query": "meeting", "memory_type": "semantic" }),
        )
        .await
        .expect("recall with filter");

    let hits = filtered.as_array().expect("array");
    assert!(!hits.is_empty(), "got results with memory_type filter");
    for hit in hits {
        let mt = hit["memory_type"].as_str().unwrap_or("");
        assert_eq!(mt, "semantic", "only semantic results returned");
    }
    let ids: Vec<&str> = hits
        .iter()
        .map(|h| h["note_id"].as_str().unwrap())
        .collect();
    assert!(
        ids.contains(&semantic_id.as_str()),
        "semantic note is in results"
    );
}

#[test]
fn test_memory_pack_requires_kg() {
    assert_eq!(MemoryPack::REQUIRES, &["kg"]);
    assert_eq!(MemoryPack::NAME, "memory");
    assert_eq!(MemoryPack::NOTE_KINDS, &["memory"]);
}

/// Regression test for issue #93: source_id must NOT be stored in note properties.
/// The annotates edge is the sole authorized source reference (ADR-036 §4).
#[tokio::test]
async fn test_remember_source_id_not_in_properties() {
    let rt = make_runtime();
    let registry = make_registry(rt.clone());

    // Create a real entity to use as the source (source_id must exist in namespace).
    let source = registry
        .dispatch(
            "create",
            json!({
                "kind": "person",
                "name": "Alice",
                "description": "test source person"
            }),
        )
        .await
        .expect("create source entity");
    let source_uuid = source["id"].as_str().unwrap().to_string();

    let result = registry
        .dispatch(
            "remember",
            json!({
                "content": "memory with a source",
                "source": source_uuid
            }),
        )
        .await
        .expect("remember with source_id");

    let note_id: Uuid = result["note_id"]
        .as_str()
        .unwrap()
        .parse()
        .expect("valid uuid");

    let note_store = rt
        .notes(&rt.authorize(Namespace::local()))
        .expect("note store");
    let note = note_store
        .get_note(note_id)
        .await
        .expect("get note")
        .expect("note exists");

    if let Some(props) = &note.properties {
        assert!(
            props.get("source_id").is_none(),
            "source_id must not be stored in note properties; got: {props:?}"
        );
    }
}

/// ADR-021 §4 (F108): decay_factor >= 0 is the only constraint — no upper cap.
/// Values above 1.0 are valid (fast-fading memories with very short effective half-lives).
/// Negative values are rejected with InvalidInput.
#[tokio::test]
async fn test_remember_decay_factor_no_upper_cap() {
    let rt = make_runtime();
    let registry = make_registry(rt.clone());

    // decay_factor = 5.0 is valid — no upper cap per ADR-021 §4
    let result = registry
        .dispatch(
            "remember",
            json!({
                "content": "memory with high decay rate",
                "decay": 5.0
            }),
        )
        .await
        .expect("remember with decay_factor > 1.0 should succeed");

    let note_id: Uuid = result["note_id"]
        .as_str()
        .unwrap()
        .parse()
        .expect("valid uuid");

    let note_store = rt
        .notes(&rt.authorize(Namespace::local()))
        .expect("note store");
    let note = note_store
        .get_note(note_id)
        .await
        .expect("get note")
        .expect("note exists");

    let df = note.decay_factor.unwrap_or(0.0);
    // Stored value must match exactly (not clamped to 1.0)
    assert!(
        (df - 5.0).abs() < 1e-10,
        "decay_factor should be stored as-is (5.0), got {df}"
    );
}

/// ADR-021 §4 (F108): negative decay_factor is rejected.
#[tokio::test]
async fn test_remember_decay_factor_negative_rejected() {
    let rt = make_runtime();
    let registry = make_registry(rt.clone());

    let result = registry
        .dispatch(
            "remember",
            json!({
                "content": "memory with negative decay",
                "decay": -0.1
            }),
        )
        .await;

    assert!(result.is_err(), "negative decay_factor must be rejected");
}

/// ADR-021 §4 (F107): remember always writes memory_type to properties.
/// When memory_type is absent, it defaults to "episodic".
#[tokio::test]
async fn test_remember_default_memory_type_written_to_properties() {
    let rt = make_runtime();
    let registry = make_registry(rt.clone());

    let result = registry
        .dispatch(
            "remember",
            json!({ "content": "memory without explicit type" }),
        )
        .await
        .expect("remember without memory_type");

    let note_id: Uuid = result["note_id"]
        .as_str()
        .unwrap()
        .parse()
        .expect("valid uuid");

    // The response must carry memory_type
    assert_eq!(
        result["memory_type"].as_str(),
        Some("episodic"),
        "response must include default memory_type"
    );

    let note_store = rt
        .notes(&rt.authorize(Namespace::local()))
        .expect("note store");
    let note = note_store
        .get_note(note_id)
        .await
        .expect("get note")
        .expect("note exists");

    let stored_type = note
        .properties
        .as_ref()
        .and_then(|p| p.get("memory_type"))
        .and_then(|v| v.as_str());
    assert_eq!(
        stored_type,
        Some("episodic"),
        "memory_type must be written to properties even when not supplied"
    );
}

/// ADR-021 §4 (F109): invalid UUID string in source_id is rejected with an error.
#[tokio::test]
async fn test_remember_invalid_source_id_uuid_rejected() {
    let rt = make_runtime();
    let registry = make_registry(rt);

    let result = registry
        .dispatch(
            "remember",
            json!({
                "content": "memory with bad source_id",
                "source": "not-a-valid-uuid"
            }),
        )
        .await;

    assert!(
        result.is_err(),
        "invalid source_id UUID must cause an error, got: {result:?}"
    );
}

/// ADR-021 §4 (F108): importance outside [0, 1] is rejected.
#[tokio::test]
async fn test_remember_importance_out_of_range_rejected() {
    let rt = make_runtime();
    let registry = make_registry(rt);

    let neg = registry
        .dispatch("remember", json!({ "content": "test", "importance": -0.1 }))
        .await;
    assert!(neg.is_err(), "negative importance must be rejected");

    let rt2 = make_runtime();
    let registry2 = make_registry(rt2);
    let above = registry2
        .dispatch("remember", json!({ "content": "test", "importance": 1.1 }))
        .await;
    assert!(above.is_err(), "importance > 1 must be rejected");
}

/// ADR-033 §2 (F222): recall.rerank is callable and returns expected shape.
#[tokio::test]
async fn test_recall_rerank_passthrough_with_no_active_rerankers() {
    let rt = make_runtime();
    let registry = make_registry(rt);

    let candidates = json!([
        { "note_id": "00000000-0000-0000-0000-000000000001", "fused_score": 0.8 },
        { "note_id": "00000000-0000-0000-0000-000000000002", "fused_score": 0.6 },
    ]);

    let result = registry
        .dispatch("recall.rerank", json!({ "candidates": candidates }))
        .await
        .expect("recall.rerank with no active rerankers");

    let reranked = result["reranked"].as_array().expect("reranked array");
    assert_eq!(reranked.len(), 2, "must return one entry per candidate");
    for entry in reranked {
        let scores = entry["rerank_scores"]
            .as_object()
            .expect("rerank_scores object");
        assert!(
            scores.is_empty(),
            "no active rerankers → empty rerank_scores, got {scores:?}"
        );
    }
    let active = result["active_rerankers"]
        .as_array()
        .expect("active_rerankers array");
    assert!(active.is_empty(), "no active rerankers expected");
}

#[test]
fn test_memory_dotted_verbs_registered() {
    let names: Vec<&str> = MemoryPack::HANDLERS.iter().map(|v| v.name).collect();
    assert!(names.contains(&"recall.candidates"));
    assert!(names.contains(&"recall.fuse"));
    assert!(names.contains(&"recall.score"));
    assert!(names.contains(&"recall.embed"));
    // F222: recall.rerank must be registered (ADR-033 §2)
    assert!(
        names.contains(&"recall.rerank"),
        "recall.rerank not found in: {names:?}"
    );
}

#[tokio::test]
async fn test_recall_candidates_returns_arrays() {
    let rt = make_runtime();
    let registry = make_registry(rt);

    registry
        .dispatch(
            "remember",
            json!({ "content": "attention recall candidates" }),
        )
        .await
        .expect("remember");

    let result = registry
        .dispatch(
            "recall.candidates",
            json!({ "query": "attention candidates" }),
        )
        .await
        .expect("recall.candidates");

    let text = result["text_candidates"].as_array().expect("text array");
    assert!(!text.is_empty());
    assert!(text[0]["note_id"].as_str().is_some());
    assert!(text[0]["score"].as_f64().is_some());
    assert!(text[0]["rank"].as_u64().is_some());
    assert!(result["candidate_limit"].as_u64().is_some());
    assert!(
        result.get("text_hits").is_none(),
        "old count field must be absent"
    );
}

#[tokio::test]
async fn test_recall_fuse_returns_fused_candidates_not_full_recall() {
    let rt = make_runtime();
    let registry = make_registry(rt);

    registry
        .dispatch(
            "remember",
            json!({ "content": "attention fusion diagnostic" }),
        )
        .await
        .expect("remember");

    let result = registry
        .dispatch("recall.fuse", json!({ "query": "attention fusion" }))
        .await
        .expect("recall.fuse");

    let fused = result["fused_candidates"].as_array().expect("fused array");
    assert!(!fused.is_empty());
    assert!(fused[0]["fused_score"].as_f64().is_some());
    assert!(fused[0]["source"].as_str().is_some());
    assert!(
        fused[0].get("content").is_none(),
        "full recall field must be absent"
    );
    assert!(
        fused[0].get("salience").is_none(),
        "full recall field must be absent"
    );
}

#[tokio::test]
async fn test_recall_breakdown_is_opt_in() {
    let rt = make_runtime();
    let registry = make_registry(rt);

    registry
        .dispatch(
            "remember",
            json!({ "content": "attention score breakdown", "importance": 0.8 }),
        )
        .await
        .expect("remember");

    let plain = registry
        .dispatch("recall", json!({ "query": "attention breakdown" }))
        .await
        .expect("recall");
    let hits = plain.as_array().unwrap();
    assert!(!hits.is_empty());
    assert!(
        hits[0].get("breakdown").is_none(),
        "breakdown must be absent by default"
    );

    let explained = registry
        .dispatch(
            "recall",
            json!({ "query": "attention breakdown", "config": { "include_breakdown": true } }),
        )
        .await
        .expect("recall with breakdown");
    let hits = explained.as_array().unwrap();
    assert!(!hits.is_empty());
    let bd = &hits[0]["breakdown"];
    assert!(bd["relevance"].as_f64().is_some());
    assert!(bd["importance_raw"].as_f64().is_some());
    assert!(bd["importance_decayed"].as_f64().is_some());
    assert!(bd["temporal"].as_f64().is_some());
    assert!(bd["weighted"]["relevance_contribution"].as_f64().is_some());
}

/// recall.candidates always includes both array keys even when the embedding model is absent
/// and the vector path returns nothing.
#[tokio::test]
async fn test_recall_candidates_vector_field_always_present() {
    let rt = make_runtime();
    let registry = make_registry(rt);

    registry
        .dispatch(
            "remember",
            json!({ "content": "text only candidate check" }),
        )
        .await
        .expect("remember");

    let result = registry
        .dispatch(
            "recall.candidates",
            json!({ "query": "text only candidate" }),
        )
        .await
        .expect("recall.candidates");

    // Both arrays must be present even if one is empty.
    assert!(
        result["vector_candidates"].as_array().is_some(),
        "vector_candidates key must always be present"
    );
    assert!(
        result["text_candidates"].as_array().is_some(),
        "text_candidates key must always be present"
    );
}

/// recall.fuse source field must be a plain string ("text"), not a serde-tagged enum.
#[tokio::test]
async fn test_recall_fuse_source_field_is_plain_string() {
    let rt = make_runtime();
    let registry = make_registry(rt);

    registry
        .dispatch("remember", json!({ "content": "fuse source string check" }))
        .await
        .expect("remember");

    let result = registry
        .dispatch("recall.fuse", json!({ "query": "fuse source string" }))
        .await
        .expect("recall.fuse");

    let fused = result["fused_candidates"].as_array().expect("fused array");
    assert!(!fused.is_empty());
    let source = fused[0]["source"].as_str().expect("source is string");
    // Must be a plain label, not a JSON object or enum tag.
    assert!(
        source == "text" || source == "vector" || source == "both",
        "source must be a plain label, got {source:?}"
    );
}

/// Verifies that recall.fuse routes through khive_retrieval::fuse_search_results
/// by injecting a non-default fusion config (Rrf k=1) and asserting the fused
/// score matches the RRF k=1 formula: 1/(k + rank) = 1/(1 + 1) = 0.5.
///
/// Under default k=60 the score would be 1/61 ≈ 0.0164. The large gap (0.5 vs
/// 0.0164) is the discriminator: if the adapter did not pass k=1 through to
/// khive_retrieval::HybridConfig, the score would not be 0.5.
#[tokio::test]
async fn test_recall_fuse_rrf_k1_uses_retrieval_adapter() {
    let rt = make_runtime();
    let registry = make_registry(rt);

    registry
        .dispatch(
            "remember",
            json!({ "content": "retrieval adapter rrf k1 probe memory" }),
        )
        .await
        .expect("remember");

    let result = registry
        .dispatch(
            "recall.fuse",
            json!({
                "query": "retrieval adapter rrf k1 probe",
                "config": {
                    "fuse_strategy": { "rrf": { "k": 1 } }
                }
            }),
        )
        .await
        .expect("recall.fuse with Rrf k=1");

    let fused = result["fused_candidates"].as_array().expect("fused array");
    assert!(
        !fused.is_empty(),
        "recall.fuse must return at least one candidate"
    );

    let score = fused[0]["fused_score"]
        .as_f64()
        .expect("fused_score is f64");
    // Rank 1 in a single text source with k=1: RRF = 1/(1+1) = 0.5.
    // If k=60 were used instead, score ≈ 0.0164 — the gap proves the adapter works.
    let expected = 0.5_f64;
    assert!(
        (score - expected).abs() < 1e-6,
        "RRF k=1, rank 1 → fused_score must be 0.5; got {score:.6} \
         (≈0.0164 means the adapter passed k=60 instead of k=1)"
    );
}

/// Regression: after wiring khive-retrieval into fuse_candidates, the recall.fuse
/// response shape must be unchanged — top-level strategy + candidate_limit, and
/// per-candidate note_id + fused_score + source must all be present. Full recall
/// fields (content, salience) must remain absent.
#[tokio::test]
async fn test_recall_fuse_shape_preserved_after_retrieval_wiring() {
    let rt = make_runtime();
    let registry = make_registry(rt);

    registry
        .dispatch(
            "remember",
            json!({ "content": "shape regression check after retrieval wiring" }),
        )
        .await
        .expect("remember");

    let result = registry
        .dispatch(
            "recall.fuse",
            json!({ "query": "shape regression retrieval wiring" }),
        )
        .await
        .expect("recall.fuse");

    // Top-level shape
    assert!(
        result.get("strategy").is_some(),
        "strategy field must be present in recall.fuse response"
    );
    assert!(
        result["candidate_limit"].as_u64().is_some(),
        "candidate_limit must be a non-negative integer"
    );

    let fused = result["fused_candidates"]
        .as_array()
        .expect("fused_candidates array");
    assert!(!fused.is_empty(), "fused_candidates must be non-empty");

    let c = &fused[0];
    assert!(
        c["note_id"].as_str().is_some(),
        "note_id must be a string UUID"
    );
    assert!(
        c["fused_score"].as_f64().is_some(),
        "fused_score must be a float"
    );
    let source = c["source"].as_str().expect("source must be a plain string");
    assert!(
        matches!(source, "text" | "vector" | "both"),
        "source must be a plain label, got {source:?}"
    );
    // Full recall fields must not leak into fuse output
    assert!(
        c.get("content").is_none(),
        "content must be absent from recall.fuse output"
    );
    assert!(
        c.get("salience").is_none(),
        "salience must be absent from recall.fuse output"
    );
}

/// When include_breakdown is true, breakdown.total() must equal the hit's composite score.
#[tokio::test]
async fn test_recall_breakdown_total_matches_composite_score() {
    let rt = make_runtime();
    let registry = make_registry(rt);

    registry
        .dispatch(
            "remember",
            json!({ "content": "arithmetic score check memory", "importance": 0.7 }),
        )
        .await
        .expect("remember");

    let result = registry
        .dispatch(
            "recall",
            json!({ "query": "arithmetic score check", "config": { "include_breakdown": true } }),
        )
        .await
        .expect("recall with breakdown");

    let hits = result.as_array().unwrap();
    assert!(!hits.is_empty());
    let hit = &hits[0];
    let score = hit["score"].as_f64().expect("hit has score");
    let bd = &hit["breakdown"];
    let rc = bd["weighted"]["relevance_contribution"].as_f64().unwrap();
    let ic = bd["weighted"]["importance_contribution"].as_f64().unwrap();
    let tc = bd["weighted"]["temporal_contribution"].as_f64().unwrap();
    let total = rc + ic + tc;
    assert!(
        (total - score).abs() < 1e-9,
        "breakdown weighted sum {total} must equal composite score {score}"
    );
}

/// Regression test for issue #94: non-memory notes must not appear in recall results.
///
/// Creates more non-memory notes than the default `limit * 4` candidate threshold (the amount
/// at which non-memory notes can dominate the candidate pool without pre-filtering), then
/// verifies that recall returns only memory-kind notes.
#[tokio::test]
async fn test_recall_excludes_non_memory_notes() {
    let rt = make_runtime();
    let registry = make_registry(rt.clone());

    // Create 50 observation notes whose content matches the recall query — enough to
    // dominate a `limit=5` candidate pool at `limit * 4 = 20` without pre-filtering.
    let tok = rt.authorize(Namespace::local());
    for i in 0..50 {
        rt.create_note(
            &tok,
            "observation",
            None,
            &format!("observation {i} about attention mechanisms in neural networks"),
            Some(0.5),
            None,
            vec![],
        )
        .await
        .expect("create observation");
    }

    // Create a small number of memory notes with matching content.
    let mem1 = registry
        .dispatch(
            "remember",
            json!({
                "content": "memory note about attention mechanisms in neural networks",
                "importance": 0.8
            }),
        )
        .await
        .expect("remember 1");
    let mem2 = registry
        .dispatch(
            "remember",
            json!({
                "content": "another memory note about attention mechanisms",
                "importance": 0.7
            }),
        )
        .await
        .expect("remember 2");
    let mem1_id = mem1["note_id"].as_str().unwrap().to_string();
    let mem2_id = mem2["note_id"].as_str().unwrap().to_string();

    let result = registry
        .dispatch(
            "recall",
            json!({ "query": "attention mechanisms neural networks", "limit": 5 }),
        )
        .await
        .expect("recall succeeds");

    let hits = result.as_array().expect("array of hits");
    assert!(
        !hits.is_empty(),
        "recall should return memory notes even when non-memory notes dominate the index"
    );
    let ids: Vec<&str> = hits
        .iter()
        .map(|h| h["note_id"].as_str().unwrap())
        .collect();
    assert!(
        ids.contains(&mem1_id.as_str()) || ids.contains(&mem2_id.as_str()),
        "at least one memory note must appear in recall results"
    );
    for hit in hits {
        // recall must never surface observation or other non-memory kinds
        assert!(
            hit.get("note_id").is_some(),
            "hit has note_id field (memory pack shape)"
        );
        assert!(
            hit.get("salience").is_some(),
            "hit has salience field (memory pack shape)"
        );
    }
}

/// Regression for #159: PackTunable::apply_config must actually affect recall
/// scoring, not just mutate a Mutex that handlers ignore.
///
/// The wire is:
///   apply_config(weights) → MemoryPack.config (Mutex)
///   → MemoryPack::active_config() reads it
///   → handle_recall / handle_recall_score use it as the base
///   → compute_score uses the tuned weights
///
/// This test uses `recall.score` (deterministic — no FTS/vector noise) with
/// no per-call `config` argument, applies different configs via
/// PackTunable::apply_config, and verifies the resulting `total` score
/// reflects the tuned weights. Without the active_config wire (issue #159
/// bug), the result would always reflect RecallConfig::default() regardless
/// of apply_config.
#[tokio::test]
async fn test_pack_tunable_apply_config_affects_recall_score() {
    use khive_pack_memory::config::RecallConfig;

    let rt = make_runtime();
    let pack = MemoryPack::new(rt.clone());

    // Sanity: with default config (0.70/0.20/0.10), the score for
    //   rrf=1.0, salience=1.0, decay=0.0, age=0 → 0.70+0.20+0.10 = 1.0
    // With importance_only (0.0/1.0/0.0), the score for
    //   rrf=1.0, salience=0.0, decay=0.0, age=0 → 0.0
    // The difference is large enough to prove the weights flow through.

    // Apply importance-only config to the pack.
    let importance_only = RecallConfig {
        relevance_weight: 0.0,
        importance_weight: 1.0,
        temporal_weight: 0.0,
        ..RecallConfig::default()
    };
    pack.apply_config(serde_json::to_value(&importance_only).unwrap())
        .expect("apply_config (importance-only) succeeds");

    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(pack);
    let registry = builder.build().expect("registry builds");

    // Call recall.score with high relevance but ZERO salience — under
    // importance-only weights, score MUST be 0.0. Under default weights
    // (the bug), it would be 0.70.
    let result = registry
        .dispatch(
            "recall.score",
            json!({
                "rrf": 1.0,
                "salience": 0.0,
                "decay_factor": 0.0,
                "age_days": 0.0,
            }),
        )
        .await
        .expect("recall.score succeeds");
    let total = result["total"].as_f64().expect("total is a number");
    assert!(
        total.abs() < 1e-9,
        "under importance_weight=1.0, salience=0 → score=0; got {total}. \
         If non-zero, MemoryPack::active_config() is not being used by \
         recall.score (#159 regression)."
    );

    // Mirror check: under relevance-only weights with rrf=1.0, salience=0 → score=1.0.
    // This requires a SECOND pack instance because PackRuntime ownership prevents
    // mutating the live registry's config from outside. We construct the test
    // by exercising the same wire on a fresh pack.
    let rt2 = make_runtime();
    let pack2 = MemoryPack::new(rt2.clone());
    let relevance_only = RecallConfig {
        relevance_weight: 1.0,
        importance_weight: 0.0,
        temporal_weight: 0.0,
        ..RecallConfig::default()
    };
    pack2
        .apply_config(serde_json::to_value(&relevance_only).unwrap())
        .expect("apply_config (relevance-only) succeeds");

    let mut builder2 = VerbRegistryBuilder::new();
    builder2.register(KgPack::new(rt2.clone()));
    builder2.register(pack2);
    let registry2 = builder2.build().expect("registry2 builds");

    let result2 = registry2
        .dispatch(
            "recall.score",
            json!({
                "rrf": 1.0,
                "salience": 0.0,
                "decay_factor": 0.0,
                "age_days": 0.0,
            }),
        )
        .await
        .expect("recall.score (relevance-only) succeeds");
    let total2 = result2["total"].as_f64().expect("total is a number");
    assert!(
        (total2 - 1.0).abs() < 1e-9,
        "under relevance_weight=1.0 with rrf=1.0 → score=1.0; got {total2}"
    );
}

// ── ADR-033 §6 knob tests ──────────────────────────────────────────────────

#[tokio::test]
async fn test_recall_default_identity() {
    let rt = make_runtime();
    let registry = make_registry(rt.clone());

    // Create multiple memories so the identity comparison is meaningful
    // (single-hit fixtures can't distinguish ordering changes).
    for content in [
        "the mitochondria is the powerhouse of the cell",
        "ribosomes synthesize proteins in the cell",
        "the nucleus contains the cell's DNA",
        "lysosomes digest cellular waste in the cell",
    ] {
        registry
            .dispatch("remember", json!({ "content": content, "importance": 0.8 }))
            .await
            .expect("remember succeeds");
    }

    // Baseline recall with no knobs
    let base = registry
        .dispatch("recall", json!({ "query": "cell" }))
        .await
        .expect("baseline recall succeeds");
    let base_hits = base.as_array().expect("array");
    assert!(
        base_hits.len() >= 2,
        "baseline must return at least two hits to make ordering meaningful, got {}",
        base_hits.len()
    );

    // Same call with all three knobs explicitly set to null — must be byte-identical
    let knobless = registry
        .dispatch(
            "recall",
            json!({
                "query": "cell",
                "top_k": null,
                "fusion_strategy": null,
                "score_floor": null,
            }),
        )
        .await
        .expect("recall with all knobs null succeeds");
    let knobless_hits = knobless.as_array().expect("array");

    assert_eq!(
        base_hits.len(),
        knobless_hits.len(),
        "null knobs must not change result count"
    );

    // Full ordering identity: each hit's note_id AND fused_score must match
    // position-by-position. This catches a regression where a null knob silently
    // shifts the ranking or rescaling.
    for (i, (b, k)) in base_hits.iter().zip(knobless_hits.iter()).enumerate() {
        assert_eq!(
            b["note_id"].as_str(),
            k["note_id"].as_str(),
            "null knobs altered note_id at position {i}"
        );
        // Scores must round-trip; allow tiny float jitter
        let bs = b["score"].as_f64().unwrap_or(0.0);
        let ks = k["score"].as_f64().unwrap_or(0.0);
        assert!(
            (bs - ks).abs() < 1e-9,
            "null knobs altered score at position {i}: baseline={bs} knobless={ks}"
        );
    }
}

#[tokio::test]
async fn test_recall_top_k_override() {
    let rt = make_runtime();
    let registry = make_registry(rt.clone());

    // Create several distinct memories to ensure the pool is large enough
    for i in 0..5 {
        registry
            .dispatch(
                "remember",
                json!({
                    "content": format!("rust ownership memory safety concept {i}"),
                    "importance": 0.7
                }),
            )
            .await
            .expect("remember succeeds");
    }

    // Recall with top_k=2 — must not return more than 2 results
    let result = registry
        .dispatch(
            "recall",
            json!({ "query": "rust ownership memory safety", "top_k": 2 }),
        )
        .await
        .expect("recall with top_k=2 succeeds");
    let hits = result.as_array().expect("array");
    assert!(
        hits.len() <= 2,
        "top_k=2 must return at most 2 results, got {}",
        hits.len()
    );

    // top_k=1 must return at most 1
    let result1 = registry
        .dispatch(
            "recall",
            json!({ "query": "rust ownership memory safety", "top_k": 1 }),
        )
        .await
        .expect("recall with top_k=1 succeeds");
    let hits1 = result1.as_array().expect("array");
    assert!(
        hits1.len() <= 1,
        "top_k=1 must return at most 1 result, got {}",
        hits1.len()
    );
}

#[tokio::test]
async fn test_recall_fusion_strategy_override() {
    let rt = make_runtime();
    let registry = make_registry(rt.clone());

    registry
        .dispatch(
            "remember",
            json!({
                "content": "gradient descent optimization machine learning",
                "importance": 0.8
            }),
        )
        .await
        .expect("remember succeeds");

    // Each valid strategy must succeed and return an array
    for strategy in &["rrf", "weighted", "union"] {
        let result = registry
            .dispatch(
                "recall",
                json!({
                    "query": "gradient descent optimization",
                    "fusion_strategy": strategy
                }),
            )
            .await
            .unwrap_or_else(|e| panic!("recall with fusion_strategy={strategy:?} failed: {e}"));
        assert!(
            result.is_array(),
            "fusion_strategy={strategy:?} must return an array, got {result}"
        );
    }

    // Invalid strategy must return an error
    let err = registry
        .dispatch(
            "recall",
            json!({
                "query": "gradient descent optimization",
                "fusion_strategy": "bogus"
            }),
        )
        .await;
    assert!(err.is_err(), "invalid fusion_strategy must return an error");
    let msg = err.unwrap_err().to_string();
    assert!(
        msg.contains("rrf") && msg.contains("weighted") && msg.contains("union"),
        "error message must list valid strategies, got: {msg}"
    );
}

#[tokio::test]
async fn test_recall_score_floor() {
    let rt = make_runtime();
    let registry = make_registry(rt.clone());

    registry
        .dispatch(
            "remember",
            json!({
                "content": "backpropagation neural network training algorithm",
                "importance": 0.6
            }),
        )
        .await
        .expect("remember succeeds");

    // Baseline: no floor — get result count
    let base = registry
        .dispatch(
            "recall",
            json!({ "query": "backpropagation neural network" }),
        )
        .await
        .expect("baseline recall succeeds");
    let base_count = base.as_array().expect("array").len();

    // score_floor=0.99 must not return MORE results than baseline
    let floored = registry
        .dispatch(
            "recall",
            json!({
                "query": "backpropagation neural network",
                "score_floor": 0.99
            }),
        )
        .await
        .expect("recall with score_floor=0.99 succeeds");
    let floored_hits = floored.as_array().expect("array");
    assert!(
        floored_hits.len() <= base_count,
        "score_floor=0.99 must return ≤ baseline count ({base_count}), got {}",
        floored_hits.len()
    );

    // All returned hits must have score >= 0.99
    for hit in floored_hits {
        let score = hit["score"].as_f64().expect("score is a number");
        assert!(
            score >= 0.99,
            "score_floor=0.99: all returned scores must be ≥ 0.99, got {score}"
        );
    }

    // score_floor=0.0 must behave same as no floor
    let zero_floor = registry
        .dispatch(
            "recall",
            json!({
                "query": "backpropagation neural network",
                "score_floor": 0.0
            }),
        )
        .await
        .expect("recall with score_floor=0.0 succeeds");
    let zero_count = zero_floor.as_array().expect("array").len();
    assert_eq!(
        zero_count, base_count,
        "score_floor=0.0 must return same count as no floor"
    );
}

// ── Reranker integration tests (PR #375) ────────────────────────────────────

/// PR #375: empty reranker_weights is a pass-through — results must be identical
/// to a baseline recall with no reranker config.
#[tokio::test]
async fn test_recall_with_empty_reranker_weights_is_passthrough() {
    let rt = make_runtime();
    let registry = make_registry(rt.clone());

    for i in 0..4 {
        registry
            .dispatch(
                "remember",
                json!({
                    "content": format!("memory about deep learning topic {i}"),
                    "importance": 0.5 + (i as f64) * 0.1,
                    "decay": 0.0
                }),
            )
            .await
            .expect("remember");
    }

    let baseline = registry
        .dispatch("recall", json!({ "query": "deep learning" }))
        .await
        .expect("baseline recall");
    let baseline_ids: Vec<String> = baseline
        .as_array()
        .expect("array")
        .iter()
        .map(|h| h["note_id"].as_str().unwrap().to_string())
        .collect();

    let with_empty_reranker = registry
        .dispatch(
            "recall",
            json!({
                "query": "deep learning",
                "config": { "reranker_weights": {} }
            }),
        )
        .await
        .expect("recall with empty reranker_weights");
    let reranker_ids: Vec<String> = with_empty_reranker
        .as_array()
        .expect("array")
        .iter()
        .map(|h| h["note_id"].as_str().unwrap().to_string())
        .collect();

    assert_eq!(
        baseline_ids, reranker_ids,
        "empty reranker_weights must be a pass-through — result ordering must match baseline"
    );
}

/// PR #375: reranker_weights with importance=1.0 must promote the highest-salience
/// memory to rank #1, even when it would rank lower under the default compute_score.
///
/// Strengthened: captures baseline ordering first (no reranker) and asserts that
/// the reranked order actually differs — proving the REPLACE wiring is not a no-op.
///
/// Fixture design: all notes contain the query keyword so all are retrieved.
/// Low-salience notes have richer keyword density (higher FTS BM25).  Baseline
/// uses pure relevance scoring (importance_weight=0) so the keyword-dense
/// low-salience notes rank first.  The importance=1.0 reranker then flips the
/// order, placing the high-salience note at rank #1.
#[tokio::test]
async fn test_recall_with_reranker_weights_changes_ordering() {
    let rt = make_runtime();
    let registry = make_registry(rt.clone());

    // Three low-salience notes with high keyword density for "gradient descent" —
    // their BM25 score will be higher than the high-salience note.
    for _ in 0..3 {
        registry
            .dispatch(
                "remember",
                json!({
                    "content": "gradient descent gradient descent gradient descent optimization",
                    "importance": 0.1,
                    "decay": 0.0
                }),
            )
            .await
            .expect("low salience remember");
    }

    // One high-salience note that mentions gradient descent only once — lower BM25
    // relevance so baseline (pure-relevance) ranks it below the low-salience notes.
    let high_salience = registry
        .dispatch(
            "remember",
            json!({
                "content": "gradient descent is a key technique in machine learning",
                "importance": 0.95,
                "decay": 0.0
            }),
        )
        .await
        .expect("high salience remember");
    let high_id = high_salience["note_id"].as_str().unwrap().to_string();

    // Step 1: baseline recall — pure relevance scoring (importance_weight=0) so
    // BM25-heavy low-salience notes rank first.
    let baseline = registry
        .dispatch(
            "recall",
            json!({
                "query": "gradient descent",
                "config": {
                    "relevance_weight": 1.0,
                    "importance_weight": 0.0,
                    "temporal_weight": 0.0
                }
            }),
        )
        .await
        .expect("baseline recall");
    let baseline_hits = baseline.as_array().expect("baseline array");
    assert!(
        baseline_hits.len() >= 2,
        "need at least 2 results to test ordering change, got {}",
        baseline_hits.len()
    );
    let baseline_ids: Vec<String> = baseline_hits
        .iter()
        .map(|h| h["note_id"].as_str().unwrap().to_string())
        .collect();
    let baseline_top = &baseline_ids[0];

    // Baseline must NOT have high_id at rank #1 — if it does, the fixture is
    // degenerate (the reranker would be a no-op for the top position).
    assert_ne!(
        baseline_top, &high_id,
        "fixture error: high-salience note already ranks first in baseline; \
         reranker change cannot be demonstrated. baseline={baseline_ids:?}"
    );

    // Step 2: reranked recall — importance weight only (REPLACE strategy).
    let reranked = registry
        .dispatch(
            "recall",
            json!({
                "query": "gradient descent",
                "config": {
                    "reranker_weights": { "importance": 1.0 }
                }
            }),
        )
        .await
        .expect("recall with importance reranker");
    let reranked_hits = reranked.as_array().expect("reranked array");
    assert!(!reranked_hits.is_empty(), "must get results");
    let reranked_ids: Vec<String> = reranked_hits
        .iter()
        .map(|h| h["note_id"].as_str().unwrap().to_string())
        .collect();
    let top_id = &reranked_ids[0];

    // Step 3: assert the reranker placed high-salience memory at rank #1.
    assert_eq!(
        top_id, &high_id,
        "importance=1.0 reranker must rank the highest-salience memory first; got {top_id} not {high_id}"
    );

    // Step 4: assert the ordering actually changed — the reranker is not a no-op.
    // baseline_top != high_id (asserted above) and top_id == high_id, so orderings differ.
    assert_ne!(
        baseline_ids, reranked_ids,
        "reranker must change the result ordering; baseline={baseline_ids:?} reranked={reranked_ids:?}"
    );
}

/// PR #375: the recall.rerank subhandler applies request weights and returns
/// non-zero rerank_scores when reranker_weights are provided.
#[tokio::test]
async fn test_rerank_subhandler_uses_request_weights() {
    let rt = make_runtime();
    let registry = make_registry(rt);

    // Build two synthetic fused candidates with different fused_scores.
    // The one with higher fused_score should get a higher rerank_score
    // when relevance weight = 1.0.
    let candidates = json!([
        {
            "note_id": "00000000-0000-0000-0000-000000000001",
            "fused_score": 0.9,
            "source": "both"
        },
        {
            "note_id": "00000000-0000-0000-0000-000000000002",
            "fused_score": 0.3,
            "source": "text"
        }
    ]);

    let result = registry
        .dispatch(
            "recall.rerank",
            json!({
                "candidates": candidates,
                "config": {
                    "reranker_weights": { "relevance": 1.0 }
                }
            }),
        )
        .await
        .expect("recall.rerank succeeds");

    let reranked = result["reranked"].as_array().expect("reranked array");
    assert_eq!(reranked.len(), 2, "both candidates returned");

    // Find scores by note_id.
    let score_for = |id: &str| -> f64 {
        reranked
            .iter()
            .find(|c| c["note_id"].as_str() == Some(id))
            .and_then(|c| c["rerank_score"].as_f64())
            .unwrap_or(f64::NAN)
    };
    let score_high = score_for("00000000-0000-0000-0000-000000000001");
    let score_low = score_for("00000000-0000-0000-0000-000000000002");

    assert!(
        score_high.is_finite() && score_low.is_finite(),
        "rerank_score must be a finite number; got high={score_high} low={score_low}"
    );
    assert!(
        score_high > score_low,
        "candidate with fused_score=0.9 must outscore fused_score=0.3 under relevance reranker; \
         got {score_high} vs {score_low}"
    );

    // Verify active_rerankers field is present.
    let active = result["active_rerankers"]
        .as_array()
        .expect("active_rerankers");
    assert!(
        active.iter().any(|v| v.as_str() == Some("relevance")),
        "active_rerankers must include 'relevance'"
    );
}
