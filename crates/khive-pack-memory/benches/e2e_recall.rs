//! End-to-end recall benchmark with real corpus, real embeddings, and real ANN.
//!
//! Uses a stripped copy of khive-graph.db (notes + entities + embeddings intact,
//! knowledge tables removed) so the full production pipeline runs:
//!   FTS5 text leg ‖ vector/ANN leg → fusion → scoring → ranking
//!
//! Run:
//! ```bash
//! cargo bench -p khive-pack-memory --bench e2e_recall
//! ```

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use serde_json::json;

use khive_pack_kg::KgPack;
use khive_pack_memory::MemoryPack;
use khive_runtime::{KhiveRuntime, RuntimeConfig, VerbRegistryBuilder};

const BENCH_DB_FIXTURE: &str = "tests/fixtures/bench.db";

fn bench_db_path() -> Option<PathBuf> {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let p = manifest.join(BENCH_DB_FIXTURE);
    if p.exists() {
        Some(p)
    } else {
        None
    }
}

struct FtsGatherStrategy {
    name: &'static str,
    env_vars: Vec<(&'static str, &'static str)>,
}

fn strategies() -> Vec<FtsGatherStrategy> {
    vec![
        FtsGatherStrategy {
            name: "baseline (fts_gather disabled)",
            env_vars: vec![("KHIVE_RECALL_FTS_GATHER", "baseline")],
        },
        FtsGatherStrategy {
            name: "ranked (gather enabled)",
            env_vars: vec![("KHIVE_RECALL_FTS_GATHER", "ranked")],
        },
        FtsGatherStrategy {
            name: "ranked + lowest_df k=5",
            env_vars: vec![
                ("KHIVE_RECALL_FTS_GATHER", "ranked"),
                ("KHIVE_RECALL_FTS_SELECTION", "lowest_df"),
                ("KHIVE_RECALL_FTS_TERM_K", "5"),
            ],
        },
        FtsGatherStrategy {
            name: "ranked + highest_idf k=5",
            env_vars: vec![
                ("KHIVE_RECALL_FTS_GATHER", "ranked"),
                ("KHIVE_RECALL_FTS_SELECTION", "highest_idf"),
                ("KHIVE_RECALL_FTS_TERM_K", "5"),
            ],
        },
        FtsGatherStrategy {
            name: "rank_subset cap=4x",
            env_vars: vec![
                ("KHIVE_RECALL_FTS_GATHER", "rank_subset"),
                ("KHIVE_RECALL_FTS_GATHER_MULTIPLIER", "4"),
            ],
        },
        FtsGatherStrategy {
            name: "unranked",
            env_vars: vec![("KHIVE_RECALL_FTS_GATHER", "unranked")],
        },
    ]
}

const FUSION_STRATEGIES: &[(&str, &str)] = &[
    ("weighted (default 0.7/0.3)", "weighted"),
    ("vector_only", "vector_only"),
    ("keyword_only", "keyword_only"),
    ("rrf", "rrf"),
];

const FTS_GATHER_ENV_VARS: &[&str] = &[
    "KHIVE_RECALL_FTS_GATHER",
    "KHIVE_RECALL_FTS_TERM_K",
    "KHIVE_RECALL_FTS_SELECTION",
    "KHIVE_RECALL_FTS_GATHER_LIMIT",
    "KHIVE_RECALL_FTS_GATHER_MULTIPLIER",
    "KHIVE_RECALL_FTS_CJK_BYPASS",
];

fn clear_fts_env() {
    for var in FTS_GATHER_ENV_VARS {
        // SAFETY: benchmark runs single-threaded setup before any concurrent
        // async work; no other thread reads these env vars during setup.
        unsafe {
            std::env::remove_var(var);
        }
    }
}

fn set_fts_env(vars: &[(&str, &str)]) {
    clear_fts_env();
    for (k, v) in vars {
        // SAFETY: benchmark runs single-threaded setup before any concurrent
        // async work; no other thread reads these env vars during setup.
        unsafe {
            std::env::set_var(k, v);
        }
    }
}

async fn run_recall(
    registry: &khive_runtime::VerbRegistry,
    query: &str,
    limit: usize,
) -> Result<(u128, Vec<String>), String> {
    run_recall_inner(registry, query, limit, None).await
}

async fn run_recall_with_fusion(
    registry: &khive_runtime::VerbRegistry,
    query: &str,
    limit: usize,
    fusion_strategy: &str,
) -> Result<(u128, Vec<String>), String> {
    run_recall_inner(registry, query, limit, Some(fusion_strategy)).await
}

