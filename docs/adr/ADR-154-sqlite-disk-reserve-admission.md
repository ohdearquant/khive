# ADR-154: SQLite Disk-Reserve Admission Before Logical Writes

**Status**: proposed\
**Date**: 2026-08-11\
**Authors**: khive maintainers\
**Tracking**: Refs #1844\
**Implementation**: none in this ADR; accepting or merging this document does not close #1844

## Context

SQLite WAL mode lets a reader retain a snapshot while later commits append frames. A pinned reader
can therefore prevent frame reuse while the WAL grows toward filesystem exhaustion. A raw
`SQLITE_FULL` is already too late for an orderly response: SQLite, the checkpointer, diagnostics,
and the operator all need working space to roll back the active transaction, identify and stop the
holder, checkpoint after the holder exits, and preserve failure evidence.

`PRAGMA journal_size_limit` does not bound an active WAL pinned by a reader. The filesystem blob
floor in ADR-111 is useful precedent, but it is not directly sufficient:

- blob admission knows the pending object size; a generic SQLite transaction does not know its
  eventual WAL or temporary-file growth before it runs;
- the blob lock is keyed by a canonical store root, while several SQLite databases can consume the
  same filesystem reserve; and
- SQLite has transaction terminators and recovery operations which must remain possible after
  ordinary logical writes are refused.

ADR-150 proposes a topology-independent WAL disk guard but intentionally leaves its exact
configuration, error, bypass, and execution semantics unspecified. This ADR supplies that contract.
It extends ADR-135's write-stage taxonomy and ADR-136's writer classification without changing
their routing decisions.

## Decision

### 1. Delivery order and scope

The documentation decision may land independently. Its implementation is based on the settled
writer surfaces and therefore follows this merge order:

1. #1897 (completion-preserving SQLite read/query cancellation);
2. #1911 (strict writer routing);
3. #1912 and #1913 (checkpoint ownership publication and standalone-writer journal limits).
   These two are semantically independent, but both touch the pool and must be rebased/merged
   serially when Git requires it; and
4. the #1844 implementation, rebased on all four.

The implementation is one storage-safety slice, not part of the single-owner topology migration.
It must work in the present cooperative multi-process topology and in ADR-150's future owner
topology. It does not change queue defaults, checkpoint modes, or writer ownership.

### 2. Reserve configuration and daemon coherence

Every writable SQLite backend resolves two independent values:

- `disk_reserve_bytes`: the backend field in `[[backends]]`, then
  `KHIVE_SQLITE_DISK_RESERVE_BYTES`, then the built-in default,
  **1 GiB (1,073,741,824 bytes)**; and
- `disk_guard_deadline_ms`: the backend field, then
  `KHIVE_SQLITE_DISK_GUARD_DEADLINE_MS`, then **2,000 ms**, validated in
  `[100, 10,000]` without clamping.

An explicit zero disables refusal for that backend and must emit a startup warning. Zero exists for
small scratch/test filesystems and deliberate operator override; it is never the implicit default.
Invalid or overflowing TOML/environment values are configuration errors, not silent fallback.
Memory and read-only backends do not create a guard; specifying a non-zero SQLite reserve for a
memory backend is a configuration error.

The warm-daemon `config_id` fingerprints the effective reserve bytes and guard deadline for the
implicit main backend and for every named SQLite backend in the same deterministic backend order as
the existing topology fold. It fingerprints the effective numeric policy, not its source, current
free space, or runtime volume identifier. Thus equivalent TOML/env/default inputs remain
compatible, while a client and daemon with different refusal policies fail the existing
configuration-coherence check before dispatch.

Diagnostics expose the effective bytes, configuration source, probe path, and volume identity.
None of those live diagnostic fields is persisted into the compatibility fingerprint.

### 3. Volume identity and one lock order

A canonical database path identifies a database, not the capacity pool it consumes. The guard
resolves the nearest existing canonical ancestor of the database path and derives a
`VolumeIdentity` with two deliberately separate parts:

- a stable `VolumeKey`: Unix `st_dev`, a stable Windows volume ID/serial, or an equivalently stable
  platform identifier; and
- diagnostic metadata: the canonical probe path and, where available, a canonical volume root.

