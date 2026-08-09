# ADR-150: Single Write-Owner Topology — Bounded Admission, Read Admission, and Query Interruption

**Status**: proposed\
**Date**: 2026-08-09\
**Authors**: khive maintainers

## Context

khive serves one SQLite (WAL-mode) store to many independent OS processes: the serving
daemon, per-session MCP stdio bridges, the `kkernel exec` CLI, scheduled jobs, and any
agent process a user runs. The product requirement this ADR designs for is explicit:
**hundreds of concurrent processes using the store simultaneously, without write loss,
stalls, or unbounded latency.**

Today every process that needs to write constructs its own `ConnectionPool` and writes
the database file directly. The daemon is already the _preferred_ path — bridges and the
CLI try the daemon socket first and fall back to a direct open only on socket absence or
version/namespace mismatch, counted by `khive_daemon_fallback_total{reason}`
(`crates/khive-mcp/src/daemon.rs`) — but the fallback is a full second writer, and
`ConnectionPool::open_standalone_writer_untracked` (`crates/khive-db/src/pool.rs`)
allows any caller to bypass the per-process writer queue entirely.

A 12-hour production incident (2026-08-09) exercised every seam of this topology at
roughly 15 concurrent client processes:

- A permanently-refused inbound message re-ingested in a tight retry loop denied the
  checkpointer a quiet window; the WAL grew to 986 MB while `checkpointed_frames` stayed
  pinned (fixed by classification + quarantine, #1841).
- Client disconnects did not cancel in-flight FTS5 queries; the orphaned computations
  ran for 10–30 minutes holding their read snapshots, pinning the WAL with **zero**
  external clients connected (observed twice by call-stack sampling; #1828).
- A scheduled whole-database replication pass held a read transaction for its entire
  copy of the 17 GB file every 15 minutes — a rolling WAL pin by design (#1836).
- Per-process writer pools amplified single-file contention into checkout timeouts
  across every client process at once: one blocking checkpoint tick stalls every
  process's own writer mutex (#1654).

Nine hardening PRs merged the same day (#1810, #1811, #1815, #1816, #1819, #1822,
#1823, #1825, #1841) guard these seams _within_ the current topology. They do not change
its scaling shape: contention grows with the number of independent writer pools, which
is exactly the dimension the product intends to scale by an order of magnitude.

### What the prior art establishes

A prior-art survey across SQLite itself, Litestream, LiteFS, libSQL, rqlite, Bedrock,
and browser/desktop products that embed SQLite at scale (Chromium, Firefox) supports
four load-bearing mechanical claims, each pinned to upstream source or public incident
history:

1. **N independent processes writing one WAL file directly does not scale in N.**
   Every surveyed system serving many writers converges on a single write-owner: a
   daemon or leader that owns the file and admits work through a bounded interface
   (rqlite and Bedrock via a server process; Chromium/Firefox via one owning process
   per profile). SQLite's own application-server guidance points the same way.
2. **A dedicated checkpoint connection does not change SQLite's lock graph.**
   FULL/RESTART/TRUNCATE checkpoints still take the writer lock, and RESTART/TRUNCATE
   additionally wait on readers. Litestream permanently removed routine RESTART after a
   production outage in which checkpointing caused 100 % application write failures
   (benbjohnson/litestream issue 724), and moved its triggers from physical file size
   to logical WAL offset after a feedback-loop bug (issue 997 / PR 999). Routine
   checkpointing must be PASSIVE; stronger modes are bounded maintenance under explicit
   write-admission control.
3. **Async-task cancellation is not query cancellation.** A cancelled async task leaves
   the blocking SQLite call running on its worker thread, snapshot held. Actual
   interruption requires `sqlite3_interrupt` (or closing the connection) — an
   application-level control plane no surveyed system gets for free. This is precisely
   the orphaned-query pin observed live.
4. **A long-lived read transaction pins the WAL regardless of how results are
   streamed**, so bounded-memory delivery alone is not WAL-safe; reads need admission
   control and transaction-closing pagination on the public surface.

### The passive-piggyback alternative, answered by mechanism

One respectable system, Bedrock, runs _without_ a dedicated checkpoint connection: it
piggybacks a PASSIVE checkpoint on the committing worker after releasing its commit
mutex. This works for Bedrock because Bedrock is **already a single write-owner
server** — its workers are threads of one process, so the piggyback point exists and is
serialized. The pattern addresses checkpoint scheduling only. It has no answer to
cross-process write contention, to orphaned reader pins, or to bypass writers, because
inside Bedrock those cannot exist by construction. Passive piggybacking is therefore a
_consequence_ of owning the writes, not a substitute for it; khive can adopt it as a
checkpoint policy detail after the topology change, and it is meaningless before.

## Decision

khive moves to a **single write-owner topology**. The serving daemon becomes the sole
process that opens the database read-write. All other processes interact through the
daemon (socket IPC, as today's primary path) or open the file read-only. Four components,
each independently shippable:

### 1. Authoritative write owner

- The daemon owns all write transactions. The `daemon_fallback` path stops being a
  second writer: on socket absence or mismatch, write-shaped ops **queue-and-retry or
  fail loudly with a typed, retryable error** — they never open a direct writer.
  Read-shaped ops may fall back to a read-only open.
- `ConnectionPool::open_standalone_writer_untracked` is removed from the public surface;
  the remaining internal callers (checkpoint, diagnostics, writer task —
  `crates/khive-db`) are daemon-internal by construction and documented as such.
- Writer-surface census at this revision, with the migration story for each:

  | Surface                          | Today                                     | Under this ADR                                                                                                           |
  | -------------------------------- | ----------------------------------------- | ------------------------------------------------------------------------------------------------------------------------ |
  | Daemon verb dispatch             | primary writer                            | the owner                                                                                                                |
  | `kkernel mcp` bridge fallback    | direct writer pool on mismatch            | read-only open under the out-of-authority reader discipline (component 3) + typed retryable refusal for writes           |
  | `kkernel exec` CLI               | daemon socket first, own pool on fallback | same as bridge: no direct write path, reads bounded by the same discipline                                               |
  | Maintenance CLI (reindex/import) | direct pool                               | daemon verb, or explicit `--exclusive` mode that requires the daemon stopped and takes the owner role for the invocation |
  | Scheduled backup replication     | long read TX on live file                 | reads a checkpointed snapshot or runs as a daemon-admitted registered holder (see 4)                                     |
  | Test/dev harnesses               | direct pools on scratch stores            | unchanged — scratch stores, never the production file                                                                    |

### 2. Bounded write admission

- One admission queue in the owner, bounded in depth and wait time, with a typed
  `WriteQueueFull`/deadline outcome (the #1811 contract generalized), so saturation is
  a loud, attributable signal instead of interleaved timeouts across N process-local
  queues.
- Admission classes: interactive verbs ahead of bulk ingest ahead of maintenance.
  Class-aware scheduling is a policy knob inside one queue — impossible to express
  across independent pools.
- Operation-local transaction coalescing where one logical request currently costs
  multiple write transactions.

### 3. Read admission and query interruption

- A process-wide read-admission authority in the owner: caps on concurrent synchronous
  reads, per-read row and serialized-byte budgets, and transaction-closing keyset
  pagination on public read surfaces.
- **Interruption is wired to disconnect**: when a client abandons a request, the owner
  calls `sqlite3_interrupt` on the executing connection (and the blocking-pool task is
  then reaped on its next bailout check). "Request abandoned" in the log must imply
  "snapshot released" within a bounded interval. This closes the orphaned-query pin
  class regardless of topology and is the highest-priority component to ship.
- Long/exact exports become durable jobs against a snapshot, not long live-file reads.
- **Out-of-authority readers are a bounded, detected class — never an ungoverned one.**
  Direct read-only opens exist for exactly one state: the owner is unreachable (socket
  absent or version/namespace mismatch). A reader in that state cannot pass through the
  owner's admission authority, and mechanical claim 4 applies to it in full — a long
  read transaction pins the WAL whoever holds it. The class is therefore bounded by
  construction on the reader's side and detected on the owner's side:
  - _Discipline on the opener_: fallback reads run statement-scoped (autocommit) or
    keyset-paginated with a per-page transaction — never a read transaction held
    across pages — and the read-only opener enforces a hard per-transaction duration
    ceiling via its own progress-handler watchdog. Work that expects to exceed the
    ceiling requires the owner (census registration, component 4) and is refused with
    a typed error while the owner is away, not silently degraded into a long pin.
  - _Detection of violators_: the owner's WAL-pin holder attribution (the pin
    attribution machinery landed in #1816) names any pinning pid outside the
    registered holder set as an out-of-authority pin — metered, logged with the
    pid, and surfaced by the checkpointer's pressure signal in place of an
    anonymous stall.
  - _Residual risk, named_: a non-conforming external process can always open the
    file read-only and pin the WAL; no in-repo discipline reaches it. The mitigation
    is visibility (the census names it) plus deferral of maintenance around it — not
    prevention, which SQLite's file-level access model does not offer.

### 4. Holder census as a first-class contract

- Every long read holder (backup replication, analytics, export jobs) registers with
  the owner: identity, purpose, expected duration. The checkpointer's pressure signal
  can then name its blocker, defer maintenance, or preempt a registered holder whose
  contract allows it (backup passes are safely preemptible — they write only the
  staging replica).
- WAL disk-space guard: refuse new writes with a typed error before disk exhaustion,
  rather than escalating checkpoint modes under pressure (the direction production
  systems converged on after their own incidents).

### Checkpoint policy under the new topology

Autocheckpoint disabled on the owner's writer connections; routine PASSIVE checkpoints
off the commit path (piggyback or tick — measured, not assumed); WAL extent tracked
logically (frames, not file bytes — the file size is retention, not debt);
RESTART/TRUNCATE reserved for bounded maintenance windows under the admission
authority's control.

## Consequences

**Gains.** Contention becomes O(1) in process count at the file-lock level; saturation,
starvation, and holder attribution become properties of one process's state instead of
emergent behavior across N; the interruption and admission controls close the two pin
classes measured in production; hundreds of client processes are bounded by the owner's
queue, not by lock collisions.

**Costs and risks.** The owner is a single point of failure and a latency chokepoint if
under-provisioned; IPC adds a hop to every write (today's primary path already pays it —
the change removes the _fallback_, not the common case); `--exclusive` maintenance mode
must be honest about requiring the daemon stopped; migration must not strand a client
whose daemon is mid-upgrade (the typed retryable refusal plus client retry covers the
window).

**Owner failure semantics (part of the topology, not ops detail).** The owner runs under
process supervision with keep-alive restart. On owner death: clients' in-flight requests
fail with the existing typed transport errors (already retryable); queued-but-unacked
writes are the client's to resubmit — the admission protocol's accepted/pending outcome
makes the boundary explicit; SQLite WAL crash-safety covers everything acknowledged
as committed. Supervision acceptance: kill the owner pid, observe a new pid serving and
a client write succeeding after one retry cycle, with both pids recorded. Clients never
elect a replacement writer — a client that cannot reach the owner has no write path,
which is the invariant, not a failure of it.

## Staged delivery, each stage with a load-shaped acceptance artifact

1. **Interrupt-on-disconnect** (component 3 core). Acceptance: under a synthetic load of
   long FTS queries whose clients disconnect mid-flight, `checkpointed_frames` tracks
   `log_frames` within one checkpoint interval — the measured production signature of
   this defect reproduced, then extinguished.
2. **Bypass closure + fallback demotion** (component 1). Acceptance: with the daemon
   stopped, a write-shaped op returns the typed retryable refusal and the store's file
   is untouched (mtime/WAL unchanged); grep-level proof that no non-daemon binary path
   reaches a writer open.
3. **Bounded admission with classes** (component 2). Acceptance: a saturating bulk
   ingest beside interactive verbs shows interactive p95 within its envelope and a
   non-zero, attributed rejection count — no silent interleaved timeouts. A second
   run exercises the design criterion itself at its stated scale: **at least 100
   concurrent OS client processes** issuing real verb traffic against the owner
   (interactive verbs beside a saturating bulk ingest), accepted with interactive
   p95 within its envelope, every refusal typed and attributed to its admission
   class, and zero write loss — every acknowledged write subsequently readable,
   every unacknowledged write refused with a typed outcome, reconciled by
   end-to-end accounting across all clients.
4. **Holder registry + disk guard** (component 4). Acceptance: a backup pass appears in
   the census with identity and ETA while it runs; a synthetic disk-pressure run refuses
   writes with the typed error before the space floor, and the checkpointer log names
   the registered blocker instead of an anonymous pin.
5. **Checkpoint policy tuning** on the resulting single-owner store, measured against
   the same load harness.

Stages 1 and 4's disk guard are topology-independent and land first; nothing in them is
wasted if a later stage is re-scoped.

## Alternatives considered

- **Harden the multi-writer status quo further** (rejected): the nine merged guards are
  necessary under any topology, but the contention mechanism scales with writer-pool
  count; no amount of per-process guarding changes the file-lock arithmetic at
  hundreds of processes.
- **Passive-piggyback checkpointing without ownership change** (rejected as primary):
  answered by mechanism above — it presupposes the single owner it would be a
  substitute for.
- **`BEGIN CONCURRENT` / wal2 branches** (deferred): non-mainline SQLite branches;
  re-evaluate if upstreamed. The topology decision does not depend on them and would
  compose with them.
- **A different storage engine** (out of scope): the operational model, tooling, and
  crash-safety story of mainline SQLite are load-bearing for the product; the survey
  found no engine swap that removes the need for an admission-controlled owner.

## References

- SQLite WAL and checkpoint semantics: sqlite.org/wal.html, sqlite.org/c3ref/wal_checkpoint_v2.html
- Litestream incident history: benbjohnson/litestream issues 724, 997 (PR 999)
- rqlite, Bedrock, libSQL checkpoint/ownership choices: respective public sources
- In-repo: #1828 (connection lifecycle / WAL pin), #1836 (replication holder), #1838
  (checkpoint self-amplification), #1654 (cross-process contention architecture),
  ADR-091 (WAL snapshot lifetime), ADR-135 (diagnostics surface), and the 2026-08-09
  hardening set (#1810 #1811 #1815 #1816 #1819 #1822 #1823 #1825 #1841)
