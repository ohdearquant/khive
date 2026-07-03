# ADR-082: Retrieval Quality Measurement Loop

**Status**: proposed
**Date**: 2026-07-02
**Authors**: Ocean, lambda:khive
**Depends on**: [ADR-015](ADR-015-schema-migrations.md) (Schema Migrations), [ADR-023](ADR-023-declarative-pack-format.md) (Pack Verb Surface, Visibility, and Composition — `Visibility::Subhandler`), [ADR-047](ADR-047-knowledge-pack.md) (Knowledge Pack)
**GitHub**: #80
**Note**: this number was previously used by a retired v0-series draft ("Engine
Configuration Schema") consolidated into [ADR-031](ADR-031-multi-engine-retrieval.md);
that draft was archived at the 2026-05-23 v0→v1 ADR renumbering and does not refer to
this document.

## Context

### The metric issue #80 points at is real, but it is not the metric it names

Issue #80 ("retrieval quality measurement loop — coverage metric is 0.0 on 68k atoms")
observes that `knowledge.stats()` reports `eval_coverage: 0.0` and reads this as evidence
that retrieval quality on the 68k-atom production corpus is unmeasured — "faith-based."

The diagnosis of the underlying problem is correct; the field it names is not the field
at fault. `eval_coverage` is computed in
`crates/khive-pack-knowledge/src/knowledge/crud.rs` (`stats`, lines 604-680) as
`finalized_atoms / total_atoms` (lines 631-638, 658, 667-668), where `finalized_atoms`
counts rows with `finalized = 1` in `knowledge_atoms`. It is a **finalization-coverage**
metric — what fraction of atoms have passed a human review gate — not a
**retrieval-quality** metric. The production corpus was ingested without
`finalized: true`, so `0.0` is an honest finalization reading, not a broken retrieval
signal. The name `eval_coverage` is generic enough to invite exactly this misreading,
which is the root of issue #80's framing.

The real gap issue #80 surfaces is structural, not nominal: **no labeled query set, no
evaluation run, and no persisted retrieval-quality signal exist anywhere in the system.**
Retrieval quality has never been measured against ground truth. The one measurement that
exists is qualitative and off-system: a 2026-06-10 12-probe study
(`.khive/workspaces/20260610/knowledge-utilization/usage_patterns.md`, local and
gitignored) found a 50% useful-rate on real project questions, with reproducible gaps in
specific domains (SQLite WAL internals, speculative decoding, Metal/MSL, admissions ML).
That finding is not recorded anywhere the system itself can read, aggregate, or track
over time.

### The evaluation math already exists; it is not wired to anything live

`crates/khive-retrieval/src/eval/engine_eval.rs` is a complete, tested retrieval
evaluation library: a five-level graded relevance taxonomy (`RetrievalLabel`:
Decisive / Supporting / Background / Irrelevant / AdjacentWrong, with a negative gain
for `AdjacentWrong` to penalize misleading-but-plausible results), a `LabeledResult`
struct, and metric functions `recall_at_k`, `precision_at_k`, `ndcg_at_k`, `mrr`, and
`compute_all`. It is used today only in bench fixtures
(`crates/khive-retrieval/benches/fusion_bench.rs`), never invoked by `knowledge.search`
or `knowledge.stats`. Issue #80's "minimal viable loop" (a labeled query set, an
evaluation verb reporting hit-rate/MRR, optional feedback-grown set expansion) needs no
new _retrieval_ math — `knowledge.search` already exists and returns ranked slugs, so the
missing piece is plumbing, not a new search algorithm. It does need its own metric
contract, not this library's label-based functions: the runner scores against a
ground-truth expected-slug set (§"Eval runner mechanics" explains why the label-based
conventions here do not fit that use case).

### Scoping and rulings

