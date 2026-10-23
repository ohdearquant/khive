//! Shared parameter diagnostics preserve each handler's ordinary error text.
use khive_pack_brain::BrainPack;
use khive_runtime::{
    KhiveRuntime, Namespace, NamespaceToken, PackRuntime, RuntimeConfig, RuntimeError,
    VerbRegistry, VerbRegistryBuilder,
};
use khive_storage::{event::EventFilter, types::PageRequest};
use serde_json::{json, Value};

// Sixteen schemas; three lifecycle verbs share one schema.
const SCHEMAS: [(&str, &str); 18] = [
    ("brain.config", "ConfigParams"),
    ("brain.events", "EventsParams"),
    ("brain.event_counts", "EventCountsParams"),
    ("brain.profiles", "ProfilesParams"),
    ("brain.profile", "ProfileParams"),
    ("brain.resolve", "ResolveParams"),
    ("brain.activate", "LifecycleParams"),
    ("brain.deactivate", "LifecycleParams"),
    ("brain.archive", "LifecycleParams"),
    ("brain.reset", "ResetParams"),
    ("brain.feedback", "FeedbackParams"),
    ("brain.record_serve", "RecordServeParams"),
    ("brain.mark_turn", "MarkTurnParams"),
    ("brain.bind", "BindParams"),
    ("brain.unbind", "UnbindParams"),
    ("brain.bindings", "BindingsParams"),
    ("brain.create_profile", "CreateProfileParams"),
    ("brain.register_adapter", "RegisterAdapterParams"),
];

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
            actor_id: Some("brain-parameter-test".into()),
            ..RuntimeConfig::no_embeddings()
        })
        .unwrap();
        let brain = BrainPack::new(runtime.clone());
        let token = runtime.authorize(Namespace::local()).unwrap();
        let registry = VerbRegistryBuilder::new().build().unwrap();
        let f = Self {
            runtime,
            brain,
            registry,
            token,
        };
        f.dispatch("brain.profiles", json!({})).await.unwrap();
        f
    }

    async fn dispatch(&self, verb: &str, params: Value) -> Result<Value, RuntimeError> {
        self.brain
            .dispatch(verb, params, &self.registry, &self.token)
            .await
    }

    async fn durable_events(&self) -> Value {
        let rows = self
            .runtime
            .events(&self.token)
            .unwrap()
            .query_events(
                EventFilter::default(),
                PageRequest {
                    limit: 1000,
                    offset: 0,
                },
            )
            .await
            .unwrap();
        assert!(
            rows.items.len() < 1000,
            "fixture event snapshot must be complete"
        );
        json!(rows.items)
    }

    async fn refuse(&self, verb: &str, params: Value) -> String {
        let before = json!(self.brain.snapshot());
        let events = self.durable_events().await;
        let error = self
            .dispatch(verb, params)
            .await
            .expect_err("schema must refuse");
        assert_eq!(
            json!(self.brain.snapshot()),
            before,
            "{verb}: state changed"
        );
        assert_eq!(
            self.durable_events().await,
            events,
            "{verb}: events changed"
        );
        let RuntimeError::InvalidInput(message) = error else {
            panic!("{verb}: expected InvalidInput, got {error:?}");
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

fn prefix(verb: &str) -> &'static str {
    if verb == "brain.reset" {
        "brain.reset: "
    } else {
        ""
    }
}

#[tokio::test]
async fn credential_type_errors_are_masked_for_every_handler_schema() {
    let f = Fixture::new().await;
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
                ),
                "{verb}"
            );
        }
    }
}

#[tokio::test]
async fn credential_unknown_field_names_are_masked() {
    let f = Fixture::new().await;
    for (verb, _) in SCHEMAS {
        // These schemas intentionally accept unknown fields; no policy is added.
        if matches!(verb, "brain.bindings" | "brain.create_profile") {
            continue;
        }
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
async fn ordinary_type_errors_keep_the_exact_handler_prefix_and_shape() {
    let f = Fixture::new().await;
    for (verb, schema) in SCHEMAS {
        let message = f.refuse(verb, json!("ordinary-invalid-params")).await;
        assert_eq!(
            message,
            format!(
                "{}invalid type: string \"ordinary-invalid-params\", expected struct {schema}",
                prefix(verb)
            ),
            "{verb}"
        );
    }
}

#[tokio::test]
async fn accepted_profile_inputs_keep_alias_defaults_and_lifecycle_behavior() {
    let f = Fixture::new().await;
    let ignored_key = credentials()[0].clone();
    let before_bindings = f.dispatch("brain.bindings", json!({})).await.unwrap();
    let with_unknown = f
        .dispatch("brain.bindings", json!({ignored_key.clone(): true}))
        .await
        .unwrap();
    assert_eq!(with_unknown, before_bindings);
    f.dispatch(
        "brain.create_profile",
        json!({"name":"parameter-profile", ignored_key: true}),
    )
    .await
    .unwrap();
    for params in [
        json!({"profile_id":"parameter-profile"}),
        json!({"id":"parameter-profile"}),
    ] {
        let result = f.dispatch("brain.profile", params).await.unwrap();
        assert_eq!(result["id"], "parameter-profile");
    }
    let all = f.dispatch("brain.profiles", json!({})).await.unwrap();
    assert!(all["count"].as_u64().unwrap() > 0);
    f.dispatch("brain.reset", json!({"profile_id":"parameter-profile"}))
        .await
        .unwrap();
    f.dispatch("brain.activate", json!({"profile_id":"parameter-profile"}))
        .await
        .unwrap();
    let active = f
        .dispatch("brain.profiles", json!({"lifecycle":"active"}))
        .await
        .unwrap();
    assert!(active["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p["id"] == "parameter-profile"));
    f.dispatch(
        "brain.deactivate",
        json!({"profile_id":"parameter-profile"}),
    )
    .await
    .unwrap();
    f.dispatch("brain.archive", json!({"profile_id":"parameter-profile"}))
        .await
        .unwrap();
}
