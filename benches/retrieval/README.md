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
  is rejected if any existing component of its path is a symlink (the root
  itself, its parent, or any ancestor hop — macOS's OS-owned `/tmp`, `/var`,
  `/etc` -> `/private/...` firmlink mappings excepted, and only those exact
  three prefixes), or if it already exists and is
  non-empty — the harness only ever writes into a scratch root it created
  itself. The child process gets a minimal
  allowlisted environment (`PATH`, `HOME`/`TMPDIR` redirected into `home`/
  `tmp` subdirectories created through the pinned scratch-root fd, `KHIVE_DB`,
  and a pinned `KHIVE_EMBEDDING_MODEL`) instead of
  the caller's full environment, so no inherited `KHIVE_*` variable can
  change what gets scored. The runner refuses to start if an inherited
  `KHIVE_DB` already points at an existing file outside its own scratch
  directory — it never reads or writes a production database.
  `evaluate.py --self-test` exercises these refusals directly (symlinked
  root, pre-existing/non-empty root, symlinked `eval.db`, directory
  substitution at cleanup, degraded/budget-capped recall envelopes) without
  needing a `kkernel` binary.

  **Residual TOCTOU window (by design, not closed):** every scratch path
  handed to `sqlite3`/the `kkernel` subprocess is verified (device + inode,
  through a TOCTOU-guarded root fd) immediately before that call —
  `verified_scratch_path`/`VerifiedScratchPath` in `evaluate.py`. Neither
  API accepts an already-open descriptor (and macOS has no `/proc/self/fd`
  to convert one), so a swap landing between that verification and the
  instant the consumer itself opens the path cannot be prevented from this
  side; `VerifiedScratchPath.recheck()`, called after the consumer returns,
  detects — but does not prevent — that residual race for paths the
  consumer only reads. Do not read this harness's path verification as
  full race-resistance for the lifetime of a returned path.

  **Residual TOCTOU window in cleanup (by design, not closed):**
  `cleanup_scratch` walks the scratch tree through a TOCTOU-guarded fd and,
  at every recursion depth, checks each directory's identity two ways: its
  `fstat`ed (device, inode) must match the inode `os.scandir()` observed
  for that name moments earlier (catching a directory swapped in *after*
  this run started walking the tree — same mechanism regardless of nesting
  depth, not just at the top level), and, for `home`/`tmp`, against the
  identity recorded when this run created them. Neither check can see a
  substitution that already happened *before* this run's own listing of a
  given directory — e.g. a directory the `kkernel` subprocess itself
  creates during the run (`blobs`, `eval.db.ann`, `.khive`, `.lattice/...`)
  has no creation-time identity recorded for it — only the listing-time
  check made when this run's own walk reaches it. A directory already
  replaced by the time that walk arrives is indistinguishable from one the
  subprocess legitimately created. This is the same category of residual
  as the path-API race above: detectable-forward-from-a-known-point, not
  closed for all time.
- **Binary identity**: every run records `kkernel --version` and prints it;
  `gold/A_fused_direct.json` embeds the `kkernel_version` it was derived
  with. A revision differing from gold's recorded one is reported as
  context — a warning when the metrics match, and a `context:` line ahead
  of any metric mismatches — never as a failure by itself, because every
  commit after the gold-derivation commit (including the one that ships
  the gold file) changes the revision hash without touching retrieval
  behavior. The gate's verdict rides on the metrics; the version line
  keeps a real binary drift from being misdiagnosed as a ranking
  regression. A gold file carrying no `kkernel_version` field at all
  warns that binary identity went unchecked rather than passing
  silently.

## Running it

```bash
# Full run, prints the aggregate + per-class tables, writes per-query rows
uv run python evaluate.py --out results/A_fused_direct.jsonl

# Re-run and diff against the committed gold baseline (exit 0 = pass).
# The tolerance matches the `make eval-retrieval-gold-check` target; the
# bare default is 0.0, which trips on a known rank-boundary tie-break
# jitter (see Determinism below).
uv run python evaluate.py --check-gold --gold-tolerance 0.002

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

## Question banks on a staged store copy

`bank.py` scores retrieval arms on a frozen, versioned question bank against a copy of a real store, where
`evaluate.py` uses its own synthetic corpus. A bank is a directory with `bank.json` (`bank_id`, `version`,
`cases_sha256`) and `cases.jsonl`; each case carries the query, the instant `T` the query belongs to, and
`needed_info`, the facts the task required, each one an equivalence set of record ids (the same fact kept as a
memory, a note and an entity counts once). The runner refuses a bank whose `cases.jsonl` no longer matches
`cases_sha256`, so a correction mints the next version instead of editing a scored one. A case may carry one
`namespace:<ns>` tag (the value is everything after the first colon, so `namespace:team-a:archive` reads
`team-a:archive`); the tag labels the case's population and must be in the bank's visible set. No recall passes it as a
`namespace` argument: an explicit namespace becomes the primary namespace, which is where a named profile's brain state
is read from, and a case namespace may hold none. Two such tags, or an empty value,
are refused when the bank loads.

```bash
uv run python bank.py self-test
uv run python bank.py check-bank banks/001/v1
uv run python bank.py run --bank banks/team-a/v1 --bank banks/team-b/v1 \
  --store-main /stage/khive.db --store-events /stage/khive-events.db \
  --snapshot-id <id> --engines-file engines.toml --models-dir ~/.lattice/models \
  --bank-visible team-a=team-a,local --bank-visible team-b=team-b,local \
  --profile profile-x --profile profile-y --expect-profiles brain_profile_snapshots.jsonl \
  --control-id <memory uuid> --feedback-actors <actor> --out runs/<name>
