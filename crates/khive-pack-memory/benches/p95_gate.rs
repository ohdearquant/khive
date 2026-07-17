//! ADR-116 gate condition 2: p95 baseline for `memory.recall` on the warm ANN path.
//!
//! Measures `memory.recall` latency (p50/p95/p99, warm/post-index) against a
//! **file-backed WAL SQLite database** with four registered embedding-model
//! generations (1 primary + 3 retired), matching the shape ADR-116's warm-hit
//! generation-read gate is defined against: "a file-backed WAL benchmark with
//! warm page cache at one and three models ... at most 1.0 ms absolute p95 and
//! at most 5% of end-to-end warm memory.recall p95" (ADR-116 §Warm hit).
//!
//! ADR-116 is Proposed, not yet implemented: this harness establishes the
//! pre-change baseline. Once the durable per-model generation read lands, rerun
//! this bench and diff against the recorded baseline to confirm the added
//! generation check stays inside the gate.
//!
//! Run (inside the fleet bench-window lock, see khive/CLAUDE.md):
//! ```bash
//! cd crates && cargo bench -p khive-pack-memory --bench p95_gate
//! ```

use std::path::PathBuf;
use std::time::Instant;

use serde_json::json;

use khive_pack_kg::KgPack;
use khive_pack_memory::MemoryPack;
use khive_runtime::{KhiveRuntime, RuntimeConfig, VerbRegistryBuilder};
use lattice_embed::EmbeddingModel;

/// Memories seeded per model. Four models × this count gives a corpus large
/// enough to force the Vamana ANN path (not the small-corpus exact fallback)
/// while keeping local CPU embedding time bounded.
const MEMORIES_PER_MODEL: usize = 200;

/// Recall iterations measured per model after warmup, for percentile stats.
const RECALL_ITERS: usize = 200;

const PRIMARY_MODEL: EmbeddingModel = EmbeddingModel::BgeSmallEnV15;
const RETIRED_MODELS: [EmbeddingModel; 3] = [
    EmbeddingModel::MultilingualE5Small,
    EmbeddingModel::AllMiniLmL6V2,
    EmbeddingModel::ParaphraseMultilingualMiniLmL12V2,
];

const CONTENT_PHRASES: &[&str] = &[
    "attention mechanism transformers query key value projection",
    "Rust ownership borrow checker lifetime memory safety",
    "knowledge graph entity edge relation ontology traversal",
    "agent orchestration parallel multi-agent coordination patterns",
    "recall scoring fusion strategy weighted reciprocal rank",
    "namespace isolation security token authentication gate",
    "git workflow commit branch pull request review merge",
    "embedding model vector search cosine similarity index",
    "full text search trigram BM25 inverted index tokenizer",
    "memory decay salience temporal ranking pipeline schedule",
    "SQLite write ahead log checkpoint durability transaction",
    "vamana ANN graph greedy construction beam search",
];

const RECALL_QUERY: &str = "recall scoring fusion vector search memory decay";

fn make_runtime(db_path: PathBuf) -> KhiveRuntime {
    KhiveRuntime::new(RuntimeConfig {
        db_path: Some(db_path),
        embedding_model: Some(PRIMARY_MODEL),
        additional_embedding_models: RETIRED_MODELS.to_vec(),
        ..RuntimeConfig::default()
    })
    .expect("file-backed WAL runtime")
}

fn make_registry(rt: &KhiveRuntime) -> khive_runtime::VerbRegistry {
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(MemoryPack::new(rt.clone()));
    builder.build().expect("registry builds")
}

async fn seed_model(registry: &khive_runtime::VerbRegistry, model: EmbeddingModel, n: usize) {
    for i in 0..n {
        let phrase = CONTENT_PHRASES[i % CONTENT_PHRASES.len()];
        let content = format!("{phrase} — {model} seed-{i}");
        registry
            .dispatch(
                "memory.remember",
                json!({
                    "content": content,
                    "memory_type": "semantic",
                    "salience": 0.6,
                    "decay_factor": 0.01,
                    "embedding_model": model.to_string(),
                }),
            )
            .await
            .unwrap_or_else(|e| panic!("remember seeding failed for {model}: {e}"));
    }
}

async fn recall_once(
    registry: &khive_runtime::VerbRegistry,
    model: Option<EmbeddingModel>,
) -> u128 {
    let mut params = json!({
        "query": RECALL_QUERY,
        "limit": 10,
    });
    if let Some(m) = model {
        params["embedding_model"] = json!(m.to_string());
    }
    let t = Instant::now();
    registry
        .dispatch("memory.recall", params)
        .await
        .expect("recall");
    t.elapsed().as_micros()
}

