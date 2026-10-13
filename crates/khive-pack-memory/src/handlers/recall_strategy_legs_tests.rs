//! A fusion strategy that discards an arm's hits never runs that arm, and
//! `include_timings` reports which stages a recall actually entered.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use khive_pack_kg::KgPack;
use khive_runtime::{EmbedderProvider, KhiveRuntime, VerbRegistry, VerbRegistryBuilder};
use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
use serde_json::{json, Value};
use serial_test::serial;

use crate::ann::SharedAnn;
use crate::MemoryPack;

const DIMS: usize = 16;

/// Deterministic vectors whose every request is counted: note writes and
/// query embeddings alike.
struct CountingVecService {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl EmbeddingService for CountingVecService {
    async fn embed(
        &self,
        texts: &[String],
        _model: EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        self.calls.fetch_add(texts.len(), Ordering::SeqCst);
        Ok(texts
            .iter()
            .map(|text| {
                let mut v = vec![0.0_f32; DIMS];
                v[0] = 1.0;
                v[1 + text.len() % (DIMS - 1)] = 0.5;
                v
            })
            .collect())
    }

    fn supports_model(&self, _model: EmbeddingModel) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "counting-vec"
    }
}

struct CountingVecProvider {
    model: &'static str,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl EmbedderProvider for CountingVecProvider {
    fn name(&self) -> &str {
        self.model
    }

    fn dimensions(&self) -> usize {
        DIMS
    }

    async fn build(&self) -> Result<Arc<dyn EmbeddingService>, khive_runtime::RuntimeError> {
        Ok(Arc::new(CountingVecService {
            calls: Arc::clone(&self.calls),
        }))
    }
}

/// Runtime with one counting embedding model, plus the registry and the
/// memory pack's ANN state.
fn counting_fixture(
    model: &'static str,
) -> (KhiveRuntime, VerbRegistry, SharedAnn, Arc<AtomicUsize>) {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let calls = Arc::new(AtomicUsize::new(0));
    rt.register_embedder(CountingVecProvider {
        model,
        calls: Arc::clone(&calls),
    });
    let pack = MemoryPack::new(crate::test_support::with_receipt_credentials(rt.clone()));
    let ann = pack.ann.clone();
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(pack);
    (rt, builder.build().expect("registry"), ann, calls)
}

async fn remember(registry: &VerbRegistry, content: &str) -> String {
    let written = registry
        .dispatch(
            "memory.remember",
            json!({ "content": content, "memory_type": "semantic", "salience": 0.9 }),
        )
        .await
        .expect("memory.remember");
    written["id"].as_str().expect("remember id").to_string()
}

fn result_ids(response: &Value) -> Vec<String> {
    response
        .as_array()
        .or_else(|| response.get("results").and_then(Value::as_array))
        .expect("recall response carries a results array")
        .iter()
        .map(|hit| hit["id"].as_str().expect("hit id").to_string())
        .collect()
}

/// Degraded in either documented shape: the empty envelope's top-level
/// marker, or the per-hit stamp on a non-empty array.
fn reports_degraded(response: &Value) -> bool {
    if response.get("degraded").and_then(Value::as_bool) == Some(true) {
        return true;
    }
    response.as_array().is_some_and(|hits| {
        !hits.is_empty() && hits.iter().all(|hit| hit.get("degraded").is_some())
    })
}

/// The measured production case: a CJK `keyword_only` query whose text arm
/// finds nothing used to embed the query and wait on ANN before returning
/// an empty result. With the vector arm skipped it embeds nothing, never
/// meets the held ANN warm lock, and returns a plain `[]`.
#[tokio::test]
#[serial(background_tasks)]
#[serial_test::serial(config_ledger)]
async fn keyword_only_recall_never_embeds_the_query_or_waits_on_ann() {
    const MODEL: &str = "legs-keyword-only-no-ann-model";
    let (_rt, registry, ann, calls) = counting_fixture(MODEL);
    remember(&registry, "alpha bravo charlie delta echo").await;

    // Any vector arm now waits on this held lock and degrades.
    let key = crate::ann::AnnKey::new(MODEL);
    let _held = crate::ann::hold_model_warm_lock_for_test(&ann, &key).await;

    let before = calls.load(Ordering::SeqCst);
    let result = registry
        .dispatch(
            "memory.recall",
            json!({
                "query": "完全无关的中文查询",
                "fusion_strategy": "keyword_only",
                "config": { "ann_ready_timeout_ms": 200 },
            }),
        )
        .await
        .expect("keyword_only recall");
    assert_eq!(
        result,
        json!([]),
        "keyword_only with no text hit must be a plain empty array, never a degraded \
         envelope from a vector arm it does not use: {result}"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        before,
        "keyword_only must not embed the query"
    );

    // Control on the same fixture: default fusion runs the vector arm, so
    // it embeds a (different, uncached) query and degrades on the held lock.
    let before = calls.load(Ordering::SeqCst);
    let control = registry
        .dispatch(
            "memory.recall",
            json!({
                "query": "另一个毫无关系的查询",
                "config": { "ann_ready_timeout_ms": 200 },
            }),
        )
        .await
        .expect("default-fusion control recall");
    assert!(
        reports_degraded(&control),
        "control: the held warm lock must degrade a vector arm that runs: {control}"
    );
    assert!(
        calls.load(Ordering::SeqCst) > before,
        "control: default fusion embeds the query"
    );
}