async fn run_recall_inner(
    registry: &khive_runtime::VerbRegistry,
    query: &str,
    limit: usize,
    fusion_strategy: Option<&str>,
) -> Result<(u128, Vec<String>), String> {
    let t = Instant::now();
    let mut params = json!({
        "query": query,
        "limit": limit,
        "full_content": false,
    });
    if let Some(fs) = fusion_strategy {
        params["fusion_strategy"] = json!(fs);
    }

    let result = registry
        .dispatch("memory.recall", params)
        .await
        .map_err(|e| format!("recall error: {e}"))?;

    let elapsed = t.elapsed().as_micros();
    let empty = vec![];
    let arr = if result.is_array() {
        result.as_array().unwrap()
    } else {
        result["results"].as_array().unwrap_or(&empty)
    };
    let ids: Vec<String> = arr
        .iter()
        .filter_map(|m| m["note_id"].as_str().map(|s| s.to_string()))
        .collect();
    Ok((elapsed, ids))
}

const QUERIES: &[&str] = &[
    "khive architecture storage traits",
    "agent orchestration parallel multi-agent patterns",
    "recall scoring fusion strategy weighted RRF",
    "namespace isolation security",
    "git workflow PR commit branch",
    "embedding model lattice MiniLM vector",
    "lionagi SDK flow orchestration",
    "Rust cargo clippy workspace",
    "FTS text search trigram BM25",
    "memory decay salience temporal",
    "ocean lambda leo session",
    "desktop app frontend wiring",
    "brain profile Bayesian posterior",
    "knowledge graph entity edge relation",
    "styx lean4 formal verification proof",
];

struct StrategyResult {
    name: String,
    latencies_us: Vec<u128>,
    result_ids: HashMap<String, Vec<String>>,
}

fn stats(latencies: &[u128]) -> (f64, f64, f64) {
    let mut sorted = latencies.to_vec();
    sorted.sort();
    let p50 = sorted[sorted.len() / 2] as f64 / 1000.0;
    let p95 = sorted[(sorted.len() as f64 * 0.95) as usize] as f64 / 1000.0;
    let mean = sorted.iter().sum::<u128>() as f64 / sorted.len() as f64 / 1000.0;
    (p50, p95, mean)
}

fn overlap_pct(
    a: &HashMap<String, Vec<String>>,
    b: &HashMap<String, Vec<String>>,
    queries: &[&str],
    k: usize,
) -> f64 {
    let mut agreements = Vec::new();
    for q in queries {
        let qs = q.to_string();
        if let (Some(a_ids), Some(b_ids)) = (a.get(&qs), b.get(&qs)) {
            let a_set: std::collections::HashSet<&str> =
                a_ids.iter().take(k).map(|s| s.as_str()).collect();
            let b_set: std::collections::HashSet<&str> =
                b_ids.iter().take(k).map(|s| s.as_str()).collect();
            let overlap = a_set.intersection(&b_set).count();
            agreements.push(overlap as f64 / a_set.len().max(1) as f64);
        }
    }
    if agreements.is_empty() {
        0.0
    } else {
        agreements.iter().sum::<f64>() / agreements.len() as f64
    }
}

async fn run_strategy_suite(
    rt: &KhiveRuntime,
    queries: &[&str],
    fts_env: &[(&str, &str)],
    fusion: Option<&str>,
) -> StrategyResult {
    set_fts_env(fts_env);
    let pack = MemoryPack::new(rt.clone());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(pack);
    let registry = builder.build().expect("registry");
    let _ = run_recall(&registry, "warmup", 5).await;

    let mut latencies = Vec::new();
    let mut result_map = HashMap::new();

    for query in queries {
        let mut iters = Vec::new();
        let mut last_ids = Vec::new();
        for _ in 0..3 {
            let res = if let Some(fs) = fusion {
                run_recall_with_fusion(&registry, query, 10, fs).await
            } else {
                run_recall(&registry, query, 10).await
            };
            match res {
                Ok((us, ids)) => {
                    iters.push(us);
                    last_ids = ids;
                }
                Err(e) => {
                    eprintln!("  WARN: {query}: {e}");
                    break;
                }
            }
        }
        if !iters.is_empty() {
            iters.sort();
            latencies.push(iters[iters.len() / 2]);
            result_map.insert(query.to_string(), last_ids);
        }
    }

    StrategyResult {
        name: String::new(),
        latencies_us: latencies,
        result_ids: result_map,
    }
}

