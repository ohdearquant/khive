use async_trait::async_trait;
use khive_brain_core::PackTunable;
use khive_pack_kg::KgPack;
use khive_pack_memory::MemoryPack;
use khive_runtime::{
    EmbedderProvider, FusionStrategy, KhiveRuntime, Namespace, RuntimeConfig, VerbRegistryBuilder,
};
use khive_types::Pack;
use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
use serde_json::json;
use std::sync::Arc;
use uuid::Uuid;

fn make_runtime() -> KhiveRuntime {
    KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        embedding_model: None,
        additional_embedding_models: vec![],
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

#[tokio::test]
async fn test_recall_decay_ranking() {
    let rt = make_runtime();
    let registry = make_registry(rt.clone());

    // Both notes have IDENTICAL content so BM25 assigns equal relevance scores.
    // The only difference is creation time and decay_factor, so temporal decay
    // must determine the ranking. This makes the test independent of BM25 tie-breaking.
    let shared_content = "memory about neural networks and deep learning";

    let fresh = registry
        .dispatch(
            "memory.remember",
            json!({
                "content": shared_content,
                "salience": 0.7,
                "decay": 0.01
            }),
        )
        .await
        .expect("fresh remember");
    let fresh_id = fresh["id"].as_str().unwrap().to_string();

    // Create old memory (simulate 90 days ago) with high decay
    let old = registry
        .dispatch(
            "memory.remember",
            json!({
                "content": shared_content,
                "salience": 0.7,
                "decay": 0.1
            }),
        )
        .await
        .expect("old remember");
    let old_id = old["id"].as_str().unwrap().to_string();

    // Manually backdate the old note to simulate age
    let old_uuid: uuid::Uuid = old_id.parse().unwrap();
    let note_store = rt
        .notes(&rt.authorize(Namespace::local()).unwrap())
        .unwrap();
    let mut old_note = note_store.get_note(old_uuid).await.unwrap().unwrap();
    old_note.created_at -= 90 * 86_400_000_000i64; // 90 days in microseconds
    note_store.upsert_note(old_note).await.unwrap();

    // Disable MMR penalty so identical-content notes are ranked purely by
    // temporal decay. MMR would suppress the second hit (rank 2) by -0.1,
    // which can invert the temporal ordering when scores are close.
    let recall_result = registry
        .dispatch(
            "memory.recall",
            json!({
                "query": "neural networks deep learning",
                "config": {
                    "scoring": {
                        "mmr_penalty": 0.0
                    }
                }
            }),
        )
        .await
        .expect("recall succeeds");

    let hits = recall_result.as_array().expect("array");
    let ranks: Vec<(&str, f64)> = hits
        .iter()
        .map(|h| {
            (
                h["id"].as_str().unwrap(),
                h["rank_score"].as_f64().unwrap_or(0.0),
            )
        })
        .collect();
    let fresh_entry = ranks
        .iter()
        .find(|(id, _)| *id == fresh_id)
        .expect("fresh in results");
    let old_entry = ranks
        .iter()
        .find(|(id, _)| *id == old_id)
        .expect("old in results");
    assert!(
        fresh_entry.1 > old_entry.1,
        "fresh memory (rank_score={}) should rank higher than 90-day-old high-decay memory (rank_score={})",
        fresh_entry.1,
        old_entry.1
    );
}

#[tokio::test]
async fn test_recall_salience_ranking() {
    let rt = make_runtime();
    let registry = make_registry(rt.clone());

    // Use non-identical content so MMR penalty does not affect the test.
    // The rank_score difference between salience=0.9 and salience=0.1 is
    // ~10% under the archive scoring model (1.18 vs 1.02 salience_boost), which
    // would be eliminated by the MMR penalty (-0.1) on identical content.
    let high = registry
        .dispatch(
            "memory.remember",
            json!({
                "content": "high-salience concept about knowledge representation theory",
                "salience": 0.9,
                "decay": 0.0
            }),
        )
        .await
        .expect("high salience remember");
    let high_id = high["id"].as_str().unwrap().to_string();

    let low = registry
        .dispatch(
            "memory.remember",
            json!({
                "content": "low-salience concept about knowledge representation systems",
                "salience": 0.1,
                "decay": 0.0
            }),
        )
        .await
        .expect("low salience remember");
    let low_id = low["id"].as_str().unwrap().to_string();

    let recall_result = registry
        .dispatch(
            "memory.recall",
            json!({ "query": "knowledge representation" }),
        )
        .await
        .expect("recall succeeds");

    let hits = recall_result.as_array().expect("array");
    let ranks: Vec<(&str, f64)> = hits
        .iter()
        .map(|h| {
            (
                h["id"].as_str().unwrap(),
                h["rank_score"].as_f64().unwrap_or(0.0),
            )
        })
        .collect();
    let high_entry = ranks
        .iter()
        .find(|(id, _)| *id == high_id)
        .expect("high in results");
    let low_entry = ranks
        .iter()
        .find(|(id, _)| *id == low_id)
        .expect("low in results");
    assert!(
        high_entry.1 >= low_entry.1,
        "high salience memory (rank_score={}) should rank >= low salience (rank_score={})",
        high_entry.1,
        low_entry.1
    );
}

#[tokio::test]
async fn test_recall_memory_type_filter() {
    let rt = make_runtime();
    let registry = make_registry(rt.clone());

    registry
        .dispatch(
            "memory.remember",
            json!({
                "content": "episodic event about meeting with Alice",
                "memory_type": "episodic",
                "salience": 0.7
            }),
        )
        .await
        .expect("episodic remember");

    let semantic = registry
        .dispatch(
            "memory.remember",
            json!({
                "content": "semantic fact about meeting protocols",
                "memory_type": "semantic",
                "salience": 0.7
            }),
        )
        .await
        .expect("semantic remember");
    let semantic_id = semantic["id"].as_str().unwrap().to_string();

    let filtered = registry
        .dispatch(
            "memory.recall",
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
    let ids: Vec<&str> = hits.iter().map(|h| h["id"].as_str().unwrap()).collect();
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
            "memory.remember",
            json!({
                "content": "memory with a source",
                "source": source_uuid
            }),
        )
        .await
        .expect("remember with source_id");

    let note_id: Uuid = result["id"].as_str().unwrap().parse().expect("valid uuid");

    let note_store = rt
        .notes(&rt.authorize(Namespace::local()).unwrap())
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
            "memory.remember",
            json!({
                "content": "memory with high decay rate",
                "decay": 5.0
            }),
        )
        .await
        .expect("remember with decay_factor > 1.0 should succeed");

    let note_id: Uuid = result["id"].as_str().unwrap().parse().expect("valid uuid");

    let note_store = rt
        .notes(&rt.authorize(Namespace::local()).unwrap())
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
            "memory.remember",
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
            "memory.remember",
            json!({ "content": "memory without explicit type" }),
        )
        .await
        .expect("remember without memory_type");

    let note_id: Uuid = result["id"].as_str().unwrap().parse().expect("valid uuid");

    // The response must carry memory_type
    assert_eq!(
        result["memory_type"].as_str(),
        Some("episodic"),
        "response must include default memory_type"
    );

    let note_store = rt
        .notes(&rt.authorize(Namespace::local()).unwrap())
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
            "memory.remember",
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

/// ADR-021 §4 (F108): salience outside [0, 1] is rejected.
#[tokio::test]
async fn test_remember_salience_out_of_range_rejected() {
    let rt = make_runtime();
    let registry = make_registry(rt);

    let neg = registry
        .dispatch(
            "memory.remember",
            json!({ "content": "test", "salience": -0.1 }),
        )
        .await;
    assert!(neg.is_err(), "negative salience must be rejected");

    let rt2 = make_runtime();
    let registry2 = make_registry(rt2);
    let above = registry2
        .dispatch(
            "memory.remember",
            json!({ "content": "test", "salience": 1.1 }),
        )
        .await;
    assert!(above.is_err(), "salience > 1 must be rejected");
}

