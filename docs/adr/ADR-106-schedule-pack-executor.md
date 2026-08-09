# ADR-106: Schedule Pack Executor — Daemon-Resident Tick for the Pending-Event Drain

**Status**: Accepted
**Date**: 2026-07-09
**Amended**: 2026-08-07 (missed-event grace policy, Amendment A; implementation note,
Amendment B; reminder delivery and failure observability, Amendment C; process-local
ticker liveness, Amendment D; ADR-119 supervision and creator-bound replay, Amendment E;
durable dispatch receipts and renewable leases, Amendment F)
**Depends on**: [ADR-040](ADR-040-communication-and-schedule-packs.md) (schedule pack
verbs and `scheduled_event` note kind), [ADR-049](ADR-049-khived-daemon.md) (warm daemon
process model), [ADR-016](ADR-016-request-dsl.md) (request DSL, replayed at fire time),
[ADR-119](ADR-119-daemon-component-supervision.md) (host-owned lifecycle and identity fence)

## Context

The `khive-pack-schedule` crate stores scheduling intent only. Its own module
documentation is explicit about the boundary: "Trigger evaluation is NOT performed by
the pack — the pack only stores intent." The pack exposes four verbs
(`schedule.remind`, `schedule.schedule`, `schedule.agenda`, `schedule.cancel`), all of
which read or write a `scheduled_event` note. The pack does not evaluate `trigger_at`,
claim rows for firing, dispatch payloads, or transition a due row to `firing` or
`fired`; that is the drain's job, described next. `schedule.cancel` is the one
exception to "stores intent only": it does transition a row it owns, from `pending` to
`cancelled` (`crates/khive-pack-schedule/src/handlers.rs`, `cancel_pending_event`), via
a conditional CAS update guarded by `status = 'pending'` so a concurrent fire can never
be clobbered by a stale cancel.

A separate, already-shipped component performs the actual firing: `kkernel exec
--pending-events` (`crates/khive-mcp/src/pending_events.rs`, entry point
`run_pending_events`). This is a complete, well-tested one-shot drain, not a stub:

- A DB-level compare-and-swap state machine moves each due row through
  `pending → firing → fired` (or back to `pending` for a repeating event), using a
  claim token (`firing_at` plus `dispatch_receipt.invocation_id`) that
  `finalize_fired_event` must match exactly before it will transition a row out of
  `firing`. The claim atomically persists occurrence/invocation identity and a renewable
  lease; expired claims are reconciled according to their durable receipt state rather
  than blindly replayed (Amendment F).
- Discovery is namespace-partitioned (`discover_pending_namespaces`) and SQL-pushed: a
  `json_extract(properties, '$.trigger_at') <= ?` pre-filter, followed by a Rust-side
  re-check against a parsed `DateTime<Utc>`.
- The stored action is a DSL string, write-time validated by
  `schedule.schedule` (single op, exactly one registered handler, literal args only, no
  `$prev`) and re-parsed at fire time by `dispatch_action`, which reconstructs JSON-form
  ops with the event's own `namespace` injected and replays them through the real
  `VerbRegistry`. Fire-time reparse (rather than persisting an already-compiled op)
  means a verb-surface change between store and fire produces an explicit failed
  dispatch, never a silent misdispatch.
- Repeat advancement (`next_trigger_at`) handles the named aliases `daily` / `weekly` /
  `monthly`. Five-field cron is rejected at write time because the drain cannot advance
  it; a legacy cron row fails closed before dispatch rather than degrading to one-shot.
- The module exposes a cron-friendly one-shot CLI drain in addition to the
  daemon-resident tick, so an external `* * * * * kkernel exec --pending-events`
  remains a supported minute-granularity invocation mode.

The gap is exactly that last sentence: nothing in a default khive deployment invokes
this drain periodically. No cron entry ships with khive, and the warm daemon process
that khive already runs for other purposes (`khived`, `khive-mcp --daemon`, per
ADR-049) never calls it. A `scheduled_event` note can sit `pending` past its
`trigger_at` indefinitely unless an operator has separately wired up external cron. The
executor logic is not missing; its invocation is.

### The daemon's existing periodic-task pattern

The warm daemon already runs one directly analogous recurring background task: the WAL
checkpoint loop. `run_daemon_with_boot_guard`
(`crates/khive-runtime/src/daemon.rs`) spawns it, once the daemon has bound its Unix
socket and written its PID file, alongside a one-shot ANN/embedder warm-up:

```rust
{
    let warm = dispatcher.clone();
    tokio::spawn(async move {
        warm.warm_all().await;
    });
}

if let Some(pool) = dispatcher.pool_for_checkpoint() {
    let cfg = CheckpointConfig::from_env();
    let event_store = dispatcher.event_store_for_checkpoint();
    let namespace = dispatcher.namespace().to_string();
    tokio::spawn(run_checkpoint_task(pool, cfg, event_store, namespace));
    tracing::info!("WAL checkpoint task started");
}
```

`run_checkpoint_task` (`crates/khive-db/src/checkpoint.rs`) is the closer precedent for
a schedule tick than the one-shot warm-up: it is a genuine interval loop
(`tokio::time::interval`, `MissedTickBehavior::Skip`) that runs for the daemon's
lifetime and detects shutdown by checking `Arc::strong_count(&pool) <= 1` on each tick
rather than a separate cancellation channel. That check does not, however, reflect the
task's full production ownership graph: `run_daemon_with_boot_guard` also passes
`run_checkpoint_task` an `event_store` (`crates/khive-runtime/src/daemon.rs:957`), and
the production `SqlEventStore` retains its own separate clone of the same
`Arc<ConnectionPool>` in its `pool` field (`crates/khive-db/src/stores/event.rs:37`), a
clone the `<= 1` check never accounts for.

A second existing pattern establishes the role gate a periodic background loop needs.
`crates/khive-mcp/src/serve.rs` gates the email-channel poll/outbox loops behind
`is_daemon_role(args)` (`args.daemon`), after those loops were previously spawned
unconditionally from every serve entrypoint and caused nine concurrent stdio client
processes to poll the same mailbox independently, exhausting the mail provider's
per-mailbox connection slots for roughly nineteen hours. The fix, spawning the
recurring loop only when the process is the daemon and never from a per-client stdio
session, is the exact shape a schedule tick needs, since `run_pending_events` is
equally unsafe to run once per client process.

### Why the drain cannot be ticked as-is

`run_pending_events(db: Option<&str>, namespace: &str, verbose: bool)` builds its own
runtime on every call: it constructs a fresh `RuntimeConfig`, opens a new
`KhiveRuntime`, and wraps it in a new `KhiveMcpServer`. This is correct for a one-shot
CLI invocation (a new process, a new connection pool, exit when done) but wrong for a
daemon-resident tick: calling it unmodified from inside the tick loop would open a
second, independent SQLite connection pool alongside the daemon's own on every tick,
sharing none of the daemon's warm ANN/embedder state and none of its connection-pool
lifecycle management. The daemon already holds a live dispatcher (`D: DaemonDispatch`,
concretely `KhiveMcpServer` in the shipped daemon binary) that owns exactly the runtime
and registry the drain needs. Reusing it, instead of constructing a parallel one, is the
in-process refactor this ADR requires.

## Decision

Add a daemon-resident tick task that periodically invokes the existing drain logic
in-process, sharing the daemon's live runtime rather than constructing a new one, gated
so only the daemon process ever runs it, with external cron left in place as a safe,
redundant fallback.

### 1. Tick task lives in the warm daemon, spawned the same way the checkpoint task is

The schedule tick is a new background task, `schedule_tick_loop`, spawned from
`run_daemon_with_boot_guard` in `khive-runtime/src/daemon.rs`, immediately after the
existing warm-up and checkpoint-task spawns, using the same unconditional
daemon-boot block (this code path runs exactly once per live daemon process, never per
MCP client). It follows the checkpoint task's loop shape: a `tokio::time::interval`
tick and `MissedTickBehavior::Skip`.

