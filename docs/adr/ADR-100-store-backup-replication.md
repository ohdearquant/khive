# ADR-100: Store backup and replication

**Status**: Accepted
**Date**: 2026-07-06
**Depends on**: ADR-067 (single-writer WriterTask), ADR-079 (ANN index persistence), ADR-091 (WAL checkpoint lifecycle)

## Context

A khive installation accumulates a knowledge store containing entities, edges, and notes. The
database uses WAL mode, and the long-running daemon owns writes through the WriterTask
(ADR-067). This data may not be reconstructible from another source, so restore capability is a
required part of storage operation.

Today the only protection is ad-hoc manual snapshots (`VACUUM INTO` a dated copy). There
is no mechanism: no automated cadence, no off-host copy, no retention policy, and no exercised
restore procedure. This ADR specifies one.

Two constraints shape the design:

1. **The database is hot.** The daemon holds the write path at all times. Plain file copy
   of a live WAL-mode database (`cp`, `rsync` on the raw file) can capture a torn state
   and is prohibited as a named invariant of this ADR. Every mechanism considered must
   provide transactional snapshot consistency under a live writer.
2. **The daemon owns WAL checkpointing.** ADR-091 gives the daemon an explicit checkpoint
   escalation lifecycle. Any backup mechanism that requires taking checkpoint control away from
   the application conflicts with that contract.

### Configurable recovery defaults

| Tier | Copy                                             | Default cadence | Recovery expectation                 |
| ---- | ------------------------------------------------ | --------------- | ------------------------------------ |
| 1    | Local replica on the same host                   | 15 minutes      | minutes (replica is a ready DB file) |
| 2    | Off-host replica on a second machine over SSH    | 1 hour          | minutes plus transfer                |
| 3    | Cold archive (dated snapshots, local + off-host) | 1 week          | hours                                |

**RPO accounting.** A completed sync is a snapshot of the origin _as of sync start_, so
the real bound on data loss is not the cadence alone:

> RPO = cadence + maximum successful sync duration + failure-detection lag.

The values in the table are public defaults, not guarantees for every store. Operators configure
cadence, timeout, retention, and failure notification for their workload. Jobs never overlap, each
sync has a hard timeout, and `snapshot_started_at` and `completed_at` are recorded per sync. A
configured recovery objective is valid only while measured sync duration and failure-detection
lag keep the equation above within its bound.

Tier 1's failure modes are specifically: accidental deletion of the primary database
file, localized corruption of the primary file, and a bad deploy or migration caught
before the next sync propagates it. Tier 1 is **not** protection against logical,
application-level mistakes (bad data written by a correct process): those replicate
faithfully within one cycle; tier 3's dated archives are the rollback for that class.
Tier 2 absorbs disk and machine loss. Before enabling a cadence, the acceptance benchmark must
measure sync wall time, transferred bytes, WAL growth, and foreground verb latency on a generated
SQLite fixture. The fixture size and mutation rate are parameters, and the benchmark records its
seed and configuration so results are reproducible. A configured cadence is accepted only when
the measured sync duration fits its timeout and foreground latency stays within the declared test
budget.

Point-in-time recovery to arbitrary seconds is explicitly **not** an objective at these
targets. That decision drives the mechanism choice below and is revisited under
Consequences.

### Scope

- **In scope**: every configured khive SQLite store in the deployment. Each database is an
  independent backup unit with its own cadence
  (the primary store uses the public defaults unless explicitly configured; lower-churn stores
  may use wider tier-1 cadences).
- **Out of scope, documented**: the ANN index directory. It is a derived artifact;
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
captures a consistent origin snapshot at sync start: safe under a live writer, no
checkpoint takeover, no resident daemon. The replica is itself a well-formed SQLite
database, which makes restore trivial and the restore drill honest. Version floor:
**≥ 3.50.1 on both ends**: 3.50.0 introduced the tool but shipped a defect in which the
final page could fail to transfer (fixed in 3.50.1), and mixed-version peers can
degrade or misbehave, so the job preflights `sqlite3_rsync --version` locally and
remotely and refuses to run on a floor violation or version mismatch. Selected.

**C. SQLite online backup API (custom tooling).** Safe under concurrent writes, but
always copies full pages with no differential persistence between runs, and building a
scheduler/retention/transport wrapper around it is hand-rolling what option B already is.
Rejected per find-existing-before-build.

