# ANN-799: matched-condition ANN benchmark -- khive-vamana vs. external baselines

Status: **RESULTS-PENDING**. This page is the public methodology skeleton
for issue #799. It ships with the harness before any measured run; the
"Results" section below is filled in by `benchmarks/ann799/report.py` after
a run passes preflight and is reviewed.

## What this answers

At 1,000,000 SIFT vectors, L2 distance, top-10 recall in the 0.95-0.96
operating band, one CPU thread, and warm steady-state cache, what latency,
build cost, and memory footprint does khive-vamana have relative to
established external ANN implementations, measured on the same host under
the same conditions?

This is a bounded, same-host comparison. It is not an all-hardware or
all-index "absolute SOTA" claim, and its numbers are **not comparable to
ann-benchmarks results published from AWS or other cloud hardware** -- those
runs use different CPUs, memory bandwidth, and OS scheduling. Any sentence
that compares this page's numbers to an external leaderboard misrepresents
both.

## Platform

The recorded host is an Apple M-series macOS ARM machine (the exact CPU
model, core counts, RAM, macOS version, and Python/package versions for a
given run are written into `environment-manifest.json` under that run's
directory and linked from the results table once available). This
supersedes an earlier draft of this benchmark that specified a bare-metal
Linux x86_64 host; the platform was changed by explicit ruling because the
target deployment and measurement environment for this comparison is
Apple silicon.

## Comparator set

| System         | Library                                                                    | Index                                          | Role                                           |
| -------------- | -------------------------------------------------------------------------- | ---------------------------------------------- | ---------------------------------------------- |
| khive-vamana   | this repository, commit under test                                         | Vamana (`R=64`, `Lbuild=128`, `alpha=1.0`)     | system under test                              |
| faiss-flat     | faiss-cpu 1.14.3                                                           | `IndexFlatL2`                                  | exact vectorized control, not an ANN contender |
| faiss-hnswflat | faiss-cpu 1.14.3                                                           | `IndexHNSWFlat` (`M=32`, `efConstruction=200`) | graph ANN reference                            |
| faiss-ivfflat  | faiss-cpu 1.14.3                                                           | `IndexIVFFlat` (`nlist=4096`)                  | coarse-partition ANN reference                 |
| hnswlib        | hnswlib 0.8.0                                                              | HNSW (`M=32`, `ef_construction=200`)           | secondary HNSW replication check               |
| diskann-memory | Microsoft DiskANN, `cpp_main` @ `78256bbab4685e1774e78d331e081a153be26823` | in-memory Vamana                               | **optional attempt** -- see below              |

`diskann-memory` is an optional-attempt comparator on this platform. Its
adapter is best-effort: if the legacy C++ line does not build on Apple
silicon within a short timebox, this run records it as _"excluded: does not
build on the test platform"_ rather than substituting a container, a
different branch, or DiskANN3. Every other row in the required comparator
set is expected to build and run on Apple silicon without modification.

## Protocol summary

Full detail lives in `benchmarks/ann799/protocol.toml` (machine-readable,
authoritative) and the harness source (`benchmarks/ann799/`,
`scripts/perf/ann799_*`). Summary:

- **Dataset:** SIFT-1M, 1,000,000 base / 10,000 query 128-d float32
  vectors, L2. Frozen file SHA256s are recorded per run in
  `dataset-manifest.json`.
- **Split:** queries 0-1999 are the immutable calibration set; queries
  2000-9999 are the immutable evaluation set.
- **Construction:** fixed per system (see `protocol.toml`
  `[systems.*]`); no construction-parameter sweep in this lane.
- **Calibration:** binary-search each system's native query-time width
  knob on the calibration split for the smallest value with recall@10 >=
  0.9500. Non-monotonic sweeps are recorded, not concealed.
- **Evaluation:** three clean index builds; ten randomized-order warm
  blocks of the full 8,000-query evaluation pass; raw per-query latency
  retained.
- **Statistics:** p50/p95/p99 per block, median of block percentiles, 95%
  bootstrap CI, recall@1/10/100 with a query-bootstrap CI, and -- for any
  "faster" claim -- a paired permutation test (p <= 0.05, |Cohen's dz| >=
  0.5, 95% CI excluding zero). A comparison that does not clear that bar is
  reported as "not distinguishable in this protocol," never as "faster."
- **Iso-recall eligibility:** a system's warm-latency figure only enters
  the head-to-head comparison table if its evaluation recall@10 falls in
  0.9500-0.9600 with a bootstrap 95% lower bound >= 0.9450. A system
  outside that band is reported on the calibration frontier only.

## Quiescence and run window

The pre-registered quiescence gate is `loadavg1 < 0.25` for all ten
one-minute samples immediately before a timed block, at least 90% idle on
the pinned core, and no sustained device utilization above 5%. The run
window for this benchmark is **coordinated**: other automated agents and
build activity on the host are held quiet for the duration, typically in an
overnight slot, rather than relying on incidental idleness.

If that bar proves unattainable even in a coordinated quiet window, the
documented procedure is to measure and record the observed idle-host
baseline load and propose a revised, justified bar for review -- never to
relabel a noisy run as having passed the original gate.

macOS provides no user-space equivalent of Linux's `taskset`/`numactl`.
Core pinning on this platform is _requested_ via best-effort process QoS
hints and recorded as such in each run's `environment-manifest.json`; it is
not a hard guarantee, and this page does not claim one.

## Claim scope

If a run's required rows pass review, the defensible public statement takes
this shape, with measured values substituted for placeholders:

> On the recorded Apple M-series host, khive-vamana vs FAISS HNSW/IVF and
> hnswlib at recall@10 0.95-0.96, one thread, warm: khive-vamana had p50
> `X` microseconds, compared with FAISS HNSWFlat `Y` microseconds, FAISS
> IVFFlat `Z` microseconds, and hnswlib `W` microseconds, alongside the
> exact FAISS Flat control. If DiskANN's memory index built successfully on
> this host, its figure is included; otherwise this page states plainly
> that it was excluded because it did not build on the test platform.

This explicitly excludes: other hardware or operating systems, GPU
execution, batch or concurrent throughput, SSD-resident DiskANN behavior,
filtered or dynamic search, high-dimensional or production-corpus
workloads, comparators outside the required set above, and any comparison
to a public ann-benchmarks leaderboard. It does not support an unqualified
"fastest ANN implementation" claim.

## Approval

This benchmark's protocol and public disclosure scope are approved by the
project's maintainers before a run is executed, and its final raw-artifact
checksums and run manifest are verified by a maintainer reviewer before any
external claim drawn from it is published.

## Results

_Pending a completed, reviewed run. `report.py` appends the iso-recall
summary table and paired-comparison table below this line._
