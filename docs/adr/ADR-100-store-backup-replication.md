# ADR-100: Store backup and replication

**Status**: Accepted
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

### Amendment (2026-07-07): routine drill captures from the replica

Steps 1-2 above (capture the manifest from the origin immediately before the designated
sync, then compare a later restored copy against that recorded capture) assume the origin
is quiescent for the capture-to-sync window. Live evidence on the motivating ~4.1GB
production store shows that assumption does not hold there: an audit `events` table takes
a row from every client dispatch on a roughly 10-second cadence, against a roughly
60-second capture-to-sync window for that store's tier-1 sync. Five controlled origin-capture
attempts were run; all five failed, and in every case the manifest diff isolated exactly
the writes that landed inside the capture-to-sync window — the differ was correct each
time, and the contract it was checking (capture the moving origin, then compare a
later-synced replica against that single instant) was the thing racing. Quiescing this
store for the window is not achievable in its current deployment: its clients fall back to
direct dispatch when the daemon that would otherwise hold writes is stopped, so there is
no quiescent origin state to capture against short of a real maintenance window.

The routine drill is amended to capture from the freshly-synced replica instead of the
origin:

1. Write the marker row through the normal write path, to the origin, as before (step 1
   is unchanged).
2. Run the designated sync.
3. Capture the validation manifest from the tier-1 replica the sync just produced, not
   from the origin. The marker round-trip is still mandatory here and is the whole proof
   this drill has any content: the marker was written to the origin _before_ the sync, so
   its presence in the replica is the evidence the sync actually carried the origin's
   state forward. Without that check, a replica-captured manifest compared to a
   replica-restored copy proves only "the replica equals itself" — not a restore drill.
4. Validate (unchanged semantics): restore the backup to a scratch copy, run
   `PRAGMA integrity_check`, and compare the restored copy's manifest against the
   recorded manifest exactly — the manifest recorded in step 3, never a manifest
   recaptured from the moving origin at validate time.

This keeps the falsifiability property the original design was protecting: the reference
manifest is still a single point-in-time recording, never a comparison against a target
that keeps changing. What moved is which side of the sync that recording is taken from —
the replica, whose state is fixed the instant the sync completes, rather than the origin,
which a live multi-writer store never holds still.

Residual risk: A sync that corrupts pages would self-consistently pass the manifest
comparison; that surface is covered by `PRAGMA integrity_check`, the marker round-trip, and
the runtime-boot steps of the drill.

The origin-exact drill (steps 1-7 as originally specified above) is retained for
maintenance-window drills, bound to the schema-migration re-drill cadence this ADR already
names in the operator runbook requirements: the first genuine maintenance window runs the
origin-exact drill once, on a store held quiescent for the window, and records the result.
It is not part of the standing operational cadence on a live store — the replica-capture
drill is.

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

## Amendment (2026-10-24): a storage-layer database copy, registered as a named WAL holder

**Status**: Proposed. Refs #1845; the scheduled tiers in #5562 build on it.

### Context

Tier 3 above names `VACUUM INTO`, but no storage-layer operation runs it today, and the
two nearest surfaces do not fit:

- `TopLevelMaintenance` (`crates/khive-storage/src/sql.rs:32-53`) is a closed set of
  statements that take no parameters. Its own doc says that a command needing a caller
  value, such as a destination path, needs a separate parameter-bound primitive.
- `SqlWriter::execute_script_top_level` sends those statements through the single writer
  task (`crates/khive-db/src/sql_bridge/sqlite_writer.rs:544-566`, `send_vacuum_bounded`).
  A copy routed there would hold the only writer for the copy's whole length.

The write-ahead-log holder census also has no name for a copy. The in-process half of the
census is the open-span registry (ADR-091 Plank 0, `crates/khive-storage/src/tx_registry.rs`),
which the checkpoint task reads when it reports what holds the WAL. A copy that does not
register there looks like any other reader, so an operator watching the WAL grow during a
backup cannot tell the backup from a leaked reader.

#1845 asked for SQLite's stepped online backup API. This amendment takes `VACUUM INTO`
instead, for four reasons:

1. SQLite restarts a stepped backup whenever the source changes through any connection
   other than the one running the backup (`sqlite3_backup_step`,
   https://sqlite.org/c3ref/backup_finish.html). Every write here arrives through the
   pool's writer connections, so on a store that keeps taking writes a stepped copy can
   restart without bound.
2. A backup run as a single step does not restart, but it holds one read snapshot for the
   whole copy. `VACUUM INTO` holds exactly that, and it also writes a compacted file.
3. khive-db builds rusqlite without the `backup` feature (`crates/khive-db/Cargo.toml:30`);
   `VACUUM INTO` needs no new feature.
4. This ADR already selects `VACUUM INTO` for tier 3.

The issue's criterion of "bounded incremental stepping" is therefore replaced by the
deadline in Decision 5. The copy pins the WAL for its own length and no longer.

SQLite facts this amendment relies on, from https://sqlite.org/lang_vacuum.html: the INTO
argument is "an arbitrary SQL expression that evaluates to a string"; "the file named by
the INTO clause must not previously exist, or else it must be an empty file"; "VACUUM (but not
VACUUM INTO) is a write operation"; a VACUUM "will fail if there is an open transaction"
on its connection; SQLite syncs the output to disk only "if the PRAGMA synchronous setting
of the original database is NORMAL or FULL"; and an interrupted `VACUUM INTO` can leave an
incomplete output file.

Read in the SQLite source that khive-db bundles (3.53.2, `sqlite3RunVacuum`): for `VACUUM INTO`
the statement clears the read-only open flag for the output file only, opens a read
transaction (not a write transaction) on the source, refuses an output file larger than zero
bytes, and gives the output the source connection's sync level. The copy statements it runs
write to the output, so a connection with `PRAGMA query_only` on refuses them; the pool's
standalone read connections do not set it (it is set only on the read-only writer slot,
`crates/khive-db/src/pool/connection_setup.rs:285`).

### Decisions

1. **Interface.** One new operation on `SqlAccess` in khive-storage, beside (not inside)
   `TopLevelMaintenance`: it takes a destination path and a deadline and returns the
   destination path and the copy's size in bytes. It sits on `SqlAccess`, not
   `SqlWriter`, because it never uses a writer. The default implementation refuses it as
   unsupported, and an in-memory database refuses it because there is no file to copy.
   `TopLevelMaintenance` stays closed and parameter-free; no variant is added.

2. **Connection.** The copy runs on its own standalone read connection, opened for the copy
   and closed when it ends. It never goes through the writer task and never takes a pooled
   reader: a pooled reader is held for one request under the request read deadline and
   then returns to the pool, while a copy holds its connection for its whole length, which
   can be hours.

   ADR-165 Slice 2 keeps standalone reads to a closed list that "lives at the pool surface"
   (`StandaloneReaderPurpose`, `crates/khive-db/src/pool/reader_counters.rs:16`) and makes a
   new standalone-read call site "a reviewable event". The copy fits none of the three
   members: it is not a raw-SQL read transaction spanning calls, not a boot-time probe, and
   not an operator diagnostic. This amendment is that review and adds a fourth member,
   `DatabaseCopy`, whose doc comment points here. The enum is the list ADR-165 names as the
   authority, so ADR-165's prose, which names the members it started with, is not edited.

   `DatabaseCopy` is classed as infrastructure (`is_infrastructure`, line 28), so its opens
   are counted apart from request traffic (`record_standalone_open`, line 148) and it takes
   no reader-admission slot. A slot held for the length of a copy would shrink the request
   read budget for that whole time, and the copy is not request traffic. Decision 5's
   one-copy-per-database rule bounds how many such connections exist.

   The statement runs with no transaction open on that connection and never inside an
   atomic unit. Before the statement runs, the operation reads back `PRAGMA synchronous` and
   `PRAGMA query_only` on the copy connection, and refuses unless `synchronous` is NORMAL,
   FULL or EXTRA and `query_only` is off. The SQLite page names NORMAL and FULL; EXTRA is the
   level above FULL, and the bundled `sqlite3RunVacuum` gives the output the source's own
   sync level, so EXTRA syncs the output as well.

3. **Destination.** The caller names an absolute path whose parent directory exists. All of
   the following happens before the statement runs, in this order:
   - The operation resolves the parent directory to its canonical path and joins the final
     name. It refuses the result if it equals the source's canonical path or that path with
     `-wal`, `-shm` or `-journal` appended. A sidecar name may not exist yet, so an existence
     check alone does not cover it.
   - It refuses if anything exists at `<dest>`, `<dest>-journal`, `<dest>-wal` or
     `<dest>-shm`, a symbolic link included. SQLite writes the output through a rollback
     journal named `<dest>-journal`, so a journal left by an earlier copy is refused, never
     adopted.
   - It creates `<dest>` exclusively, as an empty file, keeps that handle open, and records
     the file's device and inode.
   - It opens the copy connection with `SQLITE_OPEN_NOFOLLOW`. SQLite attaches the output
     with the connection's own open flags (`attachFunc` passes `db->openFlags`), and with
     that flag it refuses any path element that is a symbolic link
     (`SQLITE_CANTOPEN_SYMLINK`), so a link placed at `<dest>` is not followed. The
     connection opens the source by its fully resolved path, which is the path SQLite itself
     resolves the pool's open to, so the flag refuses only a link that appears after
     resolution, and the copy reads the same `-wal` and `-shm` files as the pool.
   - Immediately before the statement, and again after it, the operation checks that
     `<dest>` still names the device and inode it created. A mismatch fails the copy.
   - The path is bound as a statement parameter (`VACUUM INTO ?1`) and never spliced into
     the SQL text.

   These are separate steps, so the sequence is not atomic. A process that can rename
   entries in the destination directory between them can still substitute a regular file.
   The destination directory belongs to the caller and such a process is outside this
   operation's threat model; the identity check turns a substitution into a reported
   failure, never a silent success, and the removal rule below leaves a substituted file
   alone.

   On success the operation syncs the destination's parent directory (on Unix) and returns
   the size read from the held handle's metadata. On any failure it removes `<dest>` only
   while the path still names the device and inode it created, and removes
   `<dest>-journal` if one is present (none existed when the call began). If a removal
   fails, the returned error names the path. The operation never removes a path it did not
   create.

   A process killed during a copy runs none of this and can leave `<dest>` and
   `<dest>-journal` behind. A later copy to that path is refused by the existence check
   above; only an `Ok` return marks a destination complete, and clearing a refused
   destination is the caller's, which for the scheduled tiers is #5562.

4. **Census.** The copy registers one span in the open-span registry with the source
   database as its origin: `register_scoped(Some(label), TxOrigin::Database(id))`
   (`crates/khive-storage/src/tx_registry.rs:122`), where `id` is the source's
   `DbIdentity` as the pool minted it. The type's own doc names the pool as its only minting
   point, so the span attributes to the same database as the pool's own spans. `label` is a
   fixed constant that no other registration uses and that carries no caller value. The
   span opens after Decision 3's checks and before the copy connection opens, and closes
   after that connection closes. Deregistration is the handle's `Drop`
   (`tx_registry.rs:109`), so success, error, deadline and panic all leave the census.

   The registry is per process. In the process that runs the copy, the checkpoint task's
   holder reports and the transaction-age sweep name the span by its label. Another
   process learns the name only through the WAL-pin heartbeat: when the copying process
   runs a sweep (the daemon's checkpoint task or a session sweep) and the copy's span is
   that process's oldest past the warning age, the heartbeat body carries its label
   (ADR-091 Amendment 2 Plank B, `oldest_tx_label` in `crates/khive-db/src/walpin/types.rs:24`).
   A copying process that runs no sweep appears in the OS-level census as a holder process
   without a name, as any cross-process reader does today.

   The transaction-age sweep will report every copy longer than its thresholds: a warning
   past `KHIVE_TX_WARN_SECS` (30 seconds by default) and an error past
   `KHIVE_TX_MAX_AGE_SECS` (120 seconds by default), both visibility only; nothing in the
   sweep closes a span (`crates/khive-db/src/checkpoint/config.rs:85-108`, defaults at 121-122). This amendment
   does not exempt the copy's label from either rung. The label is what lets a reader tell
   an expected copy from a leaked reader, and an exemption would also hide a copy that
   never ends. Decision 5's deadline is what ends it.

5. **Bound.** The copy holds one read snapshot for its whole length, so checkpoints cannot
   backfill past it (ADR-091) and the WAL grows until it ends. The deadline is required and
   has no default, which keeps Named invariant 2 enforceable. The caller owns the budget:
   each scheduled sync already runs under this ADR's per-sync hard timeout, and a default
   here would be a second, unrelated number a caller could inherit without choosing it.

   On expiry the operation interrupts the statement through the copy connection's own
   interrupt handle (`get_interrupt_handle()`, the same call
   `crates/khive-db/src/read_cancellation.rs:322` makes for request reads), never the pool's
   or the writer's, and without going through the request-read cancellation path or its
   statement classifier. The copy then fails with a deadline error, followed by Decision 3's
   removal and Decision 4's deregistration. Within one process a second copy of the same
   database is refused while one is running, matching "at most one backup reader exists at
   a time" above; the cross-process lock across tiers stays with the scheduled job.

6. **Unchanged.** The operation performs no file operation of its own on the source's
   `-wal` or `-shm` files: it never reads, copies or deletes them (SQLite reads committed
   WAL content through the connection, as for any reader). Named invariant 1 is unchanged.
   Tier scheduling, retention, off-host transfer and the drills are #5562 and this ADR's
   existing sections, not this amendment.

### Acceptance (tests in the implementing pull request)

1. A copy taken while another task keeps committing passes `PRAGMA integrity_check` and
   holds every row committed before the copy started.
2. The writer's commits during the copy succeed: at least one commit lands inside the
   copy's window, with no busy or admission error.
3. The census shows the copy's label, with the source's identity as origin, while the copy
   runs, and not after it ends, both after a success and after a failed copy.
4. An existing `<dest>`, `<dest>-journal`, `<dest>-wal` or `<dest>-shm` is refused before
   any work: no census entry appears, the standalone open count does not move, and the
   existing file is unchanged.
5. Each of the source file, `-wal`, `-shm` and `-journal` names is refused as a
   destination, including a sidecar name that does not exist yet and a destination whose
   parent is reached through a symbolic link.
6. A destination whose name contains a single quote copies correctly, which shows the
   path is bound and not spliced.
7. A deadline that expires during the copy fails it with a deadline error, removes the
   partial file and its journal, leaves no census entry, and the writer keeps committing.
8. A copy connection whose `synchronous` reads below NORMAL, or whose `query_only` is on,
   is refused before the statement runs; NORMAL, FULL and EXTRA each copy.
9. A second copy of the same database started while the first runs is refused, and the
   first completes.
10. An in-memory database refuses the operation.
11. A symbolic link substituted at `<dest>` after the exclusive create and before the
    statement fails the copy; the link's target is unchanged and the link is not removed.
12. A regular file substituted at `<dest>` in the same window fails the copy on the
    identity check, and the substituted file is not removed.
