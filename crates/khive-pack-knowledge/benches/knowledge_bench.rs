//! Criterion benchmark suite for hot-path knowledge pack verbs.
//!
//! Run with:
//!   cd crates && cargo bench -p khive-pack-knowledge --bench knowledge_bench

use std::sync::Arc;

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use serde_json::{json, Value};

use khive_pack_kg::KgPack;
use khive_pack_knowledge::KnowledgePack;
use khive_runtime::{
    AllowAllGate, BackendId, KhiveRuntime, RuntimeConfig, VerbRegistry, VerbRegistryBuilder,
};
use khive_types::Namespace;

// ── runtime helpers ───────────────────────────────────────────────────────────

fn build_runtime() -> KhiveRuntime {
    KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        default_namespace: Namespace::local(),
        actor_ref: khive_runtime::ActorRef::anonymous(),
        embedding_model: None,
        additional_embedding_models: vec![],
        gate: Arc::new(AllowAllGate),
        packs: vec!["kg".to_string(), "knowledge".to_string()],
        backend_id: BackendId::main(),
        brain_profile: None,
        visible_namespaces: vec![],
        allowed_outbound_namespaces: vec![],
    })
    .expect("runtime")
}

fn build_registry(rt: KhiveRuntime) -> VerbRegistry {
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(KnowledgePack::new(rt.clone()));
    let registry = builder.build().expect("registry");
    rt.install_edge_rules(registry.all_edge_rules());
    registry
}

fn seed_atoms(registry: &VerbRegistry, rt: &tokio::runtime::Runtime, n: usize) {
    let atoms: Vec<Value> = (0..n)
        .map(|i| {
            json!({
                "slug": format!("bench-atom-{i:04}"),
                "name": format!("Bench Atom {i}"),
                "description": format!("knowledge retrieval benchmark atom {i} semantic neural transformer embedding"),
                // Content must satisfy MIN_ATOM_CONTENT_WORDS = 20 enforced by the knowledge pack.
                "content": format!("dense sparse vector embedding search benchmark corpus atom {i} gradient neural transformer retrieval semantic index query score rank precision recall"),
            })
        })
        .collect();
    rt.block_on(async {
        registry
            .dispatch("knowledge.upsert_atoms", json!({ "atoms": atoms }))
            .await
            .expect("seed atoms");
    });
}

// ── write benchmarks ──────────────────────────────────────────────────────────

fn bench_learn(c: &mut Criterion) {
    let mut group = c.benchmark_group("knowledge_learn");
    group.sample_size(50);

    let rt_tokio = tokio::runtime::Runtime::new().expect("tokio");

    group.bench_function("concept_create", |b| {
        b.iter_batched(
            || build_registry(build_runtime()),
            |registry| {
                rt_tokio.block_on(async {
                    let resp = registry
                        .dispatch(
                            "knowledge.learn",
                            black_box(json!({ "name": "Bench Concept", "domain": "retrieval" })),
                        )
                        .await
                        .expect("learn");
                    black_box(resp)
                })
            },
            criterion::BatchSize::SmallInput,
        );
    });

    group.finish();
}

fn bench_upsert_atoms(c: &mut Criterion) {
    let mut group = c.benchmark_group("knowledge_upsert_atoms");
    group.sample_size(30);

    let rt_tokio = tokio::runtime::Runtime::new().expect("tokio");

    for &batch_size in &[1usize, 10, 50] {
        group.bench_with_input(
            BenchmarkId::new("atoms", batch_size),
            &batch_size,
            |b, &batch_size| {
                b.iter_batched(
                    || {
                        let runtime = build_runtime();
                        let registry = build_registry(runtime);
                        let atoms: Vec<Value> = (0..batch_size)
                            .map(|i| {
                                json!({
                                    "slug": format!("upsert-bench-{i}"),
                                    "name": format!("Upsert Bench Atom {i}"),
                                    // Content must satisfy MIN_ATOM_CONTENT_WORDS = 20.
                                    "content": format!("benchmark content atom {i} embedding search neural transformer retrieval corpus dense sparse vector index query score rank precision recall fusion"),
                                })
                            })
                            .collect();
                        (registry, atoms)
                    },
                    |(registry, atoms)| {
                        rt_tokio.block_on(async {
                            let resp = registry
                                .dispatch(
                                    "knowledge.upsert_atoms",
                                    black_box(json!({ "atoms": atoms })),
                                )
                                .await
                                .expect("upsert_atoms");
                            black_box(resp)
                        })
                    },
                    criterion::BatchSize::SmallInput,
                );
            },
        );
    }

    group.finish();
}

// ── read benchmarks ───────────────────────────────────────────────────────────

fn bench_list(c: &mut Criterion) {
    let mut group = c.benchmark_group("knowledge_list");
    group.sample_size(50);

    let rt_tokio = tokio::runtime::Runtime::new().expect("tokio");

    for &corpus_size in &[10usize, 100] {
        let runtime = build_runtime();
        let registry = build_registry(runtime);
        seed_atoms(&registry, &rt_tokio, corpus_size);

        group.bench_with_input(
            BenchmarkId::new("corpus", corpus_size),
            &corpus_size,
            |b, _| {
                b.to_async(&rt_tokio).iter(|| {
                    registry.dispatch("knowledge.list", black_box(json!({ "limit": 20 })))
                });
            },
        );
    }

    group.finish();
}

fn bench_search_fts(c: &mut Criterion) {
    let mut group = c.benchmark_group("knowledge_search_fts");
    group.sample_size(50);

    let rt_tokio = tokio::runtime::Runtime::new().expect("tokio");

    let runtime = build_runtime();
    let registry = build_registry(runtime);
    seed_atoms(&registry, &rt_tokio, 50);

    group.bench_function("rerank_false", |b| {
        b.to_async(&rt_tokio).iter(|| {
            registry.dispatch(
                "knowledge.search",
                black_box(json!({ "query": "embedding search neural", "rerank": false })),
            )
        });
    });

    group.finish();
}

fn bench_stats(c: &mut Criterion) {
    let mut group = c.benchmark_group("knowledge_stats");
    group.sample_size(50);

    let rt_tokio = tokio::runtime::Runtime::new().expect("tokio");

    let runtime = build_runtime();
    let registry = build_registry(runtime);
    seed_atoms(&registry, &rt_tokio, 50);

    group.bench_function("stats_query", |b| {
        b.to_async(&rt_tokio)
            .iter(|| registry.dispatch("knowledge.stats", black_box(json!({}))));
    });

    group.finish();
}

fn bench_get(c: &mut Criterion) {
    let mut group = c.benchmark_group("knowledge_get");
    group.sample_size(50);

    let rt_tokio = tokio::runtime::Runtime::new().expect("tokio");

    let runtime = build_runtime();
    let registry = build_registry(runtime);
    seed_atoms(&registry, &rt_tokio, 10);

    group.bench_function("by_slug", |b| {
        b.to_async(&rt_tokio).iter(|| {
            registry.dispatch(
                "knowledge.get",
                black_box(json!({ "id": "bench-atom-0005" })),
            )
        });
    });

    group.finish();
}

// ── criterion entry points ────────────────────────────────────────────────────

criterion_group!(write_benches, bench_learn, bench_upsert_atoms,);

criterion_group!(
    read_benches,
    bench_list,
    bench_search_fts,
    bench_stats,
    bench_get,
);

criterion_main!(write_benches, read_benches);