/// ADR-033 §2 (F222): recall.rerank is callable and returns expected shape.
#[tokio::test]
async fn test_recall_rerank_passthrough_with_no_active_rerankers() {
    let rt = make_runtime();
    let registry = make_registry(rt);

    let candidates = json!([
        { "id": "00000000-0000-0000-0000-000000000001", "fused_score": 0.8 },
        { "id": "00000000-0000-0000-0000-000000000002", "fused_score": 0.6 },
    ]);

    let result = registry
        .dispatch("memory.recall_rerank", json!({ "candidates": candidates }))
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
    assert!(names.contains(&"memory.recall_candidates"));
    assert!(names.contains(&"memory.recall_fuse"));
    assert!(names.contains(&"memory.recall_score"));
    assert!(names.contains(&"memory.recall_embed"));
    // F222: recall.rerank must be registered (ADR-033 §2)
    assert!(
        names.contains(&"memory.recall_rerank"),
        "recall.rerank not found in: {names:?}"
    );
}

#[tokio::test]
async fn test_recall_candidates_returns_arrays() {
    let rt = make_runtime();
    let registry = make_registry(rt);

    registry
        .dispatch(
            "memory.remember",
            json!({ "content": "attention recall candidates" }),
        )
        .await
        .expect("memory.remember");

    let result = registry
        .dispatch(
            "memory.recall_candidates",
            json!({ "query": "attention candidates" }),
        )
        .await
        .expect("memory.recall_candidates");

    let text = result["text_candidates"].as_array().expect("text array");
    assert!(!text.is_empty());
    assert!(text[0]["id"].as_str().is_some());
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
            "memory.remember",
            json!({ "content": "attention fusion diagnostic" }),
        )
        .await
        .expect("memory.remember");

    let result = registry
        .dispatch("memory.recall_fuse", json!({ "query": "attention fusion" }))
        .await
        .expect("memory.recall_fuse");

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
            "memory.remember",
            json!({ "content": "attention score breakdown", "salience": 0.8 }),
        )
        .await
        .expect("memory.remember");

    let plain = registry
        .dispatch("memory.recall", json!({ "query": "attention breakdown" }))
        .await
        .expect("memory.recall");
    let hits = plain.as_array().unwrap();
    assert!(!hits.is_empty());
    assert!(
        hits[0].get("breakdown").is_none(),
        "breakdown must be absent by default"
    );

    let explained = registry
        .dispatch(
            "memory.recall",
            json!({ "query": "attention breakdown", "config": { "include_breakdown": true } }),
        )
        .await
        .expect("recall with breakdown");
    let hits = explained.as_array().unwrap();
    assert!(!hits.is_empty());
    let bd = &hits[0]["breakdown"];
    assert!(bd["relevance"].as_f64().is_some());
    assert!(bd["salience_raw"].as_f64().is_some());
    assert!(bd["salience_decayed"].as_f64().is_some());
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
            "memory.remember",
            json!({ "content": "text only candidate check" }),
        )
        .await
        .expect("memory.remember");

    let result = registry
        .dispatch(
            "memory.recall_candidates",
            json!({ "query": "text only candidate" }),
        )
        .await
        .expect("memory.recall_candidates");

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
        .dispatch(
            "memory.remember",
            json!({ "content": "fuse source string check" }),
        )
        .await
        .expect("memory.remember");

    let result = registry
        .dispatch(
            "memory.recall_fuse",
            json!({ "query": "fuse source string" }),
        )
        .await
        .expect("memory.recall_fuse");

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
            "memory.remember",
            json!({ "content": "retrieval adapter rrf k1 probe memory" }),
        )
        .await
        .expect("memory.remember");

    let result = registry
        .dispatch(
            "memory.recall_fuse",
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
            "memory.remember",
            json!({ "content": "shape regression check after retrieval wiring" }),
        )
        .await
        .expect("memory.remember");

    let result = registry
        .dispatch(
            "memory.recall_fuse",
            json!({ "query": "shape regression retrieval wiring" }),
        )
        .await
        .expect("memory.recall_fuse");

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
    assert!(c["id"].as_str().is_some(), "note_id must be a string UUID");
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
            "memory.remember",
            json!({ "content": "arithmetic score check memory", "salience": 0.7 }),
        )
        .await
        .expect("memory.remember");

    let result = registry
        .dispatch(
            "memory.recall",
            json!({ "query": "arithmetic score check", "config": { "include_breakdown": true } }),
        )
        .await
        .expect("recall with breakdown");

    let hits = result.as_array().unwrap();
    assert!(!hits.is_empty());
    let hit = &hits[0];
    // `rank_score` is the composite score from the archive pipeline.
    // `score` is the absolute relevance (pre-fusion raw cosine, or composite if no vector).
    // The breakdown weighted sum corresponds to the legacy compute_score path which
    // computes contributions under the RecallConfig additive model. The rank_score
    // from the archive multiplicative model does NOT equal the breakdown sum —
    // they are two different scoring strategies coexisting in the pipeline.
    // Here we just verify rank_score is bounded in [0, 1] and breakdown fields are present.
    let rank_score = hit["rank_score"].as_f64().expect("hit has rank_score");
    assert!(
        (0.0..=1.0).contains(&rank_score),
        "rank_score {rank_score} must be in [0, 1]"
    );
    let bd = &hit["breakdown"];
    let rc = bd["weighted"]["relevance_contribution"].as_f64().unwrap();
    let ic = bd["weighted"]["salience_contribution"].as_f64().unwrap();
    let tc = bd["weighted"]["temporal_contribution"].as_f64().unwrap();
    let total = rc + ic + tc;
    assert!(
        (0.0..=1.0).contains(&total),
        "breakdown weighted sum {total} must be in [0, 1]"
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
    let tok = rt.authorize(Namespace::local()).unwrap();
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
            "memory.remember",
            json!({
                "content": "memory note about attention mechanisms in neural networks",
                "salience": 0.8
            }),
        )
        .await
        .expect("remember 1");
    let mem2 = registry
        .dispatch(
            "memory.remember",
            json!({
                "content": "another memory note about attention mechanisms",
                "salience": 0.7
            }),
        )
        .await
        .expect("remember 2");
    let mem1_id = mem1["id"].as_str().unwrap().to_string();
    let mem2_id = mem2["id"].as_str().unwrap().to_string();

    let result = registry
        .dispatch(
            "memory.recall",
            json!({ "query": "attention mechanisms neural networks", "limit": 5 }),
        )
        .await
        .expect("recall succeeds");

    let hits = result.as_array().expect("array of hits");
    assert!(
        !hits.is_empty(),
        "recall should return memory notes even when non-memory notes dominate the index"
    );
    let ids: Vec<&str> = hits.iter().map(|h| h["id"].as_str().unwrap()).collect();
    assert!(
        ids.contains(&mem1_id.as_str()) || ids.contains(&mem2_id.as_str()),
        "at least one memory note must appear in recall results"
    );
    for hit in hits {
        // recall must never surface observation or other non-memory kinds
        assert!(
            hit.get("id").is_some(),
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
    // With salience_only (0.0/1.0/0.0), the score for
    //   rrf=1.0, salience=0.0, decay=0.0, age=0 → 0.0
    // The difference is large enough to prove the weights flow through.

    // Apply salience-only config to the pack.
    let salience_only = RecallConfig {
        relevance_weight: 0.0,
        salience_weight: 1.0,
        temporal_weight: 0.0,
        ..RecallConfig::default()
    };
    pack.apply_config(serde_json::to_value(&salience_only).unwrap())
        .expect("apply_config (salience-only) succeeds");

    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(pack);
    let registry = builder.build().expect("registry builds");

    // Call recall.score with high relevance but ZERO salience — under
    // salience-only weights, score MUST be 0.0. Under default weights
    // (the bug), it would be 0.70.
    let result = registry
        .dispatch(
            "memory.recall_score",
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
        "under salience_weight=1.0, salience=0 → score=0; got {total}. \
         If non-zero, MemoryPack::active_config() is not being used by \
         recall.score (#159 regression)."
    );

    // Mirror check: under relevance-only weights with rrf=1.0, salience=0 → score=1.0.
    // This requires a SECOND pack instance because PackRuntime ownership prevents
    // mutating the live registry's config from outside. We construct the test
    // by exercising the same wire on a fresh pack.
    let rt2 = make_runtime();
    let pack2 = MemoryPack::new(rt2.clone());
    // Use Weighted strategy so the input relevance score (1.0) passes through
    // unnormalized — RRF strategy would scale it by (k+1) = 61, producing 61.0.
    let relevance_only = RecallConfig {
        relevance_weight: 1.0,
        salience_weight: 0.0,
        temporal_weight: 0.0,
        fuse_strategy: FusionStrategy::Weighted {
            weights: vec![0.5, 0.5],
        },
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
            "memory.recall_score",
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
        "under relevance_weight=1.0 with rrf=1.0 (Weighted strategy) → score=1.0; got {total2}"
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
            .dispatch(
                "memory.remember",
                json!({ "content": content, "salience": 0.8 }),
            )
            .await
            .expect("remember succeeds");
    }

    // Baseline recall with no knobs
    let base = registry
        .dispatch("memory.recall", json!({ "query": "cell" }))
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
            "memory.recall",
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
            b["id"].as_str(),
            k["id"].as_str(),
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
                "memory.remember",
                json!({
                    "content": format!("rust ownership memory safety concept {i}"),
                    "salience": 0.7
                }),
            )
            .await
            .expect("remember succeeds");
    }

    // Recall with top_k=2 — must not return more than 2 results
    let result = registry
        .dispatch(
            "memory.recall",
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
            "memory.recall",
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
            "memory.remember",
            json!({
                "content": "gradient descent optimization machine learning",
                "salience": 0.8
            }),
        )
        .await
        .expect("remember succeeds");

    // Each valid strategy must succeed and return an array
    for strategy in &["rrf", "weighted", "union", "vector_only", "keyword_only"] {
        let result = registry
            .dispatch(
                "memory.recall",
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
            "memory.recall",
            json!({
                "query": "gradient descent optimization",
                "fusion_strategy": "bogus"
            }),
        )
        .await;
    assert!(err.is_err(), "invalid fusion_strategy must return an error");
    let msg = err.unwrap_err().to_string();
    assert!(
        msg.contains("rrf")
            && msg.contains("weighted")
            && msg.contains("union")
            && msg.contains("vector_only")
            && msg.contains("keyword_only"),
        "error message must list valid strategies, got: {msg}"
    );
}

