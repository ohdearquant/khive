# khive-fusion

Rank fusion strategies — Reciprocal Rank Fusion (RRF), Weighted, and Union — for
combining ranked result lists from multiple retrieval sources with deterministic
scoring.

## Features

- **`FusionStrategy` enum** — `Rrf { k }` (default), `Weighted { weights }`,
  `Union`, `VectorOnly`, `KeywordOnly`, and `Custom { name, params }` for
  runtime-registered strategies
- **Validated construction** — `try_rrf`/`try_weighted`/`try_custom` reject
  `k == 0`, non-finite weights, and empty custom names; `serde` deserialization
  runs the same validation
- **Deterministic** — all scoring goes through `khive-score`'s fixed-point
  `DeterministicScore`; RRF fusion is permutation-invariant across source order
- **Weight helpers** — `normalize_weights`, `try_normalize_weights`,
  `weights_are_normalized` for callers building `Weighted` configs

## Usage

```rust
use khive_fusion::{fuse, FusionStrategy};
use khive_score::DeterministicScore;

let vector_hits = vec![
    ("doc_a", DeterministicScore::from_f64(0.9)),
    ("doc_b", DeterministicScore::from_f64(0.8)),
];
let keyword_hits = vec![
    ("doc_b", DeterministicScore::from_f64(0.95)),
    ("doc_c", DeterministicScore::from_f64(0.7)),
];

let fused = fuse(vec![vector_hits, keyword_hits], &FusionStrategy::rrf(), 10).unwrap();
for (id, score) in &fused {
    println!("{id}: {}", score.to_f64());
}
```

`fuse` dispatches on `strategy`: `Rrf` and `Union`/`VectorOnly`/`KeywordOnly` are
computed inline via `reciprocal_rank_fusion`/`union_fusion`; `Weighted` goes
through `weighted_fusion`. `Custom` strategies return
`FuseError::CustomRequiresRuntime` — they are dispatched by a runtime's fusion
registry, which this crate does not implement.

RRF's score is rank-based and ignores the input scores entirely:
`score(d) = sum(1 / (k + rank_i(d)))` across every source that ranks `d`, with
`k` defaulting to 60 (`DEFAULT_RRF_K`, per Craswell et al. 2009). A document
that appears in more source lists accumulates more contributions.

## Where this sits

`khive-fusion` is the fusion leg of khive's hybrid retrieval pipeline: it takes
ranked `(Id, DeterministicScore)` lists from `khive-hnsw` (vector) and
`khive-bm25` (keyword) and produces one fused ranking. It depends only on
`khive-score` and is consumed by `khive-retrieval`'s `HybridSearcher`
composition and by the runtime's ADR-012 retrieval composition path.

Governing ADR:
ADR-006
(deterministic scoring — the `DeterministicScore` contract every strategy here
scores against).

## License

BUSL-1.1. See the repository [LICENSE](https://github.com/ohdearquant/khive/blob/main/LICENSE).
