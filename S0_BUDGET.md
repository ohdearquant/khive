# S0 — khive verb-level latency budget (Amdahl gate for opt-funnel S1)

Produced: 2026-07-05. Worktree: `khive-opt-s0` @ `481346bf`. Purpose: establish where
request time actually goes per verb, so S1 optimization candidates are scoped to a
measured ceiling instead of intuition. **No optimization changes were made in this
task — measurement only.**

## Measurement validity — read this before trusting any absolute number below

This run did **not** get an idle machine, despite that being the task's explicit
precondition. A co-resident `kkernel mcp` daemon from a separate, concurrently-running
lambda session held 400-444% CPU (12-core box) for the entire session; 1-minute load
average ranged **18–31** during measurement (`uptime` samples throughout: 17.25,
31.57, 18.26→6.24 at the very end as the other daemon's burst subsided). This is not
something this task could fix — killing another lambda's live process is a scope
violation (CLAUDE.md resource-governance rule: each λ manages only its own worktree/
processes), so the numbers below were captured as-is rather than blocked on an idle
window that may not exist on this shared machine.

**Consequence**: absolute latencies below are inflated by contention, and at least one
number (`knowledge.search` "warm" p50 ≈ 5.03s, see below) is almost certainly a
contention artifact rather than a real steady-state cost — it does not scale
sensibly against its own `cold_first_query_us` figure (47.2ms) from the same run.
**Relative ordering across verbs is probably still informative** (traverse depth-3 >
depth-2 > neighbors > FTS search reflects real algorithmic scaling, not just noise),
but nobody should treat an absolute p50/p95 here as a hard ceiling until re-measured
on a genuinely idle box. This caveat applies to every row.

## Second scope reduction — real-corpus (production-scale) measurement was abandoned

The task asked for measurement against either a copy of production `~/.khive/khive.db`
(~3.7GB, 3226 entities visible in that DB's own `entities` table — the bulk of its
23,860 notes / 12,250 edges live in the separate `~/.khive/khive-graph.db`, 2,746
entities / 12,383 notes / 8,130 edges there) or a synthetic corpus at ≥100K/500K/50K
scale. Disk headroom on `/System/Volumes/Data` was **31GB free against a 926GB volume
(97% used)** — well under the CLAUDE.md-mandated 100GB floor — for the entire session.

A `sqlite3 .backup` snapshot of `khive.db` to `/tmp/khive-s0-bench/real-corpus.db` was
taken (2.9GB, `PRAGMA quick_check` returned `ok`), but **the file was silently
truncated to 0 bytes by something external between two consecutive checks**, a few
minutes after the backup completed, with no process of mine touching it. The most
plausible explanation is an automated disk-pressure sweep on this heavily-loaded,
near-floor-capacity shared machine (consistent with the CLAUDE.md 100GB-floor
directive that Leo/the fleet enforces). Per this task's own fallback instruction
("if the backup failed... proceed with synthetic-at-scale results"), the real-corpus
backup was **not retried** — repeating a 3GB copy under the same disk pressure risked
the same outcome and further strained a machine already below its safety floor. This
is a genuine open item for S1: production-scale numbers (94K knowledge atoms, 358K
sections, 121K MiniLM vectors, per live `khive.db`) still need a real measurement
pass, ideally scheduled when disk headroom and machine load are both healthy.

In place of the abandoned real-corpus snapshot, two paths cover the embedding/ANN/FTS
real-corpus-adjacent costs without needing a large disk copy:
- **Structural graph verbs** (neighbors/traverse/search-FTS-leg/create): measured
  end-to-end against a **freshly built synthetic 100,001-entity / 500,000-edge /
  50,001-note** SQLite DB (direct-SQL bulk insert after one real `create()` call
  bootstraps the namespace's FTS5 shadow tables/triggers — see
  `scripts/perf/gen_synthetic_kg_s0.py`), driven through the real `kkernel` binary
  over MCP stdio (`scripts/perf/bench_verb_funnel_s0.py`, in-process /
  `KHIVE_NO_DAEMON=1` so `KHIVE_RECALL_PROFILE`/`KHIVE_CONTEXT_PROFILE` phase logs
  land on the harness's own stderr pipe). No embeddings in this DB — it isolates
  pure graph/FTS cost from embedding cost.
- **Real-embedding-path costs** (MiniLM inference, ANN, decay-weighted recall,
  TF-IDF+rerank fusion): taken from the **existing in-repo criterion benches**
  (`khive-pack-memory/benches/memory_bench.rs`, `khive-pack-knowledge/benches/
  search_latency.rs`), which build their own small-but-real (10–500 item) in-memory
  corpora with the production embedder — no external DB needed, so no disk risk.
  These do not reach production scale (94K atoms) but exercise the real code path
  end-to-end (embed → ANN/HNSW → fusion → score), which the disk-free structural
  corpus above cannot.

## Budget table

| Verb | p50 | p95 | Corpus | Measurement path | Phase split | Session-share* |
|---|---|---|---|---|---|---|
| `stats` (baseline) | 1318 ms | 3318 ms | synth 100K/500K/50K | real binary, stdio, in-process | — | n/a (subtracted as noise floor; see caveat — this number is itself contention, not real `stats()` cost, which is a handful of `COUNT(*)` queries) |
| `search` kind=entity (FTS-only leg) | 4.0 ms | 8.5 ms | synth 100K entities, no embeddings | real binary, stdio, in-process | not split (no vector leg present in this corpus) | **high** — search dominates real sessions |
| `search` kind=note (FTS-only leg) | 4.4 ms | 6.3 ms | synth 50K notes, no embeddings | real binary, stdio, in-process | not split | **high** |
| `memory.recall` (hybrid, real embedder) | 0.68–1.7 ms (n=10/100/500) | up to 2.2 ms | in-memory, 10/100/500 memories, criterion `memory_bench.rs` | criterion, in-process runtime (not MCP stdio) | not split | **high** — recall is the single most frequent verb in a real session |
| `memory.recall` with `min_score` filter | 4.2 ms | 5.3 ms | in-memory, criterion `memory_bench.rs` | criterion, in-process | not split | high (variant of above) |
| `memory.remember` (baseline, triggers embed+index) | 5.9–6.8 ms | 8.1 ms | in-memory, criterion `memory_bench.rs` | criterion, in-process | not split (embed+FTS+vec-insert bundled) | medium |
| `memory.remember` with source annotation | 1.6–2.7 ms | 3.8 ms | in-memory, criterion `memory_bench.rs` | criterion, in-process | not split | low-medium |
| `knowledge.search` cold first query (embed model load + rerank) | 47.2 ms | — (single sample) | 100 atoms, criterion `search_latency.rs`, real embedder | criterion, in-process | includes one-time model-load cost | low (paid once per process lifetime, not per-call in a live daemon) |
| `knowledge.search` warm (rerank on/off) | **~5.03 s — DISCARD, see validity note** | ~5.06 s | 100 atoms, criterion `search_latency.rs` | criterion, in-process | not split | unusable this run — needs re-measurement on an idle box before any conclusion is drawn |
| `neighbors` direction=both (hub, degree 1580) | 273 ms | 346 ms | synth, 100K entities / 500K edges | real binary, stdio, in-process | not split (single storage-layer BFS-1 query) | medium — used for graph-context expansion, not typically standalone in a session |
| `traverse` max_depth=2 | 1274 ms | 4076 ms | synth, same corpus | real binary, stdio, in-process | not split | low-medium — occasional, not per-turn |
| `traverse` max_depth=3 | 3183 ms | 6174 ms | synth, same corpus | real binary, stdio, in-process | not split | low — rare, deep exploration only |
| `context` (ADR-089, query+entity_id anchored) | 143.8 ms | 277.5 ms | synth, same corpus | real binary, stdio, in-process; `KHIVE_CONTEXT_PROFILE=1` phase capture | **expand** 72.3 ms p50 / 180.0 ms p95 (dominant — `neighbors_with_query` BFS), **entity_fetch** 37.0 ms p50 / 107.2 ms p95, **anchor_search** 2.2 ms p50 / 6.4 ms p95, **anchor_ids** 0.66 ms p50 / 2.6 ms p95, **assembly** 0.03 ms | medium — this is the verb ADR-089 shipped specifically to replace multi-call graph exploration, so its cost matters disproportionately to session-latency perception |
| `create` entity | 9.8 ms | 13.3 ms | synth corpus (write path) | real binary, stdio, in-process | not split (entity insert + FTS trigger sync) | low — occasional in a session |
| `create` note | 4.0 ms | 6.9 ms | synth corpus | real binary, stdio, in-process | not split | low |
| `comm.send` / `comm.inbox` | **not measured this run** | — | — | — | — | low-medium — battery included `comm.inbox` in the real-corpus path, which was abandoned; re-run needed |
| `knowledge.compose` (auto path) | **not measured this run** | — | — | — | — | low — occasional, budget-bounded by design |

*Session-share is a qualitative call, not derived from a frequency-weighted formula —
this task did not have real session-trace frequency data to weight against. Per the
khive CLAUDE.md usage patterns (`recall`/`search` dominate, `ingest` is occasional),
`memory.recall`, `search`, and `context` are the verbs that would move the needle on
perceived agent-loop latency; `traverse`/`create` are much rarer per-turn.

## Corpus provenance

| Corpus | Entities | Edges | Notes | Embeddings | Built by |
|---|---|---|---|---|---|
| Synthetic structural | 100,001 | 500,000 | 50,001 | none (plain text, no vector table populated) | `scripts/perf/gen_synthetic_kg_s0.py` — one real `create()` call to bootstrap per-namespace FTS5 tables/triggers, then bulk `executemany` raw SQL inserts (hub-biased edge distribution: 30% of edges sourced from a 100-node hub set, to give `neighbors`/`traverse` a realistic large-fan-out target) |
| criterion `memory_bench.rs` | n/a | n/a | 10 / 100 / 500 memories | real MiniLM embedder | in-memory `KhiveRuntime`, seeded inline by the bench |
| criterion `search_latency.rs` | n/a (100 knowledge atoms) | n/a | n/a | real MiniLM embedder | in-memory `KhiveRuntime`, seeded inline by the bench |
| Production `khive.db` (reference only, not measured) | 3,226 | — | 23,860 (13,233 memory / 5,380 message / 2,562 task / …) | 121,111 rows in `vec_all_minilm_l6_v2` | live system, read-only inspection via `sqlite3` only |
| Production `khive-graph.db` (reference only) | 2,746 | 8,130 | 12,383 | — | live system, read-only inspection only |
| Production knowledge corpus (reference only) | — | — | 94,174 atoms / 358,336 sections | — | live system, read-only inspection only |

## Load average during measurement

`uptime` samples across the session: **17.25 → 50.58 (5m) → 31.57 → 18.26 → 6.24**
(1-minute figures where isolated). Never below ~6 even at the tail end. The dominant
external load was PID 50205, `/Users/lion/.cargo/bin/kkernel mcp` at 400-444% CPU —
a live daemon serving a different concurrent lambda session, out of this task's
scope to stop.

## Three biggest shares + hypothesis (feeds S1)

1. **`context`'s `expand` phase (72ms p50 / 180ms p95 of a 144–278ms total call)** —
   hypothesis: `neighbors_with_query`'s BFS re-issues separate Out/In storage calls
   per direction when `direction=both` (see the doc comment in
   `crates/khive-pack-kg/src/handlers/context.rs`), so a hub-degree node pays for two
   full index scans instead of one `UNION ALL` — this is the most actionable, most
   surgical target for S1 given it's isolated to a named phase in a verb that ships
   specifically to reduce agent round-trips.
2. **`traverse` scaling depth-2 → depth-3 (1274ms → 3183ms p50, roughly 2.5×, not the
   ~10× naive branching-factor growth would suggest, but still the single largest
   absolute-latency verb measured)** — hypothesis: BFS frontier expansion cost is
   dominated by the same per-direction double-query pattern as `neighbors`/`context`,
   compounding across hops; likely shares a fix with finding #1 since `traverse` is
   built on the same `neighbors`-family primitive.
3. **FTS-only `search` (4.0–4.4ms) vs. `memory.recall` hybrid (0.7–1.7ms in-memory,
   but 4.2–5.3ms with `min_score` filtering)** — hypothesis: these are already cheap
   in absolute terms relative to `context`/`traverse`/`neighbors`, but they are the
   **highest-frequency** verb in a real session per the CLAUDE.md usage-pattern
   note ("recall/search dominate"), so a small per-call win here compounds more than
   a larger win on a rare `traverse` call — S1 should weigh frequency × latency, not
   latency alone, when picking a target.

## Open items for S1

- Real-scale (94K atom / 121K vector) `knowledge.search`/`knowledge.compose` and
  `memory.recall` numbers are still missing — needs a disk-healthy, load-healthy
  window to snapshot `khive.db` safely.
- `comm.send`/`comm.inbox` and `knowledge.compose` (auto path) were scoped into the
  battery design but never executed this run (battery only ran the synth-mode
  subset once the real-corpus path was abandoned) — straightforward to add to
  `scripts/perf/bench_verb_funnel_s0.py`'s synth-mode battery for a re-run.
- The `knowledge.search` warm-path 5.03s figure must be re-measured before it
  influences any decision; as recorded it looks like contention, not signal.
- A genuinely idle-machine re-run of the full battery (both harnesses) is the
  single highest-value next step — every absolute number here carries an unknown
  contention multiplier until that happens.
