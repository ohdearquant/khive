//! Same-source parent/new-head measurements of the real comm.thread handler.
use std::time::{Duration, Instant};

use criterion::{black_box, BenchmarkId, Criterion, SamplingMode, Throughput};
use khive_runtime::Namespace;
use khive_storage::Note;
use serde_json::{json, Value};
use uuid::Uuid;

use super::fixture::{Fixture, ACTOR};

const PHYSICAL_ROWS: u32 = 1_000;
const BODY_BYTES: usize = 4_096;

#[derive(Clone, Copy)]
enum Population {
    Unique,
    InvisibleAndTwins,
}

impl Population {
    const ALL: [Self; 2] = [Self::Unique, Self::InvisibleAndTwins];

    fn name(self) -> &'static str {
        match self {
            Self::Unique => "unique_1000",
            Self::InvisibleAndTwins => "hidden490_twins490_owners20",
        }
    }

    fn owner_read(self, index: u32) -> bool {
        matches!(self, Self::Unique) && index % 2 == 0
    }
}

fn id(index: u32) -> Uuid {
    Uuid::from_u128((0x1760_u128 << 112) | u128::from(index))
}

struct PreparedThread {
    fixture: Fixture,
    args: Value,
    population: Population,
    expected: Vec<u32>,
}

impl PreparedThread {
    async fn new(population: Population, limit: u32) -> Self {
        let fixture = Fixture::new();
        let root = id(1).to_string();
        let notes = (1..=PHYSICAL_ROWS)
            .map(|index| {
                let mut properties = json!({
                    "thread_id":root, "direction":"inbound",
                    "from_actor":"benchmark:sender", "to_actor":ACTOR,
                    "read":population.owner_read(index),
                });
                let mut created_at = i64::from(index);
                if matches!(population, Population::InvisibleAndTwins) {
                    if index <= 20 {
                        properties["direction"] = json!("outbound");
                        properties["from_actor"] = json!(ACTOR);
                        properties["to_actor"] = json!("benchmark:peer");
                        created_at += 10_000;
                    } else if index <= 510 {
                        properties["outbound_ref"] = json!(id((index - 21) % 20 + 1));
                        properties["read"] = json!(true);
                    } else {
                        properties["from_actor"] = json!("benchmark:hidden");
                        properties["to_actor"] = json!("benchmark:other");
                        created_at += 20_000;
                    }
                }
                let mut content = format!("Generated thread row {index:04}: ");
                content.push_str(&"x".repeat(BODY_BYTES - content.len()));
                Note {
                    id: id(index),
                    namespace: "local".into(),
                    kind: "message".into(),
                    status: "active".into(),
                    name: None,
                    content,
                    salience: None,
                    decay_factor: None,
                    expires_at: None,
                    properties: Some(properties),
                    created_at,
                    updated_at: created_at,
                    deleted_at: None,
                    key: None,
                    version: 1,
                }
            })
            .collect::<Vec<_>>();
        assert_eq!(notes.len(), PHYSICAL_ROWS as usize);
        let token = fixture
            .runtime
            .authorize(Namespace::local())
            .expect("local token");
        let _ = fixture
            .runtime
            .notes(&token)
            .expect("same routed notes store used by thread")
            .upsert_notes(notes)
            .await
            .expect("seed actual typed notes store");
        let last_owner = if matches!(population, Population::Unique) {
            PHYSICAL_ROWS
        } else {
            20
        };
        Self {
            fixture,
            args: json!({"id":root,"order":"desc","limit":limit}),
            population,
            expected: (1..=last_owner).rev().take(limit as usize).collect(),
        }
    }

    async fn check(&self, response: &Value) {
        assert_eq!(response["thread_id"], id(1).to_string());
        assert_eq!(response["count"], self.expected.len());
        let messages = response["messages"].as_array().expect("thread messages");
        assert_eq!(messages.len(), self.expected.len());
        for (message, &index) in messages.iter().zip(&self.expected) {
            assert_eq!(message["full_id"], id(index).to_string());
            assert_eq!(message["id"], message["full_id"]);
            assert_eq!(message["content"].as_str().unwrap().len(), BODY_BYTES);
            let owner_read = self.population.owner_read(index);
            let display_read = match self.population {
                Population::Unique => owner_read,
                Population::InvisibleAndTwins => true,
            };
            assert_eq!(message["read"], display_read);
            assert_eq!(message["properties"]["read"], owner_read);
            let stored = self
                .fixture
                .stored(id(index))
                .await
                .expect("owner persisted");
            assert_eq!(stored.content, message["content"].as_str().unwrap());
            assert_eq!(stored.properties.as_ref().unwrap()["read"], owner_read);
        }
    }
}

struct Sample {
    dispatch: Duration,
    setup: Duration,
    check_cleanup: Duration,
}

fn sample(rt: &tokio::runtime::Runtime, population: Population, limit: u32) -> Sample {
    let start = Instant::now();
    let mut prepared = rt.block_on(PreparedThread::new(population, limit));
    let args = std::mem::take(&mut prepared.args);
    let setup = start.elapsed();

    let start = Instant::now();
    let result = rt.block_on(
        prepared
            .fixture
            .registry
            .dispatch("comm.thread", black_box(args)),
    );
    let dispatch = start.elapsed();

    let start = Instant::now();
    let result = result.expect("successful real thread dispatch");
    rt.block_on(prepared.check(&result));
    drop(result);
    // One dispatch per fresh fixture: drain/join audit and destroy its DB
    // outside the timed interval, without accumulating audit rows across samples.
    rt.block_on(prepared.fixture.close());
    Sample {
        dispatch,
        setup,
        check_cleanup: start.elapsed(),
    }
}

pub fn benchmark(c: &mut Criterion) {
    let rt = super::executor();
    let mut group = c.benchmark_group("comm_thread_rendering");
    group.sample_size(10);
    group.sampling_mode(SamplingMode::Flat);
    group.warm_up_time(Duration::from_millis(100));
    group.measurement_time(Duration::from_secs(1));
    group.throughput(Throughput::Elements(1));
    for population in Population::ALL {
        for limit in [1, 20] {
            let mut reported = false;
            group.bench_function(
                BenchmarkId::new(population.name(), format!("limit_{limit}")),
                |b| {
                    b.iter_custom(|iterations| {
                        (0..iterations)
                            .map(|_| {
                                let measured = sample(&rt, population, limit);
                                if !reported {
                                    println!("{}", json!({
                                        "scenario":population.name(), "limit":limit,
                                        "physical_rows":PHYSICAL_ROWS, "body_bytes":BODY_BYTES,
                                        "first_sample_setup_ns":measured.setup.as_nanos(),
                                        "first_sample_dispatch_ns":measured.dispatch.as_nanos(),
                                        "first_sample_check_cleanup_ns":measured.check_cleanup.as_nanos(),
                                        "timed":"one complete registry comm.thread dispatch",
                                        "fresh_fixture_per_dispatch":true,
                                    }));
                                    reported = true;
                                }
                                measured.dispatch
                            })
                            .sum::<Duration>()
                    });
                },
            );
        }
    }
    group.finish();
}
