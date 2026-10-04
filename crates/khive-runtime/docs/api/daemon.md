# Daemon Wire Protocol and Metrics

`daemon.rs` implements the long-lived `kkernel mcp --daemon` process that keeps ANN/embedder
state warm across MCP sessions (ADR-049): its framed request/response protocol, boot-time
locking, and the metrics snapshot served to callers. This document covers the versioned wire
protocol, the two lock-acquisition strategies the daemon uses, and where each metrics gauge is
sourced from.

## protocol_version

Version history for `PROTOCOL_VERSION`:

- 1 — initial versioned framing (added `protocol_version` + `version_mismatch`); added
  `probe_only` request field + probe-ack sentinel shape in response
- 2 — gate subhandler verbs by wire origin (`from_wire` request field)
- 3 — added per-request identity context to the request frame (`actor_id`,
  `visible_namespaces`); the daemon now serves a request under the frame's identity instead of
  rejecting on `namespace_mismatch` (the `config_id` equality reject stays hard)
- 4 — added request-origin `process_ref` attribution. Although optional, it can affect durable
  comm message properties, so v3/v4 peers reject each other before dispatch instead of allowing a
  v3 daemon to execute a write while silently ignoring the new field.
- 5 — added `plan` (default false). A true value returns syntax and loaded-catalog information
  before request identity construction or dispatch. Older daemons reject v5 frames before they
  could ignore the flag and execute the operations. Restart a warm daemon when upgrading clients.
- 6 — added new resident verbs. Older bridges re-exec onto the installed binary instead of
  forwarding to a warm daemon that lacks the new catalog.
- 7 — expanded the verb set and the `knowledge.search` candidate provenance response.
- 8 — a daemon may serve a client whose extra embedders are a subset of its own. Older bridges
  require exact config ids and could replay a successful write locally after a compatible daemon
  serves it; reject their v7 frames before dispatch during a rolling upgrade.

`process_ref` carries the originating client's opaque `KHIVE_PROCESS_REF`; it is request
attribution, not identity, and prevents a shared daemon from substituting its own process
environment. Its `serde(default)` preserves the meaning of an omitted value within protocol v4;
it does not make a v3 daemon safe to use. `request_id` is the independent additive numeric
audit-correlation field and does not alter dispatch or persisted verb output.

Plan frames retain the protocol and configuration checks. They reject the presence of
`presentation`, `presentation_per_op`, `format`, `format_per_op`, or `request_id`, including null
values, with an `invalid_params` error naming the field. The response `result` contains the same
JSON object as `request(ops, plan=true)` and `Session.plan(ops)`. A grammar error is a successful
response whose result has `parsed=false`; planning never grants permission or resolves `$prev`.

An accepted socket has 30 seconds to supply its complete initial length-prefixed
request frame. The bound includes the length header and body; an incomplete frame
closes that connection without entering dispatch. The deadline is captured at
acceptance, before peer checks and connection-task scheduling. The per-request
read deadline starts after a complete frame is decoded and remains independent
of this bound. A frame buffered before a delayed task starts still expires at
the original acceptance deadline.

## try_acquire_flock_until

