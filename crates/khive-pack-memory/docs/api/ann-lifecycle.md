# ANN Index Lifecycle

The memory pack keeps one Vamana approximate-nearest-neighbor (ANN) index per embedding model, spanning every namespace. This reference explains the cache, freshness signals, snapshot validation, rebuild coordination, and the public epoch helpers.

## `AnnKey` and `AnnState`

`AnnKey` is model-only even though its constructors accept a namespace or token for compatibility. Namespace isolation happens after global ANN search, not by constructing a separate graph for every namespace.

`AnnState` owns five production coordination mechanisms:

| State | Purpose |
| --- | --- |
| `indexes` | Installed `AnnBridge` values, keyed by model. |
| `warming` | Synchronous fire-once guard for background rebuild tasks. |
| `model_locks` | Async per-model single-flight locks shared by boot warm, background warm, and cold recall. |
| `generations` | In-process monotonic write generation for each model. |
| `last_epoch_check` | Debounce timestamps for the durable epoch query. |

`warming` deliberately uses `std::sync::Mutex`: its critical sections never await, and `WarmingGuard::drop` must release the key synchronously on success, error, or panic. The model-lock map is separate: `warming` prevents duplicate fire-and-forget tasks, while `model_locks` lets concurrent callers wait for the same actual warm attempt. The outer map lock is held only while looking up an `Arc<tokio::sync::Mutex<()>>`, so unrelated models do not contend during builds.

Test-only notifications form deterministic barriers around rebuild attempt selection and guard release. They are scoped to one `AnnState`, avoiding process-global cross-test signal theft. `wait_until_warm_idle` observes actual tracked-task completion rather than relying on sleeps or merely observing that a warm was scheduled.

## `AnnBridge`

An `AnnBridge` contains the Vamana index, the UUID position map, the set of namespaces present in the corpus, and two freshness stamps:

- `generation` is the in-process write-generation floor captured before scanning.
- `epoch_baseline` is the durable database epoch observed when the graph was built or restored.

The namespace set lets the recall over-fetch loop skip widening when the global graph contains no namespaces outside the caller's visible set. Search returns cosine-like scores and rejects dimension mismatches through the underlying index contract.

`install_if_fresher` replaces an existing entry only when the candidate generation is strictly newer. Equal generations keep the existing entry; an older build can never overwrite a newer one, while a later build that covers more writes always wins. This replaced `or_insert`, whose winner depended on lock acquisition order rather than corpus freshness.

## `bump_generation` and stale serving

Every path that may mutate memory vectors bumps the affected model generation: `memory.remember`, `memory.prune`, and the KG note-mutation hook used by generic update, delete, and merge operations.

Since issue #791, a write does not clear the installed graph or delete its snapshot. The old graph remains an intact, stale fallback while `ensure_ann_background` builds a replacement. `search_loaded` intentionally does not consult freshness; the recall coordinator checks `is_current`, may serve the installed fallback, and schedules rebuilding. This avoids turning every write into a latency spike without merging incompatible graph generations.

`installed_is_fresh(key, minimum)` checks an explicit caller floor. `is_current(key)` compares the installed graph with the latest generation recorded in this process. An absent graph and a graph older than a committed write both return false and enter the same warm path.

## Durable epoch helpers

`ensure_ann_epoch_schema` creates the singleton `memory_ann_epoch` table idempotently. Normal daemon boot applies the table through `MemoryPack::SCHEMA_PLAN`; `kkernel reindex` calls the public helper because it works with a raw runtime and may run before any pack registry has booted.

`bump_memory_ann_epoch` increments the singleton row and returns the new `u64` epoch. Reindex calls it after invalidating the persisted snapshot. Both public functions return `RuntimeError` if the database write or schema application fails.

`durable_epoch` is best effort and returns zero when the table, row, reader, or query is unavailable. Zero means that no durable invalidation has been observed, matching a new bridge's baseline.

`maybe_check_durable_epoch` compares the installed baseline with the database value. When the durable epoch advances, it bumps the in-process generation so the ordinary `is_current` and background-rebuild machinery handles the change. Production checks are debounced to once per model every five seconds; tests use a zero interval for deterministic single-call coverage. The check returns immediately when no graph is installed because a genuine miss will read the epoch during its normal build.

