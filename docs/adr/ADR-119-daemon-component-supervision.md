# ADR-119: Host-Supervised Daemon Components Beside the Verb Plane

**Status**: accepted (2026-07-19)\
**Date**: 2026-07-19\
**Scope**: daemon-resident work that is not a caller-invoked operation\
**Amended by**: [ADR-122](ADR-122-email-outbound-delivery.md) (email Phase 2)

## Context

khive exposes one MCP tool, `request`, whose DSL dispatches pack-registered verbs. That
wire contract is owned by [ADR-016](ADR-016-request-dsl.md).
The `Pack` and `PackRuntime` mechanics are owned by
[ADR-017](ADR-017-pack-standard.md); pack verb surface,
visibility, naming, and composition are owned by
[ADR-023](ADR-023-declarative-pack-format.md).

The daemon also runs work that is not initiated by a caller: the schedule drain, email
ingest loops, session mirror scans, ANN maintenance, and checkpointing. These loops are
hand-spawned and do not share a lifecycle contract for cancellation, restart budgets,
backoff, health, or bounded shutdown. Issue #1126 demonstrates the operational failure
this permits: one un-ingestable IMAP message can hold the email channel in an unbounded
retry loop.

This does not establish that every loop belongs to its originating pack. The shipped
schedule executor needs both the schedule pack's resolved runtime and the daemon's live,
multi-backend-wired `KhiveMcpServer` to dispatch stored actions correctly
([ADR-106](ADR-106-schedule-pack-executor.md), Amendment B,
update 2). That is direct evidence against putting a service factory on `Pack` before a
narrow construction boundary has been proved.

Nor does the current evidence establish a need for an event-topic bus. Polling at the
present fleet scale is not a throughput problem, and no concrete consumer has yet shown
that freshness requires push. ADR-022 ships a durable event query surface, while ADR-017
now marks `PackEventConsumer` catch-up and live delivery as deferred design. A future
push or consumer proposal must reconcile with that durable source of truth rather than
introduce a second log or cursor authority.

## Assumptions examined

1. **“Three planes are needed now.” — Refuted.** Current failures need a host-owned
   supervisor around existing loops. They do not require a pack-trait extension or a
   notification bus. The minimal component registry solves the lifecycle defect without
   adding two speculative extension surfaces.
2. **“The schedule and email incidents prove pack-level service declaration.” —
   Refuted.** They prove inconsistent supervision. ADR-106 currently depends on the live
   MCP server for cross-pack dispatch, while email construction crosses channel and MCP
   crate boundaries. Both fail the proposed narrow-context assumption today.
3. **“The session mirror scan requires the taxonomy.” — Refuted.** An O(corpus) scan is
   an algorithmic/cursor problem. A supervisor can bound and expose failure, but cannot
   make the scan incremental. Issue #1127 remains separate corrective work.
4. **“A bus reduces meaningful load.” — Refuted at current scale.** Existing analysis
   places eight agents polling every 60 seconds at roughly 11 MiB/day on indexed SQLite.
   A future bus must be justified by latency/freshness and staleness-summary ergonomics,
   not by a load-reduction claim.
5. **“An at-least-once topic is a missing capability.” — Refuted for event-plane facts.**
   ADR-022 supplies the durable query side. ADR-017 records cursor-based pack catch-up
   and live delivery only as deferred design. A new ephemeral topic could at most be a
   wake-up optimization over durable state; it cannot become a second source of truth
   or silently make the deferred consumer contract real.
6. **“The change is cheap.” — Unsupported and dropped.** No LOC or complexity estimate
   is accepted without a prototype. This ADR makes no cheapness claim.

## Decision

khive adopts two implemented capability classes and reserves a third name without
implementing it:

1. **Verb** — unchanged. A verb is a bounded, caller-invoked operation dispatched by
   `PackRuntime::dispatch` through the ADR-016 `request` DSL. All existing visibility
   and composition rules remain under ADR-023.
