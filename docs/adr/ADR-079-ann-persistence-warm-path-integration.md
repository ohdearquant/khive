# ADR-079: ANN Persistence Warm-Path Integration

**Status**: Accepted\
**Date**: 2026-06-28\
**Authors**: khive maintainers

---

## Context

Three accepted ADRs define the surrounding contract:

- [ADR-049](./ADR-049-khived-daemon.md) owns daemon warm-state startup and nonblocking request
  service.
- [ADR-052](./ADR-052-ann-production-lifecycle.md) defines crash-safe Vamana persistence,
  incremental insert, tombstone deletion, consolidation, and SQ8 acquisition data.
- [ADR-035](./ADR-035-cli-config-and-auto-embed.md) defines configuration precedence.

This ADR connects those pieces. The goal is to restore a valid file-backed ANN segment when
possible, keep the in-memory index current after writes, and bound the unavailable window when a
rebuild is required. ANN files remain derived state; authoritative vectors remain in the vector
store.

## Base decision

### 1. Persist crash-safe segment directories

The ANN bridge delegates persistence to `VamanaIndex::save_atomic` and restoration to the v2 load
path from ADR-052. Segment files live beside the owning database:

```text
<db-file>.ann/<hex(namespace::vamana::model)>/
    metadata.bin
    graph.bin
    vectors.bin
    lifecycle.bin
    external_ids.bin
    codes.bin
```

`metadata.bin` is the commit record and is written last through temporary-file and rename steps.
Every required segment has a checksum in that record. `external_ids.bin` maps ANN ordinals to
record identifiers and carries the same commit identity. `codes.bin` stores the SQ8 codec and
codes as specified by Amendment 1 below.

The root is database-scoped so two database files in one parent directory cannot adopt each
other's segments. Older layouts outside `<db-file>.ann` are not adopted. Their absence from the new
root produces a Cold classification and a rebuild.

Pathless and in-memory backends return `None` from `backend_data_dir()`. For those backends,
persistence is disabled and warm-up always starts from the authoritative vector store.

The generic `retrieval_snapshots` table is not dropped. Other retrieval consumers may continue to
use it. The Vamana bridge stops writing its own rows there after the file-backed path is enabled.

### 2. Classify warm state explicitly

The base warm path recognizes three states:

| State   | Condition                                               | Serving behavior                                                           |
| ------- | ------------------------------------------------------- | -------------------------------------------------------------------------- |
| `Hot`   | A valid segment represents all committed vector changes | Load the segment and use ANN normally                                      |
| `Stale` | A valid segment exists but later vector changes remain  | Serve it only when `ann_serve_stale=true` while catch-up or rebuild runs   |
| `Cold`  | No valid segment can be trusted                         | Serve the lexical path with `ann_unavailable=true` until rebuild completes |

Stale serving is safe only because every ANN identifier is hydrated from the authoritative store
and filtered for visibility and deletion before return. A stale hit that no longer resolves is
dropped. Staleness may affect rank quality and omit newly added records, but it must never expose a
deleted or invisible record.

The durable classifier is refined by Amendment 1. A segment checksum proves file integrity, while
the write log proves whether committed vector changes remain to be applied.

### 3. Maintain the live index incrementally

Vector writes do not delete the durable segment. Instead:

- inserts and updates call `AnnBridge::insert`;
- deletions and orphan cleanup call `AnnBridge::tombstone`;
- every successful mutation marks the bridge dirty;
- consolidation runs after `ann_consolidate_tau` operations and applies the returned ordinal remap
  to the identifier map.

Incremental insertion has the recall tradeoff documented in ADR-052. Newly inserted nodes may have
lower recall until consolidation. The cadence is configurable, and no text in this ADR claims that
incremental maintenance is equivalent to a fresh full build.

### 4. Checkpoint dirty bridges

A background task calls `save_atomic` for dirty bridges on the configured interval. Checkpointing
is best effort and does not block request serving. A successful checkpoint clears dirtiness only
through the captured generation; writes committed after that generation remain dirty.

Publication is atomic. A failed or interrupted checkpoint leaves the previous complete segment
available.

### 5. Configuration surface

ANN lifecycle settings live under `[retrieval]`:

```toml
[retrieval]
ann_warm_timeout_ms = 5000
ann_serve_stale = true
ann_checkpoint_interval_secs = 300
ann_consolidate_tau = 40000
ann_rebuild_threshold = 0.20
ann_persist_dir = ""
```

