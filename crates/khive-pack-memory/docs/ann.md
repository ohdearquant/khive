# Memory ANN Bridge — Design Notes

This is the design companion to `crates/khive-pack-memory/src/ann.rs`. It covers material
that does not belong at any single call site: the ADR-079 Amendment 1 restart classifier's
full decision table, steady-state incremental maintenance, the ADR-118 fresh-tail exact
leg's two tiers and re-resolution convergence argument, and the replay ownership rule.
`docs/api/ann-lifecycle.md` covers the
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

## Steady-state maintenance and checkpoint cadence

The warm index host applies small write-log tails to its installed bridge when only the
write generation changed and the durable corpus epoch still matches. It reads the tail,
its scoped raw row count, and the registry compaction minimum in one read snapshot. The
tail starts at the installed index's own applied watermark; repeated writes to one subject
are coalesced to that subject's final state before mutation. A successful batch advances
the bridge's generation and applied watermark without loading or publishing a segment.

Three freshness markers have different roles:

- **Write generation** coordinates process-local warming and prevents an older warm result
  from replacing a newer one. It is not a durable log position.
- **Applied watermark** records the log prefix already represented by the in-memory index.
  The next incremental batch starts above this position.
- **Published watermark** records the committed segment's prefix. While the bridge has
  unpublished deltas, recall uses this earlier watermark for the fresh-tail exact leg.
  Successfully inserting a vector into the approximate graph does not by itself guarantee
  immediate recall visibility; the exact leg continues to cover those deltas until
  publication.

The host batches publication using these environment settings:

| Setting | Default | Meaning |
| --- | --- | --- |
| `KHIVE_ANN_CHECKPOINT_OPS` | `1000` | Maximum dirty raw log-row threshold, further limited by the corpus-relative cap below; zero is clamped to one. |
| `KHIVE_ANN_CHECKPOINT_SECS` | `300` | A dirty bridge becomes due when this many seconds have elapsed since its last checkpoint; zero disables the interval trigger. |
| `KHIVE_ANN_CONSOLIDATE_TAU` | `40000` | Insert-plus-tombstone churn threshold for consolidation before publication; zero is clamped to one. |

Invalid unsigned settings fall back to their defaults. These are environment settings;
they do not imply that the ADR's described TOML or CLI configuration wiring is available.

Let `D` be the configured dirty-row threshold, `f` the existing
`KHIVE_ANN_REBUILD_THRESHOLD` fraction (default `0.20`), and `L` the installed index's live
vector count. The effective publication threshold is
`max(1, min(D, floor(f * L / 4)))`. For example, with defaults and 10,000 live vectors, the
threshold is 500 raw log rows; at 100,000 live vectors it is 1,000. Tiny corpora checkpoint
at one row. This leaves replay headroom at normal corpus sizes; it does not bound the size
of a burst that arrives between warm passes or change the restart classifier's
`ceil(f * live corpus count)` rule.

Dirty accounting accumulates scoped **raw log rows since publication**, not the number of
coalesced subjects and not the difference between database-global sequence numbers. A
thousand updates to one subject still contribute a thousand dirty rows. Consolidation
uses a separate churn counter: replacing an existing vector normally performs one
tombstone and one insert. When that counter reaches tau, consolidation compacts tombstoned
slots and remaps the external UUID table before saving. An empty consolidation remap means
ordinals are unchanged. Consolidation resets churn, not unpublished dirty-row accounting.

Only dirty bridges checkpoint. Reaching either the effective row threshold or the enabled
interval makes publication due; a fresh, clean warm is a no-op. A tracked, one-shot deadline
handles dirty intervals even without another write or recall. Each model has at most one
deadline; shutdown cancels it. A newer checkpoint followed by another dirty period can hand
off the remaining deadline, while failures wait for the next ordinary attempt. The local model lock
serializes mutation and publication. Filesystem publication borrows the installed bridge
under a read lock so searches may continue while it saves; it does not hold the index
write lock across filesystem or database I/O. A successful file publication writes the
complete segment and UUID sidecar before conditionally raising the registry watermark and
compacting the protected log prefix, then re-adopts mmap backing. Applying a batch in RAM
alone never raises that durable watermark or authorizes compaction.