2. **Daemon component** — new, internal. A daemon component is host-constructed,
   daemon-role-only, long-running work supervised by a registry local to `khive-mcp`.
   It is not a verb and is not declared by `Pack` in this ADR.
3. **Event topic** — reserved and deferred. If later justified, it means only a bounded,
   process-local wake-up channel over already-durable state. It is not an accepted
   runtime or wire capability in ADR-119. The separate deferred decision brief records
   the proof obligations.

No separate **resource plane** is introduced. Persistent state remains accessed through
the existing substrate and verb contracts. An MCP resources surface would be a wire/API
decision with no current consumer and requires its own ADR.

### Component registry locus and construction

The first implementation MUST define a `khive-mcp`-local registry with the conceptual
shape below. Names are illustrative; the behavioral contract is normative.

```rust
struct DaemonComponentRegistration {
    name: &'static str,
    restart: RestartClass,
    restart_budget: RestartBudget,
    backoff: BackoffPolicy,
    shutdown_timeout: Duration,
    start: ComponentFactory,
}

trait DaemonComponent: Send + 'static {
    async fn run(self, host: ComponentHost) -> Result<(), ComponentError>;
}

struct ComponentHost {
    cancellation: CancellationToken,
    health: HealthReporter,
}
```

The host constructs registrations only after server configuration, actor identity,
backend routing, and pack selection have been fully resolved. A factory MAY capture
resolved, component-specific handles, including the live `KhiveMcpServer` where the
existing component genuinely needs it. `ComponentHost` carries only lifecycle services;
it is not a service locator.

The host, not the loop, owns:

- daemon-role gating and startup ordering;
- cancellation and shutdown timeout;
- panic/join-error observation and isolation at the task boundary;
- restart classification, bounded restart budget, exponential backoff, and jitter;
- last-start, last-success/heartbeat, last-error, restart-count, and terminal-state
  reporting; and
- task naming and structured logs.

Restart classes MUST distinguish at least `Never` and `OnFailure`. Normal cancellation
and clean completion MUST NOT consume a failure budget. A component that exhausts its
budget becomes terminally unhealthy; it MUST NOT hot-loop. Restart counters and backoff
state MUST be per component, not global.

In-process supervision is not process isolation. Blocking work MUST use an explicitly
bounded blocking pool or subprocess boundary; it MUST NOT occupy an async runtime worker.
A shutdown timeout MAY abort the task after cooperative cancellation. If fault injection
cannot keep a hung component inside the shutdown SLO without starving unrelated work,
the component MUST move behind a watchdog subprocess before adoption.

### Shutdown handoff from the daemon runtime

The registry does not invent a shutdown path; it joins the one the daemon already has.
ADR-106 (decision point 1a and Amendment B) fixes the contract this section composes
with: the daemon folds `SIGTERM`/`SIGINT` detection into a single shutdown future ahead
of `drain()`, and `drain()` awaits tasks registered through the runtime's
`track_background_task` helper.

Normative ordering:

1. The registry's supervisor task MUST be registered through `track_background_task`
   (or the equivalent tracked-task mechanism), so `drain()` cannot complete while the
   registry is alive.
2. When the daemon's unified shutdown future resolves, the runtime MUST invoke registry
   shutdown before or concurrently with `drain()`: cooperatively cancel every component,
   then join each with a wait bounded by that component's `shutdown_timeout`, aborting
   the task on expiry.
3. Registry shutdown completes within the `drain()` wait; socket and PID cleanup happen
   only after `drain()` returns, exactly as today.

A component that ignores cancellation is therefore bounded by its own
`shutdown_timeout`, and the daemon's total shutdown latency is bounded by the maximum
component timeout plus the existing drain behavior. No component may register a
shutdown path outside this handoff.

### Reference migration: ADR-106 schedule drain

The schedule tick becomes the reference first registry consumer. This is a lifecycle
refactor, not a change to schedule semantics:

