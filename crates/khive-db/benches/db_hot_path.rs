use std::sync::Arc;

use criterion::{black_box, criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use tempfile::TempDir;

use khive_db::backend::StorageBackend;
use khive_db::extension::ensure_extensions_loaded;
use khive_storage::types::{
    TextDocument, TextFilter, TextGatherMode, TextQueryMode, TextSearchOptions, TextSearchRequest,
    TextTermStatsRequest, VectorRecord, VectorSearchRequest,
};
use khive_types::SubstrateKind;

use chrono::Utc;
use uuid::Uuid;

// ── corpus constants ──────────────────────────────────────────────────────────

const CORPUS_SIZE: usize = 10_000;
const VECTOR_DIMS: usize = 384;
const NAMESPACE: &str = "bench_ns";
const MODEL_KEY: &str = "all_minilm_l6_v2";

// ── deterministic data generators ────────────────────────────────────────────

fn rng() -> StdRng {
    StdRng::seed_from_u64(42)
}

fn gen_body(rng: &mut StdRng, i: usize) -> String {
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
    let n_words = rng.gen_range(20..35usize);
    let mut words: Vec<&str> = (0..n_words)
        .map(|_| WORDS[rng.gen_range(0..WORDS.len())])
        .collect();
    words.push(Box::leak(format!("doc_{i}").into_boxed_str()));
    words.join(" ")
}

fn gen_title(i: usize) -> String {
    format!("Document title {}", i)
}

fn gen_vector(rng: &mut StdRng, dims: usize) -> Vec<f32> {
    let raw: Vec<f32> = (0..dims).map(|_| rng.gen_range(-1.0f32..1.0)).collect();
    let norm: f32 = raw.iter().map(|v| v * v).sum::<f32>().sqrt().max(1e-9);
    raw.into_iter().map(|v| v / norm).collect()
}

fn build_text_corpus() -> Vec<TextDocument> {
    let mut rng = rng();
    (0..CORPUS_SIZE)
        .map(|i| TextDocument {
            subject_id: Uuid::new_v4(),
            kind: SubstrateKind::Note,
            namespace: NAMESPACE.to_string(),
            title: Some(gen_title(i)),
            body: gen_body(&mut rng, i),
            tags: vec![],
            metadata: None,
            updated_at: Utc::now(),
        })
        .collect()
}

fn build_vector_corpus() -> Vec<VectorRecord> {
    let mut rng = rng();
    (0..CORPUS_SIZE)
        .map(|_| VectorRecord {
            subject_id: Uuid::new_v4(),
            kind: SubstrateKind::Note,
            namespace: NAMESPACE.to_string(),
            field: "content".to_string(),
            embedding_model: Some(MODEL_KEY.to_string()),
            vectors: vec![gen_vector(&mut rng, VECTOR_DIMS)],
            updated_at: Utc::now(),
        })
        .collect()
}

// ── shared fixture state ──────────────────────────────────────────────────────

struct FtsFixture {
    store: Arc<dyn khive_storage::TextSearch>,
    _dir: TempDir,
}

fn build_fts_fixture() -> FtsFixture {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("bench_fts.db");
    let backend = StorageBackend::sqlite(&path).expect("backend");
    let store = backend.text("bench_notes").expect("text store");

    let rt = tokio::runtime::Runtime::new().expect("rt");
    rt.block_on(async {
        let docs = build_text_corpus();
        store.upsert_documents(docs).await.expect("upsert corpus");
    });

    FtsFixture { store, _dir: dir }
}

struct VecFixture {
    store: Arc<dyn khive_storage::VectorStore>,
    query_vec: Vec<f32>,
    _dir: TempDir,
}

fn build_vec_fixture() -> VecFixture {
    ensure_extensions_loaded();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("bench_vec.db");
    let backend = StorageBackend::sqlite(&path).expect("backend");
    let store = backend
        .vectors_for_namespace(MODEL_KEY, "all-minilm-l6-v2", VECTOR_DIMS, NAMESPACE)
        .expect("vec store");

    let rt = tokio::runtime::Runtime::new().expect("rt");
    rt.block_on(async {
        let records = build_vector_corpus();
        store
            .insert_batch(records)
            .await
            .expect("insert_batch corpus");
    });

    let mut rng = rng();
    let query_vec = gen_vector(&mut rng, VECTOR_DIMS);

    VecFixture {
        store,
        query_vec,
        _dir: dir,
    }
}

// ── FTS5 benchmarks ───────────────────────────────────────────────────────────

fn bench_fts_search(c: &mut Criterion) {
    let fixture = build_fts_fixture();
    let store = Arc::clone(&fixture.store);
    let rt = tokio::runtime::Runtime::new().expect("rt");

    let mut group = c.benchmark_group("fts5_search");
    group.sample_size(50);

    group.bench_function("anyterm_1term", |b| {
        let store = Arc::clone(&store);
        b.to_async(&rt).iter(|| {
            let store = Arc::clone(&store);
            async move {
                let hits = store
                    .search(TextSearchRequest {
                        query: "knowledge".to_string(),
                        mode: TextQueryMode::AnyTerm,
                        filter: Some(TextFilter {
                            namespaces: vec![NAMESPACE.to_string()],
                            ..Default::default()
                        }),
                        top_k: 20,
                        snippet_chars: 0,
                    })
                    .await
                    .expect("search");
                black_box(hits)
            }
        });
    });

    group.bench_function("anyterm_3terms", |b| {
        let store = Arc::clone(&store);
        b.to_async(&rt).iter(|| {
            let store = Arc::clone(&store);
            async move {
                let hits = store
                    .search(TextSearchRequest {
                        query: "knowledge graph memory".to_string(),
                        mode: TextQueryMode::AnyTerm,
                        filter: Some(TextFilter {
                            namespaces: vec![NAMESPACE.to_string()],
                            ..Default::default()
                        }),
                        top_k: 20,
                        snippet_chars: 0,
                    })
                    .await
                    .expect("search");
                black_box(hits)
            }
        });
    });

    group.bench_function("anyterm_5terms", |b| {
        let store = Arc::clone(&store);
        b.to_async(&rt).iter(|| {
            let store = Arc::clone(&store);
            async move {
                let hits = store
                    .search(TextSearchRequest {
                        query: "knowledge graph memory recall search".to_string(),
                        mode: TextQueryMode::AnyTerm,
                        filter: Some(TextFilter {
                            namespaces: vec![NAMESPACE.to_string()],
                            ..Default::default()
                        }),
                        top_k: 20,
                        snippet_chars: 0,
                    })
                    .await
                    .expect("search");
                black_box(hits)
            }
        });
    });

    group.bench_function("plain_no_snippet", |b| {
        let store = Arc::clone(&store);
        b.to_async(&rt).iter(|| {
            let store = Arc::clone(&store);
            async move {
                let hits = store
                    .search(TextSearchRequest {
                        query: "semantic neural retrieval".to_string(),
                        mode: TextQueryMode::Plain,
                        filter: Some(TextFilter {
                            namespaces: vec![NAMESPACE.to_string()],
                            ..Default::default()
                        }),
                        top_k: 20,
                        snippet_chars: 0,
                    })
                    .await
                    .expect("search");
                black_box(hits)
            }
        });
    });

    group.bench_function("plain_with_snippet", |b| {
        let store = Arc::clone(&store);
        b.to_async(&rt).iter(|| {
            let store = Arc::clone(&store);
            async move {
                let hits = store
                    .search(TextSearchRequest {
                        query: "semantic neural retrieval".to_string(),
                        mode: TextQueryMode::Plain,
                        filter: Some(TextFilter {
                            namespaces: vec![NAMESPACE.to_string()],
                            ..Default::default()
                        }),
                        top_k: 20,
                        snippet_chars: 64,
                    })
                    .await
                    .expect("search");
                black_box(hits)
            }
        });
    });

    group.finish();
}

fn bench_fts_search_unranked(c: &mut Criterion) {
    let fixture = build_fts_fixture();
    let store = Arc::clone(&fixture.store);
    let rt = tokio::runtime::Runtime::new().expect("rt");

    let mut group = c.benchmark_group("fts5_search_unranked");
    group.sample_size(50);

    group.bench_function("anyterm_top20", |b| {
        let store = Arc::clone(&store);
        b.to_async(&rt).iter(|| {
            let store = Arc::clone(&store);
            async move {
                let hits = store
                    .search_with_options(
                        TextSearchRequest {
                            query: "knowledge graph memory".to_string(),
                            mode: TextQueryMode::AnyTerm,
                            filter: Some(TextFilter {
                                namespaces: vec![NAMESPACE.to_string()],
                                ..Default::default()
                            }),
                            top_k: 20,
                            snippet_chars: 0,
                        },
                        TextSearchOptions {
                            gather_mode: TextGatherMode::Unranked,
                            gather_limit: None,
                        },
                    )
                    .await
                    .expect("search_unranked");
                black_box(hits)
            }
        });
    });

    group.finish();
}

fn bench_fts_search_rank_within_cap(c: &mut Criterion) {
    let fixture = build_fts_fixture();
    let store = Arc::clone(&fixture.store);
    let rt = tokio::runtime::Runtime::new().expect("rt");

    let mut group = c.benchmark_group("fts5_rank_within_cap");
    group.sample_size(50);

    for &cap in &[50u32, 200, 500] {
        group.bench_with_input(BenchmarkId::new("cap", cap), &cap, |b, &cap| {
            let store = Arc::clone(&store);
            b.to_async(&rt).iter(|| {
                let store = Arc::clone(&store);
                async move {
                    let hits = store
                        .search_with_options(
                            TextSearchRequest {
                                query: "knowledge graph memory recall".to_string(),
                                mode: TextQueryMode::AnyTerm,
                                filter: Some(TextFilter {
                                    namespaces: vec![NAMESPACE.to_string()],
                                    ..Default::default()
                                }),
                                top_k: 20,
                                snippet_chars: 0,
                            },
                            TextSearchOptions {
                                gather_mode: TextGatherMode::RankWithinCap,
                                gather_limit: Some(cap),
                            },
                        )
                        .await
                        .expect("rank_within_cap");
                    black_box(hits)
                }
            });
        });
    }

    group.finish();
}

fn bench_fts_term_stats(c: &mut Criterion) {
    let fixture = build_fts_fixture();
    let store = Arc::clone(&fixture.store);
    let rt = tokio::runtime::Runtime::new().expect("rt");

    let mut group = c.benchmark_group("fts5_term_stats");
    group.sample_size(50);

    group.bench_function("single_term", |b| {
        let store = Arc::clone(&store);
        b.to_async(&rt).iter(|| {
            let store = Arc::clone(&store);
            async move {
                let stats = store
                    .term_stats(TextTermStatsRequest {
                        terms: vec!["knowledge".to_string()],
                        filter: Some(TextFilter {
                            namespaces: vec![NAMESPACE.to_string()],
                            ..Default::default()
                        }),
                    })
                    .await
                    .expect("term_stats");
                black_box(stats)
            }
        });
    });

    group.bench_function("five_terms", |b| {
        let store = Arc::clone(&store);
        b.to_async(&rt).iter(|| {
            let store = Arc::clone(&store);
            async move {
                let stats = store
                    .term_stats(TextTermStatsRequest {
                        terms: vec![
                            "knowledge".to_string(),
                            "graph".to_string(),
                            "memory".to_string(),
                            "recall".to_string(),
                            "search".to_string(),
                        ],
                        filter: Some(TextFilter {
                            namespaces: vec![NAMESPACE.to_string()],
                            ..Default::default()
                        }),
                    })
                    .await
                    .expect("term_stats");
                black_box(stats)
            }
        });
    });

    group.finish();
}

fn bench_fts_upsert_batch(c: &mut Criterion) {
    let mut group = c.benchmark_group("fts5_upsert_batch");
    group.sample_size(50);

    for &batch_sz in &[100usize, 500, 1000] {
        group.bench_with_input(
            BenchmarkId::new("docs", batch_sz),
            &batch_sz,
            |b, &batch_sz| {
                b.iter_batched(
                    || {
                        let mut rng = rng();
                        let docs: Vec<TextDocument> = (0..batch_sz)
                            .map(|i| TextDocument {
                                subject_id: Uuid::new_v4(),
                                kind: SubstrateKind::Note,
                                namespace: NAMESPACE.to_string(),
                                title: Some(gen_title(i)),
                                body: gen_body(&mut rng, i),
                                tags: vec![],
                                metadata: None,
                                updated_at: Utc::now(),
                            })
                            .collect();
                        let dir = tempfile::tempdir().expect("tempdir");
                        let path = dir.path().join("bench_upsert.db");
                        let backend = StorageBackend::sqlite(&path).expect("backend");
                        let store = backend.text("upsert_bench").expect("text store");
                        (store, docs, dir)
                    },
                    |(store, docs, _dir)| {
                        let rt = tokio::runtime::Runtime::new().expect("rt");
                        rt.block_on(async {
                            let summary = store
                                .upsert_documents(black_box(docs))
                                .await
                                .expect("upsert");
                            black_box(summary)
                        })
                    },
                    BatchSize::SmallInput,
                );
            },
        );
    }

    group.finish();
}

// ── Vector store benchmarks ───────────────────────────────────────────────────

fn bench_vec_search(c: &mut Criterion) {
    ensure_extensions_loaded();
    let fixture = build_vec_fixture();
    let store = Arc::clone(&fixture.store);
    let query_vec = fixture.query_vec.clone();
    let rt = tokio::runtime::Runtime::new().expect("rt");

    let mut group = c.benchmark_group("sqlite_vec_search");
    group.sample_size(50);

    for &top_k in &[10u32, 50, 100] {
        group.bench_with_input(BenchmarkId::new("top_k", top_k), &top_k, |b, &top_k| {
            let store = Arc::clone(&store);
            let qv = query_vec.clone();
            b.to_async(&rt).iter(|| {
                let store = Arc::clone(&store);
                let qv = qv.clone();
                async move {
                    let hits = store
                        .search(VectorSearchRequest {
                            query_vectors: vec![qv],
                            top_k,
                            namespace: Some(NAMESPACE.to_string()),
                            kind: Some(SubstrateKind::Note),
                            embedding_model: None,
                            filter: None,
                            backend_hints: None,
                        })
                        .await
                        .expect("vec search");
                    black_box(hits)
                }
            });
        });
    }

    group.finish();
}

fn bench_vec_insert_batch(c: &mut Criterion) {
    ensure_extensions_loaded();

    let mut group = c.benchmark_group("sqlite_vec_insert_batch");
    group.sample_size(50);

    for &batch_sz in &[100usize, 500, 1000] {
        group.bench_with_input(
            BenchmarkId::new("records", batch_sz),
            &batch_sz,
            |b, &batch_sz| {
                b.iter_batched(
                    || {
                        let mut rng = rng();
                        let records: Vec<VectorRecord> = (0..batch_sz)
                            .map(|_| VectorRecord {
                                subject_id: Uuid::new_v4(),
                                kind: SubstrateKind::Note,
                                namespace: NAMESPACE.to_string(),
                                field: "content".to_string(),
                                embedding_model: Some(MODEL_KEY.to_string()),
                                vectors: vec![gen_vector(&mut rng, VECTOR_DIMS)],
                                updated_at: Utc::now(),
                            })
                            .collect();
                        let dir = tempfile::tempdir().expect("tempdir");
                        let path = dir.path().join("bench_vec_insert.db");
                        let backend = StorageBackend::sqlite(&path).expect("backend");
                        let store = backend
                            .vectors_for_namespace(
                                MODEL_KEY,
                                "all-minilm-l6-v2",
                                VECTOR_DIMS,
                                NAMESPACE,
                            )
                            .expect("vec store");
                        (store, records, dir)
                    },
                    |(store, records, _dir)| {
                        let rt = tokio::runtime::Runtime::new().expect("rt");
                        rt.block_on(async {
                            let summary = store
                                .insert_batch(black_box(records))
                                .await
                                .expect("insert_batch");
                            black_box(summary)
                        })
                    },
                    BatchSize::SmallInput,
                );
            },
        );
    }

    group.finish();
}

// ── StorageBackend creation ───────────────────────────────────────────────────

fn bench_backend_creation(c: &mut Criterion) {
    let mut group = c.benchmark_group("storage_backend_creation");
    group.sample_size(200);

    group.bench_function("memory", |b| {
        b.iter(|| {
            let backend = StorageBackend::memory().expect("memory backend");
            black_box(backend)
        });
    });

    group.bench_function("file", |b| {
        b.iter_batched(
            || tempfile::tempdir().expect("tempdir"),
            |dir| {
                let path = dir.path().join("bench_creation.db");
                let backend = StorageBackend::sqlite(&path).expect("sqlite backend");
                black_box((backend, dir))
            },
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

// ── criterion entry points ────────────────────────────────────────────────────

criterion_group!(
    fts_benches,
    bench_fts_search,
    bench_fts_search_unranked,
    bench_fts_search_rank_within_cap,
    bench_fts_term_stats,
    bench_fts_upsert_batch,
);

criterion_group!(vec_benches, bench_vec_search, bench_vec_insert_batch,);

criterion_group!(backend_benches, bench_backend_creation,);

criterion_main!(fts_benches, vec_benches, backend_benches);
