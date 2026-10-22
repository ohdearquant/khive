//! Parameter decoding and feedback-id refusals preserve normal memory behavior.
use khive_pack_memory::MemoryPack;
use khive_runtime::{
    KhiveRuntime, Namespace, NamespaceToken, PackRuntime, RuntimeConfig, RuntimeError,
    VerbRegistry, VerbRegistryBuilder,
};
use khive_storage::{event::EventFilter, note::NoteFilter, types::PageRequest};
use serde_json::{json, Value};

// Ten routes share eight schemas; recall/candidates/fuse share RecallParams.
const SCHEMAS: [(&str, &str); 10] = [
    ("memory.remember", "RememberParams"),
    ("memory.recall", "RecallParams"),
    ("memory.recall_embed", "EmbedParams"),
    ("memory.recall_candidates", "RecallParams"),
    ("memory.recall_fuse", "RecallParams"),
    ("memory.recall_rerank", "RerankParams"),
    ("memory.recall_score", "ScoreParams"),
    ("memory.feedback", "FeedbackParams"),
    ("memory.prune", "PruneParams"),
    ("memory.vacuum", "VacuumParams"),
];

struct Fixture {
    runtime: KhiveRuntime,
    memory: MemoryPack,
    registry: VerbRegistry,
    token: NamespaceToken,
}

impl Fixture {
    fn new() -> Self {
        let runtime = KhiveRuntime::new(RuntimeConfig {
            db_path: None,
            actor_id: Some("memory-parameter-test".into()),
            brain_profile: None,
            ..RuntimeConfig::no_embeddings()
        })
        .unwrap();
        let memory = MemoryPack::new_with_index_role(runtime.clone(), false);
        let token = runtime.authorize(Namespace::local()).unwrap();
        let registry = VerbRegistryBuilder::new().build().unwrap();
        Self {
            runtime,
            memory,
            registry,
            token,
        }
    }

    async fn dispatch(&self, verb: &str, params: Value) -> Result<Value, RuntimeError> {
        self.memory
            .dispatch(verb, params, &self.registry, &self.token)
            .await
    }

    async fn stored(&self) -> Value {
        let page = PageRequest {
            limit: 1000,
            offset: 0,
        };
        let events = self
            .runtime
            .events(&self.token)
            .unwrap()
            .query_events(EventFilter::default(), page.clone())
            .await
            .unwrap();
        let notes = self
            .runtime
            .core()
            .notes(&self.token)
            .unwrap()
            .query_notes_filtered(
                self.token.namespace().as_str(),
                &NoteFilter::default(),
                page,
            )
            .await
            .unwrap();
        assert!(events.items.len() < 1000 && notes.items.len() < 1000);
        json!({"events": events.items, "notes": notes.items})
    }

    async fn refuse(&self, verb: &str, params: Value) -> String {
        let before = self.stored().await;
        let error = self.dispatch(verb, params).await.expect_err("must refuse");
        assert_eq!(self.stored().await, before, "{verb}: stored rows changed");
        let RuntimeError::InvalidInput(message) = error else {
            panic!("{verb}: changed error type: {error:?}");
        };
        message
    }
}

fn credentials() -> [String; 2] {
    [
        format!("AKIA{}", "A1".repeat(8)),
        format!("ghp_{}", "xY7z".repeat(9)),
    ]
}

fn prefix(verb: &str) -> String {
    if matches!(verb, "memory.feedback" | "memory.prune" | "memory.vacuum") {
        format!("{verb}: invalid params: ")
    } else {
        String::new()
    }
}

#[tokio::test]
async fn credential_type_errors_are_masked_across_all_memory_routes() {
    let f = Fixture::new();
    for (verb, schema) in SCHEMAS {
        for secret in credentials() {
            let message = f.refuse(verb, json!(secret)).await;
            assert!(
                !message.contains(&secret),
                "credential escaped parameter refusal: {verb}: {message}"
            );
            assert_eq!(
                message,
                format!(
                    "{}invalid type: string \"***MASKED***\", expected struct {schema}",
                    prefix(verb)
                )
            );
        }
    }
}