Unlike `acquire_recovery_lock`/`acquire_daemon_boot_guard` (unbounded blocking `flock`, correct
for the daemon's own boot sequence where waiting until quiescence IS the desired behavior), a
caller only trying to _detect_ whether a lock is currently free — without committing to wait
forever for a possibly-wedged holder — needs a deadline instead.

## Store-bound daemon ownership

The HOME-bound socket, PID file, and boot/recovery lock coordinate one client
rendezvous. They do not identify a SQLite store: two clients with different
`HOME` values may discover the same absolute `[[backends]]` paths. Both
`kkernel mcp --daemon` boot paths therefore claim a separate exclusive
`.DATABASE.khived.lock` sidecar beside
each canonical file-backed database path before runtime construction opens
stores or runs migrations. Under the HOME boot lock, a bounded protocol
probe first refuses an identified daemon already serving the same socket.
The claims are sorted and deduplicated across the configured topology, held
until serving and shutdown finish, and never unlinked. A store contender
refuses immediately; it names the holder PID when that PID has been written,
or reports an unknown holder during the narrow
post-lock/pre-PID window. In-memory backends take no store lock. The guard is
daemon-only; ordinary local/stdio writers keep their existing coordination.

Each daemon freezes the canonical pathname used for its actual SQLite open.
It revalidates a configured symlink spelling after claiming and refuses a
retarget, naming both the claimed and current paths; it never opens the
re-resolved spelling. For an existing database, the guard records `(dev, ino)`
at claim and checks it while binding an open file under the lock. For a new
writable database, it claims the sidecar first, creates the canonical file
under that lock, then binds its new identity. A missing read-only database
refuses without creating it. The daemon re-stats every canonical pathname
after SQLite construction and refuses startup on an observed identity change.
Each daemon backend passes the bound descriptor's identity into pool construction.
The pool compares that identity with the path before SQLite open. On verified
64-bit Linux and macOS, a forwarding observer records the bundled Unix VFS's
actual descriptor `fstat` identities during the synchronous writer open. Every
observation must match the held claim, and at least one must exist, before the
pool executes any SQL or enables WAL. Restoring the original pathname does not
hide a different opened descriptor. Claimed opens also disable file creation.
The `kkernel` binary installs this observer before CLI or Tokio startup; library
hosts must call `khive_db::pool::initialize_claimed_file_observer` before any
SQLite file I/O, following its unsafe API contract. Uninitialized or unsupported
VFS/ABI paths fail closed for claimed construction. Ordinary unclaimed pools
retain their existing API.

The callback preserves native descriptor ownership and POSIX lock handling.
Its check does not precede all native open-time I/O: SQLite can write to an empty
file on macOS msdos/exfat during open, and the startup contract requires trusted
nonmutating autoextensions and an unchanged syscall table. Post-open path checks
remain ahead of schema preparation and serving.

Distinct hardlink names for the same SQLite inode have different sidecars:
a second daemon can claim the other name. Hardlink aliases are unsupported.
A database replaced at the same canonical pathname still uses the same
persistent sidecar. The database directory must permit sidecar creation,
including when the database itself is opened read-only, or daemon boot fails
closed.

## build_metrics_snapshot

`tx_registry` (ADR-091 Plank 0) is a process-global singleton reachable directly, with no plumbing
through `dispatcher`. TRUNCATE counters remain module-scoped atomics. The logical WAL fields
(`wal_log_frames`, `wal_checkpointed_frames`, and `wal_pending_frames`) and
`wal_physical_bytes` come from the dispatcher's exact pool's last routine PASSIVE tick; building a
metrics frame does not issue an on-demand checkpoint or stat the sidecar. `wal_pages` remains the
compatibility projection of `wal_log_frames`. The backend key is load-bearing in a multi-backend
daemon: a secondary task's later tick cannot relabel its state as the main pool's sample.

`wal_checkpoint_stores` is an additive, default-empty array for this daemon's checkpoint
topology. Each row has `store_id` (`main` or `secondary:<index>` in dispatcher order),
`role`, and a basename-only `database` display label. Equal basenames do not merge rows;
canonical paths stay inside the process. IDs are stable only within the same process
and unchanged topology. Split-daemon stores are not measured by this process.

Each row contains cumulative `ticks`, `elapsed_us_sum`, `elapsed_us_max`, `busy_ticks`,
and `error_ticks` for actual routine PASSIVE calls, including failed calls. Skipped ticks
and the post-TRUNCATE observation are excluded. Time uses a monotonic clock around the
checkpoint call only; microseconds retain sub-millisecond work. SQLite's nonzero busy
result increments `busy_ticks`; pending frames alone do not. A call error increments
`error_ticks` and has no returned busy flag. Counters saturate at `u64::MAX`.

For two snapshots of the same store/process, mean call time is
`delta(elapsed_us_sum) / delta(ticks)` and the returned-busy rate per call is
`delta(busy_ticks) / delta(ticks)` when the tick delta is positive. Use
`delta(error_ticks)` to distinguish calls without a SQLite result; for a rate over
returned rows only, subtract that delta from the denominator. `elapsed_us_max` is the
maximum since process start, not an interval maximum. Restart, topology changes,
counter decreases or saturation require a fresh measurement interval. Metrics reads
do not issue checkpoint calls or filesystem probes.

`write_queue_depth`/`_capacity` (ADR-067 Component A) come from the same pool and are `None`
unless a writer task exists. The `write_last_*_micros` fields expose that task's latest completed
queue-wait, transaction-acquisition, body, commit, and total stages, plus the observation time.
All new fields are additive `serde(default)` metrics-only fields; older peers can omit them without
changing request dispatch or the canonical verb result.

## Demand lifetime and voluntary retirement

`kkernel mcp --daemon` defaults to persistent mode. Thin clients and recovery
spawns pass `--lifetime demand`; a supervisor launch continues to use persistent
mode. Mode belongs to that process incarnation and is never inferred from a
marker, parent process, or environment variable.

Demand mode uses a positive `--idle-timeout-secs` interval, defaulting to 1800
seconds. That default is ADR-049 Amendment 11's initial placeholder; this change
makes no measured tuning claim. The clock starts when the daemon is ready and
restarts after an admitted ordinary request finishes its response and cleanup.
Probe, planning, metrics, WAL checkpoint and cache-maintenance traffic do not
restart it. A connected client with no admitted request or retained state does
not keep the daemon alive.

Demand startup skips email and Telegram inbound/outbound loops compiled into the
host, and skips the configured schedule ticker. Logs and lifecycle diagnostics
name each skipped component. Persistent startup keeps the existing component
behavior. A configuration capable of supervising an exclusively owned events
child is idle-ineligible: the current supervision interface cannot prove that
an existing events service will remain independently owned. No events-child
shutdown request is introduced.

Retirement is prevented by admitted ordinary requests, open SQL transactions,
held raw-SQL writer permits (including autocommit handles), a checked-out pooled
writer, active operation phases, and unsettled tracked workers. Unknown dispatcher
resource inventory and linked components are named blockers. Blob upload sweeping
is a service obligation without an idle completion contract and therefore makes
that configuration idle-ineligible. The inspected WAL checkpoint and memory/knowledge
ANN rotation-watch loops are expendable maintenance; their tracked lifetimes still
participate in final drain. Maintenance does not reset ordinary activity.

Hosts without a disclosed main checkpoint pool (including in-memory or registry-only
constructors) report `main_backend_pool_inventory_unavailable` and remain idle-ineligible.
Their currently available host interface does not certify a complete pool inventory.

The idle decision and ordinary-request admission share one mutex. Once retirement
wins, the daemon closes its listener and refuses a later ordinary frame before
dispatch. It stops new maintenance, retains its rendezvous ownership while workers
settle, and keeps draining if its drain interval expires. Voluntary retirement
never aborts admitted work solely because that interval elapsed. An operator signal
uses the existing signal shutdown path, including its bounded drain and repeat-signal
behavior. Initial frames and response writes each have a 30-second transport bound;
the response bound does not interrupt dispatch or admitted writes.

`metrics_only` diagnostics carry an optional `metrics.lifecycle` object. Its
fields are `lifetime` (`demand` or `persistent`), `instance_generation` (a UUID
stable for one process incarnation), `effective_idle_interval_ms`, `phase`
(`serving`, `draining`, `stopped`), `shutdown_reason` (null, `idle`, or
`signal`), `skipped_components`, `idle_ineligible_reasons`, `ordinary_requests`,
and `idle_blockers`. The interval is reported in milliseconds, saturating at the
wire integer's maximum for an extreme configured duration. The lifecycle field
is additive and absent from older snapshots; it does not change protocol version 8.