- The schedule pack continues to store intent. Trigger evaluation, claim, dispatch,
  finalization, missed-event policy, repeat advancement, and stale-claim recovery remain
  the drain's job exactly as ADR-106 specifies.
- The component factory MAY capture both the resolved schedule runtime and the live
  multi-backend `KhiveMcpServer`; ADR-106 already proves both are required for correct
  routing.
- `KHIVE_SCHEDULE_TICK_SECS`, its 60-second default, daemon-role gating, external-cron
  compatibility, CAS behavior, and all ADR-106 amendments remain unchanged.
- The registry replaces the bare `tokio::spawn` with tracked cancellation and bounded
  shutdown, closing ADR-106 Acceptance Criterion 5. It does not claim to satisfy the
  unimplemented `DaemonDispatch::drain_pending_events` seam in Criteria 6 and 7.

### Error and restart mapping for the schedule component

The registry contract above is abstract; the reference migration fixes the concrete
mapping so its required tests are determinate. ADR-106 already distinguishes drain-level
failures from per-event action failures; this section maps that boundary onto the
component contract:

| Outcome at the component boundary                                                                                                                       | Classification                                                                       | Restart                    | Budget                                                                    | Health                                                                 |
| ------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------ | -------------------------- | ------------------------------------------------------------------------- | ---------------------------------------------------------------------- |
| Per-event action failure (dispatch error, action-level timeout)                                                                                         | Absorbed by the drain per ADR-106; recorded on the event row; NOT a `ComponentError` | none                       | not consumed                                                              | unchanged                                                              |
| Drain-level failure (database unavailable, backend/config resolution)                                                                                   | `ComponentError` (retryable)                                                         | `OnFailure`, after backoff | consumed                                                                  | `Degraded` (carries last-error)                                        |
| Panic / join error                                                                                                                                      | `ComponentError` (retryable)                                                         | `OnFailure`, after backoff | consumed                                                                  | `Degraded`                                                             |
| Permanent failure (invalid or contradictory configuration, schema/contract incompatibility — conditions that cannot change within the process lifetime) | `ComponentError` (permanent)                                                         | none — never restarted     | not consumed (budget is irrelevant; the terminal transition is immediate) | terminal `Unhealthy` (carries last-error); MUST NOT hot-loop           |
| Cooperative cancellation                                                                                                                                | clean stop                                                                           | none                       | not consumed                                                              | terminal `Stopped`                                                     |
| Clean completion (loop exits without error outside shutdown)                                                                                            | clean stop                                                                           | none                       | not consumed                                                              | terminal `Stopped`; MAY warn — the tick loop is not expected to finish |
| Budget exhausted                                                                                                                                        | —                                                                                    | none                       | —                                                                         | terminal `Unhealthy`; MUST NOT hot-loop                                |

Retryability is carried on the error type: `ComponentError` MUST distinguish retryable
from permanent (for example `ComponentError::Retryable` / `ComponentError::Permanent`).
The component author classifies; the host acts only on the classification. A permanent
error transitions the component directly to terminal `Unhealthy` with no restart and no
backoff, regardless of remaining budget. For the schedule component, database
unavailability and transient backend resolution failures are retryable; a configuration
or schema state that cannot change while the process lives is permanent.

Budget-reset policy: the default is no automatic reset within a daemon process lifetime
— the budget spans the process, and a daemon restart resets it. A time-windowed reset
(restart intensity over a sliding period) MAY be adopted later, but it is an explicit
implementation decision, not an assumed default.

### Scheduled-action identity

The component factory capturing a daemon-held `KhiveMcpServer` makes the replay
identity contract normative content of this ADR: without it, the capture this ADR
blesses is a confused-deputy seam. Generic scheduled actions (`schedule.schedule`)
MUST replay under the creator's authenticated actor identity, not the daemon's own
identity. Concretely:

- The creator's authenticated actor MUST be persisted on the scheduled event at
  schedule time.
