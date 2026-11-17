use std::sync::{atomic::Ordering, Arc, Mutex};
use std::time::Duration;

use khive_pack_kg::KgPack;
use khive_runtime::{EmbedderProvider, Namespace, VerbRegistry, VerbRegistryBuilder};
use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
use serde_json::{json, Value};

use super::*;
use crate::handlers::common::{
    retrieval_failpoints, RecallCandidateParams, RecallCandidateSet, RecallLegs,
    RECALL_DIAGNOSTIC_SNIPPET_CHARS,
};
use crate::test_support::with_receipt_credentials;

const QUERY: &str = "zirconquartz";
const TARGET: &str = "zirconquartz target memory";
const DECOY: &str = "other needle memory";
const FOREIGN: &str = "zirconquartz foreign memory";

fn basis(index: usize) -> Vec<f32> {
    let mut v = vec![0.0; 8];
    v[index] = 1.0;
    v
}

#[derive(Clone)]
struct LiteralProvider {
    name: String,
    query_axis: usize,
    calls: Arc<Mutex<Vec<(String, String)>>>,
    overlap: Arc<Mutex<Vec<bool>>>,
    observer: Arc<TraceLog>,
    fail: bool,
}

#[async_trait]
impl EmbeddingService for LiteralProvider {
    async fn embed(
        &self,
        texts: &[String],
        _model: EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        if self.fail {
            return Err(EmbedError::ModelNotLoaded("literal fixture failure".into()));
        }
        let mut out = Vec::new();
        for text in texts {
            self.calls
                .lock()
                .unwrap()
                .push((self.name.clone(), text.clone()));
            if text == QUERY {
                self.overlap
                    .lock()
                    .unwrap()
                    .push(self.observer.text_entered.load(Ordering::SeqCst));
            }
            let axis = match text.as_str() {
                QUERY | TARGET => self.query_axis,
                FOREIGN => 2,
                DECOY => 1 - self.query_axis,
                _ => {
                    return Err(EmbedError::ModelNotLoaded(format!(
                        "unseeded literal text: {text}"
                    )))
                }
            };
            out.push(basis(axis));
        }
        Ok(out)
    }
    fn supports_model(&self, _model: EmbeddingModel) -> bool {
        true
    }
    fn name(&self) -> &'static str {
        "query-ir-literal"
    }
}

#[async_trait]
impl EmbedderProvider for LiteralProvider {
    fn name(&self) -> &str {
        &self.name
    }
    fn dimensions(&self) -> usize {
        8
    }
    async fn build(&self) -> Result<Arc<dyn EmbeddingService>, RuntimeError> {
        Ok(Arc::new(self.clone()))
    }
}

struct Fixture {
    runtime: KhiveRuntime,
    registry: VerbRegistry,
    pack: MemoryPack,
    target: String,
    decoy: String,
    foreign: String,
    receipt: Value,
    models: Vec<String>,
    calls: Arc<Mutex<Vec<(String, String)>>>,
    overlap: Arc<Mutex<Vec<bool>>>,
    log: Arc<TraceLog>,
}

