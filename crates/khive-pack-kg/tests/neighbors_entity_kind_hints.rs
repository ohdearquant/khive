use chrono::Utc;
use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig};
use khive_storage::types::{Direction, LinkId, NeighborQuery};
use khive_storage::{Edge, EdgeRelation, Entity, Note};
use uuid::Uuid;

#[tokio::test]
async fn entity_kind_hints_preserve_lightweight_hits_and_visible_scope() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = KhiveRuntime::new_for_test(RuntimeConfig {
        db_path: Some(dir.path().join("neighbor-kind-hints.db")),
        ..RuntimeConfig::no_embeddings()
    })
    .unwrap();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let anchor = Entity::new("local", "concept", "Neighbor kind anchor");
    let visible = Entity::new("local", "concept", "Visible neighbor");
    let foreign = Entity::new("foreign", "concept", "Foreign neighbor");
    let mut deleted = Entity::new("local", "concept", "Deleted neighbor");
    deleted.deleted_at = Some(Utc::now().timestamp_micros());
    runtime
        .entities(&token)
        .unwrap()
        .upsert_entities(vec![
            anchor.clone(),
            visible.clone(),
            foreign.clone(),
            deleted.clone(),
        ])
        .await
        .unwrap();
    let note = Note::new("local", "observation", "Ordinary note neighbor");
    runtime
        .notes(&token)
        .unwrap()
        .upsert_note(note.clone())
        .await
        .unwrap();
    let now = Utc::now();
    let edges = [visible.id, foreign.id, deleted.id, note.id]
        .into_iter()
        .map(|target_id| Edge {
            id: LinkId::from(Uuid::new_v4()),
            namespace: "local".into(),
            source_id: anchor.id,
            target_id,
            relation: EdgeRelation::Supports,
            weight: 1.0,
            created_at: now,
            updated_at: now,
            deleted_at: None,
            metadata: None,
            target_backend: None,
        })
        .collect();
    runtime
        .graph(&token)
        .unwrap()
        .upsert_edges(edges)
        .await
        .unwrap();
    assert!(runtime
        .resolve_by_id(&token, anchor.id)
        .await
        .unwrap()
        .is_some());
    let options = || khive_runtime::KgNeighborRead {
        query: NeighborQuery {
            direction: Direction::Out,
            relations: None,
            limit: Some(10),
            min_weight: None,
        },
        after: None,
        neighbor_kinds: None,
        enrich: false,
        namespace: None,
    };
    let before = runtime
        .backend()
        .pool()
        .reader_acquisition_snapshot()
        .pooled_checkouts;
    let old_hits = runtime
        .neighbors_for_resolved_kg_read(&token, anchor.id, options())
        .await
        .unwrap();
    let old_reads = runtime
        .backend()
        .pool()
        .reader_acquisition_snapshot()
        .pooled_checkouts
        - before;
    let before = runtime
        .backend()
        .pool()
        .reader_acquisition_snapshot()
        .pooled_checkouts;
    let (hits, kinds) = runtime
        .neighbors_for_resolved_kg_read_with_entity_kinds(&token, anchor.id, options())
        .await
        .unwrap();
    let new_reads = runtime
        .backend()
        .pool()
        .reader_acquisition_snapshot()
        .pooled_checkouts
        - before;
    assert_eq!(
        serde_json::to_value(&hits).unwrap(),
        serde_json::to_value(&old_hits).unwrap()
    );
    assert_eq!(
        new_reads, old_reads,
        "kind hints must share an existing read"
    );
    assert!(hits
        .iter()
        .all(|hit| hit.kind.is_none() && hit.name.is_none()));
    assert!(hits.iter().any(|hit| hit.node_id == visible.id));
    assert!(hits.iter().any(|hit| hit.node_id == foreign.id));
    assert!(hits.iter().any(|hit| hit.node_id == note.id));
    assert!(hits.iter().all(|hit| hit.node_id != deleted.id));
    assert_eq!(kinds.len(), 1);
    assert_eq!(kinds.get(&visible.id).map(String::as_str), Some("concept"));
    assert!(!kinds.contains_key(&foreign.id));
    assert!(!kinds.contains_key(&note.id));
    assert!(!kinds.contains_key(&deleted.id));
}