- Replay MUST dispatch with that persisted actor as the request identity, so any policy
  gate evaluates the creator's authority, never the daemon's. A caller MUST NOT be able
  to schedule an action the caller is denied but the daemon is allowed to perform.
- Stored rows without a persisted creator identity MUST fail closed — the event
  transitions to its failure state with a policy error and is never dispatched under
  the daemon identity — unless a separate, explicit migration policy is adopted for
  them.

The current replay path predates this ADR and does not carry a creator identity; that
is a defect this ADR surfaces, not one it introduces. The registry migration MUST NOT
ship with the identity gap intact: the identity contract is an acceptance criterion
below.

Email, session-mirror, ANN, and checkpoint loops MAY migrate only after the schedule
reference proves the registry contract. Their migrations MUST preserve their existing
storage and correctness contracts; shared supervision is not permission to redesign
their algorithms.

### Health and public compatibility

ADR-119 adds no MCP verb, resource, subscription, notification, or capability entry.
Component status is operator-local structured logging/metrics in the first
implementation. Any public introspection surface requires a separate additive decision.

`tools/list`, the `request` schema and parser, the request tool description, and legacy
stdio response bytes MUST remain byte-identical for the same pack/configuration set.
Existing packs MUST compile without modification because neither `Pack` nor
`PackRuntime` changes.

### Event-topic compatibility contract, if revisited

This ADR does not authorize an event bus, but it fixes the invariant any future decision
must preserve:

> Commit durable state first; publish second. A slow or reconnecting subscriber gets an
> explicit lag/epoch error and resynchronizes from durable state. The bus is provably
> non-load-bearing for correctness: a dropped notification delays detection, never loses
> a fact.

Where the notification refers to an event-plane fact, the durable side is ADR-022's
event query surface. A future topic MUST compose with that surface and with any later
ADR that accepts cursor replay; it MUST NOT shadow them with a second durable log or
cursor authority.

Minimum tests for any future bus are: rollback-after-publish is unobservable;
capacity-2 publish-3 yields `Lagged(1)`; a stale cursor yields
`CursorTooOld { oldest, newest }`; and a daemon restart yields `StreamChanged`, never
sequence reinterpretation.

## Alternatives considered

### Add `BackgroundService` and event topics to `Pack` now

Rejected. It gives pack authors a uniform declaration locus, but both current
cross-crate candidates depend on MCP-host construction. Adding default trait methods
would make existing packs compile while still freezing an unproved dependency boundary.
The event-topic half also lacks a current latency consumer and overlaps ADR-017's
deferred consumer design.

### Keep hand-spawned tasks and repair each loop independently

Rejected. It minimizes shared code but preserves inconsistent restart, cancellation,
and health behavior. Issue #1126 is evidence that local retry policy can become an
unbounded channel-wide failure mode.

### Run every component as a subprocess

Rejected as the default. Process isolation is stronger but adds IPC, deployment, and
state-transfer contracts before fault injection establishes that in-process isolation is
insufficient. It remains the mandatory fallback under Acceptance Criterion 2.

### Adopt a `khive-mcp` component registry now and promote later

Chosen. It solves the observed lifecycle problem at the actual construction boundary,
keeps all public and pack contracts additive, and leaves promotion falsifiable: at least
two independent packs must demonstrate construction from a narrow host context.

## Rationale

The negation matters: without this ADR, daemon loops remain untracked and each loop can
invent retry and shutdown behavior. With it, the host has one enforceable lifecycle
contract while persistent semantics remain with the existing pack and substrate ADRs.

The registry-local choice is intentionally reversible. Moving a proven registration
descriptor outward to `Pack` later is additive. Moving a prematurely broad
`KhiveMcpServer` dependency into every pack would create a cycle and be costly to undo.

Splitting event topics is also deliberate. The service registry has current consumers
and a concrete incident. The bus has estimated magnitudes, no measured latency need,
and a durable query surface plus a deferred consumer design to reconcile. Combining
them would let the stronger service case smuggle an unproved bus into acceptance.

