# ADR-131: Admission control for parallel write batches

- Status: accepted (2026-07-28)
- Date: 2026-07-27
- Depends on: ADR-067 (write-owner daemon, single-writer task and write queue)
- Amends: ADR-067, on two narrow points: its cross-call ordering rule (Decision
  3 below) and its published observability (Decision 4 below). Nothing else in
  ADR-067 is amended, qualified, or reversed.
- Extends: ADR-067's write queue with the admission contract it left
  undecided: one shared admission authority for the full write-serialization
  domain, a caller-visible saturation result, and an interim numeric cap for
  the batch surface (Decisions 1, 2, and 5 below).

## Context

The batch surface accepts multiple write operations in one call against a
database served by `crates/khive-db`. Two independent write-serialization
mechanisms exist there today, and only one of them is the deployed default.

### The legacy pool writer: a fixed timeout, not a queue

`ConnectionPool` documents "1 writer connection protected by a Mutex
(exclusive access)" and holds it as `writer: Arc<Mutex<Connection>>`
(`crates/khive-db/src/pool.rs:216-228`). `PoolConfig::checkout_timeout`
defaults to five seconds, overridable by `KHIVE_CHECKOUT_TIMEOUT_SECS`
(`crates/khive-db/src/pool.rs:93-104`). `ConnectionPool::writer()` waits at
most `checkout_timeout` on that mutex and, on expiry, returns the literal
error `timed out after {checkout_timeout:?} waiting for sqlite writer
connection` (`crates/khive-db/src/pool.rs:562-575`). A held writer guard runs
its full transaction closure before releasing
(`crates/khive-db/src/pool.rs:337-362`; consumed directly by
`StorageBackend::apply_schema`, `crates/khive-db/src/backend.rs:147-158`).
There is no admission queue ahead of this mutex: a caller either acquires it
within the fixed budget or fails.

### ADR-067's WriterTask: an accepted queue, default for file-backed pools

ADR-067 (accepted 2026-07-05, Amendment 1 dated 2026-07-06) specifies a
three-part mechanism: Component A, a dedicated `WriterTask` that owns a
standalone writer connection and serializes writes through a bounded
`tokio::mpsc` channel; Component B, batched commits, a collect window that
groups multiple drained requests into one `BEGIN IMMEDIATE`/`COMMIT` using a
per-request SAVEPOINT hierarchy for isolation; and Component D, a
transaction watchdog that interrupts a batch exceeding a configured timeout.
Amendment 1 records only Component A as landed in full, with every
store write path and `SqlBridge`'s writer methods routed through it.
Components B and D remain accepted ADR-067 policy, not shipped behavior: the
current drain loop receives and executes one request at a time, wrapping
each in its own `BEGIN IMMEDIATE`/`COMMIT` with no collect window, no
multi-request batching, and no per-request SAVEPOINT hierarchy
(`crates/khive-db/src/writer_task.rs:345-401`). The channel's normal
saturation behavior is backpressure:
`send()` awaits capacity with no `try_send` escape hatch
(`crates/khive-db/src/writer_task.rs:214-229`); a caller that wraps the
enqueue in `send_with_timeout` instead receives `StorageError::WriteQueueFull`
if capacity is not obtained within its own deadline, a distinct failure from
the legacy mutex timeout (`crates/khive-db/src/writer_task.rs:231-267`). The
task drains one request at a time in arrival order
(`crates/khive-db/src/writer_task.rs:310-334,345-401`) and publishes only a
point-in-time depth snapshot that the source itself documents as racy and
"never used for any correctness decision"
(`crates/khive-db/src/writer_task.rs:291-307`).

Three properties of this accepted design are not yet decided for the batch
surface:

