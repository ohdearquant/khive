mod support;

use std::time::Duration;

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use khive_runtime::operations::entity_stats_counts;
use khive_storage::types::{Direction, TraversalOptions};
use support::{executor, id, Fixture, CONTENT, ROWS};

fn note_search(c: &mut Criterion) {
    let rt = executor();
    let fx = Fixture::new(false);
    rt.block_on(fx.seed_notes());
    let mut group = c.benchmark_group("runtime_search_notes");
    for (terms, query) in [
        (1, "knowledge"),
        (3, "knowledge graph memory"),
        (5, "knowledge graph memory retrieval runtime"),
    ] {
        for top_k in [10, 20, 50] {
            let search = || {
                fx.runtime
                    .search_notes(&fx.token, query, None, top_k, None, false, &[], None)
            };
            assert_eq!(
                rt.block_on(search()).expect("warm note search").len(),
                top_k as usize
            );
            group.bench_function(BenchmarkId::new(format!("terms_{terms}"), top_k), |b| {
                b.iter(|| black_box(rt.block_on(search()).expect("note search")));
            });
        }
    }
    group.finish();
}

fn hybrid_search(c: &mut Criterion) {
    let rt = executor();
    let fx = Fixture::new(true);
    rt.block_on(fx.seed_entities(true));
    // A supplied vector keeps this row about retrieval, separate from embed_batch.
    let query = rt
        .block_on(fx.runtime.embed_batch(&[CONTENT.into()]))
        .expect("stub query")
        .remove(0);
    let mut group = c.benchmark_group("runtime_hybrid_entities");
    for top_k in [10, 50] {
        assert_eq!(
            rt.block_on(fx.runtime.hybrid_search(
                &fx.token,
                "",
                Some(query.clone()),
                top_k,
                None,
                None,
                &[],
                None,
            ))
            .expect("vector-only preflight")
            .len(),
            top_k as usize,
            "a lexical fallback must not stand in for a working vector leg"
        );
        let search = || {
            fx.runtime.hybrid_search(
                &fx.token,
                "knowledge graph memory",
                Some(query.clone()),
                top_k,
                None,
                None,
                &[],
                None,
            )
        };
        assert_eq!(
            rt.block_on(search()).expect("warm hybrid search").len(),
            top_k as usize
        );
        group.bench_function(BenchmarkId::from_parameter(top_k), |b| {
            b.iter(|| black_box(rt.block_on(search()).expect("entity hybrid search")));
        });
    }
    group.finish();
}

fn graph_and_stats(c: &mut Criterion) {
    let rt = executor();
    let fx = Fixture::new(false);
    rt.block_on(fx.seed_graph(khive_types::EdgeRelation::DependsOn));
    let neighbors = || {
        fx.runtime
            .neighbors(&fx.token, id(0), Direction::Out, None, None)
    };
    assert_eq!(rt.block_on(neighbors()).expect("warm neighbors").len(), 4);
    c.bench_function("runtime_neighbors/10k_edges_one_hop", |b| {
        b.iter(|| black_box(rt.block_on(neighbors()).expect("neighbors")));
    });
    let mut group = c.benchmark_group("runtime_bfs");
    for depth in [1, 2, 3, 4] {
        let walk = || {
            fx.runtime
                .bfs_traverse(&fx.token, id(0), TraversalOptions::new(depth))
        };
        let expected = (0..=depth)
            .map(|level| 4_usize.pow(level as u32))
            .sum::<usize>();
        assert_eq!(rt.block_on(walk()).expect("warm BFS").len(), expected);
        group.bench_function(BenchmarkId::from_parameter(depth), |b| {
            b.iter(|| black_box(rt.block_on(walk()).expect("BFS")));
        });
    }
    group.finish();
    let entities = fx.runtime.entities(&fx.token).expect("entity store");
    assert_eq!(
        rt.block_on(entity_stats_counts(entities.as_ref(), &fx.token))
            .expect("warm counts")
            .entities,
        ROWS as u64
    );
    c.bench_function("runtime_entity_stats/10k", |b| {
        b.iter(|| {
            black_box(
                rt.block_on(entity_stats_counts(entities.as_ref(), &fx.token))
                    .expect("counts"),
            )
        });
    });
    let archive = rt
        .block_on(fx.runtime.export_kg(&fx.token))
        .expect("warm export");
    assert_eq!(archive.entities.len(), ROWS);
    assert_eq!(archive.edges.len(), ROWS);
    let mut group = c.benchmark_group("runtime_export_kg");
    group.throughput(Throughput::Elements(ROWS as u64));
    group.bench_function("10k_entities_10k_edges", |b| {
        b.iter(|| {
            black_box(
                rt.block_on(fx.runtime.export_kg(&fx.token))
                    .expect("export"),
            )
        });
    });
    group.finish();
}

fn embed_batch(c: &mut Criterion) {
    let rt = executor();
    let fx = Fixture::new(true);
    let mut group = c.benchmark_group("runtime_embed_batch_stub");
    for count in [1, 10, 100] {
        let texts: Vec<_> = (0..count).map(|i| format!("{CONTENT} {i}")).collect();
        assert_eq!(
            rt.block_on(fx.runtime.embed_batch(&texts))
                .expect("warm embed")
                .len(),
            count
        );
        group.bench_function(BenchmarkId::from_parameter(count), |b| {
            b.iter(|| {
                black_box(
                    rt.block_on(fx.runtime.embed_batch(&texts))
                        .expect("embed batch"),
                )
            });
        });
    }
    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default().sample_size(10).warm_up_time(Duration::from_millis(100)).measurement_time(Duration::from_secs(1));
    targets = note_search, hybrid_search, graph_and_stats, embed_batch
}
criterion_main!(benches);
