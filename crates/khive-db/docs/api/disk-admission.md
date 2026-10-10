# SQLite disk admission

[ADR-154](../../../../docs/adr/ADR-154-sqlite-disk-reserve-admission.md) defines
cooperative admission for logical writes on a physical volume. A write unit takes
the volume lease before SQLite's writer slot, samples available space at its
execution boundary, and retains the lease through proven autocommit or successful
owned connection close. When rollback and close both fail, the owner retires the
connection, returns an unknown-outcome error, and releases the lease. Checkpoints
and read-only work remain available for recovery.

The implementation below is the foundation for #3551. The raw and cached APIs in
the remaining-work table are not covered by a complete execution-time guarantee.

## Configuration

For a named file-backed SQLite backend:

```toml
[[backends]]
name = "main"
kind = "sqlite"
path = "main.db"
disk_reserve_bytes = 1073741824
disk_guard_deadline_ms = 2000
```

The reserve resolves from the backend override, then
`KHIVE_SQLITE_DISK_RESERVE_BYTES`, then the deprecated
`KHIVE_DB_FREE_SPACE_FLOOR_BYTES`, then the 1 GiB default. The new and legacy
environment values must agree if both are present. Every supplied numeric value
is validated, including environment values shadowed by an override. Zero reserve
disables the floor comparison; it still requires volume identity, the cooperative
lease and a successful capacity probe. A zero reserve removes the floor term only;
operation-specific headroom is still compared, and a missing estimate still
refuses. VACUUM still requires its working headroom.

The deadline resolves from the backend override, then
`KHIVE_SQLITE_DISK_GUARD_DEADLINE_MS`, then 2000 ms. The supported range is
100–10000 ms. Runtime construction captures environment policy once; daemon
identity, backend openers, spawned children, aliases and the events sidecar use
that resolved policy. Aliases of the same file must agree on the numeric policy.

The default absolute volume-lock directory resolves `KHIVE_VOLUME_LOCK_DIR`,
then `HOME`, then `USERPROFILE`, skipping empty values and appending
`.khive/sqlite-volume-locks`. Cooperating processes for one user share that
directory. If no source is available, configuration fails with a typed error.
Callers can supply an explicit directory through `PoolConfig.volume_lock_dir`,
`StorageBackend::sqlite_with_volume_lock_dir`, or the constructor accepting both
captured policies. Test callers use private fixture directories.

Within one process, requests for a volume slot are queued in the order they
enter its registry mutex. Only the front waiter can acquire a free slot; a new
arrival cannot overtake queued requests. An expired waiter removes itself and
retains the same `CapacityUnavailable` lock-phase error and deadline. The FIFO
order does not extend to separate processes competing for the advisory file
lock. Same-thread re-entry still refuses immediately, and detached leases retain
exclusion until their eventual release.

Under `KHIVE_TEST_HARNESS=1`, private lock directories also isolate in-process
slots. File-backed test fixtures must own a private directory for their whole
runtime lifetime or run through the isolated child-process helper. A private
database filename alone does not isolate a physical-volume lease.

Memory and read-only backends do not take the disk lease or capacity probe.
Diagnostics report the resolved numeric values and their configuration sources,
the identified volume and probe path, and any inability to inspect that volume.
The guard being enabled does not imply a nonzero floor.

## Write boundary inventory

This inventory identifies logical boundaries, rather than treating connection
checkout as proof that a later write was admitted. Symbols are under
`crates/khive-db/src` unless stated otherwise.

- **Writer task:** acquire a lease before BEGIN and sample after BEGIN. Settle
  before releasing the lease, including on terminal return or unwind.
- **Pooled transaction:** hold the lease before the writer mutex and sample
  after BEGIN. Roll back on refusal; replace a poisoned pooled connection.
- **Standalone transaction:** own the connection and lease, then sample after
  BEGIN. Close or restore autocommit before releasing the lease.
- **Autocommit writes:** admit backend constructor DDL and entity, note,
  attachment, and sparse helpers before the first SQLite write. Retain the guard
  through the script and retire an unfinished transaction.
- **FTS rowid-map backfill:** retain the constructor lease and sample after
  BEGIN. Refusal rolls back without filling the map or stamping completion.
- **Graph mutation plus events:** retain a standalone lease or pooled guard
  through the closure. The transaction helper samples after BEGIN.
- **Schema bootstrap:** sample immediately before each ledger-table DDL and
  retain the lease through autocommit.
- **Schema and cutover transactions:** sample after each BEGIN and before DDL
  or DML. Roll back on refusal and preserve settlement errors.
- **VACUUM:** estimate the database and WAL working set before execution.
  Missing metadata, overflow, or insufficient headroom refuses the operation.
- **Checkpointing:** typed checkpoint guards, the checkpoint task, and the
  diagnostics PASSIVE probe bypass the lease and floor. The capability exposes
  no ordinary SQL.
- **Reads:** reader checkout handles pure reads and schema diagnostics without
  logical-write admission, including when the reserve is exhausted.

