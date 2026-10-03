//! A scheduled event whose text exceeds the embedder input budget must still
//! be activated: the note commits with a truncated embedding, and the verb
//! response discloses the truncation instead of failing after the commit.

use khive_pack_schedule::SchedulePack;
use khive_runtime::{KhiveRuntime, VerbRegistry, VerbRegistryBuilder};

mod support;

struct TruncationEmbeddingService;

#[async_trait::async_trait]
impl lattice_embed::EmbeddingService for TruncationEmbeddingService {
    async fn embed(
        &self,
        texts: &[String],
        _model: lattice_embed::EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, lattice_embed::EmbedError> {
        Ok(vec![vec![1.0]; texts.len()])
    }

    fn supports_model(&self, _model: lattice_embed::EmbeddingModel) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "schedule-truncation-test"
    }
}

struct TruncationEmbedderProvider;

#[async_trait::async_trait]
impl khive_runtime::EmbedderProvider for TruncationEmbedderProvider {
    fn name(&self) -> &str {
        "schedule-truncation-test"
    }

    fn dimensions(&self) -> usize {
        1
    }

    async fn build(
        &self,
    ) -> Result<std::sync::Arc<dyn lattice_embed::EmbeddingService>, khive_runtime::RuntimeError>
    {
        Ok(std::sync::Arc::new(TruncationEmbeddingService))
    }
}

fn build_registry() -> (VerbRegistry, KhiveRuntime) {
    let runtime = support::memory_runtime();
    runtime.register_embedder(TruncationEmbedderProvider);
    let mut builder = VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(khive_pack_comm::CommPack::new(runtime.clone()));
    builder.register(SchedulePack::new(runtime.clone()));
    let registry = builder.build().expect("registry builds");
    (registry, runtime)
}

async fn assert_activated_with_warning(
    runtime: &KhiveRuntime,
    verb: &str,
    response: &serde_json::Value,
) {
    assert_eq!(response["status"], "pending", "{verb}: {response}");
    assert_eq!(
        response["warnings"],
        serde_json::json!([khive_runtime::retrieval::EMBEDDING_INPUT_TRUNCATED_WARNING]),
        "{verb} must disclose the truncated embedding input: {response}"
    );
    let token = runtime
        .authorize(khive_runtime::Namespace::local())
        .expect("local token");
    let stored = runtime
        .notes(&token)
        .expect("note store")
        .get_note(
            response["full_id"]
                .as_str()
                .expect("full_id")
                .parse()
                .expect("full UUID"),
        )
        .await
        .expect("read event")
        .expect("event exists");
    assert_eq!(
        stored.properties.expect("event properties")["status"],
        "pending",
        "{verb}: the committed event must not stay provisioning"
    );
}

#[tokio::test]
async fn remind_over_embedding_budget_activates_and_discloses_truncation() {
    let (registry, runtime) = build_registry();
    let response = registry
        .dispatch(
            "schedule.remind",
            serde_json::json!({
                "content": "x".repeat(lattice_embed::MAX_TEXT_BYTES + 1),
                "at": "2099-06-01T09:00:00Z"
            }),
        )
        .await
        .expect("an over-budget reminder must not fail after commit");
    assert_activated_with_warning(&runtime, "schedule.remind", &response).await;
}

#[tokio::test]
async fn schedule_over_embedding_budget_activates_and_discloses_truncation() {
    let (registry, runtime) = build_registry();
    let action = format!(
        "create(kind=\"concept\", name=\"{}\")",
        "x".repeat(lattice_embed::MAX_TEXT_BYTES)
    );
    let response = registry
        .dispatch(
            "schedule.schedule",
            serde_json::json!({"action": action, "at": "2099-06-01T10:00:00Z"}),
        )
        .await
        .expect("an over-budget scheduled action must not fail after commit");
    assert_activated_with_warning(&runtime, "schedule.schedule", &response).await;
}
