use super::*;

use crate::atomic_runner::{
    run_atomic_unit, AtomicOpFailure, AtomicRunOutcome, CommittedPostCommitEffects,
};
use crate::runtime::RuntimeConfig;
use khive_storage::Entity;
use khive_types::{ImportKindPolicy, Namespace};
use serde_json::json;

struct Fixture {
    runtime: KhiveRuntime,
    token: NamespaceToken,
    _directory: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("temporary runtime directory");
        let runtime = KhiveRuntime::new_for_test(RuntimeConfig {
            db_path: Some(directory.path().join("import-entity.db")),
            ..RuntimeConfig::no_embeddings()
        })
        .expect("runtime");
        runtime.install_kind_registry(vec!["concept".into()], vec![]);
        let token = runtime
            .authorize(Namespace::parse("local").expect("namespace"))
            .expect("authorize");
        Self {
            runtime,
            token,
            _directory: directory,
        }
    }

    fn entity(&self) -> Entity {
        Entity {
            entity_type: Some("Opaque Archive Subtype".into()),
            description: Some("archive description".into()),
            properties: Some(json!({"nested": {"archive": 1}, "source": "archive"})),
            tags: vec!["second".into(), "first".into()],
            ..Entity::minimal(
                Uuid::from_u128(0x4766),
                self.token.namespace().as_str(),
                "concept",
                "ArchiveExactName",
                1_234_567_890_123_456,
                1_234_567_890_123_999,
            )
        }
    }

    async fn commit(&self, plan: AtomicOpPlan) -> CommittedPostCommitEffects {
        match run_atomic_unit(self.runtime.sql().as_ref(), vec![plan])
            .await
            .expect("atomic owner settles")
        {
            AtomicRunOutcome::Committed { post_commit } => post_commit,
            other => panic!("expected commit, got {other:?}"),
        }
    }

    async fn stored(&self, id: Uuid) -> Option<Entity> {
        self.runtime
            .entities(&self.token)
            .expect("entity store")
            .get_entity(id)
            .await
            .expect("read entity")
    }
}

#[tokio::test]
async fn import_entity_plan_keeps_exact_record_fts_and_deferred_effect() {
    let fixture = Fixture::new();
    let entity = fixture.entity();
    let plan = prepare_import_entity_record(
        &fixture.runtime,
        &fixture.token,
        entity.clone(),
        ImportKindPolicy::Strict,
    )
    .expect("prepare exact archive record");
    assert!(fixture.stored(entity.id).await.is_none());

    let effects = fixture.commit(plan).await;
    let stored = fixture.stored(entity.id).await.expect("imported entity");
    assert_eq!(
        serde_json::to_value(stored).expect("serialize stored entity"),
        serde_json::to_value(&entity).expect("serialize archive entity"),
        "identity, microsecond timestamps, subtype and payload must survive unchanged"
    );
    assert!(fixture
        .runtime
        .text(&fixture.token)
        .expect("text store")
        .get_document(fixture.token.namespace().as_str(), entity.id)
        .await
        .expect("read FTS document")
        .is_some());
    assert_eq!(
        effects.as_slice(),
        &[PostCommitEffect::ReindexEntity {
            entity_id: entity.id
        }],
        "vector maintenance remains one deferred committed effect"
    );
}

#[tokio::test]
async fn import_entity_plan_refuses_a_winner_inserted_after_prepare() {
    let fixture = Fixture::new();
    let imported = fixture.entity();
    let plan = prepare_import_entity_record(
        &fixture.runtime,
        &fixture.token,
        imported.clone(),
        ImportKindPolicy::Strict,
    )
    .expect("prepare before another insert");
    let mut winner = imported.clone();
    winner.name = "ConcurrentWinner".into();
    winner.created_at += 1_000;
    winner.updated_at += 2_000;
    winner.properties = Some(json!({"winner": true}));
    fixture
        .runtime
        .entities(&fixture.token)
        .expect("entity store")
        .upsert_entity(winner.clone())
        .await
        .expect("insert winner after prepare");

    let failure = match run_atomic_unit(fixture.runtime.sql().as_ref(), vec![plan])
        .await
        .expect("atomic owner settles")
    {
        AtomicRunOutcome::RolledBack {
            failed_op_index: 0,
            failure,
        } => failure,
        other => panic!("the conditional insert must refuse the winner: {other:?}"),
    };
    match failure {
        AtomicOpFailure::GuardFailed {
            statement_label,
            expected,
            observed,
        } => {
            assert_eq!(statement_label.as_deref(), Some("entity-insert-if-absent"));
            assert_eq!(expected, AffectedRowGuard::exactly(1));
            assert_eq!(observed, 0);
        }
        other => panic!("expected the row-insert guard, got {other:?}"),
    }
    assert_eq!(
        serde_json::to_value(fixture.stored(winner.id).await.expect("winner remains"))
            .expect("serialize stored winner"),
        serde_json::to_value(winner).expect("serialize expected winner")
    );
    assert!(fixture
        .runtime
        .text(&fixture.token)
        .expect("text store")
        .get_document(fixture.token.namespace().as_str(), imported.id)
        .await
        .expect("read FTS document")
        .is_none());
}

