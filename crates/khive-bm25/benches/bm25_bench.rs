use criterion::measurement::WallTime;
use criterion::{criterion_group, criterion_main, BenchmarkGroup, BenchmarkId, Criterion};
use khive_bm25::{Bm25Config, Bm25Index, SearchContext};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use std::hint::black_box;

// Deterministic vocabulary: 500 content words drawn from a fixed seeded RNG.
// SimpleTokenizer filters stop words and lowercases, so we use clear content words.
const VOCAB: &[&str] = &[
    "alpha",
    "beta",
    "gamma",
    "delta",
    "epsilon",
    "zeta",
    "eta",
    "theta",
    "iota",
    "kappa",
    "lambda",
    "mu",
    "nu",
    "xi",
    "omicron",
    "pi",
    "rho",
    "sigma",
    "tau",
    "upsilon",
    "phi",
    "chi",
    "psi",
    "omega",
    "stone",
    "river",
    "forest",
    "mountain",
    "valley",
    "ocean",
    "desert",
    "plain",
    "cloud",
    "thunder",
    "lightning",
    "frost",
    "ember",
    "crystal",
    "shadow",
    "light",
    "dark",
    "swift",
    "slow",
    "bright",
    "dim",
    "sharp",
    "blunt",
    "rough",
    "smooth",
    "ancient",
    "new",
    "strong",
    "fragile",
    "heavy",
    "light",
    "deep",
    "shallow",
    "wide",
    "narrow",
    "long",
    "short",
    "copper",
    "silver",
    "gold",
    "iron",
    "steel",
    "bronze",
    "titanium",
    "carbon",
    "silicon",
    "hydrogen",
    "oxygen",
    "nitrogen",
    "plasma",
    "neutron",
    "proton",
    "quantum",
    "photon",
    "vector",
    "matrix",
    "tensor",
    "scalar",
    "gradient",
    "kernel",
    "cluster",
    "sparse",
    "dense",
    "stream",
    "batch",
    "token",
    "index",
    "query",
    "result",
    "score",
    "rank",
    "recall",
    "precision",
    "retrieval",
    "search",
    "match",
    "filter",
    "boost",
    "decay",
    "weight",
    "frequency",
    "inverse",
    "document",
    "corpus",
    "lexical",
    "semantic",
    "keyword",
    "phrase",
    "term",
    "field",
    "segment",
    "shard",
    "replica",
    "partition",
    "merge",
    "compact",
    "flush",
    "commit",
    "rollback",
    "restore",
    "encode",
    "decode",
    "compress",
    "expand",
    "serialize",
    "parse",
    "tokenize",
    "normalize",
    "stem",
    "lemma",
    "ngram",
    "bigram",
    "trigram",
    "prefix",
    "suffix",
    "infix",
    "pattern",
    "regex",
    "fuzzy",
    "exact",
    "range",
    "numeric",
    "string",
    "boolean",
    "float",
    "integer",
    "aggregate",
    "bucket",
    "histogram",
    "percentile",
    "median",
    "average",
    "variance",
    "deviation",
    "pipeline",
    "stage",
    "executor",
    "scheduler",
    "worker",
    "thread",
    "process",
    "channel",
    "buffer",
    "queue",
    "stack",
    "heap",
    "pool",
    "cache",
    "evict",
    "expire",
    "ttl",
    "lease",
    "sparse",
    "inverted",
    "posting",
    "positional",
    "proximity",
    "adjacency",
    "overlap",
    "disjoint",
    "union",
    "intersection",
    "complement",
    "subset",
    "superset",
    "member",
    "element",
    "node",
    "edge",
    "vertex",
    "path",
    "cycle",
    "tree",
    "graph",
    "lattice",
    "topology",
    "distance",
    "cosine",
    "euclidean",
    "manhattan",
    "hamming",
    "jaccard",
    "overlap",
    "dice",
    "tversky",
    "recall",
    "precision",
    "fmeasure",
    "accuracy",
    "auc",
    "ndcg",
    "mrr",
    "map",
    "relevant",
    "corpus",
    "training",
    "validation",
    "testing",
    "benchmark",
    "baseline",
    "candidate",
    "retrieve",
    "rerank",
    "fuse",
    "combine",
    "ensemble",
    "hybrid",
    "dense",
    "sparse",
    "late",
    "early",
    "cross",
    "encode",
    "biencoder",
    "crossencoder",
    "reranker",
    "retriever",
    "reader",
    "extractor",
    "generator",
    "summarizer",
    "classifier",
    "regressor",
    "ranker",
    "scorer",
    "embedding",
    "projection",
    "attention",
    "transformer",
    "encoder",
    "decoder",
    "layer",
    "activation",
    "dropout",
    "normalization",
    "residual",
    "skip",
    "connection",
    "linear",
    "convolutional",
    "pooling",
    "flatten",
    "softmax",
    "sigmoid",
    "relu",
    "gelu",
    "tanh",
    "loss",
    "gradient",
    "optimizer",
    "momentum",
    "adaptive",
    "learning",
    "rate",
    "epoch",
    "batch",
    "shuffle",
    "augment",
    "regularize",
    "prune",
    "quantize",
    "distill",
    "finetune",
    "pretrain",
    "checkpoint",
    "artifact",
    "experiment",
    "config",
    "hyperparameter",
    "tuning",
    "evaluation",
    "metric",
    "threshold",
    "cutoff",
    "topk",
    "window",
    "stride",
    "padding",
];

fn gen_doc(rng: &mut StdRng, num_words: usize) -> String {
    (0..num_words)
        .map(|_| VOCAB[rng.gen_range(0..VOCAB.len())])
        .collect::<Vec<_>>()
        .join(" ")
}

