# ADR-144: Operation-Level Write-Visibility Fence for Memory Recall

- Status: Proposed
- Date: 2026-08-05
- Related: ADR-118 (fresh exact tail), #1084 (per-hit route labels, declined shape),
  #1161 (Cold/Empty cap divergence, separate lane)

## Context

ADR-118 gives recall a fresh exact tail: rows written after the last ANN segment
publication are scored exactly and merged with ANN candidates into a single
model source before fusion. That merge is deliberately silent — a result list
can contain both ANN-served and exactly-scored hits with no per-hit marker, and
nothing in the response distinguishes them.

What the surface does not offer is any way for a caller to *prove* that a
specific write is visible to a subsequent recall:

- `memory.remember` returns `id`, `kind`, `salience`, `decay_factor`,
  `memory_type`, `created_at`, and optionally `edge_id`. It returns no log
  sequence, generation, or visibility state for the vectors it wrote.
- `memory.recall` accepts no consistency parameter. Its only inspected
  degradation field is `ann_unavailable`.
- `VectorSearchHit` is `subject_id`, `score`, `rank`; `ScoreBreakdown` exposes
  ranking components, not retrieval origin.

Mature vector and search systems place their strongest freshness control at the
write/request consistency boundary rather than in per-hit provenance:
Elasticsearch `refresh=wait_for` blocks the write acknowledgement until the
change is search-visible; Lucene exposes an index generation callers can wait
on; Milvus Session consistency maps the client's latest write timestamp into
the read guarantee; Qdrant `wait=true` returns a write only once it is applied
and searchable; LanceDB searches unindexed rows by brute force unless the
caller opts out with `fast_search=True`. The mechanisms differ, but each makes
the freshness/latency trade visible at operation or query scope.

The previously proposed alternative (#1084) — a per-model `ann | exact` route
label on responses — cannot express ADR-118's design: one model source may
legitimately contain both origins, and a route summary cannot prove that one
particular write is covered. Origin labels are diagnostics, not a correctness
primitive.

## Decision (proposed — the fork below is the sign-off question)

Add an additive, operation-scoped visibility contract to the memory pack:

1. **Write receipt.** `memory.remember` additionally returns a
   `visibility_token`: the namespace plus one `{model, ann_write_log_seq}`
   fence per vector written. Existing fields are unchanged; callers that
   ignore the token see today's behavior.

2. **Read fence.** `memory.recall` accepts an optional
   `consistency: "eventual" | "session"` parameter together with either a
   previously returned `visibility_token` or an equivalent `after` fence.
   - `eventual` (default) is today's behavior, unchanged.
   - `session` succeeds only when every requested model proves coverage of the
     fence by `segment_watermark ∪ exact_tail_snapshot`. When coverage cannot
     be proven it waits up to a caller-provided timeout, then returns a typed
     `freshness_unmet` result. It never silently serves an uncovered state.

3. **Diagnostics stay diagnostics.** Verbose responses may report per-hit
   origin and per-model watermarks, but no correctness claim rides on them.

### The fork requiring sign-off

- **Arm A (this proposal):** the receipt/fence contract above.
- **Arm B (explicit null):** formally decide that `memory.remember`
  acknowledges storage durability only, that search visibility is best-effort,
  and document that contract at the verb surface. This is a legitimate product
  decision; today's state is Arm B in behavior but undocumented, which is the
  actual defect. Accepting either arm closes it; leaving the surface silent
  does not.

## Alternatives considered

- **Per-hit origin labels (#1084 shape).** Rejected as the primitive: cannot
  prove visibility of a specific write, and ADR-118's merged model source makes
  the label set (`ann | exact`) incomplete. Retained only as optional
  diagnostics under this design.
- **Force-visible writes** (Elasticsearch `refresh=true` analog: every
  `remember` blocks until searchable). Rejected: taxes every write with
  worst-case publication latency to serve the minority of callers that need a
  fence, and removes the caller's ability to choose.
- **Global read-your-writes session state held server-side.** Rejected for the
  additive stage: khive's callers span processes and namespaces; an explicit
  token keeps the fence self-describing, replayable across processes, and free
  of server session affinity.

## Consequences

- Callers that need read-your-writes get a provable, bounded-wait contract;
  callers that do not pay nothing.
- The fence is per-model, so a mixed-model recall degrades precisely: a
  `freshness_unmet` result names the models that failed coverage.
- The exact-tail snapshot participates in coverage proofs, so under normal
  operation a `session` recall issued immediately after `remember` succeeds
  without waiting for segment publication — the tail already covers the fence.
- `freshness_unmet` is a new typed degradation state and must follow the
  established degradation-marking rules (flag says whether, log says why).
- Implementation risk concentrates in the coverage proof
  (`segment_watermark ∪ exact_tail_snapshot` per model, one snapshot); it must
  be read from one consistent snapshot to avoid proving coverage with two
  clocks.

## Out of scope

- The Cold/Empty cap divergence (fixed 20,000-row constant vs ADR-118's
  threshold-relative text vs the knowledge pack's corpus-relative
  implementation) is a live divergence tracked in #1161 and belongs to an
  ADR-118 alignment, not this contract.
- Knowledge-pack freshness: the fresh-tail helper landed (#1589) and the pack
  proves no current gap; this ADR adds no knowledge-pack requirement.

## Acceptance

This record is accepted only with an explicit choice of Arm A or Arm B. If
Arm A: implementation follows as tracked work (receipt first, fence second;
both additive). If Arm B: the verb documentation change lands with the
acceptance and this record stands as the decision trail.
