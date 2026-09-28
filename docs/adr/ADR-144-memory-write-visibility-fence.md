# ADR-144: Operation-Level Write-Visibility Fence for Memory Recall

- Status: Accepted
- Decision: Arm A — additive write receipt plus session read fence
- Date: 2026-08-05
- Related: ADR-118 (fresh exact tail), #1084 (per-hit route labels, declined shape),
  #1161 (Cold/Empty cap divergence, separate lane)

## Context

ADR-118 gives recall a fresh exact tail: rows written after the last ANN segment
publication are scored exactly and merged with ANN candidates into a single
model source before fusion. That merge is deliberately silent — a result list
can contain both ANN-served and exactly-scored hits with no per-hit marker, and
nothing in the response distinguishes them.

What the surface does not offer is any way for a caller to _prove_ that a
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

## Decision (accepted: Arm A — the fork below records the sign-off question as posed)

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

### The fork as posed at sign-off (resolved: Arm A)

- **Arm A (accepted):** the receipt/fence contract above.
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

- The Cold/Empty cap belongs to ADR-118, not this contract. Its former
  cross-pack divergence was resolved by #1161: both memory and knowledge use
  the corpus-relative rebuild threshold for the newest log suffix.
- Knowledge-pack freshness: the fresh-tail helper landed (#1589) and the pack
  proves no current gap; this ADR adds no knowledge-pack requirement.

## Acceptance

Accepted with Arm A, under four conditions recorded here as part of the
decision:

1. This record's Status and Decision lines name the accepted arm before the
   record merges.
2. Sequencing is as written: receipt first, fence second, both additive.
   Implementation is tracked follow-up work and does not preempt existing
   scheduled priorities.
3. The one-consistent-snapshot property of the coverage proof (see
   Consequences) is review-blocking at implementation time: a coverage proof
   assembled from two separately read clocks must fail review.
4. The session-fence wait carries a server-side maximum timeout cap;
   caller-provided timeouts bound below that cap and can never request an
   unbounded hold.

## Amendment 1 (2026-09-27): receipt shape and one-snapshot proof

**Status**: Accepted (2026-09-27). This amendment records the owner's rulings
on implementation choices left open by the original decision; it does not
weaken Arm A.

### Receipt and replay

`memory.remember` returns `visibility_token` as a strict JSON object:

```json
{
  "version": 1,
  "namespace": "local",
  "fences": [{ "model": "example-model", "ann_write_log_seq": 123 }]
}
```

The namespace is the actual write namespace. There is exactly one fence per
vector written, with the `ann_write_log.seq` captured in that vector write's
transaction; model entries are unique and sorted by model name for stable
serialization. A write with no registered embedding models returns the same
object with `fences: []`. No token is returned for a rolled-back write.

An exact keyed `memory.remember` replay writes nothing and returns the original
durably stored per-memory, per-model fences. It must not mint a fresh log row
or infer a sequence from a later `MAX(seq)` query. If the original receipt
cannot be recovered, the replay reports `freshness_unmet` rather than claiming
new visibility. The receipt storage and vector-log insertion commit together
for keyed writes.
The receipt records the expected model count so a missing per-model fence
cannot be mistaken for a legitimate zero-model receipt.

### Session input and bounded wait

`memory.recall` defaults to `consistency: "eventual"`, preserving existing
behavior. `consistency: "session"` requires `visibility_token`; a missing token,
malformed shape or version, foreign namespace, duplicate model, or token model
outside the recall's requested model set is `InvalidInput`. A well-formed
fence whose sequence cannot be proven, including a future/unobserved sequence,
returns a typed `freshness_unmet` result naming the failed models; this does
not assert that the token was authentic. A missing keyed replay receipt is
likewise unmet, not a new token. A token with an empty fence list has no
per-model vector coverage obligation.

The caller may supply `timeout_ms`; its default is zero (one immediate proof
attempt). Waiting is bounded by 10,000 ms and must finish strictly before
the request's remaining deadline minus a 2-second margin. The implementation
polls with bounded sleeps and honors cancellation. On timeout it reports
`freshness_unmet`, not a successful stale read or an unbounded hold.

### Coverage proof

For each requested model, session success requires evidence that the same
candidate-producing read covered its fence through the served segment
watermark together with the exact-tail snapshot. The candidates and the
coverage evidence must come from one coherent read snapshot; a separate
preflight query followed by ordinary recall cannot establish the guarantee.
A skipped, capped, floored, or degraded tail leg may succeed only if the
remaining evidence still proves the fence. An exact SQLite vector scan may
serve as coverage proof only when the scan and fence evidence share that
same snapshot. Otherwise the model is named in `freshness_unmet`.

Coverage is a retrieval-source guarantee, not a promise that every covered
memory survives ranking, caller filters, or a zero limit. Acceptance uses a
distinctive matching query, permissive limit, and no excluding filters for
the immediate remember→session recall test, plus a separate test proving that
an unprovable fence refuses success.

Returning a token while bypassing the session coverage check must turn the
unprovable-fence refusal test red while that named test runs; removing the
same-snapshot requirement (a preflight query then ordinary recall) must turn
the interleaved compaction test red.
