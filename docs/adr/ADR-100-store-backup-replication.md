# ADR-100: Store backup and replication

**Status**: Proposed
**Date**: 2026-07-06
**Depends on**: ADR-067 (single-writer WriterTask), ADR-079 (ANN index persistence), ADR-091 (WAL checkpoint lifecycle)

## Context

A khive deployment accumulates a compounding knowledge store: entities, edges, notes,
memories, tasks, and messages written continuously by agents across months. The primary
database in the motivating deployment is ~4 GB in WAL mode with a long-running daemon
owning all writes through the WriterTask (ADR-067). Losing this store is not a
reinstall-and-continue event; the data is not reconstructible from any other source.

Today the only protection is ad-hoc manual snapshots (`VACUUM INTO` a dated copy). There
is no mechanism: no schedule, no off-host copy, no retention policy, and no exercised
restore procedure. This ADR specifies one.

Two constraints shape the design:

1. **The database is hot.** The daemon holds the write path at all times. Plain file copy
   of a live WAL-mode database (`cp`, `rsync` on the raw file) can capture a torn state
   and is prohibited as a named invariant of this ADR. Every mechanism considered must
   provide transactional snapshot consistency under a live writer.
2. **The daemon owns WAL checkpointing.** ADR-091 gives the daemon an explicit checkpoint
   escalation lifecycle, adopted after a live incident in which a 15.5 GB WAL starved the
   writer. Any backup mechanism that requires taking checkpoint control away from the
   application conflicts with that ADR and re-opens that incident class.

### Recovery objectives (the targets the design argues from)

| Tier | Copy                                             | Cadence    | RTO (time to serve again)            |
| ---- | ------------------------------------------------ | ---------- | ------------------------------------ |
| 1    | Local replica on the same host                   | 15 minutes | minutes (replica is a ready DB file) |
| 2    | Off-host replica on a second machine over SSH    | 1 hour     | minutes plus transfer                |
| 3    | Cold archive (dated snapshots, local + off-host) | 1 week     | hours                                |

**RPO accounting.** A completed sync is a snapshot of the origin _as of sync start_, so
the real bound on data loss is not the cadence alone:

> RPO = cadence + maximum successful sync duration + failure-detection lag.

The advertised objectives (worst-case loss ≈ 20 minutes for tier 1, ≈ 1.25 hours for
tier 2) therefore hold only under operational controls the implementation must provide:
jobs never overlap (a running sync suppresses the next trigger); each sync runs under a
hard timeout; `snapshot_started_at` / `completed_at` are recorded per sync as metrics;
and an alert fires when observed sync duration or consecutive failures invalidate the
objective. A silently wedged sync is an RPO breach in progress, not a warning.

Rationale for the numbers: the store's write stream is agent work product. Losing under
an hour of it is recoverable annoyance (session transcripts exist and can be re-ingested);
losing a day of graph writes is a real setback; losing the store is unacceptable.
Tier 1's failure modes are specifically: accidental deletion of the primary database
file, localized corruption of the primary file, and a bad deploy or migration caught
before the next sync propagates it. Tier 1 is **not** protection against logical,
application-level mistakes (bad data written by a correct process) — those replicate
faithfully within one cycle; tier 3's dated archives are the rollback for that class.
Tier 2 absorbs disk and machine loss. The tier-1 cadence carries a measurement gate: the
implementing PR must measure per-sync wall time and IO on the motivating ~4 GB database,
and the 15-minute cadence stands only if steady-state sync cost is trivially absorbed
(seconds of runtime, no measurable verb-latency impact); otherwise the cadence widens
and the ADR's RPO table is amended with the measured value.

Point-in-time recovery to arbitrary seconds is explicitly **not** an objective at these
targets. That decision drives the mechanism choice below and is revisited under
Consequences.

### Scope

- **In scope**: every khive SQLite store in the deployment. The v1 shipped default
  registers all three standing stores — the primary store database, the session store,
  and the graph store — from day one; additional databases are registered by
  configuration. Each database is an independent backup unit with its own cadence
  (the primary store runs the full tier table; lower-churn stores may run wider tier-1
  cadences, stated in the shipped configuration).
- **Out of scope, documented**: the ANN index directory. It is a derived artifact —
  vectors persist in the database itself (the `embeddings` and `vec_*` tables) and the
  index rebuilds from them (ADR-079). Backing up the database alone is sufficient; the
  restore procedure includes the rebuild step. Backing up the index files would couple
  the backup to index-format versions for no recovery benefit.

## Options considered

