use super::*;
use crate::ann;
use khive_pack_kg::KgPack;
use khive_runtime::VerbRegistryBuilder;
use serde_json::json;
use serial_test::serial;
use uuid::Uuid;

#[test]
#[serial_test::serial(config_ledger)]
fn prune_description_publishes_optional_effective_salience_schema() {
    use khive_runtime::{RuntimeConfig, WalCeilingSource};

    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        embedding_model: None,
        additional_embedding_models: Vec::new(),
        wal_ceiling_bytes: 0,
        wal_ceiling_configured_bytes: 0,
        wal_ceiling_source: WalCeilingSource::Default,
        wal_ceiling_env_raw: None,
        disk_guard_environment: Default::default(),
        disk_guard_config: None,
        volume_lock_dir: None,
        credentials: Vec::new(),
        visibility_receipts: None,
        mounts: Vec::new(),
        events_split: None,
        actor_id: None,
        brain_profile: None,
        brain: Default::default(),
        packs: vec!["kg".into(), "memory".into()],
        ..RuntimeConfig::default()
    })
    .expect("isolated memory runtime");
    assert!(runtime.config().db_path.is_none());
    assert!(runtime.backend().pool().config().path.is_none());
    assert!(runtime.backend_data_dir().is_none());
    assert!(runtime.backend_ann_root().is_none());
    assert!(runtime.registered_embedding_model_names().is_empty());

    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime.clone()));
    builder.register(MemoryPack::new(runtime));
    // This uses the production description renderer without activating packs.
    let registry = builder.build_metadata().expect("metadata registry");
    assert!(registry.has_verb("memory.prune"));
    let description = registry
        .describe_verb("memory.prune")
        .expect("prune description");
    assert_eq!(description["verb"], "memory.prune");
    assert_eq!(description["pack"], "memory");
    assert_eq!(description["category"], "Commissive");

    let params = description["params"].as_array().expect("parameter list");
    assert_eq!(params.len(), 5);
    let schema = &description["input_schema"];
    assert_eq!(schema["type"], "object");
    assert_eq!(schema["additionalProperties"], true);
    assert!(schema["required"]
        .as_array()
        .expect("required list")
        .is_empty());
    let properties = schema["properties"].as_object().expect("schema properties");
    assert_eq!(properties.len(), 6);
    assert_eq!(properties["help"]["type"], "boolean");

    for (name, param_type) in [
        ("min_salience", "number"),
        ("min_effective_salience", "number"),
        ("before", "integer"),
        ("namespace", "string"),
        ("dry_run", "boolean"),
    ] {
        let matching: Vec<_> = params
            .iter()
            .filter(|param| param["name"].as_str() == Some(name))
            .collect();
        assert_eq!(matching.len(), 1, "exactly one declaration for {name}");
        assert_eq!(matching[0]["type"], param_type, "help type for {name}");
        assert_eq!(matching[0]["required"], false, "optional help field {name}");
        assert_eq!(
            properties[name]["type"], param_type,
            "schema type for {name}"
        );
    }
}

const FR1_MODEL: &str = "fr1-mutation-hook-model";

/// Builds a registry with the production note-mutation hook and returns its ANN state.
fn build_note_hook_registry(rt: &KhiveRuntime) -> (khive_runtime::VerbRegistry, SharedAnn) {
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    let memory_pack = MemoryPack::new(rt.clone());
    let ann = memory_pack.ann_for_test();
    builder.register(memory_pack);
    let registry = builder.build().expect("registry builds");
    registry.call_register_note_mutation_hooks(rt);
    (registry, ann)
}

fn mutation_hook_ann_key() -> ann::AnnKey {
    ann::AnnKey::new(FR1_MODEL)
}

