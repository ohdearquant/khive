use khive_runtime::{KhiveRuntime, Namespace, Resolved, RuntimeConfig};
use khive_storage::{entity::Entity, note::Note, types::DeleteMode, Edge, EdgeRelation, Event};
use khive_types::{EventKind, SubstrateKind};
use uuid::Uuid;

fn runtime() -> KhiveRuntime {
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        wal_ceiling_bytes: 0,
        wal_ceiling_configured_bytes: 0,
        wal_ceiling_source: Default::default(),
        wal_ceiling_env_raw: None,
        disk_guard_environment: Default::default(),
        disk_guard_config: None,
        volume_lock_dir: None,
        ..RuntimeConfig::no_embeddings()
    })
    .unwrap();
    assert!(runtime.backend().pool().canonical_path().is_none());
    assert!(runtime.backend_data_dir().is_none());
    assert!(runtime.backend_ann_root().is_none());
    assert!(runtime.registered_embedding_model_names().is_empty());
    runtime
}

#[tokio::test]
async fn entity_and_note_reads_keep_namespace_and_tombstone_policies() {
    let runtime = runtime();
    let local = Namespace::local();
    let other = Namespace::parse("identity-other").unwrap();
    let caller = runtime
        .authorize_with_visibility(local.clone(), vec![other.clone()])
        .unwrap();
    for namespace in [local, other] {
        let owner = runtime.authorize(namespace.clone()).unwrap();
        for deleted in [false, true] {
            for is_note in [false, true] {
                let id = Uuid::new_v4();
                if is_note {
                    let mut note = Note::new(namespace.as_str(), "observation", "fixture");
                    note.id = id;
                    note.deleted_at = deleted.then_some(1);
                    runtime
                        .notes(&owner)
                        .unwrap()
                        .upsert_note(note)
                        .await
                        .unwrap();
                } else {
                    let mut entity = Entity::new(namespace.as_str(), "concept", "fixture");
                    entity.id = id;
                    entity.deleted_at = deleted.then_some(1);
                    runtime
                        .entities(&owner)
                        .unwrap()
                        .upsert_entity(entity)
                        .await
                        .unwrap();
                }
                let live = runtime.resolve_by_id(&caller, id).await.unwrap();
                assert_eq!(live.is_some(), !deleted);
                let any = runtime
                    .resolve_by_id_including_deleted(&caller, id)
                    .await
                    .unwrap();
                assert_eq!(matches!(&any, Some(Resolved::Note(_))), is_note);
                assert_eq!(matches!(&any, Some(Resolved::Entity(_))), !is_note);
                let in_primary = &namespace == caller.namespace();
                assert_eq!(
                    runtime
                        .resolve_primary(&caller, id)
                        .await
                        .unwrap()
                        .is_some(),
                    in_primary && !deleted
                );
                assert_eq!(
                    runtime
                        .resolve_including_deleted(&caller, id)
                        .await
                        .unwrap()
                        .is_some(),
                    in_primary
                );
                assert_eq!(
                    runtime
                        .resolve_edge_endpoint(&caller, id)
                        .await
                        .unwrap()
                        .is_some(),
                    !deleted
                );
            }
        }
    }
}

#[tokio::test]
async fn event_probes_and_entity_note_precedence_remain_distinct() {
    let runtime = runtime();
    let local = runtime.authorize(Namespace::local()).unwrap();
    let foreign = runtime
        .authorize(Namespace::parse("identity-other").unwrap())
        .unwrap();
    for owner in [&local, &foreign] {
        let event = Event::new(
            owner.namespace().as_str(),
            "create",
            EventKind::EntityCreated,
            SubstrateKind::Entity,
            "identity-fixture",
        );
        let id = event.id;
        runtime
            .events(owner)
            .unwrap()
            .append_event(event)
            .await
            .unwrap();
        assert!(runtime.resolve_by_id(&local, id).await.unwrap().is_none());
        assert!(runtime
            .resolve_by_id_including_deleted(&local, id)
            .await
            .unwrap()
            .is_none());
        // By-ID edge endpoint validation resolves events regardless of their namespace.
        assert!(matches!(
            runtime.resolve_edge_endpoint(&local, id).await.unwrap(),
            Some(Resolved::Event(_))
        ));
        assert_eq!(
            runtime.resolve_primary(&local, id).await.unwrap().is_some(),
            owner.namespace() == local.namespace()
        );
        assert_eq!(
            runtime
                .resolve_including_deleted(&local, id)
                .await
                .unwrap()
                .is_some(),
            owner.namespace() == local.namespace()
        );
    }

    // Deliberately share an ID across substrates to observe lookup order and
    // primary-namespace fallthrough independently of normal UUID uniqueness.
    let mut entity = Entity::new(foreign.namespace().as_str(), "concept", "foreign first");
    let id = entity.id;
    let mut note = Note::new(local.namespace().as_str(), "observation", "local second");
    note.id = id;
    runtime
        .entities(&foreign)
        .unwrap()
        .upsert_entity(entity.clone())
        .await
        .unwrap();
    runtime
        .notes(&local)
        .unwrap()
        .upsert_note(note)
        .await
        .unwrap();
    assert!(matches!(
        runtime.resolve_by_id(&local, id).await.unwrap(),
        Some(Resolved::Entity(_))
    ));
    assert!(matches!(
        runtime.resolve_edge_endpoint(&local, id).await.unwrap(),
        Some(Resolved::Entity(_))
    ));
    assert!(matches!(
        runtime.resolve_primary(&local, id).await.unwrap(),
        Some(Resolved::Note(_))
    ));
    entity.deleted_at = Some(1);
    runtime
        .entities(&foreign)
        .unwrap()
        .upsert_entity(entity)
        .await
        .unwrap();
    assert!(matches!(
        runtime.resolve_by_id(&local, id).await.unwrap(),
        Some(Resolved::Note(_))
    ));
    assert!(matches!(
        runtime
            .resolve_by_id_including_deleted(&local, id)
            .await
            .unwrap(),
        Some(Resolved::Entity(_))
    ));
}