uv run python bootstrap.py runs/<name>/team-a/recall_rrf.jsonl runs/<name>/team-a/profile_on.profile-x.jsonl \
  --metrics hit@10,coverage@10,MRR@10
```

The copy is staged into a fresh scratch home and served by `kkernel mcp` processes with `KHIVE_NO_DAEMON=1`, one per bank; a
source under the caller's `~/.khive` is refused, and so is one with a non-empty WAL unless `--copy-wal` states that
no process writes it. A source whose inode, size or modification time changes during its copy stops the run. The
run starts only at normal memory pressure, read before the copy and again before each bank's server starts; each
bank's record holds its reading. Each server writes its stderr to its own file in the scratch home
(`kkernel-mcp-bank<N>.err`, numbered in the order the banks are given), named in that bank's record. The scratch
home is kept by default, and the record names its path and the staged files' sha256 before and after the run;
`--remove-scratch` removes it. On another machine, `--staged-home <dir>` serves a copy already placed at
`<dir>/home/.khive/` by a guarded transfer instead of copying it again: `<dir>/transfer.json` must carry verdict
`ok`, the same snapshot id, and each file's size and sha256, which are checked before anything is written, and a copy
with a `-wal` or `-shm` sidecar (one a server has already opened) is refused. That directory is never removed.
Every recall passes `created_before = T`, and a hit created at or after `T` stops the run.
Arms: three fusion strategies with configured
defaults (no binding row), a named profile against configured defaults, a profile with no feedback events, a profile
fed the named profile's explicit feedback events created before each case's `T`, and a fusion of recall with
`search` over notes and entities (whose late hits are dropped after the fact and counted, since `search` has no
time filter). The served profile's name is checked on every hit; what shows that the served profile was loaded from the frozen snapshot is the served-state check below. `composed` refuses a namespaced case before anything is
staged, because `search` has no read-namespace filter. Recall hits do not carry the namespace they were read from,
so the namespace a case names is a label checked against the bank's visible set, not a per-hit property.

`run` takes `--bank` and `--profile` more than once. One invocation stages one copy and scores
every bank in the order given, each with its own `kkernel mcp` process on that copy; within a bank the arms run in the
order given, and `profile_on` runs once per
profile. Bank ids and profiles must be distinct and usable as file names (letters, digits, dots, underscores and
hyphens, starting with a letter or digit). `--bank-visible <bank_id>=<ns>[,<ns>...]` gives the namespaces the bench actor
can read while that bank runs. It is required once for every `--bank`, a set may not name a bank that is not given, and
every case namespace tag must be in its bank's set; all of this is refused before anything is staged. The bank's process
gets a config listing exactly that set, and the actor id is the same for every bank.
`feedback_fresh` uses one fresh profile per bank;
`feedback_asof` takes exactly one `--profile` and creates one as-of profile per bank, whose replay starts again
at `--feedback-since`. Each bank writes `<out>/<bank_id>/<arm>.jsonl`, with `profile_on` written as
`<out>/<bank_id>/profile_on.<profile>.jsonl`; every row carries the bank id and version, the population, the serving
profile and the case namespace. `run.json` records each bank (id, version, cases hash, population, namespace counts, visible set, served-state reads),
the profiles, the store snapshot id, kkernel version, the staged files' sha256, the control
result and the metrics grouped by bank and file.

In each bank's server process, right after the handshake and before that process's first recall, each `--profile` is
read (`brain.profile`) and its `total_events` is compared with the count in the frozen read-back; a profile that is not
found, or a different count, stops the run naming the profile, the expected value and the served value. The order
matters: a recall that names no profile is credited to the default profile `balanced-recall-v1` in the server's memory,
so the readiness and control recalls would already have moved that count. Equal `total_events` shows that the served
profile was loaded from the frozen snapshot rather than seeded fresh under another namespace; it does not show that the
posteriors are identical. A name comparison cannot show even that: a profile seeded fresh under the same name passes
it. `--expect-profiles <path>` names the read-back, a
`brain_profile_snapshots.jsonl` export (one JSON array per line, `[profile_id, namespace, snapshot_json, updated_at]`);
the row whose profile id is `__brain__` and whose namespace is `--expect-namespace` (default `local`) supplies
`profiles.<id>.total_events`. It is required whenever `profile_on` is among the arms, and a file with no such row, or a
row lacking a requested profile, is refused before anything is staged. `run.json` records the file's path and sha256,
each compared pair, and per bank `served_state.after_in_process`: the same profiles read again in that bank's server
process after its arms. That is the in-process count, not stored state: it includes the credits for the recalls just
served, and the read-only arms (`recall_*`, `profile_off`, `profile_on`) do not write it to the staged copy. It is
recorded only and never stops the run. `--expect-profiles` is refused together with `feedback_fresh` or
`feedback_asof`: those arms call `brain.create_profile` and `brain.feedback`, which write the server's brain state to
the staged copy, so the served state is no longer the frozen one after them.

`calibrate-replay` takes one bank (or `--queries-from-events`) and one profile, and measures how closely a profile rebuilt from replayed feedback events reproduces the original
(top-10 overlap and rank-score differences), which bounds what the as-of-`T` arm can claim. Each query is recalled
live, then by the rebuilt profile, then live again; the two live answers are compared the same way and reported as
`aa`, a floor for the recall's own run-to-run variation (recency is read from the clock at each request, for one)
that the live/rebuilt difference has to clear before it counts against the replay. Its queries come from a
bank, or with `--queries-from-events` from n distinct queries the profile served, read from the copy's
`recall_executed` events and spread over time; those events undercount recalls made through `kkernel exec`
(#6259).

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