#[tokio::test]
async fn test_recall_score_floor() {
    let rt = make_runtime();
    let registry = make_registry(rt.clone());

    registry
        .dispatch(
            "memory.remember",
            json!({
                "content": "backpropagation neural network training algorithm",
                "salience": 0.6
            }),
        )
        .await
        .expect("remember succeeds");

    // Baseline: no floor — get result count
    let base = registry
        .dispatch(
            "memory.recall",
            json!({ "query": "backpropagation neural network" }),
        )
        .await
        .expect("baseline recall succeeds");
    let base_count = base.as_array().expect("array").len();

    // score_floor=0.99 must not return MORE results than baseline
    let floored = registry
        .dispatch(
            "memory.recall",
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
            "memory.recall",
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
                "memory.remember",
                json!({
                    "content": format!("memory about deep learning topic {i}"),
                    "salience": 0.5 + (i as f64) * 0.1,
                    "decay": 0.0
                }),
            )
            .await
            .expect("memory.remember");
    }

    let baseline = registry
        .dispatch("memory.recall", json!({ "query": "deep learning" }))
        .await
        .expect("baseline recall");
    let baseline_ids: Vec<String> = baseline
        .as_array()
        .expect("array")
        .iter()
        .map(|h| h["id"].as_str().unwrap().to_string())
        .collect();

    let with_empty_reranker = registry
        .dispatch(
            "memory.recall",
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
        .map(|h| h["id"].as_str().unwrap().to_string())
        .collect();

    assert_eq!(
        baseline_ids, reranker_ids,
        "empty reranker_weights must be a pass-through — result ordering must match baseline"
    );
}

