# khive-pack-kg Benchmark Ledger

<!-- bench-reference:begin -->

Generated from `perf/reference.json` by `scripts/render-bench-reference.py`; do not edit this block.

Reference baselines for `khive-pack-kg`. Values are the committed reference median for each benchmark, in the unit shown; A/A is the ratio between the two same-commit runs. The date is the committer date of the baseline commit.

| Benchmark                                    | Reference median | A/A ratio | Baseline commit                            | Commit date |
| -------------------------------------------- | ---------------- | --------- | ------------------------------------------ | ----------- |
| `kg_create/entity`                           | 3.802 ms         | 1.019     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `kg_create/note`                             | 287.978 µs       | 1.010     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `kg_delete/hard_entity_one_edge_edges_9000`  | 3.826 ms         | 1.002     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `kg_delete/soft_entity_one_edge_edges_9000`  | 4.027 ms         | 1.006     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `kg_get/by_uuid`                             | 27.576 µs        | 1.007     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `kg_link/single_edge`                        | 371.889 µs       | 1.006     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `kg_list/entity_all_limit50`                 | 247.267 µs       | 1.001     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `kg_list/entity_kind_concept_limit20`        | 134.234 µs       | 1.002     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `kg_list_notes/one_namespace_5000_limit20`   | 60.597 µs        | 1.011     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `kg_list_notes/two_namespaces_10000_limit20` | 144.250 µs       | 1.013     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `kg_neighbors/hub_100_out`                   | 1.232 ms         | 1.015     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `kg_search/entity_fts/100`                   | 2.425 ms         | 1.006     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `kg_search/entity_fts/1000`                  | 7.637 ms         | 1.007     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `kg_search/entity_fts/500`                   | 4.653 ms         | 1.014     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `kg_stats/entities_1000_edges_3999`          | 6.329 ms         | 1.017     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `kg_traverse/chain_depth/1`                  | 172.899 µs       | 1.007     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `kg_traverse/chain_depth/2`                  | 209.506 µs       | 1.025     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `kg_traverse/chain_depth/3`                  | 223.635 µs       | 1.034     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |

<!-- bench-reference:end -->

## Run command

```bash
cd crates && cargo bench -p khive-pack-kg --bench kg_bench
```

Dry-run (compile + single iteration, no timing):

```bash
cd crates && cargo bench -p khive-pack-kg --bench kg_bench -- --test
```

## Scenarios

| Group          | Scenario                      | Corpus           | What is measured                    |
| -------------- | ----------------------------- | ---------------- | ----------------------------------- |
| `kg_create`    | `entity`                      | —                | Single entity create (concept kind) |
| `kg_create`    | `note`                        | —                | Single observation note create      |
| `kg_get`       | `by_uuid`                     | 1 entity         | UUID lookup (read hot path)         |
| `kg_list`      | `entity_kind_concept_limit20` | 200 entities     | Paginated list with kind filter     |
| `kg_list`      | `entity_all_limit50`          | 200 entities     | Paginated list no kind filter       |
| `kg_search`    | `entity_fts/100`              | 100 entities     | FTS search over 100-entity corpus   |
| `kg_search`    | `entity_fts/500`              | 500 entities     | FTS search over 500-entity corpus   |
| `kg_search`    | `entity_fts/1000`             | 1 000 entities   | FTS search over 1 000-entity corpus |
| `kg_link`      | `single_edge`                 | 2 entities       | Edge upsert (idempotent re-upsert)  |
| `kg_neighbors` | `hub_100_out`                 | hub + 100 leaves | Outgoing neighbors on 100-edge hub  |
| `kg_traverse`  | `chain_depth/1`               | 10-node chain    | BFS depth 1                         |
| `kg_traverse`  | `chain_depth/2`               | 100-node chain   | BFS depth 2                         |
| `kg_traverse`  | `chain_depth/3`               | 100-node chain   | BFS depth 3                         |

## Release Ledger

### v0.2.8 - 2026-06-08

