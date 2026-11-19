# ADR-154: SQLite Disk-Reserve Admission Before Logical Writes

**Status**: accepted (2026-09-23)\
**Date**: 2026-08-11\
**Authors**: khive maintainers\
**Tracking**: Refs #1844 (first slice, PR #3423); #3551 (remaining ADR contract)\
**Implementation**: partial; PR #3423 shipped a local floor and SQLITE_FULL escalation. See Amendment 1 for remaining obligations. Amendment 2 (accepted; implemented in #5308) separates lease contention from lease refusal.

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
4. the original #1844 implementation target, rebased on all four.

PR #3423 delivered only the first local floor slice. #3551 owns completion of the remaining
ADR-154 contract; the historical merge-order decision above remains unchanged.

The implementation is one storage-safety slice, not part of the single-owner topology migration.
It must work in the present cooperative multi-process topology and in ADR-150's future owner
topology. It does not change queue defaults, checkpoint modes, or writer ownership.

This ADR addresses #1844's disk-admission floor. It does not satisfy #1876's WAL-ceiling
requirement; that requirement is decided separately in ADR-194. Its own implementation covers
every writer surface listed in §4, including compatibility and standalone routes; merging a
strict-routing change alone is not proof of complete coverage.

#### Amendment 1 — Partial implementation status after PR #3423 (2026-09-28)

Status: Accepted (2026-09-28).

ADR-154 was accepted as a target contract before the disk-floor code landed. PR #3423 closed #1844 after shipping an initial, local free-space admission floor and separate native `SQLITE_FULL` escalation. Closing that issue does **not** mean the implementation satisfies this ADR. The first slice is useful, but the following differences remain:

| Contract                                                                                                                        | Current first slice                                                                                                                                | Remaining obligation                                                                                                                                                                                                            |
| ------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| §2: per-backend `disk_reserve_bytes` and bounded `disk_guard_deadline_ms`, with effective values in the warm-daemon `config_id` | `KHIVE_DB_FREE_SPACE_FLOOR_BYTES` is read when the pool opens; no guard deadline or backend/config-id fold                                         | Implement §2's configuration, validation, warnings, and coherence contract. Define a deliberate transition for existing `KHIVE_DB_FREE_SPACE_FLOOR_BYTES` installations before changing precedence or removing the old setting. |
| §3: a stable volume identity and bounded, cross-process lease held through transaction settlement                               | Each database probes its canonical parent without a shared volume lease                                                                            | Implement the volume-key registry, advisory lease, deadline, lock order, and failure stages.                                                                                                                                    |
| §4: probe after successful `BEGIN IMMEDIATE`; rollback on refusal; top-level and migration parity                               | The writer task probes at dequeue before `BEGIN`; other entry points probe before writer checkout or operation                                     | Move each admission to the specified execution point while retaining the lease, and prove rollback/autocommit behavior on refusal.                                                                                              |
| §§4–5: checkpoint operations, including caller-issued `WalCheckpointTruncate`, bypass floor admission                           | Dedicated checkpoint infrastructure is exempt, but caller-issued `WalCheckpointTruncate` through the request path is refused at or below the floor | Classify the checkpoint operation at the central request boundary and bypass the floor without bypassing native SQLite errors.                                                                                                  |
| §4: bootstrap DDL guarded before either pre-transaction table creation                                                          | Pool startup/setup can write before floor admission                                                                                                | Guard both named bootstrap writes and verify below-floor/probe-failure refusal leaves the tables and WAL unchanged from the post-open baseline.                                                                                 |

The existing `crates/khive-db/docs/api/pool.md` documents the **shipped** first slice. The `writer-task.md` section headed “Proposed disk-reserve admission” describes the **target**, not current behavior; its #1844/not-implemented label needs updating so it does not imply that no disk-floor code exists. Neither source document should imply that the first slice conforms to all of ADR-154.