This durable signal is necessary because `kkernel reindex` runs in another process. It changes vector rows and removes snapshots without access to a warm daemon's generation map; without the epoch, that daemon could serve pre-reindex vectors indefinitely.

## `ensure_ann_background`

`ensure_ann_background` is the non-blocking stale-cache path. It captures the current generation before its fast-path freshness test, synchronously claims the `warming` key, and registers a tracked runtime task. Tracking matters: daemon shutdown drains these tasks instead of abandoning an unaccounted `tokio::spawn`.

The task runs a bounded sequence of rebuild attempts. A write that lands during an attempt makes its generation floor stale. The task retries against the newer floor; if the attempt bound is exhausted and the graph is still stale, it releases `warming` before re-enqueueing. Releasing first is load-bearing because the chained call must be able to claim the same key. Chained tasks delay one second in production (five milliseconds in tests) so continuous writes coalesce instead of producing an unbounded immediate rebuild chain.

The RAII `WarmingGuard` owns guard release on every exit path. Benign shutdown cancellation is logged at debug level; genuine build errors remain warnings.

## `ensure_ann_for_model`

`ensure_ann_for_model` is the shared warm chokepoint for boot, background work, and cold recall. It:

1. Captures the model's generation before any fast path.
2. Returns `AlreadyLoaded` when the installed graph satisfies that floor.
3. Acquires the per-model single-flight lock and repeats the freshness check.
4. Emits one ANN-warm phase span for the caller that actually warms.
5. Restores a valid snapshot or rebuilds from the vector store.

Only one concurrent caller emits the phase start/completion pair. Resource accounting snapshots cumulative CPU at entry and exit and reports the delta, while an RAII active-phase guard lets health reporting observe `ann_warm` during execution. Corpus size is diagnostic and best effort; failure to count does not fail warming. Phase-event append failures are also best effort and never change the warm result.

The function returns an `AnnEnsureStatus` distinguishing already loaded, restored snapshot, built graph, empty corpus, and a build discarded because the corpus changed during scanning.

## Snapshot restore and rebuild

Snapshot validation uses two signals:

- `CorpusFingerprint`: vector count and dimensions, used as a cheap first gate.
- `CorpusContentHash`: BLAKE3 over ordered `(subject_id, raw_embedding_bytes)` rows.

Count and dimensions alone cannot detect delete-one/add-one replacement, content re-embedding, or vector-only reindexing. In-process generations also reset on restart. The content hash detects all of those changes, including a reindex that does not update `notes.updated_at`. The expensive hash scan runs only when the cheap fingerprint matches.

Pre-hash snapshots deserialize as corrupt under the wrapper format and self-heal by rebuilding. Restored graphs repopulate their namespace set with a distinct-namespace query before installation.

A rebuild uses a fingerprint sandwich around the corpus scan. If the before and after fingerprints differ, the result is discarded. The scan calculates its content hash in the same ordered loop that feeds graph construction, so the persisted signal describes exactly the rows used by that graph. The captured generation closes the remaining race after the second fingerprint and before persistence or installation.

Graph construction trains SQ8 quantization and builds Vamana synchronously, so it runs in `spawn_blocking` and cannot monopolize a Tokio worker. Join errors remain typed storage errors so shutdown cancellation can still be classified instead of being flattened into an opaque string.

## Warm boot and namespace filtering

`warm_existing_memory_indexes` schedules indexes for registered embedding models at pack warm time. ANN search remains global, then recall post-filters hydrated hits to the token's visible namespaces. The namespace-aware over-fetch algorithm is documented in `crates/khive-pack-memory/docs/api/recall-pipeline.md`.

## Ordering and atomicity guarantees

- Generation is captured before a build or fast-path decision.
- An older generation never replaces a newer installed graph.
- The background guard is released before chained re-enqueue.
- Snapshot content hash and graph input come from the same ordered scan.
- Namespace filtering occurs before ANN hits become caller-visible.
- Writes preserve an intact prior graph until a complete replacement installs.