/// `vector_only` never runs FTS: with the notes text table dropped, a recall
/// that searches text fails, and a `vector_only` recall still serves the
/// note through the vector arm.
#[tokio::test]
#[serial(background_tasks)]
#[serial_test::serial(config_ledger)]
async fn vector_only_recall_never_runs_the_text_search() {
    const MODEL: &str = "legs-vector-only-no-fts-model";
    const CONTENT: &str = "foxtrot golf hotel india juliet";
    let (rt, registry, _ann, _calls) = counting_fixture(MODEL);
    let id = remember(&registry, CONTENT).await;

    // Positive control before the fault: the text arm finds the note.
    let warm = registry
        .dispatch(
            "memory.recall",
            json!({ "query": "foxtrot golf", "fusion_strategy": "keyword_only" }),
        )
        .await
        .expect("keyword_only recall before the fault");
    assert!(
        result_ids(&warm).contains(&id),
        "fixture: the text arm must find the note before the fault: {warm}"
    );

    {
        let sql = rt.sql();
        let mut writer = sql.writer().await.expect("fault injection writer");
        writer
            .execute(khive_storage::types::SqlStatement {
                sql: "DROP TABLE fts_notes".into(),
                params: vec![],
                label: Some("test_drop_fts_notes_for_vector_only".into()),
            })
            .await
            .expect("drop the notes text table");
    }

    // Control: an arm that runs FTS now fails.
    let broken = registry
        .dispatch(
            "memory.recall",
            json!({ "query": "foxtrot golf", "fusion_strategy": "keyword_only" }),
        )
        .await;
    assert!(
        broken.is_err(),
        "control: with fts_notes dropped the text arm must fail, got {broken:?}"
    );

    let result = registry
        .dispatch(
            "memory.recall",
            json!({ "query": CONTENT, "fusion_strategy": "vector_only" }),
        )
        .await
        .expect("vector_only must not touch the text search");
    assert!(
        result_ids(&result).contains(&id),
        "vector_only must still serve the note through the vector arm: {result}"
    );
}

/// The default response stays a bare array; `include_timings` wraps it as
/// `{results, timings}` and reports every stage the hybrid recall entered.
#[tokio::test]
#[serial(background_tasks)]
#[serial_test::serial(config_ledger)]
async fn include_timings_reports_each_stage_and_round() {
    const MODEL: &str = "legs-timings-hybrid-model";
    let (_rt, registry, _ann, _calls) = counting_fixture(MODEL);
    let id = remember(&registry, "kilo lima mike november oscar").await;

    let plain = registry
        .dispatch("memory.recall", json!({ "query": "kilo lima" }))
        .await
        .expect("plain recall");
    assert!(
        plain.is_array(),
        "without the flag the response stays a bare array: {plain}"
    );

    let response = registry
        .dispatch(
            "memory.recall",
            json!({ "query": "kilo lima", "include_timings": true }),
        )
        .await
        .expect("recall with timings");
    assert_eq!(
        result_ids(&response),
        result_ids(&plain),
        "the flag must not change the results"
    );
    assert!(result_ids(&response).contains(&id));
    let timings = &response["timings"];
    for stage in ["embed_us", "fts_us", "ann_us", "hydrate_us", "total_us"] {
        assert!(
            timings[stage].is_u64(),
            "hybrid recall entered {stage}: {timings}"
        );
    }
    assert!(
        timings.get("fresh_tail_us").is_some(),
        "fresh_tail_us is always reported, null when not entered: {timings}"
    );
    let rounds = timings["rounds"].as_array().expect("rounds array");
    assert!(
        !rounds.is_empty(),
        "at least one retrieval round: {timings}"
    );
    assert_eq!(timings["retrieval_rounds"], json!(rounds.len()));
    for field in [
        "candidate_limit",
        "text_hits",
        "vector_hits",
        "hydrated",
        "eligible",
        "embed_us",
        "fts_us",
        "ann_us",
        "hydrate_us",
    ] {
        assert!(
            rounds[0][field].is_u64(),
            "round 0 reports {field}: {timings}"
        );
    }
    assert!(rounds[0]["text_hits"].as_u64() >= Some(1));
    assert!(rounds[0]["vector_hits"].as_u64() >= Some(1));
}

