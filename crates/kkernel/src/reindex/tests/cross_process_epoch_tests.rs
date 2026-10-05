use super::*;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use khive_pack_kg::KgPack;
use khive_pack_memory::MemoryPack;
use khive_runtime::{NamespaceToken, VerbRegistry, VerbRegistryBuilder};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tracing_subscriber::prelude::*;

pub(super) struct FixedReindexEmbedder {
    pub(super) name: String,
    pub(super) dimensions: usize,
}

#[async_trait::async_trait]
impl khive_runtime::EmbedderProvider for FixedReindexEmbedder {
    fn name(&self) -> &str {
        &self.name
    }

    fn dimensions(&self) -> usize {
        self.dimensions
    }

    async fn build(
        &self,
    ) -> Result<std::sync::Arc<dyn lattice_embed::EmbeddingService>, khive_runtime::RuntimeError>
    {
        Ok(std::sync::Arc::new(FixedReindexEmbeddingService(
            self.dimensions,
        )))
    }
}

struct FixedReindexEmbeddingService(usize);

#[async_trait::async_trait]
impl lattice_embed::EmbeddingService for FixedReindexEmbeddingService {
    async fn embed(
        &self,
        texts: &[String],
        _model: lattice_embed::EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, lattice_embed::EmbedError> {
        Ok(texts.iter().map(|_| vec![1.0_f32; self.0]).collect())
    }

    fn supports_model(&self, _model: lattice_embed::EmbeddingModel) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "reindex-test"
    }
}

const MODEL: &str = "epoch_process_test_model";
const DIMS: usize = 8;
const REINDEX_CHILD: &str = "KKERNEL_EPOCH_REINDEX_CHILD";
const FIXTURE_DIR: &str = "KKERNEL_EPOCH_FIXTURE_DIR";
const QUERY: &str = "epoch1507queryonlyqxz";
const SEEDS: [&str; 4] = [
    "epoch1507seedalpha",
    "epoch1507seedbravo",
    "epoch1507seedcharlie",
    "epoch1507seeddelta",
];
const WARM_SEARCH: &str = "memory recall via warm ANN";
const BACKGROUND_DONE: &str = "memory ANN background warm complete";

struct RotatingProvider(bool);
struct RotatingService(bool);

#[async_trait::async_trait]
impl khive_runtime::EmbedderProvider for RotatingProvider {
    fn name(&self) -> &str {
        MODEL
    }

    fn dimensions(&self) -> usize {
        DIMS
    }

    async fn build(
        &self,
    ) -> Result<Arc<dyn lattice_embed::EmbeddingService>, khive_runtime::RuntimeError> {
        Ok(Arc::new(RotatingService(self.0)))
    }
}

#[async_trait::async_trait]
impl lattice_embed::EmbeddingService for RotatingService {
    async fn embed(
        &self,
        texts: &[String],
        _model: lattice_embed::EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, lattice_embed::EmbedError> {
        Ok(texts
            .iter()
            .map(|text| {
                let coordinate = if text.contains(QUERY) {
                    0
                } else {
                    let seed = SEEDS
                        .iter()
                        .position(|marker| text.contains(marker))
                        .expect("embedding input must contain a fixture marker");
                    if self.0 && seed < 2 {
                        1 - seed
                    } else {
                        seed
                    }
                };
                let mut vector = vec![0.0; DIMS];
                vector[coordinate] = 1.0;
                vector
            })
            .collect())
    }

    fn supports_model(&self, _model: lattice_embed::EmbeddingModel) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "epoch-process-test"
    }
}

#[derive(Clone, Debug, Default)]
struct ObservedEvent {
    message: String,
    model: String,
    status: String,
}

impl tracing::field::Visit for ObservedEvent {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.record_str(field, &format!("{value:?}"));
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        match field.name() {
            "message" => self.message = value.to_owned(),
            "model" => self.model = value.to_owned(),
            "status" => self.status = value.to_owned(),
            _ => {}
        }
    }
}

struct EpochObserver {
    events: Arc<Mutex<Vec<ObservedEvent>>>,
    completed: mpsc::UnboundedSender<ObservedEvent>,
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for EpochObserver {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _context: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut observed = ObservedEvent::default();
        event.record(&mut observed);
        if observed.model == MODEL {
            self.events.lock().unwrap().push(observed.clone());
            if observed.message == BACKGROUND_DONE
                || observed.message == "memory ANN background build failed"
                || observed.message == "memory ANN background warm cancelled at shutdown"
            {
                let _ = self.completed.send(observed);
            }
        }
    }
}

