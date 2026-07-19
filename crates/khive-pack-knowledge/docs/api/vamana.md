# Vamana ANN bridge internals

Source: `crates/khive-pack-knowledge/src/knowledge/vamana.rs` (entire module is
`pub(crate)` — internal to the crate, not part of the published API).

## Module overview

Wraps `khive_vamana::VamanaIndex` with an ID map (u32 → UUID) so search results can be
fused with FTS5 candidates via RRF.

Persistence (ADR-079, Amendment 1): v2 binary segments are written to
`<db-file>.ann/<hex>/` — a database-scoped root beside the backing database file, so
co-located databases can never adopt each other's segments — on every cold-start rebuild
or explicit reindex. `ensure_ann_for_model` checks the v2 segment directory first, gated
by the write-log restart classifier (`classify_and_adopt_segment`: commit-record and
watermark checks, then a log-table tail probe, ahead of any corpus read — see the
ADR-079 Amendment 1 decision table for the full rule order), falling back to legacy v1
JSON rows in `retrieval_snapshots` for in-place upgrades, then rebuilds from the full
sqlite-vec corpus on cache-miss. `kkernel reindex` re-persists v2 segments and calls
`invalidate_snapshot` to clean up stale v1 rows.

This module exceeds the 700-line soft target because it owns the complete Vamana ANN
lifecycle for knowledge search: `SharedAnn` type, `AnnKey`, snapshot persistence
(`warm_known_snapshots` / `ensure_ann_background`), index build (`build_ann`), search
(`search_loaded`), and all associated SQL queries and serialization logic. These
responsibilities are tightly coupled through the shared `AnnState` and cannot be split
without breaking the atomic lock protocol. Refactoring is deferred until a stable snapshot
format and the warm-start contract are defined.

## `AnnState::generations` (per-namespace write-generation counter, issue #770)

Bumped by `clear_namespace` whenever a corpus mutation invalidates a namespace's ANN
slots. `ensure_ann_for_model` captures the current value for its namespace before doing
anything else — including before its own "already loaded" fast path and before the corpus
scan — and stamps it on the resulting `AnnBridge`. `install_if_fresher` then only replaces
an already-installed entry when the candidate's generation is >= the installed entry's,
instead of the old `entry(key).or_insert(...)`, which always kept whichever build reached
the install site first even if it had scanned a corpus version predating a later
invalidation. Keyed by namespace (not the full `AnnKey`) because `clear_namespace` only
knows the namespace being invalidated, not which models have (or will have) a build in
flight for it.

## `save_atomic`

Writes Vamana index segments via `VamanaIndex::save_atomic` (which commits a v2
`KHVVAMG2` record in `metadata.bin` carrying a `content_hash`), then writes the id-map
sidecar (`external_ids.bin`) atomically via a tmp-then-rename sequence, stamped with the
corpus `content_hash` taken from the v2 commit record.

## `ensure_ann_for_model` load order

First hit wins:

1. **Fast path** — already in the in-memory cache; return immediately.
2. **v2 segment path** — if a `<db-file>.ann/<hex>/` directory exists with a valid
   `metadata.bin`, run the ADR-079 Amendment 1 restart classifier
   (`classify_and_adopt_segment`): a per-write delta log (`ann_write_log`) plus each
   consumer's durable watermark replace the old full-corpus content-hash check, so a
   Hot classification loads the Vamana binary segments directly via `AnnBridge::load`
   (O(load), zero corpus I/O) instead of hashing the live corpus on every restart. A
   short tail replays incrementally (Stale-tail); a long tail serves the existing
   segment while rebuilding in the background (Stale-rebuild). On Cold, fall through.
3. **v1 JSON snapshot path** — try `retrieval_snapshots`; on hit, validate the
   `CorpusFingerprint` (count + dims) and restore from JSON. On miss / stale / corrupt,
   fall through.
4. **Rebuild fallthrough** — scan the full sqlite-vec corpus, build the index from
   scratch, and atomically write a v2 segment directory so the next daemon restart can
   use path 2. Write failures are logged and do not block search.

## `install_if_fresher` (PR #815, covering issue #770's empty-slot scenario)

Two independent fences, both evaluated while holding the write lock:

1. `candidate.generation` must be >= the namespace's CURRENT generation. Comparing only
   against an existing entry (the old behavior) has nothing to compare against once
   `clear_namespace` has emptied the slot — a pre-invalidation candidate would install
   unconditionally even though it scanned a corpus version the namespace has since
   invalidated. `clear_namespace` bumps the generation counter inside this same
   write-lock scope, so a candidate's read of the current generation here can never
   observe a pre-bump value for a slot that has already been (or is about to be) evicted.
2. `candidate.generation` must be >= any already-installed entry's generation, so a
   slower-but-staler build can never clobber a faster build that already scanned a newer
   corpus.

## `clear_namespace` / `install_if_fresher` lock-scope invariant (PR #815)

Eviction and the generation-counter bump happen inside the SAME write-lock scope.
`install_if_fresher` takes this same lock before reading the namespace's current
generation, so there is no window between "slot emptied" and "generation bumped" where a
concurrent install could read a stale (pre-bump) generation and self-approve into the
just-emptied slot.