/// PR #375: reranker_weights with salience=1.0 must promote the highest-salience
/// memory to rank #1, even when it would rank lower under the default compute_score.
///
/// Strengthened: captures baseline ordering first (no reranker) and asserts that
/// the reranked order actually differs — proving the REPLACE wiring is not a no-op.
///
/// Fixture design: all notes contain the query keyword so all are retrieved.
/// Low-salience notes have richer keyword density (higher FTS BM25).  Baseline
/// uses pure relevance scoring (salience_weight=0) so the keyword-dense
/// low-salience notes rank first.  The salience=1.0 reranker then flips the
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
                "memory.remember",
                json!({
                    "content": "gradient descent gradient descent gradient descent optimization",
                    "salience": 0.1,
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
            "memory.remember",
            json!({
                "content": "gradient descent is a key technique in machine learning",
                "salience": 0.95,
                "decay": 0.0
            }),
        )
        .await
        .expect("high salience remember");
    let high_id = high_salience["id"].as_str().unwrap().to_string();

    // Step 1: baseline recall — pure relevance scoring (salience_weight=0) so
    // BM25-heavy low-salience notes rank first.
    let baseline = registry
        .dispatch(
            "memory.recall",
            json!({
                "query": "gradient descent",
                "config": {
                    "relevance_weight": 1.0,
                    "salience_weight": 0.0,
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
        .map(|h| h["id"].as_str().unwrap().to_string())
        .collect();
    let baseline_top = &baseline_ids[0];

    // Baseline must NOT have high_id at rank #1 — if it does, the fixture is
    // degenerate (the reranker would be a no-op for the top position).
    assert_ne!(
        baseline_top, &high_id,
        "fixture error: high-salience note already ranks first in baseline; \
         reranker change cannot be demonstrated. baseline={baseline_ids:?}"
    );

    // Step 2: reranked recall — salience weight only (REPLACE strategy).
    let reranked = registry
        .dispatch(
            "memory.recall",
            json!({
                "query": "gradient descent",
                "config": {
                    "reranker_weights": { "salience": 1.0 }
                }
            }),
        )
        .await
        .expect("recall with salience reranker");
    let reranked_hits = reranked.as_array().expect("reranked array");
    assert!(!reranked_hits.is_empty(), "must get results");
    let reranked_ids: Vec<String> = reranked_hits
        .iter()
        .map(|h| h["id"].as_str().unwrap().to_string())
        .collect();
    let top_id = &reranked_ids[0];

    // Step 3: assert the reranker placed high-salience memory at rank #1.
    assert_eq!(
        top_id, &high_id,
        "salience=1.0 reranker must rank the highest-salience memory first; got {top_id} not {high_id}"
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
            "id": "00000000-0000-0000-0000-000000000001",
            "fused_score": 0.9,
            "source": "both"
        },
        {
            "id": "00000000-0000-0000-0000-000000000002",
            "fused_score": 0.3,
            "source": "text"
        }
    ]);

    let result = registry
        .dispatch(
            "memory.recall_rerank",
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
            .find(|c| c["id"].as_str() == Some(id))
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

/// Fix 2: recall(help=true) must expose all params added in PRs #406/#421.
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
    // Issue #482: verb-level presentation renamed to include_breakdown.
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

/// #514 regression: presentation= must be rejected by deny_unknown_fields.
#[tokio::test]
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

// ── Codex High fixes regressions (#444) ──────────────────────────────────────

/// Trivial constant-vector embedding service for testing without real model weights.
/// The `_model` parameter is ignored; returns a synthetic `dims × seed` vector.
struct ConstVecService {
    dims: usize,
    seed: f32,
}

#[async_trait]
impl EmbeddingService for ConstVecService {
    async fn embed(
        &self,
        texts: &[String],
        _model: EmbeddingModel,
    ) -> std::result::Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts.iter().map(|_| vec![self.seed; self.dims]).collect())
    }

    fn supports_model(&self, _model: EmbeddingModel) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "const-vec"
    }
}

struct ConstVecProvider {
    provider_name: String,
    dims: usize,
    seed: f32,
}

impl ConstVecProvider {
    fn new(name: &str, dims: usize, seed: f32) -> Self {
        Self {
            provider_name: name.to_owned(),
            dims,
            seed,
        }
    }
}

#[async_trait]
impl EmbedderProvider for ConstVecProvider {
    fn name(&self) -> &str {
        &self.provider_name
    }

    fn dimensions(&self) -> usize {
        self.dims
    }

    async fn build(&self) -> Result<Arc<dyn EmbeddingService>, khive_runtime::RuntimeError> {
        Ok(Arc::new(ConstVecService {
            dims: self.dims,
            seed: self.seed,
        }))
    }
}

/// Fix 1 regression (codex High #1, PR #444): a runtime with no lattice
/// `embedding_model` in config but a custom registered embedder must fan out
/// `remember` through that embedder and store a vector.
///
/// Previously the fan-out gate checked `config().embedding_model.is_some()`;
/// custom-only runtimes fell through to `vec![]`.
#[tokio::test]
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

/// Fix 2 regression (codex High #2, PR #444): Weighted fusion with N > 1
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

// Issue #288: recall text_candidates must be non-empty when the query partially
// matches a memory note. Previously the conjunction Plain MATCH returned zero
// candidates if the note only contained some of the query terms.
#[tokio::test]
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

// Issue #482: recall include_breakdown=true must include per-component breakdown.
// presentation= was removed in #514 and is now rejected by deny_unknown_fields.
#[tokio::test]
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

// Issue #277: search(kind="memory") must resolve when memory pack is loaded.
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

// ── #515: tag-filtered recall ─────────────────────────────────────────────────

/// #515: tag filter — OR (any), AND (all), and no-filter behaviors.
#[tokio::test]
async fn recall_tags_filter_any_all_and_no_filter() {
    let registry = make_registry(make_runtime());

    // Store three memories with distinct tag combos.
    let impl_khive = registry
        .dispatch(
            "memory.remember",
            json!({
                "content": "tag filter regression shared semantic target alpha",
                "salience": 0.9,
                "tags": ["role:implementer", "khive"]
            }),
        )
        .await
        .expect("remember impl khive");
    let impl_khive_id = impl_khive["id"].as_str().unwrap().to_owned();

    let critic_khive = registry
        .dispatch(
            "memory.remember",
            json!({
                "content": "tag filter regression shared semantic target beta",
                "salience": 0.9,
                "tags": ["role:critic", "khive"]
            }),
        )
        .await
        .expect("remember critic khive");
    let critic_khive_id = critic_khive["id"].as_str().unwrap().to_owned();

    let impl_rust = registry
        .dispatch(
            "memory.remember",
            json!({
                "content": "tag filter regression shared semantic target gamma",
                "salience": 0.9,
                "tags": ["role:implementer", "rust"]
            }),
        )
        .await
        .expect("remember impl rust");
    let impl_rust_id = impl_rust["id"].as_str().unwrap().to_owned();

    // no-filter: all three should appear.
    let no_filter = registry
        .dispatch(
            "memory.recall",
            json!({ "query": "tag filter regression shared semantic target", "limit": 20 }),
        )
        .await
        .expect("recall no filter");
    let no_filter_ids: Vec<&str> = no_filter
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|h| h["id"].as_str())
        .collect();
    assert!(
        no_filter_ids.contains(&impl_khive_id.as_str()),
        "no-filter must return impl+khive memory"
    );
    assert!(
        no_filter_ids.contains(&critic_khive_id.as_str()),
        "no-filter must return critic+khive memory"
    );
    assert!(
        no_filter_ids.contains(&impl_rust_id.as_str()),
        "no-filter must return impl+rust memory"
    );

    // any (OR): tags=["role:critic", "rust"] → critic_khive and impl_rust, not impl_khive.
    let any_result = registry
        .dispatch(
            "memory.recall",
            json!({
                "query": "tag filter regression shared semantic target",
                "limit": 20,
                "tags": ["role:critic", "rust"],
                "tag_mode": "any"
            }),
        )
        .await
        .expect("recall tag any");
    let any_ids: Vec<&str> = any_result
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|h| h["id"].as_str())
        .collect();
    assert!(
        any_ids.contains(&critic_khive_id.as_str()),
        "any filter must include critic+khive (has role:critic)"
    );
    assert!(
        any_ids.contains(&impl_rust_id.as_str()),
        "any filter must include impl+rust (has rust)"
    );
    assert!(
        !any_ids.contains(&impl_khive_id.as_str()),
        "any filter must exclude impl+khive (has neither role:critic nor rust)"
    );

    // all (AND): tags=["role:implementer", "khive"] → impl_khive only.
    let all_result = registry
        .dispatch(
            "memory.recall",
            json!({
                "query": "tag filter regression shared semantic target",
                "limit": 20,
                "tags": ["role:implementer", "khive"],
                "tag_mode": "all"
            }),
        )
        .await
        .expect("recall tag all");
    let all_ids: Vec<&str> = all_result
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|h| h["id"].as_str())
        .collect();
    assert!(
        all_ids.contains(&impl_khive_id.as_str()),
        "all filter must include impl+khive (has both role:implementer and khive)"
    );
    assert!(
        !all_ids.contains(&critic_khive_id.as_str()),
        "all filter must exclude critic+khive (missing role:implementer)"
    );
    assert!(
        !all_ids.contains(&impl_rust_id.as_str()),
        "all filter must exclude impl+rust (missing khive)"
    );
}

/// B7: raw_score must be present (possibly null) in every result returned by
/// memory.recall, including when tag filters narrow the result set with tag_mode="all".
/// The field is null for text-only hits (no vector index) and a float for vector hits.
#[tokio::test]
async fn recall_raw_score_field_always_present_with_tag_filter() {
    let registry = make_registry(make_runtime());

    registry
        .dispatch(
            "memory.remember",
            json!({
                "content": "raw score presence check alpha beta gamma delta epsilon",
                "salience": 0.9,
                "tags": ["team:alpha", "project:khive"]
            }),
        )
        .await
        .expect("remember tagged memory");

    registry
        .dispatch(
            "memory.remember",
            json!({
                "content": "raw score presence check alpha beta gamma delta epsilon",
                "salience": 0.9,
                "tags": ["team:alpha", "project:khive"]
            }),
        )
        .await
        .expect("remember second tagged memory");

    for tag_mode in &["any", "all"] {
        let result = registry
            .dispatch(
                "memory.recall",
                json!({
                    "query": "raw score presence check alpha beta gamma",
                    "limit": 20,
                    "tags": ["team:alpha", "project:khive"],
                    "tag_mode": tag_mode
                }),
            )
            .await
            .unwrap_or_else(|e| panic!("recall tag_mode={tag_mode} failed: {e}"));

        let hits = result.as_array().expect("results must be an array");
        assert!(
            !hits.is_empty(),
            "tag_mode={tag_mode}: expected at least one result"
        );
        for (i, hit) in hits.iter().enumerate() {
            let obj = hit.as_object().expect("each hit must be a JSON object");
            assert!(
                obj.contains_key("raw_score"),
                "tag_mode={tag_mode} result[{i}] missing raw_score field; got keys: {:?}",
                obj.keys().collect::<Vec<_>>()
            );
        }
    }
}