1. **The queue is the default for file-backed pools.** `PoolConfig::write_queue_enabled`
   is `Option<bool>`; `ConnectionPool::new` resolves an unset value to
   `Some(true)` for file-backed pools and `Some(false)` for in-memory pools,
   with an explicit `Some(_)` always winning (`crates/khive-db/src/pool.rs:493`,
   landed in `7114a7d7e` / #1696). The `KHIVE_WRITE_QUEUE` environment
   variable can still force either state. Today the unavailability behavior
   is split. A call outside an async runtime returns
   the typed error `StorageError::WriterTaskNoRuntime` before any fallback
   state is cached (`crates/khive-db/src/pool.rs:656-680`). Any other
   writer-task spawn failure caches `None` once, after which
   `ConnectionPool::writer_task_handle()` degrades every subsequent write on
   that pool to the legacy mutex path with only a one-time log line, never a
   caller-visible error (`crates/khive-db/src/pool.rs:688-700`).
2. **Its ordering rule has no call awareness.** ADR-067's own failure-mode
   table states ordering as "FIFO within a batch window; cross-batch order is
   not guaranteed": every enqueued operation drains strictly in arrival order
   regardless of which call produced it, so one large batch can occupy a long
   contiguous run of the shared channel ahead of a concurrently arriving small
   call.
3. **Observability stops at a depth snapshot.** No wait-time, active-call, or
   saturation metric exists at the queue boundary; the only contention signal
   a caller has today is the legacy mutex's timeout error.

### Illustrative failure mode

Consider a batch surface exercising the embed-bearing and hard-delete paths
against these mechanisms. A single embed-creating operation completes
quickly, and small batches — say, 10 embed-creating operations — complete
cleanly with every operation succeeding. A batch of 15 embed-creating
operations still completes cleanly, with zero writer timeouts. A larger
batch, however — say, 36 parallel hard-delete operations — can exceed the
legacy mutex's fixed checkout budget: a fraction of the operations fail with
the signature `timed out after 5s waiting for sqlite writer connection`,
while the rest succeed; the failed operations succeed cleanly when retried as
a smaller batch. The same signature can also surface from two independently
issued, ordinary concurrent write calls against the same database, each well
within any reasonable per-call limit.

That failure text matches only `ConnectionPool::writer()`'s error format,
never `WriteQueueFull`, so it is the legacy mutex path, not the WriterTask
queue, that this failure mode exercises. The contention it illustrates is
transient (retries succeed) and workload-dependent: embed-bearing batches
tolerate more concurrency because embed inference dominates per-operation
time and staggers arrival at the writer. This does not establish a numeric
failure boundary — 15 is a conservative clean-batch size, not a proven
threshold, and a 36-operation batch is an illustration of the failure on this
path, not a wedge (the retry succeeds).

### Why per-batch admission alone cannot close this

The fixed checkout budget in `ConnectionPool::writer()` is shared by every
caller of that method, not allocated per batch. Two calls that are each
individually below any per-batch operation cap can still arrive concurrently
and jointly exceed that shared budget for whichever caller checks out last,
as the cross-call example above illustrates: two ordinary, independently
admitted concurrent calls are enough. A control that only bounds the size of
one call's batch has no visibility into
what else is concurrently queued and cannot bound the aggregate.

## Decision

### Scope

This ADR governs one canonical database serialization domain: every write
call against a database served by `crates/khive-db`, whether it submits one
operation or many, and regardless of which `ConnectionPool` instance or store
path issues it, **except** the paths listed in the exemption inventory
immediately below. "Batch surface" identifies where the interim caps
(Decision 5) and the round-robin scheduler's per-batch framing (Decision 3)
apply; the shared admission authority in Decision 1 is not limited to calls
that arrive through the batch surface, and neither is the fairness rotation
in Decision 3, which admits single-operation calls on the same terms.

#### Exemption inventory (from ADR-067 Amendment 1, "Corrected inventory (final state at merge)")

ADR-067 Amendment 1 (2026-07-06) records that Component A landed in full,
with all store write paths, `SqlBridge`'s writer methods, the
`atomic_unit` multi-statement units, and the `execute_script_top_level`
maintenance seam routed through the writer task, and that four paths do not
traverse it. Those four are excluded from this ADR's governed population.
Each is listed with the disposition Amendment 1 assigns it and the migration
or companion record that retires that disposition:

| Path                                                                                                                                            | Mechanism                                                                                                      | Disposition (ADR-067 Amendment 1)                                                                                                                                         | Retired by                                                           |
| ----------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------- |
| Periodic checkpoint task                                                                                                                        | `try_writer_nowait` plus direct `execute_batch`                                                                | EXEMPT (by design): deliberate bypass of the channel, nowait acquisition, never holds a transaction across the checkpoint                                                 | Nothing. The exemption is permanent unless ADR-067 is itself amended |
| Startup migrations and pack DDL                                                                                                                 | direct pool at boot                                                                                            | EXEMPT (by design): runs before concurrent traffic exists                                                                                                                 | Nothing. The exemption is permanent unless ADR-067 is itself amended |
| Session-mirror ingest, the sole live production caller of `begin_tx()` (`write_events_and_cursor` in `khive-pack-session/src/mirror/ingest.rs`) | standalone connection via `begin_tx()`                                                                         | DEFERRED to a follow-up ADR, which converts the caller to `atomic_unit` and retires the method if no non-test caller remains                                              | The follow-up ADR                                                    |
| Vector-store `orphan_sweep`                                                                                                                     | `with_writer_unmanaged`, opening its own `BEGIN IMMEDIATE` on a pool writer connection outside the writer task | MIGRATE: convert to `atomic_unit` (mechanical; the accepted seam covers it). Until converted this is a known residual competing-writer path alongside `begin_tx`'s caller | Conversion of `orphan_sweep` to `atomic_unit`                        |

The governed population is therefore every write call against a database
served by `crates/khive-db` other than these four paths. Decisions 1 through
4 impose no requirement on an exempt path, and no exempt path is required to
enter the admission authority, the rotation, or the deadline contract while
its disposition stands. A path whose disposition is retired by the record
named above ceases to be exempt at that point and joins the governed
population without a further amendment to this ADR.

The two paths with unretired non-permanent dispositions, session-mirror
ingest and `orphan_sweep`, remain competing writers against the pool writer
connection for as long as those dispositions stand. That is a known,
inherited condition of ADR-067's migration state, not a property this ADR
introduces, and Decision 5's retirement criteria account for it explicitly.

This ADR does not change ADR-067's specified `WriterTask` internals (channel
type; Component B's batched commits and per-request SAVEPOINT hierarchy;
Component D's transaction watchdog), whether or not those components have
landed, beyond the two points named in Decisions 3 and 4, and it does not
change the legacy pool-mutex mechanism itself, which remains available only
as an explicit, non-default opt-out. This ADR is designed against the drain
loop as it exists today, one request executed at a time in its own
transaction (`crates/khive-db/src/writer_task.rs:345-401`); it does not
depend on Components B or D landing first, because the admission and
rotation stages act only on which requests enter the writer queue's channel
and in what order, not on how the drain loop then executes them. Nothing in
Decisions 1 through 5 requires revision if Components B or D land later.

### 1. One shared admission authority, keyed by canonical database identity, for every write call; a per-batch cap is defense in depth, not the mechanism

Every write call in the serialization domain, single-operation or
multi-operation, admits through ADR-067's `WriterTask` queue by default, not
through `ConnectionPool::writer()`'s legacy mutex path.
`PoolConfig::write_queue_enabled` defaults to `true` for batch-surface-serving
deployments; the legacy path is selectable only by explicit configuration,
never a silent default.

The admission authority is keyed by canonical database identity, not by
`ConnectionPool` instance. Today `ConnectionPool::writer_task_handle()`
lazily spawns one `WriterTask` per pool, documented as "exactly one writer
task exists per `ConnectionPool` (per DB file)"
(`crates/khive-db/src/pool.rs:643-666`), and nothing prevents a second
`ConnectionPool` from opening against the same file: the repository has a
direct test of exactly that two-pool/same-file topology
(`crates/khive-db/src/backend.rs:1377-1405`), and each pool independently
spawns its own `WriterTask` when the flag is on. Two such pools produce two
independent queues serializing writes to the same file, which defeats the
single admission authority this decision requires. This decision closes that
gap with a process-wide writer-task registry keyed by canonical database
identity, the same identity `ConnectionPool::origin()` already exposes
(`crates/khive-db/src/pool.rs:625-632`): a second `ConnectionPool` opened for
an identity that already has a live writer task obtains a handle to the
existing task instead of spawning a second one. A structural prohibition on
ever constructing a second `ConnectionPool` for the same identity is
rejected as the mechanism: pool construction is call-site-scoped throughout
the codebase, so preventing a second pool would require auditing and gating
every construction call site, while keying a registry by identity requires a
change only inside `writer_task_handle()`'s spawn path.

The mechanism must not silently fall back to the legacy mutex path when the
queue is unavailable for a reason other than being deliberately disabled.
Every writer-task spawn failure other than deliberate disablement must
surface to the caller as a typed admission-unavailable condition, distinct
from the normal saturation result in Decision 2. For a missing async runtime
that surfacing already exists as `StorageError::WriterTaskNoRuntime`
(`crates/khive-db/src/pool.rs:656-680`) and maps into the admission-unavailable
condition rather than remaining a separate ad hoc error; for every other
spawn failure, the cached-`None` silent degradation to the legacy path
(`crates/khive-db/src/pool.rs:688-700`) is the behavior this requirement
eliminates.

A per-batch operation-count cap at the request layer remains useful only as
defense in depth: it bounds work contributed by one call, never the aggregate
contributed by concurrently accepted calls. The corroborating cross-call
observation in Context establishes that two ordinary, independently accepted
calls can each be arbitrarily small and still exceed the shared resource's
fixed budget; a control that inspects only one call's batch size cannot see
the others. Decision 5 keeps a numeric cap as an interim, retiring measure,
not as the permanent mechanism.

### 2. Bounded admission deadline, with one deadline authority and a deterministic saturation hint

Every write call's enqueue into the writer queue carries a finite admission
deadline per operation, using ADR-067's existing `send_with_timeout` seam
rather than the unbounded `send().await` every current caller uses. The
deadline's authority is a single server-side configuration value,
`write_admission_deadline_ms`, defaulting to 2000 ms and bounded to the
range [100, 10000] ms at configuration load; a value outside that range is a
configuration error, not silently clamped. When the caller's outer request
deadline (for example, an MCP call timeout) leaves less time remaining than
the configured admission deadline, the admission deadline applied to that
operation is the remaining outer budget instead, so an admission rejection
is always surfaced before the outer deadline expires; when no outer deadline
is supplied, the configured default applies unmodified.

When capacity is not obtained before that deadline, the batch surface rejects
only the not-yet-admitted operation with a stable typed result,
`writer_queue_saturated`, carrying `retryable: true`, `scope:
"writer_admission"`, and a server-supplied `retry_after_ms` hint set equal to
the admission deadline actually applied to the rejected operation. This is a
deterministic, deadline-derived value, not an estimate of current
contention: it tells a retrying caller to wait at least one full admission
window before retrying, giving already-admitted work a chance to clear. This
is distinct from the underlying `StorageError::WriteQueueFull`; the batch
surface maps it to the stable typed meaning rather than exposing a
storage-specific error variant to callers.

Every operation submitted in a batch carries its own outcome in the response.
An operation admitted before its deadline elapses runs to completion under
its normal operation deadline and is never reclassified as unadmitted after
the fact: `writer_queue_saturated` means the operation was never accepted,
and only an operation carrying that result may be safely retried.

### 3. Call-aware round-robin admission ahead of the writer queue, for every call in the serialization domain (amends ADR-067's ordering rule)

ADR-067's ordering rule, "FIFO within a batch window; cross-batch order is not
guaranteed," admits every enqueued operation strictly in arrival order
regardless of which call produced it. This decision amends that rule for
every call that reaches the admission authority named in Decision 1,
including single-operation calls: a single-operation call is a call with
exactly one pending operation, and it rotates on the same terms as any other
call. ADR-067's channel type and its specified batched commits and
per-request SAVEPOINT hierarchy design, whether or not yet landed, are
otherwise unchanged.

An admission stage ahead of the writer queue's bounded channel selects among
calls that currently have at least one pending, not-yet-offered operation, in
FIFO order of each call's first pending admission, and offers enqueue to at
most one operation from the selected call before moving to the next. A call
with operations remaining after its turn returns to the tail of that
rotation; a newly arriving call joins the tail. Within one call, its own
operations retain submission order.

This bound is stated from the point an operation is first offered to the
rotation (`writer_admission_scheduler_wait_seconds`'s start point in Decision
4): a newly offered operation waits for at most one operation per other
active call already in the rotation before winning its own turn and being
offered to the writer queue's bounded channel. That bound covers only the
rotation stage. It does not bound an operation's total wait once it is
offered to the channel: the channel itself still drains FIFO
(`crates/khive-db/src/writer_task.rs:345-401`), so an operation that wins its
rotation turn can still queue behind whatever was already accepted into the
channel ahead of it, from earlier rotation winners. This decision does not
reorder work already accepted into the channel; it bounds only the rotation
stage's contribution to wait. Decision 4's `writer_admission_wait_seconds`
and `writer_admission_accepted_queue_age_seconds` make the residual,
post-rotation wait measurable rather than implied by the rotation bound. A
continuously backlogged large batch cannot monopolize the rotation while any
other call is pending, and a stream of newly arriving small calls cannot
indefinitely bypass an already-active large batch either, because new calls
join the tail rather than preempting it; the rotation bound above governs
that fairness property, not the channel's residual drain-order wait.

### 4. Publish admission depth, wait time, and saturation at the writer-queue boundary (extends ADR-067's observability)

ADR-067's only published signal is `WriterTaskHandle::queue_depth()`, which
the source documents as a racy point-in-time snapshot never used for any
correctness decision. That is enough to see a queue is nonempty; it is not
enough to see how severely callers experience it. The admission boundary
introduced by Decisions 1 through 3 timestamps each operation at five points:
**offered** (first presented to the round-robin admission stage in Decision
3), **scheduler-selected** (the rotation offers this operation to the writer
queue's bounded channel), **channel-accepted** (the channel `send` in
Decision 2 completes and the operation is inside the bounded `tokio::mpsc`
channel), **execution-started** (the drain loop,
`crates/khive-db/src/writer_task.rs:345-401`, dequeues the operation and
begins its transaction), and **completed** (the typed reply is sent). It
publishes:

- `writer_admission_queue_depth`: current pending-operation count, sampled at
  enqueue, dequeue, rejection, and completion.
- `writer_admission_active_calls`: number of calls with at least one pending
  or executing operation.
- `writer_admission_scheduler_wait_seconds`: a histogram from **offered** to
  **scheduler-selected**, sampled only for operations that reach
  scheduler-selected. This is the bound Decision 3 states: at most one
  operation per other active call already in the rotation.
- `writer_admission_wait_seconds`: a histogram from **offered** to
  **channel-accepted**, sampled only for operations actually accepted into
  the channel. This is the full admission wait a caller experiences before
  its operation is inside the writer queue's channel; it includes both the
  rotation wait above and any residual wait for channel capacity.
- `writer_admission_accepted_queue_age_seconds`: a histogram from
  **channel-accepted** to **execution-started**, exposing the residual,
  post-rotation wait inside the bounded channel described in Decision 3,
  separately from the admission wait above.
- `writer_admission_service_seconds`: a histogram from **execution-started**
  to **completed**. This includes SQLite lock acquisition inside the writer
  task's transaction, since there is no separate lock-wait phase once the
  drain loop has dequeued the operation and dispatched it to
  `spawn_blocking`; it does not include any wait before
  **execution-started**.
- `writer_admission_saturation_total`: a counter of rejected operations,
  labeled by the stable saturation reason from Decision 2. Rejection is
  counted only here, never as an outcome label on a duration histogram, so a
  duration series and a rejection count are never conflated in one metric.

Metric labels identify only the database or admission domain, using a
bounded-cardinality identifier; they never carry a call, entity, or operation
identifier. A single-request correlation identifier may appear in structured
diagnostics but never in an aggregate metric label.

### 5. Interim per-batch caps of 20 and 15, retired only by measured criteria

Until Decisions 1 through 4 are in force by default in production, the batch
surface enforces:

1. A parallel batch containing any write operation must contain no more than
   20 total operations.
2. A parallel batch containing any embed-bearing write must contain no more
   than 15 total operations; this tighter limit takes precedence when both
   clauses apply.
3. An oversized batch is rejected before execution with a typed, non-partial
   validation error naming the applicable maximum.
4. Callers must inspect every per-operation result; batch-level success does
   not imply every operation succeeded.

The value 15 matches the conservative clean-batch size used in the
illustrative example in Context, not a proven failure boundary. The value
20 is the prior operational guidance carried forward, not a demonstrated
universal safe limit. Both are deliberately conservative and temporary.

Retire both limits only when all of the following hold:

1. An integration test proves every write in the governed population defined
   in Scope, for one database's serialization domain, including
   single-operation calls and a second `ConnectionPool` opened against the
   same canonical database identity, traverses the same writer queue from
   Decision 1, with no silent fallback to the legacy mutex path and no
   independent second queue. The four paths in the Scope exemption inventory
   are outside this population while their dispositions stand; the test
   asserts their exclusion by name rather than narrowing the population
   implicitly.
2. A concurrent-call test, at least two calls totaling 36 or more writes,
   including one large batch competing against repeated single-operation
   calls, completes with zero occurrences of the legacy mutex's five-second
   timeout error **attributable to a governed write** and demonstrates
   bounded progress for both the large batch and the small calls, per
   Decision 3. Each observed occurrence of that error must be attributed to
   the path that produced it. An occurrence attributed to a path in the Scope
   exemption inventory whose disposition still stands, `orphan_sweep`'s
   `with_writer_unmanaged` contention being the live example, is recorded and
   reported with its attribution but does not fail this criterion, because
   Decision 1 does not govern that path and an exempt path must not fail the
   gate for the mechanism being proven. Once a path's disposition is retired
   by the record named in the inventory, it is governed, and its timeouts
   count against this criterion. An occurrence that cannot be attributed to a
   specific path counts as governed and fails the criterion.
3. Saturation tests confirm the typed `writer_queue_saturated` result, correct
   per-operation acceptance semantics, and correct retry classification, per
   Decision 2.
4. All seven metrics in Decision 4 are emitted and exercised by tests.
5. The writer queue is enabled by default on the deployed batch path, and an
   operational observation window of at least seven consecutive days and at
   least 10,000 admitted writes on that path records zero legacy mutex
   timeouts attributable to a governed write, zero fairness-rotation
   violations, and complete emission of the required metrics. The same
   attribution rule as criterion 2 applies: a legacy mutex timeout attributed
   to a path in the Scope exemption inventory whose disposition still stands
   is recorded and reported in the window's summary with its attribution, and
   does not fail this criterion; a timeout attributed to a governed write, to
   a path whose disposition has since been retired by its named migration or
   companion, or to no identifiable path, fails it. The window's summary must
   state the count and attribution of every exempt-path timeout observed, so
   that residual competing-writer contention stays visible rather than being
   discarded by the exemption.

Meeting only a calendar date, merging code without default enablement, or
observing a single clean batch does not satisfy retirement.

## Consequences

- The writer-queue admission boundary becomes the explicit coordination point
  for every write in the serialization domain, not only batch-surface calls.
  This is intentional: those writers already contend for one legacy mutex or
  one writer-task connection, and a second `ConnectionPool` on the same
  database identity now resolves to the same writer task instead of an
  independent one (Decision 1).
- Fair scheduling (Decision 3) adds call-identity tracking and a per-call
  pending list ahead of ADR-067's channel, covering single-operation calls on
  the same terms as multi-operation batches; ADR-067's channel type and its
  specified batching and SAVEPOINT hierarchy design, whether or not yet
  landed, are otherwise unchanged.
- Overload becomes measurable waiting and a typed rejection (Decision 2)
  instead of an opaque mutex timeout. Callers that today only check the outer
  batch response's `ok` field must add a branch for `writer_queue_saturated`.
- Enabling the writer queue by default (Decision 1) exercises a code path
  ADR-067 shipped but gated behind `KHIVE_WRITE_QUEUE` for initial rollout;
  the retirement criteria in Decision 5 are the confirmation ADR-067's
  migration plan deferred.
- The interim caps (Decision 5) constrain the public batch API in the
  meantime. A workload that legitimately needs a larger batch has no relief
  until the mechanism is proven and defaulted on.
- This ADR does not alter the legacy pool-mutex mechanism itself, which
  remains available as an explicit, non-default opt-out.

## Rejected alternatives

- **Scale the legacy mutex's checkout timeout with queue depth (Decision
  2).** Rejected: it permits unbounded latency, treats a symptom as capacity
  control, and races on a depth snapshot that can change immediately after
  being read.