/// Seeds one note, warms ANN through recall, and verifies the pre-mutation state.
async fn seed_and_warm_ann(
    rt: &KhiveRuntime,
    registry: &khive_runtime::VerbRegistry,
    ann: &SharedAnn,
    content: &str,
    salience: f64,
) -> Uuid {
    rt.register_embedder(Fr1FixedVecProvider {
        model_name: FR1_MODEL.to_string(),
        vector: [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
    });
    let id = registry
        .dispatch(
            "memory.remember",
            json!({"content": content, "salience": salience}),
        )
        .await
        .expect("seed remember")["id"]
        .as_str()
        .expect("id")
        .parse::<Uuid>()
        .expect("valid uuid");

    registry
        .dispatch(
            "memory.recall",
            json!({
                "query": content,
                "namespace": "local",
                "fusion_strategy": "vector_only",
                "embedding_model": FR1_MODEL,
            }),
        )
        .await
        .expect("warm recall");

    ann::wait_until_warm_idle(ann, &mutation_hook_ann_key()).await;
    assert!(
        ann::is_current(ann, &mutation_hook_ann_key()).await,
        "precondition: completed ANN warm-up must leave the cache current \
             before testing mutation invalidation"
    );
    id
}

#[tokio::test]
#[serial(background_tasks)]
#[serial_test::serial(config_ledger)]
async fn prune_invalidates_warm_ann_without_subsequent_remember() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let (registry, ann) = build_note_hook_registry(&rt);

    seed_and_warm_ann(&rt, &registry, &ann, "fr1 prune target content", 0.9).await;

    // `min_salience: 1.0` is strictly above the seeded note's 0.9
    // salience, so it is the one candidate. No `memory.remember` call
    // follows.
    let pruned = registry
        .dispatch(
            "memory.prune",
            json!({ "min_salience": 1.0, "namespace": "local" }),
        )
        .await
        .expect("prune");
    assert_eq!(
        pruned["pruned"], 1,
        "the seeded note must be the one candidate pruned: {pruned:?}"
    );

    assert!(
        !ann::is_current(&ann, &mutation_hook_ann_key()).await,
        "memory.prune deleting a candidate must invalidate the warm ANN \
             generation for affected models (#750)"
    );
}

#[tokio::test]
#[serial(background_tasks)]
#[serial_test::serial(config_ledger)]
async fn kg_update_reindex_invalidates_warm_ann_without_subsequent_remember() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let (registry, ann) = build_note_hook_registry(&rt);

    let id = seed_and_warm_ann(&rt, &registry, &ann, "fr1 update target content", 0.7).await;

    // KG's generic `update` verb — NOT `memory.remember` — changes the
    // note's content. Same call shape `khive-pack-kg/src/handlers/
    // update.rs` dispatches through `KhiveRuntime::update_note`; no
    // `kind` param needed, the UUID resolves the substrate.
    registry
        .dispatch(
            "update",
            json!({ "id": id.to_string(), "content": "entirely different rewritten content" }),
        )
        .await
        .expect("kg update on memory-kind note");

    assert!(
        !ann::is_current(&ann, &mutation_hook_ann_key()).await,
        "a KG `update` that changes a memory-kind note's content must \
             invalidate the warm ANN generation (#750)"
    );
}

#[tokio::test]
#[serial(background_tasks)]
#[serial_test::serial(config_ledger)]
async fn kg_delete_invalidates_warm_ann_without_subsequent_remember() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let (registry, ann) = build_note_hook_registry(&rt);

    let id = seed_and_warm_ann(&rt, &registry, &ann, "fr1 delete target content", 0.7).await;

    // KG's generic `delete` verb (soft delete by default) — NOT any
    // memory-pack verb.
    let deleted = registry
        .dispatch("delete", json!({ "id": id.to_string() }))
        .await
        .expect("kg delete on memory-kind note");
    assert_eq!(
        deleted["deleted"].as_bool(),
        Some(true),
        "delete must report success: {deleted:?}"
    );

    assert!(
        !ann::is_current(&ann, &mutation_hook_ann_key()).await,
        "a KG `delete` on a memory-kind note must invalidate the warm \
             ANN generation (#750)"
    );
}

