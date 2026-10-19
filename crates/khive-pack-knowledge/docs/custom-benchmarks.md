# Reducible search and tail replay benchmarks

`search_latency` and `tail_replay` enumerate their cases with `--list` before opening
any runtime, seeding a corpus, building an ANN index or embedding input. The default cohorts
read no private fixtures and download no model weights. These synthetic cohorts measure
pipeline costs; they do not measure model inference or semantic retrieval quality.
Do not compare synthetic IDs to real-model search baselines.

## Search latency

```sh
cargo bench --manifest-path crates/Cargo.toml -p khive-pack-knowledge --bench search_latency -- --list
cargo bench --manifest-path crates/Cargo.toml -p khive-pack-knowledge --bench search_latency -- --save-baseline reference --noplot
```

Criterion records three cases under `knowledge_search_latency/synthetic_100/`:
`rerank_false`, `rerank_true`, and `default`. The corpus contains 100 generated
atoms with at least 20 words each. Atom subject numbers and revision zero seed a
BLAKE3-derived xorshift vector; normalization produces 384-dimensional vectors.
The fixed query names subject 7. No random external seed or model cache is needed.

All variants use the same in-memory corpus and actual registered
`knowledge.search`. Setup, seeding and one warm call per variant are outside the
measurements. The reranked variants exercise the unindexed-candidate fallback
embedding path; this is not a stored-vector or ANN performance claim. Every timed
call requires exactly ten results and the expected rerank provenance. Those checks
and async executor entry are included in the timings. Default Criterion settings
are 20 samples, 500 ms warmup and two seconds measurement per case; ordinary
Criterion arguments can override these explicitly. Timings are Criterion estimates,
not additional `BENCH-JSON` rows.

For explicit real-model inference, set `KHIVE_BENCH_REAL_MODEL=1` when running
`search_latency`. It retains the MiniLM runtime provider and the same three query
variants, under separate `knowledge_search_latency/real_model_100/` IDs. This mode
may download weights and measures real inference plus retrieval on generated atoms;
it still makes no semantic-quality claim. Its startup and runtime budget are separate
from the bounded synthetic default. `--list` still does not initialize the model.

## Tail replay

```sh
cargo bench --manifest-path crates/Cargo.toml -p khive-pack-knowledge --bench tail_replay -- --list
cargo bench --manifest-path crates/Cargo.toml -p khive-pack-knowledge --bench tail_replay -- --noplot --bench
```

The default is 10,000 generated atoms, `M=16` and offered rate 10 writes/second.
`KHIVE_BENCH_TAIL_100K=1` and `KHIVE_BENCH_TAIL_500K=1` opt into the larger corpora;
neither is enabled by default. `--quick` selects 64 atoms and default `M=4` for
smoke checks. `cargo test --benches` runs the target without `--bench`, and that run takes the
same quick cohort. Existing `KHIVE_BENCH_TAIL_M` and `KHIVE_BENCH_TAIL_WRITE_HZ`
overrides remain. Large corpora and custom write schedules need their own runtime
budget; the five-minute default target does not apply to them.

Each corpus has five cases: one index write, atom deletion, repeated writes to one
subject, distinct-subject writes, and two-commit vector-plus-atom deletion. Stable
IDs include corpus size, configured M, offered rate and arm:

```text
knowledge_tail_replay/synthetic/n_10000/m_16/hz_10/one_index_write/first_search
```

Single-write/delete arms still perform one operation; M is the configured burst
size used by the two burst arms. All cases keep the existing file-backed setup,
positive ANN provenance, empty starting tail, final visibility, statement-start
observations and complete `KNOWLEDGE_TAIL_REPLAY` diagnostic row. Each emits one
additional stdout record:

```text
BENCH-JSON {"id":"<the listed id>","statistic":"median_ns","value":123,"n":1}
```

The example value is illustrative. The value measured in a real run is the first
search latency after the write commit, including the existing observation overhead.
One fresh corpus produces one observation (`n=1`); its median is that observation,
not a percentile estimate over repeated samples. Commit-to-visible latency remains
in the full diagnostic row. Neither number isolates internal replay duration.
Collect at least two runs at the same source revision and environment before
using a reference. The default runtime target is under five minutes per target;
it requires measurement on the reference host, not inference from source size.

`--save-baseline <name>`, `--noplot`, `--bench` and other unrecognized arguments
are accepted and ignored by this custom target. The raw stdout capture is the
reference artifact; this target does not create Criterion baseline files.

## Protocol verification

After building both executables, pass their exact paths (not a wildcard that may
match an old build):

```sh
python3 crates/khive-pack-knowledge/perf/reducible/check.py --search /path/to/search_latency --tail /path/to/tail_replay
```

This checks exact lists, explicit large-cohort enumeration and the shared JSON
emitter. `--measure-tail` additionally runs the actual default workload with a
300-second subprocess timeout and verifies its records. The default protocol check
uses an explicitly selected `--regression-case=report-contract` control: its fixed
numbers are test data and must never be retained as benchmark measurements.
`--list --regression-case=list-only` must return before a deliberate setup guard;
this makes an omitted list branch fail immediately without running the corpus.

`perf/reducible/controls.json` specifies independent source mutations. Each must
compile first and fail the executable protocol check: forcing either list branch
to fall through reaches the guard; routing report lines away from stdout produces
missing IDs. A compiler error or missing executable is not a successful control.
The Python parser tests also reject omitted/duplicate IDs, invalid statistics,
nonfinite/nonpositive values, invalid sample counts and mixed cohorts.
