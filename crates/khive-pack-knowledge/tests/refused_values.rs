use khive_pack_kg::KgPack;
use khive_pack_knowledge::KnowledgePack;
use khive_runtime::{
    runtime_error_value, DomainDisposition, KhiveRuntime, Namespace, RuntimeConfig, RuntimeError,
    VerbRegistry, VerbRegistryBuilder,
};
use khive_storage::EventFilter;
use serde_json::{json, Value};

const SIGNALS: &str = "useful | not_useful | wrong";
const SECTIONS: &str = "overview, core_model, boundary_conditions, formalism, operational_guidance, examples, failure_modes, expert_lens";

struct Fixture {
    runtime: KhiveRuntime,
    registry: VerbRegistry,
    atom_id: String,
}

impl Fixture {
    async fn new() -> Self {
        let runtime = KhiveRuntime::new(RuntimeConfig {
            db_path: None,
            actor_id: Some("knowledge-refusal-fixture".into()),
            brain_profile: None,
            ..RuntimeConfig::no_embeddings()
        })
        .expect("private memory runtime");
        assert!(runtime.backend().pool().canonical_path().is_none());
        assert!(runtime.default_embedder_name().is_empty());
        let mut builder = VerbRegistryBuilder::new();
        builder.with_actor_id(Some("knowledge-refusal-fixture".into()));
        builder.with_default_namespace("local");
        builder.register(KgPack::new(runtime.clone()));
        builder.register(KnowledgePack::new_with_index_role(runtime.clone(), false));
        let registry = builder.build().expect("registry");
        registry
            .apply_schema_plans_with_map(&Default::default(), runtime.backend())
            .expect("knowledge schema");
        registry
            .dispatch(
                "knowledge.upsert_atoms",
                json!({"atoms":[{
                    "slug":"feedback-refusal-orchard",
                    "name":"Feedback refusal orchard",
                    "content":"dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity"
                }]}),
            )
            .await
            .expect("real feedback target");
        let atom = registry
            .dispatch("knowledge.get", json!({"id":"feedback-refusal-orchard"}))
            .await
            .expect("stored atom");
        assert_eq!(atom["kind"], "atom");
        Self {
            runtime,
            registry,
            atom_id: atom["id"].as_str().expect("atom id").to_owned(),
        }
    }

    async fn event_count(&self) -> u64 {
        let token = self.runtime.authorize(Namespace::local()).unwrap();
        self.runtime
            .events(&token)
            .unwrap()
            .count_events(EventFilter {
                verbs: vec!["knowledge.feedback".into()],
                ..Default::default()
            })
            .await
            .unwrap()
    }

    async fn refuse(&self, args: Value, raw: &str, expected: String) {
        let before = self.event_count().await;
        let error = self
            .registry
            .dispatch("knowledge.feedback", args)
            .await
            .expect_err("closed-set refusal");
        let RuntimeError::InvalidInput(message) = &error else {
            panic!("feedback changed its refusal variant");
        };
        if raw != "bogus" {
            assert!(
                !message.contains(raw),
                "feedback exposed the synthetic credential"
            );
            assert!(
                message.contains("***MASKED***"),
                "feedback omitted the mask"
            );
        }
        assert_eq!(message, &expected);
        assert_eq!(
            runtime_error_value(error, DomainDisposition::NotCommitted),
            json!({
                "kind":"runtime_error",
                "message":format!("invalid input: {expected}"),
                "domain_disposition":"not_committed"
            })
        );
        assert_eq!(self.event_count().await, before, "refusal wrote feedback");
    }

    async fn accept(&self, args: Value) -> (Value, Value) {
        let before = self.event_count().await;
        let result = self
            .registry
            .dispatch("knowledge.feedback", args)
            .await
            .expect("accepted feedback");
        assert_eq!(result["ok"], true);
        assert_eq!(result["emitted"], true);
        assert_eq!(result["tier"], "namespace_local");
        assert_eq!(result["target_id"], self.atom_id);
        assert_eq!(result["target_kind"], "atom");
        assert_eq!(result["target_id_used"], true);
        assert!(result["brain_profile"].is_null());
        assert!(result["profile_event_id"].is_null());
        assert_eq!(self.event_count().await, before + 1);
        let token = self.runtime.authorize(Namespace::local()).unwrap();
        let event = self
            .runtime
            .events(&token)
            .unwrap()
            .get_event(result["event_id"].as_str().unwrap().parse().unwrap())
            .await
            .unwrap()
            .expect("persisted feedback event");
        assert_eq!(event.verb, "knowledge.feedback");
        assert_eq!(event.target_id.unwrap().to_string(), self.atom_id);
        assert_eq!(event.payload["target_id"], self.atom_id);
        (result, event.payload)
    }
}

fn refused_values() -> [(String, &'static str); 2] {
    [
        ("bogus".into(), "bogus"),
        (format!("sk-proj-{}", "A".repeat(80)), "***MASKED***"),
    ]
}

#[tokio::test]
async fn scalar_signal_refusals_are_masked_and_valid_feedback_is_persisted() {
    let fixture = Fixture::new().await;
    for (raw, shown) in refused_values() {
        fixture
            .refuse(
                json!({"target_id":fixture.atom_id,"signal":raw}),
                &raw,
                format!("unknown feedback signal {shown:?}; expected {SIGNALS}"),
            )
            .await;
    }
    for signal in ["useful", "not_useful", "wrong"] {
        let (result, event) = fixture
            .accept(json!({"target_id":fixture.atom_id,"signal":signal}))
            .await;
        assert_eq!(result["signal"], signal);
        assert_eq!(result["signals_applied"], 0);
        assert!(result["total_events"].is_null());
        assert_eq!(event["signal"], signal);
        assert!(event.get("section_signals").is_none());
    }
}

#[tokio::test]
async fn section_signal_refusals_are_masked_without_advancing_learning() {
    let fixture = Fixture::new().await;
    for (raw, shown) in refused_values() {
        fixture
            .refuse(
                json!({"target_id":fixture.atom_id,"section_signals":{"overview":raw}}),
                &raw,
                format!("unknown feedback signal {shown:?}; expected {SIGNALS}"),
            )
            .await;
    }
    for (index, signal) in ["useful", "not_useful", "wrong"].into_iter().enumerate() {
        let (result, event) = fixture
            .accept(json!({"target_id":fixture.atom_id,"section_signals":{"overview":signal}}))
            .await;
        assert!(result["signal"].is_null());
        assert_eq!(result["signals_applied"], 1);
        assert_eq!(result["total_events"], index + 1);
        assert!(event["signal"].is_null());
        assert_eq!(event["section_signals"], json!({"overview":signal}));
    }
}

#[tokio::test]
async fn section_key_refusals_are_masked_and_existing_aliases_still_normalize() {
    let fixture = Fixture::new().await;
    for (raw, shown) in refused_values() {
        fixture
            .refuse(
                json!({"target_id":fixture.atom_id,"section_signals":{raw.clone():"useful"}}),
                &raw,
                format!("unknown section_type: {shown:?}; valid: {SECTIONS}"),
            )
            .await;
    }
    for (index, (input, canonical)) in [
        ("overview", "overview"),
        (" INTRO ", "overview"),
        ("Architecture", "core_model"),
    ]
    .into_iter()
    .enumerate()
    {
        let (result, event) = fixture
            .accept(json!({"target_id":fixture.atom_id,"section_signals":{input:"useful"}}))
            .await;
        assert_eq!(result["signals_applied"], 1);
        assert_eq!(result["total_events"], index + 1);
        assert_eq!(event["section_signals"], json!({canonical:"useful"}));
    }
}