The accepted behavior in §§2–7 and the Acceptance section remains normative. In particular, this amendment does not accept pre-`BEGIN` sampling or a floor-refused caller checkpoint as the final contract. Full implementation requires the acceptance controls already listed in this ADR, including the cross-process lease, post-`BEGIN` refusal, bootstrap, and bypass cases. A dedicated open implementation issue must carry the remaining obligations before #3523 is closed; the closed #1844 alone is not a live tracking reference.

#### Amendment 2 — Lease contention is admission pressure, not a probe failure (2026-10-12)

Status: Accepted (2026-10-13). Implemented in #5308.

**Trigger.** On a loaded host a write that holds the volume lease runs slowly, so every other writer on the volume waits out `disk_guard_deadline_ms` and receives `sqlite_capacity_unavailable` with `phase=lock`. §6 treats that outcome like a failed probe: it fails closed and is never retried automatically. Measured on one daemon over four hours at load averages between 20 and 38: 328 lease timeouts, 234 of them in audit-row generations. The events store reports the error as non-retryable, so each such generation ends `StoreFailure`, and ADR-133 folds that into the dispatch: 314 dispatches failed, 209 of them reads that wrote nothing (`comm.inbox`, `comm.health`, `gtd.tasks`, `get`, `comm.thread`), all on the admission-degrade list that exists for this kind of pressure.

**Decision.**

1. The lock phase splits in two. `lock` keeps its meaning for a lease that could not be attempted or was refused outright: no lock directory, an unopenable lock file, a lock error, same-thread re-entry. A new phase, `lock_contended`, names the one case where the guard deadline elapsed while another holder had the lease, in this process or another. Both remain `sqlite_capacity_unavailable`.
2. `lock_contended` is transient. `StorageError::is_retryable()` reports it as retryable, and the events store carries that across its socket. Floor refusal and the `identity`, `probe` and `lock` outcomes keep §6's no-automatic-retry rule. Every retry stays bounded by its caller's own budget (for audit generations, `max_commit_attempts` and its backoff), and `disk_guard_deadline_ms` still bounds each attempt.
3. For ADR-133: an audit generation that meets `lock_contended` on every attempt ends `RetryExhausted`, not `StoreFailure`. For a successful dispatch of an admission-degrade-safe verb, `RetryExhausted` degrades the way `QueueAdmissionExhausted` does: the read returns its result, and the lost row is counted on its own counter beside `audit_admission_refused_obligations`. Writes, denials, unknown verbs and `git.digest` receipts stay strictly obligation-bearing.
4. Unchanged: the 2,000 ms default and its [100, 10,000] range, the lock order of §3, the floor sample after the lease is taken, and the boundary in §7.

**Why this does not weaken the guard.** §6 forbids automatic retry because a retry cannot create disk space. Contention is not a space condition: the holder releases the lease when its transaction settles, and the floor is still sampled after a retried attempt takes the lease, so a retried write is admitted only if the reserve allows it.

**Effects on other callers.** Every caller of `is_retryable()` sees the new retryable case. The implementation lists each caller and states the bound it retries under; a caller with no bound does not get the new case.

**Acceptance.**

- A lease held by another thread past the deadline yields `phase=lock_contended`; a missing lock directory, a lock error and same-thread re-entry yield `phase=lock`. MUST-FAIL control: folding both into one phase.
- `is_retryable()` is true for `lock_contended` and false for every other capacity outcome.
- An audit generation whose store returns `lock_contended` on every attempt resolves `RetryExhausted`. MUST-FAIL control: today's classification, which yields `StoreFailure`.
- A degrade-safe read returns its result when its audit row ends `RetryExhausted`; a write verb's dispatch still fails on the same reason.
- Through the events-daemon socket the outcome stays retryable and its message names the phase; the socket protocol does not change.

#### Amendment 3 — A long write is a scheduling problem, not an admission problem

**Trigger.** On a server that serves many tenants' stores from one volume, one store's schema migration (one transaction
over about 5M edge rows, about 12 s) held the volume lease for its whole transaction. Every other database's lease-taking
call on that volume waited out `disk_guard_deadline_ms` (2,000 ms) and was refused `lock_contended`: first opens, writes,
and reads whose audit row needs the lease.