- Commit: `d3629501c550fd2f3bb7ed350a2b60309d596465`
- Crate version: `0.2.8`
- Khive version: `0.2.8`
- Toolchain: `rustc 1.94.1 (e408947bf 2026-03-25)`, release profile (Criterion)
- Machine: Apple M-series arm64, macOS Darwin 25.5.0, 16 GB
- Feature flags: default
- Command: `cd crates && cargo bench -p khive-pack-kg --bench kg_bench`
- Dataset: in-memory SQLite; entity / note / edge fixtures generated at bench setup; corpus sizes
  per scenario table above
- vs prior: first formal release ledger entry — no prior comparable baseline

#### Create

| Scenario         | Low      | Median   | High     | Outliers  |
| ---------------- | -------- | -------- | -------- | --------- |
| kg_create/entity | 6.311 ms | 6.798 ms | 7.236 ms | 2/50 (4%) |
| kg_create/note   | 2.238 ms | 2.508 ms | 2.794 ms | 1/50 (2%) |

#### Get / List

| Scenario                            | Low      | Median   | High     | Outliers   |
| ----------------------------------- | -------- | -------- | -------- | ---------- |
| kg_get/by_uuid                      | 70.85 µs | 81.20 µs | 94.17 µs | 9/100 (9%) |
| kg_list/entity_kind_concept_limit20 | 183.1 µs | 202.5 µs | 229.7 µs | 3/50 (6%)  |
| kg_list/entity_all_limit50          | 254.0 µs | 279.0 µs | 314.8 µs | 6/50 (12%) |

#### Search (FTS)

| Scenario                  | Low      | Median   | High     | Outliers   |
| ------------------------- | -------- | -------- | -------- | ---------- |
| kg_search/entity_fts/100  | 1.608 ms | 1.739 ms | 1.919 ms | 6/30 (20%) |
| kg_search/entity_fts/500  | 3.427 ms | 3.818 ms | 4.398 ms | 5/30 (17%) |
| kg_search/entity_fts/1000 | 5.879 ms | 6.289 ms | 6.937 ms | 4/30 (13%) |

#### Link / Graph

| Scenario                  | Low      | Median   | High     | Outliers   |
| ------------------------- | -------- | -------- | -------- | ---------- |
| kg_link/single_edge       | 164.8 µs | 192.8 µs | 220.9 µs | 7/50 (14%) |
| kg_neighbors/hub_100_out  | 2.406 ms | 2.931 ms | 3.406 ms | 5/50 (10%) |
| kg_traverse/chain_depth/1 | 242.4 µs | 291.3 µs | 349.5 µs | 4/30 (13%) |
| kg_traverse/chain_depth/2 | 339.2 µs | 369.1 µs | 405.7 µs | 2/30 (7%)  |
| kg_traverse/chain_depth/3 | 556.5 µs | 652.3 µs | 771.9 µs | —          |

- Notes: none

Last reviewed: v0.2.8 (2026-06-08)

## Dispatch lifecycle coverage

The `kg_lifecycle` target adds generated workloads through the public KG registry.
Its isolated in-memory runtime has no embeddings, ANN directory, event sidecar,
credentials, mounts or queued writer. It uses the same optional dispatch-audit-sink
composition as `kg_bench`: no persistent registry audit sink is attached, while
normal handler lifecycle events remain enabled. In particular, repeated `count`
reads do not append audit rows to the fixed 10,000-event ledger. This measures
in-process pack dispatch rather than MCP transport, model inference or audit-sink
batching overhead.