#[tokio::test]
async fn credential_unknown_field_names_are_masked_without_changing_schema_policy() {
    let f = Fixture::new();
    for verb in [
        "memory.remember",
        "memory.recall",
        "memory.recall_candidates",
        "memory.recall_fuse",
        "memory.prune",
        "memory.vacuum",
    ] {
        for secret in credentials() {
            let message = f.refuse(verb, json!({secret.clone(): true})).await;
            assert!(
                !message.contains(&secret),
                "credential escaped parameter refusal: {verb}: {message}"
            );
            assert!(
                message.starts_with(&format!("{}unknown field `***MASKED***`", prefix(verb))),
                "{verb}: {message}"
            );
        }
    }
}

#[tokio::test]
async fn credential_feedback_target_is_masked_after_signal_validation() {
    let f = Fixture::new();
    for secret in credentials() {
        let message = f
            .refuse(
                "memory.feedback",
                json!({"target_id": secret, "signal":"useful"}),
            )
            .await;
        assert!(
            !message.contains(&secret),
            "credential escaped feedback target: {message}"
        );
        assert_eq!(message, "memory.feedback: target_id must be a full UUID because a short-prefix resolution can miss or be ambiguous, while feedback must identify one exact recalled record; got \"***MASKED***\"");
        let earlier = f
            .refuse(
                "memory.feedback",
                json!({"target_id":secret,"signal":"bogus"}),
            )
            .await;
        assert!(earlier.starts_with("memory.feedback: invalid signal \"bogus\";"));
        assert!(!earlier.contains("target_id"));
    }
}

#[tokio::test]
async fn ordinary_refusals_keep_exact_prefixes_and_uuid_diagnostics() {
    let f = Fixture::new();
    for (verb, schema) in SCHEMAS {
        assert_eq!(
            f.refuse(verb, json!("ordinary-invalid-params")).await,
            format!(
                "{}invalid type: string \"ordinary-invalid-params\", expected struct {schema}",
                prefix(verb)
            )
        );
    }
    assert_eq!(f.refuse("memory.feedback", json!({"target_id":"not-a-uuid","signal":"useful"})).await,
        "memory.feedback: target_id must be a full UUID because a short-prefix resolution can miss or be ambiguous, while feedback must identify one exact recalled record; got \"not-a-uuid\"");
}

#[tokio::test]
async fn accepted_inputs_retain_writes_feedback_and_permissive_schemas() {
    let f = Fixture::new();
    let created = f.dispatch("memory.remember", json!({"content":"parameter fixture orchard","memory_type":"semantic","namespace":"local"})).await.unwrap();
    let id = created["id"].as_str().unwrap();
    let ignored = credentials()[0].clone();
    let feedback = f
        .dispatch(
            "memory.feedback",
            json!({"target_id":id,"signal":"useful",ignored.clone():true}),
        )
        .await
        .unwrap();
    assert_eq!(
        feedback,
        json!({"ok":true,"target_id":id,"signal":"useful"})
    );
    let before = f.stored().await;
    f.refuse("memory.prune", json!({ignored.clone():true}))
        .await;
    assert_eq!(f.stored().await, before);
    let dry = f
        .dispatch(
            "memory.prune",
            json!({"before":0,"min_salience":0.0,"dry_run":true}),
        )
        .await
        .unwrap();
    assert_eq!(dry["dry_run"], true);
    assert_eq!(f.stored().await, before);
    for (verb, mut args) in [
        ("memory.recall_embed", json!({"query":"orchard"})),
        ("memory.recall_rerank", json!({"candidates":[]})),
        (
            "memory.recall_score",
            json!({"rrf":0.2,"salience":0.5,"decay_factor":0.01,"age_days":1.0}),
        ),
    ] {
        let ordinary = f.dispatch(verb, args.clone()).await.unwrap();
        args[&ignored] = json!(true);
        assert_eq!(
            f.dispatch(verb, args).await.unwrap(),
            ordinary,
            "{verb}: unknown-field acceptance changed"
        );
    }
    let vacuum = f.dispatch("memory.vacuum", json!({})).await.unwrap();
    assert_eq!(vacuum, json!({"ok":true}));
    assert_eq!(f.stored().await["notes"], before["notes"]);
}
