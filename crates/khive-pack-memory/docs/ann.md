# Memory ANN Bridge — Design Notes

This is the design companion to `crates/khive-pack-memory/src/ann.rs`. It covers material
that does not belong at any single call site: the ADR-079 Amendment 1 restart classifier's
full decision table, the ADR-118 fresh-tail exact leg's two tiers and re-resolution
convergence argument, and the replay ownership rule. `docs/api/ann-lifecycle.md` covers the
warm cache, freshness signals, and durable-epoch helpers; `docs/recall-reliability.md`
covers the write-generation re-enqueue guarantee. This document does not repeat what those
already cover.

## ADR-079 Amendment 1 restart classifier

`classify_and_adopt_segment` is an 8-rule first-match decision table over the persisted v2
commit record, this consumer's wildcard registry row, and one same-snapshot (live, tail)
read. It replaces the retired JSON-snapshot content-hash gate.

| Rule | Condition | Outcome |
| --- | --- | --- |
| 1 | Commit record absent, corrupt, or invalid length | Cold |
| 2 | Commit record readable but pre-amendment (no watermark) | Cold |
| 3 | Configured embedder dimensions ≠ segment dimensions (read from embedder config, not the corpus — no storage I/O) | Cold |
| 4 | Own wildcard registry row absent for an extended-format state | Cold, after re-registering as pending |
| 5 | Zero live corpus | Empty, regardless of tail contents |
| 6 | No tail above the segment's watermark `S` | Hot: mmap load, zero corpus I/O |
| 7 | Tail exists and is within `ceil(rebuild_threshold * live)` | Stale-tail: mmap load + final-state replay, then checkpoint |
| 8 | Tail exceeds the threshold | Stale-rebuild: serve the checksum-valid segment while a rebuild replaces it |

**Evaluation order of rules 5 and 6.** Rule 6 is tested before rule 5. The tail-existence
probe touches only the log table (no corpus join), so the common empty-tail case fast-paths
to Hot with zero corpus I/O. With an empty tail the committed segment reflects every op
`<= S`, so adopting it serves exactly what Empty would serve even when live corpus is zero —
evaluating rule 6 first does not change the outcome, it just avoids the corpus scan rule 5
would otherwise require. The namespace set on a Hot-adopted bridge stays empty (the
documented conservative default: recall assumes non-visible namespaces may exist) rather
than paying an O(N) DISTINCT corpus scan to populate it.

**Rule 8 is a cost decision, never a demotion.** Serving a stale-but-checksum-valid segment
while a background rebuild replaces it keeps recall available; it never falls back to an
FTS-only degraded mode.

## Replay id-map ownership rule (#1150)

`AnnBridge::apply_final_ops` replays a coalesced final-state tail: `Some(embedding)` replays
a final upsert (tombstone the mapped old ordinal, then exactly one insert); `None` replays a
final delete (tombstone if mapped, no-op otherwise).

