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

| Tier | Copy                                             | RPO (max data loss) | RTO (time to serve again)            |
| ---- | ------------------------------------------------ | ------------------- | ------------------------------------ |
| 1    | Local replica on the same host                   | 15 minutes          | minutes (replica is a ready DB file) |
| 2    | Off-host replica on a second machine over SSH    | 1 hour              | minutes plus transfer                |
| 3    | Cold archive (dated snapshots, local + off-host) | 1 week              | hours                                |

Rationale for the numbers: the store's write stream is agent work product. Losing under
an hour of it is recoverable annoyance (session transcripts exist and can be re-ingested);
losing a day of graph writes is a real setback; losing the store is unacceptable. Tier 1
absorbs operator error and single-file corruption; tier 2 absorbs disk and machine loss;
tier 3 absorbs slow corruption discovered late (a bad state replicated to tiers 1-2 can be
rolled back to a dated archive).

Point-in-time recovery to arbitrary seconds is explicitly **not** an objective at these
targets. That decision drives the mechanism choice below and is revisited under
Consequences.

### Scope

- **In scope**: the primary store database, and by the same lane any other khive SQLite
  database an operator registers (session store, auxiliary graph stores). Each database
  is an independent backup unit with its own cadence.
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

**B. sqlite3_rsync (scheduled differential sync).** Ships with SQLite ≥ 3.50.0
(installable via Homebrew as `sqlite-rsync`; also builds from the SQLite source tree).
Synchronizes an origin database to a replica by comparing per-page hashes and
transferring only changed pages (~0.5% overhead on unchanged data), works over plain SSH
with the binary on both ends, and captures a consistent origin snapshot at sync start —
safe under a live writer, no checkpoint takeover, no resident daemon. The replica is
itself a well-formed SQLite database, which makes restore trivial and the restore drill
honest. RPO equals the schedule interval. Selected.

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
stderr land in a per-database log; a failed sync (nonzero exit, hash mismatch, SSH
unreachable) is surfaced through the deployment's alerting channel.

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
runbook:

1. Copy the replica to a scratch path on the recovery host.
2. `PRAGMA integrity_check` must return `ok`.
3. Row-count parity per substrate (entities, notes, edges, events) between origin and
   replica at the sync timestamp, within the delta written after that timestamp.
4. Boot a runtime against the restored copy and serve live verbs: `stats()`, one
   `search`, one `memory.recall`.
5. Rebuild the ANN index from the restored database (the ADR-079 rebuild path) and serve
   one vector-backed query, proving the out-of-scope decision for the index directory.

Steps 1-5 passing on a real restored copy — not on the origin — is the definition of
done for the implementing PR.

### Generalization and product path

The mechanism is deliberately expressible as configuration: a list of
`(database path, tier cadences, off-host target, retention counts)` entries. A later ADR
may graduate this into a daemon-native capability (a `backup` administrative surface with
status reporting), which is the natural product shape for managed deployments; this ADR
establishes the semantics that capability would wrap, and nothing in v1 blocks that
migration.

## Consequences

- RPO is bounded by schedule interval (15 min / 1 h), not by seconds. If a future
  requirement demands point-in-time restore, option A re-enters, and the checkpoint
  ownership conflict with ADR-091 must be resolved explicitly at that time rather than
  implicitly by installation.
- During a sync the replica is briefly read-only and the origin holds a read snapshot;
  at the measured page-delta rates this is seconds, not minutes, and does not contend
  with the WriterTask.
- Both hosts must keep `sqlite-rsync` installed and version-compatible; the install is
  part of the deployment runbook and the job fails loudly if the binary is absent.
- Writes that occur mid-sync land in the next cycle (the tool syncs to the origin
  snapshot taken at start); this is the correct trade for consistency and is within the
  stated RPO.