## Risks and unknowns

- **A local registry may become a permanent special case.** Mitigation: record dependency
  shapes for every migrated component; revisit pack declaration after two independent
  packs use the same narrow context.
- **In-process panic isolation may be overstated.** Mitigation: distinguish task panic,
  cooperative hang, blocking starvation, allocator abort, and process crash in fault
  tests; only the first two are candidates for in-process handling.
- **Restart policy can amplify permanent errors.** Mitigation: typed retryability,
  finite budgets, capped backoff, and a terminal unhealthy state. Issue #1126's poison
  message must not restart the whole channel forever.
- **A supervisor can conceal an algorithmic defect.** Mitigation: issue #1127's
  O(corpus) mirror behavior remains a separate acceptance gate; health telemetry must
  expose repeated full scans rather than normalize them.
- **Exact memory, latency, and restart magnitudes are unknown.** They remain prototype
  measurements, not ADR claims.

## Acceptance criteria and revisit triggers

The following gates are binding and intentionally preserve the issue #373 acceptance
frame:

1. **Taxonomy kill:** if call-site analysis shows every candidate service must depend on
   `KhiveMcpServer` directly rather than a narrow runtime context → reject pack-level
   declaration, use a daemon-component registry at khive-mcp instead.
2. **Supervision kill:** if fault injection shows a hung service can starve the runtime
   or cannot be isolated within the shutdown SLO → host supervision insufficient;
   require process isolation or watchdog subprocess.
3. **Push kill:** if measured p99 publish-to-receive latency is not ≥10x lower than the
   poll interval, or CPU/query load does not fall materially → retain polling.
4. **Memory kill:** steady-state RSS growth >2x the ~584 KiB roofline, or growth with
   subscriber count (per-subscriber payload clones) → replace representation.
5. **Correctness kill:** any lag/reconnect/restart that silently omits a durable change
   without an explicit error/resync path → bus rejected as data-delivery; best-effort
   wakeups only.
6. **Compatibility kill:** default trait methods or experimental capability entries that
   change `tools/list`, request parsing, or legacy stdio bytes → non-additive, reject.
7. **Complexity-claim kill:** no surveyed source defends a LOC-delta cheapness claim; the
   ADR makes NO cheapness claim; prototype exceeding the complexity budget → re-scope.

In addition, ADR-119 is accepted only when:

1. A call-site inventory records each current component's construction dependencies and
   demonstrates why the registry remains in `khive-mcp`.
2. The ADR-106 schedule drain runs through the registry without semantic or wire changes.
3. Tests cover clean completion, retryable error, permanent error, panic, poison input,
   cancellation, shutdown timeout, budget exhaustion, and independent-component
   progress during another component's failure, each classified per the error and
   restart mapping above.
4. A production-shaped shutdown test proves the schedule component meets the selected
   SLO through the normative shutdown handoff (registry supervisor tracked, cancel-all,
   bounded join inside the drain wait), or Acceptance Criterion 2 selects process
   isolation.
5. Golden compatibility tests prove identical `tools/list`, request parsing, and legacy
   stdio bytes.
6. Generic scheduled-action replay dispatches under the persisted creator actor; a test
   proves a caller cannot schedule an action the caller is denied but the daemon is
   allowed to perform; stored rows without a creator identity fail closed or follow a
   documented migration policy.

ADR-079 Amendment 1's daemon-resource reference is corrected from issues #1126/#1127 to
issues #1127/#1129 in the change that introduces this ADR; issue #1126 is cited here
solely as the email poison-message supervision incident.

## Implementation fences

### MAY

- Add a registry and supervisor internal to `khive-mcp`.
- Let a registration factory capture fully resolved component-specific handles.
- Migrate the ADR-106 tick first, preserving every schedule state-machine contract.
- Add operator-local health metrics and structured logs.
- Promote declaration toward packs in a later ADR after two independent packs prove a
  narrow common context.