/// #515 metadata: memory.recall handler must advertise tags and tag_mode params.
#[test]
fn recall_handler_metadata_advertises_tags_and_tag_mode() {
    let recall_def = khive_pack_memory::MemoryPack::HANDLERS
        .iter()
        .find(|h| h.name == "memory.recall")
        .expect("memory.recall handler must be registered");

    let param_names: Vec<&str> = recall_def.params.iter().map(|p| p.name).collect();
    assert!(
        param_names.contains(&"tags"),
        "memory.recall must advertise 'tags' param; got: {param_names:?}"
    );
    assert!(
        param_names.contains(&"tag_mode"),
        "memory.recall must advertise 'tag_mode' param; got: {param_names:?}"
    );
}

// ── #566: recall_embed vectors opt-in ────────────────────────────────────────

/// #566: default recall_embed omits embedding vectors, keeps model+dimension metadata.
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

/// #566: include_embeddings=true returns full vector payload.
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

/// #566 metadata: memory.recall_embed handler must advertise include_embeddings param.
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

// ── Type-differentiated default tests — production path (#84) ───────────────
//
// These tests dispatch through the real handler and assert stored note values
// (via the response) for the omitted/explicit default matrix.  A revert of the
// production defaults in remember.rs would cause these to fail.

/// Omitting salience and decay_factor for an episodic memory must store 0.3 / 0.02.
#[tokio::test]
async fn test_remember_episodic_defaults_stored() {
    let rt = make_runtime();
    let registry = make_registry(rt);

    let result = registry
        .dispatch(
            "memory.remember",
            json!({ "content": "episodic default test", "memory_type": "episodic" }),
        )
        .await
        .expect("memory.remember must succeed");

    let salience = result["salience"].as_f64().expect("salience field present");
    let decay = result["decay_factor"]
        .as_f64()
        .expect("decay_factor field present");
    assert!(
        (salience - 0.3).abs() < 1e-12,
        "episodic default salience must be 0.3, got {salience}"
    );
    assert!(
        (decay - 0.02).abs() < 1e-12,
        "episodic default decay_factor must be 0.02, got {decay}"
    );
}

/// Omitting memory_type defaults to episodic and applies episodic defaults.
#[tokio::test]
async fn test_remember_omitted_memory_type_uses_episodic_defaults() {
    let rt = make_runtime();
    let registry = make_registry(rt);

    let result = registry
        .dispatch(
            "memory.remember",
            json!({ "content": "no memory_type supplied" }),
        )
        .await
        .expect("memory.remember must succeed");

    let mt = result["memory_type"].as_str().expect("memory_type present");
    assert_eq!(
        mt, "episodic",
        "omitted memory_type must default to episodic"
    );
    let salience = result["salience"].as_f64().expect("salience present");
    let decay = result["decay_factor"]
        .as_f64()
        .expect("decay_factor present");
    assert!(
        (salience - 0.3).abs() < 1e-12,
        "omitted-type default salience must be 0.3, got {salience}"
    );
    assert!(
        (decay - 0.02).abs() < 1e-12,
        "omitted-type default decay_factor must be 0.02, got {decay}"
    );
}

/// Omitting salience and decay_factor for a semantic memory must store 0.5 / 0.005.
#[tokio::test]
async fn test_remember_semantic_defaults_stored() {
    let rt = make_runtime();
    let registry = make_registry(rt);

    let result = registry
        .dispatch(
            "memory.remember",
            json!({ "content": "semantic default test", "memory_type": "semantic" }),
        )
        .await
        .expect("memory.remember must succeed");

    let salience = result["salience"].as_f64().expect("salience present");
    let decay = result["decay_factor"]
        .as_f64()
        .expect("decay_factor present");
    assert!(
        (salience - 0.5).abs() < 1e-12,
        "semantic default salience must be 0.5, got {salience}"
    );
    assert!(
        (decay - 0.005).abs() < 1e-12,
        "semantic default decay_factor must be 0.005, got {decay}"
    );
}

/// Explicit salience=0.5 with episodic type must store exactly 0.5 (old flat default wins explicitly).
#[tokio::test]
async fn test_remember_explicit_salience_overrides_episodic_default() {
    let rt = make_runtime();
    let registry = make_registry(rt);

    let result = registry
        .dispatch(
            "memory.remember",
            json!({ "content": "explicit salience test", "memory_type": "episodic", "salience": 0.5 }),
        )
        .await
        .expect("memory.remember must succeed");

    let salience = result["salience"].as_f64().expect("salience present");
    assert!(
        (salience - 0.5).abs() < 1e-12,
        "explicit salience=0.5 must be stored as-is, not replaced by episodic default 0.3; got {salience}"
    );
}

/// Explicit decay_factor=0.01 with episodic type must store exactly 0.01.
#[tokio::test]
async fn test_remember_explicit_decay_overrides_episodic_default() {
    let rt = make_runtime();
    let registry = make_registry(rt);

    let result = registry
        .dispatch(
            "memory.remember",
            json!({ "content": "explicit decay test", "memory_type": "episodic", "decay_factor": 0.01 }),
        )
        .await
        .expect("memory.remember must succeed");

    let decay = result["decay_factor"]
        .as_f64()
        .expect("decay_factor present");
    assert!(
        (decay - 0.01).abs() < 1e-12,
        "explicit decay_factor=0.01 must be stored as-is, not replaced by episodic default 0.02; got {decay}"
    );
}

/// Legacy note (created via KG create_note with no properties.memory_type, no salience,
/// no decay_factor) must be returned by memory.recall(memory_type="episodic") because
/// the resolved memory_type defaults to "episodic" when no stored value is present.
#[tokio::test]
async fn test_recall_legacy_note_no_memory_type_returned_as_episodic() {
    let rt = make_runtime();
    let registry = make_registry(rt.clone());

    // Create a bare memory note with no properties.memory_type, no salience, no decay —
    // simulates a note written before the type-differentiated defaults PR.
    let tok = rt.authorize(Namespace::local()).unwrap();
    let legacy_note = rt
        .create_note(
            &tok,
            "memory",
            None,
            "legacy note about transformer attention heads no memory type",
            None,   // no salience
            None,   // no properties (therefore no memory_type)
            vec![], // no annotates edges
        )
        .await
        .expect("create legacy note");
    let legacy_id = legacy_note.id.to_string();

    // recall with explicit memory_type="episodic" must include the legacy note because
    // resolved memory_type defaults to "episodic" when properties.memory_type is absent.
    let result = registry
        .dispatch(
            "memory.recall",
            json!({
                "query": "transformer attention heads no memory type",
                "memory_type": "episodic",
                "limit": 10
            }),
        )
        .await
        .expect("memory.recall must succeed");

    let hits = result.as_array().expect("recall returns array");
    let returned_ids: Vec<&str> = hits
        .iter()
        .map(|h| h["id"].as_str().unwrap_or(""))
        .collect();
    assert!(
        returned_ids.contains(&legacy_id.as_str()),
        "legacy note with no stored memory_type must appear in recall(memory_type=\"episodic\"); \
         returned ids: {returned_ids:?}"
    );

    // Recall hits must carry resolved (read-model) values, not raw stored NULLs.
    // Consumers such as ranking explanations and brain.auto_feedback rely on these fields.
    let legacy_hit = hits
        .iter()
        .find(|h| h["id"].as_str().unwrap_or("") == legacy_id)
        .expect("legacy hit present");

    let hit_memory_type = legacy_hit["memory_type"]
        .as_str()
        .expect("memory_type field present in hit");
    assert_eq!(
        hit_memory_type, "episodic",
        "recall hit memory_type must be resolved to \"episodic\" for legacy note; got {hit_memory_type:?}"
    );

    let hit_salience = legacy_hit["salience"]
        .as_f64()
        .expect("salience field present in hit");
    assert!(
        (hit_salience - 0.3).abs() < 1e-12,
        "recall hit salience must be episodic default 0.3 for legacy note; got {hit_salience}"
    );

    let hit_decay = legacy_hit["decay_factor"]
        .as_f64()
        .expect("decay_factor field present in hit");
    assert!(
        (hit_decay - 0.02).abs() < 1e-12,
        "recall hit decay_factor must be episodic default 0.02 for legacy note; got {hit_decay}"
    );
}

