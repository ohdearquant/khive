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
caller's own most recent write to the same model by at most **one background rebuild
latency** — the wall-clock time of the single rebuild that write's write-generation bump
triggers. There is no bound on how far behind a _third party's_ concurrent writes the
served index may be beyond the same one-rebuild window; the bound is per-write, not
wall-clock.

Recall never blocks on this staleness. A stale-but-installed entry is served
immediately (`ann::search_loaded` does not consult freshness); the caller does not wait
for the triggered rebuild.

### 2. Rebuild trigger: write-generation bump

Every memory write path that may change a model's corpus (`memory.remember`,
`memory.prune`, and the KG-side note-mutation hook) bumps that model's write-generation
counter (`ann::bump_generation`) instead of clearing the cache or deleting the
persisted snapshot. `ensure_ann_for_model`/`ensure_ann_background` compare a build's
snapshotted generation against this counter (`install_if_fresher`) so a build that
started before a write can never clobber a later, fresher one, regardless of which
finishes first.

The write-generation guard that fires a background rebuild (`ensure_ann_background`)
must release on every exit of the task it guards — success, error, or panic — not only
on the "nothing loaded" failure path. A guard left set after a successful warm silently
rejects every subsequent write's rebuild request, serving the same index forever with
no further attempt to catch up. This is enforced by an RAII release tied to the
background task's own scope.

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
restored snapshot is current: a delete-one/add-one or a content re-embed preserves both
count and dimensions exactly, so a fingerprint-only check would classify a stale
snapshot as current forever, with no write-generation bump ever able to detect the
mismatch (the counter starts at 0 on both sides).

Restart validation therefore additionally persists a durable `CorpusContentSignal`
(live-note count plus the corpus's maximum `updated_at`) alongside the snapshot and
recomputes it fresh against the live corpus on every warm attempt, restart or not. A
new or edited note always carries a strictly fresher `updated_at` than every prior row,
so this signal changes on exactly the corpus mutations a bare fingerprint misses. A
mismatch in either the fingerprint or the content signal is treated as stale: the
snapshot is discarded and the model falls through to a full rebuild from the vector
store, the same path a genuinely absent snapshot takes.

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
- Future work extending ADR-079's v2 segment format and content-hash restart check to
  the memory pack would supersede §4 here; that migration is out of scope for this ADR.

## Status

Proposed. Implemented by PR #812 (issue #791) as of commit `2f9d037e` on
`fix/791-recall-ann-rebuild-blocking`.
