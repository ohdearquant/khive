# khive Formal Proofs

This directory contains Lean4 theorems covering the core algorithms in
`khive-retrieval`. Each proof file is self-contained: no runtime-dependency
assumptions appear in theorem statements. The proofs characterize the
algorithms, not the implementation.

**Source**: Ported from `khive-internal/platform/retrieval/` as part of
ADR-030 Phase 2.

## Theorem-to-Module Index

Every Rust module in `khive-retrieval` that corresponds to a verified
algorithm carries a header comment citing the proof namespace. The table
below maps proof namespace to Rust file and source proof file.

### Retrieval proofs (`proofs/Retrieval/`)

| Proof namespace                          | Lean file             | Rust module                                   |
| ---------------------------------------- | --------------------- | --------------------------------------------- |
| `khive.Retrieval.Distance.*`             | `Distance.lean`       | `crates/khive-hnsw/src/distance.rs`           |
| `khive.Retrieval.Cosine.*`               | `Cosine.lean`         | `crates/khive-hnsw/src/distance.rs`           |
| `khive.Retrieval.HNSW.*`                 | `HNSW.lean`           | `crates/khive-hnsw/src/index/`                |
| `khive.Retrieval.BM25.*`                 | `BM25.lean`           | `crates/khive-bm25/src/`                      |
| `khive.Retrieval.RRF.*`                  | `RRF.lean`            | `crates/khive-fusion/src/`                    |
| `khive.Retrieval.RRFAnalysis.*`          | `RRFAnalysis.lean`    | `crates/khive-fusion/src/`                    |
| `khive.Retrieval.QuantizationBounds.*`   | `QuantizationBounds.lean` | `crates/khive-hnsw/src/arena/`            |
| `khive.Retrieval.SkipCondition.*`        | `SkipCondition.lean`  | `crates/khive-hnsw/src/search_context.rs`     |
| `khive.Retrieval.Graph.*`                | `Graph.lean`          | `crates/khive-retrieval/src/graph/`           |
| `khive.Retrieval.RetrievalAlgorithms.*`  | `RetrievalAlgorithms.lean` | `crates/khive-retrieval/src/hybrid/`     |

### Scoring proofs (`proofs/Scoring/`)

| Proof namespace              | Lean file    | Rust module                           |
| ---------------------------- | ------------ | ------------------------------------- |
| `khive.Scoring.Score.*`      | `Score.lean` | `crates/khive-score/src/`             |

## Proof Status

All files in this directory are planned for port from `khive-internal` as
part of ADR-030 Phase 2. The directory structure and namespace registry are
established here so that:

1. Rust modules can carry proof-correspondence header comments immediately
   (before the `.lean` files land).
2. CI can validate that every cited namespace maps to an existing file.

See [ADR-030](../docs/adr/ADR-030-retrieval-stack-port.md) for the full
proof relocation plan and CI integration requirements.

## Usage in Rust Source

Each Rust module corresponding to a verified algorithm carries a header
comment of the form:

```rust
// Formal proof: khive.Retrieval.RRF.deterministic_ordering
```

The namespace is the canonical path under `proofs/` with dots replacing
directory separators, omitting the `.lean` extension and the final theorem
name.

## CI Integration

`lake build` is wired into CI so proofs do not drift from code. Until the
Lean files are ported, CI runs a namespace-presence check: every
`// Formal proof:` comment in Rust source must have a corresponding entry
in this README.
