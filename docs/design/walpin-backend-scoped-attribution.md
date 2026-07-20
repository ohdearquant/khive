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

Registry entries gain an explicit origin describing which database, if any, the
transaction runs against. Origin is a three-state contract, not an option type —
"no database file" and "not yet threaded" are different facts and must never
share a representation:

```rust
// khive-storage (additive; existing register() delegates with TxOrigin::Unscoped)
pub enum TxOrigin {
    /// A file-backed database, identified by the shared lossless identity below.
    Database(DbIdentity),
    /// An in-memory backend: no database file, no WAL, no sidecar — excluded
    /// from WAL-pin attribution entirely.
    Memory,
    /// A registration site that has not been threaded (external callers,
    /// tests). Observed by the main backend's attribution view, exactly as
    /// every entry is today, so scoping can never silently drop a span.
    Unscoped,
}
pub fn register_scoped(label: Option<String>, origin: TxOrigin) -> TxHandle;
pub fn oldest_for(filter: &TxOriginFilter) -> Option<(TxId, Duration, Option<String>)>;
// oldest() retained unchanged: the process-wide aggregate view.
```

The main backend's attribution view uses a filter matching
`Database(main) | Unscoped`; a secondary backend's view matches only
`Database(that backend)`; `Memory` matches no attribution view.

- **Database identity** (`DbIdentity`) is a shared key produced at exactly
  one minting point — the pool — and reused everywhere origin is compared.
  Minting is defined operationally: filesystem-canonicalize the database
  file's **parent directory** (resolving symlinks and dot segments through
  the part of the path that exists), then append the file name unchanged.
  The database file itself may not exist yet at open time, so only the
  parent is canonicalized — the same pattern `FsBlobStore` uses for its
  root-keyed write locks, and for the same reason (`Path::canonicalize`
  requires an existing path). Canonicalization is what collapses aliased
  spellings — symlink vs. target, relative vs. absolute — into one
  identity, so a daemon and a session opening the same database through
  different spellings mint the same `DbIdentity`.
- **Canonical and lossless are reconciled explicitly**, because they pull in
  opposite directions: _canonical_ collapses aliases; _lossless_ governs the
  representation of the canonical path once minted (`OsString`-based, never
  lossy UTF-8 conversion — the backend layer deliberately preserves
  non-UTF-8 path identity to avoid database collisions, and origin must not
  be weaker than that). Lossless does **not** mean preserving pre-canonical
  alias spellings; those are collapsed by design.
- **Sidecar derivation consumes the minted `DbIdentity`**, never the raw
  configured path. Today's `sidecar_dir_for` is a purely lexical sibling
  derivation, so two aliased openers would land on two different sidecar
  directories; the implementation routes it through the minted identity so
  a sidecar directory and the spans attributed to it can never disagree
  about which database they mean. Sidecar contents are ephemeral
  observability records (heartbeats, beacons), so re-keying the derivation
  requires no migration.
- **Registration sites**: every `khive_storage::tx_registry::register` call
  site is threaded in the same change — the current inventory spans
  `pool.rs` (`WriterGuard::transaction`), `writer_task.rs` (the batch-writer
  span), `stores/graph.rs` (`graph_upsert_edges`, `graph_upsert_edge_guarded`,
  the guarded-edge span, and `graph_traverse_read` — the long-lived deferred
  read snapshot held across traversal chunks, which is precisely the span
  class attribution exists for), and the daemon's own registration sites in
  `khive-runtime`. The implementation PR gates completeness with a grep over
  `tx_registry::register` — the list above is the audit baseline, not the
  contract; any site added later must register scoped or is a review defect.
- **The session sweep** keeps its single task but fans out per backend: one
  `WalpinSidecarState` per file-backed backend, each observing its backend's
  filtered view and writing into that backend's own sidecar directory.
- **The daemon checkpoint task** switches its observation to its own pool's
  filtered view, so a stall on pool X is never blamed on a transaction pinning
  pool Y.
- **Enumeration ownership must extend, not stay put.** The per-directory
  enumeration _mechanism_ is unchanged, but today the daemon wires a
  checkpoint pool — and therefore stall detection and TRUNCATE-time
  attribution enumeration — for the **main** backend only
  (`build_server_from_multi_backend_registry` calls `checkpoint_pool_for` on
  `main_backend` alone). Writing secondary sidecars without extending
  ownership would produce heartbeats nothing ever reads. The implementation
  PR therefore extends daemon checkpoint/enumeration ownership to every
  file-backed backend (one checkpoint task per file-backed pool, each owning
  its own sidecar enumeration), and the sweep fan-out lands in the same PR so
  producers and consumers of secondary sidecars ship together — never one
  without the other.

