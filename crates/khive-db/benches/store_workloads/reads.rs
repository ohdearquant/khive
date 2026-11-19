use std::time::{Duration, Instant};

use criterion::{black_box, BenchmarkId, Criterion};
use khive_storage::event::EventPageQuery;
use khive_storage::note::PropertyFilter;
use khive_storage::{
    Direction, EntityFilter, FilterOp, NeighborQuery, NoteFilter, PageRequest, SqlValue,
    TraversalOptions, TraversalRequest,
};

use crate::fixture::{self, complete, Fixture, CORPUS, EPOCH, EVENT_CORPUS, FANOUT, NAMESPACE};

pub fn bench_entities(c: &mut Criterion) {
    let rt = fixture::runtime();
    let fx = Fixture::new(&rt);
    complete(
        rt.block_on(fx.entities.upsert_entities(fixture::entities(CORPUS)))
            .expect("entity corpus"),
        CORPUS,
    );
    // Establish a real continuation after 5000 inserted rows, outside measurement.
    let mut after = None;
    for _ in 0..5 {
        let page = rt
            .block_on(fx.entities.query_entities_after(
                NAMESPACE,
                EntityFilter::default(),
                after,
                1000,
            ))
            .expect("seek fixture");
        assert_eq!(page.items.len(), 1000);
        after = Some(page.next_after.expect("more entity rows"));
    }
    let mut group = c.benchmark_group("entity_page");
    group.sample_size(20);
    for limit in [20u32, 50, 100] {
        group.bench_with_input(
            BenchmarkId::new("offset_5000", limit),
            &limit,
            |b, &limit| {
                let warm = rt
                    .block_on(fx.entities.query_entities(
                        NAMESPACE,
                        EntityFilter::default(),
                        PageRequest {
                            offset: 5000,
                            limit,
                        },
                    ))
                    .expect("warm offset");
                assert_eq!(warm.items.len(), limit as usize);
                assert_eq!(warm.total, Some(CORPUS as u64));
                b.iter_custom(|iterations| {
                    let mut elapsed = Duration::ZERO;
                    for _ in 0..iterations {
                        let filter = EntityFilter::default();
                        let page = PageRequest {
                            offset: 5000,
                            limit,
                        };
                        let start = Instant::now();
                        let result =
                            rt.block_on(fx.entities.query_entities(NAMESPACE, filter, page));
                        elapsed += start.elapsed();
                        let result = black_box(result.expect("offset page"));
                        assert_eq!(result.items.len(), limit as usize);
                        assert_eq!(result.total, Some(CORPUS as u64));
                    }
                    elapsed
                });
            },
        );
        group.bench_with_input(
            BenchmarkId::new("seek_after_5000", limit),
            &limit,
            |b, &limit| {
                let warm = rt
                    .block_on(fx.entities.query_entities_after(
                        NAMESPACE,
                        EntityFilter::default(),
                        after,
                        limit,
                    ))
                    .expect("warm seek");
                assert_eq!(warm.items.len(), limit as usize);
                b.iter_custom(|iterations| {
                    let mut elapsed = Duration::ZERO;
                    for _ in 0..iterations {
                        let filter = EntityFilter::default();
                        let start = Instant::now();
                        let result = rt.block_on(
                            fx.entities
                                .query_entities_after(NAMESPACE, filter, after, limit),
                        );
                        elapsed += start.elapsed();
                        assert_eq!(
                            black_box(result.expect("seek page")).items.len(),
                            limit as usize
                        );
                    }
                    elapsed
                });
            },
        );
    }
    group.finish();
    fx.close(&rt);
}

fn traversal(depth: usize) -> TraversalRequest {
    TraversalRequest {
        roots: vec![fixture::node_id(0)],
        options: TraversalOptions {
            max_depth: depth,
            limit: Some(1000),
            ..Default::default()
        },
        include_roots: false,
        include_properties: false,
        // A one-shot execution budget must be new for every call, never cloned/reused.
        execution_budget: Default::default(),
    }
}

