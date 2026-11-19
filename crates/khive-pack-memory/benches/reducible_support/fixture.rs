//! Generated synthetic-v1 fixture: fixed FNV-1a token hashing, no model downloads.
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use khive_db::StorageBackend;
use khive_pack_kg::KgPack;
use khive_pack_memory::MemoryPack;
use khive_runtime::{
    EmbedderProvider, KhiveRuntime, RuntimeConfig, RuntimeResult, VerbRegistry, VerbRegistryBuilder,
};
use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
use serde_json::{json, Value};

#[path = "../../../khive-runtime/tests/support/receipt_credentials.rs"]
pub(crate) mod receipt_credentials;

pub const SEED: u64 = 0x5713_6d65_6d6f_7279;
pub const TOPICS: [&str; 8] = [
    "attention transformer query",
    "rust ownership lifetime",
    "graph entity relation",
    "agent orchestration parallel",
    "recall scoring fusion",
    "namespace isolation security",
    "embedding vector cosine",
    "memory decay salience",
];

pub fn body(i: usize) -> String {
    // Deliberately mixed frequencies: all docs share function words; 1/2 have
    // mediumband; 1/8 share a topic; 1/32 share a rare token; each has an ID.
    format!(
        "the and for with {} {} rare{} document{}",
        TOPICS[i % TOPICS.len()],
        if i.is_multiple_of(2) {
            "mediumband"
        } else {
            "alternateband"
        },
        i % 32,
        i
    )
}

pub fn query(i: usize) -> String {
    format!(
        "the and for with {} mediumband rare{}",
        TOPICS[i % TOPICS.len()],
        i % 32
    )
}

#[derive(Clone)]
struct SyntheticEmbedder {
    model: EmbeddingModel,
    name: String,
}

#[async_trait]
impl EmbeddingService for SyntheticEmbedder {
    async fn embed(
        &self,
        texts: &[String],
        _model: EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts
            .iter()
            .map(|text| {
                let mut vector = vec![0.0_f32; self.model.dimensions()];
                for token in text.split_whitespace() {
                    let mut hash = SEED;
                    for byte in token.bytes().map(|b| b.to_ascii_lowercase()) {
                        hash = (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3);
                    }
                    let index = (hash % vector.len() as u64) as usize;
                    vector[index] += 1.0;
                }
                let norm = vector.iter().map(|x| x * x).sum::<f32>().sqrt();
                assert!(norm.is_finite() && norm > 0.0);
                for value in &mut vector {
                    *value /= norm;
                }
                vector
            })
            .collect())
    }
    fn supports_model(&self, model: EmbeddingModel) -> bool {
        model == self.model
    }
    fn name(&self) -> &'static str {
        "memory-benchmark-synthetic-v1"
    }
}

#[async_trait]
impl EmbedderProvider for SyntheticEmbedder {
    fn name(&self) -> &str {
        &self.name
    }
    fn dimensions(&self) -> usize {
        self.model.dimensions()
    }
    async fn build(&self) -> RuntimeResult<Arc<dyn EmbeddingService>> {
        Ok(Arc::new(self.clone()))
    }
}

pub struct Fixture {
    pub registry: VerbRegistry,
    pub runtime: KhiveRuntime,
    pub backend: Arc<StorageBackend>,
    dir: tempfile::TempDir,
}