| Option                         | Environment variable          | Constraint                             |
| ------------------------------ | ----------------------------- | -------------------------------------- |
| `ann_warm_timeout_ms`          | `KHIVE_ANN_WARM_TIMEOUT_MS`   | Nonnegative integer                    |
| `ann_serve_stale`              | `KHIVE_ANN_SERVE_STALE`       | Boolean                                |
| `ann_checkpoint_interval_secs` | `KHIVE_ANN_CHECKPOINT_SECS`   | `0` disables periodic checkpoints      |
| `ann_consolidate_tau`          | `KHIVE_ANN_CONSOLIDATE_TAU`   | Positive integer                       |
| `ann_rebuild_threshold`        | `KHIVE_ANN_REBUILD_THRESHOLD` | Floating point in `(0, 1]`             |
| `ann_persist_dir`              | `KHIVE_ANN_PERSIST_DIR`       | Empty selects the backend-derived root |

Per ADR-035, an explicit CLI value overrides the environment, which overrides TOML, which
overrides the built-in default. Tests set values through this same configuration path.

## Amendment 1: Delta-log restart classifier and mmap re-adoption

**Status**: Accepted\
**Date**: 2026-07-19

### Context

A content-hash classifier requires reading the full vector corpus at startup even when the segment
is current. A full rebuild also produces owned vector storage, so writing a segment does not by
itself return the served index to file-backed storage. At hundreds of thousands of vectors, both
costs are material.

This amendment replaces corpus-wide freshness scans with a transactional write log and reopens
every successful checkpoint through the mmap load path.

### A. Persist vector mutations in `ann_write_log`

```sql
CREATE TABLE ann_write_log (
  seq             INTEGER PRIMARY KEY AUTOINCREMENT,
  namespace       TEXT NOT NULL,
  embedding_model TEXT NOT NULL,
  kind            TEXT NOT NULL,
  field           TEXT NOT NULL,
  subject_id      TEXT NOT NULL,
  op              TEXT NOT NULL CHECK (op IN ('upsert', 'delete'))
);

CREATE INDEX idx_ann_write_log_ns_model_seq
  ON ann_write_log(namespace, embedding_model, seq);

CREATE TABLE ann_consumer_watermark (
  consumer        TEXT NOT NULL,
  namespace       TEXT NOT NULL,
  embedding_model TEXT NOT NULL,
  watermark       INTEGER NOT NULL,
  PRIMARY KEY (consumer, namespace, embedding_model)
);
```

Every vector insert, update, delete, merge cleanup, and orphan sweep appends the corresponding log
row in the same database transaction as the vector mutation. A rollback therefore leaves neither
change visible.

Each ANN consumer has a stable scope predicate over namespace, model, kind, and field. It registers
its watermark row at zero before persisting or serving an extended-format segment. Consumers whose
scope spans every namespace register with the reserved `*` namespace marker.

The segment commit record stores `last_applied_seq = S`. `S` is captured in the same read snapshot
as the data serialized into that segment.

### B. Restart classification is total and ordered

For each consumer scope, the first matching rule wins:

| Rule | Condition                                                            | Result                                                             |
| ---: | -------------------------------------------------------------------- | ------------------------------------------------------------------ |
|    1 | Commit record is missing, corrupt, or has an unknown length          | `Cold`                                                             |
|    2 | Record predates write-log watermarks or `codes.bin`                  | `Cold`; do not compact its log                                     |
|    3 | Configured model dimensions differ from segment dimensions           | `Cold`                                                             |
|    4 | Consumer watermark registration is absent                            | Re-register at zero, then `Cold`                                   |
|    5 | The source corpus for the consumer scope is empty                    | `Empty`; drop the index and serve no ANN candidates                |
|    6 | No scoped log row has `seq > S`                                      | `Hot`; mmap load without a corpus scan                             |
|    7 | Tail count is at or below `ceil(ann_rebuild_threshold * live_count)` | `Stale-tail`; mmap load and replay final-state deltas              |
|    8 | Tail count exceeds the threshold                                     | `Stale-rebuild`; optionally serve the segment while a rebuild runs |

The liveness classifier no longer hashes every vector at startup. Segment checksums remain
integrity checks for persisted files.

### C. Replay final state, not every intermediate event

Tail replay keeps the highest sequence per `subject_id`:

- final `upsert`: point-read the current scoped vector, tombstone the old ordinal if present, and
  insert the current vector once;
- final `delete`: tombstone the mapped ordinal, or do nothing when no mapping remains;
- final `upsert` with no source row: treat it as a delete only when the consumer's join-filtered
  scope intentionally excludes that row; otherwise classify `Cold`;
- inconsistent live identifier ownership: classify `Cold`.

This rule handles repeated updates and delete-update interleavings without reconstructing values
that are no longer authoritative.

Join-filtered consumers must apply the same predicate during hydration. A soft-deleted source may
remain as a segment candidate until a checkpoint, but it is never returned because hydration
rejects it.

### D. Compact logs through the minimum consumer watermark

