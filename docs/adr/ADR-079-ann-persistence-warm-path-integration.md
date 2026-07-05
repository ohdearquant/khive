# ADR-079: ANN Persistence Warm-Path Integration — Wiring v2 Persistence into the Daemon

**Status**: proposed
**Date**: 2026-06-28
**Authors**: khive maintainers

## Context

Three accepted ADRs already describe the pieces of a fast, durable ANN warm path:

- [ADR-049](ADR-049-khived-daemon.md) introduced the warm-state daemon. Its §"Background lazy
  warm" replaced the blocking `ensure_ann().await` with a fire-once background warm so the socket
  serves immediately. ADR-049 left the snapshot format unchanged and explicitly deferred the
  optimization: _"a `bincode`/mmap snapshot is a separate, orthogonal optimization (future ADR)."_
  This is that future ADR.
- [ADR-052](ADR-052-ann-production-lifecycle.md) designed and shipped — at the `khive-vamana`
  crate layer — the production lifecycle: SQ8 quantization (`khive-quant`), tombstone delete with
  eager 2-hop repair, incremental `insert`, periodic `consolidate`, and a **crash-safe v2 mmap
  persistence format** (`KHVVAMG2` magic, `save_atomic`, `load_or_build`) that turns daemon
  restart from O(rebuild) into O(load).
- [ADR-035](ADR-035-cli-config-and-auto-embed.md) established the config precedence model
  (CLI flag > project `khive.toml` > global `khive.toml` > `KHIVE_*` env > built-in default).

### The gap: the crate is built, the daemon does not use it

The `khive-vamana` crate implements every ADR-052 primitive:

| ADR-052 primitive           | `khive-vamana` symbol                        | Status  |
| --------------------------- | -------------------------------------------- | ------- |
| Crash-safe v2 mmap save     | `VamanaIndex::save_atomic` (index.rs)        | present |
| O(load) fingerprint restore | `VamanaIndex::load_or_build`                 | present |
| Incremental insert          | `VamanaIndex::insert`                        | present |
| Tombstone delete + repair   | `VamanaIndex::tombstone` / `tombstone_batch` | present |
| Consolidation               | `VamanaIndex::consolidate`                   | present |
| SQ8 acquisition tier        | `khive-quant` `GsSq8Codec`                   | present |

The knowledge-pack ANN bridge (`khive-pack-knowledge/src/knowledge/vamana.rs`) consumes **none**
of them. `AnnBridge` is a thin wrapper over `VamanaIndex` that exposes only `build` (full greedy
construction) and the **v1 JSON snapshot** path (`to_vamana_snapshot` / `from_vamana_snapshot` →
`VamanaIndex::to_snapshot` / `from_snapshot`). The live warm path is therefore still the
pre-ADR-052 behavior:

1. **Snapshot is a JSON BLOB in SQLite.** `persist_snapshot` writes `serde_json::to_vec(&snapshot)`
   into the `retrieval_snapshots` table. At 466K vectors this is the ~350 MB blob ADR-049 §Context
   measured, deserializing in ~50–120 s. The v2 mmap format that exists in the crate is unused.
2. **Snapshot miss falls back to a full rebuild.** `ensure_ann_for_model` calls
   `load_and_build_from_vector_store`, which scans the entire vector store and runs
   `AnnBridge::build` → `VamanaIndex::build` (O(N) greedy construction). `load_or_build` — which
   would mmap-restore in O(load) — is never called.
3. **Every vector write invalidates the whole snapshot.** `index_handler.rs` calls
   `invalidate_snapshot` ("Any vector write invalidates the existing snapshot") which `DELETE`s the
   `retrieval_snapshots` row and clears the in-memory index. The persisted snapshot is recreated
   **only after a subsequent full rebuild**. There is no incremental update and no periodic
   checkpoint of the ANN graph. (The only periodic checkpoint task in the daemon —
   `run_checkpoint_task`, `CheckpointConfig` — is a SQLite **WAL** checkpoint, unrelated to the
   ANN index.)

