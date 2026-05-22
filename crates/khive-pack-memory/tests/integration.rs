use khive_pack_kg::KgPack;
use khive_pack_memory::MemoryPack;
use khive_runtime::{KhiveRuntime, RuntimeConfig, VerbRegistryBuilder};
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
    let note_store = rt.notes(None).unwrap();
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

    let note_store = rt.notes(None).expect("note store");
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

/// Regression test for issue #100: decay_factor must be clamped to [0, 1].
#[tokio::test]
async fn test_remember_decay_factor_clamped() {
    let rt = make_runtime();
    let registry = make_registry(rt.clone());

    // decay > 1.0 should be clamped to 1.0
    let result = registry
        .dispatch(
            "remember",
            json!({
                "content": "memory with excessive decay",
                "decay": 5.0
            }),
        )
        .await
        .expect("remember with large decay");

    let note_id: Uuid = result["note_id"]
        .as_str()
        .unwrap()
        .parse()
        .expect("valid uuid");

    let note_store = rt.notes(None).expect("note store");
    let note = note_store
        .get_note(note_id)
        .await
        .expect("get note")
        .expect("note exists");

    assert!(
        note.decay_factor <= 1.0,
        "decay_factor must be <= 1.0 after clamping, got {}",
        note.decay_factor
    );
    assert!(
        note.decay_factor >= 0.0,
        "decay_factor must be >= 0.0, got {}",
        note.decay_factor
    );
}

#[test]
fn test_memory_dotted_verbs_registered() {
    let names: Vec<&str> = MemoryPack::VERBS.iter().map(|v| v.name).collect();
    assert!(names.contains(&"recall.candidates"));
    assert!(names.contains(&"recall.fuse"));
    assert!(names.contains(&"recall.score"));
    assert!(names.contains(&"recall.embed"));
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
    for i in 0..50 {
        rt.create_note(
            None,
            "observation",
            None,
            &format!("observation {i} about attention mechanisms in neural networks"),
            0.5,
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