A tombstoned ordinal has no owner — `id_map` entries for already-tombstoned slots are stale
(tombstoning never clears them) — so the reverse-lookup built at the start of replay excludes
them. Without that exclusion, a reused slot's new owner could be tombstoned by a replay
delete for the old, already-deleted subject. Concretely: a coalesced final tail can contain
`(id_c, Some(embedding))` (upserting into id_a's freed ordinal) followed by `(id_a, None)`
(id_a's own final delete) — a legal op order, since coalescing only guarantees per-subject
dedup, not cross-subject sequencing. Fail-closed handling of the general form of this
contradiction: if a delete's mapped ordinal's current id-map owner is no longer the subject
being deleted (a same-batch upsert already reused the slot), the tombstone is skipped with a
warning rather than erroring — the old subject's vector was already tombstoned when the slot
was reused, so there is nothing left to delete. Any other id-map contradiction returns `Err`,
and the caller escalates to a Cold rebuild.

## Watermark linearization and persistence

A full memory corpus scan (`load_and_build_from_vector_store`) captures its publication
watermark — the maximum of the retained scoped write-log sequence and this consumer's own
nonnegative active watermark — in the same SQLite statement as the vector rows, so watermark
capture and corpus read are linearized (ADR-079 Amendment 1). The active floor matters after
compaction removes the retained log prefix: the full corpus scan still reflects that prefix,
so a later generation-only rebuild remains monotone instead of regressing to `S = 0`.

`checkpoint_raise_compact_readopt` persists a built bridge, raises the wildcard registry row,
compacts the log across namespaces, then reopens the just-written segment via the mmap load
path and swaps it in for the Owned build product (ADR-079 Amendment 1 §B). Pending
registration precedes the full scan (§A step 1). A failed persistence or fenced watermark
publication never installs the candidate; a reopen failure after a successful publication may
still serve the equivalent Owned bridge. In-memory backends install the Owned candidate
*before* raising and compacting instead, because they have no segment for a concurrent
recall to re-resolve against — the registry guard rejects the candidate while it is pending,
and that ordering ensures no old bridge remains observable after the watermark advances and
its intervening tail can be deleted.

## ADR-118 fresh-tail exact leg

`fresh_tail_leg` gives recall read-your-writes visibility on top of a possibly-stale ANN
graph. It has two tiers:

- **Tier 1 (primary), `s = Some(watermark)`.** A serving bridge exists; every committed write
  above its watermark is merged in via `fresh_tail_serving`.
- **Tier 2 (§3), `s = None`.** No serving index is available at all. The leg caps its scan at
  a corpus-relative newest suffix of the log (`ceil(threshold * live corpus)` rows) instead
  of the entire scope, guaranteeing visibility of only the caller's most recent writes until
  a serving index exists again. This is a cheap log-only existence probe first (fast-paths the
  common empty-tail case with no corpus join), then one statement/snapshot for the capped
  case.

### Registration precondition

Before either tier runs, `fresh_tail_leg` re-reads this consumer's own registry watermark. A
serving bridge is trusted only while its consumer is active (`S >= 0`). Pending, recovering,
or absent registration means a peer may have retired the protection an already-captured ANN
candidate set relied on, so the leg drops those candidates (`Replace(Vec::new(), Some(reason))`)
even for an otherwise-disabled exact leg — a disabled leg must not leak stale state either.

A pathless first checkpoint replaces the in-process bridge *before* activating its pending
row. If a query observes exactly that window, it waits on the same per-model lock the
checkpoint holds and then revalidates, rather than risk the closed-state guard evicting a
just-built, about-to-activate bridge.

### Compaction linearization and mismatch re-resolution

`fresh_tail_serving` reads the wildcard-inclusive registry minimum inside the same read
snapshot as its tail statement (the "Compaction linearization" guard, ADR-118 §1). If that
minimum `m` exceeds the bridge's watermark `s`, the log may no longer retain every row above
`s`, and completeness above it is unprovable — this is a *mismatch*.

- **Pathless mismatch.** There is no filesystem commit record to re-resolve against by
  reading a file. A pathless checkpoint installs its replacement bridge before raising and
  compacting, so a recall may have captured the old bridge immediately before that swap.
  The leg re-resolves by re-searching the *currently installed* bridge under the same SQL
  snapshot's pinned registry/log state, then returns those candidates as a `Replace` — never
  merged with the stale set the caller originally captured.
- **File-backed mismatch.** A cheap filesystem commit-record read (no DB access) checks
  whether a newer persisted segment (watermark `>= m`) exists before deciding whether the
  snapshot's floor fallback is even needed. If one exists, `fresh_tail_reresolve` handles it
  (below). If not, the leg floors the scan at the same-snapshot registry minimum instead of
  dropping the leg — a coherent `(old candidates, registry minimum)` pair — and forces
  re-adoption (`bump_generation`) so a future query gets a fresh bridge.

### Re-resolution convergence (`fresh_tail_reresolve`)

When a newer persisted segment exists, `fresh_tail_reresolve` loads it, searches it directly,
and merges in its own tail above its own watermark — a self-consistent pair that never
borrows a newer watermark while serving older, stale-bridge candidates. The load is local to
the query (not installed into the shared served map); `bump_generation` still forces the
existing background machinery to adopt the segment for future queries.

A further race is possible: a peer checkpoint can advance the registry minimum *past* the
just-loaded segment's own watermark in the window between the load and this function's own
re-validation read — the same compaction race `fresh_tail_serving` already guards against for
its own tail fetch. Unlike that primary-path guard, this one can *reload*: the segment this
function loads is always the currently published one, and compaction through a minimum `M`
implies the published segment already covers `M`. So a mismatch here re-loops instead of
immediately falling back to a floored scan. Flooring on the first mismatch would leave the
`(old watermark, new minimum]` window in neither the stale candidate set nor the floored
tail, silently dropping committed writes. Only `FRESH_TAIL_RERESOLVE_MAX_ROUNDS` (3)
consecutive mismatches — peers advancing the minimum faster than this leg can load a segment
for it, which should not happen at normal checkpoint cadence — fall back to that floor. Three
back-to-back peer checkpoints landing inside one query's read window would itself be
pathological; the bound exists so a pathological run degrades to the ADR's floored fallback
instead of looping unboundedly. On the terminal round, the last loaded candidates are served
floored at the last observed minimum — coherent, at the cost of the `(s_loaded, m]` window
not being provably retained in the log.

### Outcome disclosure contract

`FreshTailOutcome` has three variants, and `outcome_into_candidates` is the single mapping
every recall path uses to fold an outcome into servable candidates plus a degradation
disclosure — so no exceptional class is silently treated as healthy:

- **`Ops`** — coalesced final tail ops, valid against the caller's existing candidates.
  Merged in via `merge_fresh_tail`. No disclosure. The common case.
- **`Replace(candidates, reason)`** — a compaction mismatch forced re-resolution; these
  candidates replace the caller's set outright (never merged with the stale one), since they
  are already a self-consistent `(new candidates, new watermark)` pair. `reason` is
  `Some(..)` when the re-resolved candidates are served *without* their fresh-tail merge
  (a reader/snapshot/registry/tail-fetch failure after re-resolution) — the candidate set is
  still coherent, but read-your-writes visibility was lost, and the caller must disclose
  that. `None` means the full `(candidates, tail)` pair was assembled — no degradation.
- **`Skipped(reason)`** — the leg sat out the query entirely (disabled, unregistered
  consumer, or an unrecoverable read failure); the caller's prior candidates are unaffected,
  but the non-empty `reason` must still be disclosed.

### Merge semantics

`merge_fresh_tail` deduplicates a fresh-tail's coalesced final ops against an existing ANN
candidate list by `subject_id`, with the tail winning (its embedding is at least as fresh as
the segment's), then re-sorts by score. A `None` op (final delete) drops the subject from the
merged list even if it was present in the stale candidate set — the tail is authoritative for
every subject it names. An empty `ops` list returns the stale candidates unchanged, so fusion
is byte-identical whenever there is nothing to merge.

## Regression history

These issues shaped invariants enforced directly by tests in `ann.rs`; they are noted here
rather than as prose scattered through the source:

- **#750** — a slow build with an older write generation must never replace a newer,
  already-installed bridge (`install_replacing`'s generation compare-and-replace rule).
- **#812** — a warming guard must release on every exit path (success, error, or panic), and
  an in-flight background warm must re-enqueue itself when a later write advances its
  generation floor, with zero further recalls or writes needed to retrigger it; a durable
  epoch bump from a separate process must also invalidate a warm daemon's cached entry.
- **#1150** — see "Replay id-map ownership rule" above.
- **#1161** — the no-index fresh-tail fallback follows ADR-118's
  `ceil(threshold * live corpus)` ceiling, not a flat row cap.
- **#1828** — `fresh_tail_serving` must retain one admitted reader across its
  `BEGIN -> registry-min -> tail -> COMMIT` sequence; its `Skipped` failure arm must report
  the exact failure-site reason so a regression here surfaces as a specific diagnostic, not
  an opaque candidate mismatch.
