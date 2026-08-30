# ADR-165: Read-Path Performance Recovery

**Status**: proposed\
**Date**: 2026-08-17\
**Authors**: khive maintainers\
**Depends on**:

- [ADR-005](ADR-005-storage-capability-traits.md) — backend-neutral storage capabilities
- [ADR-006](ADR-006-deterministic-scoring.md) — deterministic ranking and score conversion
- [ADR-012](ADR-012-retrieval-composition.md) — retrieval composition ownership
- [ADR-021](ADR-021-memory-pack.md) — memory recall contract
- [ADR-028](ADR-028-pack-scoped-backends.md) — pack-scoped backends
- [ADR-079](ADR-079-ann-persistence-warm-path-integration.md) — memory ANN warm path
  (Vamana bridge, persisted segments)
- [ADR-091](ADR-091-wal-snapshot-lifetime.md) — WAL snapshot lifetime, reader-pin
  inventory and enforcement
- [ADR-118](ADR-118-fresh-tail-recall-visibility.md) — ANN fresh-tail consistency
- [ADR-133](ADR-133-incidental-writes-off-the-request-hot-path.md) — writer-acquisition
  reduction for incidental writes (batched audit/serve appends)

**Related**:

- [ADR-166](ADR-166-hot-path-latency-regression-guard.md) — the regression guard that
  pins each slice's recovered mechanism (depends on this record, not the reverse)

**Amends**:

- [ADR-079](ADR-079-ann-persistence-warm-path-integration.md) — the note-vector
  graphs gain a second, distinctly named consumer (the note-substrate `search`
  vector leg); the memory consumer's contract and the graphs' corpus are unchanged
- [ADR-091](ADR-091-wal-snapshot-lifetime.md) — its reader-pin inventory's
  "standalone connection per file-backed read" population shrinks to the pooled
  reader set plus the enumerated structural exceptions

---

## Context

Warm-path read latency has regressed by two to three orders of magnitude against the
published budget (10-15 ms per unified verb): `search` measures 0.6-5 s warm and hard-fails
at the 5 s coordinator deadline under load; `memory.recall` measures 4.9-8.6 s with a 150 s
outlier under sustained write load. A thirteen-hypothesis investigation — live-process CPU
accounting, connection-level source reads, on-disk index inspection, daemon log analysis,
and a counter-delta experiment against the serving process — established four independent
mechanisms. Each is a separate defect with its own fix; two of them close a positive
feedback loop that converts concurrency into timeouts.

### Mechanism 1 — the `search` vector leg is a full-table scan

The `search` vector leg queries sqlite-vec `vec0` virtual tables directly
(`crates/khive-db/src/stores/vectors.rs`, KNN `MATCH` query; DDL in
`crates/khive-db/src/backend.rs`). The DDL declares no partition keys, so a KNN query
linearly scans every chunk of the whole per-model table regardless of the `kind`
filter: at the current corpus (~189k note vectors plus ~8.5k entity vectors x 384 dims
per model, two engines) that is ~290 MB read and decoded per engine, ~580 MB per
query — for a note search and an entity search alike, since both substrates share the
table. Repetition appears fast only because the OS page cache absorbs the file reads;
under memory pressure the scan cost returns and scales with system load.

The memory pack already maintains warm Vamana ANN graphs (ADR-079) whose corpus is the
global live-note content vectors — every namespace, `kind = 'note'`,
`field = 'note.content'`, deletion-filtered at build (the corpus join in
`crates/khive-pack-memory/src/ann.rs`). That is exactly the population a
note-substrate search's vector leg needs, for both engines, current under the ADR-118
fresh-tail contract, serving `memory.recall` in microseconds. The `search` path never
consults these graphs. They do NOT cover entity vectors, which bounds what Slice 3
may claim.

### Mechanism 2 — read verbs put writer acquisitions on, and behind, the request path

Two distinct write populations attach to a read verb, and they sit in different
places:

**On the request path**: the per-dispatch audit row
(`append_audit_event_best_effort` at the dispatch layer). ADR-133's census
established that in a deployed daemon **every** verb dispatch acquires the writer at
least once for this row before the dispatch completes. Under writer starvation the
append waits up to the writer-begin timeout before its failure is swallowed, so a
nominally read-only verb carries up to that full timeout on its wall clock.

