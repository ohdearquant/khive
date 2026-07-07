# ADR-100 backup operator runbook

This runbook covers the operator procedures for the store backup and
replication tooling in this directory. It implements ADR-100
(`docs/adr/ADR-100-store-backup-replication.md`) — read that document first;
this runbook assumes its terminology (tiers, invariants, manifest, the
measurement gate) without re-deriving it.

Contents:

- [Prerequisites](#prerequisites)
- [Store registry (`stores.conf`)](#store-registry-storesconf)
- [Tier-1 measurement gate (do this first)](#tier-1-measurement-gate-do-this-first)
- [Tier-2 off-host setup](#tier-2-off-host-setup)
- [Installing the scheduled jobs](#installing-the-scheduled-jobs)
- [Restore drill procedure](#restore-drill-procedure)
- [Migration pause rule](#migration-pause-rule)
- [Sidecar handling](#sidecar-handling)
- [Off-host protection](#off-host-protection)
- [Disk preflight](#disk-preflight)
- [Failure handling and alerting](#failure-handling-and-alerting)

## Prerequisites

- macOS with `launchd` (this tooling targets the host scheduler; no daemon
  or agent runtime dependency, per the ADR).
- `sqlite3_rsync` (Homebrew package `sqlite-rsync`) **>= 3.50.1** on this
  host. Install: `brew install sqlite-rsync`.
- `sqlite3` (ships with macOS; any reasonably current build is fine — it is
  only used for `VACUUM INTO`, `PRAGMA integrity_check` / `quick_check`,
  and `.sha3sum` in the restore drill).
- `ssh` reachability to the tier-2 host, with a working non-interactive
  (key-based) login for the account the backup job runs as.
- Optional: `kkernel` on PATH for the alerting fallback (`comm.send`) and
  for restore-drill steps 5-6 (booting a runtime + ANN rebuild against the
  restored copy). Both degrade gracefully to a documented fallback if
  `kkernel` is absent — see [Failure handling and alerting](#failure-handling-and-alerting)
  and [Restore drill procedure](#restore-drill-procedure).

## Store registry (`stores.conf`)

`stores.conf` (next to the scripts, or wherever `KHIVE_BACKUP_CONF` points)
registers every backed-up database, one pipe-delimited row per store:

```
name|origin|t1_replica|t2_remote|t3_archive_dir
```

The shipped default registers all three standing khive stores (`khive`,
`sessions`, `khive-graph`) per the ADR's scope section. `t2_remote` ships as
`CHANGE_ME@backup-host:/path` — every tier-2 and tier-3-off-host operation
for a row refuses loudly until that placeholder is replaced with a real
`user@host:path`.

## Tier-1 measurement gate (do this first)

The ADR's 15-minute tier-1 cadence is not trusted blindly — it is gated on
measuring actual sync cost against your production database. Do not install
the tier-1 `launchd` job until this gate has been run and passes.

1. Run a manual tier-1 sync for the store you're gating, several times in a
   row, at the interval you intend to schedule it:

   ```bash
   deploy/backup/khive-backup.sh t1 khive
   ```

2. Read the recorded durations and WAL deltas from the JSONL event log:

   ```bash
   tail -n 20 ~/.khive/backups/log/backup-events.jsonl | \
     python3 -c 'import sys,json; [print(json.loads(l)["duration_s"], json.loads(l)["wal_before_bytes"], json.loads(l)["wal_after_bytes"]) for l in sys.stdin]'
   ```

3. The gate passes if sync duration is trivial (seconds, not tens of
   seconds) relative to the 15-minute cadence, and WAL growth attributable
   to the backup reader stays within the budget agreed with the ADR-091
   checkpoint escalation thresholds (see that ADR for the numbers currently
   in force). If it does not pass, widen `KHIVE_BACKUP_INTERVAL_T1` (or the
   installer's `--interval` flag) to a cadence where it does, and record the
   widened value — the ADR requires amending its RPO table when this
   happens.
4. Only after the gate passes: install the tier-1 `launchd` job (see
   [Installing the scheduled jobs](#installing-the-scheduled-jobs)).

## Tier-2 off-host setup

Tier-2 requires the second host to have `sqlite3_rsync` installed at the
same version floor as the primary host, and `khive-backup.sh` preflights
both ends before every sync — a missing binary or a version mismatch is a
refusal, not a degraded sync. Set this up **before** editing `stores.conf`'s
`t2_remote` field away from its `CHANGE_ME` placeholder:

1. **Install `sqlite-rsync` on the remote host** (do this first — the
   preflight below will otherwise refuse):

   ```bash
   # on the remote (off-host) machine, over SSH:
   ssh <user>@<host> 'brew install sqlite-rsync'
   ```

   Confirm the installed version meets the floor:

   ```bash
   ssh <user>@<host> 'sqlite3_rsync --version'
   ```

   `khive-backup.sh` does not trust `--version`'s literal output shape (see
   the "parse version robustly" note in `lib.sh`'s
   `local_sqlite3_rsync_version` / `remote_sqlite3_rsync_version`) — some
   builds print only a source-id (date + hash) with no dotted version at
   all, in which case the tool falls back to the version string embedded in
   the binary itself. Either way the effective floor enforced is **>=
   3.50.1** on both ends.

2. **Verify SSH reachability and key-based, non-interactive login** for the
   account the backup job runs as:

   ```bash
   ssh -o BatchMode=yes <user>@<host> true
   ```

   `BatchMode=yes` is what the runner itself uses — if this hangs or
   prompts for a password/passphrase, fix the key setup before proceeding;
   a sync that blocks on an interactive prompt is exactly the wedged-sync
   pathology the ADR's timeout exists to bound, and the timeout will simply
   kill it, alert, and retry next cycle without diagnosing why.

3. **Pin the remote host key** (off-host protection requirement — see
   below) rather than relying on `StrictHostKeyChecking=accept-new` or
   disabling host key checking.

4. Edit `stores.conf`'s `t2_remote` field for the store to the real
   `user@host:path`. Confirm the preflight now passes with both ends
   version-checked:

   ```bash
   deploy/backup/khive-backup.sh t2 khive
   ```

   A refusal here (nonzero exit, a `version-preflight-failed` or
   `not-configured` row in `backup-events.jsonl`) means either the remote
   binary is still missing/below-floor, or the placeholder was not fully
   replaced — re-check step 1 and the exact `stores.conf` row before
   retrying.

## Installing the scheduled jobs

```bash
# one (tier, store) pair
deploy/backup/install-backup.sh install t1 khive
deploy/backup/install-backup.sh install t2 khive
deploy/backup/install-backup.sh install t3 khive

# or all three tiers for every registered store at once
deploy/backup/install-backup.sh install-all
```

Reruns are idempotent: an omitted `--interval` is read back from the
currently-installed plist rather than reset to the template default, so
upgrading the script path or log directory does not require re-typing an
operator-tuned interval. `install-backup.sh status [<tier> <store>]` and
`install-backup.sh uninstall <tier> <store>` manage the installed jobs;
`status` with no arguments reports every registered store's three tiers.

## Restore drill procedure

Per the ADR's invariant 3 ("a backup that has not been restored is not a
backup"), run this after initial setup, after any schema-migration release,
and on a standing cadence you define (weekly is a reasonable floor given the
tier-3 archive cadence).

1. Write a marker row through the **normal write path** (not through this
   tooling) — e.g. a note or memory entry with a fresh, unique id. Record
   that id.
2. Run (or wait for) the sync you intend to validate.
3. Run the drill against the resulting replica:

   ```bash
   deploy/backup/restore-drill.sh khive /path/to/tier2-replica.db <marker-id>
   ```

   This executes the ADR's 7 steps: capture an origin manifest in one read
   transaction (table row counts, `MAX(rowid)`, and a `.sha3sum` checksum
   per table), restore the given backup to a scratch path, run
   `PRAGMA integrity_check`, compare the restored copy's manifest to the
   origin's **exactly** (no tolerance — the manifest was captured at the
   marker-write instant), boot a runtime against the restored copy and
   serve `stats()` / `search()` / `memory.recall()`, rebuild the ANN index
   from the restored database and serve one vector query, and print the RTO
   (wall time of the restore + verification steps).
4. **Steps 5-6 require `kkernel` on PATH.** If it is absent, the drill
   prints `skipped (kkernel not on PATH)` for those two steps and still
   reports overall success on steps 1-4 and 7. A drill that only ran with
   `kkernel` unavailable is **not** a complete acceptance run per the ADR —
   re-run it where `kkernel` is reachable (the actual recovery host is the
   right place) before treating the backup as drill-verified.
5. A failed comparison prints the manifest diff and fails loudly — that is
   a defect in the backup or restore path, not "just a delta since the
   marker", because the manifest and the marker were captured atomically.

## Migration pause rule

Around any schema-migration release: pause the scheduled backup jobs for
the affected store(s) (`install-backup.sh uninstall <tier> <store>`, or
simply `launchctl bootout gui/$(id -u)/com.khive.backup.<tier>.<store>`
temporarily). After the migration lands, run a tier-2 sync manually and a
full restore drill before re-installing the jobs and considering the lane
healthy again.

## Sidecar handling

Restore is the one moment operators touch raw files directly. `-wal`/`-shm`
sidecars travel with their database:

- `khive-backup.sh` moves `<replica>-wal` / `<replica>-shm` alongside the
  promoted replica file (or removes stale ones on the destination if the
  new sync produced none) as part of the same atomic promote step.
- `restore-drill.sh` copies `<backup>-wal` / `<backup>-shm` to the scratch
  path alongside the main file, if present, before running
  `PRAGMA integrity_check`.
- Never copy a `-wal`/`-shm` sidecar independently of its database file, and
  never delete a `-wal` file directly — this is invariant 1 from the ADR
  and applies identically during manual restores an operator performs
  outside these scripts.

## Off-host protection

Replicas and archives on the tier-2/tier-3 off-host target contain
messages, memories, and tasks. At minimum:

- **Restrictive file modes**: the remote replica/archive directory and its
  contents should not be group/world readable (`chmod 700` the directory,
  `umask 077` for the account the sync runs as).
- **Pinned SSH host keys**: add the remote host's key to a dedicated
  `known_hosts` entry (or the account's default `known_hosts`) ahead of
  time via `ssh-keyscan`, rather than accepting-on-first-use during the
  first scheduled sync. `khive-backup.sh` does not disable host key
  checking.
- **Encryption at rest** on the off-host target: put the backup directory
  on an encrypted volume (FileVault on macOS, LUKS on Linux, or an
  encrypted disk image mounted for the duration of each sync) — this
  tooling does not itself encrypt the replica/archive files.

## Disk preflight

Every sync and archive run checks free space on its destination before
touching anything: `origin database size + KHIVE_BACKUP_MARGIN_BYTES`
(default 100 MiB) must fit in the destination's available space (`df -Pk`),
or the run refuses and logs a `disk-preflight-failed` row — never a silent
skip. Raise `KHIVE_BACKUP_MARGIN_BYTES` if the origin's WAL or the tool's
own temporary files historically need more headroom than the default.

## Failure handling and alerting

Every sync (success or failure) appends one JSONL row to
`~/.khive/backups/log/backup-events.jsonl` (override the root with
`KHIVE_BACKUP_ROOT`): timestamp, store, tier, outcome, duration, bytes
transferred/archived, and WAL size before/after. `outcome` is one of
`success`, `version-preflight-failed`, `disk-preflight-failed`,
`not-configured` (tier-2/3-off-host placeholder not yet edited), `timeout`,
`sync-failed`, `integrity-check-failed`, or `promote-failed`.

On any non-success outcome, the runner attempts
`kkernel exec 'comm.send(to="...", content="...")'` to alert the operating
seat responsible for the deployment. If `kkernel` is unavailable or that
call fails, the JSONL row plus this process's non-zero exit (which
`launchd` records as the job's last exit status) is the fallback detection
path — check `launchctl print gui/$(id -u)/com.khive.backup.<tier>.<store>`
or the per-job stdout/stderr logs under `~/Library/Logs/khive-backup/` if a
job has gone quiet. No alert message ever includes database contents,
credentials, or SSH details — only the store name, tier, and outcome.
