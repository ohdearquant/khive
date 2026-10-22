//! Product merge and contended link workloads, identical before/after governance migration061.

#[allow(dead_code)]
mod support;

use std::sync::Arc;
use std::time::{Duration, Instant};

use criterion::{black_box, criterion_group, Criterion, Throughput};
use khive_runtime::{ContentMergeStrategy, EntityDedupMergePolicy};
use khive_storage::{Entity, Note};
use khive_types::EdgeRelation;
use serde_json::{json, Value};
use support::{executor, id, Fixture};
use tempfile::TempDir;
use tokio::runtime::Runtime;

const INCIDENT_EDGES: usize = 8;
const WRITES_PER_PRODUCER: usize = 64;

fn fresh(rt: &Runtime) -> (Fixture, TempDir) {
    let dir = tempfile::tempdir().expect("private workload directory");
    let _entered = rt.enter();
    let fx = Fixture::file_backed(dir.path().join("edges.db"));
    assert!(
        fx.runtime
            .backend()
            .pool()
            .writer_task_handle()
            .expect("writer initialization")
            .is_some(),
        "measurement requires the writer queue"
    );
    (fx, dir)
}

fn close(rt: &Runtime, fx: Fixture, dir: TempDir) {
    let join = fx.runtime.backend().pool().take_writer_task_join();
    drop(fx);
    if let Some(join) = join {
        rt.block_on(join).expect("drain fixture writer");
    }
    dir.close().expect("remove fixture database");
}

fn notes(count: usize) -> Vec<Note> {
    (0..count)
        .map(|index| Note {
            id: id(index),
            namespace: "local".into(),
            kind: "memory".into(),
            status: "active".into(),
            name: None,
            content: "same merge memory".into(),
            salience: Some(0.5),
            decay_factor: None,
            expires_at: None,
            properties: None,
            created_at: 100,
            updated_at: 100,
            deleted_at: None,
            key: None,
            version: 1,
        })
        .collect()
}

fn bench_merge(c: &mut Criterion) {
    let rt = executor();
    let mut group = c.benchmark_group("product_merge");
    group.throughput(Throughput::Elements(INCIDENT_EDGES as u64));
    for (name, is_note) in [
        ("entity_8_incident_edges", false),
        ("note_8_incident_edges", true),
    ] {
        group.bench_function(name, |b| {
            b.iter_custom(|iterations| {
                let mut elapsed = Duration::ZERO;
                for _ in 0..iterations {
                    let (fx, dir) = fresh(&rt);
                    rt.block_on(async {
                        let summary = if is_note {
                            fx.runtime
                                .notes(&fx.token)
                                .unwrap()
                                .upsert_notes(notes(INCIDENT_EDGES + 2))
                                .await
                        } else {
                            let entities = (0..INCIDENT_EDGES + 2)
                                .map(|index| {
                                    Entity::minimal(
                                        id(index),
                                        "local",
                                        "concept",
                                        "same merge entity",
                                        100,
                                        100,
                                    )
                                })
                                .collect();
                            fx.runtime
                                .entities(&fx.token)
                                .unwrap()
                                .upsert_entities(entities)
                                .await
                        }
                        .expect("merge endpoints");
                        assert_eq!(summary.affected, (INCIDENT_EDGES + 2) as u64);
                        assert_eq!(summary.failed, 0);
                    });
                    let mut edges = Vec::new();
                    for target in 2..INCIDENT_EDGES + 2 {
                        edges.push(
                            rt.block_on(fx.runtime.link(
                                &fx.token,
                                id(1),
                                id(target),
                                if is_note {
                                    EdgeRelation::Supersedes
                                } else {
                                    EdgeRelation::Extends
                                },
                                1.0,
                                None,
                            ))
                            .expect("incident fixture edge"),
                        );
                    }
                    let start = Instant::now();
                    let result = rt.block_on(async {
                        if is_note {
                            fx.runtime
                                .merge_note(
                                    &fx.token,
                                    id(0),
                                    id(1),
                                    EntityDedupMergePolicy::PreferInto,
                                    ContentMergeStrategy::Append,
                                    false,
                                )
                                .await
                        } else {
                            fx.runtime
                                .merge_entity(
                                    &fx.token,
                                    id(0),
                                    id(1),
                                    EntityDedupMergePolicy::PreferInto,
                                    ContentMergeStrategy::Append,
                                    false,
                                )
                                .await
                        }
                    });
                    elapsed += start.elapsed();
                    assert_eq!(
                        black_box(result.expect("product merge")).edges_rewired,
                        INCIDENT_EDGES
                    );
                    let graph = fx.runtime.graph(&fx.token).expect("graph verification");
                    for edge in edges {
                        let actual = rt
                            .block_on(graph.get_edge(edge.id))
                            .unwrap()
                            .expect("rewired edge");
                        assert_eq!(actual.source_id, id(0));
                        assert_eq!(actual.target_id, edge.target_id);
                        assert_eq!(actual.relation, edge.relation);
                    }
                    drop(graph);
                    close(&rt, fx, dir);
                }
                elapsed
            })
        });
    }
    group.finish();
}

async fn admissions(fx: &Fixture) -> Value {
    let diagnostics = serde_json::to_value(fx.runtime.db_diagnostics().await.expect("diagnostics"))
        .expect("serialize admission histograms");
    let value = &diagnostics["writer_contention"]["writer_admission"];
    assert!(
        value.is_object(),
        "all-admissions telemetry unavailable: {diagnostics}"
    );
    value.clone()
}

