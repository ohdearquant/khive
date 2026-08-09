# Pending-events drain — scheduled-event firing (ADR-106)

`pending_events` (`src/pending_events.rs`) drains due `scheduled_event` notes
written by the `schedule` pack: for each due row it CAS-claims the row,
replays the stored action DSL when policy permits, and CAS-finalizes the row to
`fired`, a re-armed `pending`, `missed`, or identity-policy `failed`. It runs
from two entry points — a one-shot CLI drain
(`kkernel exec --pending-events`) and a daemon-resident periodic loop
(`schedule_tick_loop`, ADR-106) — both funnelling into the same
`run_pending_events_on`.

## Why `rt` and `server` are two separate handles (PR #782)

`run_pending_events_on(rt, server, ...)` takes a `KhiveRuntime` AND a
`KhiveMcpServer`, and the two must never be collapsed into one. The supervised
`schedule_tick_loop(rt, host, ...)` receives that same server through its ADR-119
`HostContext`:

- `rt` is the **schedule pack's own runtime**. The scan/claim/finalize SQL
  reads and CAS-writes `scheduled_event` notes directly through it, so it
  must point at whichever backend the `schedule` pack is wired to.
- `server` is the **daemon's live, fully-wired `KhiveMcpServer`** — every
  pack registered against its own backend per `[[backends]]`/
  `[packs.*].backend` — used only for `dispatch_action` (replaying a fired
  event's stored action DSL).

An earlier version built a fresh `KhiveMcpServer::new(rt.clone())` from the
schedule runtime alone. That registers EVERY pack against the schedule
backend, so a replayed `comm.send` (or any other pack's action) would
silently dispatch into the schedule backend instead of that pack's own
configured backend in a multi-backend deployment. Passing the daemon's
actual `server` through keeps replayed-action routing identical to a live
request against that daemon.

The CLI path (`run_pending_events`) resolves a fresh `rt`/`server` pair per
invocation via `build_server_with_explicit_namespace` — correct for a
short-lived cron-invoked process. The daemon tick loop must NOT build its
own pair: the daemon boot path already resolved `--config`, `[[backends]]`,
actor identity, and `--pack` selection once at startup, and a
freshly-reconstructed `RuntimeConfig::default()` would drain
`$HOME/.khive/khive.db` instead of the configured backend (the PR #782 bug).

## Scheduled-event creator identity

`created_by_actor` is display metadata, never replay authority. Both schedule
creation verbs append an immutable, target-bound creator-provenance event before
activating the staged row. The drain reconstructs the exact actor kind from that
event and supplies it through the typed per-request identity seam, so replay remains
the creator even when a different actor owns the daemon. Generic scheduled actions
without that proof fail closed. Legacy reminders ignore forgeable note metadata and
use the configured scheduler actor, then anonymous `local`, under ADR-106's narrower
compatibility policy.

## Why the tick loop uses a fixed interval with `Skip`

`schedule_tick_loop` ticks on `tokio::time::interval_at` with
`MissedTickBehavior::Skip`, not a sleep-after-drain loop. A sleep-after-drain
loop's effective cadence is `interval + drain_duration`, which drifts further
behind on every pass that finds a nontrivial backlog (PR #782); ADR-106
specifies a fixed interval. The first tick fires after one full `interval`
has elapsed, matching the original sleep-based boot behavior instead of
draining immediately at daemon start.

## ADR-119 component supervision (issue #1409)

The daemon no longer drops a bare `tokio::spawn` handle for this loop. When the
resolved daemon pack set includes `schedule`, the host adds exactly one dynamic
`schedule-tick` registration to the ADR-119 component roster. The factory captures
the already-resolved schedule runtime and receives the daemon's live server through
`HostContext`. Client/stdio roles and daemon configurations without the schedule pack
add no ticker.

The concrete policy is `OnFailure`, five restarts per daemon lifetime, exponential
backoff with positive jitter from 1 second to a hard 60-second total-delay cap, and a
5-second cooperative-shutdown bound.
The loop selects between cancellation and each interval tick. A successful drain,
including an empty one or one containing per-event action failures, records the
component heartbeat. A drain-level error returns `ComponentError::Retryable` so the
supervisor records degradation and applies the restart policy. Per-event failures stay
inside `DrainSummary` and never consume the component restart budget.

## Replay identity and legacy rows

Both `schedule.remind` and `schedule.schedule` mirror `created_by_actor` into the note for
display, but create the note in inert `status="provisioning"`, append a target-bound creator
event to the immutable event substrate, and only then activate it as `pending`. At fire time,
the runner reconstructs the exact verified actor kind from that event's actor column:
attributed principals use `VerifiedActor`, while `anonymous:local` remains anonymous. It
never treats the caller-editable note property or stored DSL as authority, and replay preserves the public
verb-visibility boundary (internal subhandlers stay denied). Generic scheduled actions
written before immutable provenance existed fail closed: the payload is not dispatched, the
row becomes terminal `status="failed"`, and `dispatch_error` plus `dispatch_failed_at`
explain the migration-policy failure. Legacy reminders ignore any unprovenanced actor claim
and use the current server actor, then `local`, preserving a safe form of Amendment C's
fallback without permitting forged delivery identity. Refused generic rows retain
`anonymous:local` in their diagnostic receipt because they have no verified creator; the daemon
fallback is reminder-only. Legacy batches and chains are also refused before
`mark_dispatch_invoking`, with terminal `failed`/`not_invoked` state, so best-effort partial
success can never be retried as a whole and duplicated.

Other generic dispatch failures remain per-event: they are persisted as
`dispatch_error`/`dispatch_failed_at`. A failed one-shot returns to `pending` for a later
drain; a failed named repeat advances to its next occurrence. A later success clears
those fields.

## Ticker liveness on `schedule.agenda` (issue #1352)

The daemon's `KhiveMcpServer` owns a process-local ticker heartbeat. After each interval
yields, `schedule_tick_loop` records the tick before it starts the drain, including passes
that find no due rows or return an error. Successful MCP dispatches of `schedule.agenda`
include a host-added health field:

```json
{ "events": [], "count": 0, "ticker": { "last_tick_at": "2026-08-01T12:34:56.123456Z" } }
```

`last_tick_at` is null before this server instance observes a tick. It is intentionally
not persisted: a newly constructed server over the same database starts with no heartbeat,
while a stopped or wedged loop leaves its last value frozen for caller-side staleness
judgment. The host does not compute a `healthy` flag because the acceptable age depends on
the configured tick interval. Agent presentation may render the timestamp relatively;
request `presentation="verbose"` when the exact RFC 3339 value is required.

## Claim / finalize CAS state machine (issue #462)

`claim_pending_event` CAS-transitions a row `pending -> firing` and atomically
persists `firing_at`, a deterministic occurrence id, a fresh invocation id, and
`lease_expires_at`. Callers thread both `firing_at` and the invocation id through
every receipt update, lease renewal, and `finalize_fired_event`; a stale claimant
cannot match a later attempt even if timestamps collide. The pending claim still
mirrors `schedule.cancel`'s CAS, so cancel and fire share one state machine.

Immediately before polling the target action, `mark_dispatch_invoking` changes the
receipt from `claimed` to `invoking`. The action has a separately spawned lease
renewer that remains active through durable outcome persistence, so a handler that
blocks its own async polling task cannot starve the lease and writer contention after
the return cannot reopen an unleased outcome gap.
`KHIVE_SCHEDULE_LEASE_SECS` is a positive seconds value (default/fallback 300), and
renewal runs every one third of the lease. `finalize_fired_event` clears the active
`firing_at`/`lease_expires_at` fields but retains the last receipt.
Pre-invocation finalizations retain that same claim identity too: policy or payload
refusals use `state="not_invoked"` with `completed_at` and a non-empty `error`, while
grace-window skips use `state="missed"` with `completed_at` and `error=null`. These
states prove that no target action future was polled; they are not dispatch outcomes.
Missed reminders still resolve immutable creator provenance so their retained receipt is
creator-attributed; only a genuinely legacy reminder without provenance uses the scheduler
fallback.
Recovery re-checks the current deadline and matches the exact serialized properties selected by
its scan in every requeue, quarantine, and lifecycle-finalization CAS. A renewal, durable outcome,
or any other intervening properties mutation therefore wins ownership instead of being overwritten
from the scan's stale snapshot.

`reclaim_stale_firing_events` reconciles expired deadlines by durable state. A
v1 receipt is fully validated before its state is interpreted: the version,
occurrence/invocation UUIDs, actor encoding, integer `claimed_at` matching the active
`firing_at`, and the occurrence UUIDv5 derived from the event id plus scheduled UTC
instant are all required. `invoking` also requires `invocation_started_at`; terminal
states require a valid `completed_at`, with `error=null` for `succeeded`/`missed` and a
non-empty error for `failed`/`indeterminate`/`not_invoked`. A `claimed` occurrence
atomically becomes `not_invoked` and returns to pending because invocation never began; this
increments `retry_pending`/`finalized`, not `failed`, `invoked`, or `outcomes_persisted`. Valid `succeeded` or `failed`
resumes finalization without invoking again; a failed one-shot remains pending, while
a failed repeat advances normally. An expired `invoking`, malformed receipt, or
completed pre-invocation receipt still attached to `status="firing"` becomes terminal
`failed`/`indeterminate`, because replay could duplicate a side effect that committed
before the claimant disappeared. The quarantine record retains the malformed source
receipt under `invalid_receipt` for diagnosis. Pre-receipt rows retain the historical
five-minute `firing_at` fallback.

Action errors retain their original structured value in optional
`dispatch_receipt.error_payload` alongside the readable `error` string. In particular,
`side_effects_unknown` and other explicit ambiguous outcomes are persisted as terminal
`indeterminate`, retaining correlation values such as `details.outbound_id`; a later drain does
not replay them. Receipt validation rejects non-null action payloads for `claimed`, `invoking`,
`succeeded`, `missed`, and `not_invoked`, while `failed` and `indeterminate` may carry one.
Each expired-row requeue/quarantine/finalization write is row-local: a write error is logged and
counted in `failed`, then recovery continues with the remaining rows and normal due-work scan.

`DrainSummary` reports `invoked`, `outcomes_persisted`, and `finalized` separately,
plus `retry_pending` and `indeterminate`. `fired`/`advanced` increment only after the
corresponding CAS commits; finalization failure never decrements an unrelated prior
counter.

## `discover_pending_namespaces` — offset-safe due-time comparison (PR #782)

Due-row discovery compares `trigger_at` via SQLite's `datetime(...)`, not a
raw string comparison. `khive-pack-schedule` round-trips the caller's
original `trigger_at` string verbatim, offset included, and any RFC 3339
offset is accepted — a raw-text `<=` only matches chronological order when
every stored string happens to share `now`'s UTC offset, which is not
guaranteed. `datetime(...)` normalizes both sides to UTC before comparing.
The Rust layer downstream still re-parses and re-checks each candidate row
with `DateTime<Utc>` as the final authority — the SQL predicate is a fetch
bound, not the last word.

## Executable recurrence boundary

Creation accepts only `daily`, `weekly`, and `monthly`, exactly the forms
`next_trigger_at` advances. Five-field cron is rejected instead of being stored
and silently consumed as a one-shot. A legacy cron row fails closed before action
invocation.

## `advance_repeat_past_missed` — no catch-up bursts (ADR-106 missed-event amendment)

Advances a missed repeating event's `trigger_at` past every occurrence at or
before `now`, landing on the first occurrence strictly after `now`. This is
what makes a missed repeat re-arm without ever firing a catch-up burst: a
daily reminder that was due 10 times while the daemon was down skips straight
to tomorrow's occurrence instead of firing 10 times in a row. Terminates
because `next_trigger_at`'s named-alias arms are always strictly increasing,
so `now` fixed plus a bounded number of forward steps reaches `next > now`.
