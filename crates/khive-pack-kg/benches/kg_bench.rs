//! Criterion benchmark suite for khive-pack-kg hot paths.
//!
//! Run with:
//!   cd crates && cargo bench -p khive-pack-kg --bench kg_bench

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use khive_pack_kg::KgPack;
use khive_runtime::{KhiveRuntime, VerbRegistry, VerbRegistryBuilder};
use serde_json::json;

// ── runtime factory ───────────────────────────────────────────────────────────

fn build_registry() -> (KhiveRuntime, VerbRegistry) {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    let registry = builder.build().expect("registry");
    rt.install_edge_rules(registry.all_edge_rules());
    (rt, registry)
}

fn tokio_rt() -> tokio::runtime::Runtime {
    tokio::runtime::Runtime::new().expect("tokio runtime")
}

// ── seeding helpers ───────────────────────────────────────────────────────────

async fn seed_entities(registry: &VerbRegistry, n: usize) -> Vec<String> {
    let mut ids = Vec::with_capacity(n);
    for i in 0..n {
        let resp = registry
            .dispatch(
                "create",
                json!({
                    "kind": "entity",
                    "name": format!("BenchEntity{i:04}"),
                    "entity_kind": "concept",
                    "description": format!(
                        "knowledge graph retrieval benchmark entity {i} semantic neural vector"
                    ),
                }),
            )
            .await
            .expect("seed entity");
        let id = resp["id"].as_str().expect("id").to_string();
        ids.push(id);
    }
    ids
}

async fn seed_graph(registry: &VerbRegistry, n: usize) -> Vec<String> {
    let ids = seed_entities(registry, n).await;
    // Chain the entities into a line graph: ids[0] → ids[1] → … → ids[n-1]
    for w in ids.windows(2) {
        registry
            .dispatch(
                "link",
                json!({
                    "source_id": w[0],
                    "target_id": w[1],
                    "relation": "contains",
                }),
            )
            .await
            .expect("seed edge");
    }
    ids
}

// ── create / get / list ───────────────────────────────────────────────────────

fn bench_create(c: &mut Criterion) {
    let rt = tokio_rt();
    let (_, registry) = build_registry();
    let mut group = c.benchmark_group("kg_create");
    group.sample_size(50);

    group.bench_function("entity", |b| {
        let mut seq: u64 = 0;
        b.to_async(&rt).iter(|| {
            seq += 1;
            let registry = &registry;
            async move {
                let resp = registry
                    .dispatch(
                        "create",
                        black_box(json!({
                            "kind": "entity",
                            "name": format!("Bench{seq}"),
                            "entity_kind": "concept",
                        })),
                    )
                    .await
                    .expect("create entity");
                black_box(resp)
            }
        });
    });

    group.bench_function("note", |b| {
        let mut seq: u64 = 0;
        b.to_async(&rt).iter(|| {
            seq += 1;
            let registry = &registry;
            async move {
                let resp = registry
                    .dispatch(
                        "create",
                        black_box(json!({
                            "kind": "note",
                            "content": format!("bench note content {seq}"),
                            "note_kind": "observation",
                        })),
                    )
                    .await
                    .expect("create note");
                black_box(resp)
            }
        });
    });

    group.finish();
}

fn bench_get(c: &mut Criterion) {
    let rt = tokio_rt();
    let (_, registry) = build_registry();

    // Pre-seed one entity whose UUID we will fetch in the hot loop.
    let id: String = rt.block_on(async {
        let resp = registry
            .dispatch(
                "create",
                json!({
                    "kind": "entity",
                    "name": "BenchGetTarget",
                    "entity_kind": "concept",
                }),
            )
            .await
            .expect("seed get target");
        resp["id"].as_str().expect("id").to_string()
    });

    let mut group = c.benchmark_group("kg_get");
    group.sample_size(100);

    group.bench_function("by_uuid", |b| {
        b.to_async(&rt).iter(|| {
            let registry = &registry;
            let id = id.clone();
            async move {
                let resp = registry
                    .dispatch("get", black_box(json!({ "id": id })))
                    .await
                    .expect("get");
                black_box(resp)
            }
        });
    });

    group.finish();
}

fn bench_list(c: &mut Criterion) {
    let rt = tokio_rt();
    let (_, registry) = build_registry();

    rt.block_on(async { seed_entities(&registry, 200).await });

    let mut group = c.benchmark_group("kg_list");
    group.sample_size(50);

    group.bench_function("entity_kind_concept_limit20", |b| {
        b.to_async(&rt).iter(|| {
            let registry = &registry;
            async move {
                let resp = registry
                    .dispatch(
                        "list",
                        black_box(json!({
                            "kind": "entity",
                            "entity_kind": "concept",
                            "limit": 20,
                        })),
                    )
                    .await
                    .expect("list");
                black_box(resp)
            }
        });
    });

    group.bench_function("entity_all_limit50", |b| {
        b.to_async(&rt).iter(|| {
            let registry = &registry;
            async move {
                let resp = registry
                    .dispatch("list", black_box(json!({ "kind": "entity", "limit": 50 })))
                    .await
                    .expect("list");
                black_box(resp)
            }
        });
    });

    group.finish();
}

// ── search ────────────────────────────────────────────────────────────────────

