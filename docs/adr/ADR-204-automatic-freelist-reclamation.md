# ADR-204: Automatic freelist reclamation with incremental auto-vacuum

**Status**: Proposed\
**Date**: 2026-10-16\
**Revised**: 2026-10-24 (last-slice clamping, backfill gate as pacing, backup consequence, page-work bound)\
**Issue**: #5526\
**Relates to**: [ADR-091](ADR-091-wal-snapshot-lifetime.md) (checkpoint task; Amendment 16, bounded FTS5 segment
maintenance), [ADR-117b](ADR-117b-session-deletion-retention.md) D4 (names this branch and its prerequisites),
[ADR-100](ADR-100-store-backup-replication.md) (backup copies whole files),
[ADR-168](ADR-168-event-retention-classes.md), [ADR-170](ADR-170-events-daemon-split.md)\
**Grounding commit**: `d9a70620f139b8eb73bb34c6ae620297f3bb010d`

## Decision summary

The main store switches to `auto_vacuum=INCREMENTAL`. The daemon's checkpoint task releases free pages in bounded
slices of at most 1,024 pages (4 MiB) with `PRAGMA incremental_vacuum(N)`, at most one slice per checkpoint tick, when
the freelist reaches 5% of the file and at least 4,096 pages. It stops when the freelist is at or below its floor,
the larger of 1% of the file and 1,024 pages; the last slice is clamped to the pages above the floor, and N is never
zero or negative. Every online bulk delete or table move arms the same drain at once. No scheduled full
`VACUUM` runs on a live store. The hourly freelist alarm stays as the backstop. Existing files convert once, with
one `VACUUM` in an announced maintenance window. The events and knowledge stores stay `NONE` until their write
pattern changes, for the reasons and costs given below.

## Context

### What happened

A long-running production main store reached 5,758,840,832 bytes with 1,405,967 pages of 4 KiB, of which 629,185
(44.75%) were on the freelist. One full `VACUUM` through `memory.vacuum` took 27 s. Writers were blocked for that
time. The WAL peaked at 3,057,901,112 bytes, about the size of the live data. The file shrank to 3,040,239,616
bytes (742,246 pages, freelist 0), and no rows were removed. The hourly whole-file backup copy of that store
dropped from 51-58 s to 27 s per run, because ADR-100's tier-1 copy reads the whole file.

The freelist was not growing. The backup event log records the file size on every successful replica run. It shows
the file at exactly 5,758,840,832 bytes on every run from 2026-09-03 to 2026-10-13. Over the same period the weekly
`VACUUM INTO` archives, which contain only live pages, grew from 2,077,851,648 bytes (2026-09-07) to
2,872,160,256 bytes (2026-10-03). Normal writes were slowly reusing free pages. At that rate, reuse would have
taken months to absorb the freelist, and every backup run in between would have copied the free pages.

The free pages were created in a window that contained two bulk operations on the main file and nothing else of
that size: moving two tables' worth of rows out of the main store into separate stores, followed the same night by a
full re-embedding pass over every entity and note. Either one frees pages faster than later writes can reuse them.
Which of the two produced this freelist is not established, because no file-size reading exists between them.
The events-sidecar cutover of ADR-170 did not cause it, because that cutover never deletes rows from the main
store's `events` table.

### Why this keeps happening

Every store file is `auto_vacuum=NONE` today. In that mode SQLite never returns free pages to the file system. The
only way to shrink a file is a full `VACUUM`, which rewrites every live page, holds the write lock for the whole
rewrite, writes a WAL about as large as the live data, and needs that much free disk. Any bulk delete, table move,
reindex, retention purge or index rebuild therefore leaves a freelist that stays until someone schedules a full
`VACUUM` by hand.

ADR-117b D4 already names the alternative and what choosing it requires: `incremental_vacuum` does nothing on a
`NONE` file; the mode must be set before tables exist or applied by one full `VACUUM`; the maintenance capability
sits outside today's closed, parameter-free set; existing files need a conversion plan; fresh and converted files
need tests for the pragma; writer time and WAL effects must be budgeted; and the branch cannot claim to repack
partly filled pages. This record is that separate contract. Section 1 and acceptance 1 give the initialization
rule for new files, sections 2 and 5 the capability, section 5 the conversion plan, acceptance 1 and 5 the tests,
section 3 the budget, and the consequences state what is given up.