Consequence for an actively written corpus: between any write and the next full rebuild there is
no snapshot, so a daemon restart in that window pays the full O(N) rebuild. Background warm
(ADR-049) hides the rebuild from the socket, but a `knowledge.suggest` / `search` / `compose`
issued during the warm window over an empty FTS result still has nothing to return. That is the
[#322](https://github.com/ohdearquant/khive/issues/322) symptom: the handler returned a hard
`RuntimeError::Internal("ANN index is warming…")`. [PR #335](https://github.com/ohdearquant/khive/pull/335)
degrades that to an FTS-only result with an `ann_unavailable: true` advisory. PR #335 is the
correct **safety net**, but it treats the symptom. The root cause is that ADR-052's persistence
was never wired into the daemon, so the warm window is an O(rebuild) window far more often than it
should be.

### Operational knobs are hardcoded, not configured

The warm-wait timeout is a pair of compile-time `const`s in `search.rs` (3 s on `main`; PR #335
raises it to a 5 s `const` plus an `AtomicU64` test override). ADR-035's precedence table never
listed it — it was never designed as a config key. The same is true for the consolidation threshold and any
checkpoint cadence. These are deployment-dependent operational knobs (corpus size, hardware,
restart frequency) and belong in `khive.toml`, not in `const`s and test-only override seams.

## Decision

Wire ADR-052's v2 persistence and lifecycle into the knowledge-pack ANN bridge and the ADR-049
daemon warm path, define the serving policy across the three warm states, replace
invalidate-and-rebuild with incremental maintenance plus a periodic ANN checkpoint, and surface the
operational knobs through `khive.toml`.

### 1. v2 mmap persistence replaces the JSON BLOB

The knowledge-pack bridge persists and restores through the ADR-052 v2 path:

- **Restore**: on warm, the bridge loads the persisted segment directory through the ADR-052 v2
  path. Two distinct checks gate it, and the ADR is precise about which does what. The public
  `CorpusFingerprint` is `vector_count` + `dimensions` **only** — a cheap pre-filter, not the
  authoritative match. The authoritative gate is a **blake3 content hash** over the corpus vectors,
  compared against the segment's v2 commit record; it is an internal v2 field, not part of
  `CorpusFingerprint`. Because of the content hash, a content-preserving change that keeps the same
  count and dimensions (re-embedding every atom, or a delete-one/add-one) is still caught and does
  not falsely classify as current. A content-hash match is an O(load) mmap restore (Hot, §2); a
  mismatch drives Stale or Cold per §2; an absent or corrupt segment rebuilds and re-persists. This
  is ADR-052 §3 behavior, now reached from the live path. (**On the cost of "O(load)":** it means
  skipping greedy graph _construction_. The load still does an O(N) corpus read plus an O(N) blake3
  pass to validate the content hash before the mmap graph load — the win is avoiding construction,
  not the corpus scan. The checkpoint-cadence math in §4 accounts for this.)
- **Persist**: `AnnBridge` gains `save_atomic` plus `load` and `load_or_build` delegations to its
  inner `VamanaIndex` (`load` for the §2 warm decision; `load_or_build` for the Cold/rebuild branch).
  The crash-safe commit-record protocol (bulk segments first, `metadata.bin` with per-segment blake3
  checksums last) is ADR-052 §3 and is inherited unchanged.

**Storage location.** v2 segments are filesystem files (`save_atomic(path)`), not a SQLite BLOB.
They live under a single per-(namespace, model) directory beside the backend's data file, named by
the lowercase-hex encoding of the snapshot key `"{namespace}::vamana::{model}"` — one directory per
pair, decoded by the warm-path filesystem enumeration:

```
<backend_data_dir>/ann/<hex(namespace::vamana::model)>/
    {metadata.bin, graph.bin, vectors.bin, lifecycle.bin, external_ids.bin}
```

The four Vamana segment files are ADR-052's crash-safe commit set, inherited unchanged: `metadata.bin`
is the commit record (written last, via tmp-then-rename), alongside `graph.bin`, `vectors.bin`, and
`lifecycle.bin` (tombstones, free slots, reverse adjacency, and the consolidation counter).
`external_ids.bin` is the id-map sidecar the in-process index needs to translate ANN ordinals back to
record UUIDs; it is written after the segment commit and stamped with the v2 commit `content_hash`. On
warm it is rejected if its hash does not match the commit (a torn segment/sidecar pair, from a crash
between the two writes) or if its UUID count does not match the loaded vector count.

`<backend_data_dir>` resolves from the pack's assigned backend (ADR-028) and, in the cloud
write-owner model, from the per-tenant database directory (ADR-067) — so ANN segments are
per-tenant by construction, never shared across namespaces. ANN segments are derived, recomputable,
local-only state — they are git-ignored exactly as `working.db` vectors are (ADR-035 §6); they are
never committed and never travel in NDJSON.

**Backend data-dir accessor (new surface, a precondition of this section).** No filesystem-directory
accessor exists on the storage/runtime surface today: `BackendConfig`/`Backend` take `path` as a
_constructor input_, not a readable accessor, and `RuntimeConfig::db_path` is deprecated in favour of
`from_backend` and is `None` for in-memory backends. Resolving `<backend_data_dir>` therefore
requires adding a `backend_data_dir() -> Option<PathBuf>` accessor on the backend handle /
`KhiveRuntime`, threaded to the knowledge pack. This is a real ADR-028 surface addition, not a free
assumption. For a pathless/in-memory backend the accessor returns `None`: ANN persistence is disabled
(segments are skipped) and every warm is a Cold rebuild (§2) — correct, since an in-memory backend
has no durable home for derived state.

**Relationship to `retrieval_snapshots` (no drop, no wholesale deprecation).** `retrieval_snapshots`
is a **shared** table with consumers beyond the knowledge pack: the memory pack's own Vamana
snapshots (`khive-pack-memory/src/ann.rs`, key `global::memory_vamana::%`), the generic HNSW/BM25
snapshot store (`khive-retrieval/src/persist/core.rs`), the admin reindex invalidation path
(`kkernel/src/reindex.rs`), and ADR-062's stale-snapshot purge. It is also created ad hoc by several
`CREATE TABLE IF NOT EXISTS` sites, not managed by a migration. This ADR therefore changes **only the
knowledge pack's own rows** (`index_type='vamana'` under the `{ns}::vamana::{model}` key): the bridge
stops _writing_ them and, on first warm after upgrade, ignores any present-but-orphaned knowledge row
(a rebuild produces v2 segments instead). The table itself and every other consumer's rows are
untouched. **No table-drop migration is in scope.** Migrating the memory pack and the
retrieval-persist layer onto v2 segments — if ever desired — is a separate ADR coordinating all
consumers and all create sites.

This is consistent with khive's data-vs-view principle: the authoritative vectors remain in the
vector store; the ANN segment directory is a rebuildable index over them, not a second source of
truth.

### 2. Serving policy across the three warm states

The handler's behavior is defined by which of three states the warm path is in. This formalizes
and extends PR #335.

| Warm state | Condition                                                                                      | Serving behavior                                                                                                                                                                  |
| ---------- | ---------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Hot**    | segment loads, content hash matches the live corpus                                            | O(load) mmap restore; ANN fused normally.                                                                                                                                         |
| **Stale**  | segment loads, content hash does **not** match (corpus changed since the segment was written)  | **Serve the just-loaded stale graph** for fused recall while a background rebuild runs; hot-swap to the rebuilt index on completion. Gated by `ann_serve_stale` (default `true`). |
| **Cold**   | no segment loads (first run or corrupt), or `ann_serve_stale=false` on a content-hash mismatch | Degrade to FTS-only with `ann_unavailable: true` (PR #335), until the background rebuild completes.                                                                               |

**How the state is reached.** On warm the bridge attempts a raw segment `load` (`VamanaIndex::load`,
O(load) mmap) and then performs an explicit content-hash check of the loaded segment against the live
corpus. Load succeeds and hash matches → Hot. Load succeeds and hash mismatches → Stale: the
just-loaded graph becomes the transient serving source while a background rebuild runs, then
hot-swaps. No segment loads (absent or corrupt) → Cold. The bridge uses raw `load` plus an explicit
content-hash check rather than `load_or_build` for the warm decision, because `load_or_build` would
_synchronously rebuild_ on a mismatch and so could not hand back the stale graph to serve.

**Why serve-stale is correctness-safe.** The safety does **not** come from the fingerprint — the
fingerprint/content-hash governs only which index becomes _durable_, and says nothing about the rows
a stale graph returns. It comes from **hydration**: every ANN hit is resolved to its source row
through a `deleted_at IS NULL`-filtered query before it is returned (`knowledge/search.rs` hydration
of fused ANN hits). A stale `id_map` ordinal pointing at a since-deleted or re-indexed UUID therefore
yields a _phantom candidate that hydrates to nothing and is dropped_, never served content. The
residual cost of serving stale is bounded to **rank quality plus newly-added atoms not yet in the
graph**, not stale or deleted content. **Invariant the implementation must assert:** no ANN-sourced
id may bypass hydrated, `deleted_at`-filtered return.

The stale divergence is **not** necessarily small. The Stale state is entered on _any_ content-hash
mismatch, which includes a full reindex or a mass deletion, so the served graph can be arbitrarily
behind — bounded only by rebuild completion, which the O(load) restore keeps short. Serve-stale is a
deliberate availability trade during that window, gated by `ann_serve_stale`; the content hash still
governs what becomes the durable index.

### 3. Incremental maintenance replaces invalidate-on-write

The eager `invalidate_snapshot` `DELETE` on every vector write is removed. In its place:

- **Vector write** applies the delta to the live in-memory bridge via ADR-052 ops:
  `AnnBridge::insert` for new/updated vectors, `AnnBridge::tombstone` for hard-deleted/orphaned
  rows. The bridge is marked dirty.
- **The in-memory graph stays current via the deltas, not via the fingerprint.** Because every write
  applies an `insert`/`tombstone` to the live bridge, the served graph reflects the corpus
  continuously while the daemon is up — no fingerprint comparison is involved at serve time. The
  content hash matters only at the next daemon **restart**, classifying Hot / Stale / Cold (§2). A
  restart that loads slightly-behind segments lands in Stale and serves while it catches up; it no
  longer finds an empty table and rebuilds from zero.
- **Consolidation** runs when `ops_since_consolidation >= ann_consolidate_tau` (ADR-052 §2,
  default τ = 40_000), reclaiming tombstoned space and renumbering ordinals; the bridge applies the
  returned `new_to_old` remap to its id→ordinal table.
- **Incremental maintenance is not lossless (ADR-052 §"insert" trade-off).** ADR-052's incremental
  `insert` truncates overflow edges at serialization and relies on never-drop back-edges, so a node
  inserted since the last `consolidate` may temporarily lack medoid in-edges and not be immediately
  searchable until the next consolidation/redistribution pass. Recall over freshly-inserted atoms can
  therefore lag a full rebuild between consolidations. This ADR does not present incremental as
  strictly superior — it accepts the same trade-off ADR-052 documents, surfaced here because the live
  path now depends on it. The mitigations in scope are the consolidation cadence
  (`ann_consolidate_tau`) and the periodic checkpoint (§4); an operator who needs strict freshness
  over a write burst can lower τ.

`invalidate_snapshot` was redundant with the fingerprint gate (a stale snapshot is already rejected
on load) and harmful (it destroyed the only persisted copy, forcing the next cold start to rebuild).
It is deleted.

### 4. Periodic ANN checkpoint task

A background ANN checkpoint task, analogous to the existing WAL `run_checkpoint_task`, calls
`save_atomic` for each dirty bridge on an interval and/or dirty-op threshold. This guarantees a
recent durable snapshot almost always exists, so an unplanned restart loads (Hot/Stale) rather than
rebuilds (Cold). The checkpoint is best-effort and never blocks request serving; a checkpoint that
races a consolidation serializes after it (ADR-052's `save`/`to_snapshot` already cap medoid degree
at write time for a loader-valid graph).

### 5. Config surface — `[retrieval]` section in `khive.toml`

A new `[retrieval]` section carries the ANN operational knobs, parsed into a
`RetrievalSectionConfig` on `KhiveConfig` (parallel to the existing `RuntimeSectionConfig`) and
threaded through `RuntimeConfig` to the knowledge pack. All keys are optional and fall through to
env then built-in default, per ADR-035.

```toml
[retrieval]
ann_warm_timeout_ms     = 5000     # max wait for background warm before a query degrades (§2 Cold)
ann_serve_stale         = true     # serve a stale-but-loaded graph during rebuild (§2 Stale)
ann_checkpoint_interval_secs = 300 # periodic save_atomic cadence (§4); 0 disables periodic checkpoint
ann_consolidate_tau     = 40000    # ops_since_consolidation threshold (§3 / ADR-052 §2)
ann_persist_dir         = ""       # override the per-(ns,model) segment root; empty = backend data dir (§1)
```

ADR-035's CLI/env/config precedence table gains the corresponding rows:

| Option                  | CLI flag                | Env var                     | Config key                               | Default          |
| ----------------------- | ----------------------- | --------------------------- | ---------------------------------------- | ---------------- |
| ANN warm timeout (ms)   | `--ann-warm-timeout-ms` | `KHIVE_ANN_WARM_TIMEOUT_MS` | `retrieval.ann_warm_timeout_ms`          | `5000`           |
| ANN serve-stale         | `--ann-serve-stale`     | `KHIVE_ANN_SERVE_STALE`     | `retrieval.ann_serve_stale`              | `true`           |
| ANN checkpoint interval | `--ann-checkpoint-secs` | `KHIVE_ANN_CHECKPOINT_SECS` | `retrieval.ann_checkpoint_interval_secs` | `300`            |
| ANN consolidate τ       | `--ann-consolidate-tau` | `KHIVE_ANN_CONSOLIDATE_TAU` | `retrieval.ann_consolidate_tau`          | `40000`          |
| ANN persist dir         | `--ann-persist-dir`     | `KHIVE_ANN_PERSIST_DIR`     | `retrieval.ann_persist_dir`              | backend data dir |

The two hardcoded `WARM_TIMEOUT_MS` consts in `search.rs` are removed (and, once PR #335 has merged,
its `AtomicU64` test-override seam along with them): tests set the timeout through the same config
path as production.

## Rationale

- **Why wire rather than re-design.** ADR-052 already decided and shipped the persistence format
  and lifecycle ops at the crate layer. The only missing decision is integration: the data path
  from the consuming pack, the serving policy, and the config surface. Re-specifying the mmap
  format would duplicate ADR-052; this ADR references it.
- **Why filesystem segments over the SQLite BLOB.** `save_atomic`/`load_or_build` are defined over
  mmap-able segment files; the O(load) restore is an mmap of those segments. Round-tripping them
  through a SQLite BLOB would defeat the zero-copy load that is the entire point. Per-(ns, model)
  directories also make per-tenant isolation (ADR-067) and selective invalidation structural.
- **Why serve-stale defaults on.** The dominant failure the user feels is "no recall during warm"
  (#322). Serve-stale converts that to recall over a possibly-behind graph during warm, bounded by an
  O(load) rebuild. Hydration's `deleted_at` filter keeps deleted rows out of results (§2), so the
  cost is rank quality and not-yet-indexed atoms, not correctness; the content hash still prevents a
  stale graph from becoming the durable index.
- **Why a periodic checkpoint.** Persisting only after a full rebuild means the durable snapshot is
  absent exactly when it is most needed (right after writes). A cadence-driven `save_atomic` keeps a
  recent snapshot present so restarts load instead of rebuild.
- **Why config, not consts.** Warm tolerance, checkpoint cadence, and consolidation pressure depend
  on corpus size, hardware, and restart frequency — deployment facts, not compile-time constants.
  ADR-035 already defines exactly this CLI/env/config tier; the knobs join it.

## Alternatives Considered

| Alternative                                                 | Pros                        | Cons                                                                                               | Why rejected                                                                                             |
| ----------------------------------------------------------- | --------------------------- | -------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------- |
| Keep JSON BLOB; only tune background warm harder            | No new storage layout       | Restore stays O(deserialize) (~50–120 s at scale); writes still invalidate; root cause unaddressed | ADR-049 already deferred this to a real mmap snapshot; tuning a const does not remove the rebuild window |
| Store v2 segments as a SQLite BLOB                          | Single-file backup          | Defeats the zero-copy mmap load; re-introduces (de)serialization cost                              | The O(load) restore requires mmap-able files                                                             |
| Always rebuild on restart (drop persistence)                | Simplest; always correct    | O(N) every restart; the #322 window is permanent                                                   | ADR-052 built persistence specifically to avoid this                                                     |
| Keep invalidate-on-write, add incremental rebuild scheduler | Smaller diff to write path  | Still a destroy-then-rebuild model; snapshot still absent post-write                               | Incremental `insert`/`tombstone` already exist (ADR-052 §2); use them                                    |
| Serve-stale off by default (FTS-only during any rebuild)    | Never serves a behind-graph | Re-creates the #322 "no recall" window on every corpus change                                      | Bounded staleness during an O(load) window beats no recall; left as an opt-out flag                      |

## Migration path

0. **Backend data-dir accessor.** Add `backend_data_dir() -> Option<PathBuf>` to the backend handle /
   `KhiveRuntime` (ADR-028 surface addition) and thread it to the knowledge pack; `None` ⇒ ANN
   persistence disabled (Cold every warm). This is the precondition for steps 1–4.
1. **Bridge persistence swap.** `AnnBridge` gains `save_atomic`/`load`/`load_or_build` delegations to
   its `VamanaIndex`; warm restores via the §2 raw-`load`-plus-content-hash decision against the
   per-(ns, model) segment dir. Keep the JSON path readable for one release for fallback; stop
   writing the knowledge pack's own `index_type='vamana'` rows (other consumers' rows are untouched).
2. **Serving policy.** Implement the Hot/Stale/Cold states (§2); fold PR #335's `ann_unavailable`
   advisory into the Cold branch; add the background-rebuild-then-hot-swap for Stale.
3. **Incremental maintenance.** Replace `invalidate_snapshot` with `insert`/`tombstone` deltas on
   write; wire `consolidate` at τ with the ordinal remap applied to the id map.
4. **Checkpoint task.** Add the periodic ANN `save_atomic` task to the daemon, mirroring
   `run_checkpoint_task`; gate cadence on `retrieval.ann_checkpoint_interval_secs`.
5. **Config.** Add `RetrievalSectionConfig`, thread it through `RuntimeConfig`, add the CLI flags
   and env vars; remove the two `WARM_TIMEOUT_MS` consts (and PR #335's `AtomicU64` override if #335
   has merged).

No table-drop step. `retrieval_snapshots` stays in place for its other consumers (§1); the only
change to it is that the knowledge bridge stops writing its own rows. Each step is independently
testable. Step 0 is the precondition; steps 1–2 land the #322 root-cause fix and can ship before
3–5; step 5 supersedes the PR #335 timeout const.

## Consequences

### Positive

- Daemon restart over an unchanged or lightly-changed corpus becomes an O(load) mmap restore
  instead of an O(N) rebuild — the optimization ADR-049 deferred and ADR-052 built is finally on
  the live path.
- The #322 warm window shrinks from "every restart after any write" to "first run and corrupt
  snapshot only," and even then serve-stale (when a prior snapshot exists) keeps recall available.
- Writes no longer destroy the durable index; incremental `insert`/`tombstone` keep it current and
  a periodic checkpoint keeps it durable.
- Operational knobs are configurable per deployment through the established ADR-035 tier; the
  test-only `AtomicU64` override is removed.

### Negative

- ANN state moves from a single SQLite table to per-(ns, model) segment directories on disk;
  operators backing up only the `.db` file must also include the `ann/` directory (or accept an
  O(rebuild) first warm after restore). Mitigation: segments are recomputable; a missing `ann/`
  dir rebuilds, it does not corrupt.
- Serve-stale introduces a window where fused recall reflects a possibly-behind graph (bounded by
  rebuild completion, not necessarily small). Hydration's `deleted_at` filter keeps deleted rows out
  of results, so the cost is rank quality and not-yet-indexed atoms, not stale/deleted content.
  Mitigation: `ann_serve_stale=false` restores strict FTS-only-during-rebuild behavior; the content
  hash still gates the durable index.
- More moving parts in the write path (incremental ops, consolidation, checkpoint task). Mitigation:
  all primitives are ADR-052-tested at the crate layer; integration adds wiring and bridge-level
  tests, not new index algorithms.

### Neutral

- No change to the `khive-vamana` crate's public surface or to ADR-052's design; this ADR consumes
  it. No change to `khive-quant` or the `VectorStore` trait. It does add a `backend_data_dir()`
  accessor to the backend handle / `KhiveRuntime` (§1, step 0) — an ADR-028 surface addition.
- The `retrieval_snapshots` table is unchanged and remains owned by the memory pack and the
  retrieval-persist layer; this ADR drops no table and adds no schema migration. The only
  DB-adjacent change is that the knowledge bridge stops writing its own `vamana` rows.

## References

- [ADR-005](ADR-005-storage-capability-traits.md) — `VectorStore`; authoritative vectors the ANN
  segment directory indexes over
- [ADR-028](ADR-028-pack-scoped-backends.md) — pack-assigned backend resolves `<backend_data_dir>`;
  this ADR adds the `backend_data_dir()` accessor (§1, step 0)
- [ADR-062](ADR-062-fts-ann-consolidation.md) — also a `retrieval_snapshots` consumer (stale-snapshot
  purge); unaffected — this ADR drops no table
- [ADR-030](ADR-030-retrieval-stack-port.md) — khive-vamana consumed as the knowledge-pack ANN bridge
- [ADR-035](ADR-035-cli-config-and-auto-embed.md) — CLI/env/config precedence; this ADR adds the
  `[retrieval]` rows
- [ADR-049](ADR-049-khived-daemon.md) — daemon warm-state owner; deferred the mmap snapshot to this ADR
- [ADR-051](ADR-051-section-embeddings-hybrid-compose.md) — knowledge compose/recall fusion the warm path feeds
- [ADR-052](ADR-052-ann-production-lifecycle.md) — v2 persistence, incremental insert, tombstone,
  consolidate; the design this ADR integrates
- [ADR-067](ADR-067-write-owner-daemon.md) — per-tenant write-owner model; ANN segments are per-tenant
- [#322](https://github.com/ohdearquant/khive/issues/322) / [PR #335](https://github.com/ohdearquant/khive/pull/335)
  — the warm-degrade symptom and its safety-net fix, subsumed by §2 and §5
