#[path = "../../khive-runtime/tests/support/receipt_credentials.rs"]
mod receipt_credentials;

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use async_trait::async_trait;
use khive_pack_kg::KgPack;
use khive_pack_memory::{config::RecallConfig, MemoryPack};
use khive_runtime::{
    EmbedderProvider, FusionStrategy, KhiveRuntime, Namespace, RuntimeConfig, RuntimeError,
    VerbRegistryBuilder,
};
use khive_storage::{Note, TextDocument};
use khive_types::SubstrateKind;
use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
use serde_json::json;
use uuid::Uuid;

struct QueryEmbeddings {
    dims: usize,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl EmbeddingService for QueryEmbeddings {
    async fn embed(
        &self,
        texts: &[String],
        _model: EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        self.calls.fetch_add(texts.len(), Ordering::SeqCst);
        Ok(texts.iter().map(|_| vec![1.0; self.dims]).collect())
    }

    fn supports_model(&self, _model: EmbeddingModel) -> bool {
        true
    }
    fn name(&self) -> &'static str {
        "short-token-query-fixture"
    }
}

struct QueryProvider {
    name: String,
    dims: usize,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl EmbedderProvider for QueryProvider {
    fn name(&self) -> &str {
        &self.name
    }
    fn dimensions(&self) -> usize {
        self.dims
    }
    async fn build(&self) -> Result<Arc<dyn EmbeddingService>, RuntimeError> {
        Ok(Arc::new(QueryEmbeddings {
            dims: self.dims,
            calls: Arc::clone(&self.calls),
        }))
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn default_public_recall_promotes_type_two_without_a_semantic_or_recency_winner() {
    assert!(matches!(
        RecallConfig::default().fuse_strategy,
        FusionStrategy::Rrf { k: 10 }
    ));
    let model = EmbeddingModel::AllMiniLmL6V2;
    let model_name = model.to_string();
    let dims = model.dimensions();
    let runtime = receipt_credentials::with_receipt_credentials(
        KhiveRuntime::new(RuntimeConfig {
            db_path: None,
            embedding_model: Some(model),
            additional_embedding_models: vec![],
            actor_id: Some("short-token-recall-fixture".into()),
            brain_profile: None,
            events_split: None,
            volume_lock_dir: None,
            wal_ceiling_bytes: 0,
            wal_ceiling_configured_bytes: 0,
            wal_ceiling_source: khive_runtime::WalCeilingSource::Default,
            wal_ceiling_env_raw: None,
            disk_guard_environment: Default::default(),
            disk_guard_config: None,
            credentials: vec![],
            visibility_receipts: None,
            mounts: vec![],
            packs: vec!["kg".into(), "memory".into()],
            ..RuntimeConfig::default()
        })
        .expect("private memory runtime with embeddings enabled"),
    );
    assert!(runtime.backend_data_dir().is_none());
    assert!(runtime.backend_ann_root().is_none());
    let calls = Arc::new(AtomicUsize::new(0));
    runtime
        .try_register_embedder(QueryProvider {
            name: model_name.clone(),
            dims,
            calls: Arc::clone(&calls),
        })
        .expect("replace the unresolved default model with a deterministic provider");
    assert!(runtime
        .registered_embedding_model_names()
        .contains(&model_name));
    let mut builder = VerbRegistryBuilder::new();
    builder.with_actor_id(Some("short-token-recall-fixture".into()));
    builder.register(KgPack::new(runtime.clone()));
    builder.register(MemoryPack::new(runtime.clone()));
    let registry = builder.build().expect("actual default MemoryPack registry");
    let token = runtime.authorize(Namespace::local()).unwrap();
    let notes = runtime.notes(&token).unwrap();
    let text = runtime.text_for_notes(&token).unwrap();
    let now = chrono::Utc::now();
    let first = Uuid::from_u128(1);
    let second = Uuid::from_u128(2);
    let mut expected_notes = Vec::new();
    // Seed only notes and their text rows: neither record owns a vector advantage.
    // Equal metadata removes recency, salience, decay and profile tiebreaks.
    for (id, title, content) in [
        (
            first,
            Some("diabetes"),
            "type 1 diabetes with chronic kidney disease",
        ),
        (second, None, "type 2 diabetes with chronic kidney disease"),
    ] {
        let mut note = Note::new("local", "memory", content);
        note.id = id;
        note.name = title.map(str::to_string);
        note.salience = Some(0.5);
        note.decay_factor = Some(0.005);
        note.created_at = now.timestamp_micros();
        note.updated_at = note.created_at;
        note.properties = Some(json!({"memory_type":"semantic"}));
        notes.upsert_note(note.clone()).await.unwrap();
        text.upsert_document(TextDocument {
            subject_id: id,
            kind: SubstrateKind::Note,
            record_kind: Some("memory".into()),
            namespace: "local".into(),
            title: note.name.clone(),
            body: content.into(),
            tags: vec![],
            metadata: None,
            updated_at: now,
        })
        .await
        .unwrap();
        expected_notes.push(note);
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "fixture seeding must not create semantic vectors"
    );
    let legacy_ids: Vec<String> = {
        let pool = runtime.backend().pool().clone();
        let writer = pool.writer().unwrap();
        let mut stmt = writer
            .conn()
            .prepare(
                "SELECT subject_id FROM fts_notes WHERE fts_notes MATCH ?1 \
             AND rank MATCH 'bm25(0.0, 0.0, 1.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0)' \
             AND namespace = 'local' AND record_kind = 'memory' ORDER BY rank LIMIT 150",
            )
            .unwrap();
        let rows = stmt
            .query_map(["type 2 diabetes with chronic kidney disease"], |row| {
                row.get(0)
            })
            .unwrap();
        rows.collect::<Result<_, _>>().unwrap()
    };
    assert_eq!(
        legacy_ids,
        vec![first.to_string(), second.to_string()],
        "old Plain BM25 must favor type1; the regression cannot be won by fixture ordering alone"
    );
    let result = registry
        .dispatch(
            "memory.recall",
            json!({
                "query":"type 2 diabetes with chronic kidney disease"
            }),
        )
        .await
        .expect("the real public recall must succeed with its default fusion policy");
    let hits = result
        .as_array()
        .expect("default recall returns its ordinary hit array");
    assert_eq!(hits.len(), 2, "both memory records remain eligible");
    assert!(
        calls.load(Ordering::SeqCst) > 0,
        "default semantic query attempt must remain enabled"
    );
    for hit in hits {
        assert!(
            hit.get("degraded").is_none(),
            "an ANN failure cannot stand in for healthy default recall: {hit}"
        );
        assert!(hit.get("degraded_reason").is_none());
        assert!(
            hit.get("truncated").is_none(),
            "no partial/degraded acceptance"
        );
        assert!(hit["score"].as_f64().unwrap().is_finite());
    }
    assert_eq!(
        hits[0]["full_id"],
        second.to_string(),
        "default public memory.recall must put type2 first"
    );
    assert_eq!(hits[1]["full_id"], first.to_string());
    for note in expected_notes {
        assert_eq!(
            notes.get_note(note.id).await.unwrap().unwrap(),
            note,
            "recall must not rewrite either stored fixture"
        );
    }
}