Neither the warm-up spawn nor the checkpoint-task spawn retains a `JoinHandle`
(`run_daemon_with_boot_guard` fires both with a bare `tokio::spawn` and drops the
handle), and neither is aborted at any teardown point: `drain()` (the function that
runs between the accept-loop/shutdown-signal `select!` and socket/PID cleanup) only
awaits tasks registered through `track_background_task` (per-connection handlers), not
these two boot-time spawns. Shutdown for `run_checkpoint_task` is therefore
self-detected: it checks `Arc::strong_count(&pool) <= 1` on every tick and exits its
loop once it is the sole remaining holder of the `Arc<ConnectionPool>` the daemon
passed it (`crates/khive-db/src/checkpoint.rs`).

`schedule_tick_loop` does not use a strong-count floor. As the Context section above
notes, `run_checkpoint_task`'s own `<= 1` check already undercounts its production
ownership graph (it never accounts for the pool clone parked inside the `event_store`
it also holds), so copying that mechanism onto a second self-terminating consumer would
add a second undercounted check on top of one that does not correctly terminate for the
task it was modeled on. Decision point 1a describes the shutdown mechanism this ADR
uses instead. Fixing `run_checkpoint_task`'s own undercount is separate follow-on work,
out of scope here.

### 1a. Shutdown: explicit cancellation, not strong-count self-termination

`schedule_tick_loop` is signalled to stop rather than inferring shutdown from a
reference count. `run_daemon_with_boot_guard` creates a `tokio::sync::watch::channel`
before spawning the warm-up, checkpoint, and tick tasks, and holds the sender for the
remainder of the function's scope. `schedule_tick_loop` is given a clone of the
receiver and `tokio::select!`s between the `tokio::time::interval` tick and a change on
the watch channel, exiting its loop as soon as the channel reports a change (an
explicit shutdown signal) or a closed sender (the daemon function returning without
signalling, e.g. an early error path). The daemon's shutdown sequence, which already
runs `sigterm`/`sigint` detection into a single `shutdown` future ahead of `drain()`
(`crates/khive-runtime/src/daemon.rs`, the `run_daemon_with_boot_guard` accept-loop
`select!`), sends on the watch channel as its first step once that future resolves, and
then proceeds to `drain()` as it does today. Because the sender lives in
`run_daemon_with_boot_guard`'s own scope, both an explicit send and the ordinary drop
at function return are sufficient to signal every receiver, so no separate "did we
remember to signal" bookkeeping is required for a clean exit path.

`schedule_tick_loop` itself, not a separately tracked `JoinHandle`, is the future passed
to the existing `track_background_task` helper (`crates/khive-runtime/src/daemon.rs`)
at spawn time, exactly as pack handlers already register fire-and-forget work today.
This gives the tick loop the same shutdown-visibility guarantee `track_background_task`
already provides, which is bounded rather than unconditional: `drain()` (called
immediately after the accept-loop/shutdown-signal `select!` resolves) waits for tracked
futures up to `KHIVE_DRAIN_TIMEOUT_SECS` (default 10 seconds), then logs a warning and
returns with any still-busy future outstanding
(`crates/khive-runtime/src/daemon.rs`, `drain_timeout`). An idle tick always exits
cleanly, because the watch channel resolves its `select!` immediately. A pass still
processing a large backlog when the drain budget expires can be cut off by process
teardown. That bounded outcome is acceptable because every drain pass is already
crash-tolerant: each event's fire is finalized individually, and rows stranded in the
`firing` state by an interrupted pass are reconciled by `reclaim_stale_firing_events`
on a subsequent drain according to their durable receipt state (Amendment F). A claim
that expired before invocation is retryable; an invocation with no proven outcome fails
closed rather than being replayed. The executor relies on that recovery path; this ADR does not
promise pass completion under shutdown, only prompt exit when idle and recoverability
when interrupted. No additional field or accessor is added to `DaemonDispatch`
beyond the existing `pool_for_checkpoint`, which `schedule_tick_loop` still uses to
obtain the `Arc<ConnectionPool>` it drains against; the change from the earlier
revision is the shutdown signal, not the pool wiring. A dispatcher whose
`pool_for_checkpoint` returns `None` (an in-memory or test dispatcher with no
persistent schedule store to drain) does not get a `schedule_tick_loop` spawned at all,
mirroring the existing `if let Some(pool) = ...` guard around the checkpoint-task spawn;
the daemon logs one warn-level line at boot noting the tick was skipped for that reason.

This ADR does not redesign the existing checkpoint task's shutdown; `run_checkpoint_task`
keeps its current `Arc::strong_count` check unchanged, and the undercount described
above is noted here only as further motivation for why the new executor is not built
the same way, not as a change this ADR makes to the checkpoint task itself.

### 2. Executor seam: a fallible trait method, `DrainSummary`/`DrainError` in `khive-runtime`

`run_daemon_with_boot_guard` only ever calls through the generic `D: DaemonDispatch`
bound (`crates/khive-runtime/src/daemon.rs`); it has no dependency on `khive-mcp` or
`kkernel` and this ADR does not add one. The seam is therefore a new method on
`DaemonDispatch` itself, alongside the existing `pool_for_checkpoint` /
`event_store_for_checkpoint` hooks:

```rust
async fn drain_pending_events(&self) -> Result<DrainSummary, DrainError>;
```

`DrainSummary` moves from `crates/kkernel/src/pending_events.rs` into
`khive-runtime/src/daemon.rs`, defined alongside `DaemonDispatch` itself, carrying all
seven fields the existing type already has today
(`crates/kkernel/src/pending_events.rs`): `scanned`, `fired`, `advanced`, `failed`,
`skipped_not_due`, `skipped_race`, and `reclaimed`. `DrainError` is a new, equally
runtime-owned error type defined in the same module; a newtype over `String` is
sufficient for v1 (no variant structure is required yet, so this ADR does not introduce
one). Moving both types into `khive-runtime` lets the trait name its own return type
without `khive-runtime` depending on `khive-mcp` or `kkernel` for either type, and
without either downstream crate depending on the other for them.

`schedule_tick_loop` calls `drain_pending_events` through the trait; it never
references `khive-mcp` or `kkernel` types directly. The contract distinguishes two
failure classes:

- **Drain infrastructure failures**: every error the current implementation
  propagates with `?` at the drain level rather than recording per event. That set is,
  today: `reclaim_stale_firing_events` (the stale-firing reclaim sweep),
  `discover_pending_namespaces` (namespace discovery), a `query_notes_filtered` page
  read failing while scanning a namespace, and pagination-offset overflow while
  advancing through pages (all in `crates/kkernel/src/pending_events.rs`).
  `KhiveMcpServer`'s implementation maps any of these into `DrainError`, and the whole
  call returns `Err` for that pass. The classification rule for future changes is
  positional, not a fixed list: an error the drain orchestration propagates instead of
  handling per event is a `DrainError`.
- **Per-event dispatch failures**: a single event's `dispatch_action` or
  `finalize_fired_event` failing. These are not infrastructure failures: they continue
  to accumulate in the returned `DrainSummary.failed` counter exactly as they do today,
  and do not turn the call into an `Err`.

`KhiveMcpServer` (`crates/khive-mcp`) implements the method. The drain's internal
functions (`discover_pending_namespaces`, `claim_pending_event`, `dispatch_action`,
`finalize_fired_event`, `reclaim_stale_firing_events`, and the `run_pending_events`
orchestration that calls them) move from `crates/kkernel/src/pending_events.rs` into
`khive-mcp`, adjacent to `dispatch_request_local` (`crates/khive-mcp/src/server.rs`),
which `dispatch_action` already requires to replay a stored op through the live
registry. `kkernel exec --pending-events` becomes a thin CLI wrapper: it constructs its
`RuntimeConfig` / `KhiveRuntime` / `KhiveMcpServer` exactly as it does today (a
CLI-owned, one-shot construction, unchanged), then calls
`server.drain_pending_events()`, the same method the daemon tick calls on its own
long-lived server, and continues to propagate a returned `DrainError` with `.await?`
before printing the summary (`crates/kkernel/src/exec.rs`), exactly its current
`.await?` behavior against `Result<DrainSummary>` today: a one-shot CLI invocation that
hits a drain infrastructure failure still exits non-zero and prints nothing. CLI
behavior and output are unchanged; only the drain logic's home crate and the fallible
type's home crate move.

