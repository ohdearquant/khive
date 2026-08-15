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
  temp directory, with `HOME` and `KHIVE_DB` both redirected there for the
  duration of the run. The runner refuses to start if an inherited `KHIVE_DB`
  already points at an existing file outside its own scratch directory — it
  never reads or writes a production database.

## Running it

```bash
# Full run, prints the aggregate + per-class tables, writes per-query rows
uv run python evaluate.py --out results/A_fused_direct.jsonl

# Re-run and diff against the committed gold baseline (exit 0 = pass)
uv run python evaluate.py --check-gold

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
