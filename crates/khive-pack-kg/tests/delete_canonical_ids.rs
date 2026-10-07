//! Delete acknowledgements identify the resolved record, not its input alias.

use khive_pack_kg::KgPack;
use khive_runtime::{
    present, KhiveRuntime, Namespace, PresentationMode, RuntimeConfig, RuntimeError, VerbRegistry,
    VerbRegistryBuilder, WalCeilingSource,
};
use khive_storage::{
    Attachment, AttachmentSubstrate, ContentRef, Edge, EdgeRelation, Entity, Note,
};
use serde_json::{json, Value};
use uuid::Uuid;

fn surface() -> (KhiveRuntime, VerbRegistry) {
    let config = RuntimeConfig {
        db_path: None,
        embedding_model: None,
        additional_embedding_models: Vec::new(),
        wal_ceiling_bytes: 0,
        wal_ceiling_configured_bytes: 0,
        wal_ceiling_env_raw: None,
        wal_ceiling_source: WalCeilingSource::Default,
        disk_guard_config: None,
        disk_guard_environment: Default::default(),
        volume_lock_dir: None,
        visibility_receipts: None,
        credentials: Vec::new(),
        actor_id: None,
        brain_profile: None,
        brain: Default::default(),
        events_split: None,
        mounts: Vec::new(),
        blob: Default::default(),
        packs: vec!["kg".into()],
        ..RuntimeConfig::no_embeddings()
    };
    assert!(config.db_path.is_none());
    assert!(config.embedding_model.is_none());
    assert!(config.additional_embedding_models.is_empty());
    let runtime = KhiveRuntime::new(config).expect("isolated in-memory runtime");
    assert!(!runtime.backend().is_file_backed());
    assert!(runtime.backend_data_dir().is_none());
    assert!(runtime.backend_ann_root().is_none());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime.clone()));
    (runtime, builder.build().expect("KG registry"))
}

fn target_id() -> Uuid {
    Uuid::parse_str("abcdef01-abcd-4abc-8abc-0123456789ab").unwrap()
}

async fn seed(runtime: &KhiveRuntime, kind: &str) {
    let token = runtime.authorize(Namespace::local()).unwrap();
    let id = target_id();
    match kind {
        "entity" => {
            let mut entity = Entity::new("local", "concept", "Delete by name");
            entity.id = id;
            runtime
                .entities(&token)
                .unwrap()
                .upsert_entity(entity)
                .await
                .unwrap();
        }
        "note" => {
            let mut note = Note::new("local", "observation", "Delete acknowledgement fixture");
            note.id = id;
            runtime
                .notes(&token)
                .unwrap()
                .upsert_note(note)
                .await
                .unwrap();
        }
        "edge" => {
            let mut source = Entity::new("local", "concept", "Source");
            source.id = Uuid::parse_str("bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb").unwrap();
            let mut target = Entity::new("local", "concept", "Target");
            target.id = Uuid::parse_str("cccccccc-cccc-4ccc-8ccc-cccccccccccc").unwrap();
            let now = chrono::Utc::now();
            let edge = Edge {
                id: id.into(),
                namespace: "local".into(),
                source_id: source.id,
                target_id: target.id,
                relation: EdgeRelation::Extends,
                weight: 1.0,
                created_at: now,
                updated_at: now,
                deleted_at: None,
                metadata: None,
                target_backend: None,
            };
            let entities = runtime.entities(&token).unwrap();
            entities.upsert_entity(source).await.unwrap();
            entities.upsert_entity(target).await.unwrap();
            runtime
                .graph(&token)
                .unwrap()
                .upsert_edge(edge)
                .await
                .unwrap();
        }
        _ => unreachable!(),
    }
}

