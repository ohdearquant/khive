# Retrieval eval harness

A committed regression gate for ranking changes to `memory.recall` (and future
retrieval conditions layered on top of it). Runs a deterministic 400-note
synthetic corpus through 40 graded queries and reports nDCG@10, Recall@100,
TargetRecall@100, and MRR@10, overall and per query class, with a paired
bootstrap comparison tool for evaluating changes against the committed gold
baseline.

## Protocol

- **Corpus**: 400 synthetic notes across 4 topic clusters (`release`,
  `ledger`, `research`, `support`), each cluster contributing 10
  proper-noun/code-bearing targets, 8 same-topic generic distractors, plus 328
  unrelated background notes shared across all clusters. Generated
  deterministically by `generate_corpus.py` from a fixed seed and a fixed
  epoch date (no wall-clock reads) — target note content is derived directly
  from the query text in `queries.jsonl`, so every target note actually
  answers the query that names it.
- **Queries**: 40 graded queries in `queries.jsonl` — 16 `exact_specific`, 12
  `paraphrase_specific`, 12 `fresh_directive` — each carrying full grade
  labels (0/1/2) over all 400 note keys. Grade 2 = the named target, grade 1 =
  same-topic generic notes in that query's cluster, grade 0 = everything else.
- **Metrics**: nDCG@10 (gain `2^grade - 1`), Recall@100 (grade ≥ 1 retrieved
  in the top-100 candidate pool), TargetRecall@100 (grade 2 retrieved in the
  pool), MRR@10 (reciprocal rank of the first grade-2 hit in the top 10).
- **Conditions**: `evaluate.py` calls `memory.recall(top_k=100,
  include_breakdown=true)` per query. `A_fused_direct` (the product default —
  no fusion/rerank override) is the only condition today; the `CONDITIONS`
  table in `evaluate.py` is the extension point for future legs (sparse
  fusion, a reranker) — add a name and the extra `memory.recall` args to
  layer on top of the shared base.
- **Isolation**: every run seeds a fresh scratch SQLite database under a
  temp directory. The default `--scratch-dir` is always a freshly created
  private directory (`tempfile.mkdtemp`); a caller-supplied `--scratch-dir`
  is rejected if it (or the parent -> root hop) is a symlink, or if it
  already exists and is non-empty — the harness only ever writes into a
  scratch root it created itself. The child process gets a minimal
  allowlisted environment (`PATH`, `HOME`/`TMPDIR` redirected into the
  scratch root, `KHIVE_DB`, and a pinned `KHIVE_EMBEDDING_MODEL`) instead of
  the caller's full environment, so no inherited `KHIVE_*` variable can
  change what gets scored. The runner refuses to start if an inherited
  `KHIVE_DB` already points at an existing file outside its own scratch
  directory — it never reads or writes a production database.
  `evaluate.py --self-test` exercises these refusals directly (symlinked
  root, pre-existing/non-empty root, symlinked `eval.db`) without needing a
  `kkernel` binary.
- **Binary identity**: every run records `kkernel --version` and prints it;
  `gold/A_fused_direct.json` embeds the `kkernel_version` it was derived
  with. A revision differing from gold's recorded one is reported as
  context — a warning when the metrics match, and a `context:` line ahead
  of any metric mismatches — never as a failure by itself, because every
  commit after the gold-derivation commit (including the one that ships
  the gold file) changes the revision hash without touching retrieval
  behavior. The gate's verdict rides on the metrics; the version line
  keeps a real binary drift from being misdiagnosed as a ranking
  regression.

## Running it

```bash
# Full run, prints the aggregate + per-class tables, writes per-query rows
uv run python evaluate.py --out results/A_fused_direct.jsonl

# Re-run and diff against the committed gold baseline (exit 0 = pass)
uv run python evaluate.py --check-gold

# Re-derive the committed gold baseline (writes kkernel_version + metrics)
uv run python evaluate.py --write-gold gold/A_fused_direct.json

# Scratch-dir/KHIVE_DB safety regression checks — no kkernel required
uv run python evaluate.py --self-test

# Paired comparison between two conditions' result JSONLs
uv run python bootstrap.py results/A_fused_direct.jsonl results/B_other.jsonl
```

