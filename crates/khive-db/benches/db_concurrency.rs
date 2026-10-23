//! Concurrency benchmarks for the SQLite store: how reads scale across threads, and how a single
//! write behaves while reads run beside it. The single-threaded hot-path benches cannot see
//! contention that lives below the connection, such as SQLite's allocator statistics mutex or a
//! page cache shared by every connection in the process.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use rand::rngs::StdRng;
use rand::{RngExt, SeedableRng};
use tempfile::TempDir;
use uuid::Uuid;

use khive_db::backend::StorageBackend;
use khive_storage::types::{TextDocument, TextFilter, TextQueryMode, TextSearchRequest};
use khive_types::SubstrateKind;

const CORPUS_SIZE: usize = 10_000;
const NAMESPACE: &str = "bench_ns";
const SEARCHES_PER_TASK: usize = 8;
const WORDS: &[&str] = &[
    "knowledge",
    "graph",
    "memory",
    "recall",
    "search",
    "entity",
    "concept",
    "vector",
    "embedding",
    "semantic",
    "neural",
    "retrieval",
    "document",
    "index",
    "query",
    "latent",
    "attention",
    "transformer",
    "inference",
    "runtime",
    "agent",
    "context",
    "token",
    "score",
    "rank",
    "precision",
    "batch",
    "stream",
    "cursor",
];

fn gen_doc(rng: &mut StdRng, i: usize) -> TextDocument {
    let n_words = rng.random_range(20..35usize);
    let mut words: Vec<String> = (0..n_words)
        .map(|_| WORDS[rng.random_range(0..WORDS.len())].to_string())
        .collect();
    words.push(format!("doc_{i}"));
    TextDocument {
        subject_id: Uuid::new_v4(),
        kind: SubstrateKind::Note,
        record_kind: None,
        namespace: NAMESPACE.to_string(),
        title: Some(format!("Document title {i}")),
        body: words.join(" "),
        tags: vec![],
        metadata: None,
        updated_at: Utc::now(),
    }
}

struct Fixture {
    store: Arc<dyn khive_storage::TextSearch>,
    rt: tokio::runtime::Runtime,
    _dir: TempDir,
}

fn build_fixture() -> Fixture {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("bench_concurrency.db");
    let backend = StorageBackend::sqlite_with_volume_lock_dir(
        &path,
        path.parent().unwrap().join("volume-locks"),
    )
    .expect("backend");
    let store = backend.text("bench_notes").expect("text store");
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(8)
        .enable_all()
        .build()
        .expect("rt");
    rt.block_on(async {
        let mut rng = StdRng::seed_from_u64(42);
        let docs: Vec<TextDocument> = (0..CORPUS_SIZE).map(|i| gen_doc(&mut rng, i)).collect();
        store.upsert_documents(docs).await.expect("upsert corpus");
    });
    Fixture {
        store,
        rt,
        _dir: dir,
    }
}

fn search_request(term: &str) -> TextSearchRequest {
    TextSearchRequest {
        query: term.to_string(),
        mode: TextQueryMode::AnyTerm,
        filter: Some(TextFilter {
            namespaces: vec![NAMESPACE.to_string()],
            ..Default::default()
        }),
        top_k: 20,
        snippet_chars: 0,
    }
}

/// One iteration = `tasks` concurrent tasks, each running `SEARCHES_PER_TASK` ranked searches.
/// Without a process-wide lock below the connection the time should stay near the single-task
/// time up to the reader cap; such a lock makes it grow with the task count.
fn bench_concurrent_reads(c: &mut Criterion) {
    let fx = build_fixture();
    let mut group = c.benchmark_group("concurrent_fts_reads");
    group.sample_size(20);
    for tasks in [1usize, 4, 8] {
        group.bench_with_input(BenchmarkId::from_parameter(tasks), &tasks, |b, &tasks| {
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for _ in 0..iters {
                    let start = Instant::now();
                    fx.rt.block_on(async {
                        let mut handles = Vec::with_capacity(tasks);
                        for t in 0..tasks {
                            let store = Arc::clone(&fx.store);
                            handles.push(tokio::spawn(async move {
                                for s in 0..SEARCHES_PER_TASK {
                                    let term = WORDS[(t * SEARCHES_PER_TASK + s) % WORDS.len()];
                                    let hits =
                                        store.search(search_request(term)).await.expect("search");
                                    black_box(hits);
                                }
                            }));
                        }
                        for h in handles {
                            h.await.expect("join");
                        }
                    });
                    total += start.elapsed();
                }
                total
            });
        });
    }
    group.finish();
}

/// One single-document upsert, timed while `readers` tasks run ranked searches in a loop.
fn bench_write_under_read_load(c: &mut Criterion) {
    let fx = build_fixture();
    let mut group = c.benchmark_group("write_under_read_load");
    group.sample_size(30);
    for readers in [0usize, 4] {
        group.bench_with_input(
            BenchmarkId::from_parameter(readers),
            &readers,
            |b, &readers| {
                b.iter_custom(|iters| {
                    let stop = Arc::new(AtomicBool::new(false));
                    let loaders: Vec<_> = (0..readers)
                        .map(|t| {
                            let store = Arc::clone(&fx.store);
                            let stop = Arc::clone(&stop);
                            fx.rt.spawn(async move {
                                let mut s = 0usize;
                                while !stop.load(Ordering::Relaxed) {
                                    let term = WORDS[(t + s) % WORDS.len()];
                                    let hits =
                                        store.search(search_request(term)).await.expect("search");
                                    black_box(hits);
                                    s += 1;
                                }
                            })
                        })
                        .collect();
                    let mut rng = StdRng::seed_from_u64(7);
                    let mut total = Duration::ZERO;
                    for i in 0..iters {
                        let doc = gen_doc(&mut rng, CORPUS_SIZE + i as usize);
                        let start = Instant::now();
                        fx.rt
                            .block_on(fx.store.upsert_documents(vec![doc]))
                            .expect("upsert");
                        total += start.elapsed();
                    }
                    stop.store(true, Ordering::Relaxed);
                    fx.rt.block_on(async {
                        for l in loaders {
                            l.await.expect("join loader");
                        }
                    });
                    total
                });
            },
        );
    }
    group.finish();
}

criterion_group!(
    concurrency_benches,
    bench_concurrent_reads,
    bench_write_under_read_load
);
criterion_main!(concurrency_benches);