fn build_index(n_docs: usize, words_per_doc: usize) -> Bm25Index {
    let mut rng = StdRng::seed_from_u64(42);
    let mut index = Bm25Index::new(Bm25Config::default());
    for i in 0..n_docs {
        let doc = gen_doc(&mut rng, words_per_doc);
        index.index_document(format!("doc{i}"), &doc).unwrap();
    }
    index
}

// ---------------------------------------------------------------------------
// Group 1: index_document throughput at varying corpus sizes
// ---------------------------------------------------------------------------

fn bench_index_throughput(c: &mut Criterion) {
    let mut group: BenchmarkGroup<WallTime> = c.benchmark_group("index_document");
    group.sample_size(50);

    for &n in &[100usize, 1_000, 5_000] {
        group.bench_with_input(BenchmarkId::new("docs", n), &n, |b, &n| {
            b.iter(|| {
                let mut rng = StdRng::seed_from_u64(42);
                let mut index = Bm25Index::new(Bm25Config::default());
                for i in 0..n {
                    let doc = gen_doc(&mut rng, 200);
                    index
                        .index_document(format!("doc{i}"), black_box(&doc))
                        .unwrap();
                }
                black_box(index)
            });
        });
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// Group 2: single-document insert into a pre-built 1K corpus
// ---------------------------------------------------------------------------

fn bench_single_insert(c: &mut Criterion) {
    let mut group: BenchmarkGroup<WallTime> = c.benchmark_group("index_document_single");
    group.sample_size(200);

    let base = build_index(1_000, 200);

    for &words in &[50usize, 200, 500] {
        group.bench_with_input(BenchmarkId::new("words", words), &words, |b, &words| {
            let mut rng = StdRng::seed_from_u64(99);
            let doc = gen_doc(&mut rng, words);
            b.iter(|| {
                let mut idx = base.clone();
                idx.index_document("bench-doc", black_box(&doc)).unwrap();
                black_box(idx)
            });
        });
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// Group 3: search latency (1-term through 5-term queries) on 1K corpus
// ---------------------------------------------------------------------------

fn bench_search_1k(c: &mut Criterion) {
    let index = build_index(1_000, 200);
    let mut group: BenchmarkGroup<WallTime> = c.benchmark_group("search_1k");
    group.sample_size(200);

    let queries: &[(&str, &str)] = &[
        ("1-term", "retrieval"),
        ("2-term", "retrieval search"),
        ("3-term", "retrieval search index"),
        ("4-term", "retrieval search index query"),
        ("5-term", "retrieval search index query score"),
    ];

    for &(name, query) in queries {
        group.bench_function(name, |b| {
            b.iter(|| black_box(index.search(black_box(query), 10)));
        });
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// Group 4: search latency at different corpus sizes
// ---------------------------------------------------------------------------

fn bench_search_scale(c: &mut Criterion) {
    let mut group: BenchmarkGroup<WallTime> = c.benchmark_group("search_corpus_scale");
    group.sample_size(100);

    for &n in &[100usize, 500, 1_000] {
        let index = build_index(n, 200);
        group.bench_with_input(BenchmarkId::new("docs", n), &n, |b, _| {
            b.iter(|| black_box(index.search(black_box("retrieval search index"), 10)));
        });
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// Group 5: search_with_context (reused SearchContext vs fresh allocation)
// ---------------------------------------------------------------------------

fn bench_search_context_reuse(c: &mut Criterion) {
    let index = build_index(1_000, 200);
    let mut group: BenchmarkGroup<WallTime> = c.benchmark_group("search_context");
    group.sample_size(200);

    group.bench_function("fresh_ctx", |b| {
        b.iter(|| black_box(index.search(black_box("retrieval search"), 10)));
    });

    group.bench_function("reused_ctx", |b| {
        let mut ctx = SearchContext::with_capacity(20);
        b.iter(|| {
            black_box(index.search_with_context(black_box("retrieval search"), 10, &mut ctx))
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Group 6: top-k variation (k=1, 10, 50)
// ---------------------------------------------------------------------------

fn bench_topk(c: &mut Criterion) {
    let index = build_index(1_000, 200);
    let mut group: BenchmarkGroup<WallTime> = c.benchmark_group("search_topk");
    group.sample_size(200);

    for &k in &[1usize, 10, 50] {
        group.bench_with_input(BenchmarkId::new("k", k), &k, |b, &k| {
            b.iter(|| black_box(index.search(black_box("retrieval search index"), k)));
        });
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// Group 7: memory_usage estimation on growing corpus
// ---------------------------------------------------------------------------

fn bench_memory_usage(c: &mut Criterion) {
    let mut group: BenchmarkGroup<WallTime> = c.benchmark_group("memory_usage");
    group.sample_size(50);

    for &n in &[100usize, 500, 1_000] {
        let index = build_index(n, 200);
        group.bench_with_input(BenchmarkId::new("docs", n), &n, |b, _| {
            b.iter(|| black_box(index.memory_usage()));
        });
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// Group 8: remove_document on a 1K index
// ---------------------------------------------------------------------------

fn bench_remove_document(c: &mut Criterion) {
    let mut group: BenchmarkGroup<WallTime> = c.benchmark_group("remove_document");
    group.sample_size(50);

    group.bench_function("1k_corpus", |b| {
        b.iter(|| {
            let mut idx = build_index(1_000, 200);
            for i in 0..100 {
                black_box(idx.remove_document(black_box(&format!("doc{i}"))));
            }
            black_box(idx)
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_index_throughput,
    bench_single_insert,
    bench_search_1k,
    bench_search_scale,
    bench_search_context_reuse,
    bench_topk,
    bench_memory_usage,
    bench_remove_document,
);
criterion_main!(benches);