Or via the repo-level gate target:

```bash
make eval-retrieval-gold-check
```

`kkernel` must be on `PATH` (or pass `--kkernel /path/to/kkernel`). Nothing
here touches `crates/` or requires a Rust rebuild.

## Adding a new condition

1. Add an entry to `CONDITIONS` in `evaluate.py` mapping a name to the extra
   `memory.recall` args that distinguish it (e.g. a `fusion_strategy` or a
   future reranker flag).
2. Run `uv run python evaluate.py --condition <name> --out results/<name>.jsonl`.
3. Compare it against the gold condition with `bootstrap.py` for a paired CI
   / significance read, or commit a new `gold/<name>.json` if it becomes a
   second baseline worth gating on.

## Determinism

`generate_corpus.py` and `evaluate.py` never read the wall clock or an
unseeded RNG — the corpus, note ages, and query order are all fixed by
`--seed`/`--epoch`. Running the harness twice back to back must produce
metric-for-metric identical per-query results; `gold/A_fused_direct.json` was
committed only after verifying that on this corpus.

Note ages are frozen via `--epoch`, but `memory.recall` itself computes
recency from `Utc::now()` at request time (`age_days`/`temporal_recency` in
`crates/khive-pack-memory/src/handlers/recall.rs` and
`crates/khive-pack-memory/src/scoring.rs`) — a fixed corpus epoch does not
freeze that clock read. `evaluate.py` sends every `memory.recall` call with
an explicit `config.scoring.weights.temporal = 0.0` (relevance/salience left
at the product defaults, 0.7/0.2, so the weight sum stays positive and the
non-temporal balance is unchanged) so the wall clock cannot influence the
composite rank score, and gold is clock-hermetic regardless of when the
harness runs. Temporal ranking behavior itself is deliberately out of this
gate's coverage until the runtime grows an evaluation-time clock override
that this harness can pin instead of disabling the term outright.

Independent of the temporal term, `--check-gold` uses a small nonzero
`--gold-tolerance` (0.002, set by the `make eval-retrieval-gold-check`
target) rather than exact equality: on repeated runs against an identical
scratch corpus and an identical pinned config, a rank-10-boundary tie in the
`fresh_directive` class occasionally resolves in either of two orderings
(nDCG@10 differs by up to ~0.0008; Recall@100/TargetRecall@100/MRR@10 are
unaffected — the retrieved candidate *set* is stable, only its order at an
exact score tie flips). This is a pre-existing `memory.recall`
scoring/fusion tie-break characteristic, not a wall-clock leak — verified by
running back to back with a fixed epoch and the temporal weight pinned to
0.0, where two of four consecutive runs matched exactly and two matched
each other exactly, in two discrete states rather than a wall-clock-driven
drift. Root-causing that tie-break is out of scope for this eval-only PR
(it would require a runtime change under `crates/`); the tolerance keeps the
gate meaningful for real regressions without chasing a single flipped rank
at an exact score tie.

## Relationship to the #939 measurement

The July 2026 measurement (issue #939, `.khive/REPORT.md`) established this
baseline discipline (400-note corpus, 40 graded queries in 3 classes, the
same four metrics, paired bootstrap CIs) and reported `A_fused_direct`
nDCG@10 = 0.5245. That run's corpus generator and scratch database were never
committed and are unrecoverable, so this harness's synthetic corpus is a
**different realization** of the same protocol, not a reproduction of it —
note content, salience, and ages here are freshly derived from `queries.jsonl`
using this harness's own generator. The historical 0.5245 figure (and the
rest of `.khive/REPORT.md`) is **directional reference only**; it is not this
harness's gold and should not be diffed against `gold/A_fused_direct.json`.
Only regressions measured against this harness's own committed gold baseline
are meaningful gate failures.
