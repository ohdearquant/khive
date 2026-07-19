# ADR-107: Memory ANN Lifecycle — Eventual Consistency Contract

**Status**: Accepted
**Date**: 2026-07-10
**Depends on**: [ADR-021](ADR-021-memory-pack.md) (Memory Pack), [ADR-079](ADR-079-ann-persistence-warm-path-integration.md) (ANN persistence warm-path integration, knowledge pack scope)
**References**: issue #791, PR #812

## Supersession note (2026-07-19)

[ADR-079](ADR-079-ann-persistence-warm-path-integration.md) Amendment 1's global-scope-consumer
addendum extends the delta-log/watermark restart classifier to the memory pack's note index. Two
provisions of this ADR are superseded by it. The supersession takes effect when the Amendment 1
implementation for this consumer lands; until then the hash-based restart validation specified
here remains the operative mechanism in shipped code:

- **The scope exclusion.** The Context statement that ADR-079 "explicitly excludes memory-pack
  migration" and that this ADR "does not extend ADR-079's scope to the memory pack" no longer
  holds: the memory note index is Amendment 1's canonical global-scope consumer, registering one
  wildcard `(consumer, '*', embedding_model, watermark)` row.
- **§4 Restart validation.** The persisted `CorpusContentHash` and its full restart re-scan are
  replaced by Amendment 1's decision table: restart freshness is established by comparing the
  segment's committed `last_applied_seq` watermark against the `ann_write_log` tail under the
  consumer's own corpus predicate, with final-state tail replay for small tails. The two gaps §4's
  hash closed remain closed by construction — same-cardinality replacement appends log rows (tail
  non-empty → Stale, never Hot), and the watermark is captured in the same SQLite read snapshot as
  the build's corpus scan, so no separately-sampled-signal race exists. §4's requirement is
  retired, not weakened: a restart never trusts a segment on count/dimension agreement alone.

One consequence of the replacement is a new obligation on §4's reindex discussion: Amendment 1's
write-path rule (every vector mutation appends a log row in the same transaction) now binds every
write path, including `kkernel reindex`'s direct embedding overwrites. A re-embed that bypassed the
log would classify Hot on stale bytes at the next restart; the reindex path must therefore append
`upsert` log rows for every row it overwrites (its snapshot-row deletion becomes moot once the
JSON snapshot path is retired).

Everything else in this ADR remains normative and unchanged: the stale bound (§1), the
write-generation bump / high-water re-enqueue rebuild trigger and non-eviction contract (§2), Cold
behavior (§3), the durable-epoch cross-process invalidation for warm daemons (§4's epoch
paragraphs), and deletion filtering (§5). The classifier replaces only what a restart validates
against — not the in-process eventual-consistency contract.

## Context

[ADR-021](ADR-021-memory-pack.md) specifies `memory.recall`'s scoring and candidate-scoping
contract but does not address ANN cache freshness — recall is defined as if the index were
always current. [ADR-079](ADR-079-ann-persistence-warm-path-integration.md) defines a warm/stale/cold
serving model with a durable content-hash restart check, but it is scoped to the
knowledge pack's v2 segment format and explicitly excludes memory-pack migration from its
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
after each attempt, it re-reads the write-generation counter and compares it against
the floor that attempt captured. If a write raced in, it immediately runs another
attempt against itself — repeating until either the installed generation catches up
(the corpus is caught up, or, e.g. an empty corpus with no further writes, there is
nothing new to catch up to) or a bounded cap of 3 consecutive attempts within one task
is reached.

The cap exists because an unbounded loop under continuous writes — a new write landing
during every single rebuild — would otherwise hold the guard and spin the task forever.
3 was chosen as small enough to bound one task's worst-case extra latency to a handful
of rebuild cycles, while still absorbing the common case (one or two writes racing
during a single in-flight build) without falling back to a later caller. Once the cap
is hit, the loop exits and the remainder is left for the post-release recheck below, or
a later recall/write, to pick up — see the note on this in Consequences.

Guard release and the re-enqueue decision are ordered deliberately: the guard is
dropped _before_ the final freshness recheck runs, not after. Checking first and
dropping second would leave a window — between the loop's last generation read and the
guard's release — where a write landing in that gap finds `warming` still occupied by
the exiting task, no-ops against it, and has nobody left to notice once the guard
disappears moments later. Dropping first means that same race instead finds the guard
already free by the time it goes looking, so the recheck that follows takes a fresh
guard and starts a genuinely new task rather than being silently dropped.

### 3. Cold behavior

A cold `memory.recall` — no cache entry and no loadable snapshot for the model — pays
for a one-time synchronous `ensure_ann_for_model` call, whose CPU-bound graph
construction runs via `spawn_blocking` so it does not monopolize the async runtime for
other concurrent work. This is a genuine cold miss, not the #791 hang: it happens once
per model per process lifetime (until the process restarts), not on every write.

`memory.remember` never pays this synchronous cost, cold or warm. It only ever bumps
the write-generation counter and calls `ensure_ann_background` (§2), which enqueues the
build and returns immediately; the graph construction itself always runs off the
write's response path. A write to a model with no cache entry yet does not force that
model's first build — it schedules it, the same as any other write. The first
synchronous build for a model is paid by whichever caller first issues a cold
`memory.recall` against it, not by `memory.remember`.

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
  it unconditionally, regardless of which write path produced the change.
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

