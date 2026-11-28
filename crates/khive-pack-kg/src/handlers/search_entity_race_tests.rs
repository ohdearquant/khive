use super::*;
use khive_runtime::curation::entity_fts_document;
use khive_runtime::{Namespace, RuntimeConfig, VerbRegistryBuilder, WalCeilingSource};
use khive_storage::Entity;
use tokio::sync::oneshot;

const QUERY: &str = "entitymetadataracefixture";

async fn race_case(request: Value) {
    let actor = "entity-metadata-race";
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
        actor_id: Some(actor.into()),
        brain_profile: None,
        brain: Default::default(),
        events_split: None,
        mounts: Vec::new(),
        blob: Default::default(),
        packs: vec!["kg".into()],
        ..RuntimeConfig::no_embeddings()
    };
    let runtime = KhiveRuntime::new(config).expect("isolated in-memory runtime");
    assert!(!runtime.backend().is_file_backed());
    assert!(runtime.backend_data_dir().is_none());
    assert!(runtime.backend_ann_root().is_none());
    let token = runtime.authorize(Namespace::local()).unwrap();
    let mut builder = VerbRegistryBuilder::new();
    builder.with_actor_id(Some(actor.into()));
    builder.register(KgPack::new(runtime.clone()));
    let registry = builder.build().unwrap();

    let target = Uuid::from_u128(1);
    let survivor = Uuid::from_u128(2);
    for (id, timestamp) in [(target, 2_000_000), (survivor, 1_000_000)] {
        let mut entity = Entity::new("local", "concept", format!("{QUERY} {id}"));
        entity.id = id;
        entity.created_at = timestamp;
        entity.updated_at = timestamp;
        entity.properties = Some(json!({"bucket": "race"}));
        entity.tags = vec!["race".to_string()];
        runtime
            .entities(&token)
            .unwrap()
            .upsert_entity(entity.clone())
            .await
            .unwrap();
        runtime
            .text(&token)
            .unwrap()
            .upsert_document(entity_fts_document(&entity))
            .await
            .unwrap();
    }

    let baseline = registry.dispatch("search", request.clone()).await.unwrap();
    let baseline_ids: Vec<_> = baseline
        .as_array()
        .unwrap()
        .iter()
        .map(|row| Uuid::parse_str(row["id"].as_str().unwrap()).unwrap())
        .collect();
    assert_eq!(
        baseline_ids.len(),
        2,
        "both indexed entities must be visible"
    );
    assert!(baseline_ids.contains(&target) && baseline_ids.contains(&survivor));
    assert!(baseline
        .as_array()
        .unwrap()
        .iter()
        .all(|row| row["kind"].is_string()));

    let (reached_tx, reached_rx) = oneshot::channel();
    let (proceed_tx, proceed_rx) = oneshot::channel();
    let search = ENTITY_HYDRATION_TEST_BARRIER.scope(
        std::cell::RefCell::new(Some((reached_tx, proceed_rx))),
        registry.dispatch("search", request),
    );
    let mutate = async {
        let candidates = reached_rx
            .await
            .expect("search reached the metadata barrier");
        assert!(
            candidates.contains(&target),
            "target must survive the first retrieval"
        );
        assert!(
            candidates.contains(&survivor),
            "survivor must survive the first retrieval"
        );
        let ack = registry
            .dispatch("delete", json!({"id": target.to_string(), "hard": false}))
            .await
            .unwrap();
        assert_eq!(ack["id"], target.to_string());
        assert_eq!(ack["deleted"], true);
        let entities = runtime.entities(&token).unwrap();
        let deleted = entities
            .get_entity_including_deleted(target)
            .await
            .unwrap()
            .unwrap();
        let still_live = entities
            .get_entity_including_deleted(survivor)
            .await
            .unwrap()
            .unwrap();
        assert!(deleted.deleted_at.is_some());
        assert!(still_live.deleted_at.is_none());
        proceed_tx
            .send(())
            .expect("release the waiting metadata fetch");
    };
    let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        tokio::join!(search, mutate)
    })
    .await
    .expect("the isolated barrier and writer must complete");
    let result = result.unwrap();
    let rows = result.as_array().unwrap();
    assert_eq!(rows.len(), 1, "the missing target must not be returned");
    assert_eq!(rows[0]["id"], survivor.to_string());
    assert!(rows[0]["kind"].is_string());
}

#[tokio::test]
async fn entity_deleted_between_retrieval_and_metadata_is_omitted() {
    for request in [
        json!({"kind": "entity", "query": QUERY, "limit": 10}),
        json!({"kind": "entity", "query": QUERY, "limit": 2, "order_by": "updated_at"}),
        json!({"kind": "entity", "query": QUERY, "limit": 10,
               "source": "text", "tags": ["race"], "properties": {"bucket": "race"}}),
    ] {
        race_case(request).await;
    }
}