fn histogram_delta(before: &Value, after: &Value, field: &str, expected: u64) -> Value {
    let before = &before[field];
    let after = &after[field];
    let count = after["count"]
        .as_u64()
        .unwrap()
        .checked_sub(before["count"].as_u64().unwrap())
        .unwrap();
    assert_eq!(
        count, expected,
        "{field}: exactly one admission per composed link"
    );
    let old = before["buckets"].as_array().expect("before buckets");
    let new = after["buckets"].as_array().expect("after buckets");
    assert_eq!(old.len(), new.len());
    let buckets: Vec<_> = old
        .iter()
        .zip(new)
        .map(|(old, new)| {
            assert_eq!(old["le_seconds"], new["le_seconds"]);
            json!({"le_seconds": new["le_seconds"], "count": new["count"].as_u64().unwrap()
            .checked_sub(old["count"].as_u64().unwrap()).unwrap()})
        })
        .collect();
    let overflow = after["overflow_count"]
        .as_u64()
        .unwrap()
        .checked_sub(before["overflow_count"].as_u64().unwrap())
        .unwrap();
    assert_eq!(
        buckets
            .iter()
            .map(|bucket| bucket["count"].as_u64().unwrap())
            .sum::<u64>()
            + overflow,
        count
    );
    let rank = (95 * count).div_ceil(100);
    let mut cumulative = 0;
    let bound = buckets
        .iter()
        .find_map(|bucket| {
            cumulative += bucket["count"].as_u64().unwrap();
            (cumulative >= rank).then(|| bucket["le_seconds"].as_f64().unwrap())
        })
        .expect("p95 outside finite histogram range");
    json!({"count": count, "p95_le_seconds": bound, "buckets": buckets,
        "overflow_count": overflow, "baseline_count": before["count"],
        "lifetime_p95_le_seconds": after["p95_le_seconds"]})
}

fn bench_writer_queue(c: &mut Criterion) {
    let rt = executor();
    let mut group = c.benchmark_group("contended_link_writes");
    group.sample_size(10);
    for producers in [4, 16] {
        let count = producers * WRITES_PER_PRODUCER;
        group.throughput(Throughput::Elements(count as u64));
        group.bench_function(format!("producers_{producers}_writes_64_each"), |b| {
            b.iter_custom(|iterations| {
                let mut elapsed = Duration::ZERO;
                for _ in 0..iterations {
                    let (fx, dir) = fresh(&rt);
                    let summary = rt
                        .block_on(
                            fx.runtime
                                .notes(&fx.token)
                                .unwrap()
                                .upsert_notes(notes(count + 1)),
                        )
                        .expect("link endpoints");
                    assert_eq!(summary.affected, (count + 1) as u64);
                    assert_eq!(summary.failed, 0);
                    let before = rt.block_on(admissions(&fx));
                    let start = Instant::now();
                    let mut samples = rt.block_on(async {
                        let barrier = Arc::new(tokio::sync::Barrier::new(producers));
                        let mut tasks = Vec::new();
                        for producer in 0..producers {
                            let runtime = fx.runtime.clone();
                            let token = fx.token.clone();
                            let barrier = Arc::clone(&barrier);
                            tasks.push(tokio::spawn(async move {
                                let mut samples = Vec::with_capacity(WRITES_PER_PRODUCER);
                                barrier.wait().await;
                                for index in 0..WRITES_PER_PRODUCER {
                                    let target = id(1 + producer * WRITES_PER_PRODUCER + index);
                                    let start = Instant::now();
                                    let edge = runtime
                                        .link(
                                            &token,
                                            id(0),
                                            target,
                                            EdgeRelation::Supersedes,
                                            1.0,
                                            None,
                                        )
                                        .await
                                        .expect("contended link");
                                    samples.push((
                                        u64::try_from(start.elapsed().as_nanos())
                                            .expect("latency range"),
                                        edge.target_id,
                                        target,
                                    ));
                                }
                                samples
                            }));
                        }
                        let mut samples = Vec::with_capacity(count);
                        for task in tasks {
                            samples.extend(task.await.expect("producer join"));
                        }
                        samples
                    });
                    elapsed += start.elapsed();
                    assert_eq!(samples.len(), count);
                    for (_, actual, expected) in &samples {
                        assert_eq!(actual, expected);
                    }
                    let after = rt.block_on(admissions(&fx));
                    let queue = histogram_delta(
                        &before,
                        &after,
                        "writer_admission_accepted_queue_age_seconds",
                        count as u64,
                    );
                    let service = histogram_delta(
                        &before,
                        &after,
                        "writer_admission_service_seconds",
                        count as u64,
                    );
                    samples.sort_unstable_by_key(|sample| sample.0);
                    let rank = (95 * count).div_ceil(100);
                    println!(
                        "writer-queue: {}",
                        json!({"producers": producers, "writes_per_producer": WRITES_PER_PRODUCER,
                    "admissions": count, "queue_age": queue, "service": service,
                    "caller_latency_p95_ns": samples[rank - 1].0, "caller_nearest_rank": rank,
                    "queue_depth_high_water": after["queue_depth_high_water"]})
                    );
                    let graph = fx.runtime.graph(&fx.token).unwrap();
                    assert_eq!(
                        rt.block_on(graph.count_edges(Default::default())).unwrap(),
                        count as u64
                    );
                    drop(graph);
                    close(&rt, fx, dir);
                }
                elapsed
            })
        });
    }
    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default().sample_size(10)
        .warm_up_time(Duration::from_millis(100)).measurement_time(Duration::from_secs(1));
    targets = bench_merge, bench_writer_queue
}
fn main() {
    // Cargo test must not construct cohorts unless a benchmark scene is requested.
    if !std::env::args().skip(1).any(|arg| {
        matches!(arg.as_str(), "--bench" | "--list" | "--help" | "-h")
            || arg.starts_with("product_merge")
            || arg.starts_with("contended_link_writes")
    }) {
        return;
    }
    benches();
    Criterion::default().configure_from_args().final_summary();
}