## Measurements

All numbers below were measured on a 16-core Linux host with the SQLite that khive bundles (3.53.2, through
`rusqlite` 0.40.1 and `libsqlite3-sys` 0.38, built with khive's `LIBSQLITE3_FLAGS`). The connection settings were
khive's pool defaults: WAL, `synchronous=NORMAL`, `cache_size=-8192`, `mmap_size=1 GiB`, `temp_store=MEMORY`, 4 KiB
pages. The data was synthetic: 350,000 rows of 0.5-4 KB text with a random 36-character unique key, three secondary
indexes, a content-bearing FTS5 table kept in sync by triggers, and 175,000 append-only rows of 0.2-0.6 KB with two
indexes. That gave a 2.75 GB file. Deleting 45% of the rows and running one FTS5 `optimize` left 270,235 of
671,335 pages free (40.3%). The host carried unrelated work; its 1-minute load average was 6 to 19 during the runs, so
the absolute times are pessimistic.

### Cost of releasing pages

`PRAGMA incremental_vacuum(N)` with an empty WAL before each slice (the WAL header write and file growth are inside
the time):

| N pages | Slice wall time (3 runs) | µs per page              | WAL frames per released page | PASSIVE checkpoint after |
| ------- | ------------------------ | ------------------------ | ---------------------------- | ------------------------ |
| 256     | 819.6, 18.3, 16.6 ms     | 3,202 (cold), 71.5, 64.6 | 0.65-0.83                    | 23-1,206 ms              |
| 1,024   | 400.2, 66.2, 28.0 ms     | 391 (cold), 64.6, 27.3   | 0.56-0.83                    | 40-816 ms                |
| 4,096   | 367.1, 293.4, 311.1 ms   | 89.6, 71.6, 75.9         | 0.93-0.98                    | 211-392 ms               |
| 16,384  | 887.5, 1,009.6, 825.2 ms | 54.2, 61.6, 50.4         | 1.02                         | 838-1,078 ms             |
| 65,536  | 3,528.3 ms (1 run)       | 53.8                     | 1.00                         | 3,181 ms                 |

The same slices with a concurrent writer (one 0.4 KB row per transaction every 10 ms), a concurrent reader (a point
lookup or an FTS5 query every 5 ms) and a PASSIVE checkpoint every 500 ms, as the daemon runs it:

| N pages                     | Slice wall time        | µs per page | Writer latency overlapping a slice | Slowest read overlapping a slice     |
| --------------------------- | ---------------------- | ----------- | ---------------------------------- | ------------------------------------ |
| none (8 s baseline, 3 runs) |                        |             | p50 114-130 µs, p99 188-272 µs     | p99 180-241 µs; max 194 µs to 8.5 ms |
| 256                         | 8.0, 6.7, 9.4 ms       | 26-37       | 8.6 ms (1 write)                   | 119 µs                               |
| 1,024                       | 32.4, 28.2, 24.4 ms    | 24-32       | p50 33.5 ms (3 writes)             | 78 µs                                |
| 4,096                       | 159.1, 104.6, 114.4 ms | 26-39       | max 179 ms (4 writes)              | 317 µs                               |
| 16,384                      | 581.4, 457.3, 436.4 ms | 27-36       | max 629 ms (3 writes)              | 254 µs                               |
| 65,536                      | 2,935.4 ms             | 44.8        | 2,940 ms (1 write)                 | 168 µs                               |

Findings:

1. Releasing a page costs about 25-40 µs of write-lock time when the WAL is warm, and 50-90 µs when every slice
   starts on an emptied WAL. A writer waits for the whole slice. Readers are not delayed.
2. Each released page adds about one WAL frame (0.93-1.02 frames per page from 4,096 pages up; fewer at small N).
   The main file shrinks at the first checkpoint that backfills the slice; a TRUNCATE checkpoint is not needed.
   After the 65,536-page slice, the PASSIVE checkpoint alone took the file from 2,482,069,504 to 2,213,306,368 bytes.
3. Under continuous reads the WAL file was not reset between slices. It reached 288,799,672 bytes over twelve slices
   totalling 65,280 pages, and 283,554,912 bytes during one 65,536-page slice. Slices therefore have to be paced by
   the checkpoint, not run back to back.
4. `incremental_vacuum` with no argument, zero or a negative value means "the whole freelist". On the production
   freelist above that is 629,185 pages, or 16-57 s of write-lock time at the rates measured here, which is the same
   order as the full `VACUUM` it replaces. An unbounded call is never correct on a live store.
5. The pragma returns one result row per released page and releases one page per step. A helper that steps a
   statement once releases one page per call. Measured: `rusqlite`'s `execute_batch("PRAGMA incremental_vacuum(4096)")`
   released exactly 1 page; stepping the same statement to `SQLITE_DONE` released 4,096. khive's existing top-level
   maintenance path runs `VACUUM` through `execute_batch`, so it cannot be reused unchanged for this pragma.
6. `PRAGMA auto_vacuum` must be issued before page 1 is written. Switching to WAL writes page 1. A fresh file that set
   `journal_mode=WAL` first and `auto_vacuum=INCREMENTAL` second stayed `NONE` (read back as 0).

### Cost of running in INCREMENTAL mode

Interleaved runs on identical 70,000-row databases (same seed, load average 6-13):

| Measure                                 | INCREMENTAL      | NONE             | Difference               |
| --------------------------------------- | ---------------- | ---------------- | ------------------------ |
| Bulk load, 1,000-row transactions, wall | 31.98 s, 28.10 s | 25.90 s, 24.74 s | +24%, +14% (paired runs) |
| Bulk load, user CPU                     | 14.16 s, 14.29 s | 13.48 s, 13.53 s | +5%                      |
| Single-row insert transaction, p50      | 169 µs, 162 µs   | 155 µs, 155 µs   | +5% to +9%               |
| Single-row delete transaction, p50      | 154 µs, 164 µs   | 148 µs, 154 µs   | +4% to +6%               |
| File size                               | 134,546 pages    | 134,381 pages    | +0.12%                   |

The size difference is the pointer-map: one page per 820 pages (also 671,335 against 670,516 pages on the 2.75 GB
pair). Deleted pages go to the freelist exactly as in `NONE` mode (2,492 free pages after 2,000 deletes in both).

### Cost of the one-time conversion and of a full VACUUM

| Operation (2.75 GB file)                              | VACUUM wall | WAL peak        | TRUNCATE checkpoint after |
| ----------------------------------------------------- | ----------- | --------------- | ------------------------- |
| `PRAGMA auto_vacuum=INCREMENTAL; VACUUM;`, freelist 0 | 30.1 s      | 2,758,372,992 B | 30.4 s                    |
| Plain `VACUUM`, 40.3% freelist                        | 18.2 s      | 1,522,756,152 B | 23.9 s                    |

The conversion read back `auto_vacuum=2` in WAL mode, so it works without leaving WAL. Its cost is that of one full
`VACUUM` of the live data. On the production main store that `VACUUM` took 27 s.

### Ordinary churn after the VACUUM

About five hours after the production `VACUUM` the main store reported 4,588 free pages of 752,620 (0.61%). Three
days later it reported 2,672 free pages of 776,864 (0.34%). That is the freelist that FTS5 merges and page splits
create and that later writes reuse on their own; the thresholds in section 3 sit above it.

## Decision

### 1. Mode per store

- **Main store: INCREMENTAL.** It takes bulk deletes and rewrites (namespace moves, store moves, reindex passes, the
  hard deletion of soft-deleted records), and its freelist is what the whole-file backup copies.
- **Events store: stays NONE for now.** Its writers only append; the event store issues no `DELETE`. Its file had a
  freelist of 0 of 893,777 pages when read. ADR-168 retention will introduce bulk deletes. The retention
  implementation converts this store as part of its own rollout, so it does not pay +5% CPU on every append before
  anything is deleted. A newly created events file is created INCREMENTAL, at no cost.
- **Knowledge store: stays NONE for now.** It is written by corpus ingest and occasional single-row hard deletes, and
  its freelist was 815 of 2,745,162 pages (0.03%). Converting its 11.24 GB file would hold its writer for an estimated
  2 minutes and need about 11 GB of WAL headroom. That estimate is a linear extrapolation of the 30.1 s measured for
  2.75 GB and is not measured. A future reindex pass over this store is a trigger for revisiting the choice; until
  then the alarm in section 6 covers it.
- **Any newly created store file** sets `auto_vacuum` before its first `journal_mode=WAL`, per finding 6. This is a
  versioned initialization rule of the store layer, not a per-pack setting.

### 2. Where slices run

The drain runs in the existing checkpoint task (`crates/khive-db/src/checkpoint/task_loop.rs`,
`run_checkpoint_task`), on its dedicated standalone connection, as a third cadence beside the checkpoint and the
bounded FTS5 maintenance of ADR-091 Amendment 16 (`crates/khive-db/src/fts_maintenance.rs`). It follows that
precedent exactly:

- It runs off the Tokio worker in `spawn_blocking`, like `run_fts_maintenance_off_worker`.
- It sets the connection's `busy_timeout` to zero for the slice and restores it afterwards. If an application writer
  holds the lock, the slice reports busy and waits for the next tick. It never queues behind a writer.
- It never enters the pooled writer or the writer task's queue. While the slice runs it holds SQLite's write lock,
  so an application writer waits for at most one slice.
- It steps the statement to `SQLITE_DONE` and counts the rows it stepped, per finding 5. The page count is a bound
  integer formatted from a typed value; there is no raw SQL path.

The writer task and the closed top-level maintenance set are not used for the drain. That set is parameter-free by
design, and its `execute_batch` path would release one page per call.

The drain runs on every backend's checkpoint task whose file reports `auto_vacuum=2`. It assumes no table, so it is
safe on secondary stores once they convert.

### 3. Threshold, slice size and pacing

- **Check:** every 60 s, read `PRAGMA freelist_count` and `PRAGMA page_count` on the checkpoint connection. Both are
  header reads. The check also runs at once when a bulk path arms it (section 4).
- **Start:** freelist at least 5% of `page_count` and at least 4,096 pages (16 MiB).
- **Slice:** `PRAGMA incremental_vacuum(N)` with `N = min(1024, freelist_count - floor)`, so at most 4 MiB per slice.
  The drain issues a slice only when `N` is positive. A zero or negative `N` would release the whole freelist in one
  call (finding 4), so it is never passed.
- **Pace:** at most one slice per checkpoint tick (500 ms by default), and only on a tick whose PASSIVE observation
  showed the WAL fully backfilled (checkpointed frames equal log frames). The checkpoint outcome does not carry that
  comparison today (`CheckpointCoreOutcome` holds the WAL page count, an unavailable reason and a sidecar
  attribution), so the implementation adds it. The tick runs the bounded FTS5 segment maintenance after the checkpoint
  core, so FTS writes can follow the observation before the slice runs.
- **Stop:** freelist at or below the floor, which is the larger of 1% of `page_count` and 1,024 pages. The clamped
  last slice never takes the freelist below the floor.

Why these numbers:

- **1,024 pages.** Under concurrent load a 1,024-page slice held the lock for 24-32 ms, and 28-66 ms on an emptied
  WAL once warm. It wrote 569-852 WAL frames, which stays under the checkpoint task's 2,000-page WAL warning.
  4,096-page slices held writers for up to 179 ms, wrote 3,803-4,024 frames, and would trip that warning on every
  slice. 256-page slices cost more per page (0.65-0.83 frames and a fixed per-transaction cost) and drain four times
  slower.
- **One slice per tick.** That keeps the write-lock duty cycle while draining at about 5-13% (24-66 ms in each
  500 ms). It drains about 2,048 pages per second. The 629,185-page production freelist would take about 5 minutes,
  and its total lock time of about 15-28 s (at the 24-45 µs per page measured with a warm WAL) is spread over 615
  slices of tens of milliseconds, instead of one 27 s hold.
- **Backfill gate.** Finding 3 shows that slices run faster than checkpoints can absorb under continuous reads. If the
  slices are not paced, the WAL grows by about 4 KiB for every released page. The gate is pacing, not a ceiling on
  WAL size: it reads an observation that FTS maintenance in the same tick can follow with its own writes.
- **5% to start.** At the measured cost, draining 5% of the current main store (about 38,000 pages) takes about 1-2 s
  of lock time in total. So the threshold does not need to be high to keep the cost low; it only needs to sit above
  the freelist that ordinary writes create and reuse on their own, which read 0.61% and 0.34% above.
- **1% to stop.** Stopping above the churn level avoids releasing pages that the next merge would allocate again.
  Releasing them would only turn each such page into one extra WAL frame and one extra file extension.

All five values are configuration with these defaults, read like the FTS5 maintenance settings. The drain can be
disabled the same way. Invalid values fall back to the defaults with a warning.

### 4. Trigger points

- **Periodic:** the 60 s check above.
- **After an online bulk delete or table move:** paths that remove many rows from a live store arm the check at once,
  so the drain starts without waiting for the next periodic check. At the grounding commit these are namespace
  moves (`crates/khive-db/src/namespace_move.rs`, which deletes moved rows from the source tables) and the hard
  deletion of soft-deleted records; future event retention under ADR-168 and session aging under ADR-117b join them
  when they land. They do not call `incremental_vacuum` themselves; the release still happens in paced slices.
- **Soft deletes arm nothing.** Memory pruning and record deletion mark rows deleted and clean their edges; no page
  is freed until the rows are hard-deleted, and the hard delete is the arming event.
- **After an offline bulk operation:** a tool that holds the store alone, with the daemon stopped, such as a reindex
  pass or a store split, may run the drain to completion before it exits, because no writer or reader is waiting.
  It still steps in 1,024-page slices and checkpoints between them, so its WAL stays bounded.
- **Migrations do not drain at boot.** A migration that frees pages leaves them for the daemon's drain, so daemon
  readiness does not wait on reclamation.

### 5. One-time conversion of existing files

Existing files are `NONE`, and the mode changes only through one full `VACUUM`:

- The conversion is an explicit maintenance operation. The closed top-level maintenance set gains one reviewed,
  parameter-free statement pair, `PRAGMA auto_vacuum=INCREMENTAL; VACUUM;`, beside the existing `VACUUM;`. The mode
  is fixed by the variant and never formatted from input. `memory.vacuum` gains an `auto_vacuum` option, with the
  value `incremental` or `none`, that selects the variant and reports the mode, `page_count` and `freelist_count`
  before and after. Today `memory.vacuum` takes no parameter, so the option is additive.
- It runs in an announced maintenance window, such as the next binary swap, because it holds writers for the length
  of a full `VACUUM`: 27 s on the production main store and 30.1 s for the 2.75 GB benchmark file. It needs free disk
  of at least the live data size for the WAL. `vacuum_capacity::estimate_vacuum_headroom` already estimates the
  need before starting, as up to twice the main file plus WAL size.
- The checkpoint task backfills the post-`VACUUM` WAL on its own; the production run took 22 s.
- The daemon never converts a file on its own at boot. A store that is still `NONE` after the window is reported by
  diagnostics (section 6) and is not converted silently.

### 6. Backstop alarm and diagnostics

- The existing hourly alarm stays. It reads `page_accounting` from
  `db_diagnostics(backend=<name>, size_composition=false)`, which is three pragma reads per store, and fires when
  `freelist_pages / page_count` exceeds 20%. With the drain active, crossing 20% means the drain is disabled,
  starved by a continuously busy writer, gated by WAL pressure, or running on a store that is still `NONE`.
- `page_accounting` reports `freelist_pages`, `page_count` and `page_size_bytes` today. It adds the file's
  `auto_vacuum` mode (one more pragma read). The alarm then reports a store still `NONE` after its conversion window
  as a separate condition.
- `db_diagnostics` adds the drain's process counters beside the FTS5 maintenance counters: checks, slices, pages
  released, busy skips, backfill-gate skips and errors. These let an operator see the drain working without reading
  logs.

## Alternatives

| Option                                                         | Cost, measured or sourced                                                                                                                                                                                 | Why not                                                                                                                                                                            |
| -------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Scheduled full `VACUUM` on the live store                      | 27 s write hold and a 3.06 GB WAL on the production store; 18.2 s and 1.52 GB for the 2.75 GB benchmark file at 40% free; needs about 2x file size in free disk                                           | One long write hold per run, sized by the live data rather than the freelist. Ruled out for live stores.                                                                           |
| Leave `NONE` and rely on reuse                                 | Free; the production freelist was being reused at roughly 7,000 pages a day (the growth of the live-data archives), so 629,185 pages would last months, and every whole-file backup copied them meanwhile | The cost lands on every backup and on disk, for months, after every bulk operation.                                                                                                |
| `auto_vacuum=FULL`                                             | Not measured here. SQLite moves pages at every commit that frees pages, so a bulk delete pays the whole relocation inside the deleting transaction                                                        | Puts the unbounded cost back on the foreground write that we are trying to keep short.                                                                                             |
| `incremental_vacuum` with no bound after a bulk delete         | 16-57 s of write-lock time for the production freelist (finding 4)                                                                                                                                        | Same hold as a full `VACUUM`.                                                                                                                                                      |
| `VACUUM INTO` a new file and swap                              | One full read and write of the live data, plus a daemon stop and an inode swap; writes between the copy and the stop are lost unless writes are frozen first                                              | Needs an outage and a second copy; it is the offline method with extra steps.                                                                                                      |
| Drain on the writer task through the top-level maintenance set | Same per-page cost                                                                                                                                                                                        | The closed set is parameter-free, its `execute_batch` path releases one page per call (finding 5), and queueing on the writer competes with application writes for the same queue. |

## Consequences

**Positive.**

- Free pages from bulk deletes, table moves and rewrites return to the file system within minutes, without an
  operator and without a long write hold.
- Whole-file backups stop copying the released pages. They still copy the free pages kept by the floor, and the
  partly filled pages that `incremental_vacuum` does not repack. On the production store a full `VACUUM` halved the
  per-run copy time.
- The conversion cost is paid once per store, in a window that already exists for binary swaps.

**Negative.**

- Every write on the main store pays the pointer-map upkeep: +5% CPU on bulk loads, +5% to +9% on single-row
  transaction p50, +0.12% file size, as measured above.
- While a drain runs, an application writer can wait up to one slice, at most once per checkpoint tick. The bound is
  on page work (at most 1,024 pages per slice), not on wall time: a warm slice measured 24-66 ms, and the first cold
  1,024-page slice measured 400.2 ms.
- Each released page costs about one WAL frame and one checkpoint write, so a drain adds write traffic roughly equal
  to the size it releases.
- `incremental_vacuum` moves pages but does not repack partly filled pages. A full `VACUUM` released 2.53 GiB from a
  2.40 GiB freelist on the production store; the extra 0.13 GiB of repacking is given up.
- The conversion is a full `VACUUM`, with its write hold, its WAL and its disk need.

**Neutral.**

- No logical records change. Recall, search and graph results are unaffected.
- The checkpoint task's PASSIVE and TRUNCATE rules are unchanged; the drain only reads their last observation.

## Acceptance

1. A fresh main store reads `auto_vacuum=2`. The test also covers a file where WAL is set before `auto_vacuum`,
   which must be refused or corrected, never silently left at `NONE`.
2. Delete enough rows to cross 5%. Within one check period the drain starts. Each slice releases exactly the page
   count it was issued with, measured as the change in `freelist_count`: 1,024 pages, or for the last slice
   the clamped positive remainder above the floor. The drain stops at the floor and never goes below it. A control
   arm runs the same slice through `execute_batch` and must release exactly one page.
3. With an application writer blocked inside an open write transaction, the slice reports busy and returns at once;
   the counter increments and nothing waits on the writer.
4. With the WAL above its backfill point, no slice runs until a PASSIVE observation shows full backfill. The test reads
   the gate from the checkpoint outcome's backfill comparison and asserts no WAL size bound, because FTS maintenance
   later in the same tick may write after the observation.
5. The conversion option on a `NONE` file reports `auto_vacuum` 0 before and 2 after, page and freelist counts before
   and after, and refuses when the disk headroom check fails.
6. The alarm fires on a `NONE` store above 20% free and reports the `auto_vacuum` mode beside the ratio.
