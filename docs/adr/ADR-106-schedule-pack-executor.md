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
which read or write a `scheduled_event` note. Nothing in the pack itself ever
transitions a `scheduled_event` past `"pending"`.

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
rather than a separate cancellation channel.

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
tick, `MissedTickBehavior::Skip`, and a `strong_count`-based shutdown check consistent
with the existing task rather than introducing a second shutdown-signaling mechanism.

### 2. In-process refactor of the drain, not a subprocess shell-out

Two options were available for how the tick invokes the drain: (a) refactor
`run_pending_events` (and the functions it calls: `discover_pending_namespaces`,
`claim_pending_event`, `dispatch_action`, `finalize_fired_event`,
`reclaim_stale_firing_events`) to operate against a runtime/server handle supplied by
the caller instead of one it constructs itself, or (b) leave the drain's CLI-only
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
is additive (a new code path shares the underlying claim/dispatch/finalize logic), not a
breaking change to the CLI.

### 3. `is_daemon_role` gating

`schedule_tick_loop` is spawned only from the daemon boot path
(`run_daemon_with_boot_guard`), which by construction runs once per live `khived`
process and never as part of a per-client stdio `kkernel mcp` session. This mirrors the
`is_daemon_role` gate already enforced for the email-channel loops in
`khive-mcp/src/serve.rs`, for the same reason: an MCP client process spawned per Claude
Code session (or per agent) must never independently start a recurring background loop
against the shared database, or every live client re-runs the same periodic work
concurrently.

### 4. External cron stays supported, and redundant invocation is safe by construction

`kkernel exec --pending-events` is not removed or deprecated by this ADR. An operator
who has cron invoking it continues to work correctly with the daemon tick running at
the same time: the drain's claim step is a `pending → firing` conditional `UPDATE ...
WHERE status = 'pending'`. Two concurrent callers, the daemon tick and an external cron
invocation, racing the same row resolve cleanly: exactly one claims it, the other's
conditional update affects zero rows and it moves on. This is the same CAS property
that already lets the drain's own reclaim sweep and a fresh claim race safely, and is
covered by the existing regression suite (`fire_claim_wins_race_against_concurrent_cancel`
and the stale-claimant tests). No additional locking or coordination between the tick
and external cron is required or added.

### 5. Interval: configurable, default 60 seconds

The tick interval is read from configuration with an environment override, defaulting
to 60 seconds, matching the cadence the drain's own module documentation already
recommends for cron-based invocation (`* * * * * kkernel exec --pending-events`). A
60-second default keeps scheduled-event latency in the same ballpark operators would
get from a standard cron minute-tick, without requiring cron to be configured at all in
a daemon-fronted deployment.

### 6. Repeat-advance semantics are unchanged

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
   verified by a new concurrent-tick regression test alongside the existing CAS race
   tests in `pending_events.rs`.
3. No MCP client process (a stdio `kkernel mcp` session without `--daemon`) spawns a
   schedule tick, verified the same way the existing `is_daemon_role_false_for_client_args`
   /`is_daemon_role_true_for_daemon_args` tests verify the email-channel gate.
4. The tick interval is overridable via environment configuration and defaults to 60
   seconds when unset.
5. Stopping the daemon stops the tick loop cleanly, following the same shutdown
   detection the checkpoint task already uses.
6. `kkernel exec --pending-events` continues to work unchanged as a standalone,
   cron-invocable one-shot drain.

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
   supported as a redundant fallback (Decision point 4), not the sole mechanism.
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
