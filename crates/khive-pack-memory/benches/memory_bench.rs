//! Criterion benchmarks for `memory.remember` and `memory.recall`.
//!
//! Run:
//! ```bash
//! cd crates && cargo bench -p khive-pack-memory --bench memory_bench
//! ```

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use khive_pack_kg::KgPack;
use khive_pack_memory::MemoryPack;
use khive_runtime::{KhiveRuntime, RuntimeConfig, VerbRegistryBuilder};
use serde_json::json;

fn make_runtime() -> KhiveRuntime {
    KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        embedding_model: None,
        additional_embedding_models: vec![],
        ..RuntimeConfig::default()
    })
    .expect("in-memory runtime")
}

fn make_registry(rt: KhiveRuntime) -> khive_runtime::VerbRegistry {
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(MemoryPack::new(rt));
    builder.build().expect("registry builds")
}

/// Seed `n` deterministic memory notes into `registry`, returning their content strings.
async fn seed_memories(registry: &khive_runtime::VerbRegistry, n: usize) -> Vec<String> {
    let phrases = [
        "attention mechanism transformers query key value",
        "Rust ownership borrow checker lifetime memory safety",
        "knowledge graph entity edge relation ontology",
        "agent orchestration parallel multi-agent patterns",
        "recall scoring fusion strategy weighted RRF",
        "namespace isolation security token authentication",
        "git workflow commit branch pull request review",
        "embedding model vector search cosine similarity",
        "FTS text search trigram BM25 inverted index",
        "memory decay salience temporal ranking pipeline",
    ];
    let mut contents = Vec::with_capacity(n);
    for i in 0..n {
        let base = phrases[i % phrases.len()];
        let content = format!("{base} seed-{i}");
        registry
            .dispatch(
                "memory.remember",
                json!({
                    "content": content,
                    "memory_type": "semantic",
                    "salience": 0.7,
                    "decay": 0.01
                }),
            )
            .await
            .expect("remember seeding");
        contents.push(content);
    }
    contents
}

fn bench_remember(c: &mut Criterion) {
    let tokio_rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let mut g = c.benchmark_group("remember");
    g.sample_size(20);

    g.bench_function("baseline", |b| {
        b.iter(|| {
            let rt = make_runtime();
            let registry = make_registry(rt);
            tokio_rt.block_on(async {
                registry
                    .dispatch(
                        "memory.remember",
                        json!({
                            "content": "attention mechanism transformers query key value matrices",
                            "memory_type": "semantic",
                            "salience": 0.8,
                            "decay": 0.01
                        }),
                    )
                    .await
                    .expect("remember baseline")
            })
        })
    });

    g.finish();
}

fn bench_remember_with_source(c: &mut Criterion) {
    let tokio_rt = tokio::runtime::Runtime::new().expect("tokio runtime");

    // Pre-create a source entity once so the bench iteration only pays for remember.
    let rt = make_runtime();
    let registry = make_registry(rt);
    let source_id: String = tokio_rt.block_on(async {
        let resp = registry
            .dispatch(
                "create",
                json!({
                    "kind": "concept",
                    "name": "Attention Mechanism",
                    "description": "Transformer attention"
                }),
            )
            .await
            .expect("create source entity");
        resp["id"].as_str().expect("entity id").to_string()
    });

    let mut g = c.benchmark_group("remember_with_source");
    g.sample_size(20);

    g.bench_function("with_annotation", |b| {
        b.iter(|| {
            tokio_rt.block_on(async {
                registry
                    .dispatch(
                        "memory.remember",
                        json!({
                            "content": "multi-head self-attention scales with sequence length",
                            "memory_type": "semantic",
                            "salience": 0.9,
                            "decay": 0.005,
                            "source_id": source_id
                        }),
                    )
                    .await
                    .expect("remember with source")
            })
        })
    });

    g.finish();
}

fn bench_recall(c: &mut Criterion) {
    let tokio_rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let mut g = c.benchmark_group("recall");
    g.sample_size(20);

    for n in [10usize, 100, 500] {
        let rt = make_runtime();
        let registry = make_registry(rt);
        tokio_rt.block_on(seed_memories(&registry, n));

        g.bench_with_input(BenchmarkId::new("n_memories", n), &n, |b, _| {
            b.iter(|| {
                tokio_rt.block_on(async {
                    registry
                        .dispatch(
                            "memory.recall",
                            json!({ "query": "attention transformers embedding", "limit": 10 }),
                        )
                        .await
                        .expect("recall")
                })
            })
        });
    }

    g.finish();
}

fn bench_recall_with_min_score(c: &mut Criterion) {
    let tokio_rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let rt = make_runtime();
    let registry = make_registry(rt);
    tokio_rt.block_on(seed_memories(&registry, 100));

    let mut g = c.benchmark_group("recall_with_min_score");
    g.sample_size(20);

    g.bench_function("min_score_0_3", |b| {
        b.iter(|| {
            tokio_rt.block_on(async {
                registry
                    .dispatch(
                        "memory.recall",
                        json!({
                            "query": "knowledge graph entity relation",
                            "limit": 10,
                            "min_score": 0.3
                        }),
                    )
                    .await
                    .expect("recall with min_score")
            })
        })
    });

    g.finish();
}

criterion_group!(
    benches,
    bench_remember,
    bench_remember_with_source,
    bench_recall,
    bench_recall_with_min_score,
);
criterion_main!(benches);