**What was considered and rejected.** A reservation ledger (hold the lease only for probe and booking, admit other
writers against free space minus outstanding bookings) does not keep §3's guarantee for a write of unknown size. Two
writers can both be admitted against headroom that the first one's actual growth then consumes. Hold-for-duration admits
the second only after the first has settled and its growth is visible. Shared-short/exclusive-long and per-database
leases fail for the same reason, and an exclusive migration still blocks every short write. These designs stay closed
until a writer can prove or enforce a bound on its own peak allocation (decision 5).

**Decision.**

1. **The guarantee is unchanged.** §2, §3, §4 and §7 stand as written. This amendment adds no guarantee and removes none;
   it changes when schema work runs, not how a write is admitted.
2. **Data-proportional schema work runs before readiness.** Before a process reports ready, it runs all schema work that
   can scan, rewrite or index the rows a populated store already holds, for every store it serves. The work is classified
   by its effect, not by the function that does it.

   **Stores.**
   - Each configured SQLite backend runs the whole barrier below.
   - A separately configured events database, which sits beside the `main` backend only, runs its own events migrations
     in the process that owns it: the events daemon in daemon mode, the serving process otherwise. In daemon mode the
     host already holds requests at an events readiness gate until the events daemon answers; this change makes the
     daemon answer only after its events migrations have finished, so no request is admitted before them.
   - A configured code map is out of the barrier: serving reads open it read-only and run no schema work on it. A code
     map that `code.ingest` opens for writing is a store first named after readiness (below).
   - A store the host serves but does not configure as a backend, such as one database file per tenant in a directory,
     is handed to the barrier by the host. The barrier is one runtime call per store, the same call the runtime makes
     for its configured backends; a host that opens its stores before it binds (for example, a boot step that lists
     the tenant directory and opens each store whose tenant is active) makes that call for each store in that step,
     before it accepts traffic. Building a runtime for a store runs its versioned migrations, but the store-schema ensures,
     the FTS rowid-map backfill and the `notes_seq` repair wait for the first request that reaches each store kind; the call
     is what moves that work to boot. A store the host does not hand in is a store first named after
     readiness.

   **In the barrier.**
   - Every pending versioned migration, including the attachment cutover and the versions after it, which already run
     before readiness. The cutover's holds are migration class.
   - Every store-schema ensure that can create an index or add a column on a populated table (entities, notes, graph,
     agents, events).
   - Pack schema plans.
   - The FTS rowid-map backfill, on each rowid-mapped FTS table without its completion marker. On such a table that is
     empty when the barrier reaches it, the barrier writes the marker, because every insert maintains the map. FTS
     tables kept in step by triggers (knowledge, sections, session messages) have no map and need no backfill.
   - The `notes_seq` repair, an anti-join over `notes` that today runs on the first notes-store acquisition.
   - The legacy quarantine repair, which already runs at boot over every backend.
   - The vector table and ANN sidecar ensure for each configured embedding model, and the sparse table and its index
     for each configured model key.
   - The forced-index check, once per store. A forced-index repair on a request after readiness then means an index was
     dropped while serving; it is reported as such, not treated as routine.

   Every lease a barrier schema unit takes is recorded as migration class. The class scope is thread-local, and many
   barrier writes run on the pool's writer task or a blocking thread, not on the thread that issued them; such a write
   carries its class with the request to the thread that takes the lease. Opening pools and their first write remain
   bootstrap class.

   **Left lazy, fixed cost.** The sparse table and index for a model key first used after readiness, which start empty,
   with the ensure cached per (store, model key) after its first success, so a later call takes no write unit (this
   cache is part of this change); embedding model registration; the identity row of a pool opened after readiness; the
   config-lock audit drain; request audit appends. Their holds appear in the lease-hold report under the class of the
   site that takes them, which does not name the path; a path moves into the barrier when a measurement that names it
   shows a long hold.

   **Stores first named after readiness.** A store opened for writing after readiness (a code map given to
   `code.ingest`, an archive) migrates when it is opened, as it does today, and its hold is recorded as migration class.
   A store created after readiness runs every version against empty tables, one short admission per version.

   **The drain is the deploy's precondition.** No barrier work on a populated store starts while any process serves traffic on the volume, and
   khive-db does not detect other serving processes: the deploy stops them first. A release that carries a pending
   migration on a populated store is therefore a drain deploy on that volume for the migration's duration, not a rolling
   one. A migration that must run under traffic uses decision 3 instead. Bounded retry on `lock_contended` (Amendment 2)
   is for contention before execution, within the caller's total budget; an accepted write whose outcome is unknown is
   never replayed.

   **A store that fails the barrier fails the boot, with one exception.** The process does not report ready, and its
   error names the store, its schema version and the step that failed. This is chosen knowingly: on a host with many
   stores, one store that fails for a reason a restart can clear (lease contention, I/O, free space) keeps every store
   down until the operator clears the cause and restarts. The exception is a failure no restart can change, from a closed
   list (the file is not a SQLite database, SQLite reports it corrupt, or the store waits on an application-assisted
   cutover): the host may refuse that one store by name on every request and serve the rest, provided the refusal is
   counted on a surface a health probe reads, so the outage does not read as healthy. The operator's move is the same
   either way: read the store and the step from the error, fix or restore that store, restart.

   Versions commit one at a time, so a boot that fails at version N leaves the store at N-1. When N-1 is newer than the
   previous binary's latest version, that binary refuses to open it ("database schema version N-1 is ahead of the latest
   known migration"). Pre-migration copies that let an operator
   return such a store to the previous binary are decided in their own amendment.