#[tokio::main]
async fn main() {
    let db_path = match bench_db_path() {
        Some(p) => p,
        None => {
            eprintln!("SKIP: bench.db fixture not found at tests/fixtures/bench.db");
            eprintln!("Copy from production DB (see CLAUDE.md for instructions).");
            return;
        }
    };

    let tmp = tempfile::Builder::new()
        .prefix("khive-e2e-bench-")
        .tempdir()
        .expect("tmpdir");
    let bench_db = tmp.path().join("bench.db");
    std::fs::copy(&db_path, &bench_db).expect("copy bench.db");

    eprintln!(
        "bench DB: {} ({:.1} MB)",
        bench_db.display(),
        std::fs::metadata(&bench_db)
            .map(|m| m.len() as f64 / 1_048_576.0)
            .unwrap_or(0.0)
    );

    let rt = KhiveRuntime::new(RuntimeConfig {
        db_path: Some(bench_db),
        ..RuntimeConfig::default()
    })
    .expect("runtime");

    // Warm embedder + ANN.
    eprintln!("\nwarming up embedder + ANN index...");
    let t0 = Instant::now();
    {
        clear_fts_env();
        let pack = MemoryPack::new(rt.clone());
        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        builder.register(pack);
        let registry = builder.build().expect("registry");
        let _ = run_recall(&registry, "warmup embedder", 5).await;
        let _ = run_recall(&registry, "warmup ANN", 5).await;
    }
    eprintln!("warmup done in {:.1}s", t0.elapsed().as_secs_f64());

    // ── Phase 1: FTS gather strategies ──
    let mut fts_results: Vec<StrategyResult> = Vec::new();
    for strategy in strategies() {
        let mut sr = run_strategy_suite(&rt, QUERIES, &strategy.env_vars, None).await;
        sr.name = strategy.name.to_string();
        fts_results.push(sr);
    }
    clear_fts_env();

    // ── Phase 2: Fusion strategies ──
    let mut fusion_results: Vec<StrategyResult> = Vec::new();
    for &(name, fs) in FUSION_STRATEGIES {
        let mut sr = run_strategy_suite(&rt, QUERIES, &[], Some(fs)).await;
        sr.name = name.to_string();
        fusion_results.push(sr);
    }
    clear_fts_env();

    // ── Report ──
    println!("{}", "=".repeat(80));
    println!("END-TO-END RECALL BENCHMARK (real corpus, real embeddings, real ANN)");
    println!("Corpus: ~12k notes (9845 memories), ~14.7k MiniLM vectors");
    println!(
        "{} queries × {} FTS strategies + {} fusion strategies × 3 iters (median)\n",
        QUERIES.len(),
        fts_results.len(),
        fusion_results.len()
    );

    println!(
        "{:<35} {:>8} {:>8} {:>8}",
        "FTS Strategy", "p50 ms", "p95 ms", "mean ms"
    );
    println!("{}", "-".repeat(65));
    for sr in &fts_results {
        if sr.latencies_us.is_empty() {
            continue;
        }
        let (p50, p95, mean) = stats(&sr.latencies_us);
        println!("{:<35} {:>8.1} {:>8.1} {:>8.1}", sr.name, p50, p95, mean);
    }

    if let Some(baseline) = fts_results.first() {
        println!("\n{:<35} {:>12}", "FTS Strategy", "baseline@10");
        println!("{}", "-".repeat(50));
        for sr in &fts_results {
            let pct = overlap_pct(&baseline.result_ids, &sr.result_ids, QUERIES, 10);
            println!("{:<35} {:>11.1}%", sr.name, pct * 100.0);
        }
    }

    println!(
        "\n{:<35} {:>8} {:>8} {:>8}",
        "Fusion Strategy", "p50 ms", "p95 ms", "mean ms"
    );
    println!("{}", "-".repeat(65));
    for sr in &fusion_results {
        if sr.latencies_us.is_empty() {
            continue;
        }
        let (p50, p95, mean) = stats(&sr.latencies_us);
        println!("{:<35} {:>8.1} {:>8.1} {:>8.1}", sr.name, p50, p95, mean);
    }

    if let Some(weighted) = fusion_results.first() {
        println!("\n{:<35} {:>12}", "Fusion Strategy", "weighted@10");
        println!("{}", "-".repeat(50));
        for sr in &fusion_results {
            let pct = overlap_pct(&weighted.result_ids, &sr.result_ids, QUERIES, 10);
            println!("{:<35} {:>11.1}%", sr.name, pct * 100.0);
        }
    }

    // Per-query detail.
    if let Some(baseline) = fts_results.first() {
        println!("\nPer-query latency (baseline), ms:");
        for (i, query) in QUERIES.iter().enumerate() {
            if i < baseline.latencies_us.len() {
                println!(
                    "  {:>6.1}  {}",
                    baseline.latencies_us[i] as f64 / 1000.0,
                    query
                );
            }
        }

        if let Some(unranked) = fts_results.iter().find(|r| r.name.starts_with("unranked")) {
            println!("\nUnranked divergence from baseline:");
            for query in QUERIES {
                let q = query.to_string();
                if let (Some(base_ids), Some(unr_ids)) =
                    (baseline.result_ids.get(&q), unranked.result_ids.get(&q))
                {
                    let base_set: std::collections::HashSet<&str> =
                        base_ids.iter().take(10).map(|s| s.as_str()).collect();
                    let unr_set: std::collections::HashSet<&str> =
                        unr_ids.iter().take(10).map(|s| s.as_str()).collect();
                    let overlap = base_set.intersection(&unr_set).count();
                    if overlap < base_set.len() {
                        println!(
                            "  {query}: {overlap}/{} overlap, -%{}, +%{}",
                            base_set.len(),
                            base_set.difference(&unr_set).count(),
                            unr_set.difference(&base_set).count()
                        );
                    }
                }
            }
        }
    }
}