`.khive/workspaces/20260626/issue80-scoping/SCOPING.md` (alpha:architect, 2026-06-26)
scoped this issue into four design forks and two Ocean-gated escalations: (1) verb
contract and visibility for the eval runner, (2) labeled query-set location and data
policy, (3) how `eval_coverage` gets computed and persisted, (4) feedback-grown set
expansion. This ADR formalizes lambda:leo's 2026-07-02 rulings on the two escalations
(D1, D2 below) and fixes the normative shape of slice-1. It does not reopen the forks;
it records the decisions and specifies what ships.

## Decision

### Scope — slice-1

This ADR specifies the smallest increment that moves retrieval-quality measurement off
"nonexistent" with an honest number: a versioned `knowledge_eval_runs` table, a labeled
query-set format (synthetic and public by default, path-overridable), a CLI-only eval
runner that reuses `knowledge.search` as its retrieval path with its own expected-set
metric contract (§"Eval runner mechanics"), hit-rate/MRR persisted per run, and
`knowledge.stats()` reading the most recent run. Feedback-grown query-set expansion
(issue #80's optional third bullet: accumulating labeled pairs from `brain.feedback`-style
signals emitted by agents) is explicitly **out of scope** for this ADR. It requires its
own accumulation-table design, its own OSS/private data-policy enforcement, and its own
ADR, and is deferred to a future slice.

### D1 — Additive field; `eval_coverage` is not renamed or redefined

`eval_coverage` in `knowledge.stats()` keeps its current name and its current semantics:
`finalized_atoms / total_atoms`, exactly as implemented today at
`crud.rs:667-668`. It is a legitimate metric and stays exactly as it is. No wire-shape
change, no consumer of the existing field is affected, and the existing integration-test
assertion (`crates/khive-pack-knowledge/tests/integration.rs:1125-1129`, `eval_coverage
== 0.5` for 1 of 2 finalized atoms) continues to hold unmodified.

Retrieval quality is reported through **new, additive** fields on the same response:

```json
{
  "total_atoms": 68000,
  "total_domains": 12,
  "total_events": 4110,
  "eval_coverage": 0.0,
  "embedding_coverage": 0.94,
  "retrieval_eval_coverage": 0.0,
  "retrieval_eval_run_count": 0,
  "retrieval_eval_last_run_at": null,
  "retrieval_eval_last_mrr": null,
  "namespace": "local"
}
```

`retrieval_eval_coverage` reports the most recent run's `precision_at_5` (§7). Before any
run has ever executed, all four `retrieval_eval_*` fields report their zero/null
sentinels (`0.0` / `0` / `null` / `null`) — a state distinguishable from "measured and
scored zero" by `retrieval_eval_run_count == 0`. `finalization` and `retrieval quality`
are orthogonal metrics; keeping them in separate fields is the honest model and requires
no breaking change to any caller of `knowledge.stats()`.

### D2 — CLI subhandler now; no MCP verb

The eval runner is exposed as `knowledge.eval_retrieval`, added to `KNOWLEDGE_HANDLERS`
in `crates/khive-pack-knowledge/src/vocab.rs` with `visibility: Visibility::Subhandler`
(ADR-023 §2). It is not reachable from MCP `request(...)`. It is callable only from the
kkernel CLI's verb-DSL `exec` subcommand:

```bash
kkernel exec 'knowledge.eval_retrieval(query_set="crates/khive-pack-knowledge/tests/fixtures/eval_set.toml")'
```

This is the same pattern already shipped for `memory.recall_embed` and
`memory.recall_fuse` (ADR-023 §2): a full-power operator/admin handler, invisible to
agents, reachable only through the kkernel CLI. Agents observe retrieval-quality state
only indirectly, through the `retrieval_eval_*` fields on `knowledge.stats()` — which
stays `Visibility::Verb` and MCP-exposed, unchanged.

Because `Subhandler` requires no MCP wire change, this ADR requires **no amendment to
ADR-023** and **no update to `AGENTS.md`**. `KNOWLEDGE_HANDLERS` grows from 19 to 20
entries (19 `Verb` + 1 `Subhandler`); the agent-facing verb count documented in
`AGENTS.md` ("Knowledge pack — 19 verbs") is unaffected, because `Subhandler` entries are
excluded from that count by definition.