The daemon tick's handling of `Err(DrainError)` is new behavior this ADR adds:
`schedule_tick_loop` logs the error at `warn` level, naming the rejected drain pass,
and continues to its next tick rather than exiting the loop or propagating the error
further. A transient drain infrastructure failure (for example, one bad SQL
round-trip during namespace discovery) must not kill the tick loop for the daemon's
whole remaining lifetime; that is a behavior the one-shot CLI wrapper does not share,
since a CLI invocation is a fresh process per drain and has no "next tick" to continue
to.

Dependency direction is unaffected by this move: `khive-runtime` gains only the new
trait method signature on `DaemonDispatch` plus the two new types (`DrainSummary`,
`DrainError`) it now owns, no new crate dependency (it depends on `khive-db` and
`khive-storage` today, not on `khive-mcp` or `khive-request`). `khive-mcp` already
depends on `khive-runtime` and `khive-request` (`crates/khive-mcp/Cargo.toml`), so
implementing the trait, hosting the drain functions, and constructing `DrainSummary` /
`DrainError` there introduces no new edge. `kkernel` already depends on both
`khive-runtime` and `khive-mcp` (`crates/kkernel/Cargo.toml`), so its thin wrapper
continues to compile unchanged, now matching on a `khive-runtime`-owned `Result` type
instead of the `kkernel`-owned one it matched on before. No cycle is introduced in
either direction.

### 3. In-process refactor of the drain, not a subprocess shell-out

Two options were available for how the tick invokes the drain: (a) move the drain
functions into `khive-mcp` behind the `drain_pending_events` trait method described in
Decision point 2, so both the daemon tick and the CLI call the same in-process
implementation against a live `KhiveMcpServer`, or (b) leave the drain's CLI-only
signature untouched and have the tick task shell out to `kkernel exec
--pending-events` as a subprocess on each interval.

This ADR decides (a). A subprocess-per-tick design means paying process-spawn
overhead every interval, forces the subprocess to reopen its own connection pool
against the same database the daemon already holds open, and gets none of the daemon's
warm state. The in-process refactor is more work up front (the drain's internal
functions currently assume they own their `KhiveRuntime`/`KhiveMcpServer`) but shares
the daemon's live registry and connection pool, matching the same reuse ADR-049 already
established for every other daemon-resident operation. The drain's CLI entry point
(`kkernel exec --pending-events`) keeps its current signature and behavior; the refactor
is additive (a new code path, the daemon tick, shares the underlying
claim/dispatch/finalize logic through the moved implementation), not a breaking change
to the CLI.

### 4. `is_daemon_role` gating

`schedule_tick_loop` is spawned only from the daemon boot path
(`run_daemon_with_boot_guard`), which by construction runs once per live `khived`
process and never as part of a per-client stdio `kkernel mcp` session. This mirrors the
`is_daemon_role` gate already enforced for the email-channel loops in
`khive-mcp/src/serve.rs`, for the same reason: an MCP client process spawned per Claude
Code session (or per agent) must never independently start a recurring background loop
against the shared database, or every live client re-runs the same periodic work
concurrently.

### 5. External cron stays supported, and redundant invocation is safe by construction

