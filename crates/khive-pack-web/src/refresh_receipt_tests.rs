use super::latest_receipt;
use crate::receipt::{RECEIPT_PROVENANCE_KEY, RECEIPT_PROVENANCE_VALUE, RECEIPT_TAG};
use khive_runtime::{BackendId, KhiveRuntime, NamespaceToken, RuntimeConfig};
use khive_storage::{Direction, Edge, EdgeRelation, Entity, NeighborHit, NeighborQuery, Note};
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
    let mut note =
        Note::new(namespace, "observation", "Fetched a document.").with_properties(json!({
            "tags": [RECEIPT_TAG],
            "khive:web_receipt": RECEIPT_PROVENANCE_VALUE,
        }));
    note.id = id;
    note.created_at = created;
    note.updated_at = created;
    runtime
        .backend()
        .notes()
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

#[tokio::test]
async fn generic_receipt_shape_is_not_refresh_or_extract_provenance() {
    let runtime = runtime(
        Arc::new(khive_db::StorageBackend::memory().unwrap()),
        BackendId::main(),
    );
    let token = runtime.authorize(Namespace::local()).unwrap();
    let entity_id = Uuid::from_u128(9);
    target(&runtime, &token, entity_id).await;
    let body_ref = "f".repeat(64);
    let forged = runtime
        .create_note(
            &token,
            "observation",
            None,
            "caller-written receipt shape",
            None,
            Some(json!({
                "tags": [RECEIPT_TAG],
                "request": {
                    "verb": "web.fetch",
                    "content_ref": body_ref,
                    "body_entity_id": entity_id.to_string(),
                    "headers": {"link": ["<https://fake.example/link>; rel=next"]},
                },
            })),
            vec![entity_id],
        )
        .await
        .unwrap();
    assert_eq!(
        latest_receipt(&runtime, &token, entity_id).await.unwrap(),
        None
    );
    let entity = runtime
        .entities(&token)
        .unwrap()
        .get_entity(entity_id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        crate::receipt::capture_for_body(&runtime, &token, &entity, &body_ref)
            .await
            .unwrap()
            .is_none()
    );

    let trusted = crate::receipt::write_receipt(
        &runtime,
        &token,
        "web.fetch",
        json!({
            "verb": "web.fetch",
            "content_ref": body_ref,
            "body_entity_id": entity_id.to_string(),
        }),
        vec![entity_id],
    )
    .await
    .unwrap();
    assert_ne!(trusted, forged.id);
    let trusted_note = runtime
        .notes(&token)
        .unwrap()
        .get_note(trusted)
        .await
        .unwrap()
        .unwrap();
    let decoy = runtime
        .create_note(
            &token,
            "observation",
            None,
            "newer caller-written receipt shape",
            None,
            Some(json!({"tags": [RECEIPT_TAG]})),
            vec![entity_id],
        )
        .await
        .unwrap();
    let mut newer_decoy = decoy.clone();
    newer_decoy.created_at = trusted_note.created_at + 1;
    newer_decoy.updated_at = newer_decoy.created_at;
    runtime
        .backend()
        .notes()
        .unwrap()
        .upsert_note(newer_decoy)
        .await
        .unwrap();
    assert_eq!(
        runtime
            .latest_annotating_note(&token, entity_id, "observation", RECEIPT_TAG)
            .await
            .unwrap(),
        Some(decoy.id)
    );
    assert_eq!(
        latest_receipt(&runtime, &token, entity_id).await.unwrap(),
        Some(trusted)
    );
    assert_eq!(
        crate::receipt::capture_for_body(&runtime, &token, &entity, &body_ref)
            .await
            .unwrap()
            .map(|(id, _)| id),
        Some(trusted)
    );
}

async fn edge(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    source: Uuid,
    target: Uuid,
    relation: EdgeRelation,
    weight: f64,
) {
    let now = chrono::Utc::now();
    runtime
        .graph(token)
        .unwrap()
        .upsert_edge(Edge {
            id: Uuid::new_v4().into(),
            namespace: token.namespace().as_str().to_string(),
            source_id: source,
            target_id: target,
            relation,
            weight,
            created_at: now,
            updated_at: now,
            deleted_at: None,
            metadata: None,
            target_backend: None,
        })
        .await
        .unwrap();
}

fn out_query(relation: EdgeRelation, limit: Option<u32>) -> NeighborQuery {
    NeighborQuery {
        direction: Direction::Out,
        relations: Some(vec![relation]),
        limit,
        min_weight: None,
    }
}

