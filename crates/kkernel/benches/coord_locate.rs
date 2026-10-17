//! Actual coordinator service lookup across two isolated SQLite backends.

#[path = "routing_support/coordinator.rs"]
mod coordinator;
mod routing_support;

use coordinator::{LocateCase, Operation};
use criterion::{criterion_group, criterion_main, Criterion};
use routing_support::{group, scene};

fn coord_locate(c: &mut Criterion) {
    let mut group = group(c, "coord_locate");
    for count in [100, 1_000] {
        for (name, operation) in [
            ("cache_hit", Operation::Hit),
            ("cache_miss", Operation::Miss),
            ("unknown_uuid", Operation::Unknown),
        ] {
            scene(&mut group, &format!("{name}/{count}"), || {
                LocateCase::new(count, operation)
            });
        }
    }
    group.finish();
}

criterion_group!(benches, coord_locate);
criterion_main!(benches);
