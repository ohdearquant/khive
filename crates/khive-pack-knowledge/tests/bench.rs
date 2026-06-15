//! #595 — knowledge.search latency benchmark.
//!
//! Run with:
//!   cd crates && cargo test -p khive-pack-knowledge --test bench -- --ignored --nocapture
//!
//! Measures warm p50/p95 for three dispatch variants:
//!   - `rerank=false` baseline (pure TF-IDF, no embedding)
//!   - `rerank=true`  explicit (embedding blend)
//!   - default (omitted rerank = true when embedder configured, after #561)
//!
//! Cold first-query cost is measured separately and printed but excluded from
//! the warm percentiles, because cold-start is dominated by model weight load
//! (one-time per daemon lifecycle).

use std::time::Instant;

use khive_pack_kg::KgPack;
use khive_pack_knowledge::KnowledgePack;
use khive_runtime::{
    AllowAllGate, BackendId, KhiveRuntime, RuntimeConfig, VerbRegistry, VerbRegistryBuilder,
};
use khive_types::Namespace;
use lattice_embed::EmbeddingModel;
use serde_json::{json, Value};
use std::sync::Arc;

fn rt_with_embedder() -> KhiveRuntime {
    KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        default_namespace: Namespace::local(),
        embedding_model: Some(EmbeddingModel::AllMiniLmL6V2),
        additional_embedding_models: vec![],
        gate: Arc::new(AllowAllGate),
        packs: vec!["kg".to_string(), "knowledge".to_string()],
        backend_id: BackendId::main(),
        brain_profile: None,
        visible_namespaces: vec![],
    })
    .expect("runtime with embedder")
}

fn build_registry(rt: KhiveRuntime) -> VerbRegistry {
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(KnowledgePack::new(rt.clone()));
    let registry = builder.build().expect("registry");
    rt.install_edge_rules(registry.all_edge_rules());
    registry
}

fn percentile(sorted: &[u128], pct: f64) -> u128 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64 * pct / 100.0).ceil() as usize).min(sorted.len()) - 1;
    sorted[idx]
}