- **Propagate a single deadline across both admission and execution (Decision
  2).** Rejected: one timeout cannot tell a caller whether an operation was
  never accepted or may already have executed, which makes retries unsafe.
- **Reject every operation immediately whenever queue depth is above zero,
  with no admission wait (Decision 2).** Rejected: it discards the transient,
  self-clearing contention the illustrative example in Context shows retries
  resolve on their own, converting brief recoverable queuing into guaranteed
  failure.
- **Wait for admission with no deadline (Decision 2).** Rejected: this is
  today's `send().await` behavior; it gives a caller no saturation signal to
  act on and no basis for a retry decision, only an indefinite suspension.
- **Structurally prohibit a second `ConnectionPool` for the same canonical
  database identity, instead of a shared registry (Decision 1).** Rejected:
  pool construction is call-site-scoped throughout the codebase, so
  preventing a second pool would require auditing and gating every
  construction call site, while a registry keyed by identity requires a
  change only inside the writer-task spawn path.
- **Keep ADR-067's global operation FIFO unchanged (Decision 3).** Rejected as
  the serialization-domain default because a large batch can occupy a long
  contiguous prefix of the shared channel and force a concurrently arriving
  small call to wait behind the entire batch.
- **Strict small-call priority (Decision 3).** Rejected: sustained small-call
  traffic can starve a large batch indefinitely, trading one starvation class
  for another.
