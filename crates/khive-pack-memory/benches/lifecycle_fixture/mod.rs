use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use khive_db::StorageBackend;
use khive_pack_kg::KgPack;
use khive_pack_memory::MemoryPack;
use khive_runtime::{
    note_fts_document, EmbedderProvider, KhiveRuntime, RuntimeConfig, RuntimeResult, VerbRegistry,
    VerbRegistryBuilder,
};
use khive_storage::{BatchWriteSummary, Note};
use khive_types::Namespace;
use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
use serde_json::{json, Value};
use tempfile::TempDir;
use uuid::Uuid;

#[path = "../../../khive-runtime/tests/support/receipt_credentials.rs"]
mod receipt_credentials;

pub const SAMPLES: usize = 40;
pub const QUERY: &str = "graph retrieval memory";
pub const DIMS: usize = 384;
const MODEL: &str = "all-minilm-l6-v2";

struct DeterministicEmbedder;

#[async_trait]
impl EmbeddingService for DeterministicEmbedder {
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
    fn supports_model(&self, model: EmbeddingModel) -> bool {
        matches!(model, EmbeddingModel::AllMiniLmL6V2)
    }
    fn name(&self) -> &'static str {
        "memory-benchmark-deterministic"
    }
}

#[async_trait]
impl EmbedderProvider for DeterministicEmbedder {
    fn name(&self) -> &str {
        MODEL
    }
    fn dimensions(&self) -> usize {
        DIMS
    }
    async fn build(&self) -> RuntimeResult<Arc<dyn EmbeddingService>> {
        Ok(Arc::new(DeterministicEmbedder))
    }
}

fn complete(summary: BatchWriteSummary, rows: usize) {
    assert_eq!(summary.attempted, rows as u64, "{summary:?}");
    assert_eq!(summary.affected, rows as u64, "{summary:?}");
    assert_eq!(summary.failed, 0, "{summary:?}");
}

pub struct Fixture {
    pub registry: VerbRegistry,
    pub runtime: KhiveRuntime,
    backend: Arc<StorageBackend>,
    dir: Option<TempDir>,
}

