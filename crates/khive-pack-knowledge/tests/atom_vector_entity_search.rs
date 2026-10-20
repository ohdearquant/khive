//! Knowledge atom vectors are written to the backend's vector store as entity vectors
//! (kind = entity, field "knowledge.atom"). The entity vector arm of hybrid search filters
//! on namespace, model and kind only, so where atoms and kg entities share a backend the
//! atoms take places in the arm's candidate window and are dropped only at entity
//! hydration. This test measures that on one in-memory backend: the same five entities
//! and the same query vector, with and without fifty indexed atoms nearer the query.

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use khive_pack_kg::KgPack;
use khive_pack_knowledge::KnowledgePack;
use khive_runtime::{
    AllowAllGate, BackendId, EmbedderProvider, KhiveRuntime, RuntimeConfig, VerbRegistry,
    VerbRegistryBuilder,
};
use khive_types::{Namespace, SubstrateKind};
use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
use serde_json::json;
use uuid::Uuid;

const MODEL_KEY: &str = "all-minilm-l6-v2";
const DIM: usize = 384;
const ATOM_MARK: &str = "atomdisplacementfixture";
const ENTITIES: usize = 5;
const ATOMS: usize = 50;

/// Atom texts embed to e0; every other text to e0 + 0.6 e1 (cosine 0.857 to e0).
fn vector_for(text: &str) -> Vec<f32> {
    let mut v = vec![0.0f32; DIM];
    v[0] = 1.0;
    if !text.contains(ATOM_MARK) {
        v[1] = 0.6;
    }
    v
}

struct FixedEmbedService;

#[async_trait]
impl EmbeddingService for FixedEmbedService {
    async fn embed(
        &self,
        texts: &[String],
        _model: EmbeddingModel,
    ) -> std::result::Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts.iter().map(|t| vector_for(t)).collect())
    }

    async fn embed_query(
        &self,
        texts: &[String],
        model: EmbeddingModel,
    ) -> std::result::Result<Vec<Vec<f32>>, EmbedError> {
        self.embed(texts, model).await
    }

    fn supports_model(&self, _model: EmbeddingModel) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "fixed-embed-service"
    }
}

struct FixedEmbedProvider;

#[async_trait]
impl EmbedderProvider for FixedEmbedProvider {
    fn name(&self) -> &str {
        MODEL_KEY
    }

    fn dimensions(&self) -> usize {
        DIM
    }

    async fn build(
        &self,
    ) -> std::result::Result<Arc<dyn EmbeddingService>, khive_runtime::RuntimeError> {
        Ok(Arc::new(FixedEmbedService))
    }
}

fn runtime() -> (KhiveRuntime, VerbRegistry) {
    let rt = KhiveRuntime::new(RuntimeConfig {
        wal_ceiling_bytes: 0,
        wal_ceiling_configured_bytes: 0,
        wal_ceiling_source: Default::default(),
        wal_ceiling_env_raw: None,
        web: Default::default(),
        telemetry: Default::default(),
        mounts: Vec::new(),
        brain: Default::default(),
        git_write: Default::default(),
        display_timezone: khive_runtime::config::resolve_default_display_timezone(),
        events_split: None,
        db_path: None,
        blob_hydration_bytes: khive_runtime::DEFAULT_BLOB_HYDRATION_BYTES,
        default_namespace: Namespace::local(),
        embedding_model: Some(EmbeddingModel::AllMiniLmL6V2),
        additional_embedding_models: vec![],
        gate: Arc::new(AllowAllGate),
        packs: vec!["kg".to_string(), "knowledge".to_string()],
        backend_id: BackendId::main(),
        brain_profile: None,
        visible_namespaces: vec![],
        allowed_outbound_namespaces: vec![],
        actor_id: None,
        exec: Default::default(),
        ..khive_runtime::RuntimeConfig::no_embeddings()
    })
    .expect("runtime");
    rt.register_embedder(FixedEmbedProvider);
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(KnowledgePack::new(rt.clone()));
    let registry = builder.build().expect("registry builds");
    registry.apply_schema_plans(rt.backend());
    rt.install_edge_rules(registry.all_edge_rules());
    (rt, registry)
}

/// Seeds the entities (and optionally the atoms), then returns
/// (entities returned by `search(kind="entity", limit=5)` for a query that embeds
///  to e0, seeded entities among the raw entity-kind KNN window of 20).
async fn measure(with_atoms: bool) -> (usize, usize) {
    let (rt, registry) = runtime();
    let token = rt.authorize(Namespace::local()).expect("token");
    let mut entity_ids = HashSet::<Uuid>::new();
    for i in 0..ENTITIES {
        let (entity, _report) = rt
            .core()
            .create_entity_with_embedding_report(
                &token,
                "concept",
                None,
                &format!("entity {i:02}"),
                Some("Controlled entity for the vector arm."),
                None,
                vec![],
            )
            .await
            .expect("create entity");
        entity_ids.insert(entity.id);
    }
    if with_atoms {
        let atoms: Vec<_> = (0..ATOMS)
            .map(|n| {
                json!({
                    "slug": format!("displace-{n:03}"),
                    "name": format!("Displace {n:03}"),
                    // atom content is refused under 20 words
                    "content": format!(
                        "{ATOM_MARK} atom {n:03} is a controlled fixture text written only to \
                         occupy vector slots near the query so the entity arm can be measured"
                    ),
                })
            })
            .collect();
        registry
            .dispatch("knowledge.upsert_atoms", json!({ "atoms": atoms }))
            .await
            .expect("upsert atoms");
        let indexed = registry
            .dispatch("knowledge.index", json!({}))
            .await
            .expect("index atoms");
        assert_eq!(
            indexed["indexed"].as_u64(),
            Some(ATOMS as u64),
            "every atom vector is written: {indexed:?}"
        );
    }
    let window = rt
        .vector_search(
            &token,
            Some(vector_for(ATOM_MARK)),
            None,
            20,
            Some(SubstrateKind::Entity),
        )
        .await
        .expect("entity-kind KNN");
    let entities_in_window = window
        .iter()
        .filter(|hit| entity_ids.contains(&hit.subject_id))
        .count();
    let hits = registry
        .dispatch(
            "search",
            json!({ "kind": "entity", "query": ATOM_MARK, "limit": 5 }),
        )
        .await
        .expect("entity search");
    let hits = hits.as_array().expect("search returns an array");
    for hit in hits {
        let id: Uuid = hit["id"].as_str().expect("hit id").parse().expect("uuid");
        assert!(
            entity_ids.contains(&id),
            "only seeded entities hydrate: {hit}"
        );
    }
    (hits.len(), entities_in_window)
}

#[tokio::test]
async fn entity_search_keeps_its_hits_when_atoms_share_the_backend() {
    let (control_hits, control_window) = measure(false).await;
    let (atom_hits, atom_window) = measure(true).await;
    let report = format!(
        "entities returned: {control_hits} without atoms, {atom_hits} with {ATOMS} atoms; \
         entities in the 20-slot entity-kind KNN window: {control_window} without, {atom_window} with"
    );
    println!("{report}");
    assert_eq!(control_hits, ENTITIES, "control: {report}");
    assert_eq!(control_window, ENTITIES, "control: {report}");
    assert_eq!(atom_hits, ENTITIES, "{report}");
}