struct Percentiles {
    p50_ms: f64,
    p95_ms: f64,
    p99_ms: f64,
    n: usize,
}

fn percentiles(mut latencies_us: Vec<u128>) -> Percentiles {
    latencies_us.sort_unstable();
    let n = latencies_us.len();
    let at = |q: f64| -> f64 {
        let idx = ((n as f64 * q).ceil() as usize)
            .saturating_sub(1)
            .min(n - 1);
        latencies_us[idx] as f64 / 1000.0
    };
    Percentiles {
        p50_ms: at(0.50),
        p95_ms: at(0.95),
        p99_ms: at(0.99),
        n,
    }
}

async fn bench_model_recall(
    registry: &khive_runtime::VerbRegistry,
    label: &str,
    model: Option<EmbeddingModel>,
) -> Percentiles {
    // Warm: force ANN build/install for this model before timing.
    for _ in 0..5 {
        let _ = recall_once(registry, model).await;
    }

    let mut latencies = Vec::with_capacity(RECALL_ITERS);
    for _ in 0..RECALL_ITERS {
        latencies.push(recall_once(registry, model).await);
    }
    let stats = percentiles(latencies);
    eprintln!(
        "  {label}: p50={:.3}ms p95={:.3}ms p99={:.3}ms n={}",
        stats.p50_ms, stats.p95_ms, stats.p99_ms, stats.n
    );
    stats
}

#[tokio::main]
async fn main() {
    let tmp = tempfile::Builder::new()
        .prefix("khive-p95-gate-")
        .tempdir()
        .expect("tmpdir");
    let db_path = tmp.path().join("p95-gate.db");

    eprintln!("db: {} (file-backed, WAL pool)", db_path.display());
    eprintln!(
        "models: primary={PRIMARY_MODEL} retired=[{}, {}, {}]",
        RETIRED_MODELS[0], RETIRED_MODELS[1], RETIRED_MODELS[2]
    );

    let rt = make_runtime(db_path.clone());
    let registry = make_registry(&rt);

    eprintln!(
        "\nseeding {} memories per model x 4 models = {} total...",
        MEMORIES_PER_MODEL,
        MEMORIES_PER_MODEL * 4
    );
    let t_seed = Instant::now();
    seed_model(&registry, PRIMARY_MODEL, MEMORIES_PER_MODEL).await;
    for model in RETIRED_MODELS {
        seed_model(&registry, model, MEMORIES_PER_MODEL).await;
    }
    eprintln!("seed done in {:.1}s", t_seed.elapsed().as_secs_f64());

    eprintln!("\nwarm + measure memory.recall p50/p95/p99 per model (n={RECALL_ITERS} each):");

    let primary_stats = bench_model_recall(&registry, "primary (BgeSmallEnV15)", None).await;
    let mut retired_stats = Vec::with_capacity(3);
    for model in RETIRED_MODELS {
        let label = format!("retired ({model})");
        retired_stats.push((
            model,
            bench_model_recall(&registry, &label, Some(model)).await,
        ));
    }

    println!("{}", "=".repeat(78));
    println!("ADR-116 GATE — memory.recall p95 baseline (file-backed WAL, warm ANN)");
    println!(
        "corpus: {} memories/model x 4 model generations (1 primary + 3 retired)",
        MEMORIES_PER_MODEL
    );
    println!("{}", "=".repeat(78));
    println!(
        "{:<32} {:>8} {:>8} {:>8} {:>6}",
        "model", "p50 ms", "p95 ms", "p99 ms", "n"
    );
    println!("{}", "-".repeat(78));
    println!(
        "{:<32} {:>8.3} {:>8.3} {:>8.3} {:>6}",
        "primary (queried by default)",
        primary_stats.p50_ms,
        primary_stats.p95_ms,
        primary_stats.p99_ms,
        primary_stats.n
    );
    for (model, stats) in &retired_stats {
        println!(
            "{:<32} {:>8.3} {:>8.3} {:>8.3} {:>6}",
            format!("retired: {model}"),
            stats.p50_ms,
            stats.p95_ms,
            stats.p99_ms,
            stats.n
        );
    }
    println!("{}", "=".repeat(78));
    println!(
        "gate reference (ADR-116 warm-hit): added generation-read cost budget is \
         <=1.0ms absolute p95 and <=5% of this baseline's warm memory.recall p95."
    );
}