The first valid insert after mmap adoption still copies the complete f32 vector store and
SQ8 codes to owned memory. Later inserts reuse that owned backing until checkpoint and
re-adoption. Tombstones mutate graph and lifecycle state without promoting the mapped
vector/code stores. Save and load remain full-segment operations: even a Hot load checksums
the segment files and reconstructs owned graph, lifecycle, and UUID-map state. Batching
amortizes those costs; it does not make the first insert or a checkpoint proportional only
to the changed rows.

### Peer rotation and recovery

The existing five-second rotation watcher still releases replaced mmap generations. A
dirty bridge retains its mapped-generation identity, including after delete-only batches.
A valid peer segment may be newer than the local published watermark yet older than its
in-memory applied watermark. Rotation compares that candidate with the published baseline,
adopts the validated segment, and advances the local write generation so the retained tail
is replayed. The new bridge and exact leg start from the peer segment's own watermark;
they never borrow the displaced dirty bridge's later applied position. The namespace set
remains conservative because the peer may cover namespaces absent from the old local set.

Registry protection, conditional publication, and compaction across overlapping consumers
are unchanged. A missing or closed consumer registration, a changed durable epoch, an
unavailable protected tail, or an incompatible segment still takes the established
reclassification/rebuild path. A tail above the rebuild threshold can still require a full
build. Restart continues to use the persisted segment and retained log: a crash before a
batched checkpoint leaves a recoverable tail, rather than falsely recording unpublished
deltas as durable. Processes that are not the warm index host retain their existing
load/replay behavior and do not publish segments or build the full corpus.

### Warm completion attribution

The `memory.ann_warm` completion payload keeps the existing phase timing fields and adds
`path` and `ops_applied`. Paths distinguish `already_fresh`, `incremental_in_place`,
`incremental_checkpoint`, `segment_load`, `stale_tail_replay`, `stale_tail_publication`,
`full_build`, `empty`, `declined`, `discarded`, and `failed`. On successful tail processing,
`ops_applied` counts coalesced final subject operations, not raw dirty rows, Vamana churn,
or vectors scanned by a full build. A path and count describe the warm attempt; they do not
replace its outcome or timing evidence. Benign shutdown cancellation retains the existing
cancellation event behavior, and event append remains best-effort.

## Replay id-map ownership rule (#1150)

`AnnBridge::apply_final_ops` replays a coalesced final-state tail: `Some(embedding)` replays
a final upsert (tombstone the mapped old ordinal, then exactly one insert); `None` replays a
final delete (tombstone if mapped, no-op otherwise).

A tombstoned ordinal has no owner — `id_map` entries for already-tombstoned slots are stale
(tombstoning never clears them) — so the reverse lookup initialized at the first replay excludes
them. Later batches update this cached map in place; consolidation remaps and refreshes it. Without that exclusion, a reused slot's new owner could be tombstoned by a replay
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
  above its exact-leg watermark is merged in via `fresh_tail_serving`. A dirty bridge uses
  its published watermark here, preserving exact coverage of unpublished incremental
  updates even when its in-memory applied watermark is newer.
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
  but the non-empty `reason` must still be disclosed. `reason` is a failure-site label plus,
  whenever the site was holding the error that caused the skip, that error's own message,
  bounded to `SKIP_DETAIL_MAX_CHARS` characters and marked when cut. One label covers causes
  that differ in what the caller should do next — a segment directory rewritten underneath the
  read self-heals on the next query, a truncated segment does not, and both arrive as
  "re-resolved segment load failed" — so a site that discards an error must pass it, and a site
  that genuinely has none keeps emitting the bare label. The rendering is one string: callers
  may depend on the reason's presence, never on its wording.

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