This ADR deliberately does **not** decide whether `knowledge.eval_retrieval` (or an
equivalent) is later promoted to an MCP `Verb`. Retrieval-quality measurement is an
operator/maintenance function today, not a daily agent operation — there is no
demonstrated agent-side demand for triggering an eval run mid-session. A future promotion,
if usage ever justifies it, needs its own ADR-023 amendment and `AGENTS.md` sync under
its own justification. The broader verb-count convention question is a separate,
unrelated open thread and this ADR does not couple to it in either direction.

### Schema — `knowledge_eval_runs`

A new table, versioned per the standard migration mechanism (ADR-015: append a new
`VersionedMigration` with `version = <current ceiling> + 1`, backed by a new
`crates/khive-db/sql/NNN-<name>.sql` file; `V1` is never edited). As of this writing the
live migration ceiling is `V5` (`unique_comm_message_external_id`,
`crates/khive-db/sql/005-unique-comm-external-id.sql`), so this table is expected to land
at `V6`; the implementer verifies the exact next version against
`crates/khive-db/src/migrations.rs` at merge time, since other migrations may land first.
The migration file itself — the literal `.sql` and its `VersionedMigration` registration
— is authored in the implementation PR, per repository convention; this ADR fixes the
schema:

```sql
-- 006-knowledge-eval-runs.sql (illustrative filename; exact version per migrations.rs at merge time)
CREATE TABLE IF NOT EXISTS knowledge_eval_runs (
    id              TEXT PRIMARY KEY,
    namespace       TEXT NOT NULL,
    run_at          INTEGER NOT NULL,   -- unix ms
    query_set       TEXT NOT NULL,      -- filesystem path used for this run
    total_queries   INTEGER NOT NULL,
    precision_at_5  REAL NOT NULL,
    recall_at_5     REAL NOT NULL,
    mrr             REAL NOT NULL,
    notes           TEXT
);

CREATE INDEX IF NOT EXISTS idx_knowledge_eval_runs_ns_run_at
    ON knowledge_eval_runs(namespace, run_at DESC);
```

Purely additive: no existing table, column, or index is touched, and no data migration
runs against `knowledge_atoms` or any other table.

### Labeled query-set format

The query set is a TOML file, one file per set, shaped:

```toml
[[queries]]
query = "how does grouped query attention reduce KV cache size"
expected_slugs = ["gqa"]

[[queries]]
query = "retrieval augmented generation retrieving passages before generation"
expected_slugs = ["rag"]
```

A query may name more than one `expected_slugs` entry when multiple atoms legitimately
satisfy it. Every query MUST have non-empty `query` text and a non-empty `expected_slugs`
list; §"Eval runner mechanics" specifies the fail-fast validation contract for violations.
There is no `min_k` or other search-depth override in slice-1 — the runner always searches
with `limit = 5` (§"Eval runner mechanics") — because k is fixed, not a per-query or
per-run parameter; a future ADR may add configurable search depth alongside the
dynamic-`k` metric storage that would require.

The shipped public fixture, `crates/khive-pack-knowledge/tests/fixtures/eval_set.toml`,
is synthetic: 10-15 queries built over the same domain vocabulary already seeded in the
integration test corpus (`rag`, `lora`, `flash-attention`, `gqa`, `rope`, `agent`,
`chain-of-thought`, `speculative`, `quantization`, `dpo` —
`crates/khive-pack-knowledge/tests/integration.rs:1216-1225`). It is safe for the public
repository.

Real-usage query sets — including the 2026-06-10 12-probe study and any future set
derived from production corpus topics or agent feedback — are **excluded from the OSS
repository** by data policy. Committing them would expose what topics live in private
operator corpora, leaking corpus composition. `query_set` accepts an arbitrary filesystem
path, so operators point it at a private, gitignored location such as `.khive/eval/` to
run against their own labeled data without touching the repository.