| Group                     | Scenario                                                                                                 | Input and timed unit                                                                                                                    | Untimed validation                                                                                       |
| ------------------------- | -------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------- |
| `kg_update_dispatch`      | `1000_updates/{100,1000}`                                                                                | A corpus of 100 or 1000 entities; 1000 serial `update` dispatches cycling IDs with a changing property                                  | Every response ID/property/version and every final stored entity; reset corpus before the next batch     |
| `kg_count_dispatch`       | `events10000/all`, `events10000/window`                                                                  | Exactly 10,000 events labelled `create`, `update`, `get`, `search`; three event kinds; one grouped `count`, without or with time bounds | Exact per-verb totals and unchanged full ledger size                                                     |
| `kg_merge_dispatch`       | `incident_each/{0,100,1000}/merge`                                                                       | Two matching-name concepts, each with N outgoing `contains` edges to disjoint leaves; one non-forced merge                              | N rewires, zero drops/conflicts, source merge tombstone, all 2N original edge IDs now rooted at survivor |
| `kg_create_bulk_dispatch` | `atomic/{10,100,1000}`                                                                                   | One atomic `create(items=[...])` dispatch with N generated concepts                                                                     | All indexed receipts, unique IDs, attempted/created counts, no failures/skips and actual stored rows     |
| `kg_traverse_matrix`      | `{out/include_roots_false,out/include_roots_true,in/include_roots_true,both/include_roots_true}/{1,2,3}` | A 100-node directed chain rooted at node 50; one traversal at the named depth                                                           | Exact node identities/depths, root inclusion and each path edge                                          |

Bulk creation supplies `kind` on every item; a top-level kind is not inherited.
The traverse matrix spells out four combinations: the existing out/false shape,
out/true, in/true and both/true. An interior root makes the incoming and both-direction
cases exercise real edges. The old endpoint-rooted traversal stays unchanged.
Merge inputs deliberately have no shared leaf, self-loop or natural-key conflict;
N source edges are rewired, while the survivor's N existing edges remain. Throughput
therefore counts N actual rewires, not the 2N input edges. The zero-edge case has
latency only, with no invented positive throughput denominator.

Fixtures are generated through public bulk creation and typed graph/event-store
batch APIs, with their actual batch outcomes checked. Entity creation is chunked
at the public 1000-item cap, and event seeding uses ten batches of 1000. The event
window uses strict `since < created_at < until`, selecting indices 2501 through
7499; expected group counts are calculated independently from generated indices.
These setup writes and all JSON argument preparation are outside timers.

### Timing and lifecycle

Every timed operation calls `VerbRegistry::dispatch`. Single-call timers include
executor entry and dispatch work; JSON receipts and stored-state validation happen
after elapsed time is captured. Merge and bulk creation get a fresh runtime for
every operation. Update gets a fresh corpus for every 1000-call batch; each call
changes its step property, so cycling a 100-row corpus cannot become an unchanged
write. Count and traverse reuse a fixed read fixture. Each fixture is destroyed
before the next one is constructed. The current-thread executor permits at most
two blocking threads; no batch of fixture runtimes is held concurrently.

In ordinary benchmark mode, each selected merge, bulk, count and traverse
scenario reports 21 actual individual call durations: nearest-rank p50 is sorted
rank 11 and p95 is rank 20. Update reports
p50/p95 from 1000 actual single-call timers (ranks 500/950), plus elapsed time and
throughput for that complete 1000-call batch. The batch timer includes loop and
per-call timer bookkeeping; argument and result buffers are prepared/reserved
before it starts. No percentile is calculated from averages of a batch.

For bulk and nonzero merge, the JSON report also derives work per second from the
median complete-call duration. Separate Criterion trends use sample size 10,
one-second warmup and three-second measurement targets. Update's Criterion unit is
one complete 1000-update batch; bulk throughput counts created items and merge
throughput counts actual rewires. Setup, validation and destruction still consume
additional wall time, especially rebuilding the largest merge fixture, even though
`iter_custom` excludes them from the measured duration. Existing `kg_bench` sampling
and workloads remain unchanged.

Listing (`--list`) and excluded scenarios construct no lifecycle executor or
fixture and execute no workload calls. Default test mode (without `--bench`)
and explicit `--test` execute one fully checked Criterion unit per selected
scenario, without the supplemental timing report. For update, that unit remains
a complete 1000-update batch; merge, bulk, count and traverse each execute one
complete operation. Selected tests still construct their full fixtures, including
the 10,000-event ledger and the largest merge graph. Profiling also skips the
supplemental report; first-use fixture setup can still appear in an external
profile. Arguments after `--` are filter text, not mode flags.

Ordinary benchmark mode emits each selected scenario's supplemental report once,
inside its first Criterion callback before `iter_custom`. The report remains
outside Criterion's recorded duration. The 22 lifecycle scenario IDs, fixtures,
per-call assertions, report sample counts, percentiles and Criterion settings
are unchanged. This scheduling change supplies no new measured baseline.

