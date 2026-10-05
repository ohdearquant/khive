# SQLite disk admission

[ADR-154](../../../../docs/adr/ADR-154-sqlite-disk-reserve-admission.md) defines
cooperative admission for logical writes on a physical volume. A write unit takes
the volume lease before SQLite's writer slot, samples available space at its
execution boundary, and retains the lease through proven autocommit or successful
owned connection close. A failed rollback cannot release the lease while the poisoned connection
still holds SQLite's transaction. Checkpoints and read-only work remain available
for recovery.

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
lease and a successful capacity probe. VACUUM still requires its working headroom.

The deadline resolves from the backend override, then
`KHIVE_SQLITE_DISK_GUARD_DEADLINE_MS`, then 2000 ms. The supported range is
100–10000 ms. Runtime construction captures environment policy once; daemon
identity, backend openers, spawned children, aliases and the events sidecar use
that resolved policy. Aliases of the same file must agree on the numeric policy.

The host supplies an absolute volume-lock directory. Runtime resolves
`KHIVE_VOLUME_LOCK_DIR`, otherwise its runtime directory's `sqlite-volume-locks`
child. Cooperating processes must share that directory. Low-level database code
does not discover HOME: file-backed callers supply `PoolConfig.volume_lock_dir`,
`StorageBackend::sqlite_with_volume_lock_dir`, or the constructor accepting both
captured policies. An absent or relative directory fails closed when a write
requires the volume lease. Test callers use private fixture directories; the CI
empty-HOME check remains in force.

Memory and read-only backends do not take the disk lease or capacity probe.
Diagnostics report the resolved numeric values and their configuration sources,
the identified volume and probe path, and any inability to inspect that volume.
The guard being enabled does not imply a nonzero floor.

## Write boundary inventory

This table identifies logical boundaries, rather than treating connection
checkout as proof that a later write was admitted. The referenced symbols are in
`crates/khive-db/src` unless stated otherwise.

| Boundary                                                                                                | Lease and sample                                                                                    | Settlement / recovery                                                                                                               |
| ------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------- |
| `writer_task::run_writer_task` transaction requests                                                     | Per dequeued request, lease before BEGIN; probe after successful BEGIN and before the callback      | Wrapped transaction settles before releasing the lease; terminal return and unwind prove autocommit or successful owned close first |
| `WriterGuard::transaction`, `ConnectionPool::transaction_write_unit`                                    | Pool owns lease before its writer mutex; probe after BEGIN                                          | Rollback on refusal; a poisoned pooled connection is replaced; autocommit or successful owned close precedes lease release          |
| `ConnectionPool::standalone_transaction_write_unit`, `execute_direct_transaction`                       | Owned connection and lease, post-BEGIN sample                                                       | Autocommit or successful owned close precedes lease destruction on failure or unwind                                                |
| `ConnectionPool::autocommit_write_unit`, backend constructor DDL, entity/note/attachment/sparse helpers | Lease and probe before the first SQLite write in the logical script                                 | Guard remains alive through script completion and restores or retires an unfinished transaction                                     |
| Backend FTS rowid-map backfill                                                                          | Constructor lease remains held; a separate post-BEGIN probe precedes backfill DML                   | Refusal rolls back without filling the map or stamping completion; an explicit later attempt may retry                              |
| Graph mutation plus events                                                                              | Standalone lease or pooled guard survives the closure; shared transaction helper probes after BEGIN | Existing usage accounting and settlement classification remain; a terminal connection cannot be reused                              |
| Schema and core migration bootstrap                                                                     | Probe immediately before each pre-BEGIN ledger DDL                                                  | Lease remains held while the bootstrap returns to autocommit                                                                        |
| Per-migration, schema-plan and attachment-cutover transactions                                          | Probe after each BEGIN, before that transaction's DDL/DML                                           | Rollback on refusal and preserve uncertain-settlement errors                                                                        |
| Typed top-level VACUUM                                                                                  | Lease and current database/WAL working-set estimate immediately before execution                    | Missing metadata, threshold overflow and insufficient headroom refuse before VACUUM                                                 |
| Typed checkpoint guard, checkpoint task and diagnostic PASSIVE probe                                    | Deliberate recovery bypass of lease and floor                                                       | The checkpoint capability does not expose ordinary SQL                                                                              |
| Pure reads, backend schema-version and attachment-status diagnostics                                    | Reader checkout; no logical-write admission                                                         | Remain usable when the reserve is exhausted                                                                                         |

The events sidecar resolves its policy from the actual main pool, including when
requested through a runtime bound to another backend. Cached registry reuse
requires equivalent effective policy. Disk admission does not alter the separate
WAL ceiling or SQLite file-identity rules.

Dedicated code-map pools receive their own runtime's captured disk policy and
absolute volume-lock directory through `sqlite_code_map_with_policies`. Their
registered handle-proving VFS, DELETE journal mode, guarded migration refusal
mapping, and no-parent-creation rule remain in force. The compatibility constructor
resolves explicit environment settings; it does not supply a HOME fallback.
The pre-pool WAL-to-DELETE transition and its callees are unchanged in this slice.

## Remaining work

| Follow-up | Outstanding boundary                                                                                                                                                                                                                  |
| --------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| #3594     | Public raw pooled `writer` / `try_writer` handles can issue SQL outside the typed transaction boundary; checkout-time sampling is insufficient                                                                                        |
| #3595     | Public `open_standalone_writer` returns a raw connection whose later or repeated writes are not an owned logical unit                                                                                                                 |
| #3596     | Internal raw standalone connection escapes need a complete call-site conversion / inventory                                                                                                                                           |
| #3597     | Cached `SqlWriter` handles and manual atomic SQL need execution-time leases and SQLite-aware classification on every actual write                                                                                                     |
| #3551     | The code-map `prepare_rollback_target` WAL-to-DELETE transition, including its checkpoint and sidecar work before pool construction, remains outside this slice; a later ADR-154 slice must first decide its admission classification |

#3598's restricted checkpoint capability is retained. These open boundaries keep
#3551 open; the foundation does not claim a universal guard over all SQL a caller
can execute. Backend/pool-owned
migration entry points carry the captured policy.

Caller-owned raw connections can use `MigrationWritePolicy::new(effective_policy,
absolute_lock_dir)` and `run_migrations_with_policy`, `apply_schema_plan_with_policy`,
`stage_attachment_cutover_with_policy` or `finalize_attachment_cutover_with_policy`.
The policy validates before database work and never rereads the environment.
Compatibility raw names resolve numeric environment policy and require an explicit
`KHIVE_VOLUME_LOCK_DIR` for file-backed connections; missing or relative namespaces
fail closed without a HOME fallback. In-memory raw connections remain exempt.
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
If neither autocommit nor successful close can establish settlement, the process
aborts after emitting the database path, volume key, rollback error, and close
error to stderr and tracing. A normal error must never release the volume lease
while leaving an unresolved transaction alive. Unsafe wrappers around external
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