### Eval runner mechanics

`knowledge.eval_retrieval(query_set: <path>)`:

**Namespace is derived, not a parameter.** The runner takes no `namespace` argument. It
derives its namespace from the dispatch token exactly the way `knowledge.stats` and
`knowledge.search` already do (`token.namespace()`), not from a caller-supplied override.
Every `knowledge.search` call the runner issues, and the `knowledge_eval_runs` row it
writes, therefore share exactly the token's namespace — `knowledge.stats()`
(§"`knowledge.stats()` reads the last run") reads the latest row for that same token
namespace, so a run's write and its later read can never diverge onto different
namespaces. There is no cross-namespace override in slice-1; an operator who needs to
evaluate a different namespace calls with a token scoped to that namespace.

**k is fixed at 5, not a parameter.** The runner takes no `k` argument and searches with
`limit = 5` for every query; every metric is computed at 5. Dynamic `k` (per-run or
per-query) requires schema support for per-k metric storage — the `knowledge_eval_runs`
schema above hard-codes `precision_at_5`/`recall_at_5` columns precisely because slice-1
fixes the contract at one k — and is out of scope here; a future ADR may add configurable
k alongside the storage change it requires.

1. Parse the TOML query set at `query_set`, and validate every `[[queries]]` entry before
   running anything. **Fail-fast contract**: if the file fails to parse, or any entry is
   invalid (empty/missing `query` text, or an empty `expected_slugs` list), the run
   **aborts** — no `knowledge_eval_runs` row is written — and `knowledge.eval_retrieval`
   returns a validation error naming the file path and the 0-indexed position of the
   offending entry in the `[[queries]]` array. Partial runs are never recorded, so
   `retrieval_eval_run_count` (introduced in D1 above) only ever counts complete, valid
   runs.