3. **An online data backfill is a resumable, chunked operation, by declaration.** A migration that must run while traffic
   is served declares an intermediate-state contract: an expand step, a backfill in chunks that each commit their data and
   their progress marker in one transaction and take a fresh admission, a validation step, and a short cutover that marks
   the migration complete. No lease is held across chunks. Releasing the lease between chunks does not by itself let a
   waiter in another process in while the chunking process has other lease traffic; that hand-off is the later
   amendment's, and the first chunked migration does not ship before it. Existing atomic migrations, batch APIs and
   import transactions are not split implicitly.
4. **Deadlines and recovery unchanged.** The 2,000 ms default, its [100, 10,000] range, the `lock` / `lock_contended`
   split, same-thread re-entry refusal, and §5's bypasses all stand. A waiter's expiry never interrupts an admitted write.
5. **Gate for any future concurrent admission.** A design that admits two writers on one volume concurrently must give
   each admitted writer a bound on its additional peak allocation (database, WAL, journal, index, temporary and settlement
   bytes) that is proved or enforced before the excess is allocated; an estimate alone is not a bound. Such a design is
   admitted only after it answers each of these, each with a named test or measurement:
   1. A process that dies holding bookings: how a stale booking is detected and reclaimed, and why reclaim can never free
      a live writer's booking (reclaim only by obtaining the booking's kernel lock; never by process id, time-to-live or
      caller cancellation).
   2. The booking of a write whose size bound is wrong in either direction: an overestimate refuses safe work with a
      stated cause; an underestimate is stopped before it allocates past its bound and rolls back.
   3. A writer when bookings exhaust the headroom: it waits under one absolute deadline with no SQLite writer or
      accounting mutex held, then is refused with a distinct retryable reason, never `sqlite_capacity_refused`.
   4. Fairness between one long writer and many short ones across processes: arrival tickets, and no younger admission
      ahead of the oldest live eligible ticket.
   5. The cross-process file format and its upgrade from today's lock file with old and new binaries on one volume: the
      legacy lock file kept as a lifetime gate, so an old binary still excludes a new one.
   6. The guarantee of §3, and what the design weakens relative to hold-for-duration, in one sentence.

   Until a design answers all six, the duration-held lease stays.

**What remains open after this amendment.** A long ordinary write (a large batch insert, an atomic multi-record update,
a VACUUM, a scheduled FTS merge) still blocks other databases on the volume for its duration, and a store first opened
after readiness migrates under traffic. This amendment fixes neither. Fixing them needs decision 5's bound, decision 3's
chunking, or independent volumes per capacity domain. How a released lease reaches a waiter in another process, and the
refusal a bystander receives while a late-named store migrates, are left to a later amendment sized from the lease-hold
report in production traffic.

