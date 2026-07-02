# ADR-082: Retrieval Quality Measurement Loop

**Status**: proposed
**Date**: 2026-07-02
**Authors**: Ocean, lambda:khive
**Depends on**: [ADR-015](ADR-015-schema-migrations.md) (Schema Migrations), [ADR-023](ADR-023-declarative-pack-format.md) (Pack Verb Surface, Visibility, and Composition — `Visibility::Subhandler`), [ADR-047](ADR-047-knowledge-pack.md) (Knowledge Pack)
**GitHub**: #80

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
evaluation verb reporting hit-rate/MRR, optional feedback-grown set expansion) can be
built almost entirely on this existing library — the missing piece is plumbing, not
retrieval math.

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
runner that reuses the existing evaluation library, hit-rate/MRR persisted per run, and
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
kkernel exec 'knowledge.eval_retrieval(query_set="crates/khive-pack-knowledge/tests/fixtures/eval_set.toml", k=5)'
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
min_k = 5

[[queries]]
query = "retrieval augmented generation retrieving passages before generation"
expected_slugs = ["rag"]
```

`min_k` is optional (defaults to the runner's `k` parameter, §6). A query may name more
than one `expected_slugs` entry when multiple atoms legitimately satisfy it.

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

`knowledge.eval_retrieval(query_set: <path>, k: usize = 5, namespace: <string>?)`:

1. Parse the TOML query set at `query_set`.
2. For each query, invoke the existing `knowledge.search` runtime path with
   `limit = max(k, min_k)`, collecting the returned atom slugs in rank order (`search`
   already returns a `slug` field per hit —
   `crates/khive-pack-knowledge/src/knowledge/search.rs`).
3. Map each ranked slug to a `khive_retrieval::eval::LabeledResult`: `label =
   RetrievalLabel::Decisive` when the slug is in `expected_slugs`, else
   `RetrievalLabel::Irrelevant`. `section_id` is populated with a placeholder UUID — it is
   unread by `recall_at_k`, `precision_at_k`, and `mrr`, which key only on `.label`. This
   is a deliberate, minimal use of the existing five-level taxonomy: atom-level retrieval
   is being scored as a binary hit/miss, not the richer graded relevance that
   `RetrievalLabel` supports for section-level retrieval. No new evaluation math is
   written; `recall_at_k` and `mrr` from `khive_retrieval::eval::engine_eval` are called
   unmodified.
4. Aggregate mean `precision_at_5`, `recall_at_5`, and `mrr` across every query in the
   set.
5. Write one row to `knowledge_eval_runs`.
6. Return `{run_id, total_queries, precision_at_5, recall_at_5, mrr}`.

### `knowledge.stats()` reads the last run

`stats()` (`crud.rs:604`) gains one additional `query_scalar` read, following the same
pattern as the existing `finalized_count` query at `crud.rs:631-638`: select the most
recent `knowledge_eval_runs` row for the namespace (`ORDER BY run_at DESC LIMIT 1`) and
emit the four `retrieval_eval_*` fields shown in D1. No run for the namespace yet
produces the zero/null sentinel row described above.

## Consequences

### Positive

- Retrieval quality moves from "never measured" to an honest, run-triggered,
  namespace-scoped signal, closing the actual gap issue #80 identifies.
- Zero risk to the existing `eval_coverage` consumer or its integration test — the
  change is purely additive at the JSON level and the SQL schema level.
- No new evaluation math: `recall_at_k` and `mrr` are reused unmodified from an already
  tested library.
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
- `crates/khive-retrieval/src/eval/engine_eval.rs` — `RetrievalLabel`, `LabeledResult`, `recall_at_k`, `precision_at_k`, `ndcg_at_k`, `mrr`, `compute_all`
- `crates/khive-pack-knowledge/src/knowledge/crud.rs` — `stats()` (lines 604-680), the `eval_coverage` computation this ADR leaves unchanged
- `crates/khive-pack-knowledge/src/knowledge/search.rs` — atom search result shape (`slug` field) consumed by the eval runner
- `crates/khive-pack-knowledge/tests/integration.rs` — existing `eval_coverage` assertion (line 1125) and domain-vocabulary fixture corpus (lines 1216-1225)