### MAY NOT

- Change `Pack`, `PackRuntime`, ADR-023 verb visibility/composition, ADR-016 `request`,
  `tools/list`, or legacy stdio bytes.
- Add an MCP resource, subscription, notification, event topic, or public health verb.
- Treat restart as correctness recovery for work that has not committed durable state.
- Dispatch a stored scheduled action under the daemon's own identity; replay carries the
  persisted creator actor or fails closed.
- Put blocking or unbounded work on async runtime workers.
- Claim that supervision fixes issue #1127's scan complexity.
- Claim complexity, memory, latency, or load benefits without measurement.

### Verify by

- Dependency inventory plus compile-time proof that existing packs are unchanged.
- Fault injection and production-shaped shutdown tests.
- ADR-106 schedule regressions, including multi-backend dispatch and external-cron races.
- Golden public-wire fixtures.
- RSS/restart/health measurements reported as measurements, not projections.

## Consequences

Daemon-resident work gets a shared lifecycle without pretending it is a verb. The
public API remains unchanged. Pack-level service declaration stays possible but must be
earned by a narrow dependency seam. Event topics, arbitrary subscriptions, and MCP
resources do not ride along with the stronger supervision decision.

---

## Amendment 1: Externally-Linked Daemon Components (2026-07-23)

**Status**: accepted (2026-07-23)\
**Trigger**: a channel-crate reorganization can silently drop a daemon component's startup
loop if the loop lives outside the crate boundary that moves. When transport-channel crates
(`khive-channel`, `khive-channel-email`, `khive-channel-telegram`) live separately from the
daemon binary that force-links them, but their daemon loops (`spawn_email_channel_loops`,
`channel_poll_loop`, `channel_outbox_loop`) live inline in `khive-mcp/src/serve.rs`, a crate
reorganization can move the crates without moving the loops, and a binary can build and run
with the transport channel silently absent — no warning, because the code is absent rather
than failing. A binary composed from separately-linked components needs a way to contribute
daemon components the core crate cannot itself name.

### Problem

The base decision makes the component registry internal to `khive-mcp`, with the host
constructing registrations from code it can see. A component whose crate lives outside
`khive-mcp`'s own dependency tree cannot be constructed by the core registry, and a binary
that links such a crate has no sanctioned way to hand its component to the supervisor. A
capability can therefore go missing with no error surface, because absent code neither
starts nor fails.

### Decision

Extend the base registry with **link-time component registration**, the same mechanism
ADR-027 already uses for pack self-registration:

1. `khive-mcp` exposes a public registration surface: an inventory-collected submission
   whose payload is a `DaemonComponentRegistration` where `start` is a factory function
   `fn(&HostContext) -> BoxFuture<Result<(), ComponentError>>` (names illustrative;
   behavioral contract normative). `HostContext` supplies only what the base ADR already
   lets internal factories capture: the resolved verb-dispatch handle, the daemon's
   resolved actor/namespace configuration, and the `ComponentHost` lifecycle services.
   It is not a service locator; anything else a component needs, it owns.
2. In daemon role, after server configuration, actor identity, backend routing, and
   pack selection are fully resolved, the host iterates the inventory and registers
   each submission with the supervisor. Every base-ADR lifecycle contract — restart
   classes, budgets, backoff, shutdown timeout, health reporting, panic isolation —
   applies to externally-linked components unchanged and without exception.
3. An empty inventory is the normal core state: the public binary builds, tests, and
   runs with zero external components, and the supervisor treats the empty set as a
   no-op. Core carries a test fixture component to prove the seam without shipping one.
4. A downstream workspace may link additional component crates and implement components
   — such as the email poll and outbox loops — as daemon components under this contract.
   Each component's per-cycle operation deadline is its own obligation; stall detection
   and restart are the supervisor's, driven by the component's heartbeat through
   `HealthReporter`.

### Fences (additive to the base ADR)