Only `VolumeKey` participates in `VolumeIdentity` equality/hashing, the in-process registry key, or
the advisory-lock filename. Diagnostic path/root metadata is excluded from all four. Two database
paths on the same filesystem therefore compare equal and select the same lease even when their
canonical parent paths differ. A platform without a stable volume key fails closed for writable
SQLite; a canonical path string alone is not an acceptable substitute.

All participating khive writers on one host use:

- an in-process guard registry keyed by `VolumeIdentity`; and
- a bounded advisory lock in khive's per-user runtime lock namespace, with a filename derived from
  a versioned hash of `VolumeKey` only, so separate khive processes and separate database paths on
  the same volume share one cooperative admission lease.

Identity resolution, lock-open, and lock acquisition failures are typed capacity-unavailable
failures. Lock acquisition uses `disk_guard_deadline_ms`; it never waits without a bound and never
reuses `write_admission_deadline_ms`, which remains exclusively the queue-capacity deadline governed
by ADR-131. The lease is held until the admitted operation commits, rolls back, or otherwise
returns to autocommit.

The lock order is **volume lease, then SQLite writer acquisition, then capacity probe, then first
logical write**. Every participating path uses that order. The probe remains after a successful
`BEGIN IMMEDIATE`, as required below, but acquiring the volume lease first prevents a top-level
operation (which has no explicit `BEGIN`) from holding the volume lease while a wrapped writer holds
the SQLite lock and waits for that lease.

### 4. Execution-time admission

Queue admission is not disk admission. Free-space state can change while a request waits, so no
enqueue-time sample can authorize a write. Each request samples at execution time:

#### Transaction-wrapped writer task

1. Dequeue the request.
2. Acquire the volume lease.
3. Execute `BEGIN IMMEDIATE`.
4. Probe available space on the resolved volume.
5. If refused or the probe fails, execute `ROLLBACK`, prove the connection returned to autocommit,
   release the lease, and reply with the typed error without invoking the operation closure.
6. Otherwise invoke the closure and retain the lease through `COMMIT` or `ROLLBACK`.

A capacity refusal with a successful rollback is non-terminal for `WriterTask`; the next request
may run after space is recovered. A failed rollback retains the existing
`WriterTaskRequestState::SideEffectsUnknown`/connection-retirement contract.

#### Other writer surfaces

| Surface                                | Required admission point                                                                                                                                                                           |
| -------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Pooled `WriterGuard` transaction       | Same sequence as the writer task: lease before `BEGIN`, probe after successful `BEGIN`, before the closure                                                                                         |
| Standalone/manual transaction          | Same sequence; no direct compatibility fallback may skip the shared guard                                                                                                                          |
| Standalone autocommit statement/script | Lease and probe immediately before the first SQLite write call; retain the lease until SQLite returns to autocommit                                                                                |
| Startup bootstrap DDL                  | Resolve against the existing parent for a new file; lease and probe immediately before each current pre-`BEGIN` autocommit bootstrap call, and retain the lease until SQLite returns to autocommit |
| Migration/schema transactions          | Lease before each transaction, probe after `BEGIN`, before its first DDL/DML                                                                                                                       |
| Top-level maintenance such as `VACUUM` | Lease and probe immediately before execution; retain it until the call returns                                                                                                                     |

Normal store modules do not each implement their own probe. After #1911 they inherit the
writer-task seam. Pool, SQL-bridge, migration, and explicitly top-level/standalone entry points are
the central enforcement boundary.

The bootstrap row names two current writes which occur before any migration transaction:
`apply_schema_plan` executes `SCHEMA_VERSION_TABLE`, and `run_migrations_locked` executes
`MIGRATION_TRACKING_TABLE`. Each call must acquire the volume lease, probe before its
`execute_batch`, skip the call on refusal/probe failure, and retain the lease until the connection
is demonstrably back in autocommit. The later per-migration transaction then performs the normal
post-`BEGIN` probe. These bootstrap writes are not exempt merely because they run during startup.

Admission compares available bytes against the configured reserve plus any conservative
operation-specific headroom. Addition is checked; overflow refuses. A top-level operation with a
known copy-sized working set, including `VACUUM`, must supply an estimate derived from current
database/WAL metadata and refuse when that metadata cannot be read. A generic transaction supplies
no trustworthy total-size estimate and therefore uses the reserve as an **admission floor**: it
does not start when the sampled available space is at or below that floor.

