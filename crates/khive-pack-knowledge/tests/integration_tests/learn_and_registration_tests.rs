// ── pack metadata ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn pack_registers_cleanly_with_verb_registry() {
    let f = pack(rt());
    let verbs: Vec<&str> = f.registry.all_verbs().iter().map(|v| v.name).collect();
    assert!(
        verbs.contains(&"knowledge.learn"),
        "expected 'learn' verb, got: {verbs:?}"
    );
    assert!(
        verbs.contains(&"knowledge.cite"),
        "expected 'cite' verb, got: {verbs:?}"
    );
    assert!(
        verbs.contains(&"knowledge.topic"),
        "expected 'topic' verb, got: {verbs:?}"
    );
    // Corpus-tier verbs must also be registered.
    assert!(
        verbs.contains(&"knowledge.upsert_atoms"),
        "expected 'knowledge.upsert_atoms' verb, got: {verbs:?}"
    );
    assert!(
        verbs.contains(&"knowledge.search"),
        "expected 'knowledge.search' verb, got: {verbs:?}"
    );
    assert!(
        verbs.contains(&"knowledge.fold"),
        "expected 'knowledge.fold' verb, got: {verbs:?}"
    );
    assert!(
        verbs.contains(&"knowledge.suggest"),
        "expected 'knowledge.suggest' verb, got: {verbs:?}"
    );
    assert!(
        verbs.contains(&"knowledge.compose"),
        "expected 'knowledge.compose' verb, got: {verbs:?}"
    );
    // No note kinds added.
    let note_kinds: Vec<&str> = f.registry.all_note_kinds();
    assert!(
        !note_kinds.contains(&"knowledge"),
        "knowledge pack should not add note kinds"
    );
}

struct LearnWarningEmbeddingService;

#[async_trait::async_trait]
impl lattice_embed::EmbeddingService for LearnWarningEmbeddingService {
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
        "learn-warning-test"
    }
}

struct LearnWarningEmbedderProvider;

#[async_trait::async_trait]
impl khive_runtime::EmbedderProvider for LearnWarningEmbedderProvider {
    fn name(&self) -> &str {
        "learn-warning-test"
    }

    fn dimensions(&self) -> usize {
        1
    }

    async fn build(&self) -> Result<Arc<dyn lattice_embed::EmbeddingService>, RuntimeError> {
        Ok(Arc::new(LearnWarningEmbeddingService))
    }
}

#[tokio::test]
async fn learn_reports_embedding_truncation() {
    let runtime = rt();
    runtime.register_embedder(LearnWarningEmbedderProvider);
    let f = pack(runtime);
    let description = "x".repeat(lattice_embed::MAX_TEXT_BYTES + 1);

    let response = f
        .dispatch(
            "knowledge.learn",
            json!({"name": "long description", "description": description}),
        )
        .await
        .expect("learn with an over-limit description");

    assert_eq!(
        response["warnings"],
        json!([khive_runtime::retrieval::EMBEDDING_INPUT_TRUNCATED_WARNING]),
        "learn must disclose a truncated embedding input: {response}"
    );
    assert_eq!(response["description"].as_str(), Some(description.as_str()));
}

// ── learn verb ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn learn_creates_concept_with_name_and_domain() {
    let f = pack(rt());
    let resp = f
        .dispatch(
            "knowledge.learn",
            json!({
                "name": "LoRA",
                "description": "Low-Rank Adaptation of large language models — covering concepts techniques algorithms implementations applications use cases and design patterns in detail",
                "domain": "fine-tuning",
                "tags": ["adapter"]
            }),
        )
        .await
        .expect("learn ok");

    assert_eq!(resp["kind"], "concept");
    assert_eq!(resp["name"], "LoRA");
    assert_eq!(resp["domain"], "fine-tuning");
    // Domain is promoted to tags.
    let tags = resp["tags"].as_array().expect("tags array");
    let tag_strs: Vec<&str> = tags.iter().filter_map(|v| v.as_str()).collect();
    assert!(
        tag_strs.contains(&"fine-tuning"),
        "domain not in tags: {tag_strs:?}"
    );
    assert!(tag_strs.contains(&"adapter"));
    // Response shape: short id (8 chars) + full UUID.
    let id = resp["id"].as_str().expect("id");
    let full_id = resp["full_id"].as_str().expect("full_id");
    assert_eq!(id.len(), 8, "expected 8-char short id, got: {id}");
    assert!(
        full_id.contains('-'),
        "expected UUID in full_id, got: {full_id}"
    );
}

#[tokio::test]
async fn learn_creates_concept_without_domain() {
    let f = pack(rt());
    let resp = f
        .dispatch("knowledge.learn", json!({ "name": "FlashAttention" }))
        .await
        .expect("learn ok");

    assert_eq!(resp["kind"], "concept");
    assert_eq!(resp["name"], "FlashAttention");
    assert!(resp["domain"].is_null());
}

#[tokio::test]
async fn learn_rejects_empty_name() {
    let f = pack(rt());
    let err = f
        .dispatch("knowledge.learn", json!({ "name": "   " }))
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("name must not be empty"), "got: {msg}");
}

#[tokio::test]
async fn learn_rejects_missing_name_and_content() {
    let f = pack(rt());
    let err = f
        .dispatch("knowledge.learn", json!({ "domain": "attention" }))
        .await
        .unwrap_err();
    let msg = err.to_string();
    // Neither name nor content supplied — handler returns a descriptive error.
    assert!(
        msg.contains("name must not be empty"),
        "expected descriptive error, got: {msg}"
    );
}

// ── learn content-alias ──────────────────────────────────────────────────────

#[tokio::test]
async fn learn_content_without_name_auto_generates_name() {
    let f = pack(rt());
    // Agent-style call: only `content` provided, no explicit `name`.
    let resp = f
        .dispatch(
            "knowledge.learn",
            json!({ "content": "Some long description about X that keeps going and going beyond sixty characters easily dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }),
        )
        .await
        .expect("learn with content only should succeed");

    assert_eq!(resp["kind"], "concept");
    let name = resp["name"].as_str().expect("name present");
    assert!(!name.is_empty(), "auto-generated name must not be empty");
    assert!(
        name.len() <= 60,
        "auto-generated name must be <= 60 chars, got: {name:?}"
    );
    // Description is populated from `content`.
    let desc = resp["description"].as_str().expect("description present");
    assert!(
        desc.contains("Some long description"),
        "description should contain content: {desc:?}"
    );
}

#[tokio::test]
async fn learn_content_alias_maps_to_description() {
    let f = pack(rt());
    // When both `name` and `content` are provided, content becomes the description.
    let resp = f
        .dispatch(
            "knowledge.learn",
            json!({
                "name": "GQA",
                "content": "Grouped-Query Attention mechanism"
            }),
        )
        .await
        .expect("learn with name + content");

    assert_eq!(resp["name"], "GQA");
    assert_eq!(resp["description"], "Grouped-Query Attention mechanism");
}

#[tokio::test]
async fn learn_short_content_uses_full_text_as_name() {
    let f = pack(rt());
    let resp = f
        .dispatch(
            "knowledge.learn",
            json!({ "content": "Speculative Decoding" }),
        )
        .await
        .expect("learn short content");

    assert_eq!(resp["name"], "Speculative Decoding");
    assert_eq!(resp["description"], "Speculative Decoding");
}