impl Fixture {
    pub async fn new(rows: usize, file_backed: bool, embeddings: bool) -> Self {
        let dir = file_backed.then(|| tempfile::tempdir().expect("private fixture directory"));
        let backend = Arc::new(match &dir {
            Some(dir) => StorageBackend::sqlite_for_test_with_journal_mode_in(
                dir.path().join("memory.db"),
                true,
                Duration::from_secs(10),
                dir.path().join("volume-locks"),
            )
            .expect("queued WAL fixture"),
            None => StorageBackend::memory().expect("memory fixture"),
        });
        backend.prepare_core_schema().expect("migrate core schema");
        assert_eq!(
            backend.pool().write_queue_active(),
            file_backed,
            "fixed writer route"
        );
        if file_backed {
            assert!(backend
                .pool()
                .writer_task_handle()
                .expect("writer initialization")
                .is_some());
        }
        let config = RuntimeConfig {
            db_path: dir.as_ref().map(|dir| dir.path().join("memory.db")),
            actor_id: Some("benchmark:memory".into()),
            packs: vec!["kg".into(), "memory".into()],
            embedding_model: embeddings.then_some(EmbeddingModel::AllMiniLmL6V2),
            ..RuntimeConfig::no_embeddings()
        };
        let runtime =
            KhiveRuntime::from_prepared_backend(backend.clone(), config).expect("prepared runtime");
        let runtime = receipt_credentials::with_receipt_credentials(runtime);
        runtime.install_kind_registry(
            vec!["concept".into()],
            vec!["memory".into(), "observation".into()],
        );
        if embeddings {
            runtime
                .try_register_embedder(DeterministicEmbedder)
                .expect("install before first embed");
        }
        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(runtime.clone()));
        builder.register(MemoryPack::new(runtime.clone()));
        let registry = builder.build().expect("memory registry");
        let fixture = Self {
            registry,
            runtime,
            backend,
            dir,
        };
        if rows > 0 {
            fixture.seed(rows).await;
        }
        fixture
    }

    async fn seed(&self, rows: usize) {
        // Only FTS scenarios seed notes; no short-lived ANN build is started.
        assert!(self.runtime.registered_embedding_model_names().is_empty());
        let token = self
            .runtime
            .authorize(Namespace::local())
            .expect("local token");
        let now = Utc::now().timestamp_micros();
        let notes: Vec<_> = (0..rows)
            .map(|i| Note {
                id: Uuid::from_u128((5_649_u128 << 64) | (i as u128 + 1)),
                namespace: "local".into(),
                kind: "memory".into(),
                status: "active".into(),
                name: Some(format!("memory {i}")),
                content: format!(
                    "{QUERY} deterministic topic {} sample {i} {}",
                    i % 17,
                    "context ".repeat(48)
                ),
                salience: Some(if i.is_multiple_of(2) { 0.1 } else { 0.8 }),
                decay_factor: Some(0.0),
                expires_at: None,
                properties: Some(json!({"memory_type":"semantic"})),
                created_at: now,
                updated_at: now,
                deleted_at: None,
                key: None,
                version: 1,
            })
            .collect();
        let documents = notes.iter().map(note_fts_document).collect();
        complete(
            self.runtime
                .notes(&token)
                .expect("notes")
                .upsert_notes(notes)
                .await
                .expect("seed notes"),
            rows,
        );
        complete(
            self.runtime
                .text_for_notes(&token)
                .expect("note FTS")
                .upsert_documents(documents)
                .await
                .expect("seed FTS"),
            rows,
        );
    }

    /// Drop all runtime/registry owners, await the file writer, then measure settled
    /// database bytes and delete its directory. Every caller invokes this untimed.
    pub async fn close(self) -> Option<u64> {
        let Self {
            registry,
            runtime,
            backend,
            dir,
        } = self;
        drop(registry);
        drop(runtime);
        let writer = backend.pool().take_writer_task_join();
        assert_eq!(writer.is_some(), dir.is_some());
        drop(backend);
        if let Some(writer) = writer {
            tokio::time::timeout(Duration::from_secs(30), writer)
                .await
                .expect("writer teardown deadline")
                .expect("writer joined");
        }
        dir.map(|dir| {
            let bytes = std::fs::metadata(dir.path().join("memory.db"))
                .expect("settled DB metadata")
                .len();
            dir.close().expect("remove fixture and private locks");
            bytes
        })
    }
}

pub fn executor() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .max_blocking_threads(2)
        .enable_all()
        .build()
        .expect("executor")
}

pub fn rows(response: &Value) -> &[Value] {
    response
        .as_array()
        .or_else(|| response.get("results").and_then(Value::as_array))
        .expect("recall results")
}

pub fn check_recall(response: &Value) {
    assert!(!rows(response).is_empty(), "nonempty generated recall");
    assert!(response.get("degraded").is_none());
    for row in rows(response) {
        assert!(row.get("degraded").is_none());
        let id = row["full_id"].as_str().expect("full UUID");
        assert_eq!(Uuid::parse_str(id).expect("UUID").to_string(), id);
        assert!(row["score"].as_f64().expect("score").is_finite());
    }
}

/// Actual individual-call samples, not Criterion batch averages. Median is the
/// midpoint of the two central values; p95 is nearest rank ceil(.95*n).
pub fn report(label: &str, mut samples: Vec<Duration>) -> f64 {
    assert_eq!(samples.len(), SAMPLES);
    samples.sort_unstable();
    let median =
        (samples[SAMPLES / 2 - 1].as_secs_f64() + samples[SAMPLES / 2].as_secs_f64()) / 2.0;
    let p95 = samples[(95 * SAMPLES).div_ceil(100) - 1].as_secs_f64();
    println!(
        "{}",
        json!({"scenario":label,"samples":SAMPLES,"p50_ms":median*1000.0,"p95_ms":p95*1000.0,"sample_kind":"individual dispatch"})
    );
    median
}
