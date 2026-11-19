use std::cell::OnceCell;
use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use criterion::{black_box, BenchmarkId, Criterion, Throughput};
use serde_json::json;

use super::{bulk_params, executor, measure, report, reports_enabled, Fixture, REPORT_SAMPLES};

const UPDATES: usize = 1000;

fn update_batch(rt: &tokio::runtime::Runtime, corpus: usize) -> (Duration, Vec<Duration>) {
    let fixture = Fixture::new();
    let ids = rt.block_on(fixture.seed_entities(corpus, false));
    let arguments: Vec<_> = (0..UPDATES)
        .map(|step| {
            json!({
                "id":ids[step % corpus], "properties":{"benchmark_step":step}
            })
        })
        .collect();
    let mut durations = Vec::with_capacity(UPDATES);
    let mut results = Vec::with_capacity(UPDATES);
    let batch_start = Instant::now();
    for params in arguments {
        let start = Instant::now();
        let result = rt.block_on(fixture.registry.dispatch("update", black_box(params)));
        durations.push(start.elapsed());
        results.push(result);
    }
    let batch_elapsed = batch_start.elapsed();
    for (step, result) in results.into_iter().enumerate() {
        let row = result.expect("update committed");
        assert_eq!(row["id"], ids[step % corpus].to_string());
        assert_eq!(row["properties"]["benchmark_step"], step);
        assert_eq!(row["version"], 2 + step / corpus);
    }
    rt.block_on(async {
        for (index, id) in ids.iter().enumerate() {
            let entity = fixture
                .runtime
                .get_entity(&fixture.token, *id)
                .await
                .unwrap();
            assert_eq!(
                entity.properties.as_ref().unwrap()["benchmark_step"],
                UPDATES - corpus + index
            );
            assert_eq!(entity.version, 1 + (UPDATES / corpus) as i64);
        }
    });
    fixture.close();
    (batch_elapsed, durations)
}

pub fn bench_updates(c: &mut Criterion) {
    let rt = OnceCell::new();
    let reports = reports_enabled();
    let mut group = c.benchmark_group("kg_update_dispatch");
    group.sample_size(10);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(3));
    group.throughput(Throughput::Elements(UPDATES as u64));
    for corpus in [100, 1000] {
        let mut reported = false;
        group.bench_with_input(
            BenchmarkId::new("1000_updates", corpus),
            &corpus,
            |b, &corpus| {
                let rt = rt.get_or_init(executor);
                if reports && !reported {
                    let (batch, calls) = update_batch(rt, corpus);
                    report(
                        &format!("kg_update_dispatch/corpus{corpus}/single_call"),
                        &calls,
                        None,
                    );
                    println!(
                        "{}",
                        json!({"scenario":format!("kg_update_dispatch/corpus{corpus}/1000_updates"),
                        "updates":UPDATES, "batch_seconds":batch.as_secs_f64(),
                        "updates_per_second":UPDATES as f64 / batch.as_secs_f64()})
                    );
                    reported = true;
                }
                b.iter_custom(|iterations| {
                    (0..iterations).map(|_| update_batch(rt, corpus).0).sum()
                });
            },
        );
    }
    group.finish();
}