### Execution

```bash
cd crates
# Existing and new targets; kg_lifecycle prints the individual-call reports too.
cargo bench -p khive-pack-kg --benches -- --quick --noplot
# Only the additional dispatch workloads.
cargo bench -p khive-pack-kg --bench kg_lifecycle -- --quick --noplot
```

## Performance guarantees

| Operation                    | Input                       | Metric         | Bound                                                                      |
| ---------------------------- | --------------------------- | -------------- | -------------------------------------------------------------------------- |
| `kg_get/by_uuid`             | 1 entity, in-memory SQLite  | median latency | Advisory trend; last recorded value 81.20 µs (`docs/benchmarks.md` v0.2.8) |
| `kg_list/entity_all_limit50` | 200 entities, limit 50      | median latency | Advisory trend; last recorded value 279.0 µs (`docs/benchmarks.md` v0.2.8) |
| `kg_link/single_edge`        | 2 entities, idempotent      | median latency | Advisory trend; last recorded value 192.8 µs (`docs/benchmarks.md` v0.2.8) |
| `kg_traverse/chain_depth/3`  | 100-node chain, BFS depth 3 | median latency | Advisory trend; last recorded value 652.3 µs (`docs/benchmarks.md` v0.2.8) |
| `bench_update` (new)         | 100 entities                | median latency | Advisory trend against the recorded baseline                               |
| `bench_merge` (new)          | 2 entities, 1000 edges each | median latency | Advisory trend against the recorded baseline                               |

All rows are advisory timing trends against rolling and fixed references, never
fixed percentage gates. Historical values describe the named historical run and
are not present-day latency promises. Fixture assertions establish that each
sample performs its declared work; they do not add ordinary performance-count or
allocation gates. The new measurements remain pending until a qualified-host run.

## Pending baseline ledger

Producer: the `bench-track` component owning `khive-pack-kg`, measured on a qualified
host. Each completed entry must name its commit, toolchain, machine, fixture sizes
and command. The historical ledger above is preserved. Existing stats, list-notes
and delete groups receive entries here without changing their workloads or
fabricating values. Workflow selection belongs to the benchmark component.

| Group / scenario                                        | Metric and input                                                                | Baseline | Commit  | Toolchain / machine |
| ------------------------------------------------------- | ------------------------------------------------------------------------------- | -------- | ------- | ------------------- |
| `kg_update_dispatch/1000_updates/{100,1000}`            | Actual per-call p50/p95 µs and complete 1k-update throughput, both corpus sizes | pending  | pending | pending             |
| `kg_count_dispatch/events10000/{all,window}`            | p50/p95 µs per grouped count over 10K events, both filters                      | pending  | pending | pending             |
| `kg_merge_dispatch/incident_each/{0,100,1000}/merge`    | p50/p95 µs per merge; N rewires/second for nonzero N                            | pending  | pending | pending             |
| `kg_create_bulk_dispatch/atomic/{10,100,1000}`          | p50/p95 µs per batch and created items/second                                   | pending  | pending | pending             |
| `kg_traverse_matrix` (all 12 combinations above)        | p50/p95 µs per direction/include-roots/depth call                               | pending  | pending | pending             |
| `kg_stats/entities_1000_edges_3999` (existing)          | Existing dispatch estimate over 1000 entities, graph and 2000 notes             | pending  | pending | pending             |
| `kg_list_notes/two_namespaces_10000_limit20` (existing) | Existing runtime list estimate, 10K notes in two visible namespaces             | pending  | pending | pending             |
| `kg_list_notes/one_namespace_5000_limit20` (existing)   | Existing runtime list estimate, 5K notes in one namespace                       | pending  | pending | pending             |
| `kg_delete/hard_entity_one_edge_edges_9000` (existing)  | Existing full create/link/hard-delete iteration in the roughly 9K-edge fixture  | pending  | pending | pending             |
| `kg_delete/soft_entity_one_edge_edges_9000` (existing)  | Existing full create/link/soft-delete iteration in the roughly 9K-edge fixture  | pending  | pending | pending             |