#[tokio::test]
async fn import_entity_plan_keeps_subtype_bytes_and_preserve_unknown_policy() {
    let fixture = Fixture::new();
    fixture
        .runtime
        .install_entity_type_validator(std::sync::Arc::new(|_, value| match value {
            None => Ok(None),
            Some(_) => Err(RuntimeError::InvalidInput(
                "the subtype normalizer must not transform an archive record".into(),
            )),
        }));
    let mut unknown = fixture.entity();
    unknown.kind = "archive-extension-kind".into();
    let error = prepare_import_entity_record(
        &fixture.runtime,
        &fixture.token,
        unknown.clone(),
        ImportKindPolicy::Strict,
    )
    .expect_err("strict policy still refuses an unregistered kind");
    assert!(
        matches!(error, RuntimeError::InvalidInput(message) if message.contains("unknown entity kind"))
    );

    let mut known = fixture.entity();
    known.id = Uuid::from_u128(0x4767);
    for (entity, policy) in [
        (unknown, ImportKindPolicy::PreserveUnknown),
        (known, ImportKindPolicy::Strict),
    ] {
        let plan =
            prepare_import_entity_record(&fixture.runtime, &fixture.token, entity.clone(), policy)
                .expect("accepted archive kind and subtype remain verbatim");
        fixture.commit(plan).await;
        assert_eq!(
            serde_json::to_value(fixture.stored(entity.id).await.expect("imported entity"))
                .expect("serialize stored entity"),
            serde_json::to_value(entity).expect("serialize archive entity")
        );
    }
}

#[tokio::test]
async fn import_entity_prepare_refuses_non_archive_fields_and_reserved_input() {
    type EntityMutationCase = (&'static str, fn(&mut Entity));

    let fixture = Fixture::new();
    let cases: [EntityMutationCase; 10] = [
        ("namespace", |entity| entity.namespace = "other".into()),
        ("deleted_at", |entity| entity.deleted_at = Some(1)),
        ("merged_into", |entity| {
            entity.merged_into = Some(Uuid::nil())
        }),
        ("merge_event_id", |entity| {
            entity.merge_event_id = Some(Uuid::nil())
        }),
        ("version", |entity| entity.version = 2),
        ("content_ref", |entity| {
            entity.content_ref = Some("attachment-projection".into())
        }),
        ("kind", |entity| entity.kind = "  ".into()),
        ("name", |entity| entity.name = "  ".into()),
        ("registry tags", |entity| {
            entity.tags = vec!["ToOl-ReGiStRy".into()]
        }),
        ("reserved property", |entity| {
            entity.properties = Some(json!({"khive:secret_gate": "caller-supplied"}))
        }),
    ];
    for (field, mutate) in cases {
        let mut entity = fixture.entity();
        mutate(&mut entity);
        let error = prepare_import_entity_record(
            &fixture.runtime,
            &fixture.token,
            entity,
            ImportKindPolicy::PreserveUnknown,
        )
        .expect_err(field);
        assert!(
            matches!(error, RuntimeError::InvalidInput(_)),
            "{field}: {error:?}"
        );
    }
    assert!(fixture.stored(fixture.entity().id).await.is_none());
}

#[tokio::test]
async fn import_entity_prepare_checks_all_content_fields() {
    let fixture = Fixture::new();
    // Same credential-shaped value used by the existing archive edge-property control.
    let credential = "AKIAFAKEKEY1234567890";
    for field in ["name", "description", "properties", "tags"] {
        let mut entity = fixture.entity();
        match field {
            "name" => entity.name = credential.into(),
            "description" => entity.description = Some(credential.into()),
            "properties" => entity.properties = Some(json!({"api_key": credential})),
            "tags" => entity.tags = vec![credential.into()],
            _ => unreachable!(),
        }
        let error = prepare_import_entity_record(
            &fixture.runtime,
            &fixture.token,
            entity,
            ImportKindPolicy::PreserveUnknown,
        )
        .expect_err(field);
        assert!(
            matches!(error, RuntimeError::SecretDetected(_)),
            "{field}: {error:?}"
        );
    }
    assert!(fixture.stored(fixture.entity().id).await.is_none());
}

#[tokio::test]
async fn import_update_projection_matches_the_bound_canonical_plan() {
    let fixture = Fixture::new();
    let original = fixture.entity();
    fixture
        .runtime
        .entities(&fixture.token)
        .expect("entity store")
        .upsert_entity(original.clone())
        .await
        .expect("seed entity");
    let (projected, plan) = prepare_update_entity_record(
        &fixture.runtime,
        &fixture.token,
        original.id,
        crate::curation::EntityPatch {
            name: Some("ImportedUpdate".into()),
            description: None,
            properties: Some(json!({"nested": {"replacement": 2}, "added": true})),
            tags: Some(vec!["replacement-tag".into()]),
            entity_type: None,
        },
        Some(original.version),
    )
    .await
    .expect("prepare canonical update and its record once");
    assert_eq!(projected.id, original.id);
    assert_eq!(projected.namespace, original.namespace);
    assert_eq!(projected.kind, original.kind);
    assert_eq!(projected.created_at, original.created_at);
    assert_eq!(projected.entity_type, original.entity_type);
    assert_eq!(projected.description, original.description);
    assert_eq!(projected.version, original.version);
    assert_eq!(projected.name, "ImportedUpdate");
    assert_eq!(projected.tags, vec!["replacement-tag".to_string()]);
    assert_eq!(
        projected.properties,
        Some(json!({"nested": {"replacement": 2}, "source": "archive", "added": true})),
        "the canonical patch merges top-level keys and replaces the nested value"
    );
    assert_eq!(
        fixture
            .stored(original.id)
            .await
            .expect("unmodified row")
            .name,
        original.name
    );

    let effects = fixture.commit(plan).await;
    let mut expected = projected;
    expected.version += 1;
    assert_eq!(
        serde_json::to_value(fixture.stored(original.id).await.expect("updated row"))
            .expect("serialize stored row"),
        serde_json::to_value(expected).expect("serialize projected row")
    );
    assert_eq!(
        effects.as_slice(),
        &[PostCommitEffect::ReindexEntity {
            entity_id: original.id
        }]
    );
}