async fn vector_rows(rt: &KhiveRuntime) -> Vec<(String, String, String)> {
    let sql = rt.sql();
    let mut reader = sql.reader().await.expect("corpus reader");
    reader
        .query_all(SqlStatement {
            sql: "SELECT v.subject_id, n.content, hex(v.embedding) AS vector_hex, \
                  length(v.embedding) AS vector_bytes FROM vec_epoch_process_test_model v \
                  JOIN notes n ON n.id = v.subject_id WHERE v.embedding_model = ?1 \
                  ORDER BY v.subject_id"
                .into(),
            params: vec![SqlValue::Text(MODEL.into())],
            label: Some("epoch_fixture_corpus".into()),
        })
        .await
        .expect("corpus rows")
        .into_iter()
        .map(|row| {
            assert_eq!(row.i64("vector_bytes").unwrap(), (DIMS * 4) as i64);
            (
                row.text("subject_id").unwrap().to_owned(),
                row.text("content").unwrap().to_owned(),
                row.text("vector_hex").unwrap().to_owned(),
            )
        })
        .collect()
}

async fn warm_events(rt: &KhiveRuntime, token: &NamespaceToken) -> Vec<khive_storage::Event> {
    rt.events(token)
        .expect("event store")
        .query_events(
            khive_storage::EventFilter {
                verbs: vec!["memory.ann_warm".into()],
                kinds: vec![khive_types::EventKind::PhaseCompleted],
                ..Default::default()
            },
            khive_storage::types::PageRequest {
                limit: 128,
                offset: 0,
            },
        )
        .await
        .expect("warm completion events")
        .items
}

async fn recall_warm(registry: &VerbRegistry, events: &Mutex<Vec<ObservedEvent>>) -> Value {
    let start = events.lock().unwrap().len();
    let candidates = registry
        .dispatch(
            "memory.recall_candidates",
            json!({"query": QUERY, "limit": 4, "embedding_model": MODEL}),
        )
        .await
        .expect("resident recall candidates");
    let observations = events.lock().unwrap();
    let query_events = &observations[start..];
    assert_eq!(
        query_events
            .iter()
            .filter(|event| event.message == WARM_SEARCH)
            .count(),
        1,
        "query must search the installed ANN graph: {query_events:?}"
    );
    assert!(
        !query_events.iter().any(
            |event| event.message == "memory recall via exact sqlite-vec"
                || event.message == "memory ANN ensured on recall miss"
                || event.message.contains("falling back")
                || event.message.contains("failed")
        ),
        "exact fallback, a cold build or an error cannot prove warm invalidation: {query_events:?}"
    );
    candidates
}

fn assert_nearest(candidates: &Value, ids: &[Uuid], expected: Uuid) {
    let hits = candidates["vector_candidates"]
        .as_array()
        .expect("vector candidates");
    assert_eq!(
        hits.len(),
        ids.len(),
        "all seeded vectors must remain candidates"
    );
    let mut found: Vec<_> = hits.iter().map(|hit| hit["id"].as_str().unwrap()).collect();
    found.sort_unstable();
    let mut wanted: Vec<_> = ids.iter().map(Uuid::to_string).collect();
    wanted.sort_unstable();
    assert_eq!(found, wanted);
    let mut scored: Vec<_> = hits
        .iter()
        .map(|hit| {
            let score = hit["score"].as_f64().expect("vector score");
            assert!(score.is_finite());
            (hit["id"].as_str().unwrap(), score)
        })
        .collect();
    scored.sort_by(|left, right| right.1.total_cmp(&left.1));
    assert_eq!(
        scored[0].0,
        expected.to_string(),
        "reindex epoch must replace the resident warm ANN neighbor; candidates={hits:?}"
    );
    assert!(
        scored[0].1 - scored[1].1 > 0.4,
        "nearest neighbor must be unambiguous: {scored:?}"
    );
}

async fn reindex_child() {
    assert!(!khive_storage::test_support::run_exact_test_in_child(
        REINDEX_CHILD,
        false,
        |_| {}
    ));
    let dir =
        std::path::PathBuf::from(std::env::var_os(FIXTURE_DIR).expect("private fixture path"));
    let mut args = snapshot_reindex_args(&dir, false);
    args.model = Some(MODEL.into());
    args.batch_size = 4;
    args.no_sections = true;
    run_reindex_with_setup(
        args,
        |mut config| {
            config.embedding_model = None;
            config.additional_embedding_models.clear();
            config
        },
        |rt| {
            assert!(rt.registered_embedding_model_names().is_empty());
            rt.register_embedder(RotatingProvider(true));
            Ok(())
        },
    )
    .await
    .expect("actual child-process reindex pipeline");
    std::fs::write(
        dir.join("child-complete.json"),
        serde_json::to_vec(&json!({
            "pid": std::process::id(),
            "db": dir.join("reindex.db").canonicalize().unwrap(),
            "model": MODEL,
        }))
        .unwrap(),
    )
    .expect("child completion receipt");
}

