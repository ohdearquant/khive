#[path = "embedding.rs"]
mod embedding;

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use khive_db::StorageBackend;
use khive_pack_kg::KgPack;
use khive_pack_knowledge::KnowledgePack;
use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig, VerbRegistry, VerbRegistryBuilder};
use serde_json::{json, Value};
use tempfile::TempDir;

pub const QUERY: &str = "draftmarker knowledge retrieval";
pub const CONTENT: &str = "knowledge retrieval benchmark generated atom corpus vector embedding search ranking domain briefing evidence budget selection normalized synthetic deterministic measurement storage reader writer publication result validation";

pub fn executor() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(2)
        .build()
        .expect("bounded executor")
}

pub fn slug(index: usize) -> String {
    format!("generated-atom-{index:06}")
}

pub struct Fixture {
    pub registry: VerbRegistry,
    pub(crate) runtime: KhiveRuntime,
    backend: Arc<StorageBackend>,
    directory: TempDir,
}

impl Fixture {
    pub fn new(rt: &tokio::runtime::Runtime, embeddings: bool) -> Self {
        // The queued writer's first initialization needs Handle::try_current.
        // Enter for setup; subsequent dispatch and teardown use this same rt.
        let _entered = rt.enter();
        let root = std::env::temp_dir()
            .canonicalize()
            .expect("physical temporary root");
        let directory = tempfile::tempdir_in(root).expect("private corpus");
        let db_path = directory.path().join("knowledge.db");
        let backend = Arc::new(
            StorageBackend::sqlite_for_test_with_journal_mode_in(
                &db_path,
                true,
                Duration::from_secs(10),
                directory.path().join("volume-locks"),
            )
            .expect("queued WAL corpus"),
        );
        backend.prepare_core_schema().expect("core schema");
        assert!(backend.pool().write_queue_active());
        assert!(backend
            .pool()
            .writer_task_handle()
            .expect("writer initialization")
            .is_some());
        let mut config = RuntimeConfig::no_embeddings();
        config.db_path = Some(db_path);
        config.actor_id = Some("benchmark:knowledge".into());
        config.default_namespace = Namespace::local();
        config.visible_namespaces.clear();
        config.allowed_outbound_namespaces.clear();
        config.packs = vec!["kg".into(), "knowledge".into()];
        config.credentials.clear();
        config.visibility_receipts = None;
        config.mounts.clear();
        config.events_split = None;
        config.brain_profile = None;
        config.brain = Default::default();
        config.wal_ceiling_bytes = 0;
        config.wal_ceiling_configured_bytes = 0;
        config.wal_ceiling_source = Default::default();
        config.wal_ceiling_env_raw = None;
        config.disk_guard_environment = Default::default();
        config.disk_guard_config = None;
        config.volume_lock_dir = None;
        config.embedding_model = embeddings.then_some(embedding::MODEL);
        config.additional_embedding_models.clear();
        let runtime =
            KhiveRuntime::from_prepared_backend(backend.clone(), config).expect("runtime");
        assert_eq!(
            runtime.backend_data_dir().as_deref(),
            Some(directory.path())
        );
        assert!(runtime
            .backend_ann_root()
            .unwrap()
            .starts_with(directory.path()));
        if embeddings {
            runtime
                .try_register_embedder(embedding::Synthetic)
                .expect("synthetic provider before use");
            assert_eq!(runtime.embedder_dimensions("all-minilm-l6-v2"), Some(384));
        } else {
            assert!(runtime.registered_embedding_model_names().is_empty());
        }
        let mut builder = VerbRegistryBuilder::new();
        builder.with_actor_id(Some("benchmark:knowledge".into()));
        builder.with_default_namespace("local");
        builder.register(KgPack::new(runtime.clone()));
        builder.register(KnowledgePack::new_with_index_role(runtime.clone(), true));
        let registry = builder.build().expect("knowledge registry");
        registry
            .apply_schema_plans_with_map(&Default::default(), runtime.backend())
            .expect("all pack schema plans applied");
        runtime.install_edge_rules(registry.all_edge_rules());
        // Match the existing knowledge bench's optional-audit-sink composition.
        assert!(registry.audit_batch_handle().is_none());
        Self {
            registry,
            runtime,
            backend,
            directory,
        }
    }

