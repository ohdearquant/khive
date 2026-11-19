use std::time::{Duration, Instant};

use criterion::{black_box, BenchmarkId, Criterion};

use crate::fixture::{self, complete, Fixture};

#[derive(Clone, Copy)]
enum Kind {
    Entity,
    Edge,
    Note,
    Event,
}

pub fn bench_writes(c: &mut Criterion) {
    let rt = fixture::runtime();
    for (name, kind) in [
        ("entity_upsert_batch", Kind::Entity),
        ("edge_upsert_batch", Kind::Edge),
        ("note_upsert_batch", Kind::Note),
        ("event_append_batch", Kind::Event),
    ] {
        let mut group = c.benchmark_group(name);
        group.sample_size(10);
        for count in [100usize, 500, 1000] {
            group.bench_with_input(BenchmarkId::new("records", count), &count, |b, &count| {
                b.iter_custom(|iterations| {
                    let mut elapsed = Duration::ZERO;
                    for _ in 0..iterations {
                        // Sequential setup, never Criterion's automatically sized fixture batches.
                        // Every timed call inserts fresh records; it cannot become an overwrite.
                        let fx = Fixture::new(&rt);
                        let (summary, duration) = match kind {
                            Kind::Entity => {
                                let records = fixture::entities(count);
                                let start = Instant::now();
                                let result = rt.block_on(fx.entities.upsert_entities(records));
                                (result, start.elapsed())
                            }
                            Kind::Edge => {
                                complete(
                                    rt.block_on(
                                        fx.entities.upsert_entities(fixture::entities(count + 1)),
                                    )
                                    .expect("edge endpoints"),
                                    count + 1,
                                );
                                let records = fixture::edges(count);
                                let start = Instant::now();
                                let result = rt.block_on(fx.graph.upsert_edges(records));
                                (result, start.elapsed())
                            }
                            Kind::Note => {
                                let records = fixture::notes(count);
                                let start = Instant::now();
                                let result = rt.block_on(fx.notes.upsert_notes(records));
                                (result, start.elapsed())
                            }
                            Kind::Event => {
                                let records = fixture::events(0, count);
                                let start = Instant::now();
                                let result = rt.block_on(fx.events.append_events(records));
                                (result, start.elapsed())
                            }
                        };
                        elapsed += duration;
                        complete(black_box(summary.expect("complete batch")), count);
                        fx.close(&rt);
                    }
                    elapsed
                });
            });
        }
        group.finish();
    }
}