fn merge_once(rt: &tokio::runtime::Runtime, incident: usize) -> Duration {
    let fixture = Fixture::new();
    let ids = rt.block_on(fixture.seed_entities(2 + incident * 2, true));
    let from = ids[0];
    let into = ids[1];
    let pairs: Vec<_> = (0..incident)
        .map(|i| (from, ids[2 + i]))
        .chain((0..incident).map(|i| (into, ids[2 + incident + i])))
        .collect();
    let edges = rt.block_on(fixture.seed_edges(&pairs));
    let args = json!({"from_id":from, "into_id":into, "kind":"entity"});
    let (elapsed, result) = measure(rt, &fixture, "merge", args);
    assert_eq!(result["kept_id"], into.to_string());
    assert_eq!(result["removed_id"], from.to_string());
    assert_eq!(result["edges_rewired"], incident);
    assert_eq!(result["edges_self_loop_dropped"], 0);
    assert_eq!(result["edges_contract_skipped"], 0);
    assert_eq!(result["dry_run"], false);
    assert!(result["edge_conflict_preimages"]
        .as_array()
        .unwrap()
        .is_empty());
    rt.block_on(async {
        let removed = fixture
            .runtime
            .get_entity_including_deleted(&fixture.token, from)
            .await
            .unwrap()
            .expect("source tombstone");
        assert!(removed.deleted_at.is_some());
        assert_eq!(removed.merged_into, Some(into));
        assert!(fixture
            .runtime
            .get_entity(&fixture.token, into)
            .await
            .unwrap()
            .deleted_at
            .is_none());
        let graph = fixture.runtime.graph(&fixture.token).unwrap();
        let actual = graph
            .get_edges(&edges.iter().map(|e| e.id).collect::<Vec<_>>())
            .await
            .unwrap();
        assert_eq!(actual.len(), 2 * incident);
        let expected: BTreeSet<_> = edges
            .iter()
            .map(|e| (uuid::Uuid::from(e.id), into, e.target_id))
            .collect();
        assert_eq!(
            actual
                .iter()
                .map(|e| (uuid::Uuid::from(e.id), e.source_id, e.target_id))
                .collect::<BTreeSet<_>>(),
            expected
        );
        for edge in actual {
            assert!(edge.deleted_at.is_none());
            assert_eq!(edge.relation, khive_types::EdgeRelation::Contains);
        }
    });
    drop(result);
    fixture.close();
    elapsed
}

pub fn bench_merge(c: &mut Criterion) {
    let rt = OnceCell::new();
    let reports = reports_enabled();
    for incident in [0, 100, 1000] {
        let mut reported = false;
        // Separate groups keep the zero-edge case from inheriting a fabricated
        // positive throughput denominator from another case.
        let mut group = c.benchmark_group(format!("kg_merge_dispatch/incident_each/{incident}"));
        group.sample_size(10);
        group.warm_up_time(Duration::from_secs(1));
        group.measurement_time(Duration::from_secs(3));
        if incident > 0 {
            group.throughput(Throughput::Elements(incident as u64));
        }
        group.bench_function("merge", |b| {
            let rt = rt.get_or_init(executor);
            if reports && !reported {
                let samples: Vec<_> = (0..REPORT_SAMPLES)
                    .map(|_| merge_once(rt, incident))
                    .collect();
                report(
                    &format!("kg_merge_dispatch/incident_each/{incident}"),
                    &samples,
                    (incident > 0).then_some(incident),
                );
                reported = true;
            }
            b.iter_custom(|iterations| (0..iterations).map(|_| merge_once(rt, incident)).sum());
        });
        group.finish();
    }
}

fn bulk_once(rt: &tokio::runtime::Runtime, count: usize) -> Duration {
    let fixture = Fixture::new();
    let names: Vec<_> = (0..count)
        .map(|i| format!("Generated bulk concept {i:04}"))
        .collect();
    let arguments = bulk_params(&names);
    let (elapsed, result) = measure(rt, &fixture, "create", arguments);
    rt.block_on(fixture.check_bulk(&result, &names));
    drop(result);
    fixture.close();
    elapsed
}

pub fn bench_bulk(c: &mut Criterion) {
    let rt = OnceCell::new();
    let reports = reports_enabled();
    let mut group = c.benchmark_group("kg_create_bulk_dispatch");
    group.sample_size(10);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(3));
    for count in [10, 100, 1000] {
        let mut reported = false;
        group.throughput(Throughput::Elements(count as u64));
        group.bench_with_input(BenchmarkId::new("atomic", count), &count, |b, &count| {
            let rt = rt.get_or_init(executor);
            if reports && !reported {
                let samples: Vec<_> = (0..REPORT_SAMPLES).map(|_| bulk_once(rt, count)).collect();
                report(
                    &format!("kg_create_bulk_dispatch/atomic/{count}"),
                    &samples,
                    Some(count),
                );
                reported = true;
            }
            b.iter_custom(|iterations| (0..iterations).map(|_| bulk_once(rt, count)).sum());
        });
    }
    group.finish();
}