**A. Litestream (continuous WAL shipping).** Actively maintained (v0.5.x), replicates the
WAL stream to file/SFTP/S3-class targets, supports point-in-time restore. Rejected for
v1 on two grounds. First, checkpoint ownership: Litestream requires effective control of
WAL checkpointing (it holds a read lock to block other checkpointers and its guidance is
to disable application checkpointing). That directly conflicts with ADR-091, where the
daemon's checkpoint escalation is load-bearing, and it re-introduces the exact failure
class (external process pinning the checkpoint boundary) that ADR-091 exists to prevent.
Second, it buys point-in-time restore, which the stated RPO targets do not require, at
the cost of a permanently running third-party daemon in the write-critical path.

**B. sqlite3_rsync (scheduled differential sync).** Ships with the SQLite source tree
(installable via Homebrew as `sqlite-rsync`). Synchronizes an origin database to a
replica by comparing per-page hashes and transferring only changed pages (bandwidth on
unchanged data is a small fraction of database size; the exact ratio is
protocol-version-dependent), works over plain SSH with the binary on both ends, and
captures a consistent origin snapshot at sync start — safe under a live writer, no
checkpoint takeover, no resident daemon. The replica is itself a well-formed SQLite
database, which makes restore trivial and the restore drill honest. Version floor:
**≥ 3.50.1 on both ends** — 3.50.0 introduced the tool but shipped a defect in which the
final page could fail to transfer (fixed in 3.50.1), and mixed-version peers can
degrade or misbehave, so the job preflights `sqlite3_rsync --version` locally and
remotely and refuses to run on a floor violation or version mismatch. Selected.

**C. SQLite online backup API (custom tooling).** Safe under concurrent writes, but
always copies full pages with no differential persistence between runs, and building a
scheduler/retention/transport wrapper around it is hand-rolling what option B already is.
Rejected per find-existing-before-build.

**D. VACUUM INTO on a schedule.** Fully consistent and simple, but every cycle rewrites
and transfers the full database; at 4 GB per cycle it fails the differential requirement
for tiers 1-2. Retained in a supporting role: it is the right tool for tier 3 dated
archives, where a compacted, self-contained, immutable file per week is exactly what is
wanted.

**E. Hybrid (Litestream locally + rsync off-host).** Two mechanisms, two failure modes,
two restore procedures, and the ADR-091 conflict remains. Not simpler than B alone;
rejected.

## Decision

Adopt **sqlite3_rsync as the replication mechanism for tiers 1 and 2, with VACUUM INTO
dated archives as tier 3**, driven by the host scheduler (launchd on macOS), independent
of the daemon and of any orchestration stack.

### Mechanism

Per registered database:

- **Tier 1** — every 15 minutes: `sqlite3_rsync <origin> <local-replica-path>`.
- **Tier 2** — hourly: `sqlite3_rsync <origin> <user>@<host>:<replica-path>` over SSH.
  The second host has `sqlite-rsync` installed; reachability failures log and alert but
  do not block tier 1.
- **Tier 3** — weekly: `VACUUM INTO '<archive-dir>/<db>-<date>.db'` locally, and the
  archive file copied to the second host (an immutable cold file may be transferred with
  ordinary tools; the hot-copy prohibition applies only to live databases). Retention:
  4 local weekly archives, 8 off-host, oldest pruned by the same job.

Bounded storage: tiers 1-2 hold exactly one replica each (page-synced in place); tier 3
is capped by count. Worst case per database ≈ 1 local replica + 4 archives locally, 1
replica + 8 archives off-host.

The schedule runs under launchd rather than the daemon or any agent runtime: the backup
lane must survive precisely the failures that take those components down. Job stdout and
stderr land in a per-database log under the backup root. The alert surface is concrete,
not implied: on any failed sync (nonzero exit, timeout kill, preflight failure, version
mismatch, SSH unreachable) the job appends a structured line to a dedicated failure log
and sends a message through the store's own messaging surface to the operating seat
responsible for the deployment; if the store itself is unavailable (the failure mode
where messaging cannot work), the failure log plus a nonzero launchd last-exit-status
remain as the detection path, and the restore-drill cadence bounds how long that state
can persist unnoticed.

### WAL and checkpoint interaction

`sqlite3_rsync` is safe under a live writer because it holds a consistent read snapshot
of the origin for the duration of the sync. That snapshot does not take the write lock,
**but it is a WAL reader, and per ADR-091 a checkpoint cannot reclaim frames past the
oldest open reader** — so every sync pins the checkpoint boundary for its wall-time.
Run 96 times a day, an unbounded sync duration would be exactly the reader-lifetime
pathology ADR-091 exists to bound. The design therefore treats sync duration as a
first-class budget, not an incidental:

- The per-sync hard timeout (RPO accounting above) doubles as the WAL-pin bound; a sync
  that exceeds it is killed and alerted, never left holding the snapshot.
- Tier 1, tier 2, and tier 3 jobs for the same database never overlap (single per-database
  lock across all tiers), so at most one backup reader exists at a time.