- **Allow a caller to send directly to the writer queue's channel, bypassing
  the rotation stage (Decision 3).** Rejected: a direct sender is invisible
  to the rotation and can rebuild the same monopolization the plain FIFO
  channel produces on its own; Decision 1's shared admission authority
  requires every caller in the serialization domain to enter through the
  rotation.
- **Keep ADR-067's queue-depth snapshot as the only published signal
  (Decision 4).** Rejected as sufficient because equal depth can produce very
  different caller-visible waits depending on per-operation service time.
- **Infer saturation from legacy-mutex timeout counts (Decision 4).**
  Rejected: those timeouts occur after opaque waiting on a different
  mechanism and cannot distinguish admission delay from unrelated
  writer-path failures.
- **Flip `write_queue_enabled` to default `true` with no other change
  (Decisions 1, 3, 4).** Rejected: it inherits the FIFO starvation risk and
  the depth-only visibility gap this ADR closes, and it does not supply the
  batch surface's caller-visible saturation contract.
- **Remove the interim caps as soon as the writer-queue code merges (Decision
  5).** Rejected: a merge proves the code compiles, not that batch-surface
  traffic routes through it by default, that fairness and saturation hold
  under concurrent load, or that production observability exists.
- **Keep the 20 and 15 caps permanently instead of retiring them (Decision
  5).** Rejected: the values are a conservative illustration and interim
  guidance, not durable capacity properties of the writer.