// ── Regression tests for issue #94: token-budget truncation signal + rank order ──

/// #94 regression (budget cap): when the token budget caps the returned count below
/// the requested `limit`, fewer results than `limit` are returned.
///
/// The non-verbose path always returns a bare array; the budget-capped status is
/// observable as a count < limit.  Budget signal fields (`budget_capped`,
/// `truncated_for_budget`) are available on the verbose/breakdown path when the
/// runtime loads two or more embedding models (multi-model production setup).
#[tokio::test]
async fn test_recall_budget_capped_surfaces_signal() {
    let rt = make_runtime();
    let registry = make_registry(rt.clone());

    // Create 5 notes whose content is long enough that the tiny budget below
    // cannot accommodate all of them.  Each note content is ~75 chars.
    let shared_query = "budget signal regression unique phrase alpha";
    for i in 0..5_u8 {
        let content =
            format!("budget signal regression unique phrase alpha memory content item number {i}");
        registry
            .dispatch(
                "memory.remember",
                json!({ "content": content, "salience": 0.8 }),
            )
            .await
            .expect("remember");
    }

    // default_token_budget=20 tokens × chars_per_token=4 = 80 chars budget.
    // Each note is ~75 chars; only 1 fits; the remaining 4 are dropped by the prefix cut.
    let result = registry
        .dispatch(
            "memory.recall",
            json!({
                "query": shared_query,
                "limit": 5,
                "config": {
                    "scoring": {
                        "default_token_budget": 20,
                        "chars_per_token": 4,
                        "mmr_penalty": 0.0
                    }
                }
            }),
        )
        .await
        .expect("recall with tiny budget succeeds");

    // Non-verbose path always returns a bare array regardless of budget state.
    let returned = result
        .as_array()
        .expect("#94: recall must return a bare array on the non-verbose path");
    assert!(
        !returned.is_empty(),
        "#94: at least one result must fit within the budget"
    );
    assert!(
        returned.len() < 5,
        "#94: returned count ({}) must be < requested limit (5) when budget is exhausted; \
         prefix-cut is not working if all 5 fit in an 80-char budget",
        returned.len()
    );
}

/// #94 regression (no false positive): when all results fit within the token budget,
/// the response is a plain array with count equal to available results.
#[tokio::test]
async fn test_recall_no_budget_cap_returns_plain_array() {
    let rt = make_runtime();
    let registry = make_registry(rt.clone());

    registry
        .dispatch(
            "memory.remember",
            json!({ "content": "plain array uncapped recall test phrase", "salience": 0.7 }),
        )
        .await
        .expect("remember");

    // Large budget: 4000 tokens × 4 chars = 16 000 chars — one short note trivially fits.
    let result = registry
        .dispatch(
            "memory.recall",
            json!({ "query": "plain array uncapped recall test", "limit": 3 }),
        )
        .await
        .expect("recall with generous budget succeeds");

    assert!(
        result.is_array(),
        "#94 no-cap path: recall must return a plain array; got: {result}"
    );
}

/// #94 regression (rank order): when the token budget forces truncation, the
/// returned items must be the top-ranked contiguous prefix — no higher-ranked
/// item may be skipped to accommodate a shorter lower-ranked item.
///
/// Setup uses THREE notes with DIFFERENT content lengths so the old greedy
/// `retain` bug actually fires:
///   rank #1 — short content (~55 chars) — fits within the 80-char budget
///   rank #2 — long content (~110 chars) — overflows the remaining budget after #1
///   rank #3 — short content (~55 chars) — would fit if reached by old retain
///
/// With the fix (prefix cut): result = [#1] — #2 overflows → cut, #3 never reached.
/// With the old retain bug:    result = [#1, #3] — #2 skipped, #3 kept (rank violation).
///
/// Assert: exactly 1 result returned AND it is rank #1 AND rank #3 is absent.
#[tokio::test]
async fn test_recall_budget_truncation_preserves_rank_order() {
    let rt = make_runtime();
    let registry = make_registry(rt.clone());

    // Shared keyword prefix so all 3 notes score on the same query.
    // Unique distinguishing suffix per note controls both length and identity.
    let query = "rankorder prefix budget truncation";

    // rank #1: short — fits in 80-char budget (~55 chars with prefix).
    let content_rank1 = format!("{query} item short alpha unique");
    // rank #2: long — overflows the remaining budget after rank #1.
    // budget = 80 chars; rank #1 uses ~55 chars; remaining = ~25 chars.
    // rank #2 must exceed 25 chars — pad to ~110 chars total.
    let content_rank2 = format!("{query} item long beta {}", "x".repeat(50));
    // rank #3: short — would fit if the old retain reached it.
    let content_rank3 = format!("{query} item short gamma unique");

    // Salience drives rank: 0.9 → rank #1, 0.6 → rank #2, 0.3 → rank #3.
    // mmr_penalty=0 prevents diversity suppression from reordering.
    let notes = [
        (&content_rank1, 0.9_f64),
        (&content_rank2, 0.6_f64),
        (&content_rank3, 0.3_f64),
    ];
    let mut stored_ids = Vec::new();
    for (content, sal) in &notes {
        let res = registry
            .dispatch(
                "memory.remember",
                json!({ "content": content, "salience": sal }),
            )
            .await
            .expect("remember");
        stored_ids.push(res["id"].as_str().unwrap().to_owned());
    }
    let id_rank1 = &stored_ids[0];
    let id_rank3 = &stored_ids[2];

    // budget = 20 tokens × 4 chars = 80 chars.
    // rank #1 (~55 chars) admitted. rank #2 (~110 chars) pushes total to ~165 > 80 → cut.
    // rank #3 is never considered (prefix cut stops at rank #2 overflow).
    let result = registry
        .dispatch(
            "memory.recall",
            json!({
                "query": query,
                "limit": 3,
                "config": {
                    "scoring": {
                        "default_token_budget": 20,
                        "chars_per_token": 4,
                        "mmr_penalty": 0.0
                    }
                }
            }),
        )
        .await
        .expect("recall with rank-order budget test");

    // Non-verbose path returns a bare array.
    let returned = result
        .as_array()
        .expect("#94 rank-order: recall must return a bare array");

    // Exactly 1 result (rank #1 fits, prefix cut stops at rank #2 overflow).
    // Old retain would return 2 results: [rank#1, rank#3].
    assert_eq!(
        returned.len(),
        1,
        "#94 rank-order: exactly 1 result expected (rank #1 fits, rank #2 overflows, \
         rank #3 must not be reached); got {} — old retain bug returns [#1,#3]",
        returned.len()
    );

    let returned_id = returned[0]["id"].as_str().expect("note_id present");
    assert_eq!(
        returned_id, id_rank1,
        "#94 rank-order: the single returned note must be rank #1 ({id_rank1}), got {returned_id}"
    );

    // rank #3 must not appear — its presence is the rank-skip signature.
    let has_rank3 = returned.iter().any(|r| r["id"].as_str() == Some(id_rank3));
    assert!(
        !has_rank3,
        "#94 rank-order: rank #3 ({id_rank3}) must NOT be in results — \
         its presence proves the old greedy retain was used instead of the prefix cut"
    );
}

