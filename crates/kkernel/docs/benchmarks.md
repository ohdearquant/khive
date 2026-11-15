# kkernel benchmarks

<!-- bench-reference:begin -->

Generated from `perf/reference.json` by `scripts/render-bench-reference.py`; do not edit this block.

Reference baselines for `kkernel`. Values are the committed reference median for each benchmark, in the unit shown; A/A is the ratio between the two same-commit runs. The date is the committer date of the baseline commit.

Comparison class: `rustc 1.95.0 (59807616e 2026-04-14)`, `bench` profile, AMD Ryzen 7 PRO 8700GE w/ Radeon 780M Graphics, cores 2,3, kernel 6.8.0-142-generic.

| Benchmark                                                       | Reference median | A/A ratio | Baseline commit                            | Commit date |
| --------------------------------------------------------------- | ---------------- | --------- | ------------------------------------------ | ----------- |
| `kkernel:coord_locate/coord_locate/cache_hit/100/median_ns`     | 24.950 µs        | 1.016     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `kkernel:coord_locate/coord_locate/cache_hit/1000/median_ns`    | 24.909 µs        | 1.003     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `kkernel:coord_locate/coord_locate/cache_miss/100/median_ns`    | 126.559 µs       | 1.002     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `kkernel:coord_locate/coord_locate/cache_miss/1000/median_ns`   | 127.103 µs       | 1.007     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `kkernel:coord_locate/coord_locate/unknown_uuid/100/median_ns`  | 157.517 µs       | 1.006     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `kkernel:coord_locate/coord_locate/unknown_uuid/1000/median_ns` | 155.792 µs       | 1.002     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `kkernel:locator/locator/get_hit/1000/median_ns`                | 71.665 ns        | 1.001     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `kkernel:locator/locator/get_hit/10000/median_ns`               | 75.500 ns        | 1.007     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `kkernel:locator/locator/get_hit/100000/median_ns`              | 88.602 ns        | 1.012     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `kkernel:locator/locator/insert_new_lru/1000/median_ns`         | 146.668 ns       | 1.032     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `kkernel:locator/locator/insert_new_lru/10000/median_ns`        | 190.231 ns       | 1.067     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `kkernel:locator/locator/insert_new_lru/100000/median_ns`       | 219.721 ns       | 1.010     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `kkernel:locator/locator/purge_live/1000/median_ns`             | 58.426 ns        | 1.000     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `kkernel:locator/locator/purge_live/10000/median_ns`            | 58.424 ns        | 1.001     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `kkernel:locator/locator/purge_live/100000/median_ns`           | 59.212 ns        | 1.000     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |

<!-- bench-reference:end -->

The routing benchmarks call the public `LocatorCache` and
`SubstrateCoordinatorService` APIs. All inputs are generated, using UUID seed
5665. They require no external database, embedding model, daemon, or network.

```sh
cd crates
cargo bench -p kkernel --benches -- --quick --noplot
cargo bench -p kkernel --bench locator -- --list
cargo bench -p kkernel --bench locator -- 'get_hit/100000' --quick --noplot
cargo bench -p kkernel --bench coord_locate -- 'cache_miss/1000' --quick --noplot
```

Fixture construction runs only for selected scenarios. `--list` and filtered-out
scenarios construct no caches or databases. Correctness-only mode (`--test`, or
execution without `--bench`) performs one checked workload call per selected
scenario, without the supplemental timing report or its warm call. Fixture setup
and its complete identity, capacity and hit-count assertions still run. In the
locator target, the default-capacity census still runs once per selected size.
Each fixture is dropped before the next scenario starts. The coordinator executor
uses one current thread and at most two blocking threads.

```sh
cargo bench -p kkernel --bench locator -- --test
cargo bench -p kkernel --bench coord_locate -- --test
```

Criterion owns listing and scenario filtering. Explicit `--test` takes precedence
over `--bench`; mode-like text after `--` remains filter text. Explicit
`--profile-time` runs also omit the supplemental report and warm call, so only
Criterion's selected workload enters the profile. These mode changes supply no
new performance baseline.

## Scenarios

| Group          | Scenarios                            | Measurement                                                                                            | Inputs                                                               | Baseline        |
| -------------- | ------------------------------------ | ------------------------------------------------------------------------------------------------------ | -------------------------------------------------------------------- | --------------- |
| `locator`      | `get_hit/{1000,10000,100000}`        | Rotating successful public cache lookups, including the returned backend-ID clone                      | Explicit capacity equal to retained entries; all entries live        | reference block |
| `locator`      | `insert_new_lru/{1000,10000,100000}` | A fresh UUID insertion into a full cache, including eviction; backend-ID input clone excluded          | Constant retained cardinality, new sequential UUID per call          | reference block |
| `locator`      | `purge_live/{1000,10000,100000}`     | `purge_expired` with no expired entries                                                                | Full live cache; oldest entry checked before and after each call     | reference block |
| `coord_locate` | `cache_hit/{100,1000}`               | Actual service lookup with a prepopulated locator key, including the backend endpoint probe            | Two independent in-memory SQLite backends, half the entities on each | reference block |
| `coord_locate` | `cache_miss/{100,1000}`              | Actual service lookup after removing the key, including concurrent backend probes and cache population | Same two-backend fixture, rotating known entity IDs                  | reference block |
| `coord_locate` | `unknown_uuid/{100,1000}`            | Actual service lookup of an absent UUID across both backends                                           | Same fixture; unknown ID never collides with fixture IDs             | reference block |