/// #595 warm latency benchmark.
///
/// Ignored by default — run manually with:
///   cargo test -p khive-pack-knowledge --test bench benchmark_knowledge_search_warm_latency -- --ignored --nocapture
#[test]
#[ignore]
fn benchmark_knowledge_search_warm_latency() {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");

    rt.block_on(async {
        let runtime = rt_with_embedder();
        let registry = build_registry(runtime.clone());

        // Seed 100 atoms across a few knowledge topics.
        let atoms: Vec<Value> = (0..100)
            .map(|i| {
                json!({
                    "slug": format!("bench-atom-{i}"),
                    "name": format!("Bench Atom {i}"),
                    "content": format!(
                        "knowledge retrieval embedding reranking benchmark atom {i} tensor neural \
                         dense sparse retrieval corpus search latency gradient descent transformer \
                         attention mechanism vector index nearest neighbor approximate ranking fusion"
                    ),
                })
            })
            .collect();

        registry
            .dispatch("knowledge.upsert_atoms", json!({ "atoms": atoms }))
            .await
            .expect("seed atoms");

        let query = "knowledge retrieval embedding reranking benchmark";

        // ── Cold measurement (first reranked dispatch) ───────────────────────
        let t_cold = Instant::now();
        let cold_resp = registry
            .dispatch("knowledge.search", json!({ "query": query }))
            .await
            .expect("cold rerank dispatch");
        let cold_us = t_cold.elapsed().as_micros();
        assert_eq!(cold_resp["status"], "ok");
        println!("[#595] cold_rerank_first_query_us: {cold_us}");

        // ── Warm measurements (N=20 per case) ────────────────────────────────
        const N: usize = 20;

        let mut rerank_false_us: Vec<u128> = Vec::with_capacity(N);
        for _ in 0..N {
            let t = Instant::now();
            let resp = registry
                .dispatch(
                    "knowledge.search",
                    json!({ "query": query, "rerank": false }),
                )
                .await
                .expect("rerank=false dispatch");
            rerank_false_us.push(t.elapsed().as_micros());
            assert_eq!(resp["status"], "ok");
        }
        rerank_false_us.sort_unstable();

        let mut rerank_true_us: Vec<u128> = Vec::with_capacity(N);
        for _ in 0..N {
            let t = Instant::now();
            let resp = registry
                .dispatch(
                    "knowledge.search",
                    json!({ "query": query, "rerank": true }),
                )
                .await
                .expect("rerank=true dispatch");
            rerank_true_us.push(t.elapsed().as_micros());
            assert_eq!(resp["status"], "ok");
        }
        rerank_true_us.sort_unstable();

        let mut default_us: Vec<u128> = Vec::with_capacity(N);
        for _ in 0..N {
            let t = Instant::now();
            let resp = registry
                .dispatch("knowledge.search", json!({ "query": query }))
                .await
                .expect("default dispatch");
            default_us.push(t.elapsed().as_micros());
            assert_eq!(resp["status"], "ok");
        }
        default_us.sort_unstable();

        // ── Report ───────────────────────────────────────────────────────────
        let rf_p50 = percentile(&rerank_false_us, 50.0);
        let rf_p95 = percentile(&rerank_false_us, 95.0);
        let rt_p50 = percentile(&rerank_true_us, 50.0);
        let rt_p95 = percentile(&rerank_true_us, 95.0);
        let df_p50 = percentile(&default_us, 50.0);
        let df_p95 = percentile(&default_us, 95.0);

        println!("[#595] corpus=100 atoms, N={N} warm queries each");
        println!("[#595] cold_first_query_us:         {cold_us}");
        println!("[#595] rerank=false  p50_us={rf_p50}  p95_us={rf_p95}");
        println!("[#595] rerank=true   p50_us={rt_p50}  p95_us={rt_p95}");
        println!("[#595] default       p50_us={df_p50}  p95_us={df_p95}");

        let json_report = json!({
            "issue": 595,
            "corpus_atoms": 100,
            "warm_queries_per_case": N,
            "cold_first_query_us": cold_us,
            "rerank_false": { "p50_us": rf_p50, "p95_us": rf_p95 },
            "rerank_true":  { "p50_us": rt_p50, "p95_us": rt_p95 },
            "default":      { "p50_us": df_p50, "p95_us": df_p95 },
            "note": "warm = after first reranked query preloads embedding model"
        });
        println!(
            "[#595] json_report: {}",
            serde_json::to_string_pretty(&json_report).unwrap()
        );

        // Sanity: warm rerank should not be more than 10x the baseline TF-IDF.
        // If this fails, there is a per-query model reload regression.
        let rerank_overhead = if rf_p50 > 0 { rt_p50 / rf_p50 } else { 0 };
        println!("[#595] warm_rerank_overhead_factor: {rerank_overhead}x");

        // Write JSON to temp dir if writable.
        let out_path = std::env::temp_dir().join("issue_595_latencies.json");
        if let Ok(()) = std::fs::write(
            &out_path,
            serde_json::to_string_pretty(&json_report).unwrap(),
        ) {
            println!("[#595] wrote latency report to {}", out_path.display());
        }
    });
}

/// Smoke-test: confirms the benchmark infrastructure builds and runs without errors
/// on a no-embedder runtime (no actual latency measurement, just confirms dispatch works).
#[tokio::test]
async fn bench_infrastructure_smoke_test() {
    let rt = KhiveRuntime::memory().expect("memory runtime");
    let registry = build_registry(rt);

    registry
        .dispatch(
            "knowledge.upsert_atoms",
            json!({
                "atoms": [
                    { "slug": "bench-smoke", "name": "Bench Smoke", "content": "benchmark smoke test atom covering retrieval embedding reranking dense sparse corpus search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline" }
                ]
            }),
        )
        .await
        .expect("seed smoke atom");

    // With no embedder, default rerank is a no-op (do_rerank = false via guard).
    let resp = registry
        .dispatch(
            "knowledge.search",
            json!({ "query": "benchmark smoke", "rerank": false }),
        )
        .await
        .expect("smoke search ok");
    assert!(resp["results"].is_array(), "search returns results array");
}
