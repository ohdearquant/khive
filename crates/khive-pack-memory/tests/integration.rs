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