fn assert_ack(response: &Value, kind: &str, deleted: bool) {
    assert_eq!(response["id"], target_id().to_string());
    assert_eq!(response["kind"], kind);
    assert_eq!(response["deleted"], deleted);
    assert_eq!(
        present(response.clone(), PresentationMode::Verbose, 0),
        *response
    );
    // The ordinary Agent transform still compacts display IDs; this is not a new full_id field.
    assert_eq!(
        present(response.clone(), PresentationMode::Agent, 0)["id"],
        "abcdef01"
    );
}

#[tokio::test]
async fn delete_returns_canonical_ids_for_each_substrate_and_input_spelling() {
    for kind in ["entity", "note", "edge"] {
        for (spelling, hard) in [
            ("canonical", false),
            ("uppercase", true),
            ("simple", false),
            ("prefix", true),
        ] {
            let (runtime, registry) = surface();
            seed(&runtime, kind).await;
            let reference = match spelling {
                "canonical" => target_id().to_string(),
                "uppercase" => target_id().to_string().to_uppercase(),
                "simple" => target_id().simple().to_string(),
                "prefix" => "abcdef01".to_string(),
                _ => unreachable!(),
            };
            let response = registry
                .dispatch("delete", json!({"id": reference, "hard": hard}))
                .await
                .unwrap();
            let resolved_kind = match kind {
                "entity" => "concept",
                "note" => "observation",
                _ => "edge",
            };
            assert_ack(&response, resolved_kind, true);
            let token = runtime.authorize(Namespace::local()).unwrap();
            let tombstone = match kind {
                "entity" => runtime
                    .entities(&token)
                    .unwrap()
                    .get_entity_including_deleted(target_id())
                    .await
                    .unwrap()
                    .map(|row| row.deleted_at.is_some()),
                "note" => runtime
                    .notes(&token)
                    .unwrap()
                    .get_note_including_deleted(target_id())
                    .await
                    .unwrap()
                    .map(|row| row.deleted_at.is_some()),
                _ => runtime
                    .graph(&token)
                    .unwrap()
                    .get_edge_including_deleted(target_id().into())
                    .await
                    .unwrap()
                    .map(|row| row.deleted_at.is_some()),
            };
            assert_eq!(tombstone, if hard { None } else { Some(true) });
        }
    }
    let (runtime, registry) = surface();
    seed(&runtime, "entity").await;
    let response = registry
        .dispatch("delete", json!({"id": "Delete by name", "hard": true}))
        .await
        .unwrap();
    assert_ack(&response, "concept", true);
    assert!(matches!(
        registry.dispatch("get", json!({"id": target_id()})).await,
        Err(RuntimeError::NotFound(_))
    ));
}

#[tokio::test]
async fn attachment_cleanup_acknowledgements_use_canonical_ids_with_or_without_kind() {
    for explicit_kind in [false, true] {
        let (runtime, registry) = surface();
        let attachments = runtime.attachments().unwrap();
        // A valid metadata-only orphan exercises the existing cleanup retry path without a blob file.
        attachments
            .upsert_attachment(Attachment {
                record_uuid: target_id(),
                substrate: AttachmentSubstrate::Entity,
                role: "content".into(),
                content_ref: ContentRef::from_hex("a".repeat(64)).unwrap(),
                media_type: None,
                size_bytes: None,
                created_at: 0,
            })
            .await
            .unwrap();
        assert_eq!(
            attachments
                .list_attachments(target_id())
                .await
                .unwrap()
                .len(),
            1
        );
        let mut params = json!({"id": target_id().to_string().to_uppercase(), "hard": true});
        if explicit_kind {
            params["kind"] = json!("entity");
        }
        let response = registry.dispatch("delete", params.clone()).await.unwrap();
        assert_ack(&response, "entity", false);
        assert_eq!(response["attachment_cleanup"], true);
        assert!(attachments
            .list_attachments(target_id())
            .await
            .unwrap()
            .is_empty());
        assert!(matches!(
            registry.dispatch("delete", params).await,
            Err(RuntimeError::NotFound(_))
        ));
    }
}
