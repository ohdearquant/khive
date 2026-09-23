use super::latest_receipt;
use crate::receipt::RECEIPT_TAG;
use khive_runtime::{BackendId, KhiveRuntime, NamespaceToken, RuntimeConfig};
use khive_storage::{Edge, EdgeRelation, Entity, Note};
use khive_types::Namespace;
use serde_json::json;
use std::sync::Arc;
use uuid::Uuid;

fn runtime(backend: Arc<khive_db::StorageBackend>, backend_id: BackendId) -> KhiveRuntime {
    backend.prepare_core_schema().expect("prepare test backend");
    KhiveRuntime::from_backend(
        backend,
        RuntimeConfig {
            db_path: None,
            events_split: None,
            actor_id: Some("test:web-receipt".to_string()),
            packs: vec!["kg".to_string(), "web".to_string()],
            visible_namespaces: vec![],
            backend_id,
            ..RuntimeConfig::no_embeddings()
        },
    )
}

async fn target(runtime: &KhiveRuntime, token: &NamespaceToken, id: Uuid) {
    let mut entity = Entity::new(token.namespace().as_str(), "document", "Receipt target");
    entity.id = id;
    runtime
        .entities(token)
        .unwrap()
        .upsert_entity(entity)
        .await
        .unwrap();
}

async fn receipt(runtime: &KhiveRuntime, namespace: &str, target: Uuid, id: Uuid, created: i64) {
    let token = runtime
        .authorize(Namespace::parse(namespace).unwrap())
        .unwrap();
    let mut note = Note::new(namespace, "observation", "Fetched a document.")
        .with_properties(json!({"tags": [RECEIPT_TAG]}));
    note.id = id;
    note.created_at = created;
    note.updated_at = created;
    runtime
        .notes(&token)
        .unwrap()
        .upsert_note(note)
        .await
        .unwrap();
    let now = chrono::Utc::now();
    runtime
        .graph(&token)
        .unwrap()
        .upsert_edge(Edge {
            id: Uuid::new_v4().into(),
            namespace: namespace.to_string(),
            source_id: id,
            target_id: target,
            relation: EdgeRelation::Annotates,
            weight: 1.0,
            created_at: now,
            updated_at: now,
            deleted_at: None,
            metadata: None,
            target_backend: None,
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn latest_receipt_reduces_visible_namespaces_on_the_bound_backend() {
    let main_backend = Arc::new(khive_db::StorageBackend::memory().unwrap());
    let main = runtime(main_backend.clone(), BackendId::main());
    let secondary = runtime(
        Arc::new(khive_db::StorageBackend::memory().unwrap()),
        BackendId::parse("web-secondary").unwrap(),
    )
    .with_core_backend(main_backend);
    let token = secondary.authorize(Namespace::local()).unwrap();
    let main_token = main.authorize(Namespace::local()).unwrap();
    let entity_id = Uuid::from_u128(1);
    // Same UUID on distinct backends deliberately makes an accidental core()/SQL
    // route observable: the main receipt is newer than every secondary receipt.
    target(&main, &main_token, entity_id).await;
    target(&secondary, &token, entity_id).await;
    let main_receipt = Uuid::from_u128(10);
    let local_receipt = Uuid::from_u128(11);
    let shared_receipt = Uuid::from_u128(12);
    let hidden_receipt = Uuid::from_u128(13);
    receipt(&main, "local", entity_id, main_receipt, 1_000).await;
    receipt(&secondary, "local", entity_id, local_receipt, 10).await;
    receipt(&secondary, "shared", entity_id, shared_receipt, 20).await;
    receipt(&secondary, "hidden", entity_id, hidden_receipt, 30).await;

    assert_eq!(
        latest_receipt(&secondary, &token, entity_id).await.unwrap(),
        Some(local_receipt)
    );
    let shared = secondary
        .authorize_with_visibility(
            Namespace::local(),
            vec![Namespace::parse("shared").unwrap()],
        )
        .unwrap();
    assert_eq!(
        latest_receipt(&secondary, &shared, entity_id)
            .await
            .unwrap(),
        Some(shared_receipt)
    );
    assert_eq!(
        latest_receipt(&main, &main_token, entity_id).await.unwrap(),
        Some(main_receipt)
    );

    // The same lookup still obeys the runtime's endpoint liveness check.
    secondary
        .entities(&token)
        .unwrap()
        .delete_entity(entity_id, khive_storage::DeleteMode::Soft)
        .await
        .unwrap();
    assert_eq!(
        latest_receipt(&secondary, &shared, entity_id)
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn latest_receipt_equal_timestamps_choose_same_note_across_namespace_order() {
    let runtime = runtime(
        Arc::new(khive_db::StorageBackend::memory().unwrap()),
        BackendId::main(),
    );
    let token = runtime.authorize(Namespace::local()).unwrap();
    let entity_id = Uuid::from_u128(1);
    target(&runtime, &token, entity_id).await;
    let smaller = Uuid::from_u128(2);
    receipt(&runtime, "shared-a", entity_id, Uuid::from_u128(3), 100).await;
    receipt(&runtime, "shared-b", entity_id, smaller, 100).await;
    for namespaces in [["shared-a", "shared-b"], ["shared-b", "shared-a"]] {
        let token = runtime
            .authorize_with_visibility(
                Namespace::local(),
                namespaces
                    .into_iter()
                    .map(|ns| Namespace::parse(ns).unwrap())
                    .collect(),
            )
            .unwrap();
        assert_eq!(
            latest_receipt(&runtime, &token, entity_id).await.unwrap(),
            Some(smaller)
        );
    }
}

#[tokio::test]
async fn latest_receipt_store_reads_stay_bounded_as_history_grows() {
    let runtime = runtime(
        Arc::new(khive_db::StorageBackend::memory().unwrap()),
        BackendId::main(),
    );
    let token = runtime.authorize(Namespace::local()).unwrap();
    let entity_id = Uuid::from_u128(1);
    let newest = Uuid::from_u128(2);
    target(&runtime, &token, entity_id).await;
    receipt(&runtime, "local", entity_id, newest, 10_000).await;
    let pool = runtime.backend().pool();
    let before = pool.reader_acquisition_snapshot().acquisitions;
    assert_eq!(
        latest_receipt(&runtime, &token, entity_id).await.unwrap(),
        Some(newest)
    );
    let small_reads = pool.reader_acquisition_snapshot().acquisitions - before;
    assert!(
        small_reads > 0,
        "the measurement must observe real store reads"
    );

    for i in 0..512 {
        receipt(
            &runtime,
            "local",
            entity_id,
            Uuid::from_u128(100 + i),
            i as i64,
        )
        .await;
    }
    let before = pool.reader_acquisition_snapshot().acquisitions;
    assert_eq!(
        latest_receipt(&runtime, &token, entity_id).await.unwrap(),
        Some(newest)
    );
    let large_reads = pool.reader_acquisition_snapshot().acquisitions - before;
    assert_eq!(
        large_reads, small_reads,
        "refresh must not hydrate each historical receipt with a separate note read"
    );
}
