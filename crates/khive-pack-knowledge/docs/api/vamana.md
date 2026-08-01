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
(`search_loaded_with_seq` plus the ADR-118 exact tail), and all associated SQL queries and
serialization logic. These
responsibilities are tightly coupled through the shared `AnnState` and cannot be split
without obscuring the generation-fenced install and warm-ownership lock protocol.

## Fresh-tail serving (ADR-118)

`knowledge.search` and `knowledge.suggest` capture ANN candidates and the loaded bridge's
write-log watermark under one cache read lock, then exact-score the final state of every
`knowledge.atom` write above that watermark. One SQLite statement reads the consumer row,
wildcard-inclusive pair minimum, optional live count, selected log suffix, and joined current
embeddings; separate calls on a pool-backed reader would not guarantee one snapshot. Tail
upserts replace stale ANN candidates, final deletes remove them, and the merged ordering remains
one vector source for RRF rather than adding a new fusion leg. Equal scores break by UUID, making
the result independent of hash-map iteration order. This makes an externally committed vector
visible on the next query even when the daemon still holds an older segment.

The same snapshot reads the wildcard-inclusive consumer-registry minimum before scanning the
log. If compaction has advanced beyond the loaded bridge, the current query reloads and searches
the newly published segment, then revalidates its watermark and reads that segment's own tail in
one snapshot. Peer checkpoints that advance during revalidation cause a bounded reload retry; the
terminal fallback serves only the exact suffix above the last same-snapshot registry floor, never
the original stale candidates under a newer floor. The stale cache generation is retired so the
normal warm path adopts the published segment for following queries. File-backed checkpoints
publish their replacement bridge before raising the durable registry watermark, making that
process's mismatch window empty while preserving crash-safe under-compaction. Pathless checkpoints
serialize the inverse raise-then-install sequence with the per-key process lock.

When no bridge is serving, the exact leg scans only the newest
`ceil(KHIVE_ANN_REBUILD_THRESHOLD × live_count)` raw retained log rows, applying the cap before
final-state coalescing. This is ADR-118's bounded Cold/Empty guarantee, not a corpus-scale exact
fallback. If the consumer row is absent, the first detector publishes the durable `-1`
force-rebuild sentinel under the bridge publication lock and rejects or evicts local serving
state. This applies
even when that process has no local or persisted bridge: local evidence cannot rule out a stale
peer. The sentinel writer re-reads the row after acquiring both publication locks; if a rebuild
already completed while it waited, it preserves the winner instead of demoting the row again.
Every process rejects cached, v2, and legacy-v1 state while `-1` remains; only a fenced successful
full scan may transition it to a normal watermark. Failed or Empty scans keep the sentinel so a
re-created row cannot be mistaken for uninterrupted registry history.
`KHIVE_ANN_FRESH_TAIL=0` disables the exact leg but does not bypass this registry guard.

## `AnnState::warm_states` (shared warm lifecycle, issue #566)

The v1 snapshot preload, v2 segment preload, and request background warm all claim the same
per-`{namespace, model}` lifecycle through `begin_warm` and publish through `finish_warm`:
implicit **Absent** → **Warming** → **Ready** or retryable **Failed**. Each `Warming` entry carries
the namespace generation, start time, and a unique attempt id. The returned ownership permit is
the only attempt allowed to finish that entry; namespace invalidation retires it, so a late old
completion cannot erase or mark ready a newer warm. Dropping a permit transitions its still-owned
entry to `Failed`, making cancellation and panic retryable rather than leaving stale ownership.

The startup callers continue to await each claimed warm before advancing, while
`ensure_ann_background` still spawns and returns immediately. `Ready` suppresses duplicate work;
both empty and operational `Failed` outcomes may be retried by a later request. An explicit worker
outcome keeps a failed Stale-rebuild replacement retryable even though ADR-079 rule 8 leaves its
older bridge available to search. Search's bounded wait and FTS degradation timing are unchanged.

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

Acquires `<segment-dir>/.bridge-checkpoint.lock`, writes Vamana index segments via
`VamanaIndex::save_atomic` (which commits a v2 `KHVVAMG2` record in `metadata.bin` carrying a
`content_hash`), then writes the id-map sidecar (`external_ids.bin`) atomically via a
tmp-then-rename sequence stamped with the commit digest. Checkpoint callers retain that same lock
through mmap re-adoption and the conditional consumer-watermark transition. Sentinel publication
takes it too, preventing both mixed commit/sidecar pairs and a checkpoint racing registry-loss
recovery. A per-key process lock wraps the same checkpoint on every runtime, including pathless
in-memory runtimes, so a later durable raise cannot be followed by an older cache install.

## `ensure_ann_for_model` load order

First hit wins:

1. **Registration guard and fast path** — read the durable consumer row before trusting the
   cache. A normal row permits an already-current in-memory bridge to return immediately. Every
   absent row is changed to `-1`, even when this process has no bridge; an existing `-1` marks
   local force-rebuild state. Both bypass every persisted-state path.
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
4. **Rebuild fallthrough** — capture full-scan authority, then scan the full sqlite-vec corpus,
   build the index from scratch, and atomically write a v2 segment directory so the next daemon
   restart can use path 2. The scan reads the global `ann_write_log` AUTOINCREMENT high-water from
   `sqlite_sequence` in the same statement as the corpus; unlike retained scoped `MAX(seq)`, that
   watermark cannot regress after compaction. The checkpoint may clear local force-rebuild state
   only after its conditional durable transition succeeds at the captured namespace generation.
   Before writing, every ordinary checkpoint also requires its candidate watermark to be at least
   the current durable watermark. The conditional raise repeats that monotonic fence, so a stale
   publisher adopts the winner instead of overwriting it while leaving a newer registry value.
   Empty and failed authoritative scans leave `-1` in place.

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