## Verification

1. A configuration test asserting `PoolConfig::write_queue_enabled` defaults
   to `true` for the batch-surface configuration, with the legacy path
   selectable only by explicit configuration.
2. A test that forces `ConnectionPool::writer_task_handle()` to fail for a
   reason other than the flag being off and asserts the batch surface returns
   a typed admission-unavailable result rather than completing silently on
   the legacy mutex path.
3. Saturation contract tests: an operation whose enqueue deadline elapses
   returns `writer_queue_saturated` with `retryable: true` and a
   `retry_after_ms` hint; an operation admitted before its deadline runs to
   completion and is never reclassified as unadmitted.
4. A fairness test with one large batch and repeated single-operation calls
   submitted concurrently, asserting the rotation-stage wait for any call
   (offered to scheduler-selected) does not exceed one operation per other
   active call per rotation, and that both the batch and the small calls make
   bounded progress through the full path including the writer queue's
   channel.
5. Metrics tests asserting emission of all seven named series under load,
   saturation, and idle conditions; asserting `writer_admission_wait_seconds`
   never carries a rejection outcome as a label (rejections are counted only
   by `writer_admission_saturation_total`); and asserting no call, entity, or
   operation identifier appears in any metric label.
6. A test asserting Decisions 1 through 3 (shared admission, saturation
   typing, rotation fairness) produce their required behavior when run
   against the current drain loop, one request executed at a time with no
   collect window and no multi-request batching, so landing ADR-067
   Components B or D later is not a precondition for this ADR's retirement
   gates.