2. For each query, invoke the existing `knowledge.search` runtime path with `limit = 5`
   and `type = "atom"` (scoped to the runner's namespace, above), collecting the returned
   atom slugs in rank order (`search` already returns a `slug` field per hit —
   `crates/khive-pack-knowledge/src/knowledge/search.rs`). Pinning the result type is
   required, not optional: `search` defaults to `type = "both"`
   (`crates/khive-pack-knowledge/src/vocab.rs:180-184`), and under that default, domain
   mirror rows (`"kind": "domain"`,
   `crates/khive-pack-knowledge/src/knowledge/search.rs:1102-1110`) can occupy top-5
   slots. Slice 1 is atom-level evaluation — `expected_slugs` are atom slugs by the
   step-1 contract — so the runner requests atoms at the search call rather than
   filtering domains out of a mixed top 5 afterwards, which would silently hand the
   metrics fewer than 5 atom candidates. Call this ordered list
   `top_5_returned_slugs`; it may have fewer than 5 entries if the corpus has fewer than 5
   matches.
3. Score the query directly against `expected_slugs` by set intersection — no
   `LabeledResult` mapping, and no call into `khive_retrieval::eval::engine_eval`:
   - `hits = |expected_slugs ∩ top_5_returned_slugs|` — a slug-set intersection;
     `expected_slugs` contributes no order, `top_5_returned_slugs`'s rank order is used
     only by `mrr` below.
   - `recall_at_5 = hits / |expected_slugs|`. The denominator is the query's own expected
     set size. `expected_slugs` is guaranteed non-empty by the fail-fast contract in step
     1, so this division is always defined.
   - `precision_at_5 = hits / 5`. The denominator is the fixed requested k, never
     `top_5_returned_slugs.len()`: if search returns fewer than 5 results, the missing
     slots count as misses instead of shrinking the denominator.
   - `mrr = 1 / rank` of the first entry in `top_5_returned_slugs` whose slug is in
     `expected_slugs` (rank is 1-indexed); `0.0` if no entry in the top 5 matches.

   This deliberately diverges from the label-based conventions in
   `khive_retrieval::eval::engine_eval`: its `recall_at_k` returns a vacuous `1.0` when a
   results list contains no relevant labels at all, and its `precision_at_k` clamps its
   denominator to `results.len()` rather than the requested `k`. Those conventions are
   correct for their own purpose — grading the ranking quality of a results list a human
   or eval pipeline has already labeled, where "no relevant labels present" describes the
   label set, not a retrieval failure. This ADR measures something different: retrieval
   quality against a ground-truth expected set, where a query whose expected atoms are
   entirely absent from the top 5 is exactly the failure case being measured, and must
   score `recall_at_5 = 0.0`, not `1.0`. Reusing the label-based functions unmodified would
   make that failure case invisible, so the runner computes its own set-intersection
   metrics instead of calling `recall_at_k`, `precision_at_k`, or `mrr` from the eval
   library — `LabeledResult` and `RetrievalLabel` are not constructed anywhere in this
   runner.
4. Aggregate mean `precision_at_5`, `recall_at_5`, and `mrr` across every query in the set.
5. Write one row to `knowledge_eval_runs` with the runner's namespace (above).
6. Return `{run_id, total_queries, precision_at_5, recall_at_5, mrr}`.

### `knowledge.stats()` reads the last run

`stats()` (`crud.rs:604`) gains two additional reads against `knowledge_eval_runs`, both
filtered to the token's namespace. Two reads are required by the storage API's shapes:
`query_scalar` returns the first column of the first row as one `Option<SqlValue>`
(`crates/khive-storage/src/sql.rs`), so no single scalar read can supply both an
aggregate count and the latest row's metric columns.

1. A `query_scalar` `COUNT(*)` over the namespace's rows supplies
   `retrieval_eval_run_count`, following the same pattern as the existing
   `finalized_count` query at `crud.rs:631-638`. Every stored row is a complete valid
   run (the fail-fast contract in §"Eval runner mechanics" writes no partial rows), so
   the row count is the run count.
2. A `query_row` read selecting the most recent row (`ORDER BY run_at DESC LIMIT 1`)
   supplies the other three D1 fields: `retrieval_eval_coverage` (the row's
   `precision_at_5`), `retrieval_eval_last_run_at`, and `retrieval_eval_last_mrr`.

Both reads use the same `token.namespace()` derivation the runner itself uses to write
the row (§"Eval runner mechanics"), so the values `stats()` reads back always describe
the caller's own runs — never a different namespace's rows. No run for the namespace yet
produces the zero/null sentinel values described above.

## Consequences

### Positive

- Retrieval quality moves from "never measured" to an honest, run-triggered,
  namespace-scoped signal, closing the actual gap issue #80 identifies.
- Zero risk to the existing `eval_coverage` consumer or its integration test — the
  change is purely additive at the JSON level and the SQL schema level.
- No new _retrieval_ math: the runner reuses the existing `knowledge.search` runtime path
  unmodified. Its metric computation is new but intentionally small — direct
  set-intersection scoring against `expected_slugs` (§"Eval runner mechanics") — because
  the existing label-based `recall_at_k`/`precision_at_k`/`mrr` in
  `khive_retrieval::eval::engine_eval` are tuned for graded-relevance ranking review, not
  ground-truth-expected-set retrieval, and would misscore a total miss as perfect recall.
- No MCP wire-surface change, no ADR-023 amendment, no `AGENTS.md` churn — the entire
  runner ships behind `Visibility::Subhandler`.
- The public repository never carries real corpus-derived query/answer pairs; the
  path-override on `query_set` gives operators a private-data path without a repository
  fork.

### Negative

- `retrieval_eval_coverage` does not refresh itself. It reports whatever the last
  `kkernel exec 'knowledge.eval_retrieval(...)'` run recorded; an operator (or a future
  automation) must trigger runs for the number to move. No daemon or scheduling
  mechanism is introduced here.
- Slice-1 measures atom-level binary hit/miss, not the full graded five-level relevance
  taxonomy `RetrievalLabel` supports. A future section-level retrieval evaluation (were
  `knowledge.search` to return section-level hits) could use the taxonomy's full
  expressiveness; that is out of scope here.
- Agents cannot self-trigger an eval run or read anything beyond the last-run summary
  fields on `stats()`. If agent-driven evaluation becomes a real workflow, that is a
  distinct, later ADR-023 amendment (§D2), not a consequence absorbed by this one.

### Neutral

- `KNOWLEDGE_HANDLERS` grows from 19 to 20 entries; the MCP-visible verb count (19) is
  unchanged.
- Feedback-grown query-set expansion (issue #80's optional bullet) remains fully
  unaddressed pending its own design and ADR.
- The synthetic fixture and the private-path override are independent of each other;
  operators may run both against the same `knowledge.eval_retrieval` call pattern with
  different `query_set` values.

## Alternatives Considered

| Alternative                                                                                   | Why rejected                                                                                                                                                                                                                          |
| --------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| MCP-exposed `knowledge.eval` verb now (Fork 1, Option A)                                      | No demonstrated agent-side demand for triggering evaluation mid-session; would force an immediate ADR-023 amendment and `AGENTS.md` verb-count sync ahead of any measured need. Ruled against (D2); revisit only if usage demands it. |
| Redefine or rename `eval_coverage` to mean retrieval precision (Fork 3, Option A/rename path) | Breaking change to a legitimate, already-correct finalization metric; breaks the existing `integration.rs:1125` assertion; conflates two orthogonal signals in one field name. Ruled against (D1).                                    |
| Fold evaluation into `knowledge.index` behind an `--eval` flag (Fork 1, Option C)             | Overloads a verb whose contract is embedding backfill with unrelated evaluation semantics; harder to test the two concerns in isolation.                                                                                              |
| Async auto-refresh of retrieval-quality on a schedule or post-index hook (Fork 3, Option C)   | Couples eval cadence to indexing/daemon lifecycle; real scope increase (daemon changes) with no corresponding need established for slice-1. Deferred, not rejected outright.                                                          |
| Feedback-grown query set from `brain.feedback` signals now (Fork 4)                           | Requires a new accumulation table, a labeling contract for `knowledge.search` callers, and its own OSS/private data-policy enforcement — a distinct feature with its own design surface. Deferred to a future ADR.                    |

## References

- GitHub #80 — retrieval quality measurement loop
- `.khive/workspaces/20260626/issue80-scoping/SCOPING.md` — design forks and escalations this ADR resolves
- [ADR-015](ADR-015-schema-migrations.md) — schema migration mechanism; append-only `VersionedMigration` convention used by §"Schema"
- [ADR-023](ADR-023-declarative-pack-format.md) — `Visibility::{Verb, Subhandler}`; kkernel `exec` CLI verb-DSL; the mechanism D2 relies on
- [ADR-047](ADR-047-knowledge-pack.md) — Knowledge Pack; `knowledge.stats`, `knowledge.search`, and the 19-verb baseline this ADR extends
- `crates/khive-retrieval/src/eval/engine_eval.rs` — existing label-based retrieval eval library (`RetrievalLabel`, `LabeledResult`, `recall_at_k`, `precision_at_k`, `ndcg_at_k`, `mrr`, `compute_all`); referenced for contrast in §"Eval runner mechanics" — not called by the runner
- `crates/khive-pack-knowledge/src/knowledge/crud.rs` — `stats()` (lines 604-680), the `eval_coverage` computation this ADR leaves unchanged
- `crates/khive-pack-knowledge/src/knowledge/search.rs` — atom search result shape (`slug` field) consumed by the eval runner
- `crates/khive-pack-knowledge/tests/integration.rs` — existing `eval_coverage` assertion (line 1125) and domain-vocabulary fixture corpus (lines 1216-1225)
