# scripts/perf/ - benchmark tooling

See `perf/README.md` for the Vamana scale-proof ledger (`perf/ledger.csv`).
This file documents the trend-tracking tooling: `bench_track.py`,
`publish_ledger.sh`, and the `bench-track.yml` CI workflow.

## Benchmark evidence protocol

Any latency or throughput result used as decision evidence in an issue, pull
request, or design record must link to a measurement plan written before the
run. The plan and result must use the same cache-state labels and warm-up
boundary.

### Cache states and warm-up

Pre-register every state that will be measured and the procedure that
establishes it. Treat cache state as a vector rather than a single `cold` or
`warm` flag:

- **Cold start:** state whether the process is new, which application caches
  begin empty, and whether model or index initialization is included in the
  timed interval.
- **Warm process:** state the initialization or request sequence that makes
  the process resident before measurement.
- **SQLite cache:** state whether each sample uses a new connection or a warm
  connection and how the SQLite page-cache condition is established.
- **OS page cache:** state how the filesystem-cache condition is established.
  If the runner cannot reliably establish a cold OS cache, report that state
  as uncontrolled rather than calling it cold.

Declare the warm-up boundary before measuring: the exact number of discarded
requests, elapsed-time rule, or observable readiness condition. Only samples
collected after that boundary are measurement samples. Preserve the cache-state
label on every sample or on an unambiguous enclosing run record.

### Analysis and isolation

Report each cache-state vector independently. Never pool samples across cache
states, and compute confidence intervals only from post-warm-up samples with
the same state label. If a required state has too few samples, report it as
insufficient rather than merging it with another state.

The exclusive benchmark window and an isolated build directory remain
required for decision evidence. They control resource contention and build
interference; they do not establish application, SQLite, or OS cache state.

### Write workloads

In addition to the cache protocol, pre-register and report the offered and
achieved write rate, overflow or backpressure policy, residency mode, vector
dimensionality, and every tail-cap parameter. A write-workload result missing
any of these fields is exploratory, not decision evidence.

## Trend ledger (`bench_track.py`)

