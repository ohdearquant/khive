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

// ── stats ─────────────────────────────────────────────────────────────────────

fn bench_stats(c: &mut Criterion) {
    let rt = tokio_rt();
    let (_, registry) = build_registry();

    // Shaped like a working store: a structural chain between entities, one
    // more structural edge per entity, and provenance notes annotating them,
    // which is where most edges on a real store come from.
    rt.block_on(async {
        let ids = seed_graph(&registry, 1000).await;
        for (i, id) in ids.iter().enumerate() {
            registry
                .dispatch(
                    "link",
                    json!({
                        "source_id": id,
                        "target_id": ids[(i * 7 + 3) % ids.len()],
                        "relation": "enables",
                    }),
                )
                .await
                .expect("seed structural edge");
        }
        for i in 0..2000 {
            let note = registry
                .dispatch(
                    "create",
                    json!({"kind": "observation", "content": format!("bench provenance note {i}")}),
                )
                .await
                .expect("seed note");
            let note_id = note["id"].as_str().expect("id").to_string();
            registry
                .dispatch(
                    "link",
                    json!({
                        "source_id": note_id,
                        "target_id": ids[i % ids.len()],
                        "relation": "annotates",
                    }),
                )
                .await
                .expect("seed annotates edge");
        }
    });

    let mut group = c.benchmark_group("kg_stats");
    group.sample_size(30);

    group.bench_function("entities_1000_edges_3999", |b| {
        b.to_async(&rt).iter(|| {
            let registry = &registry;
            async move {
                let resp = registry
                    .dispatch("stats", black_box(json!({})))
                    .await
                    .expect("stats");
                black_box(resp)
            }
        });
    });

    group.finish();
}

// ── note pages across visible namespaces ──────────────────────────────────────

fn bench_list_notes_visible(c: &mut Criterion) {
    use khive_runtime::Namespace;
    use khive_storage::note::NoteFilter;

    let rt = tokio_rt();
    let (runtime, _) = build_registry();
    let ns_a = Namespace::parse("bench-list-a").expect("namespace");
    let ns_b = Namespace::parse("bench-list-b").expect("namespace");
    // A token that reads its own namespace and one more, as an actor configured
    // with a second visible namespace does, and a token that reads one.
    let both = runtime
        .authorize_with_visibility(ns_a.clone(), vec![ns_b.clone()])
        .expect("token");
    let only_b = runtime.authorize(ns_b).expect("token");

    rt.block_on(async {
        let only_a = runtime.authorize(ns_a).expect("token");
        for i in 0..5000 {
            for token in [&only_a, &only_b] {
                runtime
                    .create_note(
                        token,
                        "observation",
                        None,
                        &format!("bench listed note {i}"),
                        None,
                        None,
                        vec![],
                    )
                    .await
                    .expect("seed note");
            }
        }
    });

    let mut group = c.benchmark_group("kg_list_notes");
    group.sample_size(50);

    for (label, token) in [
        ("two_namespaces_10000_limit20", &both),
        ("one_namespace_5000_limit20", &only_b),
    ] {
        group.bench_function(label, |b| {
            b.to_async(&rt).iter(|| {
                let runtime = &runtime;
                async move {
                    let page = runtime
                        .list_notes_filtered(token, black_box(NoteFilter::default()), 20, 0)
                        .await
                        .expect("list notes");
                    assert_eq!(page.len(), 20);
                    black_box(page)
                }
            });
        });
    }

    group.finish();
}

// ── delete ────────────────────────────────────────────────────────────────────

fn bench_delete(c: &mut Criterion) {
    let rt = tokio_rt();
    let (_, registry) = build_registry();

    // About 9,000 edges that do not touch the deleted entity: the chain from
    // `seed_graph` and eight structural edges out of every entity.
    let ids = rt.block_on(async {
        let ids = seed_graph(&registry, 1000).await;
        for stride in 1..=8 {
            for (i, id) in ids.iter().enumerate() {
                let target = (i * (2 * stride + 1) + stride) % ids.len();
                if target == i {
                    continue;
                }
                registry
                    .dispatch(
                        "link",
                        json!({
                            "source_id": id,
                            "target_id": ids[target],
                            "relation": "enables",
                        }),
                    )
                    .await
                    .expect("seed structural edge");
            }
        }
        ids
    });

    let mut group = c.benchmark_group("kg_delete");
    group.sample_size(30);

    // One iteration creates an entity, links it to the graph and deletes it.
    // A hard delete also removes its incident edges and records lineage
    // warnings; a soft delete only tombstones the row.
    for (label, hard) in [
        ("hard_entity_one_edge_edges_9000", true),
        ("soft_entity_one_edge_edges_9000", false),
    ] {
        group.bench_function(label, |b| {
            let mut seq: u64 = 0;
            b.to_async(&rt).iter(|| {
                seq += 1;
                let registry = &registry;
                let anchor = ids[(seq as usize) % ids.len()].clone();
                async move {
                    let created = registry
                        .dispatch(
                            "create",
                            json!({
                                "kind": "entity",
                                "name": format!("BenchDelete{label}{seq}"),
                                "entity_kind": "concept",
                            }),
                        )
                        .await
                        .expect("create entity");
                    let id = created["id"].as_str().expect("id").to_string();
                    registry
                        .dispatch(
                            "link",
                            json!({"source_id": id, "target_id": anchor, "relation": "enables"}),
                        )
                        .await
                        .expect("link entity");
                    let resp = registry
                        .dispatch("delete", black_box(json!({"id": id, "hard": hard})))
                        .await
                        .expect("delete entity");
                    black_box(resp)
                }
            });
        });
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
    bench_stats,
    bench_list_notes_visible,
    bench_delete,
);
criterion_main!(kg_benches);