**Acceptance.**

- `deploy_readiness_volume_barrier`: two stores on one volume, each populated at the old schema, with a migration
  fixture, a store-schema ensure that adds an index, a rowid-mapped FTS table with its completion marker cleared after
  the rowid-map migration has run, an empty rowid-mapped FTS table, deleted `notes_seq` rows, and a configured embedding model whose vector table is absent.
  At readiness every store reads the latest schema version, every rowid-mapped FTS table carries its completion marker,
  the `notes_seq` repair count equals the deleted rows, the vector table exists, and no data-proportional transaction
  overlaps a request on the volume over the whole run. Must-fail: one store left out of the barrier; expected red on the
  `notes_seq`, sparse index and vector table assertions (runtime construction still migrates the schema version).
- `barrier_holds_are_migration_class_and_none_follow_readiness`: before readiness, every lease a barrier schema unit
  takes is recorded as migration class in the lease-hold report, including the legacy quarantine repair's writes, which
  run on the writer task. After readiness, a first request against every in-scope store kind, other than a model key
  first used after readiness, executes no schema statement (counted through a test-only statement hook) and records no migration-class hold. Must-fail controls: one
  store-schema ensure left lazy; expected red on the statement count. The class set only on the issuing thread and not
  carried to the writer task; expected red on the class assertion for the quarantine repair's writes.
- `host_handed_store_runs_the_barrier`: a store that is not a configured backend, populated at a schema version at or above the
  FTS rowid-map migration, with the completion marker cleared after migrating and the rest of the fixture of
  `deploy_readiness_volume_barrier`, is handed in by a host boot step before binding. At readiness it
  meets the same assertions, and a first request against it executes no schema statement. Must-fail: the host opens the
  store without the barrier call; expected red on the `notes_seq` and FTS marker assertions.
- `structural_refusal_is_per_store_and_visible`: one handed-in store is not a SQLite database and another is healthy.
  The host serves the healthy store, refuses the bad one by name, and the refusal count read by the health probe is 1.
  A second fixture whose failure is lease contention fails the boot naming the store. Must-fail: contention placed on
  the closed list; expected red on the boot assertion.
- `empty_fts_table_marker`: the test reads `{table}_rowids_state` at readiness and finds the marker, then inserts rows
  and finds them through a freshly opened text handle with no backfill. Must-fail: the barrier's marker write removed;
  expected red on the state read.
- `sparse_ensure_is_cached`: the second sparse call for a (store, model key) takes no write unit. Must-fail: the cache
  removed; expected red.
- `chunk_restart` (with the first migration that declares decision 3): kill at every data, progress and cutover
  boundary; restart resumes exactly, and no lease is held from one chunk into the next. Must-fail: one lease held across
  chunks, or the progress marker committed before the data. Bystander progress across processes during the chunks is
  tested under the later amendment that chooses the hand-off mechanism.
- Unchanged-guarantee control: two writers on two different databases on one volume, the first holding an admitted
  transaction. The second is not admitted until the first settles. Must-fail: the lease released after the probe; the
  second is admitted early.

**Effects on callers.** A multi-tenant deployment drains the volume, then runs the barrier, before traffic. A
single-process deployment runs the same barrier at start, which moves first-use schema work from its first requests to
its boot. `code.ingest` into a code map behind the current schema still migrates that map under traffic.

#### Amendment 4 — A released lease reaches a waiter in another process