/// The receipt walk reads only the node id of each neighbour, so it must not
/// pay for the name and kind lookups that `KhiveRuntime::neighbors` adds. The
/// reference below spells the same walk with the un-enriched primitives; the
/// walk may read no more than that.
#[tokio::test]
async fn capture_for_body_walk_reads_no_neighbor_enrichment() {
    let runtime = runtime(
        Arc::new(khive_db::StorageBackend::memory().unwrap()),
        BackendId::main(),
    );
    let token = runtime.authorize(Namespace::local()).unwrap();
    let entity_id = Uuid::from_u128(1);
    target(&runtime, &token, entity_id).await;
    // Six receipts, each superseding the one before it. None of them carries
    // the requested body, so the walk visits the whole chain.
    let chain: Vec<Uuid> = (0..6).map(|i| Uuid::from_u128(100 + i)).collect();
    for (i, id) in chain.iter().enumerate() {
        receipt(&runtime, "local", entity_id, *id, 10 * (i as i64 + 1)).await;
    }
    for pair in chain.windows(2) {
        edge(
            &runtime,
            &token,
            pair[1],
            pair[0],
            EdgeRelation::Supersedes,
            1.0,
        )
        .await;
    }
    let body_ref = "f".repeat(64);
    let entity = runtime
        .entities(&token)
        .unwrap()
        .get_entity(entity_id)
        .await
        .unwrap()
        .unwrap();
    let pool = runtime.backend().pool();

    // One walk first, so a cost the store pays only once is charged to neither
    // the measured walk nor the reference below.
    crate::receipt::capture_for_body(&runtime, &token, &entity, &body_ref)
        .await
        .unwrap();
    let before = pool.reader_acquisition_snapshot().acquisitions;
    let found = crate::receipt::capture_for_body(&runtime, &token, &entity, &body_ref)
        .await
        .unwrap();
    let walk_reads = pool.reader_acquisition_snapshot().acquisitions - before;
    assert!(found.is_none());

    let before = pool.reader_acquisition_snapshot().acquisitions;
    let mut cursor = runtime
        .latest_annotating_note_with_property(
            &token,
            entity_id,
            "observation",
            RECEIPT_TAG,
            RECEIPT_PROVENANCE_KEY,
            RECEIPT_PROVENANCE_VALUE,
        )
        .await
        .unwrap();
    let notes = runtime.notes(&token).unwrap();
    let mut visited = 0;
    while let Some(id) = cursor {
        visited += 1;
        notes.get_note(id).await.unwrap();
        cursor = runtime
            .neighbors_with_query_page(
                &token,
                id,
                out_query(EdgeRelation::Supersedes, Some(1)),
                None,
                None,
                false,
            )
            .await
            .unwrap()
            .first()
            .map(|hit| hit.node_id);
    }
    let plain_reads = pool.reader_acquisition_snapshot().acquisitions - before;
    assert_eq!(visited, chain.len(), "every receipt is visited");

    // Control: on this fixture the enriched lookup really does cost more, so
    // an equal total cannot come from enrichment being free here.
    let before = pool.reader_acquisition_snapshot().acquisitions;
    runtime
        .neighbors(
            &token,
            chain[5],
            Direction::Out,
            Some(1),
            Some(vec![EdgeRelation::Supersedes]),
        )
        .await
        .unwrap();
    let enriched_step = pool.reader_acquisition_snapshot().acquisitions - before;
    let before = pool.reader_acquisition_snapshot().acquisitions;
    runtime
        .neighbors_with_query_page(
            &token,
            chain[5],
            out_query(EdgeRelation::Supersedes, Some(1)),
            None,
            None,
            false,
        )
        .await
        .unwrap();
    let plain_step = pool.reader_acquisition_snapshot().acquisitions - before;
    assert!(
        enriched_step > plain_step,
        "the fixture must show the enrichment reads this test guards against"
    );

    assert_eq!(
        walk_reads, plain_reads,
        "the receipt walk must read only what the un-enriched primitives read"
    );
}

/// Skipping enrichment must not change which neighbours come back or in what
/// order: the soft-deleted filter and the weight ordering run either way.
#[tokio::test]
async fn unenriched_neighbors_keep_the_ids_and_order_of_enriched_neighbors() {
    let runtime = runtime(
        Arc::new(khive_db::StorageBackend::memory().unwrap()),
        BackendId::main(),
    );
    let token = runtime.authorize(Namespace::local()).unwrap();
    let entity_id = Uuid::from_u128(1);
    let deleted_id = Uuid::from_u128(2);
    let note_id = Uuid::from_u128(3);
    let anchor = Uuid::from_u128(4);
    target(&runtime, &token, entity_id).await;
    target(&runtime, &token, deleted_id).await;
    // `receipt` stores a note that annotates `entity_id` at weight 1.0.
    receipt(&runtime, "local", entity_id, note_id, 10).await;
    receipt(&runtime, "local", entity_id, anchor, 20).await;
    edge(
        &runtime,
        &token,
        anchor,
        deleted_id,
        EdgeRelation::Annotates,
        0.9,
    )
    .await;
    edge(
        &runtime,
        &token,
        anchor,
        note_id,
        EdgeRelation::Annotates,
        0.5,
    )
    .await;
    runtime
        .entities(&token)
        .unwrap()
        .delete_entity(deleted_id, khive_storage::DeleteMode::Soft)
        .await
        .unwrap();

    let enriched = runtime
        .neighbors(
            &token,
            anchor,
            Direction::Out,
            None,
            Some(vec![EdgeRelation::Annotates]),
        )
        .await
        .unwrap();
    let plain = runtime
        .neighbors_with_query_page(
            &token,
            anchor,
            out_query(EdgeRelation::Annotates, None),
            None,
            None,
            false,
        )
        .await
        .unwrap();
    let ids = |hits: &[NeighborHit]| hits.iter().map(|hit| hit.node_id).collect::<Vec<_>>();
    assert_eq!(ids(&enriched), vec![entity_id, note_id]);
    assert_eq!(ids(&plain), ids(&enriched));
    assert!(enriched.iter().all(|hit| hit.name.is_some()));
    assert!(plain.iter().all(|hit| hit.name.is_none()));
}
