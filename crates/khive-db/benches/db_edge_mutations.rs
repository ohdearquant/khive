//! Fixed graph-write workloads for comparing a base with installed invalidation triggers.
//! No governance API or marker is fabricated; these measure generic dormant-state writes.

use std::time::{Duration, Instant};

use criterion::{black_box, criterion_group, Criterion, Throughput};
use khive_storage::{BatchWriteSummary, DeleteMode, Edge, StorageError};
use khive_types::EdgeRelation;
use serde_json::json;
use tokio::runtime::Runtime;

#[allow(dead_code)]
#[path = "store_workloads/fixture.rs"]
mod fixture;

#[derive(Clone, Copy)]
enum Mutation {
    Singleton,
    Bulk,
    WeightMetadata,
    Endpoint,
    SoftDelete,
    HardDelete,
}

enum Outcome {
    Single(Result<(), StorageError>),
    Batch(Result<BatchWriteSummary, StorageError>),
    Delete(Result<bool, StorageError>),
}

fn inputs(rt: &Runtime, fx: &fixture::Fixture, count: usize) -> Vec<Edge> {
    let mut notes = fixture::notes(count + 2);
    for note in &mut notes {
        note.kind = "memory".into();
    }
    let ids: Vec<_> = notes.iter().map(|note| note.id).collect();
    fixture::complete(
        rt.block_on(fx.notes.upsert_notes(notes))
            .expect("memory endpoints"),
        count + 2,
    );
    let mut edges = fixture::edges(count);
    for (index, edge) in edges.iter_mut().enumerate() {
        edge.source_id = ids[0];
        edge.target_id = ids[index + 1];
        edge.relation = EdgeRelation::Supersedes;
    }
    edges
}

fn bench_mutations(c: &mut Criterion) {
    let rt = fixture::runtime();
    let mut group = c.benchmark_group("edge_mutation");
    group.sample_size(10);
    for (id, mutation, count) in [
        ("singleton_insert", Mutation::Singleton, 1),
        ("bulk_insert_100", Mutation::Bulk, 100),
        ("bulk_insert_1000", Mutation::Bulk, 1000),
        ("update_weight_metadata", Mutation::WeightMetadata, 1),
        ("update_endpoint", Mutation::Endpoint, 1),
        ("delete_soft", Mutation::SoftDelete, 1),
        ("delete_hard", Mutation::HardDelete, 1),
    ] {
        group.throughput(Throughput::Elements(count as u64));
        group.bench_function(id, |b| {
            b.iter_custom(|iterations| {
                let mut elapsed = Duration::ZERO;
                for _ in 0..iterations {
                    // Exactly one fixture at a time; all setup is outside the clock.
                    let fx = fixture::Fixture::new(&rt);
                    let mut records = inputs(&rt, &fx, count);
                    if !matches!(mutation, Mutation::Singleton | Mutation::Bulk) {
                        fixture::complete(
                            rt.block_on(fx.graph.upsert_edges(records.clone()))
                                .expect("existing edge"),
                            count,
                        );
                    }
                    match mutation {
                        Mutation::WeightMetadata => {
                            records[0].weight = 0.75;
                            records[0].metadata = Some(json!({"unchanged_identity": true}));
                        }
                        Mutation::Endpoint => {
                            records[0].target_id = fixture::notes(count + 2)[count + 1].id;
                        }
                        _ => {}
                    }
                    let expected = records[0].clone();
                    let single = expected.clone();
                    let start = Instant::now();
                    let result = rt.block_on(async {
                        match mutation {
                            Mutation::Bulk => Outcome::Batch(fx.graph.upsert_edges(records).await),
                            Mutation::SoftDelete => Outcome::Delete(
                                fx.graph.delete_edge(expected.id, DeleteMode::Soft).await,
                            ),
                            Mutation::HardDelete => Outcome::Delete(
                                fx.graph.delete_edge(expected.id, DeleteMode::Hard).await,
                            ),
                            _ => Outcome::Single(fx.graph.upsert_edge(single).await),
                        }
                    });
                    elapsed += start.elapsed();
                    match black_box(result) {
                        Outcome::Single(result) => result.expect("edge write"),
                        Outcome::Batch(result) => fixture::complete(result.expect("batch"), count),
                        Outcome::Delete(result) => assert!(result.expect("edge deletion")),
                    }
                    let stored = rt
                        .block_on(fx.graph.get_edge_including_deleted(expected.id))
                        .expect("verify persisted mutation");
                    match mutation {
                        Mutation::HardDelete => assert!(stored.is_none()),
                        Mutation::SoftDelete => {
                            assert!(stored.expect("retained tombstone").deleted_at.is_some());
                        }
                        _ => {
                            let actual = stored.expect("persisted edge");
                            assert_eq!(actual.source_id, expected.source_id);
                            assert_eq!(actual.target_id, expected.target_id);
                            assert_eq!(actual.relation, expected.relation);
                            assert_eq!(actual.weight, expected.weight);
                            assert_eq!(actual.metadata, expected.metadata);
                        }
                    }
                    let live = rt
                        .block_on(fx.graph.count_edges(Default::default()))
                        .expect("live edge count");
                    let deleted = matches!(mutation, Mutation::SoftDelete | Mutation::HardDelete);
                    assert_eq!(live, if deleted { 0 } else { count as u64 });
                    fx.close(&rt);
                }
                elapsed
            });
        });
    }
    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default().sample_size(10)
        .warm_up_time(Duration::from_millis(100)).measurement_time(Duration::from_secs(1));
    targets = bench_mutations
}
fn main() {
    // Cargo test must not construct cohorts unless a benchmark scene is requested.
    if !std::env::args().skip(1).any(|arg| {
        matches!(arg.as_str(), "--bench" | "--list" | "--help" | "-h")
            || arg.starts_with("edge_mutation")
    }) {
        return;
    }
    benches();
    Criterion::default().configure_from_args().final_summary();
}