### Fork B — per-backend registry instances

Replace the global static with a registry owned by each pool, threaded through every
registration site.

Rejected: every caller of `register` (guards, atomic units, batch writers) would need
a registry handle plumbed through, inverting the current zero-argument seam for no
attribution gain over Fork A. The global-static registry with origin filtering
provides the same partitioning with an additive API.

## Decisions folded into Fork A

- **Origin identity is the minted `DbIdentity`, not the configured backend
  name.** The canonical identity is the same key the sidecar contract uses
  (`<db-file>.walpin/`), is stable across processes that open the same database
  — including through aliased spellings, which minting collapses — and requires
  no config plumbing. Backend names are per-process configuration and can
  differ between a session and the daemon observing the same file. `DbIdentity`
  is minted at one point (the pool, per the operational definition above); no
  other layer constructs, normalizes, or re-canonicalizes it.
- **Memory backends register `TxOrigin::Memory`, never `Unscoped`.** No database
  file means no WAL, no pins, and no sidecar directory. Conflating "memory" with
  "unscoped" would either drop unscoped spans from observation (exact filtering)
  or falsely attribute memory spans to the main database (fallback filtering) —
  the three-state contract exists precisely to keep these apart.
- **`Unscoped` entries keep today's behavior — and are labeled as such.** They
  are observed by the main backend's attribution view, exactly as every entry
  is today. Scoping narrows attribution; it never silently drops a span from
  observation. A heartbeat whose oldest span carries `Unscoped` origin is
  written with an explicit fallback-attribution marker, so diagnostics can
  distinguish "attributed to main by evidence" from "attributed to main as the
  fallback for unknown origin" and never read fallback attribution as ground
  truth. The marker is one additive heartbeat-record field; the corresponding
  record-contract text in ADR-091 Amendment 2 is amended alongside. All in-tree
  registration sites are threaded in the same change, so `Unscoped` should not
  occur from khive's own write paths; the grep gate makes any later regression a
  review defect rather than a silent hole.
- **`oldest()` survives as the aggregate view** for consumers that genuinely want
  the process-wide oldest span (logging, diagnostics). Attribution consumers move
  to `oldest_for`.

## Test plan

- Registry unit tests: `oldest_for` partitions by origin; `Unscoped` entries
  appear in the main backend's filtered view and in `oldest()`; `Memory` entries
  appear in no attribution view but still in `oldest()`; `Database` entries never
  leak into a different backend's view.
- Alias-convergence test: the same database opened via its real path, a
  symlink, and a relative spelling mints identical `DbIdentity` values and
  derives the identical sidecar directory.
- Fallback-marker test: a heartbeat produced from an `Unscoped` oldest span
  carries the fallback-attribution marker; one produced from a
  `Database(main)` span does not.
- Sweep integration test: two file-backed pools in one process; a long-running
  transaction on the secondary backend produces a heartbeat in the **secondary**
  database's sidecar directory and none in the main database's sidecar.
- Daemon-side tests: checkpoint attribution for pool X ignores a span registered
  against pool Y; with two file-backed backends, a WAL stall on the secondary is
  detected and its sidecar enumerated (per-backend ownership, not main-only).
- Traversal coverage test: a `graph_traverse_read` deferred span on a secondary
  backend appears in that backend's filtered view — the long-lived read snapshot
  is the span class this design exists for and must not remain unscoped.
- Identity test: a non-UTF-8 database path (Unix) round-trips through
  `DbIdentity` without loss and matches its own sidecar directory key.

## Rollout

Single implementation PR after this note is accepted: additive `khive-storage`
API (`TxOrigin`, `DbIdentity`, `register_scoped`, `oldest_for`), origin threading
at every registry call site (the grep-gated inventory above, including
`graph_traverse_read` and the daemon's own spans), the sidecar-derivation change
routing `sidecar_dir_for` consumers through the minted `DbIdentity`,
session-sweep fan-out, per-file-backed-backend daemon checkpoint/enumeration
ownership, and the tests above — producers and consumers of secondary sidecars
land in the same PR. No schema or wire change. The sidecar format gains exactly
one additive heartbeat-record field (the fallback-attribution marker), with the
matching record-contract text amended in ADR-091 Amendment 2 alongside;
everything else in the sidecar contract is unchanged — this note closes the
producer-side scoping gap and the main-only consumer-ownership gap against the
contract's existing per-database key.
