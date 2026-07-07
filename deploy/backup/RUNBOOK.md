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
tier-3 archive cadence). The standing cadence covers **every store registered
in `stores.conf`**, not just the primary one — a store whose backups are
synced but never drilled is in the exact state invariant 3 forbids. (For
stores on a pre-consolidation migration lineage the runtime cannot boot,
steps 5-6 are a known-partial scope — see
[Known partial-drill scopes](#known-partial-drill-scopes).) Drills are
deliberately operator-run rather than launchd-scheduled in v1: each run
writes a marker to the origin, occupies the ANN rebuild path for hours, and
produces evidence an operator should actually look at.

There are two drill modes (ADR-100 amendment, 2026-07-07): the **routine**
drill, which captures its validation manifest from the freshly-synced
replica, and the **origin-exact** drill, which captures immediately before
the sync, straight from the origin. Use routine for the standing cadence on
a live store; origin-exact only in a genuine maintenance window (see
[Migration pause rule](#migration-pause-rule)).

### Routine drill (replica-capture — use this on a live store)

Live evidence on the motivating production store showed the origin-exact
ordering below is not reliably satisfiable there: a client-write table takes
a row roughly every 10 seconds, against a roughly 60-second capture-to-sync
window, so every controlled origin-capture attempt failed on exactly the
writes landing in that window. The routine drill sidesteps this by capturing
from the replica, which is a fixed, closed point the instant the sync
completes, rather than from the origin, which a live multi-writer store
never holds still.

1. Write a marker row through the **normal write path** (not through this
   tooling), to the **origin** — e.g. a note or memory entry with a fresh,
   unique id. Record that id.
2. Run (or wait for) the designated sync, so the marker reaches the replica:

   ```bash
   deploy/backup/khive-backup.sh t1 khive
   ```

3. **Capture** the manifest from the freshly-synced replica:

   ```bash
   deploy/backup/restore-drill.sh capture-replica khive <marker-id>
   ```

   This verifies the marker is present **in the replica** — the round-trip
   proof that the sync actually carried the origin's state forward, without
   which a replica-captured manifest compared against a replica-restored
   copy would prove only "the replica equals itself" — then captures a
   manifest in one read transaction (table row counts, `MAX(rowid)`, and a
   `.sha3sum` checksum per table) to
   `~/.khive/backups/drill/manifests/khive-<UTC stamp>.manifest`, printing
   the path as `MANIFEST_PATH=...`. Pass an explicit path as a third
   argument to control the location instead.
4. **Validate** the resulting backup against the manifest captured in
   step 3 (same `validate` subcommand as the origin-exact drill — see
   below).

### Origin-exact drill (maintenance-window only)

Valid only when the origin is quiescent for the whole capture-to-sync
window — a real maintenance window, or a store with no live writers. On a
live multi-writer store this mode will fail spuriously on in-window writes;
see the amendment above. Bind this mode to the schema-migration re-drill
cadence in [Migration pause rule](#migration-pause-rule): the first genuine
maintenance window runs it once and records the result; it is not the
standing cadence.

1. Write a marker row through the normal write path, to the origin. Record
   that id.
2. **Capture** the manifest right before running the sync you intend to
   validate:

   ```bash
   deploy/backup/restore-drill.sh capture khive <marker-id>
   ```

   This verifies the marker is present in the origin, then captures the
   manifest the same way as `capture-replica` above, but reading the origin
   directly.
3. Run (or wait for) the sync being validated.
4. **Validate** the resulting backup against the manifest captured in
   step 2 (see below).

### Validate (shared by both modes)

```bash
deploy/backup/restore-drill.sh validate khive /path/to/tier2-replica.db <marker-id> <manifest-path>
```

This restores the given backup to a scratch path, runs
`PRAGMA integrity_check`, compares the restored copy's manifest to the
**recorded** manifest file **exactly** (no tolerance), boots a runtime
against the restored copy and serves `stats()` / `search()` /
`memory.recall()`, rebuilds the ANN index from the restored database and
serves one vector query, and prints the RTO (wall time of the restore +
verification steps). `validate` refuses outright — before touching
anything else — if the manifest file is missing or malformed, rather than
silently treating that as "nothing to compare".

- **Steps 5-6 (inside `validate`) require `kkernel` on PATH.** If it is
  absent, the drill prints `skipped (kkernel not on PATH)` for those two
  steps and still reports overall success on the rest. A drill that only
  ran with `kkernel` unavailable is **not** a complete acceptance run per
  the ADR — re-run it where `kkernel` is reachable (the actual recovery
  host is the right place) before treating the backup as drill-verified.
- **Steps 5-6 run `kkernel` under an isolated `HOME` inside the scratch
  dir.** This is a safety property, not just a compatibility fix: config
  discovery finds nothing under the isolated `HOME`, so the `KHIVE_DB`
  override is accepted even on hosts whose real config declares
  `[[backends]]` (which otherwise refuses the override), and the drill is
  hermetic by construction — the drill runtime provably cannot discover or
  touch the host's real stores. Every verb it serves comes from the
  restored copy and nothing else.
- A failed comparison prints the manifest diff and fails loudly — that is a
  defect in the backup or restore path, not "just a delta since the
  marker", because the manifest and the marker were captured atomically at
  the recorded point (replica-fixed for the routine drill, pre-sync origin
  for the origin-exact drill).
- **Cleanup on exit**: when `validate` is run with no explicit
  `scratch-dir` argument (the normal case), it removes its own scratch dir
  on both success and failure — the restored database copy is multi-GB and
  disposable. On failure, the small evidence (`manifest.diff`, the restored
  copy's manifest, and the `step5-*.json` / `step6-*.json` runtime-boot
  outputs) is copied first to
  `~/.khive/backups/drill/failed-<store>-<UTC stamp>/` before the scratch
  dir is removed, so the failure is diagnosable without keeping the full
  restored copy around. Passing an explicit `scratch-dir` argument opts out
  of this cleanup entirely — that directory is left for the caller to
  manage, on both success and failure.

### Known partial-drill scopes

A registered store on an incompatible schema lineage — one whose migration
history predates the current consolidated baseline — cannot run the
runtime-boot steps of the drill (steps 5-6 above: booting a runtime and
rebuilding the ANN index) against its restored copy. Such a store is
covered by `PRAGMA integrity_check` and manifest equality only; the
runtime-boot steps are out of scope for it until it is migrated to the
current baseline or explicitly exempted. Track each such store as its own
issue — do not fold it into a shared "some stores can't run steps 5-6"
note, since resolution (migrate vs. exempt) is a per-store decision.

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
- A quiescent WAL-mode database whose `-shm` sidecar is absent (common after
  a sync visits a store no live process holds open) refuses read-only opens
  with SQLite error 14: recovering the WAL index requires write access to
  recreate `-shm`. Options: open the file once briefly in read-write mode
  (any writer, e.g. `sqlite3 <db> "SELECT 1;"`), or read it without recovery
  via `sqlite3 "file:<db>?immutable=1"` — the latter only when nothing can
  be writing. The database file itself is intact; do not treat error 14 on
  such a store as corruption.

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
