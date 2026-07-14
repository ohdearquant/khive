# Pending-events drain — scheduled-event firing (ADR-106)

`pending_events` (`src/pending_events.rs`) drains due `scheduled_event` notes
written by the `schedule` pack: for each due row it CAS-claims the row,
replays the stored action DSL, and CAS-finalizes the row to `fired` or a
re-armed `pending`. It runs from two entry points — a one-shot CLI drain
(`kkernel exec --pending-events`) and a daemon-resident periodic loop
(`schedule_tick_loop`, ADR-106) — both funnelling into the same
`run_pending_events_on`.

## Why `rt` and `server` are two separate handles (PR #782)

`run_pending_events_on(rt, server, ...)` and `schedule_tick_loop(rt, server, ...)`
both take a `KhiveRuntime` AND a `KhiveMcpServer`, and the two must never be
collapsed into one:

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

## Why the tick loop uses a fixed interval with `Skip`

`schedule_tick_loop` ticks on `tokio::time::interval_at` with
`MissedTickBehavior::Skip`, not a sleep-after-drain loop. A sleep-after-drain
loop's effective cadence is `interval + drain_duration`, which drifts further
behind on every pass that finds a nontrivial backlog (PR #782); ADR-106
specifies a fixed interval. The first tick fires after one full `interval`
has elapsed, matching the original sleep-based boot behavior instead of
draining immediately at daemon start. A per-tick failure (e.g. a transient
SQL error) is logged and does not stop the loop.

## Claim / finalize CAS state machine (issue #462)

`claim_pending_event` CAS-transitions a row `pending -> firing` and returns
the transition's `firing_at` timestamp as a **claim token**. Callers MUST
thread this token through to `finalize_fired_event`, which requires the
row's CURRENT `firing_at` to still equal the token — not merely that
`status='firing'`. Without binding to the specific token, a stale claimant
that stalls past `STALE_FIRING_TIMEOUT_MICROS`, gets reclaimed by
`reclaim_stale_firing_events`, and is re-claimed by a second drain could
resume and finalize over the second drain's live claim purely because both
rows share `status='firing'`. A reclaim always rewrites `firing_at` (via a
fresh claim) or clears `status` back to `pending`, so a stale token can never
match the row's current one. The claim also mirrors the `schedule.cancel` CAS
in `khive-pack-schedule/src/handlers.rs` (which only matches
`status='pending'`) so the two writers share one state machine.

`reclaim_stale_firing_events` sweeps rows stuck in `firing` whose `firing_at`
predates a staleness threshold, across all namespaces in one statement. Rows
claimed by a pre-#462 binary (missing `firing_at` entirely) are treated as
maximally stale and reclaimed unconditionally — there is no timestamp to
compare against, and leaving them wedged forever is strictly worse.

`finalize_fired_event`'s terminal write clears `firing_at` — the event has
reached a terminal state for this cycle (`fired` or re-armed `pending`), so
no claim token should survive to be mistaken for a live claim later.

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

## `advance_repeat_past_missed` — no catch-up bursts (ADR-106 missed-event amendment)

Advances a missed repeating event's `trigger_at` past every occurrence at or
before `now`, landing on the first occurrence strictly after `now`. This is
what makes a missed repeat re-arm without ever firing a catch-up burst: a
daily reminder that was due 10 times while the daemon was down skips straight
to tomorrow's occurrence instead of firing 10 times in a row. Terminates
because `next_trigger_at`'s named-alias arms are always strictly increasing,
so `now` fixed plus a bounded number of forward steps reaches `next > now`.
