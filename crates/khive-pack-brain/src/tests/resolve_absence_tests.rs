use super::*;
use khive_brain_core::profile::{resolve_consumer_profile, ConsumerKind};
use khive_runtime::VerbRegistry;
use khive_storage::event::EventFilter;
use khive_storage::types::PageRequest;
use khive_types::{EventKind, EventOutcome};

fn audited_surface() -> (KhiveRuntime, VerbRegistry, NamespaceToken) {
    let (pack, runtime) = make_pack_with_actor("resolve-absence");
    let token = runtime.authorize(Namespace::local()).unwrap();
    let mut builder = VerbRegistryBuilder::new();
    builder.with_actor_id(runtime.config().actor_id.clone());
    builder.with_runtime_event_store(&runtime).unwrap();
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(TestConsumerPack);
    builder.register(pack);
    let registry = builder.build().unwrap();
    assert!(registry.audit_batch_handle().is_some());
    (runtime, registry, token)
}

async fn resolve_audit_outcomes(
    runtime: &KhiveRuntime,
    registry: &VerbRegistry,
    token: &NamespaceToken,
) -> Vec<EventOutcome> {
    registry.shutdown_audit_batch().await.unwrap();
    let page = runtime
        .events(token)
        .unwrap()
        .query_events(
            EventFilter {
                kinds: vec![EventKind::Audit],
                verbs: vec!["brain.resolve".into()],
                ..Default::default()
            },
            PageRequest {
                offset: 0,
                limit: 100,
            },
        )
        .await
        .unwrap();
    page.items.into_iter().map(|event| event.outcome).collect()
}

fn absent_profile(consumer_kind: &str) -> Value {
    json!({
        "resolved_profile_id": null,
        "lifecycle": null,
        "requested_consumer_kind": consumer_kind,
        "matched_consumer_kind": null,
        "matched_binding": false,
    })
}

#[tokio::test]
async fn resolve_known_absence_is_success_and_audited_without_error() {
    let (runtime, registry, token) = audited_surface();
    registry
        .dispatch(
            "brain.bind",
            json!({
                "profile_id":"balanced-recall-v1",
                "actor":token.actor().label(),
                "consumer_kind":"recall",
            }),
        )
        .await
        .unwrap();
    let result = registry
        .dispatch(
            "brain.resolve",
            json!({"consumer_kind":"knowledge_compose"}),
        )
        .await
        .expect("no compose binding is a successful tier-3 resolution");
    assert_eq!(result, absent_profile("knowledge_compose"));
    assert_eq!(
        resolve_consumer_profile(&registry, &token, ConsumerKind::KnowledgeCompose).await,
        None,
        "the production consumer helper still selects its pack-local prior"
    );
    assert_eq!(
        resolve_audit_outcomes(&runtime, &registry, &token).await,
        vec![EventOutcome::Success; 2],
        "direct resolve and the real consumer helper each persist one successful audit"
    );
}

#[tokio::test]
async fn resolve_dynamic_registered_consumer_without_profile_succeeds() {
    let (runtime, registry, token) = audited_surface();
    assert!(registry.all_brain_consumer_kinds().contains(&"search"));
    assert!("search".parse::<ConsumerKind>().is_err());
    let result = registry
        .dispatch("brain.resolve", json!({"consumer_kind":"search"}))
        .await
        .expect("loaded consumer vocabulary includes kinds outside ConsumerKind");
    assert_eq!(result, absent_profile("search"));
    assert_eq!(
        resolve_audit_outcomes(&runtime, &registry, &token).await,
        vec![EventOutcome::Success]
    );
}

#[tokio::test]
async fn resolve_matched_and_fallback_responses_remain_unchanged() {
    let (runtime, registry, token) = audited_surface();
    let fallback = registry
        .dispatch("brain.resolve", json!({"consumer_kind":"recall"}))
        .await
        .unwrap();
    assert_eq!(
        fallback,
        json!({
            "resolved_profile_id":"balanced-recall-v1", "lifecycle":"active",
            "requested_consumer_kind":"recall", "matched_consumer_kind":"recall",
            "matched_binding":false,
        })
    );
    registry
        .dispatch(
            "brain.bind",
            json!({
                "profile_id":"balanced-recall-v1", "actor":token.actor().label(),
                "consumer_kind":"recall",
            }),
        )
        .await
        .unwrap();
    let matched = registry
        .dispatch("brain.resolve", json!({"consumer_kind":"recall"}))
        .await
        .unwrap();
    assert_eq!(
        matched,
        json!({
            "resolved_profile_id":"balanced-recall-v1", "lifecycle":"active",
            "requested_consumer_kind":"recall", "matched_consumer_kind":"recall",
            "matched_binding":true,
        })
    );
    registry
        .dispatch(
            "brain.bind",
            json!({
                "profile_id":"balanced-recall-v1", "actor":token.actor().label(),
                "consumer_kind":"*",
            }),
        )
        .await
        .unwrap();
    // Existing wildcard matches precede any no-match vocabulary check.
    let wildcard = registry
        .dispatch(
            "brain.resolve",
            json!({"consumer_kind":"wildcard-only-consumer"}),
        )
        .await
        .unwrap();
    assert_eq!(
        wildcard,
        json!({
            "resolved_profile_id":"balanced-recall-v1", "lifecycle":"active",
            "requested_consumer_kind":"wildcard-only-consumer", "matched_consumer_kind":"*",
            "matched_binding":true,
        })
    );
    assert_eq!(
        resolve_audit_outcomes(&runtime, &registry, &token).await,
        vec![EventOutcome::Success; 3]
    );
}

#[tokio::test]
async fn resolve_unknown_and_malformed_consumers_remain_audited_errors() {
    let (runtime, registry, token) = audited_surface();
    let error = registry
        .dispatch(
            "brain.resolve",
            json!({"consumer_kind":"unregistered-consumer"}),
        )
        .await
        .expect_err("an unmatched unknown consumer still refuses");
    let RuntimeError::NotFound(message) = error else {
        panic!("unknown no-match must retain NotFound: {error:?}");
    };
    assert_eq!(
        message,
        "no profile resolved for consumer_kind=\"unregistered-consumer\""
    );
    let error = registry
        .dispatch("brain.resolve", json!({"consumer_kind":17}))
        .await
        .expect_err("malformed consumer params still refuse");
    assert!(matches!(error, RuntimeError::InvalidInput(_)), "{error:?}");
    assert_eq!(
        resolve_audit_outcomes(&runtime, &registry, &token).await,
        vec![EventOutcome::Error; 2]
    );
}
