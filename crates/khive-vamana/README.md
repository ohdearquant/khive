# khive-vamana

Batch-built Vamana (DiskANN-style) ANN index over pre-normalized vectors, with
crash-safe atomic persistence and incremental maintenance operations.

## Features

- **Batch build** — `VamanaIndex::build` constructs the proximity graph from a
  flat row-major `&[f32]` slice in one pass
- **Two-tier distance** — graph construction and traversal use an SQ8 codec for
  the acquisition tier; `search()` returns exact `f32` squared-L2 distances for
  the final ranking, with an out-of-distribution fallback to exact greedy search
- **Incremental maintenance** — `insert`, `tombstone`/`tombstone_batch`, and
  `consolidate` for corpora that grow after the initial batch build
- **Crash-safe persistence** — `save_atomic`/`load_or_build` stage segments,
  checksum them with blake3, and commit via an atomic rename so a crash mid-write
  never leaves a torn index; `corpus_content_hash` fingerprints the source vectors
  so a stale on-disk index is detected and rebuilt automatically
- **Offline recall evaluation** — `recall_at_k` measures the index's own
  approximate-vs-exact recall against a query set

## Usage

```rust
use khive_vamana::{VamanaConfig, VamanaIndex};

// `vectors` is row-major, `vectors.len() == n * config.dimensions`, pre-normalized.
let config = VamanaConfig::with_dimensions(384);
let index = VamanaIndex::build(&vectors, config).unwrap();

let neighbors = index.search(&query, 10).unwrap();
for (node_id, distance) in &neighbors {
    println!("{node_id}: {distance}");
}
```

`khive_vamana::build(vectors, config)` and `khive_vamana::search(&index, query, k)`
are free-function equivalents of `VamanaIndex::build`/`search` for callers that
prefer not to import the type directly.

## Configuration

| Parameter          | Default | Description                                                     |
| ------------------ | ------- | --------------------------------------------------------------- |
| `dimensions`       | 384     | Vector width; must match the corpus.                            |
| `max_degree`       | 64      | Max out-degree of any graph node.                               |
| `search_list_size` | 128     | Greedy-search candidate list capacity; must be `>= max_degree`. |
| `alpha`            | 1.2     | Robust-prune alpha; must be finite and `>= 1.0`.                |

`VamanaConfig::validate()` rejects zero dimensions/degree/list size,
`search_list_size < max_degree`, and a non-finite or sub-1.0 `alpha`. Builder
methods (`with_max_degree`, `with_search_list_size`, `with_alpha`) return a copy
without re-validating; call `validate()` before use if the values come from
external input.

## Persistence

```rust
use std::path::Path;
use khive_vamana::VamanaIndex;

index.save_atomic(Path::new("./vamana-data")).unwrap();

// On restart: fast-path load if the on-disk fingerprint matches `corpus_vectors`,
// otherwise falls back to a fresh `build()` from `fallback_config`.
let restored = VamanaIndex::load_or_build(
    Path::new("./vamana-data"),
    &corpus_vectors,
    VamanaConfig::with_dimensions(384),
)
.unwrap();
```

## Where this sits

`khive-vamana` is the production ANN index used directly by the knowledge pack's
vector bridge and by `khive-retrieval`'s hybrid searcher. It depends only on
`khive-quant` plus `rayon`/`memmap2`/`bytemuck`/`blake3` for build parallelism and
persistence — no dependency on `khive-hnsw`, `khive-fusion`, or `khive-score`.

Governing ADRs:
[ADR-054](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-054-ann-build-strategy-scaling-limits.md)
(scaling contract for batch build and query latency) and
[ADR-052](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-052-ann-production-lifecycle.md)
(SQ8 quantization, tombstone delete, consolidation, crash-safe persistence).

## License

Apache-2.0.
