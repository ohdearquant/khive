use khive_runtime::{KhiveRuntime, Namespace};
use serde_json::json;

use super::handle_store;

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
        "session-truncation-test"
    }
}

struct TruncationEmbedderProvider;

#[async_trait::async_trait]
impl khive_runtime::EmbedderProvider for TruncationEmbedderProvider {
    fn name(&self) -> &str {
        "session-truncation-test"
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

#[tokio::test]
async fn store_over_embedding_budget_keeps_the_session_and_discloses_truncation() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    rt.register_embedder(TruncationEmbedderProvider);
    let token = rt.authorize(Namespace::local()).expect("authorize local");
    let content = "x".repeat(lattice_embed::MAX_TEXT_BYTES + 1);

    let response = handle_store(&rt, &token, json!({ "content": content }))
        .await
        .expect("an over-budget session must not fail after it is committed");

    assert_eq!(
        response["warnings"],
        json!([khive_runtime::retrieval::EMBEDDING_INPUT_TRUNCATED_WARNING]),
        "store must disclose the truncated embedding input: {response}"
    );
    let id = uuid::Uuid::parse_str(response["session"]["id"].as_str().expect("session id"))
        .expect("session id is a uuid");
    let stored = rt
        .core()
        .notes(&token)
        .expect("notes store")
        .get_note(id)
        .await
        .expect("read the stored session")
        .expect("the session is stored");
    assert_eq!(stored.content.len(), lattice_embed::MAX_TEXT_BYTES + 1);
}

#[tokio::test]
async fn store_within_embedding_budget_has_no_warning() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    rt.register_embedder(TruncationEmbedderProvider);
    let token = rt.authorize(Namespace::local()).expect("authorize local");

    let response = handle_store(&rt, &token, json!({ "content": "short session" }))
        .await
        .expect("store a session");

    assert!(
        response.get("warnings").is_none(),
        "no warning expected: {response}"
    );
}

#[tokio::test]
async fn empty_content_rejected() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let token = rt.authorize(Namespace::local()).expect("authorize local");

    let err = handle_store(&rt, &token, json!({ "content": "" }))
        .await
        .unwrap_err();

    let khive_runtime::RuntimeError::InvalidInput(msg) = err else {
        panic!("expected InvalidInput, got {err:?}");
    };
    assert!(
        msg.contains("content must not be empty"),
        "error must name the empty-content violation; got: {msg}",
    );
}

#[tokio::test]
async fn blank_title_rejected() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let token = rt.authorize(Namespace::local()).expect("authorize local");

    let err = handle_store(&rt, &token, json!({ "content": "hello", "title": "" }))
        .await
        .unwrap_err();

    let khive_runtime::RuntimeError::InvalidInput(msg) = err else {
        panic!("expected InvalidInput, got {err:?}");
    };
    assert!(
        msg.contains("title must be a non-empty string when provided"),
        "error must name the blank-title violation; got: {msg}",
    );
}

#[tokio::test]
async fn blank_provider_rejected() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let token = rt.authorize(Namespace::local()).expect("authorize local");

    let err = handle_store(&rt, &token, json!({ "content": "hello", "provider": "" }))
        .await
        .unwrap_err();

    let khive_runtime::RuntimeError::InvalidInput(msg) = err else {
        panic!("expected InvalidInput, got {err:?}");
    };
    assert!(
        msg.contains("provider must be a non-empty string when provided"),
        "error must name the blank-provider violation; got: {msg}",
    );
}

#[tokio::test]
async fn blank_provider_session_id_rejected() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let token = rt.authorize(Namespace::local()).expect("authorize local");

    let err = handle_store(
        &rt,
        &token,
        json!({ "content": "hello", "provider_session_id": "" }),
    )
    .await
    .unwrap_err();

    let khive_runtime::RuntimeError::InvalidInput(msg) = err else {
        panic!("expected InvalidInput, got {err:?}");
    };
    assert!(
        msg.contains("provider_session_id must be a non-empty string when provided"),
        "error must name the blank-provider_session_id violation; got: {msg}",
    );
}

#[tokio::test]
async fn empty_tag_entry_rejected() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let token = rt.authorize(Namespace::local()).expect("authorize local");

    let err = handle_store(
        &rt,
        &token,
        json!({ "content": "hello", "tags": ["good", ""] }),
    )
    .await
    .unwrap_err();

    let khive_runtime::RuntimeError::InvalidInput(msg) = err else {
        panic!("expected InvalidInput, got {err:?}");
    };
    assert!(
        msg.contains("tags entries must be non-empty strings"),
        "error must name the empty-tag-entry violation; got: {msg}",
    );
}