    pub async fn seed_atoms(&self, count: usize, mixed_drafts: bool) {
        for start in (0..count).step_by(500) {
            let end = (start + 500).min(count);
            let atoms: Vec<_> = (start..end).map(|i| {
                let draft = mixed_drafts && i % 2 == 0;
                let marker = if draft { "draftmarker" } else { "reviewedmarker" };
                json!({"slug":slug(i), "name":format!("{marker} knowledge retrieval atom {i:06}"),
                    "content":format!("subject{i} {marker} {CONTENT}"), "finalized":!draft})
            }).collect();
            let result = self
                .registry
                .dispatch("knowledge.upsert_atoms", json!({"atoms":atoms}))
                .await
                .expect("seed generated atoms");
            assert_eq!(result["created"], end - start);
            assert_eq!(result["updated"], 0);
            assert_eq!(result["total"], end - start);
        }
    }

    pub fn measure(
        &self,
        rt: &tokio::runtime::Runtime,
        verb: &str,
        params: Value,
    ) -> (Duration, Value) {
        let start = Instant::now();
        let result = rt.block_on(self.registry.dispatch(verb, std::hint::black_box(params)));
        let elapsed = start.elapsed();
        (elapsed, result.expect("complete successful dispatch"))
    }

    pub fn index(&self, rt: &tokio::runtime::Runtime, count: usize) -> Duration {
        let params = json!({"batch_size":500, "rebuild_ann":true});
        let (elapsed, result) = self.measure(rt, "knowledge.index", params);
        assert_eq!(result["indexed"], count, "{result}");
        assert_eq!(result["total"], count);
        assert_eq!(result["ann_vectors"], count);
        assert_eq!(result["skipped"], 0);
        assert_eq!(result["failed"], 0);
        assert_eq!(result["ann_failed"], false);
        elapsed
    }

    pub async fn close(self) {
        let Self {
            registry,
            runtime,
            backend,
            directory,
        } = self;
        assert!(registry.audit_batch_handle().is_none());
        drop(registry);
        drop(runtime);
        let writer = backend
            .pool()
            .take_writer_task_join()
            .expect("writer join handle");
        drop(backend);
        tokio::time::timeout(Duration::from_secs(30), writer)
            .await
            .expect("writer teardown deadline")
            .expect("writer joined");
        directory
            .close()
            .expect("remove private corpus after writer exit");
    }
}

pub fn check_search(result: &Value, count: usize, mixed_drafts: bool, include_drafts: bool) {
    assert!(
        !include_drafts || mixed_drafts,
        "drafts require a mixed corpus"
    );
    assert!(result.get("degraded").is_none(), "{result}");
    assert_ne!(result["ann_unavailable"], true, "{result}");
    let rows = result["results"].as_array().expect("search results");
    assert_eq!(rows.len(), 20, "{result}");
    assert_eq!(result["total"], 20);
    let mut ids = BTreeSet::new();
    for row in rows {
        let id = row["id"].as_str().expect("returned ID");
        assert!(ids.insert(id));
        let index: usize = row["slug"]
            .as_str()
            .unwrap()
            .strip_prefix("generated-atom-")
            .unwrap()
            .parse()
            .unwrap();
        assert!(index < count);
        assert_eq!(row["kind"], "atom");
        assert!(row["content"]
            .as_str()
            .unwrap()
            .contains(&format!("subject{index} ")));
        assert_eq!(row["score_provenance"]["embedding_rerank"], true);
        assert_eq!(
            row["status"],
            if mixed_drafts && index.is_multiple_of(2) {
                "draft"
            } else {
                "reviewed"
            }
        );
        if !include_drafts {
            assert_eq!(row["status"], "reviewed");
        }
    }
    if include_drafts {
        assert!(
            rows.iter().any(|row| row["status"] == "draft"),
            "actual draft inclusion"
        );
    }
    let has_ann = rows.iter().any(|row| {
        row["score_provenance"]["sources"]
            .as_array()
            .is_some_and(|sources| sources.iter().any(|source| source == "ann"))
    });
    if !mixed_drafts || include_drafts {
        assert!(has_ann, "real ANN candidates: {result}");
    } else if !has_ann {
        // The query points at the draft neighborhood. Status filtering and
        // final ranking can leave only lexical candidates in the returned top
        // 20. Stored-vector reranking remains mandatory below.
        assert!(
            rows.iter().all(|row| row["score_provenance"]["sources"]
                .as_array()
                .is_some_and(|sources| sources.iter().any(|source| source == "lexical"))),
            "reviewed candidates retain their lexical source: {result}"
        );
    }
    let provenance = &result["rerank_provenance"];
    assert_eq!(provenance["stored_vector_lookup"], "supported");
    assert!(provenance["from_stored"].as_u64().unwrap() >= 20);
    assert_eq!(provenance["embedded_fallback"], 0);
}