#[tokio::test]
async fn real_process_reindex_refreshes_resident_warm_ann_without_shape_change() {
    if crate::test_process::run_in_child() {
        return;
    }
    if std::env::var_os(REINDEX_CHILD).is_some() {
        reindex_child().await;
        return;
    }
    let dir = tempfile::tempdir().expect("epoch process fixture");
    let args = snapshot_reindex_args(dir.path(), false);
    let rt = snapshot_test_runtime(&args).with_ann_fresh_tail_enabled(false);
    assert!(rt.registered_embedding_model_names().is_empty());
    rt.register_embedder(RotatingProvider(false));
    let token = rt.authorize(Namespace::local()).expect("local token");
    let mut ids = Vec::new();
    for seed in SEEDS {
        let note = rt
            .create_note(&token, "memory", None, seed, Some(0.7), None, vec![])
            .await
            .expect("seed memory");
        ids.push(note.id);
    }
    let events = Arc::new(Mutex::new(Vec::new()));
    let (completed_tx, mut completed_rx) = mpsc::unbounded_channel();
    tracing::subscriber::set_global_default(
        tracing_subscriber::registry()
            .with(tracing_subscriber::filter::LevelFilter::DEBUG)
            .with(EpochObserver {
                events: Arc::clone(&events),
                completed: completed_tx,
            }),
    )
    .expect("one observer in the isolated resident process");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(MemoryPack::new(rt.clone()));
    let registry = builder.build().expect("resident registry");
    registry
        .apply_schema_plans_with_map(&std::collections::HashMap::new(), rt.backend())
        .expect("pack schema plans");
    // Startup warm leaves the first production five-second epoch check due.
    registry.call_warm_all().await;
    assert!(
        events.lock().unwrap().iter().any(|event| {
            event.message == "memory ANN warm complete" && event.status == "Built { vectors: 4 }"
        }),
        "all four vectors must be installed before starting the reindex child"
    );
    let before_events = warm_events(&rt, &token).await;
    assert_eq!(before_events.len(), 1);
    assert_eq!(before_events[0].payload["path"], "full_build");
    let before_epoch = snapshot_test_count(
        &rt,
        "SELECT COALESCE(MAX(epoch), 0) AS n FROM memory_ann_epoch",
    )
    .await;
    let before = vector_rows(&rt).await;
    assert_eq!(before.len(), 4);
    let old_a = before
        .iter()
        .find(|row| row.0 == ids[0].to_string())
        .unwrap();
    let old_b = before
        .iter()
        .find(|row| row.0 == ids[1].to_string())
        .unwrap();
    assert_eq!(old_a.2, format!("0000803F{}", "00000000".repeat(DIMS - 1)));
    assert_eq!(
        old_b.2,
        format!("000000000000803F{}", "00000000".repeat(DIMS - 2))
    );

    assert!(khive_storage::test_support::run_exact_test_in_child(
        REINDEX_CHILD,
        false,
        |command| {
            command.env(FIXTURE_DIR, dir.path());
        }
    ));
    let child: Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("child-complete.json")).unwrap())
            .unwrap();
    assert_ne!(
        child["pid"].as_u64().unwrap(),
        u64::from(std::process::id())
    );
    assert_eq!(
        child["db"],
        json!(dir.path().join("reindex.db").canonicalize().unwrap())
    );
    assert_eq!(child["model"], MODEL);
    let after = vector_rows(&rt).await;
    assert_eq!(before.len(), after.len());
    for (old, new) in before.iter().zip(&after) {
        assert_eq!((&old.0, &old.1), (&new.0, &new.1));
        if old.1 == SEEDS[0] || old.1 == SEEDS[1] {
            let expected = if old.1 == SEEDS[0] {
                &old_b.2
            } else {
                &old_a.2
            };
            assert_eq!(
                &new.2, expected,
                "child must swap the distinguishing vectors"
            );
            assert_ne!(old.2, new.2);
        } else {
            assert_eq!(old.2, new.2);
        }
    }

    println!(
        "EPOCH_PROCESS_FIXTURE={}",
        json!({
            "stage": "reindex-written",
            "resident_pid": std::process::id(),
            "child_pid": child["pid"],
            "ids": ids,
            "shape_unchanged": true,
            "vectors_swapped": true,
        })
    );

    // ADR-107 permits this triggering query to serve the old installed graph.
    let triggering = recall_warm(&registry, &events).await;
    let completion = tokio::time::timeout(Duration::from_secs(30), completed_rx.recv()).await;
    let refreshed = recall_warm(&registry, &events).await;
    // Even an omitted epoch signal must fail on stale results, not only a timeout.
    assert_nearest(&refreshed, &ids, ids[1]);
    assert!(
        matches!(&completion, Ok(Some(event)) if event.message == BACKGROUND_DONE
        && event.status == "Built { vectors: 4 }"),
        "replacement must finish; completion={completion:?}, triggering={triggering:?}"
    );
    let after_events = warm_events(&rt, &token).await;
    let added: Vec<_> = after_events
        .iter()
        .filter(|event| event.id != before_events[0].id)
        .collect();
    assert_eq!(added.len(), 1, "one resident full rebuild after the child");
    assert_eq!(added[0].payload["path"], "full_build");
    assert_eq!(added[0].payload["ops_applied"], 0);
    assert_eq!(
        snapshot_test_count(
            &rt,
            "SELECT COALESCE(MAX(epoch), 0) AS n FROM memory_ann_epoch"
        )
        .await,
        before_epoch + 2,
        "real reindex must publish begin and completion epochs"
    );
}
