# WAL-Pin Attribution — Backend-Scoped Transaction Registry

**Status**: draft\
**Date**: 2026-07-19\
**Authors**: khive maintainers\
**Tracking**: issue #1160\
**Related**: ADR-091 (WAL snapshot lifetime) and its Amendment 2 (attribution sidecar)

---

## Problem

The open-transaction registry (`khive_storage::tx_registry`, ADR-091 Plank 0) is
process-global: `register(label)` records only an opened-at instant and an optional
diagnostic label, and `oldest()` returns the oldest span across the whole process.
Both attribution consumers observe this global view but act on exactly one database:

- The per-session sweep (`run_session_sweep_task`) writes heartbeats only into the
  sidecar directory of the database path it was spawned with — the main backend.
- The daemon checkpoint task (`run_checkpoint_task`) observes the same global
  `oldest()` while checkpointing one specific pool.

On a multi-backend process (for example a dedicated map database alongside the main
store) this produces two failures:

1. **Wrong-database attribution.** A long transaction on a secondary backend is
   reported under the main database's sidecar, pointing a checkpoint stall at a
   database that is not pinned.
2. **Invisible backends.** Secondary databases get no sweep coverage and no sidecar
   entries at all; their WAL pins cannot be attributed by any enumerator.

## Requirement

Attribution must be keyed to the database whose WAL is actually pinned. The sidecar
contract is already scoped that way — each database file owns a `<db-file>.walpin/`
directory — so the gap is entirely on the producer side: registry entries do not say
which database they belong to.

## Design forks

### Fork A — origin identity on registry entries (recommended)

Registry entries gain an optional origin: the canonical path of the database the
transaction runs against.

```rust
// khive-storage (additive; existing register() delegates with origin = None)
pub fn register_scoped(label: Option<String>, origin: Option<Arc<str>>) -> TxHandle;
pub fn oldest_for(origin: Option<&str>) -> Option<(TxId, Duration, Option<String>)>;
// oldest() retained unchanged: the process-wide aggregate view.
```

- **Registration sites** (all in `khive-db`: `WriterGuard::transaction`,
  `atomic_unit`'s span, the raw batch-writer spans) thread the pool's canonical
  database path as the origin. The pool already knows its path; no new state.
- **The session sweep** keeps its single task but fans out per backend: one
  `WalpinSidecarState` per file-backed backend, each observing
  `oldest_for(<that backend's canonical path>)` and writing into that backend's own
  sidecar directory.
- **The daemon checkpoint task** switches its observation to
  `oldest_for(<its pool's canonical path>)`, so a stall on pool X is never blamed
  on a transaction pinning pool Y.
- **Enumeration side needs zero change**: readers already enumerate per sidecar
  directory.

### Fork B — per-backend registry instances

Replace the global static with a registry owned by each pool, threaded through every
registration site.

Rejected: every caller of `register` (guards, atomic units, batch writers) would need
a registry handle plumbed through, inverting the current zero-argument seam for no
attribution gain over Fork A. The global-static registry with origin filtering
provides the same partitioning with an additive API.

## Decisions folded into Fork A

- **Origin identity is the canonical database path**, not the configured backend
  name. The path is the same key the sidecar contract uses (`<db-file>.walpin/`),
  is stable across processes that open the same database, and requires no config
  plumbing. Backend names are per-process configuration and can differ between a
  session and the daemon observing the same file.
- **Memory backends are out of scope.** No database file means no WAL, no pins, and
  no sidecar directory; their transactions register with `origin = None`.
- **Unscoped entries keep today's behavior.** An entry with no origin (external
  callers, tests, not-yet-threaded sites) is observed by the main backend's sidecar
  state, exactly as every entry is today. Scoping narrows attribution; it never
  silently drops a span from observation. All in-tree registration sites are
  threaded in the same change, so unscoped entries should not occur from khive's own
  write paths.
- **`oldest()` survives as the aggregate view** for consumers that genuinely want
  the process-wide oldest span (logging, diagnostics). Attribution consumers move to
  `oldest_for`.

## Test plan

- Registry unit tests: `oldest_for` partitions by origin; unscoped entries appear
  under `oldest_for(None)` and in `oldest()`; scoped entries never leak into a
  different origin's view.
- Sweep integration test: two file-backed pools in one process; a long-running
  transaction on the secondary backend produces a heartbeat in the **secondary**
  database's sidecar directory and none in the main database's sidecar.
- Daemon-side test: checkpoint attribution for pool X ignores a span registered
  against pool Y.

## Rollout

Single implementation PR after this note is accepted: additive `khive-storage` API,
origin threading at the `khive-db` registration sites, session-sweep fan-out, and
the daemon checkpoint task's scoped observation, with the tests above. No schema,
wire, or sidecar-format change; ADR-091 Amendment 2's sidecar contract is unchanged
— this note only closes the producer-side scoping gap against the contract's
existing per-database key.
