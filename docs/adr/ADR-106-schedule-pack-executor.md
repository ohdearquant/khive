# ADR-106: Schedule Pack Executor — Daemon-Resident Tick for the Pending-Event Drain

**Status**: Accepted
**Date**: 2026-07-09
**Amended**: 2026-07-09 (missed-event grace policy, Amendment A; implementation note, Amendment B)
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
already provides, which is bounded rather than unconditional: `drain()` (called
immediately after the accept-loop/shutdown-signal `select!` resolves) waits for tracked
futures up to `KHIVE_DRAIN_TIMEOUT_SECS` (default 10 seconds), then logs a warning and
returns with any still-busy future outstanding
(`crates/khive-runtime/src/daemon.rs`, `drain_timeout`). An idle tick always exits
cleanly, because the watch channel resolves its `select!` immediately. A pass still
processing a large backlog when the drain budget expires can be cut off by process
teardown. That bounded outcome is acceptable because every drain pass is already
crash-tolerant: each event's fire is finalized individually, and rows stranded in the
`firing` state by an interrupted pass are recovered by `reclaim_stale_firing_events`
on a subsequent drain. The executor relies on that recovery path; this ADR does not
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
4. The tick interval is overridable via `KHIVE_SCHEDULE_TICK_SECS` (seconds) and defaults
   to 60 seconds when unset, unparseable, or zero. (Amended 2026-07-09, codex PR #782
   review fix-round: the shipped implementation uses this name and unit — see Amendment
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
  `DrainError` types; those remain owned by `khive-mcp` (as `DrainSummary` already was
  before this ADR).
- Shutdown is a bare `tokio::spawn` with no `track_background_task` registration and no
  watch-channel signal, matching how the checkpoint task and the email-channel loops are
  already spawned in the current codebase (neither uses `track_background_task` today
  either). A tick in flight at process shutdown is simply dropped, not drained; the next
  daemon start (or a redundant external cron invocation) picks up any row left
  mid-claim via the existing `reclaim_stale_firing_events` sweep, the same recovery
  path Acceptance Criterion 5 relies on for the target design's bounded-drain case.
- The interval env var is `KHIVE_SCHEDULE_TICK_SECS` (seconds, default `60`), not
  `KHIVE_SCHEDULE_TICK_INTERVAL_MS` (milliseconds, default `60000`) as specified in
  Decision point 6. The resolved default cadence is identical (60 seconds); only the
  variable name and unit differ from the original decision — Acceptance Criterion 4
  above is amended to name this shipped contract directly.

### Fix-round update (2026-07-09, codex PR #782 review)

The initial cut of this amendment (above) additionally claimed the tick "constructs its
own short-lived `KhiveRuntime` against the daemon's configured `db`/`namespace`... rather
than sharing the live daemon's warm runtime," and framed that as a resource-cost-only
deviation. Codex's review of the PR carrying this ADR (#782) identified that claim as
incorrect: a tick that independently re-resolves `RuntimeConfig::default()` from raw
`--db` and an inferred namespace does not merely reconstruct the _same_ configuration at
extra cost — it silently **discards** everything the daemon's own boot path
(`khive-mcp::serve::build_server` / `build_registry_for_multi_backend`) resolves from
`--config`/`[[backends]]`/actor identity/`--pack` selection. A config-backed daemon's
tick could therefore drain `$HOME/.khive/khive.db` instead of the configured schedule
backend, trip strict-actor-mode failures the live server never has, or dispatch stored
actions through packs the daemon never loaded. This was filed as the review's High
finding and fixed in the same PR, before merge:

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
- Tick cadence was also corrected in the same fix-round (a separate, Medium-severity
  finding in the same review): `schedule_tick_loop` now ticks on
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
  this. **Superseded in round 2 below** — the full-namespace snapshot this round
  introduced turned out to have its own unbounded-memory failure mode.
- A new concurrent-drain regression (two `run_pending_events` calls racing over the same
  store, asserting exactly one fire per row across both) was added alongside the existing
  CAS-race unit tests, closing the exact gap Acceptance Criterion 2 names. **Strengthened
  in round 2 below** — this version dispatched a read-only `stats()` action, which cannot
  distinguish a clean single fire from a double-dispatch-one-finalize race.

None of this changes Acceptance Criteria 1, 3, 4, or 6's _scope_ — but their status
against the shipped implementation is restated here precisely, since the original
"None of this changes Acceptance Criteria 1, 2, 4, or 6... all four hold" claim below
(now superseded) conflated "unchanged in scope" with "met," which was not accurate for
Criterion 6:

- **Criterion 1** (a due row fires within one tick interval): **met**. Unaffected by this
  fix-round beyond the cadence and runtime-targeting corrections above, which strengthen
  rather than weaken it.
- **Criterion 2** (concurrent cron + tick invocations race to exactly one fire): **met**,
  now backed by the concurrent-drain regression added in this fix-round (previously
  claimed met on the strength of the CAS design alone, with no regression exercising
  concurrency — codex's Medium finding on this amendment), and strengthened in round 2
  below to assert on the action's own side effect, not just the CAS-tracked counters.
- **Criterion 3** (no stdio client spawns a tick): **met**, unchanged — the gate is on
  `args.daemon` regardless of which file spawns the loop.
- **Criterion 4** (interval configurable, 60s default): **met**, under the shipped
  `KHIVE_SCHEDULE_TICK_SECS` contract the criterion text above was amended to name.
- **Criterion 5** (production-shaped watch-channel shutdown regression): **not met**.
  There is no watch-channel shutdown to test; a tick in flight at process shutdown is
  still simply dropped, recovered by the next `reclaim_stale_firing_events` sweep.
- **Criterion 6** (`kkernel exec --pending-events` implemented as a thin wrapper over
  `DaemonDispatch::drain_pending_events`): **not met**. The CLI path calls
  `khive_mcp::pending_events::run_pending_events` directly, not a `DaemonDispatch` trait
  method — no such trait method exists. This criterion was incorrectly listed as met in
  the original cut of this amendment; that was a direct contradiction of this same
  amendment's own bullet stating "the tick does **not** go through a
  `DaemonDispatch::drain_pending_events` trait method," caught in the codex PR #782
  review (Medium finding).
- **Criterion 7** (`khive-runtime` gains no new dependency after `DaemonDispatch` gains
  `drain_pending_events`): **not met** — vacuously, since no such trait method was added.
  `cargo tree -p khive-runtime` showing no edge to `khive-mcp`/`khive-request`/`kkernel`
  remains true today, but for a different reason than the criterion describes (nothing
  was added to `khive-runtime` at all, rather than something being added safely).

### Fix-round 2 update (2026-07-09, codex PR #782 review round 2)

Codex's round-2 pass on this PR verified the round-1 fixes above were all present and
gates green, but found the High runtime-policy fix (`build_server` threading the
resolved `"schedule"`-pack runtime through to the tick) was still **incomplete** for
multi-backend deployments, plus two narrower regression/resource issues:

- **Dispatch still used the wrong runtime for multi-backend actions.** Round 1 fixed
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
- **The round-1 pagination fix introduced its own unbounded-memory failure mode.**
  Snapshotting every `status="pending"` row for a namespace before mutation (round 1's
  fix, above) correctly closed the offset-skip bug, but the snapshot filter checked
  only `status`, not `trigger_at` — a namespace with one due event sitting in a large
  FUTURE schedule pulled the entire future backlog into memory on every tick. Fixed by
  replacing the `NoteFilter`/`query_notes_filtered` snapshot with a raw SQL statement
  that (a) pushes `trigger_at <= now` into the `WHERE` clause directly, so future events
  are never fetched at all, and (b) pages via a `(created_at, id)` keyset cursor instead
  of `LIMIT/OFFSET` — both columns are immutable (this drain never rewrites either), so
  a row's claim/dispatch/finalize mutation between pages can never shift a later page's
  boundary (the original round-1 bug class), and at most `PAGE_SIZE` rows are held in
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
  the pre-fix-round "fresh per-tick runtime" / "Criterion 6 met" text it still carried.

None of this changes the Criteria 1-7 status table above — the High finding this round
closed was scoped entirely to dispatch routing, which Criterion 2 (fire-exactly-once)
already covers; no new criterion becomes met or unmet as a result.

Closing the Criterion 5/6/7 gap — moving to the full `DaemonDispatch::drain_pending_events`
trait seam with tracked, graceful shutdown and `DrainSummary`/`DrainError` owned by
`khive-runtime` — remains open follow-on work, tracked separately from the missed-event
policy (Amendment A) and the runtime-targeting/cadence/pagination fixes (this fix-round)
this ADR has delivered so far.