`kkernel exec --pending-events` is not removed or deprecated by this ADR. An operator
who has cron invoking it continues to work correctly with the daemon tick running at
the same time: the drain's claim step is a `pending → firing` conditional `UPDATE ...
WHERE status = 'pending'`. Two concurrent callers, the daemon tick and an external cron
invocation, racing the same row resolve cleanly: exactly one claims it, the other's
conditional update affects zero rows and it moves on. The underlying CAS mechanism is
exercised by the existing regression suite (`fire_claim_wins_race_against_concurrent_cancel`
and the stale-claimant tests), which cover fire-claim-versus-cancel and stale-finalize-
after-reclaim respectively; neither exercises two concurrent drain callers racing the
same row, which is why Acceptance Criterion 2 requires a new test for that specific
case. No additional locking or coordination between the tick and external cron is
required or added.

### 6. Interval: configurable, default 60 seconds

The accepted and shipped interval contract is the environment-only
`KHIVE_SCHEDULE_TICK_SECS`, expressed in seconds, with no `khive.toml` key. A positive
`u64` value is used directly; an unset, empty, unparseable, negative, overflowing, or
zero value falls back **silently** to `60` seconds. The resolver does not emit a warning
for a rejected value. The 60-second default matches the cadence the drain's own module
documentation already recommends for cron-based invocation
(`* * * * * kkernel exec --pending-events`), keeping scheduled-event latency in the
same ballpark operators would get from a standard cron minute-tick, without requiring
cron to be configured at all in a daemon-fronted deployment.

**Implementation evidence.** The constant and exact parse/filter/fallback chain are in
`crates/khive-mcp/src/pending_events.rs:1237-1252`; daemon-role and loaded-pack gating
resolve that duration and spawn the loop in `crates/khive-mcp/src/serve.rs:240-265`.

**Residual work.** No direct unit regression currently pins the environment resolver's
valid, zero, and invalid cases. The larger `DaemonDispatch` seam and tracked
watch-channel shutdown design remain unimplemented residuals documented under Amendment
B (Acceptance Criteria 5-7); this interval-contract correction does not imply that they
shipped.

### 7. Original repeat-advance semantics (cron clause superseded by Amendment F)

Named aliases (`daily` / `weekly` / `monthly`) continue to be computed from the row's own
stored `trigger_at`, not from the tick's observed `now`. Amendment A governs whether an
overdue occurrence is dispatched or skipped, and Amendment F supersedes this section's
original cron clause: five-field cron is now rejected at intent creation because the
executor cannot advance it safely.

## Acceptance Criteria

1. Starting the warm daemon and letting one tick interval elapse fires every due
   `scheduled_event` row: `status` transitions to `fired` (or back to `pending` with an
   advanced `trigger_at` for a repeating event), and `fired_at` is set.
2. A concurrent external `kkernel exec --pending-events` invocation racing the daemon
   tick against the same row results in exactly one fire, never zero and never two,
   verified by a new concurrent-drain regression test alongside the existing CAS race
   tests co-located with the moved drain logic in `khive-mcp`.
3. No MCP client process (a stdio `kkernel mcp` session without `--daemon`) spawns a
   schedule tick, verified the same way the existing `is_daemon_role_false_for_client_args`
   /`is_daemon_role_true_for_daemon_args` tests verify the email-channel gate.
4. The tick interval is overridable via `KHIVE_SCHEDULE_TICK_SECS` (seconds) and defaults
   to 60 seconds when unset, unparseable, or zero. (Amended 2026-07-09, PR #782:
   the shipped implementation uses this name and unit — see Amendment
   B — and this criterion is restated to name the accepted contract rather than the
   originally-proposed `KHIVE_SCHEDULE_TICK_INTERVAL_MS`/milliseconds form.)
5. A production-shaped shutdown regression, built against `KhiveMcpServer` (the real
   dispatcher, not a mock), demonstrates that stopping the daemon signals the watch
   channel, the tick loop's `select!` observes the signal while idle and exits
   promptly, and `drain()` observes the tick loop's tracked future complete before
   returning; a dispatcher with no checkpoint pool never spawns the tick. A companion
   case covers the in-flight boundary: with a drain pass deliberately held busy past a
   short `KHIVE_DRAIN_TIMEOUT_SECS`, `drain()` returns after logging the forced-shutdown
   warning rather than hanging, and a subsequent drain recovers any row left in the
   `firing` state via `reclaim_stale_firing_events`.
6. `kkernel exec --pending-events` continues to work unchanged as a standalone,
   cron-invocable one-shot drain, now implemented as a thin wrapper calling
   `DaemonDispatch::drain_pending_events` on a CLI-constructed `KhiveMcpServer`.
7. `khive-runtime` compiles with no new crate dependency after the `DaemonDispatch`
   trait gains `drain_pending_events`; `cargo tree -p khive-runtime` shows no edge to
   `khive-mcp`, `khive-request`, or `kkernel`.

## Alternatives Considered

1. **Subprocess shell-out per tick** (`schedule_tick_loop` spawns `kkernel exec
   --pending-events` as a child process on each interval). Rejected: pays process-spawn
   cost every interval, opens a second connection pool against the same database the
   daemon already holds warm, and shares none of the daemon's warm ANN/embedder state.
   Simpler to implement than the in-process refactor, but strictly worse resource
   behavior for no correctness benefit: the CAS claim makes concurrent access safe
   regardless of whether the second caller is in-process or a subprocess.
2. **Rely on external cron only, ship no daemon tick.** Rejected as the primary
   mechanism: it requires every operator to separately provision a cron entry (or
   equivalent scheduler) outside khive itself, which is an easy step to miss and leaves
   scheduled events silently stuck with no in-product signal. External cron remains
   supported as a redundant fallback (Decision point 5), not the sole mechanism.
3. **Gate the tick behind `serve.rs`'s `spawn_email_channel_loops_if_daemon` call site
   instead of `daemon.rs`.** Both entry points converge on `run_daemon_with_boot_guard`
   for `--daemon` mode, so either location is defensible. `daemon.rs` was chosen because
   it is the single point every daemon boot path reaches, keeping the schedule tick
   alongside the checkpoint task it is directly modeled on rather than splitting
   daemon-resident periodic tasks across two files.
4. **Fixed, non-configurable interval.** Rejected: the checkpoint task's own interval is
   already environment-configurable (`KHIVE_CHECKPOINT_INTERVAL_MS`), and different
   deployments have different latency tolerances for scheduled-event delivery. A fixed
   interval would force a rebuild to retune.

## Explicitly Deferred

Reminder delivery and structured reminder-failure observability were implemented as
follow-on work and are now part of the accepted contract; see Amendment C.

The following identified gaps remain out of scope for this ADR:

- **`agenda()` visibility into non-pending state.** `schedule.agenda` filters to
  `status = "pending"` only and does not distinguish an overdue-but-undrained row from
  a genuinely future one. Extending `agenda` (or adding a history-style query) is
  separate follow-on work.
- **Event-plane telemetry for drain passes.** Wiring drain-pass observability into the
  event plane is separate follow-on work and does not require any change to the
  drain's execution logic itself.
- **Five-field cron repeat advancement** was resolved by Amendment F through explicit
  write-time rejection; it is no longer deferred.

## Consequences

- A `scheduled_event` created via `schedule.remind` or `schedule.schedule` fires within
  one tick interval of its `trigger_at` in any deployment running the warm daemon, with
  no separate cron provisioning required.
- The original ADR added an invocation path without rewriting the drain. Later amendments,
  especially A, E, and F, intentionally refined missed-event, identity, receipt, lease, and
  recovery semantics; those amendments are the current contract.
- External cron invocation of `kkernel exec --pending-events` remains a supported,
  safe-to-run-redundantly fallback, at zero additional design cost beyond the CAS claim
  the drain already has.
- A new environment-configurable interval knob is introduced for the schedule tick,
  following the same override pattern already used for the checkpoint task's interval.

## Amendment A: Missed-event grace policy (2026-07-09)

The drain, as originally specified above, fires any `scheduled_event` row it finds
`pending` with `trigger_at <= now`, regardless of how overdue it is. Decision point 7
calls this out as a feature, not an oversight: "a daemon that was down for an hour
simply fires everything overdue on its first tick after restart." That behavior is
correct for a short outage. It is the wrong behavior for a long one, or for a fresh
deployment's first tick against a database that already carries an accumulated backlog
of undrained rows: every one of those rows would fire in a single pass, including rows
whose action has an externally visible, agent-facing side effect (an outbound
`comm.send`, a spawned action, and similar). Firing a large stale backlog all at once is
a mass-notification / mass-side-effect incident waiting to happen, not a recovery.

### Policy

An event is **missed** when the drain discovers it `pending` and overdue by more than a
configurable grace window, `KHIVE_FIRE_GRACE_SECS` (default `300`, five minutes).

- A missed event is **never dispatched**. Its stored action is not replayed, regardless
  of `event_type`.
- A missed, **non-repeating** event is marked terminal: `status` transitions to
  `"missed"`, `missed_at` is stamped (epoch microseconds, the same unit `firing_at` and
  the drain's other internal timestamps already use), and `fired_at` is left `null` —
  the row was never fired, so `fired_at` must not claim otherwise.
- A missed **repeating** event is not left terminal: the drain advances its
  `trigger_at` past every occurrence at or before `now` in one step, landing on the
  first occurrence strictly after `now`, and re-arms the row to `status = "pending"` at
  that new `trigger_at`. `missed_at` is still stamped, recording that at least one
  occurrence was skipped. The event never fires a catch-up burst — a daily reminder
  that accumulated ten missed occurrences advances directly to tomorrow's, not through
  ten sequential fires.
- A row overdue by less than the grace window is unaffected: it fires (or advances,
  for a repeat) exactly as specified in the base ADR, with no behavior change.

The practical consequence for a first daemon boot against a store carrying a large
stale backlog (every row overdue well past the grace window): every such row is marked
`"missed"` (or re-armed, for repeats) on the first tick, and zero of them are
dispatched. This is the intended migration behavior, not a bug — it is exactly the
scenario the policy exists to guard against.

### Why skip-and-mark, not catch-up-once or fire-everything

Prior art disagrees on missed-fire handling, and the disagreement tracks what kind of
side effect a missed action typically has:

- **systemd** (`Persistent=true` on a timer unit) catches up **once**: if the system was
  off past a timer's scheduled run, the unit fires a single time on the next boot,
  collapsing any number of missed occurrences into one. This is close to khive's
  repeat-rearm behavior in spirit (no burst), but systemd's model still fires the
  action — it assumes the missed unit's side effect is idempotent-ish or safe to run
  late (a backup job, a log rotation). khive's action space is a replayed `VerbRegistry`
  call, and nothing in the schedule pack constrains that call to be side-effect-free or
  idempotent; a `comm.send` dispatched hours or days late is not equivalent to on-time
  delivery, it is a surprise.
- **Quartz** exposes per-trigger misfire instructions (`fire now`, `do nothing`, `reset
  to next fire time`), pushing the decision to the scheduling caller on a per-schedule
  basis. This is a real, more flexible design point khive does not adopt for v1: it
  would require a new field on `scheduled_event` (a per-row misfire policy) and a
  corresponding write-time API surface on `schedule.schedule`/`schedule.remind`,
  neither of which exists today. A single global grace window is the smaller, additive
  change; a per-row override is a natural extension if a real use case demands it,
  tracked as follow-on work rather than blocking this ADR.
- **Sidekiq** (the general background-job-queue precedent, not schedule-specific) fires
  everything it finds due, in full, with no missed-fire concept at all — the job queue
  model assumes catching up on a backlog of independent jobs is exactly the desired
  behavior. That assumption is correct for typical queued work (each job is
  independent, idempotent-by-convention, and "eventually processed" is the contract)
  and wrong for khive's schedule pack, where a `scheduled_event` models a specific
  point in time the caller cared about (a reminder due today, a briefing due at 9am),
  not a work item that is equally valid whenever it happens to run.

khive chooses **skip-and-mark** over both: never replay a stale action (ruling out
Sidekiq's fire-everything and softening systemd's fire-once), but never lose the
schedule either (a repeat re-arms to its next real occurrence rather than being
abandoned, and a non-repeat's miss is visibly recorded via `status = "missed"` +
`missed_at` rather than silently vanishing). The deciding factor is that khive's action
space is closed but unconstrained in side-effect shape (Decision point 3 of the base
ADR: "replay a validated khive verb DSL string," which can be a `comm.send`, a
`create`, or any other registered verb) — the drain cannot assume any given action is
safe to fire late, so the only universally safe choice is to never fire late at all, and
instead make the miss observable and, for repeats, self-healing at the next real
occurrence.

### Interaction with the rest of this ADR

The missed-event check runs inside the existing claim/finalize CAS
(`claim_pending_event` / `finalize_fired_event`), not as a separate pass: a row is
still claimed `pending -> firing` before its missed-vs-fire disposition is decided, so
the same race protection against a concurrent `schedule.cancel` (Decision point 5's CAS
argument) and the same redundant-external-cron safety apply identically to the missed
path. No new claim mechanism was introduced. The `DrainSummary` type gains one new
field, `missed: Vec<Uuid>` (the IDs marked missed or re-armed this pass), alongside the
seven fields already specified in Decision point 2.

## Amendment B: Implementation note — the wiring seam actually shipped (2026-07-09)

Decision points 1-3 above specify a fairly involved target design for the tick's home
and lifecycle: a `DaemonDispatch::drain_pending_events` trait method with `DrainSummary`
/ `DrainError` types owned by `khive-runtime`, `schedule_tick_loop` spawned from
`run_daemon_with_boot_guard` in `khive-runtime/src/daemon.rs`, and an explicit
`tokio::sync::watch`-channel shutdown signal integrated with the daemon's existing
`track_background_task` bounded-drain shutdown sequence.

The implementation landing alongside this amendment takes a smaller step toward that
target rather than the full design in one PR, to keep the missed-event policy (Amendment
A, the change with the more immediate safety payoff) decoupled from a `DaemonDispatch`
trait-signature change that every implementor of that trait would need to absorb in the
same PR:

- The drain's internal functions (`claim_pending_event`, `dispatch_action`,
  `finalize_fired_event`, `reclaim_stale_firing_events`, `discover_pending_namespaces`,
  and the `run_pending_events` orchestration) moved from `crates/kkernel` into
  `crates/khive-mcp` (`khive_mcp::pending_events`), matching Decision point 2's target
  home for the drain logic itself. `kkernel exec --pending-events` now calls
  `khive_mcp::pending_events::run_pending_events` directly rather than a `kkernel`-local
  copy — this part of Decision point 2 is delivered as specified.
- `schedule_tick_loop` is spawned from `khive-mcp/src/serve.rs`
  (`spawn_schedule_tick_loop_if_daemon`), gated on `args.daemon` exactly the way
  `spawn_email_channel_loops_if_daemon` already gates the email-channel loops (Decision
  point 4's `is_daemon_role` gating is delivered as specified; only the _file_ differs
  from Decision point 1's `daemon.rs` target).
- The tick does **not** go through a `DaemonDispatch::drain_pending_events` trait
  method. `khive-runtime` gains no new trait method and no new `DrainSummary`/
  `DrainError` types; `DrainSummary` is owned by `khive-mcp` — it moved there from
  `crates/kkernel` as part of the same relocation described above, not something
  `khive-mcp` already owned before this ADR — and no separate `DrainError` type exists
  at all (per-event failures accumulate in `DrainSummary.failed` instead).
- Shutdown is a bare `tokio::spawn` with no `track_background_task` registration and no
  watch-channel signal, matching how the checkpoint task and the email-channel loops are
  already spawned in the current codebase (neither uses `track_background_task` today
  either). A tick in flight at process shutdown is simply dropped, not drained; the next
  daemon start (or a redundant external cron invocation) picks up any row left
  mid-claim via the existing `reclaim_stale_firing_events` sweep, the same recovery
  path Acceptance Criterion 5 relies on for the target design's bounded-drain case.
- The interval env var is `KHIVE_SCHEDULE_TICK_SECS` (seconds, default `60`), not the
  originally proposed `KHIVE_SCHEDULE_TICK_INTERVAL_MS` (milliseconds, default `60000`).
  Invalid and zero values fall back silently. Decision point 6 and Acceptance Criterion
  4 now name this shipped contract directly; this bullet remains as the historical
  implementation-amendment record.

### Amendment B, update 1 (2026-07-09)

The initial cut of this amendment (above) additionally claimed the tick "constructs its
own short-lived `KhiveRuntime` against the daemon's configured `db`/`namespace`... rather
than sharing the live daemon's warm runtime," and framed that as a resource-cost-only
deviation. That claim was incorrect: a tick that independently re-resolves
`RuntimeConfig::default()` from raw
`--db` and an inferred namespace does not merely reconstruct the _same_ configuration at
extra cost — it silently **discards** everything the daemon's own boot path
(`khive-mcp::serve::build_server` / `build_registry_for_multi_backend`) resolves from
`--config`/`[[backends]]`/actor identity/`--pack` selection. A config-backed daemon's
tick could therefore drain `$HOME/.khive/khive.db` instead of the configured schedule
backend, trip strict-actor-mode failures the live server never has, or dispatch stored
actions through packs the daemon never loaded. PR #782 corrected the runtime target before merge:

- `build_server` now returns, alongside the server, the resolved `"schedule"`-pack
  `KhiveRuntime` handle it already constructed while building the server itself
  (`Option<KhiveRuntime>` — `None` when the resolved pack set excludes `"schedule"`).
  For a single-backend boot this is the one runtime the whole daemon shares; for a
  multi-backend boot (ADR-028 `[[backends]]`) it is the specific per-pack runtime
  `"schedule"` was wired to, read out of `MultiBackendRegistry.per_pack_runtimes`. The
  coordinator-attached multi-backend path (`kkernel`'s `Command::Mcp` branch) resolves
  and threads the same handle through `serve_server`.
- `schedule_tick_loop` takes that runtime by value (`KhiveRuntime::clone()` is a cheap
  `Arc`-wrapped clone) instead of `db: Option<String>, namespace: String`, and every tick
  drains through it via a new `run_pending_events_on(rt: &KhiveRuntime, ...)` entry point.
  `run_pending_events` (the CLI one-shot path, `kkernel exec --pending-events`) is
  unchanged — it still resolves its own throwaway config per invocation, which remains
  correct for a short-lived cron-invoked process — and now delegates to
  `run_pending_events_on` internally.
- This also resolves the resource-cost concern the original text raised (a fresh
  connection-pool warm-up every tick): the tick now reuses the daemon's already-warm
  runtime and connection pool rather than constructing a new one per pass.
- Tick cadence was also incorrect and was corrected in this update: `schedule_tick_loop` now ticks on
  `tokio::time::interval_at(now + interval, interval)` with
  `MissedTickBehavior::Skip`, matching Decision point 6's fixed-interval specification,
  rather than sleeping `interval` after each drain (which had produced an effective
  cadence of `interval + drain_duration`, drifting further behind on every pass that
  found a nontrivial backlog).
- The drain's own pagination was independently found to skip rows once an overdue
  backlog exceeded one page (`PAGE_SIZE = 200`): paging `status="pending"` with
  `LIMIT/OFFSET` while the same loop mutates rows out of that predicate desynchronizes
  the offset from the shrinking result set. Fixed by snapshotting every candidate row for
  a namespace before any mutation begins, then processing the fixed-size snapshot with no
  further paginated queries. A regression with 201 overdue rows (`PAGE_SIZE + 1`) covers
  this. **Superseded in update 2 below** — the full-namespace snapshot this update
  introduced turned out to have its own unbounded-memory failure mode.
- A new concurrent-drain regression (two `run_pending_events` calls racing over the same
  store, asserting exactly one fire per row across both) was added alongside the existing
  CAS-race unit tests, closing the exact gap Acceptance Criterion 2 names. **Strengthened
  in update 2 below** — this version dispatched a read-only `stats()` action, which cannot
  distinguish a clean single fire from a double-dispatch-one-finalize race.

None of this changes Acceptance Criteria 1, 3, 4, or 6's _scope_ — but their status
against the shipped implementation is restated here precisely, since the original
"None of this changes Acceptance Criteria 1, 2, 4, or 6... all four hold" claim below
(now superseded) conflated "unchanged in scope" with "met," which was not accurate for
Criterion 6:

- **Criterion 1** (a due row fires within one tick interval): **met**. Unaffected by this
  update beyond the cadence and runtime-targeting corrections above, which strengthen
  rather than weaken it.
- **Criterion 2** (concurrent cron + tick invocations race to exactly one fire): **met**,
  now backed by the concurrent-drain regression added in this update. The prior claim relied
  on the CAS design alone, with no regression exercising concurrency. The regression was strengthened in
  update 2 below to assert on the action's own side effect, not just the CAS-tracked
  counters.
- **Criterion 3** (no stdio client spawns a tick): **met**, unchanged — the gate is on
  `args.daemon` regardless of which file spawns the loop.
- **Criterion 4** (interval configurable, 60s default): **met**, under the shipped
  `KHIVE_SCHEDULE_TICK_SECS` contract the criterion text above was amended to name.
- **Criterion 5** (production-shaped watch-channel shutdown regression): **not met**.
  There is no watch-channel shutdown to test; a tick in flight at process shutdown is
  still simply dropped and its durable state is reconciled by the next
  `reclaim_stale_firing_events` sweep under Amendment F.
- **Criterion 6** (`kkernel exec --pending-events` implemented as a thin wrapper over
  `DaemonDispatch::drain_pending_events`): **not met**. The CLI path calls
  `khive_mcp::pending_events::run_pending_events` directly, not a `DaemonDispatch` trait
  method; no such trait method exists. The original cut of this amendment incorrectly listed
  the criterion as met despite its own bullet stating "the tick does **not** go through a
  `DaemonDispatch::drain_pending_events` trait method" (PR #782).
- **Criterion 7** (`khive-runtime` gains no new dependency after `DaemonDispatch` gains
  `drain_pending_events`): **not met** — vacuously, since no such trait method was added.
  `cargo tree -p khive-runtime` showing no edge to `khive-mcp`/`khive-request`/`kkernel`
  remains true today, but for a different reason than the criterion describes (nothing
  was added to `khive-runtime` at all, rather than something being added safely).

### Amendment B, update 2 (2026-07-09)

Verification confirmed the update-1 fixes above were present and gates green, but
the runtime-policy fix (`build_server` threading the resolved `"schedule"`-pack
runtime through to the tick) was still **incomplete** for multi-backend deployments,
along with two narrower regression/resource issues:

- **Dispatch still used the wrong runtime for multi-backend actions.** Update 1 fixed
  _scanning_ — `run_pending_events_on` now reads `scheduled_event` rows from the
  daemon's own resolved `"schedule"`-pack runtime, correct for multi-backend boots
  where `schedule` is wired to its own declared backend. But the same function then
  built its action-dispatch server from that runtime alone
  (`KhiveMcpServer::new(rt.clone())`), which registers **every** pack against the
  schedule backend. A replayed action belonging to another pack — `comm.send`, or any
  `kg` verb — therefore dispatched into the schedule backend instead of that pack's
  own configured one in a multi-backend deployment: scanning was correct, dispatch was
  not. Fixed by threading the daemon's actual, already-multi-backend-wired
  `KhiveMcpServer` through to the tick as a second parameter, alongside the schedule
  runtime: `schedule_tick_loop(rt, server, interval)` and
  `run_pending_events_on(rt, server, verbose)` now take both — `rt` for the
  scan/claim/finalize SQL (schedule's own backend), `server` (cloned — cheap,
  `Arc`-wrapped internally) for `dispatch_action` only (the daemon's live, fully-wired
  registry). `spawn_schedule_tick_loop_if_daemon` clones the same `server` it is about
  to hand to the transport/daemon bind, so replayed-action routing is identical to a
  live request against this daemon. `run_pending_events` (the CLI one-shot path) now
  also resolves both through `khive-mcp::serve::build_server` rather than a throwaway
  `RuntimeConfig::default()` — a further honesty fix, since the CLI path previously
  never consulted `khive.toml` at all (`[[backends]]`, `[actor] id`) despite
  `kkernel mcp --daemon` and `kkernel exec`'s own ordinary-ops path both doing so. A new
  regression (`build_server_schedule_tick_dispatches_actions_through_the_declared_multi_backend_not_schedule`)
  declares `schedule` on `"main"` and `kg` on a separate backend, schedules a due event
  whose action is `create(kind="observation", ...)` (a `kg` verb), drains via
  `run_pending_events_on(&rt, &server, false)`, and asserts the resulting note lands
  only in `kg`'s declared backend file, never `main`.
- **The update-1 pagination fix introduced its own unbounded-memory failure mode.**
  Snapshotting every `status="pending"` row for a namespace before mutation (update 1's
  fix, above) correctly closed the offset-skip bug, but the snapshot filter checked
  only `status`, not `trigger_at` — a namespace with one due event sitting in a large
  FUTURE schedule pulled the entire future backlog into memory on every tick. Fixed by
  replacing the `NoteFilter`/`query_notes_filtered` snapshot with a raw SQL statement
  that (a) pushes `trigger_at <= now` into the `WHERE` clause directly, so future events
  are never fetched at all, and (b) pages via a `(created_at, id)` keyset cursor instead
  of `LIMIT/OFFSET` — both columns are immutable (this drain never rewrites either), so
  a row's claim/dispatch/finalize mutation between pages can never shift a later page's
  boundary (the original update-1 bug class), and at most `PAGE_SIZE` rows are held in
  memory at once, never the whole namespace. The existing 201-row backlog regression
  continues to cover the keyset-pagination-under-mutation property.
- **The concurrent-drain regression's `stats()` action was too weak to catch its own
  target bug.** A read-only action makes the CAS-tracked `status`/summary counters the
  only signal, which cannot distinguish "claimed once, dispatched once" from "claimed
  once, dispatched TWICE, only one finalize succeeded." Fixed by giving each of the 20
  rows a row-distinct `create(kind="observation", content="concurrent-drain-marker-{i}")`
  action and asserting, after both drains, that exactly one marker note exists per row —
  the double-dispatch-one-finalize regression this test exists to catch would show up as
  a marker count of 2 for the row that raced, which the counter-only version could not
  detect.
- The PR description was updated to match this shipped state (shared resolved runtime
  for scan, the daemon's real live server for dispatch, Criteria 5-7 unmet) rather than
  the pre-update "fresh per-tick runtime" / "Criterion 6 met" text it still carried.
- **Update 3 addendum (2026-07-09):** the update-2 keyset page queries and
  `discover_pending_namespaces` compared `trigger_at` as raw RFC3339 text, which sorts
  lexicographically rather than chronologically for values carrying a non-UTC offset
  (`handlers.rs` deliberately round-trips the caller's original offset, so this was
  reachable in production, not just theoretically). Fixed by wrapping both sides of the
  due-ness predicate in SQLite's `datetime(...)` (plus an `OR ... IS NULL` clause so
  unparseable `trigger_at` values stay visible to the existing Rust-side skip/log path
  instead of being silently excluded) in all three affected queries: both keyset page
  branches and `discover_pending_namespaces`. The comparison is now chronological via
  SQLite `datetime()`; storage still round-trips the caller's original string — this
  point is unchanged.
- **Update 4 addendum (2026-07-09):** the one-shot CLI path (`run_pending_events`)
  synthesized an `Args` value with `namespace: Some("local")` and called `build_server`
  directly — the same entry point real `--actor`/`--namespace` CLI flags go through.
  `build_server` derives both `namespace_explicit` and `actor_explicit` from a single
  `resolve_cli_namespace` check, which is correct for a genuine CLI parse (there is no
  way to type `--namespace` without meaning an explicit actor override) but wrong for a
  synthesized default: it made a default-resolved `"local"` namespace clear any
  project-configured `[actor] id` exactly as if the operator had typed `--actor local`,
  contradicting this section's own claim that the CLI path honors `[actor] id`, and
  could fail server construction outright under strict actor mode despite a valid
  config. Fixed by extracting `build_server`'s body (after CLI-namespace resolution)
  into a new `build_server_with_explicit_namespace(args, namespace, namespace_explicit,
  actor_explicit)` seam; `build_server` itself is unchanged (still derives both flags
  from `resolve_cli_namespace`), while `run_pending_events` now calls the new seam
  directly with `namespace_explicit: true, actor_explicit: false` — the same shape
  `kkernel exec`/`kkernel reindex` already use via their own `resolve_runtime_config`
  calls, so a `"local"`-resolved default namespace falls through to the project/db/env
  actor tiers instead of being treated as an explicit override.

None of this changes the Criteria 1-7 status table above. The defect this update closed was
scoped entirely to dispatch routing, which Criterion 2 (fire-exactly-once)
already covers; no new criterion becomes met or unmet as a result.

Closing the Criterion 5/6/7 gap — moving to the full `DaemonDispatch::drain_pending_events`
trait seam with tracked, graceful shutdown and `DrainSummary`/`DrainError` owned by
`khive-runtime` — remains open follow-on work, tracked separately from the missed-event
policy (Amendment A) and the runtime-targeting/cadence/pagination fixes (this amendment)
this ADR has delivered so far.

## Amendment C: Reminder delivery and failure observability (2026-07-12)

`schedule.remind` mirrors the creating actor in the scheduled-event row as
`created_by_actor`, but the authoritative binding is a target-bound event appended from
the dispatch token before the staged note becomes pending. When an in-grace reminder
fires, the drain derives the recipient and dispatch actor from that immutable provenance
and dispatches `comm.send`, producing an inbound message in the creator's actor-addressed
inbox. Delivery therefore remains attributed to the creator across daemon restarts and
changes in the daemon's own actor identity without trusting mutable note properties.
Rows created before immutable provenance existed are the exception: the drain ignores
any unprovenanced actor claim, logs a warning, and falls back to the current server actor,
then to `local` when the server has no configured actor.

A reminder-delivery failure is observable through the drain and persisted state. The
drain logs the error, increments `DrainSummary.failed`, and persists `delivery_error` plus
`delivery_failed_at` on the `scheduled_event` row. It also appends an error-outcome
audit event with verb `schedule.remind.fire`, the scheduled-event note as its target,
and the intended recipient actor and error text in its payload. Failure remains
per-event: it does not abort the drain or prevent later due rows from dispatching in
the same pass. Amendment F supersedes this amendment's original one-shot terminalization:
a failed one-shot returns to `pending` with its durable failure receipt and error fields,
while a named repeat remains re-armed at its next occurrence. A later successful
occurrence clears stale `delivery_error` and `delivery_failed_at` properties.

Because reminder delivery is part of the public `schedule.remind` contract, reminder
creation checks that the registry provides `comm.send`. If it does not, the handler
rejects the call before persisting a note. The schedule pack itself still declares only
`REQUIRES = ["kg"]`, so `schedule.schedule`, `schedule.agenda`, and `schedule.cancel`
remain available without `comm`. The per-event failure behavior above covers dispatch
failures after a reminder has passed that creation-time capability check.

## Amendment D: Process-local ticker liveness (2026-08-01)

The daemon schedule loop MUST expose positive liveness even when an agenda is empty. The
`KhiveMcpServer` instance owns a process-local `last_tick_at` heartbeat shared by its
clones. `schedule_tick_loop` advances it immediately after the interval yields and before
starting the drain attempt. Therefore quiet and failed drain passes both prove that the
loop ran, while a drain that wedges after starting leaves a frozen timestamp for the
caller to classify as stale.

The MCP host decorates a successful `schedule.agenda` result with:

```json
{ "ticker": { "last_tick_at": "2026-08-01T12:34:56.123456Z" } }
```

`last_tick_at` is null until that server instance observes its first tick. The heartbeat
is never written to SQLite or any other durable substrate: rebuilding the server over the
same database starts at null, so a predecessor process cannot leave behind a recent-looking
value that masquerades as current liveness. If the schedule pack is not resolved, the
`schedule.agenda` verb itself is absent. The response deliberately carries no computed
`healthy` boolean or fixed staleness threshold; callers compare the timestamp with their
own expected cadence. Standard presentation rules still apply, so callers requiring the
exact RFC 3339 value request verbose presentation.

The schedule pack's direct registry handler remains responsible only for intent data. The
decoration occurs at the MCP host boundary because that host owns the loop and is the only
layer that can truthfully report whether this process is running it.

## Amendment E: ADR-119 supervision and creator-bound replay (2026-08-01)

ADR-119 supersedes the detached-task lifecycle described in this ADR's original Decision
1/1a and closes the previously open tracked-shutdown gap. After configuration, actor,
backend routing, and pack selection resolve, daemon-role startup adds a dynamic component
named `schedule-tick` if and only if a schedule runtime exists. Its factory captures that
exact runtime and receives the already-built live `KhiveMcpServer` through `HostContext`.
The daemon roster contains one such component; client/stdio roles and pack-absent daemon
configurations contain none. External `kkernel exec --pending-events` remains supported.

The concrete process-lifetime policy is:

| Field                      | Value                                                  |
| -------------------------- | ------------------------------------------------------ |
| Restart class              | `OnFailure`                                            |
| Restart budget             | 5                                                      |
| Initial backoff            | 1 second                                               |
| Maximum backoff            | 60 seconds                                             |
| Cooperative shutdown bound | 5 seconds (also clamped inside the daemon drain bound) |

The loop selects between the interval and the component cancellation token. Each successful
drain, including an empty agenda or a drain with absorbed per-event failures, records the
ADR-119 component heartbeat. A drain-level error returns `ComponentError::Retryable`, so
the supervisor records `Degraded`, backs off, and consumes one restart. Per-event action or
reminder failures stay in `DrainSummary` and do not consume that budget. Panics, clean stop,
budget exhaustion, and cancellation follow ADR-119's generic supervisor contract.

Amendment D's agenda timestamp remains a separate, deliberately narrow signal: it records
the tick attempt before the drain, so even a failed attempt is visible to callers. ADR-119's
generic `HealthReporter` heartbeat records only successful cycles and remains process-local
operator state; this amendment adds no generic component-health verb or wire schema.

### Creator-bound replay and legacy policy

`schedule.schedule`, like `schedule.remind`, mirrors `created_by_actor` for display but
records authority in a target-bound, append-only creator-provenance event written from the
dispatch token. Creation is ordered `provisioning note -> provenance event -> pending`, so
no executable row exists before the immutable binding is durable. When a generic action
fires, the drain reconstructs the exact verified actor kind from that event and supplies it
with the event namespace to the live server. Attributed principals use `VerifiedActor`;
`anonymous:local` remains anonymous. Token minting, gate checks, audit attribution, and writes
therefore run as the creator, never as the daemon. Neither the stored DSL nor mutable note
properties can override identity. Replay also retains the public visibility gate, so a
scheduled payload cannot invoke a `Visibility::Subhandler`.

Executable `scheduled_event` content and properties are schedule-managed. Generic KG
`update` and note `merge` reject these rows (including either merge operand), so a caller
cannot replace payload, trigger, cadence, or lifecycle state while retaining another
actor's immutable provenance. `schedule.cancel` and the drain's CAS transitions remain the
only executable-state mutation paths; generic deletion can still remove a row but cannot
amend or reactivate it. Generic creation of an unprovenanced row still follows the
fail-closed policy below.

A generic scheduled-action row without immutable creator provenance fails closed: the payload is not
dispatched, the claimed row becomes terminal `status="failed"`, and the drain persists
`dispatch_error` plus `dispatch_failed_at`. This is the migration policy for rows written
before creator attribution and deliberately differs from Amendment C's reminder-only legacy
fallback: an unprovenanced reminder ignores its note actor claim and targets only the current
server actor (then `local`). The refusal receipt for an unprovenanced generic action is stamped
`anonymous:local`, because no actor was verified; the daemon fallback is reserved for genuinely
legacy reminders. Other generic dispatch failures remain per-event. Amendment F
supersedes their one-shot lifecycle: a failed one-shot remains `pending` and retryable; a
named repeat advances normally. Both persist the same error fields, and later success clears
them.

The drain also revalidates the single-operation boundary before setting a receipt to
`invoking`. A legacy action stored as a batch or chain becomes terminal `failed` with a
`not_invoked` receipt. It is never submitted to best-effort batch dispatch: otherwise one
operation could commit while a sibling returns a known failure, and retrying the occurrence
would duplicate the successful side effect.

## Amendment F: Durable dispatch receipts and renewable leases (2026-08-07)

The original claim-token CAS prevented two drains from claiming a pending row at the same
instant, but it did not make the interval between dispatch and finalization safe. A live action
running beyond the fixed stale threshold could be reclaimed, and a crash after an externally
visible side effect but before finalization could invoke that side effect again. Dispatch
failure was also conflated with successful occurrence consumption, and drain counters were
incremented before their corresponding lifecycle write committed.

### Occurrence and invocation receipt

Every new claim atomically writes a versioned `dispatch_receipt` into the schedule-managed
properties together with `status="firing"`, `firing_at`, and `lease_expires_at`:

```json
{
  "version": 1,
  "occurrence_id": "deterministic UUIDv5(event id, scheduled UTC instant)",
  "invocation_id": "fresh UUIDv4 for this attempt",
  "actor": "actor:lambda:owner",
  "state": "claimed",
  "claimed_at": 1786060800000000
}
```

Immediately before polling the action future, the claimant conditionally changes the receipt
to `state="invoking"`. When the call returns, it conditionally persists `succeeded`, `failed`,
or `indeterminate` plus `completed_at`, a human-readable `error`, and the original structured
`error_payload` when the dispatched verb returned one, still bound to both the original
`firing_at` and `invocation_id`. This preserves reconciliation fields such as an ambiguous
`comm.send` result's `details.outbound_id` instead of flattening them into a string. Only after
that durable outcome write does lifecycle
finalization clear the active lease fields. The final row retains the last receipt, so a
response loss or finalization failure remains diagnosable and recoverable. Retry attempts for
the same scheduled instant keep one `occurrence_id` and receive distinct `invocation_id`s.
Finalizations that occur before action invocation retain the claim receipt as well. Unsupported
recurrence, missing immutable provenance, and empty payload use `state="not_invoked"` with a
non-empty error; an occurrence skipped by the grace policy uses `state="missed"` and
`error=null`. Both carry `completed_at`, and neither state means that the action future began.

The receipt actor is derived from the same immutable creator provenance as replay, including
for reminders skipped by the missed-event policy. Only a genuinely legacy reminder with no
provenance uses the configured scheduler/anonymous-local fallback. A refused generic row with
no verified creator uses `anonymous:local`, never the daemon actor. The receipt is diagnostic
and never authorizes the dispatch; `VerifiedActor`, namespace injection, public visibility,
and Gate evaluation remain the authority boundary described by Amendment E.

### Renewable lease and deadline ownership

`KHIVE_SCHEDULE_LEASE_SECS` is a positive integer duration in seconds, defaulting silently to
`300` when absent, zero, or invalid. A separate task renews `lease_expires_at` every one third
of that duration through action execution and durable outcome persistence. The renewal is
independent of the action future's polling task, so a synchronously blocked handler cannot
starve its own heartbeat on a multi-thread runtime, and writer contention after action return
cannot create an unleased outcome gap. Reclaim compares the durable deadline, not the original
start time. Every recovery requeue, quarantine, or lifecycle finalization matches the exact
serialized properties snapshot selected by the recovery scan as well as re-checking the current
deadline and claim identity. Any outcome persistence, renewal, or other properties mutation after
the SELECT therefore fences the stale recovery write, including the race where an `invoking`
snapshot is followed by a durable `succeeded` outcome whose completed lease is already expired.
Rows written by older binaries without `lease_expires_at` retain the historical five-minute
`firing_at` fallback.

If renewal loss or failure is observed before the outcome write, the attempted result is
classified `indeterminate`; it is never treated as a proven success or a safe automatic retry.
Once the claim-bound outcome CAS commits, that durable receipt is authoritative: a renewal
already waiting on the writer may then observe the receipt's terminal state and stop without
changing the proven outcome. Claim and finalize continue to match the invocation id as well as
`firing_at`, so a stale claimant cannot overwrite a later attempt.

### Crash and failure recovery

Expired receipts are reconciled as follows:

| Durable state                                 | Recovery                                                                                                                         |
| --------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------- |
| `claimed`                                     | Invocation never began; atomically record `not_invoked`/claim expiry and return the same occurrence to `pending`.                |
| `invoking`                                    | Outcome is ambiguous; mark terminal `failed`/`indeterminate` and do not dispatch again.                                          |
| `succeeded`                                   | Resume lifecycle finalization without invoking the action again.                                                                 |
| `failed`                                      | Preserve the error; return a one-shot to `pending`, or advance a named repeat to its next occurrence.                            |
| `not_invoked` / `missed` on `status="firing"` | The atomic pre-invocation finalization did not complete; fail closed as `indeterminate` rather than inferring or replaying work. |
| malformed/unknown                             | Fail closed as `indeterminate`; never inherit daemon authority or replay automatically.                                          |

Recovery validates the complete typed v1 receipt before selecting one of these branches. Every
receipt needs supported `version`, parseable occurrence and invocation UUIDs, a supported actor
encoding, and integer `claimed_at` equal to the active `firing_at`. The occurrence id must equal
UUIDv5(event id, scheduled UTC instant). `invoking` additionally needs a valid
`invocation_started_at`. Terminal states need a valid `completed_at`; `succeeded` and `missed`
require `error=null`, while `failed`, `indeterminate`, and `not_invoked` require a non-empty
string error. `claimed`, `invoking`, `succeeded`, `missed`, and `not_invoked` reject a non-null
action `error_payload`; `failed` and `indeterminate` may retain one. A malformed terminal-looking receipt therefore cannot be used to mark a row fired
or retryable: it is durably quarantined as indeterminate, with the rejected receipt retained as
`invalid_receipt`, and the target action is not invoked.

This is an at-most-once safety boundary for generic non-idempotent actions, not a claim of
distributed exactly-once execution. The unavoidable crash window between the external side
effect and the local outcome write is represented explicitly as `indeterminate`; automatically
replaying that state would recreate the duplicate-side-effect bug this amendment closes.
The same classification applies when the action _returns_ an explicit ambiguous result, including
`side_effects_unknown`: the scheduler preserves the structured payload/correlation id, marks the
occurrence terminally indeterminate, and never blindly retries it. A normal per-op error remains
a known failure and follows the retry policy below.
Known action failures are different: the invocation returned a durable failure outcome, so a
one-shot remains recoverable on a later drain instead of being falsely marked fired. Operators
can still cancel a retrying pending event.

The missed-event grace policy in Amendment A is unchanged. This amendment does not add or alter
per-schedule misfire policy.

### Counters and recurrence validation

`DrainSummary` keeps its existing fields and adds `invoked`, `outcomes_persisted`, `finalized`,
`retry_pending`, and `indeterminate`. `invoked` counts action futures entered in this pass;
`outcomes_persisted` counts durable outcome writes, including an `indeterminate` classification
created during crash recovery; `finalized` counts successful lifecycle CAS writes. `fired` and
`advanced` increment only after their own finalization succeeds, so a failed write never
decrements an unrelated branch's prior count. Expiry in `claimed` increments `retry_pending` and
`finalized`, but not `failed`, `invoked`, or `outcomes_persisted`, because the receipt proves that
the target action never began. Every expired-row finalization is isolated: a storage failure for
one selected row increments `failed`, is logged with row/namespace identity, and does not prevent
later expired rows or newly due work from being processed.

Schedule creation now accepts only `daily`, `weekly`, and `monthly`. Five-field cron is rejected
with an explicit non-executable error; legacy cron rows fail closed before invocation. This
narrows a previously misleading accepted grammar without changing any recurrence the executor
could actually honor.

Deterministic regressions cover a live invocation held beyond multiple lease durations while a
second drain runs (one target-verb entry and one visible side effect), a crash after durable
success but before finalization (resume without invocation), an expired `invoking` receipt
(terminal indeterminate with no replay), failed one-shot retry with stable occurrence/distinct
invocation ids, stale-claim finalization fencing, malformed terminal receipt fields and
occurrence identity, truthful `claimed -> not_invoked -> pending` recovery, a stale recovery
snapshot losing to a newly durable success, creator-attributed missed-reminder receipts,
anonymous attribution for refused generic rows, terminal pre-invocation refusal of legacy
multi-op actions across two drains, a committed side effect followed by structured
`side_effects_unknown` across two drains (one invocation/one visible side effect), and an
injected expired-row finalization failure that does not wedge later due work.
