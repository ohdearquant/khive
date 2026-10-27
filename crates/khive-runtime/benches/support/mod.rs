use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use khive_runtime::{
    entity_fts_document, note_fts_document, EmbedderProvider, KhiveRuntime, NamespaceToken,
    RuntimeConfig, RuntimeResult,
};
use khive_storage::types::{Edge, VectorRecord};
use khive_storage::{BatchWriteSummary, Entity, Note};
use khive_types::{EdgeRelation, Namespace, SubstrateKind};
use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
use rand::{rngs::StdRng, RngExt, SeedableRng};
use uuid::Uuid;

pub const ROWS: usize = 10_000;
pub const SEED: u64 = 5643;
pub const MODEL: &str = "all-minilm-l6-v2";
pub const DIMS: usize = 384;
pub const CONTENT: &str = "knowledge graph memory retrieval runtime document";
const MICROS: i64 = 1_700_000_000_000_000;

fn complete(summary: BatchWriteSummary) {
    assert_eq!(summary.attempted, ROWS as u64, "{summary:?}");
    assert_eq!(summary.affected, ROWS as u64, "{summary:?}");
    assert_eq!(summary.failed, 0, "{summary:?}");
}

pub fn executor() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("benchmark executor")
}

pub fn id(index: usize) -> Uuid {
    Uuid::from_u128((u128::from(SEED) << 64) | (index as u128 + 1))
}

fn timestamp() -> DateTime<Utc> {
    DateTime::from_timestamp_micros(MICROS).expect("fixture timestamp")
}

struct Stub;

#[async_trait]
impl EmbeddingService for Stub {
    async fn embed(
        &self,
        texts: &[String],
        _model: EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts
            .iter()
            .map(|_| vec![1.0 / (DIMS as f32).sqrt(); DIMS])
            .collect())
    }

    fn supports_model(&self, _model: EmbeddingModel) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "runtime-benchmark-stub"
    }
}

#[async_trait]
impl EmbedderProvider for Stub {
    fn name(&self) -> &str {
        MODEL
    }

    fn dimensions(&self) -> usize {
        DIMS
    }

    async fn build(&self) -> RuntimeResult<Arc<dyn EmbeddingService>> {
        Ok(Arc::new(Stub))
    }
}

pub struct Fixture {
    pub runtime: KhiveRuntime,
    pub token: NamespaceToken,
}

impl Fixture {
    pub fn new(embeddings: bool) -> Self {
        Self::with_database(embeddings, None)
    }

    // Shared by benchmark binaries; only the edge workloads require a real writer queue.
    #[allow(dead_code)]
    pub fn file_backed(path: std::path::PathBuf) -> Self {
        Self::with_database(false, Some(path))
    }

    fn with_database(embeddings: bool, db_path: Option<std::path::PathBuf>) -> Self {
        let volume_lock_dir = db_path
            .as_ref()
            .map(|path| path.parent().expect("fixture parent").join("volume-locks"));
        let runtime = KhiveRuntime::new(RuntimeConfig {
            db_path,
            volume_lock_dir,
            packs: vec!["kg".into()],
            actor_id: Some("benchmark".into()),
            embedding_model: embeddings.then_some(EmbeddingModel::AllMiniLmL6V2),
            ..RuntimeConfig::no_embeddings()
        })
        .expect("in-memory runtime");
        runtime.install_kind_registry(
            vec!["concept".into(), "artifact".into()],
            vec!["observation".into(), "memory".into()],
        );
        if embeddings {
            runtime
                .try_register_embedder(Stub)
                .expect("replace model before first use");
        }
        let token = runtime
            .authorize(Namespace::local())
            .expect("benchmark token");
        Self { runtime, token }
    }

    pub async fn seed_notes(&self) {
        let mut rng = StdRng::seed_from_u64(SEED);
        let notes: Vec<_> = (0..ROWS)
            .map(|i| Note {
                id: id(i),
                namespace: "local".into(),
                kind: "observation".into(),
                status: "active".into(),
                name: Some(format!("note {i}")),
                content: format!("{CONTENT} word{}", rng.random_range(0..1000)),
                salience: Some(0.5),
                decay_factor: None,
                expires_at: None,
                properties: None,
                created_at: MICROS,
                updated_at: MICROS,
                deleted_at: None,
                key: None,
                version: 1,
            })
            .collect();
        let documents = notes.iter().map(note_fts_document).collect();
        let summary = self
            .runtime
            .notes(&self.token)
            .expect("note store")
            .upsert_notes(notes)
            .await
            .expect("seed 10K notes");
        complete(summary);
        let summary = self
            .runtime
            .text_for_notes(&self.token)
            .expect("note text store")
            .upsert_documents(documents)
            .await
            .expect("seed note FTS");
        complete(summary);
    }

    pub async fn seed_entities(&self, vectors: bool) {
        let entities: Vec<_> = (0..ROWS)
            .map(|i| {
                let mut entity = Entity::minimal(
                    id(i),
                    "local",
                    "concept",
                    format!("concept {i}"),
                    MICROS,
                    MICROS,
                );
                entity.description = Some(CONTENT.into());
                entity
            })
            .collect();
        let documents = entities.iter().map(entity_fts_document).collect();
        let summary = self
            .runtime
            .entities(&self.token)
            .expect("entity store")
            .upsert_entities(entities)
            .await
            .expect("seed 10K entities");
        complete(summary);
        let summary = self
            .runtime
            .text(&self.token)
            .expect("entity text store")
            .upsert_documents(documents)
            .await
            .expect("seed entity FTS");
        complete(summary);
        if vectors {
            let mut rng = StdRng::seed_from_u64(SEED);
            let records = (0..ROWS)
                .map(|i| {
                    let mut vector: Vec<f32> =
                        (0..DIMS).map(|_| rng.random_range(-1.0..1.0)).collect();
                    let norm = vector.iter().map(|v| v * v).sum::<f32>().sqrt();
                    for value in &mut vector {
                        *value /= norm;
                    }
                    VectorRecord {
                        subject_id: id(i),
                        kind: SubstrateKind::Entity,
                        namespace: "local".into(),
                        field: "content".into(),
                        embedding_model: Some(MODEL.into()),
                        vectors: vec![vector],
                        text_fingerprint: None,
                        updated_at: timestamp(),
                    }
                })
                .collect();
            let summary = self
                .runtime
                .vectors(&self.token)
                .expect("vector store")
                .insert_batch(records)
                .await
                .expect("seed 10K vectors");
            complete(summary);
        }
    }

    pub async fn seed_graph(&self, relation: EdgeRelation) {
        self.seed_entities(false).await;
        let edges = (0..ROWS)
            .map(|i| {
                let (source, target) = if i + 1 < ROWS {
                    (i / 4, i + 1)
                } else {
                    (ROWS - 1, 0)
                };
                Edge {
                    id: id(ROWS + i).into(),
                    namespace: "local".into(),
                    source_id: id(source),
                    target_id: id(target),
                    relation,
                    weight: 1.0,
                    created_at: timestamp(),
                    updated_at: timestamp(),
                    deleted_at: None,
                    metadata: None,
                    target_backend: None,
                }
            })
            .collect();
        let summary = self
            .runtime
            .graph(&self.token)
            .expect("graph store")
            .upsert_edges(edges)
            .await
            .expect("seed 10K edges");
        complete(summary);
    }
}