#[tokio::test]
async fn annotates_link_accepts_foreign_event_by_id() {
    let runtime = runtime();
    let local = runtime.authorize(Namespace::local()).unwrap();
    let foreign = runtime
        .authorize(Namespace::parse("identity-other").unwrap())
        .unwrap();
    let note = Note::new(
        local.namespace().as_str(),
        "observation",
        "event annotation",
    );
    let source_id = note.id;
    runtime
        .notes(&local)
        .unwrap()
        .upsert_note(note)
        .await
        .unwrap();
    let event = Event::new(
        foreign.namespace().as_str(),
        "create",
        EventKind::EntityCreated,
        SubstrateKind::Entity,
        "identity-fixture",
    );
    let target_id = event.id;
    runtime
        .events(&foreign)
        .unwrap()
        .append_event(event)
        .await
        .unwrap();

    let edge = runtime
        .link(
            &local,
            source_id,
            target_id,
            EdgeRelation::Annotates,
            1.0,
            None,
        )
        .await
        .expect("annotates accepts a foreign event by ID");
    assert_eq!(edge.namespace, local.namespace().as_str());
    assert_eq!(edge.source_id, source_id);
    assert_eq!(edge.target_id, target_id);
    assert_eq!(edge.relation, EdgeRelation::Annotates);
    let stored = runtime
        .get_edge(&local, edge.id.into())
        .await
        .unwrap()
        .expect("the annotation edge is stored");
    assert_eq!(stored.namespace, local.namespace().as_str());
    assert_eq!(stored.source_id, source_id);
    assert_eq!(stored.target_id, target_id);
    assert_eq!(stored.relation, EdgeRelation::Annotates);
}

#[tokio::test]
async fn edge_reads_use_stored_namespace_and_the_requested_liveness() {
    let runtime = runtime();
    let caller = runtime.authorize(Namespace::local()).unwrap();
    let owner = runtime
        .authorize(Namespace::parse("identity-other").unwrap())
        .unwrap();
    let id = Uuid::new_v4();
    let now = chrono::Utc::now();
    let edge = Edge {
        id: id.into(),
        namespace: owner.namespace().as_str().into(),
        source_id: Uuid::new_v4(),
        target_id: Uuid::new_v4(),
        relation: EdgeRelation::Supports,
        weight: 1.0,
        created_at: now,
        updated_at: now,
        deleted_at: None,
        metadata: None,
        target_backend: None,
    };
    runtime
        .graph(&owner)
        .unwrap()
        .upsert_edge(edge)
        .await
        .unwrap();
    assert_eq!(
        runtime
            .get_edge(&caller, id)
            .await
            .unwrap()
            .unwrap()
            .namespace,
        owner.namespace().as_str()
    );
    // By-ID edge reads are namespace-agnostic, including the visible variant.
    assert_eq!(
        runtime
            .get_edge_visible(&caller, id)
            .await
            .unwrap()
            .unwrap()
            .namespace,
        owner.namespace().as_str()
    );
    assert!(runtime
        .get_edge_including_deleted(&caller, id)
        .await
        .unwrap()
        .is_some());
    runtime
        .graph(&owner)
        .unwrap()
        .delete_edge(id.into(), DeleteMode::Soft)
        .await
        .unwrap();
    assert!(runtime.get_edge(&caller, id).await.unwrap().is_none());
    assert!(runtime
        .get_edge_visible(&caller, id)
        .await
        .unwrap()
        .is_none());
    assert!(runtime
        .get_edge_including_deleted(&caller, id)
        .await
        .unwrap()
        .unwrap()
        .deleted_at
        .is_some());
    let absent = Uuid::new_v4();
    assert!(runtime.get_edge(&caller, absent).await.unwrap().is_none());
    assert!(runtime
        .get_edge_including_deleted(&caller, absent)
        .await
        .unwrap()
        .is_none());
}
