use super::*;
use khive_runtime::RuntimeConfig;

fn runtime() -> KhiveRuntime {
    KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        events_split: None,
        wal_ceiling_bytes: 0,
        wal_ceiling_configured_bytes: 0,
        wal_ceiling_env_raw: None,
        ..RuntimeConfig::no_embeddings()
    })
    .unwrap()
}

#[tokio::test]
async fn empty_manifest_candidate_preserves_direct_entity_write() {
    let runtime = runtime();
    let token = runtime
        .authorize(Namespace::parse("local").unwrap())
        .unwrap();
    let entity = Entity::new("local", "concept", "clean candidate")
        .with_properties(json!({"description": "ordinary data"}));
    let prepared = prepare_ingest_entity(&entity, "entity[0]").unwrap();
    assert_eq!(
        serde_json::to_value(prepared.entity()).unwrap(),
        serde_json::to_value(&entity).unwrap()
    );
    assert!(matches!(
        persist_ingest_entity(&runtime, &token, prepared)
            .await
            .unwrap(),
        EntityCandidateAdmission::Legacy(_)
    ));
    assert_eq!(
        serde_json::to_value(
            runtime
                .entities(&token)
                .unwrap()
                .get_entity(entity.id)
                .await
                .unwrap()
                .unwrap()
        )
        .unwrap(),
        serde_json::to_value(entity).unwrap()
    );
}

#[tokio::test]
async fn direct_writer_checks_prepared_namespace_against_authorization() {
    let runtime = runtime();
    let token = runtime
        .authorize(Namespace::parse("local").unwrap())
        .unwrap();
    let entity = Entity::new("another", "concept", "clean candidate");
    let prepared = prepare_ingest_entity(&entity, "entity[0]").unwrap();
    let error = persist_ingest_entity(&runtime, &token, prepared)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("namespace"));
    let other = runtime
        .authorize(Namespace::parse("another").unwrap())
        .unwrap();
    assert!(runtime
        .entities(&other)
        .unwrap()
        .get_entity(entity.id)
        .await
        .unwrap()
        .is_none());
}

#[test]
fn preflight_refuses_unmatched_secret_before_any_runtime_is_needed() {
    let secret = format!("{}{}", "ghp_", "aB3xY7mN9qR2sT5vW8zC4dE6fG1hJ0kL");
    assert!(secret_gate::check(&secret).is_err());
    let mut entity = Entity::new("local", "concept", "clean name");
    entity.tags.push(secret);
    let batch = CodeIngestBatch {
        entities: vec![entity],
        notes: vec![],
        edges: vec![],
    };
    let error = preflight_secret_gate(&batch).unwrap_err();
    assert!(error.to_string().contains("entity[0].tags"));
}
