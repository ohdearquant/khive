# ANN-799 benchmark harness -- infra slice (macOS ARM, no Cargo)

## What this PR does

Implements the harness infrastructure for the #799 matched-condition ANN
benchmark, per `docs/design/adr-799-baseline-plan.md` (khive-work), with the
platform ruling substituted for the plan's Linux-x86_64 platform section:
target host is Apple-silicon macOS ARM, required comparator set is
khive-vamana + faiss-cpu (Flat/HNSWFlat/IVFFlat) + hnswlib, and DiskANN-memory
is an optional attempt, never a required path.

No Cargo work in this slice. Everything is pure Python/TOML/JSON/Markdown/
bash.

## Files added

```
benchmarks/ann799/
  README.md                        layout, exact command lines, known platform limits
  protocol.toml                    fixed construction settings, split, seeds, thresholds
  requirements-macos-arm64.txt     pinned faiss-cpu 1.14.3 / hnswlib 0.8.0 / numpy
  dataset.py                       fvecs/ivecs readers, SHA256 manifest, validation
  runner.py                        build x3, calibration binary search, 10 warm blocks, JSONL
  adapters/base.py                 shared AnnAdapter ABI
  adapters/faiss_cpu.py            Flat, HNSWFlat, IVFFlat adapters (import-guarded)
  adapters/hnswlib_adapter.py      hnswlib L2 HNSW adapter (import-guarded)
  adapters/FOLLOW-UP.md            status of deferred khive-vamana / diskann-memory adapters
  schema-v2.json                   raw + summary result contract
  report.py                        recall, percentiles, bootstrap CI, CV, permutation test, dz
scripts/perf/
  ann799_preflight.sh              fail-closed resource/dataset/quiescence checks
  ann799_run.sh                    thread pinning + best-effort macOS QoS + runs the matrix
  ann799_ingest.py                 schema validation + ledger append
perf/
  ann799-ledger.csv                header row only (superset of ledger.csv)
docs/benchmarks/
  ann799-matched-ann.md            methodology skeleton, RESULTS-PENDING, generic approval language
.gitignore                         + perf/ann799-runs/, target/ann799/, *.fvecs, *.ivecs, **/sift1m/
```

## Deferred (not stubbed)

- **`adapters/khive_vamana.rs`** -- held for a follow-up PR to keep this
  slice's verification lane Cargo-free (single-lane discipline for this PR).
  It compiles on macOS ARM; no platform blocker. See
  `benchmarks/ann799/adapters/FOLLOW-UP.md` for the two implementation
  shapes considered (PyO3 binding vs. stdio subprocess adapter) and the ABI
  it must satisfy.
- **`adapters/diskann_memory.*`** -- optional-attempt per the platform
  ruling. No adapter code ships until a build attempt is actually made on
  target hardware; `runner.py` skips any system with no registered adapter
  and says so on stderr, and the docs page states the exclusion reason
  plainly rather than silently omitting the row.

`runner.py --systems` currently accepts `faiss-flat,faiss-hnswflat,faiss-ivfflat,hnswlib`
(the default) -- the four adapters that exist in this slice.

## Verification performed

No cargo, per the brief. Static checks:

- `uv run python3 -m py_compile` on every `.py` file -- all pass.
- `bash -n` on both `.sh` scripts -- both pass.
- `protocol.toml` parses with stdlib `tomllib`; six systems present
  (`khive-vamana`, `faiss-flat`, `faiss-hnswflat`, `faiss-ivfflat`,
  `hnswlib`, `diskann-memory`).
- `schema-v2.json` parses as JSON; `raw_sample_record`, `calibration_point`,
  `summary_record`, `ledger_row` definitions present.
- Every module's `--help` runs cleanly (`dataset.py`, `runner.py`,
  `report.py`, `scripts/perf/ann799_ingest.py`).
- Import-guard check: `runner.py` imports successfully with faiss/hnswlib
  **not installed**; `ADAPTER_REGISTRY` still populates correctly (the
  guarded `try/except ImportError` in each adapter module works as
  designed, not just in theory).

Beyond static checks, I generated a small synthetic SIFT-shaped dataset
(5,000 base / 300 query / 32-d, brute-force top-100 ground truth) under
`target/ann799_smoke/` (gitignored, not part of this PR) and ran faiss-cpu
and hnswlib as real ephemeral `uv run --with` dependencies -- the actual
libraries, actually installed on this Apple-silicon host, not mocked. The
full pipeline ran end-to-end for real:

- `runner.py` built each of the four adapters twice, ran calibration binary
  search (all four converged; `faiss-hnswflat` and `faiss-ivfflat` landed
  in-band, `faiss-flat`'s exact recall of 1.0 correctly fell outside the
  iso-recall band as expected for the exact control, `hnswlib` landed
  slightly above the tightened smoke-test band), ran 3 randomized warm
  blocks each, and wrote valid summary JSON + per-block JSONL.
- `report.py` consumed those summaries and rendered a real iso-recall table
  with bootstrap CIs and CV.
- `scripts/perf/ann799_ingest.py` validated all four summaries against the
  required-field contract and appended four correct rows to a ledger CSV.

This confirms the calibration search, recall computation, RSS sampling,
build-repeat loop, warm-block JSONL writer, report statistics, and ingest
validation are real working code, not structurally-plausible stubs.

Not run in this slice (requires real SIFT-1M data, a reserved quiescent
host, and the deferred adapters): the actual 1M-vector benchmark, the
10K-vector adapter-equivalence smoke test wiring into preflight, and any
DiskANN build attempt.

## Flywheel note

`mcp__khive__request` was not reachable from this worktree's tool set in
this session (no khive MCP connection available), so the mandatory
before/after memory.recall / brain.auto_feedback / memory.remember calls in
the brief could not be executed. Flagging this rather than silently
skipping it -- the orchestrator should run the flywheel calls itself, or
re-run this note through a session with the khive MCP server connected. The
gotcha worth remembering once that's possible: `faiss-cpu` and `hnswlib`
both install cleanly via `uv run --with` on Apple-silicon macOS with no
native build toolchain needed (wheels exist for arm64 from faiss-cpu>=1.8.0
and hnswlib>=0.7.0) -- confirmed empirically in this session, not just from
PyPI classifiers.