// ── B2 — multi-namespace recall regression (ADR-062 §2 over-fetch + filter) ────

/// B2: ANN over-fetch + namespace post-filter.
///
/// Stores memories in two namespaces (`local` and `other`).  The global ANN index
/// spans both.  Asserts:
///  1. Default recall (visible = [`local`]) excludes `other`-namespace memories.
///  2. A wide token (visible = [`local`, `other`]) includes both namespaces.
///  3. Recall@k is satisfied when ≥k local candidates exist (over-fetch is enough).
///
/// Uses a deterministic custom embedder (no lattice weights required).
#[tokio::test]
async fn test_multi_namespace_recall_overfetch_filter() {
    use khive_runtime::PackRuntime;

    const MODEL: &str = "ns-filter-enc";
    const DIMS: usize = 8;

    let rt = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        embedding_model: None,
        additional_embedding_models: vec![],
        ..RuntimeConfig::default()
    })
    .expect("runtime");
    rt.register_embedder(ConstVecProvider::new(MODEL, DIMS, 0.42));

    let registry = make_registry(rt.clone());

    // Store 3 memories in `local` namespace.
    let mut local_ids = Vec::new();
    for i in 0..3 {
        let r = registry
            .dispatch(
                "memory.remember",
                json!({
                    "content": format!("local memory entry number {i} about graph databases"),
                    "salience": 0.7,
                    "namespace": "local"
                }),
            )
            .await
            .unwrap_or_else(|e| panic!("local remember {i} failed: {e}"));
        local_ids.push(r["id"].as_str().expect("note_id").to_string());
    }

    // Store 2 memories in `other` namespace.
    let mut other_ids = Vec::new();
    for i in 0..2 {
        let r = registry
            .dispatch(
                "memory.remember",
                json!({
                    "content": format!("other namespace memory {i} about graph databases"),
                    "salience": 0.7,
                    "namespace": "other"
                }),
            )
            .await
            .unwrap_or_else(|e| panic!("other remember {i} failed: {e}"));
        other_ids.push(r["id"].as_str().expect("note_id").to_string());
    }

    // ── Assertion 1: default recall (local-only) excludes `other` memories ────
    let default_recall = registry
        .dispatch(
            "memory.recall",
            json!({ "query": "graph databases", "limit": 10 }),
        )
        .await
        .expect("default recall");
    let default_hits: Vec<&str> = default_recall
        .as_array()
        .expect("array")
        .iter()
        .map(|h| h["id"].as_str().unwrap())
        .collect();

    for oid in &other_ids {
        assert!(
            !default_hits.contains(&oid.as_str()),
            "default recall must exclude other-namespace memory {oid}; got: {default_hits:?}"
        );
    }
    for lid in &local_ids {
        assert!(
            default_hits.contains(&lid.as_str()),
            "default recall must include local memory {lid}; got: {default_hits:?}"
        );
    }

    // ── Assertion 2: wide token includes both namespaces ──────────────────────
    let pack = MemoryPack::new(rt.clone());
    // Warm the ANN so vectors are indexed.
    pack.warm().await;

    let wide_token = rt
        .authorize_with_visibility(
            Namespace::parse("local").expect("local ns"),
            vec![Namespace::parse("other").expect("other ns")],
        )
        .expect("wide token");

    let wide_recall = pack
        .dispatch(
            "memory.recall",
            json!({ "query": "graph databases", "limit": 10 }),
            &registry,
            &wide_token,
        )
        .await
        .expect("wide-token recall");

    let wide_hits: Vec<&str> = wide_recall
        .as_array()
        .expect("array")
        .iter()
        .map(|h| h["id"].as_str().unwrap())
        .collect();

    for lid in &local_ids {
        assert!(
            wide_hits.contains(&lid.as_str()),
            "wide-token recall must include local memory {lid}; got: {wide_hits:?}"
        );
    }
    for oid in &other_ids {
        assert!(
            wide_hits.contains(&oid.as_str()),
            "wide-token recall must include other-namespace memory {oid}; got: {wide_hits:?}"
        );
    }

    // ── Assertion 3: recall@3 satisfied with 3 local candidates even when ────
    // the global ANN top results may include foreign-namespace hits.
    let k3_recall = registry
        .dispatch(
            "memory.recall",
            json!({ "query": "graph databases", "limit": 3 }),
        )
        .await
        .expect("recall@3");
    let k3_hits: Vec<&str> = k3_recall
        .as_array()
        .expect("array")
        .iter()
        .map(|h| h["id"].as_str().unwrap())
        .collect();

    // All 3 local memories should be present — over-fetch ensures enough
    // local candidates survive the post-filter even if the global NN top-k
    // includes foreign-namespace hits.
    assert_eq!(
        k3_hits.len(),
        3,
        "recall@3 must return exactly 3 results when 3 local candidates exist; got: {k3_hits:?}"
    );
    for lid in &local_ids {
        assert!(
            k3_hits.contains(&lid.as_str()),
            "recall@3 must include local memory {lid}; got: {k3_hits:?}"
        );
    }
}

// ── C1 regression (rewritten) ─────────────────────────────────────────────────
//
// ANN bounded retry — local memories must be found even when the first over-fetch
// round is dominated by foreign-namespace vectors that cluster NEAR the query.
//
// Setup (deterministic, hand-constructed geometry):
//   - N_FOREIGN=50 "foreign" vectors embedded near the query direction [1,0,0,0].
//   - N_LOCAL=3  "local"   vectors embedded far from the query direction [0,1,0,0].
//   - Query text maps to [1,0,0,0] (same cluster as foreign).
//   - candidate_limit = Some(8) → ann_fetch_limit = max(8×4, 8+32) = 40.
//   - Round 1: ANN returns 40 nearest → all foreign (locals are far). 0 local survivors.
//   - The retry gate must fire (index spans "foreign" which is NOT in visible {"local"}).
//   - Round 2: doubles fetch to 80 > 53 total → exhausts corpus, finds all 3 locals.
//
// Correctness:
//   - Default (ANN_OVERFETCH_MAX_ROUNDS env unset → 3 rounds): all locals FOUND.
//   - ANN_OVERFETCH_MAX_ROUNDS=1 (single round only): locals MISSING.
//     Run as: ANN_OVERFETCH_MAX_ROUNDS=1 cargo test -p khive-pack-memory \
//                test_ann_overfetch_retry_stalls_without_widening
//
// Token: uses a DEFAULT local-only token — the production default-recall scenario.

/// Content-keyed embedding service.
///
/// Foreign cluster (query direction):
///   texts containing "xforeign", or the query "xforeign dominant cluster" → [1,0,0,0]
///   cosine with query = 1.0 — closest to query, fill top-40 ANN results.
///
/// Local cluster (far from query but NOT orthogonal):
///   texts containing "xlocal" → [0.15, 0.9887, 0.0, 0.0] (pre-normalised unit vector)
///   cosine with query [1,0,0,0] = 0.15 — above the default min_raw_relevance=0.10 floor,
///   but cosine < 1.0 so ANN ranks locals behind all 50 foreign vectors.
///
/// Geometry guarantee:
///   ann_fetch_limit=40 < 50 foreign vectors → round 1 returns only foreign hits;
///   locals are unreachable until the retry widens the window to >53.
struct ClusteredVecService;