This is deliberately a bounded claim. The guard proves that a new cooperating logical write did not
start from inside the configured emergency reserve. It cannot prove that an arbitrarily large
already-admitted transaction will not cross the floor, nor can it reserve bytes against unrelated
processes. Callers and diagnostics must not render the floor as a filesystem quota or hard
reservation.

### 5. Recovery bypasses

The guard is applied once at the logical-write boundary. It is never inserted into generic
statement execution or transaction terminators, because doing so could admit a body and then block
the operation needed to make its outcome safe.

The following bypass disk-refusal admission:

- `COMMIT` and `ROLLBACK` for an already-admitted transaction;
- PASSIVE checkpoints, ADR-091's scheduled threshold-armed `maybe_truncate` escalation, and
  operator-authorized stronger checkpoints;
- read-only diagnostics, including the PASSIVE diagnostic probe;
- reader cancellation/termination and holder-census operations needed to release a WAL pin; and
- crash recovery and file/sidecar cleanup needed to return the store to an operable state.

The bypass means "do not refuse from the configured floor," not "ignore SQLite errors." Any native
`SQLITE_FULL` from a terminator, checkpoint, diagnostic, or recovery path is preserved and escalated
as described below. Migrations, schema creation, logical repair writes, compaction, reindex, and
`VACUUM` are not recovery bypasses.

### 6. Probe failures, typed errors, and telemetry

Failure to resolve volume identity, acquire the cooperative lease, or query available space fails
closed for new logical writes. Automatic retry is false for both floor refusal and probe failure:
blind retry consumes admission capacity and cannot create disk space. A caller may make a new
explicit attempt after operator action or a successful recovery checkpoint.

ADR-135 F6 gains three stable stages:

| Stage                         | Typed outcome                                                                                                            | Meaning                                                                                             |
| ----------------------------- | ------------------------------------------------------------------------------------------------------------------------ | --------------------------------------------------------------------------------------------------- |
| `sqlite_capacity_refused`     | `StorageError::CapacityFloor` with `capability=Sql`, volume, available, reserve, and required-headroom bytes on the wire | Preflight ran and refused before the first logical write                                            |
| `sqlite_capacity_unavailable` | new typed capacity-probe/lease error with a `phase` of `identity`, `lock`, or `probe`                                    | Safety could not be established, so the logical write did not run                                   |
| `sqlite_disk_full`            | preserved native SQLite primary/extended result codes                                                                    | SQLite returned `SQLITE_FULL`; the guard did not prevent exhaustion or a bypass path encountered it |

`SQLITE_FULL` is never rewritten as `CapacityFloor`. It is a higher-severity error and counter than
a guard refusal. The first escalation is structured tracing/stderr plus a best-effort non-database
sink; recording the incident must not recursively require a write to the guarded SQLite file.

### 7. Cross-process guarantee boundary

The cooperative volume lease prevents two participating khive processes that share the same
runtime lock namespace from simultaneously authorizing writes against one sampled reserve. It does
not fence:

- stale or older khive binaries which do not take the lease;
- a same-user process that deliberately bypasses an advisory lock;
- other applications writing the same volume; or
- filesystem consumption between the probe and physical allocation.

This matches ADR-135 F1: an advisory lock is coordination, not OS-enforced write ownership. The
implementation and operator docs must state this boundary. ADR-150's future exclusive owner reduces
the number of cooperating writers but does not turn free-space sampling into a hard quota.

## Acceptance

The implementation PR must include all of the following without consuming the developer or CI
workspace volume:

1. Pure/injected-probe tests for above/equal/below-floor boundaries, checked-add overflow, explicit
   zero, invalid reserve/deadline config, probe failure, and stable typed stage/wire fields.
2. A writer-task test that enqueues while space is available, changes the injected result while the
   request waits, and proves the execution-time sample refuses it, rolls back, leaves the closure
   uncalled, and keeps the task usable after the probe recovers.
3. Parity tests for pooled, standalone/autocommit, migration/schema transactions, and top-level
   maintenance entry points.
