//! A keyed replay keeps its prepared embedding disclosure without new stored rows.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use khive_runtime::embedder_registry::EmbedderProvider;
use khive_runtime::keyed_memory::{create_keyed_memory_with_receipt_and_report, KeyedMemorySpec};
use khive_runtime::{KhiveRuntime, RuntimeConfig, RuntimeResult};
use khive_storage::{SqlStatement, SqlValue};
use khive_types::Namespace;
use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
use serde_json::json;

const MODEL: &str = "all-minilm-l6-v2";

#[derive(Default)]
struct EmbeddingProbe {
    calls: AtomicUsize,
    inputs: Mutex<Vec<String>>,
}

struct CountingService(Arc<EmbeddingProbe>);

#[async_trait]
impl EmbeddingService for CountingService {
    async fn embed(
        &self,
        texts: &[String],
        model: EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        assert_eq!(model, EmbeddingModel::AllMiniLmL6V2);
        self.0.calls.fetch_add(1, Ordering::SeqCst);
        self.0
            .inputs
            .lock()
            .expect("embedding probe lock")
            .extend(texts.iter().cloned());
        Ok(texts
            .iter()
            .map(|_| vec![0.25; model.dimensions()])
            .collect())
    }

    async fn embed_passage(
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
        MODEL
    }
}

struct CountingProvider(Arc<EmbeddingProbe>);

#[async_trait]
impl EmbedderProvider for CountingProvider {
    fn name(&self) -> &str {
        MODEL
    }

    fn dimensions(&self) -> usize {
        EmbeddingModel::AllMiniLmL6V2.dimensions()
    }

    async fn build(&self) -> RuntimeResult<Arc<dyn EmbeddingService>> {
        Ok(Arc::new(CountingService(Arc::clone(&self.0))))
    }
}

fn spec<'a>(content: &'a str) -> KeyedMemorySpec<'a> {
    KeyedMemorySpec {
        content,
        key: "same-content-replay",
        salience: 0.7,
        decay_factor: 0.95,
        properties: json!({"memory_type": "episodic"}),
        source_id: None,
        embedding_model: Some(MODEL),
    }
}

async fn count(runtime: &KhiveRuntime, sql: &str) -> i64 {
    let mut reader = runtime.sql().reader().await.expect("SQL reader");
    match reader
        .query_scalar(SqlStatement {
            sql: sql.into(),
            params: vec![SqlValue::Text("local".into())],
            label: Some("keyed-replay-disclosure-row-count".into()),
        })
        .await
        .expect("count stored rows")
    {
        Some(SqlValue::Integer(value)) => value,
        other => panic!("unexpected count: {other:?}"),
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn identical_keyed_memory_replay_keeps_the_computed_truncation_report() {
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        embedding_model: Some(EmbeddingModel::AllMiniLmL6V2),
        additional_embedding_models: vec![],
        ..RuntimeConfig::default()
    })
    .expect("in-memory runtime");
    runtime.install_kind_registry(vec![], vec!["memory".into()]);
    let token = runtime.authorize(Namespace::local()).expect("local token");
    let probe = Arc::new(EmbeddingProbe::default());
    runtime.register_embedder(CountingProvider(Arc::clone(&probe)));
    assert_eq!(runtime.default_embedder_name(), MODEL);
    assert_eq!(
        runtime
            .resolve_embedding_model(Some(MODEL))
            .expect("configured model"),
        EmbeddingModel::AllMiniLmL6V2
    );
    assert_eq!(probe.calls.load(Ordering::SeqCst), 0);
    let content = "x".repeat(lattice_embed::MAX_TEXT_BYTES + 1);

    let (first, edge, replayed, original_fences, first_report) =
        create_keyed_memory_with_receipt_and_report(&runtime, &token, spec(&content))
            .await
            .expect("fresh keyed memory");
    assert!(!replayed);
    assert!(edge.is_none());
    assert_eq!(first.content, content, "the full source text is stored");
    assert_eq!(first_report.truncated, 1);
    assert_eq!(first_report.discarded_bytes, 1);
    assert_eq!(original_fences.len(), 1);
    assert_eq!(original_fences[0].0, MODEL);
    assert!(original_fences[0].1 > 0);
    assert_eq!(probe.calls.load(Ordering::SeqCst), 1);

    let (holder, edge, replayed, replay_fences, replay_report) =
        create_keyed_memory_with_receipt_and_report(&runtime, &token, spec(&content))
            .await
            .expect("identical keyed replay");
    assert!(replayed);
    assert!(edge.is_none());
    assert_eq!(holder.id, first.id);
    assert_eq!(holder.content, content);
    assert_eq!(replay_fences, original_fences);
    assert_eq!(
        probe.calls.load(Ordering::SeqCst),
        2,
        "the replay's actual preparation calls the embedding service"
    );
    let observed_inputs = probe.inputs.lock().expect("embedding probe lock").clone();
    let bounded_input = "x".repeat(lattice_embed::MAX_TEXT_BYTES);
    assert_eq!(
        observed_inputs,
        vec![bounded_input.clone(), bounded_input],
        "both preparations send the same bounded document input"
    );
    for sql in [
        "SELECT COUNT(*) FROM notes WHERE namespace = ?1",
        "SELECT COUNT(*) FROM fts_notes WHERE namespace = ?1",
        "SELECT COUNT(*) FROM ann_write_log WHERE namespace = ?1",
        "SELECT COUNT(*) FROM memory_visibility_receipts WHERE namespace = ?1",
        "SELECT COUNT(*) FROM memory_visibility_fences WHERE namespace = ?1",
    ] {
        assert_eq!(count(&runtime, sql).await, 1, "no replay rows for {sql}");
    }
    assert_eq!(
        runtime
            .vectors_for_model(&token, MODEL)
            .expect("vector store")
            .count()
            .await
            .expect("count vectors"),
        1
    );
    assert_eq!(
        replay_report, first_report,
        "an identical-content replay must keep its computed truncation report"
    );
}