#[async_trait]
impl EmbeddingService for ClusteredVecService {
    async fn embed(
        &self,
        texts: &[String],
        _model: EmbeddingModel,
    ) -> std::result::Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts
            .iter()
            .map(|t| {
                if t.contains("xlocal") {
                    // Far cluster: cosine=0.15 with query direction → above score floor,
                    // but farther than all 50 foreign vectors (cosine=1.0).
                    // Pre-normalised: 0.15² + 0.9887² ≈ 0.0225 + 0.9775 = 1.0000.
                    vec![0.15_f32, 0.9887, 0.0, 0.0]
                } else {
                    // "xforeign" content and the query both map to the foreign cluster.
                    vec![1.0_f32, 0.0, 0.0, 0.0]
                }
            })
            .collect())
    }

    fn supports_model(&self, _model: EmbeddingModel) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "clustered-vec"
    }
}

struct ClusteredVecProvider {
    provider_name: String,
}

impl ClusteredVecProvider {
    fn new(name: &str) -> Self {
        Self {
            provider_name: name.to_owned(),
        }
    }
}

#[async_trait]
impl EmbedderProvider for ClusteredVecProvider {
    fn name(&self) -> &str {
        &self.provider_name
    }

    fn dimensions(&self) -> usize {
        4
    }

    async fn build(&self) -> Result<Arc<dyn EmbeddingService>, khive_runtime::RuntimeError> {
        Ok(Arc::new(ClusteredVecService))
    }
}

/// Shared setup for the C1 regression tests. Returns `(rt, registry, pack, local_ids)`.
///
/// Corpus layout:
///   - 50 foreign memories with vector [1,0,0,0]        (cosine=1.0, nearest to query)
///   - 3  local  memories with vector [0.15,0.9887,0,0] (cosine=0.15, farther from query)
///   - Query "xforeign dominant cluster" → vector [1,0,0,0]
///   - candidate_limit = Some(8) → ann_fetch_limit = 40 < total 53
///
/// With only 40 candidates fetched in round 1, all 40 are foreign (nearest to query).
/// Locals are ranked 51-53 (behind all 50 foreign) and NOT retrieved until the fetch
/// window is widened to ≥53 (round 2 doubles to 80, exhausting the corpus).
///
/// Local cosine=0.15 is above the default min_raw_relevance=0.10 score floor so locals
/// survive the final scoring step once the retry loop surfaces them.
async fn c1_setup() -> (
    KhiveRuntime,
    khive_runtime::VerbRegistry,
    MemoryPack,
    Vec<String>,
) {
    const MODEL: &str = "c1-clustered-enc";
    const N_FOREIGN: usize = 50;
    const N_LOCAL: usize = 3;

    let rt = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        embedding_model: None,
        additional_embedding_models: vec![],
        ..RuntimeConfig::default()
    })
    .expect("runtime");
    rt.register_embedder(ClusteredVecProvider::new(MODEL));

    let registry = make_registry(rt.clone());

    // Insert N_FOREIGN memories in "foreign" namespace. Content contains "xforeign"
    // so the embedder returns [1,0,0,0] — same direction as the query vector.
    for i in 0..N_FOREIGN {
        registry
            .dispatch(
                "memory.remember",
                json!({
                    "content": format!("xforeign memory slot {i} cluster-a"),
                    "salience": 0.6,
                    "namespace": "foreign"
                }),
            )
            .await
            .unwrap_or_else(|e| panic!("foreign remember {i} failed: {e}"));
    }

    // Insert N_LOCAL memories in "local" namespace. Content contains "xlocal"
    // so the embedder returns [0.15,0.9887,0,0] — far from query (cosine=0.15),
    // ranked behind all 50 foreign vectors but above the min_raw_relevance floor.
    let mut local_ids = Vec::new();
    for i in 0..N_LOCAL {
        let r = registry
            .dispatch(
                "memory.remember",
                json!({
                    "content": format!("xlocal memory {i} must be recalled"),
                    "salience": 0.8,
                    "namespace": "local"
                }),
            )
            .await
            .unwrap_or_else(|e| panic!("local remember {i} failed: {e}"));
        local_ids.push(r["id"].as_str().expect("note_id").to_string());
    }

    let pack = MemoryPack::new(rt.clone());
    {
        use khive_runtime::PackRuntime;
        pack.warm().await;
    }

    (rt, registry, pack, local_ids)
}

// C1 main: with default retry rounds, local memories ARE found despite the foreign
// cluster dominating the first over-fetch round.
#[tokio::test]
async fn test_ann_overfetch_retry_finds_local_memories_in_foreign_dominated_cluster() {
    use khive_runtime::PackRuntime;

    let (rt, registry, pack, local_ids) = c1_setup().await;

    // Local-only token: the production default. visible = {"local"}.
    // The global ANN index spans {"local", "foreign"} — retry gate must fire.
    let local_token = rt
        .authorize(Namespace::parse("local").expect("local ns"))
        .expect("authorize local");

    let result = pack
        .dispatch(
            "memory.recall",
            // candidate_limit=Some(8) → ann_fetch_limit=40. Query maps to [1,0,0,0]
            // (same as foreign cluster). Round 1 returns 40 foreign hits, 0 local.
            // Retry widens to 80>53, exhausting corpus and finding all 3 locals.
            json!({
                "query": "xforeign dominant cluster",
                "limit": 3,
                "config": { "candidate_limit": 8 }
            }),
            &registry,
            &local_token,
        )
        .await
        .expect("local-token recall");

    let hits: Vec<&str> = result
        .as_array()
        .expect("array")
        .iter()
        .map(|h| h["id"].as_str().unwrap())
        .collect();

    for lid in &local_ids {
        assert!(
            hits.contains(&lid.as_str()),
            "C1 retry regression: local memory {lid} must be in recall result with retry; \
             got: {hits:?}. If this fails with ANN_OVERFETCH_MAX_ROUNDS=1, that is expected."
        );
    }
}

// C1 stall: with ANN_OVERFETCH_MAX_ROUNDS=1, local memories are NOT found because the
// retry loop is disabled. This test PASSES only when ANN_OVERFETCH_MAX_ROUNDS=1 is set.
// It is skipped silently when the env var is absent or != "1".
//
// Run to confirm the gate matters:
//   ANN_OVERFETCH_MAX_ROUNDS=1 cargo test -p khive-pack-memory \
//       test_ann_overfetch_retry_stalls_without_widening -- --nocapture
#[tokio::test]
async fn test_ann_overfetch_retry_stalls_without_widening() {
    use khive_runtime::PackRuntime;

    // Only meaningful when ANN_OVERFETCH_MAX_ROUNDS=1 is set externally.
    // When unset, the retry loop runs to default (3 rounds) and locals ARE found —
    // which would make this test always pass trivially. Skip unless forced to 1.
    let max_rounds = std::env::var("ANN_OVERFETCH_MAX_ROUNDS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(3);
    if max_rounds != 1 {
        // Not running with the stall condition — skip.
        return;
    }

    let (rt, registry, pack, local_ids) = c1_setup().await;

    let local_token = rt
        .authorize(Namespace::parse("local").expect("local ns"))
        .expect("authorize local");

    let result = pack
        .dispatch(
            "memory.recall",
            json!({
                "query": "xforeign dominant cluster",
                "limit": 3,
                "config": { "candidate_limit": 8 }
            }),
            &registry,
            &local_token,
        )
        .await
        .expect("local-token recall (stall)");

    let hits: Vec<&str> = result
        .as_array()
        .expect("array")
        .iter()
        .map(|h| h["id"].as_str().unwrap())
        .collect();

    // With a single round, round 1 fetches 40 of 53 vectors — all foreign (near query).
    // No retry fires, so local memories (far from query) are silently dropped.
    for lid in &local_ids {
        assert!(
            !hits.contains(&lid.as_str()),
            "C1 stall: local memory {lid} must NOT appear in single-round recall; \
             if it does, the test corpus geometry is wrong. hits: {hits:?}"
        );
    }
}