**D. VACUUM INTO on a cadence.** Fully consistent and simple, but every cycle rewrites
and transfers the full database, so it fails the differential requirement for tiers 1-2.
Retained in a supporting role: it is the right tool for tier 3 dated
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

Per registered database, using the configurable defaults from the recovery table:

- **Tier 1**: every 15 minutes: `sqlite3_rsync <origin> <local-replica-path>`.
- **Tier 2**: hourly: `sqlite3_rsync <origin> <user>@<host>:<replica-path>` over SSH.
  The second host has `sqlite-rsync` installed; reachability failures log and alert but
  do not block tier 1.
- **Tier 3**: weekly: `VACUUM INTO '<archive-dir>/<db>-<date>.db'` locally, and the
  archive file copied to the second host (an immutable cold file may be transferred with
  ordinary tools; the hot-copy prohibition applies only to live databases). Retention:
  4 local weekly archives, 8 off-host, oldest pruned by the same job.

Bounded storage: tiers 1-2 hold exactly one replica each (page-synced in place); tier 3
is capped by count. Worst case per database ≈ 1 local replica + 4 archives locally, 1
replica + 8 archives off-host.

The schedule runs under the host scheduler rather than the daemon or request runtime, so the
backup lane survives failures in those components. Each job records a structured completion or
failure row and exits nonzero on failure. A configurable notification sink may receive failure
rows. When no sink is configured, the failure log and scheduler status are the detection path,
and the configured recovery objective must include their polling interval.

### WAL and checkpoint interaction

`sqlite3_rsync` is safe under a live writer because it holds a consistent read snapshot
of the origin for the duration of the sync. That snapshot does not take the write lock,
**but it is a WAL reader, and per ADR-091 a checkpoint cannot reclaim frames past the
oldest open reader**, so every sync pins the checkpoint boundary for its wall-time.
An unbounded sync duration would recreate the reader-lifetime
pathology ADR-091 exists to bound. The design therefore treats sync duration as a
first-class budget, not an incidental:

- The per-sync hard timeout (RPO accounting above) doubles as the WAL-pin bound; a sync
  that exceeds it is killed and alerted, never left holding the snapshot.
- Tier 1, tier 2, and tier 3 jobs for the same database never overlap (single per-database
  lock across all tiers), so at most one backup reader exists at a time.
- The reproducible acceptance benchmark measures WAL size before, during, and after each sync.
  Notification occurs if backup-attributable WAL growth exceeds the configured budget derived
  from ADR-091 checkpoint thresholds.

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

Executed against the tier-2 replica and documented as a runbook. The procedure is also covered by
a synthetic concurrency test that keeps the origin active throughout capture and verifies that
the reference manifest remains stable.

1. Write a backup-marker event carrying a fresh nonce through the normal origin write path.
2. Run the designated sync.
3. Capture a validation manifest from the completed replica in one read transaction. The manifest
   contains per-table row counts, maximum primary keys, and deterministic checksums. The marker
   written before the sync must be present.
4. Copy the closed replica to a scratch recovery path, keeping sidecars together or confirming
   they are empty before the copy.
5. `PRAGMA integrity_check` on the restored copy must return `ok`, and its manifest must equal the
   recorded replica manifest exactly.
6. Boot a runtime against the restored copy and execute `stats()`, `search`, and `context`.
7. Rebuild the ANN index from the restored database and execute one vector-backed query.
8. Record recovery time for steps 4 through 7.

Capturing the manifest from the completed replica avoids a race with writes that occur after the
origin snapshot begins. The marker proves that the sync carried origin state forward; the
integrity check, manifest comparison, runtime boot, and index rebuild test distinct recovery
properties. The concurrency test varies write cadence and sync duration from a fixed seed and
must pass without quiescing the origin.

### Configuration and extension

The mechanism is deliberately expressible as configuration: a list of
`(database path, tier cadences, off-host target, retention counts)` entries. A later ADR
may graduate this into a daemon-native capability with status reporting. This ADR establishes
the semantics that capability would wrap, and nothing in v1 blocks that migration.

### Operator runbook requirements

The implementation ships a runbook covering, at minimum:

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
  a recovery path (cold file, sidecars taken together or verified empty): restore is
  the one moment operators touch raw files, and it must not become the hot-copy
  violation.
- **Off-host protection**: replicas and archives contain the full stored graph; off-host copies
  require restrictive file modes, pinned SSH host keys, and
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
