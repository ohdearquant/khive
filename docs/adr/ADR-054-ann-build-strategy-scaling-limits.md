# ADR-054: ANN Build Strategy and Scaling Limits

**Status**: Proposed\
**Date**: 2026-06-14

---

## Context

khive uses `khive-vamana`, a graph-based approximate nearest-neighbor index, as the vector-search
leg of hybrid retrieval. Build cost, query cost, memory use, and recall scale differently. A single
claim that the index is "sublinear" would therefore be incomplete.

This ADR defines bounded engineering claims and a reproducible evaluation protocol. It does not
publish deployment measurements or make a guarantee from unpublished data.

## Decision

### 1. State scaling claims by axis

The public contract distinguishes:

| Axis               | Contract                                                             |
| ------------------ | -------------------------------------------------------------------- |
| Query work         | Approximate graph traversal; measured at fixed recall                |
| Full build work    | At least linear in vector count because every vector is read         |
| Incremental insert | Bounded by configured graph-search and degree parameters             |
| Index storage      | Linear in vector count for fixed dimensions and graph degree         |
| Working memory     | Bounded by vector storage, graph state, and configured build buffers |
| Consolidation      | A maintenance operation whose cost must be measured separately       |

No asymptotic query claim applies without holding recall, vector dimensions, distance metric,
graph parameters, and hardware conditions constant.

### 2. Use full batch construction for the supported scale

For corpora at hundreds of thousands of vectors, the supported default is full batch construction
with bounded input batches. Incremental insertion remains available for ordinary writes, but it is
not used as a substitute for a measured bulk-build strategy.

The builder must cap temporary buffers independently of total corpus size where the algorithm
allows. If a build would exceed configured memory or time limits, it fails with a clear capacity
error instead of silently switching algorithms.

### 3. Measure at fixed recall

Benchmark points use an iso-recall protocol:

1. select a public dataset with redistribution terms recorded alongside the benchmark;
2. divide it deterministically into index and query sets;
3. compute exact top-k neighbors for the query set;
4. build indexes for several corpus sizes using one fixed configuration;
5. select the smallest search beam that reaches the target recall;
6. report latency, recall, build time, peak resident memory, and on-disk size;
7. publish the command, hardware description, seed, configuration, and raw result file.

The report must separate cold-cache and warm-cache query results. Summary percentiles are not a
substitute for the raw per-query measurements.

### 4. Do not infer intrinsic dimension from ambient dimension

Ambient vector width is not a sufficient predictor of graph-search behavior. If a benchmark uses
an intrinsic-dimension estimate, it must publish the estimator, sample method, seed, and confidence
or stability analysis. The estimate is explanatory evidence, not a runtime input.

### 5. Gate configuration changes on evidence

Changes to graph degree, construction beam, search beam, pruning factor, quantization, or rebuild
cadence require a reproducible benchmark showing the effect at fixed recall. Results from one
dataset do not establish a universal default; the ADR may be amended when public evidence supports
a different operating envelope.

## Required benchmark record

Each checked-in result must include:

```text
dataset name and version
dataset license or source
vector count and dimensions
distance metric and normalization
index configuration
query count, k, and target recall
hardware and operating system
build time and peak resident memory
index file size
query latency distribution
random seed and exact command
```

Private database names, record content, and deployment capacity are not valid public benchmark
inputs.

## Limits of the decision

- This ADR does not guarantee a particular latency or speedup.
- It does not claim that build time is sublinear.
- It does not select a distributed builder.
- It does not establish defaults for corpora outside the measured public envelope.
- It does not replace correctness tests for persistence, deletion, or recovery.

## Consequences

### Positive

- Performance statements are reproducible and scoped to a clear axis.
- Configuration changes require evidence at fixed recall.
- Resource failures are explicit rather than hidden behind fallback behavior.
- Public documentation does not depend on private deployment measurements.

### Tradeoffs

- Full rebuilds remain material maintenance operations.
- The supported envelope is conservative until public benchmarks are checked in.
- A new dataset or hardware class may require a separate benchmark run.

## Testing requirements

- The build respects configured batch and memory bounds.
- Identical input order, seed, and configuration produce the same index metadata.
- Capacity failures do not replace an existing valid index.
- Benchmark tooling records every required field above.
- Recall is computed against an exact-search ground truth.

## References

- [ADR-012](./ADR-012-retrieval-composition.md): retrieval composition
- [ADR-031](./ADR-031-multi-engine-retrieval.md): vector retrieval integration
- [ADR-052](./ADR-052-ann-production-lifecycle.md): persistence and index lifecycle