After a successful segment commit at `S`, the consumer raises its watermark monotonically. Log
rows for one namespace-model pair may be deleted only through the minimum watermark of every
registered consumer whose scope includes that pair, including wildcard registrations.

Conceptually:

```sql
DELETE FROM ann_write_log
WHERE namespace = ?1
  AND embedding_model = ?2
  AND seq <= (
    SELECT MIN(watermark)
    FROM ann_consumer_watermark
    WHERE (namespace = ?1 OR namespace = '*')
      AND embedding_model = ?2
  );
```

An empty registration set yields no deletion. A stale registration may delay compaction but cannot
cause required log rows to be removed. Removing a retired consumer registration is an explicit
administrative action; if that consumer later returns, rule 4 forces a Cold rebuild.

### E. Re-adopt checkpoints through mmap

After `save_atomic`, the bridge reopens the new segment through the mmap loader. The candidate
segment carries watermark `S` and may replace the served index only when the bridge's current
applied watermark still equals `S`.

If newer writes landed during reopen, the bridge either replays them onto the candidate before
publication or keeps the newer owned index until the next checkpoint. A newer served generation is
never replaced by an older one. In-flight queries retain their existing `Arc` and complete against
one stable generation.

`codes.bin` is checksummed with the other files and memory-mapped on load. This avoids rebuilding
the codec and allocating corpus-sized codes whenever a checkpoint is adopted.

### F. Residency invariant

The amendment guarantees that served vectors and SQ8 codes are file-backed after successful
checkpoint adoption. It does not claim that all ANN state is file-backed. Graph adjacency,
identifier maps, and lifecycle structures may remain owned memory and may scale with the corpus.

Any future residency optimization must publish a reproducible benchmark using public or synthetic
data and must report file-backed and anonymous memory separately. Operational database sizes,
process identifiers, and machine-specific allocator observations are not part of this ADR.

### G. Identifier ownership at replay

A tombstoned ordinal has no owner. The reverse identifier map excludes tombstoned entries, or
equivalently clears ownership when tombstoning occurs. Because insert may recycle a tombstoned slot,
a stale reverse mapping must never allow a later delete for the former subject to target the new
owner.

## Crash-consistency invariants

1. Vector mutation and write-log append commit in one transaction.
2. Segment contents and `S` are captured from one read snapshot.
3. The commit record becomes visible only after all required files are durable.
4. Consumer watermarks rise only after the segment commit succeeds.
5. Log compaction occurs only after watermark publication and never beyond the consumer minimum.
6. Mmap adoption publishes only a current or fully replayed generation.
7. File replacement does not invalidate mappings held by in-flight queries.

## Rebuild threshold

`ann_rebuild_threshold = 0.20` is a conservative policy default, not a performance guarantee. Tail
replay performs point reads and incremental graph updates, while a rebuild can use batch locality
and parallel construction. Implementations may change the default only with a checked-in,
reproducible benchmark that compares both paths at fixed recall and includes the configuration,
hardware, dataset source, and raw results.

## Consequences

### Positive

- A current segment reaches `Hot` without scanning the vector corpus.
- Writes preserve recoverable segment state instead of deleting it.
- Small tails can be replayed without a full rebuild.
- Checkpoint adoption returns vectors and codes to file-backed storage.
- The classifier and crash windows are explicit and testable.

### Tradeoffs

- Every vector mutation adds one transactional log row.
- Consumer registrations require lifecycle management before log compaction.
- Stale serving can reduce rank quality until replay or rebuild completes.
- Some corpus-proportional structures remain in owned memory.

## Testing requirements

- Vector mutation and log append roll back together.
- Each classifier rule has a deterministic synthetic fixture.
- Hot classification performs no full-corpus vector read.
- Replay coalesces repeated mutations to one final-state action.
- Watermark compaction preserves every slower consumer's tail.
- A missing consumer registration forces Cold.
- Concurrent writes cannot be lost during checkpoint publication.
- A torn or mismatched `codes.bin` prevents segment adoption.
- Hydration filters deleted and invisible stale candidates.
- Tombstone, slot reuse, and a later stale delete cannot remove the new occupant.

## References

- [ADR-005](./ADR-005-storage-capability-traits.md): vector-store capability boundary
- [ADR-028](./ADR-028-pack-scoped-backends.md): backend assignment and data-directory access
- [ADR-035](./ADR-035-cli-config-and-auto-embed.md): configuration precedence
- [ADR-049](./ADR-049-khived-daemon.md): daemon warm-state ownership
- [ADR-052](./ADR-052-ann-production-lifecycle.md): persistence and incremental ANN lifecycle
- [ADR-062](./ADR-062-fts-ann-consolidation.md): other snapshot-table consumers