- Registration is collected at daemon startup only; non-daemon roles MUST NOT start
  external components.
- The registration seam itself MUST remain generic; it must not be special-cased to any
  one component.
- A component that fails to construct MUST report as terminally unhealthy by name; it
  MUST NOT abort daemon startup for unrelated components.
- The install verification for any binary composing external components MUST include a
  positive artifact for each expected component (startup health row or equivalent),
  because verb counts cannot witness non-verb capability — a capability check keyed only
  on verb counts can pass with a component silently missing.

### Verify by

- Core: fixture-component test proving registration, supervision, cancellation, and the
  empty-inventory no-op.
- Core: HostContext's dispatch handle demonstrably carries daemon-internal capability —
  a fixture component performs a write that is not reachable on the MCP wire surface
  (the ingest class), and the write lands. A component limited to wire-callable verbs
  is dead on arrival for channel ingest.
- Core: at startup the daemon logs the enumerated external-component roster (names and
  count), so zero-components-where-N-expected is one visible runtime line complementing
  fence 4's install-time check. Both daemon entrypoints emit it.
- Distribution: channel component behavioral test — a REAL ingest write lands a message
  row (not heartbeat alone); heartbeat advances under a test config; a deliberately
  hung poll cycle is deadline-classified as a failure, consumes restart budget, and
  surfaces in health rather than freezing silently.
- Install: capability check extended per fence 4.

### Consequences

The email and telegram channels register as supervised components instead of
hand-spawned loops, closing the silent-drop class this amendment addresses. The core
public API grows one generic registration seam with no component-specific knowledge.
Pack-level service declaration remains deferred exactly as the base ADR left it; this
amendment registers binaries' components, not packs'.

---

## Amendment 2: Phased Email Distribution Delivery — Poll First (2026-07-23)

**Status**: accepted (2026-07-23)\
**Trigger**: Amendment 1's Decision point 4 commits to implementing "the email poll and
outbox loops" as a single deliverable, but inbound poll/ingest and outbound send are
separable units of work that need not ship atomically. Shipping the supervised inbound
poll/ingest component alone, before the outbox/send loop exists, would leave the accepted
contract and the shipped behavior in direct conflict unless the ADR itself records the
split.

**Current disposition**: the Phase 1 interval and all “Phase 2 future” language
below are historical. Amendment 3 records Phase 2 as delivered on 2026-07-29.

### Problem

Amendment 1 does not distinguish inbound and outbound delivery as separable
deliverables — it names both loops in one decision point. The distribution
workspace's `khive-channel-email` crate exposes `Channel::send`, and
`comm.send(to="email:...")` is accepted and durably stored by `khive-pack-comm`
today, but no supervised component drains stored outbound email and calls `send`.
An operator reading Amendment 1 alone would reasonably expect outbound delivery to
already work.

### Decision

Split the single "email poll and outbox loops" deliverable into two phases, each its
own daemon component under the base ADR and Amendment 1's registration contract:

1. **Phase 1 (this amendment)**: the inbound poll/ingest
   component only (`email-channel` in the roster). It reads `KHIVE_EMAIL_*`
   configuration, polls IMAP under the per-cycle watchdog deadline, and ingests
   through `comm.ingest`. This satisfies Amendment 1's fences and verification
   bar for the _poll_ half of Decision point 4.
2. **Phase 2 (separate lane, not scheduled by this amendment)**: a supervised
   outbox delivery component that drains stored outbound `email:*` messages and
   calls `Channel::send`, with its own restart/backoff registration, health
   reporting, and behavioral test proving a real send lands (mirroring the Phase 1
   ingest-lands-a-row test). Until Phase 2 lands, Decision point 4 remains only
   partially satisfied.

