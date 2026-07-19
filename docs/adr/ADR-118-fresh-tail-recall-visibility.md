# ADR-118: Fresh-Tail Exact Leg — Read-Your-Writes Visibility for Vector Recall

**Status**: Proposed
**Date**: 2026-07-19
**Depends on**: [ADR-021](ADR-021-memory-pack.md) (Memory Pack),
[ADR-079](ADR-079-ann-persistence-warm-path-integration.md) (ANN persistence, Amendment 1
delta-log/watermark classifier), [ADR-107](ADR-107-memory-ann-lifecycle.md) (Memory ANN
lifecycle — eventual consistency contract)
**References**: issue #1143 (write-to-visibility regression), issue #752 (process-local
generation counter), issue #942 (fresh-query scoring quality — related, out of scope)

## Context

`memory.remember` is fully synchronous: the note row, its FTS document, and one vector row
per registered embedding model are committed before the verb returns. Yet a freshly written
memory is invisible to `memory.recall` for tens of seconds (measured: absent at +0s, ranked
first at +33s on a ~68k-note corpus). Two mechanisms combine to produce this (issue #1143):

1. **The vector legs serve ANN-only.** Each per-model vector source in recall's fusion draws
   candidates exclusively from the warm Vamana index. A committed vector row enters that index
   only when a background rebuild or tail replay completes — O(corpus) work whose latency is
   the ADR-107 §1 stale bound. Until then the row does not exist for the vector legs.
2. **Rank fusion structurally buries single-source hits.** The FTS trigram leg does see the
   fresh row immediately (its index updates in the write transaction). But under RRF with
   `k = 60` over three sources, a hit at rank 1 in one source alone scores `1/61 ≈ 0.0164`,
   below any candidate that appears at moderate ranks in all three sources. The fresh note is
   fetched and then buried below the returned window.

Before the Vamana segment architecture, the in-process index accepted online inserts and
writes were visible to recall in milliseconds. The current behavior is a real regression of
the recall contract, not a tuning artifact, and it also has a cross-process form: an external
writer (`kkernel --atomic`, `kkernel reindex`) cannot invalidate a warm daemon's index at all
(issue #752), so its writes stay invisible until an unrelated rebuild happens to run.

ADR-107 codified the relaxation to eventual consistency as a deliberate trade for never
blocking recall on a rebuild. That trade conflated two things that this ADR separates:
**serving from a stale graph** (acceptable — the graph is a candidate generator) and
**failing to surface committed rows at all** (not acceptable — it breaks read-your-writes
for the primary write-then-recall agent workflow).

ADR-079 Amendment 1 introduced the piece that makes a cheap fix possible: `ann_write_log`, a
transactional per-write delta log, and a durable per-consumer watermark identifying exactly
which log prefix the serving segment reflects. The set of committed-but-unindexed rows — the
**tail** — is therefore precisely enumerable at query time with a log-table read.

## Decision

Each per-model vector leg of recall is augmented with a **fresh-tail exact leg**: an exact
similarity scan over the vector rows in the leg's scope whose `ann_write_log` seq is above
the serving index's watermark. Tail hits are merged into that model's existing candidate
list **before** fusion. The guarantee is two-tier: while a serving index exists, recall is
read-your-writes for every committed write; while none exists (Cold rebuild in flight, or
Empty), recall guarantees visibility of the newest threshold-sized window of writes — which
by construction contains the caller's most recent writes — with full-corpus visibility
returning when the rebuild lands (§3). The per-query cost is proportional to the tail
length, which the checkpoint lifecycle already bounds.

### 1. Tail enumeration

For a leg serving model `m` with watermark `S` (the `ann_write_log` seq stamped on the
serving bridge at build/adoption time; `S = 0` when no index is serving):

- Read `ann_write_log` rows for `m` in the leg's scope with `seq > S` and coalesce to the
  final op per `subject_id` (the same final-state semantics as ADR-079 tail replay — only
  the highest-seq op per subject counts).
- Subjects whose final op is `upsert`: point-read the current embedding row under the
  consumer's corpus predicate and score it exactly (full-precision similarity against the
  query vector).
- Subjects whose final op is `delete`: the vector row is gone; the exact leg contributes
  nothing for them. If the same subject is also returned by the ANN index, it is dropped
  from the merged candidate list — the tail is authoritative for every subject it names.

The read runs in a single snapshot (log scan + row point-reads in one read transaction), so
a write committing mid-query is either entirely visible or entirely invisible — never a torn
log/row pair.

_Compaction linearization (review follow-up, 2026-07-19)._ A tail scan above `S` proves
completeness only if the log still retains every row above `S` — and ADR-079 permits
compaction through the registry minimum, which can advance past a stale in-memory bridge's
`S` the moment the consumer's durable watermark is raised for a newer segment. The leg must
therefore validate coverage **in the same read snapshot**: read the pair's wildcard-inclusive
registry minimum alongside the log scan; if that minimum exceeds `S`, the snapshot cannot
prove tail completeness for the bridge in hand, and the leg must re-resolve the currently
published segment (whose watermark is at least the minimum) and use its watermark instead —
or, if re-resolution is not possible within the query, serve without the exact leg for that
query (degrading to the ADR-107 contract) while triggering re-adoption. Implementations
SHOULD additionally order same-process checkpoint publication so the in-process bridge is
replaced before the durable watermark is raised, making the mismatch window empty for the
process's own checkpoints; the snapshot check remains mandatory because another process can
raise and compact independently.

_Registration precondition (review follow-up, 2026-07-19)._ `S = 0` establishes an
entire-scope tail only if no compaction has ever run for the pair — which ADR-079 guarantees
for an _unregistered pair_, not an unregistered _consumer_: a registered peer consumer on the
same `(namespace, model)` pair legitimately compacts rows the unregistered consumer never
saw. The exact leg is therefore permitted only after the consumer's durable registration row
exists. ADR-079's "register before persist" rule is extended for tail consumers: register
before any **tail-dependent read path**, not merely before the first segment persist. A
consumer that finds its registration row absent must register at 0 and treat the log as
untrusted until rows accumulate under the new registration (the existing Cold classification
already produces the correct serving behavior for that window).

### 2. Merge semantics — one source, not a fourth

Tail hits are merged into the model's vector candidate list, deduplicated by `subject_id`
with the tail winning (its embedding is at least as fresh as the segment's), and the merged
list is re-sorted by score before it enters fusion. The fusion configuration is unchanged:
same source count, same RRF `k`, same weights.

This placement is load-bearing. Adding the tail as a new fusion source would change ranking
semantics for every query (RRF scores depend on source count) and would re-create the
single-source burial problem for fresh hits. Merged into the vector leg, a fresh note that
matches the query well appears at its natural rank in that leg — and if it also matches
lexically, the FTS leg corroborates it exactly as it would for an indexed note.

Mixed precision is accepted: exact hits carry full-precision similarity while ANN hits carry
the index's quantized approximation. Both orderings are consumed as ranks by fusion; the
within-leg orderings are individually correct, which is all rank fusion requires. The named
accepted risk (gate ruling, 2026-07-19): interleaving full-precision and quantized scores in
one re-sorted list means quantization bias can systematically misorder fresh-vs-indexed hits
near ties. This is bounded and accepted; if it shows up in practice, the remedy is
full-precision re-scoring of the ANN candidates within the merge window — explicitly a
scoring refinement inside the leg, never a fusion change.

### 3. Cost bound and the no-index case

The exact leg is O(|tail| × dims) per model per query. The tail is small by construction:

- The checkpoint lifecycle (ADR-079 §"checkpoint") drains the tail on a cadence; the
  steady-state tail is the writes since the last checkpoint.
- The rebuild threshold (`KHIVE_ANN_REBUILD_THRESHOLD`, default 0.20) caps how long a tail
  can grow relative to the live corpus before a full rebuild is already in flight. A tail at
  the threshold on a 68k-row corpus is ~13.6k exact comparisons. The similarity arithmetic at
  that ceiling is sub-millisecond, but the dominant cost there is the ~13.6k embedding-row
  point-reads per model, which is not — the honest bound at the pathological ceiling is the
  point-read I/O, and it applies only while a full rebuild is already in flight. The
  steady-state tail (writes since the last checkpoint) is small enough that both terms are
  negligible, and the steady state is the case the leg exists for.

When no ANN index is serving at all (Cold rebuild in flight, or Empty classification), the
watermark is 0 and the tail is the entire scope. The exact leg does **not** absorb that case:
it caps its scan at the threshold-sized tail, taking the highest-seq suffix of the log (the
newest writes) so the freshest writes stay visible while FTS covers the rest, and otherwise
defers to the existing Cold-path behavior (FTS-only serving while the rebuild runs). The leg
restores freshness on a serving index; it is not a general exact-search fallback. This is
the second tier of the §"Decision" guarantee stated precisely: with no serving index, a
committed write older than the capped suffix may be invisible to the vector legs until the
rebuild lands — the read-your-writes property in that state covers the newest
threshold-sized window, not the entire corpus. The regression this ADR fixes (#1143) lives
entirely in the first tier; the Cold window is the pre-existing ADR-107 behavior, narrowed
by the guaranteed-fresh suffix rather than contradicted by it.

No new tuning knob is introduced. One escape hatch is added for operational isolation:
`KHIVE_ANN_FRESH_TAIL=0` disables the exact leg (default enabled). Values other than `0`
are ignored.

### 4. Scope: both delta-log consumers

The mechanism is normative for both consumers of the ADR-079 Amendment 1 classifier:

- **memory pack** (global-scope note index) — the regression's primary surface; lands first.
- **knowledge pack** (per-namespace index) — same architecture, same fix; may land in a
  follow-up PR, but the contract applies to it from acceptance.

Any future consumer of the delta-log/watermark lifecycle inherits this contract: a serving
path that draws candidates from a watermarked index MUST merge the tail above that watermark
or document why staleness is acceptable for its surface.

### 5. Cross-process visibility

Because the exact leg is a query-time database read, it is indifferent to which process
performed the write and to whether the serving process's warm cache was invalidated. An
external writer that appends log rows (the Amendment 1 write-path rule binds every write
path) becomes visible to a warm daemon's recall on the daemon's next query. This closes the
visibility half of issue #752; the invalidation half (getting the index itself rebuilt) is
unchanged and remains that issue's subject.

## Relationship to ADR-107 (partial supersession)

ADR-107 §1 ("Stale bound") is **narrowed, not retired**. Its bound — recall may serve from
an index behind the caller's latest write by up to the causal rebuild latency — continues to
govern the ANN **candidate generator**: graph adjacency, quantized codes, and the candidate
pool for older corpus remain eventually consistent exactly as specified. What the bound no
longer governs is **result visibility while a serving index exists**: there, a committed
write is eligible for recall results on the next query, independent of rebuild progress.
In the no-index states (Cold, Empty) the §3 second tier applies — guaranteed visibility of
the newest threshold-sized suffix, ADR-107 behavior for anything older — so ADR-107 §1
continues to describe the verb's observable freshness in exactly and only those states.
Statements in ADR-107 §1 that recall "may serve results computed from an ANN index that is
behind the caller's own most recent write" remain true of the index and cease to describe
the verb's observable freshness whenever an index is serving.

ADR-107 §2 (generation bump, non-eviction), §3 (Cold), §4 (epoch invalidation), and §5
(deletion filtering) are unchanged. Deletion filtering in particular remains the correctness
backstop for every candidate the exact leg does not cover (for example join-predicate soft
deletes that write no log rows — see ADR-079's rule 5 qualification).

## Consequences

- Write-then-recall returns to millisecond visibility whenever a serving index exists —
  the overwhelmingly common state — matching the pre-Vamana behavior and the synchronous
  write path's implicit promise; the Cold/Empty window keeps the newest-suffix guarantee
  only (§3).
- Recall pays a small per-query cost (log-tail scan + registry-minimum coverage check +
  exact scoring) that is near zero when the tail is empty and bounded by the rebuild
  threshold when it is not.
- The tail query adds read traffic on `ann_write_log`; its index must serve
  `(embedding_model, seq)`-shaped scans efficiently (the existing namespace-first index
  shape is a known follow-up).
- Fusion behavior for fully indexed corpora is byte-identical: with an empty tail the merged
  list equals the ANN list.
- Testing obligation: a regression test writes a memory and asserts recall surfaces it in the
  same process without waiting for a rebuild, and a cross-process variant covers the external
  writer path.

## Alternatives considered

- **Fourth RRF source for the tail.** Rejected: changes ranking semantics for all queries and
  re-creates single-source burial for exactly the hits the fix targets (§2).
- **Online insert into the serving Vamana index.** Rejected: the serving index is an mmap
  segment; insertion promotes it to an owned full-corpus copy (the O(corpus) memory pattern
  this architecture exists to avoid), and concurrent graph mutation under serving reads
  requires locking the hot path.
- **Synchronous tail replay on write.** Rejected: re-introduces the write-blocks-on-index
  coupling that ADR-107 removed for issue #791.
- **Lower RRF k or boost single-source hits.** Orthogonal calibration with global ranking
  consequences (issue #942 territory); does not restore visibility for vector legs and is
  not pursued here.