/// A real merge invalidates an index warmed only after both notes were seeded.
/// See `crates/khive-pack-memory/docs/recall-reliability.md`.
#[tokio::test]
#[serial(background_tasks)]
#[serial_test::serial(config_ledger)]
async fn kg_merge_invalidates_warm_ann_without_subsequent_remember() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let (registry, ann) = build_note_hook_registry(&rt);

    rt.register_embedder(Fr1FixedVecProvider {
        model_name: FR1_MODEL.to_string(),
        vector: [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
    });

    let into_id = registry
        .dispatch(
            "memory.remember",
            json!({"content": "fr2 merge into content", "salience": 0.7}),
        )
        .await
        .expect("seed into-note")["id"]
        .as_str()
        .expect("id")
        .parse::<Uuid>()
        .expect("valid uuid");
    let from_id = registry
        .dispatch(
            "memory.remember",
            json!({"content": "fr2 merge from content", "salience": 0.7}),
        )
        .await
        .expect("seed from-note")["id"]
        .as_str()
        .expect("id")
        .parse::<Uuid>()
        .expect("valid uuid");

    // ONE warm-up recall, after BOTH notes exist.
    registry
        .dispatch(
            "memory.recall",
            json!({
                "query": "fr2 merge into content",
                "namespace": "local",
                "fusion_strategy": "vector_only",
                "embedding_model": FR1_MODEL,
            }),
        )
        .await
        .expect("warm recall");
    // Trust freshness only after all single-flight rebuilds release the warm guard.
    ann::wait_until_warm_idle(&ann, &mutation_hook_ann_key()).await;
    assert!(
        ann::is_current(&ann, &mutation_hook_ann_key()).await,
        "sanity: warm-up recall must leave the ANN cache current before \
             the merge under test"
    );
    let generation_before = ann::generation_for_test(&ann, &mutation_hook_ann_key()).await;

    registry
        .dispatch(
            "merge",
            json!({
                "into_id": into_id.to_string(),
                "from_id": from_id.to_string(),
                "kind": "memory",
            }),
        )
        .await
        .expect("kg merge on memory-kind notes");

    assert!(
        ann::generation_for_test(&ann, &mutation_hook_ann_key()).await > generation_before,
        "a KG `merge` of two memory-kind notes must invalidate the warm \
             ANN generation (#750)"
    );
    ann::wait_until_warm_idle(&ann, &mutation_hook_ann_key()).await;
    assert!(
        ann::is_current(&ann, &mutation_hook_ann_key()).await,
        "the mutation-triggered background rebuild must converge on the \
             post-merge generation without another recall"
    );
}

struct Fr1FixedVecProvider {
    model_name: String,
    vector: [f32; 8],
}

#[async_trait::async_trait]
impl khive_runtime::EmbedderProvider for Fr1FixedVecProvider {
    fn name(&self) -> &str {
        &self.model_name
    }

    fn dimensions(&self) -> usize {
        8
    }

    async fn build(
        &self,
    ) -> Result<std::sync::Arc<dyn lattice_embed::EmbeddingService>, RuntimeError> {
        Ok(std::sync::Arc::new(Fr1FixedVecService {
            vector: self.vector,
        }))
    }
}

struct Fr1FixedVecService {
    vector: [f32; 8],
}

#[async_trait::async_trait]
impl lattice_embed::EmbeddingService for Fr1FixedVecService {
    async fn embed(
        &self,
        texts: &[String],
        _model: lattice_embed::EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, lattice_embed::EmbedError> {
        // Every text maps to the SAME fixed vector — deterministic
        // cosine=1.0 between any query and any seeded content under this
        // provider, so ANN warming/hit behavior is fully controlled by
        // this test module, not by real embedding semantics.
        Ok(texts.iter().map(|_| self.vector.to_vec()).collect())
    }

    fn supports_model(&self, _model: lattice_embed::EmbeddingModel) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "fr1-fixed-vec"
    }
}
