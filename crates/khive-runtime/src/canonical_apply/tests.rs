use super::*;
use khive_changeset::{EntityPatch, EntityPreimage, Envelope, UpdatePreimage};
use khive_types::{Namespace, Timestamp};

struct Fixture {
    runtime: KhiveRuntime,
    _dir: tempfile::TempDir,
}
fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let runtime = KhiveRuntime::new_for_test(crate::RuntimeConfig {
        db_path: Some(dir.path().join("canonical.db")),
        embedding_model: None,
        additional_embedding_models: Vec::new(),
        ..Default::default()
    })
    .unwrap();
    Fixture { runtime, _dir: dir }
}
fn changeset(ops: Vec<Op>) -> ChangeSet {
    ChangeSet::new(
        Envelope::new("agent:test", "test", Timestamp::from_secs(1)),
        ops,
    )
}
fn entity_create(id: Uuid, name: &str) -> Op {
    Op::Create(khive_changeset::CreateOp {
        id: id128(id),
        namespace: Namespace::local(),
        target: CreateTarget::Entity(khive_changeset::EntityCreateFields {
            entity_kind: khive_types::EntityKind::Concept,
            entity_type: None,
            name: name.into(),
            description: None,
            properties: Default::default(),
            tags: Vec::new(),
        }),
    })
}
fn rename(id: Uuid, old: &str, new: &str) -> Op {
    Op::Update(
        UpdateOp::new(
            id128(id),
            UpdatePatch::Entity(EntityPatch {
                name: Some(new.into()),
                description: None,
                entity_type: None,
                properties: None,
                tags: None,
            }),
            UpdatePreimage::Entity(EntityPreimage {
                name: Some(old.into()),
                description: None,
                entity_type: None,
                properties: None,
                tags: None,
            }),
        )
        .unwrap(),
    )
}
fn registry() -> VerbRegistry {
    crate::VerbRegistryBuilder::new().build().unwrap()
}

#[tokio::test]
async fn ordered_creates_link_and_updates_keep_staged_ids() {
    let f = fixture();
    let rt = &f.runtime;
    let token = rt.authorize(Namespace::local()).unwrap();
    let (a, b, e) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let cs = changeset(vec![
        entity_create(a, "a"),
        entity_create(b, "b"),
        rename(a, "a", "a2"),
        rename(a, "a2", "a3"),
        Op::Link(khive_changeset::LinkOp {
            id: id128(e),
            namespace: Namespace::local(),
            source: id128(a),
            target: id128(b),
            relation: khive_types::EdgeRelation::CompetesWith,
            weight: 0.75,
            properties: Default::default(),
        }),
    ]);
    let plans = prepare_canonical_changeset(rt, &registry(), &token, &cs)
        .await
        .unwrap();
    assert!(matches!(
        crate::run_atomic_unit(rt.sql().as_ref(), plans)
            .await
            .unwrap(),
        crate::AtomicRunOutcome::Committed { .. }
    ));
    let row = rt
        .entities(&token)
        .unwrap()
        .get_entity(a)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.name, "a3");
    let edge = rt.get_edge(&token, e).await.unwrap().unwrap();
    assert_eq!(Uuid::from(edge.id), e);
    assert_eq!(edge.weight, 0.75);
}

#[tokio::test]
async fn unchanged_entity_update_is_a_no_op_and_replays_without_a_new_revision() {
    let f = fixture();
    let rt = &f.runtime;
    let token = rt.authorize(Namespace::local()).unwrap();
    let a = Uuid::new_v4();
    let updated = || khive_storage::EventFilter {
        target_id: Some(a),
        kinds: vec![khive_types::EventKind::EntityUpdated],
        ..Default::default()
    };
    let version = |rt: &KhiveRuntime, token: &NamespaceToken| {
        let entities = rt.entities(token).unwrap();
        async move { entities.get_entity(a).await.unwrap().unwrap().version }
    };
    let create = changeset(vec![entity_create(a, "same")]);
    let plans = prepare_canonical_changeset(rt, &registry(), &token, &create)
        .await
        .unwrap();
    crate::run_atomic_unit(rt.sql().as_ref(), plans)
        .await
        .unwrap();
    let created = version(rt, &token).await;
    for cs in [
        changeset(vec![rename(a, "same", "same")]),
        changeset(vec![rename(a, "same", "same")]),
    ] {
        let plans = prepare_canonical_changeset(rt, &registry(), &token, &cs)
            .await
            .unwrap();
        assert!(matches!(
            crate::run_atomic_unit(rt.sql().as_ref(), plans)
                .await
                .unwrap(),
            crate::AtomicRunOutcome::Committed { .. }
        ));
    }
    assert_eq!(version(rt, &token).await, created);
    let events = rt.events(&token).unwrap();
    assert_eq!(events.count_events(updated()).await.unwrap(), 0);

    // A real change still writes one revision and one event.
    let cs = changeset(vec![rename(a, "same", "changed")]);
    let plans = prepare_canonical_changeset(rt, &registry(), &token, &cs)
        .await
        .unwrap();
    crate::run_atomic_unit(rt.sql().as_ref(), plans)
        .await
        .unwrap();
    assert_eq!(version(rt, &token).await, created + 1);
    assert_eq!(events.count_events(updated()).await.unwrap(), 1);
}