The content-hash check above only runs when a process restarts and re-warms from a
persisted snapshot. It does nothing for a daemon that stays running across a
`kkernel reindex` invocation: `kkernel reindex` also directly deletes the active
`global::memory_vamana::*` snapshot row after re-embedding, but that deletion mutates
only the persisted row — it is invisible to an already-warm daemon, which never
re-reads the snapshot table for a model it already has installed. The daemon and
`kkernel reindex` run as separate processes sharing no in-memory generation state, so
neither the write-generation counter nor the deletion itself is a signal such a daemon
can ever observe on its own. Left uncorrected, a daemon that warmed before a reindex
serves pre-reindex vectors indefinitely, for as long as the process stays up.

This is closed by a durable corpus epoch: a single-row `memory_ann_epoch` table, declared
through `MemoryPack::SCHEMA_PLAN` (the same pack-auxiliary-table contract every other
pack-owned table in this codebase follows) rather than created ad hoc on the epoch
read/write path. Every `AnnBridge` records the epoch value it observed at build/load
time (`epoch_baseline`). The recall warm-hit path (`ann::maybe_check_durable_epoch`)
compares the installed entry's `epoch_baseline` against the current durable epoch and,
on a mismatch, folds it into the same write-generation machinery §2 describes
(`ann::bump_generation`) so the existing single-flight rebuild takes over unchanged.
This check is debounced per model — a fixed 5-second interval between DB reads in
production (`0` in tests, so a single direct call is deterministic), not on every
single recall — so it adds no DB round-trip to the common warm-hit case; the daemon's
own in-memory generation counter remains the only signal it consults for every write
it did not miss, and it uses the durable epoch only to catch the ones it did. The
5-second interval is a fixed implementation default, not itself durable state; a
deployment that needs a different bound changes the constant, not a config value.

**Crash-safety protocol.** The epoch is bumped twice per `kkernel reindex` run,
forming an in-progress/completed pair rather than a single best-effort write at the
end:

1. **Begin (in-progress marker).** Before ANY vector mutation in the pass —
   entities or notes — `kkernel reindex` durably bumps the epoch
   (`begin_reindex_epoch` in `reindex.rs`, calling
   `khive_pack_memory::ensure_ann_epoch_schema` then `bump_memory_ann_epoch`). If
   either the schema-ensure or the bump itself fails, the whole reindex aborts
   before any mutation runs — fail-closed, not warn-and-continue. A daemon that
   observes this bumped epoch mid-reindex (via the debounced check above) rebuilds
   conservatively against whatever partial corpus is on disk at that moment; that
   is never worse than the pre-fix behavior of trusting a stale index indefinitely.
2. **Completion (settled marker).** After all entity/note mutations for the pass
   have committed, `kkernel reindex` invalidates the persisted
   `global::memory_vamana::*` snapshot row and bumps the epoch a second time
   (`invalidate_active_memory_vamana_snapshot`). This forces one more rebuild once
   the corpus has reached its final, fully re-embedded state, so a daemon that
   rebuilt against the mid-reindex partial corpus in step 1 does not keep serving
   that partial build forever.

Coupling every individual committed vector-mutation batch with its own epoch advance
(one bump per batch, in the same transaction as that batch's commit) was considered
and rejected as unnecessary: the two-phase begin/completion pair already guarantees
any observer landing anywhere between the first mutation and the pass's end
eventually converges, without paying a durable write per batch. A crash between the
begin bump and the completion bump still leaves the begin bump durably recorded, so
a daemon that missed the completion bump (because the process crashed before
reaching it) still rebuilds off the in-progress signal rather than never rebuilding
at all — the previous design's exact failure mode. The completion bump's own
failure is no longer silently swallowed either: `kkernel reindex` folds it into its
report's failure set, so the process exits non-zero (unless `--best-effort`) instead
of completing looking clean while the durable signal never fired.

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
- ADR-079 Amendment 1's global-scope addendum has since extended its delta-log/watermark
  classifier to the memory index, superseding §4's memory-pack-specific hash — see the
  Supersession note at the top of this ADR.
- A write that lands after the self-driving re-enqueue loop (§2) has already fully
  exited — whether by converging or by hitting its 3-attempt cap — does not need a
  normal write-path call to `ensure_ann_background` to be picked up: if the
  generation moved past what the exiting task built for, it re-enqueues a CHAINED
  task itself (still bounded to `ATTEMPT_BOUND` attempts) rather than waiting for an
  external caller. Under sustained continuous writes, this can chain multiple linked
  tasks back-to-back; each chained task's first attempt is preceded by a fixed
  debounce delay (`REBUILD_CHAIN_DEBOUNCE`, 1 second in production) before its own
  `ATTEMPT_BOUND`-attempt loop starts, so writes landing during that delay coalesce
  into the next chained task's single generation read instead of each triggering its
  own chain link. This bounds aggregate rebuild work under continuous writes to one
  rebuild per debounce interval, not one rebuild per write. A chain still respawning
  at process shutdown is bounded by the daemon's own drain timeout
  (`KHIVE_DRAIN_TIMEOUT_SECS`), not by anything in this mechanism — there is no
  separate cancellation signal for this loop.
- A daemon that stays warm across a `kkernel reindex` run only detects the resulting
  staleness on its next amortized durable-epoch check (§4), not immediately — the check
  is debounced to a fixed 5-second interval in production, so there can be a bounded
  window of up to that interval after a reindex completes during which a recall still
  serves the pre-reindex index. This is separate from, and in addition to, the
  per-write bound in §1.

## Status

Accepted. Implemented by PR #812 (issue #791), most recently on
`fix/791-recall-ann-rebuild-blocking`.