impl Fixture {
    pub fn new(models: &[EmbeddingModel], synthetic: bool) -> Self {
        let dir = tempfile::Builder::new()
            .prefix("memory-synthetic-bench-")
            .tempdir()
            .expect("private fixture");
        let root = dir.path().canonicalize().expect("physical fixture path");
        let db_path = root.join("memory.db");
        let backend = Arc::new(
            StorageBackend::sqlite_for_test_with_journal_mode_in(
                &db_path,
                true,
                Duration::from_secs(10),
                root.join("locks"),
            )
            .expect("private WAL backend"),
        );
        backend.prepare_core_schema().expect("schema");
        assert!(backend.pool().write_queue_active());
        assert!(backend
            .pool()
            .writer_task_handle()
            .expect("writer runtime")
            .is_some());
        let runtime = KhiveRuntime::from_prepared_backend(
            backend.clone(),
            RuntimeConfig {
                db_path: Some(db_path),
                actor_id: Some("benchmark:memory-reducible".into()),
                embedding_model: models.first().copied(),
                additional_embedding_models: models.iter().skip(1).copied().collect(),
                ..RuntimeConfig::no_embeddings()
            },
        )
        .expect("prepared runtime");
        runtime.install_kind_registry(
            vec!["concept".into()],
            vec!["memory".into(), "observation".into()],
        );
        if synthetic {
            for model in models {
                runtime
                    .try_register_embedder(SyntheticEmbedder {
                        model: *model,
                        name: model.to_string(),
                    })
                    .expect("replace unresolved builtin with synthetic provider");
            }
        }
        runtime
            .finalize_embedding_engines()
            .expect("configured engine binding");
        assert_eq!(
            runtime
                .bound_embedding_engine_names()
                .expect("bound participation"),
            models.iter().map(ToString::to_string).collect::<Vec<_>>()
        );
        let mut actual = runtime.registered_embedding_model_names();
        actual.sort();
        let mut expected: Vec<_> = models.iter().map(ToString::to_string).collect();
        expected.sort();
        assert_eq!(actual, expected);
        let runtime = receipt_credentials::with_receipt_credentials(runtime);
        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(runtime.clone()));
        builder.register(MemoryPack::new(runtime.clone()));
        let registry = builder.build().expect("registry");
        Self {
            registry,
            runtime,
            backend,
            dir,
        }
    }

    pub async fn seed(&self, model: EmbeddingModel, n: usize) {
        for i in 0..n {
            self.registry.dispatch("memory.remember", json!({"content": body(i), "memory_type":"semantic", "salience":0.7, "decay_factor":0.0, "embedding_model":model.to_string()})).await.expect("generated memory");
        }
    }

    pub async fn close(self) {
        // Finish detached ANN tasks before dropping their runtime/storage owners.
        tokio::time::timeout(Duration::from_secs(60), async {
            while khive_runtime::background_task_names()
                .iter()
                .any(|n| matches!(n.as_str(), "memory_ann_build" | "memory_ann_rebuild"))
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("ANN teardown deadline");
        let Self {
            registry,
            runtime,
            backend,
            dir,
        } = self;
        drop(registry);
        drop(runtime);
        let writer = backend.pool().take_writer_task_join().expect("file writer");
        drop(backend);
        tokio::time::timeout(Duration::from_secs(30), writer)
            .await
            .expect("writer deadline")
            .expect("writer joined");
        dir.close().expect("remove isolated fixture");
    }
}

/// Use a short coalescing window for finite synthetic benchmark writes.
/// Explicit operator overrides, including the production delay, remain intact.
///
/// # Safety
/// Call only from benchmark `main` before starting any threads or executor.
pub unsafe fn configure_synthetic_ann_debounce() {
    if std::env::var_os("KHIVE_ANN_REBUILD_DEBOUNCE_MS").is_none() {
        // SAFETY: the caller guarantees this benchmark process is single-threaded.
        unsafe { std::env::set_var("KHIVE_ANN_REBUILD_DEBOUNCE_MS", "5") };
    }
}

pub fn executor() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .max_blocking_threads(2)
        .enable_all()
        .build()
        .expect("executor")
}

pub fn check_recall(value: &Value) {
    let rows = value.as_array().expect("recall array");
    assert!(!rows.is_empty() && rows.len() <= 10);
    for row in rows {
        assert!(
            row.get("degraded").is_none(),
            "refuse degraded vector route"
        );
        let id = row["full_id"].as_str().expect("full UUID");
        assert_eq!(uuid::Uuid::parse_str(id).expect("UUID").to_string(), id);
        assert!(row["score"].as_f64().expect("score").is_finite());
    }
}

pub fn reject_fts_overrides() {
    for name in [
        "KHIVE_RECALL_FTS_GATHER",
        "KHIVE_RECALL_FTS_TERM_K",
        "KHIVE_RECALL_FTS_SELECTION",
        "KHIVE_RECALL_FTS_GATHER_LIMIT",
        "KHIVE_RECALL_FTS_GATHER_MULTIPLIER",
        "KHIVE_RECALL_FTS_CJK_BYPASS",
    ] {
        assert!(
            std::env::var_os(name).is_none(),
            "unset {name} to measure the declared configuration"
        );
    }
}

/// Shared explicit opt-in. Listing reads this selector without loading a model.
pub fn synthetic_cohort() -> bool {
    match std::env::var("KHIVE_BENCH_REAL_MODEL") {
        Err(std::env::VarError::NotPresent) => true,
        Ok(value) if value == "0" => true,
        Ok(value) if value == "1" => false,
        _ => panic!("KHIVE_BENCH_REAL_MODEL must be absent, 0, or 1"),
    }
}
