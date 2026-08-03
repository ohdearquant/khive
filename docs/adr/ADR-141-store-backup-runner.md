# ADR-141: Executable store backup runner

- **Status:** Proposed
- **Date:** 2026-08-03

## Context

ADR-100 selects scheduled `sqlite3_rsync` for tiers 1 and 2, `VACUUM INTO` for tier 3, and a host scheduler independent of the daemon and orchestration stack. [ADR-100:124-126](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-100-store-backup-replication.md#L124-L126)

ADR-100 defines the required tier cadences as 15 minutes for a local replica, one hour for an SSH replica, and one week for a dated archive. [ADR-100:130-139](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-100-store-backup-replication.md#L130-L139)

The current `kkernel` command table has no backup variant, and an inspection measured on a development deployment did not identify an executable backup invocation. [crates/kkernel/src/cli.rs:53-103](https://github.com/ohdearquant/khive/blob/main/crates/kkernel/src/cli.rs#L53-L103); `rg -n --glob '!docs/**' --glob '!CHANGELOG.md' 'sqlite3_rsync|sqlite-rsync|VACUUM INTO' .`

ADR-100 requires each failed run to have a concrete failure log and a nonzero supervisor-visible exit status, including preflight, version, timeout, and SSH failures. [ADR-100:145-154](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-100-store-backup-replication.md#L145-L154)

`db_diagnostics()` reports WAL and checkpoint state for the main database, but it does not report replica or archive freshness. [docs/guide/api-reference.md:700-715](https://github.com/ohdearquant/khive/blob/main/docs/guide/api-reference.md#L700-L715)

## Decision

The deployment SHALL use a host-supervised job as the scheduling and restart boundary, with `kkernel backup run` as the one-shot executable payload. [ADR-100:124-126](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-100-store-backup-replication.md#L124-L126)

The host supervisor SHALL invoke one named job per configured cadence and retain the job's exit status, while `kkernel backup run --tier <tier> --config <path>` SHALL perform exactly one requested tier for each configured database. [ADR-100:145-153](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-100-store-backup-replication.md#L145-L153)

The host-supervised form is selected because ADR-100 requires the backup lane to survive a daemon or orchestration-stack failure. [ADR-100:145-146](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-100-store-backup-replication.md#L145-L146)

`kkernel backup run` SHALL be a finite process and SHALL neither daemonize nor implement its own persistent scheduler. [ADR-100:145-146](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-100-store-backup-replication.md#L145-L146)

### Configuration and exclusion

The configuration SHALL identify each database by a stable identifier, origin path, tier-1 replica path, optional tier-2 SSH target, tier-3 archive roots, cadences, retention counts, timeout budgets, local and remote `sqlite3_rsync` paths, an absolute path for the result sink, a per-tier successful-sync-duration bound, and a failure-detection lag. [ADR-100:38-48](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-100-store-backup-replication.md#L38-L48); [ADR-100:70-75](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-100-store-backup-replication.md#L70-L75); [ADR-100:269-274](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-100-store-backup-replication.md#L269-L274)

The successful-sync-duration bound is the configured worst-case wall-clock time a successful run of that tier is expected to take; the failure-detection lag is the configured worst-case delay between a run becoming overdue and the supervisor or a `backup.status()` reader noticing. Both default to the tier's timeout budget when not explicitly configured, and both are RPO inputs per ADR-100's `RPO = cadence + maximum successful sync duration + failure-detection lag` accounting. [ADR-100:38-48](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-100-store-backup-replication.md#L38-L48)

The runner SHALL not download or select a platform package at run time, because executable availability and version parity are deployment preconditions verified by the preflight. [ADR-100:102-105](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-100-store-backup-replication.md#L102-L105)

The runner SHALL acquire one per-database lock before preflight and release it only after it writes a terminal result record, so tier 1, tier 2, tier 3, and a restore drill cannot overlap for that database. [ADR-100:166-172](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-100-store-backup-replication.md#L166-L172)

An occupied lock SHALL produce an explicit terminal `skipped_overlap` result and a nonzero exit unless the supervisor has declared the invocation an intentional no-op. [ADR-100:45-48](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-100-store-backup-replication.md#L45-L48)

### Tier execution and preflight

For tier 1, the runner SHALL execute `sqlite3_rsync <origin> <local-replica-path>` on the configured cadence. [ADR-100:130-133](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-100-store-backup-replication.md#L130-L133)

For tier 2, the runner SHALL execute `sqlite3_rsync <origin> <user>@<host>:<replica-path>` over SSH on the configured cadence without suppressing a failure to reach the remote host. [ADR-100:133-135](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-100-store-backup-replication.md#L133-L135)

For tier 3, the runner SHALL create a dated archive with `VACUUM INTO`, transfer only the completed cold archive, and apply the configured local and off-host retention counts. [ADR-100:136-143](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-100-store-backup-replication.md#L136-L143)

Before tiers 1 and 2, the runner SHALL resolve `sqlite3_rsync` locally, collect its raw version output and a documented normalized release identifier, and reject an absent or unidentifiable executable. [ADR-100:94-105](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-100-store-backup-replication.md#L94-L105)

Before tier 2, the runner SHALL resolve the executable through the same non-interactive SSH invocation that will run the transfer, collect the remote raw output and normalized release identifier, and reject versions below 3.50.1 or any local-to-remote identifier mismatch. [ADR-100:102-105](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-100-store-backup-replication.md#L102-L105)

The runner SHALL enforce a per-run timeout, record the timeout as a failure, and terminate the child process before releasing the per-database lock. [ADR-100:43-48](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-100-store-backup-replication.md#L43-L48); [ADR-100:158-172](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-100-store-backup-replication.md#L158-L172)

The runner SHALL preflight target free space for the database, WAL, and temporary-file headroom before every sync or archive. [ADR-100:278-285](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-100-store-backup-replication.md#L278-L285)

The runner SHALL not copy a hot main database, WAL, or SHM file with a general file-copy tool. [ADR-100:177-183](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-100-store-backup-replication.md#L177-L183)

### Failure atomicity

The implementation SHALL establish, by test, whether a `sqlite3_rsync` run killed mid-transfer (by timeout, signal, or process death) leaves the destination path's previous replica state recoverable. [ADR-100:278-285](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-100-store-backup-replication.md#L278-L285)

If that test does not establish that the previous replica survives an interrupted run, tiers 1 and 2 SHALL instead sync into a staged path distinct from the destination and promote the staged result to the destination only after the sync completes successfully; a killed or failed run under this mode SHALL leave the previous destination replica untouched. [ADR-100:278-285](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-100-store-backup-replication.md#L278-L285)

"Promote" SHALL mean an atomic same-filesystem replacement of the destination path by the staged path (an atomic rename or an equivalent single-syscall atomic replace); the staged path SHALL reside on the same filesystem as the destination specifically so that promotion is not a copy, and SHALL NOT be satisfied by a copy-then-delete or delete-then-move sequence, because either sequence can itself fail after partially overwriting the destination. Before promotion, the runner SHALL fsync the staged file's contents; after promotion, the runner SHALL fsync the destination's containing directory where the platform requires a directory fsync for the rename to be crash-durable. The interruption test required above SHALL include an interruption injected during promotion itself, not only during sync, and SHALL confirm the result is always either the complete prior destination content or the complete new content, never a partially written destination. [ADR-100:278-285](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-100-store-backup-replication.md#L278-L285)

A timeout, signal termination, or process failure during any tier SHALL never leave the destination replica in a partially written state; the runner's terminal result record for that run states which of the two atomicity mechanisms (proven-recoverable-in-place, or staged promotion) was active for the invocation. [ADR-100:145-154](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-100-store-backup-replication.md#L145-L154); [ADR-100:278-285](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-100-store-backup-replication.md#L278-L285)

### Results, failures, and freshness

Every invocation SHALL append one durable JSON Lines record with `kind: "backup_run"`, a unique `run_id`, `database_id`, `tier`, `outcome`, `scheduled_at`, `snapshot_started_at`, `completed_at`, duration, destination identity, local and remote tool release identifiers when applicable, and an error code when unsuccessful. [ADR-100:43-48](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-100-store-backup-replication.md#L43-L48); [ADR-100:145-154](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-100-store-backup-replication.md#L145-L154)

Every unsuccessful invocation SHALL append a `kind: "backup_failure"` record with the same `run_id` to a dedicated failure JSON Lines sink, preserve stderr in the per-database job log, and exit nonzero. [ADR-100:145-154](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-100-store-backup-replication.md#L145-L154)

If either durable sink cannot be written, the runner SHALL emit an unbuffered `backup_failure` JSON record to stderr and exit nonzero, so the host supervisor retains a visible failure signal. [ADR-100:145-154](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-100-store-backup-replication.md#L145-L154)

The defined failure codes SHALL include `overlap`, `preflight_space`, `tool_missing`, `version_parse`, `version_floor`, `version_mismatch`, `ssh_unreachable`, `timeout`, `sync_failed`, `archive_failed`, `transfer_failed`, `retention_failed`, and `result_sink_failed`. [ADR-100:145-154](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-100-store-backup-replication.md#L145-L154); [ADR-100:278-285](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-100-store-backup-replication.md#L278-L285)

The implementation SHALL register `backup.status` as a read-only, Assertive verb in a new `backup` pack, following the single-pack-prefix rule every non-kg-substrate verb uses, rather than overloading `db_diagnostics()`. [ADR-023, lines 149-223]; [docs/guide/api-reference.md:700-715](https://github.com/ohdearquant/khive/blob/main/docs/guide/api-reference.md#L700-L715)

`backup.status(database_id?)` SHALL return, for every configured database and tier (or the single named `database_id` when supplied), the latest terminal run, most recent success, most recent failure, consecutive-failure count, configured cadence, timeout budget, successful-sync-duration bound, failure-detection lag, observed duration, `current` status, and the database's latest restore-drill outcome and completion time. An unknown `database_id` SHALL produce a validation error before any result-sink read. The verb-specific success value is `{ "databases": [ { "database_id", "tiers": [ { "tier", "current", "latest_run", "last_success", "last_failure", "consecutive_failures", "cadence", "timeout_budget", "successful_sync_duration_bound", "failure_detection_lag", "observed_duration" } ], "last_restore_drill", "last_restore_drill_completed_at" } ] }`, returned as the `result` field of the request DSL's per-op envelope (`ok`/`tool`/`result` — ADR-016, lines 376-390). `last_restore_drill` is the terminal outcome of the database's most recent `backup_restore_drill` record (`success`, `integrity_check_failed`, `manifest_mismatch`, or another defined drill failure code) and `last_restore_drill_completed_at` is that record's completion time; both are `null` when no restore drill has ever completed for that database, never a synthesized placeholder value. [ADR-100:38-48](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-100-store-backup-replication.md#L38-L48)

`current` SHALL be false whenever either condition holds: (a) the latest terminal run for that tier is a failure newer than the last success, or (b) the last success's `completed_at` is older than `now - (cadence + successful_sync_duration_bound + failure_detection_lag)`. Three examples fix the boundary: a success completed within the last cadence window is `current: true`; a success older than the window by more than the combined bound is `current: false` even with no recorded failure (a wedged or silently-stopped scheduler); and any failure recorded after the last success is `current: false` immediately, regardless of how recent that last success was. [ADR-100:38-48](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-100-store-backup-replication.md#L38-L48)

Consumers SHALL determine that a backup is current only by reading `backup.status()` and observing `current: true` for each required tier, rather than inferring freshness from a process exit status or a WAL diagnostic. [ADR-100:38-48](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-100-store-backup-replication.md#L38-L48); [docs/guide/api-reference.md:700-715](https://github.com/ohdearquant/khive/blob/main/docs/guide/api-reference.md#L700-L715)

`db_diagnostics()` remains the diagnostic surface for WAL size, checkpoint progress, and reader-pin evidence during a backup investigation. [docs/guide/api-reference.md:700-715](https://github.com/ohdearquant/khive/blob/main/docs/guide/api-reference.md#L700-L715)

### Restore drill

The implementation SHALL provide `kkernel backup restore-drill --database <id> --tier 2 --scratch <path>` and SHALL append one `kind: "backup_restore_drill"` result record to the same durable sink. [ADR-100:184-215](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-100-store-backup-replication.md#L184-L215)

The drill SHALL write a marker through the normal write path, execute the designated sync, capture the validation manifest from the freshly synchronized replica, restore to a scratch path, run `PRAGMA integrity_check`, and compare the restored manifest with the recorded replica manifest exactly. [ADR-100:233-258](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-100-store-backup-replication.md#L233-L258)

The drill SHALL boot a runtime against the restored copy, serve the specified live verbs, rebuild the ANN index, serve a vector-backed query, and record measured RTO. [ADR-100:208-215](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-100-store-backup-replication.md#L208-L215)

The supervisor configuration SHALL run the full tier-2 sync and restore drill after every schema-migration release, and `backup.status()` SHALL expose the latest drill result and completion time. [ADR-100:184-186](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-100-store-backup-replication.md#L184-L186); [ADR-100:286-288](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-100-store-backup-replication.md#L286-L288)

## Consequences

The deployment gains an executable boundary for the accepted tier policy, a supervisor-visible failure signal, and a queryable definition of backup freshness. [ADR-100:124-154](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-100-store-backup-replication.md#L124-L154)

The implementation adds a host-supervisor definition, a `kkernel backup` command family, persistent result sinks, and the `backup.status()` read surface. [ADR-100:145-154](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-100-store-backup-replication.md#L145-L154); [ADR-100:269-274](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-100-store-backup-replication.md#L269-L274)

The backup process can delay WAL checkpoint progress only for its bounded snapshot lifetime, so timeout, exclusion, and WAL measurements are acceptance requirements. [ADR-100:156-175](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-100-store-backup-replication.md#L156-L175)

The deployment must install and maintain compatible `sqlite3_rsync` executables on each endpoint because tier 2 refuses unavailable, below-floor, or mismatched versions. [ADR-100:102-105](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-100-store-backup-replication.md#L102-L105); [ADR-100:307-308](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-100-store-backup-replication.md#L307-L308)

## Alternatives considered

### A `kkernel` process that schedules and supervises itself

Rejected because ADR-100 assigns scheduling to the host specifically so the backup lane survives failure of the daemon and orchestration stack. [ADR-100:145-146](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-100-store-backup-replication.md#L145-L146)

### A daemon-native backup loop

Rejected for this amendment because ADR-100 permits a daemon-native administrative surface only as a later product path, while the accepted v1 mechanism remains host scheduled. [ADR-100:269-274](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-100-store-backup-replication.md#L269-L274)

### `db_diagnostics()` as the freshness surface

Rejected because `db_diagnostics()` exposes WAL and checkpoint observations for the main database, not tier execution, replica age, archive age, or restore-drill outcome. [docs/guide/api-reference.md:700-715](https://github.com/ohdearquant/khive/blob/main/docs/guide/api-reference.md#L700-L715)

### Litestream or raw file copying

Rejected because ADR-100 rejects Litestream's checkpoint-control conflict and prohibits raw copying of a hot WAL-mode database. [ADR-100:84-105](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-100-store-backup-replication.md#L84-L105); [ADR-100:177-183](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-100-store-backup-replication.md#L177-L183)