**Interim behavior, stated plainly**: `comm.send(to="email:...")` is accepted and
durably stored by `khive-pack-comm` in both phases. In Phase 1, that stored message
is **not delivered** — no supervised loop reads it, and no daemon component sends
SMTP traffic. Operators MUST NOT rely on daemon email delivery until Phase 2 ships;
a `comm.send` to an `email:*` target is durable storage only, not a delivery
guarantee, for the duration of Phase 1.

### Fences (additive to Amendment 1)

- The Phase 1 roster and health rows MUST NOT claim or imply outbound delivery
  capability — the `email-channel` component name and its documentation describe
  inbound poll/ingest only.
- Phase 2 MUST register as its own named component (not folded silently into
  `email-channel`'s health identity), so an operator can distinguish "inbound is
  healthy" from "outbound is healthy" once both exist.
- INSTALL.md and any other operator-facing distribution documentation MUST state
  the interim behavior from this amendment's Decision section, not a bare
  "not shipped" deferral, so an operator reading it understands stored-but-
  undelivered rather than assuming the feature is simply missing.

### Verify by

- Phase 1: Amendment 1's existing distribution behavioral test bar (real ingest
  write lands a row, heartbeat advances, hung-poll deadline classification),
  scoped explicitly to poll/ingest. Config-construction failure is also verified: an entirely absent
  `KHIVE_EMAIL_*` configuration resolves to a clean stop, while an invalid or
  partial configuration reports terminal `Unhealthy` by component name (fence 3),
  covered by component-level tests asserting both outcomes without opening a
  real network connection.
- Phase 2 (future): its own component behavioral test proving a real outbound
  send lands, plus a fence-3-equivalent construction-failure test, before
  Decision point 4 is considered fully satisfied.

### Consequences

Amendment 1's Decision point 4 is satisfied incrementally rather than atomically:
Phase 1 closes the inbound half now; the outbound half remains open, tracked, and
explicitly not claimed as delivered. This keeps the accepted ADR and the shipped
binary's actual behavior in agreement at every merge, rather than allowing a merged
contract to run silently ahead of shipped capability.

---

## Amendment 3: Email Phase 2 Delivered and Separately Supervised (2026-07-29)

**Status**: accepted (2026-07-29)\
**Trigger**: a binary force-linked `email-outbound` while this accepted ADR and
INSTALL still described Phase 2 as unshipped — the documentation lagged the shipped
capability. [ADR-122](ADR-122-email-outbound-delivery.md) now carries the accepted
outbox and delivery contract.

### Decision

Amendment 2's Phase 2 is delivered. A configured distribution daemon starts
two independently registered components:

- `email-channel` owns inbound IMAP poll/ingest; and
- `email-outbound` drains pending `email:*` outbound message notes and invokes
  the channel SMTP transport.

On configured startup, `email-outbound` includes stored messages from the
pre-delivery backlog: there is no age cutoff. It permanently records an
allowlist rejection without calling the transport, records transport failures
as pending per-message retries with bounded exponential backoff, and stamps
`delivery = "delivered"` only after transport acceptance. A crash or durable
stamp failure in that interval therefore redelivers at least once; both
attempts carry the UUIDv5 Message-ID deterministically derived from the note
UUID.

The two components have separate ADR-119 registration names, restart budgets,
and process-local health rows. Failure or terminal configuration state for
`email-outbound` cannot overwrite the `email-channel` health identity.

The Phase 1 interim statement in Amendment 2 remains historical context and is
no longer the current operator contract. A binary that does not link
`khive-component-email`, a non-daemon invocation, or an entirely absent email
configuration still performs no SMTP delivery; a downstream workspace that
links both registrations performs both.

### Verify by

Run the `khive-component-email` library tests serially:

```bash
cargo test -p khive-component-email --lib -- --test-threads=1
```

Ratification requires behavioral coverage for successful drain and stamp,
allowlist failure, transient retry/backoff, a fault between transport
acceptance and durable stamp with same-Message-ID redelivery, and distinct
supervisor health rows for `email-channel` and `email-outbound`. Static binary
inspection or a roster-string match alone is not delivery evidence.