#[tokio::test]
async fn unhooked_writer_drift_survives_but_touched_drift_rolls_back_prior_ops() {
    let f = fixture();
    let rt = &f.runtime;
    let token = rt.authorize(Namespace::local()).unwrap();
    let mut row = Entity::new("local", "concept", "old");
    let id = row.id;
    rt.entities(&token)
        .unwrap()
        .upsert_entity(row.clone())
        .await
        .unwrap();
    let plans = prepare_canonical_changeset(
        rt,
        &registry(),
        &token,
        &changeset(vec![rename(id, "old", "new")]),
    )
    .await
    .unwrap();
    row.description = Some("writer-visible".into());
    row.updated_at += 1;
    rt.entities(&token)
        .unwrap()
        .upsert_entity(row)
        .await
        .unwrap();
    assert!(matches!(
        crate::run_atomic_unit(rt.sql().as_ref(), plans)
            .await
            .unwrap(),
        crate::AtomicRunOutcome::Committed { .. }
    ));
    let mut live = rt
        .entities(&token)
        .unwrap()
        .get_entity(id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(live.description.as_deref(), Some("writer-visible"));
    assert_eq!(live.name, "new");
    let prior = Uuid::new_v4();
    let plans = prepare_canonical_changeset(
        rt,
        &registry(),
        &token,
        &changeset(vec![
            entity_create(prior, "rollback"),
            rename(id, "new", "after"),
        ]),
    )
    .await
    .unwrap();
    live.name = "concurrent".into();
    live.updated_at += 1;
    rt.entities(&token)
        .unwrap()
        .upsert_entity(live)
        .await
        .unwrap();
    assert!(
        matches!(crate::run_atomic_unit(rt.sql().as_ref(),plans).await.unwrap(),
        crate::AtomicRunOutcome::RolledBack{failed_op_index:1,failure:AtomicOpFailure::PreimageMismatch{field_path,..}} if field_path=="name")
    );
    assert!(rt
        .entities(&token)
        .unwrap()
        .get_entity(prior)
        .await
        .unwrap()
        .is_none());
}

#[derive(Debug)]
struct Owner;
#[async_trait::async_trait]
impl crate::pack::KindHook for Owner {
    async fn prepare_create(&self, _: &KhiveRuntime, _: &mut Value) -> RuntimeResult<()> {
        Ok(())
    }
}
#[tokio::test]
async fn owner_snapshot_conflict_is_distinct_from_field_preimage() {
    let f = fixture();
    let rt = &f.runtime;
    let token = rt.authorize(Namespace::local()).unwrap();
    rt.install_entity_kind_hooks(vec![("concept".into(), std::sync::Arc::new(Owner))]);
    let mut row = Entity::new("local", "concept", "old");
    let id = row.id;
    rt.entities(&token)
        .unwrap()
        .upsert_entity(row.clone())
        .await
        .unwrap();
    let plans = prepare_canonical_changeset(
        rt,
        &registry(),
        &token,
        &changeset(vec![rename(id, "old", "new")]),
    )
    .await
    .unwrap();
    row.description = Some("concurrent".into());
    row.updated_at += 1;
    rt.entities(&token)
        .unwrap()
        .upsert_entity(row)
        .await
        .unwrap();
    assert!(
        matches!(crate::run_atomic_unit(rt.sql().as_ref(),plans).await.unwrap(),
        crate::AtomicRunOutcome::RolledBack{failure:AtomicOpFailure::OwnerSnapshotConflict{target_id,..},..} if target_id==id)
    );
}

#[tokio::test]
async fn delete_ignores_only_updated_at_and_keeps_ordinary_delete_effects() {
    let f = fixture();
    let rt = &f.runtime;
    let token = rt.authorize(Namespace::local()).unwrap();
    let mut row = Entity::new("local", "concept", "delete");
    let id = row.id;
    rt.entities(&token)
        .unwrap()
        .upsert_entity(row.clone())
        .await
        .unwrap();
    let op = Op::Delete(DeleteOp {
        target_id: id128(id),
        hard: false,
        preimage: khive_changeset::DeletePreimage::Entity(Box::new(
            projection::entity(&row).unwrap(),
        )),
    });
    let plans = prepare_canonical_changeset(rt, &registry(), &token, &changeset(vec![op]))
        .await
        .unwrap();
    row.updated_at += 1;
    rt.entities(&token)
        .unwrap()
        .upsert_entity(row)
        .await
        .unwrap();
    assert!(matches!(
        crate::run_atomic_unit(rt.sql().as_ref(), plans)
            .await
            .unwrap(),
        crate::AtomicRunOutcome::Committed { .. }
    ));
    assert!(rt
        .entities(&token)
        .unwrap()
        .get_entity(id)
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn live_and_tombstone_ids_and_edge_keys_never_absorb_staged_ids() {
    let f = fixture();
    let rt = &f.runtime;
    let token = rt.authorize(Namespace::local()).unwrap();
    for deleted in [false, true] {
        let mut row = Entity::new("local", "concept", "holder");
        let id = row.id;
        if deleted {
            row.deleted_at = Some(row.updated_at);
        }
        rt.entities(&token)
            .unwrap()
            .upsert_entity(row)
            .await
            .unwrap();
        let plans = prepare_canonical_changeset(
            rt,
            &registry(),
            &token,
            &changeset(vec![entity_create(id, "staged")]),
        )
        .await
        .unwrap();
        // The staged-id check refuses it, not a later key constraint on the insert.
        match crate::run_atomic_unit(rt.sql().as_ref(), plans)
            .await
            .unwrap()
        {
            crate::AtomicRunOutcome::RolledBack {
                failed_op_index: 0,
                failure,
            } => assert!(
                format!("{failure:?}").contains("canonical staged id already exists"),
                "{failure:?}"
            ),
            other => panic!("deleted={deleted}: {other:?}"),
        }
    }
    let (a, b, e) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let link = |id: Uuid| {
        Op::Link(khive_changeset::LinkOp {
            id: id128(id),
            namespace: Namespace::local(),
            source: id128(a),
            target: id128(b),
            relation: khive_types::EdgeRelation::CompetesWith,
            weight: 0.5,
            properties: Default::default(),
        })
    };
    let plans = prepare_canonical_changeset(
        rt,
        &registry(),
        &token,
        &changeset(vec![
            entity_create(a, "a"),
            entity_create(b, "b"),
            link(e),
            link(Uuid::new_v4()),
        ]),
    )
    .await
    .unwrap();
    assert!(matches!(
        crate::run_atomic_unit(rt.sql().as_ref(), plans)
            .await
            .unwrap(),
        crate::AtomicRunOutcome::RolledBack {
            failed_op_index: 3,
            ..
        }
    ));
    assert!(rt
        .entities(&token)
        .unwrap()
        .get_entity(a)
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn missing_and_wrong_substrate_preimages_are_indexed_refusals() {
    let f = fixture();
    let rt = &f.runtime;
    let token = rt.authorize(Namespace::local()).unwrap();
    let id = Uuid::new_v4();
    let plans = prepare_canonical_changeset(
        rt,
        &registry(),
        &token,
        &changeset(vec![rename(id, "old", "new")]),
    )
    .await
    .unwrap();
    assert!(
        matches!(crate::run_atomic_unit(rt.sql().as_ref(),plans).await.unwrap(),crate::AtomicRunOutcome::RolledBack{
        failure:AtomicOpFailure::PreimageMismatch{field_path,..},..} if field_path=="id")
    );
    let row = Note::new("local", "observation", "note");
    let id = row.id;
    rt.notes(&token).unwrap().upsert_note(row).await.unwrap();
    let plans = prepare_canonical_changeset(
        rt,
        &registry(),
        &token,
        &changeset(vec![rename(id, "old", "new")]),
    )
    .await
    .unwrap();
    assert!(
        matches!(crate::run_atomic_unit(rt.sql().as_ref(),plans).await.unwrap(),crate::AtomicRunOutcome::RolledBack{
        failure:AtomicOpFailure::PreimageMismatch{field_path,..},..} if field_path=="substrate")
    );
}

#[test]
fn schema_count_and_merge_are_checked_before_preparation() {
    let mut cs = changeset(Vec::new());
    cs.envelope.schema_version += 1;
    assert!(validate(&cs).is_err());
    let cs = changeset(vec![entity_create(Uuid::new_v4(), "many"); 2001]);
    assert!(validate(&cs).is_err());
    let into = projection::entity(&Entity::new("local", "concept", "into")).unwrap();
    let from = projection::entity(&Entity::new("local", "concept", "from")).unwrap();
    let merge = Op::Merge(khive_changeset::MergeOp {
        into_id: into.header.id,
        from_id: from.header.id,
        preimage: khive_changeset::MergePreimage {
            into: Box::new(into),
            from: Box::new(from),
            incident_edges: Vec::new(),
        },
    });
    assert!(
        matches!(validate(&changeset(vec![merge])).unwrap_err(), RuntimeError::CanonicalAdmission { op_index: 0, source } if source.to_string().contains("KnownUnimplemented"))
    );
}

#[tokio::test]
async fn note_map_replacement_preserves_same_writer_tag_drift_and_tag_order() {
    let f = fixture();
    let rt = &f.runtime;
    let token = rt.authorize(Namespace::local()).unwrap();
    let mut note =
        Note::new("local", "observation", "same").with_properties(json!({"old":1,"tags":["old"]}));
    let id = note.id;
    rt.notes(&token)
        .unwrap()
        .upsert_note(note.clone())
        .await
        .unwrap();
    let old = projection::note(&note).unwrap().properties;
    let new = serde_json::from_value(json!({"replacement":2})).unwrap();
    let op = Op::Update(
        UpdateOp::new(
            id128(id),
            UpdatePatch::Note(khive_changeset::NotePatch {
                content: None,
                salience: None,
                decay_factor: None,
                properties: Some(new),
                tags: None,
            }),
            UpdatePreimage::Note(khive_changeset::NotePreimage {
                content: None,
                salience: None,
                decay_factor: None,
                properties: Some(old),
                tags: None,
            }),
        )
        .unwrap(),
    );
    let plans = prepare_canonical_changeset(rt, &registry(), &token, &changeset(vec![op]))
        .await
        .unwrap();
    note.properties = Some(json!({"old":1,"tags":["writer-b","writer-a"]}));
    note.updated_at += 1;
    rt.notes(&token).unwrap().upsert_note(note).await.unwrap();
    assert!(matches!(
        crate::run_atomic_unit(rt.sql().as_ref(), plans)
            .await
            .unwrap(),
        crate::AtomicRunOutcome::Committed { .. }
    ));
    let live = rt
        .notes(&token)
        .unwrap()
        .get_note(id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        live.properties,
        Some(json!({"replacement":2,"tags":["writer-b","writer-a"]}))
    );
}

#[tokio::test]
async fn canonical_writer_materialization_resolves_on_first_poll_and_suspend_misuse_refuses() {
    let f = fixture();
    let rt = &f.runtime;
    let token = rt.authorize(Namespace::local()).unwrap();
    let id = Uuid::new_v4();
    let plans = prepare_canonical_changeset(
        rt,
        &registry(),
        &token,
        &changeset(vec![
            entity_create(id, "ready"),
            rename(id, "ready", "committed"),
        ]),
    )
    .await
    .unwrap();
    let pool = std::sync::Arc::new(
        khive_db::ConnectionPool::new(khive_db::PoolConfig {
            path: Some(f._dir.path().join("canonical.db")),
            write_queue_enabled: Some(true),
            ..khive_db::PoolConfig::for_test()
        })
        .unwrap(),
    );
    let bridge = khive_db::SqlBridge::new(pool, true);
    assert!(matches!(
        crate::run_atomic_unit(&bridge, plans).await.unwrap(),
        crate::AtomicRunOutcome::Committed { .. }
    ));
    let row = rt
        .entities(&token)
        .unwrap()
        .get_entity(id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.name, "committed");
    let op: khive_storage::AtomicUnitOp = Box::new(|_writer| {
        Box::pin(async move {
            tokio::task::yield_now().await;
            Ok(Box::new(()) as Box<dyn std::any::Any + Send>)
        })
    });
    assert!(khive_storage::SqlAccess::atomic_unit(&bridge, op)
        .await
        .is_err());
}

#[tokio::test]
async fn edge_update_then_delete_observes_earlier_writer_state() {
    let f = fixture();
    let rt = &f.runtime;
    let token = rt.authorize(Namespace::local()).unwrap();
    let (a, b, e) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let initial = changeset(vec![
        entity_create(a, "a"),
        entity_create(b, "b"),
        Op::Link(khive_changeset::LinkOp {
            id: id128(e),
            namespace: Namespace::local(),
            source: id128(a),
            target: id128(b),
            relation: khive_types::EdgeRelation::CompetesWith,
            weight: 0.5,
            properties: Default::default(),
        }),
    ]);
    let plans = prepare_canonical_changeset(rt, &registry(), &token, &initial)
        .await
        .unwrap();
    assert!(matches!(
        crate::run_atomic_unit(rt.sql().as_ref(), plans)
            .await
            .unwrap(),
        crate::AtomicRunOutcome::Committed { .. }
    ));
    let live = rt.get_edge(&token, e).await.unwrap().unwrap();
    let mut deleted = projection::edge(&live).unwrap();
    deleted.weight = 0.75;
    let update = Op::Update(
        UpdateOp::new(
            id128(e),
            UpdatePatch::Edge(khive_changeset::EdgePatch {
                relation: None,
                weight: Some(0.75),
            }),
            UpdatePreimage::Edge(khive_changeset::EdgePreimage {
                relation: None,
                weight: Some(0.5),
            }),
        )
        .unwrap(),
    );
    let delete = Op::Delete(DeleteOp {
        target_id: id128(e),
        hard: false,
        preimage: khive_changeset::DeletePreimage::Edge(Box::new(deleted)),
    });
    let plans =
        prepare_canonical_changeset(rt, &registry(), &token, &changeset(vec![update, delete]))
            .await
            .unwrap();
    assert!(matches!(
        crate::run_atomic_unit(rt.sql().as_ref(), plans)
            .await
            .unwrap(),
        crate::AtomicRunOutcome::Committed { .. }
    ));
    assert!(rt.get_edge(&token, e).await.unwrap().is_none());
}

#[test]
fn shared_entity_fields_keep_legacy_merge_and_canonical_replace_distinct() {
    let original =
        Entity::new("local", "concept", "same").with_properties(json!({"old":1,"keep":2}));
    let patch = crate::curation::EntityPatch {
        properties: Some(json!({"new":3})),
        ..Default::default()
    };
    let mut legacy = original.clone();
    let mut canonical = original;
    KhiveRuntime::apply_entity_patch_fields(&mut legacy, patch.clone(), &[], false);
    KhiveRuntime::apply_entity_patch_fields(&mut canonical, patch, &[], true);
    assert_eq!(legacy.properties, Some(json!({"old":1,"keep":2,"new":3})));
    assert_eq!(canonical.properties, Some(json!({"new":3})));
}

#[tokio::test]
async fn restore_refuses_the_whole_changeset_before_preparation() {
    let f = fixture();
    let rt = &f.runtime;
    let token = rt.authorize(Namespace::local()).unwrap();
    let record = projection::entity(&Entity::new("local", "concept", "restored")).unwrap();
    let restored_id = uuid(record.header.id);
    let restore = Op::Restore(
        khive_changeset::RestoreOp::new(
            record.header.id,
            khive_changeset::DeletePreimage::Entity(Box::new(record)),
        )
        .unwrap(),
    );
    let first = Uuid::new_v4();
    let last = Uuid::new_v4();
    let events = rt.events(&token).unwrap();
    let before = events.count_events(Default::default()).await.unwrap();
    let error = prepare_canonical_changeset(
        rt,
        &registry(),
        &token,
        &changeset(vec![
            entity_create(first, "before"),
            restore,
            entity_create(last, "after"),
        ]),
    )
    .await
    .unwrap_err();
    assert!(matches!(
        &error,
        RuntimeError::CanonicalUnsupportedOperation {
            op_index: 1,
            operation: "restore",
        }
    ));
    let wire = crate::runtime_error_value(error, crate::DomainDisposition::Unknown);
    assert_eq!(wire["op_index"], 1);
    assert_eq!(wire["operation"], "restore");
    assert_eq!(wire["reason"], "KnownUnimplemented");
    assert_eq!(wire["domain_disposition"], "not_committed");
    assert_eq!(wire["retryable"], false);
    for id in [first, restored_id, last] {
        assert!(rt
            .entities(&token)
            .unwrap()
            .get_entity(id)
            .await
            .unwrap()
            .is_none());
    }
    assert_eq!(
        events.count_events(Default::default()).await.unwrap(),
        before
    );
}

#[tokio::test]
async fn canonical_memory_link_equal_namespace_keeps_source_namespace() {
    let f = fixture();
    let rt = &f.runtime;
    let token = rt.authorize(Namespace::local()).unwrap();
    let source = Note::new("local", "memory", "source");
    let target = Note::new("local", "memory", "target");
    let (source_id, target_id, edge_id) = (source.id, target.id, Uuid::new_v4());
    rt.notes(&token).unwrap().upsert_note(source).await.unwrap();
    rt.notes(&token).unwrap().upsert_note(target).await.unwrap();
    for id in [source_id, target_id] {
        let stored = rt
            .resolve_by_id_including_deleted(&token, id)
            .await
            .unwrap();
        let Some(Resolved::Note(stored)) = stored else {
            panic!("setup requires a stored note");
        };
        assert_eq!(stored.kind, "memory");
        assert_eq!(stored.namespace.as_str(), "local");
        assert!(stored.deleted_at.is_none());
    }
    let cs = changeset(vec![Op::Link(khive_changeset::LinkOp {
        id: id128(edge_id),
        namespace: Namespace::local(),
        source: id128(source_id),
        target: id128(target_id),
        relation: khive_types::EdgeRelation::Annotates,
        weight: 1.0,
        properties: Default::default(),
    })]);
    let plans = prepare_canonical_changeset(rt, &registry(), &token, &cs)
        .await
        .unwrap();
    assert!(matches!(
        crate::run_atomic_unit(rt.sql().as_ref(), plans)
            .await
            .unwrap(),
        crate::AtomicRunOutcome::Committed { .. }
    ));
    let edge = rt.get_edge(&token, edge_id).await.unwrap().unwrap();
    assert_eq!(Uuid::from(edge.id), edge_id);
    assert_eq!(edge.source_id, source_id);
    assert_eq!(edge.target_id, target_id);
    assert_eq!(edge.namespace.as_str(), "local");
}

#[tokio::test]
async fn canonical_memory_namespace_mismatch_refuses_before_any_write() {
    let f = fixture();
    let rt = &f.runtime;
    let source_token = rt.authorize(Namespace::local()).unwrap();
    let token = rt.authorize(Namespace::parse("other").unwrap()).unwrap();
    assert_eq!(token.namespace().as_str(), "other");
    let source = Note::new("local", "memory", "source");
    let target = Note::new("local", "memory", "target");
    let (source_id, target_id, edge_id) = (source.id, target.id, Uuid::new_v4());
    rt.notes(&source_token)
        .unwrap()
        .upsert_note(source)
        .await
        .unwrap();
    rt.notes(&source_token)
        .unwrap()
        .upsert_note(target)
        .await
        .unwrap();
    for id in [source_id, target_id] {
        let stored = rt
            .resolve_by_id_including_deleted(&token, id)
            .await
            .unwrap();
        let Some(Resolved::Note(stored)) = stored else {
            panic!("setup requires a readable stored note");
        };
        assert_eq!(stored.kind, "memory");
        assert_eq!(stored.namespace.as_str(), "local");
        assert!(stored.deleted_at.is_none());
    }
    let side_id = Uuid::new_v4();
    let mut side = entity_create(side_id, "earlier side op");
    let Op::Create(create) = &mut side else {
        panic!("setup requires an entity create");
    };
    create.namespace = Namespace::parse("other").unwrap();
    let cs = changeset(vec![
        side,
        Op::Link(khive_changeset::LinkOp {
            id: id128(edge_id),
            namespace: Namespace::parse("other").unwrap(),
            source: id128(source_id),
            target: id128(target_id),
            relation: khive_types::EdgeRelation::Annotates,
            weight: 1.0,
            properties: Default::default(),
        }),
    ]);
    let error = match prepare_canonical_changeset(rt, &registry(), &token, &cs).await {
        Err(error) => error,
        Ok(_) => panic!("canonical_memory_namespace_mismatch_admitted"),
    };
    let RuntimeError::CanonicalAdmission { op_index, source } = error else {
        panic!("expected canonical namespace admission refusal");
    };
    assert_eq!(op_index, 1);
    let RuntimeError::InvalidInput(message) = *source else {
        panic!("expected the memory namespace rule");
    };
    assert_eq!(
        message,
        "apply_changeset: memory edge source namespace local does not match link operation namespace other; set this link operation's namespace to local and prepare the change-set again"
    );
    assert!(rt.get_edge(&token, edge_id).await.unwrap().is_none());
    assert!(rt
        .entities(&token)
        .unwrap()
        .get_entity(side_id)
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn canonical_staged_memory_source_namespace_mismatch_refuses_before_any_write() {
    let f = fixture();
    let rt = &f.runtime;
    let source_token = rt.authorize(Namespace::local()).unwrap();
    let token = rt.authorize(Namespace::parse("other").unwrap()).unwrap();
    assert_eq!(token.namespace().as_str(), "other");
    let target = Note::new("local", "memory", "target");
    let (source_id, target_id, edge_id, side_id) =
        (Uuid::new_v4(), target.id, Uuid::new_v4(), Uuid::new_v4());
    rt.notes(&source_token)
        .unwrap()
        .upsert_note(target)
        .await
        .unwrap();
    let stored = rt
        .resolve_by_id_including_deleted(&token, target_id)
        .await
        .unwrap();
    let Some(Resolved::Note(stored)) = stored else {
        panic!("setup requires a readable stored target note");
    };
    assert_eq!(stored.kind, "memory");
    assert_eq!(stored.namespace.as_str(), "local");
    assert!(stored.deleted_at.is_none());
    let staged = Op::Create(khive_changeset::CreateOp {
        id: id128(source_id),
        namespace: Namespace::local(),
        target: CreateTarget::Note(khive_changeset::NoteCreateFields {
            note_kind: "memory".into(),
            content: "staged source".into(),
            properties: Default::default(),
            tags: Vec::new(),
            salience: None,
            decay_factor: None,
        }),
    });
    let setup_plans =
        prepare_canonical_changeset(rt, &registry(), &token, &changeset(vec![staged.clone()]))
            .await
            .expect("setup must admit the staged memory kind");
    assert_eq!(setup_plans.len(), 1);
    drop(setup_plans);
    assert!(rt
        .resolve_by_id_including_deleted(&token, source_id)
        .await
        .unwrap()
        .is_none());
    let mut side = entity_create(side_id, "earlier side op");
    let Op::Create(create) = &mut side else {
        panic!("setup requires an entity create");
    };
    create.namespace = Namespace::parse("other").unwrap();
    let cs = changeset(vec![
        side,
        staged,
        Op::Link(khive_changeset::LinkOp {
            id: id128(edge_id),
            namespace: Namespace::parse("other").unwrap(),
            source: id128(source_id),
            target: id128(target_id),
            relation: khive_types::EdgeRelation::Annotates,
            weight: 1.0,
            properties: Default::default(),
        }),
    ]);
    let error = match prepare_canonical_changeset(rt, &registry(), &token, &cs).await {
        Err(error) => error,
        Ok(_) => panic!("canonical_staged_memory_namespace_mismatch_admitted"),
    };
    let RuntimeError::CanonicalAdmission { op_index, source } = error else {
        panic!("expected canonical namespace admission refusal");
    };
    assert_eq!(op_index, 2);
    let RuntimeError::InvalidInput(message) = *source else {
        panic!("expected the memory namespace rule");
    };
    assert_eq!(
        message,
        "apply_changeset: memory edge source namespace local does not match link operation namespace other; set this link operation's namespace to local and prepare the change-set again"
    );
    assert!(rt.get_edge(&token, edge_id).await.unwrap().is_none());
    assert!(rt
        .resolve_by_id_including_deleted(&token, source_id)
        .await
        .unwrap()
        .is_none());
    assert!(rt
        .entities(&token)
        .unwrap()
        .get_entity(side_id)
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn canonical_memory_to_nonmemory_keeps_op_namespace() {
    let f = fixture();
    let rt = &f.runtime;
    let source_token = rt.authorize(Namespace::local()).unwrap();
    let token = rt.authorize(Namespace::parse("other").unwrap()).unwrap();
    assert_eq!(token.namespace().as_str(), "other");
    let source = Note::new("local", "memory", "source");
    let target = Note::new("other", "observation", "target");
    let (source_id, target_id, edge_id) = (source.id, target.id, Uuid::new_v4());
    rt.notes(&source_token)
        .unwrap()
        .upsert_note(source)
        .await
        .unwrap();
    rt.notes(&token).unwrap().upsert_note(target).await.unwrap();
    for (id, kind, namespace) in [
        (source_id, "memory", "local"),
        (target_id, "observation", "other"),
    ] {
        let stored = rt
            .resolve_by_id_including_deleted(&token, id)
            .await
            .unwrap();
        let Some(Resolved::Note(stored)) = stored else {
            panic!("setup requires a readable stored note");
        };
        assert_eq!(stored.kind, kind);
        assert_eq!(stored.namespace.as_str(), namespace);
        assert!(stored.deleted_at.is_none());
    }
    let cs = changeset(vec![Op::Link(khive_changeset::LinkOp {
        id: id128(edge_id),
        namespace: Namespace::parse("other").unwrap(),
        source: id128(source_id),
        target: id128(target_id),
        relation: khive_types::EdgeRelation::Annotates,
        weight: 1.0,
        properties: Default::default(),
    })]);
    let plans = prepare_canonical_changeset(rt, &registry(), &token, &cs)
        .await
        .unwrap();
    assert!(matches!(
        crate::run_atomic_unit(rt.sql().as_ref(), plans)
            .await
            .unwrap(),
        crate::AtomicRunOutcome::Committed { .. }
    ));
    let edge = rt.get_edge(&token, edge_id).await.unwrap().unwrap();
    assert_eq!(Uuid::from(edge.id), edge_id);
    assert_eq!(edge.source_id, source_id);
    assert_eq!(edge.target_id, target_id);
    assert_eq!(edge.namespace.as_str(), "other");
}

#[tokio::test]
async fn canonical_memory_namespace_change_before_writer_rolls_back() {
    let f = fixture();
    let rt = &f.runtime;
    let token = rt.authorize(Namespace::local()).unwrap();
    let moved_token = rt.authorize(Namespace::parse("other").unwrap()).unwrap();
    let source = Note::new("local", "memory", "source");
    let target = Note::new("local", "memory", "target");
    let (source_id, target_id, edge_id, side_id) =
        (source.id, target.id, Uuid::new_v4(), Uuid::new_v4());
    rt.notes(&token).unwrap().upsert_note(source).await.unwrap();
    rt.notes(&token).unwrap().upsert_note(target).await.unwrap();
    for id in [source_id, target_id] {
        let stored = rt
            .resolve_by_id_including_deleted(&token, id)
            .await
            .unwrap();
        let Some(Resolved::Note(stored)) = stored else {
            panic!("setup requires a stored note");
        };
        assert_eq!(stored.kind, "memory");
        assert_eq!(stored.namespace.as_str(), "local");
        assert!(stored.deleted_at.is_none());
    }
    let cs = changeset(vec![
        entity_create(side_id, "earlier side op"),
        Op::Link(khive_changeset::LinkOp {
            id: id128(edge_id),
            namespace: Namespace::local(),
            source: id128(source_id),
            target: id128(target_id),
            relation: khive_types::EdgeRelation::Annotates,
            weight: 1.0,
            properties: Default::default(),
        }),
    ]);
    let plans = prepare_canonical_changeset(rt, &registry(), &token, &cs)
        .await
        .expect("equal-namespace prepare must succeed before drift");
    assert_eq!(plans.len(), 2);
    let statement = SqlStatement::new(
        "UPDATE notes SET namespace = ?1 WHERE id = ?2",
        vec![
            SqlValue::Text("other".into()),
            SqlValue::Text(source_id.to_string()),
        ],
    );
    let drift: khive_storage::AtomicUnitOp = Box::new(move |writer| {
        Box::pin(async move {
            let affected = writer.execute(statement).await?;
            Ok(Box::new(affected) as Box<dyn std::any::Any + Send>)
        })
    });
    let changed = khive_storage::SqlAccess::atomic_unit(rt.sql().as_ref(), drift)
        .await
        .expect("private fixture namespace drift");
    let affected = match changed.downcast::<u64>() {
        Ok(affected) => *affected,
        Err(_) => panic!("namespace drift result type"),
    };
    assert_eq!(affected, 1, "fixture must move exactly one source row");
    let stored = rt
        .resolve_by_id_including_deleted(&moved_token, source_id)
        .await
        .unwrap();
    let Some(Resolved::Note(stored)) = stored else {
        panic!("setup requires the drifted stored source");
    };
    assert_eq!(stored.kind, "memory");
    assert_eq!(stored.namespace.as_str(), "other");
    assert!(stored.deleted_at.is_none());
    let outcome = crate::run_atomic_unit(rt.sql().as_ref(), plans)
        .await
        .unwrap();
    if matches!(&outcome, crate::AtomicRunOutcome::Committed { .. }) {
        panic!("canonical_memory_writer_namespace_mismatch_admitted");
    }
    let crate::AtomicRunOutcome::RolledBack { failure, .. } = outcome else {
        panic!("expected a canonical writer rollback");
    };
    let AtomicOpFailure::SqlError {
        statement_label,
        message,
    } = failure
    else {
        panic!("expected the canonical namespace SQL failure");
    };
    assert_eq!(statement_label.as_deref(), Some("canonical-materialize"));
    assert_eq!(
        message,
        RuntimeError::InvalidInput(
            "apply_changeset: memory edge source namespace other does not match link operation namespace local; set this link operation's namespace to other and prepare the change-set again".into()
        )
        .to_string()
    );
    assert!(rt.get_edge(&token, edge_id).await.unwrap().is_none());
    assert!(rt
        .entities(&token)
        .unwrap()
        .get_entity(side_id)
        .await
        .unwrap()
        .is_none());
    let stored = rt
        .resolve_by_id_including_deleted(&moved_token, source_id)
        .await
        .unwrap();
    let Some(Resolved::Note(stored)) = stored else {
        panic!("external namespace drift must remain stored");
    };
    assert_eq!(stored.kind, "memory");
    assert_eq!(stored.namespace.as_str(), "other");
}

#[test]
fn memory_namespace_refusals_name_operation_and_supported_remedy() {
    let source = Note::new("other", "memory", "source");
    let target = Note::new("target", "memory", "target");
    let nonmemory = Note::new("target", "observation", "nonmemory target");
    assert_eq!(source.kind, "memory");
    assert_eq!(source.namespace.as_str(), "other");
    assert_eq!(target.kind, "memory");
    assert!(source.deleted_at.is_none());
    assert!(target.deleted_at.is_none());
    for (operation, expected) in [
        (
            "restore",
            "restore: memory edge source namespace other does not match stored edge namespace local; leave this tombstone deleted and create a replacement link in namespace=other",
        ),
        (
            "apply_changeset",
            "apply_changeset: memory edge source namespace other does not match link operation namespace local; set this link operation's namespace to other and prepare the change-set again",
        ),
        (
            "link",
            "link: memory edge source namespace other does not match write namespace local; retry with namespace=other",
        ),
    ] {
        assert!(KhiveRuntime::ensure_memory_note_edge_namespace(
            operation,
            "other",
            source.id,
            target.id,
            khive_types::EdgeRelation::Annotates,
            &source,
            &target,
        )
        .is_ok());
        assert!(KhiveRuntime::ensure_memory_note_edge_namespace(
            operation,
            "local",
            source.id,
            nonmemory.id,
            khive_types::EdgeRelation::Annotates,
            &source,
            &nonmemory,
        )
        .is_ok());
        let error = KhiveRuntime::ensure_memory_note_edge_namespace(
            operation,
            "local",
            source.id,
            target.id,
            khive_types::EdgeRelation::Annotates,
            &source,
            &target,
        )
        .unwrap_err();
        let RuntimeError::InvalidInput(message) = error else {
            panic!("namespace refusal must preserve InvalidInput");
        };
        assert_eq!(
            message, expected,
            "memory_namespace_refusal_identifies_operation_and_action"
        );
    }
}