/// Timings show the skipped arm as null. `keyword_only` with no text hit
/// stops after one round even when the vector side holds more rows than
/// the candidate limit, and `vector_only` never enters FTS.
#[tokio::test]
#[serial(background_tasks)]
#[serial_test::serial(config_ledger)]
async fn include_timings_shows_the_skipped_arm_and_no_widening() {
    const MODEL: &str = "legs-timings-skipped-arm-model";
    let (_rt, registry, _ann, _calls) = counting_fixture(MODEL);
    // More vector-reachable rows than the candidate limit below, none of
    // which shares a term with the keyword query.
    for i in 0..6 {
        remember(&registry, &format!("papa quebec romeo sierra {i}")).await;
    }
    let config = json!({
        "candidate_limit": 4,
        "scoring": { "max_recall_candidates": 16 },
        "ann_overfetch_max_rounds": 3,
    });

    let keyword = registry
        .dispatch(
            "memory.recall",
            json!({
                "query": "完全无关的中文查询",
                "fusion_strategy": "keyword_only",
                "include_timings": true,
                "config": config,
            }),
        )
        .await
        .expect("keyword_only recall with timings");
    assert_eq!(keyword["results"], json!([]), "{keyword}");
    let timings = &keyword["timings"];
    for skipped in ["embed_us", "ann_us", "fresh_tail_us"] {
        assert!(
            timings[skipped].is_null(),
            "keyword_only must not enter {skipped}: {timings}"
        );
    }
    assert!(timings["fts_us"].is_u64(), "{timings}");
    assert_eq!(
        timings["retrieval_rounds"],
        json!(1),
        "an exhausted text arm ends keyword_only retrieval: {timings}"
    );
    assert_eq!(timings["rounds"][0]["vector_hits"], json!(0), "{timings}");
    assert_eq!(timings["rounds"][0]["text_hits"], json!(0), "{timings}");

    let vector = registry
        .dispatch(
            "memory.recall",
            json!({
                "query": "papa quebec romeo sierra 1",
                "fusion_strategy": "vector_only",
                "include_timings": true,
                "config": config,
            }),
        )
        .await
        .expect("vector_only recall with timings");
    let timings = &vector["timings"];
    assert!(
        timings["fts_us"].is_null(),
        "vector_only must not enter FTS: {timings}"
    );
    assert!(timings["embed_us"].is_u64(), "{timings}");
    assert!(timings["ann_us"].is_u64(), "{timings}");
    assert!(
        !result_ids(&vector).is_empty(),
        "vector_only still serves through the vector arm: {vector}"
    );
}

/// An object-shaped response keeps every field it carries and gains
/// `timings`: here the empty degraded envelope.
#[tokio::test]
#[serial(background_tasks)]
#[serial_test::serial(config_ledger)]
async fn include_timings_extends_an_object_envelope() {
    const MODEL: &str = "legs-timings-envelope-model";
    let (_rt, registry, ann, _calls) = counting_fixture(MODEL);
    let key = crate::ann::AnnKey::new(MODEL);
    let _held = crate::ann::hold_model_warm_lock_for_test(&ann, &key).await;

    let response = registry
        .dispatch(
            "memory.recall",
            json!({
                "query": "nothing stored matches this",
                "include_timings": true,
                "config": { "ann_ready_timeout_ms": 100 },
            }),
        )
        .await
        .expect("degraded recall with timings");
    assert_eq!(response["results"], json!([]), "{response}");
    assert_eq!(response["degraded"], json!(true), "{response}");
    assert!(response["degraded_reason"].is_string(), "{response}");
    assert!(response["timings"]["ann_us"].is_u64(), "{response}");
    assert!(response["timings"]["embed_us"].is_u64(), "{response}");
}
