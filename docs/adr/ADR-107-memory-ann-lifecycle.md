# ADR-107: Memory ANN Lifecycle — Eventual Consistency Contract

**Status**: Proposed
**Date**: 2026-07-10
**Depends on**: [ADR-021](ADR-021-memory-pack.md) (Memory Pack), [ADR-079](ADR-079-ann-persistence-warm-path-integration.md) (ANN persistence warm-path integration, knowledge pack scope)
**References**: issue #791, PR #812

## Context

[ADR-021](ADR-021-memory-pack.md) specifies `memory.recall`'s scoring and candidate-scoping
contract but does not address ANN cache freshness — recall is defined as if the index were
always current. [ADR-079](ADR-079-ann-persistence-warm-path-integration.md) defines a warm/stale/cold
serving model with a durable content-hash restart check, but it is Proposed, scoped to the
knowledge pack's v2 segment format, and explicitly excludes memory-pack migration from its
scope.

Issue #791 found that `memory.recall` could hang for the full rebuild duration: a write
invalidated the model's in-memory ANN index and deleted its persisted snapshot
synchronously on the write's own path, so a concurrent recall landing before the
(correctly backgrounded) rebuild finished had nothing to serve except an inline
synchronous corpus rebuild. PR #812 fixed this by making a write bump a per-model
write-generation counter instead of clearing anything — the previous index and snapshot
stay installed and keep serving reads until a fresher build replaces them. That is a
real relaxation of "recall reflects every write immediately" to an eventual-consistency
contract, and it needs its own normative definition — this ADR is that definition,
scoped to the memory pack only.

This ADR does not amend ADR-021 and does not extend ADR-079's scope to the memory pack;
it is a standalone contract for what PR #812 actually implemented.

## Decision

### 1. Stale bound

A `memory.recall` may serve results computed from an ANN index that is behind the
caller's own most recent write to the same model by at most **the wall-clock time of
the background rebuild(s) that write's write-generation bump caused to run** — not a
fixed count of rebuilds. In the common case that is a single rebuild latency. If the
write instead races in while a rebuild triggered by an earlier write is already in
flight, `ensure_ann_background`'s own task detects on exit that the installed
generation is still behind the counter and immediately re-enqueues another attempt
against itself (§2) — no second write or recall is needed to notice or retrigger it.
The bound is therefore always "however many rebuild latencies it takes that one
self-driving task to catch up", with no dependency on further external activity. There
is no bound on how far behind a _third party's_ concurrent writes the served index may
be beyond that window; the bound is per-write, not wall-clock.

Recall never blocks on this staleness. A stale-but-installed entry is served
immediately (`ann::search_loaded` does not consult freshness); the caller does not wait
for the triggered rebuild.

### 2. Rebuild trigger: write-generation bump, high-water re-enqueue

Every memory write path that may change a model's corpus (`memory.remember`,
`memory.prune`, and the KG-side note-mutation hook) bumps that model's write-generation
counter (`ann::bump_generation`) instead of clearing the cache or deleting the
persisted snapshot. `ensure_ann_for_model`/`ensure_ann_background` compare a build's
snapshotted generation against this counter (`install_if_fresher`) so a build that
started before a write can never clobber a later, fresher one, regardless of which
finishes first.

