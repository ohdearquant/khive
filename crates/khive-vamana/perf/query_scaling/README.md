# Query scaling evidence

This study supports [#168](https://github.com/ohdearquant/khive/issues/168).
It reuses `examples/vec_bench.rs`; it does not introduce another ANN producer.
The checker validates archived artifacts and recomputes descriptive fits. It
does not run Cargo, open a database, export a live corpus, load a model, or
declare the proposed ADR-054 accepted.

## Prepare three separate cohorts

1. A representative sentence corpus with its exact production embedding model
   and revision, explicitly labeled **proxy**.
2. An approved, frozen export of the live KG vectors, labeled **live_kg**. Record
   the snapshot provenance, model and revision, normalization, vector dimension,
   and query split. Do not publish private vectors, text, paths, or identifying
   corpus metadata. An input hash proves byte identity, not permission to publish.
3. A comparison corpus for the regime crossover, labeled **comparison**, with
   its own model and intrinsic-dimension evidence. More N points on two corpora
   alone do not locate a universal intrinsic-dimension threshold.

Do not combine models, dimensions, hosts, configurations, or datasets into one
fit. Freeze held-out queries and nested base prefixes once per cohort. Use at
least four increasing sizes spanning at least 10x; for example,
`100000,200000,500000,1000000`. The largest size must fit the actual corpus and
the machine's memory. A smaller live corpus requires a smaller valid range,
not duplication of vectors to manufacture N. Keep query membership independent
of every base subset and archive the split recipe.

## Run and retain raw evidence

Use a clean checkout of one exact commit and a recorded build command/toolchain.
On the measurement host, from `crates/`, a typical command is:

```sh
KHIVE_N_CAP=1000000 cargo run --release -p khive-vamana --example vec_bench -- \
  --base /data/frozen/base.fvecs --query /data/frozen/query.fvecs \
  --ns 100000,200000,500000,1000000 --dataset frozen-sentence-corpus \
  --normalization l2 --alpha 1.0 --workers 1 \
  --out /results/run-1/raw.json --bank-run /results/run-1/bank
```

Perform at least two separate executions at that same commit, with the same
cohort, host, and configuration. Use distinct output and bank directories:
the existing bank filename has only date/commit/dataset granularity. Save each
process exit code, stdout, exact argv, build receipt, host/load information, and
raw JSON. Do not rerun on a developer laptop as a substitute for the measurement
host. The tool's older target assertions are separate from this study; an absent
targets file may yield a nonzero exit even though raw JSON was written. Preserve
that status and diagnose it; do not rewrite it or treat it as a measurement pass.

`vec_bench` computes BLAKE3 over every fvecs record, including each dimension
header, while loading it. `dataset.base_blake3` and `dataset.query_blake3` bind
the actual consumed input bytes. This work is outside query timing. It neither
normalizes vectors nor proves their declared model or corpus origin. Keep files
immutable during the run. The existing `git_sha` comes from the process's working
checkout, so a clean build/run receipt is still necessary to establish executable
provenance. Timing, distance, recall, beam selection, and the legacy verdict are
unchanged.

## Check one cohort

Create a manifest beside its raw artifacts, using actual hashes and metadata:

```json
{
  "schema_version": 1,
  "source_commit": "FULL_40_CHARACTER_COMMIT",
  "dataset": {
    "kind": "proxy",
    "name": "frozen-sentence-corpus",
    "model": "EXACT_MODEL",
    "model_revision": "IMMUTABLE_MODEL_REVISION",
    "snapshot": "PUBLIC_SAFE_SNAPSHOT_REFERENCE",
    "query_split": "PUBLIC_SAFE_HELD_OUT_SPLIT_RECIPE",
    "base_blake3": "BLAKE3_FROM_RAW_DATASET",
    "query_blake3": "BLAKE3_FROM_RAW_DATASET"
  },
  "runs": [
    { "file": "run-1/raw.json", "sha256": "SHA256_OF_RAW_FILE" },
    { "file": "run-2/raw.json", "sha256": "SHA256_OF_RAW_FILE" }
  ]
}
```

The placeholders deliberately fail validation. Compute raw SHA-256 hashes with
`shasum -a 256` and copy input hashes from the retained producer output. Then:

```sh
python3 check.py manifest.json > checked.json
```

Python 3.9 or newer, standard library only. Exit 0 means artifact consistency
is valid; exit 2 means invalid evidence. It requires two distinct artifacts and
timestamps, one commit, matching inputs/configuration/host/N matrix, at least
four distinct points spanning 10x, finite positive latency with ordered
quantiles, subset-specific brute-force ground truth, and unsaturated recall@10
at or above the declared target (at least 0.95). Raw JSON SHA-256 binds the report
to the files checked. A malicious copy with altered metadata is not detectable;
execution receipts remain part of the evidence.

## Interpret, do not promote a point estimate

The report recomputes query, beam, and brute-force log-log OLS independently for
each run. It includes exponent, R², log-space fit and constant-model residuals,
and max/min ratio. Constant data has undefined R² (`null`). Producer-supplied
`fits`, `decisive_question`, and assertion labels are never treated as evidence.

Compare repeated curves, recall, beam selection, load, brute-force scaling, and
the numerical fits. A low R² on almost constant p50 values is not proof of
sublinear growth. A lower residual from a fitted slope than a constant is not
model selection: the slope model has an extra parameter. Four aggregate medians
are not independent per-query observations and do not provide confidence bounds
on an unseen knee. Report **sublinear growth over the measured range**, **flat
over the measured range with the knee unresolved**, or **inconclusive**, with
the actual supporting evidence and uncertainty. Adverse/saturated runs remain
useful raw evidence even when this iso-recall checker refuses them; do not hide
them or substitute a looser recall target.

Commit public-safe raw reports, manifests, checked reports, and execution
receipts under this crate's `perf/` tree after both qualified runs. Separately
update ADR-054 with the measured range and conclusion. This source change does
not discharge the measurements, live-corpus, crossover, or ADR evidence
requirements of #168, and adds no performance threshold to CI.

## Integrity checks

```sh
python3 -m unittest discover -s . -p 'test_*.py' -v
cargo test -p khive-vamana --example vec_bench input_identity_tests
```

Run the Rust command from `crates/` on the measurement host. The Python tests
use generated synthetic JSON only. They check known constant and power-law
curves, matrix defects, recalled-result quality, mixed provenance, tampering,
duplicate runs, and the command's exit contract. See `controls.json` for
must-fail mutations; none of these controls is a measured ANN result.
