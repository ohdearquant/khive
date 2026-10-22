//! Refused target aliases use the same credential masking as other diagnostics.
use khive_pack_brain::BrainPack;
use khive_runtime::{
    KhiveRuntime, Namespace, NamespaceToken, PackRuntime, RuntimeConfig, RuntimeError,
    VerbRegistry, VerbRegistryBuilder,
};
use khive_storage::{event::EventFilter, types::PageRequest};
use khive_types::EventKind;
use serde_json::{json, Value};

struct Fixture {
    runtime: KhiveRuntime,
    brain: BrainPack,
    registry: VerbRegistry,
    token: NamespaceToken,
}

impl Fixture {
    async fn new() -> Self {
        let runtime = KhiveRuntime::new(RuntimeConfig {
            db_path: None,
            packs: vec!["kg".into()],
            actor_id: Some("brain-refusal-test".into()),
            ..RuntimeConfig::no_embeddings()
        })
        .expect("memory runtime");
        let brain = BrainPack::new(runtime.clone());
        let token = runtime.authorize(Namespace::local()).unwrap();
        let mut builder = VerbRegistryBuilder::new();
        builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
        let registry = builder.build().unwrap();
        brain
            .dispatch("brain.profiles", json!({}), &registry, &token)
            .await
            .expect("initialize profile state before observing refusals");
        Self {
            runtime,
            brain,
            registry,
            token,
        }
    }

    async fn dispatch(&self, results: Value, target: &str) -> Result<Value, RuntimeError> {
        self.brain
            .dispatch(
                "brain.auto_feedback",
                json!({"query":"target refusal regression", "results":results,
                       "target_id":target, "signal":"unjudged"}),
                &self.registry,
                &self.token,
            )
            .await
    }

    async fn events(&self) -> Vec<khive_storage::event::Event> {
        self.runtime
            .events(&self.token)
            .unwrap()
            .query_events(
                EventFilter {
                    kinds: vec![EventKind::FeedbackUnjudged, EventKind::FeedbackExplicit],
                    ..Default::default()
                },
                PageRequest {
                    limit: 100,
                    offset: 0,
                },
            )
            .await
            .unwrap()
            .items
    }

    async fn refused(&self, results: Value, target: &str) -> String {
        let before = json!(self.brain.snapshot());
        let error = self
            .dispatch(results, target)
            .await
            .expect_err("target must refuse");
        assert_eq!(
            json!(self.brain.snapshot()),
            before,
            "refusal must not train"
        );
        assert!(
            self.events().await.is_empty(),
            "refusal must not emit feedback"
        );
        let RuntimeError::InvalidInput(message) = error else {
            panic!("expected InvalidInput, got {error:?}");
        };
        message
    }
}

fn credentials() -> [String; 2] {
    // Synthetic fixtures, assembled to keep repository secret scanning meaningful.
    [
        format!("AKIA{}", "A1".repeat(8)),
        format!("ghp_{}", "xY7z".repeat(9)),
    ]
}

fn assert_masked(message: &str, raw: &str, reason: &str) {
    assert!(
        !message.contains(raw),
        "credential escaped target refusal: {message}"
    );
    assert!(
        message.contains("***MASKED***"),
        "missing redaction marker: {message}"
    );
    assert!(
        message.contains(reason),
        "refusal reason changed: {message}"
    );
}

#[tokio::test]
async fn credential_unmatched_target_is_masked() {
    let f = Fixture::new().await;
    for secret in credentials() {
        let message = f.refused(json!([{"id":"known-alias"}]), &secret).await;
        assert_masked(
            &message,
            &secret,
            "does not match any results[].id or results[].full_id",
        );
    }
    let message = f
        .refused(json!([{"id":"known-alias"}]), "missing-alias")
        .await;
    assert_eq!(
        message,
        "auto_feedback: target_id \"missing-alias\" does not match any results[].id or results[].full_id"
    );
}

#[tokio::test]
async fn credential_ambiguous_target_is_masked() {
    let f = Fixture::new().await;
    for secret in credentials() {
        let message = f
            .refused(
                json!([{"id":secret}, {"id":"other", "full_id":secret}]),
                &secret,
            )
            .await;
        assert_masked(
            &message,
            &secret,
            "matches more than one result; the judged result must be unique",
        );
    }
    let message = f
        .refused(json!([{"id":"duplicate"}, {"id":"duplicate"}]), "duplicate")
        .await;
    assert_eq!(
        message,
        "auto_feedback: target_id \"duplicate\" matches more than one result; the judged result must be unique"
    );
}

#[tokio::test]
async fn credential_malformed_full_id_is_masked() {
    let f = Fixture::new().await;
    for secret in credentials() {
        let message = f
            .refused(
                json!([{"id":"known-alias", "full_id":secret}]),
                "known-alias",
            )
            .await;
        assert_masked(&message, &secret, "expected full UUID");
    }
    let message = f
        .refused(
            json!([{"id":"known-alias", "full_id":"not-a-uuid"}]),
            "known-alias",
        )
        .await;
    assert_eq!(
        message,
        "auto_feedback: invalid full_id \"not-a-uuid\"; expected full UUID"
    );
}

#[tokio::test]
async fn accepted_aliases_still_select_one_result_and_emit() {
    let f = Fixture::new().await;
    let (entity, _) = f
        .runtime
        .create_entity_with_embedding_report(
            &f.token,
            "concept",
            None,
            "feedback-target",
            None,
            None,
            vec![],
        )
        .await
        .unwrap();
    let id = entity.id.to_string();
    for (results, target) in [
        (json!([{"id":"short-alias", "full_id":id}]), "short-alias"),
        (json!([{"id":id, "full_id":id}]), id.as_str()),
    ] {
        let result = f
            .dispatch(results, target)
            .await
            .expect("one selected result");
        assert_eq!(result["emitted"], true);
        assert_eq!(result["target_id"], id);
        assert_eq!(result["signal"], "unjudged");
        assert_eq!(result["result_count"], 1);
    }
    let events = f.events().await;
    assert_eq!(events.len(), 2);
    assert!(events
        .iter()
        .all(|event| event.kind == EventKind::FeedbackUnjudged));
    assert_eq!(f.brain.snapshot().balanced_recall.total_events, 0);
}