**Trigger.** A volume-lease waiter in another process can lose every release to waiters in the process that holds the
lease, until its deadline (#6463). In each process only the thread that holds the process's slot polls the volume lock
file; the others wait on the slot's condition variable. When the holder drops its lease it unlocks the file and frees the
slot, and the next waiter in the same process takes the slot and calls `try_lock` at once. The waiter in the other process
is asleep for up to one poll interval (10 ms) and finds the file locked again. With steady traffic in the holding process,
the other process's waiter is refused `lock_contended` at `disk_guard_deadline_ms` although the lease was free many times
inside its deadline. Amendment 3's chunked migrations release the lease between chunks to let other databases in; across
processes that release does not reliably reach the other database's waiter.

**Decision.**

1. **The property.** With two processes on one volume, a lease waiter in one process is admitted within its deadline
   while the other process holds and releases the lease under steady local traffic.
2. **Invariants any mechanism keeps.**
   - The guarantee is unchanged. Admission still requires the exclusive lock on the volume lock file, held from the floor
     sample to settlement, as §3, §4 and §7 state. Any file the mechanism adds beside the lock file authorizes nothing:
     losing it, deleting it or a process that never writes it changes only who tries first.
   - Deadlines are unchanged. Any waiting the mechanism adds happens inside the waiting thread's own deadline, and a
     thread whose deadline passes is refused `lock_contended`, as today. No thread blocks on a file lock.
3. **Mixed binaries.** This concerns binaries that take the volume lease; binaries that predate it are outside the
   guarantee, as §7 states. A lease-taking binary without the mechanism takes part in neither side of it, so the
   property holds only while every process on the volume runs the mechanism; during a mixed deployment a waiting process
   can still lose every release, as today.
4. **The mechanism is chosen in the implementation change**, against the tests below, executed across processes on each
   supported platform. That change records the designs it considered and the races each one admits.
5. **What starts the implementation.** Either the lease-hold report from production showing a cross-process waiter
   refused `lock_contended` while the lease was being released, or the first migration that declares Amendment 3's
   decision 3, whichever comes first. The first chunked migration does not ship before the mechanism.
6. **The bystander's refusal while a late-named store migrates** stays Amendment 2's: a waiter that waits out its
   deadline is refused `lock_contended`, which is retryable within the caller's own budget, never a capacity refusal.
   This amendment adds no retry hint; one is sized from the lease-hold report when production shows the need.

**Acceptance.** Each test states its process count, thread count, hold time, deadline and run count, and the change
records each must-fail control's red rate at those settings before the mechanism lands.

- `cross_process_waiter_is_admitted_under_local_traffic`: process A runs short writes through the lease from several
  threads with no gap between holds; a waiter in process B on another database of the same volume is admitted within its
  deadline in every run. Must-fail: today's behaviour; the settings are chosen so the control is red in every run, and
  that rate is recorded.
- Unchanged-guarantee control (as in Amendment 3): two writers on two databases of one volume, the first holding an
  admitted transaction; the second is not admitted until the first settles, with the mechanism in place. Must-fail: the
  volume lease released after the floor sample; the second is admitted early.

**Effects on callers.** None at the API. Under cross-process contention a process's local throughput drops toward its
share of the volume.

### 2. Reserve configuration and daemon coherence

Every writable SQLite backend resolves two independent values:

- `disk_reserve_bytes`: the backend field in `[[backends]]`, then
  `KHIVE_SQLITE_DISK_RESERVE_BYTES`, then the built-in default,
  **1 GiB (1,073,741,824 bytes)**; and
- `disk_guard_deadline_ms`: the backend field, then
  `KHIVE_SQLITE_DISK_GUARD_DEADLINE_MS`, then **2,000 ms**, validated in
  `[100, 10,000]` without clamping.

An explicit `disk_reserve_bytes=0` disables floor refusal for that backend and must emit a startup
warning; `disk_guard_deadline_ms=0` is invalid, never a disable value. Zero reserve exists for small
scratch/test filesystems and deliberate operator override; it is never the implicit default.
Invalid or overflowing TOML/environment values are configuration errors, not silent fallback.
Memory and read-only backends do not create a guard; specifying a non-zero SQLite reserve for a
memory backend is a configuration error. Disabling the floor does not disable an independently
configured WAL ceiling; see ADR-194.

A zero reserve removes the floor term only; an operation-specific headroom is still compared, and a
missing estimate still refuses.

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

A lease request from the thread that already holds that volume's lease fails at once with a typed
re-entry error naming both call sites; nesting that crosses to another thread is not detected and
waits out `disk_guard_deadline_ms` as contention.

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

A standalone statement or script, and a call through a pool-backed writer, holds the lease and the
writer only for its own call, so no transaction it opens may outlive the call. On the standalone
path, `execute` refuses transaction control unless a manual transaction unit already holds the lease
for the unit's whole span, and a script that leaves a transaction open without that lease is rolled
back before the lease is released. A pool-backed `execute` refuses transaction control, and a
pool-backed script that leaves a transaction open is rolled back before its writer is released. The
call then fails: with `InvalidInput` if it otherwise succeeded, with its own error if it failed, and
with `WriterSettlementUnknown` if the transaction cannot be proven settled, in which case the
connection is retired. Statements inside `atomic_unit` and calls routed through the writer task run
inside a transaction whose lease already spans them, and are outside this rule.

`TopLevelMaintenance::WalCheckpointTruncate` is a checkpoint operation, not a copy-sized
maintenance operation like `VACUUM`: it follows §5's checkpoint bypass, never this table's
top-level-maintenance admission row, and never requires the volume lease or the capacity probe.

Normal store modules do not each implement their own probe. After #1911 they inherit the
writer-task seam. Pool, SQL-bridge, migration, and explicitly top-level/standalone entry points are
the central enforcement boundary.

The bootstrap row names two current writes which occur before any migration transaction:
`apply_schema_plan` executes `SCHEMA_VERSION_TABLE`, and `bootstrap_migration_ledger` executes
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
reservation. The sampled floor is neither reserved capacity nor guaranteed recovery headroom: the
bypasses in §5 mean this policy does not itself refuse a recovery operation, not that space for
that operation has been set aside. An already-admitted transaction or another filesystem consumer
may still consume the sampled headroom before recovery runs.

### 5. Recovery bypasses

The guard is applied once at the logical-write boundary. It is never inserted into generic
statement execution or transaction terminators, because doing so could admit a body and then block
the operation needed to make its outcome safe. COMMIT and ROLLBACK bypass disk-floor admission for
that reason; that exemption does not disable a separately enacted, narrower storage-capacity limit,
and a capacity failure under such a limit must still preserve real transaction settlement and
native error attribution. ADR-194 introduces exactly that limit, and its own refusal at
`COMMIT`/`ROLLBACK` is deliberately permitted: unlike a floor re-probed at the terminator, it never
blocks the terminator from running, and it always completes the terminator through a proven
whole-transaction rollback before returning a result, so no caller ever observes an unresolved
outcome, which is the property this rejection protects.

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
   Acceptance 7 is gated when it runs by the initially non-required
   `SQLite capacity acceptance` job in `.github/workflows/sqlite-capacity.yml`
   (exit 0 and `ADR154_CAPACITY_PASS` required), with a dedicated-host run quoted
   in the PR body as the fallback if the hosted runner refuses the namespace or mount.
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
checkpoint, diagnostics, and holder release remain operable (this policy does not block those
operations; it does not reserve space for them); errors distinguish prevention from actual
exhaustion; and multiple khive databases on one volume coordinate against one sampled pool.

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

- #1844 — first implementation slice, PR #3423
- #3551 — remaining ADR-154 volume-coordinated disk-reserve admission
- [ADR-015](ADR-015-schema-migrations.md) — startup schema migrations
- [ADR-091](ADR-091-wal-snapshot-lifetime.md) — checkpoint and WAL-pin governance
- [ADR-111](ADR-111-blob-store.md) — filesystem capacity-floor precedent
- [ADR-131](ADR-131-batch-write-admission-control.md) — bounded queue admission
- [ADR-135](ADR-135-write-scaling-demand-before-ownership.md) — writer stages and ownership limits
- [ADR-136](ADR-136-fair-write-admission-default.md) — strict writer routing and exemptions
- [ADR-150](ADR-150-single-write-owner-topology.md) — future write-owner topology
- [ADR-194](ADR-194-sqlite-wal-extent-ceiling.md): bounded WAL extent ceiling under a pinned reader
