use super::*;
use khive_runtime::{Namespace, RuntimeConfig};

fn runtime() -> (KhiveRuntime, NamespaceToken) {
    let namespace = Namespace::parse("direct-entity-test").unwrap();
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        default_namespace: namespace.clone(),
        events_split: None,
        wal_ceiling_bytes: 0,
        wal_ceiling_configured_bytes: 0,
        wal_ceiling_env_raw: None,
        ..RuntimeConfig::no_embeddings()
    })
    .unwrap();
    let token = runtime.authorize(namespace).unwrap();
    (runtime, token)
}

#[tokio::test]
async fn empty_manifest_keeps_guarded_entity_and_fts_writes() {
    let (runtime, token) = runtime();
    let entity = Entity::new(token.namespace().as_str(), "concept", "original");
    let id = entity.id;
    let mut report = CodeSourceIngestReport::default();
    assert_eq!(
        mutate_entity(&runtime, &token, id, "source.rs", &mut report, |_| {
            Some(entity.clone())
        })
        .await
        .unwrap(),
        RowMutationOutcome::Created
    );
    let store = runtime.entities(&token).unwrap();
    let before = store.get_entity(id).await.unwrap().unwrap();
    assert_eq!(
        mutate_entity(&runtime, &token, id, "source.rs", &mut report, |current| {
            let mut changed = current.unwrap().clone();
            changed.name = "updated".into();
            Some(changed)
        })
        .await
        .unwrap(),
        RowMutationOutcome::Updated
    );
    let after = store.get_entity(id).await.unwrap().unwrap();
    assert_eq!(after.name, "updated");
    assert_eq!(after.created_at, before.created_at);
    assert_eq!(after.version, before.version + 1);
    assert!(after.updated_at > before.updated_at);
    assert!(after.properties.is_none());
    let text = runtime
        .text(&token)
        .unwrap()
        .get_document(token.namespace().as_str(), id)
        .await
        .unwrap()
        .unwrap();
    let expected_text = entity_fts_document(&after);
    assert_eq!(text.subject_id, after.id);
    assert_eq!(text.namespace, after.namespace);
    assert_eq!(text.title, expected_text.title);
    assert_eq!(text.body, expected_text.body);
    assert_eq!(report.fts_indexed, 2);
    assert_eq!(
        mutate_entity(&runtime, &token, id, "source.rs", &mut report, |_| None)
            .await
            .unwrap(),
        RowMutationOutcome::Unchanged
    );
    assert_eq!(report.fts_indexed, 2);
    assert_eq!(
        serde_json::to_value(store.get_entity(id).await.unwrap().unwrap()).unwrap(),
        serde_json::to_value(after).unwrap()
    );
}

#[tokio::test]
async fn direct_mutation_refuses_namespace_mismatch_before_persistence() {
    let (runtime, token) = runtime();
    let entity = Entity::new("other-namespace", "concept", "candidate");
    let mut report = CodeSourceIngestReport::default();
    let error = mutate_entity(
        &runtime,
        &token,
        entity.id,
        "source.rs",
        &mut report,
        |_| Some(entity.clone()),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(error, CodeSourceIngestError::Runtime(RuntimeError::InvalidInput(ref message)) if message.contains("namespace"))
    );
    let other = runtime
        .authorize(Namespace::parse("other-namespace").unwrap())
        .unwrap();
    assert!(runtime
        .entities(&other)
        .unwrap()
        .get_entity(entity.id)
        .await
        .unwrap()
        .is_none());
    assert_eq!(report.fts_indexed, 0);
}

#[tokio::test]
async fn direct_mutation_keeps_unmatched_secret_refusal_and_existing_row() {
    let (runtime, token) = runtime();
    let entity = Entity::new(token.namespace().as_str(), "concept", "safe");
    let mut report = CodeSourceIngestReport::default();
    mutate_entity(
        &runtime,
        &token,
        entity.id,
        "source.rs",
        &mut report,
        |_| Some(entity.clone()),
    )
    .await
    .unwrap();
    let before = runtime
        .entities(&token)
        .unwrap()
        .get_entity(entity.id)
        .await
        .unwrap()
        .unwrap();
    let secret = format!("{}{}", "ghp_", "aB3xY7mN9qR2sT5vW8zC4dE6fG1hJ0kL");
    assert!(khive_runtime::secret_gate::check(&secret).is_err());
    let result = mutate_entity(
        &runtime,
        &token,
        entity.id,
        "source.rs",
        &mut report,
        |current| {
            let mut changed = current.unwrap().clone();
            changed.description = Some(secret.clone());
            Some(changed)
        },
    )
    .await
    .unwrap();
    assert_eq!(result, RowMutationOutcome::Blocked);
    assert_eq!(report.blocked_count, 1);
    assert_eq!(report.fts_indexed, 1);
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
        serde_json::to_value(before).unwrap()
    );
}
