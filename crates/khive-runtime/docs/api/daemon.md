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

## try_acquire_flock_until

Unlike `acquire_recovery_lock`/`acquire_daemon_boot_guard` (unbounded blocking `flock`, correct
for the daemon's own boot sequence where waiting until quiescence IS the desired behavior), a
caller only trying to *detect* whether a lock is currently free — without committing to wait
forever for a possibly-wedged holder — needs a deadline instead.

## build_metrics_snapshot

`tx_registry` (ADR-091 Plank 0) is a process-global singleton reachable directly, with no plumbing
through `dispatcher`. `wal_pages` and the TRUNCATE counters (ADR-091 Plank 2) are read from
`khive_db::checkpoint`'s module-scoped atomics, updated wherever the checkpoint task already calls
`query_wal_pages`/`note_truncate_outcome` — mirroring the fallback-counter pattern in
`khive-mcp/src/daemon.rs` rather than threading a metrics handle through every checkpoint call
site, since the checkpoint task itself is a fire-and-forget `tokio::spawn` with no handle retained
anywhere this accept loop can reach. `write_queue_depth`/`_capacity` (ADR-067 Component A) come
from the dispatcher's own pool, if any, and are `None` unless `KHIVE_WRITE_QUEUE=1` actually
spawned a writer task.
