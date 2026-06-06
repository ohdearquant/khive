# khive-bm25

BM25 (Okapi BM25) keyword index with deterministic scoring and Block-Max WAND
acceleration.

## Features

- **Deterministic scoring** via `khive-score` fixed-point `i64` representation
- **Block-Max WAND** for sub-linear query evaluation on large posting lists
- **SIMD batch scoring** — NEON (aarch64), AVX2/SSE2 (x86_64), scalar fallback
- **Memory budgets** — cap heap usage per index with configurable limits
- **Serde persistence** — serialize/deserialize with full invariant validation
- **Pluggable tokenizers** — swap `SimpleTokenizer` for any `Tokenizer` impl
- **Metrics** — optional `MetricsSink` for observability (latency, counts, sizes)

## Quick start

```rust
use khive_bm25::{Bm25Config, Bm25Index};

let mut index = Bm25Index::new(Bm25Config::default());

index.index_document("doc1", "the quick brown fox").unwrap();
index.index_document("doc2", "the lazy dog").unwrap();

let results = index.search("quick fox", 10);
for (doc_id, score) in &results {
    println!("{doc_id}: {}", score.to_f64());
}
```

## Configuration

| Parameter | Default | Description                                                         |
| --------- | ------- | ------------------------------------------------------------------- |
| `k1`      | 1.2     | Term frequency saturation. Higher = more weight on repeated terms.  |
| `b`       | 0.75    | Length normalization. 0 = no normalization, 1 = full normalization. |

```rust
let config = Bm25Config::new(2.0, 0.5);
let index = Bm25Index::new(config);
```

Both parameters must be finite and non-negative. `b` must be in `[0.0, 1.0]`.

## Architecture

```text
index_document() ──→ tokenize ──→ build posting lists ──→ update block-max metadata
search()         ──→ tokenize ──→ select path ──→ results
                                       │
                          ┌─────────────┼─────────────┐
                     postings < 16K  postings ≥ 16K    k = 0
                          │              │              │
                    brute-force      Block-Max        empty
                    SIMD batch        WAND
```

The search path is chosen automatically based on total posting count across
query terms. Below 16K postings, brute-force SIMD batching (4-wide on NEON,
4/8-wide on AVX2) beats WAND's per-cursor overhead. Above 16K, WAND's
block-skip pruning dominates.

## Docs

| Document                                | Contents                                                 |
| --------------------------------------- | -------------------------------------------------------- |
| [algorithm.md](docs/algorithm.md)       | BM25 formula, WAND algorithm, block-max strategy         |
| [simd.md](docs/simd.md)                 | SIMD dispatch, platform support, safety invariants       |
| [tokenization.md](docs/tokenization.md) | Tokenizer trait, stop words, pluggable analyzers         |
| [usage.md](docs/usage.md)               | ID bridging with HNSW, serde persistence, memory budgets |
| [benchmarks.md](docs/benchmarks.md)     | Benchmark suite, baselines, regression policy            |

## Crate layout

```text
src/
├── lib.rs              Public API re-exports
├── config.rs           Bm25Config with validation
├── error.rs            RetrievalError enum (BudgetExceeded, Configuration, IdSpaceExhausted)
├── metrics.rs          MetricsSink trait + RecordingSink
├── tokenizer.rs        SimpleTokenizer + Tokenizer trait
├── tests.rs            Inline tests (internal field access)
└── index/
    ├── mod.rs           Thin shim — module declarations and re-exports
    ├── core.rs          Bm25Index struct, constructors, public methods, serde
    ├── document_id.rs   DocumentId newtype with serde and wire-format tests
    ├── posting.rs       PostingList, BlockMaxBlock, BlockMaxState
    ├── scoring.rs       IdfCache, Bm25TermScorer, Bm25Stats, scoring helpers
    ├── indexing.rs      index_document, remove_document
    ├── memory.rs        Memory budget enforcement
    └── search/
        ├── mod.rs       SearchContext, main search dispatch, WAND loop
        ├── simd.rs      SIMD batch scoring (NEON/AVX2/SSE2/scalar)
        ├── cursor.rs    TermCursor for WAND iteration
        └── helpers.rs   WAND helper functions (pivot, align, heap)
tests/
├── unit.rs             Core API tests
├── golden.rs           Golden-value scoring regression tests
├── memory_budget.rs    Budget enforcement tests
├── metrics.rs          Metrics emission tests
├── integration.rs      Cross-feature integration tests
└── wand_correctness.rs WAND vs brute-force equivalence tests
benches/
└── bm25_bench.rs       Criterion benchmarks (search latency, context reuse, top-k)
```

## License

Apache-2.0
