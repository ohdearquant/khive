//! Public resolution with 100 durable bindings; setup and assertions are untimed.
use std::cell::OnceCell;
use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use khive_pack_brain::BrainPack;
use khive_pack_kg::KgPack;
use khive_runtime::{KhiveRuntime, VerbRegistry, VerbRegistryBuilder};
use serde_json::{json, Value};

const BINDINGS: usize = 100;
const REPORT_SAMPLES: usize = 101;
const PROFILE: &str = "balanced-recall-v1";

fn actor(index: usize) -> String {
    format!("brain-resolve-{index:03}")
}

fn check_resolution(value: &Value) {
    assert_eq!(value["resolved_profile_id"], PROFILE);
    assert_eq!(value["requested_consumer_kind"], "recall");
    assert_eq!(value["matched_consumer_kind"], "recall");
    assert_eq!(value["matched_binding"], true, "must not time a fallback");
}

async fn build_registry() -> VerbRegistry {
    let runtime = KhiveRuntime::memory().expect("private in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.with_actor_id(Some(actor(BINDINGS - 1)));
    builder.register(KgPack::new(runtime.clone()));
    builder.register(BrainPack::new(runtime.clone()));
    let registry = builder.build().expect("registry");
    runtime.install_edge_rules(registry.all_edge_rules());

    for index in 0..BINDINGS {
        let expected_actor = actor(index);
        let response = registry
            .dispatch(
                "brain.bind",
                json!({
                    "profile_id": PROFILE,
                    "actor": expected_actor,
                    "consumer_kind": "recall"
                }),
            )
            .await
            .expect("public binding must persist");
        assert_eq!(response["bound"], true);
        assert_eq!(response["actor"], expected_actor);
        assert_eq!(response["namespace"], "*");
        assert_eq!(response["consumer_kind"], "recall");
        assert_eq!(response["profile_id"], PROFILE);
    }

    // Keep the storage namespace fixed. Distinct actors prevent binding replacement
    // without switching BrainPack's single namespace-specific state slot.
    let snapshot = registry
        .dispatch("brain.state", json!({}))
        .await
        .expect("public state witness");
    let rows = snapshot["bindings"].as_array().expect("binding snapshot");
    assert_eq!(rows.len(), BINDINGS);
    let expected_actors: BTreeSet<_> = (0..BINDINGS).map(actor).collect();
    let actual_actors: BTreeSet<_> = rows
        .iter()
        .map(|row| {
            assert_eq!(row["namespace"], "*");
            assert_eq!(row["consumer_kind"], "recall");
            assert_eq!(row["profile_id"], PROFILE);
            assert_eq!(row["priority"], 0);
            row["actor"].as_str().expect("actor string").to_owned()
        })
        .collect();
    assert_eq!(actual_actors, expected_actors);
    let visible = registry
        .dispatch("brain.bindings", json!({}))
        .await
        .expect("caller binding view");
    assert_eq!(visible["count"], 1);
    assert_eq!(visible["bindings"][0]["actor"], actor(BINDINGS - 1));
    let resolved = registry
        .dispatch("brain.resolve", json!({"consumer_kind": "recall"}))
        .await
        .expect("warm resolution");
    check_resolution(&resolved);
    registry
}

fn resolve_once(
    executor: &tokio::runtime::Runtime,
    registry: &VerbRegistry,
    params: &Value,
) -> Duration {
    let input = params.clone();
    let start = Instant::now();
    let result = executor.block_on(registry.dispatch("brain.resolve", black_box(input)));
    let elapsed = start.elapsed();
    let value = result.expect("brain.resolve dispatch");
    check_resolution(&value);
    black_box(&value);
    elapsed
}

fn resolve(c: &mut Criterion) {
    let executor = OnceCell::new();
    let registry = OnceCell::new();
    let before = OnceCell::new();
    let params = json!({"consumer_kind": "recall"});
    let report_enabled = reports_enabled();
    let mut reported = false;

    let mut group = c.benchmark_group("brain_pack/resolve");
    group.sample_size(50);
    group.throughput(Throughput::Elements(1));
    group.bench_function("100_bindings", |b| {
        let executor = executor.get_or_init(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .max_blocking_threads(2)
                .build()
                .expect("bounded executor")
        });
        let registry = registry.get_or_init(|| executor.block_on(build_registry()));
        before.get_or_init(|| {
            executor
                .block_on(registry.dispatch("brain.state", json!({})))
                .expect("before state")
        });
        if report_enabled && !reported {
            let mut samples: Vec<_> = (0..REPORT_SAMPLES)
                .map(|_| resolve_once(executor, registry, &params).as_nanos())
                .collect();
            samples.sort_unstable();
            println!(
                "{}",
                json!({
                    "scenario": "brain_resolve_100_bindings",
                    "bindings": BINDINGS,
                    "calls": samples.len(),
                    "median_ns": samples[REPORT_SAMPLES / 2],
                    "p99_ns": samples[99],
                    "percentile_method": "nearest-rank p99: 100th of 101 single-call samples"
                })
            );
            reported = true;
        }
        b.iter_custom(|iterations| {
            (0..iterations)
                .map(|_| resolve_once(executor, registry, &params))
                .sum::<Duration>()
        })
    });
    group.finish();

    if let Some(registry) = registry.get() {
        let after = executor
            .get()
            .expect("selected executor")
            .block_on(registry.dispatch("brain.state", json!({})))
            .expect("after state");
        let before = before.get().expect("selected before state");
        assert_eq!(after["bindings"], before["bindings"]);
        assert_eq!(after["profiles"], before["profiles"]);
    }
    drop(registry);
}

fn reports_enabled() -> bool {
    let mut bench = false;
    for arg in std::env::args_os().skip(1).take_while(|arg| arg != "--") {
        if arg == "--test"
            || arg == "--profile-time"
            || arg.to_string_lossy().starts_with("--profile-time=")
        {
            return false;
        }
        bench |= arg == "--bench";
    }
    bench
}

criterion_group!(benches, resolve);
criterion_main!(benches);