`bench_track.py` is a stdlib-only companion to `bench_calibrate.py`. Where
`bench_calibrate.py` answers "what is the same-SHA noise floor for this
metric" (K>=10 runs, one SHA), `bench_track.py` answers "how has this metric
moved over time" (one run, many SHAs) - a JSONL history, not a variance
profile. It reuses `bench_calibrate.py`'s `SUITES` registry and per-suite
extractors (`pipeline`, `load`) rather than re-parsing their output, and adds
two more metric sources of its own: arbitrary bench JSON (`--source json`,
e.g. `bench_1m.sh --ci-synthetic`'s output) and Criterion's own
`estimates.json` tree (`--source criterion`).

Per the bench-program's blocking-promotion ladder, everything this
script records is **Advisory**: it is measured and trended, it never asserts
pass/fail, and it never places a threshold. Promotion to a blocking gate
requires a separate calibration pass (`bench_calibrate.py`, `K>=10` same-SHA
runs) plus a `>=2`-week zero-alarm observation window - not a one-off PR
decision.

### Record shape (`schema_version: 2`)

One JSON object per line, appended to `bench-data/<suite>.jsonl`:

```json
{
  "schema_version": 2,
  "suite": "components",
  "sha": "<full commit sha>",
  "branch": "main",
  "run_id": "<$GITHUB_RUN_ID, or 'local' outside CI>",
  "run_attempt": "<$GITHUB_RUN_ATTEMPT, or '1' outside CI>",
  "timestamp": "<commit's own commit-date, ISO8601>",
  "metrics": { "khive-score/score_ops.mean_ns": 42.3, "...": "..." },
  "host": { "os": "Linux", "arch": "x86_64", "python": "3.12.1", "cpu_count": 4, "runner": "..." }
}
```

`timestamp` is the commit's own commit-date (`git show -s --format=%cI
<sha>`), not wall-clock `time.time()`, so a re-run of the same SHA (e.g. a
workflow re-run) carries an identical, reproducible timestamp rather than
drifting with runner scheduling. It falls back to wall-clock only when git
cannot resolve the sha (a synthetic sha, or a shallow checkout missing the
commit).

`run_id`/`run_attempt` identify the workflow run a record belongs to
(`--run-id`/`--run-attempt`, defaulting to `$GITHUB_RUN_ID`/
`$GITHUB_RUN_ATTEMPT`, then `"local"`/`"1"` outside CI). `_aggregate_shards`
keys on `(sha, run_id, run_attempt)`, not sha alone, so a rerun of the same
commit is a distinct logical run in the trend - a pass-then-fail rerun of
one sha never blends one run's metrics with the other run's gate/host/error
provenance. A metric name written twice within the SAME run is a collision,
not an error: the later shard's value wins and the name is surfaced in the
rendered trend's `metric_collisions`.

### Commands

```bash
# Append one record from a Criterion output tree (component benches)
python3 scripts/perf/bench_track.py record \
  --suite components --source criterion --criterion-dir crates/target/criterion \
  --data-dir bench-data --summary-out /tmp/trend.md

# Append one record by running a bench_calibrate.py suite once (e2e pipeline)
python3 scripts/perf/bench_track.py record \
  --suite pipeline --source calibrate --data-dir bench-data

# Append one record from an arbitrary bench JSON file (e2e bench-1m synthetic)
python3 scripts/perf/bench_track.py record \
  --suite bench-1m --source json --json-file target/bench-out/SIFT-CI-synthetic.json \
  --data-dir bench-data

# Render the trend markdown for an existing ledger without recording anything
python3 scripts/perf/bench_track.py render --suite components --limit 10
```

### Reading trends

`render`/`record --summary-out` produce a markdown table: for each metric in
the latest run, the latest value, the previous run's value, a direction
arrow (`^ up` / `v down` / `= flat`), and the min/max over the rendered
window. There is no pass/fail column and no threshold column by design - a
metric moving the "wrong" direction is a prompt to look closer, not a build
failure. Use `bench_calibrate.py` if you need an actual noise-aware floor
for a specific metric.

## The `perf-data` branch

`bench-data/*.jsonl` is never committed to `main` or a feature branch (see
`.gitignore`). CI publishes it to a dedicated orphan branch, `perf-data`, via
`scripts/perf/publish_ledger.sh <file> [<file> ...]`: the script checks out
`perf-data` into a scratch git worktree, copies in the freshly-written
ledger file(s), commits, and pushes - retrying (fetch + reset + re-copy,
never a force-push) if a concurrent job's push landed first. Two jobs in the
same `bench-track.yml` run (`components`, `e2e`) both publish to this branch,
plus a nightly cron can overlap a push-triggered run, so the retry loop is
load-bearing, not defensive boilerplate.

To inspect history locally:

```bash
git fetch origin perf-data
git show origin/perf-data:bench-data/components.jsonl | tail -20
```

## CI wiring (`bench-track.yml`)

Triggers: push to `main` (path-filtered to `crates/**`, `scripts/perf/**`,
the workflow file itself - never a docs-only push), nightly cron, and
`workflow_dispatch`. Never runs on `pull_request` - no bench work rides a
PR. Two jobs:

- **`components`** - compile-checks every Criterion target
  (`cargo bench --workspace --all-targets --no-run`), then runs them with
  `--quick` under a `timeout`, bounded to fit the ~15 minute budget with
  `Swatinem/rust-cache` warm.
- **`e2e`** - runs the pipeline daemon suite (via `bench_calibrate.py`'s
  `pipeline` extractor) and the hermetic `bench-1m --ci-synthetic` gate as a
  single informational data point each.

Both jobs upload their raw output (Criterion trees / bench JSON / CSV
ledgers) as a 90-day-retention build artifact, write the rendered trend
markdown to `$GITHUB_STEP_SUMMARY`, and are skipped entirely if the
repository variable `BENCH_TRACK_DISABLED` is set to `true`.
