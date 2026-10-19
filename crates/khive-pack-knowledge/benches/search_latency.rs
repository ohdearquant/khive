//! Criterion timings for three knowledge.search reranking choices on 100 generated atoms.
//! Synthetic by default; KHIVE_BENCH_REAL_MODEL=1 selects a separate inference cohort.

#[path = "support/synthetic_embedding.rs"]
mod synthetic_embedding;

use criterion::{black_box, Criterion};
use khive_pack_kg::KgPack;
use khive_pack_knowledge::KnowledgePack;
use khive_runtime::{KhiveRuntime, RuntimeConfig, VerbRegistry, VerbRegistryBuilder};
use serde_json::{json, Value};
use std::time::Duration;
use synthetic_embedding::{SyntheticProvider, MODEL};

const CASES: [(&str, Option<bool>); 3] = [
    ("rerank_false", Some(false)),
    ("rerank_true", Some(true)),
    ("default", None),
];
fn case_id(variant: &str, real_model: bool) -> String {
    let cohort = if real_model {
        "real_model_100"
    } else {
        "synthetic_100"
    };
    format!("knowledge_search_latency/{cohort}/{variant}")
}
const QUERY: &str = "subject7 visiblemarker0 knowledge retrieval embedding reranking benchmark";

async fn fixture(real_model: bool) -> VerbRegistry {
    // In-memory store: the default config points at HOME/.khive/khive.db, which a benchmark must never open.
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        embedding_model: Some(MODEL),
        ..RuntimeConfig::no_embeddings()
    })
    .expect("synthetic runtime");
    if !real_model {
        runtime.register_embedder(SyntheticProvider);
    }
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime.clone()));
    builder.register(KnowledgePack::new(runtime.clone()));
    let registry = builder.build().expect("registry");
    registry.apply_schema_plans(runtime.backend());
    runtime.install_edge_rules(registry.all_edge_rules());
    let atoms: Vec<Value> = (0..100).map(|i| json!({
        "slug": format!("bench-atom-{i}"), "name": format!("Bench Atom {i}"), "finalized": true,
        "content": format!("subject{i} visiblemarker0 knowledge retrieval embedding reranking benchmark atom tensor neural dense sparse retrieval corpus benchmark search latency gradient transformer semantic index query score rank precision recall vector")
    })).collect();
    let seeded = registry
        .dispatch("knowledge.upsert_atoms", json!({"atoms":atoms}))
        .await
        .expect("seed atoms");
    assert_eq!(seeded["created"], 100);
    // Search returns finalized atoms only, and the rerank cases read the indexed vectors.
    let indexed = registry
        .dispatch("knowledge.index", json!({"rebuild_ann": false}))
        .await
        .expect("index atoms");
    assert_eq!(indexed["indexed"], 100);
    registry
}

async fn search(registry: &VerbRegistry, rerank: Option<bool>) -> Value {
    let mut params = json!({"query": QUERY, "limit": 10});
    if let Some(value) = rerank {
        params["rerank"] = json!(value);
    }
    let response = registry
        .dispatch("knowledge.search", params)
        .await
        .expect("search");
    let rows = response["results"].as_array().expect("results array");
    assert_eq!(rows.len(), 10, "a shortened/empty result is not a timing");
    let expected = rerank.unwrap_or(true);
    assert_eq!(
        response.get("rerank_provenance").is_some(),
        expected,
        "rerank route must match the case"
    );
    assert!(rows
        .iter()
        .all(|row| row["score_provenance"]["embedding_rerank"]
            .as_bool()
            .unwrap_or(false)
            == expected));
    response
}

fn main() {
    let args: Vec<_> = std::env::args().collect();
    let real_model = std::env::var("KHIVE_BENCH_REAL_MODEL").as_deref() == Ok("1");
    if args.iter().any(|arg| arg == "--list") {
        for (variant, _) in CASES {
            println!("{}: benchmark", case_id(variant, real_model));
        }
        return;
    }
    // A model-free control proves that listing returns before any runtime or seed work.
    assert!(
        !args.iter().any(|arg| arg == "--regression-case=list-only"),
        "listing entered workload setup"
    );
    let mut criterion = Criterion::default()
        .sample_size(20)
        .warm_up_time(Duration::from_millis(500))
        .measurement_time(Duration::from_secs(2))
        .configure_from_args();
    let executor = tokio::runtime::Runtime::new().expect("measurement runtime");
    let registry = executor.block_on(fixture(real_model));
    for (variant, rerank) in CASES {
        let id = case_id(variant, real_model);
        // Warm the selected route outside the measured iterations; each timed call
        // retains the same cardinality/provenance checks, whose cost is included.
        black_box(executor.block_on(search(&registry, rerank)));
        criterion.bench_function(&id, |b| {
            b.iter(|| black_box(executor.block_on(search(&registry, rerank))))
        });
    }
    criterion.final_summary();
}