fn bench_search(c: &mut Criterion) {
    let rt = tokio_rt();
    let mut group = c.benchmark_group("kg_search");
    group.sample_size(30);

    for &corpus in &[100usize, 500, 1000] {
        let (_, registry) = build_registry();
        rt.block_on(async { seed_entities(&registry, corpus).await });

        group.bench_with_input(BenchmarkId::new("entity_fts", corpus), &corpus, |b, _| {
            b.to_async(&rt).iter(|| {
                let registry = &registry;
                async move {
                    let resp = registry
                        .dispatch(
                            "search",
                            black_box(json!({
                                "kind": "entity",
                                "query": "knowledge graph retrieval benchmark semantic",
                                "limit": 20,
                            })),
                        )
                        .await
                        .expect("search");
                    black_box(resp)
                }
            });
        });
    }

    group.finish();
}

// ── link ─────────────────────────────────────────────────────────────────────

fn bench_link(c: &mut Criterion) {
    let rt = tokio_rt();
    let mut group = c.benchmark_group("kg_link");
    group.sample_size(50);

    group.bench_function("single_edge", |b| {
        let (_, registry) = build_registry();
        // Pre-seed a stable pair; each iteration re-upserts the same edge.
        let (src_id, tgt_id): (String, String) = rt.block_on(async {
            let src = registry
                .dispatch(
                    "create",
                    json!({"kind": "entity", "name": "LinkSrc", "entity_kind": "concept"}),
                )
                .await
                .expect("create src");
            let tgt = registry
                .dispatch(
                    "create",
                    json!({"kind": "entity", "name": "LinkTgt", "entity_kind": "concept"}),
                )
                .await
                .expect("create tgt");
            (
                src["id"].as_str().expect("src id").to_string(),
                tgt["id"].as_str().expect("tgt id").to_string(),
            )
        });

        b.to_async(&rt).iter(|| {
            let registry = &registry;
            let src = src_id.clone();
            let tgt = tgt_id.clone();
            async move {
                let resp = registry
                    .dispatch(
                        "link",
                        black_box(json!({
                            "source_id": src,
                            "target_id": tgt,
                            "relation": "extends",
                            "weight": 0.8,
                        })),
                    )
                    .await
                    .expect("link");
                black_box(resp)
            }
        });
    });

    group.finish();
}

// ── neighbors ─────────────────────────────────────────────────────────────────

fn bench_neighbors(c: &mut Criterion) {
    let rt = tokio_rt();
    let (_, registry) = build_registry();

    // Build a hub-and-spoke: hub → 100 leaves.
    let hub_id: String = rt.block_on(async {
        let hub = registry
            .dispatch(
                "create",
                json!({"kind": "entity", "name": "Hub", "entity_kind": "concept"}),
            )
            .await
            .expect("hub");
        let hub_id = hub["id"].as_str().expect("id").to_string();
        for i in 0..100 {
            let leaf = registry
                .dispatch(
                    "create",
                    json!({"kind": "entity", "name": format!("Leaf{i:03}"), "entity_kind": "concept"}),
                )
                .await
                .expect("leaf");
            let leaf_id = leaf["id"].as_str().expect("id").to_string();
            registry
                .dispatch(
                    "link",
                    json!({"source_id": hub_id, "target_id": leaf_id, "relation": "contains"}),
                )
                .await
                .expect("link leaf");
        }
        hub_id
    });

    let mut group = c.benchmark_group("kg_neighbors");
    group.sample_size(50);

    group.bench_function("hub_100_out", |b| {
        b.to_async(&rt).iter(|| {
            let registry = &registry;
            let id = hub_id.clone();
            async move {
                let resp = registry
                    .dispatch(
                        "neighbors",
                        black_box(json!({ "node_id": id, "direction": "out" })),
                    )
                    .await
                    .expect("neighbors");
                black_box(resp)
            }
        });
    });

    group.finish();
}

// ── traverse ──────────────────────────────────────────────────────────────────

fn bench_traverse(c: &mut Criterion) {
    let rt = tokio_rt();
    let mut group = c.benchmark_group("kg_traverse");
    group.sample_size(30);

    for &depth in &[1u32, 2, 3] {
        // Fresh registry per depth variant so graph sizes stay comparable.
        let (_, registry) = build_registry();
        let chain_len = 10usize.pow(depth);
        let ids: Vec<String> =
            rt.block_on(async { seed_graph(&registry, chain_len.min(100)).await });
        let root_id = ids[0].clone();

        group.bench_with_input(
            BenchmarkId::new("chain_depth", depth),
            &depth,
            |b, &depth| {
                b.to_async(&rt).iter(|| {
                    let registry = &registry;
                    let root = root_id.clone();
                    async move {
                        let resp = registry
                            .dispatch(
                                "traverse",
                                black_box(json!({
                                    "roots": [root],
                                    "max_depth": depth,
                                    "direction": "out",
                                    "include_roots": false,
                                })),
                            )
                            .await
                            .expect("traverse");
                        black_box(resp)
                    }
                });
            },
        );
    }

    group.finish();
}

// ── criterion entry points ────────────────────────────────────────────────────

criterion_group!(
    kg_benches,
    bench_create,
    bench_get,
    bench_list,
    bench_search,
    bench_link,
    bench_neighbors,
    bench_traverse,
);
criterion_main!(kg_benches);