impl Fixture {
    async fn new(model_count: usize) -> Self {
        let runtime = with_receipt_credentials(KhiveRuntime::memory().unwrap());
        let calls = Arc::new(Mutex::new(Vec::new()));
        let overlap = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::new(TraceLog::default());
        let models: Vec<String> = (0..model_count)
            .map(|i| format!("query-ir-model-{i}"))
            .collect();
        for (i, name) in models.iter().enumerate() {
            runtime.register_embedder(LiteralProvider {
                name: name.clone(),
                query_axis: i % 2,
                calls: calls.clone(),
                overlap: overlap.clone(),
                observer: log.clone(),
                fail: false,
            });
        }
        let mut pack = MemoryPack::new(runtime.clone());
        let memory = MemoryPack::new(runtime.clone());
        let ann = memory.ann_for_test();
        pack.ann = ann.clone();
        let mut locks = Vec::new();
        for model in &models {
            locks.push(
                ann::hold_model_warm_lock_for_test(&ann, &ann::AnnKey::from_token(model)).await,
            );
        }
        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(runtime.clone()));
        builder.register(memory);
        let registry = builder.build().unwrap();
        let mut writes = Vec::new();
        for (content, namespace) in [(TARGET, "local"), (DECOY, "local"), (FOREIGN, "foreign")] {
            writes.push(
                registry
                    .dispatch(
                        "memory.remember",
                        json!({
                            "content":content,"memory_type":"semantic","namespace":namespace
                        }),
                    )
                    .await
                    .unwrap(),
            );
        }
        drop(locks);
        let token = runtime.authorize(Namespace::local()).unwrap();
        for model in &models {
            tokio::time::timeout(Duration::from_secs(10), async {
                ann::ensure_ann_for_model(&runtime, &token, &ann, model)
                    .await
                    .unwrap();
                let key = ann::AnnKey::from_token(model);
                ann::wait_until_warm_idle(&ann, &key).await;
                assert!(
                    ann::is_current(&ann, &key).await,
                    "fixture ANN generation must be ready"
                );
            })
            .await
            .expect("fixture ANN readiness deadline");
        }
        calls.lock().unwrap().clear();
        overlap.lock().unwrap().clear();
        Self {
            runtime,
            registry,
            pack,
            models,
            calls,
            overlap,
            log,
            target: writes[0]["id"].as_str().unwrap().into(),
            decoy: writes[1]["id"].as_str().unwrap().into(),
            foreign: writes[2]["id"].as_str().unwrap().into(),
            receipt: writes[0]["visibility_token"].clone(),
        }
    }

    fn observer(&self) -> Observer {
        Observer {
            log: Some(self.log.clone()),
        }
    }

    async fn diagnostic(&self) -> Value {
        TRACE.scope(self.observer(), self.registry.dispatch("memory.recall_candidates", json!({
            "query": QUERY, "limit":2, "config":{"candidate_limit":2,"fts_gather":{"enabled":false}}
        }))).await.unwrap()
    }
}

