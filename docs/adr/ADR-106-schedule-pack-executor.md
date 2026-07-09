# ADR-106: Schedule Pack Executor — Daemon-Resident Tick for the Pending-Event Drain

**Status**: Proposed
**Date**: 2026-07-09
**Depends on**: [ADR-040](ADR-040-communication-and-schedule-packs.md) (schedule pack
verbs and `scheduled_event` note kind), [ADR-049](ADR-049-khived-daemon.md) (warm daemon
process model), [ADR-016](ADR-016-request-dsl.md) (request DSL, replayed at fire time)

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
--pending-events` (`crates/kkernel/src/pending_events.rs`, entry point
`run_pending_events`). This is a complete, well-tested one-shot drain, not a stub:

- A DB-level compare-and-swap state machine moves each due row through
  `pending → firing → fired` (or back to `pending` for a repeating event), using a
  claim token (`firing_at`, an epoch-microsecond timestamp) that `finalize_fired_event`
  must match exactly before it will transition a row out of `firing`. A stale-firing
  reclaim sweep (`reclaim_stale_firing_events`, 5-minute timeout) recovers rows
  abandoned by a crashed or killed drain.
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
  `monthly`. A five-field cron expression is accepted and validated at write time but is
  not advanced by the drain today: it fires once and is marked terminal, a known,
  tracked limitation (khive issue #14), out of scope for this ADR.
- The module's own doc comment frames it plainly: "This is a cron-friendly one-shot
  drain. It is NOT a long-running daemon. Run it from cron (e.g. `* * * * * kkernel exec
  --pending-events`) to achieve minute-granularity delivery."

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
already provides: `drain()` (called immediately after the accept-loop/shutdown-signal
`select!` resolves) does not return until the tick loop's future has completed, so a
`SIGTERM` landing mid-tick cannot abort a drain pass silently the way an untracked
`tokio::spawn` could. No additional field or accessor is added to `DaemonDispatch`
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

- **Drain infrastructure failures**: the two steps the current implementation
  propagates with `?` before any per-event work happens, `reclaim_stale_firing_events`
  (`crates/kkernel/src/pending_events.rs`, the stale-firing reclaim sweep) and
  `discover_pending_namespaces` (`crates/kkernel/src/pending_events.rs`, namespace
  discovery). `KhiveMcpServer`'s implementation maps a failure in either step into
  `DrainError`, and the whole call returns `Err` for that pass.
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

The tick interval is read from a single environment variable,
`KHIVE_SCHEDULE_TICK_INTERVAL_MS`, mirroring `KHIVE_CHECKPOINT_INTERVAL_MS`
(`crates/khive-db/src/checkpoint.rs`) in shape: milliseconds, env-only for v1 (no
`khive.toml` key), default 60000 (60 seconds) when unset. An unparseable or zero value
falls back to the default, with a warn-level log naming the rejected value: a stricter
failure mode than the checkpoint precedent, which falls back silently. This ADR chooses
to log because a misconfigured schedule interval is user-facing latency,
not an internal tuning knob. The 60-second default matches the cadence the drain's own
module documentation already recommends for cron-based invocation
(`* * * * * kkernel exec --pending-events`), keeping scheduled-event latency in the
same ballpark operators would get from a standard cron minute-tick, without requiring
cron to be configured at all in a daemon-fronted deployment.

### 7. Repeat-advance semantics are unchanged

This ADR does not alter how the drain advances `trigger_at` for repeating events. Named
aliases (`daily` / `weekly` / `monthly`) continue to be computed from the row's own
stored `trigger_at`, not from the tick's observed `now`. This is what already gives the
drain correct missed-fire recovery for free: a daemon that was down for an hour simply
fires everything overdue on its first tick after restart, because discovery scans
`status = 'pending' AND trigger_at <= now` rather than a specific expected slot. Five-field
cron expressions remain validated at write time and not advanced (issue #14),
unaffected by this ADR.

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
4. The tick interval is overridable via `KHIVE_SCHEDULE_TICK_INTERVAL_MS` and defaults
   to 60000 (60 seconds) when unset, unparseable, or zero.
5. A production-shaped shutdown regression, built against `KhiveMcpServer` (the real
   dispatcher, not a mock), demonstrates that stopping the daemon signals the watch
   channel, the tick loop's `select!` observes the signal and exits promptly, and
   `drain()` observes the tick loop's tracked future complete before returning; a
   dispatcher with no checkpoint pool never spawns the tick.
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

The following are real, identified gaps in the schedule pack's execution story but are
out of scope for this ADR, which is limited to wiring the existing drain into a
daemon-resident tick:

- **Delivery of fired `schedule.remind` events.** The drain's own dispatch logic
  treats `event_type != "schedule"` as a no-op today: a fired reminder is marked
  `fired` but nothing reads its content or delivers it anywhere. Building an
  owner/actor attribution field on `scheduled_event` and wiring fired reminders to an
  inbound delivery path is separate follow-on work.
- **Structured per-row failure reason.** A dispatch failure is currently visible only
  in the drain's own aggregate summary counter and verbose logging output; the
  `scheduled_event` note itself carries no persisted failure detail. Adding a
  dispatch-error property to the row is separate follow-on work.
- **`agenda()` visibility into non-pending state.** `schedule.agenda` filters to
  `status = "pending"` only and does not distinguish an overdue-but-undrained row from
  a genuinely future one. Extending `agenda` (or adding a history-style query) is
  separate follow-on work.
- **Event-plane telemetry for drain passes.** Wiring drain-pass observability into the
  event plane is separate follow-on work and does not require any change to the
  drain's execution logic itself.
- **Five-field cron repeat advancement** (khive issue #14) is unaffected by this ADR.

## Consequences

- A `scheduled_event` created via `schedule.remind` or `schedule.schedule` fires within
  one tick interval of its `trigger_at` in any deployment running the warm daemon, with
  no separate cron provisioning required.
- The drain's core claim/dispatch/finalize/reclaim logic is unchanged; this ADR adds an
  invocation path, not a rewrite of the state machine ADR-040 and the shipped
  `pending_events.rs` already established.
- External cron invocation of `kkernel exec --pending-events` remains a supported,
  safe-to-run-redundantly fallback, at zero additional design cost beyond the CAS claim
  the drain already has.
- A new environment-configurable interval knob is introduced for the schedule tick,
  following the same override pattern already used for the checkpoint task's interval.
