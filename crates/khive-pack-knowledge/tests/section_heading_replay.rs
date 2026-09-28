use async_trait::async_trait;
use khive_pack_kg::KgPack;
use khive_pack_knowledge::KnowledgePack;
use khive_runtime::{
    AllowAllGate, BackendId, EmbedderProvider, KhiveRuntime, RuntimeConfig, RuntimeError,
    VerbRegistry, VerbRegistryBuilder,
};
use khive_storage::{SqlStatement, SqlValue};
use khive_types::Namespace;
use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

const MODEL_KEY: &str = "all-minilm-l6-v2";
const DIM: usize = 384;
const SLUG: &str = "section-heading-replay";
const INITIAL: &str = "initial-heading";
const NEWER: &str = "current-heading";
const BODY: &str = "Synthetic section body remains byte identical during this controlled heading test. It supplies enough explanatory content for the existing section admission rule.";
const ATOM: &str = "retrieval embeddings vectors knowledge sections heading publication concurrency consistency revision validation synthetic deterministic fixture model search ranking index verification testing";

struct Control {
    section_calls: AtomicUsize,
}

impl Control {
    fn new() -> Self {
        Self {
            section_calls: AtomicUsize::new(0),
        }
    }
}

struct ControlledService(Arc<Control>);

#[async_trait]
impl EmbeddingService for ControlledService {
    async fn embed(
        &self,
        texts: &[String],
        _model: EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        let mut vectors = Vec::with_capacity(texts.len());
        for text in texts {
            let is_section = text.contains(BODY);
            if is_section {
                self.0.section_calls.fetch_add(1, Ordering::SeqCst);
            }
            let mut vector = vec![0.0; DIM];
            if is_section && text.contains(NEWER) {
                vector[0] = 1.0;
            } else {
                vector[1] = 1.0;
            }
            vectors.push(vector);
        }
        Ok(vectors)
    }

    async fn embed_query(
        &self,
        texts: &[String],
        model: EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        self.embed(texts, model).await
    }

    fn supports_model(&self, _model: EmbeddingModel) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "section-heading-embedder"
    }
}

struct ControlledProvider(Arc<Control>);

#[async_trait]
impl EmbedderProvider for ControlledProvider {
    fn name(&self) -> &str {
        MODEL_KEY
    }

    fn dimensions(&self) -> usize {
        DIM
    }

    async fn build(&self) -> Result<Arc<dyn EmbeddingService>, RuntimeError> {
        Ok(Arc::new(ControlledService(Arc::clone(&self.0))))
    }
}

struct Fixture {
    runtime: KhiveRuntime,
    registry: VerbRegistry,
    control: Arc<Control>,
}

fn fixture() -> Fixture {
    let runtime = KhiveRuntime::new(RuntimeConfig {
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
        additional_embedding_models: Vec::new(),
        gate: Arc::new(AllowAllGate),
        packs: vec!["kg".into(), "knowledge".into()],
        backend_id: BackendId::main(),
        brain_profile: None,
        visible_namespaces: Vec::new(),
        allowed_outbound_namespaces: Vec::new(),
        actor_id: None,
        exec: Default::default(),
    })
    .expect("in-memory knowledge runtime");
    let control = Arc::new(Control::new());
    runtime.register_embedder(ControlledProvider(Arc::clone(&control)));
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime.clone()));
    builder.register(KnowledgePack::new(runtime.clone()));
    let registry = builder.build().expect("knowledge registry");
    registry.apply_schema_plans(runtime.backend());
    runtime.install_edge_rules(registry.all_edge_rules());
    Fixture {
        runtime,
        registry,
        control,
    }
}

async fn edit(f: &Fixture, heading: &str, section_type: &str) -> Result<Value, RuntimeError> {
    f.registry
        .dispatch(
            "knowledge.edit",
            json!({"id": SLUG, "sections": [{
                "section_type": section_type, "heading": heading, "content": BODY
            }]}),
        )
        .await
}

async fn seed(f: &Fixture) -> String {
    f.registry
        .dispatch(
            "knowledge.upsert_atoms",
            json!({"atoms": [{"slug": SLUG, "name": "Synthetic Heading Test", "content": ATOM}]}),
        )
        .await
        .expect("seed atom");
    let response = edit(f, INITIAL, "overview").await.expect("seed section");
    response["sections"][0]["id"]
        .as_str()
        .expect("section ID")
        .to_owned()
}

async fn stored(f: &Fixture, id: &str) -> (String, String, Vec<u8>) {
    let mut reader = f.runtime.sql().reader().await.expect("read section");
    let row = reader
        .query_row(SqlStatement {
            sql: "SELECT heading, section_type, embedding FROM knowledge_sections WHERE id = ?1"
                .into(),
            params: vec![SqlValue::Text(id.to_owned())],
            label: Some("knowledge.heading-replay.readback".into()),
        })
        .await
        .expect("section query")
        .expect("section exists");
    let (Some(SqlValue::Text(heading)), Some(SqlValue::Text(kind)), Some(SqlValue::Blob(bytes))) = (
        row.get("heading"),
        row.get("section_type"),
        row.get("embedding"),
    ) else {
        panic!("expected heading, type and a non-null vector: {row:?}");
    };
    (heading.clone(), kind.clone(), bytes.clone())
}

fn vector_head(bytes: &[u8]) -> [f32; 2] {
    assert_eq!(bytes.len(), DIM * 4, "provider dimension contract");
    [
        f32::from_le_bytes(bytes[0..4].try_into().unwrap()),
        f32::from_le_bytes(bytes[4..8].try_into().unwrap()),
    ]
}

#[tokio::test]
async fn sequential_heading_refresh_and_identical_replay_keep_matching_vector() {
    let f = fixture();
    let id = seed(&f).await;
    assert_eq!(vector_head(&stored(&f, &id).await.2), [0.0, 1.0]);
    let initial_calls = f.control.section_calls.load(Ordering::SeqCst);
    let changed = edit(&f, NEWER, "examples")
        .await
        .expect("sequential change");
    assert_eq!(changed["sections"][0]["id"], id);
    let (heading, kind, vector) = stored(&f, &id).await;
    assert_eq!(heading, NEWER);
    assert_eq!(kind, "examples");
    assert_eq!(vector_head(&vector), [1.0, 0.0]);
    let calls = f.control.section_calls.load(Ordering::SeqCst);
    assert!(
        calls > initial_calls,
        "heading change must re-embed the section"
    );
    let replay = edit(&f, NEWER, "examples").await.expect("identical replay");
    assert_eq!(replay["sections"][0]["id"], id);
    let (replay_heading, replay_kind, replay_vector) = stored(&f, &id).await;
    assert_eq!(replay_heading, NEWER);
    assert_eq!(replay_kind, "examples");
    assert_eq!(replay_vector, vector);
    assert_eq!(f.control.section_calls.load(Ordering::SeqCst), calls);
}