4. Fresh-file bootstrap tests cover both `SCHEMA_VERSION_TABLE` and
   `MIGRATION_TRACKING_TABLE`: below-floor and probe-failure cases must return the typed error before
   the corresponding table exists, and must leave WAL bytes/frames unchanged from a baseline sampled
   after connection open but before bootstrap admission. An above-floor case creates the ledger and
   proceeds into the separately admitted migration transaction.
5. Bypass tests proving an admitted request can always reach `COMMIT`/`ROLLBACK`, and PASSIVE,
   scheduled threshold-armed `maybe_truncate`, operator checkpoint, diagnostics, reader-release,
   and recovery paths are not capacity-refused.
6. A same-volume/two-parent-path test proves distinct diagnostic canonical paths derive equal/hash-
   equal `VolumeIdentity` values and the same lock filename, then proves two database paths cannot
   hold the cooperative lease concurrently. A cross-process variant exercises the advisory lock
   and its bounded timeout.
7. A Linux-only constrained-filesystem integration test on an isolated loopback/tmpfs device. It
   must verify the device identity differs from the workspace/root volume before writing, cap the
   image size, use a cleanup trap, and abort rather than fall back to the host filesystem. A reader
   pins a real WAL snapshot while writes grow it; the next write must return
   `sqlite_capacity_refused` before any raw `SQLITE_FULL`. After the reader exits, a bypassed
   checkpoint reclaims space and an ordinary write succeeds.
8. A separate raw-`SQLITE_FULL` classification test, using SQLite's bounded page limit or the
   isolated device with the guard explicitly disabled, proves the native code is preserved and
   reported as `sqlite_disk_full`, never as a guard refusal.
9. `config_id` tests prove equal effective reserve/deadline values fingerprint identically
   regardless of source, changed values differ, and backend ordering stays deterministic.

No acceptance test may fill or intentionally pressure the checkout, home, or runner root
filesystem. The constrained-device test skips when it cannot prove isolation; the injected-probe
suite remains mandatory everywhere.

## Consequences

**Benefits.** New logical writes stop before entering a known emergency floor; rollback,
checkpoint, diagnostics, and holder release remain operable; errors distinguish prevention from
actual exhaustion; and multiple khive databases on one volume coordinate against one sampled pool.

**Costs.** Writes to otherwise independent databases on one volume serialize for the duration of
their transactions. The default changes absent-configuration behavior and can refuse writes on a
small filesystem; explicit zero is the compatibility escape hatch. Volume identity and advisory
locking require platform-specific code and failure tests.

**Residual risk.** The reserve is not a quota and cannot bound an admitted transaction of unknown
size. Raw `SQLITE_FULL` remains possible and is intentionally treated as an operational escalation,
not evidence that the typed refusal path ran.

## Alternatives considered

- **Probe when enqueueing.** Rejected: queue wait makes the sample stale.
- **Probe before `BEGIN` and release the guard immediately.** Rejected: another writer can consume
  the same observed space, and the sample precedes SQLite writer acquisition.
- **Probe every statement, including terminators.** Rejected: it can prevent rollback or commit
  after a body has run and make outcomes less safe.
- **Key the guard by canonical database path.** Rejected: separate databases can exhaust the same
  volume concurrently.
- **Treat `SQLITE_FULL` as the refusal.** Rejected: it erases whether prevention worked and arrives
  after emergency headroom may already be gone.
- **Use an advisory lock as a single-writer fence.** Rejected: ADR-135 correctly limits that claim;
  the lock coordinates participating capacity checks only.

## References

- #1844 — SQLite/WAL disk-reserve pre-write guard
- [ADR-015](ADR-015-schema-migrations.md) — startup schema migrations
- [ADR-091](ADR-091-wal-snapshot-lifetime.md) — checkpoint and WAL-pin governance
- [ADR-111](ADR-111-blob-store.md) — filesystem capacity-floor precedent
- [ADR-131](ADR-131-batch-write-admission-control.md) — bounded queue admission
- [ADR-135](ADR-135-write-scaling-demand-before-ownership.md) — writer stages and ownership limits
- [ADR-136](ADR-136-fair-write-admission-default.md) — strict writer routing and exemptions
- [ADR-150](ADR-150-single-write-owner-topology.md) — future write-owner topology