7. The five retirement-gate tests named in Decision 5, run against a build
   with the writer queue enabled by default, before the interim caps are
   removed.
8. A regression test pinning the interim caps: a 21-operation plain-write
   batch and a 16-operation embed-bearing batch are each rejected before
   execution with a typed, non-partial validation error naming the
   applicable maximum.

## References

- ADR-067: Write-owner daemon, single-writer task and write queue
- ADR-067 Amendment 1 (2026-07-06), "Corrected inventory (final state at
  merge)": the source of this record's Scope exemption inventory, including
  the `try_writer_nowait` checkpoint task and startup-migration EXEMPT
  entries, the `begin_tx()` entry DEFERRED to a follow-up ADR, and
  the `orphan_sweep` / `with_writer_unmanaged` MIGRATE entry
- `crates/khive-db/src/pool.rs`: legacy writer mutex (`:216-228`), checkout
  timeout default and override (`:93-104`), checkout wait and error
  (`:562-575`), transaction closure (`:337-362`), write-queue enablement flag
  (`:113-121`), canonical database identity (`origin()`, `:625-632`),
  one-writer-task-per-pool doc comment (`:643-666`), typed
  `WriterTaskNoRuntime` on a missing async runtime (`:656-680`), writer-task
  handle and silent cached-`None` fallback on other spawn failures
  (`:688-700`)
- `crates/khive-db/src/writer_task.rs`: bounded-channel send and backpressure
  (`:214-229`), enqueue-deadline send and `WriteQueueFull` (`:231-267`), spawn
  and drain loop (`:310-334,345-401`), depth snapshot (`:291-307`)
- `crates/khive-db/src/backend.rs`: `apply_schema` acquiring the legacy writer
  path (`:147-158`); two-pool/same-file writer topology test
  (`:1377-1405`)
