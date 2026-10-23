use std::cell::OnceCell;
use std::collections::BTreeMap;
use std::time::Duration;

use criterion::{BenchmarkId, Criterion};
use khive_storage::{Event, EventFilter};
use khive_types::{EventKind, SubstrateKind};
use serde_json::{json, Value};
use uuid::Uuid;

use super::{
    complete, executor, measure, report, reports_enabled, Fixture, ACTOR, MICROS, REPORT_SAMPLES,
};

const EVENTS: usize = 10_000;
const VERBS: [&str; 4] = ["create", "update", "get", "search"];

async fn seed_events(fixture: &Fixture) {
    let store = fixture.runtime.events(&fixture.token).unwrap();
    assert_eq!(store.count_events(EventFilter::default()).await.unwrap(), 0);
    for start in (0..EVENTS).step_by(1000) {
        let records = (start..start + 1000)
            .map(|index| {
                let kind = [
                    EventKind::Audit,
                    EventKind::EntityCreated,
                    EventKind::EntityUpdated,
                ][index % 3];
                let mut event = Event::new(
                    "local",
                    VERBS[index % VERBS.len()],
                    kind,
                    SubstrateKind::Entity,
                    ACTOR,
                );
                event.id = Uuid::from_u128((5660_u128 << 64) | (index as u128 + 1));
                event.created_at = MICROS + index as i64;
                event
            })
            .collect();
        complete(
            store
                .append_events(records)
                .await
                .expect("seed generated events"),
            1000,
        );
    }
    assert_eq!(
        store.count_events(EventFilter::default()).await.unwrap(),
        EVENTS as u64
    );
}

fn expected_counts(window: bool) -> Value {
    let mut counts = BTreeMap::<&str, u64>::new();
    for index in 0..EVENTS {
        if !window || (index > 2500 && index < 7500) {
            *counts.entry(VERBS[index % VERBS.len()]).or_default() += 1;
        }
    }
    serde_json::to_value(counts).unwrap()
}

pub fn bench_count(c: &mut Criterion) {
    let rt = OnceCell::new();
    let fixture = OnceCell::new();
    let reports = reports_enabled();
    let mut group = c.benchmark_group("kg_count_dispatch");
    group.sample_size(10);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(3));
    for (name, window) in [("events10000/all", false), ("events10000/window", true)] {
        let mut params = json!({"kind":"event", "group_by":"verb"});
        if window {
            params["since"] = json!(MICROS + 2500);
            params["until"] = json!(MICROS + 7500);
        }
        let expected = expected_counts(window);
        let mut reported = false;
        group.bench_function(name, |b| {
            let rt = rt.get_or_init(executor);
            let fixture = fixture.get_or_init(|| {
                let fixture = Fixture::new();
                rt.block_on(seed_events(&fixture));
                fixture
            });
            let sample = || {
                let prepared = params.clone();
                let (elapsed, result) = measure(rt, fixture, "count", prepared);
                assert_eq!(result, expected);
                elapsed
            };
            if reports && !reported {
                let samples: Vec<_> = (0..REPORT_SAMPLES).map(|_| sample()).collect();
                report(&format!("kg_count_dispatch/{name}"), &samples, None);
                reported = true;
            }
            b.iter_custom(|iterations| (0..iterations).map(|_| sample()).sum());
        });
    }
    group.finish();
    if let Some(fixture) = fixture.into_inner() {
        assert_eq!(
            rt.get()
                .expect("initialized fixture executor")
                .block_on(
                    fixture
                        .runtime
                        .events(&fixture.token)
                        .unwrap()
                        .count_events(EventFilter::default())
                )
                .unwrap(),
            EVENTS as u64
        );
        fixture.close();
    }
}

fn check_path(result: &Value, ids: &[Uuid], depth: usize, direction: &str, roots: bool) {
    let paths = result.as_array().expect("traversal paths");
    assert_eq!(paths.len(), 1);
    assert_eq!(paths[0]["root_id"], ids[50].to_string());
    let nodes = paths[0]["nodes"].as_array().expect("path nodes");
    let mut expected = BTreeMap::new();
    if roots {
        expected.insert(ids[50], 0);
    }
    for distance in 1..=depth {
        if direction == "in" || direction == "both" {
            expected.insert(ids[50 - distance], distance);
        }
        if direction == "out" || direction == "both" {
            expected.insert(ids[50 + distance], distance);
        }
    }
    let mut actual = BTreeMap::new();
    for node in nodes {
        let id = Uuid::parse_str(node["id"].as_str().unwrap()).unwrap();
        let distance = node["depth"].as_u64().unwrap() as usize;
        assert!(
            actual.insert(id, distance).is_none(),
            "no duplicate path nodes"
        );
        assert_eq!(node["kind"], "concept");
        assert!(node["name"]
            .as_str()
            .unwrap()
            .starts_with("Generated concept "));
        if distance == 0 {
            assert!(node["via_edge"].is_null());
        } else {
            let index = ids.iter().position(|candidate| *candidate == id).unwrap();
            let edge_index = if index < 50 { index } else { index - 1 };
            let edge_id = Uuid::from_u128((5660_u128 << 64) | (edge_index as u128 + 1));
            assert_eq!(node["via_edge"], edge_id.to_string());
        }
    }
    assert_eq!(actual, expected);
}

pub fn bench_traverse(c: &mut Criterion) {
    let rt = OnceCell::new();
    let fixture = OnceCell::new();
    let reports = reports_enabled();
    let mut group = c.benchmark_group("kg_traverse_matrix");
    group.sample_size(10);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(3));
    for (direction, roots) in [("out", false), ("out", true), ("in", true), ("both", true)] {
        for depth in [1, 2, 3] {
            let name = format!("{direction}/include_roots_{roots}");
            let mut reported = false;
            group.bench_with_input(BenchmarkId::new(&name, depth), &depth, |b, _| {
                let rt = rt.get_or_init(executor);
                let (fixture, ids) = fixture.get_or_init(|| {
                    let fixture = Fixture::new();
                    let ids = rt.block_on(fixture.seed_entities(100, false));
                    let pairs: Vec<_> = ids.windows(2).map(|w| (w[0], w[1])).collect();
                    rt.block_on(fixture.seed_edges(&pairs));
                    (fixture, ids)
                });
                let params = json!({"roots":[ids[50]], "direction":direction,
                        "include_roots":roots, "max_depth":depth, "limit":100});
                let sample = || {
                    let prepared = params.clone();
                    let (elapsed, result) = measure(rt, fixture, "traverse", prepared);
                    check_path(&result, ids, depth, direction, roots);
                    elapsed
                };
                if reports && !reported {
                    let samples: Vec<_> = (0..REPORT_SAMPLES).map(|_| sample()).collect();
                    report(
                        &format!("kg_traverse_matrix/{name}/{depth}"),
                        &samples,
                        None,
                    );
                    reported = true;
                }
                b.iter_custom(|iterations| (0..iterations).map(|_| sample()).sum());
            });
        }
    }
    group.finish();
    if let Some((fixture, _)) = fixture.into_inner() {
        fixture.close();
    }
}