The events sidecar resolves its policy from the actual main pool, including when
requested through a runtime bound to another backend. Cached registry reuse
requires equivalent effective policy. Disk admission does not alter the separate
WAL ceiling or SQLite file-identity rules.

Dedicated code-map pools receive their own runtime's captured disk policy and
absolute volume-lock directory through `sqlite_code_map_with_policies`. Their
registered handle-proving VFS, DELETE journal mode, guarded migration refusal
mapping, and no-parent-creation rule remain in force. The compatibility constructor
resolves environment settings and the shared per-user default lock directory.
The pre-pool WAL-to-DELETE transition and its callees are unchanged in this slice.

## Remaining work

- **#3594:** Public raw pooled writers can issue SQL outside the typed
  transaction boundary; checkout-time sampling is insufficient.
- **#3595:** `open_standalone_writer` returns a raw connection whose later
  writes are not an owned logical unit.
- **#3596:** Internal raw standalone connection escapes need call-site
  conversion and inventory.
- **#3597:** Cached `SqlWriter` handles and manual atomic SQL need per-write
  leases and SQLite-aware classification.
- **#3551:** The code-map WAL-to-DELETE transition, including checkpoint and
  sidecar work before pool construction, remains outside this slice. A later
  ADR-154 slice must decide its admission classification.

#3598's restricted checkpoint capability is retained. These open boundaries keep
#3551 open; the foundation does not claim a universal guard over all SQL a caller
can execute. Backend/pool-owned
migration entry points carry the captured policy.

Caller-owned raw connections can use `MigrationWritePolicy::new(effective_policy,
absolute_lock_dir)` and `run_migrations_with_policy`, `apply_schema_plan_with_policy`,
`stage_attachment_cutover_with_policy` or `finalize_attachment_cutover_with_policy`.
The policy validates before database work and never rereads the environment.
Compatibility raw names resolve numeric environment policy and require an explicit
the shared per-user default lock directory for file-backed connections; missing or
relative namespaces fail closed. In-memory raw connections remain exempt.
Never call a raw wrapper while holding a pooled writer guard: use the pool or
backend method so the existing lease is retained rather than reacquired.

These raw entry points require ordinary owned rusqlite connections. Wrappers
around externally owned SQLite handles are unsupported. `apply_schema_plan` and
`apply_schema_plan_with_policy` now require `&mut Connection`; external Rust
callers must update their borrow. Success preserves the original connection,
including its temporary objects and configuration. An inherited transaction is
refused without settling it. When an operation cannot roll back, its owner retires
the connection before releasing the volume lease and returns
`WriterSettlementUnknown`; discard the inert replacement and reopen explicitly.
Raw migration, retired pooled, standalone transaction, and lifetime writer owners
share one owned-connection settlement helper. Only on the retired original,
cleanup clears the authorizer and retries ROLLBACK. A leaked prepared statement
can make SQLite close return BUSY; field-drop order alone does not prove closure.
A BUSY close is safe for lease release only after actual autocommit is established.
If neither autocommit nor successful close can establish settlement, the library
returns `WriterSettlementUnknown` after retiring the original connection. The raw
caller receives an inert replacement and must discard it. The host decides
whether to exit. Later pooled writes are refused. Unsafe wrappers around external
handles cannot prove ownership or actual close and are outside this API contract.

## Refusals and limits

`sqlite_capacity_refused` carries available, reserve and required-headroom bytes.
`sqlite_capacity_unavailable` distinguishes `identity`, `lock` and `probe` phases.
Neither asks for automatic retry. An operator may recover space or complete a
checkpoint and then make a new explicit attempt. Native `SQLITE_FULL` retains its
SQLite primary and extended result codes and separate escalation; it is never
reported as a preflight floor refusal. Its best-effort incident sink does not write
back to the guarded SQLite database.

Admission captures its physical volume before opening or writing and refuses if
the path resolves to a different volume at lease acquisition or before/after a
capacity probe. Pooled checkout also verifies the original opened-file identity;
a directory replacement cannot silently redirect a pooled write.

The lease coordinates participating processes sharing a lock namespace. Older
binaries, deliberate advisory-lock bypasses and unrelated applications remain
outside it. A single already-admitted large transaction can cross the reserve;
sampling does not reserve bytes or impose a hard filesystem quota.

## Isolated filesystem acceptance

The ignored Linux `capacity_floor_real_fs` test uses a caller-provided disposable
96–512 MiB filesystem. Before creating a database or filler, it checks that the
device differs from `/`, the workspace and HOME. It holds an actual reader while
repeated updates grow the WAL, requires a typed guard refusal before SQLite FULL,
then ends the reader, performs a bypassed TRUNCATE checkpoint and proves a new
ordinary write succeeds.

`scripts/test-sqlite-capacity-linux.sh` runs the compiled test binary in a private
user/mount namespace with a 256 MiB tmpfs. It exits 77 when isolation is unavailable
and requires the explicit test completion marker for success. An unavailable
namespace or an unexecuted platform test is not acceptance evidence. It never
fills a workspace, root or HOME filesystem as a fallback.