pub fn bench_graph(c: &mut Criterion) {
    let rt = fixture::runtime();
    let fx = Fixture::new(&rt);
    complete(
        rt.block_on(fx.entities.upsert_entities(fixture::entities(CORPUS + 1)))
            .expect("graph endpoints"),
        CORPUS + 1,
    );
    complete(
        rt.block_on(fx.graph.upsert_edges(fixture::edges(CORPUS)))
            .expect("graph corpus"),
        CORPUS,
    );
    let mut group = c.benchmark_group("edge_neighbors");
    group.sample_size(20);
    group.bench_function("one_hop_10k_edges", |b| {
        let query = || NeighborQuery {
            direction: Direction::Out,
            relations: None,
            limit: Some(1000),
            min_weight: None,
        };
        assert_eq!(
            rt.block_on(fx.graph.neighbors(fixture::node_id(0), query()))
                .expect("warm neighbors")
                .len(),
            FANOUT
        );
        b.iter_custom(|iterations| {
            let mut elapsed = Duration::ZERO;
            for _ in 0..iterations {
                let request = query();
                let root = fixture::node_id(0);
                let start = Instant::now();
                let result = rt.block_on(fx.graph.neighbors(root, request));
                elapsed += start.elapsed();
                assert_eq!(black_box(result.expect("neighbors")).len(), FANOUT);
            }
            elapsed
        });
    });
    group.finish();
    let mut group = c.benchmark_group("edge_traverse");
    group.sample_size(20);
    for depth in [1usize, 2, 3] {
        let expected = (1..=depth).map(|d| FANOUT.pow(d as u32)).sum::<usize>();
        group.bench_with_input(BenchmarkId::new("depth", depth), &depth, |b, &depth| {
            let warm = rt
                .block_on(fx.graph.traverse(traversal(depth)))
                .expect("warm traversal");
            assert_eq!(warm.len(), 1);
            assert_eq!(warm[0].nodes.len(), expected);
            b.iter_custom(|iterations| {
                let mut elapsed = Duration::ZERO;
                for _ in 0..iterations {
                    let request = traversal(depth);
                    let start = Instant::now();
                    let result = rt.block_on(fx.graph.traverse(request));
                    elapsed += start.elapsed();
                    let result = black_box(result.expect("traverse"));
                    assert_eq!(result.len(), 1);
                    assert_eq!(result[0].nodes.len(), expected);
                }
                elapsed
            });
        });
    }
    group.finish();
    fx.close(&rt);
}

pub fn bench_notes(c: &mut Criterion) {
    let rt = fixture::runtime();
    let fx = Fixture::new(&rt);
    complete(
        rt.block_on(fx.notes.upsert_notes(fixture::notes(CORPUS)))
            .expect("note corpus"),
        CORPUS,
    );
    let mut group = c.benchmark_group("note_filtered_query");
    group.sample_size(20);
    for percent in [1usize, 10, 50] {
        let filter = NoteFilter {
            property_filters: vec![PropertyFilter {
                json_path: format!("$.p{percent}"),
                op: FilterOp::Eq,
                value: SqlValue::Bool(true),
            }],
            ..Default::default()
        };
        group.bench_with_input(
            BenchmarkId::new("percent", percent),
            &filter,
            |b, filter| {
                let warm = rt
                    .block_on(fx.notes.query_notes_filtered(
                        NAMESPACE,
                        filter,
                        PageRequest {
                            offset: 0,
                            limit: 100,
                        },
                    ))
                    .expect("warm notes");
                assert_eq!(warm.total, Some((CORPUS * percent / 100) as u64));
                assert_eq!(warm.items.len(), 100);
                b.iter_custom(|iterations| {
                    let mut elapsed = Duration::ZERO;
                    for _ in 0..iterations {
                        let page = PageRequest {
                            offset: 0,
                            limit: 100,
                        };
                        let start = Instant::now();
                        let result =
                            rt.block_on(fx.notes.query_notes_filtered(NAMESPACE, filter, page));
                        elapsed += start.elapsed();
                        let result = black_box(result.expect("filtered notes"));
                        assert_eq!(result.items.len(), 100);
                        assert_eq!(result.total, Some((CORPUS * percent / 100) as u64));
                    }
                    elapsed
                });
            },
        );
    }
    group.finish();
    fx.close(&rt);
}

pub fn bench_events(c: &mut Criterion) {
    let rt = fixture::runtime();
    let fx = Fixture::new(&rt);
    for start in (0..EVENT_CORPUS).step_by(1000) {
        complete(
            rt.block_on(fx.events.append_events(fixture::events(start, 1000)))
                .expect("event corpus batch"),
            1000,
        );
    }
    let mut group = c.benchmark_group("event_page");
    group.sample_size(20);
    for limit in [100u32, 500, 4096] {
        let query = || EventPageQuery {
            since_us: EPOCH,
            until_us: EPOCH + EVENT_CORPUS as i64,
            kinds: Vec::new(),
            actors: Vec::new(),
            exclude_namespaces: Vec::new(),
            after: None,
            max_rows: limit,
        };
        group.bench_with_input(BenchmarkId::new("rows", limit), &limit, |b, &limit| {
            let warm = rt
                .block_on(fx.events.query_event_page(query()))
                .expect("warm events");
            assert_eq!(warm.rows.len(), limit as usize);
            assert!(
                warm.budget_stop.is_none(),
                "fixture must fit the byte budget"
            );
            b.iter_custom(|iterations| {
                let mut elapsed = Duration::ZERO;
                for _ in 0..iterations {
                    let request = query();
                    let start = Instant::now();
                    let result = rt.block_on(fx.events.query_event_page(request));
                    elapsed += start.elapsed();
                    let result = black_box(result.expect("event page"));
                    assert_eq!(result.rows.len(), limit as usize);
                    assert!(result.budget_stop.is_none());
                }
                elapsed
            });
        });
    }
    group.finish();
    fx.close(&rt);
}