The write-generation guard that fires a background rebuild (`ensure_ann_background`)
releases on every exit of the task it guards — success, error, or panic — not only on
the "nothing loaded" failure path, via an RAII release tied to the background task's
own scope. Release alone is not sufficient, though: a write landing while that task's
own build is still in flight bumps the counter, but that write's own
`ensure_ann_background` call finds the guard already held and no-ops (the fire-once
single-flight guard exists precisely to prevent a second redundant task) — nobody else
is left to notice the counter moved. The tracked background task therefore loops:
before releasing its guard, it re-reads the write-generation counter and compares it
against the floor its own just-finished attempt captured. If a write raced in during
that attempt, it immediately re-enqueues another attempt against itself — repeating
until the installed generation catches up (or the corpus never had anything to build,
in which case the first attempt's fixed point is itself the terminal state). This is
what guarantees §1's bound holds without depending on a later recall or write to
retrigger it.

### 3. Cold behavior

The first `memory.recall` or `memory.remember` for a model with no cache entry and no
loadable snapshot pays for a one-time synchronous `ensure_ann_for_model` call, whose
CPU-bound graph construction runs via `spawn_blocking` so it does not monopolize the
async runtime for other concurrent work. This is a genuine cold miss, not the #791 hang:
it happens once per model per process lifetime (until the process restarts), not on
every write.

### 4. Restart validation

Write-generations live only in `AnnState`, an in-memory structure that resets to a
fresh, empty state on every process restart. A persisted snapshot's `CorpusFingerprint`
(vector count + dimensions) is therefore not sufficient on its own to decide whether a
restored snapshot is current: a delete-one/add-one, a content re-embed, or a
vector-only re-embed all preserve both count and dimensions exactly, so a
fingerprint-only check would classify a stale snapshot as current forever, with no
write-generation bump ever able to detect the mismatch (the counter starts at 0 on
both sides).

Restart validation therefore additionally persists a durable `CorpusContentHash`
alongside the snapshot: a blake3 hash of every live `note.content` row's
`(subject_id, embedding)` bytes, in the same deterministic order
(`ORDER BY v.subject_id`) the graph build itself scans in. The hash is computed once,
during the build's own corpus-scanning read — not from a separate query sampled before
or after it — so the persisted signal can never describe a different corpus snapshot
than the graph it is paired with. This closes two distinct gaps a timestamp-based
signal (e.g. `MAX(notes.updated_at)`) left open:

- **Vector-only re-embedding is invisible to `notes.updated_at`.** `kkernel reindex`
  overwrites embeddings directly without touching the `notes` table at all, so a
  same-model, same-dimension re-embed through that path changes no timestamp a
  timestamp-based signal could observe. Hashing the embedding bytes themselves catches
  it unconditionally, regardless of which write path produced the change. As defense
  in depth, `kkernel reindex` also directly deletes the active
  `global::memory_vamana::*` snapshot row after re-embedding, forcing a rebuild on the
  next warm even before this hash check would otherwise catch it — the daemon and
  `kkernel reindex` run as separate processes sharing no in-memory generation state, so
  this direct invalidation is the only signal available to a daemon that stays running
  across a reindex.
- **A separately-sampled signal races the build it is meant to describe.** A signal
  computed by its own query after the graph build finishes can observe a
  same-cardinality write that landed in the gap between the build's scan and the
  signal's own read, pairing a stale graph with a signal that looks fresh. Because the
  hash is computed from literally the same rows the build consumed, in the same read,
  no such gap exists to race in.

Restart validation recomputes the hash fresh against the live corpus by re-scanning
every live `note.content` embedding row for the model, in the same row order, and
compares it byte-for-byte against the persisted value — not a cheap aggregate query,
deliberately, since only an exact re-scan can detect every mutation a build-time hash
would have detected. This scan is paid for only when the cheap `CorpusFingerprint`
check already agrees; a fingerprint mismatch alone is sufficient to know a rebuild is
needed and skips the hash scan entirely. A mismatch in either the fingerprint or the
content hash is treated as stale: the snapshot is discarded and the model falls
through to a full rebuild from the vector store, the same path a genuinely absent
snapshot takes.

### 5. Deletion filtering

Because an ANN cache entry may be stale (relative to a same-process write or to a
restart-time corpus change already accounted for above), the `id_map` ordinals it
returns are not re-validated against `deleted_at`/expiry at lookup time. Every ANN hit
is resolved to its source row through a live-note hydration query
(`deleted_at IS NULL`, and, where applicable, `expires_at` in the future) before it is
returned to the caller. A stale ordinal pointing at a since-deleted or since-expired
note therefore hydrates to nothing and is silently dropped — never served as content.
The residual cost of serving a stale index is bounded to rank quality and to
newly-written notes not yet reflected in the graph, never to stale or deleted content
leaking through.

## Consequences

- `memory.recall` is a fast path unconditionally: it never blocks on a background
  rebuild it did not itself have to originate.
- A model that receives no further writes after a restart-time content mismatch is
  rebuilt exactly once, at the next warm attempt for that model, not repeatedly.
- Consumers that require read-after-write recall for memory notes (none currently
  documented) are not served by this contract and must poll `memory.recall` or wait
  for the rebuild to observably complete via `memory.feedback`/index inspection; this
  ADR does not add a synchronous variant.
- Future work extending ADR-079's v2 segment format to the memory pack would supersede
  §4's memory-pack-specific hash with ADR-079's own content-hash mechanism; that
  migration is out of scope for this ADR.
- A write that lands after the self-driving re-enqueue loop (§2) has already fully
  exited still needs a normal write-path call to `ensure_ann_background` to be picked
  up — the loop only protects writes that race in while its own task is still running,
  not writes arriving after it has converged and returned.

## Status

Proposed. Implemented by PR #812 (issue #791), most recently on
`fix/791-recall-ann-rebuild-blocking`.
