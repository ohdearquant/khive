use khive_pack_kg::KgPack;
use khive_pack_memory::MemoryPack;
use khive_runtime::{KhiveRuntime, RuntimeConfig, VerbRegistryBuilder};
use khive_types::Pack;
use serde_json::json;

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
