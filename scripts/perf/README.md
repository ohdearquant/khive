# scripts/perf/ - benchmark tooling

See `perf/README.md` for the Vamana scale-proof ledger (`perf/ledger.csv`).
This file documents the trend-tracking tooling: `bench_track.py`,
`publish_ledger.sh`, and the `bench-track.yml` CI workflow.

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

Per the bench-program spec's blocking-promotion ladder
(`.khive/workspaces/20260710/bench-program/SPEC-draft.md`), everything this
script records is **Advisory**: it is measured and trended, it never asserts
pass/fail, and it never places a threshold. Promotion to a blocking gate
requires a separate calibration pass (`bench_calibrate.py`, `K>=10` same-SHA
runs) plus a `>=2`-week zero-alarm observation window - not a one-off PR
decision.

### Record shape (`schema_version: 1`)

One JSON object per line, appended to `bench-data/<suite>.jsonl`:

```json
{
  "schema_version": 1,
  "suite": "components",
  "sha": "<full commit sha>",
  "branch": "main",
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