fn children(plan: &QueryNode) -> &[QueryNode] {
    let QueryNode::Collect { children } = plan else {
        panic!("raw collection must use Collect: {plan:?}");
    };
    children
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial(background_tasks)]
#[serial_test::serial(config_ledger)]
async fn stored_diagnostic_plan_consumes_literal_inputs_and_preserves_raw_models() {
    let f = Fixture::new(2).await;
    let response = f.diagnostic().await;
    let plan: QueryNode = serde_json::from_value(response["plan"].clone()).unwrap();
    let nodes = children(&plan);
    let mut trace = f.log.entries.lock().unwrap().clone();
    trace.sort_by_key(|entry| entry["slot"].as_u64().unwrap());
    assert_eq!(
        trace.len(),
        3,
        "every real text/model call must pass through the shared interpreter adapter"
    );
    assert_eq!(
        nodes.len(),
        3,
        "collection must retain every declared raw stream"
    );
    assert!(
        matches!(&nodes[0], QueryNode::Keyword { text, top_k:2, min_score:None } if text==QUERY),
        "diagnostic keyword must be the literal executed query and bound"
    );
    assert_eq!(trace[0]["input"]["text"], QUERY);
    assert_eq!(
        trace[0]["input"]["snippet_chars"],
        RECALL_DIAGNOSTIC_SNIPPET_CHARS
    );
    assert_eq!(
        response["text_candidates"][0]["id"], f.target,
        "the actual keyword provider must search the literal target query"
    );
    assert!(
        response["text_candidates"][0]["snippet"]
            .as_str()
            .is_some_and(|snippet| snippet.contains(QUERY)),
        "diagnostic keyword collection must retain its requested snippet"
    );
    for (i, model) in f.models.iter().enumerate() {
        assert!(
            matches!(&nodes[i+1], QueryNode::Vector { embedding, top_k:2, min_score:None } if embedding==&basis(i)),
            "reported vector must equal its independently specified model embedding"
        );
        assert_eq!(
            trace[i + 1]["input"]["embedding"],
            json!(basis(i)),
            "actual provider input must use the model-specific literal vector"
        );
        assert_eq!(
            trace[i + 1]["input"]["model"],
            *model,
            "equal dimensions do not permit swapping model bindings"
        );
        assert_eq!(trace[i + 1]["limit"], 2);
        assert_eq!(
            trace[i + 1]["requested"],
            2,
            "logical request bound must remain ordinary k"
        );
        assert_eq!(
            trace[i + 1]["returned"],
            3,
            "raw accounting must retain the provider's overfetch population"
        );
        assert_eq!(trace[i + 1]["namespaces"], json!(["local"]));
        let rows = response["vector_candidates_per_model"][model]
            .as_array()
            .unwrap();
        assert_eq!(
            rows[0]["id"], f.target,
            "actual stored target must outrank the opposite-vector decoy in each model"
        );
        assert_eq!(rows[0]["rank"], 1);
        assert!(rows.iter().any(|row| row["id"] == f.decoy));
        let visible: Vec<_> = trace[i + 1]["rows"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|row| row["id"] != f.foreign)
            .cloned()
            .collect();
        assert_eq!(json!(rows), json!(visible), "raw vector labels and order must survive collection and the existing namespace filter exactly");
    }
    assert_eq!(
        response["text_candidates"], trace[0]["rows"],
        "raw text rank/title/snippet/score must survive collection exactly"
    );
    let flat = response["vector_candidates"].as_array().unwrap();
    assert_eq!(
        flat.iter().filter(|row| row["id"] == f.target).count(),
        2,
        "Collect must preserve repeated stored IDs across model streams"
    );
    for array in [response["text_candidates"].as_array().unwrap(), flat] {
        assert!(
            !array.iter().any(|row| row["id"] == f.foreign),
            "foreign memory must not enter diagnostic arrays"
        );
    }
    assert!(f
        .calls
        .lock()
        .unwrap()
        .iter()
        .all(|(_, text)| text == QUERY));
    assert_eq!(
        *f.overlap.lock().unwrap(),
        vec![true, true],
        "text work must start before the model preparation barrier completes"
    );
    let serving = f
        .registry
        .dispatch(
            "memory.recall",
            json!({"query":QUERY,"limit":2,"min_score":0.0}),
        )
        .await
        .unwrap();
    assert_eq!(
        serving.as_array().unwrap()[0]["id"],
        f.target,
        "ordinary serving must retain the stored target ranking"
    );
    assert!(
        serving
            .as_array()
            .unwrap()
            .iter()
            .all(|hit| hit.get("plan").is_none()),
        "serving must discard only the diagnostic plan"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial(background_tasks)]
#[serial_test::serial(config_ledger)]
async fn authenticated_session_collection_retains_wider_exact_bound_and_rows() {
    let f = Fixture::new(1).await;
    let fence = crate::visibility::parse_recall_visibility(
        &f.runtime,
        Some(&json!("session")),
        Some(&f.receipt),
        &["local"],
        &f.models,
    )
    .unwrap()
    .unwrap();
    let token = f.runtime.authorize(Namespace::local()).unwrap();
    let gather = RecallFtsGatherConfig::default();
    let collected = TRACE
        .scope(
            f.observer(),
            f.pack.collect_recall_candidates_with_plan(
                QUERY,
                &token,
                RecallCandidateParams {
                    candidate_limit: 1,
                    legs: RecallLegs {
                        text: false,
                        vector: true,
                    },
                    embedding_model: None,
                    session_fence: Some(&fence),
                    cjk_fts_bypass: false,
                    snippet_policy: TextSnippetPolicy::Omit,
                    fts_gather: &gather,
                    ann_overfetch_max_rounds: 1,
                    ann_ready_timeout_ms: 10,
                },
            ),
        )
        .await
        .unwrap();
    let nodes = children(&collected.plan);
    assert!(matches!(nodes[0], QueryNode::Empty));
    assert!(
        matches!(&nodes[1], QueryNode::Vector{top_k:33,embedding,..} if embedding==&basis(0)),
        "session leaf must carry the actual max(4k,k+32) bound, not ordinary k"
    );
    let rows = &collected.candidates.vector_hits_per_model[0].1;
    assert_eq!(
        rows.len(),
        2,
        "authenticated exact session read must retain more than ordinary k"
    );
    assert_eq!(rows[0].subject_id.to_string(), f.target);
    assert_eq!(rows[1].subject_id.to_string(), f.decoy);
    assert!(
        collected.candidates.session_unmet_models.is_empty(),
        "the candidate-producing exact read must prove its fence"
    );
    let trace = f.log.entries.lock().unwrap();
    assert_eq!(trace.len(), 1);
    assert_eq!(trace[0]["limit"], 33);
    assert_eq!(trace[0]["rows"].as_array().unwrap().len(), 2);
}

struct RetrievalFailure(String);
impl RetrievalFailure {
    fn new(model: &str) -> Self {
        retrieval_failpoints::fail_ann(model);
        retrieval_failpoints::fail_vec(model);
        Self(model.into())
    }
}
impl Drop for RetrievalFailure {
    fn drop(&mut self) {
        retrieval_failpoints::clear_ann(&self.0);
        retrieval_failpoints::clear_vec(&self.0);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial(background_tasks)]
#[serial_test::serial(config_ledger)]
async fn collection_preserves_zero_one_two_and_spawned_model_slots() {
    for count in [0, 1, 2, 3] {
        let f = Fixture::new(count).await;
        let response = f.diagnostic().await;
        let plan: QueryNode = serde_json::from_value(response["plan"].clone()).unwrap();
        assert_eq!(
            children(&plan).len(),
            count + 1,
            "every cardinality preserves declared model order"
        );
        let mut trace = f.log.entries.lock().unwrap().clone();
        trace.sort_by_key(|entry| entry["slot"].as_u64().unwrap());
        assert_eq!(
            trace.len(),
            count + 1,
            "spawned jobs must explicitly inherit the request-owned observer"
        );
        for (i, model) in f.models.iter().enumerate() {
            assert_eq!(trace[i + 1]["input"]["model"], *model);
            assert_eq!(trace[i + 1]["input"]["embedding"], json!(basis(i % 2)));
        }
        assert_eq!(
            *f.overlap.lock().unwrap(),
            vec![true; count],
            "text and embedding overlap must survive every scheduling branch"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial(background_tasks)]
#[serial_test::serial(config_ledger)]
async fn skipped_embedding_remains_empty_but_degraded_model_remains_completed() {
    let f = Fixture::new(2).await;
    f.runtime.register_embedder(LiteralProvider {
        name: "query-ir-model-z-failed".into(),
        query_axis: 0,
        calls: f.calls.clone(),
        overlap: f.overlap.clone(),
        observer: f.log.clone(),
        fail: true,
    });
    let _failure = RetrievalFailure::new(&f.models[0]);
    let token = f.runtime.authorize(Namespace::local()).unwrap();
    let gather = RecallFtsGatherConfig::default();
    let result = TRACE
        .scope(
            f.observer(),
            f.pack.collect_recall_candidates_with_plan(
                QUERY,
                &token,
                RecallCandidateParams {
                    candidate_limit: 2,
                    legs: RecallLegs::BOTH,
                    embedding_model: None,
                    session_fence: None,
                    cjk_fts_bypass: false,
                    snippet_policy: TextSnippetPolicy::Omit,
                    fts_gather: &gather,
                    ann_overfetch_max_rounds: 1,
                    ann_ready_timeout_ms: 10,
                },
            ),
        )
        .await
        .unwrap();
    let nodes = children(&result.plan);
    assert_eq!(
        nodes.len(),
        4,
        "withheld embedding must retain its declared Empty slot"
    );
    assert!(
        matches!(nodes[3], QueryNode::Empty),
        "failed embedding must not advertise a vector search"
    );
    assert!(
        matches!(nodes[1], QueryNode::Vector { .. }),
        "completed empty degraded search remains a Vector leaf"
    );
    let models = &result.candidates.vector_hits_per_model;
    assert_eq!(
        models
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        f.models.iter().map(String::as_str).collect::<Vec<_>>(),
        "skipped and completed-empty model membership must remain distinct"
    );
    assert!(models[0].1.is_empty());
    assert!(result.candidates.ann_degraded);
    assert!(result
        .candidates
        .ann_degraded_reason
        .as_ref()
        .is_some_and(|reason| reason.contains("per-model recall retrieval failed")));
    assert!(!models[1].1.is_empty());
    assert_eq!(
        f.log.entries.lock().unwrap().len(),
        3,
        "skipped slot must never invoke its provider"
    );
}

#[tokio::test]
async fn stopped_collection_cannot_become_a_healthy_empty_plan() {
    let runtime = KhiveRuntime::memory().unwrap();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let pack = MemoryPack::new(runtime);
    let gather = RecallFtsGatherConfig::default();
    let (_cancel, rx) = tokio::sync::watch::channel(true);
    let result = khive_storage::scope_request_read_cancellation(
        rx,
        pack.collect_recall_candidates_with_plan(
            QUERY,
            &token,
            RecallCandidateParams {
                candidate_limit: 2,
                legs: RecallLegs::BOTH,
                embedding_model: None,
                session_fence: None,
                cjk_fts_bypass: false,
                snippet_policy: TextSnippetPolicy::Omit,
                fts_gather: &gather,
                ann_overfetch_max_rounds: 1,
                ann_ready_timeout_ms: 10,
            },
        ),
    )
    .await;
    assert!(matches!(result,Err(RuntimeError::Storage(khive_storage::StorageError::Timeout { .. }))),
        "a stopped collection must retain its typed cancellation instead of an empty successful plan");
}

fn raw_projection(candidates: &RecallCandidateSet) -> Value {
    json!({
        "namespace":candidates.namespace,"visible":candidates.visible_namespaces,
        "text":candidates.text_hits.iter().map(|hit|json!({"id":hit.subject_id.to_string(),
            "score":hit.score.to_f64(),"rank":hit.rank,"title":hit.title,"snippet":hit.snippet})).collect::<Vec<_>>(),
        "vectors":candidates.vector_hits_per_model.iter().map(|(model,hits)|json!({"model":model,
            "rows":hits.iter().map(|hit|json!({"id":hit.subject_id.to_string(),"score":hit.score.to_f64(),"rank":hit.rank})).collect::<Vec<_>>()
        })).collect::<Vec<_>>(),
        "degraded":candidates.ann_degraded,"reason":candidates.ann_degraded_reason,
        "unmet":candidates.session_unmet_models,
    })
}

fn ordinary_options(gather: &RecallFtsGatherConfig) -> RecallCandidateParams<'_> {
    RecallCandidateParams {
        candidate_limit: 2,
        legs: RecallLegs::BOTH,
        embedding_model: None,
        session_fence: None,
        cjk_fts_bypass: false,
        snippet_policy: TextSnippetPolicy::Omit,
        fts_gather: gather,
        ann_overfetch_max_rounds: 1,
        ann_ready_timeout_ms: 10,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial(background_tasks)]
#[serial_test::serial(config_ledger)]
async fn ordinary_collector_projection_matches_existing_stored_provider_results() {
    let f = Fixture::new(2).await;
    let token = f.runtime.authorize(Namespace::local()).unwrap();
    let gather = RecallFtsGatherConfig::default();
    // Call the unchanged providers directly, independently of the interpreter.
    let text = f
        .pack
        .collect_recall_text_hits(
            &token,
            QUERY,
            &["local".into()],
            2,
            TextSnippetPolicy::Omit,
            false,
            &gather,
        )
        .await
        .unwrap();
    let mut vectors = Vec::new();
    for (i, model) in f.models.iter().enumerate() {
        let result = collect_model_ann_hits(
            &f.runtime,
            &f.pack.ann,
            &token,
            "local",
            &["local".into()],
            model.clone(),
            basis(i),
            2,
            34,
            1,
            10,
            None,
        )
        .await
        .unwrap();
        assert!(!result.degraded, "provider baseline must be healthy");
        vectors.push((model.clone(), result.hits));
    }
    assert!(
        vectors.iter().all(|(_, hits)| hits.len() > 2),
        "the unchanged stored provider baseline must exceed ordinary k before collection"
    );
    let expected = json!({"namespace":"local","visible":["local"],
        "text":text.iter().map(|hit|json!({"id":hit.subject_id.to_string(),"score":hit.score.to_f64(),
            "rank":hit.rank,"title":hit.title,"snippet":hit.snippet})).collect::<Vec<_>>(),
        "vectors":vectors.iter().map(|(model,hits)|json!({"model":model,
            "rows":hits.iter().map(|hit|json!({"id":hit.subject_id.to_string(),"score":hit.score.to_f64(),"rank":hit.rank})).collect::<Vec<_>>()
        })).collect::<Vec<_>>(),"degraded":false,"reason":null,"unmet":[]});
    let planned_result = f
        .pack
        .collect_recall_candidates_with_plan(QUERY, &token, ordinary_options(&gather))
        .await;
    assert!(planned_result.is_ok(),
        "raw Collect must accept the existing provider overfetch instead of imposing scalar output limits");
    let planned = planned_result.unwrap();
    let serving = f
        .pack
        .collect_recall_candidates(QUERY, &token, ordinary_options(&gather))
        .await
        .unwrap();
    assert_eq!(
        raw_projection(&planned.candidates),
        expected,
        "interpreted collection must preserve the complete existing provider projection"
    );
    assert_eq!(
        raw_projection(&serving),
        expected,
        "ordinary collector must discard only the plan and preserve every raw label and model slot"
    );
    assert_eq!(serving.text_hits[0].subject_id.to_string(), f.target);
    assert!(
        serving.text_hits.iter().all(|hit| hit.snippet.is_none()),
        "ordinary recall still omits snippets"
    );
    assert!(
        serving
            .vector_hits_per_model
            .iter()
            .all(|(_, hits)| hits.len() > 2),
        "the stored warm baseline must actually exceed ordinary k before collection"
    );
}

fn install_gates(f: &Fixture) -> Vec<Arc<ProviderGate>> {
    const ERRORS: [&str; 3] = [
        "first model refusal",
        "second model refusal",
        "third model refusal",
    ];
    (0..f.models.len())
        .map(|index| {
            let gate = Arc::new(ProviderGate {
                entered: tokio::sync::Notify::new(),
                release: Semaphore::new(0),
                exited: tokio::sync::Notify::new(),
                error: ERRORS[index],
            });
            f.log.gates.lock().unwrap().insert(index + 1, gate.clone());
            gate
        })
        .collect()
}

async fn gate_signal(signal: &tokio::sync::Notify) {
    tokio::time::timeout(Duration::from_secs(5), signal.notified())
        .await
        .expect("fixture gate deadline");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial(background_tasks)]
#[serial_test::serial(config_ledger)]
async fn two_and_spawned_model_errors_keep_model_order_after_later_failure() {
    for count in [2, 3] {
        let f = Fixture::new(count).await;
        let gates = install_gates(&f);
        let observer = f.observer();
        let token = f.runtime.authorize(Namespace::local()).unwrap();
        let task = tokio::spawn(async move {
            let gather = RecallFtsGatherConfig::default();
            TRACE
                .scope(
                    observer,
                    f.pack.collect_recall_candidates_with_plan(
                        QUERY,
                        &token,
                        ordinary_options(&gather),
                    ),
                )
                .await
        });
        for gate in &gates {
            gate_signal(&gate.entered).await;
        }
        // Later model failures are fully settled while the first model remains blocked.
        for gate in gates.iter().skip(1).rev() {
            gate.release.add_permits(1);
            gate_signal(&gate.exited).await;
        }
        tokio::task::yield_now().await;
        let returned_before_first = task.is_finished();
        gates[0].release.add_permits(1);
        let result = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("collector deadline")
            .expect("collector task");
        assert!(matches!(result,Err(RuntimeError::InvalidInput(ref message)) if message=="first model refusal"),
            "two-model join and spawned ordered-await must return the first model's exact typed error");
        assert!(
            !returned_before_first,
            "later error must not bypass the first model's ordered await"
        );
        gate_signal(&gates[0].exited).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial(background_tasks)]
#[serial_test::serial(config_ledger)]
async fn cancellation_of_spawned_collection_drops_every_blocked_leaf() {
    let f = Fixture::new(3).await;
    let gates = install_gates(&f);
    let observer = f.observer();
    let token = f.runtime.authorize(Namespace::local()).unwrap();
    let (cancel, rx) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(async move {
        let gather = RecallFtsGatherConfig::default();
        TRACE
            .scope(
                observer,
                khive_storage::scope_request_read_cancellation(
                    rx,
                    f.pack.collect_recall_candidates_with_plan(
                        QUERY,
                        &token,
                        ordinary_options(&gather),
                    ),
                ),
            )
            .await
    });
    for gate in &gates {
        gate_signal(&gate.entered).await;
    }
    cancel.send(true).unwrap();
    let result = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("stop deadline")
        .expect("collector task");
    assert!(
        matches!(
            result,
            Err(RuntimeError::Storage(
                khive_storage::StorageError::Timeout { .. }
            ))
        ),
        "request stop must remain a typed timeout while all model leaves are in flight"
    );
    for gate in &gates {
        gate_signal(&gate.exited).await;
    }
    assert!(
        gates
            .iter()
            .all(|gate| gate.release.available_permits() == 0),
        "cancellation must drop every blocked model future without releasing its provider gate"
    );
}