In ordinary measurement mode (`--bench`, supplied by `cargo bench`), each selected
scenario first emits one `routing_call_report` from **1,001 separately timed calls**,
following one untabulated warm call. It reports the median at rank
501 and the nearest-rank p99 at rank 991. These include `Instant` timer overhead.
A separate Criterion run provides an advisory distribution of iteration averages;
its confidence interval is not used as a per-call p99. Criterion uses 10 samples,
a 1-second warm-up, and a 3-second measurement target unless its command-line mode
changes that behavior. No latency threshold gates these benches.

Setup, input preparation, assertions, output formatting, sorting, and result drops
are outside each call timer. Coordinator calls time `block_on(service.locate(id))`,
including the async work and endpoint reads. A cache hit still checks that the
record exists on the selected backend; it is not a zero-I/O cache-only operation.
Cold cases remove the selected key immediately before every timed call. The
returned backend and resulting cache state are checked after every call. Fixture
setup verifies all 100 or 1,000 entity identities and their exact backend counts.
No embedding provider is configured, and in-memory storage has no queued writer.

The `locator` benches build their caches with a 24-hour TTL so that no entry expires
during a run; the coordinator scenarios use the library default of 5 minutes, which
is also the production default. Hit and purge cases fail if expected entries expire;
insert cases fail if retained cardinality changes. The insertion stream always
uses new keys, so it measures saturated insertion with eviction, not overwrite.
It remains capacity bounded throughout the fixed report and Criterion iterations.
`purge_live` measures the live-entry fast path, **not an expired-entry sweep**.
Public insertion itself prunes expired entries, and there is no public clock or
backdated insertion seam for constructing a large already-expired fixture.

For every selected cache size, an untimed `locator_capacity_report` also probes
every requested UUID at the default capacity. The default cap is 65,536: inserting
100,000 keys retains 65,536, with a 0.65536 hit rate for a full scan. The census does
not reinsert misses. Each operation fixture separately proves all N keys are
retained with explicit capacity N, so the 100,000-entry latency scenario actually
contains 100,000 entries. Reports include requested keys, retained keys, hits, and
hit rate; the default and explicit-capacity figures are not interchangeable.

## Performance guarantees

These are the requested advisory measurements. Timing and memory observations are
historical measurements, not promises. Recorded baselines come from the owning
bench-track component on a qualified host and are in the reference block at the
top of this file, with their commit and comparison class. That block records
medians, so a p99 row has a recorded median and no recorded p99. The API-boundary
exclusions below have no measurement producer in this
change and must not be filled from an unrelated wrapper benchmark.

| Operation                             | Input                   | Metric                 | Bound                                        | Baseline                          |
| ------------------------------------- | ----------------------- | ---------------------- | -------------------------------------------- | --------------------------------- |
| `LocatorCache::get`                   | 100k entries, hit       | p99 latency (ns/op)    | Advisory trend against the recorded baseline | median in reference block; no p99 |
| `LocatorCache::insert`                | 100k entries pre-filled | ns/op                  | Advisory trend against the recorded baseline | reference block                   |
| `SubstrateCoordinatorService::locate` | 1k records, cache miss  | p99 latency (ns/op)    | Advisory trend against the recorded baseline | median in reference block; no p99 |
| `atomic_apply`                        | 256-op batch            | median latency (ns/op) | Advisory trend against the recorded baseline | not measured: private entry       |
| `sweep_stale_fts_partitions`          | 64 partitions × 1k rows | wall-time (ns/op)      | Advisory trend against the recorded baseline | not measured: private entry       |
| `run_code_ingest`                     | 1000 files              | throughput (files/sec) | Advisory trend against the recorded baseline | not measured: source-file premise |
| `run_code_ingest`                     | 1000 files              | peak alloc (bytes/op)  | Advisory trend against the recorded baseline | not measured: source-file premise |

## API boundaries

- `atomic_apply` is a private library module, and
  `execute_atomic_ops_file` is `pub(crate)`. A separate bench crate cannot call it.
  Public `exec::run_exec` also performs configuration, ops-file parsing,
  runtime/registry startup, and output rendering. It would measure an end-to-end
  command, not isolated atomic apply. The 256-op size itself is permitted by the
  2,000-op default limit.
- `sweep_stale_fts_partitions` is `pub(super)` in a private module. Public
  `reindex::run_reindex` performs other indexing and maintenance stages as well.
  This change does not copy private implementations or widen visibility.
- `kkernel::code_ingest::run_code_ingest` takes one findings JSON document and
  persists finding records. It does not scan a source-file corpus. Source-file
  ingestion belongs to the separate `khive-pack-code` source-map API and its
  benchmarks (#5657 / #5690). The public admin findings wrapper also owns configuration/runtime
  setup and JSON output; a future findings benchmark needs its own honest input
  and measurement definition.

The routing groups do not claim atomic, FTS-sweep, source-file ingest, allocation,
or RSS coverage. Workflow wiring is maintained separately.