**Behind the request path**: `memory.recall` schedules a detached background task
(`track_background_task`, `crates/khive-pack-memory/src/handlers/recall.rs`)
carrying the `brain.record_serve` serve-ledger dispatch — which, being itself a
dispatch, appends a second audit row — and the recall-executed event. These do not
sit on the caller's wall clock; they add writer-queue load that the _next_
request's audit append then queues behind.

ADR-133 inventoried both populations and decided the fix (batched appends sharing
one transaction; per-call serve-ledger batching; classifier-driven commit-failure
handling). ADR-133 has not been implemented.

The new measurements convert ADR-133's contention argument from structural to
observed: the serving daemon's log records its own writer-begin attempts failing at
the full 30 s timeout repeatedly under ordinary concurrent load ("writer task could
not begin within 30000ms because SQLite remained busy"), and a live counter-delta
experiment shows one `memory.recall` adding 1-4 writer-task acquisitions to the
process within a five-second window spanning the call and its background
completion — consistent with one request-path audit append plus the background
ledger/event work. The request-path exposure (audit append inheriting the writer
queue, bounded by the writer-begin timeout) is a first-order term in recall's
latency under load; the background population amplifies the very queue it waits
in.

### Mechanism 3 — every file-backed store read opens a fresh standalone connection

The file-backed read arm of every store (text, vectors, note, entity, graph, event)
calls `open_standalone_reader` per read. The pooled readers
(`ConnectionPool::reader`, pre-opened at init) are reached only by the non-file-backed
arm. ADR-091 documented this posture while deliberately leaving connection-acquisition
strategy untouched (its subject was transaction lifetime, not routing).

Costs, per read: connection setup and a cold SQLite page cache; across the process: a
churning population of standalone read connections whose WAL snapshots pin checkpoint
progress. The daemon log shows the consequence: sustained WAL pressure past the
high-water mark with PASSIVE checkpoints unable to reclaim. Pinned WAL grows writer
work, which lengthens the writer queue, which — via Mechanism 2 — lengthens reads. This
is the feedback loop.

### Mechanism 4 — coordinator search fans out to every backend unconditionally

The cross-backend coordinator (`crates/kkernel/src/coordinator/dispatch.rs`) fans
`search` out to all registered backends regardless of the requested `kind`. A
`kind=note` search also queries the session backend — a database two orders of
magnitude larger than any result it could contribute — paying its FTS and vector cost
on every note search. The coordinator registry stores only a backend identifier and
runtime; no kind information exists anywhere in registration today, so the dispatch
loop has nothing to filter on. Which kinds a backend serves is knowable from the
pack-scoped backend configuration that wires it (ADR-028), but nothing carries that
knowledge to the coordinator.

### Non-findings (measured and excluded)

Query embedding (cached; embedding threads parked under load), dispatch-layer overhead
(sub-millisecond), per-request engine re-initialization (none), and recall's ANN leg
itself (warm, current, zero degradation warnings all day) contribute nothing
measurable. Fix effort spent on those layers is spent where the measurements show no
cost.

## Decision

A four-slice recovery program. Slice 1 is ADR-133, unchanged, elevated to first
implementation slot by the measurements above — this record adds no new decision to it.
Slices 2-4 are the decisions of this record. Each slice is independently shippable,
independently revertible, and lands together with its ADR-166 mechanism invariant so
the regression cannot silently return.

### Slice 1 — implement ADR-133 (batched incidental writes)

Implementation order within ADR-133's decisions, chosen by measured impact, with each
decision mapped to the write population (Mechanism 2) it addresses:

- D8 (acquisition-site instrumentation — partially landed via the writer-task begin
  counters): prerequisite for both populations.
- D1/D1a-d (batched audit appends): the request-path population — the per-dispatch
  audit row every verb carries, including the second audit row the background
  `brain.record_serve` dispatch generates by being a dispatch.
- D7 (per-call serve-ledger batching): the background population — collapses recall's
  per-target serve-ledger acquisitions to one per call.
- D2/D3 (classifier and accounting-bearing wait), then D6 (bulk read-flag form).

No contract stated here; ADR-133 is the contract.

### Slice 2 — file-backed store reads route through pooled readers

The file-backed read arm of every store routes through `ConnectionPool::reader`.
`open_standalone_reader` remains only for enumerated structural exceptions: boot-time
schema probing, diagnostics that must observe an independent snapshot
(`db_diagnostics`), and any path ADR-091 already binds to a standalone connection by
documented constraint. The exception list is closed and lives at the pool surface; a
new standalone-read call site is a reviewable event, not a default.

Contract:

- This slice deliberately changes the failure mode under saturation: a read that
  previously succeeded slowly through standalone-connection churn now fails fast at
  the pool-checkout timeout. That is the intended trade — bounded, observable
  failure instead of unbounded degradation that also starves the writer — and a
  checkout timeout under load is this contract working, not a regression to fix by
  reintroducing a standalone fallback.
- Pool capacity and checkout timeout keep their existing envs. Pool exhaustion returns
  the existing timeout error and MUST NOT fall back to a standalone open — a fallback
  would reintroduce the connection churn under exactly the load that makes it harmful,
  invisibly.
- A pooled reader returns to the pool with no open read transaction (no WAL pin across
  checkouts). This composes with ADR-091's registry: pooled reads hold their snapshot
  for the duration of one operation, bounded by the existing read-deadline context.
- Reader-side acquisition counters are a NEW instrumentation prerequisite of this
  slice: `db_diagnostics` today separates pooled/standalone/writer-task acquisitions
  on the WRITER side only and exposes no reader-open or reader-checkout counters.
  This slice adds pooled-reader checkouts and standalone-reader opens as counters
  (with defined reset semantics, and infrastructure opens from the closed exception
  list attributed separately), landing before or with the routing change so the
  route is observable in production; the ADR-166 invariant asserts the
  standalone-reader counter stays flat across a read-verb suite.

### Slice 3 — note-substrate `search` vector leg served by the warm ANN graphs

Scope: the **note-substrate** vector leg of `search` only. The existing graphs'
corpus (global live-note `note.content` vectors, every namespace, both engines) is
the note-search population; the graphs contain no entity vectors, so entity search
is explicitly out of this slice's scope. The entity vector corpus (~8.5k vectors)
still pays the shared-table scan after this slice; whether it gets its own graph, a
partitioned table, or stays on the scan is a separate follow-up decision with its
own consumer definition — this record deliberately does not decide it.

The note-search vector leg consults the warm Vamana graphs as a **new, named
ADR-118 consumer** — not silent reuse of the memory consumer:

- The consumer registers under its own identity with its scope predicate (global,
  `kind = 'note'`, `field = 'note.content'`) per ADR-118's
  registration-before-tail-dependent-read rule, and defines its watermark and
  compaction handling with the same same-snapshot discipline as the memory
  consumer.
- Result visibility: search performs the ADR-118 fresh-tail merge (stale graph
  candidates plus exact fresh-tail leg read in the same snapshot), giving
  note-search the same read-your-writes visibility recall has. If implementation
  instead chooses graph-only candidates without the tail merge, that weaker
  visibility contract must be stated here and land as an ADR-118 amendment before
  the code does — it is not an implementation detail.
- The exact sqlite-vec scan remains solely as the explicit fallback for a model
  with no installed graph.

Contract (both routes):

- Namespace, kind, and visibility post-filtering are identical between routes; the
  ANN route overfetches and post-filters exactly as recall does today (the graphs
  are global-scope; namespace filtering happens after candidate generation).
- Ranking stays deterministic under the ADR-006 contract: distances convert through
  the same canonical conversion on both routes, and result ordering for equal
  scores keeps the existing tie-break.
- The serving route (ANN vs fallback) is recorded per query in a NEW
  `db_diagnostics` counter pair delivered by this slice, so a silent permanent
  fallback is detectable; the ADR-166 invariant asserts the fallback count is zero
  on a warm store.
- The **Amends ADR-079** line of this record means: ADR-079 gains this second,
  distinctly named consumer of the note-vector graphs. It does not mean the memory
  consumer's contract changes, and it does not extend the graphs' corpus.

### Slice 4 — coordinator fan-out filtered by declared backend kinds

The coordinator registry today stores only a backend identifier and runtime — no
kind information exists at registration. This slice ADDS a served-kinds declaration
to backend registration as new registry metadata, supplied by the same
configuration that registers the backend (the pack-scoped backend wiring,
ADR-028), with values drawn from the closed substrate-kind set. The dispatch loop
then skips backends whose declaration affirmatively excludes the requested kind. A
`kind=note` search no longer dispatches to the session backend.

Contract:

- The filter is a registration-time declaration, not a per-call heuristic. An ABSENT
  declaration (backend registered without the new metadata) is conservatively
  included; an EMPTY declaration is a configuration error rejected at registration,
  so "declares nothing" and "declares no kinds" cannot be conflated. The filter can
  only skip a backend whose declaration is present and excludes the kind.
- Merged-result semantics (RRF, visibility) are unchanged for the backends that remain.
- This slice is a prerequisite for any physical store split (for example a dedicated
  knowledge database): without it, each split adds one backend to every search's
  fan-out and makes latency strictly worse. With it, a split changes which backends
  serve which kinds, and fan-out cost follows the declaration.

## Consequences

- The four slices remove, respectively: writer-queue inheritance on reads (Slice 1),
  per-read connection setup plus WAL-pinning churn — which also shrinks the writer's
  own starvation, compounding Slice 1 (Slice 2), the ~580 MB-per-query scan for the
  note-substrate `search` vector leg (Slice 3), and cross-backend waste plus the
  structural obstacle to store splitting (Slice 4). Entity search is outside Slice 3:
  it continues to pay the shared-table scan and keeps its current same-snapshot
  freshness contract; recovering it is future work under a separate decision.
- Slice 3 makes note-substrate `search` results boundedly stale in the same way
  recall already is. The staleness bound is the ADR-118 fresh-tail contract, not a
  new one. Entity search freshness is unchanged.
- Risk concentrates in Slice 2 (connection lifecycle) and Slice 3 (ranking parity).
  Both carry route/counter observability so a production anomaly is attributable to
  the slice that caused it, and both are revertible by routing flags at their seam.
- ADR-166's guard suite is the enforcement that this recovery, unlike previous
  point fixes in this area, stays recovered: each slice's mechanism is pinned by a
  deterministic counter invariant that fails CI if the mechanism regresses.

## Verification

Each slice lands with:

1. Before/after latency for `search kind=note limit=3` and `memory.recall limit=3` on
   the same corpus and machine class, quiet and loaded, recorded in the PR.
2. Counter evidence the intended mechanism moved: Slice 1 — writer-task acquisitions
   per recall at ambient; Slice 2 — standalone acquisition counter flat under a
   read-verb suite while the pooled counter carries the traffic, WAL steady-state under
   the high-water mark; Slice 3 — ANN route counter carries the traffic, fallback near
   zero warm; Slice 4 — zero note-search dispatches to the session backend.
3. The corresponding ADR-166 mechanism invariant merged and green in the same PR.

### Slice 2 implementation note (2026-08-30)

Slice 2 is implemented by the change closing #2024 and #1987. All nine
SQLite-backed typed-store read seams (the six named in the original inventory,
plus attachments, sparse vectors, and agent-process records) share one pooled
helper. Ordinary file-backed raw-SQL reads and reads through a queue-backed
writer use the same pool. `open_standalone_reader` is crate-private and requires
a value from the closed exception enum; no saturation path has a standalone
fallback.

The reader admission semaphore now covers both pooled guards and the explicit
raw-SQL deferred-transaction exception, so their combined concurrency cannot
exceed the configured reader budget. `db_diagnostics.reader_contention` exposes
capacity/availability, pooled and separately attributed standalone routes,
admission timeouts, and pooled checkout hold lifecycle. Deterministic regressions
hold the sole pooled reader to prove file-backed typed/raw reads block at shared
admission, distinguish retryable saturation from request cancellation, assert
ordinary reads leave standalone opens flat, and prove completed hold evidence.