- The implementing PR measures WAL size before, during, and after syncs on the motivating
  database and records it with the sync metrics; an alert fires if backup-attributable
  WAL growth exceeds the budget agreed with the ADR-091 checkpoint escalation thresholds.

The Consequences section's contention statement is scoped accordingly: the sync does not
block writes, and it may delay checkpoint progress only within the bounded window above.

### Named invariants

1. **Never file-copy a hot database.** All live-database capture goes through
   sqlite3_rsync or VACUUM INTO. WAL and SHM sidecar files are never copied or deleted
   independently of their database.
2. **The daemon keeps checkpoint ownership** (ADR-091). The backup lane must not hold
   locks that pin the checkpoint boundary beyond a single sync's duration.
3. **A backup that has not been restored is not a backup.** The restore drill below is
   part of the acceptance criteria for the implementation, and re-running it is part of
   operational cadence (at minimum after any schema-migration release).

### Restore drill (acceptance test)

Executed against the tier-2 replica (the most failure-relevant copy), documented as a
runbook. The drill compares the restored copy against a **recorded validation manifest**,
never against the moving origin — a comparison whose reference keeps changing is not
falsifiable.

1. Immediately before a designated sync, write a backup-marker row through the normal
   write path (a marker event carrying a fresh nonce), then capture a manifest: per-table
   row counts, per-table `MAX(rowid)` (or max primary key), and a cheap deterministic
   checksum per substrate table, all in the same read transaction. Store the manifest
   beside the sync metrics.
2. Run the sync; copy the resulting replica to a scratch path on the recovery host
   (the replica is a closed, cold file at that point; ordinary copy is permitted, taking
   any `-wal`/`-shm` sidecars together with it or after confirming they are empty).
3. `PRAGMA integrity_check` on the restored copy must return `ok`.
4. The marker row must be present in the restored copy, and the restored copy's counts,
   max ids, and checksums must equal the manifest exactly. No tolerance window: the
   manifest was captured at the same instant the marker was written, so any divergence
   is a defect, not "delta after the timestamp".
5. Boot a runtime against the restored copy and serve live verbs: `stats()`, one
   `search`, one `memory.recall`.
6. Rebuild the ANN index from the restored database (the ADR-079 rebuild path) and serve
   one vector-backed query, proving the out-of-scope decision for the index directory.
7. RTO is measured as the wall time of steps 2-6 and recorded with the drill result.

Steps 1-7 passing on a real restored copy — not on the origin — is the definition of
done for the implementing PR.

### Generalization and product path

The mechanism is deliberately expressible as configuration: a list of
`(database path, tier cadences, off-host target, retention counts)` entries. A later ADR
may graduate this into a daemon-native capability (a `backup` administrative surface with
status reporting), which is the natural product shape for managed deployments; this ADR
establishes the semantics that capability would wrap, and nothing in v1 blocks that
migration.

### Operator runbook requirements (week-1 controls)

The implementing PR ships a runbook covering, at minimum:

- **Disk preflight**: before each sync or archive, verify free space on the target with
  headroom for the database plus WAL plus any tool temporary files; a preflight failure
  is an alert, not a silent skip.
- **Failure atomicity**: establish (by test) whether an interrupted `sqlite3_rsync` run
  leaves the previous replica state recoverable; if not, sync into a staged path and
  promote on success, so a mid-sync crash never destroys the only replica.
- **Schema migrations**: backup jobs pause around a migration release; after the
  migration, a tier-2 sync plus a full restore drill run before the lane is considered
  healthy again (also the standing drill cadence per invariant 3).
- **Sidecar handling on restore**: the runbook states exactly how a replica is copied to
  a recovery path (cold file, sidecars taken together or verified empty) — restore is
  the one moment operators touch raw files, and it must not become the hot-copy
  violation.
- **Off-host protection**: replicas and archives contain messages, memories, and tasks;
  off-host copies require restrictive file modes, pinned SSH host keys, and
  encryption-at-rest on the target (or an encrypted volume), stated in the runbook.

## Consequences

- Worst-case data loss is cadence plus sync duration plus detection lag (see RPO
  accounting), monitored and alerted rather than assumed. If a future requirement
  demands point-in-time restore, option A re-enters, and the checkpoint ownership
  conflict with ADR-091 must be resolved explicitly at that time rather than implicitly
  by installation.
- During a sync the replica is briefly read-only and the origin holds a read snapshot;
  the sync does not take the write lock, but it may delay WAL checkpoint progress within
  the bounded window defined in "WAL and checkpoint interaction".
- Both hosts must keep `sqlite-rsync` at or above the 3.50.1 floor; the job preflights
  versions on both ends and fails loudly on absence, floor violation, or mismatch.
- Writes that occur mid-sync land in the next cycle (the tool syncs to the origin
  snapshot taken at start); this is the correct trade for consistency and is accounted
  for in the stated RPO.
