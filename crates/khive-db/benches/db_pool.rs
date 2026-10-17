//! Criterion reports wave wall time; the separately printed p99 is calculated from
//! individual reader() durations, never inferred from Criterion's average.

use std::sync::Barrier;
use std::time::{Duration, Instant};

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use khive_db::pool::{ConnectionPool, PoolConfig};
use tempfile::TempDir;

const READERS: usize = 4;
const ACQUIRES_PER_THREAD: usize = 256;
const HOLD: Duration = Duration::from_micros(100);

struct Fixture {
    // Field order closes all connections before deleting the database directory.
    pool: ConnectionPool,
    _dir: TempDir,
}

impl Fixture {
    fn new() -> Self {
        khive_db::extension::ensure_extensions_loaded();
        let dir = tempfile::tempdir().expect("pool benchmark directory");
        let pool = ConnectionPool::new(PoolConfig {
            path: Some(dir.path().join("pool.db")),
            volume_lock_dir: Some(dir.path().join("volume-locks")),
            max_readers: READERS,
            wal_mode: true,
            reader_max_age: Duration::from_secs(300),
            reader_max_ops: 5000,
            checkout_timeout: Duration::from_secs(5),
            ..Default::default()
        })
        .expect("four-reader WAL pool");
        // Simultaneous warm leases also detect an unintended single-connection fallback.
        let warm: Vec<_> = (0..READERS)
            .map(|_| pool.reader().expect("warm reader"))
            .collect();
        let mode: String = warm[0]
            .conn()
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .expect("journal mode");
        assert_eq!(mode, "wal", "benchmark requires dedicated pooled readers");
        drop(warm);
        Self { pool, _dir: dir }
    }
}

struct Window {
    wall: Duration,
    p99: Duration,
    samples: usize,
    rank: usize,
}

fn measure_window(threads: usize) -> Window {
    // One fresh fixture per window bounds disk/memory and resets recycling counters.
    let fx = Fixture::new();
    let start_gate = Barrier::new(threads + 1);
    let (wall, mut samples) = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                let pool = &fx.pool;
                let gate = &start_gate;
                scope.spawn(move || {
                    let mut samples = Vec::with_capacity(ACQUIRES_PER_THREAD);
                    gate.wait();
                    for _ in 0..ACQUIRES_PER_THREAD {
                        let start = Instant::now();
                        let reader = pool.reader().expect("every checkout must succeed");
                        let acquired = start.elapsed();
                        // The lease hold creates overlap; its duration and return are not acquire time.
                        std::thread::sleep(HOLD);
                        drop(reader);
                        samples.push(acquired);
                    }
                    samples
                })
            })
            .collect();
        // Pool construction and worker creation are outside the auxiliary wall-time measurement.
        let start = Instant::now();
        start_gate.wait();
        let samples: Vec<_> = handles
            .into_iter()
            .flat_map(|handle| handle.join().expect("checkout worker"))
            .collect();
        (start.elapsed(), samples)
    });
    assert_eq!(samples.len(), threads * ACQUIRES_PER_THREAD);
    samples.sort_unstable();
    // Nearest rank: ceil(0.99 * n), with one-based rank converted to zero-based indexing.
    let rank = (99 * samples.len()).div_ceil(100);
    Window {
        wall,
        p99: samples[rank - 1],
        samples: samples.len(),
        rank,
    }
}

fn bench_pool(c: &mut Criterion) {
    let mut group = c.benchmark_group("pooled_reader_checkout_window");
    group.sample_size(10);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(3));
    for threads in [1usize, 4, 8, 16] {
        let mut selected = false;
        group.bench_with_input(
            BenchmarkId::new("threads", threads),
            &threads,
            |b, &threads| {
                selected = true;
                b.iter_custom(|iterations| {
                    let mut elapsed = Duration::ZERO;
                    for _ in 0..iterations {
                        let window = measure_window(threads);
                        elapsed += window.wall;
                        black_box(window.p99);
                    }
                    elapsed
                });
            },
        );
        if selected {
            // A separate fixed-size window produces the requested empirical per-acquire p99.
            // Respect Criterion's scenario filter and --list: unselected scenarios do no work.
            let window = measure_window(threads);
            println!(
                "pooled_reader_checkout_p99/threads/{threads} samples={} nearest_rank={} p99_ns={}",
                window.samples,
                window.rank,
                window.p99.as_nanos()
            );
        }
    }
    group.finish();
}

criterion_group!(pool_benches, bench_pool);
criterion_main!(pool_benches);
